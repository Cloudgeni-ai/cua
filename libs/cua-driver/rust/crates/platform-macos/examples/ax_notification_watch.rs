//! Which windows an app's accessibility notifications come from.
//!
//! ```sh
//! cargo run --release -p platform-macos --example ax_notification_watch -- <pid> [seconds] [window id]
//! ```
//!
//! Watches only: it registers for the app's notifications and never acts.
//! With a window id it also registers the same notifications on that window's
//! element, to show which of them a window-level registration receives.
//! For every notification it resolves the window of the element it names
//! (`_AXUIElementGetWindow` on the element, else on its `AXWindow`), and at
//! the end prints the counts per notification and window, and how long the
//! resolution took.

use std::collections::BTreeMap;
use std::os::raw::c_void;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
use core_foundation::runloop::{kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopSource};
use core_foundation::string::{CFString, CFStringRef};
use platform_macos::ax::bindings::*;

type AXObserverRef = *mut c_void;
type Callback = unsafe extern "C" fn(AXObserverRef, AXUIElementRef, CFStringRef, *mut c_void);

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXObserverCreate(pid: i32, callback: Callback, observer: *mut AXObserverRef) -> AXError;
    fn AXObserverAddNotification(
        observer: AXObserverRef,
        element: AXUIElementRef,
        notification: CFStringRef,
        refcon: *mut c_void,
    ) -> AXError;
    fn AXObserverGetRunLoopSource(observer: AXObserverRef) -> *mut c_void;
}

#[derive(Default)]
struct Seen {
    counts: BTreeMap<(&'static str, String, String), u64>,
    resolve_ms: Vec<f64>,
}

static SEEN: Mutex<Option<Seen>> = Mutex::new(None);

unsafe fn window_of(element: AXUIElementRef) -> Option<u32> {
    if let Some(id) = ax_get_window_id(element) {
        return Some(id);
    }
    let window = copy_element_attr(element, "AXWindow")?;
    let id = ax_get_window_id(window);
    CFRelease(window as CFTypeRef);
    id
}

unsafe extern "C" fn on_note(
    _observer: AXObserverRef,
    element: AXUIElementRef,
    name: CFStringRef,
    refcon: *mut c_void,
) {
    let name = CFString::wrap_under_get_rule(name).to_string();
    let on = if refcon.is_null() { "app" } else { "window" };
    let start = Instant::now();
    AXUIElementSetMessagingTimeout(element, 0.25);
    let window = window_of(element).map_or_else(|| "unknown".to_string(), |id| id.to_string());
    let took = start.elapsed().as_secs_f64() * 1000.0;
    let mut seen = SEEN.lock().unwrap();
    let seen = seen.get_or_insert_with(Seen::default);
    *seen.counts.entry((on, name, window)).or_default() += 1;
    seen.resolve_ms.push(took);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let pid: i32 = args.get(1).and_then(|p| p.parse().ok()).expect("pid");
    let seconds: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20);
    let window_id: Option<u32> = args.get(3).and_then(|w| w.parse().ok());
    unsafe {
        let mut observer: AXObserverRef = std::ptr::null_mut();
        assert_eq!(
            AXObserverCreate(pid, on_note, &mut observer),
            kAXErrorSuccess
        );
        let app_element = AXUIElementCreateApplication(pid);
        let window_element = window_id.and_then(|id| {
            let windows = copy_ax_windows(app_element);
            let found = windows
                .iter()
                .copied()
                .find(|window| ax_get_window_id(*window) == Some(id));
            for window in &windows {
                if Some(*window) != found {
                    CFRelease(*window as CFTypeRef);
                }
            }
            found
        });
        println!(
            "{}",
            serde_json::json!({"window_element_found": window_element.is_some()})
        );
        for name in [
            "AXLayoutChanged",
            "AXCreated",
            "AXUIElementDestroyed",
            "AXValueChanged",
            "AXTitleChanged",
            "AXFocusedUIElementChanged",
            "AXSelectedChildrenChanged",
            "AXRowCountChanged",
            "AXWindowCreated",
            "AXSheetCreated",
            "AXMenuOpened",
            "AXLoadComplete",
        ] {
            let cf = CFString::new(name);
            AXObserverAddNotification(
                observer,
                app_element,
                cf.as_concrete_TypeRef(),
                std::ptr::null_mut(),
            );
            if let Some(window) = window_element {
                let error = AXObserverAddNotification(
                    observer,
                    window,
                    cf.as_concrete_TypeRef(),
                    1 as *mut c_void,
                );
                println!(
                    "{}",
                    serde_json::json!({"window_register": name, "error": error})
                );
            }
        }
        let source = AXObserverGetRunLoopSource(observer);
        let source = CFRunLoopSource::wrap_under_get_rule(source as _);
        CFRunLoop::get_current().add_source(&source, kCFRunLoopDefaultMode);
    }
    let end = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < end {
        CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            Duration::from_millis(100),
            false,
        );
    }
    let seen = SEEN.lock().unwrap().take().unwrap_or_default();
    let mut resolve = seen.resolve_ms.clone();
    resolve.sort_by(f64::total_cmp);
    let p = |q: f64| {
        resolve
            .get(((resolve.len().max(1) - 1) as f64 * q).round() as usize)
            .copied()
            .unwrap_or(0.0)
    };
    for ((on, name, window), count) in &seen.counts {
        println!(
            "{}",
            serde_json::json!({"on": on, "note": name, "window": window, "count": count})
        );
    }
    println!(
        "{}",
        serde_json::json!({"seconds": seconds, "notes": resolve.len(),
            "resolve_p50_ms": p(0.5), "resolve_p95_ms": p(0.95), "resolve_max_ms": p(1.0)})
    );
}
