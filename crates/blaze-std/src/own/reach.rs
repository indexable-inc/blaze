//! The one reachability walk over a type's shape, run at compile time
//! (docs/v2/plan/design/ownership.md): from a type, through what it owns, is there something a
//! question asks about? [`reaches`] answers a [`Question`]; [`none`] is the same as a `where
//! const` condition, whose panic is the reason a bound shows.
//!
//! The walk is over `TypeId`s, read through [`raw`](super::raw) (`core::mem::type_info`), so
//! it is decided per concrete instance. How a type holds what is inside it:
//!
//! - a struct, enum or tuple holds its fields (private ones too) in place; an array or slice
//!   its elements;
//! - a type with an [`Owns`] impl holds its type arguments the way the impl says, and the walk
//!   does not look at its fields: [`Inline`] (in place: `Option`), [`Solely`] (behind a pointer
//!   it alone owns: `Box`, `Vec`), [`Jointly`] (behind a counted pointer: `Rc`, `Arc`),
//!   [`Mutably`] (in place, behind interior mutability: `UnsafeCell`, `RefCell`, a lock),
//!   [`Nothing`] (`Weak`: it owns none of them);
//! - a reference owns nothing; a function pointer and a primitive hold nothing;
//! - a raw pointer, a union, a `dyn Trait`, a closure and a foreign type are not seen into.
//!   What the walk cannot see into is a refusal, never a guess, unless the question says to
//!   pass over it.
//!
//! The walk ends on a recursive type: the types it has visited are kept in a fixed array of
//! `(TypeId, behind a cell)` and each is expanded once. An instance is its own `TypeId`, so
//! `List<u8>` and `List<Node>` are two entries. More than [`NODES`] entries is a refusal that
//! says so.
//!
//! This file is the walk alone, with no item of bstd or std in it, so the front end's tests
//! evaluate this very text over a fixture's types (`crates/hir-ty-mir/src/tests/consteval/reach.rs`).

use super::raw::{self, TypeId};

/// How a type holds its type arguments, said by an `unsafe impl Owns<How> for C<T>`. The
/// impl is a trusted assertion: the walk takes `C<T>` as that edge to each type argument and
/// does not read `C`'s fields.
///
/// # Safety
///
/// Every value of a type argument that a value of the type keeps alive is held the way `How`
/// says, and the type keeps alive nothing else that owns: [`Nothing`] only when dropping the
/// value never drops a value of an argument.
pub unsafe trait Owns<How: ?Sized> {}

/// In place, or behind a pointer the value alone owns, with no way to change it through a
/// shared reference: `Option<T>`, `[T; N]` are the first; `Box<T>`, `Vec<T>` the second.
pub struct Inline;
/// Behind a pointer the value alone owns (`Box`, `Vec`, a map).
pub struct Solely;
/// Behind a counted pointer other handles share (`Rc`, `Arc`).
pub struct Jointly;
/// In place, changeable through a shared reference (`UnsafeCell` and everything that is one).
pub struct Mutably;
/// Not owned at all (`Weak`, an index, `PhantomData`).
pub struct Nothing;

/// What reflection says a type with no [`Owns`] impl is.
#[derive(Clone, Copy)]
pub enum Shape {
    /// A primitive, `str`, a function pointer: holds nothing.
    Leaf,
    /// A struct, enum or tuple: holds its fields.
    Parts,
    /// An array or slice: holds its elements.
    Elements,
    /// A reference: owns nothing, points at one type.
    Borrow,
    /// A raw pointer: may own what it points at, or not.
    Pointer,
    /// A `dyn Trait`: the type inside is not known.
    Dyn,
    /// A union, a closure, a foreign type, anything else: not seen into.
    Unseen,
}

/// What a walk looks for.
#[derive(Clone, Copy)]
pub enum Find {
    /// A way for a value to own itself: a [`Jointly`] pointer behind a [`Mutably`] holder
    /// whose pointee leads back to the type the walk started at.
    Cycle,
    /// A value of exactly this type.
    Type(TypeId),
    /// A value of a type that implements this trait, given as the id of its `dyn` type.
    Implements(TypeId),
}

