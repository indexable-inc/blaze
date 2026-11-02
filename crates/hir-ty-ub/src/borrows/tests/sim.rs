//! A small driver over [`Borrows`] in the terms of a Rust program: allocations, pointers with
//! an offset, references made from them, reads, writes, calls and returns. The ported Miri
//! programs (`tests/fail/tree_borrows`, `tests/pass/tree_borrows`) are written against it,
//! doing the same retags and accesses Miri does for the program.
//!
//! Every operation gets a fresh `SiteId` (`Sim::last_site` names it), so a test can check
//! where an error's history points.

use crate::{
    borrows::{AliasError, AliasErrorKind, Borrows, NewPermission, PtrKind, RetagMode},
    ids::{AccessKind, AllocId, BorTag, Prov, SiteId, Span, ThreadId},
};

/// A pointer: the allocation it points into, its provenance, and its offset there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ptr {
    pub(crate) alloc: AllocId,
    pub(crate) prov: Prov,
    pub(crate) offset: u64,
}

impl Ptr {
    /// `ptr.add(n)` / `ptr.byte_add(n)`: the same provenance, `n` bytes further.
    pub(crate) fn add(self, n: u64) -> Ptr {
        Ptr { offset: self.offset + n, ..self }
    }

    /// `ptr.sub(n)`.
    pub(crate) fn sub(self, n: u64) -> Ptr {
        Ptr { offset: self.offset - n, ..self }
    }

    /// The pointer `addr as *mut T` gives after this one was exposed: wildcard provenance.
    pub(crate) fn wildcard(self) -> Ptr {
        Ptr { prov: Prov::Wildcard, ..self }
    }

    pub(crate) fn tag(self) -> BorTag {
        match self.prov {
            Prov::Concrete { tag, .. } => tag,
            Prov::Wildcard => panic!("a wildcard pointer has no tag"),
        }
    }
}

pub(crate) struct Sim {
    pub(crate) b: Borrows,
    next_alloc: u32,
    site: u32,
}

pub(crate) type R<T = ()> = Result<T, AliasError>;

impl Sim {
    /// A fresh model with `main`'s frame pushed.
    pub(crate) fn new() -> Sim {
        let mut b = Borrows::new();
        b.push_frame();
        Sim { b, next_alloc: 0, site: 0 }
    }

    fn site(&mut self) -> SiteId {
        self.site += 1;
        SiteId(self.site)
    }

    /// The site of the last operation.
    pub(crate) fn last_site(&self) -> SiteId {
        SiteId(self.site)
    }

    /// A new allocation of `size` bytes (a local, a `Box`'s heap block, a static); its root
    /// pointer.
    pub(crate) fn alloc(&mut self, size: u64) -> Ptr {
        self.next_alloc += 1;
        let alloc = AllocId::new(self.next_alloc).unwrap();
        let site = self.site();
        let tag = self.b.new_alloc(alloc, size, site);
        Ptr { alloc, prov: Prov::Concrete { alloc, tag }, offset: 0 }
    }

    pub(crate) fn free(&mut self, p: Ptr, size: u64) -> R {
        assert_eq!(p.offset, 0);
        let site = self.site();
        self.b.free(p.alloc, p.prov, size, site)
    }

    pub(crate) fn read(&mut self, p: Ptr, len: u64) -> R {
        let site = self.site();
        self.b.access(p.alloc, p.prov, AccessKind::Read, Span::new(p.offset, len), site)
    }

    pub(crate) fn write(&mut self, p: Ptr, len: u64) -> R {
        let site = self.site();
        self.b.access(p.alloc, p.prov, AccessKind::Write, Span::new(p.offset, len), site)
    }

