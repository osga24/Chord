//! Switches keyboard input sources for a Caps tap, standing in for macOS's "Use the
//! Caps Lock key to switch to and from ABC", which cannot work while Chord remaps the
//! key (see `caps_remap`).
//!
//! A tap goes from an ASCII-capable source (ABC, U.S., …) to the non-ASCII source used
//! last (Zhuyin, Pinyin, Kotoeri, …) and back. Only public Text Input Sources APIs are
//! used; whether macOS's own setting is on is not read, so Chord has its own setting.
//! Text Input Sources must be used on the main thread.

use anyhow::{Result, ensure};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNotificationCenter, CFNotificationName,
    CFNotificationSuspensionBehavior, CFRetained, CFString, CFType,
};
use parking_lot::Mutex;
use std::ffi::c_void;
use std::ptr::NonNull;

type OSStatus = i32;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentKeyboardInputSource() -> Option<NonNull<CFType>>;
    fn TISCreateInputSourceList(
        properties: &CFDictionary,
        include_all_installed: u8,
    ) -> Option<NonNull<CFArray>>;
    fn TISGetInputSourceProperty(source: &CFType, key: &CFString) -> Option<NonNull<CFType>>;
    fn TISSelectInputSource(source: &CFType) -> OSStatus;

    static kTISPropertyInputSourceCategory: &'static CFString;
    static kTISPropertyInputSourceIsASCIICapable: &'static CFString;
    static kTISPropertyInputSourceIsSelectCapable: &'static CFString;
    static kTISPropertyInputSourceID: &'static CFString;
    static kTISCategoryKeyboardInputSource: &'static CFString;
    static kTISNotifySelectedKeyboardInputSourceChanged: &'static CFString;
}

/// Preferred when no ASCII-capable source has been used yet, as in macOS.
const ABC: &str = "com.apple.keylayout.ABC";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub id: String,
    pub ascii_capable: bool,
}

/// The source last used on each side of the switch.
#[derive(Default)]
struct History {
    ascii: Option<String>,
    other: Option<String>,
}

impl History {
    fn observe(&mut self, source: &Source) {
        let last = if source.ascii_capable {
            &mut self.ascii
        } else {
            &mut self.other
        };
        *last = Some(source.id.clone());
    }

    /// The enabled source a tap switches to from `current`: the other side's last
    /// used source while it is still enabled, otherwise ABC or the first enabled one
    /// of that side. `None` when that side has no enabled source.
    fn target<'a>(&mut self, current: &Source, enabled: &'a [Source]) -> Option<&'a Source> {
        self.observe(current);
        let to_ascii = !current.ascii_capable;
        let last = if to_ascii { &self.ascii } else { &self.other };
        let mut candidates = enabled
            .iter()
            .filter(|source| source.ascii_capable == to_ascii && source.id != current.id);
        last.as_ref()
            .and_then(|id| candidates.clone().find(|source| &source.id == id))
            .or_else(|| {
                to_ascii
                    .then(|| candidates.clone().find(|source| source.id == ABC))
                    .flatten()
            })
            .or_else(|| candidates.next())
    }
}

static HISTORY: Mutex<History> = Mutex::new(History {
    ascii: None,
    other: None,
});

fn property<'a>(source: &'a CFType, key: &CFString) -> Option<&'a CFType> {
    // SAFETY: a live input source; the property follows the Get rule, so it is owned by
    // the source and lives as long as the borrow of it.
    unsafe { TISGetInputSourceProperty(source, key).map(|value| value.as_ref()) }
}

fn describe(source: &CFType) -> Option<Source> {
    // SAFETY: reading framework constants.
    let (id_key, ascii_key) = unsafe {
        (
            kTISPropertyInputSourceID,
            kTISPropertyInputSourceIsASCIICapable,
        )
    };
    Some(Source {
        id: property(source, id_key)?
            .downcast_ref::<CFString>()?
            .to_string(),
        ascii_capable: property(source, ascii_key)
            .and_then(|value| value.downcast_ref::<CFBoolean>())
            .is_some_and(CFBoolean::as_bool),
    })
}

fn current() -> Option<Source> {
    // SAFETY: the Copy rule: the returned reference is owned by the caller.
    let source = unsafe { CFRetained::from_raw(TISCopyCurrentKeyboardInputSource()?) };
    describe(&source)
}

/// Enabled keyboard sources the user can select, in the order of the Input menu.
fn enabled() -> Vec<(Source, CFRetained<CFType>)> {
    // SAFETY: reading framework constants.
    let (category_key, category, select_key) = unsafe {
        (
            kTISPropertyInputSourceCategory,
            kTISCategoryKeyboardInputSource,
            kTISPropertyInputSourceIsSelectCapable,
        )
    };
    let filter = CFDictionary::<CFString, CFType>::from_slices(
        &[category_key, select_key],
        &[category.as_ref(), CFBoolean::new(true).as_ref()],
    );
    // SAFETY: a filter dictionary of documented keys; the Create rule applies.
    let Some(list) = (unsafe { TISCreateInputSourceList(filter.as_opaque(), 0) }) else {
        return vec![];
    };
    // SAFETY: the Create rule: the caller owns the array, which holds input sources.
    let list = unsafe { CFRetained::cast_unchecked::<CFArray<CFType>>(CFRetained::from_raw(list)) };
    list.iter()
        .filter_map(|source| Some((describe(&source)?, source)))
        .collect()
}

