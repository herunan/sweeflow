//! Wire format shared with the Mac host; see PROTOCOL.md.

use anyhow::{bail, Result};

pub const VERSION: u16 = 1;

pub mod t {
    pub const HELLO: u8 = 0x01;
    pub const WELCOME: u8 = 0x02;
    /// The Mac disconnected this computer (its menu): refused until connected there again.
    pub const MAC_DISCONNECTED: u8 = 0x03;
    /// The Mac forgot this computer: forget it too.
    pub const FORGOTTEN: u8 = 0x04;
    /// Leaving on purpose (switching Macs, disconnecting, quitting): the Mac tidies up at once.
    pub const GOODBYE: u8 = 0x05;
    pub const WINDOW_ADDED: u8 = 0x10;
    pub const WINDOW_CHANGED: u8 = 0x11;
    pub const WINDOW_REMOVED: u8 = 0x12;
    pub const APP_ICON: u8 = 0x13;
    pub const CURSOR: u8 = 0x14;
    pub const FOCUSED: u8 = 0x15;
    pub const WINDOW_BUTTONS: u8 = 0x16;
    pub const MOVE_STARTED: u8 = 0x17;
    pub const TOGGLE_MAXIMIZE: u8 = 0x18;
    /// The press just made is on a spot that drags the window (title bar, toolbar): start the
    /// move on the first bit of motion instead of waiting for the app to begin dragging.
    pub const MOVE_READY: u8 = 0x2E;
    pub const MINIMIZE: u8 = 0x1A;
    pub const MENU_TREE: u8 = 0x1B;
    pub const APP_LIST: u8 = 0x1C;
    pub const BLIND_SHOW: u8 = 0x1F;
    pub const BLIND_GONE: u8 = 0x19;
    pub const SCREEN_LOCKED: u8 = 0x1D;
    pub const SETTINGS: u8 = 0x1E;
    pub const RESIZE: u8 = 0x20;
    pub const FOCUS: u8 = 0x21;
    pub const CLOSE: u8 = 0x22;
    pub const SCREEN_SIZE: u8 = 0x23;
    pub const MENU_ACTIVATE: u8 = 0x24;
    pub const MENU_REFRESH: u8 = 0x25;
    pub const RESTORE: u8 = 0x26;
    pub const LAUNCH_APP: u8 = 0x27;
    pub const SET_SETTINGS: u8 = 0x28;
    pub const BLIND_PRESS: u8 = 0x29;
    /// u8 on: open (or close) the "Mac display" view, which arrives as window DISPLAY_WID.
    pub const SHOW_DISPLAY: u8 = 0x2A;
    /// str bundle id: quit the app (as ⌘Q).
    pub const QUIT_APP: u8 = 0x2B;
    /// u16 n × (str bundle id, u8 state 3 shown / 2 minimised / 1 no window): running Mac apps.
    pub const RUNNING_APPS: u8 = 0x2C;
    /// u8 on: macOS's screenshot selection (Cmd+Shift+4) started or ended.
    pub const SCREEN_SELECT: u8 = 0x2D;
    pub const POINTER_MOVE: u8 = 0x30;
    pub const POINTER_BUTTON: u8 = 0x31;
    /// Linux started moving the window itself (after MOVE_READY): let go of the press there.
    pub const MOVE_TAKEN: u8 = 0x2F;
    pub const SCROLL: u8 = 0x32;
    pub const KEY: u8 = 0x33;
    pub const RELEASE_ALL: u8 = 0x34;
    pub const CLIPBOARD: u8 = 0x40;
    #[allow(dead_code)] // part of the protocol; this client measures RTT from QUIC instead
    pub const PING: u8 = 0x41;
    pub const CLIPBOARD_SUBSCRIBE: u8 = 0x43;
    pub const DROP_FILES: u8 = 0x46;
    pub const DRAG_BEGAN: u8 = 0x47;
    pub const DRAG_TAKEN: u8 = 0x48;
    pub const DRAG_FETCH: u8 = 0x49;
    pub const DRAG_DATA: u8 = 0x4A;
    pub const DRAG_DROP_LOCAL: u8 = 0x4B;
    pub const PONG: u8 = 0x42;
    pub const SUBSCRIBE: u8 = 0x50;
    pub const FRAME: u8 = 0x51;
    pub const REQUEST_KEYFRAME: u8 = 0x52;
    pub const PAUSE: u8 = 0x53;
    pub const FRAME_ACK: u8 = 0x54;
    pub const CORNERS: u8 = 0x55;
    pub const AUDIO_SUBSCRIBE: u8 = 0x60;
    pub const AUDIO: u8 = 0x61;
    // Pairing listener only.
    pub const PAIR_REQUEST: u8 = 0x70;
    pub const PAIR_RESULT: u8 = 0x71;
}

