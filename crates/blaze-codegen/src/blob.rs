//! Templates as the store keeps them ([`blaze_cas::Kind::Code`]): little-endian, every length
//! before what it counts, one way to write each template. Reading checks every length against
//! what is left and every tag against what it can be, so bytes that are not a template are an
//! error, never a panic or an allocation of what a length claims.

use blaze_format::{
    SpanRef,
    cursor::{self, Put, Reader, Wanted},
};
use blaze_ids::ContentId;
use blaze_items::CodeHash;
use blaze_mem::{Frame, Tag, frame};

use crate::{
    Covered, Line, ProfileRecord, Profiled, Reloc, Relocs,
    content::{arch_of, arch_tag},
    relocs::RelocsView,
    fold::{Constant, Masked},
    template::{
        DataView, Ref, StaticTemplate, Template, TemplateData, TemplateInit, TemplateUnwind, TemplateView, UnwindView,
    },
    wasm,
};

const TEMPLATE_MAGIC: [u8; 4] = *b"BZT6";
const STATIC_MAGIC: [u8; 4] = *b"BZS4";

/// Why bytes are not a template.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BlobError {
    pub at: usize,
    pub what: &'static str,
}

impl std::fmt::Display for BlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not a template at byte {}: {}", self.at, self.what)
    }
}

impl From<cursor::Error> for BlobError {
    fn from(error: cursor::Error) -> BlobError {
        let what = match error.wanted {
            Wanted::Bytes { .. } => "ends early",
            Wanted::Count { .. } => "a count longer than what is left",
            Wanted::End { .. } => "bytes after the end",
            _ => error.as_str(),
        };
        BlobError { at: error.at, what }
    }
}

fn put_len(out: &mut Vec<u8>, len: usize) {
    out.put_u32(u32::try_from(len).expect("a template's parts are under 4 GiB"));
}

/// Rows as their 16 bytes, then each reference, then the wide addends.
fn put_relocs(out: &mut Vec<u8>, relocs: &Relocs<Ref>) {
    put_len(out, relocs.len());
    for row in relocs.rows() {
        out.put_bytes(&row.to_le_bytes());
    }
    put_len(out, relocs.targets().len());
    for target in relocs.targets() {
        match *target {
            Ref::Place(at) => {
                out.put_u8(0);
                out.put_u32(at);
            }
            Ref::Data(at) => {
                out.put_u8(1);
                out.put_u32(at);
            }
            Ref::Position(SpanRef { anchor, index }) => {
                out.put_u8(2);
                out.put_bytes(&anchor.0);
                out.put_u32(index);
            }
            Ref::Extern(at) => {
                out.put_u8(3);
                out.put_u32(at);
            }
            Ref::Coverage(array) => {
                out.put_u8(4);
                out.put_u32(array);
            }
            Ref::Counter => out.put_u8(5),
            Ref::ProfileCounters => out.put_u8(6),
            Ref::Own => out.put_u8(7),
        }
    }
    put_len(out, relocs.wide().len());
    for &(row, addend) in relocs.wide() {
        out.put_u32(row);
        out.put_i64(addend);
    }
}

fn put_data(out: &mut Vec<u8>, data: &[TemplateData]) {
    put_len(out, data.len());
    for it in data {
        out.put_u32(it.align);
        out.put_bytes_u32(&it.bytes);
        put_relocs(out, &it.relocs);
        out.put_u8(u8::from(it.writable));
    }
}

fn put_index(out: &mut Vec<u8>, it: Option<u32>) {
    match it {
        Some(at) => {
            out.put_u8(1);
            out.put_u32(at);
        }
        None => out.put_u8(0),
    }
}

fn put_unwind(out: &mut Vec<u8>, unwind: Option<&TemplateUnwind>) {
    match unwind {
        None => out.put_u8(0),
        Some(TemplateUnwind::Dwarf { code_align, data_align, return_address, cie, fde, personality, lsda }) => {
            out.put_u8(1);
            out.put_u32(*code_align);
            out.put_i32(*data_align);
            out.put_u16(*return_address);
            out.put_bytes_u32(cie);
            out.put_bytes_u32(fde);
            put_index(out, *personality);
            put_index(out, *lsda);
        }
        Some(TemplateUnwind::Windows { info, handler, lsda }) => {
            out.put_u8(2);
            out.put_bytes_u32(info);
            put_index(out, *handler);
            put_index(out, *lsda);
        }
    }
}

