---
status: plan
summary: Code generation on the item layer (D97, amended by D98 and D100, built straight at the end state by D99; the fast tier as the real lowest tier by D195) and the one lowering (D119). One `Tier` seam with three tiers tested equal; the fast tier as a template composer with registers; the optimizing tier as LLVM over an offer set blaze chooses; heat by sampling; loops tier up above their frame; one typed body that the check, the evaluator and the builder all read. What is built, what is built but not wired, and what is not built.
date: 2026-11-15
---
# Backend: code generation and the one lowering

## Where it stands

- Built: piece 0, `crates/blaze-tier` (the `Tier` seam in `seam.rs`, the template table's form
  in `table.rs`, register descriptions in `regs.rs`, the code record, the keys, the sample and
  profile records, `route.rs`'s tier choice).
- Built: the Reference tier, `crates/blaze-emit` (every value in a stack cell), which is what the
  commands emit today (`blaze-ladder` code_records.rs `EMITTING`, `blaze-cli` run_parent.rs).
- Built, not wired: the Fast tier's composer and the table generators
  (`crates/blaze-emit/src/compose/`, `crates/blaze-emit/src/table_gen/`). No command emits at
  `TierName::Fast`, so D195's switch to it as the lowest tier used for real work is not done.
- Built: tier equality on every ladder rung (`crates/blaze-rung-tiers`), and the LIR evaluator
  that is its reference (`crates/blaze-lir/src/eval.rs`, `eval/`).
- Built: the offer set (`crates/blaze-run/src/offer.rs`), the sampler
  (`crates/blaze-launch-rt/src/sample.rs`, macOS and Linux halves), the loop-entry form
  (`crates/blaze-lir/src/loop_entry.rs`).
- Not built: the LLVM optimizing tier's helper (LIR to LLVM IR, the named pipeline, the object
  read back into a record). An experimental non-LLVM optimizing tier, `TierName::Sota`, lives in
  `crates/blaze-emit/src/sota/` ([sota-a64-architecture.md](sota-a64-architecture.md)).
- Not built: compact unwind encodings (`crates/blaze-code/src/unwind.rs` writes DWARF from
  platform-neutral rows; a compact writer would make its word from the same rows).
- The one lowering (D119): pieces P1 to P6 are built as crates (`blaze-body`, `blaze-body-lower`,
  `blaze-body-flow`, `blaze-body-drops`, `blaze-body-eval`, `blaze-body-build`). P7, the switch,
  is not done: blaze-flow-lower, blaze-build's walk and blaze-consteval's compiler still live.

## End state: code generation on the item layer (D97, D98, D99, D100)

These decisions sit inside bottom.md section 11 (code is records
behind slots; compile on first call; a background tier that inlines across functions; the
optimized native binary whole) and lir.md (LIR is the one input of every backend; LLVM is fed
from LIR). There is no daemon: a command answers and exits. An optional helper may live between
commands, and nothing depends on it for correctness.

1. ONE SEAM, THREE TIERS. `Tier` is a type parameter at one seam (rule 78): one LIR function and its context in,
   one code record out. `Reference`: the emitter in crates/blaze-emit, kept as the simplest machine-code oracle,
   never the default once `Fast` passes. `Fast`: point 2. `Optimized`: point 4. Every tier of one LIR function
   gives the same observable result: generated LIR programs (blaze-sim) and every ladder rung run at every tier
   and are compared with LIR's evaluator; a planted wrong template must be caught.

