//! Keyboard layout switching through Text Input Sources.
//!
//! macOS does not run its input-source shortcuts for posted events. The server
//! reads those shortcuts from the user's settings (System Settings, Keyboard
//! Shortcuts, Input Sources) and switches the layout itself when the client
//! sends one of them.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::{CFArray, CFBoolean, CFRetained, CFString, CFType};
use objc2_core_graphics::CGEventFlags;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSUserDefaults};
use tracing::{info, warn};

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    static kTISPropertyInputSourceID: &'static CFString;
    static kTISPropertyInputSourceCategory: &'static CFString;
    static kTISPropertyInputSourceIsSelectCapable: &'static CFString;
    static kTISCategoryKeyboardInputSource: &'static CFString;

    fn TISCreateInputSourceList(properties: *const c_void, include_all: u8) -> *mut CFArray;
    fn TISCopyCurrentKeyboardInputSource() -> *mut CFType;
    fn TISGetInputSourceProperty(source: &CFType, key: &CFString) -> *const CFType;
    fn TISSelectInputSource(source: &CFType) -> i32;
}

/// Symbolic hotkey ids: "Select the previous input source" and "Select next source in Input menu".
const PREVIOUS_SOURCE: &str = "60";
const NEXT_SOURCE: &str = "61";

/// The modifiers a symbolic hotkey stores; they match `CGEventFlags` bit for bit.
fn hotkey_modifiers() -> CGEventFlags {
    CGEventFlags::MaskShift
        | CGEventFlags::MaskControl
        | CGEventFlags::MaskAlternate
        | CGEventFlags::MaskCommand
}

/// Switches the layout when `key` with `modifiers` is one of the user's enabled
/// input-source shortcuts. Returns whether it was.
pub fn handle_shortcut(key: u16, modifiers: CGEventFlags) -> bool {
    let modifiers = modifiers & hotkey_modifiers();
    if modifiers.is_empty() {
        return false;
    }
    for (id, forward) in [(PREVIOUS_SOURCE, false), (NEXT_SOURCE, true)] {
        if shortcut(id).is_some_and(|(k, m)| k == key && m == modifiers) {
            select(forward);
            return true;
        }
    }
    false
}

/// An enabled symbolic hotkey as (virtual key code, modifiers).
fn shortcut(id: &str) -> Option<(u16, CGEventFlags)> {
    let domain = NSUserDefaults::standardUserDefaults()
        .persistentDomainForName(&NSString::from_str("com.apple.symbolichotkeys"))?;
    let hotkeys = dict(domain.objectForKey(&NSString::from_str("AppleSymbolicHotKeys"))?)?;
    let hotkey = dict(hotkeys.objectForKey(&NSString::from_str(id))?)?;
    if !number(hotkey.objectForKey(&NSString::from_str("enabled"))?)?.boolValue() {
        return None;
    }
    let value = dict(hotkey.objectForKey(&NSString::from_str("value"))?)?;
    let parameters = value
        .objectForKey(&NSString::from_str("parameters"))?
        .downcast::<NSArray>()
        .ok()?;
    // (character, virtual key code, modifier flags)
    let key = number(parameters.objectAtIndex(1))?.unsignedShortValue();
    let flags = number(parameters.objectAtIndex(2))?.unsignedLongLongValue();
    Some((key, CGEventFlags(flags) & hotkey_modifiers()))
}

fn dict(object: Retained<AnyObject>) -> Option<Retained<NSDictionary<NSString, AnyObject>>> {
    let dict = object.downcast::<NSDictionary>().ok()?;
    // SAFETY: property-list dictionaries have string keys.
    Some(unsafe { Retained::cast_unchecked(dict) })
}

fn number(object: Retained<AnyObject>) -> Option<Retained<NSNumber>> {
    object.downcast::<NSNumber>().ok()
}

/// Selects the enabled keyboard input source after, or before, the current one.
pub fn select(forward: bool) {
    let sources = keyboard_sources();
    if sources.len() < 2 {
        return;
    }
    let current = current_id();
    let count = sources.len();
    let step = if forward { 1 } else { count - 1 };
    let index = sources
        .iter()
        .position(|source| source_id(source) == current)
        .map_or(0, |index| (index + step) % count);
    let next = &sources[index];
    // SAFETY: `next` is an input source from TISCreateInputSourceList.
    let status = unsafe { TISSelectInputSource(next) };
    if status == 0 {
        info!(from = ?current, to = ?source_id(next), "input source switched");
    } else {
        warn!(status, "cannot switch the input source");
    }
}

/// Enabled keyboard layouts and input methods the user can select.
fn keyboard_sources() -> Vec<CFRetained<CFType>> {
    // SAFETY: a null filter lists the enabled sources; the array is ours.
    let Some(list) = NonNull::new(unsafe { TISCreateInputSourceList(std::ptr::null(), 0) }) else {
        return Vec::new();
    };
    // SAFETY: created above, so owned.
    let list = unsafe { CFRetained::from_raw(list) };
    // SAFETY: TIS input source arrays hold CF objects.
    let list = unsafe { list.cast_unchecked::<CFType>() };
    list.iter()
        .filter(|source| {
            // SAFETY: valid sources and property keys.
            let category = unsafe { property(source, kTISPropertyInputSourceCategory) };
            let selectable = unsafe { property(source, kTISPropertyInputSourceIsSelectCapable) };
            category.is_some_and(|c| c == unsafe { kTISCategoryKeyboardInputSource }.as_ref())
                && selectable
                    .and_then(|s| s.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool))
                    .unwrap_or(false)
        })
        .collect()
}

fn current_id() -> Option<String> {
    // SAFETY: the copy rule applies, so the source is ours.
    let source = NonNull::new(unsafe { TISCopyCurrentKeyboardInputSource() })?;
    let source = unsafe { CFRetained::from_raw(source) };
    source_id(&source)
}

fn source_id(source: &CFType) -> Option<String> {
    // SAFETY: valid source and property key.
    let id = unsafe { property(source, kTISPropertyInputSourceID) }?;
    id.downcast_ref::<CFString>().map(ToString::to_string)
}

/// # Safety
/// `source` is a TIS input source; the result lives as long as it does.
unsafe fn property<'a>(source: &'a CFType, key: &CFString) -> Option<&'a CFType> {
    unsafe { TISGetInputSourceProperty(source, key).as_ref() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prints this Mac's input-source shortcuts; run by hand.
    #[test]
    #[ignore]
    fn reads_shortcuts() {
        println!("previous {:?}", shortcut(PREVIOUS_SOURCE));
        println!("next {:?}", shortcut(NEXT_SOURCE));
    }

    /// Switches the layout on this Mac and back; run by hand.
    #[test]
    #[ignore]
    fn switches_and_back() {
        let before = current_id();
        let count = keyboard_sources().len();
        println!("current {before:?}, {count} sources");
        select(true);
        let after = current_id();
        println!("after {after:?}");
        select(false);
        assert_ne!(before, after);
        assert_eq!(current_id(), before);
    }
}
