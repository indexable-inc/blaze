//! Folding functions that differ only in constants (#1292) over Cranelift IR: the masked form
//! of a prepared function ([`masked`]) and a group's one body with a thunk per member
//! ([`merge`]). Which functions fold is [`blaze_codegen::fold::groups`]'s.
//!
//! A masked function is its IR walked in layout order and written as little-endian words, all
//! Cranelift's printer shows and nothing else, with every value, block, signature, stack
//! slot, pool constant, function and global value numbered by its first use, so two
//! functions built alike are equal whatever their entities' numbers. Its shape is the words'
//! BLAKE3 hash. `iconst`, `f32const` and `f64const` leave their immediate out for the
//! constants, and so does every named function and global value: one named by a place, and a
//! data object of the function's own (a vtable, a type's info, a constant), which is told by
//! its content with what it points at by target.
//!
//! The merged body is the first member's IR with each varying constant read from a trailing
//! parameter: an immediate's uses read the parameter, a varying function is called through
//! it (`call_indirect`) and its address is the parameter, a varying static's or data object's
//! address is the parameter. Each thunk has its member's signature and calls the body with
//! its arguments and its constants: a plain call and return, as Cranelift tail-calls only
//! under `tail`, which no member is.

#![forbid(unsafe_code)]

use std::{
    cell::RefCell,
    hash::{Hash, Hasher as _},
};

use blaze_codegen::{
    CodegenError, Target, Unit,
    backend::Emitted,
    content::target_id,
    fold::{Constant, Group, Masked, Merged},
    settings::Spec,
    template::Refs,
};
use blaze_format::cursor::Put;
use blaze_ids::{ContentId, Hasher};
use cranelift_codegen::{
    cursor::{Cursor as _, FuncCursor},
    ir::{self, InstBuilder as _, InstructionData, Opcode},
};
use smallvec::SmallVec;

use super::{DataPlace, Function};
use crate::{DataIdentity, fragment::DataRequest};

/// The namespace of the name a thunk calls the merged body by: the last of the references it
/// is compiled against, [`Group::thunk_refs`]'s [`blaze_codegen::Target::Direct`] of the body.
pub(crate) const BODY: u32 = u32::MAX - 1;

/// What a name of a prepared function's IR is in the build.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Name {
    /// The `n`th of the instance's references: a function or a static.
    Place(u32),
    /// A data object of its own, a position or a text, by content: its bytes, and what it
    /// points at by target. Equal for two functions only when it is the same object in both.
    Data(ContentId),
}

/// The constants of a prepared Cranelift function, prepared against `refs`, masked for
/// [`blaze_codegen::fold::groups`]. `None` for one that can't take parameters: an assembly
/// function, one without a body or with a dynamic stack slot, one whose global value has
/// a named symbol for its base, one naming a patchable callee, and one with data whose
/// content isn't final (a `TypeId` still to be written) or that is not shared by content. A
/// C-variadic function, and one whose address is significant
/// ([`blaze_codegen::backend::Address::Significant`]), are the caller's to leave out.
pub(crate) fn masked(function: &Function, refs: &Refs) -> Option<Masked> {
    masked_ir(function.ir()?, Names::Of(function, refs))
}

/// [`masked`] of `func`, whose names `names` says. `names` masks no function itself: the
/// walk's buffers are the thread's.
pub(crate) fn masked_ir(func: &ir::Function, names: Names<'_>) -> Option<Masked> {
    WORDS.with_borrow_mut(|words| {
        let size = canon(func, names, words)?;
        // clone: what it returns; the thread's buffer stays for the next walk
        Some(Masked { shape: shape(&words.bytes), constants: words.constants.clone(), size })
    })
}

const FOLD_DATA_DOMAIN: &str = "blaze v2 fold data 1";

/// What a name of `function`'s IR is: namespace 0 its function requests, 1 its data
/// requests. `None` for a callee still to be inlined, and for data [`masked`] refuses.
pub(crate) fn name(
    function: &Function,
    refs: &Refs,
    name: ir::UserExternalName,
) -> Option<Name> {
    let index = name.index as usize;
    match name.namespace {
        0 => function.places.funcs.get(index).map(|&it| Name::Place(it)),
        1 => match function.places.datas.get(index)? {
            DataPlace::Place(place) => Some(Name::Place(*place)),
            _ => data(function, refs, index).map(Name::Data),
        },
        _ => None,
    }
}

/// What the names of a prepared function's IR are: its requests' places among the references
/// ([`name`]), or a test's fixed rule.
#[derive(Clone, Copy)]
pub(crate) enum Names<'a> {
    Of(&'a Function, &'a Refs),
    #[cfg(test)]
    Plain(fn(ir::UserExternalName) -> Option<Name>),
}

impl Names<'_> {
    pub(crate) fn of(self, it: ir::UserExternalName) -> Option<Name> {
        match self {
            Names::Of(function, refs) => name(function, refs, it),
            #[cfg(test)]
            Names::Plain(names) => names(it),
        }
    }
}

/// A data request's content: what it points at by target, so equal IDs in two functions are
/// one object whatever their places.
fn data(function: &Function, refs: &Refs, index: usize) -> Option<ContentId> {
    KNOWN.with_borrow_mut(|known| {
        known.clear();
        data_of(function, refs, index, known)
    })
}

thread_local! {
    /// [`data_of`]'s `known`, one per thread and reused.
    static KNOWN: RefCell<Vec<Option<ContentId>>> = RefCell::default();
}

/// [`data`], with `known[i]` request `i`'s content once hashed: an object many others point
/// at is hashed once, not once per path to it.
fn data_of(
    function: &Function,
    refs: &Refs,
    index: usize,
    known: &mut Vec<Option<ContentId>>,
) -> Option<ContentId> {
    if let Some(&Some(id)) = known.get(index) {
        return Some(id);
    }
    let mut hasher = Hasher::new(FOLD_DATA_DOMAIN);
    match function.places.datas.get(index)? {
        DataPlace::Place(place) => {
            hasher.update(&[3]).id(&target_id(refs.get(*place as usize)?));
        }
        DataPlace::Position(span) => {
            hasher.update(&[0]).id(&span.anchor).update(&span.index.to_le_bytes());
        }
        DataPlace::Text(text) => {
            hasher.update(&[1]).part(text.as_bytes());
        }
        // Every one is the same `Location`, of no file (`convert`'s `nowhere`).
        DataPlace::Nowhere => {
            hasher.update(&[4]);
        }
        DataPlace::Own => {
            let (DataRequest::Blob { blob, identity: DataIdentity::Shared }
            | DataRequest::Vtable { blob, .. }) = function.requests().datas.get(index)?
            else {
                return None;
            };
            // A `TypeId`'s bytes are not written yet.
            if !blob.type_ids.is_empty() {
                return None;
            }
            hasher.update(&[2]).update(&blob.align.to_le_bytes()).part(&blob.bytes);
            hasher.update(&(blob.relocs.len() as u64).to_le_bytes());
            for &(at, target, addend) in &blob.relocs {
                // Only data before it, so the walk ends.
                let target = target.as_u32() as usize;
                if target >= index {
                    return None;
                }
                let id = data_of(function, refs, target, known)?;
                if known.len() <= target {
                    known.resize(target + 1, None);
                }
                known[target] = Some(id);
                hasher.update(&at.to_le_bytes()).id(&id).update(&addend.to_le_bytes());
            }
            hasher.update(&(blob.fn_relocs.len() as u64).to_le_bytes());
            for &(at, func) in &blob.fn_relocs {
                let place = *function.places.funcs.get(func.as_u32() as usize)?;
                hasher.update(&at.to_le_bytes()).id(&target_id(refs.get(place as usize)?));
            }
        }
    }
    Some(hasher.finish())
}

/// Where a masked constant is in the function.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Site {
    /// An `iconst`, `f32const` or `f64const`.
    Inst(ir::Inst),
    Func(ir::FuncRef),
    Global(ir::GlobalValue),
}

struct Walk {
    masked: Masked,
    /// Of each of `masked.constants`.
    sites: Vec<Site>,
}

const FOLD_IR_DOMAIN: &str = "blaze v2 fold ir 1";

/// The entities numbered by their first use, as indices of [`Words::numbers`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Value,
    Block,
    Func,
    Sig,
    Global,
    Slot,
    Pool,
}

const KINDS: usize = 7;

/// What starts a block's words and an instruction's.
const BLOCK: u8 = 0;
const INST: u8 = 1;

/// A walk's buffers, one set per thread and reused, so masking a function allocates only
/// what it returns.
#[derive(Default)]
struct Words {
    /// The masked IR: fixed-width little-endian words, each block and instruction tagged and
    /// each list length-prefixed, so equal words are equal IR.
    bytes: Vec<u8>,
    /// Per kind, by an entity's own number, its number of first use plus one; 0 until used.
    numbers: [Vec<u32>; KINDS],
    counts: [u32; KINDS],
    constants: Vec<Constant>,
    /// Of each of `constants`.
    sites: Vec<Site>,
}

thread_local! {
    static WORDS: RefCell<Words> = RefCell::default();
}

fn shape(bytes: &[u8]) -> Vec<u8> {
    // alloc: the 32 bytes `Masked::shape` holds, once a function
    Hasher::new(FOLD_IR_DOMAIN).update(bytes).finish().0.to_vec()
}

/// [`masked_ir`] with the sites of its constants, for [`build`].
fn walk(func: &ir::Function, names: Names<'_>) -> Option<Walk> {
    WORDS.with_borrow_mut(|words| {
        let size = canon(func, names, words)?;
        let masked =
            // clone: what it returns; the thread's buffer stays for the next walk
            Masked { shape: shape(&words.bytes), constants: words.constants.clone(), size };
        // clone: as `constants`
        Some(Walk { masked, sites: words.sites.clone() })
    })
}