2. THE FAST TIER IS ONE ANALYSIS PASS AND ONE EMISSION PASS, WITH REGISTERS, AND ITS ENCODERS ARE DATA. This is
   TPDE's shape (Schwarz, Kamm, Engelke, CGO 2026), with copy-and-patch (Xu and Kjolstad 2021; Deegen 2024) as
   the other reference.
   - Analysis, one pass over the function: block order, loop depth, each value's last use. A value live across
     a block boundary gets its HOME here (a register for its whole interval, extended to a loop's end when live
     into its header, or its cell), which gives the fixed place at each loop header.
   - Emission, one pass: instruction selection, encoding and allocation together. A value defined and last used
     inside one block takes any free register and may be evicted to its cell there.
   - The encoders are a TABLE: for each LIR operation, type and operand shape (register, constant with its
     encodability class, memory) a machine-code template with marked fields (registers, immediate, site). A
     template may cover a small tree when the inner value has one use (compare and branch, an address folded
     into a load or store, a shifted operand). A constant outside a template's class is put in a register by
     the composer. On x86-64 a template uses the canonical long form, so its register fields are at fixed bits.
   - The fast tier is written once, as the COMPOSER over the table, reading templates only through the table's
     trait. The table has two generators: blaze-emit's hand-written encoders (enc.rs), each run once with
     placeholder registers at blaze's build time, and the LIR-to-LLVM lowering the optimizing tier uses. Where
     both give an entry, the two are tested equal against LIR's evaluator. The table is checked in as data with
     its digest; building blaze does not need LLVM for it.
   - A template is admitted by evaluator equality over the edge tables; bit-vector proofs are added where they
     come for free and do not gate it.
   - The fast tier's input is LIR after the LINEAR SIMPLIFY STAGE, blaze's own LIR-to-LIR passes, each one walk:
     a trivial inliner (bodies under a small bound with no loop, and `#[inline(always)]`); scalar replacement of
     slots whose address does not escape; copy and constant propagation with folding; dead code and trivial
     branch removal. Each is validated like a tier. The fast record's read list holds the inlined bodies' code
     identities. The bounds are set by measurement, and the ratio of fast to optimized code is reported from
     that run; no number is claimed before it.

3. HEAT COMES FROM SAMPLING, NOT FROM A COUNTER IN EVERY PROLOGUE. A sample is the program counter, the frame
   pointer and the link register. The caller is the return address at [x29+8], except in the prologue's first
   words and at a `ret`, where it is the link register. The reader checks the frame pointer against the
   thread's stack bounds before it reads. On macOS the runtime's control thread samples with `thread_suspend`
   and `thread_get_state`, and holds no lock and allocates nothing while a thread is stopped; no signal reaches
   the program. On Linux the sampler uses `timer_create(CLOCK_THREAD_CPUTIME_ID)` with `SIGEV_THREAD_ID`. Past
   a bound a function is queued once per run for the optimizing tier, compiled by the run's parent on the pool
   after the answer, and published by writing one slot word: calls made after the write run the new code. A
   profile record per program, keyed by names that survive edits (instance symbols), keeps the hot set and the
   hot edges. A startup record keeps the order of first calls, so the next run asks for them in one batch.

4. A RUNNING LOOP TIERS UP ON TOP OF ITS FRAME, NEVER BY MOVING IT. The fast tier keeps every value live across
   a loop header in a fixed place there and writes the LOOP MAP into the code record. Each back edge carries one
   POLL, a load site in the form blaze-code has (`adrp x16, poll@PAGE; ldr xzr, [x16, poll@PAGEOFF]`), so no
   register is reserved. When the samples show a thread staying inside one loop, the parent makes the instance's
   LOOP-ENTRY variant and protects the poll page. The variant runs in a new frame above the fast frame: slots
   whose address is taken stay where they are and reach the variant as pointers; only values in registers or
   private cells are passed as parameters. It runs the rest of the function, drops what the fast frame held, and
   returns through a stub that pops both. The poll stays unless the ladder shows it costs more than 1%.

5. THE OPTIMIZING TIER IS LLVM'S INLINER OVER AN OFFER SET BLAZE CHOOSES. The module handed to LLVM holds the
   one instance and, as `available_externally` bodies, the callees blaze offers: transitively, under a size
   bound, hot edges from the profile first, never a cycle. Callees are offered already optimized, from their
   own optimized bitcode records (ThinLTO's import of simplified code); the parent makes hot callees before
   their callers.
   - The pipeline is a NAMED CUSTOM one: LLVM's inliner in its bottom-up order with the -O2 function
     simplification, and no pass that infers a fact about a callee from an offered body. What such passes would
     infer is SUPPLIED from blaze's own fact records (cannot unwind, pure, reads only, no-alias, ranges), each a
     read by the fact's content.
   - LLVM is told what Rust knows, carried on LIR from the front end (rule 31): no-alias and read-only
     parameters, non-null, dereferenceable sizes, alignments, value ranges of loads.
   - The read list is the bodies inlined, transitively, read from LLVM's remark stream (an unreadable stream is
     an error), plus the fact records of callees still called. An optimized caller whose non-inlined callee
     changed stays valid and reaches the new callee through its slot.
   - Single-threaded, with the target CPU named in the key and given to LLVM explicitly, never `native`. The
     helper compiles many instances in one process, a fresh module each; "alone and batched give equal bytes"
     is a test.

