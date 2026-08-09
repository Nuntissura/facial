//! Lazy LibVLC integration for the media preview.
//!
//! No VLC DLL is loaded while folders are scanned or thumbnails scroll. The
//! runtime and native child surface are created only after the operator presses
//! Play on a selected video. VLC remains an optional runtime dependency.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

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
        }
    }
}

impl VideoPlayer {
    pub fn available() -> bool {
        resolve_vlc_dir().is_some()
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
        #[cfg(windows)]
        {
            let started = Instant::now();
            if self.runtime.is_none() {
                match windows_impl::VlcRuntime::load() {
                    Ok(runtime) => self.runtime = Some(runtime),
                    Err(error) => {
                        self.last_error = Some(error.clone());
                        self.record_command(started.elapsed());
                        self.diagnostics.failure_count =
                            self.diagnostics.failure_count.saturating_add(1);
                        return Err(error);
                    }
                }
            }
            let result = self
                .runtime
                .as_mut()
                .expect("runtime initialized")
                .play(path, self.loop_enabled);
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
            let _ = path;
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
        self.diagnostics
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
        #[cfg(windows)]
        {
            return self
                .runtime
                .as_mut()
                .ok_or_else(|| "No video is loaded".to_string())?
                .show_at(rect_points, pixels_per_point);
        }
        #[cfg(not(windows))]
        {
            let _ = (rect_points, pixels_per_point);
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

#[cfg(windows)]
fn is_remote_media_path(path: &Path) -> bool {
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
fn is_remote_media_path(_path: &Path) -> bool {
    false
}

pub fn open_in_vlc(path: &Path) -> Result<(), String> {
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

#[cfg(windows)]
pub fn open_with_dialog(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
    use windows_sys::Win32::UI::Shell::{
        SHOpenWithDialog, OAIF_ALLOW_REGISTRATION, OAIF_EXEC, OPENASINFO,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let info = OPENASINFO {
        pcszFile: wide.as_ptr(),
        pcszClass: std::ptr::null(),
        oaifInFlags: OAIF_ALLOW_REGISTRATION | OAIF_EXEC,
    };
    let result = unsafe { SHOpenWithDialog(GetActiveWindow(), &info) };
    if result >= 0 {
        Ok(())
    } else {
        Err(format!("Windows app selector failed (HRESULT {result:#x})"))
    }
}

#[cfg(not(windows))]
pub fn open_with_dialog(_path: &Path) -> Result<(), String> {
    Err("The app selector is currently available on Windows".to_string())
}

#[cfg(windows)]
mod windows_impl {
    use super::{
        configured_remote_file_cache_ms, is_remote_media_path, playing_transition, resolve_vlc_dir,
        PlaybackStatus, PlayingTransition, Snapshot, Track,
    };
    use libloading::os::windows::Library;
    use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
    use std::path::Path;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos,
        ShowWindow, GWL_STYLE, SWP_NOACTIVATE, SW_HIDE, SW_SHOW, WS_CHILD, WS_CLIPCHILDREN,
        WS_CLIPSIBLINGS, WS_VISIBLE,
    };

    type VlcPtr = *mut c_void;

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
        surface_visible: bool,
    }

    impl VlcRuntime {
        pub fn load() -> Result<Self, String> {
            let dir = resolve_vlc_dir().ok_or_else(|| {
                "VLC was not found; install VLC or set FACIAL_VLC_DIR to its folder".to_string()
            })?;
            let dll = dir.join("libvlc.dll");
            let library = unsafe {
                Library::load_with_flags(&dll, 0x0000_0100 | 0x0000_1000)
                    .map_err(|error| format!("load {}: {error}", dll.display()))?
            };
            let fns = unsafe { VlcFns::load(&library)? };
            let mut option_values = vec!["--no-video-title-show", "--quiet", "--no-stats"];
            if std::env::var("FACIAL_TEST_SILENT")
                .ok()
                .is_some_and(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
            {
                option_values.push("--no-audio");
            }
            let options = option_values
                .into_iter()
                .map(|value| CString::new(value).expect("static VLC option"))
                .collect::<Vec<_>>();
            let option_ptrs = options
                .iter()
                .map(|value| value.as_ptr())
                .collect::<Vec<_>>();
            let instance = unsafe { (fns.new)(option_ptrs.len() as c_int, option_ptrs.as_ptr()) };
            if instance.is_null() {
                return Err("LibVLC could not initialize".to_string());
            }
            Ok(Self {
                _library: library,
                fns,
                instance,
                player: std::ptr::null_mut(),
                hwnd: std::ptr::null_mut(),
                parent: std::ptr::null_mut(),
                path: None,
                audio_tracks: Vec::new(),
                subtitle_tracks: Vec::new(),
                last_track_refresh: None,
                last_surface_bounds: None,
                surface_visible: false,
            })
        }

        pub fn path(&self) -> Option<&str> {
            self.path.as_deref()
        }

        pub fn play(&mut self, path: &Path, loop_enabled: bool) -> Result<(), String> {
            self.ensure_window()?;
            self.release_player();
            self.path = None;
            let path_text = path.to_string_lossy().to_string();
            let c_path = CString::new(path_text.as_bytes())
                .map_err(|_| "Video path contains an unsupported NUL character".to_string())?;
            let media = unsafe { (self.fns.media_new_path)(self.instance, c_path.as_ptr()) };
            if media.is_null() {
                return Err("LibVLC could not create media for this path".to_string());
            }
            if loop_enabled {
                // LibVLC accepts per-media input options here; -1 is the VLC
                // convention for repeat forever. Applying it before creating
                // the media player keeps this local to Facial's preview.
                let repeat = CString::new(":input-repeat=-1").expect("static VLC option");
                unsafe { (self.fns.media_add_option)(media, repeat.as_ptr()) };
            }
            if is_remote_media_path(path) {
                if let Some(milliseconds) = configured_remote_file_cache_ms() {
                    let cache = CString::new(format!(":file-caching={milliseconds}"))
                        .expect("numeric VLC cache option");
                    unsafe { (self.fns.media_add_option)(media, cache.as_ptr()) };
                }
            }
            let player = unsafe { (self.fns.player_new_from_media)(media) };
            unsafe { (self.fns.media_release)(media) };
            if player.is_null() {
                return Err("LibVLC could not create a media player".to_string());
            }
            self.player = player;
            unsafe { (self.fns.player_set_hwnd)(self.player, self.hwnd.cast()) };
            if unsafe { (self.fns.player_play)(self.player) } != 0 {
                self.release_player();
                return Err("LibVLC rejected playback".to_string());
            }
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
            self.play(Path::new(&path), enabled)?;
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
                return Ok(());
            }
            let parent = unsafe { GetActiveWindow() };
            if parent.is_null() {
                return Err(
                    "Could not identify the Facial window for embedded playback".to_string()
                );
            }
            unsafe {
                let style = GetWindowLongPtrW(parent, GWL_STYLE);
                if style & WS_CLIPCHILDREN as isize == 0 {
                    SetWindowLongPtrW(parent, GWL_STYLE, style | WS_CLIPCHILDREN as isize);
                }
            }
            let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    class.as_ptr(),
                    std::ptr::null(),
                    WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | 0x0000_0004,
                    0,
                    0,
                    16,
                    16,
                    parent,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            };
            if hwnd.is_null() {
                return Err("Windows could not create the embedded video surface".to_string());
            }
            self.parent = parent;
            self.hwnd = hwnd;
            Ok(())
        }

        pub fn show_at(&mut self, rect: egui::Rect, pixels_per_point: f32) -> Result<(), String> {
            if self.player.is_null() {
                return Ok(());
            }
            self.ensure_window()?;
            let ppp = pixels_per_point.max(0.1);
            let x = (rect.min.x * ppp).round() as i32;
            let y = (rect.min.y * ppp).round() as i32;
            let width = (rect.width() * ppp).round().max(1.0) as i32;
            let height = (rect.height() * ppp).round().max(1.0) as i32;
            let bounds = [x, y, width, height];
            if self.last_surface_bounds != Some(bounds) {
                unsafe {
                    SetWindowPos(
                        self.hwnd,
                        std::ptr::null_mut(),
                        x,
                        y,
                        width,
                        height,
                        SWP_NOACTIVATE,
                    );
                }
                self.last_surface_bounds = Some(bounds);
            }
            if !self.surface_visible {
                unsafe { ShowWindow(self.hwnd, SW_SHOW) };
                self.surface_visible = true;
            }
            Ok(())
        }

        pub fn hide(&mut self) {
            if !self.hwnd.is_null() && self.surface_visible {
                unsafe { ShowWindow(self.hwnd, SW_HIDE) };
                self.surface_visible = false;
            }
        }

        pub fn stop(&mut self) {
            self.release_player();
            self.hide();
            self.path = None;
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
            if resolve_vlc_dir().is_some() {
                let runtime = VlcRuntime::load().expect("installed LibVLC runtime loads");
                assert!(!runtime.instance.is_null());
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
