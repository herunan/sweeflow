//! Video frames as they arrive: on the window's stream (keyframes, popups' lossless frames) or
//! as QUIC datagrams (delta frames, cut into pieces). `Reassembler` puts the pieces back
//! together; `Orderer` hands frames to the decoder in sequence, skipping past one that was lost
//! (and asking for a keyframe, as the frames after it can't be decoded without it).

use std::{
    collections::{BTreeMap, HashMap},
    time::{Duration, Instant},
};

use anyhow::{bail, Result};

use crate::proto::{t, R};

pub struct Encoded {
    pub capture_us: u64,
    pub w: u16,
    pub h: u16,
    pub key: bool,
    pub lossless: bool,
    pub dark: bool,
    /// Pixels per point of this frame (2 for Retina captures).
    pub px_per_pt: u8,
    pub full_range: bool,
    pub data: Vec<u8>,
    /// HEVC frames' sequence number (older hosts and lossless frames have none).
    pub seq: Option<u32>,
}

/// A FRAME message's body.
pub fn parse_frame(body: &[u8]) -> Result<Encoded> {
    let mut r = R::new(body);
    let capture_us = r.u64()?;
    let w = r.u16()?;
    let h = r.u16()?;
    let flags = r.u8()?;
    let data = r.bytes()?.to_vec();
    Ok(Encoded {
        capture_us,
        w,
        h,
        key: flags & 1 != 0,
        lossless: flags & 2 != 0,
        dark: flags & 4 != 0,
        px_per_pt: flags >> 4,
        full_range: flags & 8 != 0,
        data,
        seq: r.u32().ok(),
    })
}

/// Datagram header: u32 window, u32 frame sequence, u16 piece index, u16 piece count.
const HEADER: usize = 12;
/// Pieces of a frame that hasn't completed in this long were lost.
const PARTIAL_TTL: Duration = Duration::from_millis(300);

struct Partial {
    pieces: Vec<Option<Vec<u8>>>,
    got: usize,
    since: Instant,
}

#[derive(Default)]
pub struct Reassembler {
    partial: HashMap<(u32, u32), Partial>,
    /// Frames completed, their pieces, and the time from first to last piece (latency figures).
    pub frames: u32,
    pub pieces: u32,
    pub spread_us: u64,
    pub worst_us: u64,
}

impl Reassembler {
    /// Takes one datagram; returns (window, frame body) once all of a frame's pieces are in.
    pub fn push(&mut self, d: &[u8], now: Instant) -> Option<(u32, Result<Encoded>)> {
        self.partial.retain(|_, p| now.duration_since(p.since) < PARTIAL_TTL);
        if d.len() < HEADER {
            return None;
        }
        let wid = u32::from_le_bytes(d[0..4].try_into().unwrap());
        let seq = u32::from_le_bytes(d[4..8].try_into().unwrap());
        let idx = u16::from_le_bytes(d[8..10].try_into().unwrap()) as usize;
        let count = u16::from_le_bytes(d[10..12].try_into().unwrap()) as usize;
        if count == 0 || idx >= count {
            return None;
        }
        let p = self.partial.entry((wid, seq)).or_insert_with(|| Partial { pieces: vec![None; count], got: 0, since: now });
        if p.pieces.len() != count || p.pieces[idx].is_some() {
            return None;
        }
        p.pieces[idx] = Some(d[HEADER..].to_vec());
        p.got += 1;
        if p.got < count {
            return None;
        }
        let p = self.partial.remove(&(wid, seq)).unwrap();
        let spread = now.duration_since(p.since).as_micros() as u64;
        self.frames += 1;
        self.pieces += count as u32;
        self.spread_us += spread;
        self.worst_us = self.worst_us.max(spread);
        let msg: Vec<u8> = p.pieces.into_iter().flatten().flatten().collect();
        Some((wid, frame_body(&msg).and_then(parse_frame)))
    }
}

