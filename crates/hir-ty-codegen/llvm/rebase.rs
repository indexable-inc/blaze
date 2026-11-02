//! Addresses LLVM is left to compute as integers once it has built one function into another.
//!
//! The IR a function is lowered from has no pointers, only integers, and [`super::lower`]
//! tells which are addresses from what the function does with them. A function that is
//! given an address and only adds to it, or keeps it, has an integer: `NonNull::add`, an
//! iterator's `next`. Built into a caller that has the address as a pointer, that is
//! `inttoptr(ptrtoint p + n)`, or a loop's `phi` of such integers, which LLVM does not take
//! to be based on `p`: what is stored through `p` is loaded again, and a slot whose address
//! it is stays in memory (nbody's `delta`, summed through `iter()`). The lowering already
//! takes a pointer plus any integer to be based on the pointer; this does so for what
//! building in made of it, with `getelementptr i8, p, n` and `phi`s of pointers.
//!
//! Unsafe: the walk is over raw `LLVMValueRef`s, as a web of addresses mixes instructions with
//! constant expressions (`ptrtoint @g`, `add` of one), which inkwell has no type for, and
//! inkwell would classify every operand it hands back with a further call into LLVM. The one
//! entry, [`rebase`], is safe: it takes a live `&Module` and keeps every handle inside the
//! call. Each `unsafe fn` here says in `# Safety` which values must be live; the tests below
//! check the rewrite against the IR it must give and leave what is not an address alone.

use blaze_hash::{HashMap, HashSet};
use inkwell::{
    llvm_sys::{
        LLVMOpcode,
        core::{
            LLVMAddIncoming, LLVMBuildGEP2, LLVMBuildPhi, LLVMBuildPtrToInt, LLVMBuildSelect,
            LLVMConstGEP2, LLVMConstNull, LLVMCountIncoming, LLVMCreateBuilderInContext,
            LLVMDisposeBuilder, LLVMGetConstOpcode, LLVMGetFirstBasicBlock, LLVMGetFirstFunction,
            LLVMGetFirstInstruction, LLVMGetIncomingBlock, LLVMGetIncomingValue,
            LLVMGetInstructionOpcode, LLVMGetInstructionParent, LLVMGetIsDisjoint,
            LLVMGetModuleContext, LLVMGetNextBasicBlock, LLVMGetNextFunction,
            LLVMGetNextInstruction, LLVMGetOperand, LLVMInstructionEraseFromParent,
            LLVMInt8TypeInContext, LLVMIsAConstantExpr, LLVMIsAConstantInt, LLVMIsAInstruction,
            LLVMIsNull, LLVMPositionBuilderBefore, LLVMReplaceAllUsesWith, LLVMTypeOf,
        },
        prelude::{LLVMBasicBlockRef, LLVMBuilderRef, LLVMTypeRef, LLVMValueRef},
    },
    module::Module,
};
use smallvec::SmallVec;

/// The most integers one address is made of that are rewritten as pointers.
const LARGEST: usize = 64;

