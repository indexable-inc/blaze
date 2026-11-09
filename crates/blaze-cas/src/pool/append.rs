//! The pool's write side: one file's append in a commit (docs/v2/plan/design/store-pool.md,
//! section 3, "Drain" to "Publish"). A commit's new bytes go straight into their file's window, at
//! the offsets they will have on disk, and are written from there ([`blaze_core::Effect::WriteFrom`]
//! of [`Append::parts`]): the pages are the write buffer, and once the commit is durable they are
//! the file's loaded pages, with no read and no second mapping.
//!
//! What keeps a commit's pages its own while it writes them:
//!
//! - **The last committed page**, when the committed bytes end inside it, is pinned first
//!   ([`Append::tail_of`], a pinned read like any other, which loads it if it was evicted). It
//!   may hold records being read: the commit writes only bytes past the committed length, which
//!   no reader's run reaches, with atomic stores. The pin keeps the sweep from giving the page
//!   back in the middle of the write. Without it the write lands whatever the kernel left there:
//!   planted bug 8 ([`Plant::UnpinnedTail`]).
//! - **Pages past it** no file commits yet, so the sweep does not visit them and no reader can
//!   ask for them. The append takes each `Unloaded -> Loaded` with its own pin
//!   ([`Pool::fresh`]).
//!
//! An append that is not committed (a write or the sync failed, or its commit was given up) gives
//! its fresh pages back as it is dropped; the next commit takes and writes them again.
//!
//! This is the pool's half. The order of a commit's effects (truncate, write, sync, the head) is
//! the store's, and its proof the store's crash tests.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::{
    marker::PhantomData,
    ops::Deref,
    sync::atomic::Ordering,
};

use blaze_core::Span;

use super::{BLOCK, Give, PAGE, Pins, Plant, Pool, PoolError, Run, WINDOW, Window, Windows, Word, block_check, loads};

/// The most one write of an append writes: a span's length is 32 bits.
pub(crate) const WRITE_MOST: u64 = 64 << 20;

const _: () = assert!(WRITE_MOST % BLOCK == 0 && WRITE_MOST <= u32::MAX as u64);

/// One file's append in a commit: the pins of the pages it writes, and how far it has written.
pub(crate) struct Append<G: Give, P: Deref<Target = Pool<G>> + Clone> {
    pool: P,
    /// The file as committed when the append began.
    window: Window,
    /// The file's byte it has written up to: the committed length, until a put.
    end: u64,
    /// The pin of the last committed page, when the committed bytes end inside it.
    tail: Option<Pins<G, P>>,
    /// Pages past the committed ones it took, from the first: each holds its pin.
    fresh: u32,
    give: PhantomData<fn() -> G>,
}

impl<G: Give, P: Deref<Target = Pool<G>> + Clone> Append<G, P> {
    /// What a commit pins before it appends to `window`'s file: the last committed byte, when
    /// the committed bytes end inside a page (the pool's byte, and a length of one). `None`
    /// when they end at a page's edge, or the file is empty: every page it writes is fresh.
    pub(crate) fn tail_of(window: &Window) -> Option<(u64, u64)> {
        (window.len % PAGE != 0).then(|| (window.at(window.len - 1), 1))
    }

    /// An append to `window`'s file begun, `tail` the pin [`Append::tail_of`] asks for.
    pub(crate) fn begin(pool: P, window: Window, tail: Option<Pins<G, P>>) -> Append<G, P> {
        assert!(
            window.len % BLOCK == 0,
            "an append to {:?}, which commits {} bytes: not whole blocks",
            window.id,
            window.len
        );
        let want = Append::<G, P>::tail_of(&window).map(|(at, len)| Run::of(at, len));
        assert_eq!(
            tail.as_ref().map(Pins::run),
            want,
            "an append to {:?} begun with the wrong pin of its last committed page",
            window.id
        );
        let tail = match pool.planted(Plant::UnpinnedTail) {
            // The pin let go before a byte is written: the sweep may give the page back under
            // the write.
            true => {
                drop(tail);
                None
            }
            false => tail,
        };
        Append { pool, window, end: window.len, tail, fresh: 0, give: PhantomData }
    }

    /// The file as committed when the append began.
    pub(crate) fn window(&self) -> Window {
        self.window
    }

    /// The file's byte it has written up to.
    pub(crate) fn end(&self) -> u64 {
        self.end
    }

    /// Pages it holds a pin of.
    pub(crate) fn pinned(&self) -> u64 {
        self.tail.as_ref().map_or(0, |it| u64::from(it.run().pages)) + u64::from(self.fresh)
    }

    /// The first page past the file's committed ones.
    fn first_fresh(&self) -> u64 {
        self.window.first_page() + self.window.pages()
    }

