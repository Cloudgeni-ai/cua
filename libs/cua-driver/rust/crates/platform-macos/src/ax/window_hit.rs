//! The element under a screen point in one window.
//!
//! `AXUIElementCopyElementAtPosition` answers for an application: it returns
//! the element of whichever of the app's windows is on top at the point.
//! Asked of a window element it fails (`kAXErrorIllegalArgument`), so a
//! window behind another window of the same app has no hit test of its own.
//! For such a window the answer is found in its own tree: the deepest
//! element whose frame contains the point, preferring a later sibling
//! (drawn over an earlier one) where siblings overlap.
//!
//! The tree gives no paint order, so the later sibling is the guess. A web
//! page can draw an earlier element over a later one (a `position: fixed`
//! overlay before a `<select>`, say): the walk then answers with the later
//! one although the click would land on the overlay. Callers that refuse a
//! click by its answer refuse it there too, and their refusal names a way
//! that does not depend on the point (the element's own ref).
//!
//! A walk that stops at its bounds before it found the deepest element says
//! so ([`WindowHit::Unknown`]) instead of answering with where it stopped:
//! the element under the point may be one it did not read.

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

/// What answers for a point in a window.
#[derive(Debug, PartialEq, Eq)]
pub enum WindowHit<E> {
    /// The element under the point.
    Element(E),
    /// The walk stopped at its bounds at this element, which contains the
    /// point; the element under it may be one below that was not read.
    Unknown(E),
}

impl<E> WindowHit<E> {
    /// The element the answer carries, whether or not it is the deepest.
    pub fn into_element(self) -> E {
        match self {
            WindowHit::Element(element) | WindowHit::Unknown(element) => element,
        }
    }
}

