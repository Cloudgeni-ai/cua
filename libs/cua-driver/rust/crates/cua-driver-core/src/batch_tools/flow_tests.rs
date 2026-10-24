//! `run_steps` v2 against a fake two-screen app: steps that name their
//! target, waits, checks and the end-of-batch summary.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{
    protocol::{Content, ToolResult},
    tool::{Tool, ToolDef, ToolRegistry},
};

/// What the fake app shows.
#[derive(Default)]
struct Ui {
    screen: u8,
    email: String,
    dialog_open: bool,
    /// The "Reminders" checkbox; "Stuck" never changes when pressed.
    reminders: bool,
    /// "Select all" reads mixed until pressed, then checked.
    select_all_pressed: bool,
    /// A transient popup (no useful controls) stacked on top.
    bubble_open: bool,
    /// Reads left before the "Finish" button appears on screen 2.
    finish_delay_reads: u32,
    /// Reads left before a dialog's "Save" button appears on screen 4,
    /// over a page that already shows a link "Save".
    save_delay_reads: u32,
    /// Reads say they stopped at their node budget.
    truncated: bool,
    /// The "Unsaved changes" window, searched first, shows two buttons "OK",
    /// and the main window one.
    dialog_ok: bool,
    snapshot: u32,
    /// Element labels of the latest read, by element index.
    last_labels: Vec<(String, String)>,
    /// Whether each window read listed only the menu bar's titles.
    menu_bar_collapsed: Vec<bool>,
    calls: Vec<(String, Value)>,
}

impl Ui {
    fn elements(&mut self, window_id: u64) -> Vec<(&'static str, String, Option<String>)> {
        if window_id == 9 {
            return vec![("AXButton", "Done".into(), None)];
        }
        if window_id == 8 {
            if self.dialog_ok {
                return vec![
                    ("AXButton", "OK".into(), None),
                    ("AXButton", "OK".into(), None),
                ];
            }
            return vec![
                ("AXButton", "Discard".into(), None),
                ("AXButton", "Keep".into(), None),
            ];
        }
        if self.dialog_ok {
            return vec![("AXButton", "OK".into(), None)];
        }
        match self.screen {
            0 => vec![
                ("AXTextField", "Email".into(), Some(self.email.clone())),
                ("AXButton", "Next".into(), None),
                ("AXButton", "OK".into(), None),
                ("AXButton", "OK".into(), None),
                (
                    "AXCheckBox",
                    "Reminders".into(),
                    Some(if self.reminders { "1" } else { "0" }.into()),
                ),
                ("AXCheckBox", "Stuck".into(), Some("0".into())),
                (
                    "AXCheckBox",
                    "Select all".into(),
                    Some(if self.select_all_pressed { "1" } else { "2" }.into()),
                ),
            ],
            // A web page: "Edit billing" is a link, "Plan" only text.
            2 => vec![
                ("AXButton", "Cancel".into(), None),
                ("AXLink", "Edit billing".into(), None),
                ("AXStaticText", "Plan".into(), None),
            ],
            // A page with a link "Save" under a dialog still rendering.
            4 => {
                let mut rows = vec![("AXLink", "Save".into(), None)];
                if self.save_delay_reads == 0 {
                    rows.push(("AXButton", "Save".into(), None));
                } else {
                    self.save_delay_reads -= 1;
                }
                rows
            }
            // A page with two links "Details" and no button of the name.
            5 => vec![
                ("AXLink", "Details".into(), None),
                ("AXLink", "Details".into(), None),
            ],
            // A page that keeps changing (a progress line), with a link
            // "Edit billing".
            6 => vec![
                ("AXLink", "Edit billing".into(), None),
                ("AXStaticText", format!("Loading {}%", self.snapshot), None),
            ],
            _ => {
                let mut rows = vec![("AXButton", "Back".into(), None)];
                if self.finish_delay_reads == 0 {
                    rows.push(("AXButton", "Finish".into(), None));
                } else {
                    self.finish_delay_reads -= 1;
                }
                rows
            }
        }
    }
}

struct Fake {
    def: ToolDef,
    ui: Arc<Mutex<Ui>>,
}

