//! LLVM for a release build of blaze v2 (docs/v2-codegen.md, "Release"): each function is
//! lowered and simplified by ThinLTO's pre-link pipeline once ([`prelink`]), which gives what
//! its summary says of it ([`Facts`]) and its export; then compiled from that module with the
//! exports it imports linked in and what the whole program says of it and its callees
//! applied ([`compile`]), through ThinLTO's post-link pipeline.
//!
//! A compile starts from the module its summary was read from, never from a second lowering:
//! a parameter the summary calls unused is passed `poison` by the callers, and is unused in
//! the code only because it is the same IR.
//!
//! Unsafe: what a summary says of a function is read from its use list, whose users may be
//! constants as well as calls, and from call operands, which inkwell gives only as values it
//! first classifies, one more call into LLVM each, and it has no `LLVMIsACallInst` to tell a
//! call user from a constant one. Those reads go through `LLVMValueRef`s here,
//! each `unsafe fn` with its `# Safety`; [`prelink`] and [`compile`] are safe and keep every
//! handle within the module they are given.

use blaze_codegen::{
    settings::{Sanitize, Sanitizers},
    summary::{Arg, Attrs},
};
use cranelift_codegen::ir;
use inkwell::{
    GlobalVisibility,
    attributes::AttributeLoc,
    context::Context,
    llvm_sys::{core, prelude::LLVMValueRef},
    memory_buffer::MemoryBuffer,
    module::{Linkage, Module},
    targets::FileType,
    types::BasicTypeEnum,
    values::{AsValueRef as _, BasicValueEnum, FunctionValue, InstructionOpcode},
};
use std::sync::Arc;

use blaze_hash::{HashMap, HashSet};
use smallvec::SmallVec;

use super::{
    INNER, Machine, Machinery, OWN, REBASED, Symbol, Unhandled, agree_calls, attribute_kind, emitted, link,
    lower::{Debug, Defined, Lowering},
    mark_features, mark_hint,
    pgo::{self, Counted},
    rebase, retold_bytes,
    sanitize::{self, Covered},
    told_constants, wrap,
};
use crate::{
    DataIdentity, Features,
    backend::{Optimization, Passes, Phase, Tally},
    fragment::{Code, DataRequest, Hint, Wants},
    imports::Import,
};

/// A function after LLVM's pre-link pipeline.
pub(crate) struct Prelinked {
    /// Its module, the function under [`OWN`], with its line table if one was asked for:
    /// what its own compile starts from.
    pub(crate) own: Arc<[u8]>,
    /// The same without a line table: what importers link in. The same buffer as `own`
    /// where there is no line table.
    pub(crate) export: Arc<[u8]>,
    /// Its instructions, as LLVM counts them.
    pub(crate) size: usize,
    pub(crate) facts: Facts,
    /// Built with a profile: whether the profile has a record of its name with another hash
    /// ([`pgo`]), so that it is built with no counts.
    pub(crate) mismatched: bool,
}

/// What the pre-linked function says of itself for its summary, its callees by the
/// requests they are of.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Facts {
    pub(crate) params: u32,
    /// The parameters it never reads, by place, leaving out those passed in memory the call
    /// makes (`byval`, `sret`...), whose argument is read by the call itself.
    pub(crate) unused: Vec<u32>,
    /// Its calls of the functions of `instances`, in the order of each callee's first call.
    pub(crate) calls: Vec<Called>,
    /// The functions of `instances` whose address it takes rather than calls, sorted.
    pub(crate) addressed: Vec<u32>,
    pub(crate) attrs: Attrs,
    pub(crate) body: Attrs,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Called {
    pub(crate) request: u32,
    pub(crate) count: u32,
    /// Per argument: what every call of it passes, where they agree.
    pub(crate) args: Vec<Arg>,
}

/// What a compile is given of the whole program, the functions by the requests they are of
/// in the compile's [`Wants`].
#[derive(Clone, Default, Debug)]
pub(crate) struct Given {
    /// Its parameters known to be these constants, by place.
    pub(crate) constants: Vec<(u32, Arg)>,
    /// What each function it calls may be assumed to be.
    pub(crate) callees: Vec<(u32, Attrs)>,
    /// The parameters of each function it calls that no caller need compute.
    pub(crate) dead: Vec<(u32, Vec<u32>)>,
}

/// The passes before the summary is read: ThinLTO's pre-link pipeline, after the slots are
/// taken apart as for a compile ([`Optimization::pipeline`]), so that the pre-link and the
/// post-link pipelines ([`postlink`]) are together what a compile at the level runs.
fn prelink_passes(optimization: Optimization) -> Option<&'static str> {
    match optimization.passes {
        Passes::None => None,
        Passes::Less => Some("function(sroa,instsimplify,sroa)"),
        Passes::Default | Passes::Aggressive => Some(super::exported(optimization)),
    }
}

/// The passes of a compile once its imports are linked in.
fn postlink(optimization: Optimization) -> Option<&'static str> {
    match optimization.passes {
        Passes::None => None,
        Passes::Less => Some("default<O1>"),
        Passes::Default => Some("thinlto<O2>"),
        Passes::Aggressive => Some(
            "cgscc(inline,function(sroa,instsimplify,sroa,jump-threading,early-cse<memssa>)),\
             thinlto<O3>,\
             function(gvn,sccp,instcombine<no-verify-fixpoint>,simplifycfg)",
        ),
    }
}

fn llvm(e: inkwell::support::LLVMString) -> Unhandled {
    Unhandled::Llvm(e.to_string_lossy().into_owned())
}

