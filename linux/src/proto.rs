//! Wire format shared with the Mac host; see PROTOCOL.md.

use anyhow::{bail, Result};

pub const VERSION: u16 = 1;

pub mod t {
    pub const HELLO: u8 = 0x01;
    pub const WELCOME: u8 = 0x02;
    pub const WINDOW_ADDED: u8 = 0x10;
    pub const WINDOW_CHANGED: u8 = 0x11;
    pub const WINDOW_REMOVED: u8 = 0x12;
    pub const APP_ICON: u8 = 0x13;
    pub const CURSOR: u8 = 0x14;
    pub const FOCUSED: u8 = 0x15;
    pub const WINDOW_BUTTONS: u8 = 0x16;
    pub const MOVE_STARTED: u8 = 0x17;
    pub const TOGGLE_MAXIMIZE: u8 = 0x18;
    pub const RESIZE: u8 = 0x20;
    pub const FOCUS: u8 = 0x21;
    pub const CLOSE: u8 = 0x22;
    pub const SCREEN_SIZE: u8 = 0x23;
    pub const POINTER_MOVE: u8 = 0x30;
    pub const POINTER_BUTTON: u8 = 0x31;
    pub const SCROLL: u8 = 0x32;
    pub const KEY: u8 = 0x33;
    pub const RELEASE_ALL: u8 = 0x34;
    pub const CLIPBOARD: u8 = 0x40;
    pub const PING: u8 = 0x41;
    pub const PONG: u8 = 0x42;
    pub const SUBSCRIBE: u8 = 0x50;
    pub const FRAME: u8 = 0x51;
    pub const REQUEST_KEYFRAME: u8 = 0x52;
    pub const PAUSE: u8 = 0x53;
    pub const FRAME_ACK: u8 = 0x54;
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
