//! One accessibility request per element for the tree walk.
//!
//! Every AX attribute read is a round trip to the application, answered on
//! its main thread. The walk used to make one per attribute: about sixteen
//! for an actionable element (role, title, value, placeholder, description,
//! identifier, help, enabled, position, size, value description, minimum,
//! maximum, selected, children, then the actions). On an app that answers
//! slowly, such as the iOS Simulator relaying every request to the simulated
//! device, or any app on a heavily loaded Mac, the round trips are what the
//! walk's time budget is spent on, so a read stopped after the first few
//! dozen elements. [`NodeAttributes::read`] asks for all of them in one
//! `AXUIElementCopyMultipleAttributeValues` request; the walk adds one
//! `AXUIElementCopyActionNames` request for elements it renders.

use super::bindings::*;
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFGetTypeID, CFRelease, CFRetain, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use std::os::raw::c_void;

/// The attributes [`NodeAttributes::read`] asks for, in request order.
const NAMES: [&str; 15] = [
    "AXRole",
    "AXTitle",
    "AXValue",
    "AXPlaceholderValue",
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
const ROLE: usize = 0;
const TITLE: usize = 1;
const VALUE: usize = 2;
const PLACEHOLDER: usize = 3;
const DESCRIPTION: usize = 4;
const IDENTIFIER: usize = 5;
const HELP: usize = 6;
const ENABLED: usize = 7;
const SELECTED: usize = 8;
const POSITION: usize = 9;
const SIZE: usize = 10;
const VALUE_DESCRIPTION: usize = 11;
const MIN_VALUE: usize = 12;
const MAX_VALUE: usize = 13;
const CHILDREN: usize = 14;

thread_local! {
    static REQUEST: CFArray<CFString> =
        CFArray::from_CFTypes(&NAMES.map(CFString::new));
}

/// What the walk reads of one element. Each field is `None` when the element
/// does not answer that attribute or answers it with an unexpected type,
/// exactly as the single-attribute readers in [`super::bindings`] report it.
#[derive(Debug, Default)]
pub struct NodeAttributes {
    pub role: Option<String>,
    pub title: Option<String>,
    pub value: Option<StringishAttrValue>,
    pub placeholder: Option<String>,
    pub description: Option<String>,
    pub identifier: Option<String>,
    pub help: Option<String>,
    pub enabled: Option<bool>,
    pub selected: Option<bool>,
    /// `[x, y, width, height]` in screen points, `None` unless both position
    /// and size are readable and the size is at least one point each way
    /// (as [`element_screen_rect`]).
    pub frame: Option<[f64; 4]>,
    pub value_description: Option<String>,
    pub min_value: Option<f64>,
    pub max_value: Option<f64>,
    /// The application did not answer AXChildren in time, so the element's
    /// children are unknown rather than none.
    pub children_timed_out: bool,
    /// Retained; released when this is dropped unless taken.
    children: Vec<AXUIElementRef>,
}

/// Why an element's attributes could not be read at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadFailure {
    /// The application did not answer within the element's messaging
    /// timeout.
    TimedOut,
    /// The element is gone, or the application refused the request.
    Unreadable,
}

impl NodeAttributes {
    /// Read every attribute the walk uses in one request. An application
    /// that does not support the multiple-attribute request (it answers with
    /// an error other than a timeout or a dead element) is read one
    /// attribute at a time instead, so no app loses rows it showed before.
    ///
    /// # Safety
    ///
    /// `element` must be a valid, live `AXUIElementRef`.
    pub unsafe fn read(element: AXUIElementRef) -> Result<Self, ReadFailure> {
        let mut values: CFArrayRef = std::ptr::null();
        let error = REQUEST.with(|request| {
            AXUIElementCopyMultipleAttributeValues(
                element,
                request.as_concrete_TypeRef(),
                0,
                &mut values,
            )
        });
        if error == kAXErrorSuccess && !values.is_null() {
            let values = CFArray::<*const c_void>::wrap_under_create_rule(values);
            let entries: Vec<CFTypeRef> = (0..NAMES.len())
                .map(|index| {
                    values
                        .get(index as isize)
                        .map_or(std::ptr::null(), |entry| *entry as CFTypeRef)
                })
                .collect();
            return Ok(Self::from_values(&entries));
        }
        if !values.is_null() {
            CFRelease(values as CFTypeRef);
        }
        if error == kAXErrorCannotComplete {
            Err(ReadFailure::TimedOut)
        } else if error == kAXErrorInvalidUIElement {
            Err(ReadFailure::Unreadable)
        } else {
            Ok(Self::read_one_by_one(element))
        }
    }

