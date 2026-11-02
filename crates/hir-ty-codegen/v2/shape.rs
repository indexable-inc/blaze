//! The canonical bytes of a prepared function ([`Shape`]): its IR as Cranelift's own `Hash` of
//! the function's stencil writes it (what Cranelift's incremental cache keys on), then each
//! name of the IR by its place among the references or the number of its own data, then that
//! data, the positions and the named locals. Nothing in it is a number of the run or of the
//! order requests were made in, so two instances whose IR is the same but for which places
//! they name are one shape (`Vec<A>::drop` and `Vec<B>::drop` of a pointer-sized `A` and `B`).
//!
//! Cost: one pass over the IR and its data into one buffer sized up front, with no clone of
//! the function, no printing and no `String`; about 4 to 8 bytes per value and instruction.

use std::hash::{Hash as _, Hasher};

use blaze_codegen::{Backend, Unit, backend::Shape};
use blaze_format::cursor::Put;
use blaze_mem::Frame;
use cranelift_codegen::ir::{self, UserExternalName};

use super::{
    Built, DataPlace, Function, Places,
    side::{Scopes, SpanScope, Variable},
};
use crate::{
    ConstBlob, DataIdentity,
    fragment::{self, DataRequest, Hint, RelocName, Requests},
};

const VERSION: u8 = 10;

/// Where data is in a shape, as the names of its IR have it.
mod namespace {
    pub(super) const PLACE: u32 = 0;
    pub(super) const STATIC: u32 = 2;
    pub(super) const POSITION: u32 = 3;
    pub(super) const OWN: u32 = 4;
    /// A body built in: Cranelift's inliner puts the body where it is called and leaves its
    /// name declared, which no instruction names any more (and the emit refuses one that is).
    pub(super) const BUILT_IN: u32 = 5;
}

/// The parts of a shape, each once, in this order.
mod part {
    pub(super) const IR: u8 = 1;
    pub(super) const NAMES: u8 = 2;
    pub(super) const NAMED: u8 = 3;
    pub(super) const ASM: u8 = 4;
    pub(super) const DATA: u8 = 5;
    pub(super) const SPANS: u8 = 6;
    pub(super) const VARIABLES: u8 = 7;
    pub(super) const HINT: u8 = 8;
    pub(super) const VALUE: u8 = 9;
    pub(super) const SCOPES: u8 = 10;
    pub(super) const INLINED: u8 = 11;
    pub(super) const UNSANITIZED: u8 = 12;
    pub(super) const FEATURES: u8 = 13;
    pub(super) const WASM: u8 = 14;
}

pub(crate) fn of(unit: &Unit, function: &Function, frame: &Frame) -> Result<Shape, String> {
    let places = &function.places;
    let requests = function.requests();
    let data = Data::of(places, requests, frame)?;
    // The bytes are hashed into the shape's key ([`Shape::of`]) once written: the thread's
    // buffer's, never the shape's.
    blaze_mem::scratch::with_bytes(|bytes| {
        bytes.reserve(estimate(function));
        let mut out = Out(bytes);
        out.u8(VERSION);
        out.u32(places.refs);
        match &function.code {
            Built::Ir(pending) => {
                let func = &pending.func;
                out.u8(part::IR);
                func.stencil.hash(&mut out);
                out.u8(part::NAMES);
                let names = func.params.user_named_funcs();
                out.u32(names.len() as u32);
                for (_, name) in names.iter() {
                    let canonical = match name.namespace {
                        0 => UserExternalName::new(namespace::PLACE, place(places, name.index)?),
                        1 => data.name(name.index)?,
                        fragment::INLINED => UserExternalName::new(namespace::BUILT_IN, 0),
                        // alloc: the error's message
                        other => return Err(format!("a name in the unknown namespace {other}")),
                    };
                    out.name(canonical);
                }
                out.u8(part::NAMED);
                out.u32(pending.named.len() as u32);
                for slot in &pending.named {
                    out.u32(slot.as_u32());
                }
                // Only LLVM is told what parameters point to (`dereferenceable`, `align`):
                // Cranelift's code is the same for `&A` and `&B`, so its shape is too.
                if matches!(unit.backend, Backend::Llvm { .. }) {
                    pending.wants.pointees.hash(&mut out);
                }
            }
            Built::Assembled(code, _) => {
                out.u8(part::ASM);
                out.bytes(&code.bytes);
                out.u64(code.align);
                out.u32(code.relocs.len() as u32);
                for reloc in code.relocs.iter() {
                    out.u32(reloc.offset);
                    out.u8(reloc_kind(reloc.kind));
                    out.u64(reloc.addend as u64);
                    match reloc.name {
                        RelocName::Func(index) => {
                            let at = place(places, index)?;
                            out.name(UserExternalName::new(namespace::PLACE, at));
                        }
                        RelocName::Data(index) => out.name(data.name(index)?),
                        // alloc: the error's message
                        other => return Err(format!("assembly naming {other:?}")),
                    }
                }
                let typed = code.relocs.iter().filter_map(|it| Some((it.offset, it.elf_type?)));
                out.u32(code.relocs.iter().filter(|it| it.elf_type.is_some()).count() as u32);
                for (at, kind) in typed {
                    out.u32(at);
                    out.u32(kind);
                }
            }
        }
        data.write(&mut out, requests, places)?;
        out.u8(part::SPANS);
        out.u32(function.spans.len() as u32);
        for span in &function.spans {
            out.raw(&span.anchor.0);
            out.u32(span.index);
        }
        out.u8(part::VARIABLES);
        variables(&mut out, &function.variables);
        out.u8(part::SCOPES);
        scopes(&mut out, &function.scopes);
        // Equal shapes are one template, which says what its code has built in.
        out.u8(part::INLINED);
        out.u32(function.inlined.len() as u32);
        for code in &function.inlined {
            out.raw(&code.0.0);
        }
        out.u8(part::HINT);
        out.u8(match function.hint {
            Hint::None => 0,
            Hint::Inline => 1,
            Hint::Always => 2,
            Hint::Never => 3,
        });
        out.u8(part::UNSANITIZED);
        out.u8(function.unsanitized.0);
        // A `#[target_feature]` function's code is its own set's: one IR, two sets, two codes.
        out.u8(part::FEATURES);
        let features = function.features.names();
        out.u32(features.len() as u32);
        for feature in features {
            out.bytes(feature.as_bytes());
        }
        // A wasm function's code holds the symbols it imports and exports, which are names of
        // its references rather than places: one template a header.
        out.u8(part::WASM);
        wasm(&mut out, &function.wasm);
        Ok(Shape::of(unit, out.0))
    })
}

