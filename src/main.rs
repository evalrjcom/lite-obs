// =============================================================================
//  litecast_v1.0.0 — LiteCast Studio
//  A lightweight OBS-style recorder / live streamer in Rust + FLTK.
//
//  Architecture (one process, ffmpeg used as an external worker binary):
//
//    [ffmpeg capture proc] --raw BGRA--> reader thread --> FrameSlot ─┐
//    [ffmpeg capture proc] --raw BGRA--> reader thread --> FrameSlot ─┤
//    [color / image source] ---------------------------> FrameSlot ─┤
//                                                                     v
//                        compositor thread (fixed FPS, alpha blend, z-order)
//                           │                     │
//                 canvas slot (Arc)          preview buffer (RGB, downscaled)
//                    │        │                     │
//       writer thread    writer thread         FLTK preview widget
//            │                │
//   [ffmpeg encoder]   [ffmpeg encoder]
//     record file        RTMP/SRT stream          (+ ffmpeg-native audio in)
//
//  Build:   cargo build --release      (needs cmake + X11 dev libs on Linux)
//  Run:     ./litecast [--profile FILE] [--record SECONDS] [--version]
//  Runtime: ffmpeg >= 4.4 on PATH (or set in Settings).
//
//  Changelog
//   1.0.0  Initial release: scenes, display/webcam/media/image/color sources,
//          z-order, opacity, drag-move / shift-drag-resize in preview,
//          recording (mkv/mp4/mov/flv/ts), RTMP/RTMPS/SRT streaming with
//          auto-reconnect, mic + desktop audio, HW encoders, screenshots,
//          JSON profiles (0600), headless --record mode.
// =============================================================================
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use fltk::{
    app,
    browser::HoldBrowser,
    button::{Button, CheckButton},
    dialog, draw,
    enums::{Align, Color, ColorDepth, Event, Font, FrameType, Key, Shortcut},
    frame::Frame,
    group::Flex,
    image::RgbImage,
    input::{Input, IntInput, SecretInput},
    menu::{Choice, MenuBar, MenuFlag},
    prelude::*,
    text::{TextBuffer, TextDisplay},
    valuator::HorValueSlider,
    window::Window,
};
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdin, Command, Stdio},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
        Arc, Mutex, MutexGuard,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

// ----------------------------------------------------------------------------
// Constants & limits (hard caps keep a hostile/corrupt profile from exhausting RAM)
// ----------------------------------------------------------------------------
const APP_NAME: &str = "LiteCast Studio";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION"); // single source of truth: Cargo.toml
const MAX_W: u32 = 7680;
const MAX_H: u32 = 4320;
const MIN_DIM: u32 = 2;
const MAX_FPS: u32 = 120;
const MAX_SCENES: usize = 64;
const MAX_SOURCES: usize = 64;
const MAX_PROFILE_BYTES: u64 = 4 << 20;
const LOG_CAP: usize = 2000;
const LOG_VIEW_MAX_BYTES: i32 = 256 * 1024;
const STREAM_MAX_RETRIES: u32 = 6;

const ENCODERS: &[&str] = &[
    "libx264",
    "h264_nvenc",
    "h264_qsv",
    "h264_amf",
    "h264_vaapi",
    "h264_videotoolbox",
];
const X264_PRESETS: &[&str] = &[
    "ultrafast", "superfast", "veryfast", "faster", "fast", "medium",
];
const REC_FORMATS: &[&str] = &["mkv", "mp4", "mov", "flv", "ts"];
const AUDIO_APIS: &[&str] = &["none", "pulse", "alsa", "dshow", "avfoundation"];

/// Internal pixel format. BGRA matches x11grab/gdigrab native order, so the
/// capture side does no channel swizzle (only alpha fill).
const PIX: &str = "bgra";

type FrameSlot = Arc<Mutex<Option<Arc<Vec<u8>>>>>;

/// Poison-tolerant lock: a panicking worker must never take the UI down.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn sleep_interruptible(d: Duration, stop: &AtomicBool) {
    let end = Instant::now() + d;
    while !stop.load(Relaxed) {
        let now = Instant::now();
        if now >= end {
            break;
        }
        thread::sleep((end - now).min(Duration::from_millis(100)));
    }
}

fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn even(v: u32) -> u32 {
    v & !1
}

// ----------------------------------------------------------------------------
// Logger (thread-safe ring buffer with secret redaction)
// ----------------------------------------------------------------------------
#[derive(Clone, Default)]
struct Logger {
    inner: Arc<Mutex<LogInner>>,
}
#[derive(Default)]
struct LogInner {
    lines: VecDeque<(u64, String)>,
    seq: u64,
    secrets: Vec<String>,
}
impl Logger {
    fn log(&self, msg: impl AsRef<str>) {
        let mut g = lock(&self.inner);
        let mut line = truncate_chars(msg.as_ref(), 600);
        // Redact stream keys / credentials: ffmpeg echoes the output URL in errors.
        for s in &g.secrets {
            if s.len() >= 4 && line.contains(s.as_str()) {
                line = line.replace(s.as_str(), "****");
            }
        }
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let line = format!(
            "{:02}:{:02}:{:02} {}",
            (secs / 3600) % 24,
            (secs / 60) % 60,
            secs % 60,
            line
        );
        if cfg!(debug_assertions) {
            eprintln!("{line}");
        }
        g.seq += 1;
        let seq = g.seq;
        g.lines.push_back((seq, line));
        while g.lines.len() > LOG_CAP {
            g.lines.pop_front();
        }
    }
    fn set_secrets(&self, v: Vec<String>) {
        lock(&self.inner).secrets = v.into_iter().filter(|s| s.len() >= 4).collect();
    }
    fn since(&self, seq: u64) -> (u64, Vec<String>) {
        let g = lock(&self.inner);
        let v = g
            .lines
            .iter()
            .filter(|(s, _)| *s > seq)
            .map(|(_, l)| l.clone())
            .collect();
        (g.seq, v)
    }
}

// ----------------------------------------------------------------------------
// Profile model (serde)
// ----------------------------------------------------------------------------
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
struct Settings {
    canvas_w: u32,
    canvas_h: u32,
    fps: u32,
    encoder: String,
    x264_preset: String,
    video_kbps: u32,
    audio_kbps: u32,
    keyint_sec: u32,
    rec_dir: String,
    rec_format: String,
    stream_url: String,
    stream_key: String,
    audio_api: String,
    mic_device: String,
    desktop_device: String,
    ffmpeg_path: String,
    vaapi_device: String,
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

impl Default for Settings {
    fn default() -> Self {
        let videos = home_dir().join("Videos");
        let rec_dir = if videos.is_dir() { videos } else { home_dir() };
        let (api, mic, desk) = if cfg!(windows) {
            ("dshow", "", "")
        } else if cfg!(target_os = "macos") {
            ("avfoundation", "", "")
        } else {
            ("pulse", "default", "@DEFAULT_MONITOR@")
        };
        Self {
            canvas_w: 1920,
            canvas_h: 1080,
            fps: 30,
            encoder: "libx264".into(),
            x264_preset: "veryfast".into(),
            video_kbps: 4500,
            audio_kbps: 160,
            keyint_sec: 2,
            rec_dir: rec_dir.to_string_lossy().into_owned(),
            rec_format: "mkv".into(),
            stream_url: "rtmp://live.twitch.tv/app".into(),
            stream_key: String::new(),
            audio_api: api.into(),
            mic_device: mic.into(),
            desktop_device: desk.into(),
            ffmpeg_path: "ffmpeg".into(),
            vaapi_device: "/dev/dri/renderD128".into(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SourceKind {
    Display { target: String, x: i32, y: i32, w: u32, h: u32, cursor: bool },
    Webcam { device: String, size: String, fps: u32 },
    Media { path: String, looping: bool },
    Image { path: String },
    Color { r: u8, g: u8, b: u8 },
}
impl SourceKind {
    fn label(&self) -> &'static str {
        match self {
            SourceKind::Display { .. } => "Display",
            SourceKind::Webcam { .. } => "Webcam",
            SourceKind::Media { .. } => "Media",
            SourceKind::Image { .. } => "Image",
            SourceKind::Color { .. } => "Color",
        }
    }
    fn index(&self) -> i32 {
        match self {
            SourceKind::Display { .. } => 0,
            SourceKind::Webcam { .. } => 1,
            SourceKind::Media { .. } => 2,
            SourceKind::Image { .. } => 3,
            SourceKind::Color { .. } => 4,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Source {
    id: u64,
    name: String,
    kind: SourceKind,
    visible: bool,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    opacity: u8,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Scene {
    name: String,
    sources: Vec<Source>, // index 0 = bottom layer
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
struct Profile {
    format: u32,
    settings: Settings,
    scenes: Vec<Scene>,
    current: usize,
}
impl Default for Profile {
    fn default() -> Self {
        let s = Settings::default();
        let (sx, sy, sw, sh) = primary_screen_px();
        let (fx, fy, fw, fh) = fit_rect(sw.max(2) as u32, sh.max(2) as u32, s.canvas_w as i32, s.canvas_h as i32);
        let disp = Source {
            id: 1,
            name: "Display Capture".into(),
            kind: SourceKind::Display {
                target: default_display_target(),
                x: sx,
                y: sy,
                w: sw.max(2) as u32,
                h: sh.max(2) as u32,
                cursor: true,
            },
            visible: true,
            x: fx,
            y: fy,
            w: fw.max(2) as u32,
            h: fh.max(2) as u32,
            opacity: 255,
        };
        Self {
            format: 1,
            settings: s,
            scenes: vec![Scene { name: "Scene 1".into(), sources: vec![disp] }],
            current: 0,
        }
    }
}

fn default_display_target() -> String {
    if cfg!(windows) {
        "desktop".into()
    } else if cfg!(target_os = "macos") {
        "1".into()
    } else {
        std::env::var("DISPLAY").ok().filter(|d| !d.is_empty()).unwrap_or_else(|| ":0.0".into())
    }
}
fn default_webcam_target() -> String {
    if cfg!(windows) {
        String::new()
    } else if cfg!(target_os = "macos") {
        "0".into()
    } else {
        "/dev/video0".into()
    }
}

/// FLTK reports logical coords; capture APIs want physical pixels.
fn screens_px() -> Vec<(i32, i32, i32, i32)> {
    let n = app::screen_count().max(0);
    (0..n)
        .map(|i| {
            let (x, y, w, h) = app::screen_xywh(i);
            let s = app::screen_scale(i).max(0.1) as f64;
            let f = |v: i32| (v as f64 * s).round() as i32;
            (f(x), f(y), f(w), f(h))
        })
        .collect()
}
fn primary_screen_px() -> (i32, i32, i32, i32) {
    screens_px().first().copied().unwrap_or((0, 0, 1920, 1080))
}

/// Clamp/repair everything coming from disk. Never trust a profile file.
fn sanitize(p: &mut Profile) {
    let d = Settings::default();
    let s = &mut p.settings;
    s.canvas_w = even(s.canvas_w.clamp(160, MAX_W));
    s.canvas_h = even(s.canvas_h.clamp(120, MAX_H));
    s.fps = s.fps.clamp(1, MAX_FPS);
    s.video_kbps = s.video_kbps.clamp(100, 200_000);
    s.audio_kbps = s.audio_kbps.clamp(32, 512);
    s.keyint_sec = s.keyint_sec.clamp(1, 20);
    if !ENCODERS.contains(&s.encoder.as_str()) {
        s.encoder = d.encoder.clone();
    }
    if !X264_PRESETS.contains(&s.x264_preset.as_str()) {
        s.x264_preset = d.x264_preset.clone();
    }
    if !REC_FORMATS.contains(&s.rec_format.as_str()) {
        s.rec_format = d.rec_format.clone();
    }
    if !AUDIO_APIS.contains(&s.audio_api.as_str()) {
        s.audio_api = d.audio_api.clone();
    }
    if s.ffmpeg_path.trim().is_empty() {
        s.ffmpeg_path = d.ffmpeg_path.clone();
    }
    for f in [
        &mut s.rec_dir,
        &mut s.stream_url,
        &mut s.stream_key,
        &mut s.mic_device,
        &mut s.desktop_device,
        &mut s.ffmpeg_path,
        &mut s.vaapi_device,
    ] {
        *f = truncate_chars(&f.replace('\0', ""), 1024);
    }
    p.scenes.truncate(MAX_SCENES);
    if p.scenes.is_empty() {
        p.scenes.push(Scene { name: "Scene 1".into(), sources: vec![] });
    }
    let mut seen = HashSet::new();
    let mut next_id = p.scenes.iter().flat_map(|s| s.sources.iter().map(|x| x.id)).max().unwrap_or(0) + 1;
    for sc in &mut p.scenes {
        sc.name = truncate_chars(&sc.name, 64);
        sc.sources.truncate(MAX_SOURCES);
        for src in &mut sc.sources {
            if src.id == 0 || !seen.insert(src.id) {
                src.id = next_id;
                seen.insert(next_id);
                next_id += 1;
            }
            src.name = truncate_chars(&src.name, 64);
            src.w = src.w.clamp(MIN_DIM, MAX_W);
            src.h = src.h.clamp(MIN_DIM, MAX_H);
            src.x = src.x.clamp(-(MAX_W as i32), MAX_W as i32);
            src.y = src.y.clamp(-(MAX_H as i32), MAX_H as i32);
            match &mut src.kind {
                SourceKind::Display { w, h, x, y, target, .. } => {
                    *w = (*w).clamp(1, 16384);
                    *h = (*h).clamp(1, 16384);
                    *x = (*x).clamp(-32768, 32767);
                    *y = (*y).clamp(-32768, 32767);
                    *target = truncate_chars(&target.replace('\0', ""), 512);
                }
                SourceKind::Webcam { device, size, fps } => {
                    *device = truncate_chars(&device.replace('\0', ""), 512);
                    if parse_size(size).is_none() {
                        size.clear();
                    }
                    *fps = (*fps).min(MAX_FPS);
                }
                SourceKind::Media { path, .. } | SourceKind::Image { path } => {
                    *path = truncate_chars(&path.replace('\0', ""), 4096);
                }
                SourceKind::Color { .. } => {}
            }
        }
    }
    if p.current >= p.scenes.len() {
        p.current = 0;
    }
}

fn parse_size(s: &str) -> Option<(u32, u32)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (a, b) = s.split_once(['x', 'X'])?;
    let w: u32 = a.trim().parse().ok()?;
    let h: u32 = b.trim().parse().ok()?;
    ((1..=MAX_W).contains(&w) && (1..=MAX_H).contains(&h)).then_some((w, h))
}

fn config_dir() -> PathBuf {
    if cfg!(windows)
        && let Some(a) = std::env::var_os("APPDATA") {
            return PathBuf::from(a).join("litecast");
        }
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(x).join("litecast");
    }
    home_dir().join(".config").join("litecast")
}

fn load_profile(path: &Path) -> Result<Profile, String> {
    let meta = fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.len() > MAX_PROFILE_BYTES {
        return Err("profile too large (>4 MiB)".into());
    }
    let data = fs::read(path).map_err(|e| e.to_string())?;
    let mut p: Profile = serde_json::from_slice(&data).map_err(|e| format!("invalid profile: {e}"))?;
    sanitize(&mut p);
    Ok(p)
}

/// Atomic write (tmp + rename) with 0600 perms — the profile holds the stream key.
fn save_profile(path: &Path, p: &Profile) -> Result<(), String> {
    let dir = path.parent().unwrap_or(Path::new("."));
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let _ = fs::DirBuilder::new().recursive(true).mode(0o700).create(dir);
    }
    #[cfg(not(unix))]
    {
        let _ = fs::create_dir_all(dir);
    }
    let data = serde_json::to_vec_pretty(p).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut o = fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let mut f = o.open(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.write_all(&data).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })
}

// ----------------------------------------------------------------------------
// ffmpeg helpers
// ----------------------------------------------------------------------------
fn ffmpeg_cmd(prog: &str) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(prog);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

/// Force the file protocol for local paths so names like "concat:..." or
/// "subfile,..." cannot be interpreted as ffmpeg protocols.
fn file_arg(p: &str) -> String {
    if p.contains("://") {
        p.to_string()
    } else {
        format!("file:{p}")
    }
}

#[derive(Clone, Debug, Default)]
struct FfInfo {
    ok: bool,
    major: u32,
    minor: u32,
    version: String,
    encoders: HashSet<String>,
}
impl FfInfo {
    fn has_fps_mode(&self) -> bool {
        self.major > 5 || (self.major == 5 && self.minor >= 1)
    }
}

/// Run a short-lived tool with bounded output, optional stdin, timeout and
/// cooperative cancellation. Never blocks forever.
fn run_tool(
    prog: &str,
    args: &[String],
    stdin_data: Option<Arc<Vec<u8>>>,
    max_out: u64,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> Result<(Vec<u8>, String, Option<i32>), String> {
    let mut cmd = ffmpeg_cmd(prog);
    cmd.args(args)
        .stdin(if stdin_data.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("cannot run {prog}: {e}"))?;
    let writer = match (stdin_data, child.stdin.take()) {
        (Some(d), Some(mut si)) => Some(thread::spawn(move || {
            let _ = si.write_all(&d);
        })),
        _ => None,
    };
    let out = child.stdout.take().ok_or("no stdout")?;
    let err = child.stderr.take().ok_or("no stderr")?;
    let to = thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out.take(max_out).read_to_end(&mut v);
        v
    });
    let te = thread::spawn(move || {
        let mut v = Vec::new();
        let _ = err.take(256 * 1024).read_to_end(&mut v);
        v
    });
    let deadline = Instant::now() + timeout;
    let mut code = None;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                code = st.code();
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline || cancel.map(|c| c.load(Relaxed)).unwrap_or(false) {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                thread::sleep(Duration::from_millis(15));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    if let Some(w) = writer {
        let _ = w.join();
    }
    let o = to.join().unwrap_or_default();
    let e = te.join().unwrap_or_default();
    Ok((o, String::from_utf8_lossy(&e).into_owned(), code))
}

fn detect_ffmpeg(prog: &str) -> FfInfo {
    let mut fi = FfInfo::default();
    let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    if let Ok((o, _, Some(0))) = run_tool(prog, &a(&["-hide_banner", "-version"]), None, 64 * 1024, Duration::from_secs(5), None) {
        let s = String::from_utf8_lossy(&o);
        let first = s.lines().next().unwrap_or("").to_string();
        fi.version = first.clone();
        let ver = first.split_whitespace().nth(2).unwrap_or("").trim_start_matches('n');
        let mut it = ver.split(|c: char| !c.is_ascii_digit());
        let maj = it.next().and_then(|v| v.parse().ok());
        let min = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        // Git builds ("N-12345-g...") are always newer than any release.
        (fi.major, fi.minor) = match maj { Some(m) => (m, min), None => (99, 0) };
        fi.ok = true;
    }
    if fi.ok
        && let Ok((o, _, _)) = run_tool(prog, &a(&["-hide_banner", "-encoders"]), None, 512 * 1024, Duration::from_secs(5), None) {
            for line in String::from_utf8_lossy(&o).lines() {
                let mut t = line.split_whitespace();
                if let (Some(flags), Some(name)) = (t.next(), t.next())
                    && flags.len() == 6 && (flags.starts_with('V') || flags.starts_with('A')) {
                        fi.encoders.insert(name.to_string());
                    }
            }
        }
    fi
}

/// (value, label) pairs. Value is what goes into the ffmpeg -i argument.
type DevList = Vec<(String, String)>;

fn parse_dshow(ff: &str) -> (DevList, DevList) {
    let args: Vec<String> = ["-hide_banner", "-list_devices", "true", "-f", "dshow", "-i", "dummy"]
        .iter().map(|s| s.to_string()).collect();
    let (mut v, mut a) = (vec![], vec![]);
    if let Ok((_, err, _)) = run_tool(ff, &args, None, 1024, Duration::from_secs(6), None) {
        let mut section = 0; // 1 video, 2 audio
        for line in err.lines() {
            if line.contains("DirectShow video devices") { section = 1; continue; }
            if line.contains("DirectShow audio devices") { section = 2; continue; }
            if line.contains("Alternative name") { continue; }
            let (Some(s), Some(e)) = (line.find('"'), line.rfind('"')) else { continue };
            if e <= s + 1 { continue; }
            let name = line[s + 1..e].to_string();
            let kind = if line.ends_with("(video)") { 1 } else if line.ends_with("(audio)") { 2 } else { section };
            match kind { 1 => v.push((name.clone(), name)), 2 => a.push((name.clone(), name)), _ => {} }
        }
    }
    (v, a)
}

fn parse_avfoundation(ff: &str) -> (DevList, DevList) {
    let args: Vec<String> = ["-hide_banner", "-f", "avfoundation", "-list_devices", "true", "-i", ""]
        .iter().map(|s| s.to_string()).collect();
    let (mut v, mut a) = (vec![], vec![]);
    if let Ok((_, err, _)) = run_tool(ff, &args, None, 1024, Duration::from_secs(6), None) {
        let mut section = 0;
        for line in err.lines() {
            if line.contains("video devices:") { section = 1; continue; }
            if line.contains("audio devices:") { section = 2; continue; }
            // "...] [0] FaceTime HD Camera"
            if let Some(p) = line.rfind("] [") {
                let rest = &line[p + 3..];
                if let Some(q) = rest.find(']') {
                    let idx = rest[..q].to_string();
                    let label = format!("{} — {}", idx, rest[q + 1..].trim());
                    if idx.chars().all(|c| c.is_ascii_digit()) {
                        match section { 1 => v.push((idx, label)), 2 => a.push((idx, label)), _ => {} }
                    }
                }
            }
        }
    }
    (v, a)
}

fn detect_video_devices(ff: &str) -> DevList {
    if cfg!(windows) {
        parse_dshow(ff).0
    } else if cfg!(target_os = "macos") {
        parse_avfoundation(ff).0
    } else {
        let mut v: Vec<String> = fs::read_dir("/dev")
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with("video") && n[5..].chars().all(|c| c.is_ascii_digit()))
                    .map(|n| format!("/dev/{n}"))
                    .collect()
            })
            .unwrap_or_default();
        v.sort_by_key(|n| n[10..].parse::<u32>().unwrap_or(u32::MAX));
        v.into_iter().map(|n| (n.clone(), n)).collect()
    }
}

fn detect_audio_devices(api: &str, ff: &str) -> DevList {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    match api {
        "pulse" => {
            let mut v: DevList = vec![
                ("default".into(), "default (default microphone)".into()),
                ("@DEFAULT_MONITOR@".into(), "@DEFAULT_MONITOR@ (desktop audio)".into()),
            ];
            if let Ok((o, _, _)) = run_tool("pactl", &s(&["list", "short", "sources"]), None, 256 * 1024, Duration::from_secs(3), None) {
                for l in String::from_utf8_lossy(&o).lines() {
                    if let Some(n) = l.split('\t').nth(1) {
                        v.push((n.to_string(), n.to_string()));
                    }
                }
            }
            v
        }
        "alsa" => {
            let mut v: DevList = vec![("default".into(), "default".into())];
            if let Ok((o, _, _)) = run_tool("arecord", &s(&["-L"]), None, 256 * 1024, Duration::from_secs(3), None) {
                for l in String::from_utf8_lossy(&o).lines() {
                    if !l.is_empty() && !l.starts_with(char::is_whitespace) && l != "default" && l != "null" {
                        v.push((l.to_string(), l.to_string()));
                    }
                }
            }
            v
        }
        "dshow" => parse_dshow(ff).1,
        "avfoundation" => parse_avfoundation(ff).1,
        _ => vec![],
    }
}

/// Bounded line reader for child stderr. Drains continuously so a chatty
/// ffmpeg can never fill the pipe and deadlock.
fn spawn_stderr_drain(err: ChildStderr, tag: String, log: Logger, last: Arc<Mutex<String>>) -> Option<JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("stderr-{tag}"))
        .spawn(move || {
            let mut r = BufReader::with_capacity(8192, err);
            let mut buf = Vec::with_capacity(512);
            loop {
                buf.clear();
                match std::io::BufRead::read_until(&mut (&mut r).take(4096), b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let s = String::from_utf8_lossy(&buf);
                        let s = s.trim();
                        if s.is_empty() { continue; }
                        *lock(&last) = truncate_chars(s, 300);
                        log.log(format!("[{tag}] {s}"));
                    }
                }
            }
        })
        .ok()
}

