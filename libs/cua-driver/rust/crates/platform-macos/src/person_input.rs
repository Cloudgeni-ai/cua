//! How long ago a person last used this Mac's keyboard, mouse or trackpad.
//!
//! `HIDIdleTime` and the HID system state's any-event timer cannot tell: an
//! event posted to a process (`SLEventPostToPid`, `CGEventPostToPid`), as this
//! driver's background keys are, resets both. The HID system state keeps a
//! timer per event type as well, and those only move for events that come in
//! through the HID system (the hardware, or input posted at the HID level as
//! foreground delivery does), not for events posted to a process.

/// The input a person makes: buttons, moves and drags of a mouse or
/// trackpad, keys, modifier keys, scrolling, a tablet, and trackpad gestures.
const PERSON_EVENT_TYPES: [u32; 22] = [
    1, 2, 3, 4, 5, 6, 7, // mouse buttons, moves, drags
    10, 11, 12, // keys and modifiers
    22, 23, 24, // scroll wheel, tablet pointer and proximity
    25, 26, 27, // other mouse buttons and drags
    29, 30, 31, 32, 34, // gestures: gesture, magnify, swipe, smart magnify, pressure
    18, // rotate
];

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
}

/// `kCGEventSourceStateHIDSystemState`.
const HID_SYSTEM_STATE: i32 = 1;

/// Seconds since a person last used the keyboard, mouse or trackpad, or
/// `None` when the HID system state does not say. Input this or another
/// driver posts to an app in the background is not counted; input posted in
/// the foreground (through the HID system) is.
pub fn idle_seconds() -> Option<f64> {
    newest(
        PERSON_EVENT_TYPES
            .iter()
            .map(|&kind| unsafe { CGEventSourceSecondsSinceLastEventType(HID_SYSTEM_STATE, kind) }),
    )
}

/// The most recent of `readings`, skipping any that are not a time.
fn newest(readings: impl Iterator<Item = f64>) -> Option<f64> {
    readings
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .min_by(f64::total_cmp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_most_recent_reading_counts_and_non_times_are_skipped() {
        assert_eq!(newest([90.0, 3.5, 12.0].into_iter()), Some(3.5));
        assert_eq!(
            newest([f64::NAN, -1.0, f64::INFINITY, 7.0].into_iter()),
            Some(7.0)
        );
        assert_eq!(newest([f64::NAN].into_iter()), None);
        assert_eq!(newest(std::iter::empty()), None);
    }

    #[test]
    fn every_kind_of_person_input_is_counted_once() {
        let mut kinds = PERSON_EVENT_TYPES.to_vec();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), PERSON_EVENT_TYPES.len());
        for kind in [1, 5, 10, 12, 22] {
            assert!(PERSON_EVENT_TYPES.contains(&kind), "{kind}");
        }
    }
}
