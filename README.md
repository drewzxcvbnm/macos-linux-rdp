# macrd — low-latency Linux ↔ macOS remote desktop

A from-scratch remote desktop: **`macrd-server`** (Swift) runs on the Mac, **`macrd`** (Rust) runs on Linux.
The other direction works too: **`macrd-host`** (Rust) shares a Linux X11 desktop, and `macrd` runs on the
Mac (see [Linux host](#linux-host-macos--linux)). It is built for LAN use with the lowest latency it can get:

- ScreenCaptureKit capture → VideoToolbox hardware H.264/HEVC (low-latency rate control, no B-frames)
- FFmpeg decode on the client (VAAPI on Linux, VideoToolbox on macOS, software fallback), SDL2 rendering, vsync off
- Frame-ack flow control: the server never builds a queue of stale frames
- Keyboard (USB HID usages, layout-independent), mouse, drag, double-click, wheel + touchpad scrolling
- Bi-directional clipboard (text + images)
- Multiple displays, stream resized to the client window, optional virtual display sized to the window
- TLS 1.3 with SSH-style fingerprint pinning, plus a password challenge bound to the TLS certificate

Wire format: [PROTOCOL.md](PROTOCOL.md).

## Server (macOS 14+)

```sh
cd server
swift build -c release
sudo cp .build/release/macrd-server /usr/local/bin/
macrd-server            # prints the TLS fingerprint and password on startup
```

On first run it creates `~/Library/Application Support/macrd/` with a self-signed TLS identity and a random
password (`password` file; edit it to change the password, or pass `--password`).

Permissions (System Settings → Privacy & Security), granted to the terminal running it or to the binary:
- **Screen & System Audio Recording**: required for video
- **Accessibility**: required for keyboard/mouse input

Options: `--port`, `--virtual-display` (allow clients to request a virtual monitor; useful for headless Macs),
`--hide-cursor`, `--max-in-flight N`, `--list-displays`, `--dump FILE` (record the screen to an Annex-B file
for debugging, e.g. `ffplay out.h264`), `--insecure` (no TLS, development only).

Run at login: `cp launchd/com.macrd.server.plist ~/Library/LaunchAgents/ && launchctl load ~/Library/LaunchAgents/com.macrd.server.plist`
(grant the permissions to `/usr/local/bin/macrd-server` in this case).

Self-test (XCTest needs full Xcode, so tests are an executable): `swift run macrd-selftest`.

## Client (Linux)

Dependencies (Debian/Ubuntu):

```sh
sudo apt install build-essential pkg-config clang libsdl2-dev \
  libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavfilter-dev libavdevice-dev \
  libva-dev libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev
# Fedora: sudo dnf install SDL2-devel ffmpeg-devel libva-devel clang pkgconf
```

Build and run:

```sh
cd client
cargo build --release
./target/release/macrd my-mac.local          # asks for the password, then to trust the fingerprint
```

The fingerprint is stored in `~/.config/macrd/known_hosts`. A changed fingerprint is refused.

Useful flags: `--display N`, `--codec hevc`, `--bitrate 60` (Mbps), `--fps 120`, `--fullscreen`,
`--native` (stream native resolution instead of the window size), `--virtual-display`,
`--swap-cmd-ctrl` (PC-style Ctrl+C/V → Cmd+C/V), `--fn-key KEY`, `--hwaccel none|vaapi`, `--no-clipboard`,
`--password` / `$MACRD_PASSWORD`, `--fingerprint HEX`. The window title shows fps, bitrate, decode time and RTT.

Default key mapping: Super → Cmd, Alt → Option, Ctrl → Control, **Menu → Fn/Globe**.
PC keyboards handle their own Fn key in firmware, so it never reaches the client; pick a stand-in with
`--fn-key menu|right-ctrl|right-alt|right-super|caps-lock|none`. Fn works as a modifier (Fn+click, Fn+key
shortcuts). macOS ignores synthetic Globe presses, so the server emulates a Globe **tap** itself, following
System Settings → Keyboard → "Press 🌐 key to": change input source (switch keyboard layout) or show emoji.
Dictation cannot be triggered remotely.

Media keys (play/pause, next/previous, volume, mute, brightness, keyboard backlight, eject) are forwarded
as macOS system media events. With the keyboard grab active, X11 delivers them to the client; some Wayland
compositors keep them for themselves.

| Hotkey          | Action                       |
|-----------------|------------------------------|
| Ctrl+Alt+G      | toggle keyboard grab         |
| Ctrl+Alt+F      | toggle fullscreen            |
| Ctrl+Alt+→ / ←  | next / previous Mac display  |
| Ctrl+Alt+Q      | quit                         |

The client also builds on macOS (`brew install sdl2 ffmpeg pkg-config`, then
`LIBRARY_PATH=/opt/homebrew/lib cargo build`), which is handy for testing against a local server.

## Linux host (macOS → Linux)

`macrd-host` shares an X11 session (tested with i3; any X11 desktop works) using the same protocol, so the
`macrd` client connects to it exactly like to a Mac:

- MIT-SHM capture of one RandR monitor, XDamage so a static screen costs (almost) nothing, cursor drawn in
- NVENC H.264/HEVC (`p1`/`ull`, CBR, no B-frames), falling back to x264/x265 `ultrafast`/`zerolatency`
- XTest keyboard/mouse/scroll injection, media keys as XF86 keys, clipboard via the X selection
- Keeps the screensaver/locker/DPMS away while a client is connected

Build on the Linux machine (same FFmpeg packages as the client; no SDL needed). For NVENC, the distro FFmpeg
must be built with it (Debian/Ubuntu/Arch/Fedora RPM Fusion are) and the NVIDIA driver installed:

```sh
cd client
cargo build --release -p macrd-host
./target/release/macrd-host --list-displays
./target/release/macrd-host                   # prints the TLS fingerprint and password
```

First run creates `~/.config/macrd-host/` with a self-signed TLS identity (`cert.der`, `key.der`) and a random
`password` (or pass `--password` / `$MACRD_PASSWORD`). Options: `--port`, `--bind`, `--encoder auto|nvenc|software`,
`--hide-cursor`, `--max-in-flight N`, `--always-capture` (ignore XDamage, e.g. if some app does not report
damage), `--insecure`.

It must run inside the X session (it needs `$DISPLAY`). With i3, add to `~/.config/i3/config`:

```
exec --no-startup-id ~/macrd/client/target/release/macrd-host
```

On the Mac, build the client (`brew install sdl2 ffmpeg pkg-config`, then
`cd client && LIBRARY_PATH=/opt/homebrew/lib cargo build --release -p macrd`) and connect:

```sh
./target/release/macrd my-linux-box.local --natural-scroll --codec hevc
```

- Cmd arrives as Super, i.e. i3's `$mod` when it is `Mod4`. Use `--swap-cmd-ctrl` if you would rather have
  Cmd+C/V act as Ctrl+C/V (Ctrl then becomes Super).
- `--natural-scroll` keeps the macOS scroll direction; without it scrolling on Linux feels reversed.
- Fn/Globe never reaches apps on macOS; the Menu-key stand-in (`--fn-key`) arrives on Linux as the Menu key.
- The display IDs printed by the client are RandR output IDs; `--display 0/1/...` (index) works as well.
- macOS keeps some shortcuts for itself even with the keyboard grab (Cmd+Tab, Cmd+Space, Mission Control,
  and media/brightness keys); remap them in System Settings or use the i3 alternatives while connected.
- `--virtual-display` is ignored by the Linux host.

## Limitations

- No audio yet.
- Cannot show the login window before a user is logged in (ScreenCaptureKit needs a user session).
- `--virtual-display` uses the private `CGVirtualDisplay` API and may break in a future macOS release.
- The cursor is drawn into the video (`--local-cursor` shows the Linux cursor too).