/// Runs `passes` over `module` through the passes kept for `machine`; `vectorize` as rustc
/// vectorizes straight-line code at `-O2` and `-O3`.
fn run(module: &Module<'_>, passes: &str, machine: &Machinery, vectorize: bool) -> Result<(), Unhandled> {
    Tally::global().time(Phase::Optimize, || machine.run(module, passes, vectorize))
}

/// Lowers `func` alone into a module, runs the pre-link pipeline of `optimization` on it,
/// and reads what its summary says. `instances` says which of the function requests of
/// `wants` are functions of the build (the others are outside symbols). `debug` asks for its
/// line table and variables. The function has the attributes of the sanitizers of `sanitize`
/// that `unsanitized` leaves on ([`sanitize::mark`]). With `pgo`, what it is built with for PGO
/// and the name its counts are kept under ([`pgo::name`]), it is instrumented or given its
/// profile's counts here, as LLVM's pre-link pipeline does. It is compiled with the target
/// features `features`, its unit's and its `#[target_feature]`'s ([`mark_features`]).
pub(crate) fn prelink(
    wants: &Wants,
    func: &ir::Function,
    named: &[ir::StackSlot],
    hint: Hint,
    debug: bool,
    optimization: Optimization,
    instances: &impl Fn(u32) -> bool,
    sanitize: &Sanitize,
    unsanitized: Sanitizers,
    pgo: Option<(pgo::Wanted, &str)>,
    features: Features,
) -> Result<Prelinked, Unhandled> {
    let tally = Tally::global();
    Machine::with_pgo(optimization, pgo.map(|it| it.0), features, |context, machine| {
        let module = context.create_module("blaze");
        module.set_triple(&machine.get_triple());
        module.set_data_layout(&machine.get_target_data().get_data_layout());
        // alloc: once per module; `Wants` (fragment.rs) lists these, and the lowering looks them up
        let constants: HashMap<u32, &[u8]> = wants.constants().collect();
        // alloc: once per module, as `constants`
        let statics: HashSet<u32> = wants.statics().collect();
        // alloc: once per module, as `constants`
        let readonly: HashMap<u32, &crate::ConstBlob> = wants.readonly().collect();
        let pointer = machine.get_target_data().get_pointer_byte_size(None) as usize;
        told_constants(context, &module, &readonly, pointer);
        let lines = if debug { Debug::of(&module)? } else { None };
        let own = Defined {
            name: OWN,
            func,
            pointees: &wants.pointees,
            named: if debug { named } else { &[] },
            weights: None,
            marked: debug,
        };
        let blocks_asm = wants.asm_blocks();
        let shown = HashMap::default();
        let function = tally.time(Phase::Lower, || {
            let function = Lowering::add(context, &module, &own)?;
            mark_hint(context, function, hint);
            mark_features(context, function, features)?;
            sanitize::mark(context, function, sanitize, unsanitized);
            Lowering::define(
                context,
                &module,
                function,
                &own,
                &constants,
                &statics,
                wants.resume(),
                wants.memcmp(),
                &blocks_asm,
                &shown,
                lines.as_ref(),
            )?;
            Ok::<_, Unhandled>(function)
        })?;
        told_functions(context, &module, wants, pointer)?;
        // The builder of the line table goes before the module is changed or let go of.
        let lined = lines.inspect(Debug::finish).is_some();
        tally.time(Phase::Verify, || module.verify()).map_err(llvm)?;
        let renamed = pgo.map(|(wanted, name)| pgo::instrumented(context, &module, function, wanted, name));
        if let Some(passes) = prelink_passes(optimization) {
            run(&module, passes, machine, true)?;
        }
        if pgo.is_some() && !pgo::in_pipeline(optimization) {
            tally.time(Phase::Optimize, || machine.run_pgo(&module))?;
        }
        let mismatched = machine.take_mismatched() > 0;
        if let Some(renamed) = renamed {
            renamed.restore();
        }
        let size = function.get_basic_blocks().iter().map(|it| it.get_instructions().count()).sum();
        let mut facts = facts(&module, function, instances);
        let own: Arc<[u8]> = module.write_bitcode_to_memory().as_slice().into();
        // The module is written: what follows changes it in place rather than a copy.
        let export = match lined {
            true => {
                module.strip_debug_info();
                module.write_bitcode_to_memory().as_slice().into()
            }
            false => Arc::clone(&own),
        };
        let body = optimistic(context, module, function, instances, machine)?;
        facts.body = Attrs { noreturn: facts.attrs.noreturn, ..body };
        Ok(Prelinked { own, export, size, facts, mismatched })
    })
}

/// LLVM's attributes `memory(read)` stands for nothing of an object's state but this: a
/// location's `Mod` bit is the high one of its two.
const MOD: u64 = 0xAAAA_AAAA_AAAA_AAAA;

/// What LLVM says of `function` in its attributes.
fn attrs_of(function: FunctionValue<'_>) -> Attrs {
    let has = |name| function.get_enum_attribute(AttributeLoc::Function, attribute_kind(name)).is_some();
    let memory = function
        .get_enum_attribute(AttributeLoc::Function, attribute_kind("memory"))
        .map(|it| it.get_enum_value());
    Attrs {
        nounwind: has("nounwind"),
        readonly: memory.is_some_and(|it| it & MOD == 0),
        readnone: memory == Some(0),
        norecurse: has("norecurse"),
        willreturn: has("willreturn"),
        noreturn: has("noreturn"),
    }
}

