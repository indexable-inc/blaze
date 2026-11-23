//! Rule 19 for a callee the walk cannot run (run.rs `callee_refusal`): the refusal names the
//! instance by its PATH and its ground arguments as Rust spells them, never a bare key.
//!
//! The hello path's stop of runner 9 (2026-11-16 20:45) read "`Clone::clone` calls item
//! 0x2980e808e42ac8c994174814ade98748, which is neither a body nor an outside symbol": it named
//! neither the `Self` type the instance was at nor the callee, and it said "calls" at a site
//! where nothing was called (an instance tabled as a fn with no body). So each site says which
//! gap it met ([`CalleeGap`]), and the arguments are shown by [`args_shown`].

use core::fmt;
use core::fmt::Write as _;
use std::boxed::Box;
use std::format;

use blaze_typeck::{ItemKey, List, Mutbl, PNode, PTy, PTys};

/// At most this many type nodes are shown per list; the rest is "..". A refusal is one line.
const MOST_NODES: usize = 64;

/// Which of the walk's sites met the instance it cannot run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalleeGap {
    /// A call resolved to an item the program tables no fn for: the walk (front2 reach.rs
    /// `Instances`) never reached it, and no left-out cause was filed for it.
    NotTabled,
    /// The item is tabled, with no body, and is no outside (foreign) fn: a trait's required
    /// method or a fn whose body did not check, reached as if it had one.
    NoBody,
    /// An instance slotted under a tabled fn whose own key is another item, that is none of a
    /// closure's, a fn shim's or a coroutine's function (run.rs `build_from`).
    NotOwnInstance,
    /// A method word of a vtable resolved to an item with no tabled body.
    VtableWord,
    /// The instance's name passed its cap (symbols.rs `NAME_MOST_NODES`, rustc's default
    /// `type_length_limit` counted in distinct types; `NAME_MOST_BYTES`): `nodes` distinct
    /// types and `bytes` bytes reached when it stopped (rule 18).
    NameTooLarge { nodes: u32, bytes: usize },
}

impl fmt::Display for CalleeGap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CalleeGap::NotTabled => "which no fn row tables (the walk never reached it, and no cause was filed)",
            CalleeGap::NoBody => "which is tabled with no body and is no outside symbol",
            CalleeGap::NotOwnInstance => "slotted under a fn whose key is another item, and none of a closure's, a shim's or a coroutine's function",
            CalleeGap::VtableWord => "a vtable's method word with no tabled body",
            CalleeGap::NameTooLarge { nodes, bytes } => {
                return write!(
                    f,
                    "whose instance name reached {nodes} distinct types and {bytes} bytes, past its cap of {} types (rustc's default `type_length_limit`) or {} bytes (symbols.rs, rule 18)",
                    crate::symbols::NAME_MOST_NODES,
                    crate::symbols::NAME_MOST_BYTES
                );
            }
        })
    }
}

/// The FORM of the first of `args` (an instance's `Self`), in words: what a resolve refusal is
/// classed by (run.rs `filed`), so a census tells "no impl at a tuple" (a builtin impl the
/// language gives) from "no impl at an ADT" (an impl table or bound gap) without one class per
/// type. `"no argument"` for an empty list.
pub fn self_form(v: PTys<'_>, args: List) -> &'static str {
    let Ok(list) = v.list(args) else { return "a list past the ground table" };
    let Some(&first) = list.first() else { return "no argument" };
    let Ok(node) = v.node(first) else { return "a type past the ground table" };
    match node {
        PNode::Adt(..) => "an ADT",
        PNode::Tuple(_) => "a tuple",
        PNode::Record(..) => "a record",
        PNode::Union(_) => "a union",
        PNode::FnDef(..) => "a fn item",
        PNode::FnPtr(..) => "a fn pointer",
        PNode::Closure { .. } => "a closure",
        PNode::Coroutine { .. } | PNode::Future(..) => "an async block",
        PNode::Ref(..) => "a reference",
        PNode::Ptr(..) => "a raw pointer",
        PNode::Array(..) => "an array",
        PNode::Slice(_) => "a slice",
        PNode::Str => "str",
        PNode::Never => "!",
        PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) => "a primitive",
        PNode::Pat(..) => "a pattern type",
        PNode::Dynamic(_) => "a trait object",
        PNode::Alias { .. } => "a projection",
        PNode::Opaque(..) => "an opaque type",
        PNode::Param(_) | PNode::ConstParam(_) | PNode::Infer => "a parameter",
        _ => "another form",
    }
}