#[async_trait]
impl Tool for Fake {
    fn def(&self) -> &ToolDef {
        &self.def
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let mut ui = self.ui.lock().unwrap();
        let name = self.def.name.clone();
        ui.calls.push((name.clone(), args.clone()));
        match name.as_str() {
            "list_windows" => {
                let mut windows = vec![json!({
                    "pid": 42, "window_id": 7, "app_name": "Demo", "title": "Main",
                    "z_index": 1, "is_on_screen": true,
                    "bounds": {"x": 0.0, "y": 0.0, "width": 800.0, "height": 600.0}
                })];
                if ui.dialog_open {
                    windows.push(json!({
                        "pid": 42, "window_id": 8, "app_name": "Demo", "title": "Unsaved changes",
                        "z_index": 2, "is_on_screen": true,
                        "bounds": {"x": 100.0, "y": 100.0, "width": 300.0, "height": 200.0}
                    }));
                }
                if ui.bubble_open {
                    windows.push(json!({
                        "pid": 42, "window_id": 9, "app_name": "Demo", "title": "Bookmark added",
                        "z_index": 3, "is_on_screen": true,
                        "bounds": {"x": 500.0, "y": 0.0, "width": 200.0, "height": 100.0}
                    }));
                }
                ToolResult::text("windows").with_structured(json!({ "windows": windows }))
            }
            "list_apps" => ToolResult::text("apps").with_structured(json!({
                "apps": [{"pid": 42, "name": "Demo", "bundle_id": "com.example.demo", "running": true, "active": false}]
            })),
            "get_window_state" => {
                let window_id = args["window_id"].as_u64().unwrap_or(7);
                ui.menu_bar_collapsed
                    .push(crate::window_state_view::menu_bar_collapsed());
                ui.snapshot += 1;
                let snapshot = ui.snapshot;
                let rows = ui.elements(window_id);
                let mut markdown = String::from("- [0] AXWindow \"Demo\"\n");
                let mut elements = Vec::new();
                ui.last_labels.clear();
                for (offset, (role, label, value)) in rows.iter().enumerate() {
                    let index = offset + 1;
                    markdown.push_str(&format!("  - [{index}] {role} \"{label}\"\n"));
                    let mut element = json!({
                        "element_index": index,
                        "element_token": format!("s{snapshot:08x}:{index}"),
                        "role": role,
                        "depth": 1,
                        "label": label,
                        "frame": {"x": 0, "y": index * 20, "w": 10, "h": 10},
                    });
                    if let Some(value) = value.as_ref().filter(|value| !value.is_empty()) {
                        element["value"] = json!(value);
                    }
                    elements.push(element);
                    ui.last_labels
                        .push((format!("s{snapshot:08x}:{index}"), label.clone()));
                }
                if ui.screen == 1 && window_id == 7 {
                    markdown.push_str("  - AXStaticText \"Step 2 of 2\"\n");
                }
                ToolResult::text(markdown.clone()).with_structured(json!({
                    "pid": 42,
                    "window_id": window_id,
                    "snapshot_id": format!("s{snapshot:08x}"),
                    "elements": elements,
                    "tree_markdown": markdown,
                    "truncated": ui.truncated,
                }))
            }
            "click" | "set_value" => {
                let token = args["element_token"].as_str().unwrap_or_default();
                let label = ui
                    .last_labels
                    .iter()
                    .find(|(candidate, _)| candidate == token)
                    .map(|(_, label)| label.clone());
                let Some(label) = label else {
                    return ToolResult::error(format!("{name}: stale or unknown element_token {token}"));
                };
                match (name.as_str(), label.as_str()) {
                    ("set_value", "Email") => {
                        ui.email = args["value"].as_str().unwrap_or_default().to_owned()
                    }
                    ("click", "Next") => ui.screen = 1,
                    ("click", "Reminders") => ui.reminders = !ui.reminders,
                    ("click", "Select all") => ui.select_all_pressed = true,
                    ("click", "Back") => ui.dialog_open = true,
                    ("click", "Keep") | ("click", "Discard") => ui.dialog_open = false,
                    _ => {}
                }
                ToolResult::text(format!("{name} {label} done"))
            }
            other => ToolResult::text(format!("{other} done")),
        }
    }
}

struct Harness {
    registry: Arc<ToolRegistry>,
    ui: Arc<Mutex<Ui>>,
}