fn put_lines(out: &mut Vec<u8>, lines: &[Line]) {
    put_len(out, lines.len());
    for line in lines {
        out.put_u32(line.at);
        out.put_bytes(&line.span.anchor.0);
        out.put_u32(line.span.index);
    }
}

fn put_coverage(out: &mut Vec<u8>, covered: Option<&Covered>) {
    let Some(Covered { counters, flags, guards, pcs }) = covered else {
        out.put_u8(0);
        return;
    };
    out.put_u8(1);
    out.put_u32(*counters);
    out.put_u32(*flags);
    out.put_u32(*guards);
    put_len(out, pcs.len());
    for (at, pc_flags) in pcs {
        out.put_u32(*at);
        out.put_u64(*pc_flags);
    }
}

fn put_profile(out: &mut Vec<u8>, profiled: Option<&Profiled>) {
    match profiled {
        None => out.put_u8(0),
        Some(Profiled::Counted(ProfileRecord { name, name_ref, hash, counters, version })) => {
            out.put_u8(1);
            out.put_bytes(name);
            out.put_u64(*name_ref);
            out.put_u64(*hash);
            out.put_u32(*counters);
            out.put_u64(*version);
        }
        Some(Profiled::Used { mismatched }) => {
            out.put_u8(2);
            out.put_u8(u8::from(*mismatched));
        }
    }
}

/// The bytes [`put_profile`] writes of a [`Profiled::Counted`], after its tag.
const RECORD_BYTES: usize = 16 + 8 + 8 + 4 + 8;

/// A template's fields read off a [`Reader`], each tag checked against what it can be.
struct In<'a>(Reader<'a>);

/// The bytes of one relocation row and one wide addend, and the least one reference and one
/// data object take: what bounds a count.
const RELOC_BYTES: usize = 16;
const REF_BYTES: usize = 1;
const WIDE_BYTES: usize = 4 + 8;
const LINE_BYTES: usize = 4 + 32 + 4;
const DATA_BYTES: usize = 4 + 4 + 4 + 4 + 4 + 1;

impl<'a> In<'a> {
    fn fail<T>(&self, what: &'static str) -> Result<T, BlobError> {
        Err(BlobError { at: self.0.at(), what })
    }

    fn id(&mut self) -> Result<ContentId, BlobError> {
        Ok(ContentId(*self.0.array()?))
    }

    /// A `u32` length, then that many bytes, copied: a length past what is left is a count
    /// longer than what is left.
    fn bytes(&mut self) -> Result<Vec<u8>, BlobError> {
        let len = self.0.count_u32(1)?;
        Ok(self.0.take(len)?.to_vec()) // alloc: the template owns its bytes
    }

    /// A `u32` length, then that many bytes of UTF-8.
    fn string(&mut self) -> Result<String, BlobError> {
        let at = self.0.at();
        // alloc: the template owns its names, moved out of the bytes read for them
        String::from_utf8(self.bytes()?).map_err(|_| BlobError { at, what: "a name that is not UTF-8" })
    }

    /// A `u32` length, then that many wasm value types, a byte each.
    fn values(&mut self) -> Result<Vec<wasm::ValType>, BlobError> {
        let at = self.0.at();
        let bytes = self.slice()?;
        bytes
            .iter()
            .map(|&it| wasm::ValType::from_byte(it).ok_or(BlobError { at, what: "no such wasm value type" }))
            // alloc: the template owns its types
            .collect()
    }