/// [`self_form`], and for an ADT its path (`an ADT `protobuf::reflect::RuntimeTypeI32``, the
/// key when the program names none): a resolve refusal at an ADT is an impl the instance did
/// not find for THAT type, so a census classes it by the type (and the callee's path, which
/// names the trait), never as one row of every ADT (census g054702: 1,068 instances in one
/// class "NoImpl at an ADT", nothing to tell its causes apart).
pub fn self_shown<P: crate::seam::FrontEnd>(program: &P, v: PTys<'_>, args: List) -> Box<str> {
    let form = self_form(v, args);
    let adt = v.list(args).ok().and_then(|l| l.first().copied()).and_then(|t| match v.node(t) {
        Ok(PNode::Adt(key, _)) => Some(key),
        _ => None,
    });
    match adt {
        Some(key) => match program.item_named(key) {
            Some(path) => format!("{form} `{path}`").into_boxed_str(),
            None => format!("{form} {:#x}", key.0).into_boxed_str(),
        },
        None => Box::from(form),
    }
}

/// One step of [`args_shown`]'s work list: a type to spell, or words to write.
enum Step {
    Ty(PTy),
    Words(&'static str),
}

/// `args` spelled as Rust writes types, comma-separated in brackets (`[Reach]`,
/// `[&alloc::sync::Arc<bstd::sync::Lock<World>>]`): an ADT by `name` (its path, else its key in
/// hex), every other node by its form. By a work list, at most [`MOST_NODES`] nodes, never by
/// recursion. A list the table does not hold is said to be malformed, never guessed.
pub fn args_shown(v: PTys<'_>, args: List, name: &dyn Fn(ItemKey) -> Option<Box<str>>) -> Box<str> {
    let Ok(list) = v.list(args) else { return Box::from("[a list past the ground table]") };
    let mut out = String::new();
    let mut work: Vec<Step> = Vec::new();
    work.push(Step::Words("]"));
    push_list(&mut work, list, ", ");
    work.push(Step::Words("["));
    let mut nodes = 0usize;
    while let Some(step) = work.pop() {
        let t = match step {
            Step::Words(w) => {
                out.push_str(w);
                continue;
            }
            Step::Ty(t) => t,
        };
        nodes += 1;
        if nodes > MOST_NODES {
            out.push_str("..");
            continue;
        }
        let Ok(node) = v.node(t) else {
            out.push_str("<a type past the ground table>");
            continue;
        };
        match node {
            PNode::Adt(k, l) => {
                match name(k) {
                    Some(p) => out.push_str(&p),
                    None => {
                        let _ = write!(out, "<ADT {:#x}>", k.0);
                    }
                }
                push_generic(&mut work, v, l);
            }
            PNode::FnDef(k, l) => {
                match name(k) {
                    Some(p) => {
                        let _ = write!(out, "fn item {p}");
                    }
                    None => {
                        let _ = write!(out, "fn item {:#x}", k.0);
                    }
                }
                push_generic(&mut work, v, l);
            }
            PNode::Ref(m, a) => {
                out.push_str(if m == Mutbl::Mut { "&mut " } else { "&" });
                work.push(Step::Ty(a));
            }
            PNode::Ptr(m, a) => {
                out.push_str(if m == Mutbl::Mut { "*mut " } else { "*const " });
                work.push(Step::Ty(a));
            }
            PNode::Slice(a) => {
                work.push(Step::Words("]"));
                work.push(Step::Ty(a));
                out.push('[');
            }
            PNode::Array(a, n) => {
                work.push(Step::Words("]"));
                work.push(Step::Ty(n));
                work.push(Step::Words("; "));
                work.push(Step::Ty(a));
                out.push('[');
            }
            PNode::Tuple(l) => match v.list(l) {
                Ok([]) => out.push_str("()"),
                Ok(&[one]) => {
                    work.push(Step::Words(",)"));
                    work.push(Step::Ty(one));
                    out.push('(');
                }
                Ok(all) => {
                    work.push(Step::Words(")"));
                    push_list(&mut work, all, ", ");
                    out.push('(');
                }
                Err(_) => out.push_str("(a list past the ground table)"),
            },
            PNode::Record(_, l) => match v.list(l) {
                Ok(all) => {
                    work.push(Step::Words(")"));
                    push_list(&mut work, all, ", ");
                    out.push_str("record(");
                }
                Err(_) => out.push_str("record(a list past the ground table)"),
            },
            PNode::Union(l) => match v.list(l) {
                Ok(all) => {
                    work.push(Step::Words(")"));
                    push_list(&mut work, all, " | ");
                    out.push_str("union(");
                }
                Err(_) => out.push_str("union(a list past the ground table)"),
            },
            PNode::Closure { owner, ordinal, .. } => match name(owner) {
                Some(p) => {
                    let _ = write!(out, "<closure {ordinal} of {p}>");
                }
                None => {
                    let _ = write!(out, "<closure {ordinal} of {:#x}>", owner.0);
                }
            },
            PNode::Coroutine { owner, ordinal, .. } => match name(owner) {
                Some(p) => {
                    let _ = write!(out, "<async block {ordinal} of {p}>");
                }
                None => {
                    let _ = write!(out, "<async block {ordinal} of {:#x}>", owner.0);
                }
            },
            PNode::Bool => out.push_str("bool"),
            PNode::Char => out.push_str("char"),
            PNode::Str => out.push_str("str"),
            PNode::Never => out.push('!'),
            // Every other form (an integer or float type, a pointer to a fn, a `dyn`, a
            // pattern type, a projection, a constant) by the table's own words for it.
            other => {
                let _ = write!(out, "{other:?}");
            }
        }
    }
    out.into_boxed_str()
}

/// Pushes `list` so it pops in order, `sep` between.
fn push_list(work: &mut Vec<Step>, list: &[PTy], sep: &'static str) {
    for (i, &t) in list.iter().enumerate().rev() {
        work.push(Step::Ty(t));
        if i != 0 {
            work.push(Step::Words(sep));
        }
    }
}

/// Pushes `<..>` around `l`'s types when it has any.
fn push_generic(work: &mut Vec<Step>, v: PTys<'_>, l: List) {
    match v.list(l) {
        Ok([]) => {}
        Ok(all) => {
            work.push(Step::Words(">"));
            push_list(work, all, ", ");
            work.push(Step::Words("<"));
        }
        Err(_) => work.push(Step::Words("<a list past the ground table>")),
    }
}

#[cfg(test)]
mod tests {
    //! The printer on the shapes the hello path's clones are at (bstd io.rs `Reach`'s derived
    //! `clone`: `&Arc<Lock<World>>`, `&Vars`, `&usize`, `&&'static [&'static str]`, `()`), and
    //! a control: an ADT the namer has no path for is shown by its whole key, never dropped.
    use blaze_typeck::{PNode, PortableTys, UintTy};

