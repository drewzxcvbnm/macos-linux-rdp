# macrd wire protocol v1

This document is the source of truth for both `server/` (Swift) and `client/` (Rust).

## Transport

A single TCP connection (default port **7878**), `TCP_NODELAY` on both ends.
Normally wrapped in TLS 1.3; `--insecure` on both ends disables TLS for development.

The server uses a self-signed certificate. The client pins the SHA-256 fingerprint
of the server's leaf certificate (DER), trust-on-first-use, like SSH `known_hosts`.

## Framing

Every message is:

```
u32  length    // number of bytes that follow (type + payload), little-endian
u8   type
...  payload   // length - 1 bytes
```

All integers are **little-endian**. `f32` is IEEE-754 little-endian.
`str` is `u16 byte_length` followed by UTF-8 bytes.
Maximum `length` is 64 MiB; a peer receiving a larger length must close the connection.
Unknown message types must be ignored (forward compatibility).

## Handshake

```
client                                   server
  Hello ───────────────────────────────►
        ◄─────────────────────────────── AuthChallenge
  AuthResponse ────────────────────────►
        ◄─────────────────────────────── AuthResult
        ◄─────────────────────────────── ServerInfo
  StartStream ─────────────────────────►
        ◄─────────────────────────────── StreamStarted
        ◄─────────────────────────────── VideoFrame (keyframe first) ...
```

`mac = HMAC-SHA256(key = UTF-8 password, msg = nonce || cert_fingerprint)` where
`cert_fingerprint` is the 32-byte SHA-256 of the server leaf certificate DER, or 32
zero bytes in insecure mode. Binding the fingerprint into the MAC means a MITM with a
different certificate cannot relay the response.

The server must ignore every message other than `Hello`/`AuthResponse`/`Ping` before
`AuthResult{ok=1}` has been sent. On failure it sends `AuthResult{ok=0}` and closes.

## Messages

| type | name            | dir | payload |
|------|-----------------|-----|---------|
| 0x01 | Hello           | C→S | `u16 version (=1)`, `u32 caps` (reserved, 0) |
| 0x02 | AuthChallenge   | S→C | `[32] nonce` |
| 0x03 | AuthResponse    | C→S | `[32] mac` |
| 0x04 | AuthResult      | S→C | `u8 ok` (1 = success) |
| 0x05 | ServerInfo      | S→C | `u8 count`, then per display: `u32 id`, `u32 width_px`, `u32 height_px`, `f32 scale`, `u8 is_main`, `str name` |
| 0x10 | StartStream     | C→S | `u32 display_id` (0 = main), `u32 width`, `u32 height` (0 = native), `u8 fps`, `u32 bitrate_kbps`, `u8 codec`, `u8 flags` |
| 0x11 | StreamStarted   | S→C | `u32 display_id`, `u32 width`, `u32 height`, `u8 codec` |
| 0x12 | RequestKeyframe | C→S | — |
| 0x13 | Resize          | C→S | `u32 width`, `u32 height` |
| 0x14 | Ping            | both| `u64 t` (sender clock, µs) |
| 0x15 | Pong            | both| `u64 t` (echo of Ping.t) |
| 0x20 | VideoFrame      | S→C | `u64 pts_us`, `u8 flags` (bit0 = keyframe), `bytes` Annex-B access unit (rest of payload) |
| 0x21 | FrameAck        | C→S | `u64 pts_us` (sent after the frame was decoded) |
| 0x30 | Key             | C→S | `u16 hid_usage` (USB HID page 0x07), `u8 down` |
| 0x31 | MouseMove       | C→S | `u16 x`, `u16 y` (normalized 0..65535 across the streamed display) |
| 0x32 | MouseButton     | C→S | `u8 button` (0 left, 1 right, 2 middle, 3 back, 4 forward), `u8 down` |
| 0x33 | Scroll          | C→S | `f32 dx`, `f32 dy` (positive dy = scroll up / content moves down; positive dx = scroll left / content moves right, as on macOS), `u8 precise` (1 = pixels, 0 = lines) |
| 0x34 | MediaKey        | C→S | `u8 key` (see Media keys), `u8 down` |
| 0x40 | Clipboard       | both| `u8 kind` (0 = UTF-8 text, 1 = PNG image), `bytes` (rest of payload) |
| 0x7F | Bye             | both| `str reason` |

### Keys
`Key.hid_usage` is a USB HID keyboard-page usage. Usage `0x65` (Keyboard Application / Menu) is
interpreted as the Mac **Fn/Globe** key: PC keyboards handle their own Fn key in firmware, so the client
sends a stand-in key as `0x65` instead.

On a Linux host (`macrd-host`) the same usages are injected as the corresponding evdev/X keys; `0x65` is
the Menu key there and `0xE3`/`0xE7` (GUI) are Super.

### Linux host notes
`ServerInfo` display ids are RandR output ids and `scale` is always 1. `StartStream` flag bit0 (virtual
display) is ignored. Scroll deltas are converted to X wheel button clicks (lines directly; precise pixels
accumulated, 30 px per click).

### Media keys
`0` play/pause, `1` next track, `2` previous track, `3` volume up, `4` volume down, `5` mute,
`6` brightness up, `7` brightness down, `8` keyboard backlight up, `9` keyboard backlight down,
`10` keyboard backlight toggle, `11` eject. Unknown values are ignored.

### Codecs
`0` = H.264, `1` = HEVC.

### StartStream flags
bit0 = use a virtual display sized `width`×`height` (server `--virtual-display` must be enabled; otherwise ignored).

### Video
Each `VideoFrame` carries exactly one access unit in Annex-B format (start codes
`00 00 00 01`). Keyframes carry parameter sets (VPS/SPS/PPS) inline. No B-frames are
ever produced, so decode order == presentation order.

### Flow control
The server keeps at most N (default 3) frames outstanding: a frame is outstanding
from send until the matching `FrameAck`. While at the limit, the server skips *encoding*
new captured frames (it never drops already encoded frames, which would corrupt
the reference chain).

### Resize
`Resize` asks the server to change the stream size (e.g. client window resized). The
server answers with a new `StreamStarted` followed by a keyframe.

### Clipboard
Either side sends `Clipboard` when its local clipboard changes. A receiver applying a
remote clipboard must not echo it back.
