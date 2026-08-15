//! Lazy LibVLC integration for the media preview.
//!
//! No VLC DLL is loaded while folders are scanned or thumbnails scroll. The
//! runtime and native child surface are created only after the operator presses
//! Play on a selected video. VLC remains an optional runtime dependency.

use std::path::{Path, PathBuf};
#[cfg(not(windows))]
use std::process::Command;
use std::time::{Duration, Instant};

/// Append one bounded phase marker when an explicit diagnostic path is set.
/// Normal runs perform no trace I/O. The path is operator/model controlled so
/// a hung native call can still be localized without focusing the GUI.
pub fn playback_trace_phase(phase: &str, detail: &str) {
    use std::io::Write;
    let Some(path) = std::env::var_os("FACIAL_PLAYBACK_TRACE") else {
        return;
    };
    let timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{timestamp_ms}\t{phase}\t{detail}");
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Track {
    pub id: i32,
    pub name: String,
}

/// LibVLC's last observed playback state. `Pending` is Facial's optimistic
/// state before LibVLC has confirmed an accepted asynchronous command.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackStatus {
    #[default]
    Pending,
    Opening,
    Buffering,
    Playing,
    Paused,
    Stopped,
    Ended,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlayingTransition {
    AlreadyConfirmed,
    AlreadyPending,
    Play,
    Resume,
    Pause,
}

fn playing_transition(
    status: PlaybackStatus,
    requested: bool,
    pending: Option<bool>,
) -> Result<PlayingTransition, String> {
    if status == PlaybackStatus::Error {
        return Err("LibVLC is in an input or playback error state".to_string());
    }
    let observed_confirms = if requested {
        status == PlaybackStatus::Playing
    } else {
        matches!(
            status,
            PlaybackStatus::Paused | PlaybackStatus::Stopped | PlaybackStatus::Ended
        )
    };
    if pending == Some(requested) {
        return Ok(if observed_confirms {
            PlayingTransition::AlreadyConfirmed
        } else {
            PlayingTransition::AlreadyPending
        });
    }
    if pending == Some(!requested) {
        return Ok(if requested {
            match status {
                PlaybackStatus::Paused | PlaybackStatus::Stopped | PlaybackStatus::Ended => {
                    PlayingTransition::Play
                }
                PlaybackStatus::Pending
                | PlaybackStatus::Opening
                | PlaybackStatus::Buffering
                | PlaybackStatus::Playing => PlayingTransition::Resume,
                PlaybackStatus::Error => unreachable!("error handled above"),
            }
        } else {
            // An earlier play/resume may still land after a currently paused
            // observation. Reassert pause to cancel that opposite target.
            PlayingTransition::Pause
        });
    }
    Ok(if requested {
        match status {
            PlaybackStatus::Playing => PlayingTransition::AlreadyConfirmed,
            PlaybackStatus::Pending | PlaybackStatus::Opening | PlaybackStatus::Buffering => {
                PlayingTransition::AlreadyPending
            }
            PlaybackStatus::Paused | PlaybackStatus::Stopped | PlaybackStatus::Ended => {
                PlayingTransition::Play
            }
            PlaybackStatus::Error => unreachable!("error handled above"),
        }
    } else {
        match status {
            PlaybackStatus::Paused | PlaybackStatus::Stopped | PlaybackStatus::Ended => {
                PlayingTransition::AlreadyConfirmed
            }
            PlaybackStatus::Pending
            | PlaybackStatus::Opening
            | PlaybackStatus::Buffering
            | PlaybackStatus::Playing => PlayingTransition::Pause,
            PlaybackStatus::Error => unreachable!("error handled above"),
        }
    })
}

fn toggled_playing_target(snapshot: Option<&Snapshot>) -> Result<bool, String> {
    snapshot
        .map(|snapshot| !snapshot.playing)
        .ok_or_else(|| "No video is loaded".to_string())
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Snapshot {
    pub path: String,
    pub playing: bool,
    pub time_ms: i64,
    pub length_ms: i64,
    pub volume: i32,
    pub audio_track: i32,
    pub subtitle_track: i32,
    pub audio_tracks: Vec<Track>,
    pub subtitle_tracks: Vec<Track>,
    pub looping: bool,
    /// False while the fields include an optimistic command update which has
    /// not yet been reconciled against LibVLC.
    pub confirmed: bool,
    pub status: PlaybackStatus,
    pub error: Option<String>,
}

/// Lightweight player timings for the built-in diagnostics surface. Values
/// are accumulated on the UI thread, so recording them never adds a lock to
/// playback controls.
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct NativeSurfaceDiagnostics {
    /// Exact eframe Win32 parent supplied by the live UI. Process-local only.
    pub parent_hwnd: Option<isize>,
    /// Native child created for LibVLC video output. Process-local only.
    pub child_hwnd: Option<isize>,
    pub parent_valid: bool,
    pub child_valid: bool,
    pub child_parent_matches: bool,
    pub child_visible: bool,
    /// Full requested child rectangle in parent-client physical pixels. This
    /// remains the complete media geometry when a Win32 region clips it.
    pub target_bounds_px: Option<[i32; 4]>,
    /// Visible intersection in parent-client physical pixels after applying
    /// the owning panel's clip. Equal to `target_bounds_px` when fully visible.
    pub clipped_bounds_px: Option<[i32; 4]>,
    /// Observed child rectangle in parent-client physical pixels.
    pub child_bounds_px: Option<[i32; 4]>,
    /// `None` until a LibVLC player exists; then verifies set/get HWND identity.
    pub libvlc_hwnd_matches: Option<bool>,
    /// Raw Win32 code for the most recent native-surface failure.
    pub last_error_code: Option<i32>,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct PlaybackDiagnostics {
    pub command_count: u64,
    pub command_total_us: u64,
    pub command_max_us: u64,
    pub poll_count: u64,
    pub poll_total_us: u64,
    pub poll_max_us: u64,
    pub forced_poll_count: u64,
    pub failure_count: u64,
    pub last_status: Option<PlaybackStatus>,
    pub surface: NativeSurfaceDiagnostics,
}

#[derive(Default)]
struct PendingConfirmation {
    playing: Option<bool>,
    time_ms: Option<i64>,
    volume: Option<i32>,
    audio_track: Option<i32>,
    subtitle_track: Option<i32>,
}

impl PendingConfirmation {
    fn clear(&mut self) {
        *self = Self::default();
    }

    fn is_empty(&self) -> bool {
        self.playing.is_none()
            && self.time_ms.is_none()
            && self.volume.is_none()
            && self.audio_track.is_none()
            && self.subtitle_track.is_none()
    }

    fn reconcile(&mut self, snapshot: &mut Snapshot) {
        if let Some(expected) = self.playing {
            let confirmed = if expected {
                snapshot.status == PlaybackStatus::Playing
            } else {
                matches!(
                    snapshot.status,
                    PlaybackStatus::Paused | PlaybackStatus::Stopped | PlaybackStatus::Ended
                )
            };
            if confirmed {
                self.playing = None;
            } else {
                snapshot.playing = expected;
            }
        }
        if let Some(expected) = self.time_ms {
            // Seeking and the decoding clock advance asynchronously. A small
            // tolerance confirms the requested neighborhood without waiting.
            if snapshot.time_ms.abs_diff(expected) <= 1_500 {
                self.time_ms = None;
            } else {
                snapshot.time_ms = expected;
            }
        }
        if let Some(expected) = self.volume {
            if snapshot.volume == expected {
                self.volume = None;
            } else {
                snapshot.volume = expected;
            }
        }
        if let Some(expected) = self.audio_track {
            if snapshot.audio_track == expected {
                self.audio_track = None;
            } else {
                snapshot.audio_track = expected;
            }
        }
        if let Some(expected) = self.subtitle_track {
            if snapshot.subtitle_track == expected {
                self.subtitle_track = None;
            } else {
                snapshot.subtitle_track = expected;
            }
        }
        snapshot.confirmed &= self.is_empty();
    }
}

pub struct VideoPlayer {
    #[cfg(windows)]
    runtime: Option<windows_impl::VlcRuntime>,
    last_error: Option<String>,
    loop_enabled: bool,
    cached_snapshot: Option<Snapshot>,
    last_snapshot_poll: Option<Instant>,
    diagnostics: PlaybackDiagnostics,
    pending_confirmation: PendingConfirmation,
    /// Stable raw handle obtained from eframe's `CreationContext`. It is stored
    /// as an integer so this module's public API remains portable; conversion
    /// to `HWND` is confined to the Windows implementation.
    parent_window_handle: Option<isize>,
}

impl Default for VideoPlayer {
    fn default() -> Self {
        Self {
            #[cfg(windows)]
            runtime: None,
            last_error: None,
            loop_enabled: true,
            cached_snapshot: None,
            last_snapshot_poll: None,
            diagnostics: PlaybackDiagnostics::default(),
            pending_confirmation: PendingConfirmation::default(),
            parent_window_handle: None,
        }
    }
}

/// Start the one-time LibVLC/plugin-cache warm-up without blocking GUI
/// construction or any render frame. The worker opens no media and creates no
/// window; it only initializes and releases a temporary LibVLC instance.
pub fn prewarm_async() {
    #[cfg(windows)]
    {
        static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        STARTED.get_or_init(|| {
            let _ = std::thread::Builder::new()
                .name("facial-vlc-prewarm".to_string())
                .spawn(|| {
                    playback_trace_phase("vlc.prewarm.begin", "");
                    match windows_impl::prewarm() {
                        Ok(()) => playback_trace_phase("vlc.prewarm.end", "ok"),
                        Err(error) => playback_trace_phase("vlc.prewarm.end", &error),
                    }
                });
        });
    }
}

impl VideoPlayer {
    pub fn available() -> bool {
        resolve_vlc_dir().is_some()
    }

    /// Supply the exact native parent returned by eframe's live
    /// `CreationContext`. Call this once during live app construction, before
    /// the first `play`. Headless inspectors intentionally leave it unset.
    pub fn set_parent_window_handle(&mut self, handle: isize) -> Result<(), String> {
        if handle == 0 {
            self.diagnostics.surface.last_error_code = Some(1400); // ERROR_INVALID_WINDOW_HANDLE
            return Err("Facial supplied an invalid zero video parent handle".to_string());
        }
        #[cfg(windows)]
        {
            let hwnd = handle as windows_sys::Win32::Foundation::HWND;
            if unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsWindow(hwnd) } == 0 {
                self.diagnostics.surface.last_error_code = Some(1400); // ERROR_INVALID_WINDOW_HANDLE
                return Err("Facial supplied an invalid video parent window".to_string());
            }
            if let Some(runtime) = self.runtime.as_ref() {
                if runtime.parent_handle() != hwnd {
                    self.diagnostics.surface.last_error_code = Some(1400);
                    return Err(
                        "The embedded video parent cannot change after LibVLC is loaded"
                            .to_string(),
                    );
                }
            }
        }
        self.parent_window_handle = Some(handle);
        self.diagnostics.surface.parent_hwnd = Some(handle);
        self.diagnostics.surface.parent_valid = true;
        self.diagnostics.surface.last_error_code = None;
        Ok(())
    }

    /// The app window handle supplied by eframe, when the app is running live.
    ///
    /// Used as the owner for modal shell dialogs. `None` in headless runs,
    /// which never open one.
    pub fn parent_window_handle(&self) -> Option<isize> {
        self.parent_window_handle
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn active_path(&self) -> Option<&str> {
        #[cfg(windows)]
        {
            return self.runtime.as_ref().and_then(|runtime| runtime.path());
        }
        #[cfg(not(windows))]
        None
    }

    pub fn play(&mut self, path: &Path) -> Result<(), String> {
        self.play_with_surface(path, None)
    }

    /// Start only after the native child has a visible, clipped owner. This
    /// prevents audio/decoding from beginning against the hidden bootstrap
    /// window or a stale placement from another panel.
    pub fn play_clipped(
        &mut self,
        path: &Path,
        rect_points: egui::Rect,
        clip_points: egui::Rect,
        pixels_per_point: f32,
    ) -> Result<(), String> {
        self.play_with_surface(path, Some((rect_points, clip_points, pixels_per_point)))
    }

    fn play_with_surface(
        &mut self,
        path: &Path,
        surface: Option<(egui::Rect, egui::Rect, f32)>,
    ) -> Result<(), String> {
        #[cfg(windows)]
        {
            let started = Instant::now();
            playback_trace_phase("video_player.play.begin", &path.to_string_lossy());
            if self.runtime.is_none() {
                let Some(parent) = self.parent_window_handle else {
                    let error = "Embedded playback has no Facial parent window handle".to_string();
                    self.last_error = Some(error.clone());
                    self.diagnostics.surface.last_error_code = Some(1400);
                    self.record_command(started.elapsed());
                    self.diagnostics.failure_count =
                        self.diagnostics.failure_count.saturating_add(1);
                    return Err(error);
                };
                playback_trace_phase("video_player.runtime_load.begin", "");
                match windows_impl::VlcRuntime::load(parent as windows_sys::Win32::Foundation::HWND)
                {
                    Ok(runtime) => {
                        playback_trace_phase("video_player.runtime_load.end", "ok");
                        self.runtime = Some(runtime)
                    }
                    Err(error) => {
                        playback_trace_phase("video_player.runtime_load.end", &error);
                        self.last_error = Some(error.clone());
                        self.record_command(started.elapsed());
                        self.diagnostics.failure_count =
                            self.diagnostics.failure_count.saturating_add(1);
                        return Err(error);
                    }
                }
            }
            let result = self.runtime.as_mut().expect("runtime initialized").play(
                path,
                self.loop_enabled,
                surface,
            );
            playback_trace_phase(
                "video_player.runtime_play.end",
                result
                    .as_ref()
                    .map_or_else(|error| error.as_str(), |_| "ok"),
            );
            self.last_error = result.as_ref().err().cloned();
            self.record_command(started.elapsed());
            if result.is_ok() {
                self.pending_confirmation.clear();
                self.pending_confirmation.playing = Some(true);
                self.cached_snapshot = Some(Snapshot {
                    path: path.to_string_lossy().to_string(),
                    playing: true,
                    time_ms: 0,
                    length_ms: 0,
                    volume: self
                        .cached_snapshot
                        .as_ref()
                        .map_or(100, |snapshot| snapshot.volume),
                    audio_track: -1,
                    subtitle_track: -1,
                    audio_tracks: Vec::new(),
                    subtitle_tracks: Vec::new(),
                    looping: self.loop_enabled,
                    confirmed: false,
                    status: PlaybackStatus::Pending,
                    error: None,
                });
                self.diagnostics.last_status = Some(PlaybackStatus::Pending);
                self.last_snapshot_poll = None;
            } else {
                self.cached_snapshot = None;
                self.last_snapshot_poll = None;
                self.pending_confirmation.clear();
                self.diagnostics.failure_count = self.diagnostics.failure_count.saturating_add(1);
            }
            return result;
        }
        #[cfg(not(windows))]
        {
            let _ = (path, surface);
            let error = "Embedded VLC preview is currently available on Windows".to_string();
            self.last_error = Some(error.clone());
            Err(error)
        }
    }

    pub fn toggle_pause(&mut self) -> Result<(), String> {
        // Toggle the optimistic target visible to the operator, not
        // libvlc_media_player_is_playing(). During Opening/Buffering LibVLC
        // reports false even though the accepted target is playing, which
        // previously turned a visible Pause click into another Play call.
        let requested = toggled_playing_target(self.cached_snapshot.as_ref())?;
        self.set_playing(requested)
    }

    /// Request an explicit transport state without deriving it from a cached
    /// snapshot. Repeating the same request is idempotent: LibVLC's current
    /// state determines whether no action, play, or pause is required.
    pub fn set_playing(&mut self, playing: bool) -> Result<(), String> {
        #[cfg(windows)]
        {
            let started = Instant::now();
            let pending = self.pending_confirmation.playing;
            let result = match self.runtime.as_mut() {
                Some(runtime) => runtime.set_playing(playing, pending),
                None => Err("No video is loaded".to_string()),
            };
            self.record_command(started.elapsed());
            match result {
                Ok((confirmed, observed_status)) => {
                    self.last_error = None;
                    self.accept_playing_request(playing, confirmed, observed_status);
                    Ok(())
                }
                Err(error) => {
                    self.last_error = Some(error.clone());
                    self.diagnostics.failure_count =
                        self.diagnostics.failure_count.saturating_add(1);
                    Err(error)
                }
            }
        }
        #[cfg(not(windows))]
        {
            let _ = playing;
            let error = "No video is loaded".to_string();
            self.last_error = Some(error.clone());
            Err(error)
        }
    }

    fn accept_playing_request(
        &mut self,
        playing: bool,
        confirmed: bool,
        observed_status: PlaybackStatus,
    ) {
        self.diagnostics.last_status = Some(observed_status);
        self.pending_confirmation.playing = (!confirmed).then_some(playing);
        if let Some(snapshot) = self.cached_snapshot.as_mut() {
            snapshot.playing = playing;
            snapshot.status = observed_status;
            snapshot.error = None;
            snapshot.confirmed = confirmed
                && self.pending_confirmation.is_empty()
                && !matches!(
                    observed_status,
                    PlaybackStatus::Pending | PlaybackStatus::Opening | PlaybackStatus::Buffering
                );
        }
    }

    pub fn set_time(&mut self, time_ms: i64) -> Result<(), String> {
        #[cfg(windows)]
        {
            let started = Instant::now();
            let result = match self.runtime.as_mut() {
                Some(runtime) => runtime.set_time(time_ms),
                None => Err("No video is loaded".to_string()),
            };
            self.record_command(started.elapsed());
            self.last_error = result.as_ref().err().cloned();
            if result.is_ok() {
                if let Some(snapshot) = self.cached_snapshot.as_mut() {
                    snapshot.time_ms = time_ms.clamp(0, snapshot.length_ms.max(time_ms));
                    self.pending_confirmation.time_ms = Some(snapshot.time_ms);
                    snapshot.confirmed = false;
                    snapshot.error = None;
                }
            } else {
                self.diagnostics.failure_count = self.diagnostics.failure_count.saturating_add(1);
            }
            return result;
        }
        #[cfg(not(windows))]
        {
            let _ = time_ms;
            let error = "No video is loaded".to_string();
            self.last_error = Some(error.clone());
            Err(error)
        }
    }

    /// Change the persisted playback preference. When a video is already open,
    /// recreate its LibVLC media with the new per-media option and restore the
    /// current timestamp so the operator does not lose their place.
    pub fn set_loop(&mut self, enabled: bool) -> Result<(), String> {
        if self.loop_enabled == enabled {
            return Ok(());
        }
        #[cfg(windows)]
        if let Some(runtime) = self.runtime.as_mut() {
            let started = Instant::now();
            let result = runtime.set_loop(enabled);
            self.record_command(started.elapsed());
            self.last_error = result.as_ref().err().cloned();
            if let Err(error) = result {
                self.cached_snapshot = None;
                self.last_snapshot_poll = None;
                self.pending_confirmation.clear();
                self.diagnostics.failure_count = self.diagnostics.failure_count.saturating_add(1);
                return Err(error);
            }
        }
        self.loop_enabled = enabled;
        if let Some(snapshot) = self.cached_snapshot.as_mut() {
            snapshot.looping = enabled;
            snapshot.confirmed = false;
            snapshot.error = None;
        }
        Ok(())
    }

    pub fn set_volume(&mut self, volume: i32) -> Result<(), String> {
        #[cfg(windows)]
        {
            let started = Instant::now();
            let result = match self.runtime.as_mut() {
                Some(runtime) => runtime.set_volume(volume),
                None => Err("No video is loaded".to_string()),
            };
            self.record_command(started.elapsed());
            self.last_error = result.as_ref().err().cloned();
            if result.is_ok() {
                if let Some(snapshot) = self.cached_snapshot.as_mut() {
                    snapshot.volume = volume.clamp(0, 125);
                    self.pending_confirmation.volume = Some(snapshot.volume);
                    snapshot.confirmed = false;
                    snapshot.error = None;
                }
            } else {
                self.diagnostics.failure_count = self.diagnostics.failure_count.saturating_add(1);
            }
            return result;
        }
        #[cfg(not(windows))]
        {
            let _ = volume;
            let error = "No video is loaded".to_string();
            self.last_error = Some(error.clone());
            Err(error)
        }
    }

    pub fn set_audio_track(&mut self, id: i32) -> Result<(), String> {
        #[cfg(windows)]
        {
            let started = Instant::now();
            let result = match self.runtime.as_mut() {
                Some(runtime) => runtime.set_audio_track(id),
                None => Err("No video is loaded".to_string()),
            };
            self.record_command(started.elapsed());
            self.last_error = result.as_ref().err().cloned();
            if result.is_ok() {
                if let Some(snapshot) = self.cached_snapshot.as_mut() {
                    snapshot.audio_track = id;
                    self.pending_confirmation.audio_track = Some(id);
                    snapshot.confirmed = false;
                    snapshot.error = None;
                }
            } else {
                self.diagnostics.failure_count = self.diagnostics.failure_count.saturating_add(1);
            }
            return result;
        }
        #[cfg(not(windows))]
        {
            let _ = id;
            let error = "No video is loaded".to_string();
            self.last_error = Some(error.clone());
            Err(error)
        }
    }

    pub fn set_subtitle_track(&mut self, id: i32) -> Result<(), String> {
        #[cfg(windows)]
        {
            let started = Instant::now();
            let result = match self.runtime.as_mut() {
                Some(runtime) => runtime.set_subtitle_track(id),
                None => Err("No video is loaded".to_string()),
            };
            self.record_command(started.elapsed());
            self.last_error = result.as_ref().err().cloned();
            if result.is_ok() {
                if let Some(snapshot) = self.cached_snapshot.as_mut() {
                    snapshot.subtitle_track = id;
                    self.pending_confirmation.subtitle_track = Some(id);
                    snapshot.confirmed = false;
                    snapshot.error = None;
                }
            } else {
                self.diagnostics.failure_count = self.diagnostics.failure_count.saturating_add(1);
            }
            return result;
        }
        #[cfg(not(windows))]
        {
            let _ = id;
            let error = "No video is loaded".to_string();
            self.last_error = Some(error.clone());
            Err(error)
        }
    }

    pub fn snapshot(&mut self) -> Option<Snapshot> {
        let now = Instant::now();
        if snapshot_poll_due(
            self.cached_snapshot.as_ref(),
            self.last_snapshot_poll,
            now,
            false,
        ) {
            // The normal UI poll preserves its Option API. A failed LibVLC
            // input is still recorded in last_error and the cached snapshot;
            // model commands use snapshot_fresh to receive the error directly.
            let _ = self.reconcile_snapshot(false);
        }
        self.cached_snapshot.clone()
    }

    /// Poll LibVLC immediately, bypassing the normal 50/500 ms UI throttle.
    /// Model-command receipts use this after an accepted command so their
    /// payload is never merely the previous throttled snapshot. Asynchronous
    /// LibVLC input failures are returned and also retained in `last_error`.
    pub fn snapshot_fresh(&mut self) -> Result<Option<Snapshot>, String> {
        self.reconcile_snapshot(true)
    }

    /// Return the most recently reconciled/optimistically updated state with
    /// no LibVLC calls. Controller seek/volume actions use this hot path.
    pub fn cached_snapshot(&self) -> Option<Snapshot> {
        self.cached_snapshot.clone()
    }

    pub fn diagnostics(&self) -> PlaybackDiagnostics {
        let mut diagnostics = self.diagnostics;
        #[cfg(windows)]
        {
            if let Some(runtime) = self.runtime.as_ref() {
                let prior_error = diagnostics.surface.last_error_code;
                diagnostics.surface = runtime.surface_diagnostics();
                if diagnostics.surface.last_error_code.is_none() {
                    diagnostics.surface.last_error_code = prior_error;
                }
            } else if let Some(parent) = self.parent_window_handle {
                diagnostics.surface.parent_valid = unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::IsWindow(
                        parent as windows_sys::Win32::Foundation::HWND,
                    )
                } != 0;
            }
        }
        diagnostics
    }

    fn reconcile_snapshot(&mut self, forced: bool) -> Result<Option<Snapshot>, String> {
        let started = Instant::now();
        #[cfg(windows)]
        let polled = self.runtime.as_mut().and_then(|runtime| runtime.snapshot());
        #[cfg(not(windows))]
        let polled = None;

        self.record_poll(started.elapsed());
        if forced {
            self.diagnostics.forced_poll_count =
                self.diagnostics.forced_poll_count.saturating_add(1);
        }
        self.last_snapshot_poll = Some(Instant::now());
        self.accept_polled_snapshot(polled)
    }

    fn accept_polled_snapshot(
        &mut self,
        polled: Option<Snapshot>,
    ) -> Result<Option<Snapshot>, String> {
        let Some(mut snapshot) = polled else {
            return Ok(self.cached_snapshot.clone());
        };
        snapshot.looping = self.loop_enabled;
        self.diagnostics.last_status = Some(snapshot.status);
        let error = snapshot.error.clone();
        if error.is_some() {
            self.pending_confirmation.clear();
        } else {
            self.pending_confirmation.reconcile(&mut snapshot);
        }
        self.cached_snapshot = Some(snapshot);
        if let Some(error) = error {
            let newly_observed = self.last_error.as_deref() != Some(error.as_str());
            self.last_error = Some(error.clone());
            if newly_observed {
                self.diagnostics.failure_count = self.diagnostics.failure_count.saturating_add(1);
            }
            Err(error)
        } else {
            self.last_error = None;
            Ok(self.cached_snapshot.clone())
        }
    }

    fn record_command(&mut self, elapsed: Duration) {
        let micros = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
        self.diagnostics.command_count = self.diagnostics.command_count.saturating_add(1);
        self.diagnostics.command_total_us =
            self.diagnostics.command_total_us.saturating_add(micros);
        self.diagnostics.command_max_us = self.diagnostics.command_max_us.max(micros);
    }

    fn record_poll(&mut self, elapsed: Duration) {
        let micros = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
        self.diagnostics.poll_count = self.diagnostics.poll_count.saturating_add(1);
        self.diagnostics.poll_total_us = self.diagnostics.poll_total_us.saturating_add(micros);
        self.diagnostics.poll_max_us = self.diagnostics.poll_max_us.max(micros);
    }

    /// Export the frame currently decoded by the embedded LibVLC player.
    /// This is the native-video half of Facial's model-safe visual debugger:
    /// egui surfaces remain covered by `ui-inspect`, while this artifact proves
    /// what the child video surface is actually displaying.
    pub fn capture_frame(&mut self, path: &Path) -> Result<(), String> {
        #[cfg(windows)]
        {
            return self
                .runtime
                .as_mut()
                .ok_or_else(|| "No video is loaded".to_string())?
                .capture_frame(path);
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            Err("Embedded VLC frame capture is currently available on Windows".to_string())
        }
    }

    /// Place the native VLC child surface in viewport pixel coordinates.
    pub fn show_at(
        &mut self,
        rect_points: egui::Rect,
        pixels_per_point: f32,
    ) -> Result<(), String> {
        self.show_clipped(rect_points, None, pixels_per_point)
    }

    /// Place the native VLC child surface, clipped to the owning panel's
    /// visible rectangle. An empty intersection hides the surface (WP-065).
    pub fn show_clipped(
        &mut self,
        rect_points: egui::Rect,
        clip_points: Option<egui::Rect>,
        pixels_per_point: f32,
    ) -> Result<(), String> {
        #[cfg(windows)]
        {
            return self
                .runtime
                .as_mut()
                .ok_or_else(|| "No video is loaded".to_string())?
                .show_clipped(rect_points, clip_points, pixels_per_point);
        }
        #[cfg(not(windows))]
        {
            let _ = (rect_points, clip_points, pixels_per_point);
            Ok(())
        }
    }

    pub fn hide(&mut self) {
        #[cfg(windows)]
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.hide();
        }
    }

    pub fn stop(&mut self) {
        #[cfg(windows)]
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.stop();
        }
        self.cached_snapshot = None;
        self.last_snapshot_poll = None;
        self.last_error = None;
        self.pending_confirmation.clear();
        self.diagnostics.last_status = Some(PlaybackStatus::Stopped);
    }
}

