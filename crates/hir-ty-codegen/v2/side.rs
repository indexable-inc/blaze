//! What a debugger is told of a function beyond its line rows ([`blaze_codegen::Line`]): its
//! named locals and where they are while it runs, and the code the MIR optimizer inlined
//! into it and where that was called. Kept in the store under
//! [`blaze_codegen::CompiledFunction::side`], in postcard, versioned by [`VERSION`].
//!
//! A local is told by its binding's index in the store of the body it is written in, never by
//! its name: the table is a function of the code its key names, which renaming a local keeps,
//! and whatever writes the local's debug record reads its name from the database then, as the
//! link reads positions from the span tables.

use std::borrow::Cow;

use blaze_format::SpanRef;
use blaze_ids::ContentId;
use blaze_mem::Frame;
use serde::Serializer;
use serde_derive::{Deserialize, Serialize};

use crate::debuginfo::{LineRow, SlotPlaces, VariablePlace};

/// The encoding's version: a change to what is below is a new one.
pub(crate) const VERSION: u32 = 4;

const SIDE_DOMAIN: &str = "blaze v2 side 4";

/// A named local of a function, as its IR was built: in a stack slot, or at an address the
/// slot holds. 60 bytes.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Variable {
    /// Its binding, by index in the store of the body it is written in.
    pub(crate) binding: u32,
    pub(crate) param: bool,
    /// Its type by content ([`super::Hashes::ty`]).
    pub(crate) ty: ContentId,
    pub(crate) slot: u32,
    /// The slot holds an address, the local is this far from it.
    pub(crate) pointer: Option<u32>,
    /// The scope its name is known in, among [`Scopes::parents`]: `None` is all of the
    /// function, as a parameter's is.
    pub(crate) scope: Option<u32>,
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<Variable>() == 60);

/// The scopes of a function's body its named locals are bound in, as the body says them:
/// which code each is of is known once the code is, from its line rows.
#[derive(Default, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Scopes {
    /// The scope each is inside of, which comes before it.
    pub(crate) parents: Vec<Option<u32>>,
    /// By the statement a line row is of (`LineRow::span`): which scope its code is of.
    pub(crate) of_span: Vec<SpanScope>,
    /// The bodies the MIR optimizer inlined into the function, each inside the one it was
    /// inlined into, which comes before it (`MirBody::inlined`'s scopes).
    pub(crate) inlined: Vec<InlinedScope>,
    /// By the statement a line row is of: the innermost of `inlined` its code is of, `None`
    /// for the function's own; empty when nothing is inlined.
    pub(crate) inlined_of_span: Vec<Option<u32>>,
}

/// A body inlined into the function at `call_site`, a statement of the body of `parent`, or
/// of the function's own without one.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct InlinedScope {
    pub(crate) parent: Option<u32>,
    pub(crate) call_site: SpanRef,
}

/// A tag and a `u32`, one a statement of the body.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum SpanScope {
    /// Of the scope of the code before it: code of no expression, or of a pattern.
    Before,
    /// Of none of the scopes.
    Outside,
    /// Of this scope and those it is inside of.
    In(u32),
}

const _: () = assert!(size_of::<SpanScope>() == 8);

/// The table as written, borrowing what it tells of, and as read, borrowing the frame base
/// from the stored bytes.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
pub(crate) struct Side<'a> {
    pub(crate) version: u32,
    /// The DWARF expression of what [`Place::Frame`] and `DW_OP_fbreg` count from.
    #[serde(borrow)]
    pub(crate) frame_base: Cow<'a, [u8]>,
    pub(crate) scopes: Vec<Scope>,
    pub(crate) variables: Vec<Told>,
    pub(crate) inlined: Vec<Inlined>,
}

/// Code of a body inlined into the function: the `DW_TAG_inlined_subroutine` it is told as,
/// whose `DW_AT_call_*` is `call_site`.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
pub(crate) struct Inlined {
    pub(crate) parent: Option<u32>,
    pub(crate) call_site: SpanRef,
    /// As [`Scope::ranges`].
    pub(crate) ranges: Vec<(u32, u32)>,
}