    /// The general retag: a `kind` pointer to `len` bytes at `p`, made by a copy of retag
    /// `mode`, to a pointee that is `freeze` and `unpin` with `UnsafeCell`s at `cells`
    /// (relative to `p`). `None` from the model means the pointer keeps its tag.
    pub(crate) fn retag(
        &mut self,
        p: Ptr,
        kind: PtrKind,
        mode: RetagMode,
        freeze: bool,
        unpin: bool,
        cells: &[Span],
        len: u64,
    ) -> R<Ptr> {
        self.retag_with(p, NewPermission::new(kind, mode, freeze, unpin, cells), len)
    }

    /// The retag `new` of `len` bytes at `p` (for `NewPermission::with_implicit_writes`).
    pub(crate) fn retag_with(&mut self, p: Ptr, new: NewPermission, len: u64) -> R<Ptr> {
        let site = self.site();
        let tag = self.b.retag(p.alloc, p.prov, new, Span::new(p.offset, len), site)?;
        Ok(match tag {
            Some(tag) => Ptr { prov: Prov::Concrete { alloc: p.alloc, tag }, ..p },
            None => p,
        })
    }

    /// `&*p` to a `Freeze` pointee of `len` bytes.
    pub(crate) fn shared(&mut self, p: Ptr, len: u64) -> R<Ptr> {
        self.retag(p, PtrKind::Shared, RetagMode::Default, true, true, &[], len)
    }

    /// `&*p` to a pointee with `UnsafeCell`s at `cells` (all of it for `&Cell<T>`).
    pub(crate) fn shared_cell(&mut self, p: Ptr, len: u64, cells: &[Span]) -> R<Ptr> {
        self.retag(p, PtrKind::Shared, RetagMode::Default, false, true, cells, len)
    }

    /// `&mut *p` to a `Freeze` pointee of `len` bytes.
    pub(crate) fn mutable(&mut self, p: Ptr, len: u64) -> R<Ptr> {
        self.retag(p, PtrKind::Mut, RetagMode::Default, true, true, &[], len)
    }

    /// `&mut *p` to a pointee with `UnsafeCell`s at `cells`.
    pub(crate) fn mutable_cell(&mut self, p: Ptr, len: u64, cells: &[Span]) -> R<Ptr> {
        self.retag(p, PtrKind::Mut, RetagMode::Default, false, true, cells, len)
    }

    /// A two-phase `&mut` (the receiver of `v.push(v.len())`).
    pub(crate) fn two_phase(&mut self, p: Ptr, len: u64) -> R<Ptr> {
        self.retag(p, PtrKind::Mut, RetagMode::TwoPhase, true, true, &[], len)
    }

    /// A `Box<T>` made from (or copied as) `p`.
    pub(crate) fn boxed(&mut self, p: Ptr, len: u64) -> R<Ptr> {
        self.retag(p, PtrKind::Box, RetagMode::Default, true, true, &[], len)
    }

    /// Runs the next calls, returns and function-entry retags on thread `t` (0 is main).
    pub(crate) fn thread(&mut self, t: u32) {
        self.b.set_thread(ThreadId(t));
    }

    /// A call on the current thread: pushes the callee's frame.
    pub(crate) fn call(&mut self) {
        self.b.push_frame();
    }

    /// The callee's argument `&T` (`Freeze`), protected until `ret`.
    pub(crate) fn arg_shared(&mut self, p: Ptr, len: u64) -> R<Ptr> {
        self.retag(p, PtrKind::Shared, RetagMode::FnEntry, true, true, &[], len)
    }

    /// The callee's argument `&T` with `UnsafeCell`s at `cells`.
    pub(crate) fn arg_shared_cell(&mut self, p: Ptr, len: u64, cells: &[Span]) -> R<Ptr> {
        self.retag(p, PtrKind::Shared, RetagMode::FnEntry, false, true, cells, len)
    }

    /// The callee's argument `&mut T` (`Freeze`), protected until `ret`.
    pub(crate) fn arg_mut(&mut self, p: Ptr, len: u64) -> R<Ptr> {
        self.retag(p, PtrKind::Mut, RetagMode::FnEntry, true, true, &[], len)
    }