    /// The attributes from the values of one multiple-attribute request, in
    /// [`NAMES`] order. Entries are borrowed; children are retained.
    ///
    /// # Safety
    ///
    /// Every non-null entry must be a valid CF object.
    unsafe fn from_values(values: &[CFTypeRef]) -> Self {
        let entry = |index: usize| {
            values
                .get(index)
                .copied()
                .filter(|value| !value.is_null() && !is_error_value(*value))
        };
        let string = |index| entry(index).and_then(|value| string_value(value));
        let number = |index| entry(index).and_then(|value| number_value(value));
        let boolean = |index| entry(index).and_then(|value| bool_value(value));
        let frame = match (
            entry(POSITION).and_then(|value| point_value(value)),
            entry(SIZE).and_then(|value| size_value(value)),
        ) {
            (Some((x, y)), Some((width, height))) if width >= 1.0 && height >= 1.0 => {
                Some([x, y, width, height])
            }
            _ => None,
        };
        Self {
            role: string(ROLE),
            title: string(TITLE),
            value: entry(VALUE).and_then(|value| coerce_stringish_value(value)),
            placeholder: string(PLACEHOLDER),
            description: string(DESCRIPTION),
            identifier: string(IDENTIFIER),
            help: string(HELP),
            enabled: boolean(ENABLED),
            selected: boolean(SELECTED),
            frame,
            value_description: string(VALUE_DESCRIPTION),
            min_value: number(MIN_VALUE),
            max_value: number(MAX_VALUE),
            children_timed_out: values
                .get(CHILDREN)
                .is_some_and(|value| ax_error_value(*value) == Some(kAXErrorCannotComplete)),
            children: entry(CHILDREN).map_or_else(Vec::new, |value| element_array(value)),
        }
    }

    /// The same attributes, one request each (the walk's former reads).
    unsafe fn read_one_by_one(element: AXUIElementRef) -> Self {
        let (children, children_timed_out) = copy_children_or_timeout(element);
        Self {
            role: copy_string_attr(element, "AXRole"),
            title: copy_string_attr(element, "AXTitle"),
            value: copy_stringish_attr(element, "AXValue"),
            placeholder: copy_string_attr(element, "AXPlaceholderValue"),
            description: copy_string_attr(element, "AXDescription"),
            identifier: copy_string_attr(element, "AXIdentifier"),
            help: copy_string_attr(element, "AXHelp"),
            enabled: copy_bool_attr(element, "AXEnabled"),
            selected: copy_bool_attr(element, "AXSelected"),
            frame: element_screen_rect(element),
            value_description: copy_string_attr(element, "AXValueDescription"),
            min_value: copy_number_attr(element, "AXMinValue"),
            max_value: copy_number_attr(element, "AXMaxValue"),
            children_timed_out,
            children,
        }
    }

    /// The element's children, retained; the caller releases each.
    pub fn take_children(&mut self) -> Vec<AXUIElementRef> {
        std::mem::take(&mut self.children)
    }
}

impl Drop for NodeAttributes {
    fn drop(&mut self) {
        for child in self.children.drain(..) {
            // SAFETY: each child was retained when it was read.
            unsafe { CFRelease(child as CFTypeRef) };
        }
    }
}

unsafe fn is_error_value(value: CFTypeRef) -> bool {
    CFGetTypeID(value) == AXValueGetTypeID()
        && AXValueGetType(value as AXValueRef) == kAXValueAXErrorType
}