/// How far a walk follows.
#[derive(Clone, Copy)]
pub enum Through {
    /// What is in the value's own bytes: fields, elements, [`Inline`] and [`Mutably`] holders.
    InPlace,
    /// And what it owns behind pointers: [`Solely`] and [`Jointly`] holders.
    Owned,
    /// And what it only points at: references and raw pointers.
    Borrowed,
}

/// A question, as data: the walk is one function and a question never runs code in it.
#[derive(Clone, Copy)]
pub struct Ask {
    pub find: Find,
    pub through: Through,
    /// What a refusal calls the thing found: "a capability", "interior mutability".
    pub noun: &'static str,
    /// Whether a type the walk cannot see into is a refusal (`true`) or passed over.
    pub unseen_refuses: bool,
    /// A trait (the id of its `dyn` type) that answers for a `dyn` type's hidden type: a `dyn
    /// Tr` that implements it (`trait Tr: Acyclic`) is not looked into. `None`: no `dyn` type
    /// is vouched for.
    pub vouch: Option<TypeId>,
}

/// A question about types, named by a type so that it can be a generic argument:
/// `reaches::<T, Q>()`.
pub trait Question {
    const ASK: Ask;
}

/// The longest refusal, in bytes; a longer one ends in `...`.
pub const TEXT: usize = 768;
/// How many `(type, behind a cell)` entries one walk visits before it refuses.
pub const NODES: usize = 192;
/// No entry.
const NONE: usize = !0;

/// The entry `at` of `items`, or `None` past the end: a lookup that cannot panic, written
/// with a slice pattern alone (no method of `core`'s slices), so the front end's fixture
/// evaluates it too. Linear in `at`; every walk already scans its entries once per `add`.
pub(crate) const fn entry<T>(items: &[T], at: usize) -> Option<&T> {
    let mut rest = items;
    let mut left = at;
    loop {
        match rest {
            [first, more @ ..] => {
                if left == 0 {
                    return Some(first);
                }
                left -= 1;
                rest = more;
            }
            [] => return None,
        }
    }
}

/// [`entry`], to write.
const fn entry_mut<T>(items: &mut [T], at: usize) -> Option<&mut T> {
    let mut rest = items;
    let mut left = at;
    loop {
        match rest {
            [first, more @ ..] => {
                if left == 0 {
                    return Some(first);
                }
                left -= 1;
                rest = more;
            }
            [] => return None,
        }
    }
}

/// Why a walk refused, as text: the field path to what it found.
#[derive(Clone, Copy)]
pub struct Reason {
    bytes: [u8; TEXT],
    len: usize,
}

impl Reason {
    const fn new() -> Reason {
        Reason { bytes: [0; TEXT], len: 0 }
    }

    /// Appends `text`, whole or not at all: once something did not fit, nothing more is added
    /// and the text ends in `...`.
    const fn push(&mut self, text: &str) {
        let text = text.as_bytes();
        if self.len == NONE {
            return;
        }
        if self.len + text.len() + 3 > TEXT {
            // Every push that fit left room for the dots, so they always fit.
            let mut dots = 0;
            while dots < 3 {
                if let Some(byte) = entry_mut(&mut self.bytes, self.len + dots) {
                    *byte = b'.';
                }
                dots += 1;
            }
            self.len = NONE;
            return;
        }
        let mut at = 0;
        let mut rest = text;
        while let [first, more @ ..] = rest {
            if let Some(byte) = entry_mut(&mut self.bytes, self.len + at) {
                *byte = *first;
            }
            at += 1;
            rest = more;
        }
        self.len += text.len();
    }

    /// `` `name` ``.
    const fn push_ty(&mut self, ty: TypeId) {
        self.push("`");
        self.push(raw::name(ty));
        self.push("`");
    }

    /// The refusal's text.
    pub const fn as_str(&self) -> &str {
        let len = if self.len == NONE {
            // Cut short: the bytes up to the dots, which follow the last whole piece.
            let mut end = TEXT;
            while end > 0 {
                match entry(&self.bytes, end - 1) {
                    Some(&0) => end -= 1,
                    _ => break,
                }
            }
            end
        } else {
            self.len
        };
        raw::text(&self.bytes, len)
    }
}

