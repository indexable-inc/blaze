//! A trait method's receiver written `self: T`, classified for dyn compatibility from the type
//! `T` as written and resolved (never from its text): the Reference, "Traits", "Dyn
//! compatibility": dispatchable functions must "Have a receiver with one of the following
//! types: `&Self` (i.e. `&self`), `&mut Self` (i.e `&mut self`), `Box<Self>`, `Rc<Self>`,
//! `Arc<Self>`, `Pin<P>` where `P` is one of the types above". The checker decides from the
//! [`Receiver`] (`blaze_typeck::dyn_compat`, the rule's one home); this module only reads which
//! one `T` is.
//!
//! `Pin` over `Pin`: the pinned library makes it dispatchable too (core/src/pin.rs:
//! `impl<Ptr, U> DispatchFromDyn<Pin<U>> for Pin<Ptr> where Ptr: DispatchFromDyn<U> +
//! PinSafePointer`, and `unsafe impl<P: PinSafePointer> PinSafePointer for Pin<P> {}`), so the
//! nested form is read as [`Receiver::PinOfPointer`] like one `Pin`: a program nightly accepts is
//! not refused.
//!
//! `Box<Self, A>` with a written allocator, `Rc<Self, A>` and `Arc<Self, A>`: the pinned library
//! dispatches only from the `Global` forms (alloc/src/boxed.rs: `impl<T: ?Sized + Unsize<U>, U:
//! ?Sized> DispatchFromDyn<Box<U>> for Box<T, Global> {}`; rcs/rc.rs and rcs/arc.rs the same
//! for `Rc<T>` and `Arc<T>`); whether a written second argument is `Global` is not read here,
//! so it stays [`Unread::TypedReceiver`], a named gap. So do a raw pointer to `Self` (dispatchable
//! under `arbitrary_self_types_pointers`: core/src/ops/unsize.rs `DispatchFromDyn<*const U> for
//! *const T`), a type alias, an associated type, a type macro and a path that does not resolve.
//!
//! A user type (`self: MyBox<Self>`) is [`Receiver::Other`]: not dispatchable, so the trait is
//! not dyn compatible unless that method has `where Self: Sized`. The unstable
//! `dispatch_from_dyn` feature lets a crate implement `DispatchFromDyn` for its own pointer; such
//! an impl is not read here (core/src/ops/unsize.rs: "`DispatchFromDyn` is used in the
//! implementation of dyn-compatibility checks ... This is not intended for general use").

use blaze_id::ItemKey as LibKey;
use blaze_lower::View;
use blaze_lower::form as lf;
use blaze_typeck::dyn_compat::{Receiver, Unread};

use super::dyn_ty::is_self;
use super::index::Index;
use super::source::NamesSource;
use super::types::{Why, path_texts};
use crate::resolve::{Kind, Ns, Paths};

/// Why one of the pointer types is not known: a gap, never "the program has none".
pub type NoAnswer = &'static str;

/// One of the library's receiver pointer types, as the program's crates hold it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pointer {
    /// The program's crates hold none (a program without `alloc`, or a `no_core` rung): no
    /// written path can name it.
    Absent,
    At(LibKey),
    /// Not found, and why: a receiver naming a struct none of the known keys match is then a
    /// gap, not [`Receiver::Other`].
    Unknown(NoAnswer),
}

/// The pointer types a dispatchable receiver may be written with, found once per program by
/// [`Pointers::find`] and kept beside `sized` in the head store ([`super::heads::Store`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pointers {
    /// `#[lang = "owned_box"]` (alloc's `Box`).
    pub boxed: Pointer,
    /// alloc's `rc::Rc` (marked `#[rustc_diagnostic_item = "Rc"]`, no `#[lang]`).
    pub rc: Pointer,
    /// alloc's `sync::Arc` (marked `#[rustc_diagnostic_item = "Arc"]`, no `#[lang]`).
    pub arc: Pointer,
    /// `#[lang = "pin"]` (core's `pin::Pin`).
    pub pin: Pointer,
}

