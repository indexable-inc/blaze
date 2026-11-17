//! A database attached to the task, for `Debug` of interned ids (which hold no database):
//! `attach(db, || format!("{id:?}"))` prints an interned id's fields one level deep, each cut
//! at [`FIELD_CAP`] bytes ([`Bounded`]), unattached the id (an input's print is its kind and
//! id either way: its fields are its value, not what it is); and for code that formats
//! against the front end's own database type ([`with_attached_db`]).
//!
//! What is attached lives on the stack of the [`attach`] call; the task-local is a pointer to
//! it, carried with the task across fiber waits (`blaze_graph::sched::carry`) as the stack of
//! runs is, so a task that parks inside `attach` and resumes on another worker still has it,
//! and the worker it left has none.

use std::{any::TypeId, cell::Cell, num::NonZeroUsize, ptr, sync::Once};

use crate::{
    Database, Gid, Interned, Kind, Memo,
    trie::{Entry, Head},
};

/// One [`attach`] call's database: its snapshot, its address and its type.
struct Attached {
    memo: *const Memo,
    /// The database's address: read back only as the (sized) `D` whose [`TypeId`] is `ty`.
    db: *const (),
    ty: TypeId,
}

thread_local! {
    static ATTACHED: Cell<*const Attached> = const { Cell::new(ptr::null()) };
}

fn register() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        blaze_graph::sched::carry(blaze_graph::sched::Carried::word(
            || {
                let it = ATTACHED.with(|cell| cell.replace(ptr::null()));
                NonZeroUsize::new(it as usize)
            },
            |it| ATTACHED.with(|cell| cell.set(it.get() as *const Attached)),
        ));
    });
}

/// Runs `f` with `db` attached to this task: its snapshot for `Debug` of ids, and `db` itself
/// for [`with_attached_db::<D>`]. The one attached before is put back when `f` returns or
/// unwinds. `D` is the database's own (sized, `'static`) type, as `blaze_query::db::attach`
/// took it, so the mechanical rewrite of spec rule 6 keeps every caller.
pub fn attach<D: Database + 'static, R>(db: &D, f: impl FnOnce() -> R) -> R {
    register();
    struct Restore(*const Attached);
    impl Drop for Restore {
        fn drop(&mut self) {
            replace_attached(self.0);
        }
    }
    let attached = Attached {
        memo: ptr::from_ref(db.memo()),
        db: ptr::from_ref(db).cast::<()>(),
        ty: TypeId::of::<D>(),
    };
    let _restore = Restore(replace_attached(&raw const attached));
    f()
}

/// Attaches `it` to this thread and gives what was attached. Never inlined, as every read of a
/// thread-local in this crate is (`task::innermost_ptr` says why): `f` in [`attach`] may park
/// its fiber and resume on another worker, and the restore after it must reach that worker's
/// slot, not the address taken before the wait.
#[inline(never)]
fn replace_attached(it: *const Attached) -> *const Attached {
    ATTACHED.with(|cell| cell.replace(it))
}

/// What is attached to this thread. Never inlined ([`replace_attached`]).
#[inline(never)]
fn attached_ptr() -> *const Attached {
    ATTACHED.with(Cell::get)
}

fn attached<'a>() -> Option<&'a Attached> {
    // SAFETY: an attached record lives on the stack of the `attach` call that attached it,
    // below this one on the task's stack, and is detached before that call returns.
    unsafe { attached_ptr().as_ref() }
}

/// Runs `f` with the attached snapshot, if any.
pub fn with_attached<R>(f: impl FnOnce(Option<&Memo>) -> R) -> R {
    // SAFETY: an attached snapshot outlives the `attach` call that attached it (see
    // `attached`).
    let memo = attached().and_then(|it| unsafe { it.memo.as_ref() });
    f(memo)
}

/// Runs `f` with the database [`attach`] attached to this task, if one is and it is a `D`
/// (one attached as another type is `None` here).
pub fn with_attached_db<D: Database + 'static, R>(f: impl FnOnce(Option<&D>) -> R) -> R {
    let db = attached().filter(|it| it.ty == TypeId::of::<D>()).map(|it| {
        // SAFETY: `attach` erased a `&D` to this address and lives below us (see `attached`);
        // the type is `D`, sized, so the address alone is the reference.
        unsafe { &*it.db.cast::<D>() }
    });
    f(db)
}

thread_local! {
    /// Whether this thread is printing the fields of an interned or input id now: inside, every
    /// id prints as its kind and id alone ([`shallow`]). A thread-local, not a task's: a
    /// `Debug` never parks, so a print begins and ends on one thread.
    static PRINTING_FIELDS: Cell<bool> = const { Cell::new(false) };
}

