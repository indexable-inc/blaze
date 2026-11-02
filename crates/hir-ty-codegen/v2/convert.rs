//! The IR builder's compiled code ([`fragment::Code`] and its [`Requests`]) as v2 keeps it
//! ([`CompiledFunction`]): every relocation at a target of the build, and the function's own
//! data by content, each object pointing only at the ones before it.

use blaze_codegen::{Arch, CompiledFunction, Data, InstanceKey, Line, RelocKind, Relocation, Relocs, Target};
use blaze_format::SpanRef;
use blaze_ids::ContentId;
use blaze_mem::Frame;
use cranelift_codegen::{binemit, isa::TargetIsa};

use super::places::{PERSONALITY, libcall_name};
use crate::{
    ConstBlob, DataIdentity,
    fragment::{self, DataRequest, FuncRequest, RelocName, Requests},
};

/// What a data request is in the build.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an ID but for an `Extern`'s name; each relocation to the data owns one
pub(crate) enum DataTarget {
    /// A static, a symbol, a position.
    Target(Target),
    /// The function's own, made of the request's blob.
    Own,
    /// A `&str` of this text: `file!()`.
    Text(String),
    /// A `core::panic::Location` of no place the program has a span of.
    Nowhere,
}

/// What each request of a function is, by its number: asked once a relocation, so it is
/// static dispatch, never `dyn` ([`super::places::Placed`] in a build).
pub(crate) trait Resolver {
    fn func(&self, index: u32, request: &FuncRequest) -> Result<Target, String>;
    fn data(&self, index: u32, request: &DataRequest) -> Result<DataTarget, String>;
    /// A symbol the backend calls on its own: a libcall, the personality routine.
    fn symbol(&self, name: &str) -> Result<Target, String>;
    /// The folded body a fold's thunk calls ([`super::fold::BODY`]).
    fn body(&self) -> Result<Target, String>;
}

/// A [`Resolver`] of four closures, for a test.
#[cfg(test)]
pub(crate) struct Resolve<F, D, S, B> {
    pub(crate) func: F,
    pub(crate) data: D,
    pub(crate) symbol: S,
    pub(crate) body: B,
}

#[cfg(test)]
impl<F, D, S, B> Resolver for Resolve<F, D, S, B>
where
    F: Fn(u32, &FuncRequest) -> Result<Target, String>,
    D: Fn(u32, &DataRequest) -> Result<DataTarget, String>,
    S: Fn(&str) -> Result<Target, String>,
    B: Fn() -> Result<Target, String>,
{
    fn func(&self, index: u32, request: &FuncRequest) -> Result<Target, String> {
        (self.func)(index, request)
    }

    fn data(&self, index: u32, request: &DataRequest) -> Result<DataTarget, String> {
        (self.data)(index, request)
    }

    fn symbol(&self, name: &str) -> Result<Target, String> {
        (self.symbol)(name)
    }

    fn body(&self) -> Result<Target, String> {
        (self.body)()
    }
}