/// Tells LLVM what each read-only object of `wants` that holds a function's address holds,
/// vtables above all, as [`told_constants`] does for the others and rustc does for every
/// constant: a call through a vtable LLVM knows is a direct call, which a summary counts and an
/// importer can build in. Only objects `module` refers to are told, once every function of it
/// is declared: a function only an object names is declared with its request's signature,
/// and each call keeps the attributes of its own signature, so a call made direct passes
/// arguments as it did. The object stays `available_externally`: no copy is emitted, and it
/// keeps its one address.
fn told_functions<'ctx>(
    context: &'ctx Context,
    module: &Module<'ctx>,
    wants: &Wants,
    pointer: usize,
) -> Result<(), Unhandled> {
    for (index, data) in wants.requests.datas.iter().enumerate() {
        let blob = match data {
            DataRequest::Vtable { blob, .. } => blob,
            DataRequest::Blob { blob, identity } if *identity != DataIdentity::Writable => blob,
            _ => continue,
        };
        if blob.fn_relocs.is_empty() || !blob.type_ids.is_empty() || blob.bytes.is_empty() {
            continue;
        }
        let name = Symbol::Data(index as u32).named();
        let Some(old) = module.get_global(&name) else { continue };
        if old.get_initializer().is_some() {
            continue;
        }
        let mut addresses = blob
            .relocs
            .iter()
            .map(|&(offset, target, addend)| (offset as usize, Symbol::Data(target.as_u32()), addend))
            .chain(blob.fn_relocs.iter().map(|&(offset, target)| (offset as usize, Symbol::Func(target.as_u32()), 0)))
            .collect::<SmallVec<[(usize, Symbol, i64); 8]>>();
        blaze_ids::radix::sort_copy_by_key(&mut addresses, |it| it.0);
        let fits = addresses.windows(2).all(|it| it[0].0 + pointer <= it[1].0)
            && addresses.last().is_none_or(|it| it.0 + pointer <= blob.bytes.len());
        if !fits {
            return Err(Unhandled::Shape("an object whose addresses overlap"));
        }
        let mut fields: Vec<BasicValueEnum<'ctx>> = Vec::with_capacity(2 * addresses.len() + 1);
        let mut at = 0;
        for &(offset, target, addend) in &addresses {
            if offset > at {
                fields.push(context.const_string(&blob.bytes[at..offset], false).into());
            }
            let address = match target {
                Symbol::Data(target) => {
                    let name = Symbol::Data(target).named();
                    let global = module.get_global(&name).unwrap_or_else(|| {
                        let declared = module.add_global(context.i8_type(), None, &name);
                        declared.set_linkage(Linkage::External);
                        declared.set_visibility(GlobalVisibility::Hidden);
                        declared
                    });
                    global.as_pointer_value()
                }
                Symbol::Func(target) => {
                    let name = Symbol::Func(target).named();
                    let function = match module.get_function(&name) {
                        Some(known) => known,
                        None => {
                            let request = wants
                                .requests
                                .funcs
                                .get(target as usize)
                                .ok_or(Unhandled::Shape("an object naming a function nobody asked for"))?;
                            let ty = Lowering::function_type(context, &request.sig)?;
                            module.add_function(&name, ty, Some(Linkage::External))
                        }
                    };
                    function.as_global_value().as_pointer_value()
                }
            };
            let address = match addend {
                0 => address,
                // SAFETY: `address` is a global of `module` and the index an `i64` constant of
                // `context`, so this builds a constant expression owned by `context`; not
                // `inbounds`, so any `addend` is defined.
                _ => unsafe { address.const_gep(context.i8_type(), &[context.i64_type().const_int(addend as u64, true)]) },
            };
            fields.push(address.into());
            at = offset + pointer;
        }
        if at < blob.bytes.len() {
            fields.push(context.const_string(&blob.bytes[at..], false).into());
        }
        let value = context.const_struct(&fields, true);
        let told = module.add_global(value.get_type(), None, "");
        told.set_initializer(&value);
        told.set_linkage(Linkage::AvailableExternally);
        told.set_visibility(GlobalVisibility::Hidden);
        told.set_constant(true);
        told.set_alignment(blob.align as u32);
        old.as_pointer_value().replace_all_uses_with(told.as_pointer_value());
        // SAFETY: `old` is owned by `module`, and `replace_all_uses_with` just above sent every
        // use to `told`; `old` is not used after, so freeing it leaves nothing dangling.
        unsafe { old.delete() };
        told.set_name(&name);
    }
    Ok(())
}

/// `memory(read)` as the linked LLVM encodes it, which depends on how many kinds of location
/// it has: the same for every context of the process.
fn read_only(context: &Context) -> Result<u64, Unhandled> {
    static READ: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    if let Some(&read) = READ.get() {
        return Ok(read);
    }
    let read = parse_read_only(context)?;
    Ok(*READ.get_or_init(|| read))
}

fn parse_read_only(context: &Context) -> Result<u64, Unhandled> {
    let text = "declare void @read() memory(read)\n\0";
    let source = MemoryBuffer::create_from_memory_range_copy(text.as_bytes(), "memory");
    let parsed = context.create_module_from_ir(source).map_err(llvm)?;
    let function = parsed.get_function("read").ok_or(Unhandled::Shape("no `memory(read)`"))?;
    let memory = function.get_enum_attribute(AttributeLoc::Function, attribute_kind("memory"));
    memory.map(|it| it.get_enum_value()).ok_or(Unhandled::Shape("no `memory(read)`"))
}

/// The attributes of a parameter whose argument the call itself reads or writes, in memory
/// it makes: such an argument is never passed `poison`.
const IN_MEMORY: [&str; 5] = ["byval", "byref", "inalloca", "preallocated", "sret"];