/// What a walk found.
#[derive(Clone, Copy)]
pub enum Verdict {
    /// Nothing the question asks about is reachable.
    Clear,
    /// Something is, or something could not be seen into: the path to it.
    Refused(Reason),
}

/// How one entry was reached from the entry before it.
#[derive(Clone, Copy)]
enum Step {
    /// The type the walk started at.
    Start,
    /// A field of the entry before: of its variant `variant`, at `field`.
    Field { variant: usize, field: usize },
    /// A type argument of a holder, an element, a pointee: no name of its own.
    Inside,
}

#[derive(Clone, Copy)]
struct Node {
    ty: TypeId,
    /// The entry of the nearest [`Mutably`] holder above this one, [`NONE`] outside every cell.
    cell: usize,
    from: usize,
    step: Step,
}

/// The entries of one walk: its work list and its visited set at once.
struct Walk {
    nodes: [Node; NODES],
    len: usize,
    /// Whether an entry did not fit.
    full: bool,
    /// Whether a lookup of one of its own entries found none: a bug in this walk, which is a
    /// refusal ([`lost`]), never a pass.
    lost: bool,
}

impl Walk {
    const fn new(start: TypeId) -> Walk {
        let first = Node { ty: start, cell: NONE, from: NONE, step: Step::Start };
        Walk { nodes: [first; NODES], len: 1, full: false, lost: false }
    }

    /// The entry `at`, or `None` (and the walk marked lost) when it has none.
    const fn node(&mut self, at: usize) -> Option<Node> {
        if at < self.len {
            if let Some(node) = entry(&self.nodes, at) {
                return Some(*node);
            }
        }
        self.lost = true;
        None
    }

    /// Adds `ty`, reached from `from` by `step`, unless the walk has been at it in the same
    /// position (behind a cell or not): the key is the instance's `TypeId`, never its name.
    const fn add(&mut self, ty: TypeId, cell: usize, from: usize, step: Step) {
        // One pass: the entries so far, then the free slot after them.
        let mut at = 0;
        let mut rest: &mut [Node] = &mut self.nodes;
        while let [seen, more @ ..] = rest {
            if at == self.len {
                *seen = Node { ty, cell, from, step };
                self.len += 1;
                return;
            }
            if raw::same(seen.ty, ty) && (seen.cell == NONE) == (cell == NONE) {
                return;
            }
            at += 1;
            rest = more;
        }
        self.full = true;
    }

    /// Adds every type argument of the holder at `at`.
    const fn add_args(&mut self, at: usize, cell: usize) {
        let Some(Node { ty, .. }) = self.node(at) else {
            return;
        };
        let mut arg = 0;
        while arg < raw::args(ty) {
            if let Some(inner) = raw::arg(ty, arg) {
                self.add(inner, cell, at, Step::Inside);
            }
            arg += 1;
        }
    }

    /// Adds every field of every variant of the struct, enum or tuple at `at`.
    const fn add_fields(&mut self, at: usize, cell: usize) {
        let Some(Node { ty, .. }) = self.node(at) else {
            return;
        };
        let mut variant = 0;
        while variant < raw::variants(ty) {
            let mut field = 0;
            while field < raw::fields(ty, variant) {
                self.add(raw::field(ty, variant, field), cell, at, Step::Field { variant, field });
                field += 1;
            }
            variant += 1;
        }
    }

    /// The named steps from the start to the entry `at`, as `` `Owner.field` `` (an enum's as
    /// `` `Owner::Variant.field` ``), with the field's type when `typed`, joined by `, then `.
    /// Whether one was written, or `None` when an entry on the way is missing.
    const fn push_path(&self, out: &mut Reason, at: usize, typed: bool) -> Option<bool> {
        // Written from the start: the entry `from` is always one added before `at`, so the
        // recursion is as deep as the path and ends at the start.
        let Some(node) = entry(&self.nodes, at) else {
            return None;
        };
        let written = if node.from == NONE {
            false
        } else {
            match self.push_path(out, node.from, typed) {
                Some(written) => written,
                None => return None,
            }
        };
        let Step::Field { variant, field } = node.step else {
            return Some(written);
        };
        let Some(&Node { ty: owner, .. }) = entry(&self.nodes, node.from) else {
            return None;
        };
        if written {
            out.push(", then ");
        }
        out.push("`");
        out.push(raw::name(owner));
        if raw::variants(owner) > 1 {
            out.push("::");
            out.push(raw::variant_name(owner, variant));
        }
        out.push(".");
        out.push(raw::field_name(owner, variant, field));
        if typed {
            out.push(": ");
            out.push(raw::name(node.ty));
        }
        out.push("`");
        Some(true)
    }
}