fn snapshot_poll_due(
    cached: Option<&Snapshot>,
    last_poll: Option<Instant>,
    now: Instant,
    forced: bool,
) -> bool {
    if forced {
        return true;
    }
    let interval = if cached.is_some_and(|snapshot| snapshot.playing) {
        Duration::from_millis(50)
    } else {
        Duration::from_millis(500)
    };
    last_poll.is_none_or(|last| now.saturating_duration_since(last) >= interval)
}

/// Convert egui logical points to parent-client physical pixels without
/// allowing NaN/infinite geometry to reach Win32.
fn physical_surface_bounds(
    rect_points: egui::Rect,
    pixels_per_point: f32,
) -> Result<[i32; 4], String> {
    let values = [
        rect_points.min.x,
        rect_points.min.y,
        rect_points.max.x,
        rect_points.max.y,
        pixels_per_point,
    ];
    if values.iter().any(|value| !value.is_finite()) || pixels_per_point <= 0.0 {
        return Err("Embedded video received invalid surface geometry".to_string());
    }
    let ppp = pixels_per_point.max(0.1);
    let x = (rect_points.min.x * ppp).round() as i32;
    let y = (rect_points.min.y * ppp).round() as i32;
    let width = (rect_points.width() * ppp).round().max(1.0) as i32;
    let height = (rect_points.height() * ppp).round().max(1.0) as i32;
    Ok([x, y, width, height])
}