/// The attributes that make a call with a `poison` argument undefined, which LLVM's
/// dead-argument elimination takes off an argument it replaces.
const UB_IF_POISON: [&str; 5] =
    ["noundef", "nonnull", "dereferenceable", "dereferenceable_or_null", "align"];

fn in_memory(function: FunctionValue<'_>, at: u32) -> bool {
    IN_MEMORY.iter().any(|name| {
        function.get_enum_attribute(AttributeLoc::Param(at), attribute_kind(name)).is_some()
    })
}

/// The request a function of the module is of, by its name.
fn request_of(function: FunctionValue<'_>) -> Option<u32> {
    match Symbol::parse(function.get_name().to_str().ok()?)? {
        Symbol::Func(at) => Some(at),
        Symbol::Data(_) => None,
    }
}

/// Whether `user` is a call or invoke that calls `function` and passes it nothing of itself.
///
/// # Safety
///
/// Both are values of one live module.
unsafe fn only_calls(user: LLVMValueRef, function: LLVMValueRef) -> bool {
    // SAFETY: `user` is a live value of one module, as this fn's `# Safety` says; these only
    // read its kind.
    let call = !unsafe { core::LLVMIsACallInst(user) }.is_null()
        // SAFETY: `user` is a live value of one module, as the `# Safety` says; this reads its
        // kind.
        || !unsafe { core::LLVMIsAInvokeInst(user) }.is_null();
    if !call {
        return false;
    }
    // SAFETY: `user` is a live call or invoke (just checked), which has a called operand.
    if unsafe { core::LLVMGetCalledValue(user) } != function {
        return false;
    }
    // SAFETY: `user` is a live call or invoke, whose first operands are its arguments.
    let args = unsafe { core::LLVMGetNumArgOperands(user) };
    // SAFETY: `at` is below `args`, so an argument operand of the live `user`.
    (0..args).all(|at| unsafe { core::LLVMGetOperand(user, at) } != function)
}

/// The calls and invokes of `function` in the module, each once.
fn calls_of(function: FunctionValue<'_>) -> SmallVec<[LLVMValueRef; 4]> {
    let mut calls = SmallVec::new();
    let target = function.as_value_ref();
    // SAFETY: `target` is a live function of its module, which owns its use list; nothing
    // changes it during this walk.
    let mut used = unsafe { core::LLVMGetFirstUse(target) };
    while !used.is_null() {
        // SAFETY: `used` is a non-null use of `target`, live during the walk.
        let user = unsafe { core::LLVMGetUser(used) };
        // SAFETY: `user` and `target` are live values of one module, as `only_calls` asks.
        if unsafe { only_calls(user, target) } && !calls.contains(&user) {
            calls.push(user);
        }
        // SAFETY: `used` is a live use of `target`; the list is unchanged during the walk.
        used = unsafe { core::LLVMGetNextUse(used) };
    }
    calls
}

/// What a constant argument is, where it is an integer of at most 64 bits.
///
/// # Safety
///
/// `value` is a live value of a module.
unsafe fn arg_of(value: LLVMValueRef) -> Arg {
    // SAFETY: `value` is a live value (the caller's operand of a live instruction, as the
    // `# Safety` asks); this reads its kind.
    if unsafe { core::LLVMIsAConstantInt(value) }.is_null() {
        return Arg::Unknown;
    }
    // SAFETY: `value` is live; its type is owned by its context.
    let ty = unsafe { core::LLVMTypeOf(value) };
    // SAFETY: `ty` is the integer type of a constant int.
    let bits = unsafe { core::LLVMGetIntTypeWidth(ty) };
    match u8::try_from(bits) {
        Ok(bits) if bits <= 64 => {
            // SAFETY: `value` is a constant int of at most 64 bits (checked by the arm).
            Arg::Int { bits, value: unsafe { core::LLVMConstIntGetZExtValue(value) } }
        }
        _ => Arg::Unknown,
    }
}

