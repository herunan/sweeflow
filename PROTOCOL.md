# sweeflow wire protocol (v1)

Transport: QUIC, ALPN `sweeflow/1`, port 44120/udp. The Mac host is the server and
presents a self-signed certificate; the Linux client pins its SHA-256 fingerprint
(obtained at pairing time).

All integers are little-endian. Every message on every stream is framed as:

```
u32 len        // length of (type + payload)
u8  type
..  payload
```

`str` = `u16 len` + UTF-8 bytes. `bytes` = `u32 len` + raw bytes.
Geometry is in macOS **points** unless stated otherwise; the client converts.

## Control stream

The first bidirectional stream the client opens. Carries everything except video.

| type | dir | name | payload |
|------|-----|------|---------|
| 0x01 | C→S | Hello | u16 version, f32 clientScale, f32 captureScale (0 = window's native backing scale) |
| 0x02 | S→C | Welcome | u16 version, str hostName |
| 0x10 | S→C | WindowAdded | u32 wid, i32 pid, str bundleId, str appName, str title, f32 w, f32 h, u32 parent, f32 relX, f32 relY, u8 kind |
| 0x11 | S→C | WindowChanged | u32 wid, str title, f32 w, f32 h, f32 relX, f32 relY |
| 0x12 | S→C | WindowRemoved | u32 wid |
| 0x13 | S→C | AppIcon | str bundleId, bytes png |
| 0x14 | S→C | Cursor | f32 hotX, f32 hotY, f32 scale, bytes png (hot spot in points) |
| 0x15 | S→C | Focused | u32 wid (Mac-side key window changed) |
| 0x16 | S→C | WindowButtons | u32 wid, 3 × (f32 x, y, w, h): close, minimise, zoom button frames (window-relative; zero = absent) |
| 0x17 | S→C | MoveStarted | u32 wid — the app began dragging its window; client should start an interactive move |
| 0x1A | S→C | Minimize | u32 wid — the window was minimised on the Mac (restored there); client minimises its window |
| 0x1B | S→C | MenuTree | i32 pid, bytes json — the app's menu bar: `[{t,e,sep?,k?,m?,c?,sub?}]` (Apple menu omitted) |
| 0x24 | C→S | MenuActivate | i32 pid, u16 n, n × u16 path — press that menu item (indices as in MenuTree) |
| 0x1C | S→C | AppList | u16 n, n × (str bundleId, str name, bytes png128) — installed apps, for launchers |
| 0x27 | C→S | LaunchApp | str bundleId — open (or reopen) the app on the Mac |
| 0x26 | C→S | Restore | u32 wid — the user restored a window that was minimised on the Mac |
| 0x25 | C→S | MenuRefresh | i32 pid — re-read and resend the app's menus |
| 0x18 | S→C | ToggleMaximize | u32 wid — the app zoomed its window (title-bar double-click); client toggles maximised |
| 0x20 | C→S | Resize | u32 wid, f32 w, f32 h |
| 0x21 | C→S | Focus | u32 wid |
| 0x22 | C→S | Close | u32 wid |
| 0x23 | C→S | ScreenSize | u16 w, u16 h — logical size of the client's largest monitor (sizes the Mac's virtual display) |
| 0x30 | C→S | PointerMove | u32 wid, f32 x, f32 y (window-relative points) |
| 0x31 | C→S | PointerButton | u32 wid, f32 x, f32 y, u8 button (0 left,1 right,2 middle,3+ other), u8 pressed |
| 0x32 | C→S | Scroll | u32 wid, f32 x, f32 y, f32 dx, f32 dy, u8 unit (0 lines, 1 pixels), u8 phase (0 none, 1 began, 2 changed, 3 ended) |
| 0x33 | C→S | Key | u32 evdevKeycode, u8 pressed |
| 0x34 | C→S | ReleaseAll | (no payload) — client lost focus; release held keys/buttons |
| 0x40 | both | Clipboard | str mime, bytes data |
| 0x41 | both | Ping | u64 t (echoed back unchanged as Pong 0x42) |

`kind`: 0 = normal toplevel, 1 = popup/menu (child of `parent`), 2 = sheet/dialog.

## Video streams

The client opens one extra bidirectional stream per window it wants to see.

| type | dir | name | payload |
|------|-----|------|---------|
| 0x50 | C→S | Subscribe | u32 wid |
| 0x51 | S→C | Frame | u64 captureUs, u16 pxW, u16 pxH, u8 flags (bit0 keyframe, bit1 lossless, bit2 dark title bar, bit3 full-range YUV, bits 4–7 pixels per point), bytes data — HEVC Annex B, or with bit1 an LZ4 block of premultiplied BGRA (pxW·pxH·4 bytes) used for popups/menus |
| 0x55 | S→C | Corners | u16 tilePx, bytes lz4 — four tilePx² premultiplied BGRA tiles (TL, TR, BL, BR) with the window's rounded, transparent corners; tilePx 0 = square corners. Applies from the next frame. |
| 0x52 | C→S | RequestKeyframe | — |
| 0x53 | C→S | Pause | u8 paused (window minimised / hidden) |
| 0x54 | C→S | FrameAck | u64 captureUs (echo when the frame has been presented; Mac uses it for latency stats and flow control) |

Closing a video stream unsubscribes.

## Audio stream

| type | dir | name | payload |
|------|-----|------|---------|
| 0x60 | C→S | AudioSubscribe | — (on a new bidirectional stream) |
| 0x61 | S→C | Audio | bytes pcm — 48 kHz stereo s16le, the streamed apps' audio mixed |