/// How the type `ty` holds what is inside it.
#[derive(Clone, Copy)]
enum Holds {
    Inline,
    Solely,
    Jointly,
    Mutably,
    Nothing,
    /// No [`Owns`] impl: what reflection says.
    Shaped(Shape),
}

const fn holds(ty: TypeId) -> Holds {
    if raw::implements(ty, raw::id::<dyn Owns<Nothing>>()) {
        Holds::Nothing
    } else if raw::implements(ty, raw::id::<dyn Owns<Mutably>>()) {
        Holds::Mutably
    } else if raw::implements(ty, raw::id::<dyn Owns<Jointly>>()) {
        Holds::Jointly
    } else if raw::implements(ty, raw::id::<dyn Owns<Solely>>()) {
        Holds::Solely
    } else if raw::implements(ty, raw::id::<dyn Owns<Inline>>()) {
        Holds::Inline
    } else {
        Holds::Shaped(raw::shape(ty))
    }
}

/// What a type the walk does not see into is called in a refusal.
const fn unseen(shape: Shape) -> &'static str {
    match shape {
        Shape::Pointer => ", a raw pointer, which reflection does not see through",
        Shape::Dyn => ", whose hidden type reflection does not see",
        _ => ", which reflection does not see into (a union, a closure or a foreign type)",
    }
}

/// The answer to `ask` about the type `start`.
pub const fn walk(start: TypeId, ask: &Ask) -> Verdict {
    let mut walk = Walk::new(start);
    let mut at = 0;
    while at < walk.len {
        if let Some(reason) = visit(&mut walk, at, start, ask) {
            return Verdict::Refused(reason);
        }
        if walk.lost {
            return Verdict::Refused(lost(start));
        }
        at += 1;
    }
    if walk.full {
        return Verdict::Refused(too_large(start));
    }
    Verdict::Clear
}

const fn too_large(start: TypeId) -> Reason {
    let mut out = Reason::new();
    out.push("the walk from ");
    out.push_ty(start);
    out.push(" visited more types than it keeps (192); split the type or state the property");
    out
}

/// The refusal of a walk that looked up one of its own entries and found none: a bug in
/// this walk, said as one rather than taken for a pass.
const fn lost(start: TypeId) -> Reason {
    let mut out = Reason::new();
    out.push("the walk from ");
    out.push_ty(start);
    out.push(" looked up an entry it does not hold (a bug in bstd's reachability walk)");
    out
}