/// Which of [`Pointers`] a resolved struct is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Which {
    Box,
    Rc,
    Arc,
    Pin,
    /// None of them, every one known.
    NotAPointer,
    /// None of the known ones, and one is not known: a gap.
    Undecided,
}

impl Pointers {
    /// A store made without [`Pointers::find`]: every struct receiver is a named gap.
    pub const NOT_LOOKED_UP: Pointers = {
        let u = Pointer::Unknown("the head store was made without the program's receiver pointer types (`receiver::Pointers::find`)");
        Pointers { boxed: u, rc: u, arc: u, pin: u }
    };

    /// The program's `Box`, `Rc`, `Arc` and `Pin`: `Box` and `Pin` by their `#[lang]` in the
    /// library's lang table, `Rc` and `Arc` by their paths in alloc (`crate::rc::Rc`,
    /// `crate::sync::Arc`), resolved from the module of alloc's `#[lang = "owned_box"]`.
    pub fn find<'n, S: NamesSource<'n>>(index: &Index<'_>, resolver: Paths<S>) -> Pointers {
        let pin = match index.lang("pin") {
            Some(k) => Pointer::At(k),
            None => Pointer::Absent,
        };
        let Some(boxed) = index.lang("owned_box") else {
            // No alloc among the program's crates: `Rc` and `Arc` are alloc's too.
            return Pointers { boxed: Pointer::Absent, rc: Pointer::Absent, arc: Pointer::Absent, pin };
        };
        let (rc, arc) = match resolver.names.home_of(boxed) {
            Some(module) => (in_alloc(resolver, module, &["crate", "rc", "Rc"]), in_alloc(resolver, module, &["crate", "sync", "Arc"])),
            None => {
                let u = Pointer::Unknown("alloc's `Box` is in no module the names library knows");
                (u, u)
            }
        };
        Pointers { boxed: Pointer::At(boxed), rc, arc, pin }
    }

    fn which(&self, key: LibKey) -> Which {
        let all = [(self.boxed, Which::Box), (self.rc, Which::Rc), (self.arc, Which::Arc), (self.pin, Which::Pin)];
        let mut unknown = false;
        for (p, w) in all {
            match p {
                Pointer::At(k) if k == key => return w,
                Pointer::Unknown(_) => unknown = true,
                Pointer::At(_) | Pointer::Absent => {}
            }
        }
        match unknown {
            true => Which::Undecided,
            false => Which::NotAPointer,
        }
    }
}

/// The struct at `texts` resolved from `module` (in alloc).
fn in_alloc<'n, S: NamesSource<'n>>(resolver: Paths<S>, module: LibKey, texts: &[&str]) -> Pointer {
    match resolver.path(module, false, texts, Ns::Type) {
        Ok(found) if found.consumed as usize == texts.len() && found.kind == Kind::Struct => Pointer::At(found.key),
        Ok(_) => Pointer::Unknown("alloc's `rc::Rc` or `sync::Arc` names no struct"),
        Err(_) => Pointer::Unknown("alloc's `rc::Rc` or `sync::Arc` does not resolve"),
    }
}

/// How many `Pin`s deep a receiver is read before it is a gap (the pinned library writes none
/// nested; the bound only keeps the walk finite over any table).
const PIN_DEPTH: u32 = 8;