/// Like read_exact but reports a clean EOF at a frame boundary as Ok(false).
fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<bool> {
    let mut off = 0;
    while off < buf.len() {
        match r.read(&mut buf[off..]) {
            Ok(0) => {
                return if off == 0 { Ok(false) } else { Err(std::io::ErrorKind::UnexpectedEof.into()) };
            }
            Ok(n) => off += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

fn all_opaque(buf: &[u8]) -> bool {
    buf.iter().skip(3).step_by(4).all(|&a| a == 255)
}

/// Swap a new frame into a slot; returns the previous buffer if nobody else
/// holds it, so the reader can recycle the allocation (no 8 MB alloc/frame).
fn publish(slot: &FrameSlot, buf: Vec<u8>) -> Option<Vec<u8>> {
    let old = lock(slot).replace(Arc::new(buf));
    old.and_then(|a| Arc::try_unwrap(a).ok())
}

/// Build the ffmpeg capture command for a live/media source. Output is raw
/// BGRA at exactly the source's transform size.
fn capture_args(kind: &SourceKind, w: u32, h: u32, fps: u32) -> Result<Vec<String>, String> {
    let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin", "-nostats"]
        .iter().map(|s| s.to_string()).collect();
    let mut pre_vf = String::new();
    let p = |a: &mut Vec<String>, v: &[&str]| a.extend(v.iter().map(|s| s.to_string()));
    match kind {
        SourceKind::Display { target, x, y, w: rw, h: rh, cursor } => {
            let cur = if *cursor { "1" } else { "0" };
            let fps_s = fps.to_string();
            let size = format!("{rw}x{rh}");
            if cfg!(windows) {
                p(&mut a, &["-f", "gdigrab", "-framerate", &fps_s, "-draw_mouse", cur]);
                if target.starts_with("title=") {
                    p(&mut a, &["-i", target]);
                } else {
                    let (xs, ys) = (x.to_string(), y.to_string());
                    p(&mut a, &["-offset_x", &xs, "-offset_y", &ys, "-video_size", &size, "-i", "desktop"]);
                }
            } else if cfg!(target_os = "macos") {
                let t = if target.trim().is_empty() { "1".to_string() } else { target.clone() };
                let inp = format!("{t}:none");
                p(&mut a, &["-f", "avfoundation", "-capture_cursor", cur, "-framerate", &fps_s, "-i", &inp]);
                pre_vf = format!("crop={rw}:{rh}:{x}:{y},");
            } else {
                let disp = if target.trim().is_empty() { default_display_target() } else { target.clone() };
                let inp = format!("{disp}+{x},{y}");
                p(&mut a, &["-f", "x11grab", "-framerate", &fps_s, "-draw_mouse", cur, "-video_size", &size, "-i", &inp]);
            }
        }
        SourceKind::Webcam { device, size, fps: wf } => {
            if device.trim().is_empty() {
                return Err("no webcam device set".into());
            }
            let api = if cfg!(windows) { "dshow" } else if cfg!(target_os = "macos") { "avfoundation" } else { "v4l2" };
            p(&mut a, &["-f", api]);
            if cfg!(windows) {
                p(&mut a, &["-rtbufsize", "64M"]);
            }
            if parse_size(size).is_some() {
                p(&mut a, &["-video_size", size.trim()]);
            }
            if *wf > 0 {
                let f = wf.to_string();
                p(&mut a, &["-framerate", &f]);
            }
            let inp = if cfg!(windows) {
                format!("video={device}")
            } else if cfg!(target_os = "macos") {
                format!("{device}:none")
            } else {
                device.clone()
            };
            p(&mut a, &["-i", &inp]);
        }
        SourceKind::Media { path, looping } => {
            if path.trim().is_empty() {
                return Err("no media file set".into());
            }
            if *looping {
                p(&mut a, &["-stream_loop", "-1"]);
            }
            let fa = file_arg(path);
            p(&mut a, &["-re", "-i", &fa]);
        }
        _ => return Err("not a process source".into()),
    }
    let vf = format!("{pre_vf}scale={w}:{h}:flags=bilinear,format={PIX}");
    p(&mut a, &["-an", "-sn", "-dn", "-vf", &vf, "-f", "rawvideo", "-pix_fmt", PIX, "pipe:1"]);
    Ok(a)
}

// ----------------------------------------------------------------------------
// Source runtime (one per distinct source signature; shared across scenes)
// ----------------------------------------------------------------------------
struct SourceRuntime {
    slot: FrameSlot,
    opaque: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    thread: Option<JoinHandle<()>>,
    status: Arc<Mutex<String>>,
}

struct RtCtx {
    ffmpeg: String,
    fps: u32,
    log: Logger,
}

/// Anything that changes the produced pixels must be in the signature.
fn source_sig(s: &Source, fps: u32, ff: &str) -> String {
    format!("{:?}|{}x{}|{}|{}", s.kind, s.w, s.h, fps, ff)
}

impl SourceRuntime {
    fn start(src: &Source, ctx: &RtCtx) -> Self {
        let mut rt = Self {
            slot: Arc::new(Mutex::new(None)),
            opaque: Arc::new(AtomicBool::new(true)),
            stop: Arc::new(AtomicBool::new(false)),
            child: Arc::new(Mutex::new(None)),
            thread: None,
            status: Arc::new(Mutex::new("starting".into())),
        };
        let (w, h) = (src.w, src.h);
        let name = src.name.clone();
        match &src.kind {
            SourceKind::Color { r, g, b } => {
                let px = [*b, *g, *r, 255u8];
                let v = px.repeat(w as usize * h as usize);
                *lock(&rt.slot) = Some(Arc::new(v));
                *lock(&rt.status) = "ok".into();
            }
            SourceKind::Image { path } => {
                let args: Vec<String> = vec![
                    "-hide_banner".into(), "-loglevel".into(), "error".into(), "-nostdin".into(),
                    "-i".into(), file_arg(path), "-frames:v".into(), "1".into(),
                    "-vf".into(), format!("scale={w}:{h}:flags=lanczos,format={PIX}"),
                    "-f".into(), "rawvideo".into(), "-pix_fmt".into(), PIX.into(), "pipe:1".into(),
                ];
                let (slot, opaque, stop, status, log, ff) =
                    (rt.slot.clone(), rt.opaque.clone(), rt.stop.clone(), rt.status.clone(), ctx.log.clone(), ctx.ffmpeg.clone());
                let need = w as usize * h as usize * 4;
                rt.thread = thread::Builder::new()
                    .name(format!("img-{name}"))
                    .spawn(move || {
                        match run_tool(&ff, &args, None, need as u64, Duration::from_secs(20), Some(&stop)) {
                            Ok((buf, _, _)) if buf.len() == need => {
                                opaque.store(all_opaque(&buf), Relaxed);
                                *lock(&slot) = Some(Arc::new(buf));
                                *lock(&status) = "ok".into();
                            }
                            Ok((_, err, _)) => {
                                let e = truncate_chars(err.trim(), 200);
                                log.log(format!("[{name}] image decode failed: {e}"));
                                *lock(&status) = format!("error: {e}");
                            }
                            Err(e) => {
                                log.log(format!("[{name}] {e}"));
                                *lock(&status) = format!("error: {e}");
                            }
                        }
                    })
                    .ok();
            }
            kind => {
                let args = match capture_args(kind, w, h, ctx.fps) {
                    Ok(a) => a,
                    Err(e) => {
                        *lock(&rt.status) = format!("error: {e}");
                        ctx.log.log(format!("[{name}] {e}"));
                        return rt;
                    }
                };
                let noloop_media = matches!(kind, SourceKind::Media { looping: false, .. });
                let cfg = CapCfg {
                    name: name.clone(),
                    args,
                    frame_len: w as usize * h as usize * 4,
                    ffmpeg: ctx.ffmpeg.clone(),
                    log: ctx.log.clone(),
                    slot: rt.slot.clone(),
                    opaque: rt.opaque.clone(),
                    stop: rt.stop.clone(),
                    child: rt.child.clone(),
                    status: rt.status.clone(),
                    noloop_media,
                };
                rt.thread = thread::Builder::new().name(format!("cap-{name}")).spawn(move || capture_loop(cfg)).ok();
            }
        }
        rt
    }

    fn stop(mut self) {
        self.stop.store(true, Relaxed);
        if let Some(c) = lock(&self.child).as_mut() {
            let _ = c.kill(); // unblocks the reader's read()
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct CapCfg {
    name: String,
    args: Vec<String>,
    frame_len: usize,
    ffmpeg: String,
    log: Logger,
    slot: FrameSlot,
    opaque: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    status: Arc<Mutex<String>>,
    noloop_media: bool,
}

/// Supervised capture: spawns ffmpeg, reads fixed-size frames, restarts with
/// exponential backoff on failure (e.g. webcam unplugged, X server hiccup).
fn capture_loop(c: CapCfg) {
    let mut backoff = Duration::from_secs(1);
    let mut spare: Option<Vec<u8>> = None;
    while !c.stop.load(Relaxed) {
        let mut cmd = ffmpeg_cmd(&c.ffmpeg);
        cmd.args(&c.args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(ch) => ch,
            Err(e) => {
                *lock(&c.status) = format!("error: cannot start ffmpeg: {e}");
                c.log.log(format!("[{}] cannot start ffmpeg ({}): {e}", c.name, c.ffmpeg));
                sleep_interruptible(backoff, &c.stop);
                backoff = (backoff * 2).min(Duration::from_secs(10));
                continue;
            }
        };
        let stdout = child.stdout.take();
        let last_err = Arc::new(Mutex::new(String::new()));
        let drain = child.stderr.take().and_then(|e| spawn_stderr_drain(e, c.name.clone(), c.log.clone(), last_err.clone()));
        *lock(&c.child) = Some(child);
        if c.stop.load(Relaxed)
            && let Some(ch) = lock(&c.child).as_mut() {
                let _ = ch.kill();
            }
        let started = Instant::now();
        let mut frames: u64 = 0;
        let mut clean_eof = false;
        if let Some(mut out) = stdout {
            loop {
                let mut buf = spare.take().filter(|b| b.len() == c.frame_len).unwrap_or_else(|| vec![0u8; c.frame_len]);
                match read_full(&mut out, &mut buf) {
                    Ok(true) => {}
                    Ok(false) => {
                        clean_eof = true;
                        break;
                    }
                    Err(_) => break,
                }
                // Alpha scan is O(n); sample it rather than every frame.
                if frames.is_multiple_of(60) {
                    c.opaque.store(all_opaque(&buf), Relaxed);
                }
                if frames == 0 {
                    *lock(&c.status) = "ok".into();
                }
                frames += 1;
                spare = publish(&c.slot, buf);
            }
        }
        if let Some(mut ch) = lock(&c.child).take() {
            let _ = ch.kill();
            let _ = ch.wait(); // reap: no zombies
        }
        if let Some(d) = drain {
            let _ = d.join();
        }
        if c.stop.load(Relaxed) {
            break;
        }
        if c.noloop_media && clean_eof && frames > 0 {
            *lock(&c.status) = "ended".into();
            c.log.log(format!("[{}] media ended", c.name));
            while !c.stop.load(Relaxed) {
                thread::sleep(Duration::from_millis(200));
            }
            break;
        }
        if started.elapsed() > Duration::from_secs(10) {
            backoff = Duration::from_secs(1);
        }
        let le = lock(&last_err).clone();
        *lock(&c.status) = format!("error: {}", if le.is_empty() { "capture ended" } else { &le });
        c.log.log(format!("[{}] capture stopped after {frames} frames; retry in {}s", c.name, backoff.as_secs()));
        sleep_interruptible(backoff, &c.stop);
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

// ----------------------------------------------------------------------------
// Compositor
// ----------------------------------------------------------------------------
#[derive(Clone)]
struct Layer {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    opacity: u8,
    slot: FrameSlot,
    opaque: Arc<AtomicBool>,
}

struct PreviewShared {
    buf: Mutex<(Vec<u8>, usize, usize)>, // RGB8, w, h
    seq: AtomicU64,
    req: AtomicU64, // (w << 32) | h requested by the UI
}

struct CompShared {
    layers: Mutex<Arc<Vec<Layer>>>,
    canvas: Mutex<Option<Arc<Vec<u8>>>>,
    stop: AtomicBool,
    cw: u32,
    ch: u32,
    fps: u32,
    compose_us: AtomicU64,
    fps_milli: AtomicU64,
    late: AtomicU64,
}

struct Compositor {
    sh: Arc<CompShared>,
    thread: Option<JoinHandle<()>>,
}

impl Compositor {
    fn start(cw: u32, ch: u32, fps: u32, pv: Arc<PreviewShared>) -> Self {
        let sh = Arc::new(CompShared {
            layers: Mutex::new(Arc::new(vec![])),
            canvas: Mutex::new(None),
            stop: AtomicBool::new(false),
            cw,
            ch,
            fps,
            compose_us: AtomicU64::new(0),
            fps_milli: AtomicU64::new(0),
            late: AtomicU64::new(0),
        });
        let s2 = sh.clone();
        let thread = thread::Builder::new().name("compositor".into()).spawn(move || comp_loop(s2, pv)).ok();
        Self { sh, thread }
    }
    fn stop(mut self) {
        self.sh.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Exact round(x / 255) for x <= 255*255 without a hardware divide.
#[inline(always)]
fn div255(x: u32) -> u32 {
    let x = x + 128;
    (x + (x >> 8)) >> 8
}

/// Clip + blit one layer. Fast path: memcpy rows when fully opaque.
fn blit(dst: &mut [u8], cw: usize, ch: usize, src: &[u8], l: &Layer, opaque: bool) {
    let (sw, sh) = (l.w as usize, l.h as usize);
    if src.len() < sw * sh * 4 || l.opacity == 0 {
        return;
    }
    let (lx, ly) = (l.x as i64, l.y as i64);
    let x0 = lx.max(0);
    let y0 = ly.max(0);
    let x1 = (lx + sw as i64).min(cw as i64);
    let y1 = (ly + sh as i64).min(ch as i64);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let n = (x1 - x0) as usize;
    let sx0 = (x0 - lx) as usize;
    let op = l.opacity as u32;
    let fast = opaque && op == 255;
    for y in y0..y1 {
        let sy = (y - ly) as usize;
        let s = &src[(sy * sw + sx0) * 4..(sy * sw + sx0 + n) * 4];
        let di = (y as usize * cw + x0 as usize) * 4;
        let d = &mut dst[di..di + n * 4];
        if fast {
            d.copy_from_slice(s);
            continue;
        }
        for (dp, sp) in d.chunks_exact_mut(4).zip(s.chunks_exact(4)) {
            let a = div255(sp[3] as u32 * op);
            if a == 0 {
                continue;
            }
            if a == 255 {
                dp.copy_from_slice(sp);
                continue;
            }
            let ia = 255 - a;
            dp[0] = div255(sp[0] as u32 * a + dp[0] as u32 * ia) as u8;
            dp[1] = div255(sp[1] as u32 * a + dp[1] as u32 * ia) as u8;
            dp[2] = div255(sp[2] as u32 * a + dp[2] as u32 * ia) as u8;
            dp[3] = 255;
        }
    }
}

fn comp_loop(sh: Arc<CompShared>, pv: Arc<PreviewShared>) {
    let (cw, ch) = (sh.cw as usize, sh.ch as usize);
    let flen = cw * ch * 4;
    let black = [0u8, 0, 0, 255].repeat(cw * ch);
    let mut pool: Vec<Arc<Vec<u8>>> = Vec::new();
    let period_ns = 1_000_000_000u64 / sh.fps.max(1) as u64;
    let mut t0 = Instant::now();
    let mut n: u64 = 0;
    let (mut fps_t, mut fps_n) = (Instant::now(), 0u64);
    let mut xmap: Vec<usize> = Vec::new();
    let mut pbuf: Vec<u8> = Vec::new();
    while !sh.stop.load(Relaxed) {
        let ts = Instant::now();
        // Recycle a canvas buffer nobody else references (encoders release theirs after write).
        let mut arc = match pool.iter().position(|a| Arc::strong_count(a) == 1) {
            Some(i) => pool.swap_remove(i),
            None => Arc::new(vec![0u8; flen]),
        };
        if Arc::get_mut(&mut arc).is_none() {
            arc = Arc::new(vec![0u8; flen]);
        }
        let buf = Arc::get_mut(&mut arc).expect("unique canvas buffer");
        let layers = lock(&sh.layers).clone();
        // Hold each slot lock only for an Arc clone.
        let frames: Vec<Option<Arc<Vec<u8>>>> = layers.iter().map(|l| lock(&l.slot).clone()).collect();
        // Skip the 8 MB clear when the bottom layer is opaque and covers the canvas.
        let covered = match (layers.first(), frames.first()) {
            (Some(l), Some(Some(_))) => {
                l.opacity == 255 && l.opaque.load(Relaxed) && l.x <= 0 && l.y <= 0
                    && l.x as i64 + l.w as i64 >= cw as i64 && l.y as i64 + l.h as i64 >= ch as i64
            }
            _ => false,
        };
        if !covered {
            buf.copy_from_slice(&black);
        }
        for (l, f) in layers.iter().zip(frames.iter()) {
            if let Some(f) = f {
                blit(buf, cw, ch, f, l, l.opaque.load(Relaxed));
            }
        }
        // Nearest-neighbour downscale into RGB preview, outside the shared lock.
        let req = pv.req.load(Relaxed);
        let (pw, ph) = ((req >> 32) as usize, (req & 0xffff_ffff) as usize);
        if pw > 0 && ph > 0 && pw <= 8192 && ph <= 8192 {
            if xmap.len() != pw {
                xmap = (0..pw).map(|x| (x * cw / pw).min(cw - 1) * 4).collect();
            }
            pbuf.resize(pw * ph * 3, 0);
            for y in 0..ph {
                let sy = (y * ch / ph).min(ch - 1);
                let row = &buf[sy * cw * 4..(sy + 1) * cw * 4];
                let out = &mut pbuf[y * pw * 3..(y + 1) * pw * 3];
                for (o, &si) in out.chunks_exact_mut(3).zip(xmap.iter()) {
                    o[0] = row[si + 2];
                    o[1] = row[si + 1];
                    o[2] = row[si];
                }
            }
            {
                let mut g = lock(&pv.buf);
                std::mem::swap(&mut g.0, &mut pbuf);
                g.1 = pw;
                g.2 = ph;
            }
            pv.seq.fetch_add(1, Relaxed);
        }
        if pool.len() < 6 {
            pool.push(arc.clone());
        }
        *lock(&sh.canvas) = Some(arc);
        sh.compose_us.store(ts.elapsed().as_micros() as u64, Relaxed);

        fps_n += 1;
        let el = fps_t.elapsed();
        if el >= Duration::from_secs(1) {
            sh.fps_milli.store((fps_n as f64 * 1000.0 / el.as_secs_f64()) as u64, Relaxed);
            fps_t = Instant::now();
            fps_n = 0;
        }
        // Drift-free pacing against an absolute schedule.
        n += 1;
        let target = t0 + Duration::from_nanos(period_ns.saturating_mul(n));
        let now = Instant::now();
        if target > now {
            thread::sleep(target - now);
        } else if now - target > Duration::from_nanos(period_ns * 3) {
            sh.late.fetch_add(1, Relaxed);
            t0 = now;
            n = 0;
        }
    }
}

// ----------------------------------------------------------------------------
// Output sessions (record / stream) — one ffmpeg encoder each
// ----------------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq, Debug)]
enum OutKind {
    Record,
    Stream,
}

fn audio_inputs(s: &Settings) -> Vec<Vec<String>> {
    let mut v = vec![];
    for dev in [&s.mic_device, &s.desktop_device] {
        let d = dev.trim();
        if d.is_empty() || s.audio_api == "none" {
            continue;
        }
        let mut a: Vec<String> = vec!["-thread_queue_size".into(), "1024".into()];
        match s.audio_api.as_str() {
            "pulse" => a.extend(["-f".into(), "pulse".into(), "-i".into(), d.into()]),
            "alsa" => a.extend(["-f".into(), "alsa".into(), "-i".into(), d.into()]),
            "dshow" => a.extend([
                "-f".into(), "dshow".into(), "-audio_buffer_size".into(), "50".into(),
                "-i".into(), format!("audio={d}"),
            ]),
            "avfoundation" => a.extend(["-f".into(), "avfoundation".into(), "-i".into(), format!(":{d}")]),
            _ => continue,
        }
        v.push(a);
    }
    v
}

/// Pure function: settings -> encoder argv. Kept side-effect free for testing.
fn encoder_args(s: &Settings, ff: &FfInfo, kind: OutKind, target: &str) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    let mut p = |v: &[&str]| a.extend(v.iter().map(|x| x.to_string()));
    p(&["-hide_banner", "-loglevel", "warning", "-nostdin", "-nostats"]);
    if kind == OutKind::Record {
        p(&["-n"]); // never overwrite an existing file
    }
    let enc = s.encoder.as_str();
    if enc == "h264_vaapi" {
        p(&["-vaapi_device", &s.vaapi_device]);
    }
    let size = format!("{}x{}", s.canvas_w, s.canvas_h);
    let fps = s.fps.to_string();
    p(&[
        "-thread_queue_size", "64", "-use_wallclock_as_timestamps", "1",
        "-f", "rawvideo", "-pix_fmt", PIX, "-video_size", &size, "-framerate", &fps, "-i", "pipe:0",
    ]);
    let auds = audio_inputs(s);
    for ai in &auds {
        a.extend(ai.iter().cloned());
    }
    let mut p = |v: &[&str]| a.extend(v.iter().map(|x| x.to_string()));
    let vf = match enc {
        "h264_vaapi" => "format=nv12,hwupload",
        "h264_qsv" => "format=nv12",
        _ => "format=yuv420p",
    };
    let mut fc = format!("[0:v]{vf}[vout]");
    if auds.len() == 2 {
        fc.push_str(";[1:a][2:a]amix=inputs=2:duration=longest:dropout_transition=0[aout]");
    }
    p(&["-filter_complex", &fc, "-map", "[vout]"]);
    match auds.len() {
        1 => p(&["-map", "1:a"]),
        2 => p(&["-map", "[aout]"]),
        _ => {}
    }
    p(&["-c:v", enc]);
    match enc {
        "libx264" => p(&["-preset", &s.x264_preset, "-profile:v", "high"]),
        "h264_nvenc" => p(&["-preset", "p4", "-rc", "cbr"]),
        "h264_qsv" => p(&["-preset", "veryfast"]),
        "h264_amf" => p(&["-quality", "speed"]),
        "h264_videotoolbox" => p(&["-realtime", "1"]),
        _ => {}
    }
    let kb = format!("{}k", s.video_kbps);
    let buf = format!("{}k", s.video_kbps.saturating_mul(2));
    let g = (s.fps * s.keyint_sec).to_string();
    p(&["-b:v", &kb, "-maxrate", &kb, "-bufsize", &buf, "-g", &g]);
    if ff.has_fps_mode() {
        p(&["-fps_mode", "cfr"]);
    } else {
        p(&["-vsync", "cfr"]);
    }
    p(&["-r", &fps]);
    if !auds.is_empty() {
        let ab = format!("{}k", s.audio_kbps);
        p(&["-c:a", "aac", "-b:a", &ab, "-ar", "48000", "-ac", "2"]);
    }
    match kind {
        OutKind::Record => {
            match s.rec_format.as_str() {
                "mp4" => p(&["-f", "mp4", "-movflags", "+faststart"]),
                "mov" => p(&["-f", "mov"]),
                "flv" => p(&["-f", "flv"]),
                "ts" => p(&["-f", "mpegts"]),
                _ => p(&["-f", "matroska"]),
            }
            p(&[&file_arg(target)]);
        }
        OutKind::Stream => {
            if target.starts_with("srt://") {
                p(&["-f", "mpegts", target]);
            } else {
                p(&["-f", "flv", "-flvflags", "no_duration_filesize", target]);
            }
        }
    }
    a
}

fn stream_target(s: &Settings) -> Result<String, String> {
    let url = s.stream_url.trim();
    let ok = ["rtmp://", "rtmps://", "srt://"].iter().any(|p| url.to_ascii_lowercase().starts_with(p));
    if !ok {
        return Err("Stream URL must start with rtmp://, rtmps:// or srt://".into());
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("Stream URL contains whitespace/control characters".into());
    }
    let key = s.stream_key.trim();
    if key.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("Stream key contains whitespace/control characters".into());
    }
    if key.is_empty() || url.starts_with("srt://") {
        Ok(url.to_string())
    } else {
        Ok(format!("{}/{}", url.trim_end_matches('/'), key))
    }
}

fn secrets_of(s: &Settings) -> Vec<String> {
    let mut v = vec![s.stream_key.trim().to_string()];
    // rtmp://user:pass@host — redact userinfo; srt passphrase in query string.
    let u = s.stream_url.trim();
    if let Some(rest) = u.split_once("://").map(|x| x.1)
        && let Some((ui, _)) = rest.split_once('@') {
            v.push(ui.to_string());
        }
    if let Some((_, q)) = u.split_once('?') {
        for kv in q.split('&') {
            if let Some((k, val)) = kv.split_once('=')
                && (k.eq_ignore_ascii_case("passphrase") || k.eq_ignore_ascii_case("streamid")) {
                    v.push(val.to_string());
                }
        }
    }
    v
}

struct OutputSession {
    kind: OutKind,
    child: Arc<Mutex<Option<Child>>>,
    stop: Arc<AtomicBool>,
    dead: Arc<AtomicBool>,
    writer: Option<JoinHandle<()>>,
    drain: Option<JoinHandle<()>>,
    started: Instant,
    frames: Arc<AtomicU64>,
    last_err: Arc<Mutex<String>>,
    label: String,
}

impl OutputSession {
    fn start(kind: OutKind, s: &Settings, ff: &FfInfo, comp: &Arc<CompShared>, log: &Logger, target: &str, label: String) -> Result<Self, String> {
        let args = encoder_args(s, ff, kind, target);
        let mut cmd = ffmpeg_cmd(&s.ffmpeg_path);
        cmd.args(&args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("cannot start ffmpeg ({}): {e}", s.ffmpeg_path))?;
        let stdin = child.stdin.take().ok_or("no stdin pipe")?;
        let tag = if kind == OutKind::Record { "rec" } else { "stream" };
        let last_err = Arc::new(Mutex::new(String::new()));
        let drain = child.stderr.take().and_then(|e| spawn_stderr_drain(e, tag.into(), log.clone(), last_err.clone()));
        let sess = Self {
            kind,
            child: Arc::new(Mutex::new(Some(child))),
            stop: Arc::new(AtomicBool::new(false)),
            dead: Arc::new(AtomicBool::new(false)),
            writer: None,
            drain,
            started: Instant::now(),
            frames: Arc::new(AtomicU64::new(0)),
            last_err,
            label,
        };
        let (stop, dead, frames, comp2, log2) = (sess.stop.clone(), sess.dead.clone(), sess.frames.clone(), comp.clone(), log.clone());
        let mut sess = sess;
        sess.writer = Some(
            thread::Builder::new()
                .name(format!("writer-{tag}"))
                .spawn(move || writer_loop(stdin, comp2, stop, dead, frames, log2, tag))
                .map_err(|e| e.to_string())?,
        );
        log.log(format!("[{tag}] started → {}", sess.label));
        Ok(sess)
    }

    fn alive(&self) -> bool {
        if self.dead.load(Relaxed) {
            return false;
        }
        match lock(&self.child).as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// Graceful stop off the UI thread: close stdin so ffmpeg flushes and
    /// finalises the container (critical for mp4 moov atom), then escalate.
    fn begin_stop(mut self, log: Logger) -> Option<JoinHandle<()>> {
        thread::Builder::new()
            .name("stopper".into())
            .spawn(move || {
                self.stop.store(true, Relaxed);
                let tag = if self.kind == OutKind::Record { "rec" } else { "stream" };
                if let Some(w) = self.writer.take() {
                    let t = Instant::now();
                    while !w.is_finished() && t.elapsed() < Duration::from_secs(3) {
                        thread::sleep(Duration::from_millis(20));
                    }
                    if !w.is_finished()
                        && let Some(c) = lock(&self.child).as_mut() {
                            let _ = c.kill(); // writer blocked on a stalled encoder
                        }
                    let _ = w.join();
                }
                let mut graceful = false;
                if let Some(mut c) = lock(&self.child).take() {
                    let t = Instant::now();
                    loop {
                        match c.try_wait() {
                            Ok(Some(_)) => {
                                graceful = true;
                                break;
                            }
                            Ok(None) if t.elapsed() < Duration::from_secs(15) => thread::sleep(Duration::from_millis(50)),
                            _ => {
                                let _ = c.kill();
                                let _ = c.wait();
                                break;
                            }
                        }
                    }
                }
                if let Some(d) = self.drain.take() {
                    let _ = d.join();
                }
                log.log(format!(
                    "[{tag}] stopped ({} frames, {}){}",
                    self.frames.load(Relaxed),
                    fmt_dur(self.started.elapsed()),
                    if graceful { "" } else { " — encoder had to be killed" }
                ));
            })
            .ok()
    }
}

fn writer_loop(mut stdin: ChildStdin, comp: Arc<CompShared>, stop: Arc<AtomicBool>, dead: Arc<AtomicBool>, frames: Arc<AtomicU64>, log: Logger, tag: &str) {
    let period_ns = 1_000_000_000u64 / comp.fps.max(1) as u64;
    let mut t0 = Instant::now();
    let mut n: u64 = 0;
    while !stop.load(Relaxed) {
        let f = lock(&comp.canvas).clone();
        if let Some(f) = f {
            if let Err(e) = stdin.write_all(&f) {
                log.log(format!("[{tag}] encoder pipe closed: {e}"));
                dead.store(true, Relaxed);
                break;
            }
            frames.fetch_add(1, Relaxed);
        }
        n += 1;
        let target = t0 + Duration::from_nanos(period_ns.saturating_mul(n));
        let now = Instant::now();
        if target > now {
            thread::sleep(target - now);
        } else if now - target > Duration::from_nanos(period_ns * 3) {
            // Encoder can't keep up; wallclock timestamps + cfr keep A/V in sync.
            t0 = now;
            n = 0;
        }
    }
    drop(stdin); // EOF → ffmpeg finalises
}

// ----------------------------------------------------------------------------
// Time helpers
// ----------------------------------------------------------------------------
fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// UTC timestamp for filenames (no chrono dependency).
fn utc_stamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}_{:02}-{:02}-{:02}Z", sod / 3600, (sod % 3600) / 60, sod % 60)
}

fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let p = dir.join(format!("{stem}.{ext}"));
    if !p.exists() {
        return p;
    }
    (1..1000)
        .map(|i| dir.join(format!("{stem}_{i}.{ext}")))
        .find(|p| !p.exists())
        .unwrap_or(p)
}

fn fit_rect(cw: u32, ch: u32, fw: i32, fh: i32) -> (i32, i32, i32, i32) {
    if cw == 0 || ch == 0 || fw <= 0 || fh <= 0 {
        return (0, 0, 0, 0);
    }
    let s = (fw as f64 / cw as f64).min(fh as f64 / ch as f64);
    let w = ((cw as f64 * s).floor() as i32).max(1);
    let h = ((ch as f64 * s).floor() as i32).max(1);
    ((fw - w) / 2, (fh - h) / 2, w, h)
}

/// FLTK widget labels treat '@' as a symbol prefix.
fn label_escape(s: &str) -> String {
    s.replace('@', "@@")
}

/// FLTK menu labels treat / & _ \ specially.
fn menu_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        match c {
            '/' => o.push_str("\\/"),
            '@' => o.push_str("@@"),
            '\\' => o.push_str("\\\\"),
            '&' => o.push_str("&&"),
            '_' if i == 0 => o.push_str("\\_"),
            '|' => o.push(' '),
            _ => o.push(c),
        }
    }
    o
}

// ----------------------------------------------------------------------------
// UI messages / queue
// ----------------------------------------------------------------------------
#[derive(Clone, Debug, PartialEq)]
enum Msg {
    Tick,
    Quit,
    SceneSelect,
    SceneAdd,
    SceneRemove,
    SceneRename,
    SceneUp,
    SceneDown,
    SourceSelect,
    SourceAdd,
    SourceEdit,
    SourceRemove,
    SourceToggle,
    SourceUp,
    SourceDown,
    PropsApply,
    PropsFill,
    PropsCenter,
    PropsNative,
    PropsOpacity,
    ToggleRecord,
    ToggleStream,
    Screenshot,
    Settings,
    SaveProfile,
    SaveProfileAs,
    LoadProfile,
    About,
    PreviewPush(i32, i32, bool),
    PreviewDrag(i32, i32),
    PreviewRelease,
}

#[derive(Clone)]
struct Q(Rc<RefCell<VecDeque<Msg>>>);
impl Q {
    fn send(&self, m: Msg) {
        let mut q = self.0.borrow_mut();
        if m == Msg::Tick && q.back() == Some(&Msg::Tick) {
            return; // coalesce ticks while a modal dialog runs
        }
        if q.len() < 10_000 {
            q.push_back(m);
        }
    }
    fn pop(&self) -> Option<Msg> {
        self.0.borrow_mut().pop_front()
    }
    fn cb<W>(&self, m: Msg) -> impl FnMut(&mut W) + 'static {
        let q = self.clone();
        move |_| q.send(m.clone())
    }
}

#[derive(Default)]
struct Overlay {
    cw: u32,
    ch: u32,
    sel: Option<(i32, i32, i32, i32)>,
}

struct Ui {
    win: Window,
    preview: Frame,
    scenes: HoldBrowser,
    sources: HoldBrowser,
    px: IntInput,
    py: IntInput,
    pw: IntInput,
    ph: IntInput,
    opac: HorValueSlider,
    btn_rec: Button,
    btn_stream: Button,
    status: Frame,
    src_status: Frame,
    logbuf: TextBuffer,
    logview: TextDisplay,
}

fn col_header(t: &str) -> Frame {
    let mut f = Frame::default().with_label(t);
    f.set_label_font(Font::HelveticaBold);
    f.set_label_size(13);
    f.set_align(Align::Left | Align::Inside);
    f
}

fn small_btn(l: &str, tip: &str) -> Button {
    let mut b = Button::default().with_label(l);
    b.set_tooltip(tip);
    b.set_label_size(12);
    b
}

fn build_ui(q: &Q, pv: Arc<PreviewShared>, ov: Arc<Mutex<Overlay>>) -> Ui {
    let mut win = Window::default()
        .with_size(1280, 820)
        .with_label(&format!("{APP_NAME} v{APP_VERSION}"));
    win.size_range(900, 600, 0, 0);
    let mut main = Flex::default_fill().column();
    main.set_margin(0);
    main.set_pad(0);

    // Menu --------------------------------------------------------------------
    let mut mb = MenuBar::default();
    let add = |mb: &mut MenuBar, path: &str, sc: Shortcut, m: Msg| {
        let q = q.clone();
        mb.add(path, sc, MenuFlag::Normal, move |_| q.send(m.clone()));
    };
    add(&mut mb, "&File/&Save Profile", Shortcut::Ctrl | 's', Msg::SaveProfile);
    add(&mut mb, "&File/Save Profile &As…", Shortcut::Ctrl | Shortcut::Shift | 's', Msg::SaveProfileAs);
    add(&mut mb, "&File/&Load Profile…", Shortcut::Ctrl | 'o', Msg::LoadProfile);
    add(&mut mb, "&File/&Quit", Shortcut::Ctrl | 'q', Msg::Quit);
    add(&mut mb, "&Controls/Toggle &Recording", Shortcut::Ctrl | 'r', Msg::ToggleRecord);
    add(&mut mb, "&Controls/Toggle &Streaming", Shortcut::Ctrl | 'l', Msg::ToggleStream);
    add(&mut mb, "&Controls/Screens&hot", Shortcut::None | Key::F12, Msg::Screenshot);
    add(&mut mb, "&Controls/S&ettings…", Shortcut::Ctrl | ',', Msg::Settings);
    add(&mut mb, "&Help/&About", Shortcut::None, Msg::About);
    main.fixed(&mb, 26);

    // Preview -----------------------------------------------------------------
    let mut preview = Frame::default();
    preview.set_frame(FrameType::FlatBox);
    preview.set_color(Color::from_rgb(22, 22, 26));
    {
        let (pv, ov) = (pv.clone(), ov.clone());
        preview.draw(move |f| {
            let (fx, fy, fw, fh) = (f.x(), f.y(), f.w(), f.h());
            draw::draw_rect_fill(fx, fy, fw, fh, Color::from_rgb(22, 22, 26));
            let o = lock(&ov);
            let (ox, oy, pw, ph) = fit_rect(o.cw, o.ch, fw, fh);
            if pw <= 0 {
                return;
            }
            {
                let g = lock(&pv.buf);
                if g.1 > 0 && g.2 > 0 && g.0.len() >= g.1 * g.2 * 3 {
                    if let Ok(mut img) = RgbImage::new(&g.0[..g.1 * g.2 * 3], g.1 as i32, g.2 as i32, ColorDepth::Rgb8) {
                        if g.1 as i32 != pw || g.2 as i32 != ph {
                            img.scale(pw, ph, false, true);
                        }
                        img.draw(fx + ox, fy + oy, pw, ph);
                    }
                } else {
                    draw::draw_rect_fill(fx + ox, fy + oy, pw, ph, Color::Black);
                }
            }
            if let Some((x, y, w, h)) = o.sel {
                let sx = |v: i32| fx + ox + (v as f64 * pw as f64 / o.cw as f64).round() as i32;
                let sy = |v: i32| fy + oy + (v as f64 * ph as f64 / o.ch as f64).round() as i32;
                let (x0, y0, x1, y1) = (sx(x), sy(y), sx(x + w), sy(y + h));
                draw::push_clip(fx, fy, fw, fh);
                draw::set_draw_color(Color::from_rgb(255, 64, 64));
                draw::set_line_style(draw::LineStyle::Solid, 2);
                draw::draw_rect(x0, y0, (x1 - x0).max(2), (y1 - y0).max(2));
                draw::draw_rect_fill(x1 - 5, y1 - 5, 10, 10, Color::from_rgb(255, 64, 64));
                draw::set_line_style(draw::LineStyle::Solid, 0);
                draw::pop_clip();
            }
        });
    }
    {
        let q = q.clone();
        preview.handle(move |f, ev| {
            let (mx, my) = (app::event_x() - f.x(), app::event_y() - f.y());
            match ev {
                Event::Push if app::event_mouse_button() == app::MouseButton::Left => {
                    q.send(Msg::PreviewPush(mx, my, app::is_event_shift()));
                    true
                }
                Event::Drag => {
                    q.send(Msg::PreviewDrag(mx, my));
                    true
                }
                Event::Released => {
                    q.send(Msg::PreviewRelease);
                    true
                }
                _ => false,
            }
        });
    }

    // Dock --------------------------------------------------------------------
    let mut dock = Flex::default().row();
    dock.set_margin(6);
    dock.set_pad(8);

    // Scenes column
    let mut c1 = Flex::default().column();
    c1.set_pad(3);
    let h1 = col_header("Scenes");
    c1.fixed(&h1, 20);
    let mut scenes = HoldBrowser::default();
    scenes.set_callback(q.cb(Msg::SceneSelect));
    let mut r1 = Flex::default().row();
    r1.set_pad(2);
    small_btn("+", "Add scene").set_callback(q.cb(Msg::SceneAdd));
    small_btn("−", "Remove scene").set_callback(q.cb(Msg::SceneRemove));
    small_btn("Ren", "Rename scene").set_callback(q.cb(Msg::SceneRename));
    small_btn("@8>", "Move up").set_callback(q.cb(Msg::SceneUp));
    small_btn("@2>", "Move down").set_callback(q.cb(Msg::SceneDown));
    r1.end();
    c1.fixed(&r1, 26);
    c1.end();
    dock.fixed(&c1, 190);

    // Sources column
    let mut c2 = Flex::default().column();
    c2.set_pad(3);
    let h2 = col_header("Sources (top layer first)");
    c2.fixed(&h2, 20);
    let mut sources = HoldBrowser::default();
    {
        let q = q.clone();
        sources.set_callback(move |_| {
            q.send(Msg::SourceSelect);
            if app::event_clicks() {
                q.send(Msg::SourceEdit);
            }
        });
    }
    let mut r2 = Flex::default().row();
    r2.set_pad(2);
    small_btn("+", "Add source").set_callback(q.cb(Msg::SourceAdd));
    small_btn("−", "Remove source").set_callback(q.cb(Msg::SourceRemove));
    small_btn("Edit", "Edit source").set_callback(q.cb(Msg::SourceEdit));
    small_btn("Eye", "Show/hide").set_callback(q.cb(Msg::SourceToggle));
    small_btn("@8>", "Raise").set_callback(q.cb(Msg::SourceUp));
    small_btn("@2>", "Lower").set_callback(q.cb(Msg::SourceDown));
    r2.end();
    c2.fixed(&r2, 26);
    c2.end();
    dock.fixed(&c2, 270);

    // Transform column
    let mut c3 = Flex::default().column();
    c3.set_pad(4);
    let h3 = col_header("Transform");
    c3.fixed(&h3, 20);
    let mk_row = |c: &mut Flex, l1: &str, l2: &str| -> (IntInput, IntInput) {
        let mut r = Flex::default().row();
        r.set_pad(4);
        let f1 = Frame::default().with_label(l1);
        r.fixed(&f1, 16);
        let a = IntInput::default();
        let f2 = Frame::default().with_label(l2);
        r.fixed(&f2, 16);
        let b = IntInput::default();
        r.end();
        c.fixed(&r, 26);
        (a, b)
    };
    let (px, py) = mk_row(&mut c3, "X", "Y");
    let (pw, ph) = mk_row(&mut c3, "W", "H");
    let mut opac = HorValueSlider::default().with_label("Opacity %");
    opac.set_align(Align::Bottom);
    opac.set_range(0.0, 100.0);
    opac.set_step(1.0, 1);
    opac.set_value(100.0);
    opac.set_callback(q.cb(Msg::PropsOpacity));
    c3.fixed(&opac, 22);
    let sp = Frame::default();
    c3.fixed(&sp, 16);
    let mut r3 = Flex::default().row();
    r3.set_pad(2);
    small_btn("Apply", "Apply X/Y/W/H").set_callback(q.cb(Msg::PropsApply));
    small_btn("Fill", "Fit to canvas, keep aspect").set_callback(q.cb(Msg::PropsFill));
    small_btn("Center", "Center on canvas").set_callback(q.cb(Msg::PropsCenter));
    small_btn("1:1", "Native capture size").set_callback(q.cb(Msg::PropsNative));
    r3.end();
    c3.fixed(&r3, 26);
    let mut src_status = Frame::default();
    src_status.set_align(Align::Left | Align::Inside | Align::Wrap | Align::Top);
    src_status.set_label_size(11);
    c3.end();
    dock.fixed(&c3, 250);

    // Controls column
    let mut c4 = Flex::default().column();
    c4.set_pad(6);
    let h4 = col_header("Controls");
    c4.fixed(&h4, 20);
    let mut btn_stream = Button::default().with_label("Start Streaming");
    btn_stream.set_callback(q.cb(Msg::ToggleStream));
    c4.fixed(&btn_stream, 34);
    let mut btn_rec = Button::default().with_label("Start Recording");
    btn_rec.set_callback(q.cb(Msg::ToggleRecord));
    c4.fixed(&btn_rec, 34);
    let mut b_shot = Button::default().with_label("Screenshot");
    b_shot.set_callback(q.cb(Msg::Screenshot));
    c4.fixed(&b_shot, 28);
    let mut b_set = Button::default().with_label("Settings…");
    b_set.set_callback(q.cb(Msg::Settings));
    c4.fixed(&b_set, 28);
    Frame::default();
    c4.end();
    dock.fixed(&c4, 170);

    // Log column
    let mut c5 = Flex::default().column();
    c5.set_pad(3);
    let h5 = col_header("Log");
    c5.fixed(&h5, 20);
    let logbuf = TextBuffer::default();
    let mut logview = TextDisplay::default();
    logview.set_buffer(logbuf.clone());
    logview.set_text_size(11);
    logview.set_text_font(Font::Courier);
    c5.end();
    dock.end();
    main.fixed(&dock, 236);

    let mut status = Frame::default();
    status.set_frame(FrameType::FlatBox);
    status.set_color(Color::from_rgb(40, 40, 46));
    status.set_label_color(Color::from_rgb(220, 220, 220));
    status.set_align(Align::Left | Align::Inside);
    status.set_label_size(12);
    main.fixed(&status, 22);
    main.end();
    win.end();
    win.resizable(&main);
    {
        let q = q.clone();
        win.set_callback(move |_| {
            // Escape also fires the window callback; only a real close quits.
            if app::event() == Event::Close {
                q.send(Msg::Quit);
            }
        });
    }
    Ui { win, preview, scenes, sources, px, py, pw, ph, opac, btn_rec, btn_stream, status, src_status, logbuf, logview }
}

// ----------------------------------------------------------------------------
// Dialogs
// ----------------------------------------------------------------------------
fn modal_run(win: &mut Window) {
    win.make_modal(true);
    win.show();
    while win.shown() {
        app::wait();
    }
}

fn fill_choice(c: &mut Choice, items: &[(String, String)]) {
    c.clear();
    for (_, l) in items {
        c.add(&menu_escape(l), Shortcut::None, MenuFlag::Normal, |_| {});
    }
}

/// Add/Edit source dialog. Returns (name, kind) on OK.
fn source_dialog(init: Option<&Source>, st: &Settings) -> Option<(String, SourceKind)> {
    let mut win = Window::default()
        .with_size(580, 440)
        .center_screen()
        .with_label(if init.is_some() { "Edit Source" } else { "Add Source" });
    let mut name = Input::new(170, 14, 390, 26, "Name:");
    let mut kind = Choice::new(170, 48, 390, 26, "Type:");
    kind.add_choice("Display Capture|Webcam \\/ Video Device|Media File|Image|Color");
    let mut target = Input::new(170, 82, 290, 26, "Display / Device / Path:");
    let mut browse = Button::new(466, 82, 94, 26, "Browse…");
    let mut detected = Choice::new(170, 116, 290, 26, "Detected:");
    let mut detect_btn = Button::new(466, 116, 94, 26, "Detect");
    let mut monitor = Choice::new(170, 150, 390, 26, "Monitor:");
    let mut rx = IntInput::new(170, 184, 80, 26, "Region X / Y / W / H:");
    let mut ry = IntInput::new(256, 184, 80, 26, None);
    let mut rw = IntInput::new(342, 184, 100, 26, None);
    let mut rh = IntInput::new(448, 184, 112, 26, None);
    let mut cursor = CheckButton::new(170, 216, 160, 26, "Capture cursor");
    let mut looping = CheckButton::new(340, 216, 160, 26, "Loop media");
    let mut wsize = Input::new(170, 248, 120, 26, "Webcam size (WxH):");
    let mut wfps = IntInput::new(420, 248, 60, 26, "Webcam FPS:");
    let mut color_btn = Button::new(170, 282, 140, 26, "Pick colour…");
    let mut swatch = Frame::new(320, 282, 60, 26, None);
    swatch.set_frame(FrameType::DownBox);
    let mut hint = Frame::new(20, 316, 540, 66, None);
    hint.set_align(Align::Left | Align::Inside | Align::Wrap | Align::Top);
    hint.set_label_size(12);
    let mut ok = Button::new(370, 396, 90, 30, "OK");
    let mut cancel = Button::new(470, 396, 90, 30, "Cancel");
    win.end();

    let screens = screens_px();
    let mut mons: Vec<(String, String)> = screens
        .iter()
        .enumerate()
        .map(|(i, (x, y, w, h))| (i.to_string(), format!("Screen {}: {}x{} at {},{}", i + 1, w, h, x, y)))
        .collect();
    let union = screens.iter().fold(None::<(i32, i32, i32, i32)>, |acc, &(x, y, w, h)| match acc {
        None => Some((x, y, x + w, y + h)),
        Some((a, b, c, d)) => Some((a.min(x), b.min(y), c.max(x + w), d.max(y + h))),
    });
    if screens.len() > 1
        && let Some((a, b, c, d)) = union {
            mons.push(("all".into(), format!("All screens: {}x{} at {},{}", c - a, d - b, a, b)));
        }
    fill_choice(&mut monitor, &mons);

    let color = Rc::new(Cell::new((40u8, 40u8, 48u8)));
    let detected_items: Rc<RefCell<DevList>> = Rc::new(RefCell::new(vec![]));
    let result = Rc::new(Cell::new(0)); // 1 ok, 2 cancel

    // Prefill
    let (p_sx, p_sy, p_sw, p_sh) = primary_screen_px();
    rx.set_value(&p_sx.to_string());
    ry.set_value(&p_sy.to_string());
    rw.set_value(&p_sw.to_string());
    rh.set_value(&p_sh.to_string());
    cursor.set_checked(true);
    looping.set_checked(true);
    wfps.set_value("0");
    match init {
        Some(s) => {
            name.set_value(&s.name);
            kind.set_value(s.kind.index());
            match &s.kind {
                SourceKind::Display { target: t, x, y, w, h, cursor: c } => {
                    target.set_value(t);
                    rx.set_value(&x.to_string());
                    ry.set_value(&y.to_string());
                    rw.set_value(&w.to_string());
                    rh.set_value(&h.to_string());
                    cursor.set_checked(*c);
                }
                SourceKind::Webcam { device, size, fps } => {
                    target.set_value(device);
                    wsize.set_value(size);
                    wfps.set_value(&fps.to_string());
                }
                SourceKind::Media { path, looping: l } => {
                    target.set_value(path);
                    looping.set_checked(*l);
                }
                SourceKind::Image { path } => target.set_value(path),
                SourceKind::Color { r, g, b } => color.set((*r, *g, *b)),
            }
        }
        None => {
            kind.set_value(0);
            name.set_value("Display Capture");
            target.set_value(&default_display_target());
            if !mons.is_empty() {
                monitor.set_value(0);
            }
        }
    }
    let (r, g, b) = color.get();
    swatch.set_color(Color::from_rgb(r, g, b));

    {
        let (mut rx, mut ry, mut rw, mut rh, mons) = (rx.clone(), ry.clone(), rw.clone(), rh.clone(), mons.clone());
        let screens = screens.clone();
        monitor.set_callback(move |c| {
            let i = c.value();
            if i < 0 { return; }
            let (x, y, w, h) = match mons.get(i as usize).map(|m| m.0.as_str()) {
                Some("all") => match union { Some((a, b, c2, d)) => (a, b, c2 - a, d - b), None => return },
                Some(_) => match screens.get(i as usize) { Some(s) => *s, None => return },
                None => return,
            };
            rx.set_value(&x.to_string());
            ry.set_value(&y.to_string());
            rw.set_value(&w.to_string());
            rh.set_value(&h.to_string());
        });
    }
    {
        let (mut target, items) = (target.clone(), detected_items.clone());
        detected.set_callback(move |c| {
            if let Some((v, _)) = items.borrow().get(c.value().max(0) as usize) {
                target.set_value(v);
            }
        });
    }
    {
        let (kind, mut target) = (kind.clone(), target.clone());
        browse.set_callback(move |_| {
            let mut fc = dialog::NativeFileChooser::new(dialog::NativeFileChooserType::BrowseFile);
            if kind.value() == 3 {
                fc.set_filter("Images\t*.{png,jpg,jpeg,bmp,gif,webp,tif,tiff}");
            } else if kind.value() == 2 {
                fc.set_filter("Media\t*.{mp4,mkv,mov,webm,avi,flv,ts,gif,mp3,m4v}");
            }
            fc.show();
            let f = fc.filename();
            if !f.as_os_str().is_empty() {
                target.set_value(&f.to_string_lossy());
            }
        });
    }
    {
        let (kind, mut detected, items) = (kind.clone(), detected.clone(), detected_items.clone());
        let ff = st.ffmpeg_path.clone();
        detect_btn.set_callback(move |_| {
            draw::set_cursor(fltk::enums::Cursor::Wait);
            app::flush();
            let list = if kind.value() == 1 { detect_video_devices(&ff) } else { vec![] };
            draw::set_cursor(fltk::enums::Cursor::Default);
            fill_choice(&mut detected, &list);
            if list.is_empty() {
                dialog::message_default("No devices detected for this source type.");
            } else {
                detected.set_value(0);
            }
            *items.borrow_mut() = list;
        });
    }
    {
        let (color, mut swatch) = (color.clone(), swatch.clone());
        color_btn.set_callback(move |_| {
            if let Some((r, g, b)) = dialog::color_chooser("Source colour", dialog::ColorMode::Byte) {
                color.set((r, g, b));
                swatch.set_color(Color::from_rgb(r, g, b));
                swatch.redraw();
            }
        });
    }
    {
        let (res, mut w) = (result.clone(), win.clone());
        ok.set_callback(move |_| {
            res.set(1);
            w.hide();
        });
    }
    {
        let (res, mut w) = (result.clone(), win.clone());
        cancel.set_callback(move |_| {
            res.set(2);
            w.hide();
        });
    }

    let names = ["Display Capture", "Webcam", "Media", "Image", "Color"];
    let update = |k: i32, target: &mut Input, browse: &mut Button, detected: &mut Choice, detect_btn: &mut Button,
                  monitor: &mut Choice, reg: &mut [&mut IntInput; 4], cursor: &mut CheckButton, looping: &mut CheckButton,
                  wsize: &mut Input, wfps: &mut IntInput, color_btn: &mut Button, hint: &mut Frame| {
        let on = |w: &mut dyn WidgetExt, b: bool| if b { w.activate() } else { w.deactivate() };
        on(target, k != 4);
        on(browse, k == 2 || k == 3);
        on(detected, k == 1);
        on(detect_btn, k == 1);
        on(monitor, k == 0);
        for r in reg.iter_mut() {
            on(*r, k == 0);
        }
        on(cursor, k == 0);
        on(looping, k == 2);
        on(wsize, k == 1);
        on(wfps, k == 1);
        on(color_btn, k == 4);
        hint.set_label(match k {
            0 if cfg!(windows) => "Target 'desktop' captures the region below. Use 'title=Window Title' to capture one window.",
            0 if cfg!(target_os = "macos") => "Target is the AVFoundation screen index (usually 1). Region is cropped from it.",
            0 => "Target is the X11 display (e.g. :0.0). Wayland-only sessions need XWayland; pure Wayland is not supported by x11grab.",
            1 => "Device path (/dev/video0), DirectShow name or AVFoundation index. Press Detect to list devices. Size/FPS blank or 0 = driver default.",
            2 => "Any file ffmpeg can decode. Plays in real time; audio of media files is not mixed (use desktop audio).",
            3 => "PNG/JPEG/etc. Transparency is preserved.",
            _ => "Solid colour fill. Combine with opacity for tints.",
        });
    };
    let mut last_k = -1;
    loop {
        win.make_modal(true);
        win.show();
        while win.shown() {
            app::wait();
            let k = kind.value();
            if k != last_k {
                // Swap default target when the user changes type on a fresh source.
                if last_k >= 0 {
                    let t = target.value();
                    let was_default = t.is_empty() || t == default_display_target() || t == default_webcam_target();
                    if was_default {
                        target.set_value(&match k { 0 => default_display_target(), 1 => default_webcam_target(), _ => String::new() });
                    }
                    if names.contains(&name.value().as_str()) {
                        name.set_value(names[k.clamp(0, 4) as usize]);
                    }
                }
                let (mut a, mut b, mut c, mut d) = (rx.clone(), ry.clone(), rw.clone(), rh.clone());
                update(k, &mut target, &mut browse, &mut detected, &mut detect_btn, &mut monitor, &mut [&mut a, &mut b, &mut c, &mut d],
                       &mut cursor, &mut looping, &mut wsize, &mut wfps, &mut color_btn, &mut hint);
                last_k = k;
            }
        }
        if result.get() != 1 {
            return None;
        }
        result.set(0);
        let nm = truncate_chars(name.value().trim(), 64);
        let t = target.value().trim().replace('\0', "");
        let pi = |i: &IntInput| i.value().trim().parse::<i64>().ok();
        let res: Result<SourceKind, String> = match kind.value() {
            0 => match (pi(&rx), pi(&ry), pi(&rw), pi(&rh)) {
                (Some(x), Some(y), Some(w), Some(h)) if (1..=16384).contains(&w) && (1..=16384).contains(&h)
                    && (-32768..=32767).contains(&x) && (-32768..=32767).contains(&y) => Ok(SourceKind::Display {
                    target: t, x: x as i32, y: y as i32, w: w as u32, h: h as u32, cursor: cursor.is_checked(),
                }),
                _ => Err("Region must be numeric; W/H between 1 and 16384.".into()),
            },
            1 => {
                let sz = wsize.value().trim().to_string();
                if t.is_empty() {
                    Err("Enter or detect a device.".into())
                } else if !sz.is_empty() && parse_size(&sz).is_none() {
                    Err("Webcam size must look like 1280x720.".into())
                } else {
                    let f = pi(&wfps).unwrap_or(0).clamp(0, MAX_FPS as i64) as u32;
                    Ok(SourceKind::Webcam { device: t, size: sz, fps: f })
                }
            }
            2 if t.is_empty() => Err("Choose a media file.".into()),
            2 => Ok(SourceKind::Media { path: t, looping: looping.is_checked() }),
            3 if t.is_empty() => Err("Choose an image file.".into()),
            3 => Ok(SourceKind::Image { path: t }),
            _ => {
                let (r, g, b) = color.get();
                Ok(SourceKind::Color { r, g, b })
            }
        };
        match res {
            Ok(k) => {
                let nm = if nm.is_empty() { k.label().to_string() } else { nm };
                return Some((nm, k));
            }
            Err(e) => dialog::alert_default(&e),
        }
    }
}

fn settings_dialog(cur: &Settings, ff: &FfInfo) -> Option<Settings> {
    let mut win = Window::default().with_size(640, 620).center_screen().with_label("Settings");
    let x = 190;
    let mut y = 14;
    let mut row = |h: i32| {
        let r = y;
        y += h + 8;
        r
    };
    let r = row(26);
    let mut cw = IntInput::new(x, r, 90, 26, "Canvas W x H:");
    let mut chh = IntInput::new(x + 100, r, 90, 26, None);
    let mut fps = IntInput::new(x + 330, r, 60, 26, "FPS:");
    let r = row(26);
    let mut enc = Choice::new(x, r, 200, 26, "Video encoder:");
    let mut preset = Choice::new(x + 330, r, 110, 26, "x264 preset:");
    let r = row(26);
    let mut vk = IntInput::new(x, r, 90, 26, "Video kbps:");
    let mut ak = IntInput::new(x + 190, r, 60, 26, "Audio kbps:");
    let mut ki = IntInput::new(x + 380, r, 50, 26, "Keyint s:");
    let r = row(26);
    let mut recdir = Input::new(x, r, 330, 26, "Recording folder:");
    let mut recbrowse = Button::new(x + 340, r, 90, 26, "Browse…");
    let r = row(26);
    let mut fmt = Choice::new(x, r, 100, 26, "Recording format:");
    let mut fmt_hint = Frame::new(x + 110, r, 320, 26, "mkv survives crashes; remux to mp4 later");
    fmt_hint.set_label_size(11);
    fmt_hint.set_align(Align::Left | Align::Inside);
    let r = row(26);
    let mut url = Input::new(x, r, 430, 26, "Stream URL:");
    let r = row(26);
    let mut key = SecretInput::new(x, r, 330, 26, "Stream key:");
    let mut show_key = CheckButton::new(x + 340, r, 90, 26, "Show");
    let r = row(26);
    let mut api = Choice::new(x, r, 150, 26, "Audio backend:");
    let mut detect_btn = Button::new(x + 160, r, 130, 26, "Detect devices");
    let r = row(26);
    let mut mic = Input::new(x, r, 240, 26, "Mic device:");
    let mut mic_pick = Choice::new(x + 250, r, 180, 26, None);
    let r = row(26);
    let mut desk = Input::new(x, r, 240, 26, "Desktop audio device:");
    let mut desk_pick = Choice::new(x + 250, r, 180, 26, None);
    let r = row(26);
    let mut ffp = Input::new(x, r, 330, 26, "ffmpeg binary:");
    let mut ffbrowse = Button::new(x + 340, r, 90, 26, "Browse…");
    let r = row(26);
    let mut vaapi = Input::new(x, r, 240, 26, "VAAPI device:");
    let r = row(46);
    let mut info = Frame::new(20, r, 600, 46, None);
    info.set_align(Align::Left | Align::Inside | Align::Wrap);
    info.set_label_size(11);
    let enc_list: Vec<String> = ENCODERS
        .iter()
        .filter(|e| !ff.ok || ff.encoders.is_empty() || ff.encoders.contains(**e) || **e == cur.encoder)
        .map(|e| e.to_string())
        .collect();
    info.set_label(&label_escape(&format!(
        "{}\nAvailable H.264 encoders in this ffmpeg: {}. Leave a device blank to disable it.",
        if ff.ok { ff.version.as_str() } else { "ffmpeg NOT FOUND — set the binary path above." },
        enc_list.join(", ")
    )));
    let mut ok = Button::new(430, 576, 90, 30, "OK");
    let mut cancel = Button::new(530, 576, 90, 30, "Cancel");
    win.end();

    cw.set_value(&cur.canvas_w.to_string());
    chh.set_value(&cur.canvas_h.to_string());
    fps.set_value(&cur.fps.to_string());
    for e in &enc_list {
        enc.add(&menu_escape(e), Shortcut::None, MenuFlag::Normal, |_| {});
    }
    enc.set_value(enc_list.iter().position(|e| *e == cur.encoder).unwrap_or(0) as i32);
    preset.add_choice(&X264_PRESETS.join("|"));
    preset.set_value(X264_PRESETS.iter().position(|p| *p == cur.x264_preset).unwrap_or(2) as i32);
    vk.set_value(&cur.video_kbps.to_string());
    ak.set_value(&cur.audio_kbps.to_string());
    ki.set_value(&cur.keyint_sec.to_string());
    recdir.set_value(&cur.rec_dir);
    fmt.add_choice(&REC_FORMATS.join("|"));
    fmt.set_value(REC_FORMATS.iter().position(|p| *p == cur.rec_format).unwrap_or(0) as i32);
    url.set_value(&cur.stream_url);
    key.set_value(&cur.stream_key);
    api.add_choice(&AUDIO_APIS.join("|"));
    api.set_value(AUDIO_APIS.iter().position(|p| *p == cur.audio_api).unwrap_or(0) as i32);
    mic.set_value(&cur.mic_device);
    desk.set_value(&cur.desktop_device);
    ffp.set_value(&cur.ffmpeg_path);
    vaapi.set_value(&cur.vaapi_device);

    let devs: Rc<RefCell<DevList>> = Rc::new(RefCell::new(vec![]));
    {
        let mut key = key.clone();
        show_key.set_callback(move |c| {
            // SecretInput can't toggle masking; show the key in a message box instead.
            if c.is_checked() {
                dialog::message_default(&format!("Stream key:\n{}", key.value()));
                c.set_checked(false);
            }
            key.redraw();
        });
    }
    {
        let mut recdir = recdir.clone();
        recbrowse.set_callback(move |_| {
            let mut fc = dialog::NativeFileChooser::new(dialog::NativeFileChooserType::BrowseDir);
            fc.show();
            let f = fc.filename();
            if !f.as_os_str().is_empty() {
                recdir.set_value(&f.to_string_lossy());
            }
        });
    }
    {
        let mut ffp = ffp.clone();
        ffbrowse.set_callback(move |_| {
            let mut fc = dialog::NativeFileChooser::new(dialog::NativeFileChooserType::BrowseFile);
            fc.show();
            let f = fc.filename();
            if !f.as_os_str().is_empty() {
                ffp.set_value(&f.to_string_lossy());
            }
        });
    }
    {
        let (api, ffp, devs) = (api.clone(), ffp.clone(), devs.clone());
        let (mut mp, mut dp) = (mic_pick.clone(), desk_pick.clone());
        detect_btn.set_callback(move |_| {
            let a = AUDIO_APIS.get(api.value().max(0) as usize).copied().unwrap_or("none");
            draw::set_cursor(fltk::enums::Cursor::Wait);
            app::flush();
            let list = detect_audio_devices(a, &ffp.value());
            draw::set_cursor(fltk::enums::Cursor::Default);
            fill_choice(&mut mp, &list);
            fill_choice(&mut dp, &list);
            if list.is_empty() {
                dialog::message_default("No audio devices detected for this backend.");
            }
            *devs.borrow_mut() = list;
        });
    }
    for (pick, inp) in [(&mut mic_pick, mic.clone()), (&mut desk_pick, desk.clone())] {
        let devs = devs.clone();
        let mut inp = inp;
        pick.set_callback(move |c| {
            if let Some((v, _)) = devs.borrow().get(c.value().max(0) as usize) {
                inp.set_value(v);
            }
        });
    }
    let result = Rc::new(Cell::new(0));
    {
        let (res, mut w) = (result.clone(), win.clone());
        ok.set_callback(move |_| {
            res.set(1);
            w.hide();
        });
    }
    {
        let (res, mut w) = (result.clone(), win.clone());
        cancel.set_callback(move |_| {
            res.set(2);
            w.hide();
        });
    }
    loop {
        modal_run(&mut win);
        if result.get() != 1 {
            return None;
        }
        result.set(0);
        let num = |i: &IntInput| i.value().trim().parse::<u32>().ok();
        let mut s = cur.clone();
        let mut errs = vec![];
        match (num(&cw), num(&chh)) {
            (Some(w), Some(h)) if (160..=MAX_W).contains(&w) && (120..=MAX_H).contains(&h) => {
                s.canvas_w = even(w);
                s.canvas_h = even(h);
            }
            _ => errs.push(format!("Canvas must be 160..{MAX_W} x 120..{MAX_H}.")),
        }
        match num(&fps) {
            Some(f) if (1..=MAX_FPS).contains(&f) => s.fps = f,
            _ => errs.push(format!("FPS must be 1..{MAX_FPS}.")),
        }
        match num(&vk) {
            Some(v) if (100..=200_000).contains(&v) => s.video_kbps = v,
            _ => errs.push("Video kbps must be 100..200000.".into()),
        }
        match num(&ak) {
            Some(v) if (32..=512).contains(&v) => s.audio_kbps = v,
            _ => errs.push("Audio kbps must be 32..512.".into()),
        }
        match num(&ki) {
            Some(v) if (1..=20).contains(&v) => s.keyint_sec = v,
            _ => errs.push("Keyframe interval must be 1..20 s.".into()),
        }
        s.encoder = enc_list.get(enc.value().max(0) as usize).cloned().unwrap_or_else(|| "libx264".into());
        s.x264_preset = X264_PRESETS.get(preset.value().max(0) as usize).unwrap_or(&"veryfast").to_string();
        s.rec_format = REC_FORMATS.get(fmt.value().max(0) as usize).unwrap_or(&"mkv").to_string();
        s.audio_api = AUDIO_APIS.get(api.value().max(0) as usize).unwrap_or(&"none").to_string();
        s.rec_dir = recdir.value().trim().to_string();
        s.stream_url = url.value().trim().to_string();
        s.stream_key = key.value().trim().to_string();
        s.mic_device = mic.value().trim().to_string();
        s.desktop_device = desk.value().trim().to_string();
        s.ffmpeg_path = ffp.value().trim().to_string();
        s.vaapi_device = vaapi.value().trim().to_string();
        if s.ffmpeg_path.is_empty() {
            errs.push("ffmpeg binary path is required.".into());
        }
        if !s.stream_url.is_empty()
            && let Err(e) = stream_target(&s) {
                errs.push(e);
            }
        if errs.is_empty() {
            return Some(s);
        }
        dialog::alert_default(&errs.join("\n"));
    }
}

// ----------------------------------------------------------------------------
// Application state & message handling (UI thread only)
// ----------------------------------------------------------------------------
struct DragState {
    id: u64,
    start: (i32, i32),
    orig: (i32, i32, u32, u32),
    resize: bool,
    moved: bool,
}

struct AppState {
    prof: Profile,
    profile_path: PathBuf,
    ui: Ui,
    log: Logger,
    log_seq: u64,
    ff: FfInfo,
    pv: Arc<PreviewShared>,
    ov: Arc<Mutex<Overlay>>,
    comp: Option<Compositor>,
    runtimes: HashMap<String, SourceRuntime>,
    rec: Option<OutputSession>,
    stream: Option<OutputSession>,
    stream_user_on: bool,
    stream_retry: Option<(Instant, u32)>,
    stoppers: Vec<JoinHandle<()>>,
    sel: Option<u64>,
    drag: Option<DragState>,
    dirty: bool,
    last_save: Instant,
    quit: bool,
    auto_record: Option<(Instant, Duration)>, // (start at, duration)
    auto_record_stop: Option<Instant>,
    rec_path: Option<PathBuf>,
}

impl AppState {
    fn scene(&self) -> &Scene {
        &self.prof.scenes[self.prof.current]
    }
    fn scene_mut(&mut self) -> &mut Scene {
        let c = self.prof.current;
        &mut self.prof.scenes[c]
    }
    fn sel_index(&self) -> Option<usize> {
        let id = self.sel?;
        self.scene().sources.iter().position(|s| s.id == id)
    }
    fn outputs_active(&self) -> bool {
        self.rec.is_some() || self.stream.is_some() || self.stream_retry.is_some()
    }

    // --- pipeline -----------------------------------------------------------
    fn start_pipeline(&mut self) {
        let s = &self.prof.settings;
        {
            let mut o = lock(&self.ov);
            o.cw = s.canvas_w;
            o.ch = s.canvas_h;
        }
        self.comp = Some(Compositor::start(s.canvas_w, s.canvas_h, s.fps, self.pv.clone()));
        self.apply_scene();
    }
    fn stop_pipeline(&mut self) {
        for (_, rt) in self.runtimes.drain() {
            rt.stop();
        }
        if let Some(c) = self.comp.take() {
            c.stop();
        }
    }

    /// Start/stop source processes so exactly the current scene's visible
    /// sources run, then publish the new layer list.
    fn apply_scene(&mut self) {
        let (fps, ff) = (self.prof.settings.fps, self.prof.settings.ffmpeg_path.clone());
        let want: Vec<(String, Source)> = self
            .scene()
            .sources
            .iter()
            .filter(|s| s.visible)
            .map(|s| (source_sig(s, fps, &ff), s.clone()))
            .collect();
        let keep: HashSet<&String> = want.iter().map(|(k, _)| k).collect();
        let dead: Vec<String> = self.runtimes.keys().filter(|k| !keep.contains(k)).cloned().collect();
        for k in dead {
            if let Some(rt) = self.runtimes.remove(&k) {
                rt.stop();
            }
        }
        let ctx = RtCtx { ffmpeg: ff, fps, log: self.log.clone() };
        for (k, s) in &want {
            if !self.runtimes.contains_key(k) {
                self.runtimes.insert(k.clone(), SourceRuntime::start(s, &ctx));
            }
        }
        self.publish_layers();
    }

    /// Cheap: re-publish positions/opacity without touching processes.
    fn publish_layers(&mut self) {
        let (fps, ff) = (self.prof.settings.fps, self.prof.settings.ffmpeg_path.clone());
        let layers: Vec<Layer> = self
            .scene()
            .sources
            .iter()
            .filter(|s| s.visible)
            .filter_map(|s| {
                let rt = self.runtimes.get(&source_sig(s, fps, &ff))?;
                Some(Layer { x: s.x, y: s.y, w: s.w, h: s.h, opacity: s.opacity, slot: rt.slot.clone(), opaque: rt.opaque.clone() })
            })
            .collect();
        if let Some(c) = &self.comp {
            *lock(&c.sh.layers) = Arc::new(layers);
        }
        self.update_overlay();
    }

    fn update_overlay(&mut self) {
        let sel = self.sel_index().map(|i| {
            let s = &self.scene().sources[i];
            (s.x, s.y, s.w as i32, s.h as i32)
        });
        let sel = match &self.drag {
            Some(d) if d.resize => {
                let s = self.sel_index().map(|i| &self.scene().sources[i]);
                s.map(|s| (s.x, s.y, s.w as i32, s.h as i32)).or(sel)
            }
            _ => sel,
        };
        lock(&self.ov).sel = sel;
        self.ui.preview.redraw();
    }

    // --- widgets -------------------------------------------------------------
    fn refresh_scenes(&mut self) {
        let b = &mut self.ui.scenes;
        b.clear();
        for sc in &self.prof.scenes {
            b.add(&format!("@.{}", sc.name));
        }
        b.select(self.prof.current as i32 + 1);
    }
    fn refresh_sources(&mut self) {
        let srcs = self.scene().sources.clone();
        let sel = self.sel;
        let b = &mut self.ui.sources;
        b.clear();
        let mut line = 0;
        for (i, s) in srcs.iter().enumerate().rev() {
            b.add(&format!("@.{} {}  [{}]", if s.visible { "●" } else { "○" }, s.name, s.kind.label()));
            if Some(s.id) == sel {
                line = (srcs.len() - i) as i32;
            }
        }
        if line > 0 {
            b.select(line);
        }
        self.load_props();
    }
    fn load_props(&mut self) {
        let s = self.sel_index().map(|i| self.scene().sources[i].clone());
        let ui = &mut self.ui;
        match s {
            Some(s) => {
                ui.px.set_value(&s.x.to_string());
                ui.py.set_value(&s.y.to_string());
                ui.pw.set_value(&s.w.to_string());
                ui.ph.set_value(&s.h.to_string());
                ui.opac.set_value((s.opacity as f64 * 100.0 / 255.0).round());
            }
            None => {
                for i in [&mut ui.px, &mut ui.py, &mut ui.pw, &mut ui.ph] {
                    i.set_value("");
                }
            }
        }
        self.update_overlay();
    }
    fn browser_to_index(&self, line: i32) -> Option<usize> {
        let n = self.scene().sources.len();
        (line >= 1 && (line as usize) <= n).then(|| n - line as usize)
    }
    fn update_buttons(&mut self) {
        let rec = self.rec.is_some();
        let live = self.stream.is_some() || self.stream_retry.is_some();
        let b = &mut self.ui.btn_rec;
        b.set_label(if rec { "Stop Recording" } else { "Start Recording" });
        b.set_color(if rec { Color::from_rgb(200, 50, 50) } else { Color::BackGround });
        b.set_label_color(if rec { Color::White } else { Color::Foreground });
        let b = &mut self.ui.btn_stream;
        b.set_label(if live { "Stop Streaming" } else { "Start Streaming" });
        b.set_color(if live { Color::from_rgb(50, 110, 200) } else { Color::BackGround });
        b.set_label_color(if live { Color::White } else { Color::Foreground });
        self.ui.btn_rec.redraw();
        self.ui.btn_stream.redraw();
    }
    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn save(&mut self) {
        match save_profile(&self.profile_path, &self.prof) {
            Ok(()) => {
                self.dirty = false;
                self.last_save = Instant::now();
            }
            Err(e) => self.log.log(format!("profile save failed: {e}")),
        }
    }

    fn next_id(&self) -> u64 {
        self.prof.scenes.iter().flat_map(|s| s.sources.iter().map(|x| x.id)).max().unwrap_or(0) + 1
    }

    fn canvas_xy(&self, mx: i32, my: i32) -> Option<(i32, i32)> {
        let s = &self.prof.settings;
        let (ox, oy, pw, ph) = fit_rect(s.canvas_w, s.canvas_h, self.ui.preview.w(), self.ui.preview.h());
        if pw <= 0 || ph <= 0 {
            return None;
        }
        Some((
            ((mx - ox) as f64 * s.canvas_w as f64 / pw as f64).round() as i32,
            ((my - oy) as f64 * s.canvas_h as f64 / ph as f64).round() as i32,
        ))
    }

    // --- outputs -------------------------------------------------------------
    fn start_record(&mut self) -> Result<(), String> {
        let comp = self.comp.as_ref().ok_or("pipeline not running")?.sh.clone();
        let s = self.prof.settings.clone();
        let dir = PathBuf::from(&s.rec_dir);
        fs::create_dir_all(&dir).map_err(|e| format!("recording folder {}: {e}", dir.display()))?;
        let path = unique_path(&dir, &format!("litecast_{}", utc_stamp()), &s.rec_format);
        let p = path.to_string_lossy().into_owned();
        let sess = OutputSession::start(OutKind::Record, &s, &self.ff, &comp, &self.log, &p, p.clone())?;
        self.rec = Some(sess);
        self.rec_path = Some(path);
        Ok(())
    }
    fn stop_record(&mut self) {
        if let Some(r) = self.rec.take() {
            if let Some(h) = r.begin_stop(self.log.clone()) {
                self.stoppers.push(h);
            }
            if let Some(p) = self.rec_path.take() {
                self.log.log(format!("[rec] finalising {}", p.display()));
            }
        }
    }
    fn start_stream(&mut self) -> Result<(), String> {
        let comp = self.comp.as_ref().ok_or("pipeline not running")?.sh.clone();
        let s = self.prof.settings.clone();
        let target = stream_target(&s)?;
        self.log.set_secrets(secrets_of(&s));
        let label = s.stream_url.split('?').next().unwrap_or("").to_string() + " (key hidden)";
        self.stream = Some(OutputSession::start(OutKind::Stream, &s, &self.ff, &comp, &self.log, &target, label)?);
        Ok(())
    }
    fn stop_stream(&mut self) {
        self.stream_retry = None;
        self.stream_user_on = false;
        if let Some(r) = self.stream.take()
            && let Some(h) = r.begin_stop(self.log.clone()) {
                self.stoppers.push(h);
            }
    }

    fn screenshot(&mut self) {
        let Some(c) = &self.comp else { return };
        let Some(frame) = lock(&c.sh.canvas).clone() else {
            self.log.log("screenshot: no frame yet");
            return;
        };
        let s = &self.prof.settings;
        let dir = PathBuf::from(&s.rec_dir);
        let _ = fs::create_dir_all(&dir);
        let path = unique_path(&dir, &format!("litecast_shot_{}", utc_stamp()), "png");
        let args: Vec<String> = vec![
            "-hide_banner".into(), "-loglevel".into(), "error".into(), "-n".into(),
            "-f".into(), "rawvideo".into(), "-pix_fmt".into(), PIX.into(),
            "-video_size".into(), format!("{}x{}", c.sh.cw, c.sh.ch), "-i".into(), "pipe:0".into(),
            "-frames:v".into(), "1".into(), file_arg(&path.to_string_lossy()),
        ];
        let (ff, log) = (s.ffmpeg_path.clone(), self.log.clone());
        let h = thread::spawn(move || match run_tool(&ff, &args, Some(frame), 1024, Duration::from_secs(20), None) {
            Ok((_, _, Some(0))) => log.log(format!("screenshot saved: {}", path.display())),
            Ok((_, e, _)) => log.log(format!("screenshot failed: {}", truncate_chars(e.trim(), 200))),
            Err(e) => log.log(format!("screenshot failed: {e}")),
        });
        self.stoppers.push(h);
    }

    // --- periodic --------------------------------------------------------------
    fn tick(&mut self) {
        // Request preview size matching the widget.
        let s = &self.prof.settings;
        let (_, _, pw, ph) = fit_rect(s.canvas_w, s.canvas_h, self.ui.preview.w(), self.ui.preview.h());
        self.pv.req.store(((pw.max(0) as u64) << 32) | ph.max(0) as u64, Relaxed);

        // Output health.
        if self.rec.as_ref().map(|r| !r.alive()).unwrap_or(false) {
            let err = self.rec.as_ref().map(|r| lock(&r.last_err).clone()).unwrap_or_default();
            self.log.log(format!("[rec] encoder exited unexpectedly: {err}"));
            if self.rec.as_ref().map(|r| r.started.elapsed() < Duration::from_secs(5)).unwrap_or(false) && !self.prof.settings.mic_device.is_empty() {
                self.log.log("[rec] hint: check the audio devices in Settings (or set backend to 'none').");
            }
            self.stop_record();
            self.update_buttons();
        }
        if self.stream.as_ref().map(|r| !r.alive()).unwrap_or(false) {
            let ok_for = self.stream.as_ref().map(|r| r.started.elapsed()).unwrap_or_default();
            let err = self.stream.as_ref().map(|r| lock(&r.last_err).clone()).unwrap_or_default();
            self.log.log(format!("[stream] disconnected: {err}"));
            if let Some(r) = self.stream.take()
                && let Some(h) = r.begin_stop(self.log.clone()) {
                    self.stoppers.push(h);
                }
            let attempts = if ok_for > Duration::from_secs(60) { 0 } else { self.stream_retry.map(|r| r.1).unwrap_or(0) };
            if self.stream_user_on && attempts < STREAM_MAX_RETRIES {
                let wait = Duration::from_secs(2u64 << attempts.min(5));
                self.log.log(format!("[stream] reconnecting in {}s (attempt {}/{STREAM_MAX_RETRIES})", wait.as_secs(), attempts + 1));
                self.stream_retry = Some((Instant::now() + wait, attempts + 1));
            } else {
                if self.stream_user_on {
                    self.log.log("[stream] giving up after repeated failures");
                }
                self.stream_user_on = false;
                self.stream_retry = None;
            }
            self.update_buttons();
        }
        if let Some((at, n)) = self.stream_retry
            && Instant::now() >= at {
                match self.start_stream() {
                    Ok(()) => self.stream_retry = Some((Instant::now() + Duration::from_secs(3600 * 24 * 365), n)),
                    Err(e) => {
                        self.log.log(format!("[stream] reconnect failed: {e}"));
                        self.stream_retry = None;
                        self.stream_user_on = false;
                    }
                }
                self.update_buttons();
            }
        // While connected, the retry slot only carries the attempt counter.
        if self.stream.is_some()
            && let Some((_, n)) = self.stream_retry {
                if self.stream.as_ref().map(|s| s.started.elapsed() > Duration::from_secs(60)).unwrap_or(false) {
                    self.stream_retry = None;
                } else {
                    self.stream_retry = Some((Instant::now() + Duration::from_secs(3600 * 24 * 365), n));
                }
            }

        // Headless/scheduled recording (--record N).
        if let Some((start_at, dur)) = self.auto_record
            && Instant::now() >= start_at && self.rec.is_none() && self.auto_record_stop.is_none() {
                match self.start_record() {
                    Ok(()) => self.auto_record_stop = Some(Instant::now() + dur),
                    Err(e) => {
                        self.log.log(format!("auto-record failed: {e}"));
                        eprintln!("auto-record failed: {e}");
                        self.auto_record = None;
                        self.quit = true;
                    }
                }
                self.update_buttons();
            }
        if let Some(t) = self.auto_record_stop
            && (Instant::now() >= t || self.rec.is_none()) {
                self.stop_record();
                self.auto_record = None;
                self.auto_record_stop = None;
                self.quit = true;
            }

        // Reap finished background stoppers.
        let mut i = 0;
        while i < self.stoppers.len() {
            if self.stoppers[i].is_finished() {
                let _ = self.stoppers.swap_remove(i).join();
            } else {
                i += 1;
            }
        }

        // Log view.
        let (seq, lines) = self.log.since(self.log_seq);
        if !lines.is_empty() {
            self.log_seq = seq;
            for l in lines {
                self.ui.logbuf.append(&l);
                self.ui.logbuf.append("\n");
            }
            let len = self.ui.logbuf.length();
            if len > LOG_VIEW_MAX_BYTES {
                let cut = len - LOG_VIEW_MAX_BYTES / 2;
                let cut = self.ui.logbuf.line_end(cut) + 1;
                self.ui.logbuf.remove(0, cut);
            }
            let len = self.ui.logbuf.length();
            self.ui.logview.set_insert_position(len);
            self.ui.logview.show_insert_position();
        }

        // Status bar.
        let mut parts: Vec<String> = vec![];
        if let Some(r) = &self.rec {
            parts.push(format!("● REC {}", fmt_dur(r.started.elapsed())));
        }
        if let Some(r) = &self.stream {
            parts.push(format!("● LIVE {}", fmt_dur(r.started.elapsed())));
        } else if let Some((at, n)) = self.stream_retry {
            parts.push(format!("LIVE reconnecting in {}s ({n}/{STREAM_MAX_RETRIES})", at.saturating_duration_since(Instant::now()).as_secs()));
        }
        if let Some(c) = &self.comp {
            parts.push(format!(
                "{}x{} @ {:.1} fps",
                c.sh.cw,
                c.sh.ch,
                c.sh.fps_milli.load(Relaxed) as f64 / 1000.0
            ));
            parts.push(format!("compose {:.1} ms", c.sh.compose_us.load(Relaxed) as f64 / 1000.0));
            parts.push(format!("lagged {}", c.sh.late.load(Relaxed)));
        }
        parts.push(format!("scene: {}", self.scene().name));
        if !self.ff.ok {
            parts.push("ffmpeg NOT FOUND — see Settings".into());
        }
        self.ui.status.set_label(&label_escape(&format!("  {}", parts.join("   |   "))));

        // Selected source status.
        let st = self.sel_index().map(|i| {
            let s = &self.scene().sources[i];
            let rt = self.runtimes.get(&source_sig(s, self.prof.settings.fps, &self.prof.settings.ffmpeg_path));
            match rt {
                Some(rt) => format!("{}: {}", s.name, lock(&rt.status)),
                None => format!("{}: hidden", s.name),
            }
        });
        self.ui.src_status.set_label(&label_escape(&truncate_chars(&st.unwrap_or_default(), 160)));

        if self.dirty && self.last_save.elapsed() > Duration::from_secs(15) {
            self.save();
        }
    }

    // --- dispatcher ----------------------------------------------------------
    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Tick => self.tick(),
            Msg::Quit => {
                if self.outputs_active() {
                    let c = dialog::choice2_default("Recording or streaming is active.\nStop and quit?", "Cancel", "Stop && Quit", "");
                    if c != Some(1) {
                        return;
                    }
                }
                self.quit = true;
            }
            Msg::SceneSelect => {
                let v = self.ui.scenes.value();
                if v >= 1 && (v as usize) <= self.prof.scenes.len() && v as usize - 1 != self.prof.current {
                    self.prof.current = v as usize - 1;
                    self.sel = None;
                    self.apply_scene();
                    self.refresh_sources();
                    self.mark_dirty();
                }
                self.refresh_scenes();
            }
            Msg::SceneAdd => {
                if self.prof.scenes.len() >= MAX_SCENES {
                    return;
                }
                let def = format!("Scene {}", self.prof.scenes.len() + 1);
                if let Some(n) = dialog::input_default("Scene name:", &def) {
                    let n = truncate_chars(n.trim(), 64);
                    self.prof.scenes.push(Scene { name: if n.is_empty() { def } else { n }, sources: vec![] });
                    self.prof.current = self.prof.scenes.len() - 1;
                    self.sel = None;
                    self.apply_scene();
                    self.refresh_scenes();
                    self.refresh_sources();
                    self.mark_dirty();
                }
            }
            Msg::SceneRemove => {
                if self.prof.scenes.len() <= 1 {
                    dialog::alert_default("At least one scene is required.");
                    return;
                }
                let nm = self.scene().name.clone();
                if dialog::choice2_default(&format!("Remove scene '{nm}'?"), "Cancel", "Remove", "") == Some(1) {
                    let c = self.prof.current;
                    self.prof.scenes.remove(c);
                    self.prof.current = c.min(self.prof.scenes.len() - 1);
                    self.sel = None;
                    self.apply_scene();
                    self.refresh_scenes();
                    self.refresh_sources();
                    self.mark_dirty();
                }
            }
            Msg::SceneRename => {
                let cur = self.scene().name.clone();
                if let Some(n) = dialog::input_default("Scene name:", &cur) {
                    let n = truncate_chars(n.trim(), 64);
                    if !n.is_empty() {
                        self.scene_mut().name = n;
                        self.refresh_scenes();
                        self.mark_dirty();
                    }
                }
            }
            Msg::SceneUp | Msg::SceneDown => {
                let c = self.prof.current;
                let to = if m == Msg::SceneUp { c.checked_sub(1) } else { Some(c + 1).filter(|&t| t < self.prof.scenes.len()) };
                if let Some(t) = to {
                    self.prof.scenes.swap(c, t);
                    self.prof.current = t;
                    self.refresh_scenes();
                    self.mark_dirty();
                }
            }
            Msg::SourceSelect => {
                let line = self.ui.sources.value();
                self.sel = self.browser_to_index(line).map(|i| self.scene().sources[i].id);
                self.load_props();
            }
            Msg::SourceAdd => {
                if self.scene().sources.len() >= MAX_SOURCES {
                    dialog::alert_default("Source limit reached for this scene.");
                    return;
                }
                let st = self.prof.settings.clone();
                if let Some((name, kind)) = source_dialog(None, &st) {
                    let (cw, ch) = (st.canvas_w, st.canvas_h);
                    let (x, y, w, h) = match &kind {
                        SourceKind::Display { w, h, .. } => fit_rect(*w, *h, cw as i32, ch as i32),
                        SourceKind::Webcam { size, .. } => {
                            let (sw, sh) = parse_size(size).unwrap_or((16, 9));
                            let tw = (cw as i32 / 4).max(2);
                            let th = ((tw as f64 * sh as f64 / sw as f64).round() as i32).max(2);
                            (cw as i32 - tw - 20, ch as i32 - th - 20, tw, th)
                        }
                        _ => (0, 0, cw as i32, ch as i32),
                    };
                    let src = Source {
                        id: self.next_id(),
                        name,
                        kind,
                        visible: true,
                        x,
                        y,
                        w: (w.max(2) as u32).min(MAX_W),
                        h: (h.max(2) as u32).min(MAX_H),
                        opacity: 255,
                    };
                    self.sel = Some(src.id);
                    self.scene_mut().sources.push(src);
                    self.apply_scene();
                    self.refresh_sources();
                    self.mark_dirty();
                }
            }
            Msg::SourceEdit => {
                let Some(i) = self.sel_index() else { return };
                let cur = self.scene().sources[i].clone();
                let st = self.prof.settings.clone();
                if let Some((name, kind)) = source_dialog(Some(&cur), &st) {
                    if let Some(s) = self.scene_mut().sources.iter_mut().find(|s| s.id == cur.id) {
                        s.name = name;
                        s.kind = kind;
                    }
                    self.apply_scene();
                    self.refresh_sources();
                    self.mark_dirty();
                }
            }
            Msg::SourceRemove => {
                let Some(i) = self.sel_index() else { return };
                let nm = self.scene().sources[i].name.clone();
                if dialog::choice2_default(&format!("Remove source '{nm}'?"), "Cancel", "Remove", "") == Some(1) {
                    if let Some(i) = self.sel_index() {
                        self.scene_mut().sources.remove(i);
                    }
                    self.sel = None;
                    self.apply_scene();
                    self.refresh_sources();
                    self.mark_dirty();
                }
            }
            Msg::SourceToggle => {
                let Some(i) = self.sel_index() else { return };
                let v = &mut self.scene_mut().sources[i].visible;
                *v = !*v;
                self.apply_scene();
                self.refresh_sources();
                self.mark_dirty();
            }
            Msg::SourceUp | Msg::SourceDown => {
                let Some(i) = self.sel_index() else { return };
                let n = self.scene().sources.len();
                // "Up" in the list = higher z = larger index.
                let to = if m == Msg::SourceUp { Some(i + 1).filter(|&t| t < n) } else { i.checked_sub(1) };
                if let Some(t) = to {
                    self.scene_mut().sources.swap(i, t);
                    self.publish_layers();
                    self.refresh_sources();
                    self.mark_dirty();
                }
            }
            Msg::PropsApply => {
                let Some(i) = self.sel_index() else { return };
                let p = |w: &IntInput| w.value().trim().parse::<i64>().ok();
                let (x, y, w, h) = (p(&self.ui.px), p(&self.ui.py), p(&self.ui.pw), p(&self.ui.ph));
                let s = &mut self.scene_mut().sources[i];
                let old = (s.w, s.h);
                if let Some(x) = x { s.x = x.clamp(-(MAX_W as i64), MAX_W as i64) as i32; }
                if let Some(y) = y { s.y = y.clamp(-(MAX_H as i64), MAX_H as i64) as i32; }
                if let Some(w) = w { s.w = w.clamp(MIN_DIM as i64, MAX_W as i64) as u32; }
                if let Some(h) = h { s.h = h.clamp(MIN_DIM as i64, MAX_H as i64) as u32; }
                let resized = old != (s.w, s.h);
                if resized { self.apply_scene(); } else { self.publish_layers(); }
                self.load_props();
                self.mark_dirty();
            }
            Msg::PropsOpacity => {
                let Some(i) = self.sel_index() else { return };
                let o = (self.ui.opac.value().clamp(0.0, 100.0) * 255.0 / 100.0).round() as u8;
                self.scene_mut().sources[i].opacity = o;
                self.publish_layers();
                self.mark_dirty();
            }
            Msg::PropsFill | Msg::PropsCenter | Msg::PropsNative => {
                let Some(i) = self.sel_index() else { return };
                let (cw, ch) = (self.prof.settings.canvas_w, self.prof.settings.canvas_h);
                let s = &mut self.scene_mut().sources[i];
                let old = (s.w, s.h);
                match m {
                    Msg::PropsFill => {
                        let (aw, ah) = match &s.kind {
                            SourceKind::Display { w, h, .. } => (*w, *h),
                            _ => (s.w, s.h),
                        };
                        let (x, y, w, h) = fit_rect(aw, ah, cw as i32, ch as i32);
                        (s.x, s.y, s.w, s.h) = (x, y, w.max(2) as u32, h.max(2) as u32);
                    }
                    Msg::PropsCenter => {
                        s.x = (cw as i32 - s.w as i32) / 2;
                        s.y = (ch as i32 - s.h as i32) / 2;
                    }
                    _ => {
                        if let SourceKind::Display { w, h, .. } = &s.kind {
                            (s.w, s.h) = ((*w).clamp(MIN_DIM, MAX_W), (*h).clamp(MIN_DIM, MAX_H));
                        } else if let SourceKind::Webcam { size, .. } = &s.kind
                            && let Some((w, h)) = parse_size(size) {
                                (s.w, s.h) = (w.max(MIN_DIM), h.max(MIN_DIM));
                            }
                    }
                }
                let resized = old != (s.w, s.h);
                if resized { self.apply_scene(); } else { self.publish_layers(); }
                self.load_props();
                self.mark_dirty();
            }
            Msg::ToggleRecord => {
                if self.rec.is_some() {
                    self.stop_record();
                } else if let Err(e) = self.start_record() {
                    self.log.log(format!("[rec] {e}"));
                    dialog::alert_default(&format!("Could not start recording:\n{e}"));
                }
                self.update_buttons();
            }
            Msg::ToggleStream => {
                if self.stream.is_some() || self.stream_retry.is_some() {
                    self.stop_stream();
                } else {
                    match self.start_stream() {
                        Ok(()) => self.stream_user_on = true,
                        Err(e) => {
                            self.log.log(format!("[stream] {e}"));
                            dialog::alert_default(&format!("Could not start streaming:\n{e}"));
                        }
                    }
                }
                self.update_buttons();
            }
            Msg::Screenshot => self.screenshot(),
            Msg::Settings => {
                let cur = self.prof.settings.clone();
                let Some(ns) = settings_dialog(&cur, &self.ff) else { return };
                let needs_restart = ns.canvas_w != cur.canvas_w || ns.canvas_h != cur.canvas_h || ns.fps != cur.fps || ns.ffmpeg_path != cur.ffmpeg_path;
                if needs_restart && self.outputs_active() {
                    dialog::alert_default("Stop recording/streaming before changing canvas size, FPS or the ffmpeg path.");
                    return;
                }
                self.prof.settings = ns;
                self.log.set_secrets(secrets_of(&self.prof.settings));
                if cur.ffmpeg_path != self.prof.settings.ffmpeg_path {
                    self.ff = detect_ffmpeg(&self.prof.settings.ffmpeg_path);
                    self.log_ff();
                }
                if needs_restart {
                    self.stop_pipeline();
                    self.start_pipeline();
                }
                if self.ff.ok && !self.ff.encoders.is_empty() && !self.ff.encoders.contains(&self.prof.settings.encoder) {
                    self.log.log(format!("warning: encoder {} not available in this ffmpeg build", self.prof.settings.encoder));
                }
                self.save();
            }
            Msg::SaveProfile => {
                self.save();
                self.log.log(format!("profile saved: {}", self.profile_path.display()));
            }
            Msg::SaveProfileAs => {
                let mut fc = dialog::NativeFileChooser::new(dialog::NativeFileChooserType::BrowseSaveFile);
                fc.set_filter("LiteCast profile\t*.json");
                fc.set_option(dialog::NativeFileChooserOptions::SaveAsConfirm);
                fc.show();
                let mut f = fc.filename();
                if f.as_os_str().is_empty() {
                    return;
                }
                if f.extension().is_none() {
                    f.set_extension("json");
                }
                match save_profile(&f, &self.prof) {
                    Ok(()) => {
                        self.profile_path = f;
                        self.dirty = false;
                        self.log.log(format!("profile saved: {}", self.profile_path.display()));
                    }
                    Err(e) => dialog::alert_default(&format!("Save failed: {e}")),
                }
            }
            Msg::LoadProfile => {
                if self.outputs_active() {
                    dialog::alert_default("Stop recording/streaming before loading a profile.");
                    return;
                }
                let mut fc = dialog::NativeFileChooser::new(dialog::NativeFileChooserType::BrowseFile);
                fc.set_filter("LiteCast profile\t*.json");
                fc.show();
                let f = fc.filename();
                if f.as_os_str().is_empty() {
                    return;
                }
                match load_profile(&f) {
                    Ok(p) => {
                        if self.dirty {
                            self.save();
                        }
                        self.stop_pipeline();
                        let ff_changed = p.settings.ffmpeg_path != self.prof.settings.ffmpeg_path;
                        self.prof = p;
                        self.profile_path = f;
                        self.sel = None;
                        self.log.set_secrets(secrets_of(&self.prof.settings));
                        if ff_changed {
                            self.ff = detect_ffmpeg(&self.prof.settings.ffmpeg_path);
                            self.log_ff();
                        }
                        self.start_pipeline();
                        self.refresh_scenes();
                        self.refresh_sources();
                        self.log.log(format!("profile loaded: {}", self.profile_path.display()));
                    }
                    Err(e) => dialog::alert_default(&format!("Load failed: {e}")),
                }
            }
            Msg::About => dialog::message_default(&format!(
                "{APP_NAME} v{APP_VERSION}\nLightweight OBS-style recorder & streamer\nRust + FLTK, encoding via ffmpeg.\n\n{}\nProfile: {}",
                if self.ff.ok { self.ff.version.as_str() } else { "ffmpeg not found" },
                self.profile_path.display()
            )),
            Msg::PreviewPush(mx, my, shift) => {
                let Some((cx, cy)) = self.canvas_xy(mx, my) else { return };
                let hit = self.scene().sources.iter().rev().find(|s| {
                    s.visible && cx >= s.x && cy >= s.y && (cx as i64) < s.x as i64 + s.w as i64 && (cy as i64) < s.y as i64 + s.h as i64
                }).map(|s| (s.id, s.x, s.y, s.w, s.h));
                match hit {
                    Some((id, x, y, w, h)) => {
                        self.sel = Some(id);
                        // Grabbing the bottom-right handle also resizes.
                        let near_corner = (cx - (x + w as i32)).abs() < 24 && (cy - (y + h as i32)).abs() < 24;
                        self.drag = Some(DragState { id, start: (cx, cy), orig: (x, y, w, h), resize: shift || near_corner, moved: false });
                    }
                    None => {
                        self.sel = None;
                        self.drag = None;
                    }
                }
                self.refresh_sources();
            }
            Msg::PreviewDrag(mx, my) => {
                let Some((cx, cy)) = self.canvas_xy(mx, my) else { return };
                let Some(d) = &mut self.drag else { return };
                let (dx, dy) = (cx - d.start.0, cy - d.start.1);
                d.moved = true;
                let (id, orig, resize) = (d.id, d.orig, d.resize);
                let Some(s) = self.scene_mut().sources.iter_mut().find(|s| s.id == id) else { return };
                if resize {
                    s.w = (orig.2 as i64 + dx as i64).clamp(MIN_DIM as i64, MAX_W as i64) as u32;
                    s.h = (orig.3 as i64 + dy as i64).clamp(MIN_DIM as i64, MAX_H as i64) as u32;
                    // Outline only while dragging; capture restarts on release.
                    let (x, y, w, h) = (s.x, s.y, s.w as i32, s.h as i32);
                    lock(&self.ov).sel = Some((x, y, w, h));
                    self.ui.preview.redraw();
                } else {
                    s.x = (orig.0 + dx).clamp(-(MAX_W as i32), MAX_W as i32);
                    s.y = (orig.1 + dy).clamp(-(MAX_H as i32), MAX_H as i32);
                    // Move keeps the source signature: no capture restart.
                    self.publish_layers();
                }
            }
            Msg::PreviewRelease => {
                if let Some(d) = self.drag.take()
                    && d.moved {
                        if d.resize {
                            // Commit the new size: apply_scene restarts only this source.
                            self.apply_scene();
                        } else {
                            self.publish_layers();
                        }
                        self.load_props();
                        self.mark_dirty();
                    }
            }
        }
    }

    fn log_ff(&self) {
        if self.ff.ok {
            self.log.log(format!("{} (fps_mode: {})", self.ff.version, self.ff.has_fps_mode()));
        } else {
            self.log.log(format!("ffmpeg not found at '{}' — set it in Settings", self.prof.settings.ffmpeg_path));
        }
    }

    fn shutdown(&mut self) {
        self.ui.win.set_label(&format!("{APP_NAME} — finalising…"));
        app::flush();
        self.stop_record();
        self.stop_stream();
        for h in self.stoppers.drain(..) {
            let _ = h.join();
        }
        self.stop_pipeline();
        self.save();
    }
}

// ----------------------------------------------------------------------------
// main
// ----------------------------------------------------------------------------
fn usage() -> String {
    format!(
        "{APP_NAME} v{APP_VERSION}\n\nUSAGE: litecast [--profile FILE] [--record SECONDS]\n\n  \
         --profile FILE     use this profile JSON (default: {})\n  \
         --record SECONDS   start recording on launch, stop after SECONDS, then exit\n  \
         --version          print version\n  --help             this help\n",
        config_dir().join("profile.json").display()
    )
}

fn main() {
    let mut profile_path = config_dir().join("profile.json");
    let mut auto_rec: Option<Duration> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--version" | "-V" => {
                println!("litecast {APP_VERSION}");
                return;
            }
            "--help" | "-h" => {
                print!("{}", usage());
                return;
            }
            "--profile" => match args.next() {
                Some(p) => profile_path = PathBuf::from(p),
                None => {
                    eprint!("--profile needs a path\n\n{}", usage());
                    std::process::exit(2);
                }
            },
            "--record" => match args.next().and_then(|v| v.parse::<u64>().ok()).filter(|v| (1..=86_400 * 7).contains(v)) {
                Some(s) => auto_rec = Some(Duration::from_secs(s)),
                None => {
                    eprint!("--record needs seconds (1..604800)\n\n{}", usage());
                    std::process::exit(2);
                }
            },
            _ => {
                eprint!("unknown argument: {a}\n\n{}", usage());
                std::process::exit(2);
            }
        }
    }

    let fl = app::App::default().with_scheme(app::Scheme::Gtk);
    let log = Logger::default();
    log.log(format!("{APP_NAME} v{APP_VERSION} starting"));

    let prof = if profile_path.exists() {
        match load_profile(&profile_path) {
            Ok(p) => p,
            Err(e) => {
                log.log(format!("profile load failed ({e}); using defaults"));
                let bak = profile_path.with_extension("json.bad");
                let _ = fs::rename(&profile_path, &bak);
                log.log(format!("bad profile moved to {}", bak.display()));
                Profile::default()
            }
        }
    } else {
        Profile::default()
    };
    log.set_secrets(secrets_of(&prof.settings));

    let q = Q(Rc::new(RefCell::new(VecDeque::new())));
    let pv = Arc::new(PreviewShared { buf: Mutex::new((vec![], 0, 0)), seq: AtomicU64::new(0), req: AtomicU64::new(0) });
    let ov = Arc::new(Mutex::new(Overlay::default()));
    let mut ui = build_ui(&q, pv.clone(), ov.clone());
    ui.win.show();

    let ff = detect_ffmpeg(&prof.settings.ffmpeg_path);
    let mut st = AppState {
        prof,
        profile_path,
        ui,
        log: log.clone(),
        log_seq: 0,
        ff,
        pv: pv.clone(),
        ov,
        comp: None,
        runtimes: HashMap::new(),
        rec: None,
        stream: None,
        stream_user_on: false,
        stream_retry: None,
        stoppers: vec![],
        sel: None,
        drag: None,
        dirty: false,
        last_save: Instant::now(),
        quit: false,
        auto_record: auto_rec.map(|d| (Instant::now() + Duration::from_millis(1500), d)),
        auto_record_stop: None,
        rec_path: None,
    };
    st.log_ff();
    if !st.ff.ok {
        dialog::alert_default(&format!(
            "ffmpeg was not found ('{}').\nInstall ffmpeg or set its path in Controls → Settings.",
            st.prof.settings.ffmpeg_path
        ));
    }
    st.start_pipeline();
    st.refresh_scenes();
    st.refresh_sources();
    st.update_buttons();

    // Preview repaint at ~30 Hz, only when a new frame arrived.
    {
        let mut preview = st.ui.preview.clone();
        let pv = pv.clone();
        let mut last = 0u64;
        app::add_timeout3(1.0 / 30.0, move |h| {
            let s = pv.seq.load(Relaxed);
            if s != last {
                last = s;
                preview.redraw();
            }
            app::repeat_timeout3(1.0 / 30.0, h);
        });
    }
    {
        let q = q.clone();
        app::add_timeout3(0.25, move |h| {
            q.send(Msg::Tick);
            app::repeat_timeout3(0.25, h);
        });
    }

    while fl.wait() {
        while let Some(m) = q.pop() {
            st.handle(m);
        }
        if st.quit {
            break;
        }
    }
    st.shutdown();
}

