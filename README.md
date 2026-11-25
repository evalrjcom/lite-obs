# LiteCast Studio v1.0.0 — User & Developer Guide

`litecast_v1.0.0` is a lightweight, OBS-style screen recorder and live streamer written in Rust with an FLTK interface. It ships as one source file (`src/main.rs`) with three crate dependencies (`fltk`, `serde`, `serde_json`). It drives **ffmpeg** as an external worker for capture and encoding.

---

## 1. Features

| Area | What you get |
|---|---|
| Scenes | Unlimited* scenes; add / remove / rename / reorder; instant cut switching |
| Sources | Display capture (region or full screen, multi-monitor), webcam / video device, media file (loop), image (alpha preserved), solid colour |
| Layout | Z-order up/down, show/hide, X/Y/W/H, opacity 0–100 %, Fill / Center / 1:1 buttons |
| Preview | Live preview. Click to select, drag to move, Shift-drag (or grab the red corner handle) to resize |
| Recording | mkv (default, crash-safe), mp4 (faststart), mov, flv, ts. Never overwrites existing files |
| Streaming | RTMP / RTMPS (Twitch, YouTube, Kick, etc.) and SRT. Auto-reconnect with exponential backoff (6 attempts) |
| Audio | Mic + desktop audio, mixed. Backends: PulseAudio/PipeWire, ALSA, DirectShow, AVFoundation |
| Encoders | libx264, NVENC, QuickSync, AMF, VAAPI, VideoToolbox (whichever your ffmpeg provides) |
| Extras | PNG screenshot (F12), JSON profiles, headless `--record N` mode for scheduled or cron recording |

\*Hard caps of 64 scenes and 64 sources per scene protect against corrupt profiles.

---

## 2. Requirements

**Runtime**
- ffmpeg ≥ 4.4 on `PATH`, or set its path in Settings. Version 5.1 or later is recommended (uses `-fps_mode`; older builds fall back to `-vsync`).
- Linux: an X11 session. Wayland sessions work only through XWayland, because ffmpeg's `x11grab` cannot see native Wayland windows.
- Windows 10/11: ffmpeg built with gdigrab/dshow (gyan.dev or BtbN builds work).
- macOS: ffmpeg with AVFoundation, plus Screen Recording permission granted to the terminal or app.

**Build**
- Rust 1.87+ (stable, edition 2024), CMake ≥ 3.15, a C++17 compiler.
- Debian/Ubuntu packages:
  ```bash
  sudo apt install build-essential cmake ninja-build pkg-config \
    libx11-dev libxext-dev libxft-dev libxinerama-dev libxcursor-dev \
    libxrender-dev libxfixes-dev libpango1.0-dev libcairo2-dev \
    libgl1-mesa-dev libglu1-mesa-dev ffmpeg
  ```

---

## 3. Build & run

```bash
git clone https://github.com/evalrjcom/lite-obs && cd lite-obs
cargo build --release
./target/release/litecast
```

**Windows (native, MSVC)**: install Rust, CMake and VS Build Tools, then run `cargo build --release`. The release binary uses the GUI subsystem, so no console window appears.
**Windows (cross from Linux)**:
```bash
sudo apt install mingw-w64
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

### Command line
```
litecast [--profile FILE] [--record SECONDS] [--version] [--help]
```
- `--profile FILE`: use a specific profile. Default: `~/.config/litecast/profile.json` on Linux and macOS, `%APPDATA%\litecast\profile.json` on Windows.
- `--record N`: start recording about 1.5 s after launch, stop after N seconds, finalise the file and exit. Useful with cron or Task Scheduler, e.g. `DISPLAY=:0 litecast --record 3600`.

---

## 4. Using it

1. **First launch** creates *Scene 1* with a full-screen Display Capture of the primary monitor, fitted to a 1920×1080 canvas.
2. **Add sources** with the **+** under *Sources*. Choose a type and fill in:
   - *Display*: the target is the X display (`:0.0`) on Linux, `desktop` on Windows (or `title=Window Name` to capture one window), or the screen index on macOS. Pick a monitor to auto-fill the region.
   - *Webcam*: press **Detect** to list devices. Size (e.g. `1280x720`) and FPS are optional.
   - *Media / Image*: use **Browse…**.
   - *Color*: pick a colour; use opacity for tints.
3. **Arrange** layers in the preview (drag to move, Shift-drag to resize) or type X/Y/W/H and press **Apply**. **Raise/Lower** change z-order. The list shows the top layer first.
4. **Settings** (Ctrl+,): canvas size, FPS, encoder, bitrate, recording folder and format, stream URL and key, audio devices, ffmpeg path.
5. **Record** (Ctrl+R) and **Stream** (Ctrl+L) run independently and can run at the same time.
6. **Screenshot** (F12) saves a PNG of the composed canvas to the recording folder.
7. Profiles auto-save every 15 s when changed, and on exit. Ctrl+S saves now; File → Save As / Load handle other files.

### Keyboard shortcuts
| Key | Action |
|---|---|
| Ctrl+R | Toggle recording |
| Ctrl+L | Toggle streaming |
| F12 | Screenshot |
| Ctrl+, | Settings |
| Ctrl+S / Ctrl+Shift+S / Ctrl+O | Save / Save As / Load profile |
| Ctrl+Q | Quit (asks first while live; always finalises recordings) |

### Streaming quick-setup
| Service | URL | Key |
|---|---|---|
| Twitch | `rtmp://live.twitch.tv/app` | your stream key |
| YouTube | `rtmp://a.rtmp.youtube.com/live2` | your stream key |
| Custom SRT | `srt://host:port?streamid=...` | leave blank |