/// Full native-child geometry plus an optional child-local clipping region.
///
/// The child must retain the requested media rectangle when only part of its
/// owning tile is visible. Resizing the child to `rect.intersect(clip)` keeps it
/// inside the panel, but also scales the complete video into the remaining
/// sliver. A Win32 window region clips pixels without changing that geometry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NativeSurfacePlacement {
    bounds_px: [i32; 4],
    visible_bounds_px: [i32; 4],
    /// Child-local left, top, right, bottom. `None` means the full child.
    clip_region_px: Option<[i32; 4]>,
}

fn physical_surface_placement(
    rect_points: egui::Rect,
    clip_points: Option<egui::Rect>,
    pixels_per_point: f32,
) -> Result<Option<NativeSurfacePlacement>, String> {
    let visible_points = clip_points
        .map(|clip| rect_points.intersect(clip))
        .unwrap_or(rect_points);
    if visible_points.width() < 1.0 || visible_points.height() < 1.0 {
        return Ok(None);
    }

    let bounds_px = physical_surface_bounds(rect_points, pixels_per_point)?;
    let visible_px = physical_surface_bounds(visible_points, pixels_per_point)?;
    let [x, y, width, height] = bounds_px;
    let [visible_x, visible_y, visible_width, visible_height] = visible_px;
    let left = visible_x.saturating_sub(x).clamp(0, width);
    let top = visible_y.saturating_sub(y).clamp(0, height);
    let right = visible_x
        .saturating_add(visible_width)
        .saturating_sub(x)
        .clamp(left, width);
    let bottom = visible_y
        .saturating_add(visible_height)
        .saturating_sub(y)
        .clamp(top, height);
    if right <= left || bottom <= top {
        return Ok(None);
    }
    let region = [left, top, right, bottom];
    Ok(Some(NativeSurfacePlacement {
        bounds_px,
        visible_bounds_px: [
            x.saturating_add(left),
            y.saturating_add(top),
            right.saturating_sub(left),
            bottom.saturating_sub(top),
        ],
        clip_region_px: (region != [0, 0, width, height]).then_some(region),
    }))
}

/// Locate a portable/local VLC runtime first, then standard Windows install
/// roots. `FACIAL_VLC_DIR` is the explicit relocation override.
pub fn resolve_vlc_dir() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(value) = std::env::var_os("FACIAL_VLC_DIR") {
        candidates.push(PathBuf::from(value));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("vlc"));
            candidates.push(dir.to_path_buf());
        }
    }
    for variable in ["PROGRAMFILES", "PROGRAMFILES(X86)"] {
        if let Some(root) = std::env::var_os(variable) {
            candidates.push(PathBuf::from(root).join("VideoLAN").join("VLC"));
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path));
    }
    candidates
        .into_iter()
        .map(|candidate| {
            if candidate
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("libvlc.dll"))
            {
                candidate
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or(candidate)
            } else {
                candidate
            }
        })
        .find(|dir| dir.join("libvlc.dll").is_file())
}

/// Optional remote-file read-ahead. There is deliberately no guessed default:
/// operators can benchmark their NAS and opt in with a bounded millisecond
/// value. VLC media options use the `:name=value` form.
fn configured_remote_file_cache_ms() -> Option<u32> {
    std::env::var("FACIAL_VLC_REMOTE_CACHE_MS")
        .ok()
        .and_then(|value| parse_remote_file_cache_ms(&value))
}

fn parse_remote_file_cache_ms(value: &str) -> Option<u32> {
    value.trim().parse::<u32>().ok().filter(|value| {
        // Keep experiments bounded: enough range for high-latency shares,
        // while preventing a typo from turning every seek into a long wait.
        (50..=10_000).contains(value)
    })
}

/// Renderer override for the embedded child surface. `wingdi` is the hardened
/// default: unlike VLC's Direct3D overlay paths it is composed into the host
/// HWND and remains visible/capturable beside egui on affected Windows/DPI
/// configurations. This is paid only while a video is explicitly playing;
/// operators may opt back into a validated accelerated module for demanding
/// 4K playback. Only known module names are accepted, so arbitrary command-line
/// options cannot be injected through the environment.
fn configured_vlc_vout() -> Option<String> {
    Some(normalize_vlc_vout(
        std::env::var("FACIAL_VLC_VOUT").ok().as_deref(),
    ))
}

fn normalize_vlc_vout(value: Option<&str>) -> String {
    let value = value.unwrap_or("wingdi").trim().to_ascii_lowercase();
    matches!(
        value.as_str(),
        "direct3d11" | "direct3d9" | "directdraw" | "wingdi" | "glwin32"
    )
    .then_some(value)
    .unwrap_or_else(|| "wingdi".to_string())
}

fn vlc_repeat_media_option() -> String {
    // VLC 3.x documents input-repeat as an unsigned 0..=65535 value. The
    // former -1 sentinel was rejected/ignored and let one-length fixtures end
    // while the wrapper still reported the persisted loop preference.
    ":input-repeat=65535".to_string()
}