/// Rewrites each `inttoptr` in `module` of an integer made of `ptrtoint`s, null and what is
/// added to them, through `phi`s and `select`s, as pointers. Says whether it rewrote any.
pub(super) fn rebase(module: &Module<'_>) -> bool {
    let mut rewrote = false;
    // SAFETY: `module` is a live module, which outlives this rewrite; this only reads it.
    let context = unsafe { LLVMGetModuleContext(module.as_mut_ptr()) };
    // SAFETY: `context` is `module`'s, live for the call; the builder is owned here and
    // disposed of at the end of this function.
    let builder = unsafe { LLVMCreateBuilderInContext(context) };
    // SAFETY: `context` is `module`'s, live for the call; the type is owned by it.
    let byte = unsafe { LLVMInt8TypeInContext(context) };
    // SAFETY: `module` is a live module, which outlives this rewrite; this only reads it.
    let mut function = unsafe { LLVMGetFirstFunction(module.as_mut_ptr()) };
    while !function.is_null() {
        // SAFETY: `function` is a live function of `module` (not null, checked above); this
        // only reads it.
        let mut block = unsafe { LLVMGetFirstBasicBlock(function) };
        while !block.is_null() {
            // SAFETY: `block` is a live block of `module` (not null, checked above); this only
            // reads it.
            let mut inst = unsafe { LLVMGetFirstInstruction(block) };
            while !inst.is_null() {
                // SAFETY: `inst` is a live instruction of `module` (not null, checked above);
                // this only reads it, before it may be erased.
                let next = unsafe { LLVMGetNextInstruction(inst) };
                // SAFETY: `inst` is a live instruction of `module`; this only reads it.
                if unsafe { LLVMGetInstructionOpcode(inst) } == LLVMOpcode::LLVMIntToPtr
                    // SAFETY: the values are live values of the module and the types of its
                    // context, as `Web::rewrite`'s `# Safety` asks: `inst` is an `inttoptr`
                    // (checked on the left of `&&`) and `next` is already taken, so erasing
                    // `inst` does not break the walk.
                    && unsafe { Web::rewrite(builder, byte, inst) }
                {
                    rewrote = true;
                }
                inst = next;
            }
            // SAFETY: `block` is a live block of `module`; this only reads it.
            block = unsafe { LLVMGetNextBasicBlock(block) };
        }
        // SAFETY: `function` is a live function of `module`; this only reads it.
        function = unsafe { LLVMGetNextFunction(function) };
    }
    // SAFETY: `builder` was made above, is disposed of once, and not used after.
    unsafe { LLVMDisposeBuilder(builder) };
    rewrote
}

/// What an integer of the web is as a pointer.
#[derive(Clone, Copy)]
enum Node {
    /// `ptrtoint` of this pointer.
    Address(LLVMValueRef),
    /// Zero, which is the null pointer.
    Null,
    Phi,
    /// `select` of a condition between two of the web.
    Select,
    /// The integer is the one of the web among the operands plus the other.
    Sum {
        based: LLVMValueRef,
        offset: LLVMValueRef,
    },
}

/// The integers an address is made of, each with what it is made of in turn.
struct Web {
    nodes: HashMap<LLVMValueRef, Node>,
    /// In an order that has an operand before what is made of it, `phi`s aside.
    order: SmallVec<[LLVMValueRef; 16]>,
    /// The `phi`s, in the order [`Web::add`] met them: what is built for them is built in this
    /// order, never in `nodes`' (by address, so another each run).
    phis: SmallVec<[LLVMValueRef; 4]>,
    addresses: usize,
    /// The type of every integer of the web: the one `inttoptr` takes.
    int: LLVMTypeRef,
}

impl Web {
    /// Rewrites `inttoptr` as the pointer its integer is, if every integer it is made of
    /// is one of [`Node`]'s with at least one `ptrtoint`.
    ///
    /// # Safety
    ///
    /// `inttoptr` is a live `inttoptr` instruction of a module nothing else uses while this
    /// runs, and `builder` and `byte` (`i8`) are of that module's context. `inttoptr` may be
    /// erased, so a caller walking the module has already taken the instruction after it.
    unsafe fn rewrite(builder: LLVMBuilderRef, byte: LLVMTypeRef, inttoptr: LLVMValueRef) -> bool {
        // SAFETY: `inttoptr` is a live value of the module being rewritten, which owns it and
        // outlives this rewrite; this only reads it.
        let pointer = unsafe { LLVMTypeOf(inttoptr) };
        // SAFETY: `inttoptr` is a live `inttoptr` of the module, which has operand 0; this only
        // reads it.
        let root = unsafe { LLVMGetOperand(inttoptr, 0) };
        // SAFETY: `root` is a live value of the module being rewritten; this only reads it.
        let int = unsafe { LLVMTypeOf(root) };
        let mut web = Web {
            nodes: HashMap::default(),
            order: SmallVec::new(),
            phis: SmallVec::new(),
            addresses: 0,
            int,
        };
        // SAFETY: the values are live values of the module and the types of its context, as
        // `Web::add`'s `# Safety` asks.
        if unsafe { web.add(root, pointer) }.is_none() || web.addresses == 0 {
            return false;
        }
        // A `ptrtoint` alone is LLVM's to fold.
        if matches!(web.nodes[&root], Node::Address(_))
            // SAFETY: every value of the web was just taken from the live module, as
            // `Web::ordered`'s `# Safety` asks.
            || !unsafe { web.ordered() }
        {
            return false;
        }
        // SAFETY: the web was just built from `root`, the operand of `inttoptr`, and passed
        // `Web::ordered`; the values are live and `builder`, `byte` and `pointer` of the
        // module's context, as `Web::build`'s `# Safety` asks.
        unsafe { web.build(builder, byte, pointer, root, inttoptr) };
        true
    }