/// From `root`, which contains `point`, the deepest node reached by stepping
/// into a child whose frame contains it, the last such child of each node
/// first. A child without a frame is not stepped into: nothing says it is
/// under the point. The walk reads at most [`MAX_FRAMES`] frames and steps
/// [`MAX_DEPTH`] times; where it stops at either bound with children of the
/// node it reached left unread, the answer is [`WindowHit::Unknown`].
pub(crate) fn deepest_at<N>(
    root: N,
    point: (f64, f64),
    mut children: impl FnMut(&N) -> Vec<N>,
    mut frame: impl FnMut(&N) -> Option<[f64; 4]>,
) -> WindowHit<N> {
    let mut current = root;
    let mut frames = 0;
    for _ in 0..MAX_DEPTH {
        let mut next = None;
        let mut unread = false;
        for child in children(&current).into_iter().rev() {
            if frames == MAX_FRAMES {
                unread = true;
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
            None if unread => return WindowHit::Unknown(current),
            None => return WindowHit::Element(current),
        }
    }
    if children(&current).is_empty() {
        WindowHit::Element(current)
    } else {
        WindowHit::Unknown(current)
    }
}

/// Which element answers for `window_id` at a point: the application's own
/// hit test when it found an element of that window, else the walk of the
/// window's own tree (`in_window`), since the app's answer then belongs to
/// another window over this one.
pub(crate) fn window_hit<E>(
    app_hit: Option<(E, Option<u32>)>,
    window_id: u32,
    release: impl FnOnce(E),
    in_window: impl FnOnce() -> Option<WindowHit<E>>,
) -> Option<WindowHit<E>> {
    match app_hit {
        Some((element, Some(owner))) if owner == window_id => Some(WindowHit::Element(element)),
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
/// `None` when the window is not found or does not contain the point;
/// [`WindowHit::Unknown`] when its tree was too large to read down to the
/// point.
///
/// # Safety
///
/// The caller must `CFRelease` the returned element.
pub unsafe fn element_at_point_in_window(
    pid: i32,
    window_id: u32,
    x: f64,
    y: f64,
) -> Option<WindowHit<AXUIElementRef>> {
    let app_hit = element_at_screen_position(pid, x, y)
        .map(|element| (element, super::exact_target::element_window_id(element)));
    window_hit(
        app_hit,
        window_id,
        |element| CFRelease(element as CFTypeRef),
        || walk_window(pid, window_id, (x, y)),
    )
}

unsafe fn walk_window(
    pid: i32,
    window_id: u32,
    point: (f64, f64),
) -> Option<WindowHit<AXUIElementRef>> {
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
    Some(match hit {
        WindowHit::Element(element) => WindowHit::Element(element.into_raw()),
        WindowHit::Unknown(element) => WindowHit::Unknown(element.into_raw()),
    })
}

#[cfg(test)]
mod tests {
    use super::{deepest_at, window_hit, WindowHit, MAX_DEPTH, MAX_FRAMES};

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

    fn walk(root: &Node, point: (f64, f64)) -> WindowHit<&'static str> {
        match deepest_at(
            root,
            point,
            |node| node.children.iter().collect(),
            |node| node.frame,
        ) {
            WindowHit::Element(node) => WindowHit::Element(node.name),
            WindowHit::Unknown(node) => WindowHit::Unknown(node.name),
        }
    }

    fn hit(root: &Node, point: (f64, f64)) -> &'static str {
        match walk(root, point) {
            WindowHit::Element(name) => name,
            WindowHit::Unknown(name) => panic!("walk stopped at {name}"),
        }
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

    /// The tree gives no paint order: an element drawn over a later sibling
    /// (a fixed overlay earlier in the page) loses to it. Kept as the known
    /// limit; callers that refuse by the answer name the element's ref.
    #[test]
    fn an_overlay_earlier_in_the_tree_loses_to_a_later_sibling() {
        let window = node(
            "window",
            Some([0.0, 0.0, 400.0, 400.0]),
            vec![
                node("fixed overlay", Some([0.0, 0.0, 400.0, 60.0]), vec![]),
                node("select", Some([20.0, 20.0, 120.0, 24.0]), vec![]),
            ],
        );
        assert_eq!(hit(&window, (30.0, 30.0)), "select");
        assert_eq!(hit(&window, (300.0, 30.0)), "fixed overlay");
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
    fn a_very_wide_container_is_not_taken_for_the_element_under_the_point() {
        let mut children: Vec<Node> = (0..MAX_FRAMES + 10)
            .map(|_| node("elsewhere", Some([1000.0, 1000.0, 10.0, 10.0]), vec![]))
            .collect();
        children.insert(0, node("first", Some([0.0, 0.0, 10.0, 10.0]), vec![]));
        let window = node("window", Some([0.0, 0.0, 2000.0, 2000.0]), children);
        // `first` was never read: the walk cannot tell what is at the point.
        assert_eq!(walk(&window, (5.0, 5.0)), WindowHit::Unknown("window"));
        // Exactly as many children as frames it reads: all were read.
        let children: Vec<Node> = (0..MAX_FRAMES)
            .map(|_| node("elsewhere", Some([1000.0, 1000.0, 10.0, 10.0]), vec![]))
            .collect();
        let window = node("window", Some([0.0, 0.0, 2000.0, 2000.0]), children);
        assert_eq!(walk(&window, (5.0, 5.0)), WindowHit::Element("window"));
    }

    #[test]
    fn a_tree_deeper_than_the_walk_is_not_taken_for_its_end() {
        fn chain(depth: usize) -> Node {
            let mut current = node("leaf", Some([0.0, 0.0, 10.0, 10.0]), vec![]);
            for _ in 0..depth {
                current = node("level", Some([0.0, 0.0, 10.0, 10.0]), vec![current]);
            }
            current
        }
        assert_eq!(
            walk(&chain(MAX_DEPTH), (5.0, 5.0)),
            WindowHit::Element("leaf")
        );
        assert_eq!(
            walk(&chain(MAX_DEPTH + 1), (5.0, 5.0)),
            WindowHit::Unknown("level")
        );
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
            || Some(WindowHit::Element("select")),
        );
        assert_eq!(answer, Some(WindowHit::Element("select")));
        assert_eq!(released, vec!["tab search button"]);
        // An element of the window itself is the answer as it is.
        let answer = window_hit(
            Some(("select", Some(7))),
            7,
            |_| panic!("kept"),
            || panic!("not walked"),
        );
        assert_eq!(answer, Some(WindowHit::Element("select")));
        // One whose window could not be told is not taken as this window's.
        let answer = window_hit(
            Some(("unknown", None)),
            7,
            |_| {},
            || None::<WindowHit<&str>>,
        );
        assert_eq!(answer, None);
    }
}