/// The error an entry of a multiple-attribute answer stands for, if it is one.
unsafe fn ax_error_value(value: CFTypeRef) -> Option<AXError> {
    if value.is_null() || !is_error_value(value) {
        return None;
    }
    let mut error: AXError = 0;
    AXValueGetValue(
        value as AXValueRef,
        kAXValueAXErrorType,
        &mut error as *mut AXError as *mut c_void,
    )
    .then_some(error)
}

/// The element's children, each retained, and whether the application did
/// not answer in time.
unsafe fn copy_children_or_timeout(element: AXUIElementRef) -> (Vec<AXUIElementRef>, bool) {
    let name = CFString::new("AXChildren");
    let mut value: CFTypeRef = std::ptr::null();
    let error = AXUIElementCopyAttributeValue(element, name.as_concrete_TypeRef(), &mut value);
    if error != kAXErrorSuccess || value.is_null() {
        if !value.is_null() {
            CFRelease(value);
        }
        return (Vec::new(), error == kAXErrorCannotComplete);
    }
    let children = element_array(value);
    CFRelease(value);
    (children, false)
}

unsafe fn string_value(value: CFTypeRef) -> Option<String> {
    (CFGetTypeID(value) == CFString::type_id())
        .then(|| CFString::wrap_under_get_rule(value as _).to_string())
}

unsafe fn number_value(value: CFTypeRef) -> Option<f64> {
    (CFGetTypeID(value) == CFNumber::type_id())
        .then(|| CFNumber::wrap_under_get_rule(value as _).to_f64())
        .flatten()
}

/// CFBoolean, or a CFNumber read as nonzero (as [`copy_bool_attr`]).
unsafe fn bool_value(value: CFTypeRef) -> Option<bool> {
    let type_id = CFGetTypeID(value);
    if type_id == CFBoolean::type_id() {
        return Some(CFBoolean::wrap_under_get_rule(value as _).into());
    }
    if type_id == CFNumber::type_id() {
        return CFNumber::wrap_under_get_rule(value as _)
            .to_f64()
            .map(|number| number != 0.0);
    }
    None
}

#[repr(C)]
struct Pair {
    first: f64,
    second: f64,
}

unsafe fn pair_value(value: CFTypeRef, kind: AXValueType) -> Option<(f64, f64)> {
    if CFGetTypeID(value) != AXValueGetTypeID() {
        return None;
    }
    let mut pair = Pair {
        first: 0.0,
        second: 0.0,
    };
    AXValueGetValue(
        value as AXValueRef,
        kind,
        &mut pair as *mut Pair as *mut c_void,
    )
    .then_some((pair.first, pair.second))
}

unsafe fn point_value(value: CFTypeRef) -> Option<(f64, f64)> {
    pair_value(value, kAXValueCGPointType)
}

unsafe fn size_value(value: CFTypeRef) -> Option<(f64, f64)> {
    pair_value(value, kAXValueCGSizeType)
}