/// What `function`, alone in `module` after the pre-link pipeline, says of itself. Its
/// `body` is its `attrs` until [`optimistic`] says more.
fn facts<'ctx>(module: &Module<'ctx>, function: FunctionValue<'ctx>, instances: &impl Fn(u32) -> bool) -> Facts {
    let params = function.count_params();
    let unused = function
        .get_param_iter()
        .enumerate()
        .filter(|(at, param)| {
            // SAFETY: `param` is a parameter of `function`, owned by `module` for `'ctx`; this
            // only reads whether its use list is empty.
            let used = unsafe { !core::LLVMGetFirstUse(param.as_value_ref()).is_null() };
            !used && !in_memory(function, *at as u32)
        })
        .map(|(at, _)| at as u32)
        // alloc: once per function, `Facts::unused`, which its summary keeps
        .collect();
    // alloc: once per function, `Facts::calls`, which its summary keeps
    let mut calls: Vec<Called> = Vec::new();
    for block in function.get_basic_block_iter() {
        for instruction in block.get_instructions() {
            if !matches!(instruction.get_opcode(), InstructionOpcode::Call | InstructionOpcode::Invoke) {
                continue;
            }
            let at = instruction.as_value_ref();
            // SAFETY: `at` is a live call or invoke of `module` (opcode checked above), so it has
            // a called operand, owned by `module` for `'ctx`; it may be any value.
            let callee = unsafe { core::LLVMGetCalledValue(at) };
            // SAFETY: `callee` is the called operand just read, a value of `module` live for
            // `'ctx`; LLVMIsAFunction only reads its kind.
            if unsafe { core::LLVMIsAFunction(callee) }.is_null() {
                continue;
            }
            // SAFETY: LLVMIsAFunction just above said `callee` is a function of `module`, live
            // for `'ctx`.
            let Some(callee) = (unsafe { FunctionValue::new(callee) }) else { continue };
            let Some(request) = request_of(callee).filter(|it| instances(*it)) else { continue };
            // SAFETY: `at` is a live call or invoke of `module`, whose first operands are its
            // arguments.
            let count = unsafe { core::LLVMGetNumArgOperands(at) };
            let args = (0..count).map(|it| {
                // SAFETY: `it` is below `count`, so an argument operand of the live `at`.
                let operand = unsafe { core::LLVMGetOperand(at, it) };
                // SAFETY: `operand` is a live value of `module`, as `arg_of` asks.
                unsafe { arg_of(operand) }
            });
            match calls.iter_mut().find(|it| it.request == request) {
                Some(called) => {
                    called.count += 1;
                    if called.args.len() != args.len() {
                        let len = called.args.len().min(args.len());
                        called.args.truncate(len);
                        called.args.fill(Arg::Unknown);
                    }
                    for (known, arg) in called.args.iter_mut().zip(args) {
                        if *known != arg {
                            *known = Arg::Unknown;
                        }
                    }
                }
                // alloc: once per callee, `Called::args`, which its summary keeps
                None => calls.push(Called { request, count: 1, args: args.collect() }),
            }
        }
    }
    // alloc: once per function, `Facts::addressed`, which its summary keeps
    let mut addressed = Vec::new();
    for other in module.get_functions() {
        let Some(request) = request_of(other).filter(|it| instances(*it)) else { continue };
        let target = other.as_value_ref();
        let taken = {
            // SAFETY: `target` is a live function of `module`, which owns its use list;
            // nothing changes it during this walk.
            let mut used = unsafe { core::LLVMGetFirstUse(target) };
            let mut taken = false;
            while !used.is_null() && !taken {
                // SAFETY: `used` is a non-null use of `target`, live during the walk.
                let user = unsafe { core::LLVMGetUser(used) };
                // SAFETY: `user` and `target` are live values of one module, as `only_calls`
                // asks.
                taken = !unsafe { only_calls(user, target) };
                // SAFETY: `used` is a live use of `target`; the list is unchanged during the
                // walk.
                used = unsafe { core::LLVMGetNextUse(used) };
            }
            taken
        };
        if taken {
            addressed.push(request);
        }
    }
    blaze_ids::radix::sort(&mut addressed);
    addressed.dedup();
    let attrs = attrs_of(function);
    Facts { params, unused, calls, addressed, attrs, body: attrs }
}

/// What LLVM's attribute inference says of `function`, of `module`, when every function of
/// `instances` it calls is nounwind, readnone, willreturn and norecurse. It takes the module,
/// as what it assumes need not be true: nothing may be written or compiled from it after.
fn optimistic<'ctx>(
    context: &'ctx Context,
    module: Module<'ctx>,
    function: FunctionValue<'ctx>,
    instances: &impl Fn(u32) -> bool,
    machine: &Machinery,
) -> Result<Attrs, Unhandled> {
    let memory = attribute_kind("memory");
    for callee in module.get_functions() {
        if request_of(callee).is_none_or(|it| !instances(it)) {
            continue;
        }
        callee.remove_enum_attribute(AttributeLoc::Function, memory);
        callee.add_attribute(AttributeLoc::Function, context.create_enum_attribute(memory, 0));
        for name in ["nounwind", "willreturn", "norecurse"] {
            let kind = attribute_kind(name);
            callee.add_attribute(AttributeLoc::Function, context.create_enum_attribute(kind, 0));
        }
    }
    run(&module, "cgscc(function-attrs)", machine, false)?;
    Ok(attrs_of(function))
}