    /// `bytes` written after what it has written so far, taking the pages they reach.
    ///
    /// # Errors
    ///
    /// A page past the committed ones could not be taken ([`PoolError::Reuse`],
    /// [`PoolError::NotFresh`]): nothing of `bytes` is written, and the append is as it was but
    /// for the pages it took before that one, which it gives back when dropped.
    #[allow(unsafe_code, reason = "atomic stores into pages the append pins")]
    pub(crate) fn put(&mut self, bytes: &[u8]) -> Result<(), PoolError> {
        let room = u64::from(self.window.count) * WINDOW;
        assert!(
            self.end + bytes.len() as u64 <= room,
            "an append of {} bytes at {} of {:?}, whose windows hold {room}: the store begins another pack first",
            bytes.len(),
            self.end,
            self.window.id
        );
        if bytes.is_empty() {
            return Ok(());
        }
        // The file's last page the bytes reach; every page before `committed` is the file's
        // already (the last of them pinned as the tail, when the bytes begin in it).
        let last = (self.end + bytes.len() as u64 - 1) / PAGE;
        let committed = self.window.pages();
        while committed + u64::from(self.fresh) <= last {
            self.pool.fresh(self.first_fresh() + u64::from(self.fresh))?;
            self.fresh += 1;
        }
        // SAFETY: every page the bytes touch is pinned by this append (the tail's guard, or a
        // fresh page's pin), so none is given back or loaded under the write; and every byte is
        // past the file's committed length, which no reader's run reaches (`Window::run`
        // refuses one), so no reference covers them.
        unsafe { self.pool.copy_in(self.window.at(self.end), bytes) };
        self.end += bytes.len() as u64;
        Ok(())
    }

    /// Zeros up to the next whole block: how a commit ends each file's append, so a block never
    /// changes once it is committed.
    pub(crate) fn pad(&mut self) -> Result<(), PoolError> {
        const ZEROS: [u8; BLOCK as usize] = [0; BLOCK as usize];
        let short = (self.end.next_multiple_of(BLOCK) - self.end) as usize;
        self.put(&ZEROS[..short])
    }

    /// The check of each block it wrote, in order: appended to the file's checks before the
    /// head names the new length, and told to the loads core as the append is committed
    /// ([`super::Loads::grow`]).
    pub(crate) fn checks(&self) -> Vec<u32> {
        assert!(self.end % BLOCK == 0, "the checks of an append to {:?} not padded to a block", self.window.id);
        checks_in(&*self.pool, self.window.at(self.window.len), (self.end - self.window.len) / BLOCK)
    }

    /// Its bytes as writes: for each, the file's offset and the span to write from, whole blocks
    /// of at most [`WRITE_MOST`]. The spans' pages are pinned until the append is committed or
    /// dropped, which upholds the contract of [`blaze_core::Effect::WriteFrom`]: drop it only
    /// once every write of it has completed.
    pub(crate) fn parts(&self) -> Vec<(u64, Span)> {
        assert!(self.end % BLOCK == 0, "the writes of an append to {:?} not padded to a block", self.window.id);
        let mut parts = Vec::new(); // alloc: a commit's writes, a few a file
        let mut at = self.window.len;
        while at < self.end {
            let len = (self.end - at).min(WRITE_MOST);
            parts.push((at, self.pool.span_at(self.window.at(at), len as u32)));
            at += len;
        }
        parts
    }

    /// Dropped with the process's memory as a run of the DST ends: its pins gone with the pool,
    /// and nothing given back.
    #[cfg(test)]
    pub(crate) fn lost(mut self) {
        self.fresh = 0;
        if let Some(tail) = self.tail.take() {
            tail.lost();
        }
    }

    /// The append made the file's: called once its writes and their sync have completed and the
    /// head names the new length. The file's window as it is now. Its pages stay `Loaded`, their
    /// second-chance bit set (what was just made is the likeliest thing to be read next), and are
    /// the sweep's to visit from now on.
    ///
    /// The caller tells the loads core ([`super::Loads::grow`], with [`Append::checks`]) before
    /// any reader can learn the new length, so a load of a new page finds its checks.
    pub(crate) fn commit(mut self, windows: &mut Windows) -> Window {
        let window = windows.grow(self.window.id, self.end);
        let first = self.first_fresh();
        for page in first..first + u64::from(self.fresh) {
            self.pool.unpin(page);
        }
        self.fresh = 0;
        window
    }
}

impl<G: Give, P: Deref<Target = Pool<G>> + Clone> Drop for Append<G, P> {
    /// Not committed: the pages it took past the committed ones are given back. (A committed
    /// append holds none.) The last committed page's pin goes with its guard.
    fn drop(&mut self) {
        if self.fresh == 0 {
            return;
        }
        let pool: &Pool<G> = &self.pool;
        let first = self.first_fresh();
        let pages: Vec<u64> = (first..first + u64::from(self.fresh)).collect(); // alloc: an append given up, rare
        for &page in &pages {
            // Its one pin is this append's: no file commits the page, so no reader reached it.
            let word = pool.word_now(page);
            pool.word(page).store(Word::evicting(word.version()).0, Ordering::Release);
        }
        if !loads::give_back(pool, &pages, |_| Word::unloaded) {
            // The kernel kept them: memory, counted, not correctness (the next append that takes
            // them writes them whole).
            pool.count_refused();
        }
        self.fresh = 0;
    }
}

/// The checks of `blocks` blocks from the pool's byte `at`, which the caller pins.
#[expect(
    blaze::sans_io_core,
    reason = "the CRC-32C is a pure function of the bytes; the syscall is std's one-time CPU feature detection (sysctl on macOS), read once and cached, never IO the core decides on"
)]
fn checks_in<G: Give>(pool: &Pool<G>, at: u64, blocks: u64) -> Vec<u32> {
    (0..blocks)
        .map(|b| {
            // SAFETY: the blocks are of pages an append pins and has written; nothing else
            // writes them (no file commits them yet, so no load reads into them).
            #[allow(unsafe_code, reason = "an append's own bytes, checked")]
            let bytes = unsafe { pool.bytes(at + b * BLOCK, BLOCK) };
            block_check(bytes)
        })
        .collect() // alloc: a commit's new checks, once a file a commit
}