/// A part of the function its names are looked up in: a block, what follows a `let`.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
pub(crate) struct Scope {
    pub(crate) parent: Option<u32>,
    /// The code of the scope and of the scopes inside it, from the first offset up to the
    /// second, in the order of the offsets.
    pub(crate) ranges: Vec<(u32, u32)>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
pub(crate) struct Told {
    /// As [`Variable::binding`].
    pub(crate) binding: u32,
    pub(crate) param: bool,
    pub(crate) ty: ContentId,
    pub(crate) place: Place,
    pub(crate) scope: Option<u32>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
pub(crate) enum Place {
    /// This far from the frame base, all through the function.
    Frame(i64),
    /// What a DWARF expression says, from the first code offset up to the second.
    Ranges(Vec<(u32, u32, Vec<u8>)>),
}

/// [`Side`] as [`encode`] writes it, its lists in the unit's frame: the same fields in the
/// same order, so postcard writes the bytes a `Side` of them would, and [`decode`] reads them
/// back as one.
#[derive(Serialize)]
struct Written<'a, 'f> {
    version: u32,
    frame_base: Cow<'a, [u8]>,
    #[serde(serialize_with = "seq")]
    scopes: Vec<WrittenScope<'f>, &'f Frame>,
    #[serde(serialize_with = "seq")]
    variables: Vec<WrittenTold<'a>, &'f Frame>,
    #[serde(serialize_with = "seq")]
    inlined: Vec<WrittenInlined<'f>, &'f Frame>,
}

/// [`Scope`] as written.
#[derive(Serialize)]
struct WrittenScope<'f> {
    parent: Option<u32>,
    #[serde(serialize_with = "seq")]
    ranges: Vec<(u32, u32), &'f Frame>,
}

/// [`Inlined`] as written.
#[derive(Serialize)]
struct WrittenInlined<'f> {
    parent: Option<u32>,
    call_site: SpanRef,
    #[serde(serialize_with = "seq")]
    ranges: Vec<(u32, u32), &'f Frame>,
}

/// [`Told`] as written: its place borrowed from the function's slots unless it is a pointee's.
#[derive(Serialize)]
struct WrittenTold<'a> {
    binding: u32,
    param: bool,
    ty: ContentId,
    place: WrittenPlace<'a>,
    scope: Option<u32>,
}

/// [`Place`] as written, its variants in `Place`'s order.
#[derive(Serialize)]
enum WrittenPlace<'a> {
    Frame(i64),
    Ranges(Cow<'a, [(u32, u32, Vec<u8>)]>),
}

/// A list in a frame as serde writes a `Vec`: its length, then each item.
fn seq<T: serde::Serialize, S: Serializer>(
    items: &Vec<T, &Frame>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(items.iter())
}

