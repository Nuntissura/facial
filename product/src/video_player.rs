//! Lazy LibVLC integration for the media preview.
//!
//! No VLC DLL is loaded while folders are scanned or thumbnails scroll. The
//! runtime and native child surface are created only after the operator presses
//! Play on a selected video. VLC remains an optional runtime dependency.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Track {
    pub id: i32,
    pub name: String,
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
}

pub struct VideoPlayer {
    #[cfg(windows)]
    runtime: Option<windows_impl::VlcRuntime>,
    last_error: Option<String>,
    loop_enabled: bool,
}

impl Default for VideoPlayer {
    fn default() -> Self {
        Self {
            #[cfg(windows)]
            runtime: None,
            last_error: None,
            loop_enabled: true,
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
        if !path.is_file() {
            return Err("Video no longer exists".to_string());
        }
        #[cfg(windows)]
        {
            if self.runtime.is_none() {
                self.runtime = Some(windows_impl::VlcRuntime::load()?);
            }
            let result = self
                .runtime
                .as_mut()
                .expect("runtime initialized")
                .play(path, self.loop_enabled);
            self.last_error = result.as_ref().err().cloned();
            return result;
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            Err("Embedded VLC preview is currently available on Windows".to_string())
        }
    }

    pub fn toggle_pause(&mut self) -> Result<(), String> {
        #[cfg(windows)]
        {
            return self
                .runtime
                .as_mut()
                .ok_or_else(|| "No video is loaded".to_string())?
                .toggle_pause();
        }
        #[cfg(not(windows))]
        Err("No video is loaded".to_string())
    }

    pub fn set_time(&mut self, time_ms: i64) {
        #[cfg(windows)]
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.set_time(time_ms);
        }
    }

    /// Change the persisted playback preference. When a video is already open,
    /// recreate its LibVLC media with the new per-media option and restore the
    /// current timestamp so the operator does not lose their place.
    pub fn set_loop(&mut self, enabled: bool) -> Result<(), String> {
        if self.loop_enabled == enabled {
            return Ok(());
        }
        self.loop_enabled = enabled;
        #[cfg(windows)]
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.set_loop(enabled)?;
        }
        Ok(())
    }

    pub fn set_volume(&mut self, volume: i32) {
        #[cfg(windows)]
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.set_volume(volume);
        }
    }

    pub fn set_audio_track(&mut self, id: i32) {
        #[cfg(windows)]
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.set_audio_track(id);
        }
    }

    pub fn set_subtitle_track(&mut self, id: i32) {
        #[cfg(windows)]
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.set_subtitle_track(id);
        }
    }

    pub fn snapshot(&mut self) -> Option<Snapshot> {
        #[cfg(windows)]
        {
            let mut snapshot = self
                .runtime
                .as_mut()
                .and_then(|runtime| runtime.snapshot())?;
            snapshot.looping = self.loop_enabled;
            return Some(snapshot);
        }
        #[cfg(not(windows))]
        None
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
    }
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
    use super::{resolve_vlc_dir, Snapshot, Track};
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
            })
        }

        pub fn path(&self) -> Option<&str> {
            self.path.as_deref()
        }

        pub fn play(&mut self, path: &Path, loop_enabled: bool) -> Result<(), String> {
            self.ensure_window()?;
            self.release_player();
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
            let was_playing =
                !self.player.is_null() && unsafe { (self.fns.player_is_playing)(self.player) != 0 };
            self.play(Path::new(&path), enabled)?;
            self.set_time(time_ms);
            if !was_playing {
                self.toggle_pause()?;
            }
            Ok(())
        }

        pub fn toggle_pause(&mut self) -> Result<(), String> {
            if self.player.is_null() {
                return Err("No video is loaded".to_string());
            }
            let playing = unsafe { (self.fns.player_is_playing)(self.player) != 0 };
            if playing {
                unsafe { (self.fns.player_set_pause)(self.player, 1) };
            } else if unsafe { (self.fns.player_play)(self.player) } != 0 {
                return Err("LibVLC could not resume playback".to_string());
            }
            Ok(())
        }

        pub fn set_time(&mut self, value: i64) {
            if !self.player.is_null() {
                unsafe { (self.fns.player_set_time)(self.player, value.max(0)) };
            }
        }

        pub fn set_volume(&mut self, value: i32) {
            if !self.player.is_null() {
                unsafe {
                    (self.fns.audio_set_volume)(self.player, value.clamp(0, 125));
                }
            }
        }

        pub fn set_audio_track(&mut self, id: i32) {
            if !self.player.is_null() {
                unsafe {
                    (self.fns.audio_set_track)(self.player, id);
                }
            }
        }

        pub fn set_subtitle_track(&mut self, id: i32) {
            if !self.player.is_null() {
                unsafe {
                    (self.fns.video_set_spu)(self.player, id);
                }
            }
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
                playing: unsafe { (self.fns.player_is_playing)(self.player) != 0 },
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
            })
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
                ShowWindow(self.hwnd, SW_SHOW);
            }
            Ok(())
        }

        pub fn hide(&mut self) {
            if !self.hwnd.is_null() {
                unsafe {
                    ShowWindow(self.hwnd, SW_HIDE);
                }
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

    #[test]
    fn installed_vlc_directory_contains_runtime_and_executable_when_found() {
        if let Some(dir) = resolve_vlc_dir() {
            assert!(dir.join("libvlc.dll").is_file());
            assert!(dir.join("vlc.exe").is_file());
        }
    }
}