impl Harness {
    fn new() -> Self {
        let ui = Arc::new(Mutex::new(Ui::default()));
        let mut registry = ToolRegistry::new();
        for name in super::BATCHABLE_TOOLS.iter().copied().chain([
            "get_window_state",
            "list_windows",
            "list_apps",
        ]) {
            registry.register(Box::new(Fake {
                def: ToolDef {
                    name: name.into(),
                    description: "fake".into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "pid": {"type": "integer"},
                            "window_id": {"type": "integer"},
                            "element_token": {"type": "string"},
                            "value": {"type": "string"},
                            "key": {"type": "string"},
                            "x": {"type": "number"},
                            "y": {"type": "number"}
                        }
                    }),
                    read_only: matches!(name, "get_window_state" | "list_windows" | "list_apps"),
                    destructive: false,
                    idempotent: false,
                    open_world: false,
                },
                ui: ui.clone(),
            }));
        }
        registry.register_session_tools();
        let registry = Arc::new(registry);
        registry.init_self_weak();
        Self { registry, ui }
    }

    async fn run(&self, args: Value) -> ToolResult {
        self.registry.invoke(super::RUN_STEPS_TOOL, args).await
    }

    fn calls(&self, tool: &str) -> Vec<Value> {
        self.ui
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(name, _)| name == tool)
            .map(|(_, args)| args.clone())
            .collect()
    }
}

fn text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn one_batch_crosses_screens_by_name() {
    let harness = Harness::new();
    let result = harness
        .run(json!({
            "steps": [
                {"set_value": {"app": "Demo", "role": "textfield", "name": "email", "value": "ada@example.com"},
                 "expect": {"role": "textfield", "name": "Email", "value": "ada@example.com"}},
                {"click": {"role": "button", "name": "Next"}, "expect": {"text": "Step 2"}},
                {"tool": "click", "args": {"role": "button", "name": "Back"},
                 "expect": {"app": "Demo", "window": "Unsaved", "role": "button", "name": "Keep"}},
                {"click": {"role": "button", "name": "Keep"},
                 "expect": {"name": "Keep", "gone": true}}
            ],
            "observe": true
        }))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let structured = result.structured_content.clone().unwrap();
    assert_eq!(structured["executed"], 4);
    // The Keep button lives in the dialog window that opened on top: the
    // app-level target found it there without a window id.
    assert_eq!(structured["steps"][3]["target"]["window"]["window_id"], 8);
    assert!(
        text(&result).contains("on [2] AXButton \"Keep\""),
        "{}",
        text(&result)
    );

    let clicks = harness.calls("click");
    assert_eq!(clicks.len(), 3);
    assert_eq!(clicks[0]["pid"], 42);
    assert_eq!(clicks[0]["window_id"], 7);
    assert!(
        clicks[0].get("role").is_none(),
        "selector keys never reach the tool"
    );
    assert!(clicks[0]["element_token"].as_str().unwrap().ends_with(":2"));
    assert_eq!(harness.ui.lock().unwrap().email, "ada@example.com");

    // Lookup reads ask for full_output, which `since` never remembers; the
    // end observation is the one diff read, of the last window used.
    let reads = harness.calls("get_window_state");
    let observed = reads.last().unwrap();
    assert_eq!(observed["since"], "latest");
    // Lookup reads are full_output, which `since` never remembers; the only
    // other reads are one plain baseline per window the batch looked in.
    let baselines: Vec<u64> = reads[..reads.len() - 1]
        .iter()
        .filter(|read| read["full_output"] != true)
        .map(|read| read["window_id"].as_u64().unwrap())
        .collect();
    assert_eq!(baselines, [7, 8], "{reads:?}");
}

