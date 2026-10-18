use super::caps_lock_guard::{CapsLockGuard, CapsTap, Release};
use super::caps_remap::{ProcessIdentity, RemapStore, SystemMappingTable};
use super::health::{Backoff, InputHealth};
use super::secure_input::SecureInputState;
use super::tap::{TapAction, TapEvent, TapHandle};
use super::{caps_guardian, caps_hotkey, input_source};
use crate::app::AppHandleExt;
use crate::app::permissions::InputPermissions;
use crate::models::{AppKeyboardState, CapturedKeyEvent, Key, KeyEvent, KeyEventAction};
use anyhow::Result;
use bitflags::bitflags;
use keycode::KeyMappingCode;
use nject::injectable;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::mpsc::{TrySendError, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, Once, PoisonError};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

const KEY_EVENT_QUEUE_CAPACITY: usize = 1_024;

/// How often the supervisor re-checks permissions and listener health.
pub const SUPERVISOR_INTERVAL: Duration = Duration::from_millis(250);

bitflags! {
  pub struct Modifiers: u16 {
      const LEFT_SHIFT      = 1 << 0;
      const RIGHT_SHIFT     = 1 << 1;
      const LEFT_CONTROL    = 1 << 2;
      const RIGHT_CONTROL   = 1 << 3;
      const LEFT_OPTION     = 1 << 4;
      const RIGHT_OPTION    = 1 << 5;
      const LEFT_COMMAND    = 1 << 6;
      const RIGHT_COMMAND   = 1 << 7;
      const FUNCTION        = 1 << 8;
  }
}

/// The listener threads currently owned by the supervisor.
#[derive(Default)]
pub(super) struct Listeners {
    tap: Option<TapHandle>,
    tap_backoff: Backoff,
    secure_input: SecureInputState,
}

#[injectable]
pub struct AppKeyboard {
    keyboard_state: AppKeyboardState,

    #[inject(AtomicU16::new(0))]
    pub modifier_flags: AtomicU16,

    #[inject(Arc::new(InputHealth::default()))]
    health: Arc<InputHealth>,

    #[inject(Mutex::new(Listeners::default()))]
    listeners: Mutex<Listeners>,

    #[inject(CapsLockGuard::default())]
    caps_lock_guard: CapsLockGuard,

    /// Whether this instance holds the Caps key mapping (see `caps_remap`).
    #[inject(AtomicBool::new(false))]
    holds_caps_remap: AtomicBool,

    handle: AppHandle,
}

impl AppKeyboard {
    pub fn state(&self) -> &AppKeyboardState {
        &self.keyboard_state
    }

    pub fn health(&self) -> &InputHealth {
        &self.health
    }

    /// The event tap is up. Input ownership (see `DevLockfileDetector`) is only
    /// claimed while this holds; the Caps key reaches Chord through the tap.
    pub fn input_handlers_running(&self) -> bool {
        self.health.tap_running()
    }

    /// Input ownership changed hands: put back the Caps Lock state from before
    /// Chord started, and forget every held key.
    pub fn reset_input_state(&self) {
        self.on_chord_mode_ended();
        self.keyboard_state.reset();
        self.modifier_flags.store(0, Ordering::Relaxed);
    }

    /// Takes or gives up the Caps key: its mapping, and the hotkey that covers for
    /// the tap under Secure Input. Only the input owner holds them, so a standby
    /// instance leaves Caps Lock alone. Called by `DevLockfileDetector`.
    pub fn set_input_owner(&self, active: bool) {
        if active {
            if let Err(error) = self.install_caps_remap() {
                log::error!(
                    "Failed to take over the Caps key; Caps+Space cannot start Chord: {error:#}"
                );
            }
        } else {
            self.release_caps_remap();
        }
        let handle = self.handle.clone();
        let registered = self.handle.run_on_main_thread(move || {
            if let Err(error) = caps_hotkey::set_registered(&handle, active) {
                log::warn!("Failed to update the Caps hotkey fallback: {error:#}");
            }
            if active {
                input_source::observe_changes();
            }
        });
        if let Err(error) = registered {
            log::warn!("Failed to schedule the Caps hotkey fallback update: {error:#}");
        }
    }