/// The function `code` is, `requests` being what its relocations name and `spans` what its
/// line rows do. Its bytes move into the result, uncopied but on wasm32 where `wasm` has
/// something to say; its tables are in `frame`, the unit's, and its lists of relocations and
/// line rows go back to the thread.
#[allow(clippy::too_many_arguments)]
pub(crate) fn function(
    frame: &Frame,
    key: InstanceKey,
    arch: Arch,
    // dyn: cranelift's API takes `&dyn TargetIsa`; its backends are private, one made per process
    isa: &dyn TargetIsa,
    mut code: fragment::Code,
    requests: &Requests,
    resolve: &impl Resolver,
    spans: &[SpanRef],
    side: Option<ContentId>,
    wasm: &super::wasm::Header,
) -> Result<CompiledFunction, String> {
    let is_wasm = arch == Arch::Wasm32;
    let moved = if is_wasm {
        let (bytes, moved) = super::wasm::with_header(std::mem::take(&mut code.bytes), wasm)?;
        code.bytes = bytes;
        moved
    } else if !wasm.is_empty() {
        // alloc: an error
        return Err(format!("a wasm header of imports and an export for code of {arch:?}"));
    } else {
        0
    };
    // alloc: none until it has data of its own, which the result keeps
    let mut own = OwnData { data: Vec::new(), word: word(arch), owner: None };
    let size = code.bytes.len();
    let names: Relocs<RelocName> = Relocs::try_from_iter_in(
        frame,
        code.relocs.iter().map(|reloc| {
            let kind = match reloc.elf_type {
                Some(elf) => elf_kind(elf)?,
                None if is_wasm => super::wasm::reloc_kind(reloc.kind, reloc.name)?,
                None => reloc_kind(reloc.kind)?,
            };
            let (target, addend) = match reloc.name {
                RelocName::Own(offset) => (RelocName::Own(0), own_addend(offset, reloc.addend, size)?),
                name => (name, reloc.addend),
            };
            // alloc: an error
            let at = reloc.offset.checked_add(moved).ok_or("a relocation past 4 GiB of code")?;
            Ok::<_, String>(Relocation { at, kind, target, addend })
        }),
    )?;
    let named = names.targets().iter().filter_map(|name| match *name {
        RelocName::Data(index) => Some(index as usize),
        _ => None,
    });
    let datas = own.requests(frame, requests, resolve, named)?;
    let relocs = names.view().try_map_in(frame, |&name| reloc_target(key, name, requests, &datas, resolve))?;
    let unwind = match &code.unwind {
        Some(unwind) => {
            let personality = || resolve.symbol(PERSONALITY);
            let (unwind, lsda) = super::unwind::convert(isa, unwind, personality)?;
            if let Some(lsda) = lsda {
                own.push(lsda);
            }
            Some(unwind)
        }
        None => None,
    };
    let mut lines = Vec::with_capacity(code.lines.len());
    for row in &code.lines {
        let Some(&span) = spans.get(row.span as usize) else {
            let at = row.span;
            // alloc: an error
            return Err(format!("a line row of statement {at}, which the body has no span of"));
        };
        lines.push(Line { at: row.offset, span });
    }
    // alloc: an error
    let align = u32::try_from(code.align).map_err(|e| e.to_string())?;
    if !align.is_power_of_two() {
        // alloc: an error
        return Err(format!("code aligned to {align}, not a power of two"));
    }
    let fragment::Code { bytes, relocs: rows, lines: line_rows, .. } = code;
    crate::spare::keep_relocs(rows);
    crate::spare::keep_lines(line_rows);
    Ok(CompiledFunction {
        key,
        arch,
        align,
        code: bytes,
        relocs,
        data: own.data,
        // alloc: empty, allocates nothing
        inlined: Vec::new(),
        coverage: None,
        profile: None,
        side,
        unwind,
        lines,
    })
}

/// The target of each user name `func` declares, by its index, as [`function`] makes a
/// relocation at it a target (namespace 0 a function request, 1 a data request), and the
/// function's own data those name, each object pointing only at the ones before it. A name
/// [`function`] has no target for is the error it would be.
pub(crate) fn names(
    frame: &Frame,
    key: InstanceKey,
    arch: Arch,
    func: &cranelift_codegen::ir::Function,
    requests: &Requests,
    resolve: &impl Resolver,
) -> Result<(Vec<Target>, Vec<Data>), String> {
    // alloc: none until it has data of its own, which the caller keeps
    let mut own = OwnData { data: Vec::new(), word: word(arch), owner: None };
    let declared = func.params.user_named_funcs();
    let named = declared.values().filter(|it| it.namespace == 1).map(|it| it.index as usize);
    let datas = own.requests(frame, requests, resolve, named)?;
    // alloc: per function handed out, its names, kept by the caller
    let mut names = Vec::with_capacity(declared.len());
    for name in declared.values() {
        let name = RelocName::user(name.namespace, name.index);
        names.push(reloc_target(key, name, requests, &datas, resolve)?);
    }
    Ok((names, own.data))
}