#[tokio::test]
async fn several_matches_fail_with_the_candidates_and_nth_picks_one() {
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [
            {"click": {"app": "Demo", "role": "button", "name": "OK"}},
            {"click": {"role": "button", "name": "Next"}}
        ]}))
        .await;
    assert_eq!(result.is_error, Some(true));
    let structured = result.structured_content.clone().unwrap();
    assert_eq!(structured["failed_step"], 0);
    assert_eq!(structured["steps"][0]["phase"], "find");
    assert_eq!(structured["steps"][0]["code"], "ambiguous");
    let message = text(&result);
    assert!(message.contains("2 elements match"), "{message}");
    assert!(message.contains("nth 1: [4] AXButton \"OK\""), "{message}");
    assert!(harness.calls("click").is_empty(), "nothing clicked");

    // Another window of the app with one "OK" is not the one meant: the tie
    // in the window searched first is the answer.
    {
        let mut ui = harness.ui.lock().unwrap();
        ui.dialog_open = true;
        ui.dialog_ok = true;
    }
    let result = harness
        .run(json!({"steps": [{"click": {"app": "Demo", "role": "button", "name": "OK"}}]}))
        .await;
    assert_eq!(result.is_error, Some(true), "{}", text(&result));
    assert!(
        text(&result).contains("2 elements match"),
        "{}",
        text(&result)
    );
    assert!(harness.calls("click").is_empty(), "nothing clicked");
    {
        let mut ui = harness.ui.lock().unwrap();
        ui.dialog_open = false;
        ui.dialog_ok = false;
    }

    let result = harness
        .run(json!({"steps": [{"click": {"app": "Demo", "role": "button", "name": "OK", "nth": 1}}]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert!(harness.calls("click")[0]["element_token"]
        .as_str()
        .unwrap()
        .ends_with(":4"));
}

#[tokio::test]
async fn a_missing_target_waits_then_names_the_nearest_elements() {
    let harness = Harness::new();
    let started = std::time::Instant::now();
    let result = harness
        .run(json!({"steps": [
            {"click": {"app": "com.example.demo", "role": "button", "name": "Submit"}, "timeout_ms": 600}
        ]}))
        .await;
    let elapsed = started.elapsed();
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(
        message.contains("1. click ERROR (find): no button \"Submit\""),
        "{message}"
    );
    assert!(
        message.contains("nearest: [2] AXButton \"Next\""),
        "{message}"
    );
    assert!(message.contains("waited"), "{message}");
    assert!(elapsed >= Duration::from_millis(500), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(2_000), "{elapsed:?}");
}

#[tokio::test]
async fn a_named_target_that_appears_late_is_waited_for() {
    let harness = Harness::new();
    harness.ui.lock().unwrap().screen = 1;
    harness.ui.lock().unwrap().finish_delay_reads = 2;
    let result = harness
        .run(json!({"steps": [
            {"wait_for": {"app": "Demo", "text": "Step 2"}},
            {"click": {"role": "button", "name": "Finish"}}
        ]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(harness.calls("click").len(), 1);
}

#[tokio::test]
async fn a_failed_expect_stops_the_batch_and_says_what_was_found() {
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [
            {"set_value": {"app": "Demo", "role": "textfield", "name": "Email", "value": "bob"},
             "expect": {"name": "Email", "value": "ada", "timeout_ms": 300}},
            {"click": {"role": "button", "name": "Next"}}
        ], "observe": true}))
        .await;
    assert_eq!(result.is_error, Some(true));
    let structured = result.structured_content.clone().unwrap();
    assert_eq!(structured["failed_step"], 0);
    assert_eq!(structured["steps"][0]["phase"], "expect");
    assert_eq!(structured["steps"][0]["code"], "unexpected_state");
    let message = text(&result);
    assert!(message.contains("value=\"bob\""), "{message}");
    assert!(harness.calls("click").is_empty(), "later steps never run");
    // The failed batch is still observed, in the window it was working in.
    assert_eq!(structured["observation"]["ok"], true);
    assert_eq!(
        harness.calls("get_window_state").last().unwrap()["window_id"],
        7
    );
}

#[tokio::test]
async fn a_check_only_step_and_gone_on_a_closed_window() {
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [
            {"expect": {"app": "Demo", "window": "Unsaved", "name": "Keep", "gone": true, "timeout_ms": 0}}
        ]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert!(harness.calls("click").is_empty());
}

#[tokio::test]
async fn bad_v2_steps_are_rejected_before_anything_runs() {
    let harness = Harness::new();
    for (step, needle) in [
        (json!({"click": {"pid": 1}, "args": {}}), "drop `args`"),
        (json!({"click": {}, "press_key": {}}), "one action per step"),
        (
            json!({"drag": {"app": "Demo", "name": "x"}}),
            "drag takes coordinates",
        ),
        (
            json!({"click": {"app": "Demo", "name": "x", "element_token": "s1:1"}}),
            "not both",
        ),
        (json!({"click": {"name": "Next"}}), "no window"),
        (
            json!({"wait_for": {"app": "Demo"}}),
            "needs `role`, `name` or `text`",
        ),
        (json!({"expect": []}), "1 to 4 checks"),
        (json!({}), "empty step"),
        (
            json!({"click": {"app": "Demo", "name": "x"}, "timeout_ms": 99999}),
            "timeout_ms",
        ),
        (json!({"wait": {"app": "Demo"}}), "cannot run in a batch"),
    ] {
        let result = harness
            .run(json!({"steps": [{"press_key": {"key": "a"}}, step.clone()]}))
            .await;
        assert_eq!(result.is_error, Some(true), "{step}");
        let message = text(&result);
        assert!(message.contains("step 2"), "{step}: {message}");
        assert!(message.contains(needle), "{step}: {message}");
    }
    assert!(harness.calls("press_key").is_empty());
    assert!(harness.calls("list_windows").is_empty());
}

#[tokio::test]
async fn later_steps_inherit_the_window() {
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [
            {"click": {"app": "Demo", "role": "textfield", "name": "Email"}},
            {"press_key": {"key": "tab"}}
        ]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let pressed = &harness.calls("press_key")[0];
    assert_eq!(pressed["pid"], 42);
    assert_eq!(pressed["window_id"], 7);
}

#[tokio::test]
async fn a_popup_on_top_does_not_hide_the_main_window() {
    let harness = Harness::new();
    harness.ui.lock().unwrap().bubble_open = true;
    let result = harness
        .run(json!({"steps": [
            {"click": {"app": "Demo", "role": "button", "name": "Next"}, "expect": {"text": "Step 2"}}
        ], "observe": true}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let structured = result.structured_content.clone().unwrap();
    assert_eq!(structured["steps"][0]["target"]["window"]["window_id"], 7);
    // The observation follows the window the batch acted in, not the popup.
    assert_eq!(
        harness.calls("get_window_state").last().unwrap()["window_id"],
        7
    );

    // A miss names the window with the most elements, and says how many
    // windows were searched.
    let result = harness
        .run(json!({"steps": [
            {"click": {"app": "Demo", "role": "button", "name": "Publish"}, "timeout_ms": 0}
        ]}))
        .await;
    let message = text(&result);
    assert!(message.contains("searched 2 windows"), "{message}");
    assert!(message.contains("window 7"), "{message}");
}

#[tokio::test]
async fn a_check_only_step_names_the_window_to_observe() {
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [{"expect": {"app": "Demo", "text": "Email", "timeout_ms": 0}}], "observe": true}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(
        result.structured_content.unwrap()["observation"]["ok"],
        true
    );
    assert_eq!(
        harness.calls("get_window_state").last().unwrap()["window_id"],
        7
    );
}

#[tokio::test]
async fn the_end_observation_diffs_against_the_batch_start() {
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [
            {"set_value": {"app": "Demo", "role": "textfield", "name": "Email", "value": "a@b.c"}}
        ], "observe": true}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let reads = harness.calls("get_window_state");
    // First a plain read with the observation's budget (remembered by
    // `since`), then the full lookup read, then the end diff.
    assert_eq!(reads[0]["max_elements"], 250);
    assert!(reads[0].get("full_output").is_none());
    assert_eq!(reads[1]["full_output"], true);
    let last = reads.last().unwrap();
    assert_eq!(last["since"], "latest");
    assert_eq!(last["max_elements"], 250);
    assert_eq!(reads.len(), 3, "one baseline per window, not per poll");

    // Without observe there is no baseline read.
    let before = harness.calls("get_window_state").len();
    harness
        .run(json!({"steps": [{"click": {"app": "Demo", "role": "button", "name": "Next"}}]}))
        .await;
    assert_eq!(harness.calls("get_window_state").len(), before + 1);
}

#[tokio::test]
async fn a_trailing_get_window_state_in_shorthand_is_the_observation() {
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [
            {"click": {"app": "Demo", "role": "button", "name": "Next"}},
            {"get_window_state": {}}
        ]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["total"], 1, "the read is not a step");
    assert_eq!(structured["observation"]["ok"], true);
    assert_eq!(
        harness.calls("get_window_state").last().unwrap()["window_id"],
        7
    );
}

#[tokio::test]
async fn a_button_named_for_a_link_is_pressed_as_the_link_at_once() {
    // Every run of a repeated web procedure asked for button "Edit billing",
    // a link on the page: the step waited its whole timeout, then failed.
    let harness = Harness::new();
    harness.ui.lock().unwrap().screen = 2;
    let started = std::time::Instant::now();
    let result = harness
        .run(json!({"steps": [
            {"wait_for": {"app": "Demo", "role": "button", "name": "Edit billing", "exact": true,
                          "enabled": true, "timeout_ms": 10000}},
            {"click": {"role": "button", "name": "Edit billing", "exact": true}}
        ]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    // The stand-in is used once the window has stayed the same for 1.5 s.
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(1_400), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
    let clicks = harness.calls("click");
    assert_eq!(clicks.len(), 1);
    assert!(clicks[0]["element_token"].as_str().unwrap().ends_with(":2"));
    let message = text(&result);
    assert!(
        message.contains(
            "2. click ok: click Edit billing done; no button \"Edit billing\" (exact) here, \
             so it used the link of that name, [2] AXLink \"Edit billing\"; name it as role \
             \"link\" next time; on [2] AXLink \"Edit billing\""
        ),
        "{message}"
    );
    let structured = result.structured_content.unwrap();
    assert!(structured["steps"][1]["target"]["role_note"]
        .as_str()
        .unwrap()
        .contains("used the link"));
    assert_eq!(
        structured["steps"][1]["target"]["stand_in"],
        json!({"role": "AXLink", "name": "Edit billing"})
    );
    assert!(structured["steps"][0]["wait_for"]["role_note"]
        .as_str()
        .unwrap()
        .contains("used the link"));
}

#[tokio::test]
async fn a_wait_for_a_name_shown_only_as_text_waits_its_whole_time() {
    // A slow page may yet render the button behind a heading of its name.
    let harness = Harness::new();
    harness.ui.lock().unwrap().screen = 2;
    let started = std::time::Instant::now();
    let result = harness
        .run(json!({"steps": [
            {"wait_for": {"app": "Demo", "role": "button", "name": "Plan", "timeout_ms": 2500}},
            {"click": {"role": "button", "name": "Plan"}}
        ]}))
        .await;
    let elapsed = started.elapsed();
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("no button \"Plan\""), "{message}");
    assert!(
        message.contains("nearest: [3] AXStaticText \"Plan\""),
        "{message}"
    );
    assert!(!message.contains("stopped"), "{message}");
    assert!(elapsed >= Duration::from_millis(2_000), "{elapsed:?}");
    assert!(harness.calls("click").is_empty(), "nothing pressed");
}

#[tokio::test]
async fn a_window_that_never_settles_takes_no_stand_in() {
    // The page keeps changing until the wait runs out: the link is not
    // taken for the button, and the step fails as it would without it.
    let harness = Harness::new();
    harness.ui.lock().unwrap().screen = 6;
    let result = harness
        .run(json!({"steps": [
            {"wait_for": {"app": "Demo", "role": "button", "name": "Edit billing",
                          "exact": true, "timeout_ms": 2500}},
            {"click": {"role": "button", "name": "Edit billing", "exact": true}}
        ]}))
        .await;
    assert_eq!(result.is_error, Some(true), "{}", text(&result));
    let message = text(&result);
    assert!(
        message.contains("no button \"Edit billing\" (exact)")
            && message.contains("nearest: [1] AXLink \"Edit billing\"")
            && message.contains("checked for"),
        "{message}"
    );
    assert!(!message.contains("used the link"), "{message}");
    assert!(harness.calls("click").is_empty(), "nothing pressed");
}

#[tokio::test]
async fn a_truncated_read_takes_no_stand_in() {
    // The read stopped at its node budget: the button may lie past the cut.
    let harness = Harness::new();
    {
        let mut ui = harness.ui.lock().unwrap();
        ui.screen = 2;
        ui.truncated = true;
    }
    let result = harness
        .run(json!({"steps": [
            {"wait_for": {"app": "Demo", "role": "button", "name": "Edit billing",
                          "exact": true, "timeout_ms": 2500}},
            {"click": {"role": "button", "name": "Edit billing", "exact": true}}
        ]}))
        .await;
    assert_eq!(result.is_error, Some(true), "{}", text(&result));
    assert!(
        !text(&result).contains("used the link"),
        "{}",
        text(&result)
    );
    assert!(harness.calls("click").is_empty(), "nothing pressed");
}

#[tokio::test]
async fn a_button_that_appears_after_a_link_of_its_name_is_the_one_pressed() {
    // An earlier step opened a dialog with a "Save" button over a page that
    // already shows a link "Save": the link must not be pressed while the
    // dialog is still rendering.
    let harness = Harness::new();
    {
        let mut ui = harness.ui.lock().unwrap();
        ui.screen = 4;
        ui.save_delay_reads = 3;
    }
    let result = harness
        .run(json!({"steps": [
            {"wait_for": {"app": "Demo", "role": "button", "name": "Save", "exact": true,
                          "enabled": true, "timeout_ms": 10000}},
            {"click": {"role": "button", "name": "Save", "exact": true}}
        ]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let clicks = harness.calls("click");
    assert_eq!(clicks.len(), 1);
    assert!(
        clicks[0]["element_token"].as_str().unwrap().ends_with(":2"),
        "{}",
        clicks[0]
    );
    let message = text(&result);
    assert!(!message.contains("used the link"), "{message}");
    let structured = result.structured_content.unwrap();
    assert!(structured["steps"][1]["target"]["stand_in"].is_null());
}

#[tokio::test]
async fn two_links_of_the_name_fail_a_button_step_once_the_window_settles() {
    // Neither link is the one meant: the wait does not hold on either, and
    // the step fails naming both, without waiting out its timeout.
    for steps in [
        json!([
            {"wait_for": {"app": "Demo", "role": "button", "name": "Details", "exact": true,
                          "enabled": true, "timeout_ms": 10000}},
            {"click": {"role": "button", "name": "Details", "exact": true}}
        ]),
        json!([{"click": {"app": "Demo", "role": "button", "name": "Details", "exact": true}}]),
    ] {
        let harness = Harness::new();
        harness.ui.lock().unwrap().screen = 5;
        let started = std::time::Instant::now();
        let result = harness.run(json!({ "steps": steps })).await;
        let elapsed = started.elapsed();
        assert_eq!(result.is_error, Some(true), "{}", text(&result));
        let message = text(&result);
        assert!(
            message.contains(
                "2 elements of other roles have that exact name; name the role of the one \
                 meant: [1] AXLink \"Details\"; [2] AXLink \"Details\""
            ),
            "{message}"
        );
        assert!(!message.contains("used the link"), "{message}");
        assert!(elapsed >= Duration::from_millis(1_400), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
        assert!(harness.calls("click").is_empty(), "nothing pressed");
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["steps"][0]["code"], "ambiguous", "{structured}");
    }
}

#[tokio::test]
async fn toggle_to_presses_a_checkbox_only_when_it_is_not_in_that_state() {
    let harness = Harness::new();
    let checkbox = json!({"app": "Demo", "role": "checkbox", "name": "Reminders", "exact": true});
    // Off, asked on: pressed, and the state is read after.
    let result = harness
        .run(json!({"steps": [{"click": checkbox.clone(), "toggle_to": true}]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert!(harness.ui.lock().unwrap().reminders);
    assert_eq!(harness.calls("click").len(), 1);
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["steps"][0]["toggle"], "pressed");

    // On, asked on again: left as it is.
    let result = harness
        .run(json!({"steps": [{"click": checkbox.clone(), "toggle_to": true}]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(harness.calls("click").len(), 1, "not pressed again");
    assert!(harness.ui.lock().unwrap().reminders);
    assert!(
        text(&result).contains("already checked"),
        "{}",
        text(&result)
    );

    // On, asked off: pressed.
    let result = harness
        .run(json!({"steps": [{"click": checkbox, "toggle_to": false}]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert!(!harness.ui.lock().unwrap().reminders);
    assert_eq!(harness.calls("click").len(), 2);
}

#[tokio::test]
async fn toggle_to_presses_a_mixed_checkbox_either_way_and_checks_where_it_lands() {
    let select_all = json!({"app": "Demo", "role": "checkbox", "name": "Select all"});
    // Mixed is neither state: asked off, it is pressed, lands checked, and
    // the step says so instead of calling it "already not checked".
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [{"click": select_all.clone(), "toggle_to": false}]}))
        .await;
    assert_eq!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(harness.calls("click").len(), 1, "pressed");
    assert!(
        text(&result).contains("pressed but does not read not checked"),
        "{}",
        text(&result)
    );
    assert!(!text(&result).contains("already"), "{}", text(&result));

    // Asked on: pressed, and it reads checked.
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [{"click": select_all, "toggle_to": true}]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(harness.calls("click").len(), 1);
    let structured = result.structured_content.clone().unwrap();
    assert_eq!(structured["steps"][0]["toggle"], "pressed");
    assert!(text(&result).contains("it read mixed"), "{}", text(&result));
}

#[tokio::test]
async fn toggle_to_fails_clearly_on_a_press_that_does_not_take_or_a_non_toggle() {
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [
            {"click": {"app": "Demo", "role": "checkbox", "name": "Stuck"}, "toggle_to": true}
        ]}))
        .await;
    assert_eq!(result.is_error, Some(true));
    let structured = result.structured_content.clone().unwrap();
    assert_eq!(structured["steps"][0]["phase"], "expect");
    assert!(
        text(&result).contains("pressed but does not read checked"),
        "{}",
        text(&result)
    );

    let result = harness
        .run(json!({"steps": [
            {"click": {"app": "Demo", "role": "button", "name": "Next"}, "toggle_to": true}
        ]}))
        .await;
    assert_eq!(result.is_error, Some(true));
    let structured = result.structured_content.clone().unwrap();
    assert_eq!(structured["steps"][0]["code"], "not_a_toggle");
    assert_eq!(harness.ui.lock().unwrap().screen, 0, "Next was not pressed");

    // Only on a click that names its element.
    let result = harness
        .run(json!({"steps": [
            {"click": {"pid": 42, "window_id": 7, "element_token": "s00000001:5"}, "toggle_to": true}
        ]}))
        .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("rejected before running anything"),
        "{}",
        text(&result)
    );
}

#[tokio::test]
async fn a_step_whose_wait_saw_its_element_acts_on_that_read() {
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [
            {"click": {"pid": 42, "window_id": 7, "role": "button", "name": "Next", "exact": true},
             "wait_for": {"pid": 42, "window_id": 7, "role": "button", "name": "Next", "exact": true}}
        ]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(harness.ui.lock().unwrap().screen, 1, "Next was pressed");
    // The wait's read found the button; the lookup did not read again.
    assert_eq!(harness.calls("get_window_state").len(), 1);

    // Another element of that same read is picked from it too.
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [
            {"click": {"pid": 42, "window_id": 7, "role": "button", "name": "Next", "exact": true},
             "wait_for": {"pid": 42, "window_id": 7, "role": "textfield", "name": "Email"}}
        ]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(harness.calls("get_window_state").len(), 1);

    // A lookup that may be after a menu item reads the menus the wait's
    // read left out.
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [
            {"click": {"pid": 42, "window_id": 7, "name": "Next", "exact": true},
             "wait_for": {"pid": 42, "window_id": 7, "role": "button", "name": "Next"}}
        ]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(harness.calls("get_window_state").len(), 2);
    assert_eq!(
        harness.ui.lock().unwrap().menu_bar_collapsed,
        vec![true, false]
    );
}

#[tokio::test]
async fn lookups_read_the_menu_bar_menus_only_when_after_a_menu_item() {
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [
            {"click": {"app": "Demo", "role": "button", "name": "Next"}}
        ]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(harness.ui.lock().unwrap().menu_bar_collapsed, vec![true]);

    // Named without a role, the element might be a menu item.
    let harness = Harness::new();
    let result = harness
        .run(json!({"steps": [
            {"click": {"app": "Demo", "name": "Next"}}
        ]}))
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(harness.ui.lock().unwrap().menu_bar_collapsed, vec![false]);

    // A menu item, by any platform's role name.
    for role in ["menuitem", "AXMenuItem", "menu item", "MenuBarItem"] {
        let harness = Harness::new();
        let _ = harness
            .run(json!({"steps": [
                {"click": {"app": "Demo", "role": role, "name": "Next", "timeout_ms": 50}}
            ]}))
            .await;
        let reads = harness.ui.lock().unwrap().menu_bar_collapsed.clone();
        assert!(
            !reads.is_empty() && reads.iter().all(|c| !c),
            "{role}: {reads:?}"
        );
    }
}