    /// Chord is exiting: restore Caps Lock and give the key back to macOS.
    pub fn release_input(&self) {
        self.on_chord_mode_ended();
        self.release_caps_remap();
    }

    fn caps_remap_store(&self) -> Result<RemapStore> {
        // Shared by every channel, like the input lease, so one guardian sees any holder.
        let directory = self
            .handle
            .path()
            .local_data_dir()?
            .join("com.leonsilicon.chord/caps-remap-v1");
        Ok(RemapStore::new(directory.join("state.json")))
    }

    fn install_caps_remap(&self) -> Result<()> {
        let store = self.caps_remap_store()?;
        static GUARDIAN: Once = Once::new();
        GUARDIAN.call_once(|| {
            let identifier = &self.handle.config().identifier;
            if let Err(error) = caps_guardian::ensure_registered(identifier, store.path()) {
                log::error!(
                    "Failed to register the Caps guardian; a crash would leave Caps Lock remapped: {error:#}"
                );
            }
        });
        store.install(&SystemMappingTable, ProcessIdentity::current()?)?;
        if !self.holds_caps_remap.swap(true, Ordering::SeqCst) {
            log::info!("Took over the Caps key (mapped to F20 while Chord owns input)");
        }
        Ok(())
    }

    fn release_caps_remap(&self) {
        if !self.holds_caps_remap.swap(false, Ordering::SeqCst) {
            return;
        }
        let released = self.caps_remap_store().and_then(|store| {
            store.uninstall(&SystemMappingTable, Some(ProcessIdentity::current()?))
        });
        match released {
            Ok(_) => log::info!("Gave the Caps key back to macOS"),
            Err(error) => log::error!("Failed to remove the Caps key mapping: {error:#}"),
        }
    }

    /// macOS toggled Caps Lock itself, so something removed Chord's mapping (UTM's
    /// "Caps Lock as key" cleanup does). Put it back; the toggle stands, like any tap.
    fn on_native_caps_toggle(&self) {
        if !self.holds_caps_remap.load(Ordering::SeqCst) {
            return;
        }
        log::warn!(
            "macOS toggled Caps Lock itself; the Caps key mapping was removed by another app. Restoring it"
        );
        let handle = self.handle.clone();
        let spawned = std::thread::Builder::new()
            .name("chord-caps-remap".into())
            .spawn(move || {
                let keyboard = handle.app_state().keyboard();
                if keyboard.holds_caps_remap.load(Ordering::SeqCst)
                    && let Err(error) = keyboard.install_caps_remap()
                {
                    log::error!("Failed to restore the Caps key mapping: {error:#}");
                }
            });
        if let Err(error) = spawned {
            log::error!("Failed to restore the Caps key mapping: {error:#}");
        }
    }

    /// Caps+Space started Chord's normal mode, from idle or from insert mode.
    /// Caps Lock stays off while normal mode is active. Called by the controller.
    pub fn on_normal_mode_started(&self) {
        let current = Self::get_caps_lock()
            .inspect_err(|error| log::warn!("Failed to read the Caps Lock toggle: {error:#}"))
            .unwrap_or(false);
        let enabled = self.caps_lock_guard.normal_mode_started(current);
        Self::apply_caps_lock(enabled);
    }

    /// Chord ended: put back the Caps Lock toggle from before it started.
    /// Called by the controller.
    pub fn on_chord_mode_ended(&self) {
        if let Some(enabled) = self.caps_lock_guard.chord_ended() {
            Self::apply_caps_lock(enabled);
        }
    }

    fn apply_caps_lock(enabled: bool) {
        log::debug!("Setting Caps Lock to {enabled}");
        if let Err(error) = Self::set_caps_lock(enabled) {
            log::warn!("Failed to set the Caps Lock toggle: {error:#}");
        }
    }