/// The body of a framed message (u32 length, u8 type, body), which must be a FRAME.
fn frame_body(msg: &[u8]) -> Result<&[u8]> {
    if msg.len() < 5 || u32::from_le_bytes(msg[..4].try_into().unwrap()) as usize != msg.len() - 4 || msg[4] != t::FRAME {
        bail!("not a frame message");
    }
    Ok(&msg[5..])
}

/// How long a missing frame is waited for before it's taken as lost.
const GAP_WAIT: Duration = Duration::from_millis(40);
/// At most one keyframe request this often (one is enough to recover).
const KEY_REQUEST_EVERY: Duration = Duration::from_millis(250);

/// Puts a window's HEVC frames back in order.
#[derive(Default)]
pub struct Orderer {
    /// The next frame to decode; none until a keyframe starts (or restarts) the sequence.
    next: Option<u32>,
    pending: BTreeMap<u32, Encoded>,
    gap_since: Option<Instant>,
    asked_key: Option<Instant>,
    pub lost: u32,
}

pub struct Ready {
    pub frames: Vec<Encoded>,
    pub request_key: bool,
}

impl Orderer {
    pub fn push(&mut self, f: Encoded, now: Instant) -> Ready {
        let Some(seq) = f.seq else {
            return Ready { frames: vec![f], request_key: false };
        };
        if self.next.is_some_and(|n| seq.wrapping_sub(n) > u32::MAX / 2) {
            return Ready { frames: Vec::new(), request_key: false }; // late: already skipped
        }
        self.pending.insert(seq, f);
        self.drain(now)
    }

    /// Called now and then with no new frame: gives up on a missing one after a while.
    pub fn tick(&mut self, now: Instant) -> Ready {
        self.drain(now)
    }