/// Compiles the function of `own`, a module [`prelink`] made, with `imports` linked in as
/// bodies only available externally and what `given` says applied, through the post-link
/// pipeline of `optimization`. `wants` has the function's requests and, after them, those
/// of the imports (`Import::funcs`, `Import::datas`); `signature` is the function's. The
/// code is instrumented for `sanitize` once optimized, and what SanitizerCoverage made for
/// it comes with it. Instrumented for PGO when [`prelink`] was, under `profiled` (its
/// [`pgo::name`]), it comes with what LLVM's record of it says. `features` are those
/// [`prelink`] was given, which `own` says of its function already.
pub(crate) fn compile(
    wants: &mut Wants,
    own: &[u8],
    signature: &ir::Signature,
    imports: &[Import],
    given: &Given,
    optimization: Optimization,
    sanitize: &Sanitize,
    profiled: Option<&str>,
    features: Features,
) -> Result<(Code, Covered, Option<Counted>), Unhandled> {
    let _span = tracing::info_span!(target: "blaze", "llvm compile").entered();
    let tally = Tally::global();
    // Where a parameter is the only way to its memory, as `compiled` wraps it.
    let wrapped = optimization.pipeline().is_some()
        && wants.pointees.iter().any(|(_, it)| it.unique)
        && crate::Target::selected().is_ok_and(|it| it.object_format == crate::ObjectFormat::MachO);
    Machine::with_pgo(optimization, None, features, |context, machine| {
        let module = context.create_module("blaze");
        module.set_triple(&machine.get_triple());
        module.set_data_layout(&machine.get_target_data().get_data_layout());
        // alloc: once per module; `Wants` (fragment.rs) lists these, and the lowering looks them up
        let readonly: HashMap<u32, &crate::ConstBlob> = wants.readonly().collect();
        let pointer = machine.get_target_data().get_pointer_byte_size(None) as usize;
        told_constants(context, &module, &readonly, pointer);
        tally.time(Phase::Lower, || {
            let source = MemoryBuffer::create_from_memory_range(own, "own");
            let parsed = context.create_module_from_ir(source).map_err(llvm)?;
            module.link_in_module(parsed).map_err(llvm)?;
            link(context, &module, imports, Linkage::AvailableExternally)?;
            if !imports.is_empty() {
                agree_calls(context, &module)?;
            }
            Ok::<_, Unhandled>(())
        })?;
        told_functions(context, &module, wants, pointer)?;
        let function = module.get_function(OWN).ok_or(Unhandled::Shape("a module without its function"))?;
        apply(context, &module, function, given, pointer as u32 * 8)?;
        if wrapped {
            function.as_global_value().set_name(INNER);
            wrap(context, &module, function, None)?;
        }
        tally.time(Phase::Verify, || module.verify()).map_err(llvm)?;
        if let Some(passes) = postlink(optimization) {
            run(&module, passes, machine, true)?;
            if rebase::rebase(&module) {
                run(&module, REBASED, machine, false)?;
            }
        }
        if !imports.is_empty() {
            run(&module, "elim-avail-extern", machine, false)?;
        }
        sanitize::instrument(&module, machine, sanitize)?;
        retold_bytes(context, &module, &readonly);
        if module.get_function(INNER).is_some() {
            return Err(Unhandled::Shape("a body not built into the function that calls it"));
        }
        let counted = profiled.map(|name| pgo::counted(&module, name)).transpose()?;
        let object = tally
            .time(Phase::Emit, || machine.write_to_memory_buffer(&module, FileType::Object))
            .map_err(llvm)?;
        let own = (OWN, signature);
        if let Some(counted) = counted {
            let covered = sanitize.coverage.is_on();
            let (code, coverage, bytes) =
                tally.time(Phase::Read, || emitted::read_profiled(object.as_slice(), own, wants, covered))?;
            if u64::from(bytes) != u64::from(counted.counters) * 8 {
                return Err(Unhandled::Shape("PGO counters of another size than the record says"));
            }
            return Ok((code, coverage, Some(counted)));
        }
        if sanitize.coverage.is_on() {
            let (code, coverage) =
                tally.time(Phase::Read, || emitted::read_covered(object.as_slice(), own, wants))?;
            return Ok((code, coverage, None));
        }
        let mut codes = tally.time(Phase::Read, || emitted::read(object.as_slice(), &[own], wants))?;
        let code = codes.pop().ok_or(Unhandled::Shape("an object without the function"))??;
        Ok((code, Covered::default(), None))
    })
}