    /// Takes `value` and what it is made of into the web, if each is of it. `None` if one
    /// is not, or the web would be larger than [`LARGEST`].
    ///
    /// # Safety
    ///
    /// `value` is a live value of the module the web is of, and `pointer` a type of its
    /// context.
    unsafe fn add(&mut self, value: LLVMValueRef, pointer: LLVMTypeRef) -> Option<()> {
        if self.nodes.contains_key(&value) {
            return Some(());
        }
        // SAFETY: `value` is a live value of the module being rewritten, which owns it and
        // outlives this rewrite; this only reads it.
        if self.nodes.len() >= LARGEST || unsafe { LLVMTypeOf(value) } != self.int {
            return None;
        }
        // SAFETY: the values are live values of the module and the types of its context, as
        // `Web::node`'s `# Safety` asks.
        let node = unsafe { Self::node(value, pointer) }?;
        self.nodes.insert(value, node);
        match node {
            Node::Address(_) => self.addresses += 1,
            Node::Null => {}
            Node::Phi => {
                self.phis.push(value);
                // SAFETY: `value` is a live `phi` of the module (its opcode matched); this only
                // reads it.
                for at in 0..unsafe { LLVMCountIncoming(value) } {
                    // SAFETY: `value` is a live `phi` of the module and `at` is below its count
                    // of incoming values; this only reads it.
                    let incoming = unsafe { LLVMGetIncomingValue(value, at) };
                    // SAFETY: the values are live values of the module and the types of its
                    // context, as `Web::add`'s `# Safety` asks.
                    unsafe { self.add(incoming, pointer) }?;
                }
            }
            Node::Select => {
                // SAFETY: `value` is a live `select` of the module, which has three operands;
                // this only reads it.
                let then = unsafe { LLVMGetOperand(value, 1) };
                // SAFETY: the values are live values of the module and the types of its
                // context, as `Web::add`'s `# Safety` asks.
                unsafe { self.add(then, pointer) }?;
                // SAFETY: `value` is a live `select` of the module, which has three operands;
                // this only reads it.
                let otherwise = unsafe { LLVMGetOperand(value, 2) };
                // SAFETY: the values are live values of the module and the types of its
                // context, as `Web::add`'s `# Safety` asks.
                unsafe { self.add(otherwise, pointer) }?;
            }
            // SAFETY: `based` is an operand of the live sum `value`, and `pointer` a type of
            // the module's context, as `Web::add`'s `# Safety` asks.
            Node::Sum { based, .. } => unsafe { self.add(based, pointer) }?,
        }
        if !matches!(node, Node::Phi) {
            self.order.push(value);
        }
        Some(())
    }

    /// Whether each integer of [`Web::order`] comes after what it is made of. Not where an
    /// instruction is made of itself without a `phi`, which only unreachable code may be.
    ///
    /// # Safety
    ///
    /// Every value of the web is still a live value of its module.
    unsafe fn ordered(&self) -> bool {
        let mut built: HashSet<LLVMValueRef> =
            HashSet::with_capacity_and_hasher(self.nodes.len(), Default::default());
        built.extend(self.phis.iter().copied());
        for &value in &self.order {
            let ready = match self.nodes[&value] {
                Node::Address(_) | Node::Null | Node::Phi => true,
                Node::Select => {
                    // SAFETY: `value` is a live `select` of the module, which has three
                    // operands; this only reads it.
                    let then = unsafe { LLVMGetOperand(value, 1) };
                    // SAFETY: `value` is a live `select` of the module, which has three
                    // operands; this only reads it.
                    let otherwise = unsafe { LLVMGetOperand(value, 2) };
                    [then, otherwise].iter().all(|it| built.contains(it))
                }
                Node::Sum { based, .. } => built.contains(&based),
            };
            if !ready {
                return false;
            }
            built.insert(value);
        }
        true
    }

