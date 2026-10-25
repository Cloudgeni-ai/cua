//! How long does the first read after an action take, and does it show the
//! action's result? Runs the real tools through the registry, so it measures
//! whatever the build's `click` and `get_window_state` do.
//!
//! ```sh
//! cargo run --release -p platform-macos --example ax_action_latency -- \
//!   <app name or pid> <window title substring> <rounds> <step> [<step> ...]
//! ```
//!
//! A step is `click:<label>` (click the element whose label is `<label>`),
//! `type:<text>` (type into the focused element) or `key:<key>`. Each round
//! runs every step in turn; after each step it reads the window at once
//! (timed), then again 2 s later, and says whether the first read already
//! matched (digits ignored, so live numbers do not count). Use it only on
//! windows you own: it acts.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

fn structured(result: &cua_driver_core::protocol::ToolResult) -> Value {
    if result.is_error == Some(true) {
        let text = result
            .content
            .iter()
            .map(|c| format!("{c:?}"))
            .collect::<Vec<_>>()
            .join(" ");
        panic!("tool error: {text}");
    }
    result.structured_content.clone().unwrap_or(Value::Null)
}

fn without_digits(text: &str) -> String {
    text.chars().filter(|c| !c.is_ascii_digit()).collect()
}

fn main() {
    configure();
    let args: Vec<String> = std::env::args().collect();
    let (app, title, rounds) = (&args[1], &args[2], args[3].parse::<usize>().unwrap());
    let steps = &args[4..];
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let registry = platform_macos::register_tools();
    let call = |name: &str, args: Value| runtime.block_on(registry.invoke(name, args));
    let windows = structured(&call("list_windows", json!({})));
    let window = windows["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| {
            (w["app_name"].as_str() == Some(app.as_str())
                || app.parse::<i64>().ok() == w["pid"].as_i64())
                && w["title"]
                    .as_str()
                    .is_some_and(|t| t.contains(title.as_str()))
        })
        .unwrap_or_else(|| panic!("no window {app} / {title}"))
        .clone();
    let (pid, window_id) = (
        window["pid"].as_i64().unwrap(),
        window["window_id"].as_i64().unwrap(),
    );
    println!("{}", json!({"window": window}));
    if rounds == 0 {
        let state = structured(&call(
            "get_window_state",
            json!({"pid": pid, "window_id": window_id, "include_screenshot": false, "tree_format": "both"}),
        ));
        for e in state["elements"].as_array().into_iter().flatten() {
            if e["element_index"].is_number() {
                println!("{} {} {}", e["element_index"], e["role"], e["label"]);
            }
        }
        return;
    }
    let read = |pid: i64, timeout_ms: u64| {
        let started = Instant::now();
        let state = structured(&call(
            "get_window_state",
            json!({"pid": pid, "window_id": window_id, "include_screenshot": false,
                   "tree_format": "both", "max_elements": 2000, "max_depth": 25,
                   "timeout_ms": timeout_ms}),
        ));
        (state, started.elapsed().as_secs_f64() * 1000.0)
    };
    let mut rows: Vec<(String, f64, f64, bool)> = Vec::new();
    for round in 0..rounds {
        for step in steps {
            // Untimed reads get a long budget so they are complete; the timed
            // read keeps the default one.
            let (before, _) = read(pid, 5_000);
            let (kind, operand) = step.split_once(':').unwrap();
            let action = match kind {
                "click" => {
                    let element = before["elements"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|e| {
                            e["label"].as_str() == Some(operand) && e["element_index"].is_number()
                        })
                        .unwrap_or_else(|| panic!("no element {operand}"));
                    json!({"tool": "click", "args": {"pid": pid, "window_id": window_id,
                        "element_token": element["element_token"]}})
                }
                "type" => {
                    json!({"tool": "type_text", "args": {"pid": pid, "window_id": window_id, "text": operand}})
                }
                "key" => {
                    json!({"tool": "press_key", "args": {"pid": pid, "window_id": window_id, "key": operand}})
                }
                other => panic!("unknown step {other}"),
            };
            let started = Instant::now();
            structured(&call(
                action["tool"].as_str().unwrap(),
                action["args"].clone(),
            ));
            let action_ms = started.elapsed().as_secs_f64() * 1000.0;
            let (first, read_ms) = read(pid, 1_000);
            std::thread::sleep(Duration::from_secs(2));
            let (later, _) = read(pid, 5_000);
            let tree =
                |state: &Value| without_digits(state["tree_markdown"].as_str().unwrap_or(""));
            let matched = tree(&first) == tree(&later);
            println!(
                "{}",
                json!({"round": round, "step": step, "action_ms": action_ms.round(),
                       "read_ms": read_ms.round(), "matched_later": matched,
                       "nodes": first["elements"].as_array().map_or(0, Vec::len),
                       "later_nodes": later["elements"].as_array().map_or(0, Vec::len),
                       "truncated": first["truncated"], "settle": first["settle"]})
            );
            rows.push((step.clone(), action_ms, read_ms, matched));
        }
    }
    for step in steps {
        let mut reads: Vec<f64> = rows.iter().filter(|r| &r.0 == step).map(|r| r.2).collect();
        let actions: Vec<f64> = rows.iter().filter(|r| &r.0 == step).map(|r| r.1).collect();
        reads.sort_by(f64::total_cmp);
        let matched = rows.iter().filter(|r| &r.0 == step && r.3).count();
        println!(
            "{}",
            json!({"summary": step, "n": reads.len(), "matched_later": matched,
                   "read_p50_ms": reads[reads.len() / 2].round(),
                   "read_mean_ms": (reads.iter().sum::<f64>() / reads.len() as f64).round(),
                   "read_max_ms": reads.last().copied().unwrap_or(0.0).round(),
                   "action_mean_ms": (actions.iter().sum::<f64>() / actions.len() as f64).round()})
        );
    }
}

/// `AX_SETTLE_TIMING=first_change,quiet,max_wait,max_reads` (ms) overrides
/// the settle timing of this process.
fn configure() {
    if let Ok(spec) = std::env::var("AX_SETTLE_TIMING") {
        let v: Vec<u64> = spec.split(',').map(|p| p.parse().unwrap()).collect();
        platform_macos::ax::activity::set_timing(platform_macos::ax::activity::Timing {
            first_change: Duration::from_millis(v[0]),
            quiet: Duration::from_millis(v[1]),
            max_wait: Duration::from_millis(v[2]),
            max_reads: v[3] as u32,
        });
    }
}
