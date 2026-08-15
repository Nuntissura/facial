//! Media input layer (WP-046): action map, bindings, controller pump.
//!
//! Everything the media surface can do is a [`MediaAction`]; keyboard chords
//! and controller buttons/axes bind to actions through a persisted, remappable
//! [`BindingTable`] (media DB settings, versioned JSON). The gilrs pump turns
//! pad state into edge/repeat action fires with analog stick scrolling.
//!
//! Pure logic (bindings, capture, repeat timing) lives here and unit-tests
//! directly; `ui.rs` feeds it egui key events + gilrs state per frame.

use std::collections::{BTreeMap, HashMap, HashSet};

use gilrs::{Axis, Button as PadButton, Gilrs, GilrsBuilder};
use serde::{Deserialize, Serialize};

/// Bump when defaults or the action vocabulary change so stored bindings migrate.
pub const BINDINGS_VERSION: u32 = 8;
pub const BINDINGS_SETTING_KEY: &str = "media_bindings_v1";

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// Everything the media surface can do from keyboard or controller.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MediaAction {
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    PageUp,
    PageDown,
    Home,
    End,
    FolderUp,
    FolderEnter,
    FolderPrevSibling,
    FolderNextSibling,
    ToggleFolderNavigator,
    OpenFile,
    OpenLocation,
    ToggleSelect,
    SelectAll,
    SelectNone,
    InvertSelection,
    Delete,
    DeletePermanent,
    Copy,
    Cut,
    Paste,
    Rename,
    ToggleFavoritesPanel,
    TogglePointerMode,
    ToggleSettingsPanel,
    ToggleViewMode,
    ToggleChromeHide,
    ThumbZoomIn,
    ThumbZoomOut,
    FocusSearch,
    Refresh,
    VideoSeekBack,
    VideoSeekForward,
    VideoVolumeDown,
    VideoVolumeUp,
}

impl MediaAction {
    pub const ALL: [MediaAction; 38] = [
        MediaAction::MoveLeft,
        MediaAction::MoveRight,
        MediaAction::MoveUp,
        MediaAction::MoveDown,
        MediaAction::PageUp,
        MediaAction::PageDown,
        MediaAction::Home,
        MediaAction::End,
        MediaAction::FolderUp,
        MediaAction::FolderEnter,
        MediaAction::FolderPrevSibling,
        MediaAction::FolderNextSibling,
        MediaAction::ToggleFolderNavigator,
        MediaAction::OpenFile,
        MediaAction::OpenLocation,
        MediaAction::ToggleSelect,
        MediaAction::SelectAll,
        MediaAction::SelectNone,
        MediaAction::InvertSelection,
        MediaAction::Delete,
        MediaAction::DeletePermanent,
        MediaAction::Copy,
        MediaAction::Cut,
        MediaAction::Paste,
        MediaAction::Rename,
        MediaAction::ToggleFavoritesPanel,
        MediaAction::TogglePointerMode,
        MediaAction::ToggleSettingsPanel,
        MediaAction::ToggleViewMode,
        MediaAction::ToggleChromeHide,
        MediaAction::ThumbZoomIn,
        MediaAction::ThumbZoomOut,
        MediaAction::FocusSearch,
        MediaAction::Refresh,
        MediaAction::VideoSeekBack,
        MediaAction::VideoSeekForward,
        MediaAction::VideoVolumeDown,
        MediaAction::VideoVolumeUp,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::MoveLeft => "Move left",
            Self::MoveRight => "Move right",
            Self::MoveUp => "Move up",
            Self::MoveDown => "Move down",
            Self::PageUp => "Page up",
            Self::PageDown => "Page down",
            Self::Home => "First item",
            Self::End => "Last item",
            Self::FolderUp => "Parent folder",
            Self::FolderEnter => "Enter first subfolder",
            Self::FolderPrevSibling => "Previous sibling folder",
            Self::FolderNextSibling => "Next sibling folder",
            Self::ToggleFolderNavigator => "Large folder navigator",
            Self::OpenFile => "Open file",
            Self::OpenLocation => "Open file location",
            Self::ToggleSelect => "Toggle selection",
            Self::SelectAll => "Select all",
            Self::SelectNone => "Select none",
            Self::InvertSelection => "Invert selection",
            Self::Delete => "Delete",
            Self::DeletePermanent => "Delete permanently",
            Self::Copy => "Copy",
            Self::Cut => "Cut",
            Self::Paste => "Paste",
            Self::Rename => "Rename",
            Self::ToggleFavoritesPanel => "Favorites panel",
            Self::TogglePointerMode => "Controller cursor mode",
            Self::ToggleSettingsPanel => "Settings panel",
            Self::ToggleViewMode => "Toggle view mode",
            Self::ToggleChromeHide => "Fullscreen",
            Self::ThumbZoomIn => "Zoom thumbnails in",
            Self::ThumbZoomOut => "Zoom thumbnails out",
            Self::FocusSearch => "Focus search",
            Self::Refresh => "Refresh folder",
            Self::VideoSeekBack => "Video seek back 10s",
            Self::VideoSeekForward => "Video seek forward 10s",
            Self::VideoVolumeDown => "Video volume down",
            Self::VideoVolumeUp => "Video volume up",
        }
    }

    /// Actions that auto-repeat while held (navigation).
    pub fn repeats(self) -> bool {
        matches!(
            self,
            Self::MoveLeft
                | Self::MoveRight
                | Self::MoveUp
                | Self::MoveDown
                | Self::PageUp
                | Self::PageDown
                | Self::ThumbZoomIn
                | Self::ThumbZoomOut
        )
    }
}

// ---------------------------------------------------------------------------
// Bindings
// ---------------------------------------------------------------------------

/// A keyboard chord (egui key name + modifiers, serialized as strings so the
/// table survives egui upgrades).
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct KeyChord {
    pub key: String,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub alt: bool,
}

impl KeyChord {
    pub fn new(key: &str, ctrl: bool, shift: bool, alt: bool) -> Self {
        Self {
            key: key.to_string(),
            ctrl,
            shift,
            alt,
        }
    }

    pub fn display(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.shift {
            parts.push("Shift".to_string());
        }
        if self.alt {
            parts.push("Alt".to_string());
        }
        parts.push(self.key.clone());
        parts.join("+")
    }
}

/// A controller input: a button, or an axis direction (sticks/triggers).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum PadInput {
    Button(PadButtonCode),
    AxisPos(PadAxisCode),
    AxisNeg(PadAxisCode),
}

/// Stable serializable mirrors of the gilrs enums we bind.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum PadButtonCode {
    South,
    East,
    North,
    West,
    LeftBumper,
    RightBumper,
    LeftTrigger,
    RightTrigger,
    Select,
    Start,
    LeftThumb,
    RightThumb,
    DPadUp,
    DPadDown,
    DPadLeft,
    DPadRight,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum PadAxisCode {
    LeftStickX,
    LeftStickY,
    RightStickX,
    RightStickY,
}