/// Writes `func` masked into `words`; its number of instructions, `None` where [`masked`]
/// refuses.
fn canon(func: &ir::Function, names: Names<'_>, words: &mut Words) -> Option<u32> {
    words.bytes.clear();
    words.numbers.iter_mut().for_each(Vec::clear);
    words.counts = [0; KINDS];
    words.constants.clear();
    words.sites.clear();
    func.layout.entry_block()?;
    if !func.dynamic_stack_slots.is_empty() {
        return None;
    }
    let mut canon = Canon { func, names, words };
    canon.plain(&func.signature);
    let mut size = 0u32;
    for block in func.layout.blocks() {
        canon.u8(BLOCK);
        canon.entity(Kind::Block, block.as_u32())?;
        let params = func.dfg.block_params(block);
        canon.len(params.len());
        for &param in params {
            canon.value(param)?;
            canon.plain(&func.dfg.value_type(param));
        }
        canon.u8(u8::from(func.layout.is_cold(block)));
        for inst in func.layout.block_insts(block) {
            size += 1;
            canon.inst(inst)?;
        }
    }
    Some(size)
}

/// Writes what a [`Hash`] impl feeds it as little-endian words: plain data (memory flags,
/// signatures, stack slots, trap and condition codes) is written whole, whatever its fields.
struct Le<'a>(&'a mut Vec<u8>);

impl std::hash::Hasher for Le<'_> {
    fn finish(&self) -> u64 {
        unreachable!("the words are hashed whole, by BLAKE3")
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    blaze_ids::fixed_width_writes!();
}

struct Canon<'a> {
    func: &'a ir::Function,
    names: Names<'a>,
    words: &'a mut Words,
}

impl Canon<'_> {
    fn u8(&mut self, it: u8) {
        self.words.bytes.push(it);
    }

    fn u32(&mut self, it: u32) {
        self.words.bytes.put_u32(it);
    }

    fn len(&mut self, len: usize) {
        self.u32(len as u32);
    }

    fn plain(&mut self, it: &impl Hash) {
        it.hash(&mut Le(&mut self.words.bytes));
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.words.bytes.put_bytes_u32(bytes);
    }

    fn constant(&mut self, constant: Constant, site: Site) {
        self.words.constants.push(constant);
        self.words.sites.push(site);
    }

    fn value(&mut self, value: ir::Value) -> Option<()> {
        self.entity(Kind::Value, value.as_u32())
    }

    fn values(&mut self, values: &[ir::Value]) -> Option<()> {
        self.len(values.len());
        values.iter().try_for_each(|&it| self.value(it))
    }

    fn flags(&mut self, flags: ir::MemFlags) {
        let data = self.func.dfg.mem_flags[flags];
        self.plain(&data);
    }

    /// Every field of every format, as the printer shows them; what an instruction can't
    /// take as a parameter it refuses.
    fn inst(&mut self, inst: ir::Inst) -> Option<()> {
        let func = self.func;
        let dfg = &func.dfg;
        let pool = &dfg.value_lists;
        self.u8(INST);
        self.values(dfg.inst_results(inst))?;
        let data = dfg.insts[inst];
        self.plain(&data.opcode());
        let ctrl = dfg.ctrl_typevar(inst);
        self.plain(&ctrl);
        match data {
            InstructionData::AtomicCas { opcode: _, args, flags } => {
                self.flags(flags);
                self.values(&args)?;
            }
            InstructionData::AtomicRmw { opcode: _, args, flags, op } => {
                self.flags(flags);
                self.plain(&op);
                self.values(&args)?;
            }
            InstructionData::Binary { opcode: _, args } => self.values(&args)?,
            InstructionData::BinaryImm8 { opcode: _, arg, imm } => {
                self.value(arg)?;
                self.plain(&imm);
            }
            InstructionData::BranchTable { opcode: _, arg, table } => {
                self.value(arg)?;
                let branches = dfg.jump_tables[table].all_branches();
                self.len(branches.len());
                for branch in branches {
                    self.block_call(branch)?;
                }
            }
            InstructionData::Brif { opcode: _, arg, blocks: [then, otherwise] } => {
                self.value(arg)?;
                self.block_call(&then)?;
                self.block_call(&otherwise)?;
            }
            InstructionData::Call { opcode: _, args, func_ref } => {
                self.entity(Kind::Func, func_ref.as_u32())?;
                self.values(args.as_slice(pool))?;
                self.stack_map(inst)?;
            }
            InstructionData::CallIndirect { opcode: _, args, sig_ref } => {
                self.entity(Kind::Sig, sig_ref.as_u32())?;
                self.values(args.as_slice(pool))?;
                self.stack_map(inst)?;
            }
            InstructionData::CondTrap { opcode: _, arg, code } => {
                self.value(arg)?;
                self.plain(&code);
            }
            InstructionData::DynamicStackAddr { opcode: _, dynamic_stack_slot: _ } => return None,
            InstructionData::ExceptionHandlerAddress { opcode: _, block, imm } => {
                self.entity(Kind::Block, block.as_u32())?;
                self.plain(&imm);
            }
            InstructionData::FloatCompare { opcode: _, args, cond } => {
                self.plain(&cond);
                self.values(&args)?;
            }
            InstructionData::FuncAddr { opcode: _, func_ref } => {
                self.entity(Kind::Func, func_ref.as_u32())?;
            }
            InstructionData::IntAddTrap { opcode: _, args, code } => {
                self.values(&args)?;
                self.plain(&code);
            }
            InstructionData::IntCompare { opcode: _, args, cond } => {
                self.plain(&cond);
                self.values(&args)?;
            }
            InstructionData::Jump { opcode: _, destination } => self.block_call(&destination)?,
            InstructionData::Load { opcode: _, arg, flags, offset } => {
                self.flags(flags);
                self.value(arg)?;
                self.plain(&offset);
            }
            InstructionData::LoadNoOffset { opcode: _, arg, flags } => {
                self.flags(flags);
                self.value(arg)?;
            }
            InstructionData::MultiAry { opcode: _, args } => self.values(args.as_slice(pool))?,
            InstructionData::NullAry { opcode: _ } => {}
            InstructionData::Shuffle { opcode: _, args, imm } => {
                self.values(&args)?;
                self.bytes(dfg.immediates.get(imm)?.as_slice());
            }
            InstructionData::StackAddr { opcode: _, stack_slot, offset } => {
                self.entity(Kind::Slot, stack_slot.as_u32())?;
                self.plain(&offset);
            }
            InstructionData::Store { opcode: _, args, flags, offset } => {
                self.flags(flags);
                self.values(&args)?;
                self.plain(&offset);
            }
            InstructionData::StoreNoOffset { opcode: _, args, flags } => {
                self.flags(flags);
                self.values(&args)?;
            }
            InstructionData::Ternary { opcode: _, args } => self.values(&args)?,
            InstructionData::TernaryImm8 { opcode: _, args, imm } => {
                self.values(&args)?;
                self.plain(&imm);
            }
            InstructionData::Trap { opcode: _, code } => self.plain(&code),
            InstructionData::TryCall { opcode: _, args, func_ref, exception } => {
                self.entity(Kind::Func, func_ref.as_u32())?;
                self.values(args.as_slice(pool))?;
                self.exception_table(exception)?;
                self.stack_map(inst)?;
            }
            InstructionData::TryCallIndirect { opcode: _, args, exception } => {
                self.values(args.as_slice(pool))?;
                self.exception_table(exception)?;
                self.stack_map(inst)?;
            }
            InstructionData::Unary { opcode: _, arg } => self.value(arg)?,
            InstructionData::UnaryConst { opcode: _, constant_handle } => {
                self.entity(Kind::Pool, constant_handle.as_u32())?;
            }
            InstructionData::UnaryGlobalValue { opcode: _, global_value } => {
                self.entity(Kind::Global, global_value.as_u32())?;
            }
            InstructionData::UnaryIeee16 { opcode: _, imm } => self.plain(&imm),
            InstructionData::UnaryIeee32 { opcode, imm } => match opcode {
                Opcode::F32const => self.constant(Constant::F32(imm.bits()), Site::Inst(inst)),
                _ => self.plain(&imm),
            },
            InstructionData::UnaryIeee64 { opcode, imm } => match opcode {
                Opcode::F64const => self.constant(Constant::F64(imm.bits()), Site::Inst(inst)),
                _ => self.plain(&imm),
            },
            InstructionData::UnaryImm { opcode, imm } => match opcode {
                Opcode::Iconst => {
                    let value = u128::from(imm.bits() as u64);
                    self.constant(Constant::Int { bits: ctrl.bits() as u8, value }, Site::Inst(inst));
                }
                // Sign-extended from the type's width, as the printer shows it.
                _ if ctrl.bits() != 0 => self.plain(&imm.sign_extend_from_width(ctrl.bits())),
                _ => self.plain(&imm),
            },
        }
        Some(())
    }

    fn block_call(&mut self, call: &ir::BlockCall) -> Option<()> {
        let func = self.func;
        let pool = &func.dfg.value_lists;
        self.entity(Kind::Block, call.block(pool).as_u32())?;
        self.len(call.len(pool));
        for arg in call.args(pool) {
            match arg {
                ir::BlockArg::Value(value) => {
                    self.u8(0);
                    self.value(value)?;
                }
                ir::BlockArg::TryCallRet(at) => {
                    self.u8(1);
                    self.u32(at);
                }
                ir::BlockArg::TryCallExn(at) => {
                    self.u8(2);
                    self.u32(at);
                }
            }
        }
        Some(())
    }

    fn exception_table(&mut self, table: ir::ExceptionTable) -> Option<()> {
        let func = self.func;
        let table = &func.dfg.exception_tables[table];
        self.entity(Kind::Sig, table.signature().as_u32())?;
        self.block_call(table.normal_return())?;
        for item in table.items() {
            match item {
                ir::ExceptionTableItem::Tag(tag, call) => {
                    self.u8(0);
                    self.u32(tag.as_u32());
                    self.block_call(&call)?;
                }
                ir::ExceptionTableItem::Default(call) => {
                    self.u8(1);
                    self.block_call(&call)?;
                }
                ir::ExceptionTableItem::Context(value) => {
                    self.u8(2);
                    self.value(value)?;
                }
            }
        }
        self.u8(3);
        Some(())
    }

    fn stack_map(&mut self, inst: ir::Inst) -> Option<()> {
        let func = self.func;
        let Some(entries) = func.dfg.user_stack_map_entries(inst) else {
            self.u8(0);
            return Some(());
        };
        self.u8(1);
        self.len(entries.len());
        for entry in entries {
            self.plain(&entry.ty);
            self.entity(Kind::Slot, entry.slot.as_u32())?;
            self.u32(entry.offset);
        }
        Some(())
    }

    /// Writes the entity's number of first use, and on its first use what it is.
    fn entity(&mut self, kind: Kind, raw: u32) -> Option<()> {
        let func = self.func;
        let raw = match kind {
            Kind::Value => func.dfg.resolve_aliases(ir::Value::from_u32(raw)).as_u32(),
            _ => raw,
        };
        let (k, at) = (kind as usize, raw as usize);
        let numbers = &mut self.words.numbers[k];
        if at >= numbers.len() {
            numbers.resize(at + 1, 0);
        }
        let known = numbers[at];
        if known != 0 {
            self.u32(known - 1);
            return Some(());
        }
        let number = self.words.counts[k];
        self.words.numbers[k][at] = number + 1;
        self.words.counts[k] = number + 1;
        self.u32(number);
        match kind {
            Kind::Value | Kind::Block => {}
            Kind::Func => {
                let callee = ir::FuncRef::from_u32(raw);
                let ir::ExtFuncData { ref name, signature, colocated, patchable } =
                    func.dfg.ext_funcs[callee];
                if self.name(name, Site::Func(callee))? && patchable {
                    return None;
                }
                self.entity(Kind::Sig, signature.as_u32())?;
                self.u8(u8::from(colocated));
                self.u8(u8::from(patchable));
            }
            Kind::Global => {
                let global = ir::GlobalValue::from_u32(raw);
                match func.global_values[global] {
                    ir::GlobalValueData::VMContext => self.u8(0),
                    ir::GlobalValueData::Load { base, offset, global_type, flags } => {
                        if self.is_named(base) {
                            return None;
                        }
                        self.u8(1);
                        self.flags(flags);
                        self.entity(Kind::Global, base.as_u32())?;
                        self.plain(&offset);
                        self.plain(&global_type);
                    }
                    ir::GlobalValueData::IAddImm { base, offset, global_type } => {
                        if self.is_named(base) {
                            return None;
                        }
                        self.u8(2);
                        self.entity(Kind::Global, base.as_u32())?;
                        self.plain(&offset);
                        self.plain(&global_type);
                    }
                    ir::GlobalValueData::Symbol { ref name, offset, colocated, tls } => {
                        self.u8(3);
                        self.name(name, Site::Global(global))?;
                        self.plain(&offset);
                        self.u8(u8::from(colocated));
                        self.u8(u8::from(tls));
                    }
                    ir::GlobalValueData::DynScaleTargetConst { vector_type } => {
                        self.u8(4);
                        self.plain(&vector_type);
                    }
                }
            }
            Kind::Sig => self.plain(&func.dfg.signatures[ir::SigRef::from_u32(raw)]),
            Kind::Slot => self.plain(&func.sized_stack_slots[ir::StackSlot::from_u32(raw)]),
            Kind::Pool => self.bytes(func.dfg.constants.get(ir::Constant::from_u32(raw)).as_slice()),
        }
        Some(())
    }

    /// Writes a function's or global value's name; whether it is a constant (a place or a
    /// data object). `None` for a user name `names` doesn't know.
    fn name(&mut self, name: &ir::ExternalName, site: Site) -> Option<bool> {
        let ir::ExternalName::User(user) = name else {
            self.u8(0);
            self.plain(name);
            return Some(false);
        };
        // clone: two `u32`s; cranelift's `UserExternalName` is not `Copy`
        let constant = match self.names.of(self.func.params.user_named_funcs()[*user].clone())? {
            Name::Place(place) => Constant::Place(place),
            Name::Data(id) => Constant::Data(id),
        };
        self.u8(1);
        self.constant(constant, site);
        Some(true)
    }

    /// Whether `global` is a symbol with a name: a constant, which can't be another global
    /// value's base, as a parameter is no global value.
    fn is_named(&self, global: ir::GlobalValue) -> bool {
        matches!(
            self.func.global_values[global],
            ir::GlobalValueData::Symbol { name: ir::ExternalName::User(_), .. }
        )
    }
}