#[cfg(windows)]
pub(crate) fn is_remote_media_path(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;

    // WinBase.h DRIVE_REMOTE. Keeping the value local avoids enabling the
    // unrelated Win32_System_WindowsProgramming feature for one constant.
    const DRIVE_REMOTE_TYPE: u32 = 4;

    let text = path.as_os_str().to_string_lossy();
    if text.starts_with("\\\\") {
        return true;
    }
    let bytes = text.as_bytes();
    if bytes.len() < 2 || bytes[1] != b':' || !bytes[0].is_ascii_alphabetic() {
        return false;
    }
    let root = format!("{}:\\", bytes[0] as char);
    let wide: Vec<u16> = std::ffi::OsStr::new(&root)
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe { GetDriveTypeW(wide.as_ptr()) == DRIVE_REMOTE_TYPE }
}

#[cfg(not(windows))]
pub(crate) fn is_remote_media_path(_path: &Path) -> bool {
    false
}

pub fn open_in_vlc(path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        // Use the Windows file association directly. Spawning vlc.exe while a
        // separate VLC instance already existed could create/focus VLC yet
        // lose the media handoff; ShellExecute is the same UTF-16 path Windows
        // Explorer uses for a double-click.
        let operation: Vec<u16> = "open".encode_utf16().chain(Some(0)).collect();
        let file = windows_external_open_path_units(path);
        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                operation.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        } as isize;
        if result > 32 {
            return Ok(());
        }
        return Err(format!(
            "Windows failed to open associated video app (code {result})"
        ));
    }
    #[cfg(not(windows))]
    {
        let dir = resolve_vlc_dir().ok_or_else(|| {
            "VLC was not found; install VLC or set FACIAL_VLC_DIR to its folder".to_string()
        })?;
        let executable = dir.join("vlc.exe");
        if !executable.is_file() {
            return Err(format!("VLC executable not found in {}", dir.display()));
        }
        Command::new(executable)
            .arg(path)
            .spawn()
            .map_err(|error| format!("failed to launch VLC: {error}"))?;
        Ok(())
    }
}