/// Builds one framed message.
pub struct W(Vec<u8>);

impl W {
    pub fn new(ty: u8) -> Self {
        let mut v = Vec::with_capacity(32);
        v.extend_from_slice(&[0, 0, 0, 0, ty]);
        W(v)
    }
    pub fn u8(mut self, v: u8) -> Self { self.0.push(v); self }
    pub fn i32(mut self, v: i32) -> Self { self.0.extend_from_slice(&v.to_le_bytes()); self }
    pub fn u16(mut self, v: u16) -> Self { self.0.extend_from_slice(&v.to_le_bytes()); self }
    pub fn u32(mut self, v: u32) -> Self { self.0.extend_from_slice(&v.to_le_bytes()); self }
    pub fn u64(mut self, v: u64) -> Self { self.0.extend_from_slice(&v.to_le_bytes()); self }
    pub fn f32(mut self, v: f32) -> Self { self.0.extend_from_slice(&v.to_le_bytes()); self }
    pub fn str(mut self, s: &str) -> Self {
        let b = &s.as_bytes()[..s.len().min(65535)];
        self = self.u16(b.len() as u16);
        self.0.extend_from_slice(b);
        self
    }
    pub fn bytes(mut self, b: &[u8]) -> Self {
        self = self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
        self
    }
    pub fn finish(mut self) -> Vec<u8> {
        let len = (self.0.len() - 4) as u32;
        self.0[..4].copy_from_slice(&len.to_le_bytes());
        self.0
    }
}

/// Reads the payload of one message (type byte already split off).
pub struct R<'a> {
    b: &'a [u8],
}

impl<'a> R<'a> {
    pub fn new(b: &'a [u8]) -> Self { R { b } }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.b.len() < n {
            bail!("short message");
        }
        let (h, t) = self.b.split_at(n);
        self.b = t;
        Ok(h)
    }
    pub fn u8(&mut self) -> Result<u8> { Ok(self.take(1)?[0]) }
    pub fn u16(&mut self) -> Result<u16> { Ok(u16::from_le_bytes(self.take(2)?.try_into()?)) }
    pub fn u32(&mut self) -> Result<u32> { Ok(u32::from_le_bytes(self.take(4)?.try_into()?)) }
    pub fn i32(&mut self) -> Result<i32> { Ok(i32::from_le_bytes(self.take(4)?.try_into()?)) }
    pub fn u64(&mut self) -> Result<u64> { Ok(u64::from_le_bytes(self.take(8)?.try_into()?)) }
    pub fn f32(&mut self) -> Result<f32> { Ok(f32::from_le_bytes(self.take(4)?.try_into()?)) }
    pub fn str(&mut self) -> Result<String> {
        let n = self.u16()? as usize;
        Ok(String::from_utf8_lossy(self.take(n)?).into_owned())
    }
    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

/// Reads one framed message from a QUIC stream. Returns (type, payload).
pub async fn read_msg(rx: &mut quinn::RecvStream) -> Result<Option<(u8, Vec<u8>)>> {
    let mut len = [0u8; 4];
    match rx.read_exact(&mut len).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > 64 << 20 {
        bail!("bad message length {len}");
    }
    let mut buf = vec![0u8; len];
    rx.read_exact(&mut buf).await?;
    let ty = buf.remove(0);
    Ok(Some((ty, buf)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_and_reader_agree() {
        let m = W::new(t::BLIND_SHOW).u32(7).str("Größe").u16(2).f32(1.5).u64(u64::MAX).bytes(&[9, 8]).u8(1).finish();
        assert_eq!(u32::from_le_bytes(m[..4].try_into().unwrap()) as usize, m.len() - 4);
        assert_eq!(m[4], t::BLIND_SHOW);
        let mut r = R::new(&m[5..]);
        assert_eq!(r.u32().unwrap(), 7);
        assert_eq!(r.str().unwrap(), "Größe");
        assert_eq!(r.u16().unwrap(), 2);
        assert_eq!(r.f32().unwrap(), 1.5);
        assert_eq!(r.u64().unwrap(), u64::MAX);
        assert_eq!(r.bytes().unwrap(), &[9, 8]);
        assert_eq!(r.u8().unwrap(), 1);
        assert!(r.u8().is_err(), "reading past the end fails");
    }
}