/// A static's value: its own data, the target of each of its requests, and its blob's
/// relocations at those.
pub(crate) fn value(
    frame: &Frame,
    arch: Arch,
    blob: &ConstBlob,
    requests: &Requests,
    owner: &ContentId,
    resolve: &impl Resolver,
) -> Result<(Vec<Data>, Relocs), String> {
    // alloc: none until it has data of its own, which the caller keeps
    let mut own = OwnData { data: Vec::new(), word: word(arch), owner: Some(*owner) };
    let named = blob.relocs.iter().map(|&(_, data, _)| data.as_u32() as usize);
    let targets = own.requests(frame, requests, resolve, named)?;
    let relocs = own.relocs(blob, requests.datas.len(), &targets, requests, resolve)?;
    Ok((own.data, relocs))
}

/// An address in data: how many bytes it takes, and the kind of a relocation that writes one
/// of data there and one of a function.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Word {
    pub(crate) size: usize,
    pub(crate) data: RelocKind,
    pub(crate) func: RelocKind,
}

/// On wasm32 an address is four bytes: of data where it is in linear memory, of a function
/// its slot in the table.
pub(crate) fn word(arch: Arch) -> Word {
    match arch {
        Arch::X86_64 | Arch::Aarch64 => Word { size: 8, data: RelocKind::Abs64, func: RelocKind::Abs64 },
        Arch::Wasm32 => Word { size: 4, data: RelocKind::WasmMemory, func: RelocKind::WasmTable },
    }
}

/// A function's own data, in the order it is made: an object points only at the ones before.
struct OwnData {
    data: Vec<Data>,
    word: Word,
    /// The item key of the static whose value this is, whose writable objects are its own
    /// ([`Data::owned`]); none for a function, whose data is read-only.
    owner: Option<ContentId>,
}

impl OwnData {
    fn push(&mut self, data: Data) -> Target {
        let target = Target::Data(data.id);
        if !self.data.iter().any(|it| it.id == data.id) {
            self.data.push(data);
        }
        target
    }

