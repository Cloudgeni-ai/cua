//! The element under a screen point in one window.
//!
//! `AXUIElementCopyElementAtPosition` answers for an application: it returns
//! the element of whichever of the app's windows is on top at the point.
//! Asked of a window element it fails (`kAXErrorIllegalArgument`), so a
//! window behind another window of the same app has no hit test of its own.
//! For such a window the answer is found in its own tree: the deepest
//! element whose frame contains the point, preferring a later sibling
//! (drawn over an earlier one) where siblings overlap.

use core_foundation::base::{CFRelease, CFTypeRef};

use super::bindings::{
    ax_get_window_id, copy_ax_windows_including, copy_children, element_at_screen_position,
    element_screen_rect, AXUIElementCreateApplication, AXUIElementRef,
};

/// How deep the walk goes below the window.
const MAX_DEPTH: usize = 64;
/// How many frames the walk reads at most, so a window with very wide
/// containers still answers quickly.
const MAX_FRAMES: usize = 600;

fn contains(frame: [f64; 4], (x, y): (f64, f64)) -> bool {
    let [left, top, width, height] = frame;
    x >= left && x < left + width && y >= top && y < top + height
}

/// From `root`, which contains `point`, the deepest node reached by stepping
/// into a child whose frame contains it, the last such child of each node
/// first. A child without a frame is not stepped into: nothing says it is
/// under the point. The walk stops after [`MAX_DEPTH`] steps or
/// [`MAX_FRAMES`] frame reads, at the node it reached.
pub(crate) fn deepest_at<N>(
    root: N,
    point: (f64, f64),
    mut children: impl FnMut(&N) -> Vec<N>,
    mut frame: impl FnMut(&N) -> Option<[f64; 4]>,
) -> N {
    let mut current = root;
    let mut frames = 0;
    for _ in 0..MAX_DEPTH {
        let mut next = None;
        for child in children(&current).into_iter().rev() {
            if frames == MAX_FRAMES {
                break;
            }
            frames += 1;
            if frame(&child).is_some_and(|frame| contains(frame, point)) {
                next = Some(child);
                break;
            }
        }
        match next {
            Some(child) => current = child,
            None => break,
        }
    }
    current
}

/// Which element answers for `window_id` at a point: the application's own
/// hit test when it found an element of that window, else the walk of the
/// window's own tree (`in_window`), since the app's answer then belongs to
/// another window over this one.
pub(crate) fn window_hit<E>(
    app_hit: Option<(E, Option<u32>)>,
    window_id: u32,
    release: impl FnOnce(E),
    in_window: impl FnOnce() -> Option<E>,
) -> Option<E> {
    match app_hit {
        Some((element, Some(owner))) if owner == window_id => Some(element),
        Some((element, _)) => {
            release(element);
            in_window()
        }
        None => in_window(),
    }
}

/// A +1 element reference released when dropped.
struct Owned(AXUIElementRef);

impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as CFTypeRef) };
    }
}

impl Owned {
    fn into_raw(self) -> AXUIElementRef {
        let element = self.0;
        std::mem::forget(self);
        element
    }
}

/// The element of window `window_id` of `pid` under the screen point
/// (`x`, `y`), even where another window of the app is over it there.
/// `None` when the window is not found or does not contain the point.
///
/// # Safety
///
/// The caller must `CFRelease` the returned element.
pub unsafe fn element_at_point_in_window(
    pid: i32,
    window_id: u32,
    x: f64,
    y: f64,
) -> Option<AXUIElementRef> {
    let app_hit = element_at_screen_position(pid, x, y)
        .map(|element| (element, super::exact_target::element_window_id(element)));
    window_hit(
        app_hit,
        window_id,
        |element| CFRelease(element as CFTypeRef),
        || walk_window(pid, window_id, (x, y)),
    )
}

unsafe fn walk_window(pid: i32, window_id: u32, point: (f64, f64)) -> Option<AXUIElementRef> {
    let app = AXUIElementCreateApplication(pid);
    if app.is_null() {
        return None;
    }
    let windows: Vec<Owned> = copy_ax_windows_including(app, pid, window_id)
        .into_iter()
        .map(Owned)
        .collect();
    CFRelease(app as CFTypeRef);
    let window = windows
        .into_iter()
        .find(|window| ax_get_window_id(window.0) == Some(window_id))?;
    if !element_screen_rect(window.0).is_some_and(|frame| contains(frame, point)) {
        return None;
    }
    let hit = deepest_at(
        window,
        point,
        |element| copy_children(element.0).into_iter().map(Owned).collect(),
        |element| element_screen_rect(element.0),
    );
    Some(hit.into_raw())
}