    /// A `u32` length, then that many bytes, borrowed from the blob.
    fn slice(&mut self) -> Result<&'a [u8], BlobError> {
        let len = self.0.count_u32(1)?;
        Ok(self.0.take(len)?)
    }

    fn row(&mut self) -> Result<Reloc, BlobError> {
        match Reloc::from_le_bytes(*self.0.array::<RELOC_BYTES>()?) {
            Some(row) => Ok(row),
            None => self.fail("no such relocation kind, or padding"),
        }
    }

    fn target(&mut self) -> Result<Ref, BlobError> {
        Ok(match self.0.u8()? {
            0 => Ref::Place(self.0.u32()?),
            1 => Ref::Data(self.0.u32()?),
            2 => Ref::Position(SpanRef { anchor: self.id()?, index: self.0.u32()? }),
            3 => Ref::Extern(self.0.u32()?),
            4 => match self.0.u32()? {
                array if array < Covered::ARRAYS => Ref::Coverage(array),
                _ => return self.fail("no such coverage array"),
            },
            5 => Ref::Counter,
            6 => Ref::ProfileCounters,
            7 => Ref::Own,
            _ => return self.fail("no such reference"),
        })
    }

    fn wide(&mut self) -> Result<(u32, i64), BlobError> {
        Ok((self.0.u32()?, self.0.i64()?))
    }

    fn relocs(&mut self) -> Result<Relocs<Ref>, BlobError> {
        let count = self.0.count_u32(RELOC_BYTES)?;
        let mut rows = Vec::with_capacity(count);
        for _ in 0..count {
            rows.push(self.row()?);
        }
        let count = self.0.count_u32(REF_BYTES)?;
        let mut targets = Vec::with_capacity(count);
        for _ in 0..count {
            targets.push(self.target()?);
        }
        let count = self.0.count_u32(WIDE_BYTES)?;
        let mut wide = Vec::with_capacity(count);
        for _ in 0..count {
            wide.push(self.wide()?);
        }
        Relocs::from_parts(rows, targets, wide).map_err(|what| BlobError { at: self.0.at(), what })
    }

    /// [`In::relocs`], its parts in `frame` and checked there.
    fn relocs_in(&mut self, frame: &'a Frame) -> Result<RelocsView<'a, Ref>, BlobError> {
        let count = self.0.count_u32(RELOC_BYTES)?;
        let mut rows = Vec::with_capacity_in(count, frame);
        for _ in 0..count {
            rows.push(self.row()?);
        }
        let count = self.0.count_u32(REF_BYTES)?;
        let mut targets = Vec::with_capacity_in(count, frame);
        for _ in 0..count {
            targets.push(self.target()?);
        }
        let count = self.0.count_u32(WIDE_BYTES)?;
        let mut wide = Vec::with_capacity_in(count, frame);
        for _ in 0..count {
            wide.push(self.wide()?);
        }
        RelocsView::from_parts(rows.leak(), targets.leak(), wide.leak(), frame)
            .map_err(|what| BlobError { at: self.0.at(), what })
    }

    fn data_in(&mut self, frame: &'a Frame) -> Result<&'a [DataView<'a>], BlobError> {
        let count = self.0.count_u32(DATA_BYTES)?;
        let mut data = Vec::with_capacity_in(count, frame);
        for _ in 0..count {
            let align = self.0.u32()?;
            let bytes = self.slice()?;
            let relocs = self.relocs_in(frame)?;
            let writable = self.bool()?;
            data.push(DataView { align, bytes, relocs, writable });
        }
        Ok(data.leak())
    }

    fn data(&mut self) -> Result<Vec<TemplateData>, BlobError> {
        let count = self.0.count_u32(DATA_BYTES)?;
        let mut data = Vec::with_capacity(count);
        for _ in 0..count {
            let align = self.0.u32()?;
            let bytes = self.bytes()?;
            let relocs = self.relocs()?;
            let writable = self.bool()?;
            data.push(TemplateData { align, bytes, relocs, writable });
        }
        Ok(data)
    }

    fn bool(&mut self) -> Result<bool, BlobError> {
        match self.0.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => self.fail("not a bool"),
        }
    }

    fn index(&mut self) -> Result<Option<u32>, BlobError> {
        Ok(match self.bool()? {
            true => Some(self.0.u32()?),
            false => None,
        })
    }

    /// A function's unwind information, its bytes borrowed from the blob.
    fn unwind_view(&mut self) -> Result<Option<UnwindView<'a>>, BlobError> {
        Ok(match self.0.u8()? {
            0 => None,
            1 => Some(UnwindView::Dwarf {
                code_align: self.0.u32()?,
                data_align: self.0.i32()?,
                return_address: self.0.u16()?,
                cie: self.slice()?,
                fde: self.slice()?,
                personality: self.index()?,
                lsda: self.index()?,
            }),
            2 => Some(UnwindView::Windows { info: self.slice()?, handler: self.index()?, lsda: self.index()? }),
            _ => return self.fail("no such unwind information"),
        })
    }

    fn line(&mut self) -> Result<Line, BlobError> {
        let at = self.0.u32()?;
        Ok(Line { at, span: SpanRef { anchor: self.id()?, index: self.0.u32()? } })
    }

    fn lines_in(&mut self, frame: &'a Frame) -> Result<&'a [Line], BlobError> {
        let count = self.0.count_u32(LINE_BYTES)?;
        let mut lines = Vec::with_capacity_in(count, frame);
        for _ in 0..count {
            lines.push(self.line()?);
        }
        Ok(lines.leak())
    }

    fn coverage(&mut self) -> Result<Option<Covered>, BlobError> {
        if !self.bool()? {
            return Ok(None);
        }
        let (counters, flags, guards) = (self.0.u32()?, self.0.u32()?, self.0.u32()?);
        let count = self.0.count_u32(4 + 8)?;
        let mut pcs = Vec::with_capacity(count);
        for _ in 0..count {
            let at = self.0.u32()?;
            pcs.push((at, self.0.u64()?));
        }
        Ok(Some(Covered { counters, flags, guards, pcs }))
    }

    fn profile(&mut self) -> Result<Option<Profiled>, BlobError> {
        Ok(match self.0.u8()? {
            0 => None,
            1 => Some(Profiled::Counted(ProfileRecord {
                name: *self.0.array()?,
                name_ref: self.0.u64()?,
                hash: self.0.u64()?,
                counters: self.0.u32()?,
                version: self.0.u64()?,
            })),
            2 => Some(Profiled::Used { mismatched: self.bool()? }),
            _ => return self.fail("no such profile"),
        })
    }

    fn magic(&mut self, magic: [u8; 4]) -> Result<(), BlobError> {
        if *self.0.array::<4>()? != magic {
            return self.fail("another kind of blob, or another version of this one");
        }
        Ok(())
    }

    fn end(&self) -> Result<(), BlobError> {
        Ok(self.0.finish()?)
    }
}