    /// The target of each data request that `named` (the requests the code or value names)
    /// reaches, making the function's own as they come: what nothing reaches is none of it.
    fn requests<'f>(
        &mut self,
        frame: &'f Frame,
        requests: &Requests,
        resolve: &impl Resolver,
        named: impl Iterator<Item = usize>,
    ) -> Result<Vec<Option<Target>, &'f Frame>, String> {
        let used: &mut [bool] = frame.alloc_slice_fill_with(requests.datas.len(), |_| false);
        for index in named {
            *used
                .get_mut(index)
                // alloc: an error
                .ok_or_else(|| format!("a relocation at data {index}, never requested"))? = true;
        }
        for index in (0..requests.datas.len()).rev() {
            if !used[index] {
                continue;
            }
            if let DataRequest::Blob { blob, .. } | DataRequest::Vtable { blob, .. } =
                &requests.datas[index]
            {
                for &(_, data, _) in &blob.relocs {
                    let at = data.as_u32() as usize;
                    if at >= index {
                        // alloc: an error
                        return Err(format!("data {index} points at data {at}, not one before it"));
                    }
                    used[at] = true;
                }
            }
        }
        // A `Vec`, never a frame slice: an `Extern`'s name has to be dropped.
        let mut targets: Vec<Option<Target>, &Frame> =
            Vec::with_capacity_in(requests.datas.len(), frame);
        for (index, request) in requests.datas.iter().enumerate() {
            if !used[index] {
                targets.push(None);
                continue;
            }
            let target = match resolve.data(index as u32, request)? {
                DataTarget::Target(target) => target,
                DataTarget::Own => {
                    let (blob, writable) = own_blob(request)?;
                    let data = self.blob(blob, writable, index, &targets, requests, resolve)?;
                    self.push(data)
                }
                DataTarget::Text(text) => self.text(text),
                DataTarget::Nowhere => self.nowhere(),
            };
            targets.push(Some(target));
        }
        Ok(targets)
    }

    fn blob(
        &mut self,
        blob: &ConstBlob,
        writable: bool,
        index: usize,
        before: &[Option<Target>],
        requests: &Requests,
        resolve: &impl Resolver,
    ) -> Result<Data, String> {
        let relocs = self.relocs(blob, index, before, requests, resolve)?;
        // alloc: an error
        let align = u32::try_from(blob.align).map_err(|e| e.to_string())?;
        // clone: the data owns its bytes; the request, which the caller keeps, has the blob's
        let bytes = blob.bytes.clone();
        match (writable, &self.owner) {
            (false, _) => Ok(Data::new(align, bytes, relocs)),
            // Its index is where `push` puts it, as `StaticTemplate::instantiate` numbers it.
            (true, Some(owner)) => Ok(Data::owned(owner, self.data.len() as u32, align, bytes, relocs)),
            // alloc: an error
            (true, None) => Err("data a function writes to, which is no constant (`promoted_slot`)".to_owned()),
        }
    }

    /// The relocations of `blob`, the data request `index` (or what comes after them all),
    /// which points only at requests before it.
    fn relocs(
        &self,
        blob: &ConstBlob,
        index: usize,
        before: &[Option<Target>],
        requests: &Requests,
        resolve: &impl Resolver,
    ) -> Result<Relocs, String> {
        if !blob.type_ids.is_empty() {
            // alloc: an error
            return Err("a constant with a `TypeId` the adapter did not fill".to_owned());
        }
        let mut relocs = Vec::with_capacity(blob.relocs.len() + blob.fn_relocs.len());
        for &(at, data, addend) in &blob.relocs {
            let at_data = data.as_u32() as usize;
            let Some(Some(target)) = before.get(at_data).filter(|_| at_data < index) else {
                // alloc: an error
                return Err(format!("data {index} points at data {at_data}, not one before it"));
            };
            // clone: a `Target` is an ID but for an `Extern`'s name; each relocation at the request owns one
            relocs.push(Relocation { at, kind: self.word.data, target: target.clone(), addend });
        }
        for &(at, func) in &blob.fn_relocs {
            let request = requests.funcs.get(func.as_u32() as usize).ok_or_else(|| {
                // alloc: an error
                format!("data {index} points at function {}, never requested", func.as_u32())
            })?;
            let target = resolve.func(func.as_u32(), request)?;
            relocs.push(Relocation { at, kind: self.word.func, target, addend: 0 });
        }
        Ok(relocs.into())
    }

    /// The bytes of `text`, and the `&str` of them.
    fn text(&mut self, text: String) -> Target {
        let Word { size: word, data: kind, .. } = self.word;
        let len = text.len() as u64;
        let bytes = self.push(Data::new(1, text.into_bytes(), Relocs::new()));
        // alloc: the `&str`'s bytes, which the data owns
        let mut fat = vec![0; 2 * word];
        fat[word..].copy_from_slice(&len.to_le_bytes()[..word]);
        let pointer = Relocation { at: 0, kind, target: bytes, addend: 0 };
        self.push(Data::new(word as u32, fat, Relocs::from_iter([pointer])))
    }

    /// A `core::panic::Location` of no file, as v1 makes it: `<blaze>`, line 0, column 0. The
    /// file's pointer and length, then the two `u32`s: 24 bytes aligned to 8, on wasm32 16
    /// aligned to 4.
    fn nowhere(&mut self) -> Target {
        let Word { size: word, data: kind, .. } = self.word;
        let file = "<blaze>";
        let mut name = Vec::with_capacity(file.len() + 1);
        name.extend_from_slice(file.as_bytes());
        // The name is followed by a NUL that its length leaves out.
        name.push(0);
        let name = self.push(Data::new(1, name, Relocs::new()));
        // alloc: the `Location`'s bytes, which the data owns
        let mut bytes = vec![0u8; 2 * word + 8];
        bytes[word..2 * word].copy_from_slice(&(file.len() as u64).to_le_bytes()[..word]);
        let pointer = Relocation { at: 0, kind, target: name, addend: 0 };
        self.push(Data::new(word as u32, bytes, Relocs::from_iter([pointer])))
    }
}

/// The blob of a request that is the function's or static's own data, and whether it is
/// written to.
fn own_blob(request: &DataRequest) -> Result<(&ConstBlob, bool), String> {
    match request {
        DataRequest::Blob { blob, identity } => Ok((blob, *identity == DataIdentity::Writable)),
        DataRequest::Vtable { blob, .. } => Ok((blob, false)),
        // alloc: an error
        _ => Err("a data request that has no blob taken as the function's own".to_owned()),
    }
}