    /// What `value` is in the web, if it may be of it.
    ///
    /// # Safety
    ///
    /// `value` is a live value of a module, and `pointer` a type of its context.
    unsafe fn node(value: LLVMValueRef, pointer: LLVMTypeRef) -> Option<Node> {
        // SAFETY: `value` is a live value of the module being rewritten, which owns it and
        // outlives this rewrite; this only reads it.
        if !unsafe { LLVMIsAConstantInt(value) }.is_null() {
            // SAFETY: `value` is a live constant of the module; this only reads it.
            return (unsafe { LLVMIsNull(value) } != 0).then_some(Node::Null);
        }
        // SAFETY: `value` is a live value of the module being rewritten; this only reads it.
        let opcode = if !unsafe { LLVMIsAInstruction(value) }.is_null() {
            // SAFETY: `value` is a live instruction of the module (checked just above); this
            // only reads it.
            unsafe { LLVMGetInstructionOpcode(value) }
        } else {
            // SAFETY: `value` is a live value of the module being rewritten; this only reads
            // it.
            if unsafe { LLVMIsAConstantExpr(value) }.is_null() {
                return None;
            }
            // SAFETY: `value` is a live constant expression of the module (checked just
            // above); this only reads it.
            unsafe { LLVMGetConstOpcode(value) }
        };
        Some(match opcode {
            LLVMOpcode::LLVMPtrToInt => {
                // SAFETY: `value` is a live `ptrtoint` of the module, which has operand 0; this
                // only reads it.
                let address = unsafe { LLVMGetOperand(value, 0) };
                // SAFETY: `address` is a live value of the module; this only reads it.
                if unsafe { LLVMTypeOf(address) } != pointer {
                    return None;
                }
                Node::Address(address)
            }
            LLVMOpcode::LLVMPHI => Node::Phi,
            LLVMOpcode::LLVMSelect => {
                // SAFETY: `value` is a live value of the module being rewritten; this only
                // reads it.
                if unsafe { LLVMIsAInstruction(value) }.is_null() {
                    return None;
                }
                Node::Select
            }
            // SAFETY: `value` is a live `add` (opcode matched), as `Web::sum`'s `# Safety`
            // asks.
            LLVMOpcode::LLVMAdd => unsafe { Self::sum(value) }?,
            LLVMOpcode::LLVMOr => {
                // SAFETY: `value` is a live value of the module being rewritten; this only
                // reads it.
                if unsafe { LLVMIsAInstruction(value) }.is_null() {
                    return None;
                }
                // SAFETY: `value` is a live `or` instruction (opcode matched and checked just
                // above), which has the disjoint flag; this only reads it.
                if unsafe { LLVMGetIsDisjoint(value) } == 0 {
                    return None;
                }
                // SAFETY: `value` is a live disjoint `or` (opcode matched), as `Web::sum`'s
                // `# Safety` asks.
                unsafe { Self::sum(value) }?
            }
            _ => return None,
        })
    }

    /// Which operand of the sum `value` is the address and which is added to it: the one
    /// that may be an address, if the other may not.
    ///
    /// # Safety
    ///
    /// `value` is a live `add` or `or` (an instruction or a constant expression), which has
    /// two operands.
    unsafe fn sum(value: LLVMValueRef) -> Option<Node> {
        // SAFETY: `value` is a live `add` or `or` of the module, which has two operands; this
        // only reads it.
        let a = unsafe { LLVMGetOperand(value, 0) };
        // SAFETY: `value` is a live `add` or `or` of the module, which has two operands; this
        // only reads it.
        let b = unsafe { LLVMGetOperand(value, 1) };
        // SAFETY: `a` and `b` are live values of the module, as `Web::may_be_address`'s
        // `# Safety` asks.
        match (unsafe { Self::may_be_address(a) }, unsafe { Self::may_be_address(b) }) {
            (true, false) => Some(Node::Sum { based: a, offset: b }),
            (false, true) => Some(Node::Sum { based: b, offset: a }),
            _ => None,
        }
    }

