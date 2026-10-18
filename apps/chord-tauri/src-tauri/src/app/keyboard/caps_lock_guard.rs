//! Caps Lock behaviour while Chord is its only writer.
//!
//! The physical key is remapped (see `caps_remap`), so macOS never toggles Caps Lock
//! on its own and the state read when the key goes down is exactly the state before
//! the press. Chord decides every change:
//! - A plain tap (no Space while held) toggles Caps Lock. Turning it off happens at
//!   key-down, like macOS; turning it on waits for release, because at key-down Chord
//!   cannot know whether Space will follow and make the press an activation.
//! - With input source switching on, a plain tap outside Chord switches input sources
//!   instead, and holding the key for `LONG_PRESS` turns Caps Lock on, like macOS's
//!   "Use the Caps Lock key to switch to and from ABC". Both wait for release for the
//!   same reason. A tap that turns Caps Lock off does only that.
//! - Normal mode keeps Caps Lock off, and Chord's exit restores the state from before
//!   it started. Toggles made while typing in insert mode are not kept.

use parking_lot::Mutex;
use std::time::{Duration, Instant};

/// How long the key must be held to turn Caps Lock on when taps switch input sources.
pub const LONG_PRESS: Duration = Duration::from_millis(500);

/// What a Caps press may do on release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapsTap {
    ToggleCapsLock,
    SwitchInputSource,
}

/// A change to make when the Caps key is released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Release {
    SetCapsLock(bool),
    SwitchInputSource,
}

/// A press that may still be a plain tap.
struct Press {
    /// The Caps Lock state before the press.
    caps_lock: bool,
    at: Instant,
    action: CapsTap,
}

#[derive(Default)]
struct State {
    press: Option<Press>,
    /// The state from before Chord started, restored when Chord ends.
    saved: Option<bool>,
}

#[derive(Default)]
pub struct CapsLockGuard(Mutex<State>);

impl CapsLockGuard {
    /// Caps went down at `at`. `current` is the lock state now; returns the state to
    /// apply.
    pub fn pressed(
        &self,
        current: bool,
        in_normal_mode: bool,
        action: CapsTap,
        at: Instant,
    ) -> Option<bool> {
        let mut state = self.0.lock();
        if in_normal_mode {
            // Caps runs the chord in normal mode; it never toggles.
            state.press = None;
            return None;
        }
        state.press = Some(Press {
            caps_lock: current,
            at,
            action,
        });
        current.then_some(false)
    }

    /// Caps+Space started normal mode, from idle or from insert mode. The press was
    /// an activation, not a tap; returns the state to apply (always off).
    pub fn normal_mode_started(&self, current: bool) -> bool {
        let mut state = self.0.lock();
        let before = state.press.take().map_or(current, |press| press.caps_lock);
        // Returning from insert mode keeps the state from before Chord started.
        state.saved.get_or_insert(before);
        false
    }

    /// Caps went up at `at`. Returns the change to make when the press was a plain tap.
    pub fn released(&self, at: Instant) -> Option<Release> {
        let press = self.0.lock().press.take()?;
        if press.caps_lock {
            // Turning off already happened at key-down.
            return None;
        }
        let held = at.saturating_duration_since(press.at);
        Some(match press.action {
            CapsTap::SwitchInputSource if held < LONG_PRESS => Release::SwitchInputSource,
            _ => Release::SetCapsLock(true),
        })
    }