/// Expands the entry `at`: adds what it holds, or gives the refusal it is.
const fn visit(walk: &mut Walk, at: usize, start: TypeId, ask: &Ask) -> Option<Reason> {
    let Some(Node { ty, cell, .. }) = walk.node(at) else {
        return Some(lost(start));
    };
    let found = match ask.find {
        Find::Cycle => false,
        Find::Type(what) => raw::same(ty, what),
        Find::Implements(marker) => raw::implements(ty, marker),
    };
    if found {
        let mut out = Reason::new();
        match walk.push_path(&mut out, at, true) {
            Some(true) => out.push(": "),
            Some(false) => {}
            None => return Some(lost(start)),
        }
        out.push_ty(ty);
        out.push(" is ");
        out.push(ask.noun);
        return Some(out);
    }
    let owned = !matches!(ask.through, Through::InPlace);
    let borrowed = matches!(ask.through, Through::Borrowed);
    match holds(ty) {
        Holds::Nothing => None,
        Holds::Inline => {
            walk.add_args(at, cell);
            None
        }
        Holds::Mutably => {
            walk.add_args(at, at);
            None
        }
        Holds::Solely => {
            if owned {
                walk.add_args(at, cell);
            }
            None
        }
        Holds::Jointly => {
            if matches!(ask.find, Find::Cycle) && cell != NONE {
                // A counted pointer that can be set after the value exists. It decides for
                // everything below it: what a deeper one leads back to, this one does too.
                return leads_back(walk, at, start);
            }
            if owned {
                walk.add_args(at, cell);
            }
            None
        }
        Holds::Shaped(shape) => match shape {
            Shape::Leaf => None,
            Shape::Parts => {
                walk.add_fields(at, cell);
                None
            }
            Shape::Elements => {
                if let Some(inner) = raw::inner(ty) {
                    walk.add(inner, cell, at, Step::Inside);
                }
                None
            }
            Shape::Borrow => {
                if borrowed {
                    if let Some(inner) = raw::inner(ty) {
                        walk.add(inner, cell, at, Step::Inside);
                    }
                }
                None
            }
            Shape::Pointer | Shape::Dyn | Shape::Unseen => {
                if matches!(shape, Shape::Pointer) {
                    if borrowed {
                        if let Some(inner) = raw::inner(ty) {
                            walk.add(inner, cell, at, Step::Inside);
                        }
                        return None;
                    }
                    // A pointer in the value's own bytes holds nothing in place.
                    if !owned {
                        return None;
                    }
                }
                if matches!(shape, Shape::Dyn) && cell == NONE && at != 0 {
                    // Its trait answers for whatever is inside, and nothing can put a pointer
                    // back into it later: it is outside every cell. Not asked of the type the
                    // walk started at, which is the type being decided.
                    if let Some(vouch) = ask.vouch {
                        if raw::implements(ty, vouch) {
                            return None;
                        }
                    }
                }
                if !ask.unseen_refuses {
                    return None;
                }
                let mut out = Reason::new();
                match walk.push_path(&mut out, at, false) {
                    Some(true) => out.push(": "),
                    Some(false) => {}
                    None => return Some(lost(start)),
                }
                out.push("a ");
                out.push_ty(ty);
                if matches!(shape, Shape::Dyn) && cell != NONE {
                    let Some(Node { ty: holder, .. }) = walk.node(cell) else {
                        return Some(lost(start));
                    };
                    out.push(" behind ");
                    out.push_ty(holder);
                    out.push(", whose hidden type could hold a pointer back");
                } else {
                    out.push(unseen(shape));
                }
                Some(out)
            }
        },
    }
}

/// Whether a pointee of the counted pointer at `at`, which is behind a cell, leads back to
/// `start` through anything it owns: the refusal naming the path, or `None`. A type on the way
/// that is not seen into is a refusal too: it could.
const fn leads_back(outer: &mut Walk, at: usize, start: TypeId) -> Option<Reason> {
    let Some(pointer) = outer.node(at) else {
        return Some(lost(start));
    };
    let mut arg = 0;
    while arg < raw::args(pointer.ty) {
        if let Some(pointee) = raw::arg(pointer.ty, arg) {
            let mut back = Walk::new(pointee);
            let mut next = 0;
            while next < back.len {
                let Some(Node { ty, .. }) = back.node(next) else {
                    return Some(lost(start));
                };
                let mut hidden = None;
                if raw::same(ty, start) {
                    return Some(cycle(outer, at, &back, next, start, None));
                }
                match holds(ty) {
                    Holds::Nothing => {}
                    Holds::Inline | Holds::Solely | Holds::Jointly | Holds::Mutably => {
                        back.add_args(next, NONE);
                    }
                    Holds::Shaped(shape) => match shape {
                        Shape::Leaf | Shape::Borrow => {}
                        Shape::Parts => back.add_fields(next, NONE),
                        Shape::Elements => {
                            if let Some(inner) = raw::inner(ty) {
                                back.add(inner, NONE, next, Step::Inside);
                            }
                        }
                        Shape::Pointer | Shape::Dyn | Shape::Unseen => hidden = Some(shape),
                    },
                }
                if let Some(shape) = hidden {
                    return Some(cycle(outer, at, &back, next, start, Some(shape)));
                }
                if back.lost {
                    return Some(lost(start));
                }
                next += 1;
            }
            if back.full {
                return Some(too_large(start));
            }
        }
        arg += 1;
    }
    None
}