/// The canonical bytes of a static's value, as [`of`] has a function's data.
pub(crate) fn value(blob: &ConstBlob, requests: &Requests, places: &Places) -> Result<Vec<u8>, String> {
    blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
        let data = Data::of(places, requests, frame)?;
        // alloc: the static's key bytes, returned to its caller
        let mut bytes = Vec::with_capacity(64 + 2 * blob.bytes.len());
        let mut out = Out(&mut bytes);
        out.u8(VERSION);
        out.u32(places.refs);
        data.write(&mut out, requests, places)?;
        out.u8(part::VALUE);
        data.blob(&mut out, blob, places)?;
        Ok(bytes)
    })
}

/// A shape's size, give or take: the stencil's `Hash` writes a few words per value, instruction
/// and block.
fn estimate(function: &Function) -> usize {
    let data: usize = function
        .requests()
        .datas
        .iter()
        .map(|it| match it {
            DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } => {
                blob.bytes.len() + 16 * (blob.relocs.len() + blob.fn_relocs.len())
            }
            _ => 40,
        })
        .sum();
    let code = match &function.code {
        Built::Ir(pending) => {
            let dfg = &pending.func.dfg;
            24 * dfg.num_insts() + 8 * dfg.num_values() + 16 * dfg.num_blocks()
        }
        Built::Assembled(code, _) => code.bytes.len() + 24 * code.relocs.len(),
    };
    let scopes = 5 * function.scopes.parents.len()
        + 5 * function.scopes.of_span.len()
        + 41 * function.scopes.inlined.len()
        + 5 * function.scopes.inlined_of_span.len();
    let inlined = 32 * function.inlined.len();
    256 + code + data + 36 * function.spans.len() + 53 * function.variables.len() + scopes + inlined
}

/// The shape being written. As a [`Hasher`] it keeps every byte it is given: the stencil's
/// derived `Hash` is a serialization of it, so equal bytes are equal IR, with no hash between.
struct Out<'b>(&'b mut Vec<u8>);

impl Out<'_> {
    fn u8(&mut self, it: u8) {
        self.0.push(it);
    }

    fn u32(&mut self, it: u32) {
        self.0.put_u32(it);
    }

    fn u64(&mut self, it: u64) {
        self.0.put_u64(it);
    }

    fn raw(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.0.put_bytes_u64(bytes);
    }

    fn name(&mut self, name: UserExternalName) {
        self.u32(name.namespace);
        self.u32(name.index);
    }
}

impl Hasher for Out<'_> {
    fn finish(&self) -> u64 {
        self.0.len() as u64
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
}

fn place(places: &Places, index: u32) -> Result<u32, String> {
    // alloc: the error's message
    places.funcs.get(index as usize).copied().ok_or_else(|| format!("function {index} has no place"))
}

/// What each data request is in a shape: a place, or the number of the object among the
/// function's own data and positions in the order they are first asked for.
struct Data<'f> {
    names: &'f [UserExternalName],
}