pub fn encode(template: &Template) -> Vec<u8> {
    // alloc: the blob, kept in the store: one allocation of its exact size
    let mut out = Vec::with_capacity(encoded_len(template));
    out.put_bytes(&TEMPLATE_MAGIC);
    out.put_u8(arch_tag(template.arch));
    out.put_u32(template.align);
    out.put_bytes_u32(&template.code);
    put_relocs(&mut out, &template.relocs);
    put_data(&mut out, &template.data);
    put_len(&mut out, template.inlined.len());
    for code in &template.inlined {
        out.put_bytes(&code.0.0);
    }
    match &template.side {
        Some(side) => {
            out.put_u8(1);
            out.put_bytes(&side.0);
        }
        None => out.put_u8(0),
    }
    put_unwind(&mut out, template.unwind.as_ref());
    put_lines(&mut out, &template.lines);
    out.put_u32(template.refs);
    put_len(&mut out, template.externs.len());
    for name in &template.externs {
        out.put_bytes_u32(name.as_bytes());
    }
    put_coverage(&mut out, template.coverage.as_ref());
    put_profile(&mut out, template.profile.as_ref());
    debug_assert_eq!(out.len(), encoded_len(template));
    out
}

/// How many bytes [`encode`] writes of `template`: each term is what its `put_*` writes.
fn encoded_len(template: &Template) -> usize {
    let bytes = |it: &[u8]| 4 + it.len();
    let relocs = |relocs: &Relocs<Ref>| {
        let targets = relocs.targets().iter().map(|it| match it {
            Ref::Position(_) => 1 + 32 + 4,
            Ref::Place(_) | Ref::Data(_) | Ref::Extern(_) | Ref::Coverage(_) => 1 + 4,
            Ref::Counter | Ref::ProfileCounters | Ref::Own => 1,
        });
        4 + RELOC_BYTES * relocs.len() + 4 + targets.sum::<usize>() + 4 + WIDE_BYTES * relocs.wide().len()
    };
    let index = |it: &Option<u32>| if it.is_some() { 1 + 4 } else { 1 };
    let unwind = match &template.unwind {
        None => 1,
        Some(TemplateUnwind::Dwarf { cie, fde, personality, lsda, .. }) => {
            1 + 4 + 4 + 2 + bytes(cie) + bytes(fde) + index(personality) + index(lsda)
        }
        Some(TemplateUnwind::Windows { info, handler, lsda }) => 1 + bytes(info) + index(handler) + index(lsda),
    };
    let data = 4 + template.data.iter().map(|it| 4 + bytes(&it.bytes) + relocs(&it.relocs) + 1).sum::<usize>();
    let externs = 4 + template.externs.iter().map(|it| bytes(it.as_bytes())).sum::<usize>();
    let coverage = match &template.coverage {
        None => 1,
        Some(covered) => 1 + 3 * 4 + 4 + covered.pcs.len() * (4 + 8),
    };
    let profile = match &template.profile {
        None => 1,
        Some(Profiled::Counted(_)) => 1 + RECORD_BYTES,
        Some(Profiled::Used { .. }) => 1 + 1,
    };
    TEMPLATE_MAGIC.len()
        + 1
        + 4
        + bytes(&template.code)
        + relocs(&template.relocs)
        + data
        + 4
        + 32 * template.inlined.len()
        + 1
        + if template.side.is_some() { 32 } else { 0 }
        + unwind
        + 4
        + LINE_BYTES * template.lines.len()
        + 4
        + externs
        + coverage
        + profile
}