/// The refusal of a cycle: the path to the counted pointer at `at` of `outer`, the cell it
/// is behind, and the path from its pointee back to `start` (the entry `end` of `back`), or
/// to the type on it that is not seen into (`hidden`).
const fn cycle(
    outer: &Walk,
    at: usize,
    back: &Walk,
    end: usize,
    start: TypeId,
    hidden: Option<Shape>,
) -> Reason {
    let Some(&pointer) = entry(&outer.nodes, at) else {
        return lost(start);
    };
    let Some(&Node { ty: holder, .. }) = entry(&outer.nodes, pointer.cell) else {
        return lost(start);
    };
    let Some(&Node { ty: last, .. }) = entry(&back.nodes, end) else {
        return lost(start);
    };
    let mut out = Reason::new();
    match outer.push_path(&mut out, at, false) {
        Some(true) => {}
        Some(false) => out.push_ty(start),
        None => return lost(start),
    }
    out.push(": a ");
    out.push_ty(pointer.ty);
    out.push(" behind ");
    out.push_ty(holder);
    out.push(" reaches ");
    let Some(named) = back.push_path(&mut out, end, true) else {
        return lost(start);
    };
    match hidden {
        None => {
            if named {
                out.push(", which reaches ");
            }
            out.push_ty(start);
        }
        Some(shape) => {
            if named {
                out.push(", then ");
            }
            out.push("a ");
            out.push_ty(last);
            out.push(unseen(shape));
            out.push(", so it could reach ");
            out.push_ty(start);
        }
    }
    out
}

/// The answer to the question `Q` about `T`.
pub const fn reaches<T: ?Sized, Q: Question>() -> Verdict {
    walk(raw::id::<T>(), &Q::ASK)
}

/// `true` when nothing `Q` asks about is reachable from `T`; otherwise a panic whose message
/// is the path. Written as a `where const { none::<T, Q>() }` condition: the bound that fails
/// shows the message. The panic is the bound's message at compile time, and the one panic
/// this walk keeps: a `where const` condition has no error value to return.
pub const fn none<T: ?Sized, Q: Question>() -> bool {
    match reaches::<T, Q>() {
        Verdict::Clear => true,
        Verdict::Refused(reason) => refuse(reason.as_str()),
    }
}

#[expect(clippy::panic, reason = "compile time only: the message a `where const { none::<T, Q>() }` bound shows; nothing calls it at run time (docs/v2/plan/design/fallible-bstd.md, what is left)")]
const fn refuse(reason: &str) -> ! {
    panic!("{}", reason)
}

/// The question [`Acyclic`] asks: can a value of the type come to own itself? It can only
/// through a counted pointer ([`Jointly`]) that is set after the value exists (behind a
/// [`Mutably`] holder) and whose pointee leads back to the type.
pub struct Cycle;

impl Question for Cycle {
    const ASK: Ask = Ask {
        find: Find::Cycle,
        through: Through::Owned,
        noun: "a cycle",
        unseen_refuses: true,
        vouch: Some(raw::id::<dyn Acyclic>()),
    };
}

/// No chain of owning references leads from a value of the type back to that value, so a
/// counted pointer to one is freed when its last handle is released.
///
/// It is derived: bstd's one blanket impl holds for every type the [`Cycle`] walk clears
/// (`where const { none::<T, Cycle>() }`), decided per instance, and a bound that fails shows
/// the field path the walk found. It is a `#[marker]` trait, so the impls below may overlap
/// the derived one:
///
/// - `unsafe impl Acyclic for X {}`, the trusted assertion for a type the walk refuses (a raw
///   pointer inside, a foreign type), with a `// SAFETY:` saying why no value of `X` can own
///   itself. It answers for `X` only: a type that holds an `X` is still walked through `X`'s
///   fields. To tell the walk how a container holds its contents, implement [`Owns`].
/// - `trait Job: Acyclic`, which makes `dyn Job` a type the walk passes over where it is
///   owned outside every cell: each type inside is acyclic, and nothing can be put into it
///   later. Behind a cell a `dyn` type is refused whatever its trait says, because the type
///   inside could hold a pointer back. (`dyn Tr + Acyclic` is not a type: Rust allows only
///   auto traits after the first.)
///
/// # Safety
///
/// An impl promises the property for every value of the type.
#[marker]
pub unsafe trait Acyclic {}