/// A group's body and thunks, built and not compiled.
pub(crate) struct Folded {
    pub(crate) body: ir::Function,
    /// In the group's order.
    pub(crate) thunks: Vec<ir::Function>,
}

/// A member of a group: its prepared IR, what its names are, its references.
pub(crate) type Member<'a> = (&'a ir::Function, Names<'a>, &'a Refs);

/// Builds `group`'s body from its first member and a thunk for each. Every member's masked IR
/// must be the group's, with the group's values of its varying constants and equal values
/// (a place by its target) of the rest.
pub(crate) fn build(
    group: &Group,
    members: &[Member<'_>],
    pointer: ir::Type,
) -> Result<Folded, String> {
    if members.is_empty()
        || members.len() != group.members.len()
        || members.len() != group.constants.len()
    {
        // alloc: an error
        return Err(format!(
            "{} functions given for a group of {}",
            members.len(),
            group.members.len()
        ));
    }
    let walks = members
        .iter()
        .enumerate()
        .map(|(at, (func, names, _))| {
            // alloc: an error
            walk(func, *names).ok_or_else(|| format!("member {at} has no masked form"))
        })
        .collect::<Result<Vec<Walk>, String>>()?;
    let first = &walks[0];
    for (at, (walk, (_, _, refs))) in walks.iter().zip(members).enumerate() {
        if walk.masked.shape != first.masked.shape
            || walk.masked.constants.len() != first.masked.constants.len()
        {
            // alloc: an error
            return Err(format!("member {at}'s masked IR is not the first's"));
        }
        let mut varying = group.varying.iter().zip(&group.constants[at]).peekable();
        for (index, (&mine, &firsts)) in
            walk.masked.constants.iter().zip(&first.masked.constants).enumerate()
        {
            if let Some((_, &value)) = varying.next_if(|it| *it.0 as usize == index) {
                if mine != value {
                    // alloc: an error
                    let what = format!("member {at}'s constant {index} is {mine:?}, not {value:?}");
                    return Err(what);
                }
                continue;
            }
            let equal = match (mine, firsts) {
                (Constant::Place(mine), Constant::Place(firsts)) => {
                    let mine = refs.get(mine as usize);
                    mine.is_some() && mine == members[0].2.get(firsts as usize)
                }
                (mine, firsts) => mine == firsts,
            };
            if !equal {
                // alloc: an error
                return Err(format!("member {at}'s constant {index} varies, but is no parameter"));
            }
        }
        if varying.next().is_some() {
            // alloc: an error
            return Err(format!("the group's varying constants are not among member {at}'s"));
        }
    }
    let (reads, params) = group.parameters();
    let source = members[0].0;
    // Each parameter's type, from the first constant that reads it.
    let types = (0..params)
        .map(|param| {
            let at = reads.iter().position(|&it| it == param).expect("each parameter is read");
            match first.sites[group.varying[at] as usize] {
                Site::Inst(inst) => source.dfg.value_type(source.dfg.first_result(inst)),
                Site::Func(_) | Site::Global(_) => pointer,
            }
        })
        .collect::<SmallVec<[ir::Type; 8]>>();
    let body = body(source, first, group, &reads, &types)?;
    let thunks = members
        .iter()
        .zip(&walks)
        .zip(&group.constants)
        .map(|(((func, _, _), walk), constants)| {
            thunk(func, walk, group, constants, &reads, params, &body.signature, pointer)
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Folded { body, thunks })
}

fn body(
    source: &ir::Function,
    walk: &Walk,
    group: &Group,
    reads: &[u32],
    types: &[ir::Type],
) -> Result<ir::Function, String> {
    // clone: the body is the first member's IR, rewritten; the member keeps its own
    let mut body = source.clone();
    let entry = body.layout.entry_block().ok_or("the first member has no body")?;
    let params = types
        .iter()
        .map(|&ty| {
            body.signature.params.push(ir::AbiParam::new(ty));
            body.dfg.append_block_param(entry, ty)
        })
        .collect::<SmallVec<[ir::Value; 8]>>();
    // A varying function's or global value's parameter, found in one pass over the body.
    let mut funcs: SmallVec<[(ir::FuncRef, ir::Value); 4]> = SmallVec::new();
    let mut globals: SmallVec<[(ir::GlobalValue, ir::Value); 4]> = SmallVec::new();
    for (&at, &param) in group.varying.iter().zip(reads) {
        let param = params[param as usize];
        match walk.sites[at as usize] {
            Site::Inst(inst) => alias(&mut body, inst, param)?,
            Site::Func(callee) => funcs.push((callee, param)),
            Site::Global(global) => globals.push((global, param)),
        }
    }
    if funcs.is_empty() && globals.is_empty() {
        return Ok(body);
    }
    let func_param = |callee| funcs.iter().find(|it: &&(ir::FuncRef, ir::Value)| it.0 == callee);
    let insts: Vec<ir::Inst> =
        // alloc: the body's instructions, listed before it is rewritten in place; once a group
        body.layout.blocks().flat_map(|block| body.layout.block_insts(block)).collect();
    for inst in insts {
        let data = body.dfg.insts[inst];
        match data {
            InstructionData::FuncAddr { func_ref, .. } => {
                if let Some(&(_, param)) = func_param(func_ref) {
                    alias(&mut body, inst, param)?;
                }
            }
            InstructionData::Call { opcode, func_ref, .. } => {
                let Some(&(_, param)) = func_param(func_ref) else { continue };
                let sig = body.dfg.ext_funcs[func_ref].signature;
                let args = SmallVec::<[ir::Value; 8]>::from_slice(body.dfg.inst_args(inst));
                match opcode {
                    Opcode::Call => body.replace(inst).call_indirect(sig, param, &args[..]),
                    Opcode::ReturnCall => body.replace(inst).return_call_indirect(sig, param, &args[..]),
                    // alloc: an error
                    other => return Err(format!("{other} of a varying function")),
                };
            }
            InstructionData::TryCall { func_ref, exception, .. } => {
                let Some(&(_, param)) = func_param(func_ref) else { continue };
                let args = SmallVec::<[ir::Value; 8]>::from_slice(body.dfg.inst_args(inst));
                body.replace(inst).try_call_indirect(param, &args[..], exception);
            }
            InstructionData::UnaryGlobalValue { global_value, .. } => {
                if let Some(&(_, param)) = globals.iter().find(|it| it.0 == global_value) {
                    alias(&mut body, inst, param)?;
                }
            }
            _ => {}
        }
    }
    Ok(body)
}

/// Takes `inst` out, its one result standing for `param` from there on.
fn alias(func: &mut ir::Function, inst: ir::Inst, param: ir::Value) -> Result<(), String> {
    let result = func.dfg.first_result(inst);
    let (was, now) = (func.dfg.value_type(result), func.dfg.value_type(param));
    if was != now {
        // alloc: an error
        return Err(format!("a constant of type {was} would be read from a parameter of {now}"));
    }
    func.layout.remove_inst(inst);
    func.dfg.detach_inst_results(inst);
    func.dfg.change_to_alias(result, param);
    Ok(())
}

/// `name`, of `from`, declared in `to`.
fn copied(from: &ir::Function, name: &ir::ExternalName, to: &mut ir::Function) -> ir::ExternalName {
    match name {
        ir::ExternalName::User(user) => {
            // clone: two `u32`s; cranelift's `UserExternalName` is not `Copy`
            let user = from.params.user_named_funcs()[*user].clone();
            ir::ExternalName::user(to.declare_imported_user_function(user))
        }
        // clone: `to` owns its names; a libcall's or known symbol's is a tag, a test case's text
        other => other.clone(),
    }
}

/// What a thunk makes a parameter of the body from.
enum Made {
    Int(ir::Type, i64),
    F32(u32),
    F64(u64),
    Func(ir::FuncRef),
    Global { global: ir::GlobalValue, tls: bool },
}

/// A function with `member`'s signature that calls the body with its arguments and
/// `constants`, the member's values of the group's varying ones.
fn thunk(
    member: &ir::Function,
    walk: &Walk,
    group: &Group,
    constants: &[Constant],
    reads: &[u32],
    params: u32,
    body: &ir::Signature,
    pointer: ir::Type,
) -> Result<ir::Function, String> {
    let mut thunk =
        // clone: the thunk is a function of its own with the member's name and signature
        ir::Function::with_name_signature(member.name.clone(), member.signature.clone());
    let mut made = Vec::with_capacity(params as usize);
    for param in 0..params {
        let at = reads.iter().position(|&it| it == param).expect("each parameter is read");
        let site = walk.sites[group.varying[at] as usize];
        made.push(match (site, constants[at]) {
            (Site::Inst(inst), Constant::Int { value, .. }) => {
                Made::Int(member.dfg.value_type(member.dfg.first_result(inst)), value as u64 as i64)
            }
            (Site::Inst(_), Constant::F32(bits)) => Made::F32(bits),
            (Site::Inst(_), Constant::F64(bits)) => Made::F64(bits),
            (Site::Func(callee), Constant::Place(_)) => {
                let data = &member.dfg.ext_funcs[callee];
                let signature =
                    // clone: the thunk owns the signatures it imports
                    thunk.import_signature(member.dfg.signatures[data.signature].clone());
                let name = copied(member, &data.name, &mut thunk);
                let colocated = data.colocated;
                Made::Func(thunk.import_function(ir::ExtFuncData {
                    name,
                    signature,
                    colocated,
                    patchable: false,
                }))
            }
            (Site::Global(global), Constant::Place(_) | Constant::Data(_)) => {
                let ir::GlobalValueData::Symbol { ref name, offset, colocated, tls } =
                    member.global_values[global]
                else {
                    // alloc: an error
                    return Err(format!("varying global value {global} is not a symbol"));
                };
                let name = copied(member, name, &mut thunk);
                let data = ir::GlobalValueData::Symbol { name, offset, colocated, tls };
                Made::Global { global: thunk.create_global_value(data), tls }
            }
            (site, constant) => {
                // alloc: an error
                return Err(format!("{constant:?} is not what {site:?} of the member makes"));
            }
        });
    }
    // clone: the thunk owns the signatures it imports
    let signature = thunk.import_signature(body.clone());
    let name = thunk.declare_imported_user_function(ir::UserExternalName::new(BODY, 0));
    let callee = thunk.import_function(ir::ExtFuncData {
        name: ir::ExternalName::user(name),
        signature,
        colocated: true,
        patchable: false,
    });
    let block = thunk.dfg.make_block();
    thunk.layout.append_block(block);
    let mut args: Vec<ir::Value> = Vec::with_capacity(member.signature.params.len() + made.len());
    args.extend(member.signature.params.iter().map(|it| thunk.dfg.append_block_param(block, it.value_type)));
    let mut cursor = FuncCursor::new(&mut thunk).at_bottom(block);
    for it in made {
        args.push(match it {
            Made::Int(ty, imm) => cursor.ins().iconst(ty, imm),
            Made::F32(bits) => cursor.ins().f32const(ir::immediates::Ieee32::with_bits(bits)),
            Made::F64(bits) => cursor.ins().f64const(ir::immediates::Ieee64::with_bits(bits)),
            Made::Func(func) => cursor.ins().func_addr(pointer, func),
            Made::Global { global, tls: true } => cursor.ins().tls_value(pointer, global),
            Made::Global { global, tls: false } => cursor.ins().symbol_value(pointer, global),
        });
    }
    let call = cursor.ins().call(callee, &args);
    let results = SmallVec::<[ir::Value; 8]>::from_slice(cursor.func.dfg.inst_results(call));
    cursor.ins().return_(&results[..]);
    Ok(thunk)
}

/// Compiles `group`: its body from the first member, a thunk for each member, each with
/// `emit`, the adapter's Cranelift compile of a prepared function with `func` in place of its
/// IR, `func`'s names being the prepared function's (and [`BODY`] the last of `refs`). The
/// body is emitted as the first member against its references and keyed [`Group::key`];
/// thunk `i` as member `i` against [`Group::thunk_refs`] of its own.
pub(crate) fn merge(
    group: &Group,
    members: &[(&Unit, &Spec, &Function, &Refs)],
    pointer: ir::Type,
    emit: impl Fn(&Unit, &Spec, &Function, ir::Function, &Refs) -> Result<Emitted, CodegenError>,
) -> Result<Merged, CodegenError> {
    let &(first, ..) = members.first().expect("a group has members");
    let key = group.key();
    let fail = |what: String| CodegenError {
        unit: key,
        item: first.item,
        // alloc: an error
        what: format!("folding {} functions into one body: {what}", group.members.len()),
    };
    for (at, (unit, ..)) in members.iter().enumerate() {
        if group.members.get(at) != Some(&unit.instance.key()) {
            // alloc: an error
            return Err(fail(format!("member {at} is not the group's")));
        }
    }
    let mut sources: Vec<Member<'_>> = Vec::with_capacity(members.len());
    for (at, &(_, _, prepared, refs)) in members.iter().enumerate() {
        let Some(func) = prepared.ir() else {
            // alloc: an error
            return Err(fail(format!("member {at} is assembly, which has no IR to fold")));
        };
        sources.push((func, Names::Of(prepared, refs), refs));
    }
    let Folded { body, thunks } = build(group, &sources, pointer).map_err(fail)?;
    let (unit, spec, prepared, refs) = members[0];
    let mut body = emit(unit, spec, prepared, body, refs)?;
    body.function.key = key;
    // Each thunk's references are `Group::thunk_refs` of its member's.
    let most = members.iter().map(|it| it.3.len()).max().unwrap_or(0);
    let mut own: Vec<Target> = Vec::with_capacity(most + 1);
    let mut emitted = Vec::with_capacity(members.len());
    for (&(unit, spec, prepared, refs), thunk) in members.iter().zip(thunks) {
        own.clear();
        own.extend_from_slice(refs);
        own.push(Target::Direct(key));
        emitted.push(emit(unit, spec, prepared, thunk, &own[..])?);
    }
    Ok(Merged { body, thunks: emitted })
}

#[cfg(test)]
mod tests {
    use blaze_codegen::{
        InstanceKey, Settings, Target,
        fold::{Forms, Limits, Masking, groups},
    };
    use cranelift_codegen::{
        ir::{AbiParam, types},
        settings,
    };

    use super::*;

    fn key(n: u8) -> InstanceKey {
        InstanceKey(ContentId([n; 32]))
    }

    /// Namespace 0 are the places among the references, namespace 1 a data object of
    /// content `[index; 32]`.
    fn names(name: ir::UserExternalName) -> Option<Name> {
        match name.namespace {
            0 => Some(Name::Place(name.index)),
            1 => Some(Name::Data(ContentId([name.index as u8; 32]))),
            _ => None,
        }
    }

    const PLAIN: Names<'static> = Names::Plain(names);

    #[test]
    fn plain_names_are_what_their_rule_says() {
        for (namespace, index) in [(0, 3), (1, 5), (2, 0)] {
            let name = ir::UserExternalName::new(namespace, index);
            assert_eq!(PLAIN.of(name.clone()), names(name));
        }
    }

    fn signature() -> ir::Signature {
        let mut sig = ir::Signature::new(crate::target().unwrap().default_call_conv());
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        sig
    }

    /// `Vec<T>::grow`-like: multiplies its argument by `T`'s size, calls place `callee` with
    /// it, adds that function's address, a static's (place `stat`) and data object 7's.
    /// `shuffled` declares an unused signature and name first and builds the constant last,
    /// so its entities have other numbers.
    fn grow(size: i64, callee: u32, stat: u32, shuffled: bool) -> ir::Function {
        grow_with(size, callee, stat, 7, shuffled)
    }

    /// [`grow`] adding data object `data`'s address.
    fn grow_with(size: i64, callee: u32, stat: u32, data: u32, shuffled: bool) -> ir::Function {
        let ptr = types::I64;
        let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), signature());
        if shuffled {
            let call_conv = func.signature.call_conv;
            func.import_signature(ir::Signature::new(call_conv));
            func.declare_imported_user_function(ir::UserExternalName::new(0, 99));
        }
        let user = |func: &mut ir::Function, namespace, index| {
            ir::ExternalName::user(
                func.declare_imported_user_function(ir::UserExternalName::new(namespace, index)),
            )
        };
        let name = user(&mut func, 0, callee);
        let signature = func.import_signature(signature());
        let ext = ir::ExtFuncData { name, signature, colocated: true, patchable: false };
        let callee = func.import_function(ext);
        let name = user(&mut func, 0, stat);
        let stat = func.create_global_value(ir::GlobalValueData::Symbol {
            name,
            offset: 0.into(),
            colocated: true,
            tls: false,
        });
        let name = user(&mut func, 1, data);
        let data = func.create_global_value(ir::GlobalValueData::Symbol {
            name,
            offset: 0.into(),
            colocated: true,
            tls: false,
        });
        let block = func.dfg.make_block();
        func.layout.append_block(block);
        let arg = func.dfg.append_block_param(block, types::I64);
        let mut cursor = FuncCursor::new(&mut func).at_bottom(block);
        if shuffled {
            let dead = cursor.ins().iconst(types::I64, 0);
            let inst = cursor.func.dfg.value_def(dead).unwrap_inst();
            cursor.func.layout.remove_inst(inst);
        }
        let size = cursor.ins().iconst(types::I64, size);
        let scaled = cursor.ins().imul(arg, size);
        let call = cursor.ins().call(callee, &[scaled]);
        let called = cursor.func.dfg.first_result(call);
        let address = cursor.ins().func_addr(ptr, callee);
        let stat = cursor.ins().symbol_value(ptr, stat);
        let data = cursor.ins().symbol_value(ptr, data);
        let sum = cursor.ins().iadd(called, address);
        let sum = cursor.ins().iadd(sum, stat);
        let sum = cursor.ins().iadd(sum, data);
        cursor.ins().return_(&[sum]);
        func
    }

    #[test]
    fn numbering_is_not_the_shape() {
        let (a, b) = (grow(8, 0, 1, false), grow(8, 0, 1, true));
        let (a, b) = (masked_ir(&a, PLAIN).unwrap(), masked_ir(&b, PLAIN).unwrap());
        assert_eq!(a, b);
        assert_eq!(a.constants, vec![
            Constant::Int { bits: 64, value: 8 },
            Constant::Place(0),
            Constant::Place(1),
            Constant::Data(ContentId([7; 32])),
        ]);
        assert_eq!(a.size, 10);
    }

    #[test]
    fn constants_are_masked() {
        let (a, b) = (grow(8, 0, 1, false), grow(-4, 2, 3, false));
        let (a, b) = (masked_ir(&a, PLAIN).unwrap(), masked_ir(&b, PLAIN).unwrap());
        assert_eq!(a.shape, b.shape);
        assert_eq!(b.constants[0], Constant::Int { bits: 64, value: u128::from(-4i64 as u64) });
        assert_eq!(b.constants[1], Constant::Place(2));
        assert_eq!(a.shape.len(), 32);
    }

    #[test]
    fn other_control_flow_is_another_shape() {
        let mut other = grow(8, 0, 1, false);
        let block = other.layout.entry_block().unwrap();
        let ret = other.layout.last_inst(block).unwrap();
        let sum = other.dfg.inst_args(ret)[0];
        let next = other.dfg.make_block();
        other.layout.append_block(next);
        let mut cursor = FuncCursor::new(&mut other).at_inst(ret);
        cursor.ins().brif(sum, next, &[], next, &[]);
        cursor.func.layout.remove_inst(ret);
        let mut cursor = cursor.at_bottom(next);
        cursor.ins().return_(&[sum]);
        let a = masked_ir(&grow(8, 0, 1, false), PLAIN).unwrap();
        assert_ne!(masked_ir(&other, PLAIN).unwrap().shape, a.shape);
    }

    #[test]
    fn what_has_no_parameters_is_not_masked() {
        assert!(masked_ir(&ir::Function::new(), PLAIN).is_none());
        assert!(masked_ir(&grow(8, 0, 1, false), Names::Plain(|_| None)).is_none());
    }

    /// The masked form as it was: Cranelift's printed text, its entities renumbered by first
    /// use. The model [`masked_ir`] is held to, but for what the printer leaves out (an
    /// atomic's memory flags) or shows by number (a global value load's flags).
    mod text {
        use std::collections::HashMap;

        use super::super::*;

        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        enum Kind {
            Value,
            Block,
            Func,
            Sig,
            Global,
            Slot,
            Pool,
        }

        const PREFIXES: [(&str, Option<Kind>); 11] = [
            ("block", Some(Kind::Block)),
            ("const", Some(Kind::Pool)),
            ("dss", None),
            ("sig", Some(Kind::Sig)),
            ("fn", Some(Kind::Func)),
            ("gv", Some(Kind::Global)),
            ("jt", None),
            ("et", None),
            ("dt", None),
            ("ss", Some(Kind::Slot)),
            ("v", Some(Kind::Value)),
        ];

        struct Canon<'a> {
            func: &'a ir::Function,
            names: &'a dyn Fn(ir::UserExternalName) -> Option<Name>,
            text: String,
            numbers: HashMap<(Kind, u32), u32>,
            counts: HashMap<Kind, u32>,
            constants: Vec<Constant>,
            refused: bool,
        }

        pub(super) fn masked(
            func: &ir::Function,
            names: &dyn Fn(ir::UserExternalName) -> Option<Name>,
        ) -> Option<(String, Vec<Constant>)> {
            func.layout.entry_block()?;
            if !func.dynamic_stack_slots.is_empty() {
                return None;
            }
            let mut canon = Canon {
                func,
                names,
                text: format!("{}\n", func.signature),
                numbers: HashMap::new(),
                counts: HashMap::new(),
                constants: Vec::new(),
                refused: false,
            };
            for block in func.layout.blocks() {
                canon.entity(Kind::Block, block.as_u32());
                canon.text.push('(');
                for &param in func.dfg.block_params(block) {
                    canon.entity(Kind::Value, param.as_u32());
                    canon.text.push_str(&format!(": {}, ", func.dfg.value_type(param)));
                }
                canon.text.push_str(if func.layout.is_cold(block) { ") cold:\n" } else { "):\n" });
                for inst in func.layout.block_insts(block) {
                    canon.inst(inst);
                    canon.text.push('\n');
                }
            }
            if canon.refused {
                return None;
            }
            Some((canon.text, canon.constants))
        }

        impl Canon<'_> {
            fn inst(&mut self, inst: ir::Inst) {
                let dfg = &self.func.dfg;
                let constant = match dfg.insts[inst] {
                    InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => {
                        let bits = dfg.ctrl_typevar(inst).bits() as u8;
                        Some(Constant::Int { bits, value: u128::from(imm.bits() as u64) })
                    }
                    InstructionData::UnaryIeee32 { opcode: Opcode::F32const, imm } => {
                        Some(Constant::F32(imm.bits()))
                    }
                    InstructionData::UnaryIeee64 { opcode: Opcode::F64const, imm } => {
                        Some(Constant::F64(imm.bits()))
                    }
                    _ => None,
                };
                let Some(constant) = constant else {
                    let shown = dfg.display_inst(inst).to_string();
                    return self.tokens(shown.split("  ; ").next().unwrap_or_default());
                };
                self.entity(Kind::Value, dfg.first_result(inst).as_u32());
                let opcode = dfg.insts[inst].opcode();
                // `f32const` and `f64const` are not polymorphic: no type suffix, as Cranelift
                // prints them.
                let ty = dfg.ctrl_typevar(inst);
                match ty.is_invalid() {
                    true => self.text.push_str(&format!(" = {opcode} #{}", self.constants.len())),
                    false => self.text.push_str(&format!(" = {opcode}.{ty} #{}", self.constants.len())),
                }
                self.constants.push(constant);
            }

            fn tokens(&mut self, text: &str) {
                let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
                let mut rest = text;
                while let Some(start) = rest.find(word) {
                    self.text.push_str(&rest[..start]);
                    let len = rest[start..].find(|c: char| !word(c)).unwrap_or(rest.len() - start);
                    self.token(&rest[start..start + len]);
                    rest = &rest[start + len..];
                }
                self.text.push_str(rest);
            }

            fn token(&mut self, word: &str) {
                for (prefix, kind) in PREFIXES {
                    let Some(digits) = word.strip_prefix(prefix) else { continue };
                    if digits.is_empty() || !digits.bytes().all(|it| it.is_ascii_digit()) {
                        continue;
                    }
                    match (kind, digits.parse::<u32>()) {
                        (Some(kind), Ok(raw)) => self.entity(kind, raw),
                        _ => self.refused = true,
                    }
                    return;
                }
                self.text.push_str(word);
            }

            fn entity(&mut self, kind: Kind, raw: u32) {
                let raw = match kind {
                    Kind::Value => self.func.dfg.resolve_aliases(ir::Value::from_u32(raw)).as_u32(),
                    _ => raw,
                };
                let prefix = PREFIXES.iter().find(|it| it.1 == Some(kind)).map_or("?", |it| it.0);
                if let Some(number) = self.numbers.get(&(kind, raw)) {
                    return self.text.push_str(&format!("{prefix}{number}"));
                }
                let count = self.counts.entry(kind).or_default();
                let number = *count;
                *count += 1;
                self.numbers.insert((kind, raw), number);
                self.text.push_str(&format!("{prefix}{number}"));
                let func = self.func;
                match kind {
                    Kind::Value | Kind::Block => {}
                    Kind::Func => {
                        let callee = ir::FuncRef::from_u32(raw);
                        let data = &func.dfg.ext_funcs[callee];
                        self.text.push('{');
                        let place = self.name(&data.name);
                        self.text.push(' ');
                        self.entity(Kind::Sig, data.signature.as_u32());
                        self.text.push_str(&format!(" {} {}}}", data.colocated, data.patchable));
                        if place && data.patchable {
                            self.refused = true;
                        }
                    }
                    Kind::Global => {
                        let global = ir::GlobalValue::from_u32(raw);
                        self.text.push('{');
                        match func.global_values[global] {
                            ir::GlobalValueData::Symbol { ref name, offset, colocated, tls } => {
                                self.name(name);
                                let offset: i64 = offset.into();
                                self.text.push_str(&format!(" {offset} {colocated} {tls}"));
                            }
                            ir::GlobalValueData::Load { base, offset, global_type, flags } => {
                                self.refused |= self.is_named(base);
                                self.text.push_str(&format!("load.{global_type} {flags} "));
                                self.entity(Kind::Global, base.as_u32());
                                self.text.push_str(&format!("{offset}"));
                            }
                            ir::GlobalValueData::IAddImm { base, offset, global_type } => {
                                self.refused |= self.is_named(base);
                                self.text.push_str(&format!("iadd_imm.{global_type} "));
                                self.entity(Kind::Global, base.as_u32());
                                self.text.push_str(&format!(", {offset}"));
                            }
                            ref other => {
                                let shown = other.display(&func.dfg.mem_flags).to_string();
                                self.text.push_str(&shown);
                            }
                        }
                        self.text.push('}');
                    }
                    Kind::Sig => {
                        let sig = &func.dfg.signatures[ir::SigRef::from_u32(raw)];
                        self.text.push_str(&format!("{{{sig}}}"));
                    }
                    Kind::Slot => {
                        let slot = &func.sized_stack_slots[ir::StackSlot::from_u32(raw)];
                        self.text.push_str(&format!("{{{slot}}}"));
                    }
                    Kind::Pool => {
                        let data = func.dfg.constants.get(ir::Constant::from_u32(raw));
                        self.text.push_str(&format!("{{{data}}}"));
                    }
                }
            }

            fn name(&mut self, name: &ir::ExternalName) -> bool {
                let ir::ExternalName::User(user) = name else {
                    self.text.push_str(&name.display(None).to_string());
                    return false;
                };
                let constant = match (self.names)(self.func.params.user_named_funcs()[*user].clone()) {
                    Some(Name::Place(place)) => Constant::Place(place),
                    Some(Name::Data(id)) => Constant::Data(id),
                    None => {
                        self.refused = true;
                        return false;
                    }
                };
                self.text.push_str(&format!("#{}", self.constants.len()));
                self.constants.push(constant);
                true
            }

            fn is_named(&self, global: ir::GlobalValue) -> bool {
                matches!(
                    self.func.global_values[global],
                    ir::GlobalValueData::Symbol { name: ir::ExternalName::User(_), .. }
                )
            }
        }
    }

    /// SplitMix64. With `mutate`, that draw is moved by one: the same function but for one
    /// choice.
    struct Draws {
        rng: blaze_ids::rng::SplitMix64,
        count: u32,
        mutate: Option<u32>,
    }

    impl Draws {
        fn new(seed: u64, mutate: Option<u32>) -> Draws {
            Draws { rng: blaze_ids::rng::SplitMix64::new(seed), count: 0, mutate }
        }

        fn below(&mut self, n: u32) -> u32 {
            let value = self.rng.below(u64::from(n)) as u32;
            let at = self.count;
            self.count += 1;
            if self.mutate == Some(at) { (value + 1) % n } else { value }
        }
    }

    /// Unused entities and dead values between a function's own, from a stream of their own,
    /// so the same function gets other entity numbers.
    struct Noise(Option<Draws>);

    impl Noise {
        fn now(&mut self) -> bool {
            self.0.as_mut().is_some_and(|it| it.below(3) == 0)
        }

        fn dummy(&mut self, func: &mut ir::Function) {
            if !self.now() {
                return;
            }
            let call_conv = func.signature.call_conv;
            match self.0.as_mut().map_or(0, |it| it.below(7)) {
                0 => {
                    func.import_signature(ir::Signature::new(call_conv));
                }
                1 => {
                    let name = func.declare_imported_user_function(ir::UserExternalName::new(0, 77));
                    let signature = func.import_signature(ir::Signature::new(call_conv));
                    let name = ir::ExternalName::user(name);
                    let data = ir::ExtFuncData { name, signature, colocated: false, patchable: false };
                    func.import_function(data);
                }
                2 => {
                    func.create_global_value(ir::GlobalValueData::VMContext);
                }
                3 => {
                    let data = ir::StackSlotData::new(ir::StackSlotKind::ExplicitSlot, 4, 0);
                    func.create_sized_stack_slot(data);
                }
                4 => {
                    func.dfg.constants.insert(vec![0xee; 16].into());
                }
                5 => {
                    let flags = ir::MemFlagsData::new().with_readonly().with_can_move();
                    let _ = func.dfg.mem_flags.insert(flags);
                }
                _ => {
                    func.dfg.make_block();
                }
            }
        }

        fn dead(&mut self, cursor: &mut FuncCursor<'_>) {
            if self.now() {
                let dead = cursor.ins().iconst(types::I64, 0);
                let inst = cursor.func.dfg.value_def(dead).unwrap_inst();
                cursor.func.layout.remove_inst(inst);
            }
        }
    }

    /// A function of seed `seed`: blocks of loads, stores, compares, calls (some with stack
    /// maps), constants, vector constants, stack, function and symbol addresses, traps and
    /// atomics, ending in jumps, branches, jump tables and `try_call`s. Its entities are
    /// declared with `noise` between.
    fn random(seed: u64, mutate: Option<u32>, noise: Option<u64>) -> ir::Function {
        let mut d = Draws::new(seed, mutate);
        let mut z = Noise(noise.map(|it| Draws::new(it, None)));
        let call_conv = crate::target().unwrap().default_call_conv();
        let sig_of = |params: u32| {
            let mut sig = ir::Signature::new(call_conv);
            sig.params.extend((0..params).map(|_| AbiParam::new(types::I64)));
            sig.returns.push(AbiParam::new(types::I64));
            sig
        };
        let mut func = ir::Function::with_name_signature(
            ir::UserFuncName::default(),
            sig_of(1 + d.below(3)),
        );
        let mut user = |func: &mut ir::Function, d: &mut Draws| {
            let namespace = if d.below(48) == 0 { 2 } else { d.below(2) };
            let name = ir::UserExternalName::new(namespace, d.below(4));
            ir::ExternalName::user(func.declare_imported_user_function(name))
        };
        let mut sigs = Vec::new();
        for _ in 0..2 {
            z.dummy(&mut func);
            let params = 1 + d.below(2);
            sigs.push((func.import_signature(sig_of(params)), params));
        }
        let mut funcs = Vec::new();
        for _ in 0..3 {
            z.dummy(&mut func);
            let name = match d.below(8) {
                0 => ir::ExternalName::testcase([b'a' + d.below(2) as u8]),
                _ => user(&mut func, &mut d),
            };
            let (signature, params) = sigs[d.below(2) as usize];
            let colocated = d.below(2) == 1;
            let data = ir::ExtFuncData { name, signature, colocated, patchable: false };
            funcs.push((func.import_function(data), params));
        }
        let vmctx = func.create_global_value(ir::GlobalValueData::VMContext);
        let mut globals = Vec::new();
        for _ in 0..3 {
            z.dummy(&mut func);
            let data = match d.below(3) {
                0 => ir::GlobalValueData::IAddImm {
                    base: vmctx,
                    offset: i64::from(d.below(3)).into(),
                    global_type: types::I64,
                },
                _ => ir::GlobalValueData::Symbol {
                    name: user(&mut func, &mut d),
                    offset: i64::from(d.below(2)).into(),
                    colocated: d.below(2) == 1,
                    tls: false,
                },
            };
            globals.push(func.create_global_value(data));
        }
        let mut slots = Vec::new();
        for _ in 0..2 {
            z.dummy(&mut func);
            let size = 8 * (1 + d.below(3));
            let data = ir::StackSlotData::new(ir::StackSlotKind::ExplicitSlot, size, d.below(4) as u8);
            slots.push(func.create_sized_stack_slot(data));
        }
        let mut blocks = Vec::new();
        for _ in 0..1 + d.below(3) {
            z.dummy(&mut func);
            let block = func.dfg.make_block();
            func.layout.append_block(block);
            if d.below(4) == 0 {
                func.layout.set_cold(block);
            }
            blocks.push(block);
        }
        let mut values = Vec::new();
        for (at, &block) in blocks.iter().enumerate() {
            let count = if at == 0 { func.signature.params.len() } else { d.below(2) as usize };
            for _ in 0..count {
                values.push(func.dfg.append_block_param(block, types::I64));
            }
        }
        let pick = |d: &mut Draws, values: &[ir::Value]| values[d.below(values.len() as u32) as usize];
        let flags = |d: &mut Draws| {
            let trusted = ir::MemFlagsData::trusted();
            [ir::MemFlagsData::new(), trusted, trusted.with_readonly()][d.below(3) as usize]
        };
        let args = |d: &mut Draws, values: &[ir::Value], n: usize| -> Vec<ir::BlockArg> {
            (0..n).map(|_| ir::BlockArg::Value(pick(d, values))).collect()
        };
        for (at, &block) in blocks.iter().enumerate() {
            let mut cursor = FuncCursor::new(&mut func).at_bottom(block);
            for _ in 0..1 + d.below(6) {
                z.dead(&mut cursor);
                let made = match d.below(16) {
                    0 => Some(cursor.ins().iconst(types::I64, i64::from(d.below(5)) - 2)),
                    1 => Some(cursor.ins().iconst(types::I32, i64::from(d.below(3)))),
                    2 => {
                        let (a, b) = (pick(&mut d, &values), pick(&mut d, &values));
                        Some(match d.below(3) {
                            0 => cursor.ins().iadd(a, b),
                            1 => cursor.ins().imul(a, b),
                            _ => cursor.ins().band(a, b),
                        })
                    }
                    3 => {
                        use ir::condcodes::IntCC;
                        let conds = [
                            IntCC::Equal,
                            IntCC::NotEqual,
                            IntCC::SignedLessThan,
                            IntCC::UnsignedLessThan,
                        ];
                        let cond = conds[d.below(4) as usize];
                        let (a, b) = (pick(&mut d, &values), pick(&mut d, &values));
                        let compared = cursor.ins().icmp(cond, a, b);
                        Some(cursor.ins().uextend(types::I64, compared))
                    }
                    4 => {
                        let (c, a) = (pick(&mut d, &values), pick(&mut d, &values));
                        Some(cursor.ins().select(c, a, pick(&mut d, &values)))
                    }
                    5 => {
                        let (flags, address) = (flags(&mut d), pick(&mut d, &values));
                        let offset = 8 * d.below(3) as i32;
                        Some(cursor.ins().load(types::I64, flags, address, offset))
                    }
                    6 => {
                        let (flags, value) = (flags(&mut d), pick(&mut d, &values));
                        let (address, offset) = (pick(&mut d, &values), 8 * d.below(3) as i32);
                        cursor.ins().store(flags, value, address, offset);
                        None
                    }
                    7 => {
                        let slot = slots[d.below(2) as usize];
                        Some(cursor.ins().stack_addr(types::I64, slot, 8 * d.below(2) as i32))
                    }
                    8 => {
                        let (callee, params) = funcs[d.below(3) as usize];
                        let args: Vec<ir::Value> = (0..params).map(|_| pick(&mut d, &values)).collect();
                        let call = cursor.ins().call(callee, &args);
                        if d.below(4) == 0 {
                            let slot = slots[d.below(2) as usize];
                            let entry =
                                ir::UserStackMapEntry { ty: types::I64, slot, offset: 8 * d.below(2) };
                            cursor.func.dfg.append_user_stack_map_entry(call, entry);
                        }
                        Some(cursor.func.dfg.first_result(call))
                    }
                    9 => Some(cursor.ins().func_addr(types::I64, funcs[d.below(3) as usize].0)),
                    10 => Some(cursor.ins().symbol_value(types::I64, globals[d.below(3) as usize])),
                    11 => {
                        let bits = u64::from(d.below(3)) << 52;
                        cursor.ins().f64const(ir::immediates::Ieee64::with_bits(bits));
                        None
                    }
                    12 => {
                        let bits = d.below(3) << 23;
                        cursor.ins().f32const(ir::immediates::Ieee32::with_bits(bits));
                        None
                    }
                    13 => {
                        let data = vec![d.below(3) as u8; 16].into();
                        let constant = cursor.func.dfg.constants.insert(data);
                        cursor.ins().vconst(types::I8X16, constant);
                        None
                    }
                    14 => {
                        let code = [ir::TrapCode::INTEGER_OVERFLOW, ir::TrapCode::HEAP_OUT_OF_BOUNDS]
                            [d.below(2) as usize];
                        cursor.ins().trapnz(pick(&mut d, &values), code);
                        None
                    }
                    _ => {
                        let op = [ir::AtomicRmwOp::Add, ir::AtomicRmwOp::Xchg][d.below(2) as usize];
                        let (address, value) = (pick(&mut d, &values), pick(&mut d, &values));
                        let flags = ir::MemFlagsData::trusted();
                        Some(cursor.ins().atomic_rmw(types::I64, flags, op, address, value))
                    }
                };
                values.extend(made);
            }
            let Some(&next) = blocks.get(at + 1) else {
                cursor.ins().return_(&[pick(&mut d, &values)]);
                continue;
            };
            let wants = cursor.func.dfg.block_params(next).len();
            match d.below(4) {
                0 => {
                    cursor.ins().jump(next, &args(&mut d, &values, wants));
                }
                1 => {
                    let condition = pick(&mut d, &values);
                    let (then, otherwise) = (args(&mut d, &values, wants), args(&mut d, &values, wants));
                    cursor.ins().brif(condition, next, &then, next, &otherwise);
                }
                2 => {
                    let calls: Vec<ir::BlockCall> = (0..1 + d.below(3))
                        .map(|_| {
                            let args = args(&mut d, &values, wants);
                            cursor.func.dfg.block_call(next, &args)
                        })
                        .collect();
                    let table = ir::JumpTableData::new(calls[0], &calls[1..]);
                    let table = cursor.func.create_jump_table(table);
                    cursor.ins().br_table(pick(&mut d, &values), table);
                }
                _ => {
                    let (callee, params) = funcs[d.below(3) as usize];
                    let args: Vec<ir::Value> = (0..params).map(|_| pick(&mut d, &values)).collect();
                    let signature = cursor.func.dfg.ext_funcs[callee].signature;
                    let normal = cursor.func.dfg.block_call(next, &[ir::BlockArg::TryCallRet(0)]);
                    let caught = cursor.func.dfg.block_call(next, &[ir::BlockArg::TryCallExn(0)]);
                    let other = cursor.func.dfg.block_call(next, &[]);
                    let tag = ir::ExceptionTag::from_u32(d.below(2));
                    let items = [
                        ir::ExceptionTableItem::Tag(tag, caught),
                        ir::ExceptionTableItem::Default(other),
                    ];
                    let table = ir::ExceptionTableData::new(signature, normal, items);
                    let table = cursor.func.dfg.exception_tables.push(table);
                    cursor.ins().try_call(callee, &args, table);
                }
            }
        }
        func
    }

    #[test]
    fn the_walk_tells_apart_what_the_text_did() {
        let (mut equal, mut other) = (0, 0);
        for seed in 0..300u64 {
            let base = random(seed, None, None);
            let want = text::masked(&base, &names);
            let got = masked_ir(&base, PLAIN);
            assert_eq!(want.is_some(), got.is_some(), "seed {seed}");
            let renumbered = random(seed, None, Some(seed ^ 0x5eed));
            if let (Some((text, _)), Some((renumbered, _))) = (&want, text::masked(&renumbered, &names))
            {
                assert_eq!(*text, renumbered, "seed {seed}: the noise changed the function");
            }
            let mut variants = vec![renumbered];
            for at in [0, 2, 5, 9, 14, 20, 27, 35, 44, 54, 65, 80] {
                variants.push(random(seed, Some(at), Some(seed.wrapping_mul(31) + u64::from(at))));
            }
            variants.push(random(seed + 10_000, None, None));
            for variant in &variants {
                let (theirs, ours) = (text::masked(variant, &names), masked_ir(variant, PLAIN));
                assert_eq!(theirs.is_some(), ours.is_some(), "seed {seed}");
                let (Some((text_a, constants_a)), Some((text_b, constants_b))) = (&want, &theirs)
                else {
                    continue;
                };
                let (a, b) = (got.as_ref().unwrap(), ours.as_ref().unwrap());
                assert_eq!(*constants_a, a.constants, "seed {seed}");
                assert_eq!(*constants_b, b.constants, "seed {seed}");
                assert_eq!(text_a == text_b, a.shape == b.shape, "seed {seed}:\n{text_a}\n{text_b}");
                if text_a == text_b { equal += 1 } else { other += 1 }
            }
        }
        assert!(equal > 300 && other > 300, "{equal} equal, {other} other pairs");
    }

    /// One `atomic_rmw` with `flags`.
    fn atomic(flags: ir::MemFlagsData) -> ir::Function {
        let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), signature());
        let block = func.dfg.make_block();
        func.layout.append_block(block);
        let arg = func.dfg.append_block_param(block, types::I64);
        let mut cursor = FuncCursor::new(&mut func).at_bottom(block);
        let old = cursor.ins().atomic_rmw(types::I64, flags, ir::AtomicRmwOp::Add, arg, arg);
        cursor.ins().return_(&[old]);
        func
    }

    #[test]
    fn an_atomics_flags_are_in_the_shape() {
        let trusted = ir::MemFlagsData::trusted();
        let (a, b) = (atomic(trusted), atomic(trusted.with_endianness(ir::Endianness::Big)));
        // The printer leaves them out.
        assert_eq!(text::masked(&a, &names), text::masked(&b, &names));
        let (a, b) = (masked_ir(&a, PLAIN).unwrap(), masked_ir(&b, PLAIN).unwrap());
        assert_ne!(a.shape, b.shape);
    }

    /// A function reading a global value loaded with `flags` from the VM context.
    fn global_load(flags: ir::MemFlagsData) -> ir::Function {
        let mut func = ir::Function::with_name_signature(ir::UserFuncName::default(), signature());
        let vmctx = func.create_global_value(ir::GlobalValueData::VMContext);
        let flags = func.dfg.mem_flags.insert(flags).unwrap();
        let loaded = func.create_global_value(ir::GlobalValueData::Load {
            base: vmctx,
            offset: 8.into(),
            global_type: types::I64,
            flags,
        });
        let block = func.dfg.make_block();
        func.layout.append_block(block);
        func.dfg.append_block_param(block, types::I64);
        let mut cursor = FuncCursor::new(&mut func).at_bottom(block);
        let value = cursor.ins().symbol_value(types::I64, loaded);
        cursor.ins().return_(&[value]);
        func
    }

    #[test]
    fn a_global_loads_flags_are_in_the_shape() {
        let trusted = ir::MemFlagsData::trusted();
        let (a, b) = (global_load(trusted), global_load(trusted.with_readonly()));
        // The text named the flags by their handle, the same number in both.
        assert_eq!(text::masked(&a, &names), text::masked(&b, &names));
        let (a, b) = (masked_ir(&a, PLAIN).unwrap(), masked_ir(&b, PLAIN).unwrap());
        assert_ne!(a.shape, b.shape);
        assert_eq!(masked_ir(&global_load(trusted), PLAIN).unwrap().shape, a.shape);
    }

    #[test]
    fn masking_reuses_the_threads_buffers() {
        let (big, small) = (random(7, None, None), grow(8, 0, 1, false));
        let first = masked_ir(&small, PLAIN);
        masked_ir(&big, PLAIN);
        assert_eq!(masked_ir(&small, PLAIN), first);
    }

    fn verified(func: &ir::Function) {
        let flags = settings::Flags::new(settings::builder());
        if let Err(errors) = cranelift_codegen::verify_function(func, &flags) {
            let shown = cranelift_codegen::print_errors::pretty_verifier_error(func, None, errors);
            panic!("{shown}");
        }
        let mut ctx = cranelift_codegen::Context::for_function(func.clone());
        assert!(!crate::emit(&mut ctx).unwrap().bytes.is_empty());
    }

    #[test]
    fn two_members_merge_into_a_body_and_thunks() {
        let pointer = crate::target().unwrap().pointer_type();
        // Two sizes, two callees, one static (place 1 of both).
        let (a, b) = (grow(8, 0, 1, false), grow(24, 0, 1, true));
        let refs_a = vec![Target::Function(key(10)), Target::Extern("STATIC".to_owned())];
        let refs_b = vec![Target::Function(key(11)), Target::Extern("STATIC".to_owned())];
        let (ma, mb) = (masked_ir(&a, PLAIN).unwrap(), masked_ir(&b, PLAIN).unwrap());
        let settings = Settings(ContentId([1; 32]));
        let limits = Limits { thunk_size: 0, ..Limits::default() };
        let forms = Forms::new();
        let (ma, mb) = (Masking::of(&forms, &ma), Masking::of(&forms, &mb));
        let found = groups(
            &[(key(1), settings, ma, &refs_a[..]), (key(2), settings, mb, &refs_b[..])],
            &limits,
        );
        let group = &found[0];
        assert_eq!(group.varying, vec![0, 1]);
        let members: [Member<'_>; 2] = [(&a, PLAIN, &refs_a[..]), (&b, PLAIN, &refs_b[..])];
        let Folded { body, thunks } = build(group, &members, pointer).unwrap();

        let mut want = signature();
        want.params.push(AbiParam::new(types::I64));
        want.params.push(AbiParam::new(pointer));
        assert_eq!(body.signature, want);
        let opcodes: Vec<Opcode> = body
            .layout
            .blocks()
            .flat_map(|block| body.layout.block_insts(block))
            .map(|inst| body.dfg.insts[inst].opcode())
            .collect();
        assert!(opcodes.contains(&Opcode::CallIndirect));
        assert!(!opcodes.contains(&Opcode::Call) && !opcodes.contains(&Opcode::FuncAddr));
        assert!(!opcodes.contains(&Opcode::Iconst));
        // The static and the data are equal for both: still named, not parameters.
        assert_eq!(opcodes.iter().filter(|&&it| it == Opcode::SymbolValue).count(), 2);
        verified(&body);

        for ((thunk, member), size) in thunks.iter().zip([&a, &b]).zip([8, 24]) {
            assert_eq!(thunk.signature, member.signature);
            let block = thunk.layout.entry_block().unwrap();
            let insts: Vec<ir::Inst> = thunk.layout.block_insts(block).collect();
            let call = insts[insts.len() - 2];
            let InstructionData::Call { func_ref, .. } = thunk.dfg.insts[call] else {
                panic!("a thunk calls the body")
            };
            let ir::ExternalName::User(name) = thunk.dfg.ext_funcs[func_ref].name else {
                panic!("the body is named by a user name")
            };
            assert_eq!(thunk.params.user_named_funcs()[name].namespace, BODY);
            let args = thunk.dfg.inst_args(call);
            assert_eq!(args.len(), 3);
            assert_eq!(args[0], thunk.dfg.block_params(block)[0]);
            let InstructionData::UnaryImm { imm, .. } = thunk.dfg.insts[insts[0]] else {
                panic!("the size is made first")
            };
            assert_eq!(imm.bits(), size);
            let InstructionData::FuncAddr { func_ref, .. } = thunk.dfg.insts[insts[1]] else {
                panic!("then the callee's address")
            };
            let ir::ExternalName::User(name) = thunk.dfg.ext_funcs[func_ref].name else {
                panic!("the callee is named as the member names it")
            };
            assert_eq!(thunk.params.user_named_funcs()[name], ir::UserExternalName::new(0, 0));
            assert_eq!(thunk.dfg.insts[insts[insts.len() - 1]].opcode(), Opcode::Return);
            verified(thunk);
        }
        assert_eq!(group.thunk_refs(&refs_b).last(), Some(&Target::Direct(group.key())));
    }

    #[test]
    fn differing_data_is_made_by_the_thunks() {
        let pointer = crate::target().unwrap().pointer_type();
        // One body for two vtables: data objects 7 and 9.
        let (a, b) = (grow_with(8, 0, 1, 7, false), grow_with(8, 0, 1, 9, true));
        let refs = vec![Target::Function(key(10)), Target::Extern("STATIC".to_owned())];
        let (ma, mb) = (masked_ir(&a, PLAIN).unwrap(), masked_ir(&b, PLAIN).unwrap());
        let settings = Settings(ContentId([1; 32]));
        let limits = Limits { thunk_size: 0, ..Limits::default() };
        let forms = Forms::new();
        let (ma, mb) = (Masking::of(&forms, &ma), Masking::of(&forms, &mb));
        let found = groups(
            &[(key(1), settings, ma, &refs[..]), (key(2), settings, mb, &refs[..])],
            &limits,
        );
        let group = &found[0];
        assert_eq!(group.varying, vec![3]);
        assert_eq!(group.constants[1], vec![Constant::Data(ContentId([9; 32]))]);
        let members: [Member<'_>; 2] = [(&a, PLAIN, &refs[..]), (&b, PLAIN, &refs[..])];
        let Folded { body, thunks } = build(group, &members, pointer).unwrap();
        assert_eq!(body.signature.params.last(), Some(&AbiParam::new(pointer)));
        let symbols = body
            .layout
            .blocks()
            .flat_map(|block| body.layout.block_insts(block))
            .filter(|&inst| body.dfg.insts[inst].opcode() == Opcode::SymbolValue)
            .count();
        assert_eq!(symbols, 1);
        verified(&body);
        for (thunk, data) in thunks.iter().zip([7, 9]) {
            let block = thunk.layout.entry_block().unwrap();
            let first = thunk.layout.first_inst(block).unwrap();
            let InstructionData::UnaryGlobalValue { opcode, global_value } = thunk.dfg.insts[first]
            else {
                panic!("the thunk makes the data's address")
            };
            assert_eq!(opcode, Opcode::SymbolValue);
            let ir::GlobalValueData::Symbol { name: ir::ExternalName::User(name), .. } =
                thunk.global_values[global_value]
            else {
                panic!("the data is named as the member names it")
            };
            assert_eq!(thunk.params.user_named_funcs()[name], ir::UserExternalName::new(1, data));
            verified(thunk);
        }
    }

    #[test]
    fn a_member_unlike_the_group_is_refused() {
        let pointer = types::I64;
        let (a, b) = (grow(8, 0, 1, false), grow(24, 0, 1, false));
        let refs = vec![Target::Function(key(10)), Target::Extern("STATIC".to_owned())];
        let other = vec![Target::Function(key(10)), Target::Extern("OTHER".to_owned())];
        let (ma, mb) = (masked_ir(&a, PLAIN).unwrap(), masked_ir(&b, PLAIN).unwrap());
        let settings = Settings(ContentId([1; 32]));
        let limits = Limits { thunk_size: 0, ..Limits::default() };
        let forms = Forms::new();
        let (ma, mb) = (Masking::of(&forms, &ma), Masking::of(&forms, &mb));
        let found = groups(
            &[(key(1), settings, ma, &refs[..]), (key(2), settings, mb, &refs[..])],
            &limits,
        );
        let group = &found[0];
        assert_eq!(group.varying, vec![0]);
        // The static differs, but the group was made with it equal.
        let members: [Member<'_>; 2] = [(&a, PLAIN, &refs[..]), (&b, PLAIN, &other[..])];
        let Err(why) = build(group, &members, pointer) else { panic!("a member unlike the group was built") };
        assert!(why.contains("varies"), "{why}");
        assert!(build(group, &members[..1], pointer).is_err());
    }
}