/// The side table of a function whose variables Cranelift put where `slots` says, with its
/// content ID; `None` for one with nothing to tell. The code of each scope is from `lines`,
/// the function's line rows, and `len`, where its code ends.
pub(crate) fn encode(
    variables: &[Variable],
    scopes: &Scopes,
    slots: &SlotPlaces,
    lines: &[LineRow],
    len: u32,
    frame: &Frame,
) -> Result<Option<(Vec<u8>, ContentId)>, String> {
    let mut told: Vec<WrittenTold<'_>, &Frame> = Vec::with_capacity_in(variables.len(), frame);
    told.extend(variables.iter().filter_map(|variable| {
        let place = slots.of(variable.slot)?;
        let place = match (variable.pointer, place) {
            // alloc: a pointee's place, made for it
            (Some(offset), place) => match place.pointee(offset) {
                VariablePlace::Frame(offset) => WrittenPlace::Frame(offset),
                VariablePlace::Ranges(ranges) => WrittenPlace::Ranges(Cow::Owned(ranges)),
            },
            (None, VariablePlace::Frame(offset)) => WrittenPlace::Frame(*offset),
            (None, VariablePlace::Ranges(ranges)) => {
                WrittenPlace::Ranges(Cow::Borrowed(&ranges[..]))
            }
        };
        Some(WrittenTold {
            binding: variable.binding,
            param: variable.param,
            ty: variable.ty,
            place,
            scope: variable.scope,
        })
    }));
    if told.is_empty() && scopes.inlined.is_empty() {
        return Ok(None);
    }
    let inlined = inlined_ranges(scopes, lines, len, frame)?;
    let scopes =
        if told.is_empty() { Vec::new_in(frame) } else { ranges(scopes, lines, len, frame)? };
    let frame_base = Cow::Borrowed(&slots.frame_base[..]);
    let side = Written { version: VERSION, frame_base, scopes, variables: told, inlined };
    // alloc: the error's message
    let size = postcard::experimental::serialized_size(&side).map_err(|e| e.to_string())?;
    // alloc: the side table's bytes, kept in the store: one allocation of their exact size
    let bytes = postcard::to_extend(&side, Vec::<u8>::with_capacity(size))
        .map_err(|e| e.to_string())?; // alloc: the error's message
    let id = ContentId::of(SIDE_DOMAIN, &bytes);
    Ok(Some((bytes, id)))
}

/// [`encode`] of a function's `variables` and `scopes`, where its `code` put them.
pub(crate) fn of(
    variables: &[Variable],
    scopes: &Scopes,
    code: &crate::fragment::Code,
    frame: &Frame,
) -> Result<Option<(Vec<u8>, ContentId)>, String> {
    // alloc: the error's message
    let len = u32::try_from(code.bytes.len()).map_err(|e| e.to_string())?;
    encode(variables, scopes, &code.slots, &code.lines, len, frame)
}

/// The code of each scope: that of each line row whose statement is of it or of a scope
/// inside it, up to the next row's offset or `len`.
fn ranges<'f>(
    scopes: &Scopes,
    lines: &[LineRow],
    len: u32,
    frame: &'f Frame,
) -> Result<Vec<WrittenScope<'f>, &'f Frame>, String> {
    let mut out: Vec<WrittenScope<'f>, &'f Frame> =
        Vec::with_capacity_in(scopes.parents.len(), frame);
    out.extend(
        scopes.parents.iter().map(|&parent| WrittenScope { parent, ranges: Vec::new_in(frame) }),
    );
    if out.is_empty() {
        return Ok(out);
    }
    let mut scope = None;
    let ends = lines.iter().skip(1).map(|row| row.offset).chain([len]);
    for (row, to) in lines.iter().zip(ends) {
        match scopes.of_span.get(row.span as usize) {
            Some(SpanScope::Before) => {}
            Some(SpanScope::Outside) => scope = None,
            Some(&SpanScope::In(it)) => scope = Some(it),
            // alloc: the error's message
            None => return Err(format!("a line row of statement {}, which has no scope", row.span)),
        }
        let len = out.len();
        extend(&mut out, |it| (it.parent, &mut it.ranges), scope, (row.offset, to))
            .map_err(|e| format!("{e} of {len}"))?; // alloc: the error's message
    }
    Ok(out)
}

/// The code of each of `scopes.inlined`, as [`ranges`] has a scope's: that of each line row
/// whose statement is of it or of one inlined inside it.
fn inlined_ranges<'f>(
    scopes: &Scopes,
    lines: &[LineRow],
    len: u32,
    frame: &'f Frame,
) -> Result<Vec<WrittenInlined<'f>, &'f Frame>, String> {
    let mut out: Vec<WrittenInlined<'f>, &'f Frame> =
        Vec::with_capacity_in(scopes.inlined.len(), frame);
    out.extend(scopes.inlined.iter().map(|it| WrittenInlined {
        parent: it.parent,
        call_site: it.call_site,
        ranges: Vec::new_in(frame),
    }));
    if out.is_empty() {
        return Ok(out);
    }
    let ends = lines.iter().skip(1).map(|row| row.offset).chain([len]);
    for (row, to) in lines.iter().zip(ends) {
        let Some(&scope) = scopes.inlined_of_span.get(row.span as usize) else {
            // alloc: the error's message
            return Err(format!("a line row of statement {}, which is of no body", row.span));
        };
        let len = out.len();
        extend(&mut out, |it| (it.parent, &mut it.ranges), scope, (row.offset, to))
            .map_err(|e| format!("inlined {e} of {len}"))?; // alloc: the error's message
    }
    Ok(out)
}