    /// Starts the supervisor thread that owns listener registration for the
    /// lifetime of the process. Permissions are polled here as well, so a
    /// grant made in System Settings is picked up without the settings UI.
    pub(super) fn start_supervisor(&self) -> Result<()> {
        let handle = self.handle.clone();
        std::thread::Builder::new()
            .name("chord-input-supervisor".into())
            .spawn(move || {
                let mut permissions_error_reported = false;
                loop {
                    let state = handle.app_state();
                    let permissions = match state.permissions().refresh_if_changed() {
                        Ok(permissions) => {
                            permissions_error_reported = false;
                            permissions
                        }
                        Err(error) => {
                            if !permissions_error_reported {
                                permissions_error_reported = true;
                                log::error!("Failed to refresh input permissions: {error:#}");
                            }
                            InputPermissions::UNAVAILABLE
                        }
                    };
                    state
                        .keyboard()
                        .reconcile_listeners(permissions, Instant::now());
                    state.keyboard().check_secure_input();
                    std::thread::sleep(SUPERVISOR_INTERVAL);
                }
            })?;
        Ok(())
    }

    fn listeners(&self) -> MutexGuard<'_, Listeners> {
        self.listeners
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Brings the listeners in line with the granted permissions, restarting
    /// anything that died. All non-granted results fail closed.
    pub(super) fn reconcile_listeners(&self, permissions: InputPermissions, now: Instant) {
        let mut listeners = self.listeners();
        self.reconcile_tap(&mut listeners, permissions.accessibility, now);
    }

    fn reconcile_tap(&self, listeners: &mut Listeners, accessibility: bool, now: Instant) {
        if listeners.tap.as_ref().is_some_and(TapHandle::is_finished) {
            log::error!("Keyboard event tap thread exited unexpectedly; scheduling a restart");
            if let Some(tap) = listeners.tap.take() {
                tap.stop();
            }
            listeners.tap_backoff.failed(now);
        }
        if !accessibility {
            if let Some(tap) = listeners.tap.take() {
                log::warn!(
                    "Accessibility check no longer reports granted; stopping the keyboard event tap"
                );
                tap.stop();
            }
            return;
        }
        if listeners.tap.is_some() || !listeners.tap_backoff.ready(now) {
            return;
        }
        match self.spawn_tap() {
            Ok(tap) => {
                if listeners.tap_backoff.failures() > 0 {
                    log::info!(
                        "Keyboard event tap recovered after {} failed attempt(s)",
                        listeners.tap_backoff.failures()
                    );
                }
                listeners.tap_backoff.succeeded();
                listeners.tap = Some(tap);
            }
            Err(error) => {
                let delay = listeners.tap_backoff.failed(now);
                log::error!(
                    "Failed to start the keyboard event tap (retrying in {delay:?}): {error:#}"
                );
            }
        }
    }

    fn check_secure_input(&self) {
        let current = SecureInputState::current();
        let mut listeners = self.listeners();
        if listeners.secure_input == current {
            return;
        }
        match &current {
            SecureInputState::Disabled => {
                log::warn!("Secure Input is disabled again; keyboard events reach Chord")
            }
            enabled => log::warn!(
                "Secure Input is {}; macOS withholds keyboard events from Chord until it is disabled",
                enabled.describe()
            ),
        }
        listeners.secure_input = current;
    }

