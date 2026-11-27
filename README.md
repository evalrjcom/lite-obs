# LiteCast Studio v1.2.0 — User & Developer Guide

[![CI](https://github.com/evalrjcom/litecast/actions/workflows/ci.yml/badge.svg)](https://github.com/evalrjcom/litecast/actions/workflows/ci.yml)

`litecast_v1.2.0` is a lightweight, OBS-style screen recorder and live streamer written in Rust with an FLTK interface. It ships as one source file (`src/main.rs`) with three crate dependencies (`fltk`, `serde`, `serde_json`), plus `wasapi` and `windows-sys` on Windows only. It drives **ffmpeg** as an external worker for capture and encoding.

---

## 1. Features

| Area | What you get |
|---|---|
| Scenes | Unlimited* scenes; add / remove / rename / reorder; instant cut switching |
| Sources | Display capture (region or full screen, multi-monitor), webcam / video device, media file (loop), image (alpha preserved), solid colour |
| Layout | Z-order up/down, show/hide, X/Y/W/H, opacity 0–100 %, Fill / Center / 1:1 buttons |
| Preview | Live preview. Click to select, drag to move, Shift-drag (or grab the red corner handle) to resize |
| Recording | mkv (default, crash-safe), mp4 (faststart), mov, flv, ts. Never overwrites existing files |
| Streaming | **Kick, Rumble**, Twitch, YouTube and custom RTMP/RTMPS/SRT presets. **Multistream** to up to 8 destinations from one encode, with per-destination auto-reconnect so one failing platform never drops the others |
| Audio | Mic + desktop (system) audio, mixed. **Windows: native WASAPI capture, on by default, with no Stereo Mix needed.** Other backends: PulseAudio/PipeWire, ALSA, DirectShow, AVFoundation |
| Encoders | libx264, NVENC, QuickSync, AMF, VAAPI, VideoToolbox (whichever your ffmpeg provides) |
| Extras | PNG screenshot (F12), JSON profiles, headless `--record N` / `--stream N` modes for scheduled or cron use |

\*Hard caps of 64 scenes and 64 sources per scene protect against corrupt profiles.

---

## 2. Requirements

**Runtime**
- ffmpeg ≥ 4.4 on `PATH`, or set its path in Settings. Version 5.1 or later is recommended (uses `-fps_mode`; older builds fall back to `-vsync`).
- Linux: an X11 session. Wayland sessions work only through XWayland, because ffmpeg's `x11grab` cannot see native Wayland windows.
- Windows 10/11: ffmpeg built with gdigrab (gyan.dev or BtbN builds work). Audio is captured by LiteCast itself, so ffmpeg needs no audio device support.
- macOS: ffmpeg with AVFoundation, plus Screen Recording permission granted to the terminal or app.

**Build**
- Rust 1.88+ (stable, edition 2024), CMake ≥ 3.15, a C++17 compiler.
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
git clone https://github.com/evalrjcom/litecast && cd litecast
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
litecast [--profile FILE] [--record SECONDS] [--stream SECONDS] [--version] [--help]
```
- `--profile FILE`: use a specific profile. Default: `~/.config/litecast/profile.json` on Linux and macOS, `%APPDATA%\litecast\profile.json` on Windows.
- `--record N`: start recording about 1.5 s after launch, stop after N seconds, finalise the file and exit. Useful with cron or Task Scheduler, e.g. `DISPLAY=:0 litecast --record 3600`.
- `--stream N`: same, but goes live to every enabled destination for N seconds. Both flags can be combined; the app exits when both finish.

---

## 4. Using it

1. **First launch** creates *Scene 1* with a full-screen Display Capture of the primary monitor, fitted to a 1920×1080 canvas.
2. **Add sources** with the **+** under *Sources*. Choose a type and fill in:
   - *Display*: the target is the X display (`:0.0`) on Linux, `desktop` on Windows (or `title=Window Name` to capture one window), or the screen index on macOS. Pick a monitor to auto-fill the region.
   - *Webcam*: press **Detect** to list devices. Size (e.g. `1280x720`) and FPS are optional.
   - *Media / Image*: use **Browse…**.
   - *Color*: pick a colour; use opacity for tints.
3. **Arrange** layers in the preview (drag to move, Shift-drag to resize) or type X/Y/W/H and press **Apply**. **Raise/Lower** change z-order. The list shows the top layer first.
4. **Settings** (Ctrl+,): canvas size, FPS, encoder, bitrate, recording folder and format, stream destinations, audio devices, ffmpeg path.
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
| Ctrl+D | Stream destinations |
| Ctrl+S / Ctrl+Shift+S / Ctrl+O | Save / Save As / Load profile |
| Ctrl+Q | Quit (asks first while live; always finalises recordings) |

### Streaming to Kick, Rumble & others (multistream)
Open **Controls → Stream Destinations…** (Ctrl+D), or **Edit…** in Settings. Press **Add**, pick a service, and the server URL fills in automatically. Paste your stream key and press **OK**. Add as many destinations as you like (up to 8) and enable or disable each one. **Ctrl+L** then goes live to every enabled destination at once.

| Service | Default server URL | Where to get the key | Limits checked by LiteCast |
|---|---|---|---|
| **Kick** | `rtmps://fa723fc1b171.global-contribute.live-video.net/app` | Kick Creator Dashboard → stream settings (copy the URL too if it differs from the default) | RTMPS only, ≤ 8000 kbps, CBR, 2 s keyframes |
| **Rumble** | `rtmp://rtmp.rumble.com/live` | rumble.com → Live Stream / *Get Streamer Configuration*. Keys are per-event unless you enable a static key | keyframes ≤ 2 s recommended |
| Twitch | `rtmp://live.twitch.tv/app` | Creator Dashboard → Settings → Stream | 2 s keyframes |
| YouTube | `rtmp://a.rtmp.youtube.com/live2` | YouTube Studio → Go Live → Stream | 2 s keyframes |
| Custom | any `rtmp://`, `rtmps://` or `srt://host:port?streamid=…` | — | URL syntax only |

How it works and what to expect:
- **One encode, many outputs.** The canvas is encoded once and fanned out with ffmpeg's `tee` muxer. Each destination gets its own `fifo` queue, so CPU cost does not grow with the number of platforms.
- **Independent reconnects.** If one platform drops, only that output retries (every 2 s, up to 25 times, restarting on a keyframe). The others carry on. The status bar shows each one: `● LIVE 00:12:31 → Kick ✓, Rumble ⟳`, where ⟳ means it reported an error in the last 10 s.
- **Pre-flight checks** run when you save destinations or settings, and again on Go Live. Hard errors block the start: no enabled destination, an invalid URL, or an `rtmps://` URL when your ffmpeg lacks TLS support. Warnings are only logged: bitrate above a service limit, keyframe interval mismatch.
- **Live encoding** uses CBR (`nal-hrd=cbr`) with libx264, as Kick and Rumble require. Recommended for 1080p30: *veryfast*, 6000 kbps (stay ≤ 8000 for Kick), keyframe interval 2 s.
- **No audio configured?** A silent stereo track is added automatically while streaming, because several platforms, Kick included, reject video-only streams.
- Changes to destinations apply on the next Go Live.

### Audio notes
- **Linux (PulseAudio/PipeWire)**: Mic `default`, Desktop `@DEFAULT_MONITOR@`. **Detect devices** lists everything `pactl` knows about.
- **Windows (backend `wasapi`, the default)**: LiteCast captures the default microphone and the default speakers/headphones (*loopback*, i.e. everything you hear) itself. Both fields default to `default`. **Detect devices** lists every input (`[in]`) and output (`[out]`) by name: pick one, or clear a field to turn that source off. Unplugging a device or changing the Windows default is handled automatically: the source reopens within a few seconds. A source that plays nothing records as silence, not as a gap.
- **Upgrading from ≤ 1.1.1 on Windows**: profiles that used `dshow` with no devices (the old default, which recorded no audio) switch to `wasapi` automatically. A dshow device you had configured yourself is left unchanged.
- **Windows, dshow backend**: still available for capture cards. Desktop audio there needs a loopback device such as *Stereo Mix* or a virtual cable.
- With ffmpeg-side backends (pulse/alsa/dshow), mixing two inputs uses `amix`, which lowers each input's level. The native Windows mixer sums at full level, clamped to avoid wrap-around.
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
| No audio in recordings (Windows) | Settings → Audio backend `wasapi`, with Mic and Desktop set to `default` (or a device from **Detect devices**). The log shows `[audio] mic: capturing "…"` when it works |
| Recording stops instantly with *Error parsing options for input file pipe:0* | Your ffmpeg is newer than LiteCast ≤ 1.1.0 expected. Update to 1.1.1 or later |
| Stream keeps reconnecting / ⟳ next to a destination | Check that destination's URL and key; the log shows the redacted ffmpeg error. Rumble keys are per-event, so generate a new one or enable a static key |
| "ffmpeg lacks RTMPS" on Go Live | Your ffmpeg was built without TLS. Install a full build (distro ffmpeg, gyan.dev or BtbN). Kick needs RTMPS |

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
        ffmpeg encoder  ffmpeg encoder  (+ ffmpeg-native audio inputs, or ↓)

Windows audio:  WASAPI mic ─▶ ring ┐
                WASAPI loopback ─▶ ring ┴▶ mixer (48 kHz f32, 10 ms, wall-clock paced)
                                              └▶ owner-only named pipe per output ─▶ ffmpeg -f f32le
         (record file)   tee ─┬─ fifo ─▶ Kick (RTMPS)
                              ├─ fifo ─▶ Rumble (RTMP)
                              └─ fifo ─▶ … (≤ 8, onfail=ignore)
```
- **UI thread** owns all state (`AppState`). Widget callbacks only push `Msg` values into an `Rc<RefCell<VecDeque>>` queue, which is drained after every `app.wait()`. No `RefCell` borrow is held across modal dialogs, and no FLTK awake ring buffer is involved.
- **Sources** are keyed by a *signature* (kind + size + FPS + ffmpeg path). Scenes that share an identical source reuse one capture process. Moves and opacity changes never restart a capture; resizes do.
- **Timing**: all loops pace against an absolute schedule (drift-free) and resync after more than 3 periods of lag. Encoders use `-use_wallclock_as_timestamps 1` with CFR output, which keeps A/V in sync even when a frame write stalls.
- **Zero-copy-ish**: frames move as `Arc<Vec<u8>>`. Reader and canvas buffers are recycled through `Arc::try_unwrap` and a small pool, so there is no 8 MB allocation per frame.

### Tests
```bash
DISPLAY=:0 cargo test --release   # 12 unit tests: blending, clipping, sanitising, args, redaction, destinations, migration, pre-flight, audio mixer
```
(One test builds a default profile, which queries monitor geometry, so it needs an X display; Xvfb works.)

### CI & releases
`.github/workflows/ci.yml` runs on every push and pull request:
- **Linux** (Ubuntu 24.04): `cargo fmt --check`, `clippy -D warnings`, unit tests under Xvfb, a release build, and a **headless smoke test**. The smoke test records 4 s of a display + media + colour scene with `--record` and fails if the file has fewer than 60 frames. A **multistream smoke test** then runs `--stream` against two local RTMP listeners and checks that both received H.264 + AAC.
- **Windows** (latest): clippy and release build, which compiles the WASAPI and named-pipe code.
- Artifacts are `litecast_v<ver>-linux-x86_64.tar.gz` and `litecast_v<ver>-windows-x86_64.zip`, each with a SHA-256 file. Download them from the run page.
- **Release**: bump `version` in `Cargo.toml` and the version in this README's title, commit, then
  `git tag v1.1.2 && git push origin v1.1.2`. The tag must match `Cargo.toml`, otherwise CI fails, and the binaries are attached to a GitHub release automatically.

### Security model
- Every child process is spawned with an argv list; no shell is ever invoked.
- Local paths are passed as `file:<path>`, so a filename can't be read as an ffmpeg protocol (`concat:`, `subfile,`, etc.).
- Profiles are size-capped (4 MiB), clamped and repaired by `sanitize()`. Encoder, format and backend names are whitelisted. A broken profile is moved aside to `profile.json.bad`.
- The profile is written atomically with mode **0600** in a 0700 directory, because it contains stream keys.
- Destination URLs are validated: scheme whitelist (`rtmp`, `rtmps`, `srt`), length caps, and rejection of `| [ ] \\ ' " < > ` { } ^` and whitespace. A crafted key cannot inject a second tee output or ffmpeg options.
- Stream keys of every destination, URL credentials and SRT passphrases/streamids are **redacted** from the log, the UI and status text.
- Windows native audio travels over a named pipe with a random 128-bit name. It has an owner-only DACL (plus SYSTEM), rejects remote clients, and allows exactly one instance (`FILE_FLAG_FIRST_PIPE_INSTANCE`), so other users cannot connect and listen to the microphone.
- **Known limitation**: the full stream URL, including the key, appears in the ffmpeg command line, which other local users can read via `ps` or `/proc`. ffmpeg offers no way to read the output URL from stdin. Use a single-user machine for streaming.

### Limits / not yet implemented
Scene transitions (cut only), text sources, filters (chroma key, crop, colour correction), per-source audio levels/meters and media-file audio, studio mode, recording splitting, native Wayland/PipeWire capture.

---

## 7. Changelog
- **1.2.0**: **native Windows audio**. Mic and system audio (WASAPI loopback) are captured by LiteCast, mixed in-app, and fed to ffmpeg over a secure named pipe. This is on by default and needs no Stereo Mix. Silence is filled when nothing plays. Devices reopen automatically after being unplugged. Record and stream share one capture. Old empty-dshow Windows profiles migrate automatically.
- **1.1.1**: fix recording and streaming on current ffmpeg builds (e.g. 2026 Windows builds), which failed with *Error parsing options for input file pipe:0*. `-thread_queue_size` is now passed only to ffmpeg 4.x–6.x.
- **1.1.0**: Kick and Rumble destinations, plus Twitch/YouTube/custom presets. Multistream from one encode (tee + fifo) with per-destination reconnect and status. Silent audio track when no audio is configured. CBR for live x264. RTMPS capability check and pre-flight warnings. `--stream N` CLI flag. v1.0 single URL/key profiles migrate automatically. Rust 1.88+.
- **1.0.0**: initial release.