impl PadButtonCode {
    pub const ALL: [PadButtonCode; 16] = [
        Self::South,
        Self::East,
        Self::North,
        Self::West,
        Self::LeftBumper,
        Self::RightBumper,
        Self::LeftTrigger,
        Self::RightTrigger,
        Self::Select,
        Self::Start,
        Self::LeftThumb,
        Self::RightThumb,
        Self::DPadUp,
        Self::DPadDown,
        Self::DPadLeft,
        Self::DPadRight,
    ];

    pub fn from_gilrs(b: PadButton) -> Option<Self> {
        Some(match b {
            PadButton::South => Self::South,
            PadButton::East => Self::East,
            PadButton::North => Self::North,
            PadButton::West => Self::West,
            PadButton::LeftTrigger => Self::LeftBumper,
            PadButton::RightTrigger => Self::RightBumper,
            PadButton::LeftTrigger2 => Self::LeftTrigger,
            PadButton::RightTrigger2 => Self::RightTrigger,
            PadButton::Select => Self::Select,
            PadButton::Start => Self::Start,
            PadButton::LeftThumb => Self::LeftThumb,
            PadButton::RightThumb => Self::RightThumb,
            PadButton::DPadUp => Self::DPadUp,
            PadButton::DPadDown => Self::DPadDown,
            PadButton::DPadLeft => Self::DPadLeft,
            PadButton::DPadRight => Self::DPadRight,
            _ => return None,
        })
    }

    pub fn to_gilrs(self) -> PadButton {
        match self {
            Self::South => PadButton::South,
            Self::East => PadButton::East,
            Self::North => PadButton::North,
            Self::West => PadButton::West,
            Self::LeftBumper => PadButton::LeftTrigger,
            Self::RightBumper => PadButton::RightTrigger,
            Self::LeftTrigger => PadButton::LeftTrigger2,
            Self::RightTrigger => PadButton::RightTrigger2,
            Self::Select => PadButton::Select,
            Self::Start => PadButton::Start,
            Self::LeftThumb => PadButton::LeftThumb,
            Self::RightThumb => PadButton::RightThumb,
            Self::DPadUp => PadButton::DPadUp,
            Self::DPadDown => PadButton::DPadDown,
            Self::DPadLeft => PadButton::DPadLeft,
            Self::DPadRight => PadButton::DPadRight,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::South => "A / Cross",
            Self::East => "B / Circle",
            Self::North => "Y / Triangle",
            Self::West => "X / Square",
            Self::LeftBumper => "LB",
            Self::RightBumper => "RB",
            Self::LeftTrigger => "LT",
            Self::RightTrigger => "RT",
            Self::Select => "Select / Back",
            Self::Start => "Start / Menu",
            Self::LeftThumb => "L3",
            Self::RightThumb => "R3",
            Self::DPadUp => "D-pad up",
            Self::DPadDown => "D-pad down",
            Self::DPadLeft => "D-pad left",
            Self::DPadRight => "D-pad right",
        }
    }
}

impl PadAxisCode {
    pub fn from_gilrs(a: Axis) -> Option<Self> {
        Some(match a {
            Axis::LeftStickX => Self::LeftStickX,
            Axis::LeftStickY => Self::LeftStickY,
            Axis::RightStickX => Self::RightStickX,
            Axis::RightStickY => Self::RightStickY,
            _ => return None,
        })
    }

    pub fn to_gilrs(self) -> Axis {
        match self {
            Self::LeftStickX => Axis::LeftStickX,
            Self::LeftStickY => Axis::LeftStickY,
            Self::RightStickX => Axis::RightStickX,
            Self::RightStickY => Axis::RightStickY,
        }
    }
}

impl PadInput {
    pub fn display(&self) -> String {
        match self {
            PadInput::Button(b) => b.label().to_string(),
            PadInput::AxisPos(a) => format!("{a:?}+"),
            PadInput::AxisNeg(a) => format!("{a:?}-"),
        }
    }
}

/// The full remappable table: per action, an optional keyboard chord and an
/// optional pad input. Serialized to JSON in the media DB settings.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BindingTable {
    pub version: u32,
    pub keyboard: BTreeMap<MediaAction, KeyChord>,
    pub pad: BTreeMap<MediaAction, PadInput>,
}