    /// Creates the tap and the thread that runs the controller for its events.
    fn spawn_tap(&self) -> Result<TapHandle> {
        let (tx, rx) = sync_channel::<(u64, CapturedKeyEvent)>(KEY_EVENT_QUEUE_CAPACITY);
        let handle = self.handle.clone();
        // Keeping the tap callback minimal: controller work runs here instead.
        std::thread::Builder::new()
            .name("chord-key-events".into())
            .spawn(move || {
                while let Ok((generation, event)) = rx.recv() {
                    handle
                        .app_state()
                        .dev_lockfile_detector()
                        .with_input_generation(generation, || {
                            // In order with the controller, which decides the mode.
                            handle
                                .app_state()
                                .keyboard()
                                .handle_caps_event(&event.event, event.captured_at);
                            if let Err(error) =
                                handle.app_state().app_controller().handle_key_event(&event)
                            {
                                log::error!("Failed to handle key event: {error:#}");
                            }
                        });
                }
            })?;

        let handle = self.handle.clone();
        let mut queue_full_reported = false;
        let mut disconnected_reported = false;
        let sink = move |event: TapEvent| -> TapAction {
            let keyboard = handle.app_state().keyboard();
            handle
                .app_state()
                .dev_lockfile_detector()
                .with_input_owner(|generation| {
                    let event = match event {
                        TapEvent::Key(event) => event,
                        TapEvent::RemappedCapsRepeat => return TapAction::Consume,
                        TapEvent::NativeCapsToggle => {
                            keyboard.on_native_caps_toggle();
                            return TapAction::Forward;
                        }
                        // Only the input owner rewrites, so two instances never both do.
                        TapEvent::CapsLockChanged => return TapAction::ForwardAsCapsLockKey,
                        TapEvent::CapsLockRepeated => return TapAction::Consume,
                    };
                    let input = CapturedKeyEvent {
                        event,
                        caps_pressed: keyboard.state().is_caps_pressed(),
                        captured_at: Instant::now(),
                    };
                    let action = keyboard.handle_key_event(&input);
                    match tx.try_send((generation, input)) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => {
                            if !queue_full_reported {
                                queue_full_reported = true;
                                log::warn!(
                                    "Dropping keyboard events because the bounded input queue is saturated"
                                );
                            }
                        }
                        Err(TrySendError::Disconnected(_)) => {
                            if !disconnected_reported {
                                disconnected_reported = true;
                                log::error!("Keyboard event handler thread is gone");
                            }
                        }
                    }
                    action.into()
                })
                .unwrap_or(TapAction::Forward)
        };
        TapHandle::spawn(self.health.clone(), Box::new(sink))
    }

    pub fn emit_caps_lock() -> Result<()> {
        super::caps_lock::toggle()
    }

    pub fn set_caps_lock_off() -> Result<()> {
        Self::set_caps_lock(false)
    }

    pub fn set_caps_lock(enabled: bool) -> Result<()> {
        super::caps_lock::set(enabled)
    }

    pub fn get_caps_lock() -> Result<bool> {
        super::caps_lock::get()
    }

    /// Updates the tracked keyboard state and decides whether the event
    /// reaches other applications. Runs on the listener threads, so it must
    /// stay cheap.
    fn handle_key_event(&self, input: &CapturedKeyEvent) -> KeyEventAction {
        let event = &input.event;
        log::debug!("Processing event {event:?}");
        self.keyboard_state.handle_key_event(event);
        self.update_modifier_flags(event);

        let app_mode = self.handle.app_state().app_controller().app_mode();

        // Space while Caps is held enters Chord mode and is never typed. The Caps
        // key itself is Chord's alone: it arrives remapped and never reaches apps.
        if input.activates_chord_mode()
            || matches!(
                event,
                KeyEvent::Press(Key(KeyMappingCode::CapsLock))
                    | KeyEvent::Release(Key(KeyMappingCode::CapsLock))
            )
        {
            return KeyEventAction::Consume;
        }

        if app_mode.is_chord() {
            return KeyEventAction::Consume;
        }

        KeyEventAction::Forward
    }

    /// Applies Caps Lock for a Caps press or release made at `at`, in order with the
    /// controller. See `CapsLockGuard` for the rules.
    fn handle_caps_event(&self, event: &KeyEvent, at: Instant) {
        match event {
            KeyEvent::Press(Key(KeyMappingCode::CapsLock)) => {
                let state = self.handle.app_state();
                let app_mode = state.app_controller().app_mode();
                let in_normal_mode =
                    app_mode.is_chord() && !state.chord_mode_manager().is_inserting();
                // Insert mode types into Chord, where a tap keeps toggling Caps Lock.
                let action =
                    if app_mode.is_idle() && state.settings().caps_lock_switches_input_source() {
                        CapsTap::SwitchInputSource
                    } else {
                        CapsTap::ToggleCapsLock
                    };
                let current = match Self::get_caps_lock() {
                    Ok(current) => current,
                    Err(error) => {
                        log::warn!("Failed to read the Caps Lock toggle: {error:#}");
                        return;
                    }
                };
                if let Some(enabled) =
                    self.caps_lock_guard
                        .pressed(current, in_normal_mode, action, at)
                {
                    Self::apply_caps_lock(enabled);
                }
            }
            KeyEvent::Release(Key(KeyMappingCode::CapsLock)) => {
                match self.caps_lock_guard.released(at) {
                    Some(Release::SetCapsLock(enabled)) => Self::apply_caps_lock(enabled),
                    Some(Release::SwitchInputSource) => self.switch_input_source(),
                    None => {}
                }
            }
            _ => {}
        }
    }

    /// Switches input sources for a Caps tap. Text Input Sources only work on the main
    /// thread, which runs these in order, so quick taps alternate. With nothing to
    /// switch to, the tap turns Caps Lock on instead.
    fn switch_input_source(&self) {
        let scheduled = self
            .handle
            .run_on_main_thread(|| match input_source::switch() {
                Ok(Some(source)) => log::debug!("Caps tap switched the input source to {source}"),
                Ok(None) => {
                    log::debug!("No input source to switch to; the Caps tap turns Caps Lock on");
                    Self::apply_caps_lock(true);
                }
                Err(error) => log::warn!("Failed to switch the input source: {error:#}"),
            });
        if let Err(error) = scheduled {
            log::warn!("Failed to schedule the input source switch: {error:#}");
        }
    }

    /// A Caps press or release the tap could not see (Secure Input), delivered by
    /// the hotkey fallback instead.
    pub(super) fn handle_unseen_caps_event(&self, event: &KeyEvent) {
        let detector = self.handle.app_state().dev_lockfile_detector();
        detector.with_input_owner(|_generation| {
            self.keyboard_state.handle_key_event(event);
            self.handle_caps_event(event, Instant::now());
        });
    }

    #[allow(dead_code)]
    pub fn get_modifier_flags(&self) -> Modifiers {
        Modifiers::from_bits_truncate(self.modifier_flags.load(Ordering::Relaxed))
    }

    fn modifier_key_to_flag(key: &Key) -> Option<Modifiers> {
        let flag = match key.0 {
            KeyMappingCode::ShiftLeft => Modifiers::LEFT_SHIFT,
            KeyMappingCode::ShiftRight => Modifiers::RIGHT_SHIFT,
            KeyMappingCode::ControlLeft => Modifiers::LEFT_CONTROL,
            KeyMappingCode::ControlRight => Modifiers::RIGHT_CONTROL,
            KeyMappingCode::AltLeft => Modifiers::LEFT_OPTION,
            KeyMappingCode::AltRight => Modifiers::RIGHT_OPTION,
            KeyMappingCode::MetaLeft => Modifiers::LEFT_COMMAND,
            KeyMappingCode::MetaRight => Modifiers::RIGHT_COMMAND,
            KeyMappingCode::Fn => Modifiers::FUNCTION,
            _ => return None,
        };

        Some(flag)
    }

    fn update_modifier_flags(&self, event: &KeyEvent) {
        match event {
            KeyEvent::Press(key) => {
                if let Some(flag) = Self::modifier_key_to_flag(key) {
                    self.modifier_flags.fetch_or(flag.bits(), Ordering::Relaxed);
                }
            }
            KeyEvent::Release(key) => {
                if let Some(flag) = Self::modifier_key_to_flag(key) {
                    self.modifier_flags
                        .fetch_and(!flag.bits(), Ordering::Relaxed);
                }
            }
        }
    }
}