#[cfg(windows)]
fn windows_external_open_path_units(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// Open the Windows "Open with" chooser for `path`.
///
/// `owner` is the app window the modal should belong to — pass
/// [`VideoPlayer::parent_window_handle`]. This used to call `GetActiveWindow()`
/// instead, which returns the active window *of the calling thread* and is null
/// whenever no window of this thread is active. A null owner makes the chooser
/// ownerless: it can open behind the app, on the wrong monitor, or leave the app
/// clickable underneath a supposedly modal dialog (WP-060/WP-065). The known
/// parent handle is authoritative and does not depend on focus state.
#[cfg(windows)]
pub fn open_with_dialog(path: &Path, owner: Option<isize>) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
    use windows_sys::Win32::UI::Shell::{
        SHOpenWithDialog, OAIF_ALLOW_REGISTRATION, OAIF_EXEC, OPENASINFO,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::IsWindow;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let info = OPENASINFO {
        pcszFile: wide.as_ptr(),
        pcszClass: std::ptr::null(),
        oaifInFlags: OAIF_ALLOW_REGISTRATION | OAIF_EXEC,
    };
    // A handle can go stale between frames (the viewport can be recreated), so
    // it is re-validated here rather than trusted. GetActiveWindow remains only
    // as the last resort when no parent was ever supplied.
    let owner = owner
        .map(|handle| handle as HWND)
        .filter(|handle| !handle.is_null() && unsafe { IsWindow(*handle) } != 0)
        .unwrap_or_else(|| unsafe { GetActiveWindow() });
    let result = unsafe { SHOpenWithDialog(owner, &info) };
    if result >= 0 {
        Ok(())
    } else {
        Err(format!("Windows app selector failed (HRESULT {result:#x})"))
    }
}

#[cfg(not(windows))]
pub fn open_with_dialog(_path: &Path, _owner: Option<isize>) -> Result<(), String> {
    Err("The app selector is currently available on Windows".to_string())
}

#[cfg(windows)]
mod windows_impl {
    use super::{
        configured_remote_file_cache_ms, configured_vlc_vout, is_remote_media_path,
        physical_surface_placement, playback_trace_phase, playing_transition, resolve_vlc_dir,
        vlc_repeat_media_option, NativeSurfaceDiagnostics, PlaybackStatus, PlayingTransition,
        Snapshot, Track,
    };
    use libloading::os::windows::Library;
    use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
    use std::path::Path;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        CreateRectRgn, DeleteObject, InvalidateRect, MapWindowPoints, SetWindowRgn,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetParent, GetWindowLongPtrW, GetWindowRect, IsWindow,
        IsWindowVisible, SetWindowLongPtrW, SetWindowPos, ShowWindow, GWL_STYLE, SWP_NOACTIVATE,
        SWP_SHOWWINDOW, SW_HIDE, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS,
    };

    type VlcPtr = *mut c_void;

    fn last_win32_error_code() -> Option<i32> {
        std::io::Error::last_os_error().raw_os_error()
    }

    fn child_bounds_in_parent(parent: HWND, child: HWND) -> Option<[i32; 4]> {
        if parent.is_null() || child.is_null() {
            return None;
        }
        let mut rect = RECT::default();
        if unsafe { GetWindowRect(child, &mut rect) } == 0 {
            return None;
        }
        let mut points = [
            POINT {
                x: rect.left,
                y: rect.top,
            },
            POINT {
                x: rect.right,
                y: rect.bottom,
            },
        ];
        unsafe {
            MapWindowPoints(
                std::ptr::null_mut(),
                parent,
                points.as_mut_ptr(),
                points.len() as u32,
            );
        }
        Some([
            points[0].x,
            points[0].y,
            points[1].x.saturating_sub(points[0].x),
            points[1].y.saturating_sub(points[0].y),
        ])
    }

    #[repr(C)]
    struct TrackDescription {
        i_id: c_int,
        psz_name: *mut c_char,
        p_next: *mut TrackDescription,
    }

    #[derive(Clone, Copy)]
    struct VlcFns {
        new: unsafe extern "C" fn(c_int, *const *const c_char) -> VlcPtr,
        release: unsafe extern "C" fn(VlcPtr),
        media_new_path: unsafe extern "C" fn(VlcPtr, *const c_char) -> VlcPtr,
        media_add_option: unsafe extern "C" fn(VlcPtr, *const c_char),
        media_release: unsafe extern "C" fn(VlcPtr),
        player_new_from_media: unsafe extern "C" fn(VlcPtr) -> VlcPtr,
        player_release: unsafe extern "C" fn(VlcPtr),
        player_play: unsafe extern "C" fn(VlcPtr) -> c_int,
        player_stop: unsafe extern "C" fn(VlcPtr),
        player_set_pause: unsafe extern "C" fn(VlcPtr, c_int),
        player_is_playing: unsafe extern "C" fn(VlcPtr) -> c_int,
        player_get_state: unsafe extern "C" fn(VlcPtr) -> c_int,
        player_get_time: unsafe extern "C" fn(VlcPtr) -> i64,
        player_set_time: unsafe extern "C" fn(VlcPtr, i64),
        player_get_length: unsafe extern "C" fn(VlcPtr) -> i64,
        player_set_hwnd: unsafe extern "C" fn(VlcPtr, *mut c_void),
        player_get_hwnd: unsafe extern "C" fn(VlcPtr) -> *mut c_void,
        audio_get_volume: unsafe extern "C" fn(VlcPtr) -> c_int,
        audio_set_volume: unsafe extern "C" fn(VlcPtr, c_int) -> c_int,
        audio_get_track: unsafe extern "C" fn(VlcPtr) -> c_int,
        audio_set_track: unsafe extern "C" fn(VlcPtr, c_int) -> c_int,
        audio_get_tracks: unsafe extern "C" fn(VlcPtr) -> *mut TrackDescription,
        video_get_spu: unsafe extern "C" fn(VlcPtr) -> c_int,
        video_set_spu: unsafe extern "C" fn(VlcPtr, c_int) -> c_int,
        video_get_spus: unsafe extern "C" fn(VlcPtr) -> *mut TrackDescription,
        video_take_snapshot:
            unsafe extern "C" fn(VlcPtr, c_uint, *const c_char, c_uint, c_uint) -> c_int,
        tracks_release: unsafe extern "C" fn(*mut TrackDescription),
    }

    impl VlcFns {
        unsafe fn load(library: &Library) -> Result<Self, String> {
            macro_rules! symbol {
                ($name:literal) => {{
                    *library
                        .get(concat!($name, "\0").as_bytes())
                        .map_err(|error| format!("missing LibVLC symbol {}: {error}", $name))?
                }};
            }
            Ok(Self {
                new: symbol!("libvlc_new"),
                release: symbol!("libvlc_release"),
                media_new_path: symbol!("libvlc_media_new_path"),
                media_add_option: symbol!("libvlc_media_add_option"),
                media_release: symbol!("libvlc_media_release"),
                player_new_from_media: symbol!("libvlc_media_player_new_from_media"),
                player_release: symbol!("libvlc_media_player_release"),
                player_play: symbol!("libvlc_media_player_play"),
                player_stop: symbol!("libvlc_media_player_stop"),
                player_set_pause: symbol!("libvlc_media_player_set_pause"),
                player_is_playing: symbol!("libvlc_media_player_is_playing"),
                player_get_state: symbol!("libvlc_media_player_get_state"),
                player_get_time: symbol!("libvlc_media_player_get_time"),
                player_set_time: symbol!("libvlc_media_player_set_time"),
                player_get_length: symbol!("libvlc_media_player_get_length"),
                player_set_hwnd: symbol!("libvlc_media_player_set_hwnd"),
                player_get_hwnd: symbol!("libvlc_media_player_get_hwnd"),
                audio_get_volume: symbol!("libvlc_audio_get_volume"),
                audio_set_volume: symbol!("libvlc_audio_set_volume"),
                audio_get_track: symbol!("libvlc_audio_get_track"),
                audio_set_track: symbol!("libvlc_audio_set_track"),
                audio_get_tracks: symbol!("libvlc_audio_get_track_description"),
                video_get_spu: symbol!("libvlc_video_get_spu"),
                video_set_spu: symbol!("libvlc_video_set_spu"),
                video_get_spus: symbol!("libvlc_video_get_spu_description"),
                video_take_snapshot: symbol!("libvlc_video_take_snapshot"),
                tracks_release: symbol!("libvlc_track_description_list_release"),
            })
        }
    }

    pub fn prewarm() -> Result<(), String> {
        let dir = resolve_vlc_dir().ok_or_else(|| {
            "VLC was not found; install VLC or set FACIAL_VLC_DIR to its folder".to_string()
        })?;
        let dll = dir.join("libvlc.dll");
        let library = unsafe {
            Library::load_with_flags(&dll, 0x0000_0100 | 0x0000_1000)
                .map_err(|error| format!("load {}: {error}", dll.display()))?
        };
        let fns = unsafe { VlcFns::load(&library)? };
        let mut option_values = vec![
            "--no-video-title-show".to_string(),
            "--quiet".to_string(),
            "--no-stats".to_string(),
        ];
        if std::env::var("FACIAL_TEST_SILENT")
            .ok()
            .is_some_and(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
        {
            option_values.push("--no-audio".to_string());
        }
        if let Some(vout) = configured_vlc_vout() {
            option_values.push(format!("--vout={vout}"));
        }
        let options = option_values
            .into_iter()
            .map(|value| CString::new(value).expect("validated VLC option"))
            .collect::<Vec<_>>();
        let option_ptrs = options
            .iter()
            .map(|value| value.as_ptr())
            .collect::<Vec<_>>();
        let instance = unsafe { (fns.new)(option_ptrs.len() as c_int, option_ptrs.as_ptr()) };
        if instance.is_null() {
            return Err("LibVLC prewarm could not initialize".to_string());
        }
        unsafe { (fns.release)(instance) };
        Ok(())
    }

    pub struct VlcRuntime {
        _library: Library,
        fns: VlcFns,
        instance: VlcPtr,
        player: VlcPtr,
        hwnd: HWND,
        parent: HWND,
        path: Option<String>,
        audio_tracks: Vec<Track>,
        subtitle_tracks: Vec<Track>,
        last_track_refresh: Option<Instant>,
        last_surface_bounds: Option<[i32; 4]>,
        last_surface_clipped_bounds: Option<[i32; 4]>,
        /// Child-local Win32 region currently owned by the child window. Cache
        /// the coordinates so steady frames do not allocate/apply a new HRGN.
        last_surface_region: Option<[i32; 4]>,
        surface_visible: bool,
        last_surface_error_code: Option<i32>,
    }

    impl VlcRuntime {
        pub fn load(parent: HWND) -> Result<Self, String> {
            if parent.is_null() || unsafe { IsWindow(parent) } == 0 {
                return Err("Facial video parent window is no longer valid".to_string());
            }
            let dir = resolve_vlc_dir().ok_or_else(|| {
                "VLC was not found; install VLC or set FACIAL_VLC_DIR to its folder".to_string()
            })?;
            let dll = dir.join("libvlc.dll");
            playback_trace_phase("vlc.load_library.begin", &dll.to_string_lossy());
            let library = unsafe {
                Library::load_with_flags(&dll, 0x0000_0100 | 0x0000_1000)
                    .map_err(|error| format!("load {}: {error}", dll.display()))?
            };
            playback_trace_phase("vlc.load_library.end", "ok");
            let fns = unsafe { VlcFns::load(&library)? };
            playback_trace_phase("vlc.load_symbols.end", "ok");
            let mut option_values = vec![
                "--no-video-title-show".to_string(),
                "--quiet".to_string(),
                "--no-stats".to_string(),
            ];
            if std::env::var("FACIAL_TEST_SILENT")
                .ok()
                .is_some_and(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
            {
                option_values.push("--no-audio".to_string());
            }
            if let Some(vout) = configured_vlc_vout() {
                option_values.push(format!("--vout={vout}"));
            }
            playback_trace_phase("vlc.instance_new.begin", &option_values.join(" "));
            let options = option_values
                .into_iter()
                .map(|value| CString::new(value).expect("validated VLC option"))
                .collect::<Vec<_>>();
            let option_ptrs = options
                .iter()
                .map(|value| value.as_ptr())
                .collect::<Vec<_>>();
            let instance = unsafe { (fns.new)(option_ptrs.len() as c_int, option_ptrs.as_ptr()) };
            if instance.is_null() {
                return Err("LibVLC could not initialize".to_string());
            }
            playback_trace_phase("vlc.instance_new.end", "ok");
            Ok(Self {
                _library: library,
                fns,
                instance,
                player: std::ptr::null_mut(),
                hwnd: std::ptr::null_mut(),
                parent,
                path: None,
                audio_tracks: Vec::new(),
                subtitle_tracks: Vec::new(),
                last_track_refresh: None,
                last_surface_bounds: None,
                last_surface_clipped_bounds: None,
                last_surface_region: None,
                surface_visible: false,
                last_surface_error_code: None,
            })
        }

        pub fn parent_handle(&self) -> HWND {
            self.parent
        }

        pub fn surface_diagnostics(&self) -> NativeSurfaceDiagnostics {
            let parent_valid = !self.parent.is_null() && unsafe { IsWindow(self.parent) } != 0;
            let child_valid = !self.hwnd.is_null() && unsafe { IsWindow(self.hwnd) } != 0;
            let child_parent_matches =
                child_valid && unsafe { GetParent(self.hwnd) } == self.parent;
            let child_visible = child_valid && unsafe { IsWindowVisible(self.hwnd) } != 0;
            let libvlc_hwnd_matches = (!self.player.is_null())
                .then(|| unsafe { (self.fns.player_get_hwnd)(self.player) == self.hwnd.cast() });
            NativeSurfaceDiagnostics {
                parent_hwnd: (!self.parent.is_null()).then_some(self.parent as isize),
                child_hwnd: (!self.hwnd.is_null()).then_some(self.hwnd as isize),
                parent_valid,
                child_valid,
                child_parent_matches,
                child_visible,
                target_bounds_px: self.last_surface_bounds,
                clipped_bounds_px: self.last_surface_clipped_bounds,
                child_bounds_px: child_valid
                    .then(|| child_bounds_in_parent(self.parent, self.hwnd))
                    .flatten(),
                libvlc_hwnd_matches,
                last_error_code: self.last_surface_error_code,
            }
        }

        pub fn path(&self) -> Option<&str> {
            self.path.as_deref()
        }

        pub fn play(
            &mut self,
            path: &Path,
            loop_enabled: bool,
            surface: Option<(egui::Rect, egui::Rect, f32)>,
        ) -> Result<(), String> {
            playback_trace_phase("vlc.ensure_window.begin", "");
            self.ensure_window()?;
            playback_trace_phase("vlc.ensure_window.end", "ok");
            self.release_player();
            self.path = None;
            let path_text = path.to_string_lossy().to_string();
            let c_path = CString::new(path_text.as_bytes())
                .map_err(|_| "Video path contains an unsupported NUL character".to_string())?;
            playback_trace_phase("vlc.media_new_path.begin", &path_text);
            let media = unsafe { (self.fns.media_new_path)(self.instance, c_path.as_ptr()) };
            if media.is_null() {
                return Err("LibVLC could not create media for this path".to_string());
            }
            playback_trace_phase("vlc.media_new_path.end", "ok");
            if loop_enabled {
                // Applying the maximum supported repeat count before creating
                // the media player keeps the effectively-continuous preview
                // local to Facial instead of relying on playlist state.
                let repeat = CString::new(vlc_repeat_media_option()).expect("static VLC option");
                unsafe { (self.fns.media_add_option)(media, repeat.as_ptr()) };
            }
            if is_remote_media_path(path) {
                if let Some(milliseconds) = configured_remote_file_cache_ms() {
                    let cache = CString::new(format!(":file-caching={milliseconds}"))
                        .expect("numeric VLC cache option");
                    unsafe { (self.fns.media_add_option)(media, cache.as_ptr()) };
                }
            }
            playback_trace_phase("vlc.player_new.begin", "");
            let player = unsafe { (self.fns.player_new_from_media)(media) };
            unsafe { (self.fns.media_release)(media) };
            if player.is_null() {
                return Err("LibVLC could not create a media player".to_string());
            }
            playback_trace_phase("vlc.player_new.end", "ok");
            self.player = player;
            unsafe { (self.fns.player_set_hwnd)(self.player, self.hwnd.cast()) };
            playback_trace_phase("vlc.player_set_hwnd.end", "ok");
            let assigned_hwnd = unsafe { (self.fns.player_get_hwnd)(self.player) };
            if assigned_hwnd != self.hwnd.cast() {
                self.last_surface_error_code = Some(1400); // ERROR_INVALID_WINDOW_HANDLE
                self.release_player();
                return Err("LibVLC did not retain Facial's video child window".to_string());
            }
            if let Some((rect, clip, pixels_per_point)) = surface {
                if !self.place_clipped(rect, Some(clip), pixels_per_point)? {
                    self.release_player();
                    return Err(
                        "Embedded video start was deferred because its surface is not visible"
                            .to_string(),
                    );
                }
                playback_trace_phase("vlc.pre_play_place", "current owner geometry");
            }
            // WP-065: the child is created at 16x16 and hidden, and only a
            // render frame gives it real bounds. If a previous placement left
            // usable bounds, restore them BEFORE play so the very first decoded
            // frame lands somewhere visible rather than into a hidden 16x16
            // window that a later frame has to rescue.
            if surface.is_none() {
                if let Some([x, y, width, height]) = self.last_surface_bounds {
                    if width > 1 && height > 1 {
                        unsafe {
                            SetWindowPos(
                                self.hwnd,
                                std::ptr::null_mut(),
                                x,
                                y,
                                width,
                                height,
                                SWP_NOACTIVATE | SWP_SHOWWINDOW,
                            );
                        }
                        self.surface_visible = unsafe { IsWindowVisible(self.hwnd) } != 0;
                        playback_trace_phase(
                            "vlc.pre_play_place",
                            &format!("x={x} y={y} w={width} h={height}"),
                        );
                    }
                }
            }
            playback_trace_phase("vlc.player_play.begin", "");
            if unsafe { (self.fns.player_play)(self.player) } != 0 {
                self.release_player();
                return Err("LibVLC rejected playback".to_string());
            }
            playback_trace_phase("vlc.player_play.end", "ok");
            self.path = Some(path_text);
            self.audio_tracks.clear();
            self.subtitle_tracks.clear();
            self.last_track_refresh = None;
            Ok(())
        }

        pub fn set_loop(&mut self, enabled: bool) -> Result<(), String> {
            let Some(path) = self.path.clone() else {
                return Ok(());
            };
            let time_ms = if self.player.is_null() {
                0
            } else {
                unsafe { (self.fns.player_get_time)(self.player) }.max(0)
            };
            let was_playing = matches!(
                self.playback_status(),
                PlaybackStatus::Pending
                    | PlaybackStatus::Opening
                    | PlaybackStatus::Buffering
                    | PlaybackStatus::Playing
            );
            self.play(Path::new(&path), enabled, None)?;
            self.set_time(time_ms)?;
            if !was_playing {
                let _ = self.set_playing(false, Some(true))?;
            }
            Ok(())
        }

        pub fn set_playing(
            &mut self,
            requested: bool,
            pending: Option<bool>,
        ) -> Result<(bool, PlaybackStatus), String> {
            self.ensure_usable_player()?;
            let observed = self.playback_status();
            match playing_transition(observed, requested, pending)? {
                PlayingTransition::AlreadyConfirmed => Ok((true, observed)),
                PlayingTransition::AlreadyPending => Ok((false, observed)),
                PlayingTransition::Play => {
                    if unsafe { (self.fns.player_play)(self.player) } != 0 {
                        return Err("LibVLC could not start or resume playback".to_string());
                    }
                    Ok((false, observed))
                }
                PlayingTransition::Resume => {
                    unsafe { (self.fns.player_set_pause)(self.player, 0) };
                    Ok((false, observed))
                }
                PlayingTransition::Pause => {
                    unsafe { (self.fns.player_set_pause)(self.player, 1) };
                    Ok((false, observed))
                }
            }
        }

        pub fn set_time(&mut self, value: i64) -> Result<(), String> {
            self.ensure_usable_player()?;
            unsafe { (self.fns.player_set_time)(self.player, value.max(0)) };
            Ok(())
        }

        pub fn set_volume(&mut self, value: i32) -> Result<(), String> {
            self.ensure_usable_player()?;
            let value = value.clamp(0, 125);
            if unsafe { (self.fns.audio_set_volume)(self.player, value) } != 0 {
                return Err(format!("LibVLC rejected volume {value}"));
            }
            Ok(())
        }

        pub fn set_audio_track(&mut self, id: i32) -> Result<(), String> {
            self.ensure_usable_player()?;
            if unsafe { (self.fns.audio_set_track)(self.player, id) } != 0 {
                return Err(format!("LibVLC rejected audio track {id}"));
            }
            Ok(())
        }

        pub fn set_subtitle_track(&mut self, id: i32) -> Result<(), String> {
            self.ensure_usable_player()?;
            if unsafe { (self.fns.video_set_spu)(self.player, id) } != 0 {
                return Err(format!("LibVLC rejected subtitle track {id}"));
            }
            Ok(())
        }

        pub fn capture_frame(&mut self, path: &Path) -> Result<(), String> {
            if self.player.is_null() {
                return Err("No video is loaded".to_string());
            }
            let parent = path
                .parent()
                .ok_or_else(|| "Frame capture output has no parent folder".to_string())?;
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create frame capture folder: {error}"))?;

            let extension = path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("png");
            let temp = parent.join(format!(
                ".facial-video-capture-{}.{}",
                uuid::Uuid::new_v4(),
                extension
            ));
            let c_path = CString::new(temp.to_string_lossy().as_bytes()).map_err(|_| {
                "Frame capture path contains an unsupported NUL character".to_string()
            })?;
            let result =
                unsafe { (self.fns.video_take_snapshot)(self.player, 0, c_path.as_ptr(), 0, 0) };
            if result != 0 {
                return Err("LibVLC could not capture the current video frame".to_string());
            }

            // LibVLC normally writes synchronously, but a short bounded wait
            // makes the artifact contract reliable across video outputs.
            let deadline = Instant::now() + Duration::from_secs(2);
            while !temp.metadata().is_ok_and(|metadata| metadata.len() > 0)
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            if !temp.metadata().is_ok_and(|metadata| metadata.len() > 0) {
                let _ = std::fs::remove_file(&temp);
                return Err(
                    "LibVLC frame capture timed out before producing an artifact".to_string(),
                );
            }
            if path.exists() {
                std::fs::remove_file(path)
                    .map_err(|error| format!("replace existing frame capture: {error}"))?;
            }
            std::fs::rename(&temp, path)
                .map_err(|error| format!("publish frame capture: {error}"))?;
            Ok(())
        }

        pub fn snapshot(&mut self) -> Option<Snapshot> {
            if self.player.is_null() {
                return None;
            }
            let status = self.playback_status();
            let refresh = self
                .last_track_refresh
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(1));
            if refresh {
                self.audio_tracks =
                    unsafe { self.read_tracks((self.fns.audio_get_tracks)(self.player)) };
                self.subtitle_tracks =
                    unsafe { self.read_tracks((self.fns.video_get_spus)(self.player)) };
                self.last_track_refresh = Some(Instant::now());
            }
            Some(Snapshot {
                path: self.path.clone().unwrap_or_default(),
                // Opening and buffering are active pending play states even
                // though libvlc_media_player_is_playing still returns zero.
                playing: matches!(
                    status,
                    PlaybackStatus::Pending
                        | PlaybackStatus::Opening
                        | PlaybackStatus::Buffering
                        | PlaybackStatus::Playing
                ),
                time_ms: unsafe { (self.fns.player_get_time)(self.player) }.max(0),
                length_ms: unsafe { (self.fns.player_get_length)(self.player) }.max(0),
                volume: unsafe { (self.fns.audio_get_volume)(self.player) }.max(0),
                audio_track: unsafe { (self.fns.audio_get_track)(self.player) },
                subtitle_track: unsafe { (self.fns.video_get_spu)(self.player) },
                audio_tracks: self.audio_tracks.clone(),
                subtitle_tracks: self.subtitle_tracks.clone(),
                // The wrapper supplies the persisted preference because the
                // LibVLC snapshot API does not expose media options.
                looping: false,
                confirmed: !matches!(
                    status,
                    PlaybackStatus::Pending | PlaybackStatus::Opening | PlaybackStatus::Buffering
                ),
                status,
                error: (status == PlaybackStatus::Error).then(|| self.input_error()),
            })
        }

        fn playback_status(&self) -> PlaybackStatus {
            if self.player.is_null() {
                return PlaybackStatus::Stopped;
            }
            // libvlc_state_t values are stable across the VLC 3.x API whose
            // symbols this wrapper loads dynamically.
            match unsafe { (self.fns.player_get_state)(self.player) } {
                0 => PlaybackStatus::Pending,
                1 => PlaybackStatus::Opening,
                2 => PlaybackStatus::Buffering,
                3 => PlaybackStatus::Playing,
                4 => PlaybackStatus::Paused,
                5 => PlaybackStatus::Stopped,
                6 => PlaybackStatus::Ended,
                7 => PlaybackStatus::Error,
                _ => PlaybackStatus::Error,
            }
        }

        fn input_error(&self) -> String {
            self.path.as_deref().map_or_else(
                || "LibVLC reported an input or playback error".to_string(),
                |path| format!("LibVLC reported an input or playback error for {path}"),
            )
        }

        fn ensure_usable_player(&self) -> Result<(), String> {
            if self.player.is_null() {
                return Err("No video is loaded".to_string());
            }
            if self.playback_status() == PlaybackStatus::Error {
                return Err(self.input_error());
            }
            Ok(())
        }

        unsafe fn read_tracks(&self, head: *mut TrackDescription) -> Vec<Track> {
            let mut tracks = Vec::new();
            let mut cursor = head;
            while !cursor.is_null() {
                let node = &*cursor;
                let name = if node.psz_name.is_null() {
                    format!("Track {}", node.i_id)
                } else {
                    CStr::from_ptr(node.psz_name).to_string_lossy().into_owned()
                };
                tracks.push(Track {
                    id: node.i_id,
                    name,
                });
                cursor = node.p_next;
            }
            if !head.is_null() {
                (self.fns.tracks_release)(head);
            }
            tracks
        }

        fn ensure_window(&mut self) -> Result<(), String> {
            if !self.hwnd.is_null() {
                if unsafe { IsWindow(self.hwnd) } == 0 {
                    self.last_surface_error_code = Some(1400);
                    return Err(
                        "Facial's embedded video child window is no longer valid".to_string()
                    );
                }
                if unsafe { GetParent(self.hwnd) } != self.parent {
                    self.last_surface_error_code = Some(1400);
                    return Err("Facial's embedded video child has the wrong parent".to_string());
                }
                return Ok(());
            }
            if self.parent.is_null() || unsafe { IsWindow(self.parent) } == 0 {
                self.last_surface_error_code = Some(1400);
                return Err("Facial video parent window is no longer valid".to_string());
            }
            unsafe {
                let style = GetWindowLongPtrW(self.parent, GWL_STYLE);
                if style & WS_CLIPCHILDREN as isize == 0 {
                    SetWindowLongPtrW(self.parent, GWL_STYLE, style | WS_CLIPCHILDREN as isize);
                    if GetWindowLongPtrW(self.parent, GWL_STYLE) & WS_CLIPCHILDREN as isize == 0 {
                        self.last_surface_error_code = last_win32_error_code();
                        return Err(format!(
                            "Windows could not enable child clipping on Facial: {}",
                            std::io::Error::last_os_error()
                        ));
                    }
                }
            }
            let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    class.as_ptr(),
                    std::ptr::null(),
                    // No SS_* paint style: this child is a neutral native host
                    // owned by LibVLC, not a STATIC black-rectangle control.
                    WS_CHILD | WS_CLIPSIBLINGS,
                    0,
                    0,
                    16,
                    16,
                    self.parent,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            };
            if hwnd.is_null() {
                self.last_surface_error_code = last_win32_error_code();
                return Err("Windows could not create the embedded video surface".to_string());
            }
            if unsafe { IsWindow(hwnd) } == 0 || unsafe { GetParent(hwnd) } != self.parent {
                unsafe { DestroyWindow(hwnd) };
                self.last_surface_error_code = Some(1400);
                return Err("Windows created an invalid embedded video child".to_string());
            }
            self.hwnd = hwnd;
            self.last_surface_error_code = None;
            Ok(())
        }

        pub fn show_at(&mut self, rect: egui::Rect, pixels_per_point: f32) -> Result<(), String> {
            self.show_clipped(rect, None, pixels_per_point)
        }

        /// Place the native child at `rect`, clipped to `clip` when the owning
        /// panel supplies one.
        ///
        /// WP-065: `show_at` previously received the raw tile rect with no
        /// intersection against the owning panel or its scroll viewport, and
        /// `SetWindowPos` is called with a NULL `hWndInsertAfter` and without
        /// `SWP_NOZORDER`, so the child is raised to the top of Z on every
        /// bounds change. A tile scrolled half out of view therefore painted a
        /// full-size video window over the toolbar and the Viewer. Clipping to
        /// the owner's visible rectangle is what stops that; an empty
        /// intersection hides the surface instead of parking it somewhere.
        pub fn show_clipped(
            &mut self,
            rect: egui::Rect,
            clip: Option<egui::Rect>,
            pixels_per_point: f32,
        ) -> Result<(), String> {
            if self.player.is_null() {
                return Ok(());
            }
            self.place_clipped(rect, clip, pixels_per_point).map(|_| ())
        }

        /// Position and show the child independently of decoder state. The
        /// deferred-start path calls this after assigning the HWND but before
        /// `libvlc_media_player_play`.
        fn place_clipped(
            &mut self,
            rect: egui::Rect,
            clip: Option<egui::Rect>,
            pixels_per_point: f32,
        ) -> Result<bool, String> {
            let Some(placement) = physical_surface_placement(rect, clip, pixels_per_point)? else {
                self.hide();
                super::playback_trace_phase("vlc.clip", "owner clip is empty; surface hidden");
                return Ok(false);
            };
            self.ensure_window()?;
            self.apply_clip_region(placement.clip_region_px)?;
            let bounds = placement.bounds_px;
            let [x, y, width, height] = bounds;
            let visible = unsafe { IsWindowVisible(self.hwnd) } != 0;
            if self.last_surface_bounds != Some(bounds) || !visible {
                let positioned = unsafe {
                    SetWindowPos(
                        self.hwnd,
                        std::ptr::null_mut(),
                        x,
                        y,
                        width,
                        height,
                        SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    )
                };
                if positioned == 0 {
                    // Applying a new region succeeds independently from moving
                    // the child. If positioning then fails, leaving the old
                    // visible child behind would expose a region that no
                    // longer matches the published bounds diagnostics. Fail
                    // closed and preserve the positioning error before the
                    // cleanup Win32 calls have a chance to replace it.
                    let error = std::io::Error::last_os_error();
                    let error_code = error.raw_os_error();
                    self.hide();
                    self.last_surface_error_code = error_code;
                    return Err(format!(
                        "Windows could not position the embedded video surface: {error}"
                    ));
                }
                self.last_surface_bounds = Some(bounds);
                super::playback_trace_phase(
                    "vlc.show_at",
                    &format!(
                        "x={x} y={y} w={width} h={height} clipped={}",
                        clip.is_some()
                    ),
                );
            }
            self.surface_visible = unsafe { IsWindowVisible(self.hwnd) } != 0;
            if !self.surface_visible {
                // Do not retain requested/clipped diagnostics for a surface
                // which Windows refused to show. Hiding also invalidates the
                // destination so no stale decoded frame survives the failure.
                self.hide();
                self.last_surface_error_code = Some(1400);
                return Err("Windows did not make the embedded video surface visible".to_string());
            }
            self.last_surface_clipped_bounds = Some(placement.visible_bounds_px);
            self.last_surface_error_code = None;
            Ok(true)
        }

        /// Apply a child-local Win32 region only when its geometry changes.
        /// `SetWindowRgn` owns a non-null HRGN after success; on failure the
        /// caller remains responsible for deleting it.
        fn apply_clip_region(&mut self, region: Option<[i32; 4]>) -> Result<(), String> {
            if self.last_surface_region == region {
                return Ok(());
            }
            let handle = match region {
                Some([left, top, right, bottom]) => unsafe {
                    CreateRectRgn(left, top, right, bottom)
                },
                None => std::ptr::null_mut(),
            };
            if region.is_some() && handle.is_null() {
                let error = std::io::Error::last_os_error();
                self.last_surface_error_code = error.raw_os_error();
                return Err(format!(
                    "Windows could not create the embedded video clipping region: {error}"
                ));
            }
            let applied = unsafe { SetWindowRgn(self.hwnd, handle, 1) };
            if applied == 0 {
                // Capture first: DeleteObject is required for caller-owned
                // failure handles and may itself overwrite the thread's last
                // Win32 error.
                let error = std::io::Error::last_os_error();
                let error_code = error.raw_os_error();
                if !handle.is_null() {
                    unsafe {
                        DeleteObject(handle);
                    }
                }
                self.last_surface_error_code = error_code;
                return Err(format!(
                    "Windows could not clip the embedded video surface: {error}"
                ));
            }
            self.last_surface_region = region;
            super::playback_trace_phase(
                "vlc.clip",
                &region.map_or_else(
                    || "full child region restored".to_string(),
                    |[left, top, right, bottom]| format!("region={left},{top},{right},{bottom}"),
                ),
            );
            Ok(())
        }

        /// Hide the native child and repaint the region it occupied.
        ///
        /// WP-065: this used to be gated on the cached `surface_visible` flag,
        /// so any divergence between the flag and the real window state turned
        /// `hide()` into a silent no-op that left a stale video frame on screen.
        /// The live `IsWindowVisible` result is authoritative. `SW_HIDE` also
        /// does not repaint whatever the child was covering, so the vacated
        /// parent rectangle is invalidated explicitly — without that, the last
        /// decoded frame survives until egui happens to repaint that region,
        /// which is what painted the previous video across the application.
        pub fn hide(&mut self) {
            if self.hwnd.is_null() {
                self.surface_visible = false;
                self.last_surface_bounds = None;
                self.last_surface_clipped_bounds = None;
                return;
            }
            let live_visible = unsafe { IsWindowVisible(self.hwnd) } != 0;
            let vacated = self.last_surface_bounds;
            if live_visible {
                unsafe { ShowWindow(self.hwnd, SW_HIDE) };
            }
            self.surface_visible = false;
            // Force the next placement to reissue SetWindowPos rather than
            // short-circuiting on unchanged bounds.
            self.last_surface_bounds = None;
            self.last_surface_clipped_bounds = None;
            if let (Some([x, y, width, height]), false) = (vacated, self.parent.is_null()) {
                let rect = RECT {
                    left: x,
                    top: y,
                    right: x.saturating_add(width),
                    bottom: y.saturating_add(height),
                };
                unsafe {
                    InvalidateRect(self.parent, &rect, 1);
                }
            }
            super::playback_trace_phase("vlc.hide", "surface hidden; vacated region invalidated");
        }

        pub fn stop(&mut self) {
            self.release_player();
            self.hide();
            self.path = None;
            super::playback_trace_phase("vlc.stop", "player released and surface hidden");
        }

        fn release_player(&mut self) {
            if !self.player.is_null() {
                unsafe {
                    (self.fns.player_stop)(self.player);
                    (self.fns.player_release)(self.player);
                }
                self.player = std::ptr::null_mut();
            }
        }
    }

    impl Drop for VlcRuntime {
        fn drop(&mut self) {
            self.release_player();
            if !self.hwnd.is_null() {
                unsafe {
                    DestroyWindow(self.hwnd);
                }
                self.hwnd = std::ptr::null_mut();
                self.surface_visible = false;
                self.last_surface_bounds = None;
                self.last_surface_clipped_bounds = None;
                self.last_surface_region = None;
            }
            if !self.instance.is_null() {
                unsafe {
                    (self.fns.release)(self.instance);
                }
                self.instance = std::ptr::null_mut();
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn installed_vlc_runtime_loads_every_required_symbol() {
            if let Some(dir) = resolve_vlc_dir() {
                let dll = dir.join("libvlc.dll");
                let library = unsafe {
                    Library::load_with_flags(&dll, 0x0000_0100 | 0x0000_1000)
                        .expect("installed LibVLC library loads")
                };
                let _fns = unsafe { VlcFns::load(&library) }
                    .expect("installed LibVLC exports every required symbol");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_with(status: PlaybackStatus) -> Snapshot {
        Snapshot {
            path: "sample.mp4".to_string(),
            playing: matches!(
                status,
                PlaybackStatus::Pending
                    | PlaybackStatus::Opening
                    | PlaybackStatus::Buffering
                    | PlaybackStatus::Playing
            ),
            time_ms: 100,
            length_ms: 1_000,
            volume: 75,
            audio_track: -1,
            subtitle_track: -1,
            audio_tracks: Vec::new(),
            subtitle_tracks: Vec::new(),
            looping: true,
            confirmed: !matches!(
                status,
                PlaybackStatus::Pending | PlaybackStatus::Opening | PlaybackStatus::Buffering
            ),
            status,
            error: None,
        }
    }

    #[cfg(windows)]
    #[test]
    fn external_open_preserves_exact_windows_unicode_path() {
        use std::os::windows::ffi::OsStringExt;
        let path = Path::new(r"Z:\Video\4K Video\café-日本語.mp4");
        let wide = windows_external_open_path_units(path);
        assert_eq!(wide.last(), Some(&0));
        let decoded = std::ffi::OsString::from_wide(&wide[..wide.len() - 1]);
        assert_eq!(decoded, path.as_os_str());
    }

    #[test]
    fn native_surface_geometry_scales_points_to_physical_pixels() {
        let rect = egui::Rect::from_min_max(egui::pos2(12.25, 20.5), egui::pos2(112.5, 70.75));
        assert_eq!(
            physical_surface_bounds(rect, 2.0).unwrap(),
            [25, 41, 201, 101]
        );
    }

    #[test]
    fn native_surface_geometry_rejects_non_finite_or_zero_scale() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 50.0));
        assert!(physical_surface_bounds(rect, 0.0).is_err());
        assert!(physical_surface_bounds(rect, f32::NAN).is_err());
        let invalid = egui::Rect::from_min_max(
            egui::pos2(f32::INFINITY, 0.0),
            egui::pos2(f32::INFINITY, 50.0),
        );
        assert!(physical_surface_bounds(invalid, 1.0).is_err());
    }

    #[test]
    fn partial_native_clip_preserves_child_geometry_and_uses_a_local_region() {
        let requested = egui::Rect::from_min_max(egui::pos2(10.0, 20.0), egui::pos2(110.0, 70.0));
        let clip = egui::Rect::from_min_max(egui::pos2(30.0, 25.0), egui::pos2(90.0, 65.0));
        assert_eq!(
            physical_surface_placement(requested, Some(clip), 2.0).unwrap(),
            Some(NativeSurfacePlacement {
                // The child stays at the complete requested 200x100 geometry.
                bounds_px: [20, 40, 200, 100],
                // Receipt/capture diagnostics expose the exact visible part in
                // parent-client coordinates as a separate value.
                visible_bounds_px: [60, 50, 120, 80],
                // Only the child-local visible pixels are admitted by Win32.
                clip_region_px: Some([40, 10, 160, 90]),
            })
        );
    }

    #[test]
    fn full_or_empty_native_clip_has_unambiguous_region_semantics() {
        let requested = egui::Rect::from_min_max(egui::pos2(10.0, 20.0), egui::pos2(110.0, 70.0));
        let full = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(200.0, 200.0));
        assert_eq!(
            physical_surface_placement(requested, Some(full), 1.0).unwrap(),
            Some(NativeSurfacePlacement {
                bounds_px: [10, 20, 100, 50],
                visible_bounds_px: [10, 20, 100, 50],
                clip_region_px: None,
            })
        );

        let disjoint = egui::Rect::from_min_max(egui::pos2(300.0, 300.0), egui::pos2(400.0, 400.0));
        assert_eq!(
            physical_surface_placement(requested, Some(disjoint), 1.0).unwrap(),
            None
        );
    }

    #[test]
    fn offscreen_native_child_keeps_full_bounds_and_reports_only_visible_pixels() {
        let requested = egui::Rect::from_min_max(egui::pos2(-20.0, -10.0), egui::pos2(80.0, 40.0));
        let framebuffer_clip =
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(640.0, 480.0));
        assert_eq!(
            physical_surface_placement(requested, Some(framebuffer_clip), 1.0).unwrap(),
            Some(NativeSurfacePlacement {
                bounds_px: [-20, -10, 100, 50],
                visible_bounds_px: [0, 0, 80, 40],
                clip_region_px: Some([20, 10, 100, 50]),
            })
        );
    }

    #[test]
    fn clipped_start_places_surface_before_invoking_libvlc_play() {
        let source = include_str!("video_player.rs");
        let runtime_start = source
            .find("pub fn play(\n            &mut self,\n            path: &Path,\n            loop_enabled")
            .expect("Windows runtime play implementation");
        let runtime = &source[runtime_start..];
        let placement = runtime
            .find("self.place_clipped")
            .expect("clipped start places the child");
        let play = runtime
            .find("vlc.player_play.begin")
            .expect("LibVLC play phase");
        assert!(
            placement < play,
            "LibVLC playback must not begin before current owner geometry is visible"
        );
    }

    #[test]
    fn zero_native_parent_handle_is_rejected_without_loading_vlc() {
        let mut player = VideoPlayer::default();
        assert!(player.set_parent_window_handle(0).is_err());
        let diagnostics = player.diagnostics();
        assert_eq!(diagnostics.surface.parent_hwnd, None);
        assert!(!diagnostics.surface.parent_valid);
        assert_eq!(diagnostics.surface.last_error_code, Some(1400));
    }

    #[test]
    fn vlc_vout_invalid_or_missing_override_retains_safe_wingdi_default() {
        assert_eq!(normalize_vlc_vout(None), "wingdi");
        assert_eq!(normalize_vlc_vout(Some(" typo ")), "wingdi");
        assert_eq!(normalize_vlc_vout(Some(" Direct3D11 ")), "direct3d11");
    }

    #[test]
    fn vlc_loop_option_stays_inside_documented_unsigned_range() {
        let option = vlc_repeat_media_option();
        let repeats = option
            .strip_prefix(":input-repeat=")
            .expect("input-repeat media option")
            .parse::<u16>()
            .expect("VLC repeat count must be unsigned 16-bit");
        assert_eq!(repeats, u16::MAX);
    }

    #[test]
    fn snapshot_poll_throttle_has_forced_bypass() {
        let now = Instant::now();
        let playing = snapshot_with(PlaybackStatus::Playing);
        let paused = snapshot_with(PlaybackStatus::Paused);
        assert!(!snapshot_poll_due(
            Some(&playing),
            Some(now - Duration::from_millis(49)),
            now,
            false,
        ));
        assert!(snapshot_poll_due(
            Some(&playing),
            Some(now - Duration::from_millis(50)),
            now,
            false,
        ));
        assert!(!snapshot_poll_due(
            Some(&paused),
            Some(now - Duration::from_millis(499)),
            now,
            false,
        ));
        assert!(snapshot_poll_due(Some(&paused), Some(now), now, true,));
    }

    #[test]
    fn fresh_snapshot_bypasses_recent_poll_without_loading_vlc() {
        let mut player = VideoPlayer::default();
        player.cached_snapshot = Some(snapshot_with(PlaybackStatus::Paused));
        player.last_snapshot_poll = Some(Instant::now());
        assert_eq!(player.diagnostics().poll_count, 0);
        let cached = player.snapshot().expect("cached snapshot");
        assert_eq!(cached.status, PlaybackStatus::Paused);
        assert_eq!(player.diagnostics().poll_count, 0);

        let fresh = player
            .snapshot_fresh()
            .expect("no runtime is not a poll failure")
            .expect("cached snapshot remains available");
        assert_eq!(fresh.status, PlaybackStatus::Paused);
        assert_eq!(player.diagnostics().poll_count, 1);
        assert_eq!(player.diagnostics().forced_poll_count, 1);
    }

    #[test]
    fn asynchronous_poll_error_is_retained_and_returned() {
        let mut player = VideoPlayer::default();
        player.cached_snapshot = Some(snapshot_with(PlaybackStatus::Opening));
        let mut failed = snapshot_with(PlaybackStatus::Error);
        failed.error = Some("input failed".to_string());
        let error = player
            .accept_polled_snapshot(Some(failed))
            .expect_err("LibVLC error state must reject a fresh receipt");
        assert_eq!(error, "input failed");
        assert_eq!(player.last_error(), Some("input failed"));
        let cached = player.cached_snapshot().expect("error snapshot retained");
        assert_eq!(cached.status, PlaybackStatus::Error);
        assert!(cached.confirmed);
        assert_eq!(
            player.diagnostics().last_status,
            Some(PlaybackStatus::Error)
        );
        assert_eq!(player.diagnostics().failure_count, 1);

        let mut repeated = snapshot_with(PlaybackStatus::Error);
        repeated.error = Some("input failed".to_string());
        let _ = player.accept_polled_snapshot(Some(repeated));
        assert_eq!(player.diagnostics().failure_count, 1);
    }

    #[test]
    fn optimistic_pause_remains_pending_until_libvlc_confirms_it() {
        let mut player = VideoPlayer::default();
        player.pending_confirmation.playing = Some(false);
        let still_playing = player
            .accept_polled_snapshot(Some(snapshot_with(PlaybackStatus::Playing)))
            .expect("healthy poll")
            .expect("snapshot");
        assert!(!still_playing.playing, "optimistic pause stays visible");
        assert!(!still_playing.confirmed);

        let paused = player
            .accept_polled_snapshot(Some(snapshot_with(PlaybackStatus::Paused)))
            .expect("healthy poll")
            .expect("snapshot");
        assert!(!paused.playing);
        assert!(paused.confirmed);
        assert!(player.pending_confirmation.is_empty());
    }

    #[test]
    fn repeated_explicit_transport_requests_never_toggle_the_target() {
        for _ in 0..2 {
            assert_eq!(
                playing_transition(PlaybackStatus::Playing, true, None).unwrap(),
                PlayingTransition::AlreadyConfirmed
            );
            assert_eq!(
                playing_transition(PlaybackStatus::Playing, false, None).unwrap(),
                PlayingTransition::Pause
            );
            assert_eq!(
                playing_transition(PlaybackStatus::Paused, false, None).unwrap(),
                PlayingTransition::AlreadyConfirmed
            );
            assert_eq!(
                playing_transition(PlaybackStatus::Paused, true, None).unwrap(),
                PlayingTransition::Play
            );
            assert_eq!(
                playing_transition(PlaybackStatus::Opening, true, None).unwrap(),
                PlayingTransition::AlreadyPending
            );
            assert_eq!(
                playing_transition(PlaybackStatus::Playing, false, Some(false)).unwrap(),
                PlayingTransition::AlreadyPending
            );
            assert_eq!(
                playing_transition(PlaybackStatus::Opening, true, Some(true)).unwrap(),
                PlayingTransition::AlreadyPending
            );
        }
    }

    #[test]
    fn pending_pause_reversed_to_play_forces_direct_resume_or_play() {
        assert_eq!(
            playing_transition(PlaybackStatus::Playing, true, Some(false)).unwrap(),
            PlayingTransition::Resume
        );
        assert_eq!(
            playing_transition(PlaybackStatus::Opening, true, Some(false)).unwrap(),
            PlayingTransition::Resume
        );
        assert_eq!(
            playing_transition(PlaybackStatus::Paused, true, Some(false)).unwrap(),
            PlayingTransition::Play
        );

        let mut player = VideoPlayer::default();
        player.cached_snapshot = Some(snapshot_with(PlaybackStatus::Playing));
        player.accept_playing_request(false, false, PlaybackStatus::Playing);
        assert_eq!(player.pending_confirmation.playing, Some(false));
        player.accept_playing_request(true, false, PlaybackStatus::Playing);
        let reversed = player.cached_snapshot().expect("reversed play snapshot");
        assert!(reversed.playing);
        assert!(!reversed.confirmed);
        assert_eq!(player.pending_confirmation.playing, Some(true));
    }

    #[test]
    fn pending_play_reversed_to_pause_forces_direct_pause() {
        for observed in [
            PlaybackStatus::Pending,
            PlaybackStatus::Opening,
            PlaybackStatus::Buffering,
            PlaybackStatus::Playing,
            PlaybackStatus::Paused,
        ] {
            assert_eq!(
                playing_transition(observed, false, Some(true)).unwrap(),
                PlayingTransition::Pause
            );
        }

        let mut player = VideoPlayer::default();
        player.cached_snapshot = Some(snapshot_with(PlaybackStatus::Paused));
        player.accept_playing_request(true, false, PlaybackStatus::Paused);
        assert_eq!(player.pending_confirmation.playing, Some(true));
        player.accept_playing_request(false, false, PlaybackStatus::Paused);
        let reversed = player.cached_snapshot().expect("reversed pause snapshot");
        assert!(!reversed.playing);
        assert!(!reversed.confirmed);
        assert_eq!(player.pending_confirmation.playing, Some(false));
    }

    #[test]
    fn opening_toggle_requests_pause_and_rapid_second_toggle_requests_resume() {
        let mut player = VideoPlayer::default();
        player.cached_snapshot = Some(snapshot_with(PlaybackStatus::Opening));
        player.pending_confirmation.playing = Some(true);

        let pause_target = toggled_playing_target(player.cached_snapshot.as_ref()).unwrap();
        assert!(!pause_target);
        assert_eq!(
            playing_transition(
                PlaybackStatus::Opening,
                pause_target,
                player.pending_confirmation.playing,
            )
            .unwrap(),
            PlayingTransition::Pause
        );
        player.accept_playing_request(false, false, PlaybackStatus::Opening);

        let resume_target = toggled_playing_target(player.cached_snapshot.as_ref()).unwrap();
        assert!(resume_target);
        assert_eq!(
            playing_transition(
                PlaybackStatus::Opening,
                resume_target,
                player.pending_confirmation.playing,
            )
            .unwrap(),
            PlayingTransition::Resume
        );
        player.accept_playing_request(true, false, PlaybackStatus::Opening);
        assert!(player.cached_snapshot().unwrap().playing);
        assert_eq!(player.pending_confirmation.playing, Some(true));
    }

    #[test]
    fn stopped_and_ended_confirm_pending_not_playing() {
        for status in [PlaybackStatus::Stopped, PlaybackStatus::Ended] {
            let mut player = VideoPlayer::default();
            player.pending_confirmation.playing = Some(false);
            let snapshot = player
                .accept_polled_snapshot(Some(snapshot_with(status)))
                .expect("healthy terminal-state poll")
                .expect("snapshot");
            assert!(!snapshot.playing);
            assert!(snapshot.confirmed);
            assert!(player.pending_confirmation.is_empty());
        }
    }

    #[test]
    fn ended_does_not_confirm_a_pending_play_request() {
        assert_eq!(
            playing_transition(PlaybackStatus::Ended, true, Some(true)).unwrap(),
            PlayingTransition::AlreadyPending
        );
        let mut player = VideoPlayer::default();
        player.pending_confirmation.playing = Some(true);
        let snapshot = player
            .accept_polled_snapshot(Some(snapshot_with(PlaybackStatus::Ended)))
            .expect("healthy terminal-state poll")
            .expect("snapshot");
        assert!(snapshot.playing, "optimistic play target stays visible");
        assert!(!snapshot.confirmed);
        assert_eq!(player.pending_confirmation.playing, Some(true));
    }

    #[test]
    fn repeated_explicit_pause_and_play_preserve_pending_target_until_confirmation() {
        let mut player = VideoPlayer::default();
        player.cached_snapshot = Some(snapshot_with(PlaybackStatus::Playing));
        player.accept_playing_request(false, false, PlaybackStatus::Playing);
        player.accept_playing_request(false, false, PlaybackStatus::Playing);
        let pending_pause = player.cached_snapshot().expect("pending pause snapshot");
        assert!(!pending_pause.playing);
        assert!(!pending_pause.confirmed);
        assert_eq!(player.pending_confirmation.playing, Some(false));

        player
            .accept_polled_snapshot(Some(snapshot_with(PlaybackStatus::Paused)))
            .expect("pause confirmation");
        player.accept_playing_request(false, true, PlaybackStatus::Paused);
        let paused = player.cached_snapshot().expect("confirmed pause snapshot");
        assert!(!paused.playing);
        assert!(paused.confirmed);

        player.accept_playing_request(true, false, PlaybackStatus::Paused);
        player.accept_playing_request(true, false, PlaybackStatus::Paused);
        let pending_play = player.cached_snapshot().expect("pending play snapshot");
        assert!(pending_play.playing);
        assert!(!pending_play.confirmed);
        assert_eq!(player.pending_confirmation.playing, Some(true));

        let playing = player
            .accept_polled_snapshot(Some(snapshot_with(PlaybackStatus::Playing)))
            .expect("play confirmation")
            .expect("playing snapshot");
        assert!(playing.playing);
        assert!(playing.confirmed);
        assert!(player.pending_confirmation.is_empty());
    }

    #[test]
    fn unloaded_setters_return_errors_without_loading_vlc() {
        let mut player = VideoPlayer::default();
        assert_eq!(
            player.set_playing(true),
            Err("No video is loaded".to_string())
        );
        assert_eq!(player.set_time(1), Err("No video is loaded".to_string()));
        assert_eq!(
            player.set_volume(100),
            Err("No video is loaded".to_string())
        );
        assert_eq!(
            player.set_audio_track(1),
            Err("No video is loaded".to_string())
        );
        assert_eq!(
            player.set_subtitle_track(1),
            Err("No video is loaded".to_string())
        );
    }

    #[test]
    fn remote_cache_override_is_opt_in_and_bounded() {
        assert_eq!(parse_remote_file_cache_ms("50"), Some(50));
        assert_eq!(parse_remote_file_cache_ms(" 2500 "), Some(2500));
        assert_eq!(parse_remote_file_cache_ms("10000"), Some(10_000));
        assert_eq!(parse_remote_file_cache_ms("49"), None);
        assert_eq!(parse_remote_file_cache_ms("10001"), None);
        assert_eq!(parse_remote_file_cache_ms("not-a-number"), None);
    }

    #[cfg(windows)]
    #[test]
    fn unc_paths_are_classified_as_remote_without_touching_the_share() {
        assert!(is_remote_media_path(Path::new(
            r"\\server\share\folder\video.mp4"
        )));
        assert!(!is_remote_media_path(Path::new(r"relative\video.mp4")));
    }

    #[test]
    fn installed_vlc_directory_contains_runtime_and_executable_when_found() {
        if let Some(dir) = resolve_vlc_dir() {
            assert!(dir.join("libvlc.dll").is_file());
            assert!(dir.join("vlc.exe").is_file());
        }
    }
}