6. THE OPTIMIZED NATIVE BINARY IS THE SAME UNITS, ALL OF THEM (`blaze build --release`). Every reached instance
   at the optimized tier, the inline plan from the profile record when there is one and from static sizes when
   not, direct calls in place of slots, the layout from the profile. Whole-program passes
   (devirt.md, icf.md) run here, since this is not incremental.

7. KEYS. A fast record: the instance's symbol, its body's digest, the facts of types it was built with, the
   target and features, the tier, the producer of that tier (D87), the table's digest. An optimized record: the
   same, plus the pipeline's name, LLVM's version, the digest of the offer set in a total order, and its read
   list. The pointer from (instance, tier) to the newest record is local and never shared; a record made from an
   observed profile stays in the private layer. Shared instances (polymorph.md) are found in the shared cache at
   the optimized tier and never pass through Fast. The profile and startup records hold no address and no time.

8. EVERY TIER'S RECORD CARRIES UNWINDING, POSITIONS AND SITES, IN ONE FORMAT. A code record carries its unwind
   rows and landing pads, a position map (item-relative, rule 71) with the inline frames the simplifier and LLVM
   made, the loop map and the poll sites. A site is one relocation: an offset, a kind (page, page offset, branch,
   GOT page, GOT page offset) and a target. Placed code is registered with the unwinder through
   `__unw_add_find_dynamic_unwind_sections` on macOS and its like on Linux; a debugger sees it through the JIT
   interface.

9. EXPLICIT TAIL CALLS ARE KEPT OR REFUSED, NEVER APPROXIMATED. `become f(args)` (RFC 3407) promises
   that the callee replaces the caller's frame: a chain of any length runs in the stack of one, and
   the caller's locals are dropped before the callee runs. Rule 4 makes it required and rule 3 makes
   the promise binding: a tier that cannot keep it for a call (a by-value stack argument, a vtable
   callee whose parameter list differs, a coroutine) refuses that call with an error naming it, never a
   call and a return in its place. The callee's convention and return are the caller's; an argument
   passed by reference is copied into a temporary, then into the caller's own incoming memory for that
   parameter, so `become f(b, a)` can swap two of them; nothing else of the caller's frame is passed,
   and no landing pad of the caller outlives the call. Lowered by blaze-body-lower (`become_`) and
   blaze-build (walk/tail_call.rs) to LIR's `Op::TailCall` / `Op::TailCallIndirect`; walk/tail_call.rs
   still falls back to a call and a return for an ineligible call: to change.
10. Admission. A tier becomes the default only after the equality harness passes on it and the real command has
   run a real project through it with the profile on (rules 68, 82).

Open, by measurement:
- A call through a slot word is a load and an indirect branch; the alternative is `bl` to a per-callee `b` that
  tier-up rewrites. Slot words stay until the ladder's call-heavy rungs show the difference. Promoted code is
  placed together in a hot region ordered by the sampled call edges.
- The cell baseline against registers (codegen.md: ratio 4.41) once Fast is the emitting tier.
- Emission time per LIR instruction at Reference and at Fast, over hello world's reached bodies.
- The simplify stage's bounds: the ladder and tempo's tests at Fast, with the inliner, with the whole stage, and
  Optimized, instructions retired.
- The poll's cost on the ladder; the number of first-call asks before `main` prints; the cost of sampling at the
  chosen rate.
- LLVM's bytes equal across two processes; `default<O2>` against `default<O3>` on the ladder and tempo, compile
  and run time, before O3 is ever named.

## One lowering (D119)

A checked body is lowered three times today, each over the same 37 expression forms: blaze-flow-lower to a
check-only skeleton, blaze-build's walk to LIR once per INSTANCE, and blaze-consteval's compiler to its own ops.
The borrow check proves a different program from the one that runs, and no proof passes between them.

