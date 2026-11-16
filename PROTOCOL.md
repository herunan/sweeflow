# sweeflow wire protocol (v1)

Transport: QUIC, ALPN `sweeflow/1`, port 44120/udp. The Mac host is the server and
presents a self-signed certificate; the Linux client pins its SHA-256 fingerprint, and
the host only lets in client certificates whose fingerprint it has (both obtained when the
two are linked, see Linking).

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
| 0x01 | C→S | Hello | u16 version, f32 clientScale, f32 captureScale (0 = window's native backing scale), str computerName (newer clients; see One computer at a time) |
| 0x02 | S→C | Welcome | u16 version, str hostName, str kind ("laptop", "imac", "desktop"; newer hosts) |
| 0x03 | S→C | MacDisconnected | — Stop on the Mac (its menu) for this client: a client from version 3 stops as its own Stop would, and connects again only on Start, pressed there or on the Mac (the Mac then announces `start` = the start of the client's fingerprint in its TXT record, and in the PairResult of a PairCheck, until that client connects); the Mac lets a version-3 client in again on connecting. Older clients are refused until Start on the Mac |
| 0x05 | C→S | Goodbye | — leaving on purpose (switching Macs, disconnecting, quitting): the Mac tidies up at once |
| 0x04 | S→C | Forgotten | — the Mac forgot this client; it forgets the Mac too |
| 0x06 | S→C | Busy | str computerName — the host is streaming to that computer: this one is turned away (closed half a second later; what it sends meanwhile is ignored) |
| 0x10 | S→C | WindowAdded | u32 wid, i32 pid, str bundleId, str appName, str title, f32 w, f32 h, u32 parent, f32 relX, f32 relY, u8 kind |
| 0x11 | S→C | WindowChanged | u32 wid, str title, f32 w, f32 h, f32 relX, f32 relY |
| 0x12 | S→C | WindowRemoved | u32 wid |
| 0x13 | S→C | AppIcon | str bundleId, bytes png |
| 0x14 | S→C | Cursor | f32 hotX, f32 hotY, f32 scale, bytes png (hot spot in points) |
| 0x15 | S→C | Focused | u32 wid (Mac-side key window changed) |
| 0x16 | S→C | WindowButtons | u32 wid, 3 × (f32 x, y, w, h): close, minimise, zoom button frames (window-relative; zero = absent) |
| 0x17 | S→C | MoveStarted | u32 wid — the app began dragging its window; client should start an interactive move |
| 0x2E | S→C | MoveReady | u32 wid, u32 pressId — the press with that id is on a spot that drags the window (title bar, toolbar, or a spot that did before): the client starts its move on the first motion |
| 0x2F | C→S | MoveTaken | u32 wid — the client started moving the window itself: release the press on the Mac |
| 0x37 | S→C | DragSpot | u32 wid, f32 x, f32 y (window points), u8 drags — what the host found under the pointer as it hovered the window's title-bar strip, before any press: `drags` 1 means a press there drags the window (bare title bar or toolbar, or a spot that did before). A client that then gets a press within a few points of (x, y), and soon, starts its move on the first motion without waiting for MoveReady (and sends MoveTaken as usual) |
| 0x38 | S→C | WindowReplaced | u32 oldWid, then a window as in WindowAdded — that window took the old one's place at the same frame in one step (a tabbed window's tab switched: each tab is a window of its own on the Mac). The client keeps the window it shows (place, size, focus, its last picture until the new one's first frame) and makes it the new one's: it unsubscribes the old video stream and subscribes the new. The old wid isn't mentioned again (no WindowRemoved). Sent only to clients whose Hello says version 2 or more; others get WindowAdded and WindowRemoved |
| 0x1A | S→C | Minimize | u32 wid — the window was minimised on the Mac (restored there); client minimises its window |
| 0x1B | S→C | MenuTree | i32 pid, bytes json — the app's menu bar: `[{t,e,sep?,k?,m?,c?,sub?}]` (Apple menu omitted) |
| 0x24 | C→S | MenuActivate | i32 pid, u16 n, n × u16 path — press that menu item (indices as in MenuTree) |
| 0x1C | S→C | AppList | u16 n, n × (str bundleId, str name, bytes png128) — installed apps, for launchers |
| 0x1D | S→C | ScreenLocked | u8 locked — while locked the host sends no frames (capture is blank) |
| 0x1E | S→C | Settings | bytes JSON `{enabled, exclude[], sound, dim, clipboard, dragDrop, launcher, quality, desktop}` — on hello and after every change (from either side) |
| 0x1F | S→C | BlindShow | u32 id, str app, str title, u16 n × str text, u16 n × str button — a window that refuses capture (security prompt, sign-in verifier): shown as a native stand-in |
| 0x19 | S→C | BlindGone | u32 id — that window closed |
| 0x27 | C→S | LaunchApp | str bundleId — open (or reopen) the app on the Mac |
| 0x28 | C→S | SetSettings | bytes JSON, same shape as Settings (any subset of the fields) — the host saves it and answers with Settings |
| 0x29 | C→S | BlindPress | u32 id, u16 button — press that button of an uncapturable window |
| 0x2A | C→S | ShowDisplay | u8 on — open (1) or close (0) the "Mac display" view: the host announces the whole virtual display as window 0xFFFFFF00 (WindowAdded kind 3), captured as a display; pointer events for it are display-relative points. The client scales it to fit and never sends Resize for it; Close on it closes the view |
| 0x2B | C→S | QuitApp | str bundleId — quit the app as ⌘Q would |
| 0x2C | S→C | RunningApps | u16 n, n × (str bundleId, u8 state) — regular apps running on the Mac; state 3 = a window shown on Linux, 2 = only minimised windows, 1 = no window. Sent on change |
| 0x2D | S→C | ScreenSelect | u8 on — macOS's screenshot selection (Cmd+Shift+4) started (1) or ended (0). Its windows can't be captured and aren't streamed: the client draws the rectangle of a left drag itself meanwhile |
| 0x26 | C→S | Restore | u32 wid — the user restored a window that was minimised on the Mac |
| 0x25 | C→S | MenuRefresh | i32 pid — re-read and resend the app's menus |
| 0x18 | S→C | ToggleMaximize | u32 wid — the app zoomed its window (title-bar double-click); client toggles maximised |
| 0x20 | C→S | Resize | u32 wid, f32 w, f32 h |
| 0x21 | C→S | Focus | u32 wid |
| 0x22 | C→S | Close | u32 wid |
| 0x23 | C→S | ScreenSize | u16 w, u16 h — logical size of the client's largest monitor (sizes the Mac's virtual display) |
| 0x30 | C→S | PointerMove | u32 wid, f32 x, f32 y (window-relative points) |
| 0x31 | C→S | PointerButton | u32 wid, f32 x, f32 y, u8 button (0 left,1 right,2 middle,3+ other), u8 pressed, u32 pressId (newer clients; numbers left presses, for MoveReady) |
| 0x32 | C→S | Scroll | u32 wid, f32 x, f32 y, f32 dx, f32 dy, u8 unit (0 wheel notches, fractional — the host animates them; 1 pixels), u8 phase (0 none, 1 began, 2 changed, 3 ended) |
| 0x33 | C→S | Key | u32 evdevKeycode, u8 pressed |
| 0x34 | C→S | ReleaseAll | (no payload) — client lost focus; release held keys/buttons |
| 0x40 | both | Clipboard | u16 n, n × (str kind, str name, bytes data) — kind `text/plain;charset=utf-8`, `image/png`, `file` (name = file name; folders as .zip) or, from the Mac, `file-ref` (a file named only, data = u32 copy number; fetched with ClipFetch when pasted, so the Mac reads no file that isn't pasted). Sent on the clipboard stream. |
| 0x4C | C→S | ClipFetch | u32 copy number — the Mac's `file-ref`s of that copy are being pasted (clipboard stream) |
| 0x4D | S→C | ClipFiles | u32 copy number, u16 n, n × (str kind, str name, bytes data) — those files (none: the clipboard changed since, or they couldn't be read) |
| 0x46 | C→S | DropFiles | u32 wid, f32 x, f32 y, u16 n, n × (str kind, str name, bytes data) — dropped on a window (clipboard stream); kind `file`, `url` or `text/plain;charset=utf-8` |
| 0x47 | S→C | DragBegan | u32 wid, u16 n, n × str name — a file drag started in a Mac window |
| 0x48 | C→S | DragTaken | u32 wid — Linux took the drag over; cancel it on the Mac |
| 0x49 | C→S | DragFetch | — send the dragged files (clipboard stream) |
| 0x4A | S→C | DragData | u16 n, n × (str kind, str name, bytes data) — the dragged files (clipboard stream) |
| 0x4B | C→S | DragDropLocal | u32 wid, f32 x, f32 y — a drag out of a Mac window was dropped on another Mac window: drop the dragged files there |
| 0x43 | C→S | ClipboardSubscribe | — (opens the clipboard stream, a new bidirectional stream) |
| 0x41 | both | Ping | u64 t (echoed back unchanged as Pong 0x42) |

### One computer at a time

The host serves one client's session, and only the client being served lets go of it (Stop
there, quitting, or its connection dropping). A hello from another client while one is served:

- the same computer reconnecting (same certificate fingerprint and same `computerName`; a
  certificate alone doesn't tell computers apart, as one can be copied), or a served client the
  host hasn't heard from for 8 s (clients ping every 2 s): the old connection is closed and the
  new one served;
- any other: Busy, naming the computer served, and nothing else. The served computer doesn't
  notice.

A client told Busy shows "In use by ‹computerName›" with Start unavailable ("Stop streaming on
‹computerName› first"), and looks again with a fresh hello after 5 s, 10 s, 20 s, then every
30 s; once it is let in it streams (unless Stop was chosen there meanwhile). Older clients (no
`computerName`) are turned away the same way and just see the connection close. A host older
than Busy replaces the session on every new connection.

`kind`: 0 = normal toplevel, 1 = popup/menu (child of `parent`), 2 = sheet/dialog, 3 = the Mac display view.

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

## Video datagrams

Delta (non-key) HEVC frames go as QUIC datagrams instead of on the window's stream, so a lost
packet costs that frame rather than stalling the frames behind it. Keyframes, lossless (popup)
frames and corners stay on the stream. A FRAME message's body ends with a `u32` sequence number
(HEVC frames only); the frame message, framed exactly as on the stream, is cut into datagrams of
at most 1100 payload bytes, each with a 12-byte header: `u32 window id, u32 sequence, u16 piece
index, u16 piece count` (little-endian). The client reassembles, orders by sequence, gives up on a
missing frame after 40 ms and sends REQUEST_KEYFRAME (0x52) on the window's stream. The host
forgets unacknowledged frames after 300 ms. `videoDatagrams: false` in the host's config.json
turns this off.

## Linking

How a Linux client and a Mac host get each other's certificate fingerprint, and how either one
unlinks. Started from Linux: the person picks the Mac, the Mac shows a 4-digit code, the person
types it on Linux, and that's it. Nothing has to be opened on the Mac first.

Transport: a second QUIC listener on the host, ALPN `sweeflow-pair/3`, port 44121/udp, up
whenever the host runs and advertised as `_sweeflow-pair._udp` (TXT: `id` = first 16 hex digits
of the host certificate's fingerprint, `port` = the streaming port, `v`). TLS 1.3 with a
certificate from each side, as for streaming, but any certificate is accepted in the handshake:
each side only notes the SHA-256 fingerprint of the certificate the other one proved it holds.
Messages are framed as everywhere else, on a bidirectional stream the client opens (one per
request).

| type | dir | name | payload |
|------|-----|------|---------|
| 0x70 | C→S | PairRequest | u16 linkVersion (3), str computerName |
| 0x75 | S→C | PairShare | u8 triesLeft, bytes hostShare (32) — the host is showing its code |
| 0x76 | C→S | PairGuess | bytes clientShare (32), bytes clientTag (32) |
| 0x77 | C→S | PairUnlink | — the client unlinked the host: the host forgets the client's certificate |
| 0x78 | C→S | PairCheck | — is the client still linked here? |
| 0x71 | S→C | PairResult | u8 status, str hostName, u16 streamPort, u16 linkVersion (the host's), bytes hostTag (32 with status 0 after a PairGuess, else empty), u8 busy, u8 locked, u8 start (Start was pressed on the Mac for the asking client) |

(0x72–0x74 were version 2's nonce exchange, where both screens showed a code to compare.)

### Linking

1. The client sends PairRequest. The host answers with PairResult straight away when it won't
   show a code (status 2 to 6 below). Otherwise it picks a random 4-digit code, shows it ("Enter
   this code on ‹computerName›"), and sends PairShare.
2. The person types the code on the client, which sends PairGuess.
3. The host checks clientTag. Right: it adds the client's fingerprint to its linked clients and
   answers PairResult status 0 with hostTag; the client checks hostTag and only then stores the
   host's fingerprint with `hostName` and `streamPort`. Wrong: the host sends a new PairShare
   (fresh scalar, `triesLeft` one less), and the client asks the person again; after the third
   wrong code the host answers status 7 and takes the code down.
4. The code is shown for five minutes (status 8 after that); the person at the host can cancel it
   (status 1). Closing the connection takes it down.

The host shows one code at a time. Wrong codes in a row (counted across codes, reset by a
successful link) make it wait before showing another: nothing for the first three, then 30 s after
the last wrong one, doubling with every three more, up to an hour (status 3 meanwhile).

### The key exchange

CPace over X25519, with the code as the password and both fingerprints bound in. Fingerprints go in
as 64 lowercase hex characters (ASCII), each side using its own certificate's and the one the
other side proved it holds in this connection's handshake. `H(a, b, …)` is SHA-256 over the parts,
each preceded by its length as one byte. `label` = `"sweeflow-link/3"`.

```
r     = H(label, "generator", code, clientFingerprint, hostFingerprint), top bit (bit 255) cleared,
        as a little-endian integer mod p = 2^255 − 19
G     = Elligator 2 of r onto Curve25519 (RFC 9380 §6.7.1, Z = 2), the u-coordinate:
          x1 = −A / (1 + 2r²)   (x1 = −A if that's 0; A = 486662)
          G  = x1 if x1³ + A·x1² + x1 is a square mod p, else −x1 − A
share = X25519(random 32-byte scalar, G)          (clientShare, hostShare)
K     = X25519(own scalar, other side's share)    refused if all zeroes
key   = H(label, "key", clientFingerprint, hostFingerprint, clientShare, hostShare, K)
clientTag = HMAC-SHA256(key, label ‖ "client")
hostTag   = HMAC-SHA256(key, label ‖ "host")
```

Why it holds up with only four digits: someone without the code (a computer asking to link, a fake
Mac, or a computer relaying between the two) gets to test one guess per PairShare/PairGuess run,
and only by talking to the other side. Nothing on the wire lets it test codes offline: G's discrete
log to any other code's G is unknown, and Elligator 2 always lands on the curve itself rather than
its twist, so shares (clamped scalars times G) are points of the prime-order subgroup whatever the
code is. A relay holds its own certificate on each side, so the two sides derive different
generators and keys: relaying counts as a wrong guess. Three guesses per code, a code window on the
Mac's screen for each, and the growing wait make guessing one code a 3-in-10,000 chance per window
shown. Shares received may be anything (also twist or small-order points): X25519's clamping makes
scalars multiples of 8, which clears the small-order part, Curve25519 is twist-secure, and an
all-zero K is refused. Each side keeps the other's certificate only after checking the other's tag.

### Linked once, unlinked on both sides

- The host answers a PairRequest from a client it already has with status 5, and the client
  doesn't ask a host it already has (it shows it as linked). Linking again takes unlinking first.
- Unlinking on Linux: the client sends PairUnlink (pinning the host's certificate) and the host
  forgets it and answers status 9. If the host can't be reached, the client notes it and sends
  PairUnlink once it sees the host advertised again, or before a PairRequest to that host.
- Unlinking on the Mac: a connected client is told with Forgotten (0x04) on its streaming
  connection; one that isn't connected is told on its next streaming connection, or when it next
  sees the host advertised: the client sends PairCheck to each linked host it sees (at most every
  ten minutes, pinning its certificate), and on status 10 forgets it too and says so.
- PairUnlink and PairCheck act on the certificate of the connection they come on, as the TLS
  handshake proved it, so a computer can only unlink itself.

`status`:

| status | meaning |
|--------|---------|
| 0 | linked: the code was right (with hostTag); or, to PairCheck, still linked |
| 1 | cancelled: the person at the host closed the code window |
| 2 | busy: a code is showing for another computer |
| 3 | wait: too many wrong codes lately |
| 4 | locked: the Mac's screen is locked |
| 5 | linked already: the host has this client. No code |
| 6 | the host doesn't speak the request's `linkVersion`; its own is in the result |
| 7 | three wrong codes: the host stopped |
| 8 | the code expired |
| 9 | unlinked (answer to PairUnlink) |
| 10 | not linked (answer to PairCheck) |

Versions: the ALPN names the exchange. The host also accepts `sweeflow-pair/2`, only to answer a
version 2 client's request with status 6, so it says to update rather than failing the handshake;
the client offers `sweeflow-pair/2` too, so a version 2 host answers status 6 likewise. A client
that can't connect tries `sweeflow-pair/1` once: if that handshake works, the Mac's Sweeflow is
from before both (it showed a code only while its window was open).