    /// The callee's argument `&mut T` with `UnsafeCell`s at `cells`.
    pub(crate) fn arg_mut_cell(&mut self, p: Ptr, len: u64, cells: &[Span]) -> R<Ptr> {
        self.retag(p, PtrKind::Mut, RetagMode::FnEntry, false, true, cells, len)
    }

    /// The callee's argument `Box<T>`, weakly protected until `ret`.
    pub(crate) fn arg_box(&mut self, p: Ptr, len: u64) -> R<Ptr> {
        self.retag(p, PtrKind::Box, RetagMode::FnEntry, true, true, &[], len)
    }

    /// The callee returns: its protectors end.
    pub(crate) fn ret(&mut self) -> R {
        let site = self.site();
        self.b.pop_frame(site)
    }

    /// `p as usize` (`expose_provenance`).
    pub(crate) fn expose(&mut self, p: Ptr) {
        if let Prov::Concrete { alloc, tag } = p.prov {
            self.b.expose(alloc, tag);
        }
    }

    /// Runs the GC with `held` as every pointer the program still holds.
    pub(crate) fn gc(&mut self, held: &[Ptr]) {
        self.b.gc(|live| {
            for p in held {
                if let Prov::Concrete { tag, .. } = p.prov {
                    live.mark(tag);
                }
            }
        });
    }

    pub(crate) fn name(&mut self, p: Ptr, name: &str) {
        assert!(self.b.name_tag(p.alloc, p.tag(), 0, name));
    }

    pub(crate) fn print(&self, p: Ptr, show_unnamed: bool) -> String {
        self.b.print_tree(p.alloc, show_unnamed).unwrap()
    }
}

/// Asserts `r` is the aliasing error of `kind` whose title starts with `title` (Miri's
/// `//~ ERROR: /write access through .* is forbidden/` is `"write access through"`), and
/// returns it.
#[track_caller]
pub(crate) fn expect_ub<T: std::fmt::Debug>(r: R<T>, kind: AliasErrorKind, title: &str) -> AliasError {
    match r {
        Ok(v) => panic!("expected `{title}...`, got Ok({v:?})"),
        Err(e) => {
            assert_eq!(e.kind(), kind, "{e}");
            assert!(e.title().starts_with(title), "expected `{title}...`, got `{}`", e.title());
            assert!(e.title().ends_with("is forbidden"), "{}", e.title());
            e
        }
    }
}

/// Asserts some help of `e` is `detail`, with tags written `<TAG>` as in Miri's `.stderr`
/// files (`the accessed tag <TAG> has state Frozen which forbids this child write access`).
#[track_caller]
pub(crate) fn expect_detail(e: &AliasError, detail: &str) {
    let found = e.details().iter().any(|d| normalize(d) == detail);
    assert!(found, "no help `{detail}` in:\n{e}");
}

/// Asserts the history of `e`, normalized as `expect_detail`, is `notes` (the `help:` lines
/// of Miri's `.stderr` after the details, without the `-->` places).
#[track_caller]
pub(crate) fn expect_history(e: &AliasError, notes: &[&str]) {
    let got: Vec<String> = e.history().iter().map(|ev| normalize(&ev.note)).collect();
    assert_eq!(got, notes, "history of:\n{e}");
}

/// Tags `<n>` (with a name `<n> (name)`) and allocations `allocN` written as Miri's test
/// normalization does: `<TAG>`, `ALLOC`.
pub(crate) fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let digits = bytes[i + 1..].iter().take_while(|b| b.is_ascii_digit()).count();
            if digits > 0 && bytes.get(i + 1 + digits) == Some(&b'>') {
                out.push_str("<TAG>");
                i += digits + 2;
                continue;
            }
        }
        if s[i..].starts_with("alloc") {
            let digits = bytes[i + 5..].iter().take_while(|b| b.is_ascii_digit()).count();
            if digits > 0 {
                out.push_str("ALLOC");
                i += 5 + digits;
                continue;
            }
        }
        let c = s[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}