impl Default for BindingTable {
    fn default() -> Self {
        use MediaAction as A;
        let mut keyboard = BTreeMap::new();
        let k = |key: &str| KeyChord::new(key, false, false, false);
        let ctrl = |key: &str| KeyChord::new(key, true, false, false);
        let ctrl_shift = |key: &str| KeyChord::new(key, true, true, false);
        let alt = |key: &str| KeyChord::new(key, false, false, true);
        keyboard.insert(A::MoveLeft, k("ArrowLeft"));
        keyboard.insert(A::MoveRight, k("ArrowRight"));
        keyboard.insert(A::MoveUp, k("ArrowUp"));
        keyboard.insert(A::MoveDown, k("ArrowDown"));
        keyboard.insert(A::PageUp, k("PageUp"));
        keyboard.insert(A::PageDown, k("PageDown"));
        keyboard.insert(A::Home, k("Home"));
        keyboard.insert(A::End, k("End"));
        keyboard.insert(A::FolderUp, k("Backspace"));
        keyboard.insert(A::FolderEnter, alt("ArrowDown"));
        keyboard.insert(A::FolderPrevSibling, alt("ArrowLeft"));
        keyboard.insert(A::FolderNextSibling, alt("ArrowRight"));
        keyboard.insert(A::ToggleFolderNavigator, ctrl("G"));
        keyboard.insert(A::OpenFile, k("Enter"));
        keyboard.insert(A::OpenLocation, ctrl("L"));
        keyboard.insert(A::ToggleSelect, k("Space"));
        keyboard.insert(A::SelectAll, ctrl("A"));
        keyboard.insert(A::SelectNone, ctrl_shift("A"));
        keyboard.insert(A::InvertSelection, ctrl("I"));
        keyboard.insert(A::Delete, k("Delete"));
        // Explorer parity (WP-073): Shift+Delete is the explicit permanent
        // path. It must be its own action — the generic Shift+<binding>
        // fallback in media_handle_input reads Shift as "extend selection".
        keyboard.insert(
            A::DeletePermanent,
            KeyChord::new("Delete", false, true, false),
        );
        keyboard.insert(A::Copy, ctrl("C"));
        keyboard.insert(A::Cut, ctrl("X"));
        keyboard.insert(A::Paste, ctrl("V"));
        keyboard.insert(A::Rename, k("F2"));
        keyboard.insert(A::ToggleFavoritesPanel, ctrl("B"));
        keyboard.insert(A::TogglePointerMode, ctrl("M"));
        keyboard.insert(A::ToggleSettingsPanel, ctrl("P"));
        keyboard.insert(A::ToggleViewMode, k("Tab"));
        keyboard.insert(A::ToggleChromeHide, ctrl("F"));
        // egui 0.27 key names are Debug-formatted ("Equals", "Minus" — there
        // is no "PlusEquals" in this egui line; review round 3, finding 4).
        keyboard.insert(A::ThumbZoomIn, ctrl("Equals"));
        keyboard.insert(A::ThumbZoomOut, ctrl("Minus"));
        keyboard.insert(A::FocusSearch, ctrl("K"));
        keyboard.insert(A::Refresh, k("F5"));
        keyboard.insert(A::VideoSeekBack, k("J"));
        keyboard.insert(A::VideoSeekForward, k("L"));
        keyboard.insert(A::VideoVolumeDown, ctrl("ArrowDown"));
        keyboard.insert(A::VideoVolumeUp, ctrl("ArrowUp"));

        let mut pad = BTreeMap::new();
        use PadButtonCode as B;
        pad.insert(A::MoveLeft, PadInput::Button(B::DPadLeft));
        pad.insert(A::MoveRight, PadInput::Button(B::DPadRight));
        pad.insert(A::MoveUp, PadInput::Button(B::DPadUp));
        pad.insert(A::MoveDown, PadInput::Button(B::DPadDown));
        pad.insert(A::OpenFile, PadInput::Button(B::South));
        pad.insert(A::FolderUp, PadInput::Button(B::East));
        pad.insert(A::ToggleSelect, PadInput::Button(B::West));
        pad.insert(A::ToggleViewMode, PadInput::Button(B::North));
        pad.insert(A::FolderPrevSibling, PadInput::Button(B::LeftBumper));
        pad.insert(A::FolderNextSibling, PadInput::Button(B::RightBumper));
        pad.insert(A::ThumbZoomOut, PadInput::Button(B::LeftTrigger));
        pad.insert(A::ThumbZoomIn, PadInput::Button(B::RightTrigger));
        pad.insert(A::TogglePointerMode, PadInput::Button(B::RightThumb));
        pad.insert(A::ToggleChromeHide, PadInput::Button(B::LeftThumb));
        // Horizontal left-stick movement is otherwise unused. It gives the
        // controller a direct, conflict-free way to enter search.
        pad.insert(A::FocusSearch, PadInput::AxisPos(PadAxisCode::LeftStickX));
        // Start/Menu stays unbound by default: Steam/Guide + Start is the
        // desktop Alt+Tab chord, and a bare Start binding can leak from that
        // chord on virtual controllers. Settings remains keyboard/click/remap.
        pad.insert(A::ToggleFolderNavigator, PadInput::Button(B::Select));
        // The right stick is unused by browser navigation. Reserve it for
        // active embedded-video transport so couch playback stays fully
        // controller-driven without stealing the grid's D-pad bindings.
        pad.insert(
            A::VideoSeekBack,
            PadInput::AxisNeg(PadAxisCode::RightStickX),
        );
        pad.insert(
            A::VideoSeekForward,
            PadInput::AxisPos(PadAxisCode::RightStickX),
        );
        pad.insert(
            A::VideoVolumeDown,
            PadInput::AxisNeg(PadAxisCode::RightStickY),
        );
        pad.insert(
            A::VideoVolumeUp,
            PadInput::AxisPos(PadAxisCode::RightStickY),
        );

        Self {
            version: BINDINGS_VERSION,
            keyboard,
            pad,
        }
    }
}

impl BindingTable {
    /// Deserialize stored JSON, merging in defaults for any action a newer
    /// build added (forward-compatible upgrades).
    pub fn from_json(raw: &str) -> Self {
        let mut table: BindingTable = match serde_json::from_str(raw) {
            Ok(t) => t,
            Err(_) => return Self::default(),
        };
        let stored_version = table.version;
        let defaults = Self::default();
        // WP-050: migrate only the exact v1 defaults. Custom bindings remain
        // untouched; the legacy Ctrl+F favorites/F11 chrome pair becomes
        // Ctrl+B favorites/Ctrl+F native fullscreen.
        if stored_version < 2 {
            let legacy_favorites = KeyChord::new("F", true, false, false);
            let legacy_chrome = KeyChord::new("F11", false, false, false);
            if table.keyboard.get(&MediaAction::ToggleFavoritesPanel) == Some(&legacy_favorites)
                && table.keyboard.get(&MediaAction::ToggleChromeHide) == Some(&legacy_chrome)
            {
                table.keyboard.insert(
                    MediaAction::ToggleFavoritesPanel,
                    KeyChord::new("B", true, false, false),
                );
                table.keyboard.insert(
                    MediaAction::ToggleChromeHide,
                    KeyChord::new("F", true, false, false),
                );
            }
        }
        // WP-051: Select/Back is the couch-distance Folders surface. Migrate
        // only the exact prior default; a custom Select binding remains the
        // operator's choice and prevents an automatic conflicting default.
        if stored_version < 3 {
            let old_select = PadInput::Button(PadButtonCode::Select);
            if table.pad.get(&MediaAction::OpenLocation) == Some(&old_select) {
                table.pad.remove(&MediaAction::OpenLocation);
                table
                    .pad
                    .insert(MediaAction::ToggleFolderNavigator, old_select);
            }
        }
        // WP-051 hardening: preserve Steam's most-used desktop chord. Remove
        // only Facial's exact old Start/Menu -> Settings default; all other
        // operator remaps remain untouched.
        if stored_version < 4 {
            let steam_alt_tab_second = PadInput::Button(PadButtonCode::Start);
            if table.pad.get(&MediaAction::ToggleSettingsPanel) == Some(&steam_alt_tab_second) {
                table.pad.remove(&MediaAction::ToggleSettingsPanel);
            }
        }
        // WP-051 controller handoff: R3 now explicitly toggles pointer mode.
        // Migrate only the exact prior R3 Favorites default.
        if stored_version < 5 {
            let old_r3 = PadInput::Button(PadButtonCode::RightThumb);
            if table.pad.get(&MediaAction::ToggleFavoritesPanel) == Some(&old_r3) {
                table.pad.remove(&MediaAction::ToggleFavoritesPanel);
                table.pad.insert(MediaAction::TogglePointerMode, old_r3);
            }
        }
        for action in MediaAction::ALL {
            table.keyboard.entry(action).or_insert_with(|| {
                defaults
                    .keyboard
                    .get(&action)
                    .cloned()
                    .unwrap_or(KeyChord::new("", false, false, false))
            });
            if let Some(default_pad) = defaults.pad.get(&action) {
                let already_used = table.pad.values().any(|bound| bound == default_pad);
                if !already_used {
                    table.pad.entry(action).or_insert(*default_pad);
                }
            }
        }
        table.version = BINDINGS_VERSION;
        table
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// The action bound to a keyboard chord, if any. Exact modifier match.
    pub fn action_for_chord(&self, chord: &KeyChord) -> Option<MediaAction> {
        self.keyboard
            .iter()
            .find(|(_, bound)| *bound == chord)
            .map(|(action, _)| *action)
    }

    /// The action bound to a pad input, if any.
    pub fn action_for_pad(&self, input: PadInput) -> Option<MediaAction> {
        self.pad
            .iter()
            .find(|(_, bound)| **bound == input)
            .map(|(action, _)| *action)
    }

    /// Conflicting action already using `chord` (excluding `for_action`).
    pub fn keyboard_conflict(
        &self,
        chord: &KeyChord,
        for_action: MediaAction,
    ) -> Option<MediaAction> {
        self.keyboard
            .iter()
            .find(|(action, bound)| **action != for_action && *bound == chord)
            .map(|(action, _)| *action)
    }

    pub fn pad_conflict(&self, input: PadInput, for_action: MediaAction) -> Option<MediaAction> {
        self.pad
            .iter()
            .find(|(action, bound)| **action != for_action && **bound == input)
            .map(|(action, _)| *action)
    }

    /// Rebind, stealing the input from any conflicting action (the stolen
    /// action keeps no binding of that type — visible in the remap UI).
    pub fn rebind_keyboard(&mut self, action: MediaAction, chord: KeyChord) {
        if let Some(conflict) = self.keyboard_conflict(&chord, action) {
            self.keyboard.remove(&conflict);
        }
        self.keyboard.insert(action, chord);
    }

    pub fn rebind_pad(&mut self, action: MediaAction, input: PadInput) {
        if let Some(conflict) = self.pad_conflict(input, action) {
            self.pad.remove(&conflict);
        }
        self.pad.insert(action, input);
    }
}

// ---------------------------------------------------------------------------
// Repeat timing (held navigation)
// ---------------------------------------------------------------------------

pub const REPEAT_INITIAL_MS: u64 = 350;
pub const REPEAT_INTERVAL_MS: u64 = 90;

/// Per-input hold state driving initial-delay + repeat-rate firing.
/// Time is injected (ms since an arbitrary epoch) so tests are deterministic.
#[derive(Default)]
pub struct RepeatClock {
    held: HashMap<PadInput, HeldState>,
}

struct HeldState {
    pressed_at: u64,
    last_fire: u64,
}

impl RepeatClock {
    /// Advance one input's state; returns true when the action should fire
    /// this frame (edge fire on press, then repeats while held).
    pub fn should_fire(
        &mut self,
        input: PadInput,
        is_down: bool,
        now_ms: u64,
        repeats: bool,
    ) -> bool {
        if !is_down {
            self.held.remove(&input);
            return false;
        }
        match self.held.get_mut(&input) {
            None => {
                self.held.insert(
                    input,
                    HeldState {
                        pressed_at: now_ms,
                        last_fire: now_ms,
                    },
                );
                true // edge
            }
            Some(state) if repeats => {
                let since_press = now_ms.saturating_sub(state.pressed_at);
                let since_fire = now_ms.saturating_sub(state.last_fire);
                if since_press >= REPEAT_INITIAL_MS && since_fire >= REPEAT_INTERVAL_MS {
                    state.last_fire = now_ms;
                    true
                } else {
                    false
                }
            }
            Some(_) => false,
        }
    }

