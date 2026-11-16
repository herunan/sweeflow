//! Focus while windows arrive after (re)connecting.
//!
//! As the Mac's windows come up here, the desktop gives each one the focus in turn. Forwarded,
//! each came to the front on the Mac, and the last one shown ended up the Mac's key window,
//! not the one the user left there. What follows the key window on the Mac followed it too:
//! Finder's Quick Look showed the other window's file and moved over to it. So until the user
//! does something here, a focus is passed on only for the window the Mac has in front, and
//! that one is focused here as the Mac says which it is. Held back, the focus is passed on at
//! the user's first press (a key or a click), so what they act on is the Mac's key window then.

use std::time::{Duration, Instant};

/// How long after the last window arrived the desktop's focusing it counts as its arriving.
const SETTLE: Duration = Duration::from_millis(2000);

#[derive(Default)]
pub struct Arrival {
    /// Arriving until then, and the user hasn't done anything yet.
    until: Option<Instant>,
    /// The Mac's key window, as it last said.
    mac_key: Option<u32>,
    /// A focus not passed on.
    held: Option<u32>,
}

impl Arrival {
    pub fn connected(&mut self) {
        *self = Self { until: Some(Instant::now() + SETTLE), ..Self::default() };
    }

    pub fn window_added(&mut self) {
        if self.until.is_some() {
            self.until = Some(Instant::now() + SETTLE);
        }
    }

    fn arriving(&mut self) -> bool {
        if self.until.is_some_and(|u| Instant::now() > u) {
            self.until = None;
        }
        self.until.is_some()
    }

    /// The desktop focused `wid`: whether to tell the Mac. If not, the Mac's own key window
    /// is returned, to be focused here instead.
    pub fn focus(&mut self, wid: u32) -> Result<(), Option<u32>> {
        if !self.arriving() || self.mac_key == Some(wid) {
            self.held = None;
            return Ok(());
        }
        log::info!("window {wid} focused as it arrived: kept from the Mac (its key window is {:?})", self.mac_key);
        self.held = Some(wid);
        Err(self.mac_key.filter(|&k| k != wid))
    }

    /// The Mac says its key window is `wid`: whether to focus it here (windows are arriving).
    pub fn mac_focused(&mut self, wid: u32) -> bool {
        self.mac_key = Some(wid);
        let arriving = self.arriving();
        if arriving {
            log::info!("the Mac's key window is {wid}");
        }
        arriving
    }

    /// Mac window `old` was replaced by `new` (WINDOW_REPLACED): the same window here.
    pub fn replaced(&mut self, old: u32, new: u32) {
        for slot in [&mut self.mac_key, &mut self.held] {
            crate::ui::follow_wid(slot.as_mut(), old, new);
        }
    }

    /// The user pressed a key or a button: arriving is over. A focus held back, if it's still
    /// where the focus is, is passed on now (returned).
    pub fn input(&mut self, focus: Option<u32>) -> Option<u32> {
        self.until = None;
        self.held.take().filter(|&h| Some(h) == focus)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arriving_focus_is_held_until_input() {
        let mut a = Arrival::default();
        assert_eq!(a.focus(1), Ok(()), "not connected: passed on");
        a.connected();
        a.window_added();
        assert_eq!(a.focus(1), Err(None), "the Mac hasn't said yet");
        assert!(a.mac_focused(2));
        assert_eq!(a.focus(3), Err(Some(2)));
        assert_eq!(a.focus(2), Ok(()), "the Mac's key window: passed on");
        assert_eq!(a.input(Some(2)), None, "nothing held");
        assert_eq!(a.focus(3), Ok(()), "after input: passed on");
    }

    #[test]
    fn held_focus_passed_on_at_first_input() {
        let mut a = Arrival::default();
        a.connected();
        a.mac_focused(2);
        assert_eq!(a.focus(3), Err(Some(2)));
        assert_eq!(a.input(Some(3)), Some(3));
        assert!(!a.mac_focused(2), "arriving over");
    }

    #[test]
    fn a_replaced_window_keeps_its_place() {
        let mut a = Arrival::default();
        a.connected();
        a.mac_focused(2);
        assert_eq!(a.focus(3), Err(Some(2)));
        a.replaced(3, 4);
        a.replaced(2, 5);
        assert_eq!(a.input(Some(4)), Some(4), "the held focus follows its window");
        assert_eq!(a.mac_key, Some(5));
    }
}
