#![windows_subsystem = "windows"]
//! Ferrocap: clip it after it happens.
//!
//! A tiny Windows replay-buffer app. Everything lives in this one file, split into modules:
//!
//!   settings  persisted settings (JSON in %APPDATA%\Ferrocap)
//!   sys       Win32 helpers: ffmpeg lookup, job object, monitors, hotkey names, sounds
//!   procs     game & app detection (Roblox, Minecraft, Discord…)
//!   audio     WASAPI capture (system, mic, per-app) -> named pipes
//!   engine    GPU capture -> hardware encoder -> RAM ring buffer -> MP4
//!   hotkeys   global hotkeys thread
//!   tray      tray icon (right-click opens the custom menu drawn in `ui`)
//!   shots     lossless PNG screenshots
//!   library   clips & screenshots with thumbnails
//!   editor    trim / cut / crop / track mute / frame blending / export
//!   app       shared state between UI, tray and hotkeys
//!   theme     palettes, fonts and hand-drawn widgets
//!   ui        the window: Live, Library, Editor, Settings, tray menu, toasts


use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub static START_HIDDEN: AtomicBool = AtomicBool::new(false);

/// Warnings and errors from Ferrocap and its libraries go to %LOCALAPPDATA%\Ferrocap\app.log.
struct FileLog;
static FILE_LOG: FileLog = FileLog;
impl log::Log for FileLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Warn
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            use std::io::Write as _;
            let dir = settings::cache_dir();
            let _ = std::fs::create_dir_all(&dir);
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("app.log")) {
                let _ = writeln!(f, "[{}] {} {}: {}", sys::timestamp(), r.level(), r.target(), r.args());
            }
        }
    }
    fn flush(&self) {}
}

fn main() {
    // write panics to %LOCALAPPDATA%\Ferrocap\crash.log (there's no console in a windows-subsystem app)
    std::panic::set_hook(Box::new(|info| {
        let dir = settings::cache_dir();
        let _ = std::fs::create_dir_all(&dir);
        let bt = std::backtrace::Backtrace::force_capture();
        let msg = format!("[{}] {info}
{bt}

", sys::timestamp());
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("crash.log")) {
            let _ = f.write_all(msg.as_bytes());
        }
    }));
    if let Some(spec) = std::env::args().find_map(|a| a.strip_prefix("--snip-render-test=").map(String::from)) {
        snip::render_test(&spec);
        return;
    }
    if let Some(tid) = std::env::args().find_map(|a| a.strip_prefix("--keyhook=").and_then(|t| t.parse().ok())) {
        hotkeys::run_keyhook(tid);
        return;
    }
    log::set_logger(&FILE_LOG).ok();
    log::set_max_level(log::LevelFilter::Warn);
    sys::set_dpi_aware();
    if !sys::claim_single_instance() {
        return; // the running instance has been asked to show itself
    }
    let settings = settings::Settings::load();
    let start_hidden = std::env::args().any(|a| a == "--tray") && settings.start_minimized;
    START_HIDDEN.store(start_hidden, Ordering::SeqCst);
    if settings.start_with_windows {
        sys::set_start_with_windows(true); // keep the path current if the exe moved
    }
    let core = app::init(settings);
    hotkeys::spawn();
    if let Some(f) = std::env::args().find_map(|a| a.strip_prefix("--snip-edit=").map(std::path::PathBuf::from)) {
        snip::open_file(f);
    }
    if std::env::args().any(|a| a == "--snip") {
        snip::trigger(1500, false);
    }
    update::cleanup_old();
    update::spawn_auto();
    if std::env::args().any(|a| a == "--updated") {
        core.toast(app::ToastKind::Clip, "Ferrocap updated", format!("You're on version {}.", release::VERSION), None);
    }
    discord::spawn();
    procs::spawn(|| {
        let c = app::core();
        c.engine.on_detection();
        c.repaint();
    });
    sys::on_second_instance(|| app::core().request_show());

    let (rgba, width, height) = tray::rounded_rgba(64);
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Ferrocap")
            .with_app_id("Ferrocap")
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([1180.0, 760.0])
            .with_decorations(false)
            .with_resizable(true)
            .with_visible(!start_hidden)
            .with_icon(Arc::new(eframe::egui::IconData { rgba, width, height })),
        centered: true,
        ..Default::default()
    };
    let _ = eframe::run_native(
        "Ferrocap",
        options,
        Box::new(move |cc| {
            let _ = core.ctx.set(cc.egui_ctx.clone());
            tray::create();
            Ok(Box::new(ui::FerrocapApp::new(cc)))
        }),
    );
    app::core().engine.shutdown();
}

// ================================================================================================
// settings
// ================================================================================================

mod settings {
    use serde::{Deserialize, Serialize};
    use std::path::PathBuf;

    #[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Hotkey {
        pub vk: u32,
        pub ctrl: bool,
        pub shift: bool,
        pub alt: bool,
    }

    impl Hotkey {
        pub const fn key(vk: u32) -> Self {
            Self { vk, ctrl: false, shift: false, alt: false }
        }
        pub fn parts(&self) -> Vec<String> {
            let mut v = Vec::new();
            if self.ctrl {
                v.push("Ctrl".into());
            }
            if self.shift {
                v.push("Shift".into());
            }
            if self.alt {
                v.push("Alt".into());
            }
            v.push(crate::sys::vk_name(self.vk));
            v
        }
        pub fn label(&self) -> String {
            self.parts().join(" + ")
        }
    }

    #[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Quality {
        Low,
        Balanced,
        High,
        Ultra,
    }

    impl Quality {
        pub const ALL: [Quality; 4] = [Quality::Low, Quality::Balanced, Quality::High, Quality::Ultra];
        pub fn name(self) -> &'static str {
            match self {
                Quality::Low => "Low",
                Quality::Balanced => "Balanced",
                Quality::High => "High",
                Quality::Ultra => "Ultra",
            }
        }
        fn base_mbps(self) -> f64 {
            match self {
                Quality::Low => 9.0,
                Quality::Balanced => 18.0,
                Quality::High => 34.0,
                Quality::Ultra => 60.0,
            }
        }
    }

    #[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
    pub enum ShotMode {
        Screen,
        Window,
    }

    /// What gets recorded.
    #[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Source {
        /// The whole monitor.
        Display,
        /// Follow the detected game's window; falls back to the display when no game is running.
        Game,
    }

    #[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Blend {
        Off,
        Light,
        Strong,
    }

    #[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
    pub enum EncoderPref {
        Auto,
        Nvenc,
        Amf,
        Qsv,
        Cpu,
    }

    impl EncoderPref {
        pub fn codec(self) -> Option<&'static str> {
            match self {
                EncoderPref::Auto => None,
                EncoderPref::Nvenc => Some("h264_nvenc"),
                EncoderPref::Amf => Some("h264_amf"),
                EncoderPref::Qsv => Some("h264_qsv"),
                EncoderPref::Cpu => Some("libx264"),
            }
        }
    }

    #[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
    pub enum ThemeId {
        Ferro,
        Sable,
        Cobalt,
        Verdant,
    }

    impl ThemeId {
        pub const ALL: [ThemeId; 4] = [ThemeId::Ferro, ThemeId::Sable, ThemeId::Cobalt, ThemeId::Verdant];
        pub fn name(self) -> &'static str {
            match self {
                ThemeId::Ferro => "Ferro",
                ThemeId::Sable => "Sable",
                ThemeId::Cobalt => "Cobalt",
                ThemeId::Verdant => "Verdant",
            }
        }
    }

    /// An application whose audio is recorded on its own track.
    #[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
    pub struct AppTrack {
        /// lowercase exe name, or `GAME` for "whatever game is detected"
        pub exe: String,
        pub label: String,
        pub enabled: bool,
    }

    pub const GAME: &str = "<game>";

    #[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
    #[serde(default)]
    pub struct Settings {
        pub clip_key: Hotkey,
        pub shot_key: Hotkey,
        pub buffer_secs: u32,
        pub quality: Quality,
        pub fps: u32,
        pub monitor: u32,
        pub show_cursor: bool,
        /// mark clips as 16:9 when capturing a stretched resolution (e.g. 1440x1080), like in game
        pub stretch_169: bool,
        pub source: Source,
        pub game_only: bool,
        pub extra_games: Vec<String>,
        pub desktop_audio: bool,
        pub mic: bool,
        pub mic_device: Option<String>,
        pub app_tracks: Vec<AppTrack>,
        pub clips_dir: PathBuf,
        pub shots_dir: PathBuf,
        pub shot_mode: ShotMode,
        /// Win+Shift+S opens Ferrocap's snip instead of Windows' Snipping Tool
        pub snip_override: bool,
        /// Win+Shift+S snips go straight to the clipboard instead of opening the editor
        pub snip_copy_only: bool,
        /// open the editor after an F9 screenshot
        pub shot_editor: bool,
        pub copy_shot: bool,
        pub start_with_windows: bool,
        pub start_minimized: bool,
        pub sound: bool,
        /// small on-screen badge while a clip saves (works while Ferrocap is in the tray)
        pub save_hud: bool,
        /// Discord Rich Presence ("Clipping Roblox")
        pub discord_rpc: bool,
        pub discord_app_id: String,
        pub paused: bool,
        pub blend: Blend,
        pub blend_fps: u32,
        pub blend_on_save: bool,
        pub encoder_pref: EncoderPref,
        pub theme: ThemeId,
        /// Cached result of encoder auto-detection.
        pub encoder: Option<String>,
    }

    pub const BUFFER_STEPS: [u32; 12] = [5, 10, 15, 20, 30, 45, 60, 90, 120, 180, 240, 300];
    pub const FPS_STEPS: [u32; 5] = [30, 60, 120, 144, 240];

    impl Default for Settings {
        fn default() -> Self {
            let videos = dirs::video_dir().unwrap_or_else(|| home().join("Videos"));
            let pics = dirs::picture_dir().unwrap_or_else(|| home().join("Pictures"));
            Self {
                clip_key: Hotkey::key(0x77), // F8
                shot_key: Hotkey::key(0x78), // F9
                buffer_secs: 30,
                quality: Quality::Balanced,
                fps: 60,
                monitor: 0,
                show_cursor: true,
                stretch_169: false,
                source: Source::Display,
                game_only: false,
                extra_games: vec![],
                desktop_audio: true,
                mic: false,
                mic_device: None,
                app_tracks: vec![
                    AppTrack { exe: GAME.into(), label: "Game".into(), enabled: true },
                    AppTrack { exe: "discord.exe".into(), label: "Discord".into(), enabled: true },
                ],
                clips_dir: videos.join("Ferrocap"),
                shots_dir: pics.join("Ferrocap"),
                shot_mode: ShotMode::Screen,
                snip_override: true,
                snip_copy_only: false,
                shot_editor: false,
                copy_shot: true,
                start_with_windows: false,
                start_minimized: true,
                sound: true,
                save_hud: true,
                discord_rpc: true,
                discord_app_id: String::new(),
                paused: false,
                blend: Blend::Off,
                blend_fps: 0,
                blend_on_save: false,
                encoder_pref: EncoderPref::Auto,
                theme: ThemeId::Ferro,
                encoder: None,
            }
        }
    }

    fn home() -> PathBuf {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("C:\\"))
    }

    pub fn config_dir() -> PathBuf {
        dirs::config_dir().unwrap_or_else(home).join("Ferrocap")
    }

    pub fn cache_dir() -> PathBuf {
        dirs::cache_dir().unwrap_or_else(home).join("Ferrocap")
    }

    /// Everything that requires restarting the capture pipeline when it changes.
    #[derive(PartialEq, Clone)]
    pub struct CaptureKey {
        fps: u32,
        quality: Quality,
        monitor: u32,
        cursor: bool,
        source: Source,
        game_only: bool,
        desktop: bool,
        mic: bool,
        mic_device: Option<String>,
        apps: Vec<AppTrack>,
        encoder: Option<String>,
        pref: EncoderPref,
        paused: bool,
    }

    impl Settings {
        pub fn load() -> Self {
            let p = config_dir().join("settings.json");
            std::fs::read_to_string(p)
                .ok()
                .and_then(|s| serde_json::from_str::<Settings>(s.trim_start_matches('\u{feff}')).ok())
                .map(|mut s| {
                    if !BUFFER_STEPS.contains(&s.buffer_secs) {
                        s.buffer_secs = 30;
                    }
                    if !FPS_STEPS.contains(&s.fps) {
                        s.fps = 60;
                    }
                    if ![0, 30, 60].contains(&s.blend_fps) {
                        s.blend_fps = 0;
                    }
                    s
                })
                .unwrap_or_default()
        }

        pub fn save(&self) {
            let dir = config_dir();
            let _ = std::fs::create_dir_all(&dir);
            if let Ok(json) = serde_json::to_string_pretty(self) {
                let tmp = dir.join("settings.json.tmp");
                if std::fs::write(&tmp, json).is_ok() {
                    let _ = std::fs::rename(tmp, dir.join("settings.json"));
                }
            }
        }

        pub fn capture_key(&self) -> CaptureKey {
            CaptureKey {
                fps: self.fps,
                quality: self.quality,
                monitor: self.monitor,
                cursor: self.show_cursor,
                source: self.source,
                game_only: self.game_only,
                desktop: self.desktop_audio,
                mic: self.mic,
                mic_device: self.mic_device.clone(),
                apps: self.app_tracks.iter().filter(|a| a.enabled).cloned().collect(),
                encoder: self.encoder.clone(),
                pref: self.encoder_pref,
                paused: self.paused,
            }
        }

        pub fn video_kbps(&self, w: u32, h: u32) -> u32 {
            let px = (w.max(320) as f64) * (h.max(240) as f64);
            let res = (px / 2_073_600.0).powf(0.8);
            let fps = (self.fps as f64 / 60.0).powf(0.6);
            (self.quality.base_mbps() * res * fps * 1000.0).round() as u32
        }

        pub fn audio_tracks(&self) -> usize {
            let apps = self.app_tracks.iter().filter(|a| a.enabled).count();
            let base = self.desktop_audio as usize + self.mic as usize + apps;
            base + (self.desktop_audio && self.mic) as usize
        }

        pub fn buffer_bytes(&self, w: u32, h: u32) -> u64 {
            let kbps = self.video_kbps(w, h) as u64 + 160 * self.audio_tracks() as u64;
            (kbps * 1000 / 8) * (self.buffer_secs as u64 + 1) * 104 / 100
        }
    }
}

// ================================================================================================
// sys
// ================================================================================================

mod sys {
    //! Small Win32 helpers.
    #![allow(clippy::missing_safety_doc)]

    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::OnceLock;

    use windows_sys::Win32::Foundation::*;
    use windows_sys::core::BOOL;
    use windows_sys::Win32::Graphics::Dwm::*;
    use windows_sys::Win32::Graphics::Gdi::*;
    use windows_sys::Win32::System::JobObjects::*;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    use windows_sys::Win32::System::Threading::*;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    use windows_sys::Win32::UI::Shell::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    pub const BELOW_NORMAL: u32 = 0x0000_4000;

    pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
        s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
    }

    /// A background command with no console window.
    pub fn quiet(cmd: impl AsRef<OsStr>) -> Command {
        let mut c = Command::new(cmd);
        c.creation_flags(CREATE_NO_WINDOW);
        c
    }

    pub fn set_dpi_aware() {
        unsafe {
            windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }
    }

    // ---------------------------------------------------------------- ffmpeg location

    /// Where Ferrocap keeps the capture engine it downloads on first launch.
    pub fn engine_dir() -> PathBuf {
        crate::settings::cache_dir().join("engine")
    }

    pub fn ffmpeg_path() -> Option<PathBuf> {
        static P: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);
        if let Some(p) = P.lock().unwrap().clone() {
            return Some(p);
        }
        let found = (|| {
            let own = engine_dir().join("ffmpeg.exe");
            if own.exists() {
                return Some(own);
            }
            if let Ok(exe) = std::env::current_exe() {
                if let Some(dir) = exe.parent() {
                    for c in [dir.join("ffmpeg.exe"), dir.join("bin").join("ffmpeg.exe")] {
                        if c.exists() {
                            return Some(c);
                        }
                    }
                }
            }
            if let Some(path) = std::env::var_os("PATH") {
                for d in std::env::split_paths(&path) {
                    let c = d.join("ffmpeg.exe");
                    if c.exists() {
                        return Some(c);
                    }
                }
            }
            // winget install location
            if let Some(local) = dirs::data_local_dir() {
                let pk = local.join("Microsoft").join("WinGet").join("Packages");
                if let Ok(rd) = std::fs::read_dir(&pk) {
                    for e in rd.flatten() {
                        if e.file_name().to_string_lossy().starts_with("Gyan.FFmpeg") {
                            if let Ok(rd2) = std::fs::read_dir(e.path()) {
                                for e2 in rd2.flatten() {
                                    let c = e2.path().join("bin").join("ffmpeg.exe");
                                    if c.exists() {
                                        return Some(c);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            None
        })();
        if found.is_some() {
            *P.lock().unwrap() = found.clone();
        }
        found
    }

    /// Download + unpack the capture engine (FFmpeg) from Ferrocap's GitHub release.
    /// Uses curl.exe and tar.exe, which ship with Windows 10/11. `progress` gets bytes so far.
    pub fn download_engine(progress: impl Fn(u64)) -> Result<PathBuf, String> {
        let dir = engine_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let zip = dir.join("engine.zip.part");
        let _ = std::fs::remove_file(&zip);
        let mut c = quiet("curl.exe")
            .args(["-L", "--fail", "--silent", "--retry", "3", "-o"])
            .arg(&zip)
            .arg(crate::release::ENGINE_URL)
            .spawn()
            .map_err(|e| format!("curl: {e}"))?;
        adopt_child(&c);
        loop {
            match c.try_wait() {
                Ok(Some(st)) => {
                    if !st.success() {
                        return Err("Couldn't download the capture engine. Check your internet connection.".into());
                    }
                    break;
                }
                Ok(None) => {
                    progress(std::fs::metadata(&zip).map(|m| m.len()).unwrap_or(0));
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
                Err(e) => return Err(e.to_string()),
            }
        }
        let ok = quiet("tar.exe").arg("-xf").arg(&zip).arg("-C").arg(&dir).status().map(|s| s.success()).unwrap_or(false);
        let _ = std::fs::remove_file(&zip);
        let exe = dir.join("ffmpeg.exe");
        if !ok || !exe.exists() {
            return Err("The capture engine download was damaged. Retrying…".into());
        }
        Ok(exe)
    }

    // ---------------------------------------------------------------- job object

    /// Every child process is put in a job that dies with us, so ffmpeg never outlives Ferrocap.
    pub fn adopt_child(child: &std::process::Child) {
        use std::os::windows::io::AsRawHandle;
        static JOB: OnceLock<usize> = OnceLock::new();
        let job = *JOB.get_or_init(|| unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            job as usize
        });
        unsafe {
            AssignProcessToJobObject(job as HANDLE, child.as_raw_handle() as HANDLE);
        }
    }

    // ---------------------------------------------------------------- CPU usage

    /// Total CPU time (user+kernel) of a process in 100ns units.
    pub fn process_cpu_time(pid: Option<u32>) -> Option<u64> {
        unsafe {
            let (h, close) = match pid {
                None => (GetCurrentProcess(), false),
                Some(pid) => {
                    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                    if h.is_null() {
                        return None;
                    }
                    (h, true)
                }
            };
            let mut c = std::mem::zeroed();
            let mut e = std::mem::zeroed();
            let mut k: FILETIME = std::mem::zeroed();
            let mut u: FILETIME = std::mem::zeroed();
            let ok = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u);
            if close {
                CloseHandle(h);
            }
            if ok == 0 {
                return None;
            }
            let f = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
            Some(f(k) + f(u))
        }
    }

    // ---------------------------------------------------------------- monitors

    #[derive(Clone, Copy, Debug)]
    pub struct Monitor {
        pub x: i32,
        pub y: i32,
        pub w: i32,
        pub h: i32,
        pub primary: bool,
    }

    /// Monitors, primary first (matches DXGI output order on single-GPU systems).
    pub fn monitors() -> Vec<Monitor> {
        unsafe extern "system" fn cb(hm: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
            unsafe {
                let v = &mut *(data as *mut Vec<Monitor>);
                let mut mi: MONITORINFO = std::mem::zeroed();
                mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
                if GetMonitorInfoW(hm, &mut mi) != 0 {
                    let r = mi.rcMonitor;
                    v.push(Monitor {
                        x: r.left,
                        y: r.top,
                        w: r.right - r.left,
                        h: r.bottom - r.top,
                        primary: mi.dwFlags & MONITORINFOF_PRIMARY != 0,
                    });
                }
                1
            }
        }
        let mut v: Vec<Monitor> = Vec::new();
        unsafe {
            EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(cb), &mut v as *mut _ as LPARAM);
        }
        v.sort_by_key(|m| (!m.primary, m.x, m.y));
        if v.is_empty() {
            v.push(Monitor { x: 0, y: 0, w: 1920, h: 1080, primary: true });
        }
        v
    }

    // ---------------------------------------------------------------- foreground window

    pub fn own_pid() -> u32 {
        std::process::id()
    }

    pub fn foreground_title() -> Option<String> {
        unsafe {
            let h = GetForegroundWindow();
            if h.is_null() {
                return None;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, &mut pid);
            if pid == own_pid() {
                return None;
            }
            let mut buf = [0u16; 256];
            let n = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
            if n <= 0 {
                return None;
            }
            let s = String::from_utf16_lossy(&buf[..n as usize]);
            let skip = ["Program Manager", "Windows Input Experience", "Task Switching", "Search"];
            if skip.contains(&s.as_str()) {
                return None;
            }
            Some(s)
        }
    }

    /// Visible bounds of the foreground window in physical desktop coordinates.
    pub fn foreground_rect() -> Option<(i32, i32, i32, i32)> {
        unsafe {
            let h = GetForegroundWindow();
            if h.is_null() {
                return None;
            }
            let mut r: RECT = std::mem::zeroed();
            let ok = DwmGetWindowAttribute(
                h,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                &mut r as *mut _ as *mut _,
                std::mem::size_of::<RECT>() as u32,
            );
            if ok != 0 && GetWindowRect(h, &mut r) == 0 {
                return None;
            }
            Some((r.left, r.top, r.right - r.left, r.bottom - r.top))
        }
    }

    pub fn sanitize_filename(s: &str) -> String {
        let mut out: String = s
            .chars()
            .map(|c| if "<>:\"/\\|?*".contains(c) || c.is_control() { ' ' } else { c })
            .collect();
        out = out.split_whitespace().collect::<Vec<_>>().join(" ");
        let out: String = out.chars().take(48).collect();
        out.trim().trim_end_matches('.').to_string()
    }

    pub fn cursor_pos() -> (i32, i32) {
        unsafe {
            let mut pt: POINT = std::mem::zeroed();
            GetCursorPos(&mut pt);
            (pt.x, pt.y)
        }
    }

    /// Instant screen grab of a rectangle (physical px) as RGBA. GDI: fast, fine for the desktop.
    pub fn grab_screen(x: i32, y: i32, w: i32, h: i32) -> Option<(Vec<u8>, u32, u32)> {
        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            let mem = CreateCompatibleDC(screen);
            let bmp = CreateCompatibleBitmap(screen, w, h);
            let old = SelectObject(mem, bmp as _);
            let ok = BitBlt(mem, 0, 0, w, h, screen, x, y, SRCCOPY | CAPTUREBLT) != 0;
            SelectObject(mem, old);
            let mut bi: BITMAPINFO = std::mem::zeroed();
            bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = w;
            bi.bmiHeader.biHeight = -h;
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            let mut px = vec![0u8; (w * h * 4) as usize];
            let got = GetDIBits(mem, bmp, 0, h as u32, px.as_mut_ptr() as *mut _, &mut bi, DIB_RGB_COLORS);
            DeleteObject(bmp as _);
            DeleteDC(mem);
            ReleaseDC(std::ptr::null_mut(), screen);
            if !ok || got == 0 {
                return None;
            }
            for c in px.chunks_exact_mut(4) {
                c.swap(0, 2);
                c[3] = 255;
            }
            Some((px, w as u32, h as u32))
        }
    }

    /// Visible top-level windows (top of the z-order first) with their on-screen frame, physical px.
    pub fn window_rects() -> Vec<(i32, i32, i32, i32)> {
        unsafe extern "system" fn cb(h: HWND, data: LPARAM) -> BOOL {
            unsafe {
                let v = &mut *(data as *mut Vec<(i32, i32, i32, i32)>);
                if IsWindowVisible(h) == 0 || IsIconic(h) != 0 {
                    return 1;
                }
                let mut cloaked: u32 = 0;
                DwmGetWindowAttribute(h, DWMWA_CLOAKED as u32, &mut cloaked as *mut _ as *mut _, 4);
                if cloaked != 0 {
                    return 1;
                }
                let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
                if ex & WS_EX_TOOLWINDOW != 0 && ex & WS_EX_APPWINDOW == 0 {
                    return 1;
                }
                let mut pid = 0;
                GetWindowThreadProcessId(h, &mut pid);
                if pid == own_pid() {
                    return 1;
                }
                let mut r: RECT = std::mem::zeroed();
                if DwmGetWindowAttribute(h, DWMWA_EXTENDED_FRAME_BOUNDS as u32, &mut r as *mut _ as *mut _, std::mem::size_of::<RECT>() as u32) != 0 {
                    GetWindowRect(h, &mut r);
                }
                let (w, hh) = (r.right - r.left, r.bottom - r.top);
                if w >= 40 && hh >= 30 {
                    v.push((r.left, r.top, w, hh));
                }
                1
            }
        }
        let mut v: Vec<(i32, i32, i32, i32)> = Vec::new();
        unsafe {
            EnumWindows(Some(cb), &mut v as *mut _ as LPARAM);
        }
        v
    }

    pub fn timestamp() -> String {
        unsafe {
            let mut t: SYSTEMTIME = std::mem::zeroed();
            GetLocalTime(&mut t);
            format!(
                "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
                t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
            )
        }
    }

    pub fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
        let mut p = dir.join(format!("{stem}.{ext}"));
        let mut i = 2;
        while p.exists() {
            p = dir.join(format!("{stem} ({i}).{ext}"));
            i += 1;
        }
        p
    }

    // ---------------------------------------------------------------- exe icons

    /// The icon embedded in an .exe, as RGBA at roughly `size` px.
    pub fn exe_icon(path: &str, size: i32) -> Option<(Vec<u8>, u32, u32)> {
        unsafe {
            let mut hicon: HICON = std::ptr::null_mut();
            let w = wide(path);
            let n = PrivateExtractIconsW(w.as_ptr(), 0, size, size, &mut hicon, std::ptr::null_mut(), 1, 0);
            if n == 0 || n == u32::MAX || hicon.is_null() {
                return None;
            }
            let mut info: ICONINFO = std::mem::zeroed();
            if GetIconInfo(hicon, &mut info) == 0 || info.hbmColor.is_null() {
                DestroyIcon(hicon);
                return None;
            }
            let mut bm: BITMAP = std::mem::zeroed();
            GetObjectW(info.hbmColor as _, std::mem::size_of::<BITMAP>() as i32, &mut bm as *mut _ as *mut _);
            let (bw, bh) = (bm.bmWidth.max(1), bm.bmHeight.max(1));
            let mut bi: BITMAPINFO = std::mem::zeroed();
            bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = bw;
            bi.bmiHeader.biHeight = -bh; // top-down
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = 0; // BI_RGB
            let mut px = vec![0u8; (bw * bh * 4) as usize];
            let dc = GetDC(std::ptr::null_mut());
            let got = GetDIBits(dc, info.hbmColor, 0, bh as u32, px.as_mut_ptr() as *mut _, &mut bi, DIB_RGB_COLORS);
            ReleaseDC(std::ptr::null_mut(), dc);
            DeleteObject(info.hbmColor as _);
            if !info.hbmMask.is_null() {
                DeleteObject(info.hbmMask as _);
            }
            DestroyIcon(hicon);
            if got == 0 {
                return None;
            }
            let any_alpha = px.chunks_exact(4).any(|c| c[3] != 0);
            for c in px.chunks_exact_mut(4) {
                c.swap(0, 2); // BGRA -> RGBA
                if !any_alpha {
                    c[3] = 255;
                }
            }
            Some((px, bw as u32, bh as u32))
        }
    }

    // ---------------------------------------------------------------- keys

    pub fn vk_name(vk: u32) -> String {
        match vk {
            0x70..=0x87 => format!("F{}", vk - 0x6F),
            0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk).unwrap().to_string(),
            0x60..=0x69 => format!("Num {}", vk - 0x60),
            0x2C => "PrtSc".into(),
            0x13 => "Pause".into(),
            0x91 => "ScrLk".into(),
            0x2D => "Ins".into(),
            0x2E => "Del".into(),
            0x24 => "Home".into(),
            0x23 => "End".into(),
            0x21 => "PgUp".into(),
            0x22 => "PgDn".into(),
            0x25 => "←".into(),
            0x26 => "↑".into(),
            0x27 => "→".into(),
            0x28 => "↓".into(),
            0x20 => "Space".into(),
            0x09 => "Tab".into(),
            0x08 => "Backspace".into(),
            0x0D => "Enter".into(),
            0x6A => "Num *".into(),
            0x6B => "Num +".into(),
            0x6D => "Num -".into(),
            0x6E => "Num .".into(),
            0x6F => "Num /".into(),
            0x05 => "Mouse 4".into(),
            0x06 => "Mouse 5".into(),
            _ => unsafe {
                let sc = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
                let mut buf = [0u16; 32];
                let n = GetKeyNameTextW((sc << 16) as i32, buf.as_mut_ptr(), 32);
                if n > 0 {
                    String::from_utf16_lossy(&buf[..n as usize])
                } else {
                    format!("Key {vk:#04x}")
                }
            },
        }
    }

    fn down(vk: u32) -> bool {
        unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
    }

    /// Poll the keyboard for a newly pressed non-modifier key. Used while rebinding.
    pub struct KeyCapture {
        was_down: [bool; 256],
    }

    impl KeyCapture {
        pub fn new() -> Self {
            let mut s = Self { was_down: [false; 256] };
            for vk in 0..256u32 {
                s.was_down[vk as usize] = down(vk);
            }
            s
        }
        /// Returns Some(None) for Escape (cancel), Some(Some(hotkey)) for a new key.
        pub fn poll(&mut self) -> Option<Option<crate::settings::Hotkey>> {
            const SKIP: [u32; 14] = [0x01, 0x02, 0x04, 0x10, 0x11, 0x12, 0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0x5B, 0x5C];
            let mut hit = None;
            for vk in 0x08..0xFFu32 {
                let d = down(vk);
                if d && !self.was_down[vk as usize] && !SKIP.contains(&vk) && hit.is_none() {
                    hit = Some(vk);
                }
                self.was_down[vk as usize] = d;
            }
            let vk = hit?;
            if vk == 0x1B {
                return Some(None);
            }
            Some(Some(crate::settings::Hotkey { vk, ctrl: down(0x11), shift: down(0x10), alt: down(0x12) }))
        }
    }

    // ---------------------------------------------------------------- shell

    pub fn open_path(p: &Path) {
        unsafe {
            let op = wide("open");
            let f = wide(p.as_os_str());
            ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), f.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
        }
    }

    pub fn reveal(p: &Path) {
        let _ = Command::new("explorer.exe").raw_arg(format!("/select,\"{}\"", p.display())).spawn();
    }

    /// Move a file to the Recycle Bin (undoable).
    pub fn recycle(p: &Path) -> bool {
        unsafe {
            let mut from: Vec<u16> = p.as_os_str().encode_wide().collect();
            from.push(0);
            from.push(0);
            let mut op: SHFILEOPSTRUCTW = std::mem::zeroed();
            op.wFunc = FO_DELETE;
            op.pFrom = from.as_ptr();
            op.fFlags = (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI) as u16;
            SHFileOperationW(&mut op) == 0
        }
    }

    // ---------------------------------------------------------------- startup

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    pub fn set_start_with_windows(on: bool) {
        use winreg::RegKey;
        use winreg::enums::*;
        let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_WRITE) else {
            return;
        };
        if on {
            if let Ok(exe) = std::env::current_exe() {
                let _ = key.set_value("Ferrocap", &format!("\"{}\" --tray", exe.display()));
            }
        } else {
            let _ = key.delete_value("Ferrocap");
        }
    }

    // ---------------------------------------------------------------- single instance

    const SHOW_EVENT: &str = "Local\\FerrocapShowWindow";

    /// Returns false if another instance is already running (and asks it to show itself).
    pub fn claim_single_instance() -> bool {
        unsafe {
            let name = wide("Local\\FerrocapSingleInstance");
            let m = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            if !m.is_null() && GetLastError() == ERROR_ALREADY_EXISTS {
                let ev = OpenEventW(EVENT_MODIFY_STATE, 0, wide(SHOW_EVENT).as_ptr());
                if !ev.is_null() {
                    SetEvent(ev);
                    CloseHandle(ev);
                }
                return false;
            }
            // leak the mutex handle: held for the life of the process
            true
        }
    }

    /// Calls `f` whenever a second instance is launched.
    pub fn on_second_instance(f: impl Fn() + Send + 'static) {
        std::thread::spawn(move || unsafe {
            let ev = CreateEventW(std::ptr::null(), 0, 0, wide(SHOW_EVENT).as_ptr());
            if ev.is_null() {
                return;
            }
            loop {
                if WaitForSingleObject(ev, INFINITE) == WAIT_OBJECT_0 {
                    f();
                }
            }
        });
    }

    // ---------------------------------------------------------------- window chrome

    /// Win11 rounded corners + dark mode for our borderless window.
    /// Make one of our own windows (found by title) unable to take focus or appear in Alt+Tab:
    /// for overlays drawn on top of games.
    pub fn make_window_passive(title: &str) {
        struct Find<'a> {
            title: &'a str,
        }
        unsafe extern "system" fn cb(h: HWND, data: LPARAM) -> BOOL {
            unsafe {
                let f = &*(data as *const Find);
                let mut pid = 0;
                GetWindowThreadProcessId(h, &mut pid);
                if pid == own_pid() {
                    let mut buf = [0u16; 64];
                    let n = GetWindowTextW(h, buf.as_mut_ptr(), 64);
                    if String::from_utf16_lossy(&buf[..n.max(0) as usize]) == f.title {
                        let want = (WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW) as isize;
                        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
                        if ex & want != want {
                            SetWindowLongPtrW(h, GWL_EXSTYLE, ex | want);
                        }
                    }
                }
                1
            }
        }
        let f = Find { title };
        unsafe {
            EnumWindows(Some(cb), &f as *const Find as LPARAM);
        }
    }

    /// Is Ferrocap's main window really shown (Win32 view, not egui's)? eframe paints hidden windows
    /// outside the event loop, where it can't create extra windows, so snip windows wait for this.
    pub fn main_window_visible() -> bool {
        unsafe extern "system" fn cb(h: HWND, out: LPARAM) -> BOOL {
            unsafe {
                let mut pid = 0;
                GetWindowThreadProcessId(h, &mut pid);
                if pid == own_pid() {
                    let mut buf = [0u16; 32];
                    let n = GetWindowTextW(h, buf.as_mut_ptr(), 32);
                    if String::from_utf16_lossy(&buf[..n.max(0) as usize]) == "Ferrocap" {
                        *(out as *mut bool) = IsWindowVisible(h) != 0;
                        return 0;
                    }
                }
                1
            }
        }
        let mut vis = false;
        unsafe {
            EnumWindows(Some(cb), &mut vis as *mut bool as LPARAM);
        }
        vis
    }

    pub fn style_own_window() {
        unsafe extern "system" fn cb(h: HWND, _: LPARAM) -> BOOL {
            unsafe {
                let mut pid = 0;
                GetWindowThreadProcessId(h, &mut pid);
                if pid == own_pid() {
                    let mut buf = [0u16; 32];
                    let n = GetWindowTextW(h, buf.as_mut_ptr(), 32);
                    if String::from_utf16_lossy(&buf[..n.max(0) as usize]) == "Ferrocap" {
                        let round: u32 = 2; // DWMWCP_ROUND
                        DwmSetWindowAttribute(h, DWMWA_WINDOW_CORNER_PREFERENCE as u32, &round as *const _ as *const _, 4);
                        let dark: u32 = 1;
                        DwmSetWindowAttribute(h, DWMWA_USE_IMMERSIVE_DARK_MODE as u32, &dark as *const _ as *const _, 4);
                        let border: u32 = 0x0014_1a20; // COLORREF 0x00BBGGRR -> #201a14
                        DwmSetWindowAttribute(h, DWMWA_BORDER_COLOR as u32, &border as *const _ as *const _, 4);
                    }
                }
                1
            }
        }
        unsafe {
            EnumWindows(Some(cb), 0);
        }
    }

    // ---------------------------------------------------------------- sounds

    fn tone_wav(parts: &[(f32, f32, f32)]) -> Vec<u8> {
        // (freq Hz, duration s, gain)
        let rate = 44_100u32;
        let mut samples: Vec<i16> = Vec::new();
        for &(f, d, g) in parts {
            let n = (d * rate as f32) as usize;
            for i in 0..n {
                let t = i as f32 / rate as f32;
                let env = (1.0 - i as f32 / n as f32).powf(2.2) * (i as f32 / 60.0).min(1.0);
                let s = if f > 0.0 {
                    (t * f * std::f32::consts::TAU).sin() + 0.25 * (t * f * 2.0 * std::f32::consts::TAU).sin()
                } else {
                    // noise burst (shutter)
                    let x = ((i as u32).wrapping_mul(2_654_435_761) >> 16) as f32 / 32768.0 - 1.0;
                    x
                };
                samples.push((s * env * g * 9000.0) as i16);
            }
        }
        let data_len = (samples.len() * 2) as u32;
        let mut w = Vec::with_capacity(44 + data_len as usize);
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data_len).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&rate.to_le_bytes());
        w.extend_from_slice(&(rate * 2).to_le_bytes());
        w.extend_from_slice(&2u16.to_le_bytes());
        w.extend_from_slice(&16u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            w.extend_from_slice(&s.to_le_bytes());
        }
        w
    }

    pub enum Sound {
        Clip,
        Shot,
        Error,
    }

    pub fn play(sound: Sound) {
        static CLIP: OnceLock<Vec<u8>> = OnceLock::new();
        static SHOT: OnceLock<Vec<u8>> = OnceLock::new();
        static ERR: OnceLock<Vec<u8>> = OnceLock::new();
        let data = match sound {
            Sound::Clip => CLIP.get_or_init(|| tone_wav(&[(880.0, 0.07, 0.8), (1318.5, 0.16, 0.9)])),
            Sound::Shot => SHOT.get_or_init(|| include_bytes!("../assets/shutter.wav").to_vec()),
            Sound::Error => ERR.get_or_init(|| tone_wav(&[(330.0, 0.12, 0.8), (247.0, 0.2, 0.8)])),
        };
        unsafe {
            use windows_sys::Win32::Media::Audio::*;
            PlaySoundW(data.as_ptr() as *const u16, std::ptr::null_mut(), SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
        }
    }

    pub fn human_bytes(b: u64) -> String {
        let b = b as f64;
        if b >= 1e9 {
            format!("{:.1} GB", b / 1e9)
        } else if b >= 1e6 {
            format!("{:.0} MB", b / 1e6)
        } else {
            format!("{:.0} KB", b / 1e3)
        }
    }
}

// ================================================================================================
// procs
// ================================================================================================

mod procs {
    //! Finds running games and apps (Roblox, Minecraft, Discord…). Polled every 2 s on its own thread;
    //! cheap: one ToolHelp snapshot + one EnumWindows pass.

    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use windows_sys::Win32::Foundation::*;
    use windows_sys::Win32::Graphics::Dwm::*;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    use windows_sys::core::BOOL;

    #[derive(Clone, Debug, PartialEq)]
    pub struct Win {
        pub hwnd: usize,
        pub pid: u32,
        pub exe: String, // lowercase
        pub title: String,
        pub w: i32,
        pub h: i32,
    }

    #[derive(Clone, Debug, PartialEq)]
    pub struct Game {
        pub name: String,
        pub win: Win,
        /// root pid of the game's process tree (for per-app audio)
        pub root_pid: u32,
        /// full path of the game's exe (for its icon)
        pub exe_path: Option<String>,
    }

    #[derive(Default, Clone)]
    pub struct Detected {
        pub game: Option<Game>,
        /// Visible app windows, one per exe, for the "record this app on its own track" picker.
        pub apps: Vec<Win>,
        /// exe -> root pids
        pub procs: HashMap<String, Vec<u32>>,
    }

    /// (exe, friendly name). Lowercase exe names.
    pub const KNOWN_GAMES: &[(&str, &str)] = &[
        ("robloxplayerbeta.exe", "Roblox"),
        ("windows10universal.exe", "Roblox"),
        ("minecraft.windows.exe", "Minecraft"),
        ("javaw.exe", "Minecraft"),
        ("valorant-win64-shipping.exe", "VALORANT"),
        ("cs2.exe", "Counter-Strike 2"),
        ("csgo.exe", "CS:GO"),
        ("fortniteclient-win64-shipping.exe", "Fortnite"),
        ("r5apex.exe", "Apex Legends"),
        ("r5apex_dx12.exe", "Apex Legends"),
        ("gta5.exe", "GTA V"),
        ("gta5_enhanced.exe", "GTA V"),
        ("rocketleague.exe", "Rocket League"),
        ("overwatch.exe", "Overwatch 2"),
        ("league of legends.exe", "League of Legends"),
        ("dota2.exe", "Dota 2"),
        ("rainbowsix.exe", "Rainbow Six Siege"),
        ("rainbowsix_be.exe", "Rainbow Six Siege"),
        ("cod.exe", "Call of Duty"),
        ("marvel-win64-shipping.exe", "Marvel Rivals"),
        ("project8.exe", "Deadlock"),
        ("deadlock.exe", "Deadlock"),
        ("tslgame.exe", "PUBG"),
        ("destiny2.exe", "Destiny 2"),
        ("eldenring.exe", "Elden Ring"),
        ("osu!.exe", "osu!"),
        ("rustclient.exe", "Rust"),
        ("eft.exe", "Escape from Tarkov"),
        ("escapefromtarkov.exe", "Escape from Tarkov"),
        ("bf2042.exe", "Battlefield 2042"),
        ("bf6.exe", "Battlefield 6"),
        ("thefinals.exe", "THE FINALS"),
        ("discovery.exe", "THE FINALS"),
        ("gtx.exe", "Splitgate"),
        ("terraria.exe", "Terraria"),
        ("geometrydash.exe", "Geometry Dash"),
        ("cyberpunk2077.exe", "Cyberpunk 2077"),
        ("re4.exe", "Resident Evil 4"),
        ("palworld-win64-shipping.exe", "Palworld"),
        ("helldivers2.exe", "Helldivers 2"),
        ("warframe.x64.exe", "Warframe"),
        ("starrail.exe", "Honkai: Star Rail"),
        ("genshinimpact.exe", "Genshin Impact"),
        ("zenlesszonezero.exe", "Zenless Zone Zero"),
        ("brawlhalla.exe", "Brawlhalla"),
        ("left4dead2.exe", "Left 4 Dead 2"),
        ("hl2.exe", "Source game"),
        ("tf_win64.exe", "Team Fortress 2"),
    ];

    /// Never treated as games even when fullscreen.
    const NOT_GAMES: &[&str] = &[
        "explorer.exe", "chrome.exe", "msedge.exe", "firefox.exe", "opera.exe", "brave.exe", "vlc.exe",
        "mpc-hc64.exe", "discord.exe", "spotify.exe", "ferrocap.exe", "code.exe", "obs64.exe", "steamwebhelper.exe",
        "applicationframehost.exe", "searchhost.exe", "startmenuexperiencehost.exe", "lockapp.exe", "claude.exe",
        "textinputhost.exe", "shellexperiencehost.exe", "microsoft.media.player.exe", "powerpnt.exe", "winword.exe",
    ];

    pub fn state() -> &'static Mutex<Detected> {
        static S: OnceLock<Mutex<Detected>> = OnceLock::new();
        S.get_or_init(|| Mutex::new(Detected::default()))
    }

    pub fn spawn(on_change: impl Fn() + Send + 'static) {
        std::thread::Builder::new()
            .name("detect".into())
            .spawn(move || loop {
                let extra = crate::app::core().settings().extra_games;
                let d = scan(&extra);
                let changed = {
                    let mut s = state().lock().unwrap();
                    let changed = s.game != d.game || s.apps.len() != d.apps.len();
                    *s = d;
                    changed
                };
                if changed {
                    on_change();
                }
                std::thread::sleep(Duration::from_secs(2));
            })
            .unwrap();
    }

    struct Proc {
        pid: u32,
        parent: u32,
        exe: String,
    }

    fn processes() -> Vec<Proc> {
        let mut v = Vec::new();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return v;
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            if Process32FirstW(snap, &mut e) != 0 {
                loop {
                    let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                    v.push(Proc {
                        pid: e.th32ProcessID,
                        parent: e.th32ParentProcessID,
                        exe: String::from_utf16_lossy(&e.szExeFile[..n]).to_lowercase(),
                    });
                    if Process32NextW(snap, &mut e) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snap);
        }
        v
    }

    fn windows(exe_of: &HashMap<u32, String>) -> Vec<Win> {
        struct Ctx<'a> {
            exe_of: &'a HashMap<u32, String>,
            out: Vec<Win>,
        }
        unsafe extern "system" fn cb(h: HWND, data: LPARAM) -> BOOL {
            unsafe {
                let ctx = &mut *(data as *mut Ctx);
                if IsWindowVisible(h) == 0 || !GetWindow(h, GW_OWNER).is_null() {
                    return 1;
                }
                let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
                if ex & WS_EX_TOOLWINDOW != 0 {
                    return 1;
                }
                let mut cloaked: u32 = 0;
                DwmGetWindowAttribute(h, DWMWA_CLOAKED as u32, &mut cloaked as *mut _ as *mut _, 4);
                if cloaked != 0 {
                    return 1;
                }
                let mut buf = [0u16; 256];
                let n = GetWindowTextW(h, buf.as_mut_ptr(), 256);
                if n <= 0 {
                    return 1;
                }
                let mut r: RECT = std::mem::zeroed();
                GetWindowRect(h, &mut r);
                let (w, hh) = (r.right - r.left, r.bottom - r.top);
                if w < 160 || hh < 120 {
                    return 1;
                }
                let mut pid = 0;
                GetWindowThreadProcessId(h, &mut pid);
                let exe = ctx.exe_of.get(&pid).cloned().unwrap_or_default();
                ctx.out.push(Win { hwnd: h as usize, pid, exe, title: String::from_utf16_lossy(&buf[..n as usize]), w, h: hh });
                1
            }
        }
        let mut ctx = Ctx { exe_of, out: Vec::new() };
        unsafe {
            EnumWindows(Some(cb), &mut ctx as *mut _ as LPARAM);
        }
        ctx.out
    }

    fn root_pid(pid: u32, procs: &[Proc]) -> u32 {
        let by_pid: HashMap<u32, &Proc> = procs.iter().map(|p| (p.pid, p)).collect();
        let mut cur = pid;
        for _ in 0..16 {
            let Some(p) = by_pid.get(&cur) else { break };
            match by_pid.get(&p.parent) {
                Some(parent) if parent.exe == p.exe => cur = parent.pid,
                _ => break,
            }
        }
        cur
    }

    pub fn scan(extra_games: &[String]) -> Detected {
        let procs = processes();
        let exe_of: HashMap<u32, String> = procs.iter().map(|p| (p.pid, p.exe.clone())).collect();
        let wins = windows(&exe_of);

        let mut roots: HashMap<String, Vec<u32>> = HashMap::new();
        for p in &procs {
            let r = root_pid(p.pid, &procs);
            let e = roots.entry(p.exe.clone()).or_default();
            if !e.contains(&r) {
                e.push(r);
            }
        }

        // 1) known game exes / user-added exes with a real window
        let mut game = None;
        for w in &wins {
            let known = KNOWN_GAMES.iter().find(|(e, _)| *e == w.exe).map(|(_, n)| n.to_string());
            let extra = extra_games.iter().any(|e| e.eq_ignore_ascii_case(&w.exe));
            let is_mc_java = w.exe == "javaw.exe" && !w.title.to_lowercase().contains("minecraft");
            if (known.is_some() || extra) && !is_mc_java {
                let name = known.unwrap_or_else(|| pretty_exe(&w.exe));
                game = Some(Game { name, root_pid: root_pid(w.pid, &procs), exe_path: exe_path(w.pid), win: w.clone() });
                break;
            }
        }
        // 2) heuristic: a fullscreen foreground app that isn't a browser/player
        if game.is_none() {
            unsafe {
                let fg = GetForegroundWindow() as usize;
                if let Some(w) = wins.iter().find(|w| w.hwnd == fg) {
                    let mons = crate::sys::monitors();
                    let full = mons.iter().any(|m| w.w >= m.w && w.h >= m.h);
                    if full && !NOT_GAMES.contains(&w.exe.as_str()) && !w.exe.is_empty() {
                        game = Some(Game { name: pretty_title(&w.title, &w.exe), root_pid: root_pid(w.pid, &procs), exe_path: exe_path(w.pid), win: w.clone() });
                    }
                }
            }
        }

        let mut seen = std::collections::HashSet::new();
        let mut apps: Vec<Win> = wins
            .into_iter()
            .filter(|w| !w.exe.is_empty() && w.exe != "ferrocap.exe" && w.exe != "explorer.exe")
            .filter(|w| seen.insert(w.exe.clone()))
            .collect();
        apps.sort_by(|a, b| a.exe.cmp(&b.exe));
        Detected { game, apps, procs: roots }
    }

    fn exe_path(pid: u32) -> Option<String> {
        use windows_sys::Win32::System::Threading::*;
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let mut buf = [0u16; 1024];
            let mut n = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut n);
            CloseHandle(h);
            (ok != 0).then(|| String::from_utf16_lossy(&buf[..n as usize]))
        }
    }

    pub fn pretty_exe(exe: &str) -> String {
        let s = exe.trim_end_matches(".exe");
        let mut c = s.chars();
        match c.next() {
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            None => String::new(),
        }
    }

    fn pretty_title(title: &str, exe: &str) -> String {
        let t = title.trim();
        if t.is_empty() || t.len() > 40 { pretty_exe(exe) } else { t.to_string() }
    }

    /// Root pid to use for an app track right now.
    pub fn pid_for(exe: &str) -> Option<u32> {
        let s = state().lock().unwrap();
        if exe == crate::settings::GAME {
            return s.game.as_ref().map(|g| g.root_pid);
        }
        s.procs.get(exe).and_then(|v| v.first().copied())
    }

    pub fn game_name() -> Option<String> {
        state().lock().unwrap().game.as_ref().map(|g| g.name.clone())
    }

    /// Lowercase exe file name of the detected game, e.g. "robloxplayerbeta.exe".
    pub fn game_exe() -> Option<String> {
        state().lock().unwrap().game.as_ref().map(|g| g.win.exe.rsplit(['\\', '/']).next().unwrap_or(&g.win.exe).to_lowercase())
    }
}

// ================================================================================================
// audio
// ================================================================================================

mod audio {
    //! Audio capture through WASAPI, fed to ffmpeg over named pipes as raw f32 stereo 48 kHz.
    //!
    //! Sources: the whole system mix (loopback), a microphone, or a single application's process tree
    //! (Windows 10 2004+ process loopback). App sources attach and detach on their own as the app
    //! starts and quits; the writer pads silence against the wall clock so every track stays in sync.

    use std::collections::VecDeque;
    use std::io::Write;
    use std::os::windows::io::FromRawHandle;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::*;
    use windows_sys::Win32::Storage::FileSystem::*;
    use windows_sys::Win32::System::Pipes::*;

    use crate::sys::wide;

    pub const RATE: u32 = 48_000;

    #[derive(Clone, Debug)]
    pub enum Source {
        System,
        Mic(Option<String>),
        /// lowercase exe name or settings::GAME
        App(String),
    }

    pub struct AudioInput {
        pub pipe: String,
        pub name: String,
        /// latest peak level, f32 bits
        pub level: Arc<AtomicU32>,
        /// true while the source is actually delivering (app running, device open)
        pub live: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
    }

    impl AudioInput {
        pub fn stop(&self) {
            if self.stop.swap(true, Ordering::SeqCst) {
                return;
            }
            unsafe {
                // unblock ConnectNamedPipe if ffmpeg never connected
                let h = CreateFileW(wide(&self.pipe).as_ptr(), FILE_GENERIC_READ, 0, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut());
                if h != INVALID_HANDLE_VALUE {
                    CloseHandle(h);
                }
            }
        }
    }

    impl Drop for AudioInput {
        fn drop(&mut self) {
            self.stop();
        }
    }

    pub fn mic_devices() -> Vec<String> {
        let _ = wasapi::initialize_mta();
        let Ok(en) = wasapi::DeviceEnumerator::new() else { return vec![] };
        let Ok(col) = en.get_device_collection(wasapi::Direction::Capture) else { return vec![] };
        (&col).into_iter().filter_map(|d| d.ok()).filter_map(|d| d.get_friendlyname().ok()).collect()
    }

    struct Open {
        client: wasapi::AudioClient,
        capture: wasapi::AudioCaptureClient,
        _event: wasapi::Handle,
        device_id: Option<String>,
    }

    fn fmt() -> wasapi::WaveFormat {
        wasapi::WaveFormat::new(32, 32, wasapi::SampleType::Float, RATE as usize, 2, None)
    }

    fn open(src: &Source) -> Option<Open> {
        use wasapi::*;
        let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 2_000_000 };
        let (mut client, device_id) = match src {
            Source::System => {
                let d = DeviceEnumerator::new().ok()?.get_default_device(Direction::Render).ok()?;
                let id = d.get_id().ok();
                (d.get_iaudioclient().ok()?, id)
            }
            Source::Mic(name) => {
                let en = DeviceEnumerator::new().ok()?;
                let mut dev = None;
                if let Some(name) = name {
                    if let Ok(col) = en.get_device_collection(Direction::Capture) {
                        dev = (&col).into_iter().filter_map(|d| d.ok()).find(|d| d.get_friendlyname().ok().as_deref() == Some(name));
                    }
                }
                let d = match dev {
                    Some(d) => d,
                    None => en.get_default_device(Direction::Capture).ok()?,
                };
                let id = d.get_id().ok();
                (d.get_iaudioclient().ok()?, id)
            }
            Source::App(exe) => {
                let pid = crate::procs::pid_for(exe)?;
                (AudioClient::new_application_loopback_client(pid, true).ok()?, Some(format!("pid{pid}")))
            }
        };
        client.initialize_client(&fmt(), Direction::Capture, mode, None).ok()?;
        let event = client.set_get_eventhandle().ok()?;
        let capture = client.get_audiocaptureclient().ok()?;
        client.start_stream().ok()?;
        Some(Open { client, capture, _event: event, device_id })
    }

    /// What the source *should* be bound to right now (to notice device switches / app restarts).
    fn wanted_id(src: &Source) -> Option<String> {
        match src {
            Source::System => wasapi::DeviceEnumerator::new().ok()?.get_default_device(wasapi::Direction::Render).ok()?.get_id().ok(),
            Source::Mic(None) => wasapi::DeviceEnumerator::new().ok()?.get_default_device(wasapi::Direction::Capture).ok()?.get_id().ok(),
            Source::Mic(Some(_)) => None,
            Source::App(exe) => crate::procs::pid_for(exe).map(|p| format!("pid{p}")),
        }
    }

    pub fn start(source: Source, name: String, pipe: String) -> Result<AudioInput, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let level = Arc::new(AtomicU32::new(0));
        let live = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        let (s2, l2, v2, p2) = (stop.clone(), level.clone(), live.clone(), pipe.clone());
        std::thread::Builder::new()
            .name(format!("audio-{name}"))
            .spawn(move || run(source, p2, s2, l2, v2, tx))
            .map_err(|e| e.to_string())?;
        rx.recv_timeout(Duration::from_secs(5)).map_err(|_| "audio timed out".to_string())??;
        Ok(AudioInput { pipe, name, level, live, stop })
    }

    fn run(src: Source, pipe: String, stop: Arc<AtomicBool>, level: Arc<AtomicU32>, live: Arc<AtomicBool>, ready: mpsc::Sender<Result<(), String>>) {
        let h = unsafe {
            CreateNamedPipeW(wide(&pipe).as_ptr(), PIPE_ACCESS_OUTBOUND, PIPE_TYPE_BYTE | PIPE_WAIT, 1, 1 << 16, 0, 0, std::ptr::null())
        };
        if h == INVALID_HANDLE_VALUE {
            let _ = ready.send(Err("could not create audio pipe".into()));
            return;
        }
        let _ = ready.send(Ok(()));

        // Capture runs on its own thread: opening a device or an app's loopback can block for
        // seconds, and the writer below must keep feeding ffmpeg (silence) meanwhile, or the whole
        // stream stalls waiting for this track.
        let q: Arc<Mutex<VecDeque<u8>>> = Arc::default();
        {
            let (q, stop, live, src) = (q.clone(), stop.clone(), live.clone(), src.clone());
            std::thread::Builder::new()
                .name("audio-capture".into())
                .spawn(move || capture_loop(src, q, stop, live))
                .ok();
        }

        unsafe {
            if ConnectNamedPipe(h, std::ptr::null_mut()) == 0 && GetLastError() != ERROR_PIPE_CONNECTED {
                CloseHandle(h);
                stop.store(true, Ordering::SeqCst);
                return;
            }
        }
        let mut file = unsafe { std::fs::File::from_raw_handle(h as _) };
        if stop.load(Ordering::SeqCst) {
            return;
        }

        let start = Instant::now();
        q.lock().unwrap().clear();
        let mut written: u64 = 0;
        let mut last_data = Instant::now();
        let mut out: Vec<u8> = Vec::with_capacity(64 * 1024);
        let r = RATE as f64;
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10));
            let target = ((start.elapsed().as_secs_f64() - 0.05).max(0.0) * r) as i64;
            out.clear();
            {
                let mut g = q.lock().unwrap();
                let avail = (g.len() / 8) as i64;
                if avail > 0 {
                    last_data = Instant::now();
                    let ahead = written as i64 + avail - target;
                    let max_ahead = (0.15 * r) as i64;
                    if ahead > max_ahead {
                        let n = ((ahead - max_ahead) as usize).min(avail as usize);
                        g.drain(..n * 8);
                    }
                    let take = (g.len() / 8) * 8;
                    out.extend(g.drain(..take));
                }
            }
            let mut frames = (out.len() / 8) as u64;
            let need = target - written as i64 - frames as i64;
            if need > 0 && last_data.elapsed() > Duration::from_millis(40) {
                out.resize(out.len() + need as usize * 8, 0);
                frames += need as u64;
            }
            let mut peak = 0f32;
            for c in out.chunks_exact(4) {
                let v = f32::from_le_bytes([c[0], c[1], c[2], c[3]]).abs();
                if v > peak {
                    peak = v;
                }
            }
            let prev = f32::from_bits(level.load(Ordering::Relaxed));
            level.store((peak.max(prev * 0.85)).to_bits(), Ordering::Relaxed);
            if !out.is_empty() {
                if file.write_all(&out).is_err() {
                    break;
                }
                written += frames;
            }
        }
        stop.store(true, Ordering::SeqCst);
    }

    /// Owns the WASAPI client. (Re)binds every 2 s (app started/quit, default device changed)
    /// and pushes captured f32 stereo into `q`.
    fn capture_loop(src: Source, q: Arc<Mutex<VecDeque<u8>>>, stop: Arc<AtomicBool>, live: Arc<AtomicBool>) {
        let _ = wasapi::initialize_mta();
        let mut dev: Option<Open> = None;
        let mut last_check = Instant::now() - Duration::from_secs(10);
        let mut local: VecDeque<u8> = VecDeque::with_capacity(RATE as usize * 2);
        let cap = RATE as usize * 8;
        while !stop.load(Ordering::Relaxed) {
            if last_check.elapsed() > Duration::from_secs(2) {
                last_check = Instant::now();
                let want = wanted_id(&src);
                let stale = match (&dev, &want) {
                    (Some(d), Some(w)) => d.device_id.as_ref() != Some(w),
                    (Some(_), None) => matches!(src, Source::App(_)),
                    (None, _) => true,
                };
                if stale {
                    if let Some(d) = dev.take() {
                        let _ = d.client.stop_stream();
                    }
                    if want.is_some() || matches!(src, Source::Mic(Some(_))) {
                        dev = open(&src);
                    }
                }
                live.store(dev.is_some(), Ordering::Relaxed);
            }
            if let Some(d) = &dev {
                let mut failed = false;
                loop {
                    match d.capture.get_next_packet_size() {
                        Ok(Some(n)) if n > 0 => {
                            if d.capture.read_from_device_to_deque(&mut local).is_err() {
                                failed = true;
                                break;
                            }
                        }
                        Ok(_) => break,
                        Err(_) => {
                            failed = true;
                            break;
                        }
                    }
                }
                if failed {
                    dev = None;
                    live.store(false, Ordering::Relaxed);
                }
            }
            if !local.is_empty() {
                let mut g = q.lock().unwrap();
                g.extend(local.drain(..));
                if g.len() > cap {
                    let drop = (g.len() - cap) / 8 * 8;
                    g.drain(..drop);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if let Some(d) = dev {
            let _ = d.client.stop_stream();
        }
    }
}

// ================================================================================================
// engine
// ================================================================================================

mod engine {
    //! The replay buffer.
    //!
    //! ffmpeg grabs the screen with Desktop Duplication (`ddagrab`) or, for a single game window,
    //! Windows.Graphics.Capture (`gfxcapture`). Both hand frames over as D3D11 textures that go straight
    //! into the GPU's hardware encoder (NVENC / AMF / Quick Sync) without a trip through system memory.
    //! The compressed MPEG-TS stream comes back over stdout into a RAM ring buffer cut at keyframes.
    //! Saving a clip is a lossless remux of that buffer to MP4.

    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use crate::audio::{self, AudioInput};
    use crate::settings::{GAME, Settings, Source};
    use crate::{procs, sys};

    const TS: usize = 188;
    const PTS_HZ: u64 = 90_000;

    #[derive(Clone, Debug, PartialEq)]
    pub enum State {
        Starting,
        Recording,
        Paused,
        WaitingForGame,
        Error(String),
    }

    #[derive(Clone)]
    pub struct Level {
        pub name: String,
        pub level: Arc<AtomicU32>,
        pub live: Arc<AtomicBool>,
    }

    fn pack_size(w: i32, h: i32) -> u64 {
        ((w.max(0) as u64) << 32) | h.max(0) as u64
    }

    /// True when `new` is a real, different window size compared to `old` (both from `pack_size`).
    fn size_changed(old: u64, new: u64) -> bool {
        let (ow, oh) = ((old >> 32) as i32, (old & 0xffff_ffff) as i32);
        let (nw, nh) = ((new >> 32) as i32, (new & 0xffff_ffff) as i32);
        if nw < 320 || nh < 200 || ow == 0 {
            return false; // minimized, or no size recorded
        }
        (nw - ow).abs() > 8 || (nh - oh).abs() > 8
    }

    /// Set when this launch found no hardware encoder, so restarts of the capture don't re-probe.
    static CPU_THIS_SESSION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    #[derive(Clone, Default)]
    pub struct Info {
        pub encoder: String,
        pub width: u32,
        pub height: u32,
        pub fps: u32,
        pub kbps: u32,
        pub tracks: Vec<String>,
        pub source: String,
        pub levels: Vec<Level>,
    }

    #[derive(Clone)]
    pub struct Stats {
        pub state: State,
        pub info: Info,
        pub held_secs: f32,
        pub bytes: u64,
        /// (seconds-ago, bytes) per keyframe chunk, oldest first. "ago" is the chunk's start, measured
        /// from the start of the newest chunk (the one still being recorded).
        pub chunks: Vec<(f32, u32)>,
        /// seconds since the newest chunk started (capped, so a stalled capture doesn't scroll away)
        pub since_chunk: f32,
        pub ffmpeg_pid: Option<u32>,
    }

    struct Chunk {
        pts: u64,
        data: Vec<u8>,
    }

    #[derive(Default)]
    struct Ring {
        chunks: VecDeque<Chunk>,
        pat: Option<[u8; TS]>,
        pmt: Option<[u8; TS]>,
        pmt_pid: Option<u16>,
        video_pid: Option<u16>,
        bytes: usize,
        last_pts: u64,
        pts_base: u64,
        raw_last: u64,
        /// newest timestamp seen on any stream (unwrapped)
        latest: u64,
        /// added to every PTS/DTS/PCR of the current capture session, so a restarted ffmpeg
        /// continues the timeline instead of starting over at 0
        shift: u64,
        /// set when a new session should be joined onto the existing buffer
        join: Option<u64>,
        /// when the newest chunk started arriving (lets the chart scroll with the clock)
        last_chunk_at: Option<Instant>,
    }

    const WRAP: u64 = 1 << 33;

    fn read_ts(b: &[u8]) -> u64 {
        (((b[0] >> 1) & 0x07) as u64) << 30 | (b[1] as u64) << 22 | ((b[2] >> 1) as u64) << 15 | (b[3] as u64) << 7 | (b[4] >> 1) as u64
    }

    fn write_ts(b: &mut [u8], v: u64) {
        let v = v % WRAP;
        b[0] = (b[0] & 0xF1) | ((((v >> 30) & 7) as u8) << 1);
        b[1] = (v >> 22) as u8;
        b[2] = ((((v >> 15) & 0x7F) as u8) << 1) | 1;
        b[3] = (v >> 7) as u8;
        b[4] = (((v & 0x7F) as u8) << 1) | 1;
    }

    impl Ring {
        fn clear(&mut self) {
            *self = Ring::default();
        }

        /// Start of a new ffmpeg session. `keep` = same video layout as before, so stitch it on.
        fn begin_session(&mut self, keep: bool) {
            if keep && !self.chunks.is_empty() {
                self.join = Some(self.latest + PTS_HZ / 10);
                self.shift = 0;
            } else {
                self.clear();
            }
        }

        /// Shift this packet's PCR / PTS / DTS by the session offset (deciding the offset on the
        /// first timestamp after a join) and track the newest timestamp.
        fn retime(&mut self, pk: &mut [u8; TS], pusi: bool, afc: u8, off: usize) {
            if afc & 2 != 0 && pk[4] >= 7 && pk[5] & 0x10 != 0 {
                let base = (pk[6] as u64) << 25 | (pk[7] as u64) << 17 | (pk[8] as u64) << 9 | (pk[9] as u64) << 1 | (pk[10] >> 7) as u64;
                if let Some(j) = self.join.take() {
                    self.shift = (j % WRAP + WRAP - base) % WRAP;
                }
                if self.shift != 0 {
                    let v = (base + self.shift) % WRAP;
                    pk[6] = (v >> 25) as u8;
                    pk[7] = (v >> 17) as u8;
                    pk[8] = (v >> 9) as u8;
                    pk[9] = (v >> 1) as u8;
                    pk[10] = (pk[10] & 0x7F) | (((v & 1) as u8) << 7);
                }
            }
            if pusi && off + 14 <= TS && pk[off] == 0 && pk[off + 1] == 0 && pk[off + 2] == 1 {
                let flags = pk[off + 7];
                if flags & 0x80 != 0 {
                    let raw = read_ts(&pk[off + 9..]);
                    if let Some(j) = self.join.take() {
                        self.shift = (j % WRAP + WRAP - raw) % WRAP;
                    }
                    let v = (raw + self.shift) % WRAP;
                    if self.shift != 0 {
                        write_ts(&mut pk[off + 9..off + 14], v);
                        if flags & 0x40 != 0 && off + 19 <= TS {
                            let d = read_ts(&pk[off + 14..]);
                            write_ts(&mut pk[off + 14..off + 19], d + self.shift);
                        }
                    }
                    let u = self.unwrap_pts(v);
                    if u > self.latest {
                        self.latest = u;
                    }
                }
            }
        }

        fn unwrap_pts(&mut self, raw: u64) -> u64 {
            // 33-bit wrap (~26.5 h)
            if self.raw_last > raw && self.raw_last - raw > (1 << 32) {
                self.pts_base += 1 << 33;
            }
            self.raw_last = raw;
            self.pts_base + raw
        }

        fn push_packet(&mut self, p: &[u8], max_secs: u32) {
            let pid = (((p[1] & 0x1f) as u16) << 8) | p[2] as u16;
            let pusi = p[1] & 0x40 != 0;
            let afc = (p[3] >> 4) & 3;
            let mut off = 4usize;
            let mut rai = false;
            if afc & 2 != 0 {
                let alen = p[4] as usize;
                if alen > 0 {
                    rai = p[5] & 0x40 != 0;
                }
                off = 5 + alen;
            }
            if pid == 0 {
                let mut a = [0u8; TS];
                a.copy_from_slice(p);
                self.pat = Some(a);
                if self.pmt_pid.is_none() && pusi && off < TS {
                    let ptr = p[off] as usize;
                    let s = off + 1 + ptr;
                    // program loop begins 8 bytes into the section
                    let mut i = s + 8;
                    while i + 4 <= TS {
                        let prog = ((p[i] as u16) << 8) | p[i + 1] as u16;
                        let ppid = (((p[i + 2] & 0x1f) as u16) << 8) | p[i + 3] as u16;
                        if prog != 0 {
                            self.pmt_pid = Some(ppid);
                            break;
                        }
                        i += 4;
                    }
                }
                return;
            }
            if Some(pid) == self.pmt_pid {
                let mut a = [0u8; TS];
                a.copy_from_slice(p);
                self.pmt = Some(a);
                if self.video_pid.is_none() && pusi && off < TS {
                    let ptr = p[off] as usize;
                    let s = off + 1 + ptr;
                    if s + 12 < TS {
                        let sec_len = ((((p[s + 1] & 0x0f) as usize) << 8) | p[s + 2] as usize).min(TS - s - 3);
                        let pinfo = (((p[s + 10] & 0x0f) as usize) << 8) | p[s + 11] as usize;
                        let mut i = s + 12 + pinfo;
                        let end = (s + 3 + sec_len).saturating_sub(4);
                        while i + 5 <= end {
                            let st = p[i];
                            let epid = (((p[i + 1] & 0x1f) as u16) << 8) | p[i + 2] as u16;
                            let ilen = (((p[i + 3] & 0x0f) as usize) << 8) | p[i + 4] as usize;
                            if st == 0x1b || st == 0x24 {
                                self.video_pid = Some(epid);
                                break;
                            }
                            i += 5 + ilen;
                        }
                    }
                }
                return;
            }

            let mut pk = [0u8; TS];
            pk.copy_from_slice(p);
            self.retime(&mut pk, pusi, afc, off);
            if Some(pid) == self.video_pid && pusi && rai && off + 14 <= TS {
                // PES header: 00 00 01 sid len(2) flags(2) hdrlen(1) PTS(5)
                let h = &pk[off..];
                if h[0] == 0 && h[1] == 0 && h[2] == 1 && h[7] & 0x80 != 0 {
                    let raw = read_ts(&h[9..]);
                    let pts = self.unwrap_pts(raw);
                    self.chunks.push_back(Chunk { pts, data: Vec::with_capacity(256 * 1024) });
                    self.last_pts = pts;
                    self.last_chunk_at = Some(Instant::now());
                    // trim: keep the newest chunk that starts at or before (now - max)
                    let keep = max_secs as u64 * PTS_HZ;
                    while self.chunks.len() >= 2 && pts.saturating_sub(self.chunks[1].pts) >= keep {
                        if let Some(c) = self.chunks.pop_front() {
                            self.bytes -= c.data.len();
                        }
                    }
                }
            }
            if let Some(c) = self.chunks.back_mut() {
                c.data.extend_from_slice(&pk);
                self.bytes += TS;
            }
        }

        fn held_secs(&self) -> f32 {
            match self.chunks.front() {
                Some(f) => (self.last_pts.saturating_sub(f.pts) as f32 / PTS_HZ as f32) + 0.5,
                None => 0.0,
            }
        }

        /// Copy of the last `secs` seconds as a standalone transport stream.
        fn snapshot(&self, secs: u32) -> Option<Vec<u8>> {
            let (pat, pmt) = (self.pat?, self.pmt?);
            if self.chunks.is_empty() {
                return None;
            }
            let cutoff = self.last_pts.saturating_sub(secs as u64 * PTS_HZ);
            let mut start = 0;
            for (i, c) in self.chunks.iter().enumerate() {
                if c.pts <= cutoff {
                    start = i;
                }
            }
            let total: usize = self.chunks.iter().skip(start).map(|c| c.data.len()).sum();
            let mut out = Vec::with_capacity(total + 2 * TS);
            out.extend_from_slice(&pat);
            out.extend_from_slice(&pmt);
            for c in self.chunks.iter().skip(start) {
                out.extend_from_slice(&c.data);
            }
            Some(out)
        }
    }


    /// A finished capture session kept for stitching (see `Engine::prev`).
    struct PrevSession {
        ring: Ring,
        w: u32,
        h: u32,
        tracks: Vec<String>,
        fps: u32,
        ended: Instant,
    }

    pub struct Engine {
        settings: Mutex<Settings>,
        ring: Mutex<Ring>,
        state: Mutex<State>,
        info: Mutex<Info>,
        generation: AtomicU64,
        child_pid: AtomicU32,
        child: Mutex<Option<Child>>,
        shutting_down: AtomicBool,
        /// hwnd currently being captured in game-window mode (0 = display)
        target: AtomicUsize,
        /// window size (w << 32 | h) the current window capture was started with
        target_size: std::sync::atomic::AtomicU64,
        /// (width, height, audio track names, fps) of the session filling `ring`
        meta: Mutex<Option<(u32, u32, Vec<String>, u32)>>,
        /// Earlier capture sessions with a different layout (other size / source / tracks), newest
        /// last. Kept so a clip right after a game switch or resize still covers the whole buffer.
        prev: Mutex<VecDeque<PrevSession>>,
        /// a game window that window-capture couldn't get frames from: use the screen for it instead
        gfx_failed: Mutex<Option<usize>>,
        gfx_stalls: AtomicU32,
        /// video layout of the last session (w, h, audio inputs, encoder, fps, source)
        layout: Mutex<Option<(u32, u32, usize, String, u32, String)>>,
        pub saving: AtomicBool,
    }

    pub enum Saved {
        Ok { path: PathBuf, secs: f32, bytes: u64 },
        Err(String),
    }

    impl Engine {
        pub fn new(settings: Settings) -> Arc<Self> {
            let e = Arc::new(Self {
                settings: Mutex::new(settings),
                ring: Mutex::new(Ring::default()),
                state: Mutex::new(State::Starting),
                info: Mutex::new(Info::default()),
                generation: AtomicU64::new(0),
                child_pid: AtomicU32::new(0),
                child: Mutex::new(None),
                shutting_down: AtomicBool::new(false),
                target: AtomicUsize::new(0),
                target_size: std::sync::atomic::AtomicU64::new(0),
                meta: Mutex::new(None),
                prev: Mutex::new(VecDeque::new()),
                gfx_failed: Mutex::new(None),
                gfx_stalls: AtomicU32::new(0),
                layout: Mutex::new(None),
                saving: AtomicBool::new(false),
            });
            let e2 = e.clone();
            std::thread::Builder::new().name("supervisor".into()).spawn(move || e2.supervise()).unwrap();
            e
        }

        pub fn stats(&self) -> Stats {
            let ring = self.ring.lock().unwrap();
            let last = ring.last_pts;
            let chunks = ring
                .chunks
                .iter()
                .map(|c| (last.saturating_sub(c.pts) as f32 / PTS_HZ as f32, c.data.len() as u32))
                .collect();
            let pid = self.child_pid.load(Ordering::Relaxed);
            let since_chunk = ring.last_chunk_at.map(|t| t.elapsed().as_secs_f32().min(1.5)).unwrap_or(0.0);
            Stats {
                since_chunk,
                state: self.state.lock().unwrap().clone(),
                info: self.info.lock().unwrap().clone(),
                held_secs: {
                    let secs = self.settings.lock().unwrap().buffer_secs as f32;
                    let cur = ring.held_secs().min(secs);
                    cur + self.prev_plan(secs - cur, secs).iter().map(|p| p.1).sum::<f32>()
                },
                bytes: ring.bytes as u64,
                chunks,
                ffmpeg_pid: (pid != 0).then_some(pid),
            }
        }

        pub fn update_settings(&self, s: &Settings) {
            let restart = {
                let mut cur = self.settings.lock().unwrap();
                let r = cur.capture_key() != s.capture_key();
                *cur = s.clone();
                r
            };
            if restart {
                self.restart();
            }
        }

        /// Called when game detection changes. Restarts only if the capture target actually moved.
        pub fn on_detection(&self) {
            let s = self.settings.lock().unwrap().clone();
            if s.source != Source::Game && !s.game_only {
                return;
            }
            let failed = *self.gfx_failed.lock().unwrap();
            let (want, want_size) = procs::state()
                .lock()
                .unwrap()
                .game
                .as_ref()
                .filter(|g| Some(g.win.hwnd) != failed)
                .map(|g| (g.win.hwnd, pack_size(g.win.w, g.win.h)))
                .unwrap_or((0, 0));
            let waiting = *self.state.lock().unwrap() == State::WaitingForGame;
            let cur = self.target.load(Ordering::SeqCst);
            let has_game = want != 0;
            // game_only: waiting but a game appeared, or recording but the game is gone
            let game_only_flip = s.game_only && waiting == has_game;
            let target_moved = s.source == Source::Game && want != cur;
            // The window capture's frame size is fixed when it starts. If the game window then changes
            // size (launches small and gets maximized, goes fullscreen, changes resolution) the picture
            // gets squeezed into the old frame, pinned top-left with black bars. Start over at the new
            // size. Ignore minimized windows (Windows reports a tiny rect for those) and tiny jitter.
            let resized = s.source == Source::Game && want != 0 && want == cur && size_changed(self.target_size.load(Ordering::SeqCst), want_size);
            if game_only_flip || target_moved || resized {
                self.restart();
            }
        }

        pub fn restart(&self) {
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.kill_child();
        }

        pub fn shutdown(&self) {
            self.shutting_down.store(true, Ordering::SeqCst);
            self.kill_child();
        }

        fn kill_child(&self) {
            if let Some(c) = self.child.lock().unwrap().as_mut() {
                let _ = c.kill();
            }
        }

        fn set_state(&self, s: State) {
            *self.state.lock().unwrap() = s;
        }

        fn wait_gen(&self, gen_: u64, ms: u64) {
            let t = Instant::now();
            while t.elapsed() < Duration::from_millis(ms)
                && self.generation.load(Ordering::SeqCst) == gen_
                && !self.shutting_down.load(Ordering::SeqCst)
            {
                std::thread::sleep(Duration::from_millis(100));
            }
        }

        fn supervise(self: Arc<Self>) {
            let mut failures = 0u32;
            let mut prev_gen = u64::MAX;
            loop {
                if self.shutting_down.load(Ordering::SeqCst) {
                    return;
                }
                let gen_ = self.generation.load(Ordering::SeqCst);
                let s = self.settings.lock().unwrap().clone();
                *self.info.lock().unwrap() = Info::default();
                // a deliberate restart (settings / source change) starts a fresh buffer;
                // a capture loss keeps it and the next session is stitched on
                if s.paused {
                    self.ring.lock().unwrap().clear();
                    self.prev.lock().unwrap().clear();
                    *self.layout.lock().unwrap() = None;
                } else if gen_ != prev_gen {
                    // new capture setup (game switch, resize, settings): keep what we have for stitching
                    self.archive_session(s.buffer_secs);
                    *self.layout.lock().unwrap() = None;
                }
                prev_gen = gen_;
                if s.paused {
                    self.set_state(State::Paused);
                    self.wait_gen(gen_, u64::MAX);
                    continue;
                }
                let game = procs::state().lock().unwrap().game.clone();
                if s.game_only && game.is_none() {
                    self.target.store(0, Ordering::SeqCst);
                    self.set_state(State::WaitingForGame);
                    self.wait_gen(gen_, u64::MAX);
                    continue;
                }
                self.set_state(State::Starting);
                let Some(ffmpeg) = sys::ffmpeg_path() else {
                    // first launch: fetch the capture engine (one time, about 88 MB)
                    self.set_state(State::Error("Downloading the capture engine (one time)…".into()));
                    let r = sys::download_engine(|b| {
                        self.set_state(State::Error(format!("Downloading the capture engine (one time)… {:.0} MB", b as f64 / 1e6)));
                    });
                    if let Err(e) = r {
                        self.set_state(State::Error(e));
                        self.wait_gen(gen_, 10_000);
                    }
                    continue;
                };
                // A saved "libx264" from an older version (or a failed probe) is never trusted across launches:
                // look for a hardware encoder again each time Ferrocap starts.
                let saved = s.encoder.clone().filter(|e| e != "libx264" || CPU_THIS_SESSION.load(Ordering::SeqCst));
                let encoder = match (s.encoder_pref.codec(), saved) {
                    (Some(forced), _) => forced.to_string(),
                    (None, Some(e)) => e,
                    (None, None) => {
                        let e = detect_encoder(&ffmpeg, s.monitor);
                        self.settings.lock().unwrap().encoder = Some(e.clone());
                        CPU_THIS_SESSION.store(e == "libx264", Ordering::SeqCst);
                        crate::app::on_encoder_detected(&e);
                        e
                    }
                };
                let failed = *self.gfx_failed.lock().unwrap();
                let window = if s.source == Source::Game { game.clone().filter(|g| Some(g.win.hwnd) != failed) } else { None };
                self.target.store(window.as_ref().map(|g| g.win.hwnd).unwrap_or(0), Ordering::SeqCst);
                self.target_size.store(window.as_ref().map(|g| pack_size(g.win.w, g.win.h)).unwrap_or(0), Ordering::SeqCst);
                let started = Instant::now();
                let err = self.run_once(&ffmpeg, &s, &encoder, window.as_ref(), gen_);
                if self.generation.load(Ordering::SeqCst) != gen_ || self.shutting_down.load(Ordering::SeqCst) {
                    failures = 0;
                    continue;
                }
                if started.elapsed() > Duration::from_secs(20) {
                    failures = 0;
                }
                failures += 1;
                self.set_state(State::Error(err.unwrap_or_else(|| "capture stopped".into())));
                // capture losses (alt-tab, fullscreen mode switch) are routine: come back fast
                self.wait_gen(gen_, if failures <= 4 { 120 } else { 400 * failures.min(10) as u64 });
                if failures == 6 && s.encoder.is_some() && s.encoder_pref.codec().is_none() {
                    self.settings.lock().unwrap().encoder = None;
                }
            }
        }

        fn run_once(&self, ffmpeg: &Path, s: &Settings, encoder: &str, game: Option<&procs::Game>, gen_: u64) -> Option<String> {
            let mons = sys::monitors();
            let mon = mons.get(s.monitor as usize).copied().unwrap_or(mons[0]);
            // CPU encoding at 120/144 fps eats several cores and still drops frames, and the "+mem" paths
            // copy every frame through system memory: cap both at 60
            let mut s = s.clone();
            if encoder == "libx264" || encoder.ends_with("+mem") {
                s.fps = s.fps.min(60);
            }
            let s = &s;
            let (w, h, source_label, video_src) = match game {
                Some(g) => (
                    (g.win.w.max(2) as u32) & !1,
                    (g.win.h.max(2) as u32) & !1,
                    format!("{} window", g.name),
                    format!(
                        "gfxcapture=hwnd={}:max_framerate={}:capture_cursor={}:resize_mode=scale_aspect:width=-2:height=-2",
                        g.win.hwnd,
                        s.fps,
                        s.show_cursor as u8
                    ),
                ),
                None => (
                    (mon.w.max(2) as u32) & !1,
                    (mon.h.max(2) as u32) & !1,
                    if mons.len() > 1 { format!("Screen {}", s.monitor + 1) } else { "Whole screen".to_string() },
                    format!(
                        "ddagrab=output_idx={}:framerate={}:draw_mouse={}:dup_frames=1",
                        s.monitor,
                        s.fps,
                        s.show_cursor as u8
                    ),
                ),
            };
            let kbps = s.video_kbps(w, h);
            let fps = s.fps;

            // ---- audio inputs
            let pid = std::process::id();
            let mut inputs: Vec<AudioInput> = Vec::new();
            let mut add = |src: audio::Source, name: String| {
                let pipe = format!(r"\\.\pipe\ferrocap-{pid}-{gen_}-{}", inputs.len());
                match audio::start(src, name, pipe) {
                    Ok(a) => inputs.push(a),
                    Err(e) => eprintln!("audio: {e}"),
                }
            };
            if s.desktop_audio {
                add(audio::Source::System, "System".into());
            }
            for a in s.app_tracks.iter().filter(|a| a.enabled) {
                add(audio::Source::App(a.exe.clone()), a.label.clone());
            }
            if s.mic {
                add(audio::Source::Mic(s.mic_device.clone()), "Mic".into());
            }

            let mut a: Vec<String> = Vec::new();
            let mut arg = |x: &str| a.push(x.to_string());
            for x in ["-hide_banner", "-nostats", "-loglevel", "warning", "-nostdin", "-f", "lavfi", "-i"] {
                arg(x);
            }
            arg(&video_src);
            for inp in &inputs {
                for x in ["-thread_queue_size", "1024", "-f", "f32le", "-ac", "2", "-ar", "48000", "-i"] {
                    arg(x);
                }
                arg(&inp.pipe);
            }

            let vfilter = video_filter(encoder);
            let mut fc: Vec<String> = Vec::new();
            let vlabel = if let Some(f) = vfilter {
                fc.push(format!("[0:v]{f}[v]"));
                "[v]".to_string()
            } else {
                "0:v".to_string()
            };
            // Track 1 is always "what you heard": System+Mic mixed when both exist.
            let mut amaps: Vec<String> = Vec::new();
            let mut tracks: Vec<String> = Vec::new();
            let sys_idx = inputs.iter().position(|i| i.name == "System");
            let mic_idx = inputs.iter().position(|i| i.name == "Mic");
            if let (Some(si), Some(mi)) = (sys_idx, mic_idx) {
                fc.push(format!("[{}:a][{}:a]amix=inputs=2:duration=longest:normalize=0[mix]", si + 1, mi + 1));
                amaps.push("[mix]".into());
                tracks.push("Mix".into());
            }
            for (i, inp) in inputs.iter().enumerate() {
                amaps.push(format!("{}:a", i + 1));
                tracks.push(inp.name.clone());
            }
            if !fc.is_empty() {
                arg("-filter_complex");
                arg(&fc.join(";"));
            }
            arg("-map");
            arg(&vlabel);
            for m in &amaps {
                arg("-map");
                arg(m);
            }
            let k = kbps.to_string() + "k";
            let max = (kbps * 3 / 2).to_string() + "k";
            let buf = (kbps * 2).to_string() + "k";
            let g = fps.to_string();
            for x in encoder_args(encoder, &k, &max, &buf, &g, true, s.quality, fps) {
                arg(&x);
            }
            for x in ["-fps_mode", "cfr", "-r", &g] {
                arg(x);
            }
            if !amaps.is_empty() {
                for x in ["-c:a", "aac", "-b:a", "160k", "-ar", "48000", "-ac", "2"] {
                    arg(x);
                }
            }
            for x in ["-max_interleave_delta", "500000", "-f", "mpegts", "-flush_packets", "1", "-muxdelay", "0", "-muxpreload", "0", "pipe:1"] {
                arg(x);
            }

            {
                let layout = (w, h, inputs.len(), encoder.to_string(), fps, video_src.clone());
                let mut last = self.layout.lock().unwrap();
                let keep = last.as_ref() == Some(&layout);
                if !keep {
                    self.archive_session(s.buffer_secs);
                }
                self.ring.lock().unwrap().begin_session(keep);
                if std::env::var("FERROCAP_DUMP").is_ok() {
                    use std::io::Write as _;
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(std::env::temp_dir().join("fc_session.log")) {
                        let _ = writeln!(f, "keep={keep} new={:?} old={:?}", layout, last);
                    }
                }
                *last = Some(layout);
            }
            let mut cmd = sys::quiet(ffmpeg);
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(sys::CREATE_NO_WINDOW | sys::BELOW_NORMAL);
            }
            cmd.args(&a).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => return Some(format!("could not start ffmpeg: {e}")),
            };
            sys::adopt_child(&child);
            let mut stdout = child.stdout.take().unwrap();
            let mut stderr = child.stderr.take().unwrap();
            self.child_pid.store(child.id(), Ordering::SeqCst);
            *self.child.lock().unwrap() = Some(child);
            // a restart requested while ffmpeg was starting would otherwise be lost
            if self.generation.load(Ordering::SeqCst) != gen_ || self.shutting_down.load(Ordering::SeqCst) {
                self.kill_child();
            }
            *self.meta.lock().unwrap() = Some((w, h, tracks.clone(), fps));
            *self.info.lock().unwrap() = Info {
                encoder: encoder.to_string(),
                width: w,
                height: h,
                fps,
                kbps,
                tracks,
                source: source_label,
                levels: inputs
                    .iter()
                    .map(|i| Level { name: i.name.clone(), level: i.level.clone(), live: i.live.clone() })
                    .collect(),
            };

            let err_lines = Arc::new(Mutex::new(Vec::<String>::new()));
            let el = err_lines.clone();
            let err_thread = std::thread::spawn(move || {
                let mut s = String::new();
                // keep a log of the last capture session for troubleshooting
                let log = crate::settings::cache_dir().join("capture.log");
                let _ = std::fs::create_dir_all(crate::settings::cache_dir());
                let mut f = std::fs::File::create(&log).ok();
                let mut buf = [0u8; 4096];
                loop {
                    match stderr.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Some(f) = f.as_mut() {
                                let _ = f.write_all(&buf[..n]);
                            }
                            if s.len() < 64 * 1024 {
                                s.push_str(&String::from_utf8_lossy(&buf[..n]));
                            }
                        }
                    }
                }
                *el.lock().unwrap() = s.lines().map(|l| l.to_string()).collect();
            });

            let mut buf = vec![0u8; 64 * 1024];
            let mut carry: Vec<u8> = Vec::with_capacity(TS * 2);
            let mut first = true;
            let spawned = Instant::now();
            let mut wd_pts = u64::MAX;
            let mut wd_at = Instant::now();
            let mut wd_keyframes = 0u32;
            loop {
                let n = match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                if first {
                    first = false;
                    self.set_state(State::Recording);
                    if std::env::var("FERROCAP_DUMP").is_ok() {
                        use std::io::Write as _;
                        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(std::env::temp_dir().join("fc_session.log")) {
                            let r = self.ring.lock().unwrap();
                            let _ = writeln!(f, "first data after {:?}, ring chunks={} latest={} join={:?}", spawned.elapsed(), r.chunks.len(), r.latest, r.join);
                        }
                    }
                }
                let max_secs = self.settings.lock().unwrap().buffer_secs;
                let mut ring = self.ring.lock().unwrap();
                let mut data: &[u8] = &buf[..n];
                if !carry.is_empty() {
                    let need = TS - carry.len();
                    let take = need.min(data.len());
                    carry.extend_from_slice(&data[..take]);
                    data = &data[take..];
                    if carry.len() == TS {
                        if carry[0] == 0x47 {
                            ring.push_packet(&carry, max_secs);
                        }
                        carry.clear();
                    }
                }
                let mut i = 0;
                while i + TS <= data.len() {
                    if data[i] != 0x47 {
                        i += 1;
                        continue;
                    }
                    ring.push_packet(&data[i..i + TS], max_secs);
                    i += TS;
                }
                carry.extend_from_slice(&data[i..]);
                // watchdog: screen duplication sometimes hangs (fullscreen games, mode switches)
                // while audio keeps flowing. No new video keyframe for 4 s -> restart the capture;
                // the next session is stitched onto this buffer.
                if ring.last_pts != wd_pts {
                    wd_pts = ring.last_pts;
                    wd_at = Instant::now();
                    wd_keyframes += 1;
                    if wd_keyframes == 4 {
                        self.gfx_stalls.store(0, Ordering::SeqCst); // healthy session
                    }
                } else if wd_at.elapsed() > Duration::from_millis(2600) && spawned.elapsed() > Duration::from_secs(3) {
                    if let Some(g) = game {
                        // window capture gave nothing twice in a row: use screen capture for this game
                        if self.gfx_stalls.fetch_add(1, Ordering::SeqCst) + 1 >= 2 {
                            *self.gfx_failed.lock().unwrap() = Some(g.win.hwnd);
                            self.gfx_stalls.store(0, Ordering::SeqCst);
                        }
                    }
                    if wd_keyframes <= 1 && !ring.chunks.is_empty() && ring.chunks.len() <= 1 {
                        // this session never really got going: don't keep a frozen frame
                        ring.clear();
                        *self.layout.lock().unwrap() = None;
                    }
                    if std::env::var("FERROCAP_DUMP").is_ok() {
                        use std::io::Write as _;
                        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(std::env::temp_dir().join("fc_session.log")) {
                            let _ = writeln!(f, "watchdog: video stalled after {:?}, keyframes={wd_keyframes}", spawned.elapsed());
                        }
                    }
                    drop(ring);
                    self.kill_child();
                    break;
                }
            }

            let mut c = self.child.lock().unwrap().take();
            if let Some(c) = c.as_mut() {
                let _ = c.kill();
                let _ = c.wait();
            }
            self.child_pid.store(0, Ordering::SeqCst);
            for i in &inputs {
                i.stop();
            }
            let _ = err_thread.join();
            let lines = err_lines.lock().unwrap().clone();
            lines.into_iter().rev().find(|l| !l.trim().is_empty()).map(|l| tidy_error(&l))
        }

        /// Move the current session's buffer aside (for stitching) and start an empty one.
        fn archive_session(&self, window_secs: u32) {
            let mut ring = self.ring.lock().unwrap();
            let meta = self.meta.lock().unwrap().take();
            let mut prev = self.prev.lock().unwrap();
            match meta {
                Some((w, h, tracks, fps)) if ring.chunks.len() >= 2 && ring.pat.is_some() && ring.pmt.is_some() => {
                    prev.push_back(PrevSession { ring: std::mem::take(&mut *ring), w, h, tracks, fps, ended: Instant::now() });
                }
                _ => ring.clear(),
            }
            // anything that ended longer ago than the buffer can't be in a clip any more
            prev.retain(|p| p.ended.elapsed().as_secs_f32() < window_secs as f32);
            while prev.len() > 4 {
                prev.pop_front();
            }
        }

        /// Which earlier sessions fill `need` more seconds: (index into `prev`, seconds), newest first.
        fn prev_plan(&self, mut need: f32, window_secs: f32) -> Vec<(usize, f32)> {
            let prev = self.prev.lock().unwrap();
            let mut plan = Vec::new();
            for (i, p) in prev.iter().enumerate().rev() {
                if need < 0.75 {
                    break;
                }
                if p.ended.elapsed().as_secs_f32() >= window_secs {
                    continue;
                }
                let take = p.ring.held_secs().min(need);
                if take >= 0.75 {
                    plan.push((i, take));
                    need -= take;
                }
            }
            plan
        }

        pub fn save_clip(&self, dir: &Path, name_hint: Option<String>) -> Saved {
            let secs = self.settings.lock().unwrap().buffer_secs;
            let tracks = self.info.lock().unwrap().tracks.clone();
            let (vw, vh) = { let i = self.info.lock().unwrap(); (i.width, i.height) };
            let stretch = self.settings.lock().unwrap().stretch_169 && vw > 0 && vh > 0 && vw * 9 != vh * 16;
            let (data, held) = {
                let ring = self.ring.lock().unwrap();
                (ring.snapshot(secs), ring.held_secs().min(secs as f32))
            };
            // the current session is shorter than the buffer (just switched game / resized): stitch the
            // missing seconds on from the earlier sessions
            let plan = self.prev_plan(secs as f32 - held, secs as f32);
            if !plan.is_empty() {
                if let Some(d) = &data {
                    return self.save_stitched(dir, name_hint, d, held, &plan);
                }
            }
            let Some(data) = data else {
                return Saved::Err("Nothing buffered yet".into());
            };
            if std::env::var("FERROCAP_DUMP").is_ok() {
                let _ = std::fs::write(std::env::temp_dir().join("fc_snapshot.ts"), &data);
            }
            let Some(ffmpeg) = sys::ffmpeg_path() else {
                return Saved::Err("ffmpeg.exe not found".into());
            };
            if std::fs::create_dir_all(dir).is_err() {
                return Saved::Err(format!("Can't write to {}", dir.display()));
            }
            let stem = format!("Ferrocap_Clip_{}", sys::timestamp());
            let path = sys::unique_path(dir, &stem, "mp4");
            let game = name_hint.map(|n| sys::sanitize_filename(&n)).unwrap_or_default();
            let mut cmd = sys::quiet(&ffmpeg);
            cmd.args(["-hide_banner", "-loglevel", "error", "-f", "mpegts", "-i", "pipe:0", "-map", "0", "-c", "copy"]);
            if stretch {
                // 1440x1080 -> shown as 1920x1080: pixels are 4/3 wide. Lossless, players honour it.
                cmd.args(["-aspect", "16:9"]);
                cmd.arg("-bsf:v").arg(format!("h264_metadata=sample_aspect_ratio={}/{}", 16 * vh, 9 * vw));
            }
            for (i, t) in tracks.iter().enumerate() {
                // MP4 keeps per-track names as handler_name (VLC, Premiere, DaVinci show it)
                cmd.arg(format!("-metadata:s:a:{i}")).arg(format!("handler_name={t}"));
                if i == 0 {
                    cmd.arg("-disposition:a:0").arg("default");
                } else {
                    cmd.arg(format!("-disposition:a:{i}")).arg("0");
                }
            }
            cmd.arg("-metadata").arg(format!("title={game}"));
            cmd.args(["-metadata", "comment=Ferrocap"]);
            cmd.args(["-movflags", "+faststart", "-y"]).arg(&path);
            cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => return Saved::Err(e.to_string()),
            };
            sys::adopt_child(&child);
            let mut stdin = child.stdin.take().unwrap();
            let writer = std::thread::spawn(move || {
                let _ = stdin.write_all(&data);
            });
            let out = child.wait_with_output();
            let _ = writer.join();
            match out {
                Ok(o) if o.status.success() => {
                    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    Saved::Ok { path, secs: held, bytes }
                }
                Ok(o) => Saved::Err(tidy_error(String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("remux failed"))),
                Err(e) => Saved::Err(e.to_string()),
            }
        }
    }

    impl Engine {
        /// A clip made of several capture sessions (different sizes / sources / track layouts): each
        /// part is scaled to the current size (centered, never stretched) and joined, then encoded on
        /// the GPU. Slower than the lossless copy, so only used when the buffer really spans sessions.
        fn save_stitched(&self, dir: &Path, name_hint: Option<String>, cur: &[u8], cur_held: f32, plan: &[(usize, f32)]) -> Saved {
            let Some((w, h, cur_tracks, fps)) = self.meta.lock().unwrap().clone() else {
                return Saved::Err("Nothing buffered yet".into());
            };
            // parts oldest first: (ts bytes, audio track count)
            let mut parts: Vec<(Vec<u8>, usize)> = Vec::new();
            let mut total = cur_held;
            {
                let prev = self.prev.lock().unwrap();
                for &(i, take) in plan.iter().rev() {
                    if let Some(d) = prev.get(i).and_then(|p| p.ring.snapshot(take.ceil() as u32).map(|d| (d, p.tracks.len()))) {
                        parts.push(d);
                        total += take;
                    }
                }
            }
            parts.push((cur.to_vec(), cur_tracks.len()));
            let Some(ffmpeg) = sys::ffmpeg_path() else {
                return Saved::Err("ffmpeg.exe not found".into());
            };
            if std::fs::create_dir_all(dir).is_err() {
                return Saved::Err(format!("Can't write to {}", dir.display()));
            }
            let tmp = std::env::temp_dir();
            let mut files = Vec::new();
            for (i, (d, _)) in parts.iter().enumerate() {
                let f = tmp.join(format!("ferrocap-part-{}-{i}.ts", std::process::id()));
                if std::fs::write(&f, d).is_err() {
                    return Saved::Err("Couldn't write a temporary file".into());
                }
                files.push(f);
            }
            let k = parts.iter().map(|p| p.1).min().unwrap_or(0); // audio tracks every part has
            let (w, h, fps) = (w & !1, h & !1, fps.max(1));
            let mut fc = String::new();
            let mut cat = String::new();
            for i in 0..parts.len() {
                fc.push_str(&format!(
                    "[{i}:v:0]setpts=PTS-STARTPTS,scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps={fps},format=nv12[v{i}];"
                ));
                cat.push_str(&format!("[v{i}]"));
                for t in 0..k {
                    fc.push_str(&format!("[{i}:a:{t}]asetpts=PTS-STARTPTS,aresample=48000:async=1[a{i}_{t}];"));
                    cat.push_str(&format!("[a{i}_{t}]"));
                }
            }
            fc.push_str(&cat);
            fc.push_str(&format!("concat=n={}:v=1:a={k}[vout]", parts.len()));
            for t in 0..k {
                fc.push_str(&format!("[aout{t}]"));
            }
            let s = self.settings.lock().unwrap().clone();
            let enc = s.encoder_pref.codec().map(String::from).or(s.encoder.clone()).unwrap_or_else(|| "libx264".into());
            let kbps = s.video_kbps(w, h);
            let (kb, max, buf, gop) = (format!("{kbps}k"), format!("{}k", kbps * 3 / 2), format!("{}k", kbps * 2), fps.to_string());
            let stem = format!("Ferrocap_Clip_{}", sys::timestamp());
            let path = sys::unique_path(dir, &stem, "mp4");
            let game = name_hint.map(|n| sys::sanitize_filename(&n)).unwrap_or_default();
            let mut cmd = sys::quiet(&ffmpeg);
            cmd.args(["-hide_banner", "-loglevel", "error"]);
            for f in &files {
                cmd.args(["-f", "mpegts", "-i"]).arg(f);
            }
            cmd.arg("-filter_complex").arg(&fc).args(["-map", "[vout]"]);
            for t in 0..k {
                cmd.arg("-map").arg(format!("[aout{t}]"));
            }
            cmd.args(encoder_args(&enc, &kb, &max, &buf, &gop, false, s.quality, fps));
            if k > 0 {
                cmd.args(["-c:a", "aac", "-b:a", "160k"]);
            }
            for (i, t) in cur_tracks.iter().take(k).enumerate() {
                cmd.arg(format!("-metadata:s:a:{i}")).arg(format!("handler_name={t}"));
                cmd.arg(format!("-disposition:a:{i}")).arg(if i == 0 { "default" } else { "0" });
            }
            cmd.arg("-metadata").arg(format!("title={game}"));
            cmd.args(["-metadata", "comment=Ferrocap"]);
            cmd.args(["-movflags", "+faststart", "-y"]).arg(&path);
            cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
            let out = match cmd.spawn() {
                Ok(c) => {
                    sys::adopt_child(&c);
                    c.wait_with_output()
                }
                Err(e) => Err(e),
            };
            for f in &files {
                let _ = std::fs::remove_file(f);
            }
            match out {
                Ok(o) if o.status.success() => {
                    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    Saved::Ok { path, secs: total.min(s.buffer_secs as f32), bytes }
                }
                Ok(o) => {
                    let e = String::from_utf8_lossy(&o.stderr).to_string();
                    log::warn!("stitched clip failed: {}", e.lines().rev().take(4).collect::<Vec<_>>().join(" | "));
                    Saved::Err(tidy_error(e.lines().last().unwrap_or("saving failed")))
                }
                Err(e) => Saved::Err(e.to_string()),
            }
        }
    }

    /// The ffmpeg encoder behind an encoder id. "h264_amf+mem" is AMF fed through system memory
    /// (see `video_filter`); everything else is the ffmpeg name itself.
    pub fn codec(encoder: &str) -> &str {
        encoder.split('+').next().unwrap_or(encoder)
    }

    /// Filter between the captured D3D11 frames and the encoder.
    pub fn video_filter(encoder: &str) -> Option<&'static str> {
        match encoder {
            "h264_qsv" => Some("hwmap=derive_device=qsv,format=qsv"),
            "libx264" => Some("hwdownload,format=bgra"),
            // "+mem": the GPU encoder can't take frames from the capture's D3D11 device (seen with AMF
            // on some AMD PCs, and on laptops where another GPU drives the screen). Copy each frame to
            // system memory and let the encoder upload it to its own GPU. AMF and NVENC take BGRA as-is
            // and convert on the GPU, so this costs a memory copy, not a CPU encode.
            "h264_amf+mem" | "h264_nvenc+mem" => Some("hwdownload,format=bgra"),
            "h264_qsv+mem" => Some("hwdownload,format=bgra,format=nv12"),
            _ => None,
        }
    }

    /// Encoder arguments. `live` = low-latency settings for the replay buffer; `fps` is the frame rate
    /// being encoded (the heavier presets only keep up at lower rates).
    pub fn encoder_args(encoder: &str, k: &str, max: &str, buf: &str, gop: &str, live: bool, q: crate::settings::Quality, fps: u32) -> Vec<String> {
        use crate::settings::Quality;
        let encoder = codec(encoder);
        // Each quality level really encodes differently now. (Before, every level used the same
        // constant-quality target and only the bitrate cap changed, so Ultra looked like High.)
        let (cq, preset, multipass, lookahead, temporal_aq) = match q {
            Quality::Low => ("26", "p3", "disabled", "0", false),
            Quality::Balanced => ("22", "p4", "qres", "8", false),
            Quality::High => ("18", "p5", "qres", "16", true),
            // the best preset NVENC can still run in real time at this frame rate (measured on an RTX 3080:
            // p6 + full-res two-pass falls just behind at 240 fps)
            Quality::Ultra if fps <= 120 => ("15", "p6", "fullres", "20", true),
            Quality::Ultra => ("15", "p5", "qres", "20", true),
        };
        let x264_crf = match q {
            Quality::Low => "26",
            Quality::Balanced => "23",
            Quality::High => "20",
            Quality::Ultra => "18",
        };
        let v: Vec<&str> = match encoder {
            "h264_nvenc" => {
                // NVENC runs on its own chip, so the slower, better preset costs the game nothing.
                // Adaptive quantization spends bits where eyes look (edges, faces, text) and stops banding
                // in dark gradients; lookahead plans bits for fast motion.
                let mut v = vec![
                    "-c:v", "h264_nvenc", "-preset", preset, "-tune", "hq", "-multipass", multipass, "-rc", "vbr", "-cq", cq,
                    "-b:v", k, "-maxrate", max, "-bufsize", buf, "-g", gop, "-bf", "0", "-rc-lookahead", lookahead,
                ];
                if q != Quality::Low {
                    v.extend(["-spatial-aq", "1", "-aq-strength", "8"]);
                }
                if temporal_aq {
                    v.extend(["-temporal-aq", "1"]);
                }
                v
            }
            "h264_amf" => vec![
                "-c:v", "h264_amf", "-usage", "transcoding", "-quality", if live { "balanced" } else { "quality" },
                "-rc", "vbr_peak", "-b:v", k, "-maxrate", max, "-bufsize", buf, "-g", gop, "-bf", "0",
            ],
            "h264_qsv" => vec![
                "-c:v", "h264_qsv", "-preset", if live { "faster" } else { "medium" }, "-b:v", k, "-maxrate", max,
                "-bufsize", buf, "-g", gop, "-bf", "0",
            ],
            // Windows' own hardware encoder (Media Foundation): works on any AMD / Intel / NVIDIA GPU, and
            // takes the captured D3D11 frames directly. The fallback when the vendor encoder won't start.
            "h264_mf" => {
                let mut v = vec![
                    "-c:v", "h264_mf", "-hw_encoding", "1", "-scenario", if live { "live_streaming" } else { "archive" },
                    "-rate_control", "pc_vbr", "-b:v", k, "-maxrate", max, "-bufsize", buf, "-g", gop, "-bf", "0",
                ];
                if !live {
                    v.extend(["-pix_fmt", "nv12"]); // file exports: the hardware MFT only takes NV12
                }
                v
            }
            _ => vec![
                "-c:v", "libx264", "-preset", if live { "ultrafast" } else { "veryfast" }, "-crf", x264_crf, "-maxrate", max,
                "-bufsize", buf, "-g", gop, "-pix_fmt", "yuv420p",
            ],
        };
        let mut out: Vec<String> = v.into_iter().map(String::from).collect();
        if live && encoder == "libx264" {
            out.extend(["-tune".into(), "zerolatency".into(), "-threads".into(), "2".into()]);
        }
        out
    }

    pub fn tidy_error(l: &str) -> String {
        let l = l.trim();
        let l = match l.find("] ") {
            Some(i) if l.starts_with('[') => &l[i + 2..],
            _ => l,
        };
        l.chars().take(160).collect()
    }

    /// Finds the first hardware encoder that can take the captured frames, best first:
    ///  1. the GPU encoders fed straight from the capture (no copies at all),
    ///  2. the same encoders fed through system memory (works when the encoder can't use the capture's
    ///     GPU device, which is what broke AMF on some AMD PCs),
    ///  3. x264 on the CPU as a last resort.
    /// AMD's AMF runtime can be slow or flaky to start right after boot or while a game loads, so AMF
    /// gets a second try before giving up.
    pub fn detect_encoder(ffmpeg: &Path, monitor: u32) -> String {
        const ORDER: [&str; 7] = ["h264_nvenc", "h264_amf", "h264_qsv", "h264_mf", "h264_amf+mem", "h264_nvenc+mem", "h264_qsv+mem"];
        for attempt in 0..2 {
            let list: &[&str] = if attempt == 0 { &ORDER } else { &["h264_amf", "h264_amf+mem"] };
            for &enc in list {
                match probe_encoder(ffmpeg, monitor, enc) {
                    Ok(()) => {
                        log::warn!("using encoder {enc}"); // (warn so it reaches app.log)
                        return enc.to_string();
                    }
                    Err(e) => log::warn!("encoder {enc} unavailable (try {}): {e}", attempt + 1),
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        "libx264".to_string()
    }

    fn probe_encoder(ffmpeg: &Path, monitor: u32, enc: &str) -> Result<(), String> {
        let src = format!("ddagrab=output_idx={monitor}:framerate=10");
        let mut cmd = sys::quiet(ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", &src]);
        if let Some(f) = video_filter(enc) {
            cmd.args(["-vf", f]);
        }
        if codec(enc) == "h264_mf" {
            cmd.args(["-hw_encoding", "1"]); // a software MFT would just be x264 with extra steps
        }
        cmd.args(["-frames:v", "3", "-c:v", codec(enc), "-f", "null", "-"]);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut c = cmd.spawn().map_err(|e| e.to_string())?;
        sys::adopt_child(&c);
        let mut err = c.stderr.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = std::io::Read::read_to_string(&mut err, &mut s);
            s
        });
        let t = Instant::now();
        loop {
            match c.try_wait() {
                Ok(Some(st)) => {
                    let text = reader.join().unwrap_or_default();
                    return if st.success() {
                        Ok(())
                    } else {
                        // keep the lines that say why, not ffmpeg's generic "Terminating thread…" at the end
                        let why: Vec<String> = text
                            .lines()
                            .map(tidy_error)
                            .filter(|l| !l.is_empty() && !l.starts_with("Terminating thread") && !l.starts_with("Task finished"))
                            .collect();
                        Err(if why.is_empty() { "failed".into() } else { why[why.len().saturating_sub(4)..].join(" | ") })
                    };
                }
                // first AMF start-up can take several seconds while the driver loads its runtime
                Ok(None) if t.elapsed() < Duration::from_secs(10) => std::thread::sleep(Duration::from_millis(50)),
                _ => {
                    let _ = c.kill();
                    return Err("timed out".into());
                }
            }
        }
    }

    pub fn encoder_label(e: &str) -> &'static str {
        match e {
            "h264_nvenc" => "NVENC",
            "h264_amf" => "AMF",
            "h264_qsv" => "Quick Sync",
            "h264_amf+mem" => "AMF (copy)",
            "h264_nvenc+mem" => "NVENC (copy)",
            "h264_qsv+mem" => "Quick Sync (copy)",
            "h264_mf" => "GPU (Windows)",
            "libx264" => "x264 CPU",
            _ => "detecting",
        }
    }

    #[allow(dead_code)]
    fn _game_marker() -> &'static str {
        GAME
    }
}

// ================================================================================================
// badge
// ================================================================================================

mod badge {
    //! The "Saving clip… / Clip saved" badge in the top-right corner. A plain Win32 layered window on
    //! its own thread (not egui): it shows while Ferrocap sits in the tray, never takes focus
    //! (WS_EX_NOACTIVATE, shown with SW_SHOWNOACTIVATE) and lets clicks through (WS_EX_TRANSPARENT),
    //! so it can't interrupt a game. It reads `Core::hud` on a 30 ms timer.

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use windows_sys::Win32::Foundation::*;
    use windows_sys::Win32::Graphics::Gdi::*;
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    use eframe::egui::Color32;

    use crate::app::{HudPhase, core};

    static RUNNING: AtomicBool = AtomicBool::new(false);
    const W: i32 = 300;
    const H: i32 = 62;
    const SHOW_DONE_MS: u128 = 2600;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn rgb(c: Color32) -> u32 {
        c.r() as u32 | (c.g() as u32) << 8 | (c.b() as u32) << 16
    }

    pub fn ensure_running() {
        if RUNNING.swap(true, Ordering::SeqCst) {
            return;
        }
        std::thread::Builder::new()
            .name("badge".into())
            .spawn(|| unsafe {
                let class = wide("FerrocapBadge");
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(proc_),
                    lpszClassName: class.as_ptr(),
                    hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
                    ..std::mem::zeroed()
                };
                RegisterClassW(&wc);
                let title = wide("Ferrocap badge");
                let hwnd = CreateWindowExW(
                    WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW,
                    class.as_ptr(),
                    title.as_ptr(),
                    WS_POPUP,
                    0,
                    0,
                    W,
                    H,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                );
                if hwnd.is_null() {
                    RUNNING.store(false, Ordering::SeqCst);
                    return;
                }
                SetTimer(hwnd, 1, 30, None);
                let mut msg: MSG = std::mem::zeroed();
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    DispatchMessageW(&msg);
                }
            })
            .ok();
    }

    /// (visible?, opacity 0..1)
    fn state() -> (Option<crate::app::Hud>, f32) {
        let h = core().hud.lock().unwrap().clone();
        let Some(h) = h else { return (None, 0.0) };
        let ms = h.since.elapsed().as_millis();
        match h.phase {
            HudPhase::Saving { .. } => (Some(h), (ms as f32 / 120.0).min(1.0)),
            _ if ms >= SHOW_DONE_MS => (None, 0.0),
            _ => (Some(h), ((SHOW_DONE_MS - ms) as f32 / 400.0).min(1.0)),
        }
    }

    unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        unsafe {
            match msg {
                WM_TIMER => {
                    let (h, a) = state();
                    if h.is_none() {
                        if IsWindowVisible(hwnd) != 0 {
                            ShowWindow(hwnd, SW_HIDE);
                        }
                        return 0;
                    }
                    let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
                    let (w, hh) = ((W as f32 * scale) as i32, (H as f32 * scale) as i32);
                    let m = crate::sys::monitors()[0];
                    let margin = (24.0 * scale) as i32;
                    SetWindowPos(hwnd, HWND_TOPMOST, m.x + m.w - w - margin, m.y + margin, w, hh, SWP_NOACTIVATE);
                    SetLayeredWindowAttributes(hwnd, 0, (a.clamp(0.0, 1.0) * 245.0) as u8, LWA_ALPHA);
                    if IsWindowVisible(hwnd) == 0 {
                        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    }
                    InvalidateRect(hwnd, std::ptr::null(), 0);
                    0
                }
                WM_PAINT => {
                    let mut ps: PAINTSTRUCT = std::mem::zeroed();
                    let dc = BeginPaint(hwnd, &mut ps);
                    paint(hwnd, dc);
                    EndPaint(hwnd, &ps);
                    0
                }
                WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
                _ => DefWindowProcW(hwnd, msg, w, l),
            }
        }
    }

    unsafe fn paint(hwnd: HWND, dc: HDC) {
        unsafe {
            let (Some(h), _) = state() else { return };
            let p = crate::theme::pal();
            let mut rc: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rc);
            let (w, hh) = (rc.right, rc.bottom);
            let s = w as f32 / W as f32;
            let px = |v: f32| (v * s) as i32;
            // double buffer
            let mem = CreateCompatibleDC(dc);
            let bmp = CreateCompatibleBitmap(dc, w, hh);
            let old_bmp = SelectObject(mem, bmp as _);
            let fill = |r: RECT, c: Color32| {
                let b = CreateSolidBrush(rgb(c));
                FillRect(mem, &r, b);
                DeleteObject(b as _);
            };
            fill(rc, p.panel2);
            // 1 px border
            let bp = CreatePen(PS_SOLID, 1, rgb(p.line2));
            let old_pen = SelectObject(mem, bp as _);
            let old_brush = SelectObject(mem, GetStockObject(NULL_BRUSH));
            Rectangle(mem, 0, 0, w, hh);
            let (accent, title, detail) = match &h.phase {
                HudPhase::Saving { secs } => (p.accent, "Saving clip…".to_string(), format!("Last {secs} s")),
                HudPhase::Done { title, detail } => (p.good, title.clone(), detail.clone()),
                HudPhase::Failed { detail } => (p.warn, "Couldn't save clip".to_string(), detail.clone()),
            };
            fill(RECT { left: 0, top: 0, right: px(3.0), bottom: hh }, accent);
            // icon
            let (cx, cy, r) = (px(32.0), hh / 2, px(10.5));
            match &h.phase {
                HudPhase::Saving { .. } => {
                    let track = CreatePen(PS_SOLID, px(2.5).max(2), rgb(p.line2));
                    SelectObject(mem, track as _);
                    Ellipse(mem, cx - r, cy - r, cx + r, cy + r);
                    let arc = CreatePen(PS_SOLID, px(2.5).max(2), rgb(accent));
                    SelectObject(mem, arc as _);
                    let t = h.since.elapsed().as_secs_f32() * 5.0;
                    let (a0, a1) = (t, t + 4.4);
                    let (x0, y0) = (cx + (a0.cos() * 30.0 * s) as i32, cy + (a0.sin() * 30.0 * s) as i32);
                    let (x1, y1) = (cx + (a1.cos() * 30.0 * s) as i32, cy + (a1.sin() * 30.0 * s) as i32);
                    Arc(mem, cx - r, cy - r, cx + r, cy + r, x1, y1, x0, y0);
                    SelectObject(mem, bp as _);
                    DeleteObject(track as _);
                    DeleteObject(arc as _);
                }
                _ => {
                    let b = CreateSolidBrush(rgb(accent));
                    let np = CreatePen(PS_SOLID, 1, rgb(accent));
                    SelectObject(mem, b as _);
                    SelectObject(mem, np as _);
                    Ellipse(mem, cx - r, cy - r, cx + r, cy + r);
                    let ink = CreatePen(PS_SOLID, px(2.4).max(2), rgb(p.bg));
                    SelectObject(mem, ink as _);
                    if matches!(h.phase, HudPhase::Done { .. }) {
                        let pts = [
                            POINT { x: cx - px(5.0), y: cy },
                            POINT { x: cx - px(1.5), y: cy + px(3.5) },
                            POINT { x: cx + px(5.0), y: cy - px(4.0) },
                        ];
                        Polyline(mem, pts.as_ptr(), 3);
                    } else {
                        MoveToEx(mem, cx, cy - px(5.0), std::ptr::null_mut());
                        LineTo(mem, cx, cy + px(1.5));
                        MoveToEx(mem, cx, cy + px(4.5), std::ptr::null_mut());
                        LineTo(mem, cx, cy + px(5.0));
                    }
                    SelectObject(mem, GetStockObject(NULL_BRUSH));
                    SelectObject(mem, bp as _);
                    DeleteObject(b as _);
                    DeleteObject(np as _);
                    DeleteObject(ink as _);
                }
            }
            // text
            SetBkMode(mem, TRANSPARENT as _);
            let face = wide("Segoe UI Semibold");
            let face2 = wide("Segoe UI");
            let f1 = CreateFontW(-px(15.0), 0, 0, 0, FW_SEMIBOLD as i32, 0, 0, 0, DEFAULT_CHARSET as u32, 0, 0, CLEARTYPE_QUALITY as u32, 0, face.as_ptr());
            let f2 = CreateFontW(-px(12.5), 0, 0, 0, FW_NORMAL as i32, 0, 0, 0, DEFAULT_CHARSET as u32, 0, 0, CLEARTYPE_QUALITY as u32, 0, face2.as_ptr());
            let old_font = SelectObject(mem, f1 as _);
            SetTextColor(mem, rgb(p.text));
            let t1 = wide(&title);
            let mut r1 = RECT { left: px(54.0), top: px(11.0), right: w - px(10.0), bottom: px(32.0) };
            DrawTextW(mem, t1.as_ptr(), -1, &mut r1, DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX);
            SelectObject(mem, f2 as _);
            SetTextColor(mem, rgb(p.muted));
            let t2 = wide(&detail);
            let mut r2 = RECT { left: px(54.0), top: px(33.0), right: w - px(10.0), bottom: px(52.0) };
            DrawTextW(mem, t2.as_ptr(), -1, &mut r2, DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX);
            BitBlt(dc, 0, 0, w, hh, mem, 0, 0, SRCCOPY);
            SelectObject(mem, old_font);
            SelectObject(mem, old_pen);
            SelectObject(mem, old_brush);
            SelectObject(mem, old_bmp);
            DeleteObject(f1 as _);
            DeleteObject(f2 as _);
            DeleteObject(bp as _);
            DeleteObject(bmp as _);
            DeleteDC(mem);
            let _ = Duration::ZERO;
        }
    }
}

// ================================================================================================
// hotkeys
// ================================================================================================

mod hotkeys {
    //! Global hotkeys on a dedicated thread (RegisterHotKey works in fullscreen games and
    //! doesn't install a keyboard hook, so it adds no input latency).

    use std::sync::atomic::{AtomicU32, Ordering};

    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    use crate::app::core;
    use crate::settings::Hotkey;

    static THREAD: AtomicU32 = AtomicU32::new(0);
    static SNIP_SWALLOW_UP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    const SNIP: u32 = WM_APP + 3;
    const RELOAD: u32 = WM_APP + 1;
    const SUSPEND: u32 = WM_APP + 2;
    const ID_CLIP: i32 = 1;
    const ID_SHOT: i32 = 2;

    fn mods(k: &Hotkey) -> HOT_KEY_MODIFIERS {
        let mut m = MOD_NOREPEAT;
        if k.ctrl {
            m |= MOD_CONTROL;
        }
        if k.shift {
            m |= MOD_SHIFT;
        }
        if k.alt {
            m |= MOD_ALT;
        }
        m
    }

    unsafe fn register_all() {
        unsafe {
            UnregisterHotKey(std::ptr::null_mut(), ID_CLIP);
            UnregisterHotKey(std::ptr::null_mut(), ID_SHOT);
            let s = core().settings();
            set_keyhook(s.snip_override);
            let mut errs = Vec::new();
            if RegisterHotKey(std::ptr::null_mut(), ID_CLIP, mods(&s.clip_key), s.clip_key.vk) == 0 {
                errs.push(format!("{} is already used by another app", s.clip_key.label()));
            }
            if s.shot_key != s.clip_key
                && RegisterHotKey(std::ptr::null_mut(), ID_SHOT, mods(&s.shot_key), s.shot_key.vk) == 0
            {
                errs.push(format!("{} is already used by another app", s.shot_key.label()));
            }
            *core().hotkey_error.lock().unwrap() = if errs.is_empty() { None } else { Some(errs.join(" · ")) };
            core().repaint();
        }
    }

    fn down(vk: u16) -> bool {
        unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
    }

    thread_local! {
        static KEYHOOK: std::cell::RefCell<Option<std::process::Child>> = const { std::cell::RefCell::new(None) };
    }

    /// Start or stop the Win+Shift+S helper process (see `run_keyhook`).
    fn set_keyhook(on: bool) {
        KEYHOOK.with(|k| {
            let mut k = k.borrow_mut();
            if let Some(c) = k.as_mut() {
                if !on || c.try_wait().ok().flatten().is_some() {
                    let _ = c.kill();
                    let _ = c.wait();
                    *k = None;
                }
            }
            if on && k.is_none() {
                let tid = THREAD.load(Ordering::SeqCst);
                match std::env::current_exe().and_then(|exe| std::process::Command::new(exe).arg(format!("--keyhook={tid}")).spawn()) {
                    Ok(c) => {
                        crate::sys::adopt_child(&c);
                        *k = Some(c);
                    }
                    Err(e) => log::warn!("could not start the Win+Shift+S helper: {e}"),
                }
            }
        });
    }

    /// Entry point of `Ferrocap.exe --keyhook=<thread id>`: a windowless helper process that owns the
    /// low-level keyboard hook. Windows doesn't call a process's low-level hook while one of that
    /// process's own windows is in the foreground, so with the hook inside Ferrocap, Win+Shift+S
    /// fell through to the Snipping Tool whenever Ferrocap was focused. The helper never has a
    /// window, so its hook always runs. It lives in Ferrocap's job object and dies with it.
    pub fn run_keyhook(tid: u32) {
        HOOK_TARGET.store(tid, Ordering::SeqCst);
        unsafe {
            use windows_sys::Win32::System::Threading::{
                GetCurrentProcess, GetCurrentThread, HIGH_PRIORITY_CLASS, SetPriorityClass, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
            };
            // Windows silently and permanently removes a low-level hook that answers too slowly even
            // once (e.g. while a game keeps every core busy), after which Win+Shift+S goes to the
            // Snipping Tool again. This process sleeps almost all the time and does a few comparisons
            // per key, so running it at high priority costs nothing and keeps it answering on time.
            SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
            let module = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null());
            let mut hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_keyboard), module, 0);
            if hook.is_null() {
                return;
            }
            // and in case it gets dropped anyway: put a fresh hook in place every few seconds (new one
            // first, then remove the old one, so there's never a moment without a hook)
            SetTimer(std::ptr::null_mut(), 0, 3000, None);
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                if msg.message == WM_TIMER {
                    let fresh = SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_keyboard), module, 0);
                    if !fresh.is_null() {
                        UnhookWindowsHookEx(hook);
                        hook = fresh;
                    }
                }
            }
        }
    }

    static HOOK_TARGET: AtomicU32 = AtomicU32::new(0);

    /// Low-level keyboard hook (in the helper process): catches Win+Shift+S before Windows' Snipping
    /// Tool does. Only does work for the S key, and only posts a message, so typing stays instant.
    unsafe extern "system" fn ll_keyboard(code: i32, w: windows_sys::Win32::Foundation::WPARAM, l: windows_sys::Win32::Foundation::LPARAM) -> windows_sys::Win32::Foundation::LRESULT {
        unsafe {
            if code == HC_ACTION as i32 {
                let k = &*(l as *const KBDLLHOOKSTRUCT);
                if k.vkCode == 0x53 {
                    let msg = w as u32;
                    if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
                        let win = down(VK_LWIN) || down(VK_RWIN);
                        if win && down(VK_SHIFT) && !down(VK_CONTROL) && !down(VK_MENU) {
                            if !SNIP_SWALLOW_UP.swap(true, Ordering::SeqCst)
                                && PostThreadMessageW(HOOK_TARGET.load(Ordering::SeqCst), SNIP, 0, 0) == 0
                            {
                                std::process::exit(0); // Ferrocap is gone: let Windows have the key back
                            }
                            return 1;
                        }
                    } else if (msg == WM_KEYUP || msg == WM_SYSKEYUP) && SNIP_SWALLOW_UP.swap(false, Ordering::SeqCst) {
                        return 1;
                    }
                }
            }
            CallNextHookEx(std::ptr::null_mut(), code, w, l)
        }
    }

    /// Tap an unassigned key while Win is still held, so letting go of Win doesn't open Start.
    fn cancel_start_menu() {
        unsafe {
            let mut inputs: [INPUT; 2] = std::mem::zeroed();
            for (i, up) in [(0usize, false), (1, true)] {
                inputs[i].r#type = INPUT_KEYBOARD;
                inputs[i].Anonymous.ki.wVk = 0xE8;
                inputs[i].Anonymous.ki.dwFlags = if up { KEYEVENTF_KEYUP } else { 0 };
            }
            SendInput(2, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32);
        }
    }

    pub fn spawn() {
        std::thread::Builder::new()
            .name("hotkeys".into())
            .spawn(|| unsafe {
                let mut msg: MSG = std::mem::zeroed();
                // make sure the thread has a message queue before publishing its id
                PeekMessageW(&mut msg, std::ptr::null_mut(), WM_USER, WM_USER, PM_NOREMOVE);
                THREAD.store(GetCurrentThreadId(), Ordering::SeqCst);
                register_all();
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    match msg.message {
                        WM_HOTKEY => match msg.wParam as i32 {
                            ID_CLIP => core().save_clip(),
                            ID_SHOT => core().screenshot(),
                            _ => {}
                        },
                        RELOAD => register_all(),
                        SNIP => {
                            cancel_start_menu();
                            crate::snip::trigger(0, false);
                        }
                        SUSPEND => {
                            if msg.wParam != 0 {
                                UnregisterHotKey(std::ptr::null_mut(), ID_CLIP);
                                UnregisterHotKey(std::ptr::null_mut(), ID_SHOT);
                            } else {
                                register_all();
                            }
                        }
                        _ => {}
                    }
                }
            })
            .unwrap();
    }

    fn post(msg: u32, w: usize) {
        let t = THREAD.load(Ordering::SeqCst);
        if t != 0 {
            unsafe {
                PostThreadMessageW(t, msg, w, 0);
            }
        }
    }

    pub fn reload() {
        post(RELOAD, 0);
    }

    /// Temporarily release the hotkeys while the user is picking a new one.
    pub fn suspend(on: bool) {
        post(SUSPEND, on as usize);
    }
}

// ================================================================================================
// tray
// ================================================================================================

mod tray {
    //! System tray icon. Lives on the main (UI) thread. Right-click opens Ferrocap's own menu
    //! (drawn by the UI as a small borderless window), left-click opens the app.

    use std::cell::RefCell;

    use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    use crate::app::core;

    thread_local! {
        static TRAY: RefCell<Option<TrayIcon>> = const { RefCell::new(None) };
    }

    /// The black logo with rounded corners (taskbar, tray, File Explorer).
    pub fn rounded_rgba(size: u32) -> (Vec<u8>, u32, u32) {
        let img = image::load_from_memory(include_bytes!("../assets/icon-rounded.png")).expect("icon").to_rgba8();
        let img = image::imageops::resize(&img, size, size, image::imageops::FilterType::Lanczos3);
        let (w, h) = img.dimensions();
        (img.into_raw(), w, h)
    }

    /// The transparent logo, scaled to `fill` of a `size`×`size` canvas (no background).
    #[allow(dead_code)]
    pub fn logo_rgba(size: u32, fill: f32) -> (Vec<u8>, u32, u32) {
        let logo = image::load_from_memory(include_bytes!("../assets/logo.png")).expect("logo").to_rgba8();
        let inner = ((size as f32) * fill).round().max(8.0) as u32;
        let small = image::imageops::resize(&logo, inner, inner, image::imageops::FilterType::Lanczos3);
        let mut canvas = image::RgbaImage::new(size, size);
        let off = ((size - inner) / 2) as i64;
        image::imageops::overlay(&mut canvas, &small, off, off);
        (canvas.into_raw(), size, size)
    }

    #[allow(dead_code)]
    pub fn icon_rgba(size: u32) -> (Vec<u8>, u32, u32) {
        let img = image::load_from_memory(include_bytes!("../assets/icon.png")).expect("icon").to_rgba8();
        let img = image::imageops::resize(&img, size, size, image::imageops::FilterType::Lanczos3);
        let (w, h) = img.dimensions();
        (img.into_raw(), w, h)
    }

    pub fn create() {
        let (rgba, w, h) = rounded_rgba(32);
        let Ok(icon) = TrayIconBuilder::new()
            .with_tooltip("Ferrocap")
            .with_icon(tray_icon::Icon::from_rgba(rgba, w, h).expect("tray icon"))
            .build()
        else {
            return;
        };
        TrayIconEvent::set_event_handler(Some(|e: TrayIconEvent| match e {
            TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
            | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => core().request_show(),
            TrayIconEvent::Click { button: MouseButton::Right, button_state: MouseButtonState::Up, position, .. } => {
                let c = core();
                *c.tray_menu.lock().unwrap() = Some((position.x as f32, position.y as f32));
                c.repaint();
            }
            _ => {}
        }));
        TRAY.with(|t| *t.borrow_mut() = Some(icon));
        refresh();
    }

    /// Update the tooltip. No-op when called off the main thread.
    pub fn refresh() {
        TRAY.with(|t| {
            if let Some(icon) = t.borrow().as_ref() {
                let s = core().settings();
                let tip = if s.paused {
                    "Ferrocap · paused".to_string()
                } else {
                    format!("Ferrocap · last {} s · {} to save", s.buffer_secs, s.clip_key.label())
                };
                let _ = icon.set_tooltip(Some(tip));
            }
        });
    }
}

// ================================================================================================
// shots
// ================================================================================================

mod shots {
    //! Lossless PNG screenshots via a one-frame Desktop Duplication grab.

    use std::path::PathBuf;
    use std::process::Stdio;

    use crate::settings::Settings;
    use crate::sys;

    pub fn take(s: &Settings, window: Option<(i32, i32, i32, i32)>, hint: Option<String>) -> Result<PathBuf, String> {
        let ffmpeg = sys::ffmpeg_path().ok_or("ffmpeg.exe not found")?;
        std::fs::create_dir_all(&s.shots_dir).map_err(|e| e.to_string())?;
        let mons = sys::monitors();
        let mut idx = (s.monitor as usize).min(mons.len() - 1);
        let mut crop = None;
        if let Some((x, y, w, h)) = window {
            let (cx, cy) = (x + w / 2, y + h / 2);
            if let Some(i) = mons.iter().position(|m| cx >= m.x && cx < m.x + m.w && cy >= m.y && cy < m.y + m.h) {
                idx = i;
            }
            let m = mons[idx];
            let x0 = (x - m.x).clamp(0, m.w);
            let y0 = (y - m.y).clamp(0, m.h);
            let x1 = (x + w - m.x).clamp(0, m.w);
            let y1 = (y + h - m.y).clamp(0, m.h);
            if x1 - x0 >= 16 && y1 - y0 >= 16 && !(x0 == 0 && y0 == 0 && x1 == m.w && y1 == m.h) {
                crop = Some((x1 - x0, y1 - y0, x0, y0));
            }
        }
        let mut vf = "hwdownload,format=bgra".to_string();
        if let Some((w, h, x, y)) = crop {
            vf += &format!(",crop={w}:{h}:{x}:{y}");
        }
        let _ = hint;
        let stem = format!("Ferrocap_Screenshot_{}", sys::timestamp());
        let path = sys::unique_path(&s.shots_dir, &stem, "png");
        let src = format!("ddagrab=output_idx={idx}:framerate=30:draw_mouse=0");
        let mut cmd = sys::quiet(&ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", &src, "-vf", &vf, "-frames:v", "1", "-y"])
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let child = cmd.spawn().map_err(|e| e.to_string())?;
        sys::adopt_child(&child);
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if !out.status.success() || !path.exists() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(err.lines().last().unwrap_or("capture failed").chars().take(140).collect());
        }
        if s.copy_shot {
            if let Err(e) = copy_to_clipboard(&path) {
                eprintln!("clipboard: {e}");
            }
        }
        Ok(path)
    }

    fn copy_to_clipboard(path: &PathBuf) -> Result<(), String> {
        let img = image::open(path).map_err(|e| e.to_string())?.to_rgba8();
        let (w, h) = img.dimensions();
        let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
        cb.set_image(arboard::ImageData { width: w as usize, height: h as usize, bytes: img.into_raw().into() })
            .map_err(|e| e.to_string())
    }
}

// ================================================================================================
// library
// ================================================================================================

mod library {
    //! Clip & screenshot library with cached thumbnails (generated off-thread, only while the window is open).

    use std::collections::{HashMap, HashSet};
    use std::hash::{Hash, Hasher};
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::SystemTime;

    use crate::settings::cache_dir;
    use crate::sys;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Kind {
        Clip,
        Shot,
    }

    #[derive(Clone)]
    pub struct Item {
        pub path: PathBuf,
        pub name: String,
        pub bytes: u64,
        pub modified: SystemTime,
        pub kind: Kind,
        /// subfolder of the clips / screenshots folder the file sits in (its category)
        pub category: Option<String>,
        key: String,
    }

    #[derive(Clone)]
    pub struct Meta {
        pub thumb: Option<PathBuf>,
        pub duration: Option<f32>,
        pub game: Option<String>,
    }

    pub struct Library {
        pub clips: Vec<Item>,
        pub shots: Vec<Item>,
        /// category names (subfolders of the clips and screenshots folders), sorted
        pub categories: Vec<String>,
        meta: Arc<Mutex<HashMap<String, Meta>>>,
        queued: HashSet<String>,
        tx: mpsc::Sender<Item>,
    }

    impl Library {
        pub fn new() -> Self {
            let meta: Arc<Mutex<HashMap<String, Meta>>> = Arc::default();
            let (tx, rx) = mpsc::channel::<Item>();
            let m2 = meta.clone();
            std::thread::Builder::new()
                .name("thumbs".into())
                .spawn(move || {
                    for item in rx {
                        let m = make_thumb(&item);
                        m2.lock().unwrap().insert(item.key.clone(), m);
                        crate::app::core().repaint();
                    }
                })
                .unwrap();
            Self { clips: vec![], shots: vec![], categories: vec![], meta, queued: HashSet::new(), tx }
        }

        pub fn rescan(&mut self, clips_dir: &Path, shots_dir: &Path) {
            let mut clips = scan(clips_dir, Kind::Clip, &["mp4", "mkv", "mov"], None);
            let mut shots = scan(shots_dir, Kind::Shot, &["png", "jpg", "jpeg"], None);
            let mut cats: Vec<String> = Vec::new();
            for (dir, kind, exts, out) in [
                (clips_dir, Kind::Clip, &["mp4", "mkv", "mov"][..], &mut clips),
                (shots_dir, Kind::Shot, &["png", "jpg", "jpeg"][..], &mut shots),
            ] {
                for c in subfolders(dir) {
                    out.extend(scan(&dir.join(&c), kind, exts, Some(c.clone())));
                    if !cats.iter().any(|x| x.eq_ignore_ascii_case(&c)) {
                        cats.push(c);
                    }
                }
                out.sort_by(|a, b| b.modified.cmp(&a.modified));
                out.truncate(600);
            }
            cats.sort_by_key(|c| c.to_lowercase());
            self.clips = clips;
            self.shots = shots;
            self.categories = cats;
        }

        /// Cached metadata; queues thumbnail generation on first request.
        pub fn meta(&mut self, item: &Item) -> Option<Meta> {
            if let Some(m) = self.meta.lock().unwrap().get(&item.key) {
                return Some(m.clone());
            }
            if self.queued.insert(item.key.clone()) {
                // fast path: already on disk from an earlier run
                let dir = thumbs_dir();
                let jpg = dir.join(format!("{}.jpg", item.key));
                if jpg.exists() {
                    let txt = std::fs::read_to_string(dir.join(format!("{}.txt", item.key))).unwrap_or_default();
                    let mut lines = txt.lines();
                    let duration = lines.next().and_then(|s| s.trim().parse().ok());
                    let game = lines.next().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
                    let m = Meta { thumb: Some(jpg), duration, game };
                    self.meta.lock().unwrap().insert(item.key.clone(), m.clone());
                    return Some(m);
                }
                let _ = self.tx.send(item.clone());
            }
            None
        }

        pub fn remove(&mut self, path: &Path) {
            self.clips.retain(|i| i.path != path);
            self.shots.retain(|i| i.path != path);
        }
    }

    fn thumbs_dir() -> PathBuf {
        cache_dir().join("thumbs")
    }

    /// Category folders inside `dir` (skips hidden / dot folders).
    fn subfolders(dir: &Path) -> Vec<String> {
        let mut v = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    let n = e.file_name().to_string_lossy().to_string();
                    if !n.starts_with('.') {
                        v.push(n);
                    }
                }
            }
        }
        v
    }

    fn clean_name(s: &str) -> Result<String, String> {
        let n = sys::sanitize_filename(s.trim()).trim().trim_matches('.').to_string();
        if n.is_empty() {
            return Err("Type a name first.".into());
        }
        Ok(n.chars().take(120).collect())
    }

    /// Rename a file (keeps its extension). Returns the new path.
    pub fn rename_file(path: &Path, new_name: &str) -> Result<PathBuf, String> {
        let stem = clean_name(new_name)?;
        let ext = path.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
        let target = path.with_file_name(if ext.is_empty() { stem } else { format!("{stem}.{ext}") });
        if target == path {
            return Ok(target);
        }
        if target.exists() {
            return Err("A file with that name already exists.".into());
        }
        std::fs::rename(path, &target).map_err(|e| format!("Couldn't rename: {e}"))?;
        Ok(target)
    }

    /// Move a file into `base/category` (or back to `base` for None). Returns the new path.
    pub fn move_to_category(path: &Path, base: &Path, category: Option<&str>) -> Result<PathBuf, String> {
        let dir = match category {
            Some(c) => base.join(clean_name(c)?),
            None => base.to_path_buf(),
        };
        if path.parent() == Some(dir.as_path()) {
            return Ok(path.to_path_buf());
        }
        std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't create the folder: {e}"))?;
        let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let ext = path.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
        let target = sys::unique_path(&dir, &stem, &ext);
        std::fs::rename(path, &target).map_err(|e| format!("Couldn't move: {e}"))?;
        Ok(target)
    }

    /// Create a category (a folder in the clips folder).
    pub fn create_category(base: &Path, name: &str) -> Result<String, String> {
        let n = clean_name(name)?;
        std::fs::create_dir_all(base.join(&n)).map_err(|e| format!("Couldn't create the folder: {e}"))?;
        Ok(n)
    }

    /// Rename a category in every library folder.
    pub fn rename_category(bases: &[&Path], old: &str, new: &str) -> Result<String, String> {
        let n = clean_name(new)?;
        for b in bases {
            let from = b.join(old);
            if from.is_dir() {
                let to = b.join(&n);
                if to.exists() && !to.eq(&from) {
                    return Err("A category with that name already exists.".into());
                }
                std::fs::rename(&from, &to).map_err(|e| format!("Couldn't rename: {e}"))?;
            }
        }
        Ok(n)
    }

    /// Remove a category: its files go back to the main folder, then the empty folder is removed.
    pub fn delete_category(bases: &[&Path], name: &str) -> Result<(), String> {
        for b in bases {
            let dir = b.join(name);
            if !dir.is_dir() {
                continue;
            }
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_file() {
                        move_to_category(&p, b, None)?;
                    }
                }
            }
            let _ = std::fs::remove_dir(&dir);
        }
        Ok(())
    }

    fn scan(dir: &Path, kind: Kind, exts: &[&str], category: Option<String>) -> Vec<Item> {
        let mut v = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                let ext = p.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
                if !exts.contains(&ext.as_str()) {
                    continue;
                }
                let Ok(md) = e.metadata() else { continue };
                let modified = md.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                let mut h = std::collections::hash_map::DefaultHasher::new();
                p.hash(&mut h);
                md.len().hash(&mut h);
                modified.hash(&mut h);
                v.push(Item {
                    name: p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
                    path: p,
                    bytes: md.len(),
                    modified,
                    kind,
                    category: category.clone(),
                    key: format!("{:016x}", h.finish()),
                });
            }
        }
        v.sort_by(|a, b| b.modified.cmp(&a.modified));
        v.truncate(300);
        v
    }

    fn parse_duration(stderr: &str) -> Option<f32> {
        let i = stderr.find("Duration: ")? + 10;
        let t = stderr.get(i..i + 11)?;
        let mut parts = t.split(':');
        let h: f32 = parts.next()?.parse().ok()?;
        let m: f32 = parts.next()?.parse().ok()?;
        let s: f32 = parts.next()?.parse().ok()?;
        Some(h * 3600.0 + m * 60.0 + s)
    }

    fn make_thumb(item: &Item) -> Meta {
        let Some(ffmpeg) = sys::ffmpeg_path() else {
            return Meta { thumb: None, duration: None, game: None };
        };
        let dir = thumbs_dir();
        let _ = std::fs::create_dir_all(&dir);
        let out = dir.join(format!("{}.jpg", item.key));
        let run = |seek: Option<&str>| {
            let mut c = sys::quiet(&ffmpeg);
            c.args(["-hide_banner"]);
            if let Some(s) = seek {
                c.args(["-ss", s]);
            }
            c.arg("-i").arg(&item.path);
            c.args(["-frames:v", "1", "-vf", "scale=480:-2:flags=bicubic", "-q:v", "4", "-y"]).arg(&out);
            c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
            c.output().ok()
        };
        let o = if item.kind == Kind::Clip { run(Some("1.5")) } else { run(None) };
        let text = o.as_ref().map(|o| String::from_utf8_lossy(&o.stderr).to_string()).unwrap_or_default();
        let duration = parse_duration(&text);
        let game = parse_title(&text);
        if item.kind == Kind::Clip && !out.exists() {
            run(None);
        }
        let _ = std::fs::write(
            dir.join(format!("{}.txt", item.key)),
            format!("{}
    {}", duration.map(|d| d.to_string()).unwrap_or_default(), game.clone().unwrap_or_default()),
        );
        Meta { thumb: out.exists().then_some(out), duration: if item.kind == Kind::Clip { duration } else { None }, game }
    }

    /// Format-level `title` from ffmpeg's banner (the first Metadata block, before any stream).
    fn parse_title(text: &str) -> Option<String> {
        let head = text.split("Stream #").next()?;
        head.lines()
            .map(str::trim)
            .find(|l| l.starts_with("title") && l.contains(':'))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    pub fn ago(t: SystemTime) -> String {
        let s = SystemTime::now().duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
        match s {
            0..=59 => "just now".into(),
            60..=3599 => format!("{} min ago", s / 60),
            3600..=86_399 => format!("{} h ago", s / 3600),
            86_400..=604_799 => format!("{} d ago", s / 86_400),
            _ => format!("{} wk ago", s / 604_800),
        }
    }

    pub fn fmt_duration(d: f32) -> String {
        let s = d.round() as u32;
        format!("{}:{:02}", s / 60, s % 60)
    }
}

// ================================================================================================
// editor
// ================================================================================================

mod editor {
    //! Lightweight clip editor: preview player, trim / split / cut, crop with aspect presets,
    //! per-track mute + volume, frame blending, and export (lossless copy when possible).

    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader, Read};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Instant;

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use eframe::egui::{self, Align2, Color32, CornerRadius, Key, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

    use crate::app::{ToastKind, core};
    use crate::settings::Blend;
    use crate::sys;
    use crate::theme::*;

    // ====================================================================== probe

    #[derive(Clone, Debug, Default)]
    pub struct Probe {
        pub duration: f64,
        pub width: u32,
        pub height: u32,
        pub fps: f64,
        pub tracks: Vec<String>,
        /// display aspect ratio when the pixels aren't square (stretched clips)
        pub dar: Option<f32>,
    }

    pub fn probe(path: &Path) -> Option<Probe> {
        let ff = sys::ffmpeg_path()?;
        let o = sys::quiet(&ff).args(["-hide_banner", "-i"]).arg(path).stdin(Stdio::null()).output().ok()?;
        let text = String::from_utf8_lossy(&o.stderr).to_string();
        let mut p = Probe::default();
        if let Some(i) = text.find("Duration: ") {
            let t = &text[i + 10..i + 21];
            let mut it = t.split(':');
            let h: f64 = it.next()?.parse().ok()?;
            let m: f64 = it.next()?.parse().ok()?;
            let s: f64 = it.next()?.parse().ok()?;
            p.duration = h * 3600.0 + m * 60.0 + s;
        }
        let mut in_audio = false;
        for line in text.lines() {
            let l = line.trim();
            if l.starts_with("Stream #") {
                in_audio = false;
                if l.contains("Video:") && p.width == 0 {
                    for tok in l.split([',', ' ']) {
                        if let Some((a, b)) = tok.split_once('x') {
                            if let (Ok(w), Ok(h)) = (a.parse::<u32>(), b.parse::<u32>()) {
                                if w >= 16 && h >= 16 {
                                    p.width = w;
                                    p.height = h;
                                }
                            }
                        }
                    }
                    if let Some(i) = l.find("DAR ") {
                        let t: String = l[i + 4..].chars().take_while(|c| c.is_ascii_digit() || *c == ':').collect();
                        if let Some((a, b)) = t.split_once(':') {
                            if let (Ok(a), Ok(b)) = (a.parse::<f32>(), b.parse::<f32>()) {
                                if b > 0.0 && l.contains("SAR ") && !l.contains("SAR 1:1") {
                                    p.dar = Some(a / b);
                                }
                            }
                        }
                    }
                    if let Some(i) = l.find(" fps") {
                        let before = &l[..i];
                        if let Some(n) = before.rsplit([' ', ',']).next() {
                            p.fps = n.parse().unwrap_or(60.0);
                        }
                    }
                } else if l.contains("Audio:") {
                    in_audio = true;
                    p.tracks.push(format!("Track {}", p.tracks.len() + 1));
                }
            } else if in_audio && (l.starts_with("title") || l.starts_with("handler_name")) {
                if let Some((_, v)) = l.split_once(':') {
                    let v = v.trim();
                    if !v.is_empty() && v != "SoundHandler" {
                        if let Some(last) = p.tracks.last_mut() {
                            *last = v.to_string();
                        }
                    }
                }
            }
        }
        if p.fps <= 0.0 {
            p.fps = 60.0;
        }
        (p.duration > 0.0).then_some(p)
    }

    // ====================================================================== player

    struct Dec {
        stop: Arc<AtomicBool>,
        pid: Arc<AtomicU32>,
    }

    impl Drop for Dec {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            kill_pid(self.pid.load(Ordering::SeqCst));
        }
    }

    fn kill_pid(pid: u32) {
        if pid == 0 {
            return;
        }
        unsafe {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Threading::*;
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !h.is_null() {
                TerminateProcess(h, 1);
                CloseHandle(h);
            }
        }
    }

    fn spawn_ff(args: &[String]) -> Option<Child> {
        let ff = sys::ffmpeg_path()?;
        let c = sys::quiet(&ff).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
        sys::adopt_child(&c);
        Some(c)
    }

    type Frame = (f64, Vec<u8>);

    pub struct Player {
        path: PathBuf,
        pub dur: f64,
        pf: f64,
        src_w: u32,
        src_h: u32,
        pw: u32,
        ph: u32,
        pub playing: bool,
        pub pos: f64,
        clock_start: Instant,
        clock_pos: f64,
        vrx: Option<mpsc::Receiver<Frame>>,
        vdec: Option<Dec>,
        pending: Option<Frame>,
        still: Arc<Mutex<Option<Frame>>>,
        still_busy: Arc<AtomicBool>,
        still_want: Option<f64>,
        pub tex: Option<egui::TextureHandle>,
        audio_q: Arc<Mutex<VecDeque<f32>>>,
        audio_dec: Option<Dec>,
        stream: Option<cpal::Stream>,
        out_rate: u32,
        pub audio_filter: Option<String>,
    }

    impl Player {
        pub fn new(path: &Path, p: &Probe) -> Self {
            // display width (accounts for stretched / non-square pixels)
            let disp_w = p.dar.map(|d| (p.height.max(2) as f32 * d).round() as u32).unwrap_or(p.width.max(2));
            let pw = 960u32.min(disp_w) & !1;
            let ph = ((pw as f64 * p.height.max(2) as f64 / disp_w as f64).round() as u32).max(2) & !1;
            let audio_q: Arc<Mutex<VecDeque<f32>>> = Arc::default();
            let (stream, out_rate) = open_output(audio_q.clone()).map(|(s, r)| (Some(s), r)).unwrap_or((None, 48_000));
            let mut me = Self {
                path: path.to_path_buf(),
                dur: p.duration,
                pf: p.fps.max(1.0),
                src_w: p.dar.map(|d| (p.height.max(2) as f32 * d).round() as u32).unwrap_or(p.width.max(2)),
                src_h: p.height.max(2),
                pw,
                ph,
                playing: false,
                pos: 0.0,
                clock_start: Instant::now(),
                clock_pos: 0.0,
                vrx: None,
                vdec: None,
                pending: None,
                still: Arc::default(),
                still_busy: Arc::default(),
                still_want: Some(0.0),
                tex: None,
                audio_q,
                audio_dec: None,
                stream,
                out_rate,
                audio_filter: None,
            };
            me.audio_filter = default_audio_filter(p.tracks.len());
            me
        }

        /// Decode at the size the video is actually shown at (in physical pixels, capped at the
        /// source size), so the preview is pixel-for-pixel sharp instead of stretched.
        pub fn fit_to(&mut self, px_w: f32) {
            let w = (px_w.round() as u32).clamp(160, self.src_w) & !1;
            if (w as i64 - self.pw as i64).abs() < 24 {
                return;
            }
            self.pw = w;
            self.ph = ((w as f64 * self.src_h as f64 / self.src_w as f64).round() as u32).max(2) & !1;
            self.tex = None;
            if self.playing {
                self.play();
            } else {
                self.still_want = Some(self.pos);
            }
        }

        pub fn aspect(&self) -> f32 {
            self.pw as f32 / self.ph as f32
        }

        pub fn play(&mut self) {
            if self.pos >= self.dur - 0.05 {
                self.pos = 0.0;
            }
            self.playing = true;
            self.clock_pos = self.pos;
            self.clock_start = Instant::now();
            self.pending = None;
            // a few frames of slack, so a decoding hiccup at 240 fps doesn't show as a stutter
            let (tx, rx) = mpsc::sync_channel::<Frame>(12);
            self.vrx = Some(rx);
            self.vdec = Some(self.start_video(self.pos, tx));
            self.restart_audio();
            if let Some(s) = &self.stream {
                let _ = s.play();
            }
        }

        pub fn pause(&mut self) {
            self.playing = false;
            self.vdec = None;
            self.vrx = None;
            self.audio_dec = None;
            self.audio_q.lock().unwrap().clear();
            if let Some(s) = &self.stream {
                let _ = s.pause();
            }
        }

        pub fn seek(&mut self, t: f64) {
            let t = t.clamp(0.0, (self.dur - 0.01).max(0.0));
            self.pos = t;
            if self.playing {
                self.play();
            } else {
                self.still_want = Some(t);
            }
        }

        pub fn restart_audio(&mut self) {
            self.audio_dec = None;
            self.audio_q.lock().unwrap().clear();
            if !self.playing {
                return;
            }
            let Some(filter) = self.audio_filter.clone() else { return };
            let stop = Arc::new(AtomicBool::new(false));
            let pid = Arc::new(AtomicU32::new(0));
            let args: Vec<String> = [
                "-hide_banner", "-loglevel", "error", "-ss", &format!("{:.3}", self.pos), "-i",
                &self.path.to_string_lossy(), "-filter_complex", &filter, "-map", "[aout]", "-f", "f32le", "-ac", "2",
                "-ar", &self.out_rate.to_string(), "pipe:1",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            let q = self.audio_q.clone();
            let (s2, p2) = (stop.clone(), pid.clone());
            let max = self.out_rate as usize; // 0.5 s stereo
            std::thread::spawn(move || {
                let Some(mut c) = spawn_ff(&args) else { return };
                p2.store(c.id(), Ordering::SeqCst);
                let mut out = c.stdout.take().unwrap();
                let mut buf = vec![0u8; 8192];
                while !s2.load(Ordering::Relaxed) {
                    let n = match out.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n & !3,
                    };
                    loop {
                        if s2.load(Ordering::Relaxed) {
                            break;
                        }
                        let len = q.lock().unwrap().len();
                        if len < max {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    let mut g = q.lock().unwrap();
                    for c in buf[..n].chunks_exact(4) {
                        g.push_back(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
                    }
                }
                let _ = c.kill();
            });
            self.audio_dec = Some(Dec { stop, pid });
        }

        fn start_video(&self, t: f64, tx: mpsc::SyncSender<Frame>) -> Dec {
            let stop = Arc::new(AtomicBool::new(false));
            let pid = Arc::new(AtomicU32::new(0));
            // play high-refresh clips at their real frame rate (up to 240) at normal preview sizes
            let px = self.pw * self.ph;
            let cap = if px <= 1_100_000 { 240.0 } else if px <= 2_100_000 { 120.0 } else { 60.0 };
            let (pw, ph, pf) = (self.pw, self.ph, self.pf.min(cap));
            // "area" is sharp for shrinking and cheaper than lanczos, which matters above 120 fps
            let flags = if pf > 120.0 { "area" } else { "lanczos" };
            let args: Vec<String> = [
                "-hide_banner", "-loglevel", "error", "-hwaccel", "auto", "-ss", &format!("{t:.3}"), "-i",
                &self.path.to_string_lossy(), "-map", "0:v:0", "-vf", &format!("fps={pf},scale={pw}:{ph}:flags={flags}"),
                "-f", "rawvideo", "-pix_fmt", "rgba", "pipe:1",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            let (s2, p2) = (stop.clone(), pid.clone());
            std::thread::spawn(move || {
                let Some(mut c) = spawn_ff(&args) else { return };
                p2.store(c.id(), Ordering::SeqCst);
                let mut out = c.stdout.take().unwrap();
                let size = (pw * ph * 4) as usize;
                let mut i = 0u64;
                while !s2.load(Ordering::Relaxed) {
                    let mut f = vec![0u8; size];
                    if out.read_exact(&mut f).is_err() {
                        break;
                    }
                    if tx.send((t + i as f64 / pf, f)).is_err() {
                        break;
                    }
                    i += 1;
                }
                let _ = c.kill();
            });
            Dec { stop, pid }
        }

        fn request_still(&mut self, ctx: &egui::Context) {
            let Some(t) = self.still_want else { return };
            if self.still_busy.swap(true, Ordering::SeqCst) {
                return;
            }
            self.still_want = None;
            let (pw, ph) = (self.pw, self.ph);
            let args: Vec<String> = [
                "-hide_banner", "-loglevel", "error", "-ss", &format!("{t:.3}"), "-i", &self.path.to_string_lossy(),
                "-map", "0:v:0", "-frames:v", "1", "-vf", &format!("scale={pw}:{ph}:flags=lanczos"), "-f", "rawvideo",
                "-pix_fmt", "rgba", "pipe:1",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            let (slot, busy, ctx) = (self.still.clone(), self.still_busy.clone(), ctx.clone());
            std::thread::spawn(move || {
                if let Some(mut c) = spawn_ff(&args) {
                    let mut f = vec![0u8; (pw * ph * 4) as usize];
                    if c.stdout.as_mut().unwrap().read_exact(&mut f).is_ok() {
                        *slot.lock().unwrap() = Some((t, f));
                    }
                    let _ = c.kill();
                }
                busy.store(false, Ordering::SeqCst);
                ctx.request_repaint();
            });
        }

        fn upload(&mut self, ctx: &egui::Context, rgba: &[u8]) {
            // a frame decoded before a resize has the old size: skip it, the next one will match
            if rgba.len() != (self.pw * self.ph * 4) as usize {
                if !self.playing {
                    self.still_want = Some(self.pos);
                }
                return;
            }
            // video frames are fully opaque, so they're already "premultiplied": no per-pixel conversion
            let img = egui::ColorImage::from_rgba_premultiplied([self.pw as usize, self.ph as usize], rgba);
            match &mut self.tex {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => self.tex = Some(ctx.load_texture("preview", img, egui::TextureOptions::LINEAR)),
            }
        }

        /// Advance playback. `segs` are the kept ranges: playback skips cut parts and stops at the end.
        pub fn tick(&mut self, ctx: &egui::Context, segs: &[(f64, f64)]) {
            if self.playing {
                let clock = self.clock_pos + self.clock_start.elapsed().as_secs_f64();
                // skip removed ranges
                if let Some(next) = next_kept(segs, clock) {
                    if next > clock + 0.02 {
                        self.seek(next);
                        return;
                    }
                } else {
                    self.pos = segs.last().map(|s| s.1).unwrap_or(self.dur).min(self.dur);
                    self.pause();
                    return;
                }
                let mut latest: Option<Frame> = None;
                if let Some(p) = self.pending.take() {
                    if p.0 <= clock { latest = Some(p) } else { self.pending = Some(p) }
                }
                if self.pending.is_none() {
                    if let Some(rx) = &self.vrx {
                        while let Ok(f) = rx.try_recv() {
                            if f.0 <= clock {
                                latest = Some(f);
                            } else {
                                self.pending = Some(f);
                                break;
                            }
                        }
                    }
                }
                if let Some((t, f)) = latest {
                    self.upload(ctx, &f);
                    self.pos = t;
                } else {
                    self.pos = clock.min(self.dur);
                }
                ctx.request_repaint();
            } else {
                self.request_still(ctx);
                let still = self.still.lock().unwrap().take();
                if let Some((_, f)) = still {
                    self.upload(ctx, &f);
                }
            }
        }
    }

    fn next_kept(segs: &[(f64, f64)], t: f64) -> Option<f64> {
        for &(a, b) in segs {
            if t < b {
                return Some(t.max(a));
            }
        }
        None
    }

    fn default_audio_filter(n: usize) -> Option<String> {
        (n > 0).then(|| "[0:a:0]anull[aout]".to_string())
    }

    fn open_output(q: Arc<Mutex<VecDeque<f32>>>) -> Option<(cpal::Stream, u32)> {
        let host = cpal::default_host();
        let dev = host.default_output_device()?;
        let cfg = dev.default_output_config().ok()?;
        let rate = cfg.sample_rate();
        let ch = cfg.channels() as usize;
        let sc = cfg.config();
        let stream = match cfg.sample_format() {
            cpal::SampleFormat::F32 => dev
                .build_output_stream(
                    sc,
                    move |out: &mut [f32], _: &_| {
                        let mut g = q.lock().unwrap();
                        for f in out.chunks_mut(ch.max(1)) {
                            let l = g.pop_front().unwrap_or(0.0);
                            let r = g.pop_front().unwrap_or(l);
                            for (i, s) in f.iter_mut().enumerate() {
                                *s = if i % 2 == 0 { l } else { r };
                            }
                        }
                    },
                    |_| {},
                    None,
                )
                .ok()?,
            cpal::SampleFormat::I16 => dev
                .build_output_stream(
                    sc,
                    move |out: &mut [i16], _: &_| {
                        let mut g = q.lock().unwrap();
                        for f in out.chunks_mut(ch.max(1)) {
                            let l = g.pop_front().unwrap_or(0.0);
                            let r = g.pop_front().unwrap_or(l);
                            for (i, s) in f.iter_mut().enumerate() {
                                *s = ((if i % 2 == 0 { l } else { r }).clamp(-1.0, 1.0) * 32767.0) as i16;
                            }
                        }
                    },
                    |_| {},
                    None,
                )
                .ok()?,
            _ => return None,
        };
        let _ = stream.pause();
        Some((stream, rate))
    }

    // ====================================================================== editor state

    #[derive(Clone, Copy, PartialEq)]
    pub enum Aspect {
        Original,
        Free,
        Wide,
        Tall,
        Square,
        Portrait,
    }

    impl Aspect {
        fn ratio(self) -> Option<f32> {
            match self {
                Aspect::Wide => Some(16.0 / 9.0),
                Aspect::Tall => Some(9.0 / 16.0),
                Aspect::Square => Some(1.0),
                Aspect::Portrait => Some(4.0 / 5.0),
                _ => None,
            }
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Tool {
        Trim,
        Crop,
        Audio,
        Export,
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Res {
        Original,
        P1080,
        P720,
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Size {
        Original,
        Mb25,
        Mb10,
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Drag {
        Playhead,
        Edge(usize, bool),
        CropMove(Pos2, [f32; 4]),
        CropCorner(usize),
    }

    struct Job {
        progress: Arc<AtomicU32>,
        result: Arc<Mutex<Option<Result<std::path::PathBuf, String>>>>,
        pid: Arc<AtomicU32>,
        lossless: bool,
    }

    pub struct Editor {
        pub path: PathBuf,
        pub probe: Probe,
        player: Player,
        segs: Vec<(f64, f64)>,
        sel: Option<usize>,
        aspect: Aspect,
        /// normalized x, y, w, h
        crop: [f32; 4],
        tracks: Vec<(bool, f32)>,
        blend: Blend,
        blend_fps: u32,
        res: Res,
        size: Size,
        tool: Tool,
        name: String,
        drag: Option<Drag>,
        strip: Arc<Mutex<Option<egui::ColorImage>>>,
        strip_tex: Option<egui::TextureHandle>,
        wave: Arc<Mutex<Option<Vec<f32>>>>,
        job: Option<Job>,
    }

    impl Editor {
        pub fn open(path: &Path) -> Result<Self, String> {
            let probe = probe(path).ok_or("Couldn't read that video")?;
            let player = Player::new(path, &probe);
            let s = core().settings();
                    let mut e = Self {
                path: path.to_path_buf(),
                segs: vec![(0.0, probe.duration)],
                sel: None,
                aspect: Aspect::Original,
                crop: [0.0, 0.0, 1.0, 1.0],
                tracks: probe.tracks.iter().enumerate().map(|(i, _)| (i == 0, 1.0)).collect(),
                blend: s.blend,
                blend_fps: s.blend_fps,
                res: Res::Original,
                size: Size::Original,
                tool: Tool::Trim,
                name: format!("Ferrocap_Edit_{}", sys::timestamp()),
                drag: None,
                strip: Arc::default(),
                strip_tex: None,
                wave: Arc::default(),
                job: None,
                    player,
                probe,
            };
            e.spawn_strip();
            e.spawn_wave();
            e.update_audio_filter();
            Ok(e)
        }

        fn spawn_strip(&self) {
            let (path, dur, slot) = (self.path.clone(), self.probe.duration, self.strip.clone());
            std::thread::spawn(move || {
                let n = 16;
                let args: Vec<String> = [
                    "-hide_banner", "-loglevel", "error", "-i", &path.to_string_lossy(), "-vf",
                    &format!("fps={:.5},scale=-2:90,tile={n}x1", n as f64 / dur.max(0.5)), "-frames:v", "1", "-f",
                    "image2pipe", "-c:v", "png", "pipe:1",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();
                if let Some(mut c) = spawn_ff(&args) {
                    let mut bytes = Vec::new();
                    let _ = c.stdout.as_mut().unwrap().read_to_end(&mut bytes);
                    let _ = c.wait();
                    if let Ok(img) = image::load_from_memory(&bytes) {
                        let img = img.to_rgba8();
                        let size = [img.width() as usize, img.height() as usize];
                        *slot.lock().unwrap() = Some(egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()));
                        core().repaint();
                    }
                }
            });
        }

        fn spawn_wave(&self) {
            if self.probe.tracks.is_empty() {
                return;
            }
            let (path, slot) = (self.path.clone(), self.wave.clone());
            std::thread::spawn(move || {
                let args: Vec<String> = [
                    "-hide_banner", "-loglevel", "error", "-i", &path.to_string_lossy(), "-map", "0:a:0", "-ac", "1",
                    "-ar", "8000", "-f", "f32le", "pipe:1",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();
                if let Some(mut c) = spawn_ff(&args) {
                    let mut bytes = Vec::new();
                    let _ = c.stdout.as_mut().unwrap().read_to_end(&mut bytes);
                    let _ = c.wait();
                    // 100 peaks per second
                    let peaks: Vec<f32> = bytes
                        .chunks(80 * 4)
                        .map(|ch| {
                            ch.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]).abs()).fold(0.0, f32::max)
                        })
                        .collect();
                    *slot.lock().unwrap() = Some(peaks);
                    core().repaint();
                }
            });
        }

        fn update_audio_filter(&mut self) {
            let on: Vec<(usize, f32)> = self.tracks.iter().enumerate().filter(|(_, t)| t.0).map(|(i, t)| (i, t.1)).collect();
            self.player.audio_filter = if on.is_empty() {
                None
            } else {
                let mut parts: Vec<String> = on.iter().map(|(i, v)| format!("[0:a:{i}]volume={v:.2}[t{i}]")).collect();
                let ins: String = on.iter().map(|(i, _)| format!("[t{i}]")).collect();
                if on.len() == 1 {
                    parts.push(format!("{ins}anull[aout]"));
                } else {
                    parts.push(format!("{ins}amix=inputs={}:normalize=0:duration=longest[aout]", on.len()));
                }
                Some(parts.join(";"))
            };
            self.player.restart_audio();
        }

        pub fn stop(&mut self) {
            self.player.pause();
        }

        fn kept_duration(&self) -> f64 {
            self.segs.iter().map(|(a, b)| b - a).sum()
        }

        fn is_lossless(&self) -> bool {
            self.segs.len() == 1
                && self.aspect == Aspect::Original
                && self.blend == Blend::Off
                && self.res == Res::Original
                && self.size == Size::Original
                && self.tracks.iter().enumerate().all(|(i, t)| t.0 == (i == 0) && (t.1 - 1.0).abs() < 0.01)
        }

        fn split(&mut self) {
            let t = self.player.pos;
            if let Some(i) = self.segs.iter().position(|&(a, b)| t > a + 0.1 && t < b - 0.1) {
                let (a, b) = self.segs[i];
                self.segs[i] = (a, t);
                self.segs.insert(i + 1, (t, b));
                self.sel = Some(i + 1);
            }
        }

        fn delete_sel(&mut self) {
            if let Some(i) = self.sel {
                if self.segs.len() > 1 && i < self.segs.len() {
                    self.segs.remove(i);
                    self.sel = None;
                }
            }
        }

        // ------------------------------------------------------------------ export

        fn start_export(&mut self) {
            let Some(ff) = sys::ffmpeg_path() else { return };
            let s = core().settings();
            let dir = self.path.parent().map(Path::to_path_buf).unwrap_or_else(|| s.clips_dir.clone());
            let stem = sys::sanitize_filename(&self.name);
            let out = sys::unique_path(&dir, if stem.is_empty() { "Edit" } else { &stem }, "mp4");
            let src = self.path.to_string_lossy().to_string();
            let total = self.kept_duration().max(0.1);
            let lossless = self.is_lossless();
            let mut args: Vec<String> = vec!["-hide_banner".into(), "-loglevel".into(), "error".into(), "-nostats".into()];
            if lossless {
                let (a, b) = self.segs[0];
                args.extend(["-ss".into(), format!("{a:.3}"), "-to".into(), format!("{b:.3}"), "-i".into(), src]);
                args.extend(["-map".into(), "0".into(), "-c".into(), "copy".into(), "-avoid_negative_ts".into(), "make_zero".into()]);
            } else {
                args.extend(["-hwaccel".into(), "auto".into(), "-i".into(), src]);
                let mut g: Vec<String> = Vec::new();
                let on: Vec<(usize, f32)> =
                    self.tracks.iter().enumerate().filter(|(_, t)| t.0).map(|(i, t)| (i, t.1)).collect();
                let mut concat_in = String::new();
                for (k, (a, b)) in self.segs.iter().enumerate() {
                    g.push(format!("[0:v]trim=start={a:.3}:end={b:.3},setpts=PTS-STARTPTS[v{k}]"));
                    concat_in += &format!("[v{k}]");
                    if !on.is_empty() {
                        let mut ins = String::new();
                        for (i, v) in &on {
                            g.push(format!("[0:a:{i}]atrim=start={a:.3}:end={b:.3},asetpts=PTS-STARTPTS,volume={v:.2}[a{k}_{i}]"));
                            ins += &format!("[a{k}_{i}]");
                        }
                        if on.len() == 1 {
                            g.push(format!("{ins}anull[a{k}]"));
                        } else {
                            g.push(format!("{ins}amix=inputs={}:normalize=0:duration=longest[a{k}]", on.len()));
                        }
                        concat_in += &format!("[a{k}]");
                    }
                }
                let n = self.segs.len();
                let has_a = !on.is_empty();
                g.push(format!("{concat_in}concat=n={n}:v=1:a={}[vc]{}", has_a as u8, if has_a { "[ac]" } else { "" }));
                // crop / scale / blend
                let (sw, sh) = (self.probe.width as f32, self.probe.height as f32);
                let mut post: Vec<String> = Vec::new();
                let (mut ow, mut oh) = (sw, sh);
                if self.aspect != Aspect::Original {
                    let [x, y, w, h] = self.crop;
                    let cw = ((w * sw) as u32 & !1).max(16);
                    let chh = ((h * sh) as u32 & !1).max(16);
                    post.push(format!("crop={cw}:{chh}:{}:{}", (x * sw) as u32, (y * sh) as u32));
                    ow = cw as f32;
                    oh = chh as f32;
                }
                let target_h = match self.res {
                    Res::P1080 => Some(1080.0),
                    Res::P720 => Some(720.0),
                    Res::Original => None,
                };
                if let Some(th) = target_h {
                    let short = ow.min(oh);
                    if short > th {
                        if ow >= oh { post.push(format!("scale=-2:{th}")) } else { post.push(format!("scale={th}:-2")) }
                        let k = th / short;
                        ow *= k;
                        oh *= k;
                    }
                }
                let mut out_fps = self.probe.fps;
                if self.blend != Blend::Off {
                    out_fps = blend_out_fps(self.probe.fps, self.blend_fps);
                    let frames = blend_frames(self.blend, self.probe.fps, out_fps);
                    post.push(format!("tmix=frames={frames}"));
                    post.push(format!("fps={out_fps}"));
                }
                post.push("format=yuv420p".into());
                g.push(format!("[vc]{}[vout]", post.join(",")));
                args.extend(["-filter_complex".into(), g.join(";"), "-map".into(), "[vout]".into()]);
                if has_a {
                    args.extend(["-map".into(), "[ac]".into()]);
                }
                // bitrate
                let px = (ow * oh) as f64;
                let mut kbps = (18_000.0 * (px / 2_073_600.0).powf(0.8) * (out_fps / 60.0).powf(0.6)) as u32;
                let limit_mb = match self.size {
                    Size::Mb25 => Some(24.0),
                    Size::Mb10 => Some(9.5),
                    Size::Original => None,
                };
                if let Some(mb) = limit_mb {
                    let budget = (mb * 8_000.0 / total) as u32;
                    kbps = kbps.min(budget.saturating_sub(if has_a { 160 } else { 0 }).max(400));
                }
                let enc = s.encoder_pref.codec().map(String::from).or(s.encoder.clone()).unwrap_or_else(|| "libx264".into());
                let gop = (out_fps.round() as u32 * 2).to_string();
                let k = format!("{kbps}k");
                let max = format!("{}k", if limit_mb.is_some() { kbps } else { kbps * 3 / 2 });
                let buf = format!("{}k", kbps * 2);
                let mut ea = crate::engine::encoder_args(&enc, &k, &max, &buf, &gop, false, s.quality, out_fps.round() as u32);
                if limit_mb.is_some() && enc == "libx264" {
                    // crf ignores the size budget: use plain bitrate mode
                    if let Some(i) = ea.iter().position(|a| a == "-crf") {
                        ea.drain(i..i + 2);
                        ea.extend(["-b:v".into(), k.clone()]);
                    }
                }
                args.extend(ea);
                if has_a {
                    args.extend(["-c:a".into(), "aac".into(), "-b:a".into(), "160k".into()]);
                }
                args.extend(["-movflags".into(), "+faststart".into()]);
            }
            args.extend(["-map_metadata".into(), "0".into()]);
            args.extend(["-progress".into(), "pipe:1".into(), "-y".into(), out.to_string_lossy().to_string()]);

            let progress = Arc::new(AtomicU32::new(0));
            let result: Arc<Mutex<Option<Result<std::path::PathBuf, String>>>> = Arc::default();
            let pid = Arc::new(AtomicU32::new(0));
            let (p2, r2, pid2) = (progress.clone(), result.clone(), pid.clone());
            std::thread::spawn(move || {
                let mut cmd = sys::quiet(&ff);
                {
                    use std::os::windows::process::CommandExt;
                    cmd.creation_flags(sys::CREATE_NO_WINDOW | sys::BELOW_NORMAL);
                }
                let child = cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
                let mut child = match child {
                    Ok(c) => c,
                    Err(e) => {
                        *r2.lock().unwrap() = Some(Err(e.to_string()));
                        return;
                    }
                };
                sys::adopt_child(&child);
                pid2.store(child.id(), Ordering::SeqCst);
                let mut err = child.stderr.take().unwrap();
                let et = std::thread::spawn(move || {
                    let mut s = String::new();
                    let _ = err.read_to_string(&mut s);
                    s
                });
                for line in BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok) {
                    if let Some(v) = line.strip_prefix("out_time_us=") {
                        if let Ok(us) = v.trim().parse::<f64>() {
                            p2.store(((us / 1e6 / total).clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed);
                            core().repaint();
                        }
                    }
                }
                let st = child.wait();
                let errtxt = et.join().unwrap_or_default();
                *r2.lock().unwrap() = Some(match st {
                    Ok(s) if s.success() => Ok(out),
                    _ => {
                        let _ = std::fs::remove_file(&out);
                        Err(crate::engine::tidy_error(errtxt.lines().last().unwrap_or("export failed")))
                    }
                });
                core().repaint();
            });
            self.job = Some(Job { progress, result, pid, lossless });
        }

        // ------------------------------------------------------------------ ui

        /// Returns true when the editor should close.
        pub fn ui(&mut self, ui: &mut Ui) -> bool {
            let p = pal();
            let ctx = ui.ctx().clone();
            let mut close = false;
            self.player.tick(&ctx, &self.segs);
            if self.strip_tex.is_none() {
                if let Some(img) = self.strip.lock().unwrap().take() {
                    self.strip_tex = Some(ctx.load_texture("strip", img, egui::TextureOptions::LINEAR));
                }
            }
            // finished export?
            let finished = self.job.as_ref().and_then(|j| j.result.lock().unwrap().take());
            if let Some(r) = finished {
                self.job = None;
                match r {
                    Ok(path) => {
                        core().library_dirty.store(true, Ordering::SeqCst);
                        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                        core().toast(ToastKind::Clip, "Export finished", sys::human_bytes(bytes), Some(path));
                    }
                    Err(e) => core().toast(ToastKind::Error, "Export failed", e, None),
                }
            }

            // keyboard
            let typing = ctx.memory(|m| m.focused().is_some());
            if !typing {
                ctx.input(|i| {
                    if i.key_pressed(Key::Space) {
                        if self.player.playing { self.player.pause() } else { self.player.play() }
                    }
                    if i.key_pressed(Key::S) {
                        self.split();
                    }
                    if i.key_pressed(Key::Delete) || i.key_pressed(Key::Backspace) {
                        self.delete_sel();
                    }
                    if i.key_pressed(Key::ArrowLeft) {
                        let t = self.player.pos - 1.0 / self.probe.fps.max(1.0) * if i.modifiers.shift { 30.0 } else { 1.0 };
                        self.player.seek(t);
                    }
                    if i.key_pressed(Key::ArrowRight) {
                        let t = self.player.pos + 1.0 / self.probe.fps.max(1.0) * if i.modifiers.shift { 30.0 } else { 1.0 };
                        self.player.seek(t);
                    }
                });
            }

            let full = ui.available_rect_before_wrap();
            let side_w = 330.0;
            let gap = 18.0;
            let tl_h = 190.0;
            let preview_area = Rect::from_min_max(full.min, pos2(full.right() - side_w - gap, full.bottom() - tl_h - gap));
            let side = Rect::from_min_max(pos2(full.right() - side_w, full.top()), pos2(full.right(), full.bottom() - tl_h - gap));
            let tl = Rect::from_min_max(pos2(full.left(), full.bottom() - tl_h), full.max);
            ui.allocate_rect(full, Sense::hover());

            // ---------------- preview
            let painter = ui.painter().clone();
            frame_box(&painter, preview_area);
            let inner = preview_area.shrink(14.0);
            let ar = self.player.aspect();
            let mut vw = inner.width();
            let mut vh = vw / ar;
            if vh > inner.height() - 40.0 {
                vh = inner.height() - 40.0;
                vw = vh * ar;
            }
            let vid = Rect::from_center_size(pos2(inner.center().x, inner.top() + vh / 2.0), vec2(vw, vh));
            if self.drag.is_none() {
                self.player.fit_to(vid.width() * ctx.pixels_per_point());
            }
            painter.rect_filled(vid, CornerRadius::same(4), Color32::BLACK);
            if let Some(t) = &self.player.tex {
                painter.image(t.id(), vid, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
            } else {
                painter.text(vid.center(), Align2::CENTER_CENTER, "loading…", mono(12.0), p.dim);
            }
            let vresp = ui.interact(vid, ui.scope_id().with("vid"), Sense::click_and_drag());
            if self.aspect != Aspect::Original {
                self.crop_overlay(ui, vid, &vresp);
            } else if vresp.clicked() {
                if self.player.playing { self.player.pause() } else { self.player.play() }
            }
            // transport row
            let tr = Rect::from_min_max(pos2(inner.left(), vid.bottom() + 10.0), pos2(inner.right(), vid.bottom() + 38.0));
            ui.scope_builder(egui::UiBuilder::new().max_rect(tr).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
                if play_button(ui, self.player.playing).clicked() {
                    if self.player.playing { self.player.pause() } else { self.player.play() }
                }
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(format!("{}  /  {}", fmt_t(self.player.pos), fmt_t(self.probe.duration)))
                        .font(mono(12.5))
                        .color(p.text),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "{}×{} · {:.0} fps · {} track{}",
                            self.probe.width,
                            self.probe.height,
                            self.probe.fps,
                            self.probe.tracks.len(),
                            if self.probe.tracks.len() == 1 { "" } else { "s" }
                        ))
                        .font(mono(11.5))
                        .color(p.dim),
                    );
                });
            });

            // ---------------- side panel
            frame_box(&painter, side);
            ui.scope_builder(egui::UiBuilder::new().max_rect(side.shrink2(vec2(18.0, 16.0))), |ui| {
                ui.horizontal(|ui| {
                    let mut t = self.tool;
                    segmented(ui, &mut t, &[(Tool::Trim, "Trim"), (Tool::Crop, "Crop"), (Tool::Audio, "Audio"), (Tool::Export, "Export")], 72.0);
                    self.tool = t;
                });
                ui.add_space(14.0);
                match self.tool {
                    Tool::Trim => self.panel_trim(ui),
                    Tool::Crop => self.panel_crop(ui),
                    Tool::Audio => self.panel_audio(ui),
                    Tool::Export => self.panel_export(ui),
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    if button(ui, "← Back to library").clicked() {
                        close = true;
                    }
                });
            });

            // ---------------- timeline
            self.timeline(ui, tl);
            close
        }

        fn panel_trim(&mut self, ui: &mut Ui) {
            let p = pal();
            section_title(ui, "Trim & cut");
            hint(ui, "Drag the edges on the timeline. Split at the playhead (S), select a piece and Delete it to cut it out.");
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if button(ui, "Split  S").clicked() {
                    self.split();
                }
                let can_del = self.sel.is_some() && self.segs.len() > 1;
                if button_enabled(ui, "Delete piece", can_del).clicked() {
                    self.delete_sel();
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if button(ui, "Set start here").clicked() {
                    let t = self.player.pos;
                    if let Some(f) = self.segs.first_mut() {
                        if t < f.1 - 0.2 {
                            f.0 = t;
                        }
                    }
                }
                if button(ui, "Set end here").clicked() {
                    let t = self.player.pos;
                    if let Some(l) = self.segs.last_mut() {
                        if t > l.0 + 0.2 {
                            l.1 = t;
                        }
                    }
                }
            });
            ui.add_space(8.0);
            if button(ui, "Reset").clicked() {
                self.segs = vec![(0.0, self.probe.duration)];
                self.sel = None;
            }
            ui.add_space(16.0);
            stat_line(ui, "Kept", &fmt_t(self.kept_duration()), p.text);
            stat_line(ui, "Pieces", &self.segs.len().to_string(), p.text);
            stat_line(ui, "Removed", &fmt_t(self.probe.duration - self.kept_duration()), p.muted);
        }

        fn panel_crop(&mut self, ui: &mut Ui) {
            section_title(ui, "Crop");
            hint(ui, "Pick a shape, then drag the box on the video. 9:16 is ready for TikTok, Shorts and Reels.");
            ui.add_space(10.0);
            let opts = [
                (Aspect::Original, "Original"),
                (Aspect::Free, "Free"),
                (Aspect::Wide, "16:9"),
                (Aspect::Tall, "9:16"),
                (Aspect::Square, "1:1"),
                (Aspect::Portrait, "4:5"),
            ];
            for row in opts.chunks(3) {
                ui.horizontal(|ui| {
                    for (a, label) in row {
                        let on = self.aspect == *a;
                        if pill(ui, label, on, 92.0).clicked() && !on {
                            self.aspect = *a;
                            self.reset_crop();
                        }
                    }
                });
                ui.add_space(4.0);
            }
            if self.aspect != Aspect::Original {
                ui.add_space(10.0);
                let w = (self.crop[2] * self.probe.width as f32) as u32 & !1;
                let h = (self.crop[3] * self.probe.height as f32) as u32 & !1;
                stat_line(ui, "Output", &format!("{w} × {h}"), pal().text);
                ui.add_space(6.0);
                if button(ui, "Center").clicked() {
                    self.crop[0] = (1.0 - self.crop[2]) / 2.0;
                    self.crop[1] = (1.0 - self.crop[3]) / 2.0;
                }
            }
        }

        fn reset_crop(&mut self) {
            let src_ar = self.probe.width as f32 / self.probe.height.max(1) as f32;
            self.crop = match self.aspect.ratio() {
                None => [0.05, 0.05, 0.9, 0.9],
                Some(r) => {
                    // largest centered box with aspect r, in normalized units
                    let (w, h) = if r < src_ar { (r / src_ar, 1.0) } else { (1.0, src_ar / r) };
                    [(1.0 - w) / 2.0, (1.0 - h) / 2.0, w, h]
                }
            };
            if self.aspect == Aspect::Original {
                self.crop = [0.0, 0.0, 1.0, 1.0];
            }
        }

        fn panel_audio(&mut self, ui: &mut Ui) {
            let p = pal();
            section_title(ui, "Audio tracks");
            hint(ui, "Mute what you don't want: Discord, your mic, the whole desktop. Mix and System already contain the app tracks.");
            ui.add_space(10.0);
            if self.probe.tracks.is_empty() {
                ui.label(egui::RichText::new("This clip has no audio.").font(body(13.0)).color(p.muted));
                return;
            }
            let mut changed = false;
            let names = self.probe.tracks.clone();
            for (i, name) in names.iter().enumerate() {
                let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 58.0), Sense::hover());
                let pt = ui.painter();
                pt.rect_filled(rect, CornerRadius::same(6), p.panel2);
                pt.rect_stroke(rect, CornerRadius::same(6), Stroke::new(1.0, p.line), StrokeKind::Inside);
                let on = self.tracks[i].0;
                pt.text(rect.min + vec2(14.0, 18.0), Align2::LEFT_CENTER, name, semibold(13.5), if on { p.text } else { p.dim });
                pt.text(
                    rect.min + vec2(14.0, 40.0),
                    Align2::LEFT_CENTER,
                    format!("{:.0}%", self.tracks[i].1 * 100.0),
                    mono(11.0),
                    p.muted,
                );
                let tg = Rect::from_min_size(pos2(rect.right() - 58.0, rect.center().y - 12.0), vec2(44.0, 24.0));
                ui.scope_builder(egui::UiBuilder::new().max_rect(tg), |ui| {
                    if toggle(ui, &mut self.tracks[i].0).changed() {
                        changed = true;
                    }
                });
                let sl = Rect::from_min_size(pos2(rect.left() + 64.0, rect.top() + 30.0), vec2(rect.width() - 140.0, 20.0));
                ui.scope_builder(egui::UiBuilder::new().max_rect(sl), |ui| {
                    if value_slider(ui, &mut self.tracks[i].1, 0.0, 2.0, sl.width()) {
                        changed = true;
                    }
                });
                ui.add_space(6.0);
            }
            if changed {
                self.update_audio_filter();
            }
        }

        fn panel_export(&mut self, ui: &mut Ui) {
            let p = pal();
            section_title(ui, "Export");
            ui.add_space(4.0);
            ui.label(egui::RichText::new("Name").font(mono_bold(10.5)).color(p.dim));
            ui.add(egui::TextEdit::singleline(&mut self.name).font(body(13.5)).desired_width(f32::INFINITY));
            ui.add_space(10.0);
            ui.label(egui::RichText::new("RESOLUTION").font(mono_bold(10.5)).color(p.dim));
            segmented(ui, &mut self.res, &[(Res::Original, "Source"), (Res::P1080, "1080p"), (Res::P720, "720p")], 92.0);
            ui.add_space(10.0);
            ui.label(egui::RichText::new("FILE SIZE").font(mono_bold(10.5)).color(p.dim));
            segmented(ui, &mut self.size, &[(Size::Original, "Best"), (Size::Mb25, "≤ 25 MB"), (Size::Mb10, "≤ 10 MB")], 92.0);
            ui.add_space(10.0);
            ui.label(egui::RichText::new("FRAME BLENDING").font(mono_bold(10.5)).color(p.dim));
            segmented(ui, &mut self.blend, &[(Blend::Off, "Off"), (Blend::Light, "Light"), (Blend::Strong, "Strong")], 92.0);
            if self.blend != Blend::Off {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Output").font(body(12.5)).color(p.muted));
                    segmented(ui, &mut self.blend_fps, &[(0, "Original"), (60, "60 fps"), (30, "30 fps")], 70.0);
                });
                let src = self.probe.fps;
                let out = blend_out_fps(src, self.blend_fps);
                let note = if (out - src).abs() < 0.5 {
                    format!("Stays {src:.0} fps, smooth as recorded, with a soft motion trail.")
                } else {
                    format!("{src:.0} → {out:.0} fps, frames merged for motion blur.")
                };
                hint(ui, &note);
            }
            ui.add_space(14.0);
            let lossless = self.is_lossless();
            let mode = if lossless { "Lossless copy · instant" } else { "Re-encode on the GPU" };
            stat_line(ui, "Length", &fmt_t(self.kept_duration()), p.text);
            stat_line(ui, "Mode", mode, if lossless { p.accent } else { p.muted });
            ui.add_space(12.0);
            if let Some(j) = &self.job {
                let f = j.progress.load(Ordering::Relaxed) as f32 / 1000.0;
                let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::hover());
                let pt = ui.painter();
                pt.rect_filled(r, CornerRadius::same(6), p.panel2);
                let fr = Rect::from_min_size(r.min, vec2(r.width() * if j.lossless { 0.5 } else { f }, r.height()));
                pt.rect_filled(fr, CornerRadius::same(6), alpha(p.accent, 0.35));
                pt.text(r.center(), Align2::CENTER_CENTER, format!("Exporting… {:.0}%", f * 100.0), semibold(13.0), p.text);
                ui.add_space(6.0);
                if link(ui, "Cancel", p.muted).clicked() {
                    kill_pid(j.pid.load(Ordering::SeqCst));
                }
            } else if primary(ui, "Export clip", ui.available_width()).clicked() {
                self.player.pause();
                self.start_export();
            }
        }

        fn crop_overlay(&mut self, ui: &mut Ui, vid: Rect, resp: &egui::Response) {
            let p = pal();
            let [x, y, w, h] = self.crop;
            let r = Rect::from_min_size(
                pos2(vid.left() + x * vid.width(), vid.top() + y * vid.height()),
                vec2(w * vid.width(), h * vid.height()),
            );
            let pt = ui.painter();
            let shade = alpha(Color32::BLACK, 0.62);
            pt.rect_filled(Rect::from_min_max(vid.min, pos2(vid.right(), r.top())), CornerRadius::ZERO, shade);
            pt.rect_filled(Rect::from_min_max(pos2(vid.left(), r.bottom()), vid.max), CornerRadius::ZERO, shade);
            pt.rect_filled(Rect::from_min_max(pos2(vid.left(), r.top()), pos2(r.left(), r.bottom())), CornerRadius::ZERO, shade);
            pt.rect_filled(Rect::from_min_max(pos2(r.right(), r.top()), pos2(vid.right(), r.bottom())), CornerRadius::ZERO, shade);
            pt.rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.5, p.accent), StrokeKind::Middle);
            for i in 1..3 {
                let fx = r.left() + r.width() * i as f32 / 3.0;
                let fy = r.top() + r.height() * i as f32 / 3.0;
                pt.line_segment([pos2(fx, r.top()), pos2(fx, r.bottom())], Stroke::new(1.0, alpha(Color32::WHITE, 0.18)));
                pt.line_segment([pos2(r.left(), fy), pos2(r.right(), fy)], Stroke::new(1.0, alpha(Color32::WHITE, 0.18)));
            }
            let corners = [r.left_top(), r.right_top(), r.left_bottom(), r.right_bottom()];
            for c in corners {
                pt.rect_filled(Rect::from_center_size(c, vec2(10.0, 10.0)), CornerRadius::same(2), p.accent);
            }
            if let Some(pos) = resp.interact_pointer_pos() {
                if resp.drag_started() {
                    self.drag = corners
                        .iter()
                        .position(|c| c.distance(pos) < 14.0)
                        .map(Drag::CropCorner)
                        .or_else(|| r.contains(pos).then_some(Drag::CropMove(pos, self.crop)));
                }
                if resp.dragged() {
                    match self.drag {
                        Some(Drag::CropMove(start, orig)) => {
                            let d = pos - start;
                            self.crop[0] = (orig[0] + d.x / vid.width()).clamp(0.0, 1.0 - orig[2]);
                            self.crop[1] = (orig[1] + d.y / vid.height()).clamp(0.0, 1.0 - orig[3]);
                        }
                        Some(Drag::CropCorner(i)) => {
                            let nx = ((pos.x - vid.left()) / vid.width()).clamp(0.0, 1.0);
                            let ny = ((pos.y - vid.top()) / vid.height()).clamp(0.0, 1.0);
                            let (mut x0, mut y0, mut x1, mut y1) = (x, y, x + w, y + h);
                            match i {
                                0 => { x0 = nx; y0 = ny; }
                                1 => { x1 = nx; y0 = ny; }
                                2 => { x0 = nx; y1 = ny; }
                                _ => { x1 = nx; y1 = ny; }
                            }
                            let mut nw = (x1 - x0).max(0.05);
                            let mut nh = (y1 - y0).max(0.05);
                            if let Some(ar) = self.aspect.ratio() {
                                let src_ar = vid.width() / vid.height();
                                // keep aspect: height follows width
                                nh = nw * src_ar / ar;
                                if nh > 1.0 {
                                    nh = 1.0;
                                    nw = nh * ar / src_ar;
                                }
                            }
                            let ax = if i == 0 || i == 2 { x + w - nw } else { x };
                            let ay = if i == 0 || i == 1 { y + h - nh } else { y };
                            self.crop = [ax.clamp(0.0, 1.0 - nw), ay.clamp(0.0, 1.0 - nh), nw.min(1.0), nh.min(1.0)];
                        }
                        _ => {}
                    }
                }
            }
            if resp.drag_stopped() {
                self.drag = None;
            }
        }

        fn timeline(&mut self, ui: &mut Ui, rect: Rect) {
            let p = pal();
            let pt = ui.painter().clone();
            frame_box(&pt, rect);
            let inner = rect.shrink2(vec2(18.0, 14.0));
            let dur = self.probe.duration.max(0.01);
            let ruler = Rect::from_min_size(inner.min, vec2(inner.width(), 18.0));
            let strip = Rect::from_min_size(pos2(inner.left(), ruler.bottom() + 6.0), vec2(inner.width(), 84.0));
            let wave_r = Rect::from_min_size(pos2(inner.left(), strip.bottom() + 6.0), vec2(inner.width(), 40.0));
            let x_of = |t: f64| strip.left() + (t / dur) as f32 * strip.width();
            let t_of = |x: f32| (((x - strip.left()) / strip.width()) as f64 * dur).clamp(0.0, dur);

            // ruler
            let step = [0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0]
                .into_iter()
                .find(|s| strip.width() / (dur / s) as f32 > 70.0)
                .unwrap_or(60.0);
            let mut t = 0.0;
            while t <= dur + 0.001 {
                let x = x_of(t);
                pt.line_segment([pos2(x, ruler.bottom() - 5.0), pos2(x, ruler.bottom())], Stroke::new(1.0, p.line2));
                pt.text(pos2(x + 3.0, ruler.center().y - 2.0), Align2::LEFT_CENTER, fmt_t(t), mono(10.0), p.dim);
                t += step;
            }

            // filmstrip
            pt.rect_filled(strip, CornerRadius::same(4), p.bg2);
            if let Some(tex) = &self.strip_tex {
                pt.image(tex.id(), strip, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), alpha(Color32::WHITE, 0.55));
            }
            // dim removed parts
            let mut last = 0.0;
            for &(a, b) in self.segs.iter().chain(std::iter::once(&(dur, dur))) {
                if a > last + 0.001 {
                    let r = Rect::from_min_max(pos2(x_of(last), strip.top()), pos2(x_of(a), strip.bottom()));
                    pt.rect_filled(r, CornerRadius::ZERO, alpha(p.bg, 0.82));
                    // hatching
                    let clip = pt.with_clip_rect(r);
                    let mut hx = r.left() - r.height();
                    while hx < r.right() {
                        clip.line_segment([pos2(hx, r.bottom()), pos2(hx + r.height(), r.top())], Stroke::new(1.0, alpha(p.warn, 0.25)));
                        hx += 9.0;
                    }
                }
                last = b;
            }
            // kept segments
            for (i, &(a, b)) in self.segs.iter().enumerate() {
                let r = Rect::from_min_max(pos2(x_of(a), strip.top()), pos2(x_of(b), strip.bottom()));
                let sel = self.sel == Some(i);
                pt.rect_stroke(r, CornerRadius::same(4), Stroke::new(if sel { 2.0 } else { 1.2 }, if sel { p.accent2 } else { p.accent }), StrokeKind::Inside);
                for (x, left) in [(r.left(), true), (r.right(), false)] {
                    let hr = Rect::from_center_size(pos2(if left { x + 5.0 } else { x - 5.0 }, r.center().y), vec2(10.0, 40.0));
                    pt.rect_filled(hr, CornerRadius::same(4), p.accent);
                    pt.line_segment([hr.center() - vec2(0.0, 9.0), hr.center() + vec2(0.0, 9.0)], Stroke::new(1.5, on(p.accent)));
                }
            }

            // waveform
            pt.rect_filled(wave_r, CornerRadius::same(4), p.bg2);
            if let Some(w) = self.wave.lock().unwrap().as_ref() {
                let n = w.len().max(1);
                let cols = wave_r.width() as usize / 2;
                for c in 0..cols {
                    let i0 = c * n / cols;
                    let i1 = ((c + 1) * n / cols).max(i0 + 1).min(n);
                    let v = w[i0..i1].iter().cloned().fold(0.0, f32::max).min(1.0).sqrt();
                    let x = wave_r.left() + c as f32 * 2.0 + 1.0;
                    let t = t_of(x);
                    let kept = self.segs.iter().any(|&(a, b)| t >= a && t <= b);
                    let h = (v * wave_r.height() * 0.9).max(1.0);
                    pt.line_segment(
                        [pos2(x, wave_r.center().y - h / 2.0), pos2(x, wave_r.center().y + h / 2.0)],
                        Stroke::new(1.2, if kept { alpha(p.accent2, 0.8) } else { alpha(p.dim, 0.5) }),
                    );
                }
            } else {
                pt.text(wave_r.center(), Align2::CENTER_CENTER, "reading audio…", mono(10.5), p.dim);
            }

            // interaction: hit-test where the button went DOWN (not where the drag was recognised),
            // so grabbing a trim handle always resizes instead of moving the playhead
            let area = Rect::from_min_max(ruler.min, wave_r.max);
            let resp = ui.interact(area, ui.scope_id().with("timeline"), Sense::click_and_drag());
            let edge_at = |segs: &[(f64, f64)], pos: Pos2| -> Option<Drag> {
                if pos.y < strip.top() - 6.0 || pos.y > wave_r.bottom() + 4.0 {
                    return None;
                }
                let mut best: Option<(f32, Drag)> = None;
                for (i, &(a, b)) in segs.iter().enumerate() {
                    for (x, left) in [(x_of(a), true), (x_of(b), false)] {
                        let d = (x - pos.x).abs();
                        if d <= 12.0 && best.map(|(bd, _)| d < bd).unwrap_or(true) {
                            best = Some((d, Drag::Edge(i, left)));
                        }
                    }
                }
                best.map(|(_, d)| d)
            };
            let timeline_drag = matches!(self.drag, Some(Drag::Playhead) | Some(Drag::Edge(..)));
            if resp.is_pointer_button_down_on() && !timeline_drag {
                if let Some(o) = ui.input(|i| i.pointer.press_origin()) {
                    if area.contains(o) {
                        let hit = edge_at(&self.segs, o);
                        self.drag = Some(hit.unwrap_or(Drag::Playhead));
                        if hit.is_none() {
                            let t = t_of(o.x);
                            if strip.contains(o) {
                                self.sel = self.segs.iter().position(|&(a, b)| t >= a && t <= b);
                            }
                            self.player.seek(t);
                        }
                    }
                }
            }
            let timeline_drag = matches!(self.drag, Some(Drag::Playhead) | Some(Drag::Edge(..)));
            if timeline_drag && ui.input(|i| i.pointer.primary_down()) {
                if let Some(pos) = ui.ctx().pointer_interact_pos() {
                    let t = t_of(pos.x);
                    match self.drag {
                        Some(Drag::Playhead) => {
                            if (t - self.player.pos).abs() > 0.02 {
                                self.player.seek(t);
                            }
                        }
                        Some(Drag::Edge(i, left)) if i < self.segs.len() => {
                            let lo = if i > 0 { self.segs[i - 1].1 } else { 0.0 };
                            let hi = if i + 1 < self.segs.len() { self.segs[i + 1].0 } else { dur };
                            if left {
                                self.segs[i].0 = t.clamp(lo, self.segs[i].1 - 0.2);
                                self.player.seek(self.segs[i].0);
                            } else {
                                self.segs[i].1 = t.clamp(self.segs[i].0 + 0.2, hi);
                                self.player.seek((self.segs[i].1 - 0.03).max(self.segs[i].0));
                            }
                            self.sel = Some(i);
                        }
                        _ => {}
                    }
                }
            }
            if timeline_drag && !ui.input(|i| i.pointer.primary_down()) {
                self.drag = None;
            }
            // cursor + handle highlight
            let grabbing = matches!(self.drag, Some(Drag::Edge(..)));
            let hover_edge = resp.hover_pos().and_then(|h| edge_at(&self.segs, h));
            if grabbing || hover_edge.is_some() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            } else if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if let Some(Drag::Edge(i, left)) = if grabbing { self.drag } else { hover_edge } {
                if let Some(&(a, b)) = self.segs.get(i) {
                    let x = if left { x_of(a) } else { x_of(b) };
                    let hr = Rect::from_center_size(pos2(if left { x + 5.0 } else { x - 5.0 }, strip.center().y), vec2(10.0, 44.0));
                    pt.rect_filled(hr.expand(4.0), CornerRadius::same(5), alpha(p.accent, 0.25));
                    pt.rect_filled(hr, CornerRadius::same(4), p.accent);
                    pt.line_segment([hr.center() - vec2(0.0, 9.0), hr.center() + vec2(0.0, 9.0)], Stroke::new(1.5, on(p.accent)));
                    let tt = if left { a } else { b };
                    let lab = fmt_t(tt);
                    let g = pt.layout_no_wrap(lab, semibold(11.5), on(p.accent));
                    let tr = Rect::from_center_size(pos2(x, strip.top() - 12.0), g.size() + vec2(12.0, 6.0));
                    pt.rect_filled(tr, CornerRadius::same(5), p.accent);
                    pt.galley(tr.min + vec2(6.0, 3.0), g, on(p.accent));
                }
            }

            // playhead
            let x = x_of(self.player.pos);
            pt.line_segment([pos2(x, ruler.top()), pos2(x, wave_r.bottom())], Stroke::new(1.5, p.text));
            let tri = vec![pos2(x - 6.0, ruler.top()), pos2(x + 6.0, ruler.top()), pos2(x, ruler.top() + 8.0)];
            pt.add(egui::Shape::convex_polygon(tri, p.text, Stroke::NONE));
        }
    }

    // ====================================================================== small ui helpers

    fn fmt_t(t: f64) -> String {
        let t = t.max(0.0);
        let m = (t / 60.0).floor() as u32;
        let s = t - m as f64 * 60.0;
        format!("{m}:{s:05.2}")
    }

    fn frame_box(p: &egui::Painter, r: Rect) {
        card(p, r);
    }

    fn section_title(ui: &mut Ui, t: &str) {
        ui.label(egui::RichText::new(t).font(semibold(18.0)).color(pal().text));
    }

    fn hint(ui: &mut Ui, t: &str) {
        ui.label(egui::RichText::new(t).font(body(12.5)).color(pal().muted));
    }

    fn stat_line(ui: &mut Ui, k: &str, v: &str, c: Color32) {
        let p = pal();
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
        ui.painter().text(r.left_center(), Align2::LEFT_CENTER, k, body(12.5), p.muted);
        ui.painter().text(r.right_center(), Align2::RIGHT_CENTER, v, mono_bold(12.5), c);
        hline(ui, r.bottom(), r.left(), r.right());
    }

    fn play_button(ui: &mut Ui, playing: bool) -> egui::Response {
        let p = pal();
        let (r, resp) = ui.allocate_exact_size(vec2(34.0, 28.0), Sense::click());
        let pt = ui.painter();
        pt.rect_filled(r, CornerRadius::same(4), if resp.hovered() { p.accent } else { p.panel2 });
        pt.rect_stroke(r, CornerRadius::same(4), Stroke::new(1.0, p.line2), StrokeKind::Inside);
        let c = r.center();
        let col = if resp.hovered() { p.ink } else { p.text };
        if playing {
            for dx in [-4.0, 1.5] {
                pt.rect_filled(Rect::from_min_size(c + vec2(dx, -6.0), vec2(3.0, 12.0)), CornerRadius::ZERO, col);
            }
        } else {
            pt.add(egui::Shape::convex_polygon(vec![c + vec2(-4.0, -7.0), c + vec2(7.0, 0.0), c + vec2(-4.0, 7.0)], col, Stroke::NONE));
        }
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    fn pill(ui: &mut Ui, label: &str, on: bool, w: f32) -> egui::Response {
        let p = pal();
        let (r, resp) = ui.allocate_exact_size(vec2(w, 32.0), Sense::click());
        let pt = ui.painter();
        pt.rect_filled(r, CornerRadius::same(8), if on { p.text } else if resp.hovered() { p.panel3 } else { p.panel2 });
        pt.rect_stroke(r, CornerRadius::same(8), Stroke::new(1.0, if on { p.text } else { p.line2 }), StrokeKind::Inside);
        pt.text(r.center(), Align2::CENTER_CENTER, label, semibold(12.5), if on { p.bg } else { p.text });
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    fn button_enabled(ui: &mut Ui, label: &str, enabled: bool) -> egui::Response {
        if enabled {
            button(ui, label)
        } else {
            let p = pal();
            let font = semibold(13.0);
            let w = ui.painter().layout_no_wrap(label.to_string(), font.clone(), p.dim).size().x + 28.0;
            let (r, resp) = ui.allocate_exact_size(vec2(w, 34.0), Sense::hover());
            ui.painter().rect_stroke(r, CornerRadius::same(4), Stroke::new(1.0, p.line), StrokeKind::Inside);
            ui.painter().text(r.center(), Align2::CENTER_CENTER, label, font, p.dim);
            resp
        }
    }

    /// How many recorded frames go into each blended frame, like a camera shutter: Light keeps the
    /// shutter open for half of each output frame (subtle blur, stays sharp), Strong for the whole
    /// frame (cinematic). The old settings averaged a full frame (Light) and two full frames (Strong),
    /// which smeared motion into ghosts.
    pub fn blend_frames(blend: Blend, src_fps: f64, out_fps: f64) -> u32 {
        let ratio = (src_fps / out_fps.max(1.0)).round().max(1.0) as u32;
        match blend {
            Blend::Off => 1,
            // same frame rate out as in: a soft trail, every frame kept (smooth as the recording)
            Blend::Light if ratio == 1 => 2,
            Blend::Strong if ratio == 1 => 4,
            Blend::Light => ratio.div_ceil(2).max(2),
            Blend::Strong => ratio.max(3),
        }
    }

    /// Output frame rate for blending. 0 = keep the recording's rate. Otherwise the closest rate that
    /// divides the recording evenly (144 -> 72, not 60), so frames drop in an even pattern instead of
    /// the uneven cadence that made blended clips judder.
    pub fn blend_out_fps(src_fps: f64, setting: u32) -> f64 {
        let src = src_fps.max(1.0);
        if setting == 0 || setting as f64 >= src - 0.5 {
            return src;
        }
        src / (src / setting as f64).round().max(1.0)
    }

    /// Post-process a freshly saved clip with frame blending, replacing it in place. Runs in the background.
    pub fn blend_in_place(path: PathBuf) {
        std::thread::spawn(move || {
            let Some(pr) = probe(&path) else { return };
            let s = core().settings();
            let Some(ff) = sys::ffmpeg_path() else { return };
            let out_fps = blend_out_fps(pr.fps, s.blend_fps);
            let frames = blend_frames(s.blend, pr.fps, out_fps);
            let tmp = path.with_extension("blend.mp4");
            let enc = s.encoder_pref.codec().map(String::from).or(s.encoder.clone()).unwrap_or_else(|| "libx264".into());
            let kbps = s.video_kbps(pr.width, pr.height);
            let gop = (out_fps.round() as u32 * 2).to_string();
            let mut cmd = sys::quiet(&ff);
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(sys::CREATE_NO_WINDOW | sys::BELOW_NORMAL);
            }
            cmd.args(["-hide_banner", "-loglevel", "error", "-hwaccel", "auto", "-i"]).arg(&path);
            cmd.args(["-map", "0:v:0", "-map", "0:a?", "-vf", &format!("tmix=frames={frames},fps={out_fps},format=yuv420p")]);
            cmd.args(crate::engine::encoder_args(&enc, &format!("{kbps}k"), &format!("{}k", kbps * 3 / 2), &format!("{}k", kbps * 2), &gop, false, s.quality, out_fps.round() as u32));
            cmd.args(["-c:a", "copy", "-map_metadata", "0", "-movflags", "+faststart", "-y"]).arg(&tmp);
            cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            if let Ok(c) = cmd.spawn() {
                sys::adopt_child(&c);
                let mut c = c;
                if c.wait().map(|s| s.success()).unwrap_or(false) && std::fs::rename(&tmp, &path).is_ok() {
                    core().library_dirty.store(true, Ordering::SeqCst);
                    core().repaint();
                } else {
                    let _ = std::fs::remove_file(&tmp);
                }
            }
        });
    }
}

// ================================================================================================
// app
// ================================================================================================

mod app {
    //! Shared app state used by the UI, tray, and hotkey thread.

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Instant;

    use crate::engine::{Engine, Saved};
    use crate::settings::{Settings, ShotMode};
    use crate::{hotkeys, shots, sys};

    #[derive(Clone, Copy, PartialEq)]
    pub enum ToastKind {
        Clip,
        Shot,
        Error,
    }

    /// The on-screen saving badge.
    #[derive(Clone)]
    pub enum HudPhase {
        Saving { secs: u32 },
        Done { title: String, detail: String },
        Failed { detail: String },
    }

    #[derive(Clone)]
    pub struct Hud {
        pub phase: HudPhase,
        pub since: Instant,
    }

    #[derive(Clone)]
    pub struct Toast {
        pub kind: ToastKind,
        pub title: String,
        pub detail: String,
        pub path: Option<PathBuf>,
        pub born: Instant,
    }

    pub struct Core {
        pub settings: Mutex<Settings>,
        pub engine: Arc<Engine>,
        pub ctx: OnceLock<eframe::egui::Context>,
        pub toasts: Mutex<Vec<Toast>>,
        pub hud: Mutex<Option<Hud>>,
        pub show_request: AtomicBool,
        pub quit_request: AtomicBool,
        pub library_dirty: AtomicBool,
        pub hotkey_error: Mutex<Option<String>>,
        pub shot_busy: AtomicBool,
        pub open_in_editor: Mutex<Option<PathBuf>>,
        /// physical screen position of a tray right-click, waiting for the UI to open the menu
        pub tray_menu: Mutex<Option<(f32, f32)>>,
        /// page the UI should switch to when shown (from the tray menu)
        pub goto: Mutex<Option<u8>>,
    }

    static CORE: OnceLock<Arc<Core>> = OnceLock::new();

    pub fn init(settings: Settings) -> Arc<Core> {
        let engine = Engine::new(settings.clone());
        let core = Arc::new(Core {
            settings: Mutex::new(settings),
            engine,
            ctx: OnceLock::new(),
            toasts: Mutex::new(Vec::new()),
            hud: Mutex::new(None),
            show_request: AtomicBool::new(false),
            quit_request: AtomicBool::new(false),
            library_dirty: AtomicBool::new(true),
            hotkey_error: Mutex::new(None),
            shot_busy: AtomicBool::new(false),
            open_in_editor: Mutex::new(None),
            tray_menu: Mutex::new(None),
            goto: Mutex::new(None),
        });
        let _ = CORE.set(core.clone());
        core
    }

    pub fn core() -> &'static Arc<Core> {
        CORE.get().expect("core not initialised")
    }

    /// Called from the engine thread once the encoder has been auto-detected.
    pub fn on_encoder_detected(enc: &str) {
        if let Some(c) = CORE.get() {
            let mut s = c.settings.lock().unwrap();
            s.encoder = Some(enc.to_string());
            s.save();
            c.repaint();
        }
    }

    impl Core {
        pub fn repaint(&self) {
            if let Some(ctx) = self.ctx.get() {
                ctx.request_repaint();
            }
        }

        pub fn settings(&self) -> Settings {
            self.settings.lock().unwrap().clone()
        }

        pub fn request_show(&self) {
            self.show_request.store(true, Ordering::SeqCst);
            self.repaint();
        }

        pub fn request_quit(&self) {
            self.quit_request.store(true, Ordering::SeqCst);
            self.repaint();
        }

        pub fn toast(&self, kind: ToastKind, title: impl Into<String>, detail: impl Into<String>, path: Option<PathBuf>) {
            let mut t = self.toasts.lock().unwrap();
            t.push(Toast { kind, title: title.into(), detail: detail.into(), path, born: Instant::now() });
            if t.len() > 4 {
                t.remove(0);
            }
            drop(t);
            self.repaint();
        }

        pub fn apply(&self, new: Settings) {
            let old = {
                let mut s = self.settings.lock().unwrap();
                let old = s.clone();
                *s = new.clone();
                old
            };
            if old == new {
                return;
            }
            new.save();
            self.engine.update_settings(&new);
            if old.clip_key != new.clip_key || old.shot_key != new.shot_key || old.snip_override != new.snip_override {
                hotkeys::reload();
            }
            if old.start_with_windows != new.start_with_windows {
                sys::set_start_with_windows(new.start_with_windows);
            }
            if old.clips_dir != new.clips_dir || old.shots_dir != new.shots_dir {
                self.library_dirty.store(true, Ordering::SeqCst);
            }
            crate::tray::refresh();
        }

        pub fn save_clip(&'static self) {
            if self.engine.saving.swap(true, Ordering::SeqCst) {
                return;
            }
            // Grab the game's window title now, before anything can steal focus.
            let hint = crate::procs::game_name();
            // Feedback right away: the clip's content is fixed the moment F8 is pressed, and saving a
            // big (or stitched) clip can take a few seconds.
            {
                let s = self.settings();
                if s.sound {
                    sys::play(sys::Sound::Clip);
                }
                if s.save_hud {
                    let secs = self.engine.stats().held_secs.min(s.buffer_secs as f32).round() as u32;
                    *self.hud.lock().unwrap() = Some(Hud { phase: HudPhase::Saving { secs }, since: Instant::now() });
                    crate::badge::ensure_running();
                }
                self.repaint();
            }
            std::thread::spawn(move || {
                let s = self.settings();
                let r = self.engine.save_clip(&s.clips_dir, hint);
                self.engine.saving.store(false, Ordering::SeqCst);
                match r {
                    Saved::Ok { path, secs, bytes } => {
                        if s.save_hud {
                            *self.hud.lock().unwrap() = Some(Hud {
                                phase: HudPhase::Done { title: "Clip saved".into(), detail: format!("{:.0} s · {}", secs, sys::human_bytes(bytes)) },
                                since: Instant::now(),
                            });
                        }
                        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        self.toast(
                            ToastKind::Clip,
                            "Clip saved",
                            format!("{:.0} s · {} · {}", secs, sys::human_bytes(bytes), name),
                            Some(path.clone()),
                        );
                        self.library_dirty.store(true, Ordering::SeqCst);
                        if s.blend_on_save && s.blend != crate::settings::Blend::Off {
                            crate::editor::blend_in_place(path);
                        }
                    }
                    Saved::Err(e) => {
                        if s.sound {
                            sys::play(sys::Sound::Error);
                        }
                        if s.save_hud {
                            *self.hud.lock().unwrap() = Some(Hud { phase: HudPhase::Failed { detail: e.clone() }, since: Instant::now() });
                        }
                        self.toast(ToastKind::Error, "Couldn't save clip", e, None);
                    }
                }
            });
        }

        pub fn screenshot(&'static self) {
            if self.shot_busy.swap(true, Ordering::SeqCst) {
                return;
            }
            let rect = {
                let s = self.settings.lock().unwrap();
                if s.shot_mode == ShotMode::Window { sys::foreground_rect() } else { None }
            };
            let hint = sys::foreground_title();
            std::thread::spawn(move || {
                let s = self.settings();
                let r = shots::take(&s, rect, hint);
                self.shot_busy.store(false, Ordering::SeqCst);
                match r {
                    Ok(path) => {
                        if s.sound {
                            sys::play(sys::Sound::Shot);
                        }
                        let what = if s.copy_shot { "Saved + copied to clipboard" } else { "Saved as PNG" };
                        if s.shot_editor {
                            crate::snip::open_file(path.clone());
                        }
                        self.toast(ToastKind::Shot, "Screenshot", what, Some(path));
                        self.library_dirty.store(true, Ordering::SeqCst);
                    }
                    Err(e) => self.toast(ToastKind::Error, "Screenshot failed", e, None),
                }
            });
        }

        pub fn toggle_pause(&self) {
            let mut s = self.settings();
            s.paused = !s.paused;
            self.apply(s);
        }
    }
}

// ================================================================================================
// theme
// ================================================================================================

mod theme {
    //! Look & feel: graphite palettes with one accent each, a hand-made 5×7 pixel font, pixel-art icons,
    //! and the widget kit. Clean, rounded cards; pixels for the details that give it character.

    use std::sync::Arc;
    use std::sync::atomic::{AtomicU8, Ordering};

    use eframe::egui::{
        self, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Pos2, Rect, Response, Sense,
        Stroke, StrokeKind, Ui, Vec2, pos2, text::LayoutJob, vec2,
    };

    use crate::settings::ThemeId;

    #[derive(Clone, Copy)]
    pub struct Pal {
        pub bg: Color32,
        pub bg2: Color32,
        pub panel: Color32,
        pub panel2: Color32,
        pub panel3: Color32,
        pub line: Color32,
        pub line2: Color32,
        pub text: Color32,
        pub muted: Color32,
        pub dim: Color32,
        pub accent: Color32,
        pub accent2: Color32,
        pub warn: Color32,
        pub good: Color32,
        pub ink: Color32,
        pub glow: [Color32; 3],
    }

    const fn rgb(h: u32) -> Color32 {
        Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8)
    }

    static THEME: AtomicU8 = AtomicU8::new(0);

    pub fn set_theme(t: ThemeId) {
        THEME.store(t as u8, Ordering::Relaxed);
    }

    /// Track colors for audio sources (green, blue, yellow, coral, violet, cyan).
    pub const TRACK_COLORS: [Color32; 6] =
        [rgb(0x22c55e), rgb(0x3b82f6), rgb(0xfacc15), rgb(0xff7a59), rgb(0xa78bfa), rgb(0x22d3ee)];

    pub fn palette(t: ThemeId) -> Pal {
        let base = |bg: u32, bg2: u32, panel: u32, panel2: u32, panel3: u32, line: u32, line2: u32| {
            (rgb(bg), rgb(bg2), rgb(panel), rgb(panel2), rgb(panel3), rgb(line), rgb(line2))
        };
        let (bg, bg2, panel, panel2, panel3, line, line2, accent, accent2, ink, glow) = match t {
            ThemeId::Ferro => {
                let b = base(0x0d0c0b, 0x121110, 0x181615, 0x1f1d1b, 0x2a2725, 0x272422, 0x36322f);
                (b.0, b.1, b.2, b.3, b.4, b.5, b.6, rgb(0xff5b14), rgb(0xffae42), rgb(0x170802), [rgb(0x9c2408), rgb(0xff5b14), rgb(0xffe2b0)])
            }
            ThemeId::Sable => {
                let b = base(0x0e0e0e, 0x121212, 0x181818, 0x1e1e1e, 0x292929, 0x262626, 0x343434);
                (b.0, b.1, b.2, b.3, b.4, b.5, b.6, rgb(0xf4f4f4), rgb(0xd9f99d), rgb(0x0e0e0e), [rgb(0x4b38d8), rgb(0xb05ad0), rgb(0xffffff)])
            }
            ThemeId::Cobalt => {
                let b = base(0x0c0d10, 0x101216, 0x16181d, 0x1c1f25, 0x262a32, 0x23272e, 0x313641);
                (b.0, b.1, b.2, b.3, b.4, b.5, b.6, rgb(0x3b82f6), rgb(0x7dd3fc), rgb(0x020814), [rgb(0x1a2f8a), rgb(0x3b82f6), rgb(0xd2f1ff)])
            }
            ThemeId::Verdant => {
                let b = base(0x0d0e0d, 0x111311, 0x171917, 0x1d201d, 0x272b27, 0x242824, 0x323732);
                (b.0, b.1, b.2, b.3, b.4, b.5, b.6, rgb(0x22c55e), rgb(0xbef264), rgb(0x04130a), [rgb(0x15603a), rgb(0x22c55e), rgb(0xecfccb)])
            }
        };
        Pal {
            bg,
            bg2,
            panel,
            panel2,
            panel3,
            line,
            line2,
            text: rgb(0xf3f3f2),
            muted: rgb(0xa1a19d),
            dim: rgb(0x63635f),
            accent,
            accent2,
            warn: rgb(0xef4444),
            good: rgb(0x22c55e),
            ink,
            glow,
        }
    }

    pub fn pal() -> Pal {
        palette(ThemeId::ALL[THEME.load(Ordering::Relaxed) as usize % 4])
    }

    pub fn alpha(c: Color32, a: f32) -> Color32 {
        Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (a.clamp(0.0, 1.0) * 255.0) as u8)
    }

    pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
        let t = t.clamp(0.0, 1.0);
        let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
        Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
    }

    /// Readable ink on top of `c` (dark on bright accents, white on dark ones).
    pub fn on(c: Color32) -> Color32 {
        let l = 0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32;
        if l > 150.0 { rgb(0x0b0b0b) } else { Color32::WHITE }
    }

    // ------------------------------------------------------------------ fonts

    pub fn display(size: f32) -> FontId {
        FontId::new(size, FontFamily::Name("display".into()))
    }
    pub fn semibold(size: f32) -> FontId {
        FontId::new(size, FontFamily::Name("semibold".into()))
    }
    pub fn body(size: f32) -> FontId {
        FontId::proportional(size)
    }
    pub fn mono(size: f32) -> FontId {
        FontId::monospace(size)
    }
    pub fn mono_bold(size: f32) -> FontId {
        FontId::new(size, FontFamily::Name("monob".into()))
    }

    pub fn install_fonts(ctx: &egui::Context) {
        let mut fonts = FontDefinitions::default();
        let dp = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
        let dm = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
        let mut load = |key: &str, files: &[&str]| -> bool {
            for f in files {
                if let Ok(bytes) = std::fs::read(format!("C:\\Windows\\Fonts\\{f}")) {
                    fonts.font_data.insert(key.to_string(), Arc::new(FontData::from_owned(bytes)));
                    return true;
                }
            }
            false
        };
        let ok = [
            ("ui", load("ui", &["segoeui.ttf"])),
            ("ui-sb", load("ui-sb", &["seguisb.ttf", "segoeuib.ttf"])),
            ("display", load("display", &["bahnschrift.ttf", "seguisb.ttf"])),
            ("mono", load("mono", &["consola.ttf", "CascadiaMono.ttf"])),
            ("mono-b", load("mono-b", &["consolab.ttf", "consola.ttf"])),
        ];
        let has = |k: &str| ok.iter().any(|(n, o)| *n == k && *o);
        let fam = |first: &[&str], rest: &Vec<String>| -> Vec<String> {
            let mut v: Vec<String> = first.iter().filter(|k| has(k)).map(|s| s.to_string()).collect();
            v.extend(rest.iter().cloned());
            v
        };
        fonts.families.insert(FontFamily::Proportional, fam(&["ui"], &dp));
        fonts.families.insert(FontFamily::Monospace, fam(&["mono"], &dm));
        fonts.families.insert(FontFamily::Name("semibold".into()), fam(&["ui-sb", "ui"], &dp));
        fonts.families.insert(FontFamily::Name("display".into()), fam(&["display", "ui-sb", "ui"], &dp));
        fonts.families.insert(FontFamily::Name("monob".into()), fam(&["mono-b", "mono"], &dm));
        ctx.set_fonts(fonts);
    }

    pub fn apply_visuals(ctx: &egui::Context) {
        let p = pal();
        let mut style = (*ctx.global_style()).clone();
        let v = &mut style.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = p.bg;
        v.window_fill = p.panel2;
        v.extreme_bg_color = p.bg2;
        v.faint_bg_color = p.panel;
        v.override_text_color = Some(p.text);
        v.selection.bg_fill = alpha(p.accent, 0.35);
        v.selection.stroke = Stroke::new(1.0, p.accent);
        v.hyperlink_color = p.accent2;
        v.window_stroke = Stroke::new(1.0, p.line2);
        v.window_corner_radius = CornerRadius::same(10);
        v.menu_corner_radius = CornerRadius::same(10);
        v.text_cursor.stroke = Stroke::new(1.5, p.accent);
        for (w, fill, stroke) in [
            (&mut v.widgets.noninteractive, p.panel, p.line),
            (&mut v.widgets.inactive, p.panel2, p.line2),
            (&mut v.widgets.hovered, p.panel3, p.line2),
            (&mut v.widgets.active, p.panel3, p.accent),
            (&mut v.widgets.open, p.panel3, p.line2),
        ] {
            w.bg_fill = fill;
            w.weak_bg_fill = fill;
            w.bg_stroke = Stroke::new(1.0, stroke);
            w.corner_radius = CornerRadius::same(8);
            w.fg_stroke = Stroke::new(1.0, p.text);
        }
        style.spacing.item_spacing = vec2(8.0, 8.0);
        style.spacing.button_padding = vec2(12.0, 6.0);
        style.spacing.scroll.bar_width = 5.0;
        style.spacing.scroll.floating = true;
        style.interaction.selectable_labels = false;
        ctx.set_global_style(style);
    }

    // ------------------------------------------------------------------ pixel font



    fn snap(v: f32, ppp: f32) -> f32 {
        (v * ppp).round() / ppp
    }

    /// Cell size in points snapped to whole physical pixels (so the pixels stay crisp).
    pub fn pixel_cell(ctx: &egui::Context, cell: f32) -> f32 {
        let ppp = ctx.pixels_per_point();
        ((cell * ppp).round().max(1.0)) / ppp
    }




    /// Big number with a dimmed tail, like "$11.55M" in the reference: `head` bright, `tail` muted.
    /// `pos` is the left of the baseline-ish bottom edge.
    pub fn big_value(p: &egui::Painter, left_bottom: Pos2, head: &str, tail: &str, size: f32) -> Rect {
        let c = pal();
        let g1 = p.layout_no_wrap(head.to_string(), semibold(size), c.text);
        let g2 = p.layout_no_wrap(tail.to_string(), semibold(size), alpha(c.text, 0.38));
        let h = g1.size().y;
        let top = left_bottom.y - h;
        let w1 = g1.size().x;
        let w2 = g2.size().x;
        p.galley(pos2(left_bottom.x, top), g1, c.text);
        p.galley(pos2(left_bottom.x + w1, top), g2, c.text);
        Rect::from_min_size(pos2(left_bottom.x, top), vec2(w1 + w2, h))
    }

    // ------------------------------------------------------------------ pixel icons

    #[derive(Clone, Copy, PartialEq)]
    pub enum Icon {
        Live,
        Library,
        Editor,
        Settings,
        Clip,
        Cpu,
        Memory,
        Wave,
        Stream,
        Gamepad,
        Audio,
        Key,
        Folder,
        Palette,
        Chip,
        Arrow,
        Search,
        Calendar,
        Power,
        Plus,
        Check,
        Sidebar,
        Chevron,
        Cursor,
        Camera,
        Pen,
        Highlighter,
        LineTool,
        RectTool,
        EllipseTool,
        TextTool,
        Pixelate,
        Crop,
        Eraser,
        Undo,
        Redo,
        Copy,
        Save,
    }

    fn icon_rows(i: Icon) -> [&'static str; 8] {
        match i {
            Icon::Live => ["..####..", ".#....#.", "#..##..#", "#.####.#", "#.####.#", "#..##..#", ".#....#.", "..####.."],
            Icon::Library => ["###..###", "#.#..#.#", "###..###", "........", "........", "###..###", "#.#..#.#", "###..###"],
            Icon::Editor => ["##....##", "##...##.", "..#.##..", "...##...", "..#.##..", "##...##.", "##....##", "........"],
            Icon::Settings => ["...##...", ".#.##.#.", "..####..", "###..###", "###..###", "..####..", ".#.##.#.", "...##..."],
            Icon::Clip => ["########", "#.#..#.#", "########", "#..#...#", "#..##..#", "########", "#.#..#.#", "########"],
            Icon::Cpu | Icon::Chip => [".#.##.#.", "########", ".#....#.", "##.##.##", "##.##.##", ".#....#.", "########", ".#.##.#."],
            Icon::Memory => ["........", "########", "#.#..#.#", "#.#..#.#", "########", ".#.#.#.#", "........", "........"],
            Icon::Wave | Icon::Audio => ["....#...", "..#.#...", "..#.#.#.", "#.#.#.#.", "#.#.#.#.", "..#.#.#.", "..#.#...", "....#..."],
            Icon::Stream => ["#.......", "#.....##", "#....#..", "#.#.#...", "#..#....", "#.......", "########", "........"],
            Icon::Gamepad => ["........", ".######.", "#.#....#", "###..#.#", "#.#.#..#", "#......#", ".##..##.", "........"],
            Icon::Key => ["########", "#.#.#.##", "########", "##.##.##", "########", "#......#", "########", "........"],
            Icon::Folder => ["###.....", "#..#####", "#......#", "#......#", "#......#", "#......#", "########", "........"],
            Icon::Palette => ["..####..", ".#....#.", "#.#..#.#", "#......#", "#.#...##", "#....#..", ".#..#...", "..##...."],
            Icon::Arrow => ["..######", ".....###", "....#.##", "...#..##", "..#....#", ".#......", "#.......", "........"],
            Icon::Search => [".####...", "#....#..", "#....#..", "#....#..", "#....#..", ".####...", ".....##.", "......##"],
            Icon::Calendar => [".#...#..", "#######.", "#.....#.", "#######.", "#.#.#.#.", "#.....#.", "#######.", "........"],
            Icon::Power => ["...#....", ".#.#.#..", "#..#..#.", "#.....#.", "#.....#.", ".#...#..", "..###...", "........"],
            Icon::Plus => ["........", "...#....", "...#....", ".#####..", "...#....", "...#....", "........", "........"],
            Icon::Check => ["........", "......#.", ".....##.", "#...##..", "##.##...", ".###....", "..#.....", "........"],
            Icon::Sidebar => ["########", "#..#...#", "#..#...#", "#..#...#", "#..#...#", "#..#...#", "########", "........"],
            Icon::Chevron => ["........", "........", "#.....#.", ".#...#..", "..#.#...", "...#....", "........", "........"],
            Icon::Cursor => ["#.......", "##......", "#.#.....", "#..#....", "#...#...", "#..###..", "##.#....", "#...#..."],
            Icon::Camera => ["..###...", "########", "#..##..#", "#.#..#.#", "#.#..#.#", "#..##..#", "########", "........"],
            Icon::Pen => [".....##.", "....#..#", "...#..#.", "..#..#..", ".#..#...", "#..#....", "##......", "#......."],
            Icon::Highlighter => ["....###.", "...#####", "..#####.", ".#####..", "#####...", ".###....", "..#.....", "########"],
            Icon::LineTool => ["......##", ".....##.", "....##..", "...##...", "..##....", ".##.....", "##......", "#......."],
            Icon::RectTool => ["########", "#......#", "#......#", "#......#", "#......#", "#......#", "#......#", "########"],
            Icon::EllipseTool => ["..####..", ".#....#.", "#......#", "#......#", "#......#", "#......#", ".#....#.", "..####.."],
            Icon::TextTool => ["########", "#..##..#", "...##...", "...##...", "...##...", "...##...", "...##...", "..####.."],
            Icon::Pixelate => ["##..##..", "##..##..", "..##..##", "..##..##", "##..##..", "##..##..", "..##..##", "..##..##"],
            Icon::Crop => [".#......", ".#######", ".#....#.", ".#....#.", ".#....#.", ".#....#.", "#######.", "......#."],
            Icon::Eraser => ["....###.", "...#####", "..#####.", ".#.###..", "#...#...", "#..#....", "####....", "........"],
            Icon::Undo => ["..#.....", ".##.....", "#######.", ".##....#", "..#....#", ".......#", "..#####.", "........"],
            Icon::Redo => [".....#..", ".....##.", ".#######", "#....##.", "#....#..", "#.......", ".#####..", "........"],
            Icon::Copy => ["#####...", "#...#...", "#.######", "#.#....#", "###....#", "..#....#", "..######", "........"],
            Icon::Save => ["#######.", "#.###.##", "#.###..#", "#......#", "#.####.#", "#.####.#", "########", "........"],
        }
    }

    /// Draw a pixel icon centered at `c`. `k` = 1.0 gives a 16 pt icon.
    pub fn draw_icon(p: &egui::Painter, icon: Icon, c: Pos2, col: Color32, k: f32) {
        let cell = pixel_cell(p.ctx(), (2.0 * k).round().max(2.0));
        let ppp = p.ctx().pixels_per_point();
        let origin = pos2(snap(c.x - 4.0 * cell, ppp), snap(c.y - 4.0 * cell, ppp));
        for (row, line) in icon_rows(icon).iter().enumerate() {
            let b = line.as_bytes();
            let mut x = 0;
            while x < b.len() {
                if b[x] == b'#' {
                    let s = x;
                    while x < b.len() && b[x] == b'#' {
                        x += 1;
                    }
                    let r = Rect::from_min_size(origin + vec2(s as f32 * cell, row as f32 * cell), vec2((x - s) as f32 * cell, cell));
                    p.rect_filled(r, CornerRadius::ZERO, col);
                } else {
                    x += 1;
                }
            }
        }
    }

    /// Rounded square holding a pixel icon (card header style).
    pub fn icon_box(p: &egui::Painter, r: Rect, icon: Icon, color: Color32) {
        let c = pal();
        p.rect_filled(r, CornerRadius::same(8), c.panel2);
        p.rect_stroke(r, CornerRadius::same(8), Stroke::new(1.0, c.line2), StrokeKind::Inside);
        draw_icon(p, icon, r.center(), color, 0.75);
    }

    // ------------------------------------------------------------------ text helpers


    pub fn truncate(ui: &Ui, text: &str, font: &FontId, max_w: f32) -> String {
        let w = |s: &str| ui.painter().layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE).size().x;
        if w(text) <= max_w {
            return text.to_string();
        }
        let chars: Vec<char> = text.chars().collect();
        let (mut lo, mut hi) = (0usize, chars.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            let s: String = chars[..mid].iter().collect::<String>() + "…";
            if w(&s) <= max_w { lo = mid } else { hi = mid - 1 }
        }
        chars[..lo].iter().collect::<String>() + "…"
    }

    // ------------------------------------------------------------------ surfaces

    /// The basic surface: rounded card with a hairline border, slightly see-through so the backdrop glows show.
    pub fn card(p: &egui::Painter, r: Rect) {
        let c = pal();
        p.rect_filled(r, CornerRadius::same(14), alpha(c.panel, 0.88));
        p.rect_stroke(r, CornerRadius::same(14), Stroke::new(1.0, c.line), StrokeKind::Inside);
    }


    pub fn hline(ui: &Ui, y: f32, x0: f32, x1: f32) {
        ui.painter().line_segment([pos2(x0, y), pos2(x1, y)], Stroke::new(1.0, pal().line));
    }

    pub fn dashed(p: &egui::Painter, a: Pos2, b: Pos2, dash: f32, gap: f32, stroke: Stroke) {
        let d = b - a;
        let len = d.length();
        if len < 0.5 {
            return;
        }
        let dir = d / len;
        let mut t = 0.0;
        while t < len {
            let e = (t + dash).min(len);
            p.line_segment([a + dir * t, a + dir * e], stroke);
            t += dash + gap;
        }
    }

    pub fn glow_dot(p: &egui::Painter, c: Pos2, r: f32, color: Color32, pulse: f32) {
        p.circle_filled(c, r + 4.0 * pulse, alpha(color, 0.14 * pulse));
        p.circle_filled(c, r, color);
    }

    /// Tiny pixel "chip" like the reference's +12.20% badges: tinted background, pixel text.
    pub fn pixel_chip(p: &egui::Painter, left_center: Pos2, text: &str, color: Color32) -> Rect {
        let cell = pixel_cell(p.ctx(), 2.0);
        let _ = cell;
        let g = p.layout_no_wrap(text.to_string(), semibold(11.5), color);
        let w = g.size().x;
        let r = Rect::from_min_size(left_center - vec2(0.0, 11.0), vec2(w + 16.0, 22.0));
        p.rect_filled(r, CornerRadius::same(5), alpha(color, 0.13));
        p.rect_stroke(r, CornerRadius::same(5), Stroke::new(1.0, alpha(color, 0.28)), StrokeKind::Inside);
        p.galley(pos2(r.left() + 8.0, r.center().y - g.size().y / 2.0), g, color);
        r
    }

    /// Outlined pill with body text (table "stage" tags).
    pub fn tag_at(p: &egui::Painter, left_center: Pos2, text: &str, fg: Color32) -> Rect {
        let c = pal();
        let g = p.layout_no_wrap(text.to_string(), body(12.0), fg);
        let r = Rect::from_min_size(left_center - vec2(0.0, (g.size().y + 8.0) / 2.0), g.size() + vec2(18.0, 8.0));
        p.rect_filled(r, CornerRadius::same(6), c.panel2);
        p.rect_stroke(r, CornerRadius::same(6), Stroke::new(1.0, c.line2), StrokeKind::Inside);
        p.galley(r.min + vec2(9.0, 4.0), g, fg);
        r
    }

    pub fn chip_at(p: &egui::Painter, anchor: Pos2, text: &str, fg: Color32, bg: Color32) -> Rect {
        let cell = pixel_cell(p.ctx(), 2.0);
        let _ = cell;
        let g = p.layout_no_wrap(text.to_string(), semibold(11.5), fg);
        let size = g.size() + vec2(12.0, 6.0);
        let rect = Rect::from_min_size(anchor - size, size);
        p.rect_filled(rect, CornerRadius::same(5), bg);
        p.galley(rect.min + vec2(6.0, 3.0), g, fg);
        rect
    }

    /// Kept for the old look; draws nothing heavy.
    pub fn claws(p: &egui::Painter, r: Rect, color: Color32, strength: f32) {
        let clip = p.with_clip_rect(r);
        for (i, a) in [(0.0f32, 1.0f32), (12.0, 0.6), (24.0, 0.3)] {
            let x = r.right() - r.height() * 0.9 - 30.0 + i;
            clip.line_segment(
                [pos2(x, r.bottom() + 6.0), pos2(x + r.height() * 0.9, r.top() - 6.0)],
                Stroke::new(1.4, alpha(color, 0.35 * a * strength)),
            );
        }
    }

    /// Background art: slowly drifting soft glows (true radial gradients), a fine grid that fades out
    /// from the light, and a few twinkling pixels.
    pub fn backdrop(p: &egui::Painter, r: Rect, c: &Pal, t: f32) {
        let p = p.with_clip_rect(r);
        let light = r.right_top() + vec2(-220.0 + (t * 0.05).sin() * 60.0, 140.0 + (t * 0.04).cos() * 40.0);
        // glows
        radial(&p, light, r.width() * 0.55, c.accent, 0.11);
        radial(&p, r.left_bottom() + vec2(240.0 + (t * 0.035).cos() * 80.0, -120.0), r.width() * 0.42, c.glow[0], 0.09);
        radial(&p, r.center() + vec2(220.0 + (t * 0.03).sin() * 120.0, 160.0), r.width() * 0.28, c.accent2, 0.045);
        // grid, brightest near the light
        let step = 46.0;
        let reach = r.width() * 0.75;
        let fade = |q: Pos2| (1.0 - (q.distance(light) / reach)).clamp(0.0, 1.0).powf(1.6);
        let mut x = r.left() + (r.width() % step) / 2.0;
        while x < r.right() {
            let mut y = r.top();
            while y < r.bottom() {
                let a = fade(pos2(x, y + step / 2.0));
                if a > 0.01 {
                    p.line_segment([pos2(x, y), pos2(x, y + step)], Stroke::new(1.0, alpha(c.text, 0.07 * a)));
                }
                y += step;
            }
            x += step;
        }
        let mut y = r.top() + (r.height() % step) / 2.0;
        while y < r.bottom() {
            let mut x = r.left();
            while x < r.right() {
                let a = fade(pos2(x + step / 2.0, y));
                if a > 0.01 {
                    p.line_segment([pos2(x, y), pos2(x + step, y)], Stroke::new(1.0, alpha(c.text, 0.07 * a)));
                }
                x += step;
            }
            y += step;
        }
        // twinkling pixels on grid crossings near the light
        let ppp = p.ctx().pixels_per_point();
        let sz = (2.0 * ppp).round().max(2.0) / ppp;
        for i in 0..28u32 {
            let h = |k: u32| (((i.wrapping_mul(2_654_435_761) ^ k.wrapping_mul(40503)).wrapping_mul(2_246_822_519)) >> 8) as f32 / 16_777_216.0;
            let gx = r.left() + (r.width() % step) / 2.0 + (h(1) * (r.width() / step)).floor() * step;
            let gy = r.top() + (r.height() % step) / 2.0 + (h(2) * (r.height() / step)).floor() * step;
            let q = pos2(gx, gy);
            let a = fade(q) * ((t * (0.5 + h(3)) + h(4) * 6.28).sin().max(0.0)).powf(3.0);
            if a > 0.02 {
                p.rect_filled(Rect::from_center_size(q, vec2(sz, sz)), CornerRadius::ZERO, alpha(c.accent2, 0.7 * a));
            }
        }
    }

    /// Smooth radial gradient: `color` at `a` in the middle fading to nothing at `radius`.
    pub fn radial(p: &egui::Painter, center: Pos2, radius: f32, color: Color32, a: f32) {
        use egui::epaint::{Mesh, Vertex, WHITE_UV};
        let mut m = Mesh::default();
        let rings = [(0.0f32, 1.0f32), (0.3, 0.62), (0.6, 0.25), (1.0, 0.0)];
        let seg = 48u32;
        m.vertices.push(Vertex { pos: center, uv: WHITE_UV, color: alpha(color, a) });
        for (k, &(rr, f)) in rings.iter().enumerate().skip(1) {
            for j in 0..seg {
                let ang = j as f32 / seg as f32 * std::f32::consts::TAU;
                let pos = center + vec2(ang.cos(), ang.sin()) * radius * rr;
                m.vertices.push(Vertex { pos, uv: WHITE_UV, color: alpha(color, a * f) });
            }
            let ring = 1 + (k as u32 - 1) * seg;
            for j in 0..seg {
                let n = (j + 1) % seg;
                if k == 1 {
                    m.add_triangle(0, ring + j, ring + n);
                } else {
                    let inner = ring - seg;
                    m.add_triangle(inner + j, ring + j, ring + n);
                    m.add_triangle(inner + j, ring + n, inner + n);
                }
            }
        }
        p.add(egui::Shape::mesh(m));
    }

    /// The periodic-table tile from the website: 26 / Fe.
    pub fn element_tile(p: &egui::Painter, r: Rect, pulse: f32) {
        let c = pal();
        p.rect_filled(r.expand(3.0 * pulse), CornerRadius::same(10), alpha(c.accent, 0.08 * pulse));
        p.rect_filled(r, CornerRadius::same(9), mix(c.panel2, c.accent, 0.10));
        p.rect_stroke(r, CornerRadius::same(9), Stroke::new(1.0, alpha(c.accent, 0.5)), StrokeKind::Inside);
        let small = pixel_cell(p.ctx(), 1.0);
        let _ = small;
        p.text(r.min + vec2(5.0, 3.0), Align2::LEFT_TOP, "26", body(8.5), c.accent);
        let big = pixel_cell(p.ctx(), (r.height() / 12.0).max(2.0));
        let _ = big;
        p.text(r.center() + vec2(1.0, 2.0), Align2::CENTER_CENTER, "Fe", display(r.height() * 0.42), c.text);
    }

    /// Thin-bar sparkline.
    pub fn mini_bars(p: &egui::Painter, r: Rect, vals: &[f32], hot: Color32) {
        let c = pal();
        let n = vals.len().max(1);
        let bw = 2.0;
        let gap = (r.width() - bw * n as f32) / (n as f32 - 1.0).max(1.0);
        for (i, v) in vals.iter().enumerate() {
            let h = (v.clamp(0.0, 1.0) * r.height()).max(3.0);
            let x = r.left() + i as f32 * (bw + gap);
            let col = if i + 1 == n { hot } else { mix(c.line2, c.muted, i as f32 / n as f32) };
            p.rect_filled(Rect::from_min_max(pos2(x, r.bottom() - h), pos2(x + bw, r.bottom())), CornerRadius::same(1), col);
        }
    }

    // ------------------------------------------------------------------ widgets

    pub fn toggle(ui: &mut Ui, on: &mut bool) -> Response {
        let c = pal();
        let (rect, mut resp) = ui.allocate_exact_size(vec2(40.0, 22.0), Sense::click());
        if resp.clicked() {
            *on = !*on;
            resp.mark_changed();
        }
        let t = ui.ctx().animate_bool_with_time(resp.id, *on, 0.14);
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::same(11), mix(c.panel3, c.accent, t));
        p.rect_stroke(rect, CornerRadius::same(11), Stroke::new(1.0, mix(c.line2, c.accent, t)), StrokeKind::Inside);
        let x = egui::lerp(rect.left() + 11.0..=rect.right() - 11.0, t);
        p.circle_filled(pos2(x, rect.center().y), 7.5, mix(c.muted, self::on(c.accent), t));
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    /// Segmented pill like the reference's 1D/1W/1M/6M. Selected cell is a raised light chip.
    pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)], cell_w: f32) -> bool {
        let c = pal();
        let h = 32.0;
        let (rect, _) = ui.allocate_exact_size(vec2(cell_w * options.len() as f32 + 4.0, h), Sense::hover());
        let p = ui.painter().clone();
        p.rect_filled(rect, CornerRadius::same(9), c.bg2);
        p.rect_stroke(rect, CornerRadius::same(9), Stroke::new(1.0, c.line2), StrokeKind::Inside);
        let mut changed = false;
        let sel_idx = options.iter().position(|(v, _)| v == value).unwrap_or(0) as f32;
        let id = ui.scope_id().with(("seg", options.len(), options[0].1));
        let anim = ui.ctx().animate_value_with_time(id, sel_idx, 0.16);
        let pill = Rect::from_min_size(pos2(rect.left() + 2.0 + anim * cell_w, rect.top() + 2.0), vec2(cell_w, h - 4.0));
        p.rect_filled(pill, CornerRadius::same(7), c.text);
        for (i, (v, label)) in options.iter().enumerate() {
            let r = Rect::from_min_size(pos2(rect.left() + 2.0 + i as f32 * cell_w, rect.top()), vec2(cell_w, h));
            let resp = ui.interact(r, id.with(i), Sense::click());
            if resp.clicked() && v != value {
                *value = *v;
                changed = true;
            }
            let near = 1.0 - (anim - i as f32).abs().min(1.0);
            let col = if near > 0.5 { c.bg } else if resp.hovered() { c.text } else { c.muted };
            p.text(r.center(), Align2::CENTER_CENTER, *label, semibold(12.5), col);
            resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        }
        changed
    }

    /// Segmented pill with pixel-font labels (for short labels like 15S / 30S / 1M).
    pub fn pixel_segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)], cell_w: f32) -> bool {
        let c = pal();
        let h = 32.0;
        let (rect, _) = ui.allocate_exact_size(vec2(cell_w * options.len() as f32 + 4.0, h), Sense::hover());
        let p = ui.painter().clone();
        p.rect_filled(rect, CornerRadius::same(9), c.bg2);
        p.rect_stroke(rect, CornerRadius::same(9), Stroke::new(1.0, c.line2), StrokeKind::Inside);
        let id = ui.scope_id().with(("pseg", options.len(), options[0].1));
        let sel_idx = options.iter().position(|(v, _)| v == value).unwrap_or(0) as f32;
        let anim = ui.ctx().animate_value_with_time(id, sel_idx, 0.16);
        p.rect_filled(
            Rect::from_min_size(pos2(rect.left() + 2.0 + anim * cell_w, rect.top() + 2.0), vec2(cell_w, h - 4.0)),
            CornerRadius::same(7),
            c.text,
        );
        let mut changed = false;
        for (i, (v, label)) in options.iter().enumerate() {
            let r = Rect::from_min_size(pos2(rect.left() + 2.0 + i as f32 * cell_w, rect.top()), vec2(cell_w, h));
            if i > 0 {
                p.line_segment([r.left_top() + vec2(0.0, 8.0), r.left_bottom() - vec2(0.0, 8.0)], Stroke::new(1.0, c.line));
            }
            let resp = ui.interact(r, id.with(i), Sense::click());
            if resp.clicked() && v != value {
                *value = *v;
                changed = true;
            }
            let near = 1.0 - (anim - i as f32).abs().min(1.0);
            let col = if near > 0.5 { c.bg } else if resp.hovered() { c.text } else { c.muted };
            p.text(r.center(), Align2::CENTER_CENTER, *label, semibold(12.5), col);
            resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        }
        changed
    }

    pub fn keycaps(ui: &mut Ui, parts: &[String], listening: bool) -> Response {
        let c = pal();
        let plus_w = 14.0;
        let cell = pixel_cell(ui.ctx(), 2.0);
        let widths: Vec<f32> = parts
            .iter()
            .map(|t| (ui.painter().layout_no_wrap(t.clone(), semibold(12.5), c.text).size().x + 24.0).max(38.0))
            .collect();
        let _ = cell;
        let total: f32 = if listening { 150.0 } else { widths.iter().sum::<f32>() + plus_w * (parts.len() as f32 - 1.0) };
        let (rect, resp) = ui.allocate_exact_size(vec2(total, 34.0), Sense::click());
        let p = ui.painter();
        let time = ui.input(|i| i.time) as f32;
        let draw_cap = |r: Rect, label: &str, hovered: bool, glow: f32| {
            let base = Rect::from_min_max(r.min + vec2(0.0, 3.0), r.max);
            p.rect_filled(base, CornerRadius::same(7), Color32::from_rgb(4, 4, 4));
            let top = Rect::from_min_max(r.min, r.max - vec2(0.0, 3.0));
            p.rect_filled(top, CornerRadius::same(7), if hovered { c.panel3 } else { c.panel2 });
            let sc = if glow > 0.0 { alpha(c.accent, 0.4 + 0.6 * glow) } else { c.line2 };
            p.rect_stroke(top, CornerRadius::same(7), Stroke::new(1.0, sc), StrokeKind::Inside);
            p.text(top.center(), Align2::CENTER_CENTER, label, semibold(12.5), if glow > 0.0 { c.accent } else { c.text });
        };
        if listening {
            draw_cap(rect, "Press a key…", false, (time * 4.0).sin() * 0.5 + 0.5);
            ui.ctx().request_repaint();
        } else {
            let mut x = rect.left();
            for (i, (t, w)) in parts.iter().zip(&widths).enumerate() {
                if i > 0 {
                    p.text(pos2(x + plus_w / 2.0, rect.center().y - 1.0), Align2::CENTER_CENTER, "+", body(12.0), c.dim);
                    x += plus_w;
                }
                draw_cap(Rect::from_min_size(pos2(x, rect.top()), vec2(*w, 34.0)), t, resp.hovered(), 0.0);
                x += w;
            }
        }
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    pub fn button(ui: &mut Ui, label: &str) -> Response {
        let c = pal();
        let font = semibold(12.5);
        let w = ui.painter().layout_no_wrap(label.to_string(), font.clone(), c.text).size().x + 26.0;
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 32.0), Sense::click());
        let p = ui.painter();
        let hov = resp.hovered();
        p.rect_filled(rect, CornerRadius::same(8), if hov { c.panel3 } else { c.panel2 });
        p.rect_stroke(rect, CornerRadius::same(8), Stroke::new(1.0, c.line2), StrokeKind::Inside);
        p.text(rect.center(), Align2::CENTER_CENTER, label, font, c.text);
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    /// Light call-to-action with a pixel arrow.
    pub fn cta(ui: &mut Ui, label: &str) -> Response {
        let c = pal();
        let font = semibold(13.0);
        let tw = ui.painter().layout_no_wrap(label.to_string(), font.clone(), c.bg).size().x;
        let (rect, resp) = ui.allocate_exact_size(vec2(tw + 52.0, 38.0), Sense::click());
        let p = ui.painter();
        let hov = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
        let fill = mix(c.text, c.accent, hov);
        p.rect_filled(rect, CornerRadius::same(10), fill);
        let ink = on(fill);
        p.text(pos2(rect.left() + 16.0, rect.center().y), Align2::LEFT_CENTER, label, font, ink);
        draw_icon(p, Icon::Arrow, pos2(rect.right() - 18.0 + hov * 1.5, rect.center().y - hov * 1.5), ink, 0.6);
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    pub fn primary(ui: &mut Ui, label: &str, width: f32) -> Response {
        let c = pal();
        let (rect, resp) = ui.allocate_exact_size(vec2(width, 40.0), Sense::click());
        let p = ui.painter();
        let fill = if resp.hovered() { mix(c.accent, Color32::WHITE, 0.15) } else { c.accent };
        p.rect_filled(rect, CornerRadius::same(10), fill);
        p.text(rect.center(), Align2::CENTER_CENTER, label, semibold(13.5), on(fill));
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    pub fn link(ui: &mut Ui, label: &str, color: Color32) -> Response {
        let c = pal();
        let g = ui.painter().layout_no_wrap(label.to_string(), semibold(12.5), color);
        let (rect, resp) = ui.allocate_exact_size(g.size() + vec2(4.0, 4.0), Sense::click());
        let col = if resp.hovered() { c.text } else { color };
        ui.painter().galley_with_override_text_color(rect.min + vec2(2.0, 2.0), g, col);
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    /// Stepped slider. Fixed size, so it never moves under the cursor while dragging.
    pub fn step_slider(ui: &mut Ui, idx: &mut usize, steps: usize, width: f32) -> bool {
        let c = pal();
        let (rect, resp) = ui.allocate_exact_size(vec2(width, 28.0), Sense::click_and_drag());
        let track = Rect::from_center_size(rect.center(), vec2(width - 18.0, 6.0));
        let n = (steps - 1).max(1) as f32;
        let mut changed = false;
        if resp.is_pointer_button_down_on() || resp.dragged() {
            if let Some(pos) = ui.ctx().pointer_interact_pos() {
                let t = ((pos.x - track.left()) / track.width()).clamp(0.0, 1.0);
                let ni = (t * n).round() as usize;
                if ni != *idx {
                    *idx = ni;
                    changed = true;
                }
            }
        }
        let anim = ui.ctx().animate_value_with_time(resp.id, *idx as f32 / n, 0.08);
        let p = ui.painter();
        let track = Rect::from_center_size(track.center(), vec2(track.width(), 4.0));
        p.rect_filled(track, CornerRadius::same(2), c.panel3);
        p.rect_filled(Rect::from_min_max(track.min, pos2(track.left() + track.width() * anim, track.bottom())), CornerRadius::same(2), c.accent);
        for i in 0..steps {
            let x = track.left() + track.width() * i as f32 / n;
            p.circle_filled(pos2(x, track.bottom() + 7.0), 1.0, c.line2);
        }
        let kx = track.left() + track.width() * anim;
        if resp.hovered() || resp.dragged() {
            p.circle_filled(pos2(kx, track.center().y), 13.0, alpha(c.accent, 0.18));
        }
        p.circle_filled(pos2(kx, track.center().y), 8.0, c.text);
        resp.on_hover_cursor(egui::CursorIcon::Grab);
        changed
    }

    pub fn value_slider(ui: &mut Ui, v: &mut f32, min: f32, max: f32, width: f32) -> bool {
        let c = pal();
        let (rect, resp) = ui.allocate_exact_size(vec2(width, 18.0), Sense::click_and_drag());
        let track = Rect::from_center_size(rect.center(), vec2(width - 12.0, 4.0));
        let mut changed = false;
        if resp.is_pointer_button_down_on() || resp.dragged() {
            if let Some(pos) = ui.ctx().pointer_interact_pos() {
                let t = ((pos.x - track.left()) / track.width()).clamp(0.0, 1.0);
                let mut nv = min + t * (max - min);
                if (nv - 1.0).abs() < 0.04 {
                    nv = 1.0;
                }
                if (nv - *v).abs() > 0.001 {
                    *v = nv;
                    changed = true;
                }
            }
        }
        let t = (*v - min) / (max - min);
        let p = ui.painter();
        p.rect_filled(track, CornerRadius::same(2), c.panel3);
        p.rect_filled(Rect::from_min_max(track.min, pos2(track.left() + track.width() * t, track.bottom())), CornerRadius::same(2), c.accent);
        let one = track.left() + track.width() * (1.0 - min) / (max - min);
        p.line_segment([pos2(one, track.top() - 4.0), pos2(one, track.bottom() + 4.0)], Stroke::new(1.0, c.dim));
        p.circle_filled(pos2(track.left() + track.width() * t, track.center().y), 6.5, c.text);
        changed
    }
}

// ================================================================================================
// ui
// ================================================================================================

mod ui {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant, SystemTime};

    use eframe::egui::{
        self, Align, Align2, Color32, CornerRadius, Layout, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, UiBuilder,
        ViewportCommand, pos2, vec2,
    };

    use crate::app::{Core, ToastKind, core};
    use crate::editor::Editor;
    use crate::engine::{State, Stats, encoder_label};
    use crate::library::{self, Kind, Library};
    use crate::settings::{
        AppTrack, BUFFER_STEPS, Blend, EncoderPref, FPS_STEPS, GAME, Quality, Settings, ShotMode, Source, ThemeId,
    };
    use crate::theme::*;
    use crate::{procs, sys};

    #[derive(Clone, Copy, PartialEq)]
    enum Page {
        Live,
        Library,
        Editor,
        Settings,
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Which {
        Clip,
        Shot,
    }

    struct Hist {
        at: Instant,
        held: VecDeque<f32>,
        mem: VecDeque<f32>,
        cpu: VecDeque<f32>,
        rate: VecDeque<f32>,
    }

    impl Hist {
        fn push(q: &mut VecDeque<f32>, v: f32) {
            q.push_back(v);
            while q.len() > 14 {
                q.pop_front();
            }
        }
    }

    struct CpuMeter {
        at: Instant,
        pid: Option<u32>,
        ff: u64,
        own: u64,
        pct: f32,
    }

    pub struct FerrocapApp {
        core: &'static Arc<Core>,
        page: Page,
        page_changed: Instant,
        logo: egui::TextureHandle,
        library: Library,
        lib_tab: Kind,
        /// Library category filter (None = all)
        lib_cat: Option<String>,
        lib_dialog: Option<LibDialog>,
        rebinding: Option<(Which, sys::KeyCapture)>,
        cpu: CpuMeter,
        hist: Hist,
        mics: Option<Vec<String>>,
        editor: Option<Editor>,
        editor_err: Option<String>,
        quitting: bool,
        styled_window: u32,
        hide_frames: u32,
        theme: ThemeId,
        levels: Vec<f32>,
        last_frame: Instant,
        menu: Option<TrayMenu>,
        viewer: Option<Viewer>,
        view_req: Option<std::path::PathBuf>,
        snip: crate::snip::SnipUi,
        /// main window was hidden when a snip started: parked off-screen meanwhile (outer rect, in points)
        parked: Option<Rect>,
        icons: std::collections::HashMap<String, Option<egui::TextureHandle>>,
        search: String,
        side_collapsed: bool,
    }

    /// Full-window player for clips (and viewer for screenshots).
    struct Viewer {
        path: std::path::PathBuf,
        shot: bool,
        player: Option<crate::editor::Player>,
        dur: f64,
        opened: Instant,
        scrubbing: bool,
        err: Option<String>,
        game: Option<String>,
    }

    struct TrayMenu {
        pos: Pos2,
        opened: Instant,
        seen_focus: bool,
    }

    impl FerrocapApp {
        pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
            let s = core().settings();
            set_theme(s.theme);
            install_fonts(&cc.egui_ctx);
            apply_visuals(&cc.egui_ctx);
            egui_extras::install_image_loaders(&cc.egui_ctx);
            let img = image::load_from_memory(include_bytes!("../assets/logo.png")).expect("logo").to_rgba8();
            let size = [img.width() as usize, img.height() as usize];
            let logo = cc.egui_ctx.load_texture("logo", egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()), egui::TextureOptions::LINEAR);
            let page = match std::env::args().find_map(|a| a.strip_prefix("--page=").map(str::to_owned)).as_deref() {
                Some("settings") => Page::Settings,
                Some("library") => Page::Library,
                _ => Page::Live,
            };
            let mut me = Self {
                core: core(),
                page,
                page_changed: Instant::now(),
                logo,
                library: Library::new(),
                lib_tab: Kind::Clip,
                lib_cat: None,
                lib_dialog: None,
                rebinding: None,
                cpu: CpuMeter { at: Instant::now(), pid: None, ff: 0, own: 0, pct: 0.0 },
                hist: Hist { at: Instant::now(), held: VecDeque::new(), mem: VecDeque::new(), cpu: VecDeque::new(), rate: VecDeque::new() },
                mics: None,
                editor: None,
                editor_err: None,
                quitting: false,
                styled_window: 0,
                hide_frames: if crate::START_HIDDEN.load(Ordering::SeqCst) { 3 } else { 0 },
                theme: s.theme,
                levels: vec![],
                last_frame: Instant::now(),
                menu: None,
                viewer: None,
                view_req: None,
                snip: crate::snip::SnipUi::new(),
                parked: None,
                icons: std::collections::HashMap::new(),
                search: String::new(),
                side_collapsed: false,
            };
            if let Some(path) = std::env::args().find_map(|a| a.strip_prefix("--play=").map(std::path::PathBuf::from)) {
                me.view_req = Some(path);
            }
            if std::env::args().any(|a| a == "--maximized") {
                cc.egui_ctx.send_viewport_cmd(ViewportCommand::Maximized(true));
            }
            if let Some(path) = std::env::args().find_map(|a| a.strip_prefix("--edit=").map(std::path::PathBuf::from)) {
                me.open_editor(&path);
            }
            me
        }

        /// Icon of the detected game's exe, extracted once and cached.
        fn game_icon(&mut self, ctx: &egui::Context, g: &procs::Game) -> Option<egui::TextureId> {
            let key = g.exe_path.clone()?;
            let entry = self.icons.entry(key.clone()).or_insert_with(|| {
                sys::exe_icon(&key, 64).map(|(rgba, w, h)| {
                    ctx.load_texture(format!("icon:{key}"), egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba), egui::TextureOptions::LINEAR)
                })
            });
            entry.as_ref().map(|t| t.id())
        }

        fn open_viewer(&mut self, path: &std::path::Path) {
            if let Some(v) = self.viewer.as_mut() {
                if let Some(p) = v.player.as_mut() {
                    p.pause();
                }
            }
            let shot = path.extension().map(|e| e.eq_ignore_ascii_case("png") || e.eq_ignore_ascii_case("jpg")).unwrap_or(false);
            let item = self.library.clips.iter().find(|i| i.path == path).cloned();
            let game = item.and_then(|i| self.library.meta(&i)).and_then(|m| m.game);
            let mut v = Viewer { path: path.to_path_buf(), shot, player: None, dur: 0.0, opened: Instant::now(), scrubbing: false, err: None, game };
            if !shot {
                match crate::editor::probe(path) {
                    Some(pr) => {
                        let mut pl = crate::editor::Player::new(path, &pr);
                        pl.play();
                        v.dur = pr.duration;
                        v.player = Some(pl);
                    }
                    None => v.err = Some("Couldn't read this video".into()),
                }
            }
            self.viewer = Some(v);
        }

        fn close_viewer(&mut self) {
            if let Some(mut v) = self.viewer.take() {
                if let Some(p) = v.player.as_mut() {
                    p.pause();
                }
            }
        }

        fn viewer_ui(&mut self, ui: &mut Ui, area: Rect) {
            let p = pal();
            let ctx = ui.ctx().clone();
            let Some(v) = self.viewer.as_mut() else { return };
            let fade = (v.opened.elapsed().as_secs_f32() / 0.2).min(1.0);
            let ease = 1.0 - (1.0 - fade).powi(3);
            if fade < 1.0 {
                ctx.request_repaint();
            }
            let pt = ui.painter().clone();
            // dim everything behind; clicking outside closes
            let bg = ui.interact(area, ui.scope_id().with("viewer-bg"), Sense::click());
            pt.rect_filled(area, CornerRadius::ZERO, alpha(Color32::from_rgb(4, 4, 4), 0.78 * ease));
            let card_r = area.shrink2(vec2(36.0, 30.0)).translate(vec2(0.0, (1.0 - ease) * 16.0));
            pt.rect_filled(card_r, CornerRadius::same(16), alpha(p.panel, ease));
            pt.rect_stroke(card_r, CornerRadius::same(16), Stroke::new(1.0, alpha(p.line2, ease)), StrokeKind::Inside);

            // header
            let (kind, when) = split_name(&v.path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default());
            let title = if v.shot { "Screenshot".to_string() } else { v.game.clone().unwrap_or(kind.clone()) };
            pt.text(card_r.min + vec2(22.0, 30.0), Align2::LEFT_CENTER, &title, semibold(17.0), p.text);
            let tw = pt.layout_no_wrap(title.clone(), semibold(17.0), p.text).size().x;
            pt.text(card_r.min + vec2(34.0 + tw, 31.0), Align2::LEFT_CENTER, when, body(13.0), p.muted);
            let mut close = false;
            let mut edit = false;
            let hb = Rect::from_min_max(pos2(card_r.right() - 420.0, card_r.top() + 12.0), pos2(card_r.right() - 16.0, card_r.top() + 48.0));
            ui.scope_builder(UiBuilder::new().max_rect(hb).layout(Layout::right_to_left(Align::Center)), |ui| {
                if button(ui, "Close  Esc").clicked() {
                    close = true;
                }
                if button(ui, "Show in folder").clicked() {
                    sys::reveal(&v.path);
                }
                if cta(ui, "Edit").clicked() {
                    edit = true;
                }
            });

            let media = Rect::from_min_max(pos2(card_r.left() + 18.0, card_r.top() + 60.0), pos2(card_r.right() - 18.0, card_r.bottom() - if v.shot { 18.0 } else { 74.0 }));
            pt.rect_filled(media, CornerRadius::same(10), Color32::BLACK);
            let fit = |aspect: f32| {
                let mut w = media.width();
                let mut h = w / aspect;
                if h > media.height() {
                    h = media.height();
                    w = h * aspect;
                }
                Rect::from_center_size(media.center(), vec2(w, h))
            };
            if v.shot {
                let uri = format!("file:///{}", v.path.display().to_string().replace('\\', "/"));
                let img = egui::Image::new(uri).fit_to_exact_size(media.size()).maintain_aspect_ratio(true);
                ui.put(media, img);
            } else if let Some(err) = &v.err {
                pt.text(media.center(), Align2::CENTER_CENTER, err, body(14.0), p.muted);
            } else if let Some(pl) = v.player.as_mut() {
                pl.tick(&ctx, &[(0.0, v.dur)]);
                let vr = fit(pl.aspect());
                if !v.scrubbing {
                    pl.fit_to(vr.width() * ctx.pixels_per_point());
                }
                if let Some(t) = &pl.tex {
                    pt.image(t.id(), vr, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
                } else {
                    pt.text(vr.center(), Align2::CENTER_CENTER, "loading…", body(13.0), p.dim);
                }
                let click = ui.interact(media, ui.scope_id().with("viewer-media"), Sense::click());
                let toggle_play = click.clicked() || (ui.input(|i| i.key_pressed(egui::Key::Space)) && ui.ctx().memory(|m| m.focused().is_none()));
                if toggle_play {
                    if pl.playing { pl.pause() } else { pl.play() }
                }
                // big play glyph when paused
                if !pl.playing {
                    let c = vr.center();
                    pt.circle_filled(c, 34.0, alpha(Color32::BLACK, 0.55));
                    pt.add(egui::Shape::convex_polygon(vec![c + vec2(-10.0, -15.0), c + vec2(16.0, 0.0), c + vec2(-10.0, 15.0)], Color32::WHITE, Stroke::NONE));
                }
                if ui.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
                    pl.seek(pl.pos + 5.0);
                }
                if ui.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
                    pl.seek(pl.pos - 5.0);
                }

                // controls
                let cy = card_r.bottom() - 37.0;
                let pb = Rect::from_center_size(pos2(card_r.left() + 40.0, cy), vec2(38.0, 38.0));
                let pr = ui.interact(pb, ui.scope_id().with("viewer-play"), Sense::click());
                let hv = ctx.animate_bool_with_time(pr.id, pr.hovered(), 0.12);
                pt.rect_filled(pb, CornerRadius::same(10), mix(p.text, p.accent, hv));
                let c = pb.center();
                let ink = on(mix(p.text, p.accent, hv));
                if pl.playing {
                    for dx in [-5.0, 1.5] {
                        pt.rect_filled(Rect::from_min_size(c + vec2(dx, -7.0), vec2(3.5, 14.0)), CornerRadius::same(1), ink);
                    }
                } else {
                    pt.add(egui::Shape::convex_polygon(vec![c + vec2(-5.0, -8.0), c + vec2(8.0, 0.0), c + vec2(-5.0, 8.0)], ink, Stroke::NONE));
                }
                if pr.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    if pl.playing { pl.pause() } else { pl.play() }
                }
                let tlabel = format!("{}  /  {}", fmt_clock(pl.pos), fmt_clock(v.dur));
                let tg = pt.layout_no_wrap(tlabel, body(13.0), p.muted);
                pt.galley(pos2(card_r.right() - 24.0 - tg.size().x, cy - tg.size().y / 2.0), tg.clone(), p.muted);
                let bar = Rect::from_min_max(pos2(pb.right() + 18.0, cy - 14.0), pos2(card_r.right() - 40.0 - tg.size().x, cy + 14.0));
                let br = ui.interact(bar, ui.scope_id().with("viewer-scrub"), Sense::click_and_drag());
                let track = Rect::from_center_size(bar.center(), vec2(bar.width(), 4.0));
                let hover_t = br.hover_pos().map(|h| (((h.x - track.left()) / track.width()).clamp(0.0, 1.0) as f64) * v.dur);
                if br.is_pointer_button_down_on() || br.dragged() {
                    if let Some(h) = ctx.pointer_interact_pos() {
                        let t = (((h.x - track.left()) / track.width()).clamp(0.0, 1.0) as f64) * v.dur;
                        if !v.scrubbing {
                            v.scrubbing = true;
                        }
                        if (t - pl.pos).abs() > 0.05 {
                            pl.seek(t);
                        }
                    }
                } else {
                    v.scrubbing = false;
                }
                let grow = ctx.animate_bool_with_time(br.id, br.hovered() || v.scrubbing, 0.12);
                let track = Rect::from_center_size(bar.center(), vec2(bar.width(), 4.0 + 2.0 * grow));
                let f = if v.dur > 0.0 { (pl.pos / v.dur).clamp(0.0, 1.0) as f32 } else { 0.0 };
                pt.rect_filled(track, CornerRadius::same(3), p.panel3);
                pt.rect_filled(Rect::from_min_size(track.min, vec2(track.width() * f, track.height())), CornerRadius::same(3), p.accent);
                pt.circle_filled(pos2(track.left() + track.width() * f, track.center().y), 5.0 + 2.0 * grow, p.text);
                if let (Some(ht), true) = (hover_t, br.hovered()) {
                    let x = track.left() + track.width() * (ht / v.dur.max(0.001)) as f32;
                    let lab = fmt_clock(ht);
                    let g = pt.layout_no_wrap(lab, semibold(11.5), p.text);
                    let r = Rect::from_center_size(pos2(x, track.top() - 16.0), g.size() + vec2(12.0, 6.0));
                    pt.rect_filled(r, CornerRadius::same(5), p.panel3);
                    pt.galley(r.min + vec2(6.0, 3.0), g, p.text);
                }
            }

            if ui.input(|i| i.key_pressed(egui::Key::Escape)) || (bg.clicked() && !card_r.contains(bg.interact_pointer_pos().unwrap_or(card_r.center()))) {
                close = true;
            }
            let path = v.path.clone();
            let is_shot = v.shot;
            if edit && is_shot {
                self.close_viewer();
                crate::snip::open_file(path);
            } else if edit {
                self.close_viewer();
                self.open_editor(&path);
            } else if close {
                self.close_viewer();
            }
        }

        fn go(&mut self, page: Page) {
            if self.page != page {
                if self.page == Page::Editor {
                    if let Some(e) = self.editor.as_mut() {
                        e.stop();
                    }
                }
                self.page = page;
                self.page_changed = Instant::now();
                if page == Page::Library {
                    self.core.library_dirty.store(true, Ordering::SeqCst);
                }
            }
        }

        fn open_editor(&mut self, path: &std::path::Path) {
            match Editor::open(path) {
                Ok(e) => {
                    self.editor = Some(e);
                    self.editor_err = None;
                }
                Err(e) => self.editor_err = Some(e),
            }
            self.go(Page::Editor);
        }
    }

    impl eframe::App for FerrocapApp {
        fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
            pal().bg.to_normalized_gamma_f32()
        }

        fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            if let Some((x, y)) = self.core.tray_menu.lock().unwrap().take() {
                let ppp = ctx.pixels_per_point().max(0.5);
                self.menu = Some(TrayMenu { pos: pos2(x / ppp, y / ppp), opened: Instant::now(), seen_focus: false });
            }
            // Snip windows can only be created while the main window is "visible". If Ferrocap is in the
            // tray, park the main window off-screen for the duration instead.
            let hidden = !sys::main_window_visible();
            // (the tray menu needs this too: drawing it from a hidden main window crashed Ferrocap)
            let want_park = crate::snip::has_pending() || self.menu.is_some();
            if want_park && hidden && self.parked.is_none() {
                let r = ctx.input(|i| i.viewport().outer_rect).unwrap_or(Rect::from_min_size(pos2(100.0, 100.0), vec2(1360.0, 860.0)));
                self.parked = Some(r);
                ctx.send_viewport_cmd(ViewportCommand::OuterPosition(pos2(-32000.0, -32000.0)));
                ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            }
            if self.parked.is_some() && !crate::snip::has_pending() && !self.snip.active() && self.menu.is_none() {
                let r = self.parked.take().unwrap();
                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
                ctx.send_viewport_cmd(ViewportCommand::OuterPosition(r.min));
            }
            if crate::snip::has_pending() || self.snip.active() {
                ctx.request_repaint();
            }
            if self.menu.is_some() {
                ctx.request_repaint();
            }
            if self.core.show_request.swap(false, Ordering::SeqCst) {
                if let Some(r) = self.parked.take() {
                    ctx.send_viewport_cmd(ViewportCommand::OuterPosition(r.min));
                }
                self.hide_frames = 0;
                ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(ViewportCommand::Focus);
                self.core.library_dirty.store(true, Ordering::SeqCst);
            }
            if self.core.quit_request.load(Ordering::SeqCst) && !self.quitting {
                self.quitting = true;
                if let Some(e) = self.editor.as_mut() {
                    e.stop();
                }
                self.core.engine.shutdown();
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
                if let Some(e) = self.editor.as_mut() {
                    e.stop();
                }
                if self.rebinding.take().is_some() {
                    crate::hotkeys::suspend(false);
                }
            }
        }

        fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
            let ctx = ui.ctx().clone();
            if self.hide_frames > 0 {
                self.hide_frames -= 1;
                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
                ctx.request_repaint();
            }
            let s = self.core.settings();
            if let Some(g) = self.core.goto.lock().unwrap().take() {
                self.go(match g {
                    1 => Page::Library,
                    3 => Page::Settings,
                    _ => Page::Live,
                });
            }
            if self.menu.is_some() && sys::main_window_visible() {
                self.tray_menu(&ctx);
            }
            if sys::main_window_visible() {
                self.snip.poll(&ctx);
                self.snip.ui(&ctx);
            }
            if !ui.input(|i| i.viewport().visible()).unwrap_or(true) {
                return; // hidden: only the tray menu needed this pass
            }
            if self.styled_window < 3 {
                sys::style_own_window();
                self.styled_window += 1;
            }
            if s.theme != self.theme {
                self.theme = s.theme;
                set_theme(s.theme);
                apply_visuals(&ctx);
            }
            if self.core.library_dirty.swap(false, Ordering::SeqCst) {
                self.library.rescan(&s.clips_dir, &s.shots_dir);
            }
            if let Some(p) = self.core.open_in_editor.lock().unwrap().take() {
                self.open_editor(&p);
            }
            let stats = self.core.engine.stats();
            self.sample(&stats);

            let p = pal();
            let full = ui.max_rect();
            let side_w = ctx.animate_bool_with_time(ui.scope_id().with("collapse"), self.side_collapsed, 0.18);
            let side_w = egui::lerp(244.0..=76.0, side_w);
            let side = Rect::from_min_max(full.min, pos2(full.left() + side_w, full.bottom()));
            let main = Rect::from_min_max(pos2(side.right(), full.top()), full.max);
            let pt = ui.painter().clone();
            pt.rect_filled(full, CornerRadius::ZERO, p.bg);
            pt.rect_filled(main, CornerRadius::ZERO, p.bg2);
            backdrop(&pt, main, &p, ui.input(|i| i.time) as f32);
            pt.line_segment([side.right_top(), side.right_bottom()], Stroke::new(1.0, p.line));

            ui.scope_builder(UiBuilder::new().max_rect(side), |ui| self.sidebar(ui, &stats));
            ui.scope_builder(UiBuilder::new().max_rect(main), |ui| {
                let top = Rect::from_min_size(main.min + vec2(24.0, 0.0), vec2(main.width() - 48.0, 70.0));
                self.topbar(ui, top, &stats);
                let fade = (self.page_changed.elapsed().as_secs_f32() / 0.28).min(1.0);
                let ease = 1.0 - (1.0 - fade).powi(3);
                let content = Rect::from_min_max(pos2(main.left() + 24.0, top.bottom() + 18.0), main.max - vec2(24.0, 22.0))
                    .translate(vec2(0.0, (1.0 - ease) * 14.0));
                ui.scope_builder(UiBuilder::new().max_rect(content), |ui| {
                    if fade < 1.0 {
                        ui.set_opacity(ease);
                        ui.ctx().request_repaint();
                    }
                    match self.page {
                        Page::Live => self.page_live(ui, content, &stats, &s),
                        Page::Library => self.page_library(ui, content, &s),
                        Page::Editor => self.page_editor(ui, content),
                        Page::Settings => self.page_settings(ui, content, &stats),
                    }
                });
            });
            pt.rect_stroke(full, CornerRadius::ZERO, Stroke::new(1.0, p.line), StrokeKind::Inside);

            if let Some(p) = self.view_req.take() {
                self.open_viewer(&p);
            }
            if self.viewer.is_some() {
                let area = Rect::from_min_max(pos2(main.left(), main.top()), main.max);
                ui.scope_builder(UiBuilder::new().max_rect(area), |ui| self.viewer_ui(ui, area));
            }
            self.toasts(&ctx);
            let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
            let every = match self.page {
                Page::Live if focused => 16,
                Page::Live => 33,
                Page::Editor => 50,
                _ if self.rebinding.is_some() => 16,
                _ => 120,
            };
            ctx.request_repaint_after(Duration::from_millis(every));
        }
    }

    impl FerrocapApp {
        fn sample(&mut self, stats: &Stats) {
            // audio levels: fast attack, smooth release
            let lv: Vec<f32> = stats.info.levels.iter().map(|l| f32::from_bits(l.level.load(Ordering::Relaxed))).collect();
            if self.levels.len() != lv.len() {
                self.levels = lv.clone();
            }
            let now = Instant::now();
            let dt = now.duration_since(self.last_frame).as_secs_f32().clamp(0.0, 0.25);
            self.last_frame = now;
            // fast attack, smooth release (~0.9 per 1/30 s, whatever the frame rate)
            let keep = 0.9f32.powf(dt * 30.0);
            for (a, b) in self.levels.iter_mut().zip(lv) {
                *a = if b > *a { b } else { *a * keep + b * (1.0 - keep) };
            }
            let dt = now.duration_since(self.cpu.at).as_secs_f64();
            if dt >= 1.0 || self.cpu.pid != stats.ffmpeg_pid {
                let ff = stats.ffmpeg_pid.and_then(|p| sys::process_cpu_time(Some(p))).unwrap_or(0);
                let own = sys::process_cpu_time(None).unwrap_or(0);
                if self.cpu.pid == stats.ffmpeg_pid && dt > 0.0 {
                    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
                    let used = ff.saturating_sub(self.cpu.ff);
                    self.cpu.pct = (used as f64 / (dt * 1e7) / cores * 100.0) as f32;
                }
                self.cpu = CpuMeter { at: now, pid: stats.ffmpeg_pid, ff, own, pct: self.cpu.pct };
            }
            if now.duration_since(self.hist.at) >= Duration::from_millis(1000) {
                self.hist.at = now;
                let s = self.core.settings();
                Hist::push(&mut self.hist.held, stats.held_secs / s.buffer_secs.max(1) as f32);
                Hist::push(&mut self.hist.mem, stats.bytes as f32);
                Hist::push(&mut self.hist.cpu, self.cpu.pct);
                let last_rate = stats.chunks.iter().rev().nth(1).map(|c| c.1 as f32 * 8.0 / 1000.0).unwrap_or(0.0);
                Hist::push(&mut self.hist.rate, last_rate);
            }
            let _ = self.cpu.own;
        }

        // ------------------------------------------------------------------ chrome

        fn topbar(&mut self, ui: &mut Ui, r: Rect, _stats: &Stats) {
            let p = pal();
            let pt = ui.painter().clone();
            let cy = r.center().y;
            let maximized = ui.input(|i| i.viewport().maximized).unwrap_or(false);
            // drag anywhere empty on the bar, double-click to maximize
            let drag = ui.interact(r, ui.scope_id().with("drag"), Sense::click_and_drag());
            if drag.drag_started() {
                ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
            }
            if drag.double_clicked() {
                ui.ctx().send_viewport_cmd(ViewportCommand::Maximized(!maximized));
            }
            // search
            let sr = Rect::from_min_size(pos2(r.left(), cy - 19.0), vec2(300.0, 38.0));
            let focus = ui.ctx().animate_bool_with_time(ui.scope_id().with("sfocus"), ui.ctx().memory(|m| m.focused().is_some()) || !self.search.is_empty(), 0.15);
            pt.rect_filled(sr, CornerRadius::same(10), alpha(p.panel, 0.9));
            pt.rect_stroke(sr, CornerRadius::same(10), Stroke::new(1.0, mix(p.line2, p.accent, focus * 0.6)), StrokeKind::Inside);
            draw_icon(&pt, Icon::Search, pos2(sr.left() + 19.0, cy), mix(p.muted, p.text, focus), 1.0);
            let te = Rect::from_min_max(pos2(sr.left() + 36.0, cy - 11.0), pos2(sr.right() - 10.0, cy + 11.0));
            let before = self.search.clone();
            ui.scope_builder(UiBuilder::new().max_rect(te), |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .frame(egui::Frame::NONE)
                        .hint_text(egui::RichText::new("Search clips").color(p.dim))
                        .font(body(13.5))
                        .desired_width(te.width())
                        .align(Align2::LEFT_CENTER),
                );
            });
            if self.search != before && !self.search.is_empty() && self.page != Page::Library {
                self.go(Page::Library);
            }

            // window buttons, right to left: close, maximize, minimize
            let mut x = r.right();
            for (k, kind) in [(0, "close"), (1, "max"), (2, "min")] {
                let rr = Rect::from_min_size(pos2(x - 36.0, cy - 18.0), vec2(36.0, 36.0));
                x = rr.left() - 8.0;
                let resp = ui.interact(rr, ui.scope_id().with(("wb", k)), Sense::click());
                let h = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.14);
                let target = if k == 0 { Color32::from_rgb(0xd9, 0x34, 0x2b) } else { p.panel3 };
                let rr2 = rr.shrink(1.5 * (1.0 - h) * 0.0);
                pt.rect_filled(rr2, CornerRadius::same(10), mix(p.panel, target, h));
                pt.rect_stroke(rr2, CornerRadius::same(10), Stroke::new(1.0, mix(p.line2, target, h)), StrokeKind::Inside);
                let c = rr.center();
                let col = mix(p.muted, Color32::WHITE, h);
                let st = Stroke::new(1.3, col);
                match kind {
                    "close" => {
                        let d = 4.5 + h * 0.8;
                        pt.line_segment([c + vec2(-d, -d), c + vec2(d, d)], st);
                        pt.line_segment([c + vec2(-d, d), c + vec2(d, -d)], st);
                    }
                    "max" => {
                        if maximized {
                            pt.rect_stroke(Rect::from_center_size(c + vec2(-1.5, 1.5), vec2(8.0, 8.0)), CornerRadius::same(2), st, StrokeKind::Middle);
                            pt.line_segment([c + vec2(-0.5, -4.5), c + vec2(4.5, -4.5)], st);
                            pt.line_segment([c + vec2(4.5, -4.5), c + vec2(4.5, 0.5)], st);
                        } else {
                            let e = 9.0 + h * 1.5;
                            pt.rect_stroke(Rect::from_center_size(c, vec2(e, e)), CornerRadius::same(2), st, StrokeKind::Middle);
                        }
                    }
                    _ => {
                        let w = 5.0 + h * 1.0;
                        pt.line_segment([c + vec2(-w, 0.0), c + vec2(w, 0.0)], st);
                    }
                }
                if resp.clicked() {
                    ui.ctx().send_viewport_cmd(match kind {
                        "close" => ViewportCommand::Close,
                        "max" => ViewportCommand::Maximized(!maximized),
                        _ => ViewportCommand::Minimized(true),
                    });
                }
            }
            x -= 6.0;
            // snip button
            {
                let g = pt.layout_no_wrap("Snip".to_string(), semibold(13.0), p.text);
                let bw = g.size().x + 46.0;
                let br = Rect::from_min_size(pos2(x - bw, cy - 18.0), vec2(bw, 36.0));
                let resp = ui.interact(br, ui.scope_id().with("snipbtn"), Sense::click());
                let h = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
                pt.rect_filled(br, CornerRadius::same(10), mix(alpha(p.panel, 0.9), p.panel2, h));
                pt.rect_stroke(br, CornerRadius::same(10), Stroke::new(1.0, mix(p.line2, p.accent, h * 0.7)), StrokeKind::Inside);
                draw_icon(&pt, Icon::Editor, pos2(br.left() + 19.0, cy), mix(p.muted, p.accent, h), 1.0);
                pt.galley(pos2(br.left() + 34.0, cy - g.size().y / 2.0), g, p.text);
                if resp.on_hover_text("Snip part of the screen (Win+Shift+S)").clicked() {
                    // get our own window out of the way first
                    ui.ctx().send_viewport_cmd(ViewportCommand::Visible(false));
                    crate::snip::trigger(250, true);
                }
                x = br.left() - 10.0;
            }
            // date pill
            let date = sys::timestamp();
            let d = date.split('_').next().unwrap_or("").split('-').collect::<Vec<_>>();
            let dtxt = if d.len() == 3 { format!("{} - {} - {}", d[1], d[2], d[0]) } else { date.clone() };
            let g = pt.layout_no_wrap(dtxt, body(13.0), p.muted);
            let dw = g.size().x + 46.0;
            let dr = Rect::from_min_size(pos2(x - dw, cy - 18.0), vec2(dw, 36.0));
            pt.rect_filled(dr, CornerRadius::same(10), alpha(p.panel, 0.9));
            pt.rect_stroke(dr, CornerRadius::same(10), Stroke::new(1.0, p.line2), StrokeKind::Inside);
            draw_icon(&pt, Icon::Calendar, pos2(dr.left() + 19.0, cy), p.muted, 1.0);
            pt.galley(pos2(dr.left() + 34.0, cy - g.size().y / 2.0), g, p.muted);

            pt.line_segment([pos2(r.left(), r.bottom()), pos2(r.right(), r.bottom())], Stroke::new(1.0, p.line));
        }

        fn sidebar(&mut self, ui: &mut Ui, stats: &Stats) {
            let p = pal();
            let full = ui.max_rect();
            let pt = ui.painter().clone();
            let collapsed = full.width() < 150.0;
            let x0 = full.left() + 16.0;
            let w = full.width() - 32.0;
            // brand
            let brand = Rect::from_min_size(full.min, vec2(full.width(), 70.0));
            if ui.interact(brand, ui.scope_id().with("brand"), Sense::click_and_drag()).drag_started() {
                ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
            }
            let lr = Rect::from_min_size(pos2(x0 + 6.0, brand.center().y - 13.0), vec2(26.0, 26.0));
            pt.image(self.logo.id(), lr, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
            let tog = Rect::from_center_size(pos2(full.right() - 30.0, brand.center().y), vec2(28.0, 28.0));
            if !collapsed {
                pt.text(pos2(lr.right() + 10.0, brand.center().y), Align2::LEFT_CENTER, "Ferrocap", semibold(17.0), p.text);
            }
            let tr = ui.interact(if collapsed { lr.expand(6.0) } else { tog }, ui.scope_id().with("collapse-btn"), Sense::click());
            if !collapsed {
                draw_icon(&pt, Icon::Sidebar, tog.center(), if tr.hovered() { p.text } else { p.muted }, 0.7);
            }
            if tr.on_hover_text(if collapsed { "Expand" } else { "Collapse" }).clicked() {
                self.side_collapsed = !self.side_collapsed;
            }

            // main menu
            let mut y = brand.bottom() + 18.0;
            if !collapsed {
                pt.text(pos2(x0 + 10.0, y + 10.0), Align2::LEFT_CENTER, "Main Menu", body(13.0), p.muted);
                let plus = Rect::from_center_size(pos2(full.right() - 30.0, y + 10.0), vec2(22.0, 22.0));
                let pr = ui.interact(plus, ui.scope_id().with("plus"), Sense::click());
                draw_icon(&pt, Icon::Plus, plus.center(), if pr.hovered() { p.accent } else { p.muted }, 0.7);
                if pr.on_hover_text("Save a clip now").clicked() {
                    self.core.save_clip();
                }
                y += 34.0;
            }
            let items = [
                (Page::Live, "Overview", Icon::Live, None),
                (Page::Library, "Library", Icon::Library, Some(self.library.clips.len() + self.library.shots.len())),
                (Page::Editor, "Editor", Icon::Editor, None),
                (Page::Settings, "Settings", Icon::Settings, None),
            ];
            let active_i = items.iter().position(|c| c.0 == self.page).unwrap_or(0) as f32;
            let anim = ui.ctx().animate_value_with_time(ui.scope_id().with("navsel"), active_i, 0.16);
            let sel = Rect::from_min_size(pos2(x0, y + anim * 46.0), vec2(w, 40.0));
            pt.rect_filled(sel, CornerRadius::same(10), p.panel2);
            pt.rect_stroke(sel, CornerRadius::same(10), Stroke::new(1.0, p.line2), StrokeKind::Inside);
            for (page, label, icon, count) in items {
                let r = Rect::from_min_size(pos2(x0, y), vec2(w, 40.0));
                let resp = ui.interact(r, ui.scope_id().with(label), Sense::click());
                let active = self.page == page;
                if resp.clicked() {
                    if page == Page::Editor && self.editor.is_none() {
                        if let Some(c) = self.library.clips.first().cloned() {
                            self.open_editor(&c.path);
                        } else {
                            self.go(page);
                        }
                    } else {
                        self.go(page);
                    }
                }
                let hv = ui.ctx().animate_bool_with_time(resp.id, resp.hovered() && !active, 0.14);
                if hv > 0.0 {
                    pt.rect_filled(r, CornerRadius::same(10), alpha(p.panel2, 0.7 * hv));
                }
                let col = if active { p.text } else { mix(p.muted, p.text, hv) };
                let ix = if collapsed { r.center().x } else { r.left() + 20.0 + hv * 2.0 };
                draw_icon(&pt, icon, pos2(ix, r.center().y), if active { p.accent } else { col }, 0.62);
                if !collapsed {
                    pt.text(pos2(r.left() + 40.0, r.center().y), Align2::LEFT_CENTER, label, body(14.5), col);
                    if let Some(n) = count {
                        pt.text(pos2(r.right() - 12.0, r.center().y), Align2::RIGHT_CENTER, n.to_string(), body(12.5), p.dim);
                    }
                }
                let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                if collapsed {
                    resp.on_hover_text(label);
                }
                y += 46.0;
            }

            // update available card
            if !collapsed {
                let card = crate::update::status_for_card();
                let version = match &card {
                    crate::update::Status::Available { version, .. } | crate::update::Status::Error(version) => Some(version.clone()),
                    crate::update::Status::Downloading(_) => Some(String::new()),
                    _ => None,
                };
                if let Some(version) = version {
                    let ur = Rect::from_min_size(pos2(x0, full.bottom() - 284.0), vec2(w, 62.0));
                    let t = ui.input(|i| i.time) as f32;
                    let glow = (t * 2.0).sin() * 0.5 + 0.5;
                    pt.rect_filled(ur, CornerRadius::same(12), mix(p.panel, p.accent, 0.10));
                    pt.rect_stroke(ur, CornerRadius::same(12), Stroke::new(1.0, alpha(p.accent, 0.45 + 0.3 * glow)), StrokeKind::Inside);
                    draw_icon(&pt, Icon::Arrow, ur.min + vec2(20.0, 20.0), p.accent, 0.6);
                    pt.text(ur.min + vec2(36.0, 20.0), Align2::LEFT_CENTER, "Update available", semibold(13.5), p.text);
                    let sub = match crate::update::status() {
                        crate::update::Status::Downloading(b) => format!("Downloading… {:.1} MB", b as f64 / 1e6),
                        crate::update::Status::Error(e) => e,
                        _ => format!("Version {version} · click to install"),
                    };
                    let subtxt = truncate(ui, &sub, &body(11.5), w - 32.0);
                    pt.text(ur.min + vec2(16.0, 43.0), Align2::LEFT_CENTER, subtxt, body(11.5), p.muted);
                    let r = ui.interact(ur, ui.scope_id().with("update-card"), Sense::click());
                    if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        crate::update::install();
                    }
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                }
            }

            // now playing card
            if !collapsed {
                let game = procs::state().lock().unwrap().game.clone();
                let cr = Rect::from_min_size(pos2(x0, full.bottom() - 212.0), vec2(w, 104.0));
                let hov = ui.ctx().animate_bool_with_time(ui.scope_id().with("npcard"), ui.rect_contains_pointer(cr), 0.15);
                pt.rect_filled(cr, CornerRadius::same(12), mix(p.panel, p.panel2, hov));
                pt.rect_stroke(cr, CornerRadius::same(12), Stroke::new(1.0, mix(p.line, p.line2, hov)), StrokeKind::Inside);
                let ir = Rect::from_min_size(cr.min + vec2(14.0, 13.0), vec2(22.0, 22.0));
                match game.as_ref().and_then(|g| self.game_icon(ui.ctx(), g)) {
                    Some(tex) => {
                        egui::Image::from_texture((tex, ir.size())).corner_radius(CornerRadius::same(5)).paint_at(ui, ir);
                    }
                    None => draw_icon(&pt, if game.is_some() { Icon::Gamepad } else { Icon::Live }, ir.center(), p.dim, 1.0),
                }
                pt.text(pos2(ir.right() + 10.0, ir.center().y), Align2::LEFT_CENTER, if game.is_some() { "Now playing" } else { "No game running" }, body(12.5), p.muted);
                let name = game.as_ref().map(|g| g.name.clone()).unwrap_or_else(|| "Clipping your screen".into());
                let nm = truncate(ui, &name, &semibold(15.0), w - 32.0);
                pt.text(cr.min + vec2(16.0, 54.0), Align2::LEFT_CENTER, nm, semibold(15.0), p.text);
                let s = self.core.settings();
                let held = (stats.held_secs / s.buffer_secs.max(1) as f32).clamp(0.0, 1.0);
                let held_a = ui.ctx().animate_value_with_time(ui.scope_id().with("npbar"), held, 0.4);
                let bar = Rect::from_min_size(pos2(cr.left() + 16.0, cr.bottom() - 22.0), vec2(w - 32.0, 4.0));
                pt.rect_filled(bar, CornerRadius::same(2), p.panel3);
                pt.rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * held_a, 4.0)), CornerRadius::same(2), p.accent);
                let src = if stats.info.source.is_empty() { "Starting…".to_string() } else { stats.info.source.clone() };
                pt.text(pos2(bar.left(), bar.top() - 10.0), Align2::LEFT_CENTER, src, body(11.0), p.dim);
                pt.text(
                    pos2(bar.right(), bar.top() - 10.0),
                    Align2::RIGHT_CENTER,
                    format!("{:.0} s ready", stats.held_secs.min(s.buffer_secs as f32)),
                    body(11.0),
                    p.dim,
                );
            }

            // bottom links
            let mut by = full.bottom() - 92.0;
            for (label, icon, quit) in [("Clips folder", Icon::Folder, false), ("Quit Ferrocap", Icon::Power, true)] {
                let r = Rect::from_min_size(pos2(x0, by), vec2(w, 36.0));
                let resp = ui.interact(r, ui.scope_id().with(label), Sense::click());
                let col = if quit { if resp.hovered() { Color32::from_rgb(0xff, 0x80, 0x70) } else { p.warn } } else if resp.hovered() { p.text } else { p.muted };
                let ix = if collapsed { r.center().x } else { r.left() + 20.0 };
                draw_icon(&pt, icon, pos2(ix, r.center().y), col, 0.6);
                if !collapsed {
                    pt.text(pos2(r.left() + 40.0, r.center().y), Align2::LEFT_CENTER, label, body(14.0), col);
                }
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    if quit {
                        self.core.request_quit();
                    } else {
                        let d = self.core.settings().clips_dir;
                        let _ = std::fs::create_dir_all(&d);
                        sys::open_path(&d);
                    }
                }
                by += 40.0;
            }
        }

        /// Page title row inside the content area. Returns the rect of the remaining content.
        fn header(&mut self, ui: &mut Ui, main: Rect, title: &str, sub: &str) -> Rect {
            let p = pal();
            let pt = ui.painter();
            let g = pt.layout_no_wrap(title.to_string(), semibold(22.0), p.text);
            let tw = g.size().x;
            pt.galley(pos2(main.left(), main.top()), g, p.text);
            pt.text(pos2(main.left() + tw + 14.0, main.top() + 15.0), Align2::LEFT_CENTER, sub, body(13.0), p.muted);
            Rect::from_min_size(main.min, vec2(main.width(), 40.0))
        }

        // ------------------------------------------------------------------ live

        fn page_live(&mut self, ui: &mut Ui, main: Rect, stats: &Stats, s: &Settings) {
            let p = pal();
            let pt = ui.painter().clone();
            let gap = 14.0;
            let held = stats.held_secs.min(s.buffer_secs as f32);
            let mons = sys::monitors();
            let mon = mons.get(s.monitor as usize).copied().unwrap_or(mons[0]);
            let est = s.buffer_bytes(mon.w as u32, mon.h as u32).max(1);
            let clips_24h = self
                .library
                .clips
                .iter()
                .filter(|c| SystemTime::now().duration_since(c.modified).map(|d| d.as_secs() < 86_400).unwrap_or(false))
                .count();
            let split = |v: f32, d: usize| -> (String, String) {
                let s = format!("{v:.d$}");
                match s.split_once('.') {
                    Some((a, b)) => (a.to_string(), format!(".{b}")),
                    None => (s, String::new()),
                }
            };
            let held_a = ui.ctx().animate_value_with_time(ui.scope_id().with("k_held"), held, 0.35);
            let (h1, t1) = split(held_a, 1);
            let mb = ui.ctx().animate_value_with_time(ui.scope_id().with("k_mb"), stats.bytes as f32 / 1e6, 0.35);
            let cpu_a = ui.ctx().animate_value_with_time(ui.scope_id().with("k_cpu"), self.cpu.pct, 0.5);
            let (h3, t3) = split(cpu_a, 2);
            let kpis = [
                (Icon::Clip, "Clip ready", h1, format!("{t1}s"), format!("{} s max", s.buffer_secs), p.good),
                (Icon::Memory, "Memory in use", format!("{mb:.0}"), "MB".to_string(), format!("of {:.0} MB", est as f32 / 1e6), p.good),
                (Icon::Cpu, "Capture CPU", h3, format!("{t3}%"), encoder_label(&stats.info.encoder).to_string(), if stats.info.encoder == "libx264" { p.warn } else { p.good }),
                (Icon::Camera, "Clips today", clips_24h.to_string(), String::new(), format!("{} total", self.library.clips.len()), p.good),
            ];
            let kh = 124.0;
            let kw = (main.width() - gap * 3.0) / 4.0;
            for (i, (icon, label, head, tail, chip, chip_c)) in kpis.iter().enumerate() {
                let r = Rect::from_min_size(pos2(main.left() + i as f32 * (kw + gap), main.top()), vec2(kw, kh));
                card(&pt, r);
                let hv = ui.ctx().animate_bool_with_time(ui.scope_id().with(("kpi", i)), ui.rect_contains_pointer(r), 0.18);
                if hv > 0.0 {
                    pt.rect_stroke(r, CornerRadius::same(14), Stroke::new(1.0, alpha(p.accent, 0.45 * hv)), StrokeKind::Inside);
                    radial(&pt.with_clip_rect(r.shrink(1.0)), r.right_top(), r.width() * 0.7, p.accent, 0.07 * hv);
                }
                draw_icon(&pt, *icon, r.min + vec2(26.0, 28.0), mix(p.muted, p.accent, hv), 0.65);
                pt.text(r.min + vec2(44.0, 28.0), Align2::LEFT_CENTER, *label, body(14.0), p.muted);
                let vr = big_value(&pt, pos2(r.left() + 18.0, r.bottom() - 16.0), head, tail, 34.0);
                pixel_chip(&pt, pos2(vr.right() + 12.0, vr.center().y + 6.0), chip, *chip_c);
            }

            // row 2: chart + audio mix
            let table_h = 292.0;
            let r2 = Rect::from_min_max(pos2(main.left(), main.top() + kh + gap), pos2(main.right(), main.bottom() - table_h - gap));
            let cw = r2.width() * 0.64;
            let chart = Rect::from_min_size(r2.min, vec2(cw, r2.height()));
            let mixr = Rect::from_min_max(pos2(chart.right() + gap, r2.top()), r2.max);
            card(&pt, chart);
            card(&pt, mixr);
            pt.text(chart.min + vec2(20.0, 30.0), Align2::LEFT_CENTER, "Replay Buffer", semibold(17.0), p.text);
            let segr = Rect::from_min_size(pos2(chart.right() - 20.0 - 4.0 * 46.0 - 4.0, chart.top() + 14.0), vec2(4.0 * 46.0 + 4.0, 32.0));
            ui.scope_builder(UiBuilder::new().max_rect(segr), |ui| {
                let mut b = s.buffer_secs;
                if pixel_segmented(ui, &mut b, &[(15, "15s"), (30, "30s"), (60, "1m"), (120, "2m")], 46.0) {
                    let mut ns = s.clone();
                    ns.buffer_secs = b;
                    self.core.apply(ns);
                }
            });
            pt.line_segment([pos2(chart.left() + 20.0, chart.top() + 58.0), pos2(chart.right() - 20.0, chart.top() + 58.0)], Stroke::new(1.0, p.line));
            let plot = Rect::from_min_max(pos2(chart.left() + 70.0, chart.top() + 84.0), pos2(chart.right() - 24.0, chart.bottom() - 40.0));
            self.stream_chart(ui, plot, chart, stats, s);

            pt.text(mixr.min + vec2(20.0, 30.0), Align2::LEFT_CENTER, "Audio Mix", semibold(17.0), p.text);
            pt.line_segment([pos2(mixr.left() + 20.0, mixr.top() + 58.0), pos2(mixr.right() - 20.0, mixr.top() + 58.0)], Stroke::new(1.0, p.line));
            self.levels_panel(ui, Rect::from_min_max(mixr.min + vec2(20.0, 70.0), mixr.max - vec2(20.0, 14.0)), stats);

            // row 3: recent clips table
            let tr = Rect::from_min_max(pos2(main.left(), main.bottom() - table_h), main.max);
            card(&pt, tr);
            self.recent_panel(ui, tr, s);
        }

        /// Smooth curve through points sorted by x (Fritsch–Carlson monotone cubic): no wobble or
        /// overshoot between points, so the line never shows a bitrate that wasn't there.
        fn monotone_curve_impl(k: &[Pos2], per: usize) -> Vec<Pos2> {
            let n = k.len();
            if n < 3 {
                return k.to_vec();
            }
            let h: Vec<f32> = (0..n - 1).map(|i| (k[i + 1].x - k[i].x).max(1e-3)).collect();
            let d: Vec<f32> = (0..n - 1).map(|i| (k[i + 1].y - k[i].y) / h[i]).collect();
            let mut m = vec![0.0f32; n];
            m[0] = d[0];
            m[n - 1] = d[n - 2];
            for i in 1..n - 1 {
                m[i] = if d[i - 1] * d[i] <= 0.0 {
                    0.0
                } else {
                    let (w1, w2) = (2.0 * h[i] + h[i - 1], h[i] + 2.0 * h[i - 1]);
                    (w1 + w2) / (w1 / d[i - 1] + w2 / d[i])
                };
            }
            let mut out = Vec::with_capacity((n - 1) * per + 1);
            for i in 0..n - 1 {
                for j in 0..per {
                    let s = j as f32 / per as f32;
                    let (s2, s3) = (s * s, s * s * s);
                    let y = (2.0 * s3 - 3.0 * s2 + 1.0) * k[i].y
                        + (s3 - 2.0 * s2 + s) * h[i] * m[i]
                        + (-2.0 * s3 + 3.0 * s2) * k[i + 1].y
                        + (s3 - s2) * h[i] * m[i + 1];
                    out.push(pos2(k[i].x + s * h[i], y));
                }
            }
            out.push(k[n - 1]);
            out
        }

        fn stream_chart(&self, ui: &mut Ui, plot: Rect, outer: Rect, stats: &Stats, s: &Settings) {
            let monotone_curve = Self::monotone_curve_impl;
            let p = pal();
            let reveal = (self.page_changed.elapsed().as_secs_f32() / 0.7).min(1.0);
            let reveal = 1.0 - (1.0 - reveal).powi(3);
            let pt = ui.painter().with_clip_rect(Rect::from_min_max(outer.min, pos2(outer.left() + outer.width() * reveal, outer.bottom())).shrink(1.0));
            let t = ui.input(|i| i.time) as f32;
            let span = s.buffer_secs as f32;
            let target_kbps = if stats.info.kbps > 0 { stats.info.kbps as f32 } else { s.video_kbps(1920, 1080) as f32 };
            // One point per finished chunk, placed where that chunk ENDED and rated over its real length.
            // Ages run on the wall clock (since_chunk), so the line scrolls smoothly instead of jumping
            // a second at a time; the chunk still being recorded isn't drawn yet.
            let sc = stats.since_chunk;
            let data: Vec<(f32, f32)> = stats
                .chunks
                .windows(2)
                .map(|w| {
                    let dur = (w[0].0 - w[1].0).max(0.2);
                    (w[1].0 + sc, w[0].1 as f32 * 8.0 / 1000.0 / dur)
                })
                .filter(|c| c.0 <= span + 1.0)
                .collect();
            // linear axis with round steps (in Mb/s), fitted to what's actually in the buffer
            let peak = data.iter().map(|d| d.1).fold(0.0f32, f32::max).max(if data.is_empty() { target_kbps } else { 500.0 });
            let step_mb = [0.5f32, 1.0, 2.0, 5.0, 10.0, 20.0, 25.0, 50.0, 100.0]
                .into_iter()
                .find(|st| peak * 1.15 / 1000.0 / st <= 4.0)
                .unwrap_or(200.0);
            let ticks = ((peak * 1.15 / 1000.0) / step_mb).ceil().max(1.0) as u32;
            // the scale glides to its new size instead of jumping (and the labels glide with it)
            let ymax = ui.ctx().animate_value_with_time(egui::Id::new("ferrocap-chart-ymax"), ticks as f32 * step_mb * 1000.0, 0.6);
            let x_of = |ago: f32| plot.right() - (ago / span) * plot.width();
            let y_of = |kbps: f32| plot.bottom() - (kbps / ymax).clamp(0.0, 1.08) * plot.height();

            // grid: round steps; lines above the top fade out while the scale shrinks
            for k in 0..=ticks + 3 {
                let v = k as f32 * step_mb;
                let y = plot.bottom() - (v * 1000.0 / ymax) * plot.height();
                if y < plot.top() - 6.0 {
                    break;
                }
                let fade = ((y - (plot.top() - 6.0)) / 12.0).clamp(0.0, 1.0);
                if k == 0 {
                    pt.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(1.0, p.line2));
                } else {
                    dashed(&pt, pos2(plot.left(), y), pos2(plot.right(), y), 3.0, 5.0, Stroke::new(1.0, alpha(p.line2, 0.5 * fade)));
                }
                let lab = if step_mb < 1.0 { format!("{v:.1} Mb/s") } else { format!("{v:.0} Mb/s") };
                pt.text(pos2(outer.left() + 20.0, y), Align2::LEFT_CENTER, lab, body(11.0), alpha(p.muted, fade));
            }
            let step = if span <= 15.0 { 3.0 } else if span <= 30.0 { 5.0 } else if span <= 60.0 { 10.0 } else if span <= 180.0 { 30.0 } else { 60.0 };
            let mut a = 0.0;
            while a <= span + 0.01 {
                let lab = if a == 0.0 { "now".to_string() } else { format!("−{}s", a as u32) };
                pt.text(pos2(x_of(a), plot.bottom() + 20.0), Align2::CENTER_CENTER, lab, body(11.0), p.muted);
                a += step;
            }
            // summary next to the title: what a clip saved right now would hold
            if !data.is_empty() {
                let held = stats.held_secs.min(span);
                let avg = data.iter().map(|d| d.1).sum::<f32>() / data.len() as f32;
                let summary = format!("{:.0} s · {} · avg {:.1} Mb/s", held, sys::human_bytes(stats.bytes), avg / 1000.0);
                pt.text(outer.min + vec2(150.0, 30.0), Align2::LEFT_CENTER, summary, body(12.0), p.muted);
            }
            if data.len() < 2 {
                let msg = match &stats.state {
                    State::Error(e) => e.clone(),
                    State::Paused => "Buffering is paused.".into(),
                    State::WaitingForGame => "Start a game and Ferrocap kicks in on its own.".into(),
                    _ => "Warming up the encoder…".into(),
                };
                pt.text(plot.center(), Align2::CENTER_CENTER, msg, body(13.5), p.muted);
                return;
            }
            // A smooth line through the real 1-second chunks (monotone cubic, so it never overshoots
            // into values that weren't there), a gradient fill under it, and a color that runs from
            // quiet gray for the oldest seconds to the accent at the live edge.
            let knots: Vec<Pos2> = data.iter().map(|&(a, k)| pos2(x_of(a), y_of(k))).collect();
            let mut curve = monotone_curve(&knots, 10);
            // a new second doesn't pop in: the last segment draws in over ~0.35 s
            if knots.len() >= 2 {
                let f = (sc / 0.35).clamp(0.0, 1.0);
                let f = 1.0 - (1.0 - f) * (1.0 - f);
                let (a, b) = (knots[knots.len() - 2], knots[knots.len() - 1]);
                let limit = a.x + (b.x - a.x) * f;
                curve.retain(|q| q.x <= limit + 0.01); // samples are ~2 px apart, so the cut is smooth
            }
            let color_at = |x: f32| {
                let f = ((x - plot.left()) / plot.width()).clamp(0.0, 1.0);
                let f = ((f - 0.45) / 0.45).clamp(0.0, 1.0);
                (mix(mix(p.panel, p.text, 0.62), p.accent2, f * f * (3.0 - 2.0 * f)), 0.05 + 0.25 * f)
            };
            let mut mesh = egui::Mesh::default();
            for w in curve.windows(2) {
                let (a, b) = (w[0], w[1]);
                let (_, fa) = color_at(a.x);
                let (_, fb) = color_at(b.x);
                let i = mesh.vertices.len() as u32;
                mesh.colored_vertex(a, alpha(p.accent2, fa));
                mesh.colored_vertex(b, alpha(p.accent2, fb));
                mesh.colored_vertex(pos2(b.x, plot.bottom()), alpha(p.accent2, 0.0));
                mesh.colored_vertex(pos2(a.x, plot.bottom()), alpha(p.accent2, 0.0));
                mesh.add_triangle(i, i + 1, i + 2);
                mesh.add_triangle(i, i + 2, i + 3);
            }
            pt.add(egui::Shape::mesh(mesh));
            // average line
            let avg = data.iter().map(|d| d.1).sum::<f32>() / data.len() as f32;
            let ay = y_of(avg);
            dashed(&pt, pos2(plot.left(), ay), pos2(plot.right(), ay), 6.0, 5.0, Stroke::new(1.0, alpha(p.accent2, 0.45)));
            pt.text(pos2(plot.left() + 6.0, ay - 9.0), Align2::LEFT_CENTER, "avg", body(10.5), alpha(p.accent2, 0.8));
            // the line: a soft glow underneath, then the colored stroke
            pt.add(egui::Shape::line(curve.clone(), Stroke::new(6.0, alpha(p.accent2, 0.10))));
            for w in curve.windows(2) {
                let (c, _) = color_at((w[0].x + w[1].x) / 2.0);
                pt.line_segment([w[0], w[1]], Stroke::new(2.0, c));
            }
            // live edge: the head of the drawn line
            let hot = *curve.last().unwrap_or(&knots[knots.len() - 1]);
            pt.circle_filled(hot, 7.0 + 2.0 * (t * 2.4).sin(), alpha(p.accent2, 0.18));
            pt.circle_filled(hot, 4.0, p.panel);
            pt.circle_filled(hot, 3.0, p.accent2);

            let hover_ago = ui
                .ctx()
                .pointer_hover_pos()
                .filter(|hp| plot.expand2(vec2(0.0, 20.0)).contains(*hp))
                .map(|hp| ((plot.right() - hp.x) / plot.width() * span).clamp(0.0, span));
            let hovered = hover_ago.and_then(|ago| {
                data.iter().copied().min_by(|a, b| (a.0 - ago).abs().partial_cmp(&(b.0 - ago).abs()).unwrap())
            });

            // hover tooltip
            if hover_ago.is_some() {
                {
                    if let Some((ca, ck)) = hovered {
                        let q = pos2(x_of(ca), y_of(ck));
                        dashed(&pt, pos2(q.x, plot.top()), pos2(q.x, plot.bottom()), 2.0, 3.0, Stroke::new(1.0, alpha(p.text, 0.35)));
                        pt.circle_filled(q, 5.0, p.panel);
                        pt.circle_stroke(q, 4.0, Stroke::new(2.0, p.text));
                        let tip = Rect::from_min_size(q + vec2(-60.0, -86.0), vec2(124.0, 64.0));
                        let tip = tip.translate(vec2((plot.left() - tip.left()).max(0.0) + (plot.right() - tip.right()).min(0.0), (outer.top() + 64.0 - tip.top()).max(0.0)));
                        pt.rect_filled(tip, CornerRadius::same(8), p.panel2);
                        pt.rect_stroke(tip, CornerRadius::same(8), Stroke::new(1.0, p.line2), StrokeKind::Inside);
                        let lines = [
                            format!("{:.0} s ago", ca),
                            format!("Bitrate {:.1} Mb/s", ck / 1000.0),
                            format!("Chunk {}", sys::human_bytes((ck * 125.0) as u64)),
                        ];
                        for (i, l) in lines.iter().enumerate() {
                            pt.text(tip.min + vec2(10.0, 14.0 + i as f32 * 17.0), Align2::LEFT_CENTER, l, body(11.5), if i == 0 { p.muted } else { p.text });
                        }
                    }
                }
            }
        }

        fn levels_panel(&mut self, ui: &mut Ui, r: Rect, stats: &Stats) {
            let p = pal();
            let pt = ui.painter().clone();
            let levels = &stats.info.levels;
            if levels.is_empty() {
                pt.text(r.center(), Align2::CENTER_CENTER, "No audio sources. Turn some on in Settings.", body(13.0), p.dim);
                return;
            }
            let n = levels.len().min(6);
            let top = r.top() + 30.0;
            let bottom = r.bottom() - 46.0;
            let hgt = bottom - top;
            // "loud" reference line at -6 dB
            let ref_y = bottom - hgt * (54.0 / 60.0);
            dashed(&pt, pos2(r.left(), ref_y), pos2(r.right(), ref_y), 3.0, 4.0, Stroke::new(1.0, alpha(p.muted, 0.35)));
            pt.line_segment([pos2(r.left(), bottom), pos2(r.right(), bottom)], Stroke::new(1.0, p.line2));
            let cw = r.width() / n as f32;
            for i in 0..n {
                let v = self.levels.get(i).copied().unwrap_or(0.0).max(0.0);
                let live = levels[i].live.load(Ordering::Relaxed);
                let db = if v > 0.001 { 20.0 * v.log10() } else { -60.0 };
                let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
                let frac = ui.ctx().animate_value_with_time(ui.scope_id().with(("lvl", i)), frac, 0.12);
                let col = TRACK_COLORS[i % TRACK_COLORS.len()];
                let cx = r.left() + cw * (i as f32 + 0.5);
                let bw = (cw * 0.42).clamp(14.0, 34.0);
                let track = Rect::from_min_max(pos2(cx - bw / 2.0, top), pos2(cx + bw / 2.0, bottom));
                pt.rect_filled(track, CornerRadius::same(7), alpha(p.panel3, 0.55));
                let h = (frac * hgt).max(if live { 4.0 } else { 0.0 });
                if h > 0.5 {
                    let bar = Rect::from_min_max(pos2(track.left(), bottom - h), track.max);
                    pt.rect_filled(bar, CornerRadius::same(7), if live { col } else { alpha(col, 0.35) });
                    // soft fade toward the floor
                    use egui::epaint::{Mesh, Vertex, WHITE_UV};
                    let mut m = Mesh::default();
                    let top_c = Color32::TRANSPARENT;
                    let bot_c = alpha(p.panel, 0.55);
                    for (q, c) in [(bar.left_top(), top_c), (bar.right_top(), top_c), (bar.right_bottom(), bot_c), (bar.left_bottom(), bot_c)] {
                        m.vertices.push(Vertex { pos: q, uv: WHITE_UV, color: c });
                    }
                    m.add_triangle(0, 1, 2);
                    m.add_triangle(0, 2, 3);
                    pt.add(egui::Shape::mesh(m));
                }
                let lab = if live { format!("{db:.0} dB") } else { "idle".into() };
                pt.text(pos2(cx, top - 14.0), Align2::CENTER_CENTER, lab, semibold(12.0), if db > -3.0 { p.warn } else { p.muted });
                let nm = truncate(ui, &levels[i].name, &body(13.5), cw - 6.0);
                pt.text(pos2(cx, bottom + 16.0), Align2::CENTER_CENTER, nm, body(13.5), p.text);
                let kind = if levels[i].name == "System" || levels[i].name == "Mic" { "device" } else { "app" };
                pt.text(pos2(cx, bottom + 34.0), Align2::CENTER_CENTER, kind, body(11.5), p.dim);
            }
        }

        fn recent_panel(&mut self, ui: &mut Ui, r: Rect, s: &Settings) {
            let p = pal();
            let pt = ui.painter().clone();
            pt.text(r.min + vec2(20.0, 30.0), Align2::LEFT_CENTER, "Recent Clips", semibold(17.0), p.text);
            let q = self.search.to_lowercase();
            let mut clips: Vec<library::Item> = Vec::new();
            for c in self.library.clips.iter() {
                if q.is_empty() || c.name.to_lowercase().contains(&q) {
                    clips.push(c.clone());
                }
            }
            let total = clips.len();
            clips.truncate(5);
            pt.text(pos2(r.right() - 20.0, r.top() + 30.0), Align2::RIGHT_CENTER, format!("Top {} of {}", clips.len(), total), body(12.5), p.muted);
            pt.line_segment([pos2(r.left() + 20.0, r.top() + 58.0), pos2(r.right() - 20.0, r.top() + 58.0)], Stroke::new(1.0, p.line));
            let cols = [0.0, 0.26, 0.40, 0.55, 0.66, 0.77, 0.92];
            let heads = ["CLIP", "TYPE", "SAVED", "LENGTH", "SIZE", "STATUS", "VIEW"];
            let inner_l = r.left() + 20.0;
            let inner_w = r.width() - 40.0;
            let cx = |k: usize| inner_l + inner_w * cols[k];
            for (k, h) in heads.iter().enumerate() {
                pt.text(pos2(cx(k), r.top() + 82.0), Align2::LEFT_CENTER, *h, body(11.5), p.muted);
            }
            if clips.is_empty() {
                let msg = if total == 0 && q.is_empty() { format!("No clips yet. Press {} in a game.", s.clip_key.label()) } else { "No matches.".into() };
                pt.text(pos2(r.center().x, r.top() + 170.0), Align2::CENTER_CENTER, msg, body(13.5), p.dim);
                return;
            }
            let top = r.top() + 100.0;
            let row_h = ((r.bottom() - top - 10.0) / 5.0).min(38.0);
            for (i, c) in clips.iter().enumerate() {
                let rr = Rect::from_min_size(pos2(r.left() + 10.0, top + i as f32 * row_h), vec2(r.width() - 20.0, row_h));
                let resp = ui.interact(rr, ui.scope_id().with(("recent", i)), Sense::click());
                let hv = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
                if hv > 0.0 {
                    pt.rect_filled(rr, CornerRadius::same(8), alpha(p.panel2, hv));
                }
                let y = rr.center().y;
                let meta = self.library.meta(c);
                let (kind, when) = split_name(&c.name);
                let title = meta.as_ref().and_then(|m| m.game.clone()).unwrap_or_else(|| "Desktop".into());
                let ib = Rect::from_center_size(pos2(cx(0) + 11.0, y), vec2(22.0, 22.0));
                pt.rect_filled(ib, CornerRadius::same(6), p.panel3);
                draw_icon(&pt, Icon::Clip, ib.center(), TRACK_COLORS[i % TRACK_COLORS.len()], 0.5);
                let nm = truncate(ui, &title, &body(14.0), inner_w * 0.24 - 40.0);
                pt.text(pos2(cx(0) + 32.0, y), Align2::LEFT_CENTER, nm, body(14.0), p.text);
                tag_at(&pt, pos2(cx(1), y), if kind == "Edit" { "Edit" } else { "Replay" }, p.muted);
                let when = if when.is_empty() { library::ago(c.modified) } else { when };
                pt.text(pos2(cx(2), y), Align2::LEFT_CENTER, when, body(13.0), p.muted);
                let dur = meta.as_ref().and_then(|m| m.duration).map(library::fmt_duration).unwrap_or_else(|| "…".into());
                pt.text(pos2(cx(3), y), Align2::LEFT_CENTER, dur, body(13.5), p.text);
                pt.text(pos2(cx(4), y), Align2::LEFT_CENTER, sys::human_bytes(c.bytes), body(13.5), p.text);
                let fresh = SystemTime::now().duration_since(c.modified).map(|d| d.as_secs() < 600).unwrap_or(false);
                let chip = pixel_chip(&pt, pos2(cx(5) + 20.0, y), if fresh { "New" } else { "Saved" }, if fresh { p.accent2 } else { p.good });
                draw_icon(&pt, Icon::Check, pos2(chip.left() - 10.0, y), if fresh { p.accent2 } else { p.good }, 0.45);
                let vb = Rect::from_min_size(pos2(cx(6), y - 13.0), vec2(48.0, 26.0));
                let vr = ui.interact(vb, ui.scope_id().with(("view", i)), Sense::click());
                pt.rect_filled(vb, CornerRadius::same(7), if vr.hovered() { p.panel3 } else { p.panel2 });
                pt.rect_stroke(vb, CornerRadius::same(7), Stroke::new(1.0, p.line2), StrokeKind::Inside);
                pt.text(vb.center(), Align2::CENTER_CENTER, "Edit", body(12.5), p.text);
                if vr.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.open_editor(&c.path);
                } else if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.view_req = Some(c.path.clone());
                }
            }
        }

        // ------------------------------------------------------------------ library

        fn page_library(&mut self, ui: &mut Ui, main: Rect, s: &Settings) {
            let p = pal();
            let n = self.library.clips.len() + self.library.shots.len();
            self.header(ui, main, "Library", &format!("{n} items"));
            let ctl = Rect::from_min_max(pos2(main.right() - 640.0, main.top() - 2.0), pos2(main.right(), main.top() + 36.0));
            ui.scope_builder(UiBuilder::new().max_rect(ctl).layout(Layout::right_to_left(Align::Center)), |ui| {
                let dir = if self.lib_tab == Kind::Clip { s.clips_dir.clone() } else { s.shots_dir.clone() };
                if cta(ui, "Open folder").clicked() {
                    let _ = std::fs::create_dir_all(&dir);
                    sys::open_path(&dir);
                }
                ui.add_space(8.0);
                if button(ui, "Refresh").clicked() {
                    self.library.rescan(&s.clips_dir, &s.shots_dir);
                }
                ui.add_space(8.0);
                let mut tab = self.lib_tab;
                let a = format!("Clips  {}", self.library.clips.len());
                let b = format!("Shots  {}", self.library.shots.len());
                segmented(ui, &mut tab, &[(Kind::Clip, a.as_str()), (Kind::Shot, b.as_str())], 96.0);
                self.lib_tab = tab;
            });
            // category bar
            let bar = Rect::from_min_size(pos2(main.left(), main.top() + 52.0), vec2(main.width(), 34.0));
            if let Some(c) = &self.lib_cat {
                if !self.library.categories.iter().any(|x| x == c) {
                    self.lib_cat = None;
                }
            }
            ui.scope_builder(UiBuilder::new().max_rect(bar).layout(Layout::left_to_right(Align::Center)), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let src = if self.lib_tab == Kind::Clip { &self.library.clips } else { &self.library.shots };
                let mut chips: Vec<(Option<String>, String)> = vec![(None, format!("All  {}", src.len()))];
                for c in &self.library.categories {
                    let n = src.iter().filter(|i| i.category.as_deref() == Some(c.as_str())).count();
                    chips.push((Some(c.clone()), format!("{c}  {n}")));
                }
                for (cat, label) in chips {
                    let on = self.lib_cat == cat;
                    let r = category_chip(ui, &label, on);
                    if r.clicked() {
                        self.lib_cat = cat.clone();
                    }
                    if let Some(c) = cat {
                        egui::Popup::context_menu(&r).show(|ui| {
                            if ui.button("Rename category…").clicked() {
                                self.lib_dialog = Some(LibDialog::RenameCategory { old: c.clone(), text: c.clone(), err: None, opened: Instant::now() });
                                ui.close();
                            }
                            if ui.button("Delete category").on_hover_text("Its clips move back to the main folder. Nothing is deleted.").clicked() {
                                let bases = [s.clips_dir.as_path(), s.shots_dir.as_path()];
                                match library::delete_category(&bases, &c) {
                                    Ok(()) => self.core.toast(ToastKind::Shot, "Category removed", format!("{c}: its files are back in the main folder."), None),
                                    Err(e) => self.core.toast(ToastKind::Error, "Couldn't remove category", e, None),
                                }
                                self.library.rescan(&s.clips_dir, &s.shots_dir);
                                ui.close();
                            }
                        });
                    }
                }
                if category_chip(ui, "+  New category", false).clicked() {
                    self.lib_dialog = Some(LibDialog::NewCategory { move_item: None, text: String::new(), err: None, opened: Instant::now() });
                }
            });
            let body_r = Rect::from_min_max(pos2(main.left(), main.top() + 98.0), main.max);
            let q = self.search.to_lowercase();
            let src = if self.lib_tab == Kind::Clip { &self.library.clips } else { &self.library.shots };
            let items: Vec<library::Item> = src
                .iter()
                .filter(|i| self.lib_cat.is_none() || i.category == self.lib_cat)
                .filter(|i| q.is_empty() || i.name.to_lowercase().contains(&q) || i.category.as_deref().is_some_and(|c| c.to_lowercase().contains(&q)))
                .cloned()
                .collect();
            self.lib_dialog_ui(ui, main, s);
            ui.scope_builder(UiBuilder::new().max_rect(body_r), |ui| {
                if items.is_empty() {
                    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 260.0), Sense::hover());
                    let pt = ui.painter();
                    card(pt, r);
                    let (a, b) = if !q.is_empty() {
                        ("No matches".to_string(), format!("Nothing matches \"{}\".", self.search))
                    } else if let Some(c) = &self.lib_cat {
                        (format!("{c} is empty"), "Right-click a clip (or use its ⋯ menu) and pick Move to.".to_string())
                    } else if self.lib_tab == Kind::Clip {
                        ("Nothing here yet".to_string(), format!("Press {} and your clip lands here.", s.clip_key.label()))
                    } else {
                        ("No screenshots yet".to_string(), format!("Press {} for a lossless PNG.", s.shot_key.label()))
                    };
                    draw_icon(pt, Icon::Clip, r.center() - vec2(0.0, 44.0), p.dim, 1.4);
                    pt.text(r.center() - vec2(0.0, 2.0), Align2::CENTER_CENTER, a, semibold(18.0), p.text);
                    pt.text(r.center() + vec2(0.0, 24.0), Align2::CENTER_CENTER, b, body(13.5), p.muted);
                    return;
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    let w = ui.available_width() - 8.0;
                    let cols = if w > 1100.0 { 4 } else { 3 };
                    let gap = 16.0;
                    let cw = (w - gap * (cols as f32 - 1.0)) / cols as f32;
                    let ch = cw * 9.0 / 16.0 + 70.0;
                    let rows = items.len().div_ceil(cols);
                    let (area, _) = ui.allocate_exact_size(vec2(w, rows as f32 * (ch + gap)), Sense::hover());
                    let clip = ui.clip_rect();
                    for (i, item) in items.iter().enumerate() {
                        let rect = Rect::from_min_size(area.min + vec2((i % cols) as f32 * (cw + gap), (i / cols) as f32 * (ch + gap)), vec2(cw, ch));
                        if rect.intersects(clip) {
                            self.media_card(ui, rect, item);
                        }
                    }
                    ui.add_space(24.0);
                });
            });
        }

        fn media_card(&mut self, ui: &mut Ui, rect: Rect, item: &library::Item) {
            let p = pal();
            let id = ui.scope_id().with(("card", &item.path));
            let pad = 8.0;
            let thumb = Rect::from_min_size(rect.min + vec2(pad, pad), vec2(rect.width() - pad * 2.0, (rect.width() - pad * 2.0) * 9.0 / 16.0));
            let resp = ui.interact(thumb, id, Sense::click());
            let hover = ui.rect_contains_pointer(rect);
            let pt = ui.painter().clone();
            pt.rect_filled(rect, CornerRadius::same(14), if hover { p.panel2 } else { p.panel });
            pt.rect_stroke(rect, CornerRadius::same(14), Stroke::new(1.0, if hover { p.line2 } else { p.line }), StrokeKind::Inside);
            let meta = self.library.meta(item);
            match meta.as_ref().and_then(|m| m.thumb.clone()) {
                Some(t) => {
                    let uri = format!("file:///{}", t.display().to_string().replace('\\', "/"));
                    egui::Image::new(uri).corner_radius(CornerRadius::same(9)).paint_at(ui, thumb);
                }
                None => {
                    pt.rect_filled(thumb, CornerRadius::same(9), p.panel3);
                    draw_icon(&pt, Icon::Clip, thumb.center(), p.dim, 1.0);
                }
            }
            if resp.hovered() {
                pt.rect_filled(thumb, CornerRadius::same(9), alpha(Color32::BLACK, 0.35));
                let c = thumb.center();
                pt.circle_filled(c, 22.0, alpha(p.text, 0.95));
                if item.kind == Kind::Clip {
                    pt.add(egui::Shape::convex_polygon(vec![c + vec2(-5.0, -8.0), c + vec2(9.0, 0.0), c + vec2(-5.0, 8.0)], p.bg, Stroke::NONE));
                } else {
                    pt.circle_filled(c, 5.0, p.bg);
                }
            }
            if let Some(d) = meta.as_ref().and_then(|m| m.duration) {
                chip_at(&pt, thumb.right_bottom() - vec2(8.0, 8.0), &library::fmt_duration(d), Color32::WHITE, alpha(Color32::BLACK, 0.7));
            }
            let (_, when) = split_name(&item.name);
            let title = item_title(item, meta.as_ref().and_then(|m| m.game.clone()));
            if self.lib_cat.is_none() {
                if let Some(c) = &item.category {
                    chip_at(&pt, thumb.left_top() + vec2(8.0 + 6.0 * c.len().min(18) as f32 + 14.0, 8.0 + 11.0), c, Color32::WHITE, alpha(Color32::BLACK, 0.7));
                }
            }
            let nm = truncate(ui, &title, &semibold(14.5), rect.width() - 150.0);
            pt.text(pos2(rect.left() + 16.0, thumb.bottom() + 22.0), Align2::LEFT_CENTER, nm, semibold(14.5), p.text);
            pt.text(
                pos2(rect.left() + 16.0, thumb.bottom() + 44.0),
                Align2::LEFT_CENTER,
                format!("{} · {}", if when.is_empty() { library::ago(item.modified) } else { when }, sys::human_bytes(item.bytes)),
                body(12.5),
                p.muted,
            );
            self.item_menu(egui::Popup::context_menu(&resp), item);
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                self.view_req = Some(item.path.clone());
            }
            let more_open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&ui.interact(Rect::NOTHING, id.with(9u8), Sense::hover())));
            if hover || more_open {
                let y = thumb.bottom() + 33.0;
                let mut x = rect.right() - 12.0;
                let mut acts: Vec<(&str, u8)> = vec![("⋯", 9), ("Delete", 2)];
                if item.kind == Kind::Clip {
                    acts.push(("Edit", 0));
                }
                for (label, k) in acts {
                    let w = match k {
                        2 => 56.0,
                        9 => 32.0,
                        _ => 46.0,
                    };
                    let r = Rect::from_min_max(pos2(x - w, y - 13.0), pos2(x, y + 13.0));
                    x -= w + 6.0;
                    let rr = ui.interact(r, id.with(k), Sense::click());
                    if k == 9 {
                        self.item_menu(egui::Popup::menu(&rr), item);
                    }
                    let h = rr.hovered();
                    let (fill, fg) = match k {
                        0 => (p.text, p.bg),
                        2 if h => (alpha(p.warn, 0.15), p.warn),
                        _ => (if h { p.panel3 } else { p.panel }, p.text),
                    };
                    pt.rect_filled(r, CornerRadius::same(7), fill);
                    if k != 0 {
                        pt.rect_stroke(r, CornerRadius::same(7), Stroke::new(1.0, p.line2), StrokeKind::Inside);
                    }
                    pt.text(r.center(), Align2::CENTER_CENTER, label, semibold(12.0), fg);
                    if rr.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        match k {
                            0 => self.open_editor(&item.path),
                            1 => sys::reveal(&item.path),
                            9 => {}
                            _ => {
                                if sys::recycle(&item.path) {
                                    self.library.remove(&item.path);
                                    self.core.toast(ToastKind::Shot, "Moved to Recycle Bin", item.name.clone(), None);
                                }
                            }
                        }
                    }
                }
            }
        }

        /// The ⋯ / right-click menu of a library item: rename, categories, show, delete.
        fn item_menu(&mut self, popup: egui::Popup<'_>, item: &library::Item) {
            let s = self.core.settings();
            let base = if item.kind == Kind::Clip { s.clips_dir.clone() } else { s.shots_dir.clone() };
            let cats = self.library.categories.clone();
            let mut moved: Option<Result<std::path::PathBuf, String>> = None;
            popup.show(|ui| {
                ui.set_min_width(190.0);
                if ui.button("Rename…").clicked() {
                    let text = if item.name.starts_with("Ferrocap_") { String::new() } else { item.name.clone() };
                    self.lib_dialog = Some(LibDialog::Rename { path: item.path.clone(), text, err: None, opened: Instant::now() });
                    ui.close();
                }
                ui.menu_button("Move to", |ui| {
                    ui.set_min_width(170.0);
                    for c in &cats {
                        let here = item.category.as_deref() == Some(c.as_str());
                        if ui.add_enabled(!here, egui::Button::new(c.as_str())).clicked() {
                            moved = Some(library::move_to_category(&item.path, &base, Some(c)));
                            ui.close();
                        }
                    }
                    if !cats.is_empty() {
                        ui.separator();
                    }
                    if ui.button("New category…").clicked() {
                        self.lib_dialog = Some(LibDialog::NewCategory { move_item: Some(item.path.clone()), text: String::new(), err: None, opened: Instant::now() });
                        ui.close();
                    }
                });
                if item.category.is_some() && ui.button("Remove from category").clicked() {
                    moved = Some(library::move_to_category(&item.path, &base, None));
                    ui.close();
                }
                ui.separator();
                if ui.button("Show in folder").clicked() {
                    sys::reveal(&item.path);
                    ui.close();
                }
                if ui.button("Delete").clicked() {
                    if sys::recycle(&item.path) {
                        self.library.remove(&item.path);
                        self.core.toast(ToastKind::Shot, "Moved to Recycle Bin", item.name.clone(), None);
                    }
                    ui.close();
                }
            });
            match moved {
                Some(Ok(_)) => self.library.rescan(&s.clips_dir, &s.shots_dir),
                Some(Err(e)) => self.core.toast(ToastKind::Error, "Couldn't move", e, None),
                None => {}
            }
        }

        /// Rename / new category / rename category dialog, drawn over the Library.
        fn lib_dialog_ui(&mut self, ui: &mut Ui, area: Rect, s: &Settings) {
            let Some(dlg) = self.lib_dialog.as_mut() else { return };
            let p = pal();
            let (title, hint_txt, text, err, opened) = match dlg {
                LibDialog::Rename { text, err, opened, .. } => ("Rename", "New name (the file is renamed too)", text, err, *opened),
                LibDialog::NewCategory { text, err, opened, .. } => ("New category", "Name, e.g. Funny, Wins, Fails", text, err, *opened),
                LibDialog::RenameCategory { text, err, opened, .. } => ("Rename category", "New category name", text, err, *opened),
            };
            let pt = ui.painter().clone();
            let full = ui.ctx().content_rect();
            let bg = ui.interact(full, ui.scope_id().with("libdlg-bg"), Sense::click());
            pt.rect_filled(full, CornerRadius::ZERO, alpha(Color32::BLACK, 0.55));
            let r = Rect::from_center_size(area.center() - vec2(0.0, 60.0), vec2(420.0, if err.is_some() { 196.0 } else { 172.0 }));
            pt.rect_filled(r, CornerRadius::same(14), p.panel2);
            pt.rect_stroke(r, CornerRadius::same(14), Stroke::new(1.0, p.line2), StrokeKind::Inside);
            pt.text(r.min + vec2(22.0, 30.0), Align2::LEFT_CENTER, title, semibold(17.0), p.text);
            let mut submit = false;
            let mut cancel = bg.clicked();
            let te_r = Rect::from_min_size(r.min + vec2(22.0, 56.0), vec2(r.width() - 44.0, 40.0));
            pt.rect_filled(te_r, CornerRadius::same(9), p.panel);
            pt.rect_stroke(te_r, CornerRadius::same(9), Stroke::new(1.0, p.accent), StrokeKind::Inside);
            ui.scope_builder(UiBuilder::new().max_rect(te_r.shrink2(vec2(12.0, 9.0))), |ui| {
                let te = ui.add(egui::TextEdit::singleline(text).hint_text(hint_txt).frame(egui::Frame::NONE).desired_width(f32::INFINITY).font(body(14.0)));
                if opened.elapsed() < Duration::from_millis(150) {
                    te.request_focus();
                }
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    cancel = true;
                }
            });
            if let Some(e) = err.as_ref() {
                pt.text(pos2(r.left() + 22.0, te_r.bottom() + 16.0), Align2::LEFT_CENTER, e, body(12.5), p.warn);
            }
            let btn_y = r.bottom() - 42.0;
            ui.scope_builder(UiBuilder::new().max_rect(Rect::from_min_max(pos2(r.left() + 22.0, btn_y), pos2(r.right() - 22.0, btn_y + 30.0))).layout(Layout::right_to_left(Align::Center)), |ui| {
                if cta(ui, "Save").clicked() {
                    submit = true;
                }
                ui.add_space(8.0);
                if button(ui, "Cancel").clicked() {
                    cancel = true;
                }
            });
            if cancel {
                self.lib_dialog = None;
                return;
            }
            if !submit {
                return;
            }
            let bases = [s.clips_dir.as_path(), s.shots_dir.as_path()];
            let res: Result<String, String> = match self.lib_dialog.as_ref().unwrap() {
                LibDialog::Rename { path, text, .. } => library::rename_file(path, text).map(|np| {
                    if self.viewer.as_ref().is_some_and(|v| &v.path == path) {
                        self.viewer = None;
                    }
                    np.file_stem().map(|x| x.to_string_lossy().to_string()).unwrap_or_default()
                }),
                LibDialog::NewCategory { move_item, text, .. } => {
                    let base = if self.lib_tab == Kind::Clip { &s.clips_dir } else { &s.shots_dir };
                    library::create_category(base, text).and_then(|c| match move_item {
                        Some(pth) => library::move_to_category(pth, base, Some(&c)).map(|_| c),
                        None => Ok(c),
                    })
                }
                LibDialog::RenameCategory { old, text, .. } => library::rename_category(&bases, old, text).inspect(|n| {
                    if self.lib_cat.as_deref() == Some(old.as_str()) {
                        self.lib_cat = Some(n.clone());
                    }
                }),
            };
            match res {
                Ok(_) => {
                    self.lib_dialog = None;
                    self.library.rescan(&s.clips_dir, &s.shots_dir);
                }
                Err(e) => match self.lib_dialog.as_mut().unwrap() {
                    LibDialog::Rename { err, .. } | LibDialog::NewCategory { err, .. } | LibDialog::RenameCategory { err, .. } => *err = Some(e),
                },
            }
        }

        // ------------------------------------------------------------------ editor

        fn page_editor(&mut self, ui: &mut Ui, main: Rect) {
            let p = pal();
            let sub = self
                .editor
                .as_ref()
                .map(|e| e.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())
                .unwrap_or_else(|| "Pick a clip from the library".into());
            self.header(ui, main, "Editor", &sub);
            let body_r = Rect::from_min_max(pos2(main.left(), main.top() + 54.0), main.max);
            match self.editor.as_mut() {
                Some(e) => {
                    let mut close = false;
                    ui.scope_builder(UiBuilder::new().max_rect(body_r), |ui| close = e.ui(ui));
                    if close {
                        e.stop();
                        self.go(Page::Library);
                    }
                }
                None => {
                    let r = Rect::from_min_size(body_r.min, vec2(body_r.width(), 260.0));
                    let pt = ui.painter();
                    card(pt, r);
                    let msg = self.editor_err.clone().unwrap_or_else(|| "No clip open".into());
                    draw_icon(pt, Icon::Editor, r.center() - vec2(0.0, 44.0), p.dim, 1.4);
                    pt.text(r.center() - vec2(0.0, 2.0), Align2::CENTER_CENTER, msg, semibold(18.0), p.text);
                    pt.text(r.center() + vec2(0.0, 24.0), Align2::CENTER_CENTER, "Hover a clip in the Library and press Edit.", body(13.5), p.muted);
                }
            }
        }

        // ------------------------------------------------------------------ settings

        fn page_settings(&mut self, ui: &mut Ui, main: Rect, stats: &Stats) {
            let mut s = self.core.settings();
            let before = s.clone();
            self.header(ui, main, "Settings", "Keys, capture, games, audio and looks");

            let mut rebind_ended = false;
            if let Some((which, cap)) = self.rebinding.as_mut() {
                if let Some(res) = cap.poll() {
                    rebind_ended = true;
                    if let Some(k) = res {
                        match which {
                            Which::Clip => s.clip_key = k,
                            Which::Shot => s.shot_key = k,
                        }
                    }
                    self.rebinding = None;
                }
            }
            let body_r = Rect::from_min_max(pos2(main.left(), main.top() + 54.0), main.max + vec2(14.0, 0.0));
            ui.scope_builder(UiBuilder::new().max_rect(body_r), |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    let w = ui.available_width() - 14.0;
                    self.settings_body(ui, w, &mut s, stats);
                    ui.add_space(30.0);
                });
            });
            if s != before {
                self.core.apply(s);
            }
            if rebind_ended {
                crate::hotkeys::suspend(false);
            }
        }

        fn settings_body(&mut self, ui: &mut Ui, w: f32, s: &mut Settings, stats: &Stats) {
            let p = pal();
            // ---------------- appearance first: it's the fun one
            panel(ui, w, Icon::Palette, "Appearance", "Theme", |ui, w| {
                let n = ThemeId::ALL.len() as f32;
                let gap = 14.0;
                let cw = (w - 44.0 - gap * (n - 1.0)) / n;
                ui.horizontal(|ui| {
                    ui.add_space(22.0);
                    ui.spacing_mut().item_spacing.x = gap;
                    for t in ThemeId::ALL {
                        if theme_card(ui, t, s.theme == t, cw).clicked() {
                            s.theme = t;
                        }
                    }
                });
                ui.add_space(18.0);
            });

            panel(ui, w, Icon::Key, "Hotkeys", "Works in fullscreen games", |ui, w| {
                let listening = self.rebinding.as_ref().map(|r| r.0);
                let mut start = None;
                row(ui, w, "Save clip", "Click to rebind. Esc cancels. Any key works, even PrtSc or F13.", false, |ui| {
                    if keycaps(ui, &s.clip_key.parts(), listening == Some(Which::Clip)).clicked() {
                        start = Some(Which::Clip);
                    }
                });
                row(ui, w, "Win+Shift+S opens Ferrocap Snip", "Replaces Windows' Snipping Tool: region, window or screen, with drawing tools.", false, |ui| {
                    toggle(ui, &mut s.snip_override);
                });
                row(ui, w, "Win+Shift+S: copy only", "Skip the editor: the snip goes straight to your clipboard.", false, |ui| {
                    toggle(ui, &mut s.snip_copy_only);
                });
                row(ui, w, "Edit after F9 screenshots", "Opens each F9 screenshot in the editor. Off is better mid-game.", false, |ui| {
                    toggle(ui, &mut s.shot_editor);
                });
                row(ui, w, "Screenshot", "Lossless PNG, saved and copied to the clipboard.", true, |ui| {
                    if keycaps(ui, &s.shot_key.parts(), listening == Some(Which::Shot)).clicked() {
                        start = Some(Which::Shot);
                    }
                });
                if let Some(wh) = start {
                    crate::hotkeys::suspend(true);
                    self.rebinding = Some((wh, sys::KeyCapture::new()));
                }
                if let Some(e) = self.core.hotkey_error.lock().unwrap().clone() {
                    ui.horizontal(|ui| {
                        ui.add_space(22.0);
                        ui.label(egui::RichText::new(format!("{e}. Pick a different key.")).font(body(13.0)).color(p.warn));
                    });
                    ui.add_space(10.0);
                }
            });

            panel(ui, w, Icon::Stream, "Capture", "GPU only, never touches the CPU", |ui, w| {
                let mons = sys::monitors();
                let mon = mons.get(s.monitor as usize).copied().unwrap_or(mons[0]);
                let est = s.buffer_bytes(mon.w as u32, mon.h as u32);
                row(ui, w, "Buffer length", &format!("About {} of RAM. Nothing is written to disk until you save.", sys::human_bytes(est)), false, |ui| {
                    // fixed-width value cell, so the slider never shifts while you drag it
                    let (vr, _) = ui.allocate_exact_size(vec2(84.0, 28.0), Sense::hover());
                    ui.painter().text(
                        vr.right_center(),
                        Align2::RIGHT_CENTER,
                        secs_short(s.buffer_secs),
                        display(22.0),
                        if est > 2_000_000_000 { p.warn } else { p.text },
                    );
                    let mut idx = BUFFER_STEPS.iter().position(|&v| v == s.buffer_secs).unwrap_or(4);
                    if step_slider(ui, &mut idx, BUFFER_STEPS.len(), 300.0) {
                        s.buffer_secs = BUFFER_STEPS[idx];
                    }
                });
                let kb = s.video_kbps(mon.w as u32, mon.h as u32);
                row(ui, w, "Quality", &format!("H.264 at {:.0} Mb/s for {}×{}.", kb as f32 / 1000.0, mon.w, mon.h), false, |ui| {
                    let opts: Vec<(Quality, &str)> = Quality::ALL.iter().map(|q| (*q, q.name())).collect();
                    segmented(ui, &mut s.quality, &opts, 82.0);
                });
                row(ui, w, "Frame rate", "Higher is smoother. 240 gives the best motion blur (needs a strong GPU).", false, |ui| {
                    let labels: Vec<String> = FPS_STEPS.iter().map(|f| f.to_string()).collect();
                    let opts: Vec<(u32, &str)> = FPS_STEPS.iter().zip(&labels).map(|(f, l)| (*f, l.as_str())).collect();
                    segmented(ui, &mut s.fps, &opts, 54.0);
                });
                if mons.len() > 1 {
                    row(ui, w, "Display", "Which monitor to record in display mode.", false, |ui| {
                        let labels: Vec<String> = (0..mons.len()).map(|i| format!("{}", i + 1)).collect();
                        let opts: Vec<(u32, &str)> = labels.iter().enumerate().map(|(i, l)| (i as u32, l.as_str())).collect();
                        segmented(ui, &mut s.monitor, &opts, 48.0);
                    });
                }
                row(ui, w, "Stretch to 16:9", "For stretched resolutions like 1440×1080: clips play back stretched, just like in game. Lossless.", false, |ui| {
                    toggle(ui, &mut s.stretch_169);
                });
                row(ui, w, "Record mouse cursor", "Hide it if your game draws its own crosshair.", true, |ui| {
                    toggle(ui, &mut s.show_cursor);
                });
            });

            let det = procs::state().lock().unwrap().clone();
            panel(ui, w, Icon::Gamepad, "Games", &det.game.as_ref().map(|g| format!("Detected: {}", g.name)).unwrap_or_else(|| "No game running".into()), |ui, w| {
                row(ui, w, "Capture", "Auto clips the game's own window (works in fullscreen), and the whole screen when no game runs.", false, |ui| {
                    segmented(ui, &mut s.source, &[(Source::Game, "Auto"), (Source::Display, "Whole screen")], 120.0);
                });
                row(ui, w, "Only buffer while a game is running", "Zero work while you're on the desktop. Starts when Roblox, Minecraft & co. open.", false, |ui| {
                    toggle(ui, &mut s.game_only);
                });
                row(ui, w, "Add a game", "Not detected? Pick its window here. Fullscreen apps are detected automatically too.", s.extra_games.is_empty(), |ui| {
                    let mut pick: Option<String> = None;
                    egui::ComboBox::from_id_salt("addgame")
                        .selected_text(egui::RichText::new("Choose a running app").font(body(13.0)))
                        .width(250.0)
                        .show_ui(ui, |ui| {
                            for a in &det.apps {
                                let label = format!("{}  ·  {}", procs::pretty_exe(&a.exe), truncate(ui, &a.title, &body(12.0), 160.0));
                                if ui.selectable_label(false, label).clicked() {
                                    pick = Some(a.exe.clone());
                                }
                            }
                        });
                    if let Some(e) = pick {
                        if !s.extra_games.contains(&e) {
                            s.extra_games.push(e);
                        }
                    }
                });
                let mut remove = None;
                let n = s.extra_games.len();
                for (i, g) in s.extra_games.iter().enumerate() {
                    row(ui, w, &procs::pretty_exe(g), g, i + 1 == n, |ui| {
                        if button(ui, "Remove").clicked() {
                            remove = Some(i);
                        }
                    });
                }
                if let Some(i) = remove {
                    s.extra_games.remove(i);
                }
            });

            panel(ui, w, Icon::Audio, "Audio", "Every source on its own track", |ui, w| {
                row(ui, w, "System audio", "Everything you hear. Saved as the main track.", false, |ui| {
                    toggle(ui, &mut s.desktop_audio);
                });
                row(ui, w, "Microphone", "Your voice, on its own track. With system audio on, track 1 becomes a Mix of both.", false, |ui| {
                    toggle(ui, &mut s.mic);
                });
                if s.mic {
                    let mics = self.mics.get_or_insert_with(crate::audio::mic_devices).clone();
                    row(ui, w, "Mic device", "Which input to record.", false, |ui| {
                        let sel = s.mic_device.clone().unwrap_or_else(|| "Default microphone".into());
                        egui::ComboBox::from_id_salt("mic")
                            .selected_text(egui::RichText::new(truncate(ui, &sel, &body(13.0), 230.0)).font(body(13.0)))
                            .width(260.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut s.mic_device, None, "Default microphone");
                                for m in &mics {
                                    ui.selectable_value(&mut s.mic_device, Some(m.clone()), m);
                                }
                            });
                    });
                }
                ui.horizontal(|ui| {
                    ui.add_space(22.0);
                    ui.label(
                        egui::RichText::new("APP TRACKS · mute any of these later in the editor")
                            .font(mono(11.0))
                            .color(p.dim),
                    );
                });
                ui.add_space(4.0);
                let mut remove = None;
                let live: Vec<(String, bool)> = stats.info.levels.iter().map(|l| (l.name.clone(), l.live.load(Ordering::Relaxed))).collect();
                for (i, a) in s.app_tracks.iter_mut().enumerate() {
                    let running = live.iter().any(|(n, l)| n == &a.label && *l);
                    let desc = if a.exe == GAME {
                        "Whatever game Ferrocap detects (Roblox, Minecraft, VALORANT…)".to_string()
                    } else {
                        format!("{}{}", a.exe, if running { " · running" } else { "" })
                    };
                    let is_game = a.exe == GAME;
                    row(ui, w, &a.label, &desc, false, |ui| {
                        toggle(ui, &mut a.enabled);
                        if !is_game {
                            ui.add_space(8.0);
                            if link(ui, "Remove", p.dim).clicked() {
                                remove = Some(i);
                            }
                        }
                    });
                }
                if let Some(i) = remove {
                    s.app_tracks.remove(i);
                }
                row(ui, w, "Add an app track", "Spotify, a browser, a voice app… picks up its sound only.", true, |ui| {
                    let mut pick: Option<String> = None;
                    egui::ComboBox::from_id_salt("addapp")
                        .selected_text(egui::RichText::new("Choose a running app").font(body(13.0)))
                        .width(250.0)
                        .show_ui(ui, |ui| {
                            for a in &det.apps {
                                if ui.selectable_label(false, procs::pretty_exe(&a.exe)).clicked() {
                                    pick = Some(a.exe.clone());
                                }
                            }
                        });
                    if let Some(e) = pick {
                        if !s.app_tracks.iter().any(|t| t.exe == e) {
                            s.app_tracks.push(AppTrack { label: procs::pretty_exe(&e), exe: e, enabled: true });
                        }
                    }
                });
            });

            panel(ui, w, Icon::Chip, "Encoder", "Works on NVIDIA, AMD and Intel", |ui, w| {
                let det_enc = s.encoder.clone().unwrap_or_default();
                row(ui, w, "Video encoder", &format!("Auto picked {}. Force one if your GPU misbehaves.", encoder_label(&det_enc)), false, |ui| {
                    segmented(
                        ui,
                        &mut s.encoder_pref,
                        &[
                            (EncoderPref::Auto, "Auto"),
                            (EncoderPref::Nvenc, "NVENC"),
                            (EncoderPref::Amf, "AMF"),
                            (EncoderPref::Qsv, "QSV"),
                            (EncoderPref::Cpu, "CPU"),
                        ],
                        62.0,
                    );
                });
                row(ui, w, "Re-detect", "Run the GPU check again, e.g. after a driver update or new GPU.", true, |ui| {
                    if button(ui, "Re-detect now").clicked() {
                        s.encoder = None;
                    }
                });
            });

            panel(ui, w, Icon::Folder, "Saving & editing", "Where things go", |ui, w| {
                for (title, is_clips) in [("Clips folder", true), ("Screenshots folder", false)] {
                    let dir = if is_clips { s.clips_dir.clone() } else { s.shots_dir.clone() };
                    row(ui, w, title, &dir.display().to_string(), false, |ui| {
                        if button(ui, "Browse").clicked() {
                            if let Some(d) = rfd::FileDialog::new().set_directory(&dir).pick_folder() {
                                if is_clips { s.clips_dir = d } else { s.shots_dir = d }
                            }
                        }
                        if button(ui, "Open").clicked() {
                            let _ = std::fs::create_dir_all(&dir);
                            sys::open_path(&dir);
                        }
                    });
                }
                row(ui, w, "Screenshot area", "Whole monitor, or just the window you're in.", false, |ui| {
                    segmented(ui, &mut s.shot_mode, &[(ShotMode::Screen, "Screen"), (ShotMode::Window, "Window")], 84.0);
                });
                row(ui, w, "Copy screenshots to clipboard", "Paste straight into Discord.", false, |ui| {
                    toggle(ui, &mut s.copy_shot);
                });
                row(ui, w, "Frame blending", "Motion blur from the frames in between. Light stays sharp, Strong is cinematic.", false, |ui| {
                    segmented(ui, &mut s.blend, &[(Blend::Off, "Off"), (Blend::Light, "Light"), (Blend::Strong, "Strong")], 70.0);
                });
                row(ui, w, "Blend output", "Original keeps your recording's smoothness. 60 / 30 trade smoothness for a film look.", false, |ui| {
                    segmented(ui, &mut s.blend_fps, &[(0, "Original"), (60, "60 fps"), (30, "30 fps")], 70.0);
                });
                row(ui, w, "Blend every saved clip", "Re-encodes each clip right after saving, in the background at low priority.", true, |ui| {
                    toggle(ui, &mut s.blend_on_save);
                });
            });

            panel(ui, w, Icon::Settings, "General", &format!("Ferrocap {}", crate::release::VERSION), |ui, w| {
                let st = crate::update::status();
                let desc = match &st {
                    crate::update::Status::Idle => format!("You're on version {}.", crate::release::VERSION),
                    crate::update::Status::Checking => "Checking…".to_string(),
                    crate::update::Status::UpToDate => format!("You're up to date (version {}).", crate::release::VERSION),
                    crate::update::Status::Available { version, .. } => format!("Version {version} is available. You have {}.", crate::release::VERSION),
                    crate::update::Status::Downloading(b) => format!("Downloading the update… {:.1} MB", *b as f64 / 1e6),
                    crate::update::Status::Error(e) => e.clone(),
                };
                row(ui, w, "Updates", &desc, false, |ui| match &st {
                    crate::update::Status::Available { .. } => {
                        if primary(ui, "Update now", 130.0).clicked() {
                            crate::update::install();
                        }
                    }
                    crate::update::Status::Checking | crate::update::Status::Downloading(_) => {
                        ui.add(egui::Spinner::new().size(18.0).color(p.accent));
                    }
                    _ => {
                        if button(ui, "Check for updates").clicked() {
                            crate::update::check(true);
                        }
                    }
                });
                row(ui, w, "Start with Windows", "Launches quietly into the tray.", false, |ui| {
                    toggle(ui, &mut s.start_with_windows);
                });
                row(ui, w, "Start minimized", "When started with Windows, don't open this window.", false, |ui| {
                    toggle(ui, &mut s.start_minimized);
                });
                let status = crate::discord::status();
                row(ui, w, "Discord status", &format!("Shows \"Clipping Roblox\" (or whatever you play) on your profile. {status}."), false, |ui| {
                    toggle(ui, &mut s.discord_rpc);
                });
                if crate::discord::BUILTIN_APP_ID.is_empty() {
                row(ui, w, "Discord Application ID", "Make an app called Ferrocap at discord.com/developers, add the logo as \"logo\", paste its ID.", false, |ui| {
                    if button(ui, "Open portal").clicked() {
                        sys::open_path(std::path::Path::new("https://discord.com/developers/applications"));
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut s.discord_app_id)
                            .hint_text("Application ID")
                            .desired_width(190.0)
                            .font(body(13.0)),
                    );
                });
                }
                row(ui, w, "Sound on F8", "A short chirp the moment you press it, so you know it caught the moment.", false, |ui| {
                    toggle(ui, &mut s.sound);
                });
                row(ui, w, "Saving badge", "A small badge in the corner while the clip saves. Never takes focus from your game.", true, |ui| {
                    toggle(ui, &mut s.save_hud);
                });
            });
        }

        // ------------------------------------------------------------------ tray menu

        fn tray_menu(&mut self, ctx: &egui::Context) {
            let Some(m) = self.menu.as_ref() else { return };
            let s = self.core.settings();
            let stats = self.core.engine.stats();
            let items: Vec<(&str, String, u8)> = vec![
                ("Open Ferrocap", String::new(), 0),
                ("Save clip", s.clip_key.label(), 1),
                ("Screenshot", s.shot_key.label(), 2),
                ("Snip", if s.snip_override { "Win+Shift+S".to_string() } else { String::new() }, 8),
                (if s.paused { "Resume buffering" } else { "Pause buffering" }, String::new(), 3),
                ("-", String::new(), 255),
                ("Library", String::new(), 4),
                ("Open clips folder", String::new(), 5),
                ("Settings", String::new(), 6),
                ("-", String::new(), 255),
                ("Quit Ferrocap", String::new(), 7),
            ];
            let w = 280.0;
            let h = 8.0 + items.iter().map(|i| if i.2 == 255 { 11.0 } else { 36.0 }).sum::<f32>() + 10.0;
            let pos = pos2((m.pos.x - w + 12.0).max(4.0), (m.pos.y - h - 12.0).max(4.0));
            let first = m.opened.elapsed() < Duration::from_millis(60);
            let mut action: Option<u8> = None;
            let mut close = false;
            let builder = egui::ViewportBuilder::default()
                .with_title("Ferrocap menu")
                .with_decorations(false)
                .with_resizable(false)
                .with_always_on_top()
                .with_taskbar(false)
                .with_active(true)
                .with_inner_size([w, h])
                .with_position(pos);
            let _status = match &stats.state {
                State::Recording => {
                    let t = stats.held_secs as u32;
                    (format!("REC {:02}:{:02}:{:02} · {}", t / 3600, t / 60 % 60, t % 60, encoder_label(&stats.info.encoder)), pal().warn)
                }
                State::Paused => ("PAUSED".to_string(), pal().dim),
                State::WaitingForGame => ("WAITING FOR A GAME".to_string(), pal().accent2),
                _ => ("STARTING".to_string(), pal().accent2),
            };
            let mut focused_now = None;
            ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("ferrocap-tray-menu"), builder, |ui, _| {
                let p = pal();
                if first {
                    ui.ctx().send_viewport_cmd(ViewportCommand::Focus);
                }
                focused_now = ui.input(|i| i.viewport().focused);
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
                let r = ui.max_rect();
                let pt = ui.painter().clone();
                pt.rect_filled(r, CornerRadius::ZERO, Color32::from_rgb(6, 5, 5));
                pt.rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.0, p.line2), StrokeKind::Inside);
                // items
                let mut y = r.top() + 8.0;
                for (label, key, id) in &items {
                    if *id == 255 {
                        pt.line_segment([pos2(r.left() + 14.0, y + 5.0), pos2(r.right() - 14.0, y + 5.0)], Stroke::new(1.0, p.line));
                        y += 11.0;
                        continue;
                    }
                    let ir = Rect::from_min_size(pos2(r.left() + 6.0, y), vec2(r.width() - 12.0, 34.0));
                    let resp = ui.interact(ir, ui.scope_id().with(("tm", *id)), Sense::click());
                    let quit = *id == 7;
                    if resp.hovered() {
                        pt.rect_filled(ir, CornerRadius::same(3), p.panel2);
                        pt.rect_filled(Rect::from_min_size(ir.min, vec2(2.0, ir.height())), CornerRadius::ZERO, if quit { p.warn } else { p.accent });
                    }
                    let col = if resp.hovered() { if quit { p.warn } else { p.text } } else { p.muted };
                    pt.text(pos2(ir.left() + 16.0, ir.center().y), Align2::LEFT_CENTER, *label, body(14.0), col);
                    if !key.is_empty() {
                        let g = pt.layout_no_wrap(key.clone(), mono_bold(11.0), p.text);
                        let kr = Rect::from_min_size(pos2(ir.right() - 12.0 - g.size().x - 14.0, ir.center().y - 10.0), vec2(g.size().x + 14.0, 20.0));
                        pt.rect_filled(kr.translate(vec2(0.0, 2.0)), CornerRadius::same(3), Color32::BLACK);
                        pt.rect_filled(kr, CornerRadius::same(3), p.panel2);
                        pt.rect_stroke(kr, CornerRadius::same(3), Stroke::new(1.0, p.line2), StrokeKind::Inside);
                        pt.galley(kr.min + vec2(7.0, 10.0 - g.size().y / 2.0), g, p.text);
                    }
                    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        action = Some(*id);
                    }
                    y += 36.0;
                }
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            });
            let m = self.menu.as_mut().unwrap();
            if focused_now == Some(true) {
                m.seen_focus = true;
            }
            let lost_focus = m.seen_focus && focused_now == Some(false);
            let stale = !m.seen_focus && m.opened.elapsed() > Duration::from_secs(8);
            if let Some(a) = action {
                close = true;
                let c = self.core;
                match a {
                    0 => c.request_show(),
                    1 => c.save_clip(),
                    2 => c.screenshot(),
                    3 => c.toggle_pause(),
                    4 => {
                        *c.goto.lock().unwrap() = Some(1);
                        c.request_show();
                    }
                    5 => {
                        let d = s.clips_dir.clone();
                        let _ = std::fs::create_dir_all(&d);
                        sys::open_path(&d);
                    }
                    6 => {
                        *c.goto.lock().unwrap() = Some(3);
                        c.request_show();
                    }
                    8 => crate::snip::trigger(200, false),
                    _ => c.request_quit(),
                }
            }
            if close || lost_focus || stale {
                self.menu = None;
            }
        }

        // ------------------------------------------------------------------ toasts

        fn toasts(&mut self, ctx: &egui::Context) {
            let p = pal();
            let mut toasts = self.core.toasts.lock().unwrap();
            toasts.retain(|t| t.born.elapsed() < Duration::from_millis(6500));
            if toasts.is_empty() {
                return;
            }
            let list = toasts.clone();
            drop(toasts);
            egui::Area::new(egui::Id::unique("ferrocap-toasts"))
                .anchor(Align2::RIGHT_BOTTOM, vec2(-22.0, -22.0))
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    for t in list.iter() {
                        let age = t.born.elapsed().as_secs_f32();
                        ui.set_opacity(((6.5 - age) / 0.5).clamp(0.0, 1.0) * (age / 0.15).clamp(0.0, 1.0));
                        let accent = match t.kind {
                            ToastKind::Clip => p.accent,
                            ToastKind::Shot => p.accent2,
                            ToastKind::Error => p.warn,
                        };
                        egui::Frame::new()
                            .fill(p.panel2)
                            .stroke(Stroke::new(1.0, p.line2))
                            .corner_radius(CornerRadius::same(12))
                            .inner_margin(egui::Margin { left: 18, right: 16, top: 12, bottom: 12 })
                            .show(ui, |ui| {
                                ui.set_width(340.0);
                                let r = ui.max_rect();
                                ui.painter().rect_filled(
                                    Rect::from_min_size(pos2(r.left() - 18.0, r.top() - 12.0), vec2(2.0, r.height() + 24.0)),
                                    CornerRadius::ZERO,
                                    accent,
                                );
                                ui.label(egui::RichText::new(&t.title).font(semibold(15.0)).color(p.text));
                                let detail = truncate(ui, &t.detail, &body(12.5), 330.0);
                                ui.label(egui::RichText::new(detail).font(body(12.5)).color(p.muted));
                                if let Some(path) = &t.path {
                                    ui.horizontal(|ui| {
                                        if link(ui, if t.kind == ToastKind::Clip { "Play" } else { "Open" }, accent).clicked() {
                                            self.view_req = Some(path.clone());
                                            self.core.request_show();
                                        }
                                        if t.kind == ToastKind::Clip && path.extension().map(|e| e == "mp4").unwrap_or(false) {
                                            if link(ui, "Edit", p.text).clicked() {
                                                *self.core.open_in_editor.lock().unwrap() = Some(path.clone());
                                                self.core.request_show();
                                            }
                                        }
                                        if link(ui, "Show in folder", p.muted).clicked() {
                                            sys::reveal(path);
                                        }
                                    });
                                }
                            });
                        ui.add_space(8.0);
                    }
                });
            ctx.request_repaint_after(Duration::from_millis(33));
        }
    }

    // ---------------------------------------------------------------------- helpers

    fn fmt_clock(t: f64) -> String {
        let t = t.max(0.0) as u32;
        format!("{}:{:02}", t / 60, t % 60)
    }

    fn secs_short(s: u32) -> String {
        if s >= 60 && s % 60 == 0 {
            format!("{} min", s / 60)
        } else if s > 60 {
            format!("{}m {}s", s / 60, s % 60)
        } else {
            format!("{s} s")
        }
    }

    /// "Roblox 2026-11-19 21-04-33" -> ("Roblox", "2026-11-19 21-04-33")
    /// "Ferrocap_Clip_2026-11-19_21-04-33" -> ("Clip", "Nov 19 · 21:04")
    /// Rename / category dialogs in the Library.
    enum LibDialog {
        Rename { path: std::path::PathBuf, text: String, err: Option<String>, opened: Instant },
        NewCategory { move_item: Option<std::path::PathBuf>, text: String, err: Option<String>, opened: Instant },
        RenameCategory { old: String, text: String, err: Option<String>, opened: Instant },
    }

    /// A pill-shaped filter chip for the Library's category bar.
    fn category_chip(ui: &mut Ui, label: &str, on: bool) -> egui::Response {
        let p = pal();
        let g = ui.painter().layout_no_wrap(label.to_string(), semibold(12.5), p.text);
        let (r, resp) = ui.allocate_exact_size(vec2(g.size().x + 26.0, 30.0), Sense::click());
        let hv = resp.hovered();
        let (fill, fg, line) = if on { (p.text, p.bg, p.text) } else if hv { (p.panel3, p.text, p.line2) } else { (p.panel, p.muted, p.line) };
        ui.painter().rect_filled(r, CornerRadius::same(15), fill);
        ui.painter().rect_stroke(r, CornerRadius::same(15), Stroke::new(1.0, line), StrokeKind::Inside);
        ui.painter().text(r.center(), Align2::CENTER_CENTER, label, semibold(12.5), fg);
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    /// Display title for a library item: the user's own name when they renamed it, otherwise the game
    /// (or "Desktop" / "Screenshot").
    fn item_title(item: &library::Item, game: Option<String>) -> String {
        if !item.name.starts_with("Ferrocap_") {
            return item.name.clone();
        }
        let (kind, _) = split_name(&item.name);
        game.unwrap_or_else(|| if item.kind == Kind::Shot { "Screenshot".into() } else if kind == "Edit" { "Edit".into() } else { "Desktop".into() })
    }

    fn split_name(name: &str) -> (String, String) {
        let parts: Vec<&str> = name.split('_').collect();
        if parts.len() >= 4 && parts[0] == "Ferrocap" {
            let d: Vec<&str> = parts[2].split('-').collect();
            let t: Vec<&str> = parts[3].split('-').collect();
            if d.len() == 3 && t.len() >= 2 {
                const M: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
                let mi: usize = d[1].parse().unwrap_or(1);
                let day: u32 = d[2].parse().unwrap_or(1);
                return (parts[1].to_string(), format!("{} {} · {}:{}", M[(mi.max(1) - 1) % 12], day, t[0], t[1]));
            }
        }
        (name.to_string(), String::new())
    }

    fn panel(ui: &mut Ui, w: f32, icon: Icon, title: &str, right: &str, body_fn: impl FnOnce(&mut Ui, f32)) {
        let p = pal();
        let top = ui.cursor().top();
        let left = ui.cursor().left();
        // reserve the background now, fill it in once we know the height
        let bg = ui.painter().add(egui::Shape::Noop);
        let border = ui.painter().add(egui::Shape::Noop);
        let (hr, _) = ui.allocate_exact_size(vec2(w, 62.0), Sense::hover());
        let pt = ui.painter().clone();
        icon_box(&pt, Rect::from_min_size(hr.min + vec2(18.0, 15.0), vec2(32.0, 32.0)), icon, p.text);
        pt.text(pos2(hr.left() + 64.0, hr.top() + 31.0), Align2::LEFT_CENTER, title, semibold(16.5), p.text);
        pt.text(pos2(hr.right() - 20.0, hr.top() + 31.0), Align2::RIGHT_CENTER, right, body(12.5), p.muted);
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            body_fn(ui, w);
        });
        ui.add_space(6.0);
        let bottom = ui.cursor().top();
        let r = Rect::from_min_max(pos2(left, top), pos2(left + w, bottom));
        pt.set(bg, egui::Shape::rect_filled(r, CornerRadius::same(14), p.panel));
        pt.set(border, egui::Shape::rect_stroke(r, CornerRadius::same(14), Stroke::new(1.0, p.line), StrokeKind::Inside));
        ui.add_space(16.0);
    }

    fn row(ui: &mut Ui, w: f32, title: &str, desc: &str, last: bool, control: impl FnOnce(&mut Ui)) {
        let p = pal();
        let h = 66.0;
        let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
        let pt = ui.painter();
        pt.line_segment([pos2(rect.left() + 20.0, rect.top()), pos2(rect.right() - 20.0, rect.top())], Stroke::new(1.0, p.line));
        pt.text(pos2(rect.left() + 20.0, rect.top() + 23.0), Align2::LEFT_CENTER, title, body(14.5), p.text);
        let d = truncate(ui, desc, &body(12.5), (rect.width() * 0.5).max(220.0));
        pt.text(pos2(rect.left() + 20.0, rect.top() + 44.0), Align2::LEFT_CENTER, d, body(12.5), p.muted);
        let _ = last;
        let ctrl = Rect::from_min_max(pos2(rect.left() + rect.width() * 0.42, rect.top()), pos2(rect.right() - 20.0, rect.bottom()));
        ui.scope_builder(UiBuilder::new().max_rect(ctrl).layout(Layout::right_to_left(Align::Center)), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            control(ui)
        });
    }

    fn theme_card(ui: &mut Ui, t: ThemeId, on: bool, w: f32) -> egui::Response {
        let cur = pal();
        let tp = palette(t);
        let (r, resp) = ui.allocate_exact_size(vec2(w, 110.0), Sense::click());
        let pt = ui.painter();
        pt.rect_filled(r, CornerRadius::same(12), tp.bg2);
        // mini dashboard: a card, a line, a chip
        let inner = r.shrink(10.0);
        let mini = Rect::from_min_size(inner.min, vec2(inner.width(), 58.0));
        pt.rect_filled(mini, CornerRadius::same(8), tp.panel);
        pt.rect_stroke(mini, CornerRadius::same(8), Stroke::new(1.0, tp.line), StrokeKind::Inside);
        let mut prev = None;
        for i in 0..=24 {
            let u = i as f32 / 24.0;
            let x = mini.left() + 8.0 + (mini.width() - 16.0) * u;
            let y = mini.bottom() - 12.0 - u.powf(1.4) * 26.0 + ((u * 40.0).sin() * 3.0);
            let q = pos2(x, y);
            if let Some(pr) = prev {
                pt.line_segment([pr, q], Stroke::new(1.3, if u > 0.72 { tp.accent2 } else { alpha(tp.text, 0.7) }));
            }
            prev = Some(q);
        }
        pt.rect_filled(Rect::from_min_size(mini.min + vec2(8.0, 8.0), vec2(26.0, 6.0)), CornerRadius::same(2), tp.line2);
        pt.text(pos2(inner.left() + 2.0, inner.bottom() - 10.0), Align2::LEFT_CENTER, t.name(), semibold(14.0), tp.text);
        pt.rect_filled(Rect::from_center_size(pos2(inner.right() - 8.0, inner.bottom() - 10.0), vec2(14.0, 14.0)), CornerRadius::same(4), tp.accent);
        let stroke = if on { Stroke::new(2.0, cur.text) } else if resp.hovered() { Stroke::new(1.0, cur.muted) } else { Stroke::new(1.0, cur.line2) };
        pt.rect_stroke(r, CornerRadius::same(12), stroke, StrokeKind::Inside);
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }
}

// ================================================================================================
// discord
// ================================================================================================

mod discord {
    //! Discord Rich Presence over Discord's local IPC pipe (no SDK, no extra dependency).
    //! Shows "Clipping <game>" while a game is detected, "Clipping my screen" otherwise.

    use std::fs::File;
    use std::io::{Read, Write};
    use std::sync::Mutex;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use crate::{app, procs};

    /// Ferrocap's own Discord application. Built in, so users never have to set anything up.
    pub const BUILTIN_APP_ID: &str = "1572913468401647698";

    static STATUS: Mutex<&'static str> = Mutex::new("Off");

    pub fn status() -> &'static str {
        *STATUS.lock().unwrap()
    }

    fn set_status(s: &'static str) {
        *STATUS.lock().unwrap() = s;
    }

    fn connect() -> Option<File> {
        for i in 0..10 {
            if let Ok(f) = std::fs::OpenOptions::new().read(true).write(true).open(format!(r"\\.\pipe\discord-ipc-{i}")) {
                return Some(f);
            }
        }
        None
    }

    fn send(f: &mut File, op: u32, payload: &serde_json::Value) -> std::io::Result<()> {
        let body = payload.to_string();
        let mut buf = Vec::with_capacity(8 + body.len());
        buf.extend_from_slice(&op.to_le_bytes());
        buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
        buf.extend_from_slice(body.as_bytes());
        f.write_all(&buf)
    }

    fn recv(f: &mut File) -> std::io::Result<(u32, serde_json::Value)> {
        let mut h = [0u8; 8];
        f.read_exact(&mut h)?;
        let op = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
        let len = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as usize;
        let mut b = vec![0u8; len.min(1 << 20)];
        f.read_exact(&mut b)?;
        Ok((op, serde_json::from_slice(&b).unwrap_or(serde_json::Value::Null)))
    }

    // ---------------------------------------------------------------- game icons
    //
    // Discord publishes the games it can detect, with each game's exe names and official icon. Ferrocap
    // fetches that list at most once a week (only while Rich Presence is on), keeps just the exe/name ->
    // icon pairs in %LOCALAPPDATA%\Ferrocap\discord-icons.json, and shows the game's icon as the small
    // image in the corner of Ferrocap's status.

    const DETECTABLE_URL: &str = "https://discord.com/api/v9/applications/detectable";
    /// The current app icon, straight from the repo, so the status always matches the exe's icon.
    const LOGO_URL: &str = "https://raw.githubusercontent.com/maykha3v/ferrocap-releases/main/assets/icon-rounded.png";

    #[derive(Default, serde::Serialize, serde::Deserialize)]
    struct IconIndex {
        /// exe file name (lowercase) -> icon URL
        exes: std::collections::HashMap<String, String>,
        /// game name (lowercase) -> icon URL
        names: std::collections::HashMap<String, String>,
    }

    static ICONS: Mutex<Option<IconIndex>> = Mutex::new(None);
    static ICONS_LOADING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    fn icons_path() -> std::path::PathBuf {
        crate::settings::cache_dir().join("discord-icons.json")
    }

    fn build_index(raw: &[u8]) -> Option<IconIndex> {
        let list: Vec<serde_json::Value> = serde_json::from_slice(raw).ok()?;
        let mut idx = IconIndex::default();
        let mut ambiguous = std::collections::HashSet::new();
        for app in &list {
            let (Some(id), Some(hash)) = (app["id"].as_str(), app["icon_hash"].as_str()) else { continue };
            let url = format!("https://cdn.discordapp.com/app-icons/{id}/{hash}.png?size=128");
            if let Some(name) = app["name"].as_str() {
                idx.names.entry(name.to_lowercase()).or_insert_with(|| url.clone());
            }
            for e in app["executables"].as_array().into_iter().flatten() {
                if e["os"].as_str() != Some("win32") {
                    continue;
                }
                let Some(n) = e["name"].as_str() else { continue };
                if n.starts_with('>') {
                    continue; // generic hosts like javaw.exe
                }
                let file = n.rsplit(['/', '\\']).next().unwrap_or(n).to_lowercase();
                match idx.exes.get(&file) {
                    Some(u) if *u != url => {
                        ambiguous.insert(file);
                    }
                    _ => {
                        idx.exes.insert(file, url.clone());
                    }
                }
            }
        }
        for f in ambiguous {
            idx.exes.remove(&f); // several games ship an exe with this name: match by game name instead
        }
        Some(idx)
    }

    /// Loads the cached index, and refreshes it in the background when it's missing or a week old.
    fn ensure_icons() {
        let path = icons_path();
        if ICONS.lock().unwrap().is_none() {
            if let Some(idx) = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<IconIndex>(&b).ok()) {
                *ICONS.lock().unwrap() = Some(idx);
            }
        }
        let fresh = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().unwrap_or_default() < Duration::from_secs(7 * 24 * 3600))
            .unwrap_or(false);
        static LAST_TRY: Mutex<Option<Instant>> = Mutex::new(None);
        if fresh || LAST_TRY.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_secs(3600)) {
            return;
        }
        if ICONS_LOADING.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        *LAST_TRY.lock().unwrap() = Some(Instant::now());
        std::thread::spawn(move || {
            let tmp = path.with_extension("download");
            let ok = crate::sys::quiet("curl.exe")
                .args(["-L", "--fail", "--silent", "--compressed", "--max-time", "60", "-o"])
                .arg(&tmp)
                .arg(DETECTABLE_URL)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if let Some(idx) = ok.then(|| std::fs::read(&tmp).ok()).flatten().and_then(|b| build_index(&b)) {
                if let Ok(json) = serde_json::to_vec(&idx) {
                    let _ = std::fs::write(&path, json);
                }
                *ICONS.lock().unwrap() = Some(idx);
            } else if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&path) {
                let _ = f.set_modified(SystemTime::now()); // keep the old list, try again in a week
            }
            let _ = std::fs::remove_file(&tmp);
            ICONS_LOADING.store(false, std::sync::atomic::Ordering::SeqCst);
        });
    }

    fn game_icon(exe: Option<&str>, name: &str) -> Option<String> {
        ensure_icons();
        let url = {
            let icons = ICONS.lock().unwrap();
            let idx = icons.as_ref()?;
            exe.and_then(|e| idx.exes.get(e)).or_else(|| idx.names.get(&name.to_lowercase())).cloned()?
        };
        // icons drawn on a transparent background lose their corners to Discord's round crop: for the
        // known games, use the copy from assets/badges (made by tools/make_badges.py) that fits the circle
        Some(match exe.filter(|e| BADGES.contains(e)) {
            Some(e) => format!("{BADGES_URL}{}.png", e.strip_suffix(".exe").unwrap_or(e).replace(' ', "_")),
            None => url,
        })
    }

    const BADGES_URL: &str = "https://raw.githubusercontent.com/maykha3v/ferrocap-releases/main/assets/badges/";
    /// Output of tools/make_badges.py: known games whose icon has a fitted badge in assets/badges.
    const BADGES: &[&str] = &[
        "brawlhalla.exe", "deadlock.exe", "destiny2.exe", "discovery.exe", "dota2.exe", "gta5.exe", "gta5_enhanced.exe",
        "gtx.exe", "javaw.exe", "league of legends.exe", "left4dead2.exe", "minecraft.windows.exe", "osu!.exe",
        "overwatch.exe", "project8.exe", "robloxplayerbeta.exe", "rocketleague.exe", "terraria.exe", "thefinals.exe",
        "tslgame.exe", "windows10universal.exe",
    ];

    fn set_activity(f: &mut File, activity: serde_json::Value, nonce: u64) -> std::io::Result<bool> {
        let msg = serde_json::json!({
            "cmd": "SET_ACTIVITY",
            "args": { "pid": std::process::id(), "activity": activity },
            "nonce": format!("ferrocap-{nonce}"),
        });
        send(f, 1, &msg)?;
        let (_, resp) = recv(f)?;
        let ok = resp.get("evt").and_then(|e| e.as_str()) != Some("ERROR");
        if !ok {
            log::warn!("Discord refused the status: {}", resp.get("data").unwrap_or(&serde_json::Value::Null));
        }
        Ok(ok)
    }

    pub fn spawn() {
        std::thread::Builder::new()
            .name("discord".into())
            .spawn(|| {
                let mut conn: Option<(File, String)> = None;
                let mut last_sent: Option<String> = None;
                let mut since = (String::new(), SystemTime::now());
                let mut nonce = 0u64;
                let mut retry_at = Instant::now();
                loop {
                    std::thread::sleep(Duration::from_secs(3));
                    let s = app::core().settings();
                    let custom = s.discord_app_id.trim();
                    let id = if custom.is_empty() { BUILTIN_APP_ID.to_string() } else { custom.to_string() };
                    if !s.discord_rpc || id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
                        if let Some((mut f, _)) = conn.take() {
                            nonce += 1;
                            let _ = set_activity(&mut f, serde_json::Value::Null, nonce);
                        }
                        last_sent = None;
                        set_status(if !s.discord_rpc { "Off" } else { "Needs an Application ID" });
                        continue;
                    }
                    // (re)connect, also when the ID changed
                    if conn.as_ref().map(|c| c.1 != id).unwrap_or(true) {
                        conn = None;
                        last_sent = None;
                        if Instant::now() < retry_at {
                            continue;
                        }
                        match connect() {
                            Some(mut f) => {
                                let ok = send(&mut f, 0, &serde_json::json!({ "v": 1, "client_id": id }))
                                    .and_then(|_| recv(&mut f))
                                    .map(|(op, v)| op == 1 && v.get("evt").and_then(|e| e.as_str()) == Some("READY"))
                                    .unwrap_or(false);
                                if ok {
                                    conn = Some((f, id.clone()));
                                    set_status("Connected");
                                } else {
                                    set_status("Discord rejected the Application ID");
                                    retry_at = Instant::now() + Duration::from_secs(30);
                                    continue;
                                }
                            }
                            None => {
                                set_status("Discord isn't running");
                                retry_at = Instant::now() + Duration::from_secs(15);
                                continue;
                            }
                        }
                    }
                    let game = procs::game_name();
                    let details = match &game {
                        Some(g) => format!("Clipping {g}"),
                        None => "Clipping my screen".to_string(),
                    };
                    if since.0 != details {
                        since = (details.clone(), SystemTime::now());
                    }
                    let state = if s.paused {
                        "Buffering paused".to_string()
                    } else {
                        format!("Last {} s ready · {} to clip", s.buffer_secs, s.clip_key.label())
                    };
                    let icon = game.as_ref().and_then(|g| game_icon(procs::game_exe().as_deref(), g));
                    let key = format!("{details}|{state}|{}", icon.as_deref().unwrap_or(""));
                    if last_sent.as_deref() == Some(key.as_str()) {
                        continue;
                    }
                    let start = since.1.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                    // Ferrocap's logo big, the game's icon as the round badge in its corner
                    let mut assets = serde_json::json!({ "large_image": LOGO_URL, "large_text": "Ferrocap · clip it after it happens" });
                    if let (Some(url), Some(g)) = (&icon, &game) {
                        assets["small_image"] = url.clone().into();
                        assets["small_text"] = g.clone().into();
                    }
                    let activity = serde_json::json!({
                        "details": details,
                        "state": state,
                        "timestamps": { "start": start },
                        "assets": assets,
                        "buttons": [{ "label": "Download Ferrocap", "url": crate::release::SITE_URL }],
                        "instance": false,
                    });
                    nonce += 1;
                    let (f, _) = conn.as_mut().unwrap();
                    match set_activity(f, activity, nonce) {
                        Ok(true) => {
                            last_sent = Some(key);
                            set_status("Connected");
                        }
                        Ok(false) => {
                            set_status("Discord refused the status update");
                            last_sent = Some(key);
                        }
                        Err(_) => {
                            conn = None;
                            set_status("Discord isn't running");
                            retry_at = Instant::now() + Duration::from_secs(10);
                        }
                    }
                }
            })
            .ok();
    }
}

// ================================================================================================
// release
// ================================================================================================

mod release {
    //! Where Ferrocap is published. The website's Download button, the Discord status button and the
    //! first-launch engine download all point at the latest GitHub release.
    pub const REPO: &str = "maykha3v/ferrocap-releases";
    pub const SITE_URL: &str = "https://www.ferrocap.xyz";
    pub const LATEST_API: &str = "https://api.github.com/repos/maykha3v/ferrocap-releases/releases/latest";
    /// Version of this build (FERROCAP_VERSION can override it, for testing the updater).
    pub const VERSION: &str = match option_env!("FERROCAP_VERSION") {
        Some(v) => v,
        None => env!("CARGO_PKG_VERSION"),
    };
    pub const DOWNLOAD_URL: &str = "https://github.com/maykha3v/ferrocap-releases/releases/latest/download/Ferrocap.exe";
    pub const ENGINE_URL: &str = "https://github.com/maykha3v/ferrocap-releases/releases/latest/download/ffmpeg-win64.zip";
    #[allow(dead_code)]
    pub fn repo() -> &'static str {
        REPO
    }
}

// ================================================================================================
// update
// ================================================================================================

mod update {
    //! Checks GitHub for a newer release (at start-up, then every 6 hours, or on demand) and installs
    //! it in one click: download the new exe, swap it in (Windows lets a running exe be renamed),
    //! restart. Uses curl.exe, which ships with Windows 10/11.

    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use crate::app::{ToastKind, core};
    use crate::{release, sys};

    #[derive(Clone, Debug, PartialEq)]
    pub enum Status {
        Idle,
        Checking,
        UpToDate,
        Available { version: String, url: String },
        Downloading(u64),
        Error(String),
    }

    static STATUS: Mutex<Status> = Mutex::new(Status::Idle);
    static LATEST: Mutex<Option<(String, String)>> = Mutex::new(None);
    static NOTIFIED: Mutex<Option<String>> = Mutex::new(None);
    static BUSY: AtomicBool = AtomicBool::new(false);

    pub fn status() -> Status {
        STATUS.lock().unwrap().clone()
    }

    /// For the sidebar card: stays visible (with the version) while downloading or after a failed install.
    pub fn status_for_card() -> Status {
        match status() {
            Status::Available { version, url } => Status::Available { version, url },
            Status::Downloading(b) => Status::Downloading(b),
            Status::Error(_) => match LATEST.lock().unwrap().clone() {
                Some((v, _)) => Status::Error(v),
                None => Status::Idle,
            },
            _ => Status::Idle,
        }
    }

    fn set(s: Status) {
        *STATUS.lock().unwrap() = s;
        core().repaint();
    }

    fn parse(v: &str) -> Vec<u64> {
        v.trim().trim_start_matches(['v', 'V']).split(['.', '-']).map(|p| p.parse().unwrap_or(0)).take(3).collect()
    }

    fn newer(remote: &str, local: &str) -> bool {
        let (mut a, mut b) = (parse(remote), parse(local));
        a.resize(3, 0);
        b.resize(3, 0);
        a > b
    }

    fn fetch_latest() -> Result<(String, String), String> {
        let out = sys::quiet("curl.exe")
            .args(["-sL", "--fail", "--max-time", "15", "-H", "Accept: application/vnd.github+json", "-H"])
            .arg(format!("User-Agent: Ferrocap/{}", release::VERSION))
            .arg(release::LATEST_API)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err("Couldn't reach GitHub to check for updates.".into());
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
        let tag = v.get("tag_name").and_then(|t| t.as_str()).ok_or("No release found.")?.to_string();
        let url = v
            .get("assets")
            .and_then(|a| a.as_array())
            .and_then(|a| a.iter().find(|x| x.get("name").and_then(|n| n.as_str()) == Some("Ferrocap.exe")))
            .and_then(|x| x.get("browser_download_url"))
            .and_then(|u| u.as_str())
            .unwrap_or(release::DOWNLOAD_URL)
            .to_string();
        Ok((tag.trim_start_matches(['v', 'V']).to_string(), url))
    }

    /// Check now. `manual` = the user pressed the button (always shows a result).
    pub fn check(manual: bool) {
        if BUSY.swap(true, Ordering::SeqCst) {
            return;
        }
        set(Status::Checking);
        std::thread::spawn(move || {
            let r = fetch_latest();
            BUSY.store(false, Ordering::SeqCst);
            match r {
                Ok((version, url)) if newer(&version, release::VERSION) => {
                    *LATEST.lock().unwrap() = Some((version.clone(), url.clone()));
                    set(Status::Available { version: version.clone(), url });
                    let mut n = NOTIFIED.lock().unwrap();
                    if manual || n.as_deref() != Some(version.as_str()) {
                        *n = Some(version.clone());
                        drop(n);
                        core().toast(ToastKind::Clip, "Update available", format!("Ferrocap {version} is out. Install it from the sidebar."), None);
                    }
                }
                Ok(_) => set(Status::UpToDate),
                Err(e) => set(if manual { Status::Error(e) } else { Status::Idle }),
            }
        });
    }

    pub fn spawn_auto() {
        std::thread::Builder::new()
            .name("updates".into())
            .spawn(|| {
                if std::env::args().any(|a| a == "--install-update") {
                    // dev/test: check and install without clicking
                    check(true);
                    for _ in 0..60 {
                        std::thread::sleep(Duration::from_millis(500));
                        if matches!(status(), Status::Available { .. }) {
                            install();
                            return;
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs(20));
                loop {
                    if !matches!(status(), Status::Downloading(_)) {
                        check(false);
                    }
                    std::thread::sleep(Duration::from_secs(6 * 3600));
                }
            })
            .ok();
    }

    fn old_path(exe: &PathBuf) -> PathBuf {
        exe.with_extension("old.exe")
    }

    /// Remove the previous version left behind by an update.
    pub fn cleanup_old() {
        if let Ok(exe) = std::env::current_exe() {
            let old = old_path(&exe);
            std::thread::spawn(move || {
                for _ in 0..10 {
                    if !old.exists() || std::fs::remove_file(&old).is_ok() {
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            });
        }
    }

    /// Download the new version, swap it in and restart.
    pub fn install() {
        let Some((version, url)) = LATEST.lock().unwrap().clone() else { return };
        if BUSY.swap(true, Ordering::SeqCst) {
            return;
        }
        set(Status::Downloading(0));
        std::thread::spawn(move || {
            let r = (|| -> Result<(), String> {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                let new = exe.with_extension("new.exe");
                let _ = std::fs::remove_file(&new);
                let mut c = sys::quiet("curl.exe").args(["-L", "--fail", "--silent", "--retry", "3", "-o"]).arg(&new).arg(&url).spawn().map_err(|e| e.to_string())?;
                loop {
                    match c.try_wait() {
                        Ok(Some(st)) if st.success() => break,
                        Ok(Some(_)) => return Err("Download failed. Try again in a bit.".into()),
                        Ok(None) => {
                            set(Status::Downloading(std::fs::metadata(&new).map(|m| m.len()).unwrap_or(0)));
                            std::thread::sleep(Duration::from_millis(200));
                        }
                        Err(e) => return Err(e.to_string()),
                    }
                }
                // sanity check: a real Windows exe of plausible size
                let head = std::fs::read(&new).map_err(|e| e.to_string())?;
                if head.len() < 1_000_000 || &head[..2] != b"MZ" {
                    let _ = std::fs::remove_file(&new);
                    return Err("The download was damaged. Try again.".into());
                }
                let old = old_path(&exe);
                let _ = std::fs::remove_file(&old);
                std::fs::rename(&exe, &old).map_err(|e| format!("Couldn't replace Ferrocap: {e}"))?;
                if let Err(e) = std::fs::rename(&new, &exe) {
                    let _ = std::fs::rename(&old, &exe); // put the old one back
                    return Err(format!("Couldn't replace Ferrocap: {e}"));
                }
                // start the new version once this one has exited (it holds the single-instance lock)
                let cmd = format!("ping -n 3 127.0.0.1 >nul & start \"\" \"{}\" --updated", exe.display());
                {
                    use std::os::windows::process::CommandExt;
                    std::process::Command::new("cmd.exe").raw_arg("/c").raw_arg(&cmd).creation_flags(sys::CREATE_NO_WINDOW).spawn().map_err(|e| e.to_string())?;
                }
                Ok(())
            })();
            BUSY.store(false, Ordering::SeqCst);
            match r {
                Ok(()) => core().request_quit(),
                Err(e) => {
                    set(Status::Error(e.clone()));
                    core().toast(ToastKind::Error, "Update failed", format!("{e} (version {version})"), None);
                }
            }
        });
    }
}

// ================================================================================================
// snip
// ================================================================================================

mod snip {
    //! Snipping: Win+Shift+S (or tray / top bar) freezes the screen under the cursor, then you pick a
    //! region, a window or the whole screen. The snip opens in an editor with drawing tools.

    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use eframe::egui::{
        self, Align2, Color32, CornerRadius, Key, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2, ViewportCommand, pos2, vec2,
    };

    use crate::app::{ToastKind, core};
    use crate::sys;
    use crate::theme::*;

    #[derive(Clone)]
    pub struct Img {
        pub rgba: Vec<u8>,
        pub w: u32,
        pub h: u32,
    }

    impl Img {
        fn crop(&self, x: u32, y: u32, w: u32, h: u32) -> Img {
            let (x, y) = (x.min(self.w - 1), y.min(self.h - 1));
            let (w, h) = (w.min(self.w - x).max(1), h.min(self.h - y).max(1));
            let mut out = Vec::with_capacity((w * h * 4) as usize);
            for row in y..y + h {
                let s = ((row * self.w + x) * 4) as usize;
                out.extend_from_slice(&self.rgba[s..s + (w * 4) as usize]);
            }
            Img { rgba: out, w, h }
        }
    }

    struct Pending {
        shot: Img,
        /// monitor origin + size in physical pixels
        mon: (i32, i32, i32, i32),
        /// visible top-level windows, z-order, physical absolute rects
        wins: Vec<(i32, i32, i32, i32)>,
        restore_main: bool,
    }

    static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
    static PENDING_FILE: Mutex<Option<PathBuf>> = Mutex::new(None);

    pub fn has_pending() -> bool {
        PENDING.lock().unwrap().is_some() || PENDING_FILE.lock().unwrap().is_some()
    }

    /// Freeze the monitor under the cursor and open the overlay. `delay_ms` lets our own window hide first.
    pub fn trigger(delay_ms: u64, restore_main: bool) {
        std::thread::spawn(move || {
            if delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(delay_ms));
            }
            let (cx, cy) = sys::cursor_pos();
            let mons = sys::monitors();
            let m = mons.iter().copied().find(|m| cx >= m.x && cx < m.x + m.w && cy >= m.y && cy < m.y + m.h).unwrap_or(mons[0]);
            if let Some((rgba, w, h)) = sys::grab_screen(m.x, m.y, m.w, m.h) {
                let wins = sys::window_rects();
                *PENDING.lock().unwrap() = Some(Pending { shot: Img { rgba, w, h }, mon: (m.x, m.y, m.w, m.h), wins, restore_main });
                core().repaint();
            }
        });
    }

    /// Open an existing image (e.g. an F9 screenshot) in the editor.
    pub fn open_file(path: PathBuf) {
        *PENDING_FILE.lock().unwrap() = Some(path);
        core().repaint();
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Region,
        Window,
        Screen,
    }

    struct Overlay {
        shot: Img,
        tex: egui::TextureHandle,
        mon: (i32, i32, i32, i32),
        wins: Vec<(i32, i32, i32, i32)>,
        mode: Mode,
        drag: Option<(Pos2, Pos2)>,
        opened: Instant,
        ppp: f32,
        restore_main: bool,
    }

    // ------------------------------------------------------------------ editor model

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Tool {
        Pen,
        Highlighter,
        Arrow,
        Line,
        Rect,
        Ellipse,
        Text,
        Pixelate,
        Crop,
        Eraser,
    }

    impl Tool {
        const ALL: [Tool; 10] = [
            Tool::Pen,
            Tool::Highlighter,
            Tool::Arrow,
            Tool::Line,
            Tool::Rect,
            Tool::Ellipse,
            Tool::Text,
            Tool::Pixelate,
            Tool::Crop,
            Tool::Eraser,
        ];
        fn icon(self) -> Icon {
            match self {
                Tool::Pen => Icon::Pen,
                Tool::Highlighter => Icon::Highlighter,
                Tool::Arrow => Icon::Arrow,
                Tool::Line => Icon::LineTool,
                Tool::Rect => Icon::RectTool,
                Tool::Ellipse => Icon::EllipseTool,
                Tool::Text => Icon::TextTool,
                Tool::Pixelate => Icon::Pixelate,
                Tool::Crop => Icon::Crop,
                Tool::Eraser => Icon::Eraser,
            }
        }
        fn name(self) -> &'static str {
            match self {
                Tool::Pen => "Pen  P",
                Tool::Highlighter => "Highlighter  H",
                Tool::Arrow => "Arrow  A",
                Tool::Line => "Line  L",
                Tool::Rect => "Rectangle  R",
                Tool::Ellipse => "Ellipse  O",
                Tool::Text => "Text  T",
                Tool::Pixelate => "Pixelate (hide names, chat…)  X",
                Tool::Crop => "Crop  C",
                Tool::Eraser => "Eraser: click a drawing to remove it  E",
            }
        }
        fn key(self) -> Key {
            match self {
                Tool::Pen => Key::P,
                Tool::Highlighter => Key::H,
                Tool::Arrow => Key::A,
                Tool::Line => Key::L,
                Tool::Rect => Key::R,
                Tool::Ellipse => Key::O,
                Tool::Text => Key::T,
                Tool::Pixelate => Key::X,
                Tool::Crop => Key::C,
                Tool::Eraser => Key::E,
            }
        }
    }

    #[derive(Clone, Debug)]
    enum Mark {
        Pen { pts: Vec<Pos2>, color: Color32, w: f32, hl: bool },
        Line { a: Pos2, b: Pos2, color: Color32, w: f32, arrow: bool },
        Rect { r: Rect, color: Color32, w: f32 },
        Ellipse { r: Rect, color: Color32, w: f32 },
        Text { pos: Pos2, text: String, color: Color32, size: f32 },
    }

    impl Mark {
        fn bounds(&self) -> Rect {
            match self {
                Mark::Pen { pts, w, .. } => Rect::from_points(pts).expand(*w),
                Mark::Line { a, b, w, .. } => Rect::from_two_pos(*a, *b).expand(*w * 3.0),
                Mark::Rect { r, w, .. } | Mark::Ellipse { r, w, .. } => r.expand(*w),
                Mark::Text { pos, text, size, .. } => {
                    let lines = text.lines().count().max(1) as f32;
                    let longest = text.lines().map(|l| l.chars().count()).max().unwrap_or(1) as f32;
                    Rect::from_min_size(*pos, vec2(longest * size * 0.55, lines * size * 1.25))
                }
            }
        }
        fn translate(&mut self, d: Vec2) {
            match self {
                Mark::Pen { pts, .. } => pts.iter_mut().for_each(|p| *p += d),
                Mark::Line { a, b, .. } => {
                    *a += d;
                    *b += d;
                }
                Mark::Rect { r, .. } | Mark::Ellipse { r, .. } => *r = r.translate(d),
                Mark::Text { pos, .. } => *pos += d,
            }
        }
    }

    #[derive(Clone)]
    struct State {
        base: Option<Img>, // only stored when the base image changed (pixelate / crop)
        marks: Vec<Mark>,
    }

    const PALETTE: [Color32; 8] = [
        Color32::from_rgb(0xff, 0x3b, 0x30),
        Color32::from_rgb(0xff, 0x5b, 0x14),
        Color32::from_rgb(0xff, 0xd6, 0x0a),
        Color32::from_rgb(0x34, 0xc7, 0x59),
        Color32::from_rgb(0x0a, 0x84, 0xff),
        Color32::from_rgb(0xbf, 0x5a, 0xf2),
        Color32::WHITE,
        Color32::from_rgb(0x12, 0x12, 0x12),
    ];

    struct Editor {
        base: Img,
        tex: Option<egui::TextureHandle>,
        dirty: bool,
        marks: Vec<Mark>,
        undo: Vec<State>,
        redo: Vec<State>,
        tool: Tool,
        color: Color32,
        size: f32,
        drawing: Option<Mark>,
        drag_rect: Option<(Pos2, Pos2)>,
        text: Option<(Pos2, String, bool)>,
        saved: Option<PathBuf>,
        flash: Option<(String, Instant)>,
        opened: Instant,
        title: String,
        ppp: f32,
        mon: (i32, i32, i32, i32),
    }

    pub struct SnipUi {
        overlay: Option<Overlay>,
        editor: Option<Editor>,
    }

    impl SnipUi {
        pub fn new() -> Self {
            Self { overlay: None, editor: None }
        }

        pub fn active(&self) -> bool {
            self.overlay.is_some() || self.editor.is_some()
        }

        fn ppp(ctx: &egui::Context) -> f32 {
            ctx.input(|i| i.viewport().native_pixels_per_point).unwrap_or(ctx.pixels_per_point()).max(0.5)
        }

        pub fn poll(&mut self, ctx: &egui::Context) {
            if let Some(p) = PENDING.lock().unwrap().take() {
                let tex = ctx.load_texture(
                    "snip-frozen",
                    egui::ColorImage::from_rgba_unmultiplied([p.shot.w as usize, p.shot.h as usize], &p.shot.rgba),
                    egui::TextureOptions::NEAREST,
                );
                self.overlay = Some(Overlay {
                    shot: p.shot,
                    tex,
                    mon: p.mon,
                    wins: p.wins,
                    mode: Mode::Region,
                    drag: None,
                    opened: Instant::now(),
                    ppp: Self::ppp(ctx),
                    restore_main: p.restore_main,
                });
            }
            if let Some(path) = PENDING_FILE.lock().unwrap().take() {
                if let Ok(img) = image::open(&path) {
                    let img = img.to_rgba8();
                    let (w, h) = img.dimensions();
                    let title = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let mons = sys::monitors();
                    let m = mons[0];
                    self.open_editor(ctx, Img { rgba: img.into_raw(), w, h }, title, (m.x, m.y, m.w, m.h), Some(path));
                }
            }
        }

        fn open_editor(&mut self, ctx: &egui::Context, img: Img, title: String, mon: (i32, i32, i32, i32), saved: Option<PathBuf>) {
            self.editor = Some(Editor {
                base: img,
                tex: None,
                dirty: true,
                marks: vec![],
                undo: vec![],
                redo: vec![],
                tool: Tool::Arrow,
                color: PALETTE[0],
                size: 5.0,
                drawing: None,
                drag_rect: None,
                text: None,
                saved,
                flash: None,
                opened: Instant::now(),
                title,
                ppp: Self::ppp(ctx),
                mon,
            });
        }

        pub fn ui(&mut self, ctx: &egui::Context) {
            if self.overlay.is_some() {
                self.overlay_ui(ctx);
            }
            if self.editor.is_some() {
                self.editor_ui(ctx);
            }
        }

        // ------------------------------------------------------------------ overlay

        fn overlay_ui(&mut self, ctx: &egui::Context) {
            let o = self.overlay.as_mut().unwrap();
            let (mx, my, mw, mh) = o.mon;
            let builder = egui::ViewportBuilder::default()
                .with_title("Ferrocap snip")
                .with_decorations(false)
                .with_resizable(false)
                .with_always_on_top()
                .with_taskbar(false)
                .with_active(true)
                .with_position(pos2(mx as f32 / o.ppp, my as f32 / o.ppp))
                .with_inner_size([mw as f32 / o.ppp, mh as f32 / o.ppp]);
            let mut result: Option<Option<Img>> = None; // Some(None) = cancelled
            let first = o.opened.elapsed() < Duration::from_millis(80);
            ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("ferrocap-snip-overlay"), builder, |ui, _| {
                if first {
                    ui.ctx().send_viewport_cmd(ViewportCommand::Focus);
                }
                let p = pal();
                let full = ui.max_rect();
                let pt = ui.painter().clone();
                let sx = o.shot.w as f32 / full.width(); // image px per point
                let to_img = |q: Pos2| pos2((q.x - full.left()) * sx, (q.y - full.top()) * sx);
                let to_scr = |q: Pos2| pos2(full.left() + q.x / sx, full.top() + q.y / sx);
                pt.image(o.tex.id(), full, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);

                let resp = ui.interact(full, ui.id().with("snip-area"), Sense::click_and_drag());
                let hover = ui.input(|i| i.pointer.hover_pos());
                ui.input(|i| {
                    if i.key_pressed(Key::Escape) {
                        result = Some(None);
                    }
                    if i.key_pressed(Key::R) {
                        o.mode = Mode::Region;
                    }
                    if i.key_pressed(Key::W) {
                        o.mode = Mode::Window;
                    }
                    if i.key_pressed(Key::F) {
                        o.mode = Mode::Screen;
                    }
                });

                // the selection (in image px)
                let mut sel: Option<Rect> = None;
                match o.mode {
                    Mode::Region => {
                        if resp.drag_started() {
                            if let Some(q) = ui.input(|i| i.pointer.press_origin()) {
                                o.drag = Some((to_img(q), to_img(q)));
                            }
                        }
                        if let (Some((a, _)), Some(q)) = (o.drag, hover.or(resp.interact_pointer_pos())) {
                            if resp.dragged() || resp.drag_stopped() {
                                o.drag = Some((a, to_img(q)));
                            }
                        }
                        if let Some((a, b)) = o.drag {
                            sel = Some(Rect::from_two_pos(a, b));
                        }
                        if resp.drag_stopped() {
                            if let Some(r) = sel.filter(|r| r.width() >= 3.0 && r.height() >= 3.0) {
                                result = Some(Some(o.shot.crop(r.left() as u32, r.top() as u32, r.width() as u32, r.height() as u32)));
                            }
                            o.drag = None;
                        }
                    }
                    Mode::Window => {
                        if let Some(h) = hover {
                            let hi = to_img(h);
                            let (ax, ay) = (hi.x as i32 + mx, hi.y as i32 + my);
                            if let Some(&(x, y, w, hh)) = o.wins.iter().find(|(x, y, w, hh)| ax >= *x && ax < x + w && ay >= *y && ay < y + hh) {
                                let r = Rect::from_min_size(pos2((x - mx) as f32, (y - my) as f32), vec2(w as f32, hh as f32))
                                    .intersect(Rect::from_min_size(Pos2::ZERO, vec2(o.shot.w as f32, o.shot.h as f32)));
                                sel = Some(r);
                            }
                        }
                        if resp.clicked() {
                            if let Some(r) = sel {
                                result = Some(Some(o.shot.crop(r.left() as u32, r.top() as u32, r.width() as u32, r.height() as u32)));
                            }
                        }
                    }
                    Mode::Screen => {
                        sel = Some(Rect::from_min_size(Pos2::ZERO, vec2(o.shot.w as f32, o.shot.h as f32)));
                        if resp.clicked() {
                            result = Some(Some(o.shot.clone()));
                        }
                    }
                }

                // dim everything but the selection
                let dim = alpha(Color32::BLACK, 0.5);
                match sel.map(|r| Rect::from_min_max(to_scr(r.min), to_scr(r.max))) {
                    Some(s) => {
                        pt.rect_filled(Rect::from_min_max(full.min, pos2(full.right(), s.top())), CornerRadius::ZERO, dim);
                        pt.rect_filled(Rect::from_min_max(pos2(full.left(), s.bottom()), full.max), CornerRadius::ZERO, dim);
                        pt.rect_filled(Rect::from_min_max(pos2(full.left(), s.top()), pos2(s.left(), s.bottom())), CornerRadius::ZERO, dim);
                        pt.rect_filled(Rect::from_min_max(pos2(s.right(), s.top()), pos2(full.right(), s.bottom())), CornerRadius::ZERO, dim);
                        pt.rect_stroke(s, CornerRadius::ZERO, Stroke::new(2.0, p.accent), StrokeKind::Outside);
                        for c in [s.left_top(), s.right_top(), s.left_bottom(), s.right_bottom()] {
                            pt.rect_filled(Rect::from_center_size(c, vec2(8.0, 8.0)), CornerRadius::same(2), Color32::WHITE);
                        }
                        let r = sel.unwrap();
                        let label = format!("{} × {}", r.width().round() as i32, r.height().round() as i32);
                        let g = pt.layout_no_wrap(label, semibold(12.0), Color32::WHITE);
                        let lr = Rect::from_min_size(pos2(s.left(), (s.top() - g.size().y - 12.0).max(full.top() + 4.0)), g.size() + vec2(14.0, 8.0));
                        pt.rect_filled(lr, CornerRadius::same(6), alpha(Color32::BLACK, 0.8));
                        pt.galley(lr.min + vec2(7.0, 4.0), g, Color32::WHITE);
                    }
                    None => {
                        pt.rect_filled(full, CornerRadius::ZERO, dim);
                    }
                }

                // crosshair + magnifier (region mode, before dragging)
                if let Some(h) = hover {
                    // One crosshair in the accent color with a dark outline (shows on light and dark screens).
                    // Full-screen guide lines in region mode, joined to a bolder cross at the cursor.
                    // Reticle: four orange arcs that slowly spin around the cursor, short ticks pointing at it,
                    // and a white center dot on the exact pixel. Everything has a dark edge so it reads on any
                    // background. In region mode thin guide lines run from the ticks to the screen edges.
                    let t = ui.input(|i| i.time) as f32;
                    let dark = Stroke::new(4.5, alpha(Color32::BLACK, 0.6));
                    let ring_r = 13.0;
                    let (tick_in, tick_out) = (17.0, 23.0);
                    pt.circle_filled(h, ring_r + 7.0, alpha(p.accent, 0.10)); // soft glow
                    let arc = |a0: f32| -> Vec<Pos2> {
                        (0..=12).map(|i| a0 + i as f32 / 12.0 * 1.15).map(|a| h + vec2(a.cos(), a.sin()) * ring_r).collect()
                    };
                    let spin = t * 0.9;
                    let arcs: Vec<Vec<Pos2>> = (0..4).map(|k| arc(spin + k as f32 * std::f32::consts::FRAC_PI_2 + 0.2)).collect();
                    let dirs = [vec2(1.0, 0.0), vec2(-1.0, 0.0), vec2(0.0, 1.0), vec2(0.0, -1.0)];
                    let guides = o.mode == Mode::Region && o.drag.is_none();
                    let far = |d: Vec2| if d.x > 0.0 { pos2(full.right(), h.y) } else if d.x < 0.0 { pos2(full.left(), h.y) } else if d.y > 0.0 { pos2(h.x, full.bottom()) } else { pos2(h.x, full.top()) };
                    // outlines first, then color on top
                    for d in dirs {
                        if guides {
                            pt.line_segment([h + d * tick_out, far(d)], Stroke::new(3.5, alpha(Color32::BLACK, 0.5)));
                        }
                        pt.line_segment([h + d * (tick_in - 1.0), h + d * (tick_out + 1.0)], dark);
                    }
                    for a in &arcs {
                        pt.add(egui::Shape::line(a.clone(), dark));
                    }
                    for d in dirs {
                        if guides {
                            pt.line_segment([h + d * tick_out, far(d)], Stroke::new(1.5, p.accent));
                        }
                        pt.line_segment([h + d * tick_in, h + d * tick_out], Stroke::new(2.5, p.accent));
                    }
                    for a in arcs {
                        pt.add(egui::Shape::line(a, Stroke::new(2.5, p.accent)));
                    }
                    pt.circle_filled(h, 3.0, Color32::BLACK);
                    pt.circle_filled(h, 1.8, Color32::WHITE);
                    if o.mode == Mode::Region {
                        loupe(&pt, &o.tex, &o.shot, to_img(h), h, full);
                    }
                }

                // mode toolbar
                let tb = Rect::from_center_size(pos2(full.center().x, full.top() + 40.0), vec2(388.0, 46.0));
                pt.rect_filled(tb, CornerRadius::same(14), alpha(p.panel, 0.96));
                pt.rect_stroke(tb, CornerRadius::same(14), Stroke::new(1.0, p.line2), StrokeKind::Inside);
                let modes = [(Mode::Region, "Region  R", Icon::RectTool), (Mode::Window, "Window  W", Icon::Library), (Mode::Screen, "Screen  F", Icon::Live)];
                for (i, (m, label, icon)) in modes.iter().enumerate() {
                    let r = Rect::from_min_size(pos2(tb.left() + 6.0 + i as f32 * 112.0, tb.top() + 6.0), vec2(108.0, 34.0));
                    let rr = ui.interact(r, ui.id().with(("snipmode", i)), Sense::click());
                    let on = o.mode == *m;
                    if on || rr.hovered() {
                        pt.rect_filled(r, CornerRadius::same(10), if on { p.text } else { p.panel3 });
                    }
                    let col = if on { p.bg } else { p.text };
                    draw_icon(&pt, *icon, pos2(r.left() + 18.0, r.center().y), col, 0.55);
                    pt.text(pos2(r.left() + 32.0, r.center().y), Align2::LEFT_CENTER, *label, semibold(12.5), col);
                    if rr.clicked() {
                        o.mode = *m;
                        o.drag = None;
                    }
                }
                let xr = Rect::from_min_size(pos2(tb.right() - 46.0, tb.top() + 6.0), vec2(40.0, 34.0));
                let xresp = ui.interact(xr, ui.id().with("snipx"), Sense::click());
                if xresp.hovered() {
                    pt.rect_filled(xr, CornerRadius::same(10), Color32::from_rgb(0xd9, 0x34, 0x2b));
                }
                let c = xr.center();
                pt.line_segment([c + vec2(-5.0, -5.0), c + vec2(5.0, 5.0)], Stroke::new(1.5, p.text));
                pt.line_segment([c + vec2(-5.0, 5.0), c + vec2(5.0, -5.0)], Stroke::new(1.5, p.text));
                if xresp.clicked() {
                    result = Some(None);
                }
                // our own outlined cross replaces the system cursor over the screenshot
                let over_toolbar = hover.is_some_and(|h| tb.contains(h));
                ui.ctx().set_cursor_icon(if over_toolbar || hover.is_none() { egui::CursorIcon::Default } else { egui::CursorIcon::None });
                ui.ctx().request_repaint();
            });

            if let Some(r) = result {
                let o = self.overlay.take().unwrap();
                if o.restore_main {
                    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                }
                if let Some(img) = r {
                    // Win+Shift+S with "copy only": straight to the clipboard, no editor (the Snip button
                    // in Ferrocap, which sets restore_main, still opens the editor)
                    if core().settings().snip_copy_only && !o.restore_main {
                        copy_img(&img);
                        core().toast(crate::app::ToastKind::Shot, "Copied to clipboard", format!("Snip {} × {}", img.w, img.h), None);
                        return;
                    }
                    if core().settings().copy_shot {
                        copy_img(&img);
                    }
                    let title = format!("Snip {}", sys::timestamp().replace('_', " "));
                    self.open_editor(ctx, img, title, o.mon, None);
                    if let Some(e) = self.editor.as_mut() {
                        e.flash = Some((if core().settings().copy_shot { "Copied to clipboard".into() } else { "Snip ready".into() }, Instant::now()));
                    }
                }
            }
        }

        // ------------------------------------------------------------------ editor

        fn editor_ui(&mut self, ctx: &egui::Context) {
            let e = self.editor.as_mut().unwrap();
            if e.dirty || e.tex.is_none() {
                e.tex = Some(ctx.load_texture(
                    "snip-base",
                    egui::ColorImage::from_rgba_unmultiplied([e.base.w as usize, e.base.h as usize], &e.base.rgba),
                    egui::TextureOptions::LINEAR,
                ));
                e.dirty = false;
            }
            let (mx, my, mw, mh) = e.mon;
            let ppp = e.ppp;
            let img_pts = vec2(e.base.w as f32 / ppp, e.base.h as f32 / ppp);
            let max = vec2(mw as f32 / ppp * 0.86, mh as f32 / ppp * 0.86);
            let size = vec2((img_pts.x + 48.0).clamp(1020.0, max.x.max(1020.0)), (img_pts.y + 140.0).clamp(560.0, max.y.max(560.0)));
            let pos = pos2(mx as f32 / ppp + (mw as f32 / ppp - size.x) / 2.0, my as f32 / ppp + (mh as f32 / ppp - size.y) / 2.0);
            let builder = egui::ViewportBuilder::default()
                .with_title("Ferrocap Snip")
                .with_decorations(false)
                .with_resizable(true)
                .with_min_inner_size([980.0, 520.0])
                .with_inner_size(size)
                .with_position(pos)
                .with_icon(std::sync::Arc::new({
                    let (rgba, width, height) = crate::tray::rounded_rgba(64);
                    egui::IconData { rgba, width, height }
                }));
            let mut close = false;
            let first = e.opened.elapsed() < Duration::from_millis(80);
            ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("ferrocap-snip-editor"), builder, |ui, _| {
                if first {
                    ui.ctx().send_viewport_cmd(ViewportCommand::Focus);
                }
                if ui.input(|i| i.viewport().close_requested()) {
                    close = true;
                }
                close |= e.ui(ui);
            });
            if close {
                self.editor = None;
            }
        }
    }

    /// Pixel-zoom loupe next to the cursor.
    fn loupe(pt: &egui::Painter, tex: &egui::TextureHandle, img: &Img, at: Pos2, cursor: Pos2, full: Rect) {
        let p = pal();
        let n = 15.0; // pixels across
        let box_sz = 135.0;
        let mut r = Rect::from_min_size(cursor + vec2(24.0, 24.0), vec2(box_sz, box_sz + 34.0));
        if r.right() > full.right() - 8.0 {
            r = r.translate(vec2(-box_sz - 48.0, 0.0));
        }
        if r.bottom() > full.bottom() - 8.0 {
            r = r.translate(vec2(0.0, -box_sz - 82.0));
        }
        let zoom = Rect::from_min_size(r.min, vec2(box_sz, box_sz));
        let (cx, cy) = (at.x.floor(), at.y.floor());
        let uv = Rect::from_min_max(
            pos2((cx - n / 2.0 + 0.5) / img.w as f32, (cy - n / 2.0 + 0.5) / img.h as f32),
            pos2((cx + n / 2.0 + 0.5) / img.w as f32, (cy + n / 2.0 + 0.5) / img.h as f32),
        );
        pt.rect_filled(r.expand(3.0), CornerRadius::same(10), alpha(Color32::BLACK, 0.85));
        pt.with_clip_rect(zoom).image(tex.id(), zoom, uv, Color32::WHITE);
        let cell = box_sz / n;
        for i in 1..n as i32 {
            let o = i as f32 * cell;
            pt.line_segment([pos2(zoom.left() + o, zoom.top()), pos2(zoom.left() + o, zoom.bottom())], Stroke::new(0.5, alpha(Color32::BLACK, 0.25)));
            pt.line_segment([pos2(zoom.left(), zoom.top() + o), pos2(zoom.right(), zoom.top() + o)], Stroke::new(0.5, alpha(Color32::BLACK, 0.25)));
        }
        let mid = Rect::from_min_size(zoom.min + vec2((n / 2.0).floor() * cell, (n / 2.0).floor() * cell), vec2(cell, cell));
        pt.rect_stroke(mid, CornerRadius::ZERO, Stroke::new(1.5, p.accent), StrokeKind::Outside);
        pt.rect_stroke(zoom, CornerRadius::same(4), Stroke::new(1.0, p.line2), StrokeKind::Inside);
        let (ix, iy) = (cx.clamp(0.0, img.w as f32 - 1.0) as u32, cy.clamp(0.0, img.h as f32 - 1.0) as u32);
        let k = ((iy * img.w + ix) * 4) as usize;
        let (rr, gg, bb) = (img.rgba[k], img.rgba[k + 1], img.rgba[k + 2]);
        pt.rect_filled(Rect::from_min_size(pos2(zoom.left() + 4.0, zoom.bottom() + 10.0), vec2(14.0, 14.0)), CornerRadius::same(3), Color32::from_rgb(rr, gg, bb));
        pt.text(
            pos2(zoom.left() + 24.0, zoom.bottom() + 17.0),
            Align2::LEFT_CENTER,
            format!("#{rr:02X}{gg:02X}{bb:02X}  {ix},{iy}"),
            mono(11.0),
            Color32::WHITE,
        );
    }

    fn copy_img(img: &Img) {
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = cb.set_image(arboard::ImageData { width: img.w as usize, height: img.h as usize, bytes: img.rgba.clone().into() });
        }
    }

    impl Editor {
        fn push_undo(&mut self, with_base: bool) {
            self.undo.push(State { base: with_base.then(|| self.base.clone()), marks: self.marks.clone() });
            if self.undo.len() > 60 {
                self.undo.remove(0);
            }
            self.redo.clear();
        }

        fn restore(&mut self, from_undo: bool) {
            let (src, dst) = if from_undo { (&mut self.undo, &mut self.redo) } else { (&mut self.redo, &mut self.undo) };
            let Some(st) = src.pop() else { return };
            let cur = State { base: st.base.as_ref().map(|_| self.base.clone()), marks: self.marks.clone() };
            dst.push(cur);
            self.marks = st.marks;
            if let Some(b) = st.base {
                self.base = b;
                self.dirty = true;
            }
        }

        fn commit_text(&mut self) {
            if let Some((pos, text, _)) = self.text.take() {
                if !text.trim().is_empty() {
                    self.push_undo(false);
                    self.marks.push(Mark::Text { pos, text, color: self.color, size: (self.size * 5.0).max(16.0) });
                }
            }
        }

        fn pixelate(&mut self, r: Rect) {
            let r = r.intersect(Rect::from_min_size(Pos2::ZERO, vec2(self.base.w as f32, self.base.h as f32)));
            if r.width() < 2.0 || r.height() < 2.0 {
                return;
            }
            self.push_undo(true);
            let block = ((r.width().min(r.height()) / 8.0).clamp(6.0, 18.0)) as u32;
            let (x0, y0, x1, y1) = (r.left() as u32, r.top() as u32, r.right() as u32, r.bottom() as u32);
            let w = self.base.w;
            let mut by = y0;
            while by < y1 {
                let mut bx = x0;
                while bx < x1 {
                    let (ex, ey) = ((bx + block).min(x1), (by + block).min(y1));
                    let mut acc = [0u64; 3];
                    let mut n = 0u64;
                    for y in by..ey {
                        for x in bx..ex {
                            let k = ((y * w + x) * 4) as usize;
                            acc[0] += self.base.rgba[k] as u64;
                            acc[1] += self.base.rgba[k + 1] as u64;
                            acc[2] += self.base.rgba[k + 2] as u64;
                            n += 1;
                        }
                    }
                    let n = n.max(1);
                    for y in by..ey {
                        for x in bx..ex {
                            let k = ((y * w + x) * 4) as usize;
                            self.base.rgba[k] = (acc[0] / n) as u8;
                            self.base.rgba[k + 1] = (acc[1] / n) as u8;
                            self.base.rgba[k + 2] = (acc[2] / n) as u8;
                        }
                    }
                    bx += block;
                }
                by += block;
            }
            self.dirty = true;
        }

        fn crop(&mut self, r: Rect) {
            let r = r.intersect(Rect::from_min_size(Pos2::ZERO, vec2(self.base.w as f32, self.base.h as f32)));
            if r.width() < 4.0 || r.height() < 4.0 {
                return;
            }
            self.push_undo(true);
            self.base = self.base.crop(r.left() as u32, r.top() as u32, r.width() as u32, r.height() as u32);
            let d = -r.min.to_vec2();
            self.marks.iter_mut().for_each(|m| m.translate(d));
            self.dirty = true;
        }

        /// Flatten the drawings onto the image.
        fn render(&self) -> Img {
            render(&self.base, &self.marks)
        }

        fn copy(&mut self) {
            copy_img(&self.render());
            self.flash = Some(("Copied to clipboard".into(), Instant::now()));
        }

        fn save(&mut self, choose: bool) {
            let s = core().settings();
            let path = if choose {
                match rfd::FileDialog::new().set_directory(&s.shots_dir).set_file_name(format!("Ferrocap_Snip_{}.png", sys::timestamp())).add_filter("PNG", &["png"]).save_file() {
                    Some(p) => p,
                    None => return,
                }
            } else {
                match &self.saved {
                    Some(p) => p.clone(),
                    None => {
                        let _ = std::fs::create_dir_all(&s.shots_dir);
                        sys::unique_path(&s.shots_dir, &format!("Ferrocap_Snip_{}", sys::timestamp()), "png")
                    }
                }
            };
            let img = self.render();
            match image::save_buffer(&path, &img.rgba, img.w, img.h, image::ColorType::Rgba8) {
                Ok(()) => {
                    self.flash = Some((format!("Saved {}", path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()), Instant::now()));
                    self.saved = Some(path);
                    core().library_dirty.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                Err(err) => self.flash = Some((format!("Couldn't save: {err}"), Instant::now())),
            }
        }

        /// Draws the editor; returns true to close.
        fn ui(&mut self, ui: &mut Ui) -> bool {
            let p = pal();
            let full = ui.max_rect();
            let pt = ui.painter().clone();
            pt.rect_filled(full, CornerRadius::ZERO, p.bg);
            let mut close = false;

            // ---------------- keyboard
            let typing = self.text.is_some();
            let mut do_copy = false;
            let mut do_save = false;
            ui.input(|i| {
                let cmd = i.modifiers.command;
                if cmd && i.key_pressed(Key::Z) && !i.modifiers.shift {
                    self.restore(true);
                }
                if cmd && (i.key_pressed(Key::Y) || (i.modifiers.shift && i.key_pressed(Key::Z))) {
                    self.restore(false);
                }
                if cmd && i.key_pressed(Key::C) && !typing {
                    do_copy = true;
                }
                if cmd && i.key_pressed(Key::S) {
                    do_save = true;
                }
                if !cmd && !typing {
                    for t in Tool::ALL {
                        if i.key_pressed(t.key()) {
                            self.tool = t;
                        }
                    }
                    if i.key_pressed(Key::Escape) {
                        close = true;
                    }
                }
            });
            // Ctrl+C on the event list (egui turns it into Event::Copy)
            if !typing && ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy))) {
                do_copy = true;
            }
            if do_copy {
                self.copy();
            }
            if do_save {
                self.save(false);
            }

            // ---------------- toolbar
            let bar = Rect::from_min_size(full.min, vec2(full.width(), 58.0));
            pt.rect_filled(bar, CornerRadius::ZERO, p.bg2);
            pt.line_segment([bar.left_bottom(), bar.right_bottom()], Stroke::new(1.0, p.line));
            let drag = ui.interact(bar, ui.id().with("snipbar"), Sense::click_and_drag());
            if drag.drag_started() {
                ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
            }
            if drag.double_clicked() {
                let m = ui.input(|i| i.viewport().maximized).unwrap_or(false);
                ui.ctx().send_viewport_cmd(ViewportCommand::Maximized(!m));
            }
            let mut x = bar.left() + 14.0;
            let cy = bar.center().y;
            let ir = Rect::from_center_size(pos2(x + 13.0, cy), vec2(26.0, 26.0));
            let (rgba, w, h) = crate::tray::rounded_rgba(26);
            let _ = (rgba, w, h);
            draw_icon(&pt, Icon::Editor, ir.center(), p.accent, 0.8);
            x += 34.0;
            pt.text(pos2(x, cy - 8.0), Align2::LEFT_CENTER, "Snip", semibold(14.0), p.text);
            pt.text(pos2(x, cy + 9.0), Align2::LEFT_CENTER, format!("{} × {}", self.base.w, self.base.h), body(11.0), p.dim);
            x += 74.0;
            // tools
            for t in Tool::ALL {
                let r = Rect::from_center_size(pos2(x + 18.0, cy), vec2(36.0, 36.0));
                let resp = ui.interact(r, ui.id().with(("tool", t as u8)), Sense::click());
                let on = self.tool == t;
                let hv = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.1);
                if on {
                    pt.rect_filled(r, CornerRadius::same(9), p.text);
                } else if hv > 0.0 {
                    pt.rect_filled(r, CornerRadius::same(9), alpha(p.panel3, hv));
                }
                draw_icon(&pt, t.icon(), r.center(), if on { p.bg } else { p.text }, 0.75);
                if resp.on_hover_text(t.name()).clicked() {
                    if self.tool == Tool::Text {
                        self.commit_text();
                    }
                    self.tool = t;
                }
                x += 40.0;
            }
            x += 8.0;
            pt.line_segment([pos2(x, cy - 14.0), pos2(x, cy + 14.0)], Stroke::new(1.0, p.line2));
            x += 14.0;
            // colours
            for (i, c) in PALETTE.iter().enumerate() {
                let r = Rect::from_center_size(pos2(x + 11.0, cy), vec2(22.0, 22.0));
                let resp = ui.interact(r, ui.id().with(("col", i)), Sense::click());
                let on = self.color == *c;
                if on {
                    pt.circle_stroke(r.center(), 13.5, Stroke::new(2.0, p.text));
                }
                pt.circle_filled(r.center(), if resp.hovered() { 11.0 } else { 10.0 }, *c);
                if *c == PALETTE[7] {
                    pt.circle_stroke(r.center(), 10.0, Stroke::new(1.0, p.line2));
                }
                if resp.clicked() {
                    self.color = *c;
                }
                x += 28.0;
            }
            x += 10.0;
            // size slider
            let sr = Rect::from_min_size(pos2(x, cy - 10.0), vec2(96.0, 20.0));
            ui.scope_builder(egui::UiBuilder::new().max_rect(sr), |ui| {
                value_slider(ui, &mut self.size, 1.0, 16.0, 96.0);
            });
            pt.circle_filled(pos2(sr.right() + 16.0, cy), (self.size * 0.7).clamp(1.5, 10.0), self.color);
            // actions (right side, right to left)
            let mut rx = bar.right() - 10.0;
            let acts: [(&str, Icon, u8); 6] =
                [("Close  Esc", Icon::Plus, 0), ("Save as…", Icon::Folder, 1), ("Save  Ctrl+S", Icon::Save, 2), ("Copy  Ctrl+C", Icon::Copy, 3), ("Redo  Ctrl+Y", Icon::Redo, 4), ("Undo  Ctrl+Z", Icon::Undo, 5)];
            for (tip, icon, k) in acts {
                let r = Rect::from_center_size(pos2(rx - 18.0, cy), vec2(36.0, 36.0));
                rx -= 40.0;
                let resp = ui.interact(r, ui.id().with(("act", k)), Sense::click());
                let enabled = match k {
                    4 => !self.redo.is_empty(),
                    5 => !self.undo.is_empty(),
                    _ => true,
                };
                let hv = ui.ctx().animate_bool_with_time(resp.id, resp.hovered() && enabled, 0.1);
                let fill = if k == 0 { mix(p.bg2, Color32::from_rgb(0xd9, 0x34, 0x2b), hv) } else if k == 3 { mix(p.accent, mix(p.accent, Color32::WHITE, 0.2), hv) } else { mix(p.bg2, p.panel3, hv) };
                pt.rect_filled(r, CornerRadius::same(9), fill);
                let col = if !enabled { p.dim } else if k == 3 { on(p.accent) } else { p.text };
                if k == 0 {
                    let c = r.center();
                    pt.line_segment([c + vec2(-5.0, -5.0), c + vec2(5.0, 5.0)], Stroke::new(1.5, col));
                    pt.line_segment([c + vec2(-5.0, 5.0), c + vec2(5.0, -5.0)], Stroke::new(1.5, col));
                } else {
                    draw_icon(&pt, icon, r.center(), col, 0.75);
                }
                if resp.on_hover_text(tip).clicked() && enabled {
                    match k {
                        0 => close = true,
                        1 => self.save(true),
                        2 => self.save(false),
                        3 => self.copy(),
                        4 => self.restore(false),
                        _ => self.restore(true),
                    }
                }
            }

            // ---------------- canvas
            let area = Rect::from_min_max(pos2(full.left(), bar.bottom()), pos2(full.right(), full.bottom() - 32.0));
            // subtle checker so transparent / dark edges read
            pt.rect_filled(area, CornerRadius::ZERO, p.bg);
            let ppp = ui.ctx().pixels_per_point();
            let avail = area.shrink(22.0);
            let scale = (avail.width() / self.base.w as f32).min(avail.height() / self.base.h as f32).min(1.0 / ppp * 2.0).min(1.0 / ppp * if self.base.w as f32 / ppp < avail.width() { 1.0 } else { 2.0 });
            let scale = scale.max(0.02);
            let disp = vec2(self.base.w as f32 * scale, self.base.h as f32 * scale);
            let canvas = Rect::from_center_size(avail.center(), disp);
            pt.rect_filled(canvas.expand(1.0).translate(vec2(0.0, 6.0)), CornerRadius::same(4), alpha(Color32::BLACK, 0.35));
            if let Some(t) = &self.tex {
                pt.image(t.id(), canvas, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
            }
            let to_img = |q: Pos2| pos2((q.x - canvas.left()) / scale, (q.y - canvas.top()) / scale);
            let to_scr = |q: Pos2| pos2(canvas.left() + q.x * scale, canvas.top() + q.y * scale);
            let cpt = pt.with_clip_rect(canvas);
            for m in self.marks.iter().chain(self.drawing.iter()) {
                paint_mark(&cpt, m, &to_scr, scale);
            }

            let resp = ui.interact(area, ui.id().with("canvas"), Sense::click_and_drag());
            let hover = resp.hover_pos();
            let press = ui.input(|i| i.pointer.press_origin());
            if resp.drag_started() || (resp.clicked() && self.tool == Tool::Text) || (resp.clicked() && self.tool == Tool::Eraser) {
                if let Some(o) = press {
                    let a = to_img(o);
                    let w = self.size;
                    match self.tool {
                        Tool::Pen => self.drawing = Some(Mark::Pen { pts: vec![a], color: self.color, w, hl: false }),
                        Tool::Highlighter => self.drawing = Some(Mark::Pen { pts: vec![a], color: self.color, w: w * 4.0, hl: true }),
                        Tool::Arrow => self.drawing = Some(Mark::Line { a, b: a, color: self.color, w, arrow: true }),
                        Tool::Line => self.drawing = Some(Mark::Line { a, b: a, color: self.color, w, arrow: false }),
                        Tool::Rect => self.drawing = Some(Mark::Rect { r: Rect::from_two_pos(a, a), color: self.color, w }),
                        Tool::Ellipse => self.drawing = Some(Mark::Ellipse { r: Rect::from_two_pos(a, a), color: self.color, w }),
                        Tool::Pixelate | Tool::Crop => self.drag_rect = Some((a, a)),
                        Tool::Text => {
                            self.commit_text();
                            if canvas.contains(o) {
                                self.text = Some((a, String::new(), true));
                            }
                        }
                        Tool::Eraser => {
                            if let Some(i) = self.marks.iter().rposition(|m| m.bounds().contains(a)) {
                                self.push_undo(false);
                                self.marks.remove(i);
                            }
                        }
                    }
                }
            }
            if let Some(q) = ui.ctx().pointer_interact_pos().filter(|_| resp.dragged()) {
                let b = to_img(q);
                match &mut self.drawing {
                    Some(Mark::Pen { pts, .. }) => {
                        if pts.last().map(|l| l.distance(b) > 0.8).unwrap_or(true) {
                            pts.push(b);
                        }
                    }
                    Some(Mark::Line { b: lb, .. }) => *lb = b,
                    Some(Mark::Rect { r, .. }) | Some(Mark::Ellipse { r, .. }) => {
                        let a = to_img(press.unwrap_or(q));
                        *r = Rect::from_two_pos(a, b);
                    }
                    _ => {}
                }
                if let Some((a, _)) = self.drag_rect {
                    self.drag_rect = Some((a, b));
                }
            }
            if resp.drag_stopped() {
                if let Some(m) = self.drawing.take() {
                    let tiny = m.bounds().width() < 2.0 && m.bounds().height() < 2.0 && !matches!(m, Mark::Pen { .. });
                    if !tiny {
                        self.push_undo(false);
                        self.marks.push(m);
                    }
                }
                if let Some((a, b)) = self.drag_rect.take() {
                    let r = Rect::from_two_pos(a, b);
                    if self.tool == Tool::Pixelate {
                        self.pixelate(r);
                    } else if self.tool == Tool::Crop {
                        self.crop(r);
                    }
                }
            }
            if let Some((a, b)) = self.drag_rect {
                let r = Rect::from_min_max(to_scr(Rect::from_two_pos(a, b).min), to_scr(Rect::from_two_pos(a, b).max));
                let col = if self.tool == Tool::Crop { p.text } else { p.accent };
                if self.tool == Tool::Crop {
                    let dim = alpha(Color32::BLACK, 0.55);
                    cpt.rect_filled(Rect::from_min_max(canvas.min, pos2(canvas.right(), r.top())), CornerRadius::ZERO, dim);
                    cpt.rect_filled(Rect::from_min_max(pos2(canvas.left(), r.bottom()), canvas.max), CornerRadius::ZERO, dim);
                    cpt.rect_filled(Rect::from_min_max(pos2(canvas.left(), r.top()), pos2(r.left(), r.bottom())), CornerRadius::ZERO, dim);
                    cpt.rect_filled(Rect::from_min_max(pos2(r.right(), r.top()), pos2(canvas.right(), r.bottom())), CornerRadius::ZERO, dim);
                } else {
                    cpt.rect_filled(r, CornerRadius::ZERO, alpha(col, 0.15));
                }
                dashed(&cpt, r.left_top(), r.right_top(), 5.0, 4.0, Stroke::new(1.5, col));
                dashed(&cpt, r.right_top(), r.right_bottom(), 5.0, 4.0, Stroke::new(1.5, col));
                dashed(&cpt, r.right_bottom(), r.left_bottom(), 5.0, 4.0, Stroke::new(1.5, col));
                dashed(&cpt, r.left_bottom(), r.left_top(), 5.0, 4.0, Stroke::new(1.5, col));
            }
            // eraser hover highlight
            if self.tool == Tool::Eraser {
                if let Some(h) = hover {
                    let a = to_img(h);
                    if let Some(m) = self.marks.iter().rev().find(|m| m.bounds().contains(a)) {
                        let b = m.bounds();
                        let r = Rect::from_min_max(to_scr(b.min), to_scr(b.max));
                        dashed(&cpt, r.left_top(), r.right_top(), 4.0, 3.0, Stroke::new(1.5, p.warn));
                        dashed(&cpt, r.right_top(), r.right_bottom(), 4.0, 3.0, Stroke::new(1.5, p.warn));
                        dashed(&cpt, r.right_bottom(), r.left_bottom(), 4.0, 3.0, Stroke::new(1.5, p.warn));
                        dashed(&cpt, r.left_bottom(), r.left_top(), 4.0, 3.0, Stroke::new(1.5, p.warn));
                    }
                }
            }
            // cursor
            if resp.hovered() {
                ui.ctx().set_cursor_icon(match self.tool {
                    Tool::Text => egui::CursorIcon::Text,
                    Tool::Eraser => egui::CursorIcon::PointingHand,
                    _ => egui::CursorIcon::Crosshair,
                });
                if matches!(self.tool, Tool::Pen | Tool::Highlighter) {
                    if let Some(h) = hover {
                        let r = if self.tool == Tool::Highlighter { self.size * 2.0 } else { self.size / 2.0 } * scale;
                        pt.circle_stroke(h, r.max(2.0), Stroke::new(1.0, alpha(Color32::WHITE, 0.8)));
                    }
                }
            }

            // text being typed
            let mut commit = false;
            if let Some((pos, text, focus)) = self.text.as_mut() {
                let size = (self.size * 5.0).max(16.0) * scale;
                let at = to_scr(*pos);
                let w = (text.chars().count().max(6) as f32 * size * 0.62 + 30.0).min(canvas.right() - at.x).max(80.0);
                let r = Rect::from_min_size(at, vec2(w, size * 1.4 * text.lines().count().max(1) as f32));
                pt.rect_stroke(r.expand(4.0), CornerRadius::same(4), Stroke::new(1.0, alpha(Color32::WHITE, 0.6)), StrokeKind::Outside);
                let te = egui::TextEdit::multiline(text)
                    .font(semibold(size))
                    .text_color(self.color)
                    .frame(egui::Frame::NONE)
                    .desired_width(w)
                    .hint_text("Type, Enter to place");
                let out = ui.put(r, te);
                if *focus {
                    out.request_focus();
                    *focus = false;
                }
                if ui.input(|i| i.key_pressed(Key::Enter) && !i.modifiers.shift) {
                    if text.ends_with('\n') {
                        text.pop();
                    }
                    commit = true;
                }
                if ui.input(|i| i.key_pressed(Key::Escape)) {
                    text.clear();
                    commit = true;
                }
            }
            if commit {
                self.commit_text();
            }

            // ---------------- status bar
            let sb = Rect::from_min_max(pos2(full.left(), full.bottom() - 32.0), full.max);
            pt.rect_filled(sb, CornerRadius::ZERO, p.bg2);
            pt.line_segment([sb.left_top(), sb.right_top()], Stroke::new(1.0, p.line));
            let hint = match self.tool {
                Tool::Text => "Click to type, Enter to place",
                Tool::Pixelate => "Drag over names, chat or anything private",
                Tool::Crop => "Drag the area to keep",
                Tool::Eraser => "Click a drawing to remove it",
                _ => "Drag to draw · Ctrl+Z undo · Ctrl+C copy · Ctrl+S save",
            };
            pt.text(pos2(sb.left() + 14.0, sb.center().y), Align2::LEFT_CENTER, hint, body(12.0), p.muted);
            if let Some((msg, at)) = &self.flash {
                let age = at.elapsed().as_secs_f32();
                if age < 3.0 {
                    let a = (3.0 - age).min(1.0);
                    pt.text(pos2(sb.right() - 14.0, sb.center().y), Align2::RIGHT_CENTER, msg, semibold(12.0), alpha(p.good, a));
                    ui.ctx().request_repaint();
                }
            } else if let Some(sv) = &self.saved {
                pt.text(pos2(sb.right() - 14.0, sb.center().y), Align2::RIGHT_CENTER, sv.display().to_string(), body(11.5), p.dim);
            }
            let _ = &self.title;
            close
        }
    }

    fn paint_mark(pt: &egui::Painter, m: &Mark, to_scr: &dyn Fn(Pos2) -> Pos2, scale: f32) {
        match m {
            Mark::Pen { pts, color, w, hl } => {
                let col = if *hl { alpha(*color, 0.38) } else { *color };
                let pts: Vec<Pos2> = pts.iter().map(|q| to_scr(*q)).collect();
                if pts.len() == 1 {
                    pt.circle_filled(pts[0], w * scale / 2.0, col);
                } else {
                    pt.add(egui::Shape::line(pts, Stroke::new(w * scale, col)));
                }
            }
            Mark::Line { a, b, color, w, arrow } => {
                let (a, b) = (to_scr(*a), to_scr(*b));
                let ww = w * scale;
                if *arrow {
                    let d = b - a;
                    let len = d.length();
                    if len > 1.0 {
                        let dir = d / len;
                        let head = (ww * 4.5).max(10.0).min(len * 0.6);
                        let n = vec2(-dir.y, dir.x);
                        let base = b - dir * head;
                        pt.line_segment([a, base + dir * (head * 0.3)], Stroke::new(ww, *color));
                        pt.add(egui::Shape::convex_polygon(vec![b, base + n * head * 0.55, base - n * head * 0.55], *color, Stroke::NONE));
                    }
                } else {
                    pt.line_segment([a, b], Stroke::new(ww, *color));
                }
            }
            Mark::Rect { r, color, w } => {
                pt.rect_stroke(Rect::from_min_max(to_scr(r.min), to_scr(r.max)), CornerRadius::same(2), Stroke::new(w * scale, *color), StrokeKind::Middle);
            }
            Mark::Ellipse { r, color, w } => {
                let r = Rect::from_min_max(to_scr(r.min), to_scr(r.max));
                let pts: Vec<Pos2> = (0..=72)
                    .map(|i| {
                        let t = i as f32 / 72.0 * std::f32::consts::TAU;
                        r.center() + vec2(t.cos() * r.width() / 2.0, t.sin() * r.height() / 2.0)
                    })
                    .collect();
                pt.add(egui::Shape::line(pts, Stroke::new(w * scale, *color)));
            }
            Mark::Text { pos, text, color, size } => {
                let at = to_scr(*pos);
                for (i, line) in text.lines().enumerate() {
                    pt.text(at + vec2(0.0, i as f32 * size * 1.25 * scale), Align2::LEFT_TOP, line, semibold(size * scale), *color);
                }
            }
        }
    }

    // ------------------------------------------------------------------ export

    fn font() -> Option<&'static ab_glyph::FontVec> {
        use std::sync::OnceLock;
        static F: OnceLock<Option<ab_glyph::FontVec>> = OnceLock::new();
        F.get_or_init(|| {
            for f in ["seguisb.ttf", "segoeuib.ttf", "segoeui.ttf", "arial.ttf"] {
                if let Ok(b) = std::fs::read(format!("C:\\Windows\\Fonts\\{f}")) {
                    if let Ok(font) = ab_glyph::FontVec::try_from_vec(b) {
                        return Some(font);
                    }
                }
            }
            None
        })
        .as_ref()
    }

    fn render(base: &Img, marks: &[Mark]) -> Img {
        use tiny_skia::*;
        let Some(size) = IntSize::from_wh(base.w, base.h) else { return base.clone() };
        let Some(mut pix) = Pixmap::from_vec(base.rgba.clone(), size) else { return base.clone() };
        let paint_for = |c: Color32, a: f32, mult: bool| {
            let mut p = Paint::default();
            p.set_color_rgba8(c.r(), c.g(), c.b(), (a * 255.0) as u8);
            p.anti_alias = true;
            if mult {
                p.blend_mode = BlendMode::Multiply;
            }
            p
        };
        let stroke = |w: f32| Stroke { width: w, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Default::default() };
        for m in marks {
            match m {
                Mark::Pen { pts, color, w, hl } => {
                    let mut pb = PathBuilder::new();
                    pb.move_to(pts[0].x, pts[0].y);
                    if pts.len() == 1 {
                        pb.line_to(pts[0].x + 0.01, pts[0].y);
                    }
                    for q in &pts[1..] {
                        pb.line_to(q.x, q.y);
                    }
                    if let Some(path) = pb.finish() {
                        let paint = if *hl { paint_for(*color, 0.55, true) } else { paint_for(*color, 1.0, false) };
                        pix.stroke_path(&path, &paint, &stroke(*w), Transform::identity(), None);
                    }
                }
                Mark::Line { a, b, color, w, arrow } => {
                    let d = *b - *a;
                    let len = d.length();
                    if len < 0.5 {
                        continue;
                    }
                    let dir = d / len;
                    let head = (w * 4.5).max(10.0).min(len * 0.6);
                    let end = if *arrow { *b - dir * (head * 0.7) } else { *b };
                    let mut pb = PathBuilder::new();
                    pb.move_to(a.x, a.y);
                    pb.line_to(end.x, end.y);
                    let paint = paint_for(*color, 1.0, false);
                    if let Some(path) = pb.finish() {
                        pix.stroke_path(&path, &paint, &stroke(*w), Transform::identity(), None);
                    }
                    if *arrow {
                        let n = vec2(-dir.y, dir.x);
                        let base = *b - dir * head;
                        let (l, r) = (base + n * head * 0.55, base - n * head * 0.55);
                        let mut pb = PathBuilder::new();
                        pb.move_to(b.x, b.y);
                        pb.line_to(l.x, l.y);
                        pb.line_to(r.x, r.y);
                        pb.close();
                        if let Some(path) = pb.finish() {
                            pix.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
                        }
                    }
                }
                Mark::Rect { r, color, w } => {
                    if let Some(rr) = tiny_skia::Rect::from_xywh(r.left(), r.top(), r.width().max(1.0), r.height().max(1.0)) {
                        let path = PathBuilder::from_rect(rr);
                        pix.stroke_path(&path, &paint_for(*color, 1.0, false), &stroke(*w), Transform::identity(), None);
                    }
                }
                Mark::Ellipse { r, color, w } => {
                    if let Some(rr) = tiny_skia::Rect::from_xywh(r.left(), r.top(), r.width().max(1.0), r.height().max(1.0)) {
                        if let Some(path) = PathBuilder::from_oval(rr) {
                            pix.stroke_path(&path, &paint_for(*color, 1.0, false), &stroke(*w), Transform::identity(), None);
                        }
                    }
                }
                Mark::Text { pos, text, color, size } => {
                    if let Some(f) = font() {
                        draw_text(&mut pix, f, *pos, text, *color, *size);
                    }
                }
            }
        }
        Img { rgba: pix.take(), w: base.w, h: base.h }
    }

    /// Dev self-test: "in.png|out.png" -> draws every annotation type and saves the result.
    pub fn render_test(spec: &str) {
        let Some((inp, out)) = spec.split_once('|') else { return };
        let Ok(img) = image::open(inp) else { return };
        let img = img.to_rgba8();
        let (w, h) = img.dimensions();
        let base = Img { rgba: img.into_raw(), w, h };
        let (fw, fh) = (w as f32, h as f32);
        let marks = vec![
            Mark::Pen { pts: (0..40).map(|i| pos2(fw * 0.08 + i as f32 * 6.0, fh * 0.2 + (i as f32 * 0.4).sin() * 20.0)).collect(), color: PALETTE[0], w: 6.0, hl: false },
            Mark::Pen { pts: vec![pos2(fw * 0.1, fh * 0.42), pos2(fw * 0.45, fh * 0.42)], color: PALETTE[2], w: 22.0, hl: true },
            Mark::Line { a: pos2(fw * 0.55, fh * 0.75), b: pos2(fw * 0.8, fh * 0.35), color: PALETTE[1], w: 6.0, arrow: true },
            Mark::Line { a: pos2(fw * 0.1, fh * 0.9), b: pos2(fw * 0.4, fh * 0.9), color: PALETTE[4], w: 4.0, arrow: false },
            Mark::Rect { r: Rect::from_min_size(pos2(fw * 0.55, fh * 0.1), vec2(fw * 0.3, fh * 0.15)), color: PALETTE[3], w: 5.0 },
            Mark::Ellipse { r: Rect::from_min_size(pos2(fw * 0.1, fh * 0.55), vec2(fw * 0.3, fh * 0.25)), color: PALETTE[5], w: 5.0 },
            Mark::Text { pos: pos2(fw * 0.5, fh * 0.82), text: "Ferrocap Snip ✓".into(), color: Color32::WHITE, size: 40.0 },
        ];
        let mut ed_base = base.clone();
        // pixelate a corner like the tool does
        let block = 14u32;
        for by in (0..(h / 4)).step_by(block as usize) {
            for bx in ((w * 3 / 4)..w).step_by(block as usize) {
                let k = ((by * w + bx) * 4) as usize;
                let c = [ed_base.rgba[k], ed_base.rgba[k + 1], ed_base.rgba[k + 2]];
                for y in by..(by + block).min(h / 4) {
                    for x in bx..(bx + block).min(w) {
                        let k = ((y * w + x) * 4) as usize;
                        ed_base.rgba[k..k + 3].copy_from_slice(&c);
                    }
                }
            }
        }
        let r = render(&ed_base, &marks);
        let _ = image::save_buffer(out, &r.rgba, r.w, r.h, image::ColorType::Rgba8);
    }

    fn draw_text(pix: &mut tiny_skia::Pixmap, font: &ab_glyph::FontVec, pos: Pos2, text: &str, color: Color32, size: f32) {
        use ab_glyph::{Font, ScaleFont, point};
        let scaled = font.as_scaled(size);
        let (w, h) = (pix.width() as i32, pix.height() as i32);
        let data = pix.data_mut();
        for (li, line) in text.lines().enumerate() {
            let mut x = pos.x;
            let y = pos.y + li as f32 * size * 1.25 + scaled.ascent();
            let mut prev = None;
            for ch in line.chars() {
                let id = font.glyph_id(ch);
                if let Some(pv) = prev {
                    x += scaled.kern(pv, id);
                }
                let g = id.with_scale_and_position(size, point(x, y));
                if let Some(og) = font.outline_glyph(g) {
                    let b = og.px_bounds();
                    og.draw(|gx, gy, cov| {
                        let px = b.min.x as i32 + gx as i32;
                        let py = b.min.y as i32 + gy as i32;
                        if px < 0 || py < 0 || px >= w || py >= h {
                            return;
                        }
                        let k = ((py * w + px) * 4) as usize;
                        let a = cov.clamp(0.0, 1.0);
                        for (i, c) in [color.r(), color.g(), color.b()].iter().enumerate() {
                            data[k + i] = (data[k + i] as f32 * (1.0 - a) + *c as f32 * a) as u8;
                        }
                        data[k + 3] = 255;
                    });
                }
                x += scaled.h_advance(id);
                prev = Some(id);
            }
        }
    }

    #[allow(dead_code)]
    fn _unused(_: ToastKind) {}
}