UNTIL THE SWITCH, one owner of identities: the FLOW layer names every local and temporary and EMITS the table (a
binding's pattern to its local; an expression and its role (value, borrow, awaitee, pin, result) to its local).
The builder never invents a slot that lives across a suspension: at each such site it LOOKS UP the local in that
table and writes the value to that local's place in the state; a value it wants to keep across a suspension that
the table does not name is refused, naming the expression. The builder's own values (addresses, derived pointers)
are recomputed after a resume. Test, a rung each: every slot kept across a suspension names a flow local; plant:
a builder-made slot is caught.

END STATE. One lowering, to one complete, typed, generic BODY; everything else reads it:

    checked body --lower (once a body)--> Body --check: loans, moves, saved locals (proof)
                                               --elaborate drops (body to body, once a body)
                                               --evaluate (constants, at ground arguments)
                                               --build (LIR, at ground arguments: one linear pass)

The body is a RECORD (crate `blaze-body`): flat tables read in place, keyed like the check that made it,
positions item-relative (rule 71), no pointer in it. It is generic: its types are the checked record's
portable types; an instance grounds them when it builds or evaluates. What it holds, so that no reader goes
back to the expression tree:

- `locals`: type, kind (return, argument, binding, temporary), mutability, and its identity (binding's
  pattern, or expression and role). A local is one thing for every reader.
- `places`: interned (local, projections): field (index, type), deref, index (by a local), constant index
  and subslice (slice patterns), downcast (variant).
- `consts`: a literal's bits with its type; a string or byte data row; an item constant (key, arguments);
  a promoted; a fn item (key, arguments); a static's address; an anonymous constant.
- statements, fixed-width rows: `Assign(place, value)`, `StorageLive`, `StorageDead`, `Activate(loan)`,
  `FakeRead` (a guard's shallow borrow). A value is one of: use, borrow (kind, place, loan), raw address,
  binary op, checked binary op (value and overflow flag), unary op, cast (kind, operand, type), aggregate
  (tuple, array, ADT variant, closure, coroutine; operands), repeat, length, discriminant, a type's size or
  alignment, a thread-local's address.
- terminators: `Goto`; `Switch` (an operand; (value, block) cases; otherwise); `Call` (callee, arguments,
  destination, target, unwind; its lifetime part for the check); `Drop` (place, target, unwind); `Assert`
  (condition, expected, kind: overflow of an op, bounds (length, index), division or remainder by zero,
  a resumed-after-completion coroutine; target, unwind); `Suspend` (resume, cancel); `Return`;
  `Unreachable`; `Resume`; `Terminate`; `InlineAsm`. A callee is a fn item (key, arguments), a trait method
  (key, arguments: resolved to an impl or a vtable slot when the instance is ground), a value (a fn
  pointer), or an intrinsic. `unwind` is continue, a cleanup block, unreachable or terminate.
- blocks (rows, terminator, normal or cleanup), and the borrow facts (loans, origins, subsets), carried by
  the same ids.

Rules that make it better than a second MIR:

1. A `match` is lowered ONCE to tests (switches on discriminants and values, in Rust's order of arms and
   guards), and the borrow check, the evaluator and the builder all run those same tests.
2. Unwinding is in the body: every call, drop and assert that may unwind names its cleanup block, made once
   a body. `panic = "abort"` is a build fact: the builder then emits no pad; the body does not change.
3. Drops are written un-elaborated (`Drop(place)` means "if initialized") so the check sees Rust's rule;
   ELABORATION is one body-to-body pass that reads the check's move dataflow and writes drop flags as
   locals. Once a body, not once an instance.
4. The builder takes the check's PROOF (rule 74): `build(body: &Checked<Body>, ..)`. No code is built from
   a body the check has not passed.
5. Building an instance is a linear pass over the rows with a substitution: layout, ABI, intrinsics,
   vtables, format templates and checks stay in blaze-build (layout.rs, cabi.rs, intrinsics.rs, vtable.rs
   are kept); scope logic, temporaries, match lowering and drop flags leave it. FLOOR: an instance's build
   reads each row once; the first measurement to quote is rows a second against the walk on the same
   instances (the profile's build rows), and the target is within 10% of LIR emission alone.
6. Two instances whose builds READ the same facts (layouts, callee symbols) share one code record: the key
   is the body's digest and the facts read, never the type arguments as written.
7. The evaluator interprets the same rows over blaze-consteval's memory; its own compiler is deleted.
8. The Lean semantics of rule 57 is of this body; each body-to-body pass (elaboration, the coroutine
   transform when it moves here) is validated against it.

What is left is P7, the switch: front2 calls the new path, and flow-lower, the walk and the evaluator's
compiler are deleted. PROOF (rule 78: the two builders are one seam with two instantiations, tested equal):
every x0 ladder rung and the std census run through both paths and must print the same; the body's
interpreter against the built code on generated bodies; a planted bug in each pass is caught.
