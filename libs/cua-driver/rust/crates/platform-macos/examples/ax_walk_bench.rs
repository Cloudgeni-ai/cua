//! Measure where the time of a window's accessibility read goes.
//!
//! ```sh
//! cargo run --release -p platform-macos --example ax_walk_bench -- <app name or pid> [window title substring] [runs]
//! ```
//!
//! Reads only: it never acts on the window. For the window it prints
//! one JSON line per measurement:
//!
//! - `attr`: the latency of each single-attribute AX request over the first
//!   nodes of the window (breadth first), and of one
//!   `AXUIElementCopyMultipleAttributeValues` request for the same attributes.
//! - `walk`: `get_window_state`'s walk under several time budgets: elapsed
//!   time, nodes visited and pending, actionable elements found, and the
//!   lowest actionable element's bottom edge relative to the window's.

use std::time::{Duration, Instant};

use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
use core_foundation::string::CFString;
use cua_driver_core::walk_budget::WalkBudget;
use platform_macos::ax::bindings::*;

// Declared here too, so the bench builds against a walker that predates it.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    #[link_name = "AXUIElementCopyMultipleAttributeValues"]
    fn copy_multiple_attribute_values(
        element: AXUIElementRef,
        attributes: CFArrayRef,
        options: u32,
        values: *mut CFArrayRef,
    ) -> AXError;
}

const ATTRS: [&str; 15] = [
    "AXRole",
    "AXSubrole",
    "AXTitle",
    "AXValue",
    "AXDescription",
    "AXIdentifier",
    "AXHelp",
    "AXEnabled",
    "AXSelected",
    "AXPosition",
    "AXSize",
    "AXValueDescription",
    "AXMinValue",
    "AXMaxValue",
    "AXChildren",
];

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i]
}