impl<'f> Data<'f> {
    fn of(places: &Places, requests: &Requests, frame: &'f Frame) -> Result<Data<'f>, String> {
        if places.datas.len() != requests.datas.len() {
            // alloc: the error's message
            return Err(format!(
                "{} data requests and {} places",
                requests.datas.len(),
                places.datas.len()
            ));
        }
        let (mut own, mut positions) = (0, 0);
        let names = frame.alloc_slice_from_iter(places.datas.iter().map(|place| match place {
            DataPlace::Place(place) => UserExternalName::new(namespace::STATIC, *place),
            DataPlace::Position(_) => {
                positions += 1;
                UserExternalName::new(namespace::POSITION, positions - 1)
            }
            DataPlace::Own | DataPlace::Text(_) | DataPlace::Nowhere => {
                own += 1;
                UserExternalName::new(namespace::OWN, own - 1)
            }
        }));
        Ok(Data { names })
    }

    fn name(&self, index: u32) -> Result<UserExternalName, String> {
        // alloc: the error's message
        self.names.get(index as usize).cloned().ok_or_else(|| format!("data {index} has no place"))
    }

    /// The content of the function's own data and its positions, in their order.
    fn write(&self, out: &mut Out<'_>, requests: &Requests, places: &Places) -> Result<(), String> {
        out.u8(part::DATA);
        out.u32(places.datas.len() as u32);
        for (index, place) in places.datas.iter().enumerate() {
            match place {
                DataPlace::Place(_) => out.u8(0),
                DataPlace::Position(span) => {
                    out.u8(1);
                    out.raw(&span.anchor.0);
                    out.u32(span.index);
                }
                DataPlace::Text(it) => {
                    out.u8(2);
                    out.bytes(it.as_bytes());
                }
                DataPlace::Nowhere => out.u8(3),
                DataPlace::Own => {
                    let (blob, tag) = match &requests.datas[index] {
                        DataRequest::Blob { blob, identity: DataIdentity::Writable } => (blob, 5),
                        DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } => (blob, 4),
                        // alloc: the error's message
                        _ => return Err(format!("data {index} is its own but has no blob")),
                    };
                    out.u8(tag);
                    self.blob(out, blob, places)?;
                }
            }
        }
        Ok(())
    }

    fn blob(&self, out: &mut Out<'_>, blob: &ConstBlob, places: &Places) -> Result<(), String> {
        if !blob.type_ids.is_empty() {
            // alloc: the error's message
            return Err("a constant with a `TypeId` not filled in".to_owned());
        }
        out.u64(blob.align);
        out.bytes(&blob.bytes);
        out.u32(blob.relocs.len() as u32);
        for &(at, data, addend) in &blob.relocs {
            out.u32(at);
            out.name(self.name(data.as_u32())?);
            out.u64(addend as u64);
        }
        out.u32(blob.fn_relocs.len() as u32);
        for &(at, func) in &blob.fn_relocs {
            out.u32(at);
            out.u32(place(places, func.as_u32())?);
        }
        Ok(())
    }
}

/// A relocation kind of an assembly function by number: Cranelift's `Reloc` has no `Hash`.
fn reloc_kind(kind: cranelift_codegen::binemit::Reloc) -> u8 {
    // A fieldless enum: its discriminant is the kind, stable within one Cranelift, which the
    // unit's settings name.
    kind as u8
}

fn variables(out: &mut Out<'_>, variables: &[Variable]) {
    out.u32(variables.len() as u32);
    for variable in variables {
        out.u32(variable.binding);
        out.u8(u8::from(variable.param));
        out.raw(&variable.ty.0);
        out.u32(variable.slot);
        option(out, variable.pointer);
        option(out, variable.scope);
    }
}

fn scopes(out: &mut Out<'_>, scopes: &Scopes) {
    out.u32(scopes.parents.len() as u32);
    for &parent in &scopes.parents {
        option(out, parent);
    }
    out.u32(scopes.of_span.len() as u32);
    for &of in &scopes.of_span {
        match of {
            SpanScope::Before => out.u8(0),
            SpanScope::Outside => out.u8(1),
            SpanScope::In(scope) => {
                out.u8(2);
                out.u32(scope);
            }
        }
    }
    out.u32(scopes.inlined.len() as u32);
    for inlined in &scopes.inlined {
        option(out, inlined.parent);
        out.raw(&inlined.call_site.anchor.0);
        out.u32(inlined.call_site.index);
    }
    out.u32(scopes.inlined_of_span.len() as u32);
    for &of in &scopes.inlined_of_span {
        option(out, of);
    }
}

fn wasm(out: &mut Out<'_>, header: &super::wasm::Header) {
    out.u32(header.imports.len() as u32);
    for (name, import) in &header.imports {
        out.bytes(name.as_bytes());
        out.bytes(import.module.as_bytes());
        out.bytes(import.field.as_bytes());
        for values in [&import.ty.params, &import.ty.results] {
            out.u32(values.len() as u32);
            out.0.extend(values.iter().map(|&it| it as u8));
        }
    }
    match &header.export {
        Some(name) => {
            out.u8(1);
            out.bytes(name.as_bytes());
        }
        None => out.u8(0),
    }
}

fn option(out: &mut Out<'_>, it: Option<u32>) {
    match it {
        Some(it) => {
            out.u8(1);
            out.u32(it);
        }
        None => out.u8(0),
    }
}