    /// Whether `value` is of a kind an address of the web is: a `ptrtoint`, a `phi` or a
    /// `select`, or a sum with one of those in it.
    ///
    /// # Safety
    ///
    /// `value` is a live value of a module.
    unsafe fn may_be_address(value: LLVMValueRef) -> bool {
        // SAFETY: `value` is a live value of the module being rewritten, which owns it and
        // outlives this rewrite; this only reads it.
        let opcode = if !unsafe { LLVMIsAInstruction(value) }.is_null() {
            // SAFETY: `value` is a live instruction of the module (checked just above); this
            // only reads it.
            unsafe { LLVMGetInstructionOpcode(value) }
        } else {
            // SAFETY: `value` is a live value of the module being rewritten; this only reads
            // it.
            if unsafe { LLVMIsAConstantExpr(value) }.is_null() {
                return false;
            }
            // SAFETY: `value` is a live constant expression of the module (checked just
            // above); this only reads it.
            unsafe { LLVMGetConstOpcode(value) }
        };
        match opcode {
            LLVMOpcode::LLVMPtrToInt | LLVMOpcode::LLVMPHI | LLVMOpcode::LLVMSelect => true,
            LLVMOpcode::LLVMAdd | LLVMOpcode::LLVMOr => {
                // SAFETY: `value` is a live `add` or `or` of the module, which has two
                // operands; this only reads it.
                let a = unsafe { LLVMGetOperand(value, 0) };
                // SAFETY: `value` is a live `add` or `or` of the module, which has two
                // operands; this only reads it.
                let b = unsafe { LLVMGetOperand(value, 1) };
                [a, b].iter().any(|&it| {
                    // SAFETY: `it` is a live operand of the module's `value`; this only reads
                    // it.
                    let inner = if !unsafe { LLVMIsAInstruction(it) }.is_null() {
                        // SAFETY: `it` is a live instruction of the module (checked just
                        // above); this only reads it.
                        unsafe { LLVMGetInstructionOpcode(it) }
                    } else {
                        // SAFETY: `it` is a live operand of the module's `value`; this only
                        // reads it.
                        if unsafe { LLVMIsAConstantExpr(it) }.is_null() {
                            return false;
                        }
                        // SAFETY: `it` is a live constant expression of the module (checked
                        // just above); this only reads it.
                        unsafe { LLVMGetConstOpcode(it) }
                    };
                    matches!(
                        inner,
                        LLVMOpcode::LLVMPtrToInt | LLVMOpcode::LLVMPHI | LLVMOpcode::LLVMSelect
                    )
                })
            }
            _ => false,
        }
    }

