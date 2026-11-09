//! A menu's one highlight (prims.flecs `ui.MenuItem`, which lights where `Lit` is, not under the pointer): the pointer
//! and the arrow keys move it together, NSMenu's way. The pointer coming onto an item highlights it; leaving the item it
//! highlighted takes the highlight off; a pointer that stays put leaves a highlight the keys moved alone. The arrow keys
//! step among the items that can be chosen, round from the last to the first. [`Nav`] is the state, sans IO; the owner
//! tells it where the pointer is each frame and puts `Lit` where it says ([`light`]).
use harness_native_api::Lit;
use harness_native_flecs::prelude::*;

/// Which item of a menu is highlighted, and which the pointer was last on (indices into the menu's rows).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Nav {
    pub hot: Option<usize>,
    pointer: Option<usize>,
}

impl Nav {
    /// The row under the pointer this frame (None when it is on none).
    pub fn pointer(&mut self, on: Option<usize>) {
        if on == self.pointer {
            return;
        }
        match on {
            Some(i) => self.hot = Some(i),
            None if self.hot == self.pointer => self.hot = None,
            None => {}
        }
        self.pointer = on;
    }

    /// The row the pointer was last on.
    #[must_use]
    pub fn pointer_at(&self) -> Option<usize> {
        self.pointer
    }

    /// The rows were rebuilt: the highlight and the pointer's row are now at `hot` and `pointer` (None when the row they
    /// were on is gone). Indices alone would leave them on whatever row slid into the place.
    pub fn retarget(&mut self, hot: Option<usize>, pointer: Option<usize>) {
        self.hot = hot;
        self.pointer = pointer;
    }

    /// An arrow key: the highlight steps `down` (or up) to the next of the `n` rows that `choosable` says can be chosen,
    /// round past the end; from none, to the first (or the last).
    pub fn step(&mut self, down: bool, n: usize, choosable: impl Fn(usize) -> bool) {
        if n == 0 {
            return;
        }
        let mut at = self.hot.filter(|&h| h < n);
        for _ in 0..n {
            let next = match (at, down) {
                (None, true) => 0,
                (None, false) => n - 1,
                (Some(i), true) => (i + 1) % n,
                (Some(i), false) => (i + n - 1) % n,
            };
            at = Some(next);
            if choosable(next) {
                self.hot = Some(next);
                return;
            }
        }
    }
}

/// Keep `Lit` on row `e` exactly while it is the highlighted one (written only on change).
pub fn light(e: EntityView<'_>, hot: bool) {
    match (hot, e.has(Lit::id())) {
        (true, false) => {
            e.add(Lit::id());
        }
        (false, true) => {
            e.remove(Lit::id());
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rebuilt_menu_keeps_the_highlight_on_its_row() {
        let mut n = Nav::default();
        n.pointer(Some(4));
        n.retarget(Some(2), Some(2));
        assert_eq!((n.hot, n.pointer_at()), (Some(2), Some(2)));
        n.retarget(None, None);
        assert_eq!(n.hot, None, "a row that is gone takes the highlight with it");
    }

    #[test]
    fn the_pointer_and_the_keys_share_one_highlight() {
        let mut n = Nav::default();
        let ok = |i: usize| i != 0 && i != 3; // a head at 0, a separator at 3
        n.step(true, 6, ok);
        assert_eq!(n.hot, Some(1), "down from none: the first item");
        n.pointer(Some(4));
        assert_eq!(n.hot, Some(4), "the pointer comes onto an item");
        n.step(true, 6, ok);
        assert_eq!(n.hot, Some(5));
        n.pointer(Some(4));
        assert_eq!(n.hot, Some(5), "a pointer that stays put leaves the keys' highlight");
        n.step(true, 6, ok);
        assert_eq!(n.hot, Some(1), "round past the end, over the head");
        n.step(false, 6, ok);
        assert_eq!(n.hot, Some(5));
        n.pointer(Some(2));
        n.pointer(None);
        assert_eq!(n.hot, None, "leaving the item it highlighted takes the highlight off");
    }
}