/// The template in `bytes`, borrowed from them, its lists in `frame`: what instantiating a
/// stored template reads, copying each part once, into the function.
pub fn decode_view<'a>(bytes: &'a [u8], frame: &'a Frame) -> Result<TemplateView<'a>, BlobError> {
    let mut input = In(Reader::new(bytes));
    input.magic(TEMPLATE_MAGIC)?;
    let Some(arch) = arch_of(input.0.u8()?) else { return input.fail("no such architecture") };
    let align = input.0.u32()?;
    if !align.is_power_of_two() {
        return input.fail("an alignment that is not a power of two");
    }
    let code = input.slice()?;
    let relocs = input.relocs_in(frame)?;
    let data = input.data_in(frame)?;
    let count = input.0.count_u32(32)?;
    let mut inlined = Vec::with_capacity_in(count, frame);
    for _ in 0..count {
        inlined.push(CodeHash(input.id()?));
    }
    let side = match input.bool()? {
        true => Some(input.id()?),
        false => None,
    };
    let unwind = input.unwind_view()?;
    let lines = input.lines_in(frame)?;
    let refs = input.0.u32()?;
    let count = input.0.count_u32(4)?;
    let mut externs = Vec::with_capacity_in(count, frame);
    for _ in 0..count {
        let name = input.slice()?;
        match std::str::from_utf8(name) {
            Ok(name) => externs.push(name),
            Err(_) => return input.fail("a symbol name that is not UTF-8"),
        }
    }
    // alloc: its PC table, which the function made of the view takes as it is
    let coverage = input.coverage()?;
    let profile = input.profile()?;
    input.end()?;
    Ok(TemplateView {
        arch,
        align,
        code,
        relocs,
        data,
        inlined: inlined.leak(),
        side,
        unwind,
        lines,
        refs,
        externs: externs.leak(),
        coverage,
        profile,
    })
}

/// The template in `bytes`.
pub fn decode(bytes: &[u8]) -> Result<Template, BlobError> {
    frame::task(Tag::Codegen, |frame| decode_view(bytes, frame).map(TemplateView::owned))
}

pub fn encode_static(template: &StaticTemplate) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.put_bytes(&STATIC_MAGIC);
    out.put_u32(template.align);
    match &template.init {
        TemplateInit::Bytes { bytes, relocs } => {
            out.put_u8(0);
            out.put_bytes_u32(bytes);
            put_relocs(&mut out, relocs);
        }
        TemplateInit::Zeroed { size } => {
            out.put_u8(1);
            out.put_u64(*size);
        }
    }
    put_data(&mut out, &template.data);
    out.put_u8(template.mutable as u8);
    out.put_u8(template.thread_local as u8);
    match &template.section {
        Some(section) => {
            out.put_u8(1);
            out.put_bytes_u32(section.as_bytes());
        }
        None => out.put_u8(0),
    }
    out.put_u32(template.imports.len() as u32);
    for (name, import) in &template.imports {
        out.put_bytes_u32(name.as_bytes());
        out.put_bytes_u32(import.module.as_bytes());
        out.put_bytes_u32(import.field.as_bytes());
        for values in [&import.ty.params, &import.ty.results] {
            // alloc: per import of a static, its types as bytes
            out.put_bytes_u32(&values.iter().map(|&it| it as u8).collect::<Vec<u8>>());
        }
    }
    out.put_u32(template.refs);
    out
}