/// The target of a relocation of instance `key`'s code at `name`.
fn reloc_target(
    key: InstanceKey,
    name: RelocName,
    requests: &Requests,
    datas: &[Option<Target>],
    resolve: &impl Resolver,
) -> Result<Target, String> {
    match name {
        RelocName::Func(index) => {
            let request = requests
                .funcs
                .get(index as usize)
                // alloc: an error
                .ok_or_else(|| format!("a relocation at function {index}, never requested"))?;
            resolve.func(index, request)
        }
        RelocName::Data(index) => datas
            .get(index as usize)
            .cloned()
            .flatten()
            // alloc: an error
            .ok_or_else(|| format!("a relocation at data {index}, never requested")),
        RelocName::User { namespace: super::fold::BODY, index: 0 } => resolve.body(),
        RelocName::User { namespace: fragment::INLINED, .. } => {
            // alloc: an error
            Err("a relocation at a reference of a body built in".to_owned())
        }
        #[cfg(feature = "llvm")]
        RelocName::User { namespace: crate::llvm::sanitize::COVERAGE, index } => match index {
            0..3 => Ok(Target::Coverage(index)),
            // alloc: an error
            _ => Err(format!("a relocation at SanitizerCoverage array {index}, of 3")),
        },
        // An instrumented function's own PGO counters, the counter by the addend.
        #[cfg(feature = "llvm")]
        RelocName::User { namespace: crate::llvm::pgo::COUNTER_RELOCS, index } => match index {
            0 => Ok(Target::ProfileCounters),
            // alloc: an error
            _ => Err(format!("a relocation at PGO counters {index}, where a function has only its own (0)")),
        },
        // Stencil code's and counting Cranelift code's (`blaze_stencil::tier::instrument`):
        // the instance's own cells, the cell by the addend.
        RelocName::User { namespace: blaze_stencil::tier::COUNTER_NAMESPACE, index: 0 } => Ok(Target::Counter(key)),
        RelocName::User { namespace: blaze_stencil::tier::COUNTER_NAMESPACE, index } => {
            // alloc: an error
            Err(format!("a relocation at tier counter cells {index}, where a function has only its own (0)"))
        }
        RelocName::User { namespace, .. } => {
            // alloc: an error
            Err(format!("a relocation in the unknown namespace {namespace}"))
        }
        RelocName::LibCall(libcall) => resolve.symbol(libcall_name(libcall)?),
        RelocName::Known(symbol) => {
            // alloc: an error
            Err(format!("a relocation at the linker's {symbol:?}, which v2 has no target for"))
        }
        // The function's own code, at its start: `function` moved the offset into the addend.
        RelocName::Own(0) => Ok(Target::Direct(key)),
        RelocName::Own(offset) => {
            // alloc: an error
            Err(format!("a relocation at offset {offset} of the function itself, not moved into its addend"))
        }
    }
}

/// The addend of a relocation `offset` bytes into the function's own code of `size` bytes,
/// at the function's start: the offset added in, which must be inside the code.
fn own_addend(offset: u32, addend: i64, size: usize) -> Result<i64, String> {
    if offset as usize > size {
        // alloc: an error
        return Err(format!("a relocation at offset {offset} of the function itself, which has {size} bytes"));
    }
    addend.checked_add(i64::from(offset)).ok_or_else(|| {
        // alloc: an error
        format!("a relocation at offset {offset} of the function itself, whose addend {addend} overflows with it")
    })
}