    /// Clear all hold state (pad disconnect/reconnect — prevents stale edges).
    pub fn clear(&mut self) {
        self.held.clear();
    }

    /// Register an input as already-held WITHOUT firing its edge. Used right
    /// after a rebind capture so the button that was pressed to capture does
    /// not immediately trigger its new action (review round 3, finding 10).
    pub fn suppress(&mut self, input: PadInput, now_ms: u64) {
        self.held.insert(
            input,
            HeldState {
                pressed_at: now_ms,
                last_fire: u64::MAX - REPEAT_INTERVAL_MS, // never repeats
            },
        );
    }
}

/// Analog stick deadzone.
pub const STICK_DEADZONE: f32 = 0.25;

/// Convert a stick deflection into scroll rows/sec (smooth analog scrolling).
pub fn stick_scroll_velocity(deflection: f32) -> f32 {
    let magnitude = deflection.abs();
    if magnitude < STICK_DEADZONE {
        return 0.0;
    }
    // Normalize past the deadzone, then square for fine control near center.
    let t = (magnitude - STICK_DEADZONE) / (1.0 - STICK_DEADZONE);
    let speed = t * t * 14.0; // max ~14 rows/sec at full deflection
    speed * deflection.signum()
}

/// Convert one right-stick axis into pointer pixels/sec. The cubic-ish curve
/// keeps couch-distance fine movement controllable while retaining fast travel.
pub fn pointer_velocity(deflection: f32) -> f32 {
    let magnitude = deflection.abs();
    if magnitude < STICK_DEADZONE {
        return 0.0;
    }
    let t = (magnitude - STICK_DEADZONE) / (1.0 - STICK_DEADZONE);
    (90.0 + t * t * 1350.0) * t * deflection.signum()
}

// ---------------------------------------------------------------------------
// Rebind capture state machine
// ---------------------------------------------------------------------------

/// Which binding slot a capture is armed for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CaptureSlot {
    Keyboard(MediaAction),
    Pad(MediaAction),
}

/// Armed rebind capture: the next key chord / pad press within the timeout
/// becomes the binding; Esc cancels.
pub struct Capture {
    pub slot: CaptureSlot,
    pub armed_at_ms: u64,
}

pub const CAPTURE_TIMEOUT_MS: u64 = 5000;

/// Controller input is local to Facial: background windows and Steam/Guide
/// chord layers own their input and must never leak actions into this app.
pub fn suppress_controller_actions(app_focused: bool, guide_pressed: bool) -> bool {
    !app_focused || guide_pressed
}

/// SDL mappings reported for the Nacon Revolution 5 Pro in PS4 and PS5 modes.
///
/// `gilrs` already ships the ordinary USB GUIDs for these product IDs. These
/// additional GUIDs cover the alternate Windows HID identity used when Steam
/// Input is not translating the controller into a virtual XInput device.
pub const NACON_REVOLUTION_5_PRO_MAPPINGS: &str = concat!(
    "03008a4785320000170d000000000000,REVOLUTION 5 PRO (PS4 Mode),a:b1,b:b2,x:b0,y:b3,back:b8,guide:b12,start:b9,leftshoulder:b4,rightshoulder:b5,leftstick:b10,rightstick:b11,dpup:h0.1,dpleft:h0.8,dpdown:h0.4,dpright:h0.2,-leftx:-a0,+leftx:+a0,-lefty:-a1,+lefty:+a1,-rightx:-a2,+rightx:+a2,righty:a5,lefttrigger:b6,righttrigger:b7,platform:Windows,\n",
    "03008a4785320000190d000000000000,REVOLUTION 5 PRO (PS5 Mode),a:b1,b:b2,x:b0,y:b3,back:b8,guide:b12,start:b9,leftshoulder:b4,rightshoulder:b5,leftstick:b10,rightstick:b11,dpup:h0.1,dpleft:h0.8,dpdown:h0.4,dpright:h0.2,-leftx:-a0,+leftx:+a0,-lefty:-a1,+lefty:+a1,-rightx:-a2,+rightx:+a2,righty:a5,lefttrigger:b6,righttrigger:b7,platform:Windows,"
);