    use super::*;

    #[test]
    fn args_are_spelled_as_rust_writes_them() {
        // The test's one allocator, made at its top: a frame, its memory from the OS.
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let mut g = PortableTys::new_in(erased.handle());
        let (world, lock, arc) = (ItemKey(1), ItemKey(2), ItemKey(3));
        let w = g.intern(PNode::Adt(world, List::EMPTY)).unwrap();
        let wl = g.intern_list(&[w]).unwrap();
        let l = g.intern(PNode::Adt(lock, wl)).unwrap();
        let ll = g.intern_list(&[l]).unwrap();
        let a = g.intern(PNode::Adt(arc, ll)).unwrap();
        let ra = g.intern(PNode::Ref(Mutbl::Not, a)).unwrap();
        let us = g.intern(PNode::Uint(UintTy::Usize)).unwrap();
        let s = g.intern(PNode::Str).unwrap();
        let rs = g.intern(PNode::Ref(Mutbl::Not, s)).unwrap();
        let sl = g.intern(PNode::Slice(rs)).unwrap();
        let rsl = g.intern(PNode::Ref(Mutbl::Not, sl)).unwrap();
        let unit = g.intern(PNode::Tuple(List::EMPTY)).unwrap();
        let args = g.intern_list(&[ra, rsl, unit]).unwrap();
        let name = |k: ItemKey| -> Option<Box<str>> {
            match k.0 {
                1 => Some(Box::from("bstd::io::World")),
                2 => Some(Box::from("bstd::lock::Lock")),
                3 => Some(Box::from("alloc::sync::Arc")),
                _ => None,
            }
        };
        let shown = args_shown(g.view(), args, &name);
        assert_eq!(&*shown, "[&alloc::sync::Arc<bstd::lock::Lock<bstd::io::World>>, &[&str], ()]");
        // The control: no path for the ADT, so its whole key is shown, and `usize` by its form.
        let none = |_: ItemKey| -> Option<Box<str>> { None };
        let one = g.intern(PNode::Adt(ItemKey(0x2980e808e42ac8c994174814ade98748), List::EMPTY)).unwrap();
        let args = g.intern_list(&[one, us]).unwrap();
        let shown = args_shown(g.view(), args, &none);
        assert!(shown.starts_with("[<ADT 0x2980e808e42ac8c994174814ade98748>, "), "{shown}");
        println!("args_shown: {shown}");
    }
}