/// Says whether this thread is printing an id's fields `now`, and gives what it said before.
/// Never inlined ([`replace_attached`]): a print parks nowhere, but the function it is inlined
/// into may.
#[inline(never)]
fn printing_fields(now: bool) -> bool {
    PRINTING_FIELDS.with(|it| it.replace(now))
}

/// Hands `f` the fields `found` gives, unless this thread is printing an id's fields already:
/// then `None`, so the id prints as its kind and id alone. So a print of an id is one level
/// deep whatever its fields hold, through any container (a `Box<[Id]>`, an `Option`, a
/// hand-written `Debug` of a struct of ids): each id inside asks here, and is refused. Without
/// it a key shaped like a graph (an env key whose ancestors hold decl keys that hold env keys)
/// prints every path through it, exponential in its depth.
fn shallow<F: ?Sized, R>(found: Option<&F>, f: impl FnOnce(Option<&F>) -> R) -> R {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            printing_fields(false);
        }
    }
    match found {
        Some(fields) if !printing_fields(true) => {
            let _restore = Restore;
            f(Some(fields))
        }
        _ => f(None),
    }
}

/// The fields of interned `id` in the attached snapshot, `None` unattached, where it holds no
/// such id, or inside the print of another id's fields ([`shallow`]). Lists no read: printing
/// is not a dependency.
pub fn attached_interned<T: Interned, R>(id: Gid, f: impl FnOnce(Option<&T::Fields>) -> R) -> R {
    with_attached(|memo| {
        let head = memo.and_then(|memo| memo.try_head(id, T::KIND));
        // SAFETY: an entry of `T::KIND` is an `Entry<T::Fields, ()>`.
        shallow(head.map(|head| &unsafe { Entry::<T::Fields, ()>::of(head) }.key), f)
    })
}

/// A value's `Debug`, cut at `cap` bytes: what a generated `Debug` prints of each field of an
/// id ([`Bounded::field`], [`FIELD_CAP`]) and what a message prints of a query's key
/// ([`Bounded::key`], [`KEY_CAP`]). The value's own `Debug` writes into a writer that stops it
/// with an error once `cap` bytes are written (most impls return at the first error, so a
/// megabyte of text costs `cap` bytes of work, not a megabyte); the print then ends in
/// `… (cut at N bytes)`. A text or bytes field shows its first bytes this way; its length is
/// not known to a `Debug` in general, so it is not printed.
pub struct Bounded<'a, T: ?Sized> {
    value: &'a T,
    cap: usize,
}

/// The most of one field's `Debug` an id's print shows.
pub const FIELD_CAP: usize = 160;
/// The most of a query key's `Debug` a message shows.
pub const KEY_CAP: usize = 1024;

impl<'a, T: ?Sized> Bounded<'a, T> {
    /// One field of an id's print, cut at [`FIELD_CAP`].
    pub fn field(value: &'a T) -> Bounded<'a, T> {
        Bounded { value, cap: FIELD_CAP }
    }

    /// A query key in a message, cut at [`KEY_CAP`].
    pub fn key(value: &'a T) -> Bounded<'a, T> {
        Bounded { value, cap: KEY_CAP }
    }
}

impl<T: std::fmt::Debug + ?Sized> std::fmt::Debug for Bounded<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        struct Cap<'f, 'b> {
            out: &'f mut std::fmt::Formatter<'b>,
            left: usize,
            cut: bool,
        }
        impl std::fmt::Write for Cap<'_, '_> {
            fn write_str(&mut self, s: &str) -> std::fmt::Result {
                if s.len() <= self.left {
                    self.left -= s.len();
                    return self.out.write_str(s);
                }
                let mut end = self.left;
                while !s.is_char_boundary(end) {
                    end -= 1;
                }
                self.out.write_str(&s[..end])?;
                self.left = 0;
                self.cut = true;
                Err(std::fmt::Error)
            }
        }
        let alternate = f.alternate();
        let mut cap = Cap { out: f, left: self.cap, cut: false };
        let written = match alternate {
            true => std::fmt::Write::write_fmt(&mut cap, format_args!("{:#?}", self.value)),
            false => std::fmt::Write::write_fmt(&mut cap, format_args!("{:?}", self.value)),
        };
        match cap.cut {
            true => write!(f, "… (cut at {} bytes)", self.cap),
            false => written,
        }
    }
}

impl Memo {
    /// The head of `id` if this snapshot holds it and it is of `kind`.
    pub(crate) fn try_head(&self, id: Gid, kind: &'static Kind) -> Option<&Head> {
        let head = if id.is_spill() {
            self.spill().local(id.local())
        } else {
            self.try_slot(id.arena())?.local(id.local())
        }?;
        ptr::eq(head.kind, kind).then_some(head)
    }
}