pub fn decode_static(bytes: &[u8]) -> Result<StaticTemplate, BlobError> {
    let mut input = In(Reader::new(bytes));
    input.magic(STATIC_MAGIC)?;
    let align = input.0.u32()?;
    if !align.is_power_of_two() {
        return input.fail("an alignment that is not a power of two");
    }
    let init = match input.0.u8()? {
        0 => TemplateInit::Bytes { bytes: input.bytes()?, relocs: input.relocs()? },
        1 => TemplateInit::Zeroed { size: input.0.u64()? },
        _ => return input.fail("no such initializer"),
    };
    let data = input.data()?;
    let mutable = input.bool()?;
    let thread_local = input.bool()?;
    let section = match input.bool()? {
        // alloc: the template owns its section name, moved out of the bytes read for it
        true => match String::from_utf8(input.bytes()?) {
            Ok(it) => Some(it),
            Err(_) => return input.fail("a section name that is not UTF-8"),
        },
        false => None,
    };
    let count = input.0.count_u32(4 * 5)?;
    let mut imports = Vec::with_capacity(count);
    for _ in 0..count {
        let name = input.string()?;
        let module = input.string()?;
        let field = input.string()?;
        let params = input.values()?;
        let results = input.values()?;
        imports.push((name, wasm::Import { module, field, ty: wasm::Signature { params, results } }));
    }
    let refs = input.0.u32()?;
    input.end()?;
    Ok(StaticTemplate { align, init, data, mutable, thread_local, section, imports, refs })
}

const MASKED_MAGIC: [u8; 4] = *b"BZM1";

/// What a function is with its constants masked (#1292), or that it can't be folded: kept per
/// instance, so a warm build groups without building any IR.
pub fn encode_masked(masked: Option<&Masked>) -> Vec<u8> {
    // The magic, the tag, the shape and its length, the size, the count, and at most 33 bytes
    // a constant.
    let most = masked.map_or(0, |it| it.shape.len() + 33 * it.constants.len());
    let mut out = Vec::with_capacity(MASKED_MAGIC.len() + 1 + 4 + 4 + 4 + most);
    out.put_bytes(&MASKED_MAGIC);
    let Some(masked) = masked else {
        out.put_u8(0);
        return out;
    };
    out.put_u8(1);
    out.put_bytes_u32(&masked.shape);
    out.put_u32(masked.size);
    put_len(&mut out, masked.constants.len());
    for constant in &masked.constants {
        match *constant {
            Constant::Int { bits, value } => {
                out.put_u8(0);
                out.put_u8(bits);
                out.put_u128(value);
            }
            Constant::F32(bits) => {
                out.put_u8(1);
                out.put_u32(bits);
            }
            Constant::F64(bits) => {
                out.put_u8(2);
                out.put_u64(bits);
            }
            Constant::Place(at) => {
                out.put_u8(3);
                out.put_u32(at);
            }
            Constant::Data(id) => {
                out.put_u8(4);
                out.put_bytes(&id.0);
            }
        }
    }
    out
}