/// The receiver `self: ty` of a trait method whose names resolve in `module`: `Ok(receiver)`
/// for one of the Reference's forms or [`Receiver::Other`] (a verdict: not dispatchable), or
/// `Err(unread)` for a type whose meaning is not read here (module docs). `Err(Why)` only for a
/// malformed table. No recursion: one step per `Pin`.
pub fn classify<'n, S: NamesSource<'n>>(resolver: &Paths<S>, module: LibKey, pointers: &Pointers, v: &View<'_>, ty: lf::TypeId) -> Result<Result<Receiver, Unread>, Why> {
    let mut t = ty;
    let mut pinned = false;
    for _ in 0..PIN_DEPTH {
        let p = match v.ty(t)? {
            lf::TypeRef::Path(p) => p,
            // `&Self`, `&mut Self`, or under `Pin`.
            lf::TypeRef::Ref { mutbl, inner, .. } => {
                if !is_self(v, inner)? {
                    return Ok(Ok(Receiver::Other));
                }
                return Ok(Ok(match (pinned, mutbl) {
                    (true, _) => Receiver::PinOfPointer,
                    (false, lf::Mutbl::Mut) => Receiver::RefMut,
                    (false, lf::Mutbl::Not) => Receiver::Ref,
                }));
            }
            // `*const Self` (feature-gated, dispatchable): not read. Under `Pin` a raw pointer
            // is no `PinSafePointer`: not dispatchable.
            lf::TypeRef::Ptr { inner, .. } => {
                if !pinned && is_self(v, inner)? {
                    return Ok(Err(Unread::TypedReceiver));
                }
                return Ok(Ok(Receiver::Other));
            }
            lf::TypeRef::Macro(_) | lf::TypeRef::Error => return Ok(Err(Unread::TypedReceiver)),
            lf::TypeRef::Tuple(_)
            | lf::TypeRef::Array { .. }
            | lf::TypeRef::Slice(_)
            | lf::TypeRef::FnPtr { .. }
            | lf::TypeRef::Dyn(_)
            | lf::TypeRef::Impl(_)
            | lf::TypeRef::Never
            | lf::TypeRef::Infer
            | lf::TypeRef::For { .. }
            | lf::TypeRef::Pat { .. }
            | lf::TypeRef::Record(_)
            | lf::TypeRef::Union(_) => return Ok(Ok(Receiver::Other)),
        };
        if is_self(v, t)? {
            // `self: Self` is `self`; `Pin<Self>`: `Self` is no pointer.
            return Ok(Ok(if pinned { Receiver::Other } else { Receiver::Value }));
        }
        let (abs, texts, segs, _) = match path_texts(v, p) {
            Ok(read) => read,
            Err(Why::Unsupported(..)) => return Ok(Err(Unread::TypedReceiver)),
            Err(e) => return Err(e),
        };
        let Ok(found) = resolver.path(module, abs, &texts, Ns::Type) else { return Ok(Err(Unread::TypedReceiver)) };
        if found.consumed as usize != texts.len() {
            // `Self::Ptr`, `T::Assoc`: a projection.
            return Ok(Err(Unread::TypedReceiver));
        }
        match found.kind {
            Kind::Struct => {}
            Kind::Enum | Kind::Union | Kind::Builtin(_) | Kind::WideInt { .. } => return Ok(Ok(Receiver::Other)),
            // A type alias (its right-hand side is not read here), a trait (a bare trait
            // object), anything else.
            _ => return Ok(Err(Unread::TypedReceiver)),
        }
        let which = pointers.which(found.key);
        match which {
            Which::NotAPointer => return Ok(Ok(Receiver::Other)),
            Which::Undecided => return Ok(Err(Unread::TypedReceiver)),
            Which::Box | Which::Rc | Which::Arc | Which::Pin => {}
        }
        // The pointer's one argument, a type, on the last segment; `Box<Self, A>` and any
        // argument on an earlier segment are not read.
        let Some((last, before)) = segs.split_last() else { return Ok(Err(Unread::TypedReceiver)) };
        if before.iter().any(|s| s.has_args) || last.parenthesized || !last.has_args {
            return Ok(Err(Unread::TypedReceiver));
        }
        let args = v.rows(last.args)?;
        let [arg] = args else { return Ok(Err(Unread::TypedReceiver)) };
        let lf::GenericArg::Type(inner) = lf::GenericArg::decode(*arg)? else { return Ok(Err(Unread::TypedReceiver)) };
        if which == Which::Pin {
            pinned = true;
            t = inner;
            continue;
        }
        if !is_self(v, inner)? {
            return Ok(Ok(Receiver::Other));
        }
        return Ok(Ok(match (pinned, which) {
            (true, _) => Receiver::PinOfPointer,
            (false, Which::Rc) => Receiver::Rc,
            (false, Which::Arc) => Receiver::Arc,
            (false, _) => Receiver::Box,
        }));
    }
    Ok(Err(Unread::TypedReceiver))
}