#[cfg(test)]
mod tests {
    use super::{deepest_at, window_hit, MAX_FRAMES};

    struct Node {
        name: &'static str,
        frame: Option<[f64; 4]>,
        children: Vec<Node>,
    }

    fn node(name: &'static str, frame: Option<[f64; 4]>, children: Vec<Node>) -> Node {
        Node {
            name,
            frame,
            children,
        }
    }

    fn hit(root: &Node, point: (f64, f64)) -> &'static str {
        deepest_at(
            root,
            point,
            |node| node.children.iter().collect(),
            |node| node.frame,
        )
        .name
    }

    #[test]
    fn the_deepest_element_under_the_point_answers() {
        let window = node(
            "window",
            Some([0.0, 0.0, 800.0, 600.0]),
            vec![
                node("toolbar", Some([0.0, 0.0, 800.0, 80.0]), vec![]),
                node(
                    "page",
                    Some([0.0, 80.0, 800.0, 520.0]),
                    vec![
                        node("select", Some([40.0, 120.0, 120.0, 24.0]), vec![]),
                        node("button", Some([40.0, 200.0, 300.0, 80.0]), vec![]),
                    ],
                ),
            ],
        );
        assert_eq!(hit(&window, (100.0, 130.0)), "select");
        assert_eq!(hit(&window, (100.0, 250.0)), "button");
        // Over the page but no control: the page answers.
        assert_eq!(hit(&window, (600.0, 500.0)), "page");
        assert_eq!(hit(&window, (10.0, 10.0)), "toolbar");
    }

    #[test]
    fn a_later_sibling_is_over_an_earlier_one() {
        let window = node(
            "window",
            Some([0.0, 0.0, 400.0, 400.0]),
            vec![
                node("under", Some([0.0, 0.0, 400.0, 400.0]), vec![]),
                node("over", Some([100.0, 100.0, 100.0, 100.0]), vec![]),
            ],
        );
        assert_eq!(hit(&window, (150.0, 150.0)), "over");
        assert_eq!(hit(&window, (50.0, 50.0)), "under");
    }

    #[test]
    fn a_child_without_a_frame_is_not_stepped_into() {
        let window = node(
            "window",
            Some([0.0, 0.0, 400.0, 400.0]),
            vec![node(
                "unframed",
                None,
                vec![node("inside", Some([0.0, 0.0, 400.0, 400.0]), vec![])],
            )],
        );
        assert_eq!(hit(&window, (10.0, 10.0)), "window");
    }

    #[test]
    fn a_very_wide_container_stops_the_walk_where_it_got() {
        let mut children: Vec<Node> = (0..MAX_FRAMES + 10)
            .map(|_| node("elsewhere", Some([1000.0, 1000.0, 10.0, 10.0]), vec![]))
            .collect();
        children.insert(0, node("first", Some([0.0, 0.0, 10.0, 10.0]), vec![]));
        let window = node("window", Some([0.0, 0.0, 2000.0, 2000.0]), children);
        assert_eq!(hit(&window, (5.0, 5.0)), "window");
    }

    #[test]
    fn another_window_over_the_point_does_not_answer_for_this_one() {
        let mut released = Vec::new();
        // The app's hit test found an element of window 9, over window 7 at
        // the point: window 7's own walk answers.
        let answer = window_hit(
            Some(("tab search button", Some(9))),
            7,
            |element| released.push(element),
            || Some("select"),
        );
        assert_eq!(answer, Some("select"));
        assert_eq!(released, vec!["tab search button"]);
        // An element of the window itself is the answer as it is.
        let answer = window_hit(
            Some(("select", Some(7))),
            7,
            |_| panic!("kept"),
            || panic!("not walked"),
        );
        assert_eq!(answer, Some("select"));
        // One whose window could not be told is not taken as this window's.
        let answer = window_hit(Some(("unknown", None)), 7, |_| {}, || None::<&str>);
        assert_eq!(answer, None);
    }
}