// ----------------------------------------------------------------------------
// Tests (pure logic only; no display needed)
// ----------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    fn layer(x: i32, y: i32, w: u32, h: u32, op: u8) -> Layer {
        Layer { x, y, w, h, opacity: op, slot: Arc::new(Mutex::new(None)), opaque: Arc::new(AtomicBool::new(true)) }
    }

    #[test]
    fn blit_clips_all_edges() {
        let (cw, ch) = (4usize, 4usize);
        let mut dst = [0u8, 0, 0, 255].repeat(cw * ch);
        let src = [10u8, 20, 30, 255].repeat(9);
        for (x, y) in [(-2, -2), (3, 3), (-5, 0), (10, 10), (2, -1)] {
            blit(&mut dst, cw, ch, &src, &layer(x, y, 3, 3, 255), true);
        }
        assert_eq!(&dst[0..4], &[10, 20, 30, 255]); // (-2,-2) covers (0,0)
        assert_eq!(&dst[(3 * cw + 3) * 4..(3 * cw + 3) * 4 + 4], &[10, 20, 30, 255]);
    }

    #[test]
    fn blit_half_opacity() {
        let mut dst = [0u8, 0, 0, 255].repeat(1);
        let src = [200u8, 100, 50, 255].repeat(1);
        blit(&mut dst, 1, 1, &src, &layer(0, 0, 1, 1, 128), true);
        assert_eq!(dst, vec![100, 50, 25, 255]);
    }

    #[test]
    fn div255_exact() {
        for x in 0..=255u32 * 255 {
            assert_eq!(div255(x), (x + 127) / 255, "x={x}");
        }
    }

    #[test]
    fn blit_rejects_short_buffer() {
        let mut dst = vec![0u8; 16];
        blit(&mut dst, 2, 2, &[1, 2, 3], &layer(0, 0, 2, 2, 255), true);
        assert_eq!(dst, vec![0u8; 16]);
    }

    #[test]
    fn sanitize_clamps_hostile_profile() {
        let mut p = Profile::default();
        p.settings.canvas_w = 999_999;
        p.settings.canvas_h = 1;
        p.settings.fps = 0;
        p.settings.encoder = "evil; rm -rf /".into();
        p.scenes[0].sources[0].w = u32::MAX;
        let dup = p.scenes[0].sources[0].clone();
        p.scenes[0].sources.push(dup); // duplicate id
        p.current = 99;
        sanitize(&mut p);
        assert_eq!(p.settings.canvas_w, MAX_W);
        assert_eq!(p.settings.canvas_h, 120);
        assert_eq!(p.settings.fps, 1);
        assert_eq!(p.settings.encoder, "libx264");
        assert_eq!(p.scenes[0].sources[0].w, MAX_W);
        assert_ne!(p.scenes[0].sources[0].id, p.scenes[0].sources[1].id);
        assert_eq!(p.current, 0);
    }

    #[test]
    fn stream_target_and_redaction() {
        let mut s = Settings::default();
        s.stream_url = "rtmp://example.com/app/".into();
        s.stream_key = "live_abc123".into();
        assert_eq!(stream_target(&s).unwrap(), "rtmp://example.com/app/live_abc123");
        s.stream_url = "http://x".into();
        assert!(stream_target(&s).is_err());
        s.stream_url = "rtmp://u:p@h/app".into();
        let sec = secrets_of(&s);
        assert!(sec.contains(&"u:p".to_string()) && sec.contains(&"live_abc123".to_string()));
        let l = Logger::default();
        l.set_secrets(sec);
        l.log("error opening rtmp://h/app/live_abc123");
        assert!(!l.since(0).1[0].contains("live_abc123"));
    }

    #[test]
    fn encoder_args_shape() {
        let mut s = Settings::default();
        s.audio_api = "pulse".into();
        let ff = FfInfo { ok: true, major: 6, minor: 1, ..Default::default() };
        let a = encoder_args(&s, &ff, OutKind::Record, "/tmp/x.mkv");
        let j = a.join(" ");
        assert!(j.contains("-f rawvideo -pix_fmt bgra"));
        assert!(j.contains("amix=inputs=2"));
        assert!(j.contains("-map [aout]"));
        assert!(j.contains("-fps_mode cfr"));
        assert!(a.last().unwrap() == "file:/tmp/x.mkv");
        s.audio_api = "none".into();
        let a = encoder_args(&s, &FfInfo { major: 4, ..ff }, OutKind::Stream, "rtmp://h/a/k");
        let j = a.join(" ");
        assert!(!j.contains("-c:a") && j.contains("-vsync cfr") && j.contains("-f flv"));
    }

    #[test]
    fn misc_helpers() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_412), (2025, 11, 20));
        assert_eq!(parse_size("1280x720"), Some((1280, 720)));
        assert_eq!(parse_size("0x720"), None);
        assert_eq!(parse_size("99999x1"), None);
        assert_eq!(fit_rect(1920, 1080, 960, 1000), (0, 230, 960, 540));
        assert_eq!(menu_escape("/dev/video0"), "\\/dev\\/video0");
        assert_eq!(menu_escape("@DEFAULT_MONITOR@"), "@@DEFAULT_MONITOR@@");
        assert_eq!(label_escape("30 @ 60"), "30 @@ 60");
        assert_eq!(file_arg("concat:a|b"), "file:concat:a|b");
        let mut v = vec![0u8; 8];
        assert!(read_full(&mut &[1u8, 2, 3, 4, 5, 6, 7, 8][..], &mut v).unwrap());
        assert!(!read_full(&mut &[][..], &mut v).unwrap());
        assert!(read_full(&mut &[1u8, 2][..], &mut v).is_err());
    }
}