/// The kind of a relocation as v2 names it.
pub(crate) fn reloc_kind(kind: binemit::Reloc) -> Result<RelocKind, String> {
    use binemit::Reloc as R;
    Ok(match kind {
        R::Abs8 => RelocKind::Abs64,
        R::X86PCRel4 | R::X86CallPCRel4 | R::X86CallPLTRel4 => RelocKind::PcRel32,
        R::X86GOTPCRel4 => RelocKind::GotPcRel32,
        R::Arm64Call => RelocKind::Call26,
        R::Aarch64AdrPrelPgHi21 => RelocKind::AdrpPage21,
        R::Aarch64AddAbsLo12Nc => RelocKind::PageOffset12,
        R::Aarch64AdrGotPage21 => RelocKind::GotPage21,
        R::Aarch64Ld64GotLo12Nc => RelocKind::GotPageOffset12,
        R::ElfX86_64TlsGd => RelocKind::TlsGd32,
        R::MachOX86_64Tlv => RelocKind::TlvRel32,
        R::MachOAarch64TlsAdrPage21 => RelocKind::TlvPage21,
        R::MachOAarch64TlsAdrPageOff12 => RelocKind::TlvPageOffset12,
        R::Aarch64TlsDescAdrPage21 => RelocKind::TlsDescPage21,
        R::Aarch64TlsDescLd64Lo12 => RelocKind::TlsDescLd64Lo12,
        R::Aarch64TlsDescAddLo12 => RelocKind::TlsDescAddLo12,
        R::Aarch64TlsDescCall => RelocKind::TlsDescCall,
        R::X86SecRel => RelocKind::SectionRel32,
        // alloc: an error
        other => return Err(format!("a {other:?} relocation, which v2 has no kind for")),
    })
}

/// The kind of a relocation whose ELF type the producer said.
pub(crate) fn elf_kind(elf: u32) -> Result<RelocKind, String> {
    use cranelift_object::object::elf;
    Ok(match elf::RelocationType(elf) {
        elf::R_X86_64_64 | elf::R_AARCH64_ABS64 => RelocKind::Abs64,
        elf::R_X86_64_PC32 | elf::R_X86_64_PLT32 => RelocKind::PcRel32,
        elf::R_X86_64_GOTPCREL | elf::R_X86_64_GOTPCRELX | elf::R_X86_64_REX_GOTPCRELX => {
            RelocKind::GotPcRel32
        }
        elf::R_AARCH64_CALL26 | elf::R_AARCH64_JUMP26 => RelocKind::Call26,
        elf::R_AARCH64_ADR_PREL_PG_HI21 => RelocKind::AdrpPage21,
        elf::R_AARCH64_ADD_ABS_LO12_NC
        | elf::R_AARCH64_LDST8_ABS_LO12_NC
        | elf::R_AARCH64_LDST16_ABS_LO12_NC
        | elf::R_AARCH64_LDST32_ABS_LO12_NC
        | elf::R_AARCH64_LDST64_ABS_LO12_NC
        | elf::R_AARCH64_LDST128_ABS_LO12_NC => RelocKind::PageOffset12,
        elf::R_AARCH64_ADR_GOT_PAGE => RelocKind::GotPage21,
        elf::R_AARCH64_LD64_GOT_LO12_NC => RelocKind::GotPageOffset12,
        elf::R_X86_64_TLSGD => RelocKind::TlsGd32,
        elf::R_X86_64_GOTTPOFF => RelocKind::TlsGotTpOff32,
        elf::R_X86_64_TPOFF32 => RelocKind::TlsOffset,
        elf::R_AARCH64_TLSDESC_ADR_PAGE21 => RelocKind::TlsDescPage21,
        elf::R_AARCH64_TLSDESC_LD64_LO12 => RelocKind::TlsDescLd64Lo12,
        elf::R_AARCH64_TLSDESC_ADD_LO12 => RelocKind::TlsDescAddLo12,
        elf::R_AARCH64_TLSDESC_CALL => RelocKind::TlsDescCall,
        elf::R_AARCH64_TLSIE_ADR_GOTTPREL_PAGE21 => RelocKind::TlsIePage21,
        elf::R_AARCH64_TLSIE_LD64_GOTTPREL_LO12_NC => RelocKind::TlsIeLo12,
        elf::R_AARCH64_TLSLE_ADD_TPREL_HI12 => RelocKind::TpHi12,
        elf::R_AARCH64_TLSLE_ADD_TPREL_LO12_NC => RelocKind::TpLo12,
        // alloc: an error
        other => return Err(format!("a relocation of ELF type {}, which v2 has no kind for", other.0)),
    })
}

#[cfg(test)]
mod tests;