pub fn decode_masked(bytes: &[u8]) -> Result<Option<Masked>, BlobError> {
    let mut input = In(Reader::new(bytes));
    input.magic(MASKED_MAGIC)?;
    if !input.bool()? {
        input.end()?;
        return Ok(None);
    }
    let shape = input.bytes()?;
    let size = input.0.u32()?;
    let count = input.0.count_u32(5)?;
    let mut constants = Vec::with_capacity(count);
    for _ in 0..count {
        constants.push(match input.0.u8()? {
            0 => {
                let bits = input.0.u8()?;
                Constant::Int { bits, value: input.0.u128()? }
            }
            1 => Constant::F32(input.0.u32()?),
            2 => Constant::F64(input.0.u64()?),
            3 => Constant::Place(input.0.u32()?),
            4 => Constant::Data(input.id()?),
            _ => return input.fail("no such constant"),
        });
    }
    input.end()?;
    Ok(Some(Masked { shape, constants, size }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Arch, RelocKind, Relocation};

    #[test]
    fn a_masked_shape_reads_back() {
        let masked = Masked {
            shape: vec![1, 2, 3],
            constants: vec![
                Constant::Int { bits: 64, value: 24 },
                Constant::F32(0x3f80_0000),
                Constant::F64(1),
                Constant::Place(4),
                Constant::Data(ContentId([9; 32])),
            ],
            size: 40,
        };
        assert_eq!(decode_masked(&encode_masked(Some(&masked))).unwrap(), Some(masked));
        assert_eq!(decode_masked(&encode_masked(None)).unwrap(), None);
    }

    fn template() -> Template {
        let reloc = |target, addend| Relocation { at: 3, kind: RelocKind::Call26, target, addend };
        Template {
            arch: Arch::Aarch64,
            align: 4,
            code: vec![1, 2, 3, 4, 5, 6, 7, 8],
            relocs: vec![
                reloc(Ref::Place(2), -8),
                reloc(Ref::Data(0), 1 << 40),
                reloc(Ref::Position(SpanRef { anchor: ContentId([4; 32]), index: 9 }), -8),
                reloc(Ref::Extern(0), -8),
                reloc(Ref::Coverage(2), i64::MIN),
                reloc(Ref::Counter, 8),
                reloc(Ref::ProfileCounters, 16),
                reloc(Ref::Own, 24),
                reloc(Ref::Place(2), 0),
            ]
            .into(),
            data: vec![TemplateData {
                align: 8,
                bytes: vec![9; 8],
                relocs: vec![reloc(Ref::Place(0), -8)].into(),
                writable: false,
            }],
            inlined: vec![CodeHash(ContentId([5; 32]))],
            side: Some(ContentId([6; 32])),
            unwind: Some(TemplateUnwind::Dwarf {
                code_align: 4,
                data_align: -8,
                return_address: 30,
                cie: vec![0x0c, 31, 0],
                fde: vec![0x44, 0x0e, 16],
                personality: Some(1),
                lsda: Some(0),
            }),
            lines: vec![Line { at: 4, span: SpanRef { anchor: ContentId([7; 32]), index: 3 } }],
            refs: 3,
            externs: vec!["bcmp".to_owned()],
            coverage: Some(Covered { counters: 2, flags: 0, guards: 8, pcs: vec![(0, 1), (4, 0)] }),
            profile: Some(Profiled::Counted(ProfileRecord {
                name: [8; 16],
                name_ref: 1 << 60,
                hash: 3,
                counters: 5,
                version: 10,
            })),
        }
    }

    #[test]
    fn windows_unwinding_and_none_read_back() {
        let mut it = template();
        it.unwind = Some(TemplateUnwind::Windows { info: vec![1, 2], handler: None, lsda: Some(0) });
        assert_eq!(decode(&encode(&it)).unwrap(), it);
        it.unwind = None;
        it.lines.clear();
        it.coverage = None;
        assert_eq!(decode(&encode(&it)).unwrap(), it);
    }

    #[test]
    fn a_template_reads_back() {
        let template = template();
        assert_eq!(decode(&encode(&template)).unwrap(), template);
    }

    #[test]
    fn a_static_reads_back() {
        let reloc = Relocation { at: 8, kind: RelocKind::Abs64, target: Ref::Data(0), addend: 0 };
        let template = StaticTemplate {
            align: 16,
            init: TemplateInit::Bytes { bytes: vec![0; 24], relocs: vec![reloc].into() },
            data: vec![
                TemplateData { align: 1, bytes: b"hi".to_vec(), relocs: Relocs::new(), writable: false },
                TemplateData { align: 4, bytes: vec![7; 4], relocs: Relocs::new(), writable: true },
            ],
            mutable: true,
            thread_local: true,
            section: Some("__DATA,__mod_init_func".to_owned()),
            imports: vec![(
                "fd_write".to_owned(),
                crate::wasm::Import {
                    module: "wasi_snapshot_preview1".to_owned(),
                    field: "fd_write".to_owned(),
                    ty: crate::wasm::Signature {
                        params: vec![crate::wasm::ValType::I32; 4],
                        results: vec![crate::wasm::ValType::I32],
                    },
                },
            )],
            refs: 0,
        };
        let bytes = encode_static(&template);
        assert_eq!(decode_static(&bytes).unwrap(), template);
        assert!(decode(&bytes).is_err());
        for len in 0..bytes.len() {
            assert!(decode_static(&bytes[..len]).is_err(), "{len} bytes read as a static");
        }
        let zeroed = StaticTemplate { init: TemplateInit::Zeroed { size: 1 << 30 }, section: None, ..template };
        assert_eq!(decode_static(&encode_static(&zeroed)).unwrap(), zeroed);
    }

    #[test]
    fn every_cut_is_an_error() {
        let bytes = encode(&template());
        for len in 0..bytes.len() {
            assert!(decode(&bytes[..len]).is_err(), "{len} bytes read as a template");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(decode(&longer).is_err());
    }

    #[test]
    fn a_count_past_the_end_allocates_nothing() {
        let mut bytes = TEMPLATE_MAGIC.to_vec();
        bytes.push(0);
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode(&bytes).unwrap_err().what, "a count longer than what is left");
    }

    #[test]
    fn a_short_field_fails_where_its_read_starts() {
        let bytes = encode(&template());
        let at = TEMPLATE_MAGIC.len() + 1;
        let want = BlobError { at, what: "ends early" };
        assert_eq!(decode(&bytes[..at + 2]).unwrap_err(), want);
        let viewed = frame::task(Tag::Codegen, |frame| decode_view(&bytes[..at + 2], frame).map(|_| ()));
        assert_eq!(viewed.unwrap_err(), want);
    }

    /// Reading a stored template in place allocates nothing on a warm thread but the coverage
    /// table the function takes as it is: its code, data bytes and names are the blob's, its
    /// lists the frame's. It is the template `decode` gives.
    #[test]
    fn a_warm_view_of_a_blob_allocates_only_its_coverage() {
        let template = template();
        let bytes = encode(&template);
        let read = || {
            frame::task(Tag::Codegen, |frame| {
                decode_view(&bytes, frame)
                    .map(|view| (view.code.len(), view.relocs.len(), view.data.len(), view.externs.len()))
            })
        };
        let cold = read().expect("a view");
        let coverage = u64::from(template.coverage.as_ref().is_some_and(|it| !it.pcs.is_empty()));
        let warm = blaze_alloc_count::assert_at_most(coverage, "a warm read of a stored template", read).expect("a view");
        assert_eq!(cold, warm);
        assert_eq!(decode(&bytes).expect("decoded"), template);
        let mut none = template;
        none.coverage = None;
        let bytes = encode(&none);
        let read = || frame::task(Tag::Codegen, |frame| decode_view(&bytes, frame).map(|view| view.code.len()));
        read().expect("a view");
        blaze_alloc_count::assert_at_most(0, "a warm read of a template with no coverage", read).expect("a view");
    }

    /// `encode` sizes its bytes exactly: one allocation, of `encoded_len`, whatever unwind
    /// information and coverage the template has.
    #[test]
    fn encode_writes_into_one_allocation_of_its_length() {
        let windows = TemplateUnwind::Windows { info: vec![1, 2], handler: Some(1), lsda: None };
        let profiles = [template().profile, Some(Profiled::Used { mismatched: true }), None];
        for unwind in [template().unwind, Some(windows), None] {
            for coverage in [template().coverage, None] {
                for profile in profiles {
                    let template = Template { unwind: unwind.clone(), coverage: coverage.clone(), profile, ..template() };
                    let (bytes, used) = blaze_alloc_count::measure(|| encode(&template));
                    assert_eq!(bytes.len(), encoded_len(&template));
                    assert_eq!(used.allocations, 1);
                    assert_eq!(decode(&bytes).expect("decoded"), template);
                }
            }
        }
    }

    #[test]
    fn every_flipped_byte_is_an_error_or_another_template() {
        let template = template();
        let bytes = encode(&template);
        for at in 0..bytes.len() {
            let mut flipped = bytes.clone();
            flipped[at] ^= 0xff;
            if let Ok(other) = decode(&flipped) {
                assert_ne!(other, template);
                assert_eq!(encode(&other), flipped);
            }
        }
    }
}