    /// Chord ended, or input ownership moved: returns the state from before Chord
    /// started, and forgets the current press.
    pub fn chord_ended(&self) -> Option<bool> {
        let mut state = self.0.lock();
        state.press = None;
        state.saved.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies the guard's decisions to a simulated lock state. With the key remapped,
    /// nothing else changes it.
    struct Computer {
        guard: CapsLockGuard,
        caps_lock: bool,
        /// What a plain tap does outside Chord.
        action: CapsTap,
        /// Input source switches made so far.
        switches: usize,
        now: Instant,
    }

    impl Computer {
        fn new(caps_lock: bool) -> Self {
            Self {
                guard: CapsLockGuard::default(),
                caps_lock,
                action: CapsTap::ToggleCapsLock,
                switches: 0,
                now: Instant::now(),
            }
        }
        fn switching_input_sources(caps_lock: bool) -> Self {
            Self {
                action: CapsTap::SwitchInputSource,
                ..Self::new(caps_lock)
            }
        }
        fn apply(&mut self, change: Option<bool>) {
            if let Some(enabled) = change {
                self.caps_lock = enabled;
            }
        }
        fn press(&mut self, in_normal_mode: bool) {
            let change = self
                .guard
                .pressed(self.caps_lock, in_normal_mode, self.action, self.now);
            self.apply(change);
        }
        fn hold(&mut self, duration: Duration) {
            self.now += duration;
        }
        fn release(&mut self) {
            match self.guard.released(self.now) {
                Some(Release::SetCapsLock(enabled)) => self.caps_lock = enabled,
                Some(Release::SwitchInputSource) => self.switches += 1,
                None => {}
            }
        }
        fn start_normal_mode(&mut self) {
            let change = self.guard.normal_mode_started(self.caps_lock);
            self.apply(Some(change));
        }
        fn end_chord(&mut self) {
            let change = self.guard.chord_ended();
            self.apply(change);
        }
    }

    #[test]
    fn plain_taps_toggle_turning_off_at_key_down_and_on_at_release() {
        let mut computer = Computer::new(false);
        computer.press(false);
        assert!(!computer.caps_lock, "turning on waits for release");
        computer.release();
        assert!(computer.caps_lock);
        computer.press(false);
        assert!(!computer.caps_lock, "turning off is immediate");
        computer.release();
        assert!(!computer.caps_lock);
    }

    #[test]
    fn the_activating_press_never_changes_caps_lock_and_chord_restores_it() {
        for before in [false, true] {
            let mut computer = Computer::new(before);
            computer.press(false);
            computer.start_normal_mode();
            assert!(!computer.caps_lock, "normal mode keeps Caps Lock off");
            computer.release();
            assert!(!computer.caps_lock, "the activation was not a tap");
            // Running chords with Caps never toggles it.
            for _ in 0..3 {
                computer.press(true);
                computer.release();
                assert!(!computer.caps_lock);
            }
            computer.end_chord();
            assert_eq!(computer.caps_lock, before);
            // Afterwards, taps behave normally again.
            computer.press(false);
            computer.release();
            assert_eq!(computer.caps_lock, !before);
        }
    }

    #[test]
    fn releasing_space_before_caps_still_restores_and_leaves_no_tap_behind() {
        for before in [false, true] {
            let mut computer = Computer::new(before);
            computer.press(false);
            computer.start_normal_mode();
            computer.end_chord();
            computer.release();
            assert_eq!(computer.caps_lock, before);
        }
    }

    #[test]
    fn insert_mode_toggles_freely_and_its_toggles_are_not_kept() {
        for before in [false, true] {
            let mut computer = Computer::new(before);
            computer.press(false);
            computer.start_normal_mode();
            computer.release();
            // Insert mode: plain taps toggle.
            computer.press(false);
            computer.release();
            assert!(computer.caps_lock);
            // Caps+Space returns to normal mode: off, and the earlier state is kept.
            computer.press(false);
            computer.start_normal_mode();
            computer.release();
            assert!(!computer.caps_lock);
            computer.press(false);
            computer.release();
            assert!(computer.caps_lock);
            computer.end_chord();
            assert_eq!(computer.caps_lock, before);
        }
    }

    #[test]
    fn taps_switch_input_sources_and_a_long_press_turns_caps_lock_on() {
        let mut computer = Computer::switching_input_sources(false);
        for expected in 1..=3 {
            computer.press(false);
            computer.hold(Duration::from_millis(80));
            computer.release();
            assert_eq!(computer.switches, expected, "each quick tap switches once");
            assert!(!computer.caps_lock);
        }
        computer.press(false);
        computer.hold(LONG_PRESS);
        assert!(!computer.caps_lock, "turning on waits for release");
        computer.release();
        assert!(computer.caps_lock);
        assert_eq!(computer.switches, 3);
        // With Caps Lock on, a tap only turns it off, at key-down.
        computer.press(false);
        assert!(!computer.caps_lock);
        computer.release();
        assert_eq!(computer.switches, 3);
    }

    #[test]
    fn activating_chord_never_switches_input_sources_in_either_release_order() {
        for caps_released_first in [false, true] {
            let mut computer = Computer::switching_input_sources(false);
            computer.press(false);
            computer.start_normal_mode();
            if caps_released_first {
                computer.release();
                computer.end_chord();
            } else {
                computer.end_chord();
                computer.release();
            }
            assert_eq!(computer.switches, 0);
            assert!(!computer.caps_lock);
            // Running chords with Caps in normal mode never switches either.
            computer.press(false);
            computer.start_normal_mode();
            for _ in 0..3 {
                computer.press(true);
                computer.release();
            }
            computer.end_chord();
            assert_eq!(computer.switches, 0);
        }
    }

    #[test]
    fn insert_mode_and_lost_ownership_never_switch_input_sources() {
        let mut computer = Computer::switching_input_sources(false);
        computer.press(false);
        computer.start_normal_mode();
        computer.release();
        // Insert mode toggles Caps Lock, as the controller asks it to.
        computer.action = CapsTap::ToggleCapsLock;
        computer.press(false);
        computer.release();
        assert!(computer.caps_lock);
        computer.end_chord();
        assert!(!computer.caps_lock);
        // A press whose release never arrives (ownership moved) leaves nothing behind.
        computer.action = CapsTap::SwitchInputSource;
        computer.press(false);
        computer.end_chord();
        computer.release();
        assert_eq!(computer.switches, 0);
        assert!(!computer.caps_lock);
    }
}