/// Construct the single controller backend used by both the GUI and its
/// headless diagnostic. Product mappings are installed before device
/// enumeration, while environment and gilrs' bundled mappings remain enabled.
pub fn new_controller_backend() -> Result<Gilrs, String> {
    GilrsBuilder::new()
        .add_mappings(NACON_REVOLUTION_5_PRO_MAPPINGS)
        .build()
        .map_err(|error| format!("gilrs initialization failed: {error}"))
}

/// Keep direct-device acquisition and WGI initialization ordering identical
/// in the GUI and controller probe.
pub fn should_initialize_gilrs(input_enabled: bool, legacy_acquired: bool) -> bool {
    input_enabled && !legacy_acquired
}

/// Pure decision seam for the reserved Start/Menu app-switch edge. Keeping
/// this separate from `platform_input::switch_apps` lets automated tests prove
/// the focus gate without ever synthesizing Alt+Tab.
pub fn reserved_app_switch_edge(app_focused: bool, start_down: bool, start_was_down: bool) -> bool {
    app_focused && start_down && !start_was_down
}

/// Complete live ordering for Facial's reserved Start/Menu edge. Guide owns
/// its chord layer, so Guide+Start and every background Start edge are
/// suppressed before Facial can inject Alt+Tab.
pub fn reserved_app_switch_edge_with_guide(
    app_focused: bool,
    guide_down: bool,
    start_down: bool,
    start_was_down: bool,
) -> bool {
    !suppress_controller_actions(app_focused, guide_down)
        && reserved_app_switch_edge(app_focused, start_down, start_was_down)
}

/// Model-safe snapshot of the exact controller backend used by the GUI.
/// This deliberately opens no window, captures no global input, and performs
/// no device mutation. A successful probe with an empty `gamepads` array
/// proves the backend initialized but did not acquire a compatible controller.
pub fn controller_probe() -> Result<serde_json::Value, String> {
    // Prefer a controller Windows already exposes through the direct joystick
    // path. On affected HID devices WGI initialization can block or abort
    // inside its Windows binding before Facial has a chance to fall back.
    // WGI remains available for pads that are not visible through WinMM.
    let mut legacy = LegacyController::default();
    let legacy_snapshot = legacy.poll();
    let mut gilrs = if should_initialize_gilrs(true, legacy_snapshot.is_some()) {
        Some(new_controller_backend()?)
    } else {
        None
    };
    let mut queued_events = 0usize;
    if let Some(gilrs) = gilrs.as_mut() {
        while gilrs.next_event().is_some() {
            queued_events = queued_events.saturating_add(1);
        }
    }
    let mut gamepads = Vec::new();
    if let Some(gilrs) = gilrs.as_ref() {
        for (id, gamepad) in gilrs.gamepads() {
            let pressed_buttons: Vec<&str> = PadButtonCode::ALL
                .into_iter()
                .filter(|button| gamepad.is_pressed(button.to_gilrs()))
                .map(PadButtonCode::label)
                .collect();
            gamepads.push(serde_json::json!({
                "id": format!("{id:?}"),
                "name": gamepad.name(),
                "connected": gamepad.is_connected(),
                "pressed_buttons": pressed_buttons,
                "axes": {
                    "left_x": gamepad.value(Axis::LeftStickX),
                    "left_y": gamepad.value(Axis::LeftStickY),
                    "right_x": gamepad.value(Axis::RightStickX),
                    "right_y": gamepad.value(Axis::RightStickY),
                },
            }));
        }
    }
    Ok(serde_json::json!({
        "status": "ok",
        "backend": if legacy_snapshot.is_some() && gamepads.is_empty() { "winmm-directinput-fallback" } else if cfg!(target_os = "windows") { "gilrs-wgi" } else { "gilrs-default" },
        "gilrs_initialized": gilrs.is_some(),
        "built_in_mapping_overrides": [
            "Nacon Revolution 5 Pro PS4 mode (VID_3285/PID_0D17)",
            "Nacon Revolution 5 Pro PS5 mode (VID_3285/PID_0D19)",
        ],
        "queued_events": queued_events,
        "gamepad_count": gamepads.len(),
        "gamepads": gamepads,
        "legacy_fallback": legacy_snapshot.map(|snapshot| snapshot.to_json()),
    }))
}

/// Owned logical controller state shared by the WGI and legacy Windows paths.
#[derive(Clone, Debug, Default)]
pub struct ControllerSnapshot {
    pub source: String,
    pub device_id: String,
    pub device_name: String,
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    buttons: HashSet<PadButtonCode>,
    axes: HashMap<PadAxisCode, f32>,
    pub guide_pressed: bool,
}

impl ControllerSnapshot {
    pub fn pressed(&self, button: PadButtonCode) -> bool {
        self.buttons.contains(&button)
    }