/// Adds the code from `from` up to `to` to `scope` and the scopes it is inside of, joining
/// it to a range it follows.
fn extend<'f, T>(
    out: &mut [T],
    parts: impl Fn(&mut T) -> (Option<u32>, &mut Vec<(u32, u32), &'f Frame>),
    scope: Option<u32>,
    (from, to): (u32, u32),
) -> Result<(), String> {
    if from >= to {
        return Ok(());
    }
    let mut at = scope;
    // Each scope's parent comes before it, so the chain ends within as many steps.
    for _ in 0..out.len() {
        let Some(index) = at else { break };
        // alloc: the error's message
        let Some(it) = out.get_mut(index as usize) else { return Err(format!("scope {index}")) };
        let (parent, ranges) = parts(it);
        match ranges.last_mut() {
            Some(last) if last.1 == from => last.1 = to,
            _ => ranges.push((from, to)),
        }
        at = parent;
    }
    Ok(())
}

/// The table in `bytes`, its frame base borrowed from them.
pub(crate) fn decode(bytes: &[u8]) -> Result<Side<'_>, String> {
    // alloc: the error's message
    let side: Side<'_> = postcard::from_bytes(bytes).map_err(|e| e.to_string())?;
    if side.version != VERSION {
        // alloc: the error's message
        return Err(format!("a side table of version {}, not {VERSION}", side.version));
    }
    Ok(side)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_side_table_reads_back() {
        let slots = SlotPlaces {
            frame_base: vec![0x56],
            slots: vec![(0, VariablePlace::Frame(-16)), (2, VariablePlace::Frame(8))],
        };
        let ty = ContentId::of("test", b"u32");
        let variable = |binding, slot, pointer| Variable {
            binding,
            param: false,
            ty,
            slot,
            pointer,
            scope: None,
        };
        let variables = [variable(7, 0, None), variable(8, 1, None), variable(3, 2, Some(4))];
        let encoded = || {
            blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
                encode(&variables, &Scopes::default(), &slots, &[], 16, frame)
            })
        };
        let (bytes, id) = encoded().expect("encoded").expect("variables");
        let side = decode(&bytes).expect("decoded");
        let bindings: Vec<_> = side.variables.iter().map(|it| it.binding).collect();
        assert_eq!(bindings, [7, 3], "the slot of binding 8 holds nothing");
        assert_eq!(side.variables[0].place, Place::Frame(-16));
        assert!(matches!(side.variables[1].place, Place::Ranges(_)));
        assert_eq!(encoded().expect("encoded").expect("variables").1, id);
    }

    #[test]
    fn nothing_to_tell_is_no_table() {
        let none = blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
            encode(&[], &Scopes::default(), &SlotPlaces::default(), &[], 0, frame)
        });
        assert_eq!(none, Ok(None));
    }

    /// A scope's code is that of its rows and of its inner scopes', joined where they meet;
    /// a row of no expression stays in the scope before it.
    #[test]
    fn a_scope_is_the_code_of_its_rows() {
        let scopes = Scopes {
            parents: vec![None, Some(0)],
            of_span: vec![SpanScope::Outside, SpanScope::In(0), SpanScope::In(1), SpanScope::Before],
            ..Scopes::default()
        };
        let row = |offset, span| LineRow { offset, span };
        let lines = [row(0, 0), row(4, 1), row(8, 2), row(12, 3), row(16, 0)];
        blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
            let ranges = ranges(&scopes, &lines, 20, frame).expect("the ranges");
            assert_eq!((ranges[0].parent, &ranges[0].ranges[..]), (None, &[(4, 16)][..]));
            assert_eq!((ranges[1].parent, &ranges[1].ranges[..]), (Some(0), &[(8, 16)][..]));
            let unknown = [row(0, 9)];
            assert!(super::ranges(&scopes, &unknown, 4, frame).is_err());
        });
    }

    /// An inlined body's code is that of its statements' rows and of the bodies inlined into
    /// it, each told with its call site; a function with no named local still has a table
    /// for them.
    #[test]
    fn inlined_code_is_told_with_its_call_site() {
        let site = |index| SpanRef { anchor: ContentId::of("test", b"caller"), index };
        let scopes = Scopes {
            inlined: vec![
                InlinedScope { parent: None, call_site: site(1) },
                InlinedScope { parent: Some(0), call_site: site(2) },
            ],
            inlined_of_span: vec![None, Some(0), Some(1)],
            ..Scopes::default()
        };
        let row = |offset, span| LineRow { offset, span };
        let lines = [row(0, 0), row(4, 1), row(8, 2), row(12, 1), row(16, 0)];
        let (bytes, _) = blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
            encode(&[], &scopes, &SlotPlaces::default(), &lines, 20, frame)
        })
        .expect("encoded")
        .expect("a table of the inlined code");
        let side = decode(&bytes).expect("decoded");
        assert!(side.variables.is_empty() && side.scopes.is_empty());
        assert_eq!(side.inlined, [
            Inlined { parent: None, call_site: site(1), ranges: vec![(4, 16)] },
            Inlined { parent: Some(0), call_site: site(2), ranges: vec![(8, 12)] },
        ]);
        let unknown = [row(0, 9)];
        blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
            assert!(inlined_ranges(&scopes, &unknown, 4, frame).is_err());
        });
    }

    /// What `encode` writes is, byte for byte, what postcard writes of the `Side` it reads back
    /// as: writing from the frame changes no byte of the format. A warm encode allocates only
    /// the bytes it returns.
    #[test]
    fn a_table_is_postcard_of_its_side_and_allocates_only_its_bytes() {
        let slots = SlotPlaces {
            frame_base: vec![0x56],
            slots: vec![
                (0, VariablePlace::Frame(-16)),
                (2, VariablePlace::Ranges(vec![(0, 8, vec![0x50])])),
            ],
        };
        let ty = ContentId::of("test", b"u32");
        let variable = |binding, slot, scope| Variable {
            binding,
            param: false,
            ty,
            slot,
            pointer: None,
            scope,
        };
        let variables = [variable(0, 0, None), variable(1, 2, Some(1))];
        let scopes = Scopes {
            parents: vec![None, Some(0)],
            of_span: vec![SpanScope::Outside, SpanScope::In(0), SpanScope::In(1)],
            ..Scopes::default()
        };
        let row = |offset, span| LineRow { offset, span };
        let lines = [row(0, 0), row(4, 1), row(8, 2), row(12, 0)];
        let encoded = || {
            blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
                encode(&variables, &scopes, &slots, &lines, 16, frame)
            })
        };
        let (bytes, id) = encoded().expect("encoded").expect("a table");
        let side = decode(&bytes).expect("decoded");
        assert_eq!(postcard::to_allocvec(&side).expect("postcard"), bytes);
        assert_eq!(side.scopes, [
            Scope { parent: None, ranges: vec![(4, 12)] },
            Scope { parent: Some(0), ranges: vec![(8, 12)] },
        ]);
        assert_eq!(side.variables[1].place, Place::Ranges(vec![(0, 8, vec![0x50])]));
        let (warm, warm_id) =
            blaze_alloc_count::assert_at_most(1, "a warm side table: its bytes", encoded)
                .expect("encoded")
                .expect("a table");
        assert_eq!((warm, warm_id), (bytes, id));
    }
}