fn stats(mut v: Vec<f64>) -> serde_json::Value {
    v.sort_by(f64::total_cmp);
    let n = v.len().max(1) as f64;
    serde_json::json!({
        "n": v.len(),
        "mean_ms": (v.iter().sum::<f64>() / n * 1000.0).round() / 1000.0,
        "p50_ms": (percentile(&v, 0.5) * 1000.0).round() / 1000.0,
        "p95_ms": (percentile(&v, 0.95) * 1000.0).round() / 1000.0,
        "max_ms": (v.last().copied().unwrap_or(0.0) * 1000.0).round() / 1000.0,
    })
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

unsafe fn window_element(pid: i32, window_id: u32) -> Option<AXUIElementRef> {
    let app = AXUIElementCreateApplication(pid);
    AXUIElementSetMessagingTimeout(app, 2.0);
    // Also finds a window on another Space, as the walk does.
    let mut found = None;
    for w in copy_ax_windows_including(app, pid, window_id) {
        if found.is_none() && ax_get_window_id(w) == Some(window_id) {
            found = Some(w);
        } else {
            CFRelease(w as CFTypeRef);
        }
    }
    found
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let app = args.get(1).cloned().unwrap_or_else(|| "Simulator".into());
    let title = args.get(2).cloned().unwrap_or_default();
    let runs: usize = args.get(3).and_then(|r| r.parse().ok()).unwrap_or(3);
    let pid_arg: Option<i32> = app.parse().ok();
    let windows = platform_macos::windows::all_windows();
    let Some(window) = windows.iter().find(|w| {
        w.layer == 0
            && pid_arg.map_or(w.app_name == app, |pid| w.pid == pid)
            && w.title.contains(&title)
            && w.bounds.width > 64.0
    }) else {
        eprintln!("no window for {app} {title}");
        std::process::exit(2);
    };
    let (pid, wid) = (window.pid, window.window_id);
    let bounds = &window.bounds;
    println!(
        "{}",
        serde_json::json!({"window": {"pid": pid, "window_id": wid, "title": window.title,
            "x": bounds.x, "y": bounds.y, "w": bounds.width, "h": bounds.height}})
    );

    unsafe {
        // Per-attribute latency over the first nodes, breadth first.
        let root = window_element(pid, wid).expect("window not in AXWindows");
        let mut queue = std::collections::VecDeque::from([root]);
        let mut sample = Vec::new();
        while let Some(e) = queue.pop_front() {
            AXUIElementSetMessagingTimeout(e, 2.0);
            sample.push(e);
            if sample.len() >= 120 {
                break;
            }
            for c in copy_children(e) {
                queue.push_back(c);
            }
        }
        let mut per_attr: Vec<(String, Vec<f64>)> =
            ATTRS.iter().map(|a| (a.to_string(), Vec::new())).collect();
        let mut actions = Vec::new();
        let mut multi = Vec::new();
        let mut single_total = Vec::new();
        let names: Vec<CFString> = ATTRS.iter().map(|a| CFString::new(a)).collect();
        let array = CFArray::from_CFTypes(&names);
        for &e in &sample {
            let node_start = Instant::now();
            for (i, attr) in ATTRS.iter().enumerate() {
                let name = CFString::new(attr);
                let mut value: CFTypeRef = std::ptr::null();
                let start = Instant::now();
                let _ = AXUIElementCopyAttributeValue(e, name.as_concrete_TypeRef(), &mut value);
                per_attr[i].1.push(ms(start.elapsed()));
                if !value.is_null() {
                    CFRelease(value);
                }
            }
            let start = Instant::now();
            let _ = copy_action_names(e);
            actions.push(ms(start.elapsed()));
            single_total.push(ms(node_start.elapsed()));
            let mut values: CFArrayRef = std::ptr::null();
            let start = Instant::now();
            let _ = copy_multiple_attribute_values(e, array.as_concrete_TypeRef(), 0, &mut values);
            let _ = copy_action_names(e);
            multi.push(ms(start.elapsed()));
            if !values.is_null() {
                CFRelease(values as CFTypeRef);
            }
        }
        let attrs: serde_json::Map<String, serde_json::Value> = per_attr
            .into_iter()
            .map(|(name, v)| (name, stats(v)))
            .collect();
        println!(
            "{}",
            serde_json::json!({"attr": {
                "nodes": sample.len(),
                "single": attrs,
                "actions": stats(actions),
                "per_node_single_requests": stats(single_total),
                "per_node_multi_plus_actions": stats(multi),
            }})
        );
        for e in sample {
            CFRelease(e as CFTypeRef);
        }
    }

    for timeout_ms in [1_000_u64, 3_000, 60_000] {
        for run in 0..runs {
            let start = Instant::now();
            // Model-facing reads list only the menu bar's titles.
            let result = platform_macos::ax::tree::with_menus_collapsed(true, || {
                platform_macos::ax::tree::walk_tree_budgeted(
                    pid,
                    Some(wid),
                    None,
                    platform_macos::ax::DEFAULT_MAX_DEPTH,
                    WalkBudget::new(timeout_ms, 2_000),
                )
            });
            let elapsed = ms(start.elapsed());
            if std::env::var_os("AX_BENCH_DUMP").is_some() && run == 0 && timeout_ms == 60_000 {
                eprintln!("{}", result.tree_markdown);
            }
            let actionable: Vec<_> = result
                .nodes
                .iter()
                .filter(|n| n.element_index.is_some())
                .collect();
            let lowest = actionable
                .iter()
                .filter_map(|n| n.frame)
                .map(|f| f[1] + f[3])
                .fold(f64::MIN, f64::max);
            println!(
                "{}",
                serde_json::json!({"walk": {
                    "timeout_ms": timeout_ms,
                    "run": run,
                    "elapsed_ms": elapsed.round(),
                    "walk_elapsed_ms": result.walk.elapsed_ms,
                    "truncated": result.truncated,
                    "reason": result.walk.reason(),
                    "visited": result.walk.nodes_visited,
                    "pending": result.walk.nodes_pending,
                    "nodes": result.nodes.len(),
                    "actionable": actionable.len(),
                    "lowest_actionable_bottom_rel": if actionable.is_empty() { serde_json::Value::Null } else { serde_json::json!(((lowest - bounds.y) / bounds.height * 1000.0).round() / 1000.0) },
                }})
            );
        }
    }
}