    pub fn axis(&self, axis: PadAxisCode) -> f32 {
        self.axes.get(&axis).copied().unwrap_or(0.0)
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "source": self.source,
            "device_id": self.device_id,
            "device_name": self.device_name,
            "vendor_id": self.vendor_id.map(|value| format!("{value:04X}")),
            "product_id": self.product_id.map(|value| format!("{value:04X}")),
            "pressed_buttons": PadButtonCode::ALL.into_iter().filter(|button| self.pressed(*button)).map(PadButtonCode::label).collect::<Vec<_>>(),
            "axes": {
                "left_x": self.axis(PadAxisCode::LeftStickX),
                "left_y": self.axis(PadAxisCode::LeftStickY),
                "right_x": self.axis(PadAxisCode::RightStickX),
                "right_y": self.axis(PadAxisCode::RightStickY),
            }
        })
    }

    pub fn from_gilrs(id: gilrs::GamepadId, gamepad: &gilrs::Gamepad<'_>) -> Self {
        let buttons = PadButtonCode::ALL
            .into_iter()
            .filter(|button| gamepad.is_pressed(button.to_gilrs()))
            .collect();
        let axes = [
            PadAxisCode::LeftStickX,
            PadAxisCode::LeftStickY,
            PadAxisCode::RightStickX,
            PadAxisCode::RightStickY,
        ]
        .into_iter()
        .map(|axis| (axis, gamepad.value(axis.to_gilrs())))
        .collect();
        Self {
            source: "gilrs-wgi".to_string(),
            device_id: format!("{id:?}"),
            device_name: gamepad.name().to_string(),
            vendor_id: None,
            product_id: None,
            buttons,
            axes,
            guide_pressed: gamepad.is_pressed(gilrs::Button::Mode),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn controller_snapshot_from_legacy_values(
    id: u32,
    device_name: String,
    vendor_id: Option<u16>,
    product_id: Option<u16>,
    raw_buttons: u32,
    pov: u32,
    x: (u32, u32, u32),
    y: (u32, u32, u32),
    z: (u32, u32, u32),
    v: (u32, u32, u32),
) -> ControllerSnapshot {
    let mut buttons = HashSet::new();
    let mapping = [
        PadButtonCode::West,
        PadButtonCode::South,
        PadButtonCode::East,
        PadButtonCode::North,
        PadButtonCode::LeftBumper,
        PadButtonCode::RightBumper,
        PadButtonCode::LeftTrigger,
        PadButtonCode::RightTrigger,
        PadButtonCode::Select,
        PadButtonCode::Start,
        PadButtonCode::LeftThumb,
        PadButtonCode::RightThumb,
    ];
    for (index, button) in mapping.into_iter().enumerate() {
        if raw_buttons & (1u32 << index) != 0 {
            buttons.insert(button);
        }
    }
    if pov != 0xffff {
        if pov >= 31_500 || pov <= 4_500 {
            buttons.insert(PadButtonCode::DPadUp);
        }
        if (4_500..=13_500).contains(&pov) {
            buttons.insert(PadButtonCode::DPadRight);
        }
        if (13_500..=22_500).contains(&pov) {
            buttons.insert(PadButtonCode::DPadDown);
        }
        if (22_500..=31_500).contains(&pov) {
            buttons.insert(PadButtonCode::DPadLeft);
        }
    }
    let normalize = |(raw, min, max): (u32, u32, u32)| {
        if max <= min {
            return 0.0;
        }
        (((raw.saturating_sub(min)) as f32 / (max - min) as f32) * 2.0 - 1.0).clamp(-1.0, 1.0)
    };
    let axes = [
        (PadAxisCode::LeftStickX, normalize(x)),
        (PadAxisCode::LeftStickY, -normalize(y)),
        (PadAxisCode::RightStickX, normalize(z)),
        (PadAxisCode::RightStickY, -normalize(v)),
    ]
    .into_iter()
    .collect();
    ControllerSnapshot {
        source: "winmm-directinput-fallback".to_string(),
        device_id: format!("joy-{id}"),
        device_name,
        vendor_id,
        product_id,
        buttons,
        axes,
        guide_pressed: raw_buttons & (1u32 << 12) != 0,
    }
}

#[derive(Debug, Default)]
pub struct LegacyController {
    active_id: Option<u32>,
}

#[cfg(windows)]
impl LegacyController {
    pub fn poll(&mut self) -> Option<ControllerSnapshot> {
        use windows_sys::Win32::Media::Multimedia::{
            joyGetDevCapsW, joyGetNumDevs, joyGetPosEx, JOYCAPSW, JOYINFOEX,
        };
        let count = unsafe { joyGetNumDevs() }.min(32);
        let candidates = self
            .active_id
            .into_iter()
            .chain((0..count).filter(|id| Some(*id) != self.active_id));
        for id in candidates {
            let mut state = JOYINFOEX {
                dwSize: std::mem::size_of::<JOYINFOEX>() as u32,
                dwFlags: 0xff,
                ..JOYINFOEX::default()
            };
            if unsafe { joyGetPosEx(id, &mut state) } != 0 {
                continue;
            }
            self.active_id = Some(id);
            let mut caps = JOYCAPSW::default();
            let caps_ok = unsafe {
                joyGetDevCapsW(
                    id as usize,
                    &mut caps,
                    std::mem::size_of::<JOYCAPSW>() as u32,
                )
            } == 0;
            let name_units = caps.szPname;
            let name_len = name_units
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(name_units.len());
            let device_name = if caps_ok {
                String::from_utf16_lossy(&name_units[..name_len])
            } else {
                "Windows legacy game controller".to_string()
            };
            let (vendor_id, product_id) = if caps_ok {
                (Some(caps.wMid), Some(caps.wPid))
            } else {
                (None, None)
            };
            let raw_buttons = state.dwButtons;
            let pov = state.dwPOV;
            let default_range = (0, 65_535);
            let x_range = if caps_ok {
                (caps.wXmin, caps.wXmax)
            } else {
                default_range
            };
            let y_range = if caps_ok {
                (caps.wYmin, caps.wYmax)
            } else {
                default_range
            };
            let z_range = if caps_ok {
                (caps.wZmin, caps.wZmax)
            } else {
                default_range
            };
            let v_range = if caps_ok {
                (caps.wVmin, caps.wVmax)
            } else {
                default_range
            };
            return Some(controller_snapshot_from_legacy_values(
                id,
                device_name,
                vendor_id,
                product_id,
                raw_buttons,
                pov,
                (state.dwXpos, x_range.0, x_range.1),
                (state.dwYpos, y_range.0, y_range.1),
                (state.dwZpos, z_range.0, z_range.1),
                (state.dwVpos, v_range.0, v_range.1),
            ));
        }
        self.active_id = None;
        None
    }
}

#[cfg(not(windows))]
impl LegacyController {
    pub fn poll(&mut self) -> Option<ControllerSnapshot> {
        None
    }
}

impl Capture {
    pub fn expired(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.armed_at_ms) > CAPTURE_TIMEOUT_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_table_covers_every_action_on_keyboard() {
        let table = BindingTable::default();
        for action in MediaAction::ALL {
            assert!(
                table.keyboard.contains_key(&action),
                "keyboard default missing for {action:?}"
            );
        }
        // Controller covers the operator-critical set including R3 cursor mode.
        assert_eq!(
            table.pad.get(&MediaAction::TogglePointerMode),
            Some(&PadInput::Button(PadButtonCode::RightThumb)),
            "R3 cursor-mode default"
        );
        assert!(table.pad.contains_key(&MediaAction::FolderUp));
        assert!(table.pad.contains_key(&MediaAction::MoveDown));
        assert_eq!(
            table.action_for_pad(PadInput::Button(PadButtonCode::Select)),
            Some(MediaAction::ToggleFolderNavigator)
        );
        assert!(!table.pad.contains_key(&MediaAction::OpenLocation));
        assert!(!table.pad.contains_key(&MediaAction::ToggleSettingsPanel));
        assert_eq!(
            table.action_for_pad(PadInput::Button(PadButtonCode::Start)),
            None,
            "Start is reserved by the focus-gated app-switch path"
        );
    }

    #[test]
    fn bindings_serialize_round_trip_and_merge_defaults() {
        let table = BindingTable::default();
        let json = table.to_json();
        let restored = BindingTable::from_json(&json);
        assert_eq!(restored.keyboard, table.keyboard);
        assert_eq!(restored.pad, table.pad);
        // A stored table missing a newer action gains the default on load.
        let mut trimmed = table.clone();
        trimmed.keyboard.remove(&MediaAction::Refresh);
        trimmed.pad.remove(&MediaAction::OpenFile);
        let merged = BindingTable::from_json(&trimmed.to_json());
        assert!(merged.keyboard.contains_key(&MediaAction::Refresh));
        assert!(merged.pad.contains_key(&MediaAction::OpenFile));
        assert_eq!(
            table.pad.get(&MediaAction::VideoSeekForward),
            Some(&PadInput::AxisPos(PadAxisCode::RightStickX))
        );
        assert_eq!(
            table.pad.get(&MediaAction::VideoVolumeDown),
            Some(&PadInput::AxisNeg(PadAxisCode::RightStickY))
        );
        assert_eq!(
            table.pad.get(&MediaAction::FocusSearch),
            Some(&PadInput::AxisPos(PadAxisCode::LeftStickX))
        );
        // Corrupt JSON falls back to defaults, no panic.
        let fallback = BindingTable::from_json("{not json");
        assert_eq!(fallback.keyboard, BindingTable::default().keyboard);
    }

    #[test]
    fn v7_table_gains_delete_permanent_without_touching_custom_bindings() {
        // A stored v7 table predates DeletePermanent entirely (WP-073).
        let mut legacy = BindingTable::default();
        legacy.version = 7;
        legacy.keyboard.remove(&MediaAction::DeletePermanent);
        // The operator's custom Delete chord must survive the upgrade.
        let custom_delete = KeyChord::new("X", false, false, true);
        legacy
            .keyboard
            .insert(MediaAction::Delete, custom_delete.clone());
        let migrated = BindingTable::from_json(&serde_json::to_string(&legacy).unwrap());
        assert_eq!(migrated.version, BINDINGS_VERSION);
        assert_eq!(
            migrated.keyboard.get(&MediaAction::DeletePermanent),
            Some(&KeyChord::new("Delete", false, true, false)),
            "forward-merge back-fills the new default chord"
        );
        assert_eq!(
            migrated.keyboard.get(&MediaAction::Delete),
            Some(&custom_delete),
            "existing custom bindings stay untouched"
        );
    }

    #[test]
    fn v1_default_shortcuts_migrate_to_ctrl_f_fullscreen() {
        let mut legacy = BindingTable::default();
        legacy.version = 1;
        legacy.keyboard.insert(
            MediaAction::ToggleFavoritesPanel,
            KeyChord::new("F", true, false, false),
        );
        legacy.keyboard.insert(
            MediaAction::ToggleChromeHide,
            KeyChord::new("F11", false, false, false),
        );
        let migrated = BindingTable::from_json(&serde_json::to_string(&legacy).unwrap());
        assert_eq!(migrated.version, BINDINGS_VERSION);
        assert_eq!(
            migrated.keyboard.get(&MediaAction::ToggleChromeHide),
            Some(&KeyChord::new("F", true, false, false))
        );
        assert_eq!(
            migrated.keyboard.get(&MediaAction::ToggleFavoritesPanel),
            Some(&KeyChord::new("B", true, false, false))
        );
    }

    #[test]
    fn v2_select_default_migrates_to_large_folder_navigator() {
        let mut legacy = BindingTable::default();
        legacy.version = 2;
        legacy.pad.remove(&MediaAction::ToggleFolderNavigator);
        legacy.pad.insert(
            MediaAction::OpenLocation,
            PadInput::Button(PadButtonCode::Select),
        );
        let migrated = BindingTable::from_json(&serde_json::to_string(&legacy).unwrap());
        assert_eq!(migrated.version, BINDINGS_VERSION);
        assert_eq!(
            migrated.pad.get(&MediaAction::ToggleFolderNavigator),
            Some(&PadInput::Button(PadButtonCode::Select))
        );
        assert!(!migrated.pad.contains_key(&MediaAction::OpenLocation));

        let mut custom = legacy;
        custom.pad.insert(
            MediaAction::OpenLocation,
            PadInput::Button(PadButtonCode::RightThumb),
        );
        custom.pad.insert(
            MediaAction::ToggleFavoritesPanel,
            PadInput::Button(PadButtonCode::Select),
        );
        let preserved = BindingTable::from_json(&serde_json::to_string(&custom).unwrap());
        assert_eq!(
            preserved.pad.get(&MediaAction::ToggleFavoritesPanel),
            Some(&PadInput::Button(PadButtonCode::Select))
        );
        assert!(!preserved
            .pad
            .contains_key(&MediaAction::ToggleFolderNavigator));
    }

    #[test]
    fn v3_start_settings_default_is_removed_for_steam_alt_tab() {
        let mut legacy = BindingTable::default();
        legacy.version = 3;
        legacy.pad.insert(
            MediaAction::ToggleSettingsPanel,
            PadInput::Button(PadButtonCode::Start),
        );
        let migrated = BindingTable::from_json(&serde_json::to_string(&legacy).unwrap());
        assert_eq!(migrated.version, BINDINGS_VERSION);
        assert!(!migrated.pad.contains_key(&MediaAction::ToggleSettingsPanel));

        let mut custom = legacy;
        custom.pad.insert(
            MediaAction::ToggleSettingsPanel,
            PadInput::Button(PadButtonCode::RightThumb),
        );
        let preserved = BindingTable::from_json(&serde_json::to_string(&custom).unwrap());
        assert_eq!(
            preserved.pad.get(&MediaAction::ToggleSettingsPanel),
            Some(&PadInput::Button(PadButtonCode::RightThumb))
        );
    }

    #[test]
    fn steam_guide_and_background_always_suppress_controller_actions() {
        assert!(!suppress_controller_actions(true, false));
        assert!(suppress_controller_actions(true, true));
        assert!(suppress_controller_actions(false, false));
        assert!(suppress_controller_actions(false, true));
    }

    #[test]
    fn reserved_start_switch_is_focus_gated_and_rising_edge_only() {
        assert!(reserved_app_switch_edge(true, true, false));
        assert!(!reserved_app_switch_edge(true, true, true));
        assert!(!reserved_app_switch_edge(true, false, false));
        assert!(!reserved_app_switch_edge(false, true, false));
    }

    #[test]
    fn direct_joystick_acquisition_prevents_redundant_wgi_initialization() {
        assert!(!should_initialize_gilrs(false, false));
        assert!(!should_initialize_gilrs(true, true));
        assert!(should_initialize_gilrs(true, false));
    }

    #[test]
    fn reserved_start_switch_never_fires_under_guide_or_after_guide_release_while_held() {
        assert!(!reserved_app_switch_edge_with_guide(
            true, true, true, false
        ));
        // The live caller latches Start while suppressed; releasing Guide with
        // Start still held must not manufacture a delayed Facial edge.
        assert!(!reserved_app_switch_edge_with_guide(
            true, false, true, true
        ));
        assert!(reserved_app_switch_edge_with_guide(
            true, false, true, false
        ));
        assert!(!reserved_app_switch_edge_with_guide(
            false, false, true, false
        ));
    }

    #[test]
    fn legacy_fallback_maps_nacon_button_pov_and_axis_layout() {
        let snapshot = controller_snapshot_from_legacy_values(
            0,
            "REVOLUTION 5 PRO".to_string(),
            Some(0x3285),
            Some(0x0d19),
            (1 << 1) | (1 << 9),
            9_000,
            (65_535, 0, 65_535),
            (0, 0, 65_535),
            (32_768, 0, 65_535),
            (65_535, 0, 65_535),
        );
        assert_eq!(snapshot.device_name, "REVOLUTION 5 PRO");
        assert_eq!(snapshot.vendor_id, Some(0x3285));
        assert_eq!(snapshot.product_id, Some(0x0d19));
        assert!(snapshot.pressed(PadButtonCode::South));
        assert!(snapshot.pressed(PadButtonCode::Start));
        assert!(snapshot.pressed(PadButtonCode::DPadRight));
        assert!(snapshot.axis(PadAxisCode::LeftStickX) > 0.99);
        assert!(snapshot.axis(PadAxisCode::LeftStickY) > 0.99);
        assert!(snapshot.axis(PadAxisCode::RightStickX).abs() < 0.01);
        assert!(snapshot.axis(PadAxisCode::RightStickY) < -0.99);
    }

    #[test]
    fn built_in_nacon_mappings_cover_ps4_and_ps5_windows_ids() {
        assert!(NACON_REVOLUTION_5_PRO_MAPPINGS.contains("85320000170d"));
        assert!(NACON_REVOLUTION_5_PRO_MAPPINGS.contains("85320000190d"));
        assert!(NACON_REVOLUTION_5_PRO_MAPPINGS.contains("start:b9"));
        assert!(NACON_REVOLUTION_5_PRO_MAPPINGS.contains("guide:b12"));
        assert!(NACON_REVOLUTION_5_PRO_MAPPINGS
            .lines()
            .all(|mapping| mapping.ends_with("platform:Windows,")));
    }

    #[test]
    fn v4_r3_favorites_default_migrates_to_pointer_mode() {
        let mut legacy = BindingTable::default();
        legacy.version = 4;
        legacy.pad.remove(&MediaAction::TogglePointerMode);
        legacy.pad.insert(
            MediaAction::ToggleFavoritesPanel,
            PadInput::Button(PadButtonCode::RightThumb),
        );
        let migrated = BindingTable::from_json(&serde_json::to_string(&legacy).unwrap());
        assert_eq!(migrated.version, BINDINGS_VERSION);
        assert!(!migrated
            .pad
            .contains_key(&MediaAction::ToggleFavoritesPanel));
        assert_eq!(
            migrated.pad.get(&MediaAction::TogglePointerMode),
            Some(&PadInput::Button(PadButtonCode::RightThumb))
        );
    }

    #[test]
    fn rebind_steals_conflicting_binding() {
        let mut table = BindingTable::default();
        let chord = KeyChord::new("Delete", false, false, false);
        assert_eq!(
            table.keyboard_conflict(&chord, MediaAction::Copy),
            Some(MediaAction::Delete)
        );
        table.rebind_keyboard(MediaAction::Copy, chord.clone());
        assert_eq!(table.action_for_chord(&chord), Some(MediaAction::Copy));
        assert!(!table.keyboard.contains_key(&MediaAction::Delete), "stolen");
        // Pad steal.
        let r3 = PadInput::Button(PadButtonCode::RightThumb);
        table.rebind_pad(MediaAction::OpenFile, r3);
        assert_eq!(table.action_for_pad(r3), Some(MediaAction::OpenFile));
        assert!(!table.pad.contains_key(&MediaAction::TogglePointerMode));
    }

    #[test]
    fn repeat_clock_edge_then_delayed_repeats() {
        let mut clock = RepeatClock::default();
        let input = PadInput::Button(PadButtonCode::DPadDown);
        assert!(clock.should_fire(input, true, 0, true), "edge fires");
        assert!(
            !clock.should_fire(input, true, 100, true),
            "within initial delay"
        );
        assert!(!clock.should_fire(input, true, 340, true));
        assert!(
            clock.should_fire(input, true, 360, true),
            "after initial delay"
        );
        assert!(
            !clock.should_fire(input, true, 400, true),
            "within interval"
        );
        assert!(clock.should_fire(input, true, 460, true), "next repeat");
        assert!(!clock.should_fire(input, false, 500, true), "release stops");
        assert!(clock.should_fire(input, true, 510, true), "fresh edge");
        // Non-repeating action: edge only.
        let open = PadInput::Button(PadButtonCode::South);
        assert!(clock.should_fire(open, true, 0, false));
        assert!(!clock.should_fire(open, true, 1000, false), "no repeat");
        clock.clear();
        assert!(
            clock.should_fire(open, true, 1100, false),
            "clear resets edges"
        );
    }

    #[test]
    fn stick_velocity_deadzone_and_curve() {
        assert_eq!(stick_scroll_velocity(0.1), 0.0, "inside deadzone");
        assert_eq!(stick_scroll_velocity(-0.2), 0.0);
        let slow = stick_scroll_velocity(0.4);
        let fast = stick_scroll_velocity(1.0);
        assert!(slow > 0.0 && fast > slow, "{slow} {fast}");
        assert!((fast - 14.0).abs() < 0.5);
        assert!(stick_scroll_velocity(-1.0) < -13.0, "sign preserved");
    }

    #[test]
    fn pointer_velocity_has_deadzone_fine_control_and_fast_travel() {
        assert_eq!(pointer_velocity(0.1), 0.0);
        assert_eq!(pointer_velocity(-0.24), 0.0);
        let fine = pointer_velocity(0.35);
        let fast = pointer_velocity(1.0);
        assert!(fine > 0.0 && fine < 100.0, "fine={fine}");
        assert!(fast > 1400.0, "fast={fast}");
        assert!(pointer_velocity(-1.0) < -1400.0);
    }

    #[test]
    fn capture_times_out() {
        let capture = Capture {
            slot: CaptureSlot::Keyboard(MediaAction::OpenFile),
            armed_at_ms: 1000,
        };
        assert!(!capture.expired(5900));
        assert!(capture.expired(6001));
    }

    #[test]
    fn pad_button_codes_round_trip_gilrs() {
        for code in [
            PadButtonCode::South,
            PadButtonCode::RightThumb,
            PadButtonCode::DPadLeft,
            PadButtonCode::LeftTrigger,
        ] {
            assert_eq!(PadButtonCode::from_gilrs(code.to_gilrs()), Some(code));
        }
        assert_eq!(PadButtonCode::from_gilrs(PadButton::Unknown), None);
    }
}