/// Gives `function` and the functions of `module` it calls what `given` says of them.
/// `pointer` is the width of an address, in bits.
fn apply<'ctx>(
    context: &'ctx Context,
    module: &Module<'ctx>,
    function: FunctionValue<'ctx>,
    given: &Given,
    pointer: u32,
) -> Result<(), Unhandled> {
    for &(at, arg) in &given.constants {
        let param = function
            .get_nth_param(at)
            .ok_or(Unhandled::Shape("a constant for a parameter the function lacks"))?;
        let Arg::Int { bits, value } = arg else {
            return Err(Unhandled::Shape("a constant parameter that is not a constant"));
        };
        let constant: BasicValueEnum<'ctx> = match param.get_type() {
            BasicTypeEnum::IntType(ty) if ty.get_bit_width() == u32::from(bits) => {
                ty.const_int(value, false).into()
            }
            BasicTypeEnum::PointerType(ty) if u32::from(bits) == pointer => {
                let int = std::num::NonZero::new(u32::from(bits))
                    .and_then(|bits| context.custom_width_int_type(bits).ok())
                    .ok_or(Unhandled::Shape("a pointer constant of a width LLVM has no integer of"))?;
                int.const_int(value, false).const_to_pointer(ty).into()
            }
            _ => return Err(Unhandled::Shape("a constant of another type than its parameter")),
        };
        // SAFETY: `param` is a parameter of `function` and `constant` a constant of `context`,
        // both live for `'ctx` and of one type (matched just above); a different type would
        // break the IR's typing.
        unsafe { core::LLVMReplaceAllUsesWith(param.as_value_ref(), constant.as_value_ref()) };
    }
    let memory = attribute_kind("memory");
    let read = if given.callees.iter().any(|(_, it)| it.readonly && !it.readnone) {
        read_only(context)?
    } else {
        0
    };
    for &(request, attrs) in &given.callees {
        let Some(callee) = module.get_function(&Symbol::Func(request).named()) else { continue };
        let flags = [
            ("nounwind", attrs.nounwind),
            ("willreturn", attrs.willreturn),
            ("norecurse", attrs.norecurse),
            ("noreturn", attrs.noreturn),
        ];
        for (name, _) in flags.iter().filter(|it| it.1) {
            let kind = attribute_kind(name);
            callee.add_attribute(AttributeLoc::Function, context.create_enum_attribute(kind, 0));
        }
        let assumed = match (attrs.readnone, attrs.readonly) {
            (true, _) => Some(0),
            (false, true) => Some(read),
            (false, false) => None,
        };
        if let Some(assumed) = assumed {
            // What the declaration says already holds too: the two are met.
            let had = callee.get_enum_attribute(AttributeLoc::Function, memory).map(|it| it.get_enum_value());
            let value = had.map_or(assumed, |had| had & assumed);
            callee.remove_enum_attribute(AttributeLoc::Function, memory);
            callee.add_attribute(AttributeLoc::Function, context.create_enum_attribute(memory, value));
        }
    }
    for (request, params) in &given.dead {
        let Some(callee) = module.get_function(&Symbol::Func(*request).named()) else { continue };
        let params = params
            .iter()
            .copied()
            .filter(|&at| at < callee.count_params() && !in_memory(callee, at))
            .collect::<SmallVec<[u32; 8]>>();
        for &at in &params {
            for name in UB_IF_POISON {
                callee.remove_enum_attribute(AttributeLoc::Param(at), attribute_kind(name));
            }
        }
        for call in calls_of(callee) {
            // SAFETY: `call` is a live call or invoke of `module` (from `calls_of`), changed
            // below only in its operands and attributes.
            let args = unsafe { core::LLVMGetNumArgOperands(call) };
            for &at in params.iter().filter(|&&at| at < args) {
                // SAFETY: `at` is below `args`, so an argument operand of `call`.
                let arg = unsafe { core::LLVMGetOperand(call, at) };
                // SAFETY: `arg` is live; its type is owned by the context.
                let ty = unsafe { core::LLVMTypeOf(arg) };
                // SAFETY: `ty` is a first-class type of the module's context, which owns the
                // poison constant.
                let poison = unsafe { core::LLVMGetPoison(ty) };
                // SAFETY: `poison` has the type of the argument it replaces, so the call stays
                // well typed; `at` is an argument operand of `call`.
                unsafe { core::LLVMSetOperand(call, at, poison) };
                for name in UB_IF_POISON {
                    // SAFETY: `at + 1` is the attribute index of argument `at` of the live
                    // `call`.
                    unsafe {
                        core::LLVMRemoveCallSiteEnumAttribute(call, at + 1, attribute_kind(name))
                    };
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use inkwell::values::AnyValue as _;

    use super::*;

    fn parsed<'ctx>(context: &'ctx Context, text: &str) -> Module<'ctx> {
        let text = format!("{text}\0");
        let source = MemoryBuffer::create_from_memory_range_copy(text.as_bytes(), "test");
        context.create_module_from_ir(source).unwrap()
    }

    /// A target machine and its passes, for the one context of a test.
    fn machine() -> Machinery {
        let optimization = Optimization {
            passes: Passes::Aggressive,
            selection: crate::backend::Selection::Optimized,
        };
        Machinery::new(Machine::create(optimization).unwrap())
    }

    /// Calls its request 1 twice with one constant and its request 2 with two, takes the
    /// address of its request 3, calls the symbol 4 and never reads its second parameter.
    const CALLER: &str = r#"
define i32 @"blaze.own"(i32 %n, i64 %unused, ptr byval(i64) %copied) {
  %a = call i32 @"blaze.f.1"(i32 7, i32 %n)
  %b = call i32 @"blaze.f.1"(i32 7, i32 3)
  %c = call i32 @"blaze.f.2"(i32 1)
  %d = call i32 @"blaze.f.2"(i32 2)
  %e = call i32 @"blaze.f.4"(ptr @"blaze.f.3")
  %ab = add i32 %a, %b
  %cd = add i32 %c, %d
  %all = add i32 %ab, %cd
  %more = add i32 %all, %e
  ret i32 %more
}

declare i32 @"blaze.f.1"(i32, i32)
declare i32 @"blaze.f.2"(i32)
declare i32 @"blaze.f.3"(i32)
declare i32 @"blaze.f.4"(ptr)
"#;

    /// A call through the second word of the object of request 0, whose word is the address
    /// of function request 0, which nothing else names.
    #[test]
    fn a_call_through_a_told_vtable_is_direct() {
        use cranelift_codegen::ir::{AbiParam, Signature, types};

        let context = Context::create();
        let module = parsed(
            &context,
            r#"
@"blaze.d.0" = external hidden global i8

define i32 @"blaze.own"() {
  %slot = getelementptr i8, ptr @"blaze.d.0", i64 8
  %method = load ptr, ptr %slot
  %r = call i32 %method()
  ret i32 %r
}
"#,
        );
        let mut wants = Wants::new(&**crate::target().unwrap());
        let mut signature = Signature::new(cranelift_codegen::isa::CallConv::SystemV);
        signature.returns.push(AbiParam::new(types::I32));
        let method = wants.func(crate::fragment::FuncTarget::Symbol("method".to_owned()), &signature);
        let vtable = crate::ConstBlob {
            bytes: vec![0; 16],
            fn_relocs: vec![(8, method)],
            align: 8,
            ..crate::ConstBlob::default()
        };
        wants.define_data_as(vtable, DataIdentity::Shared);
        told_functions(&context, &module, &wants, 8).unwrap();
        module.verify().unwrap();
        run(&module, "function(instcombine)", &machine(), false).unwrap();
        let text = module.print_to_string().to_string();
        assert!(text.contains(r#"call i32 @"blaze.f.0"()"#), "{text}");
        let told = module.get_global("blaze.d.0").unwrap();
        assert_eq!(told.get_linkage(), Linkage::AvailableExternally);
        assert!(told.is_constant());
    }

    #[test]
    fn the_facts_of_a_function_alone() {
        let context = Context::create();
        let module = parsed(&context, CALLER);
        let function = module.get_function(OWN).unwrap();
        let instances = |it: u32| it != 4;
        let facts = facts(&module, function, &instances);
        assert_eq!(facts.params, 3);
        // The `byval` one is read by the call.
        assert_eq!(facts.unused, vec![1]);
        let int = |value| Arg::Int { bits: 32, value };
        assert_eq!(
            facts.calls,
            vec![
                Called { request: 1, count: 2, args: vec![int(7), Arg::Unknown] },
                Called { request: 2, count: 2, args: vec![Arg::Unknown] },
            ]
        );
        assert_eq!(facts.addressed, vec![3]);
    }

    const PURE: &str = r#"
define i32 @"blaze.own"(i32 %n) {
  %a = call i32 @"blaze.f.1"(i32 %n)
  %b = add i32 %a, 1
  ret i32 %b
}

declare i32 @"blaze.f.1"(i32)
"#;

    #[test]
    fn the_body_assumes_the_best_of_the_builds_callees_and_the_function_nothing() {
        let context = Context::create();
        let machine = machine();
        let module = parsed(&context, PURE);
        let function = module.get_function(OWN).unwrap();
        let alone = facts(&module, function, &|_| true);
        assert!(!alone.attrs.nounwind && !alone.attrs.readnone, "{:?}", alone.attrs);
        assert_eq!(alone.body, alone.attrs);
        // Run on the module itself, it says what it said run on a copy.
        let copy = module.clone();
        let copied = copy.get_function(OWN).unwrap();
        let of_copy = optimistic(&context, copy, copied, &|_| true, &machine).unwrap();
        let body = optimistic(&context, module, function, &|_| true, &machine).unwrap();
        assert!(body.nounwind && body.readnone && body.readonly, "{body:?}");
        assert!(body.willreturn && body.norecurse, "{body:?}");
        assert_eq!(of_copy, body);
        // An outside symbol is assumed nothing of.
        let module = parsed(&context, PURE);
        let function = module.get_function(OWN).unwrap();
        let outside = facts(&module, function, &|_| false);
        assert!(outside.calls.is_empty());
        let body = optimistic(&context, module, function, &|_| false, &machine).unwrap();
        assert!(!body.nounwind && !body.readnone, "{body:?}");
    }

    #[test]
    fn given_facts_are_applied() {
        let context = Context::create();
        let module = parsed(&context, CALLER);
        let function = module.get_function(OWN).unwrap();
        let given = Given {
            constants: vec![(0, Arg::Int { bits: 32, value: 5 })],
            callees: vec![
                (1, Attrs { nounwind: true, readonly: true, ..Attrs::default() }),
                (2, Attrs { readnone: true, readonly: true, willreturn: true, ..Attrs::default() }),
            ],
            dead: vec![(1, vec![0])],
        };
        apply(&context, &module, function, &given, 64).unwrap();
        module.verify().unwrap();
        let text = function.print_to_string().to_string_lossy().into_owned();
        assert!(text.contains("@blaze.f.1(i32 poison, i32 5)"), "{text}");
        assert!(text.contains("@blaze.f.1(i32 poison, i32 3)"), "{text}");
        let all = module.print_to_string().to_string_lossy().into_owned();
        let one = attrs_of(module.get_function("blaze.f.1").unwrap());
        assert!(one.nounwind && one.readonly && !one.readnone, "{all}");
        let two = attrs_of(module.get_function("blaze.f.2").unwrap());
        assert!(two.readnone && two.willreturn && !two.nounwind, "{all}");
    }

    #[test]
    fn a_dead_argument_loses_what_makes_poison_undefined() {
        let context = Context::create();
        let module = parsed(
            &context,
            r#"
define void @"blaze.own"(ptr %p) {
  call void @"blaze.f.1"(ptr noundef nonnull %p)
  ret void
}

declare void @"blaze.f.1"(ptr noundef nonnull)
"#,
        );
        let function = module.get_function(OWN).unwrap();
        let given = Given { dead: vec![(1, vec![0])], ..Given::default() };
        apply(&context, &module, function, &given, 64).unwrap();
        module.verify().unwrap();
        let all = module.print_to_string().to_string_lossy().into_owned();
        assert!(all.contains("@blaze.f.1(ptr poison)"), "{all}");
        assert!(!all.contains("noundef") && !all.contains("nonnull"), "{all}");
    }

    #[test]
    fn a_pre_linked_function_compiles_with_its_import_built_in() {
        let context = Context::create();
        let optimization = Optimization {
            passes: Passes::Aggressive,
            selection: crate::backend::Selection::Optimized,
        };
        let machine = machine();
        let module = parsed(&context, PURE);
        run(&module, prelink_passes(optimization).unwrap(), &machine, true).unwrap();
        let callee = parsed(
            &context,
            r#"
define i32 @"blaze.own"(i32 %n) {
  %m = mul i32 %n, 3
  ret i32 %m
}
"#,
        );
        let export = crate::imports::Export {
            bitcode: callee.write_bitcode_to_memory().as_slice().into(),
            requests: Default::default(),
            signature: ir::Signature::new(cranelift_codegen::isa::CallConv::SystemV),
            size: 2,
            hint: Hint::None,
        };
        let import = Import { export: std::sync::Arc::new(export), id: 1, funcs: vec![1], datas: Vec::new() };
        let into = context.create_module("blaze");
        into.link_in_module(module).unwrap();
        link(&context, &into, std::slice::from_ref(&import), Linkage::AvailableExternally).unwrap();
        run(&into, postlink(optimization).unwrap(), &machine, true).unwrap();
        run(&into, "elim-avail-extern", &machine, false).unwrap();
        let own = into.get_function(OWN).unwrap();
        let text = own.print_to_string().to_string_lossy().into_owned();
        assert!(!text.contains("blaze.f.1") && text.contains("mul i32"), "{text}");
    }
}