    /// Builds the pointer of each integer of the web where the integer is, has every other
    /// use of an integer take `ptrtoint` of its pointer, and `inttoptr` the pointer of `root`.
    ///
    /// # Safety
    ///
    /// The web was just built by [`Web::add`] from `root`, the operand of `inttoptr`, and
    /// passed [`Web::ordered`]; every value of it and `inttoptr` are live; `builder`, `byte`
    /// and `pointer` are of the module's context. `inttoptr` is erased here, so a caller
    /// walking the module has already taken the instruction after it.
    unsafe fn build(
        &self,
        builder: LLVMBuilderRef,
        byte: LLVMTypeRef,
        pointer: LLVMTypeRef,
        root: LLVMValueRef,
        inttoptr: LLVMValueRef,
    ) {
        let mut pointers: HashMap<LLVMValueRef, LLVMValueRef> = HashMap::default();
        let phis = &self.phis;
        for &phi in phis {
            // SAFETY: `builder` was made in the module's context and is disposed of only at
            // the end of `rebase`; the position `phi` is a live instruction of the module.
            unsafe { LLVMPositionBuilderBefore(builder, phi) };
            // SAFETY: `builder` was made in the module's context and is disposed of only at
            // the end of `rebase`, positioned among the `phi`s; `pointer` is a type of the
            // context.
            pointers.insert(phi, unsafe { LLVMBuildPhi(builder, pointer, c"".as_ptr()) });
        }
        for &value in &self.order {
            let built = match self.nodes[&value] {
                Node::Address(address) => address,
                // SAFETY: `pointer` is a type of the module's context; the constant is owned
                // by the context.
                Node::Null => unsafe { LLVMConstNull(pointer) },
                Node::Phi => unreachable!("phis are built first"),
                Node::Select => {
                    // SAFETY: `builder` was made in the module's context and is disposed of
                    // only at the end of `rebase`; the position `value` is a live instruction
                    // of the module.
                    unsafe { LLVMPositionBuilderBefore(builder, value) };
                    // SAFETY: `value` is a live `select` of the module, which has three
                    // operands; this only reads it.
                    let then = unsafe { LLVMGetOperand(value, 1) };
                    // SAFETY: `value` is a live `select` of the module, which has three
                    // operands; this only reads it.
                    let otherwise = unsafe { LLVMGetOperand(value, 2) };
                    // SAFETY: `value` is a live `select` of the module, which has three
                    // operands; this only reads it.
                    let condition = unsafe { LLVMGetOperand(value, 0) };
                    // SAFETY: `builder` was made in the module's context and is disposed of
                    // only at the end of `rebase`; the condition is a live `i1` of the module
                    // and both arms are pointers built for the web, of type `pointer`.
                    unsafe {
                        LLVMBuildSelect(
                            builder,
                            condition,
                            pointers[&then],
                            pointers[&otherwise],
                            c"".as_ptr(),
                        )
                    }
                }
                Node::Sum { based, mut offset } => {
                    // SAFETY: `value` is a live value of the module being rewritten; this only
                    // reads it.
                    if unsafe { LLVMIsAInstruction(value) }.is_null() {
                        // A constant's sum is of constants, and is in no block to build before.
                        // SAFETY: `byte` is a type of the module's context and
                        // `pointers[&based]` a constant pointer of it, `offset` one constant
                        // index; the constant is owned by the context.
                        unsafe { LLVMConstGEP2(byte, pointers[&based], &mut offset, 1) }
                    } else {
                        // SAFETY: `builder` was made in the module's context and is disposed
                        // of only at the end of `rebase`; the position `value` is a live
                        // instruction of the module.
                        unsafe { LLVMPositionBuilderBefore(builder, value) };
                        // SAFETY: `builder` was made in the module's context and is disposed
                        // of only at the end of `rebase`; `byte` is of the context,
                        // `pointers[&based]` a pointer built for the web and `offset` one live
                        // integer, passed as an array of length 1.
                        unsafe {
                            LLVMBuildGEP2(
                                builder,
                                byte,
                                pointers[&based],
                                &mut offset,
                                1,
                                c"".as_ptr(),
                            )
                        }
                    }
                }
            };
            pointers.insert(value, built);
        }
        for &phi in phis {
            // SAFETY: `phi` is a live `phi` of the module; this only reads it.
            for at in 0..unsafe { LLVMCountIncoming(phi) } {
                // SAFETY: `phi` is a live `phi` of the module and `at` is below its count of
                // incoming values; this only reads it.
                let value = unsafe { LLVMGetIncomingValue(phi, at) };
                let mut incoming = pointers[&value];
                // SAFETY: `phi` is a live `phi` of the module and `at` is below its count of
                // incoming values; this only reads it.
                let mut from: LLVMBasicBlockRef = unsafe { LLVMGetIncomingBlock(phi, at) };
                // SAFETY: `pointers[&phi]` is the pointer phi built above and `incoming`/`from`
                // one live value and block, passed as arrays of length 1.
                unsafe { LLVMAddIncoming(pointers[&phi], &mut incoming, &mut from, 1) };
            }
        }
        // SAFETY: both are live values of the module of one type (`inttoptr` and the pointer
        // built for its operand).
        unsafe { LLVMReplaceAllUsesWith(inttoptr, pointers[&root]) };
        // SAFETY: `inttoptr` has no use left (replaced just above), and the walk in `rebase`
        // already took the instruction after it, so nothing reads it again.
        unsafe { LLVMInstructionEraseFromParent(inttoptr) };
        // What still takes an integer of the web takes the pointer's: the integers then
        // have no use left but each other, and go. In a fixed order: two `phi`s of one block
        // both put their `ptrtoint` before its first other instruction.
        for &value in phis.iter().chain(&self.order) {
            let node = &self.nodes[&value];
            // SAFETY: `value` is a live value of the module being rewritten; this only reads
            // it.
            if unsafe { LLVMIsAInstruction(value) }.is_null() || matches!(node, Node::Address(_)) {
                continue;
            }
            let after = if matches!(node, Node::Phi) {
                // SAFETY: `value` is a live `phi` instruction of the module; this only reads
                // it.
                let block = unsafe { LLVMGetInstructionParent(value) };
                // SAFETY: `block` is a live block of a well-formed function of the module, as
                // `first_after_phis`'s `# Safety` asks.
                unsafe { first_after_phis(block) }
            } else {
                // SAFETY: `value` is a live integer instruction of the module, not a
                // terminator, so one follows it; this only reads it.
                unsafe { LLVMGetNextInstruction(value) }
            };
            // SAFETY: `builder` was made in the module's context and is disposed of only at
            // the end of `rebase`; the position `after` is a live instruction of the module.
            unsafe { LLVMPositionBuilderBefore(builder, after) };
            // SAFETY: `value` is a live value of the module being rewritten; this only reads
            // it.
            let integer = unsafe { LLVMTypeOf(value) };
            // SAFETY: `builder` was made in the module's context and is disposed of only at
            // the end of `rebase`; `pointers[&value]` is the pointer built for `value` and
            // `integer` an integer type of the context.
            let int =
                unsafe { LLVMBuildPtrToInt(builder, pointers[&value], integer, c"".as_ptr()) };
            // SAFETY: both are live values of the module of one type (`value` and its
            // `ptrtoint`).
            unsafe { LLVMReplaceAllUsesWith(value, int) };
        }
    }
}

