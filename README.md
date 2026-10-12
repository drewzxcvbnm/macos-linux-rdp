# macrd — low-latency Linux → macOS remote desktop

A from-scratch remote desktop: **`macrd-server`** (Swift) runs on the Mac, **`macrd`** (Rust) runs on Linux.
It is built for LAN use with the lowest latency it can get:

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
shortcuts); some Globe-key system shortcuts (emoji picker, dictation) may not respond to synthetic events.

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

## Limitations

- No audio yet.
- Cannot show the login window before a user is logged in (ScreenCaptureKit needs a user session).
- `--virtual-display` uses the private `CGVirtualDisplay` API and may break in a future macOS release.
- The cursor is drawn into the video (`--local-cursor` shows the Linux cursor too).
