//! What a test of a fork server read, as [`Trace::traced`] has it, with the lines every test
//! of the server shares hashed once. A server's results share their lines of every scope (its
//! libraries, the runtime's reads) in one buffer until a new one comes ([`Lines::common_is`]):
//! their [`Traced`] is assembled once per buffer and kept, and each result assembles only its
//! own lines and lends the kept one with them spliced in, taking them out after. The shared
//! lines are reads the server made before it forked the test, so their values as the first
//! result of the buffer saw them are what each later test inherited.
//!
//! Splicing is one pass from the back over the entries past the first own one, and taking
//! them out one pass forward: no allocation once the kept lists have grown to a result's size.

use std::cmp::Ordering;

use blaze_core::{Traced, Untraceable};
use blaze_usercode::{
    test::{Lines, Trace},
    trace::{self, Inputs},
};

/// The shared part of one server's results, assembled once.
#[derive(Default)]
pub(super) struct Common {
    /// A result whose shared buffer `traced` is of: holding it keeps that buffer alive, so no
    /// other buffer can be taken for it.
    held: Option<Lines>,
    /// `held`'s shared lines assembled; a result's own spliced in while it is lent.
    traced: Traced,
    splice: Splice,
}

impl Common {
    /// `with` called with what `trace` read, hashed as `inputs` has it now, or `None` when not
    /// known: the same [`Traced`] as [`Trace::traced`].
    pub(super) fn traced<I: Inputs, R>(
        &mut self,
        trace: Trace,
        inputs: &mut I,
        with: impl FnOnce(Option<&Traced>) -> R,
    ) -> Result<R, I::Error> {
        let lines = match trace {
            Trace::Lines(lines) => lines,
            Trace::Whole(traced) => return Ok(with(Some(&traced))),
            Trace::Unknown(_) => return Ok(with(None)),
        };
        let shared = lines.common();
        let kept = self.held.as_ref().is_some_and(|it| it.common_is(&lines));
        if !kept {
            self.held = None;
            self.traced = trace::assemble_seen(lines.iter().take(shared), inputs)?;
        }
        let own = trace::assemble_seen(lines.iter().skip(shared), inputs)?;
        if !kept {
            self.held = Some(lines);
        }
        Ok(self.lend(own, with))
    }

    fn lend<R>(&mut self, own: Traced, with: impl FnOnce(Option<&Traced>) -> R) -> R {
        self.splice.put(&mut self.traced, own);
        let out = with(Some(&self.traced));
        self.splice.undo(&mut self.traced);
        out
    }
}

/// What [`Splice::put`] put into a [`Traced`], for [`Splice::undo`] to take out: which places
/// of each list hold an entry of the own part, and which files of the shared part it found.
#[derive(Default)]
pub(super) struct Splice {
    files: Vec<bool>,
    dirs: Vec<bool>,
    env: Vec<bool>,
    untraceable: Vec<Untraceable>,
    /// Files the shared part looked for and did not find (a listing of nothing) that the own
    /// part read: a read's value stands, as one assembly of both gives it.
    found: Vec<u32>,
}

impl Splice {
    /// `own`'s entries put into `base`, both as [`trace::assemble_seen`] makes them: `base` is
    /// then what one assembly of both parts' lines makes.
    pub(super) fn put(&mut self, base: &mut Traced, own: Traced) {
        let Traced { files, dirs, env, untraceable } = own;
        let found = &mut self.found;
        found.clear();
        merge(&mut base.files, files, &mut self.files, |it| &it.0, |at, kept, given| {
            if kept.1.is_none() && given.1.is_some() {
                kept.1 = given.1;
                found.push(at as u32);
            }
        });
        merge(&mut base.dirs, dirs, &mut self.dirs, |it| &it.0, |_, _, _| {});
        merge(&mut base.env, env, &mut self.env, |it| &it.0, |_, _, _| {});
        self.untraceable.clear();
        for it in untraceable {
            if let Err(at) = base.untraceable.binary_search(&it) {
                base.untraceable.insert(at, it);
                self.untraceable.push(it);
            }
        }
    }

    /// `base` as it was before [`Splice::put`].
    pub(super) fn undo(&mut self, base: &mut Traced) {
        unmerge(&mut base.files, &mut self.files);
        unmerge(&mut base.dirs, &mut self.dirs);
        unmerge(&mut base.env, &mut self.env);
        for &at in &self.found {
            base.files[at as usize].1 = None;
        }
        self.found.clear();
        base.untraceable.retain(|it| !self.untraceable.contains(it));
        self.untraceable.clear();
    }
}

/// `own` merged into `base`, both sorted by `key` with each key once. An entry of a key `base`
/// has is not added: `both` is told of it, with its place in `base`. `ours` marks the places
/// that then hold an entry of `own`.
fn merge<T: Default, K: Ord + ?Sized>(
    base: &mut Vec<T>,
    mut own: Vec<T>,
    ours: &mut Vec<bool>,
    key: impl Fn(&T) -> &K,
    mut both: impl FnMut(usize, &mut T, &T),
) {
    ours.clear();
    own.retain(|given| match base.binary_search_by(|it| key(it).cmp(key(given))) {
        Ok(at) => {
            both(at, &mut base[at], given);
            false
        }
        Err(_) => true,
    });
    if own.is_empty() {
        return;
    }
    let (kept, added) = (base.len(), own.len());
    base.resize_with(kept + added, T::default);
    ours.resize(kept + added, false);
    // `base[i..w]` are empty places, and `w - i` is how many of `own` are left: each step fills
    // `w - 1` from whichever list's last entry is the greater.
    let (mut i, mut j, mut w) = (kept, added, kept + added);
    while j > 0 {
        w -= 1;
        if i > 0 && key(&base[i - 1]).cmp(key(&own[j - 1])) == Ordering::Greater {
            base.swap(i - 1, w);
            i -= 1;
        } else {
            j -= 1;
            base[w] = std::mem::take(&mut own[j]);
            ours[w] = true;
        }
    }
}

/// What [`merge`] added to `base`, taken out, in one pass.
fn unmerge<T>(base: &mut Vec<T>, ours: &mut Vec<bool>) {
    if ours.is_empty() {
        return;
    }
    let mut at = 0;
    base.retain(|_| {
        let keep = !ours[at];
        at += 1;
        keep
    });
    ours.clear();
}