/// The first instruction of `block` that is not a `phi` nor the landing pad.
///
/// # Safety
///
/// `block` is a live block of a well-formed function, so it ends in a terminator and the
/// walk stops before running off its end.
unsafe fn first_after_phis(block: LLVMBasicBlockRef) -> LLVMValueRef {
    // SAFETY: `block` is live and ends in a terminator, as the `# Safety` asks, so `inst` is
    // never null here.
    let mut inst = unsafe { LLVMGetFirstInstruction(block) };
    while matches!(
        // SAFETY: `block` (and so `inst`) is live and ends in a terminator, as the `# Safety`
        // asks, so `inst` is never null here.
        unsafe { LLVMGetInstructionOpcode(inst) },
        LLVMOpcode::LLVMPHI | LLVMOpcode::LLVMLandingPad
    ) {
        // SAFETY: `block` (and so `inst`) is live and ends in a terminator, as the `# Safety`
        // asks, so `inst` is never null here.
        inst = unsafe { LLVMGetNextInstruction(inst) };
    }
    inst
}

#[cfg(test)]
mod tests {
    use inkwell::{context::Context, memory_buffer::MemoryBuffer};

    use super::*;

    fn parsed<'ctx>(context: &'ctx Context, text: &str) -> Module<'ctx> {
        let text = format!("{text}\0");
        let source = MemoryBuffer::create_from_memory_range_copy(text.as_bytes(), "test");
        context.create_module_from_ir(source).unwrap()
    }

    fn printed(module: &Module<'_>) -> String {
        module.print_to_string().to_string_lossy().into_owned()
    }

    /// The name of the value a line of `text` that contains `what` defines.
    fn defined<'a>(text: &'a str, what: &str) -> &'a str {
        let line =
            text.lines().find(|line| line.contains(what)).unwrap_or_else(|| panic!("{text}"));
        line.trim().split(" = ").next().unwrap()
    }

    #[test]
    fn an_offset_address_becomes_a_gep_of_its_pointer() {
        let context = Context::create();
        let module = parsed(
            &context,
            r#"
define ptr @offset(ptr %p, i64 %n) {
  %a = ptrtoint ptr %p to i64
  %s = add i64 %a, %n
  %q = inttoptr i64 %s to ptr
  ret ptr %q
}
"#,
        );
        assert!(rebase(&module));
        module.verify().unwrap();
        let text = printed(&module);
        assert!(!text.contains("inttoptr"), "{text}");
        let gep = defined(&text, "getelementptr i8, ptr %p, i64 %n");
        assert!(text.contains(&format!("ret ptr {gep}")), "{text}");
    }

    #[test]
    fn a_loops_phi_of_addresses_becomes_a_phi_of_pointers() {
        let context = Context::create();
        let module = parsed(
            &context,
            r#"
define ptr @walk(ptr %p, i64 %n, i64 %step) {
entry:
  %a = ptrtoint ptr %p to i64
  br label %loop

loop:
  %i = phi i64 [ %a, %entry ], [ %next, %loop ]
  %k = phi i64 [ 0, %entry ], [ %k1, %loop ]
  %next = add i64 %i, %step
  %k1 = add i64 %k, 1
  %done = icmp eq i64 %k1, %n
  br i1 %done, label %exit, label %loop

exit:
  %q = inttoptr i64 %next to ptr
  ret ptr %q
}
"#,
        );
        assert!(rebase(&module));
        module.verify().unwrap();
        let text = printed(&module);
        assert!(!text.contains("inttoptr"), "{text}");
        let phi = defined(&text, "phi ptr");
        assert!(text.contains("[ %p, %entry ]"), "{text}");
        let gep = defined(&text, &format!("getelementptr i8, ptr {phi}, i64 %step"));
        assert!(text.contains(&format!("ret ptr {gep}")), "{text}");
    }

    /// Two `phi`s of one block, both still read as integers, each get their `ptrtoint` at the
    /// block's first other instruction: in the order the web met them, the same every run
    /// whatever addresses LLVM gave the values (each run in a fresh context, others alive).
    #[test]
    fn two_phis_of_one_block_rebase_to_the_same_ir_every_time() {
        const IR: &str = r#"
define ptr @two(ptr %p, ptr %r, i1 %c, i64 %n, i64 %step, ptr %out) {
entry:
  %a = ptrtoint ptr %p to i64
  %b = ptrtoint ptr %r to i64
  br label %loop

loop:
  %i = phi i64 [ %a, %entry ], [ %i1, %loop ]
  %j = phi i64 [ %b, %entry ], [ %j1, %loop ]
  %k = phi i64 [ 0, %entry ], [ %k1, %loop ]
  %x = xor i64 %i, %j
  store i64 %x, ptr %out
  %i1 = add i64 %i, %step
  %j1 = add i64 %j, %step
  %k1 = add i64 %k, 1
  %done = icmp eq i64 %k1, %n
  br i1 %done, label %exit, label %loop

exit:
  %d = select i1 %c, i64 %i1, i64 %j1
  %q = inttoptr i64 %d to ptr
  ret ptr %q
}
"#;
        let mut kept = Vec::new();
        let mut first: Option<String> = None;
        for _ in 0..8 {
            let context = Context::create();
            let module = parsed(&context, IR);
            assert!(rebase(&module));
            module.verify().unwrap();
            let text = printed(&module);
            assert!(!text.contains("inttoptr"), "{text}");
            match &first {
                Some(first) => assert_eq!(*first, text),
                None => first = Some(text),
            }
            kept.push(Context::create());
        }
    }

    #[test]
    fn a_select_between_offset_addresses_becomes_a_select_of_pointers() {
        let context = Context::create();
        let module = parsed(
            &context,
            r#"
define ptr @pick(ptr %p, ptr %r, i1 %c, i64 %n) {
  %a = ptrtoint ptr %p to i64
  %b = ptrtoint ptr %r to i64
  %s = select i1 %c, i64 %a, i64 %b
  %t = add i64 %s, %n
  %q = inttoptr i64 %t to ptr
  ret ptr %q
}
"#,
        );
        assert!(rebase(&module));
        module.verify().unwrap();
        let text = printed(&module);
        assert!(!text.contains("inttoptr"), "{text}");
        let select = defined(&text, "select i1 %c, ptr %p, ptr %r");
        let gep = defined(&text, &format!("getelementptr i8, ptr {select}, i64 %n"));
        assert!(text.contains(&format!("ret ptr {gep}")), "{text}");
    }

    #[test]
    fn what_is_not_made_of_an_address_is_left_alone() {
        let context = Context::create();
        let source = r#"
define ptr @plain(i64 %x) {
  %q = inttoptr i64 %x to ptr
  ret ptr %q
}

define ptr @alone(ptr %p) {
  %a = ptrtoint ptr %p to i64
  %q = inttoptr i64 %a to ptr
  ret ptr %q
}

define ptr @two(ptr %p, ptr %r) {
  %a = ptrtoint ptr %p to i64
  %b = ptrtoint ptr %r to i64
  %s = add i64 %a, %b
  %q = inttoptr i64 %s to ptr
  ret ptr %q
}
"#;
        let module = parsed(&context, source);
        let before = printed(&module);
        assert!(!rebase(&module));
        assert_eq!(printed(&module), before);
    }
}