Recommended settings for 1080p30 streaming: libx264 *veryfast*, 4500–6000 kbps, keyframe interval 2 s.

### Audio notes
- **Linux (PulseAudio/PipeWire)**: Mic `default`, Desktop `@DEFAULT_MONITOR@`. **Detect devices** lists everything `pactl` knows about.
- **Windows**: desktop audio needs a loopback device such as *Stereo Mix* or a virtual cable. Leave a field blank to disable it.
- Mixing two inputs uses ffmpeg's `amix`, which lowers each input's level. Adjust gain in your OS mixer if needed.
- If a recording stops within seconds of starting, the cause is almost always a wrong audio device. Check the log, or set the backend to `none`.

---

## 5. Troubleshooting

| Symptom | Fix |
|---|---|
| Black display source on Linux | You are on pure Wayland. Log in to an X11 session, or capture an XWayland app |
| "ffmpeg NOT FOUND" in the status bar | Install ffmpeg or set the full path in Settings |
| Source shows `error:` under Transform | Read the Log panel. The source retries automatically (1 s → 10 s backoff) |
| `lagged` counter rising | Lower canvas/FPS, use a hardware encoder, or use a faster x264 preset |
| mp4 won't play after a crash | Use mkv (the default). It stays playable; remux later with `ffmpeg -i in.mkv -c copy out.mp4` |
| Stream keeps reconnecting | Check the URL and key; the log shows the redacted ffmpeg error |

---

## 6. Developer notes

### Architecture
```
capture ffmpeg ─BGRA─▶ reader thread ─▶ FrameSlot ┐
color / image ───────────────────────▶ FrameSlot ┤
                                                  ▼
                compositor thread (fixed FPS, z-order, alpha, opacity)
                  │ canvas Arc (pooled)        │ RGB preview (downscaled)
          writer thread  writer thread          FLTK preview widget
               │              │
        ffmpeg encoder  ffmpeg encoder  (+ ffmpeg-native audio inputs)
         (record file)   (RTMP/SRT)
```
- **UI thread** owns all state (`AppState`). Widget callbacks only push `Msg` values into an `Rc<RefCell<VecDeque>>` queue, which is drained after every `app.wait()`. No `RefCell` borrow is held across modal dialogs, and no FLTK awake ring buffer is involved.
- **Sources** are keyed by a *signature* (kind + size + FPS + ffmpeg path). Scenes that share an identical source reuse one capture process. Moves and opacity changes never restart a capture; resizes do.
- **Timing**: all loops pace against an absolute schedule (drift-free) and resync after more than 3 periods of lag. Encoders use `-use_wallclock_as_timestamps 1` with CFR output, which keeps A/V in sync even when a frame write stalls.
- **Zero-copy-ish**: frames move as `Arc<Vec<u8>>`. Reader and canvas buffers are recycled through `Arc::try_unwrap` and a small pool, so there is no 8 MB allocation per frame.

### Tests
```bash
DISPLAY=:0 cargo test --release   # 8 unit tests: blending, clipping, sanitising, args, redaction, helpers
```
(One test builds a default profile, which queries monitor geometry, so it needs an X display; Xvfb works.)

### Security model
- Every child process is spawned with an argv list; no shell is ever invoked.
- Local paths are passed as `file:<path>`, so a filename can't be read as an ffmpeg protocol (`concat:`, `subfile,`, etc.).
- Profiles are size-capped (4 MiB), clamped and repaired by `sanitize()`. Encoder, format and backend names are whitelisted. A broken profile is moved aside to `profile.json.bad`.
- The profile is written atomically with mode **0600** in a 0700 directory, because it contains the stream key.
- Stream keys, URL credentials and SRT passphrases are **redacted** from the log, the UI and status text.
- **Known limitation**: the full stream URL, including the key, appears in the ffmpeg command line, which other local users can read via `ps` or `/proc`. ffmpeg offers no way to read the output URL from stdin. Use a single-user machine for streaming.

### Limits / not yet implemented
Scene transitions (cut only), text sources, filters (chroma key, crop, colour correction), per-source audio and media audio, studio mode, recording splitting, native Wayland/PipeWire capture.

---

## 7. Changelog
- **1.0.0**: initial release.