    fn drain(&mut self, now: Instant) -> Ready {
        let mut out = Vec::new();
        loop {
            // A keyframe in hand beats waiting: everything before it is obsolete.
            let first_key = self.pending.iter().find(|(_, f)| f.key).map(|(s, _)| *s);
            match self.next {
                None => {
                    let Some(k) = first_key else { break };
                    self.pending.retain(|s, _| *s >= k);
                    self.next = Some(k);
                }
                Some(n) if !self.pending.contains_key(&n) => {
                    match first_key {
                        Some(k) => {
                            self.lost += k.wrapping_sub(n);
                            self.pending.retain(|s, _| *s >= k);
                            self.next = Some(k);
                        }
                        None => break,
                    }
                }
                Some(_) => {}
            }
            let n = self.next.unwrap();
            if let Some(f) = self.pending.remove(&n) {
                out.push(f);
                self.next = Some(n.wrapping_add(1));
                self.gap_since = None;
            }
        }
        let mut request_key = false;
        if self.pending.is_empty() {
            self.gap_since = None;
        } else {
            let since = *self.gap_since.get_or_insert(now);
            if now.duration_since(since) >= GAP_WAIT {
                // Lost: what's waiting can't be decoded without it. Start again from a keyframe.
                if let Some(n) = self.next {
                    self.lost += 1;
                    log::debug!("frame {n} lost; waiting for a keyframe");
                }
                self.pending.clear();
                self.next = None;
                self.gap_since = None;
                if self.asked_key.is_none_or(|t| now.duration_since(t) >= KEY_REQUEST_EVERY) {
                    self.asked_key = Some(now);
                    request_key = true;
                }
            }
        }
        Ready { frames: out, request_key }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::W;

    fn frame(seq: u32, key: bool) -> Encoded {
        Encoded { capture_us: seq as u64, w: 4, h: 4, key, lossless: false, dark: false, px_per_pt: 2, full_range: false, data: vec![seq as u8; 3000], seq: Some(seq) }
    }

    fn message(f: &Encoded) -> Vec<u8> {
        let flags = f.key as u8 | (f.px_per_pt << 4);
        W::new(t::FRAME).u64(f.capture_us).u16(f.w).u16(f.h).u8(flags).bytes(&f.data).u32(f.seq.unwrap()).finish()
    }

    /// Cuts a message the way the Mac does (Datagrams.swift).
    fn split(msg: &[u8], wid: u32, seq: u32) -> Vec<Vec<u8>> {
        let chunks: Vec<&[u8]> = msg.chunks(1100).collect();
        chunks
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut d = Vec::new();
                d.extend_from_slice(&wid.to_le_bytes());
                d.extend_from_slice(&seq.to_le_bytes());
                d.extend_from_slice(&(i as u16).to_le_bytes());
                d.extend_from_slice(&(chunks.len() as u16).to_le_bytes());
                d.extend_from_slice(c);
                d
            })
            .collect()
    }

    fn seqs(r: &Ready) -> Vec<u32> {
        r.frames.iter().map(|f| f.seq.unwrap()).collect()
    }

    #[test]
    fn pieces_in_any_order_make_the_frame() {
        let f = frame(5, false);
        let mut pieces = split(&message(&f), 9, 5);
        assert_eq!(pieces.len(), 3);
        pieces.swap(0, 2);
        let mut r = Reassembler::default();
        let now = Instant::now();
        assert!(r.push(&pieces[0], now).is_none());
        assert!(r.push(&pieces[1], now).is_none());
        let (wid, got) = r.push(&pieces[2], now).unwrap();
        let got = got.unwrap();
        assert_eq!((wid, got.seq, got.data.len()), (9, Some(5), 3000));
    }

    #[test]
    fn a_frame_missing_a_piece_is_forgotten() {
        let pieces = split(&message(&frame(1, false)), 9, 1);
        let mut r = Reassembler::default();
        let t0 = Instant::now();
        r.push(&pieces[0], t0);
        // The rest come too late: it was dropped meanwhile, so it doesn't complete.
        let later = t0 + Duration::from_millis(400);
        assert!(r.push(&pieces[1], later).is_none());
        assert!(r.push(&pieces[2], later).is_none());
        assert!(r.partial.len() <= 1);
    }

    #[test]
    fn frames_come_out_in_order() {
        let mut o = Orderer::default();
        let now = Instant::now();
        assert_eq!(seqs(&o.push(frame(10, true), now)), [10]);
        assert!(o.push(frame(12, false), now).frames.is_empty());
        assert_eq!(seqs(&o.push(frame(11, false), now)), [11, 12]);
    }

    #[test]
    fn deltas_before_the_first_keyframe_wait_for_it() {
        // The keyframe comes on the stream, its successors as datagrams, which can overtake it.
        let mut o = Orderer::default();
        let now = Instant::now();
        assert!(o.push(frame(2, false), now).frames.is_empty());
        assert_eq!(seqs(&o.push(frame(1, true), now)), [1, 2]);
    }

    #[test]
    fn a_lost_frame_asks_for_a_keyframe_and_skips_to_it() {
        let mut o = Orderer::default();
        let t0 = Instant::now();
        o.push(frame(1, true), t0);
        assert!(o.push(frame(3, false), t0).frames.is_empty()); // 2 is missing
        let r = o.tick(t0 + Duration::from_millis(50));
        assert!(r.request_key && r.frames.is_empty());
        assert_eq!(o.lost, 1);
        // Late arrivals of the old run are dropped; the keyframe restarts the sequence.
        assert!(o.push(frame(4, false), t0 + Duration::from_millis(60)).frames.is_empty());
        assert_eq!(seqs(&o.push(frame(5, true), t0 + Duration::from_millis(70))), [5]);
    }

    #[test]
    fn a_keyframe_already_in_hand_skips_the_gap_at_once() {
        let mut o = Orderer::default();
        let now = Instant::now();
        o.push(frame(1, true), now);
        let r = o.push(frame(4, true), now);
        assert_eq!(seqs(&r), [4]);
        assert!(!r.request_key);
    }

    #[test]
    fn frames_without_a_sequence_pass_straight_through() {
        let mut o = Orderer::default();
        let mut f = frame(0, false);
        f.seq = None;
        assert_eq!(o.push(f, Instant::now()).frames.len(), 1);
    }
}