/// Switches to the other side's source for a tap. Returns the selected source, or
/// `None` when there is nothing to switch to. Main thread only.
pub fn switch() -> Result<Option<String>> {
    let current = current().ok_or_else(|| anyhow::anyhow!("no current input source"))?;
    let enabled = enabled();
    let sources: Vec<Source> = enabled.iter().map(|(source, _)| source.clone()).collect();
    let Some(target) = HISTORY.lock().target(&current, &sources).cloned() else {
        return Ok(None);
    };
    let (_, reference) = enabled
        .iter()
        .find(|(source, _)| *source == target)
        .expect("target is one of the enabled sources");
    // SAFETY: a live, selectable input source.
    let status = unsafe { TISSelectInputSource(reference) };
    ensure!(
        status == 0,
        "TISSelectInputSource({}) failed: {status}",
        target.id
    );
    Ok(Some(target.id))
}

unsafe extern "C-unwind" fn on_selected_source_changed(
    _: *mut CFNotificationCenter,
    _: *mut c_void,
    _: *const CFNotificationName,
    _: *const c_void,
    _: *const CFDictionary,
) {
    if let Some(source) = current() {
        HISTORY.lock().observe(&source);
    }
}

/// Follows changes made in the Input menu, with shortcuts or by other apps, so a tap
/// returns to the source used last. Main thread only: notifications arrive on the
/// run loop of the thread that observes.
pub fn observe_changes() {
    static OBSERVING: std::sync::Once = std::sync::Once::new();
    OBSERVING.call_once(|| {
        let Some(center) = CFNotificationCenter::distributed_center() else {
            log::warn!("No distributed notification center; Caps may switch to a stale source");
            return;
        };
        if let Some(source) = current() {
            HISTORY.lock().observe(&source);
        }
        // SAFETY: a static callback and observer token that live for the process.
        unsafe {
            center.add_observer(
                (&raw const HISTORY).cast(),
                Some(on_selected_source_changed),
                Some(kTISNotifySelectedKeyboardInputSourceChanged),
                std::ptr::null(),
                CFNotificationSuspensionBehavior::DeliverImmediately,
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str, ascii_capable: bool) -> Source {
        Source {
            id: id.into(),
            ascii_capable,
        }
    }

    fn switch(history: &mut History, current: &Source, enabled: &[Source]) -> Option<String> {
        history
            .target(current, enabled)
            .map(|source| source.id.clone())
    }

    #[test]
    fn taps_alternate_between_the_last_used_source_of_each_side() {
        let abc = source(ABC, true);
        let us = source("com.apple.keylayout.US", true);
        let zhuyin = source("com.apple.inputmethod.TCIM.Zhuyin", false);
        let pinyin = source("com.apple.inputmethod.SCIM.ITABC", false);
        let enabled = [us.clone(), abc.clone(), zhuyin.clone(), pinyin.clone()];
        let mut history = History::default();
        // Nothing used yet: the first enabled non-ASCII source.
        assert_eq!(switch(&mut history, &us, &enabled), Some(zhuyin.id.clone()));
        assert_eq!(switch(&mut history, &zhuyin, &enabled), Some(us.id.clone()));
        // Pinyin chosen from the Input menu becomes the one a tap returns to.
        history.observe(&pinyin);
        history.observe(&abc);
        assert_eq!(
            switch(&mut history, &abc, &enabled),
            Some(pinyin.id.clone())
        );
        assert_eq!(
            switch(&mut history, &pinyin, &enabled),
            Some(abc.id.clone())
        );
    }

    #[test]
    fn prefers_abc_and_skips_sources_that_are_no_longer_enabled() {
        let abc = source(ABC, true);
        let us = source("com.apple.keylayout.US", true);
        let zhuyin = source("com.apple.inputmethod.TCIM.Zhuyin", false);
        let cangjie = source("com.apple.inputmethod.TCIM.Cangjie", false);
        let mut history = History::default();
        assert_eq!(
            switch(
                &mut history,
                &zhuyin,
                &[us.clone(), abc.clone(), zhuyin.clone()]
            ),
            Some(ABC.into())
        );
        // Zhuyin was removed from the Input menu meanwhile.
        assert_eq!(
            switch(&mut history, &abc, &[abc.clone(), cangjie.clone()]),
            Some(cangjie.id.clone())
        );
    }

    #[test]
    fn nothing_to_switch_to_without_a_source_on_the_other_side() {
        let abc = source(ABC, true);
        let us = source("com.apple.keylayout.US", true);
        let mut history = History::default();
        assert_eq!(switch(&mut history, &abc, &[abc.clone(), us]), None);
        assert_eq!(switch(&mut history, &abc, &[]), None);
    }

    #[test]
    #[ignore = "reads the real input sources; requires an active macOS desktop"]
    fn reads_the_current_and_enabled_sources_without_changing_them() {
        let current = current().expect("a current keyboard input source");
        let enabled = enabled();
        assert!(!enabled.is_empty());
        println!("current: {current:?}");
        for (source, _) in enabled {
            println!("enabled: {source:?}");
        }
    }

    #[test]
    #[ignore = "switches this user's input source twice; requires two enabled sources"]
    fn switching_takes_effect_at_once_and_two_switches_return() {
        let original = current().expect("a current keyboard input source");
        let selected = super::switch().unwrap().expect("a source to switch to");
        assert_eq!(
            current().unwrap().id,
            selected,
            "the next tap sees the switch"
        );
        assert_ne!(selected, original.id);
        let back = super::switch()
            .unwrap()
            .expect("a source to switch back to");
        assert_eq!(back, original.id);
        assert_eq!(current().unwrap(), original);
    }
}