/// The accessibility elements of a CFArray, each retained.
unsafe fn element_array(value: CFTypeRef) -> Vec<AXUIElementRef> {
    if CFGetTypeID(value) != CFArray::<*const c_void>::type_id() {
        return Vec::new();
    }
    let array = CFArray::<*const c_void>::wrap_under_get_rule(value as _);
    let element_type = AXUIElementGetTypeID();
    (0..array.len())
        .filter_map(|index| {
            let item = *array.get(index)? as CFTypeRef;
            (CFGetTypeID(item) == element_type).then(|| {
                CFRetain(item);
                item as AXUIElementRef
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_foundation::base::CFType;

    fn ax_value<T>(kind: AXValueType, value: &T) -> CFType {
        unsafe {
            CFType::wrap_under_create_rule(
                AXValueCreate(kind, value as *const T as *const c_void) as CFTypeRef
            )
        }
    }

    #[test]
    fn a_multiple_attribute_answer_reads_like_the_single_readers() {
        let error = ax_value(kAXValueAXErrorType, &kAXErrorNoValue);
        let role = CFString::new("AXButton");
        let title = CFString::new("Save");
        let number = CFNumber::from(8.0);
        let enabled = CFBoolean::true_value();
        let selected = CFNumber::from(0);
        let position = ax_value(
            kAXValueCGPointType,
            &Pair {
                first: 10.0,
                second: 20.0,
            },
        );
        let size = ax_value(
            kAXValueCGSizeType,
            &Pair {
                first: 30.0,
                second: 40.0,
            },
        );
        let mut values: Vec<CFTypeRef> = vec![error.as_CFTypeRef(); NAMES.len()];
        values[ROLE] = role.as_CFTypeRef();
        values[TITLE] = title.as_CFTypeRef();
        values[VALUE] = number.as_CFTypeRef();
        values[ENABLED] = enabled.as_CFTypeRef();
        values[SELECTED] = selected.as_CFTypeRef();
        values[POSITION] = position.as_CFTypeRef();
        values[SIZE] = size.as_CFTypeRef();
        // A title where a number belongs is not a number.
        values[MIN_VALUE] = title.as_CFTypeRef();
        let read = unsafe { NodeAttributes::from_values(&values) };
        assert_eq!(read.role.as_deref(), Some("AXButton"));
        assert_eq!(read.title.as_deref(), Some("Save"));
        let value = read.value.as_ref().unwrap();
        assert_eq!(value.string_value, None);
        assert_eq!(value.state_value, "8");
        assert_eq!(read.description, None);
        assert_eq!(read.placeholder, None);
        assert_eq!(read.enabled, Some(true));
        assert_eq!(read.selected, Some(false));
        assert_eq!(read.frame, Some([10.0, 20.0, 30.0, 40.0]));
        assert_eq!(read.min_value, None);
        assert!(read.children.is_empty());
    }

    #[test]
    fn a_frame_needs_a_position_and_a_size_of_at_least_a_point() {
        let error = ax_value(kAXValueAXErrorType, &kAXErrorNoValue);
        let position = ax_value(
            kAXValueCGPointType,
            &Pair {
                first: 1.0,
                second: 2.0,
            },
        );
        let flat = ax_value(
            kAXValueCGSizeType,
            &Pair {
                first: 30.0,
                second: 0.5,
            },
        );
        let mut values: Vec<CFTypeRef> = vec![error.as_CFTypeRef(); NAMES.len()];
        values[POSITION] = position.as_CFTypeRef();
        assert_eq!(unsafe { NodeAttributes::from_values(&values) }.frame, None);
        values[SIZE] = flat.as_CFTypeRef();
        assert_eq!(unsafe { NodeAttributes::from_values(&values) }.frame, None);
    }

    #[test]
    fn a_short_or_null_answer_reads_as_missing_attributes() {
        let role = CFString::new("AXGroup");
        let read = unsafe { NodeAttributes::from_values(&[role.as_CFTypeRef(), std::ptr::null()]) };
        assert_eq!(read.role.as_deref(), Some("AXGroup"));
        assert_eq!(read.title, None);
        assert!(read.children.is_empty());
        assert!(!read.children_timed_out);
    }

    #[test]
    fn children_the_app_did_not_answer_in_time_are_unknown_not_none() {
        let no_value = ax_value(kAXValueAXErrorType, &kAXErrorNoValue);
        let timed_out = ax_value(kAXValueAXErrorType, &kAXErrorCannotComplete);
        let mut values: Vec<CFTypeRef> = vec![no_value.as_CFTypeRef(); NAMES.len()];
        let read = unsafe { NodeAttributes::from_values(&values) };
        assert!(read.children.is_empty());
        assert!(!read.children_timed_out, "an element without children");
        values[CHILDREN] = timed_out.as_CFTypeRef();
        let read = unsafe { NodeAttributes::from_values(&values) };
        assert!(read.children.is_empty());
        assert!(read.children_timed_out);
    }
}
