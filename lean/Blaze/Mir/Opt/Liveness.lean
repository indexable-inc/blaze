import Blaze.Mir.Opt.Puts
import Blaze.Mir.Opt.Renumber

/-!
Liveness as a certificate, and the pass it licenses: global dead-store elimination (ledger
V11), piece (c).

A certificate gives, for each function and block, the locals live at each point of the
block: before each statement, and before the terminator. It is data, as the Rust pass or
anything else computes it; `checkLiveness` checks it, and the kernel evaluates the check.

What the check asks, of every local `l` the function has that is not live before a
statement: the statement stores to the whole of `l` (so what `l` held is forgotten), or does
not mention `l` and `l` is not live after it either (`deadOk`). Of a local not live before
the terminator: the terminator does not read it, and it is not live on entry to any block
the terminator goes to (a call's destination, written whole, may be). A `ret` reads local 0.

`removeDead cert borrowed prog`: a store `l = rv` to a whole local that is not live after
it, never borrowed, of an rvalue that cannot panic or move, becomes a `nop`.

Here: the definitions, the check, what a passed check says of one statement
(`stmtLive_dead`), and examples. Not here, and so no theorem about `removeDead` yet: the
terminators, calls and returns on related states (piece d) and `removeDead_refines`
(piece e); `Opt/Puts.lean` is what they rest on.
-/

namespace Blaze.Mir.Opt

open Blaze.Mir

/-- `p` holds of every number below `n`. -/
def allBelowP (p : Nat → Bool) : Nat → Bool
  | 0 => true
  | n + 1 => p n && allBelowP p n

theorem allBelowP_lt (p : Nat → Bool) (l : Nat) :
    ∀ (n : Nat), allBelowP p n = true → l < n → p l = true := by
  intro n
  induction n with
  | zero =>
    intro _ h
    exact absurd h (Nat.not_lt_zero l)
  | succ n ih =>
    intro hc hl
    by_cases e : l = n
    · rw [e]
      exact and_true_left hc
    · have hlt : l < n := Nat.lt_of_le_of_ne (Nat.le_of_lt_succ hl) e
      exact ih (and_true_right hc) hlt

/-- A local dead before the statement is stored to whole, or is not mentioned and stays
dead. -/
def deadOk (before after : List Local) (s : Statement) (l : Local) : Bool :=
  has l before || (isOverwrite l s || (stmtFine l s && !has l after))

/-- The live sets before and after the statement are consistent, for the function's `n`
locals. -/
def stmtLive (n : Nat) (before after : List Local) (s : Statement) : Bool :=
  allBelowP (deadOk before after s) n

/-- What the check says of a local dead before a statement. -/
theorem stmtLive_dead (n : Nat) (before after : List Local) (s : Statement) (l : Local)
    (h : stmtLive n before after s = true) (hl : l < n) (hd : has l before = false) :
    isOverwrite l s = true ∨ (stmtFine l s = true ∧ has l after = false) := by
  have ok : deadOk before after s l = true := allBelowP_lt (deadOk before after s) l n h hl
  have ok' : (has l before || (isOverwrite l s || (stmtFine l s && !has l after))) = true := ok
  rw [hd] at ok'
  have rest : (isOverwrite l s || (stmtFine l s && !has l after)) = true := ok'
  cases or_true_cases rest with
  | inl over => exact Or.inl over
  | inr stays =>
    exact Or.inr (And.intro (and_true_left stays) (not_true_false (and_true_right stays)))

/-- The statements against the points: one live set before each, and one before the
terminator. -/
def stmtsLive (n : Nat) : List (List Local) → List Statement → Bool
  | [_], [] => true
  | before :: after :: rest, s :: ss =>
    stmtLive n before after s && stmtsLive n (after :: rest) ss
  | [], _ => false
  | [_], _ :: _ => false
  | _ :: _ :: _, [] => false

/-- The live set before the terminator: the last point. -/
def lastPoint : List (List Local) → List Local
  | [] => []
  | [last] => last
  | _ :: rest => lastPoint rest

/-- The destination of a call is the whole local `l`. -/
def wholeDest (l : Local) (dest : Place) : Bool :=
  decide (dest.base = l) && decide (dest.proj = [])

/-- The terminator does not read the local. -/
def termFine (l : Local) : Terminator → Bool
  | Terminator.goto _ => true
  | Terminator.switchInt d _ _ => operandFine l d
  | Terminator.call _ args dest _ => operandsFine l args && (placeFine l dest || wholeDest l dest)
  | Terminator.ret => decide (¬ l = 0)
  | Terminator.assert c _ _ _ => operandFine l c
  | Terminator.drop _ _ => true
  | Terminator.unreachable => true

/-- The local is what the terminator, a call, writes whole on return. -/
def termWrites (l : Local) : Terminator → Bool
  | Terminator.call _ _ dest _ => wholeDest l dest
  | Terminator.goto _ => false
  | Terminator.switchInt _ _ _ => false
  | Terminator.ret => false
  | Terminator.assert _ _ _ _ => false
  | Terminator.drop _ _ => false
  | Terminator.unreachable => false

/-- The locals live on entry to a block: its first point. -/
def liveIn (points : List (List (List Local))) (b : BlockId) : List Local :=
  match nth? points b with
  | some (first :: _) => first
  | _ => []

/-- A local dead before the terminator is not read by it, and is dead on entry to every
block it goes to, unless the terminator is a call that writes it whole. -/
def termLive (n : Nat) (points : List (List (List Local))) (last : List Local)
    (term : Terminator) : Bool :=
  allBelowP
    (fun l =>
      has l last ||
        (termFine l term &&
          (termWrites l term || (successors term).all (fun t => !has l (liveIn points t)))))
    n

def blockLive (n : Nat) (points : List (List (List Local))) (blk : BasicBlock)
    (pts : List (List Local)) : Bool :=
  stmtsLive n pts blk.stmts && termLive n points (lastPoint pts) blk.terminator

def blocksLive (n : Nat) (points : List (List (List Local))) :
    List BasicBlock → List (List (List Local)) → Bool
  | [], [] => true
  | blk :: blks, pts :: rest => blockLive n points blk pts && blocksLive n points blks rest
  | [], _ :: _ => false
  | _ :: _, [] => false

/-- The certificate of one function is consistent. -/
def bodyLive (body : Body) (points : List (List (List Local))) : Bool :=
  blocksLive body.locals.length points body.blocks points

def fnsLive : List Body → List (List (List (List Local))) → Bool
  | [], [] => true
  | body :: bodies, pts :: rest => bodyLive body pts && fnsLive bodies rest
  | [], _ :: _ => false
  | _ :: _, [] => false

/-- The certificate is consistent: per function, per block, per point. -/
def checkLiveness (prog : Program) (cert : List (List (List (List Local)))) : Bool :=
  fnsLive prog.fns cert

/-! The pass. -/

/-- The store can go: it is to a whole local the function has, not live after it and never
borrowed, of an rvalue that cannot panic or move. -/
def deadAssign (n : Nat) (bw : Local → Bool) (after : List Local) (p : Place) (rv : Rvalue) :
    Bool :=
  decide (p.proj = []) &&
    (decide (p.base < n) && (!bw p.base && (rvalueCalm rv && !has p.base after)))

def deadStore (n : Nat) (bw : Local → Bool) (after : List Local) : Statement → Bool
  | Statement.assign p rv => deadAssign n bw after p rv
  | Statement.storageLive _ => false
  | Statement.storageDead _ => false
  | Statement.nop => false

/-- The statements with each dead store made a `nop`. -/
def killDead (n : Nat) (bw : Local → Bool) : List (List Local) → List Statement → List Statement
  | _ :: after :: rest, s :: ss =>
    (if deadStore n bw after s = true then Statement.nop else s) ::
      killDead n bw (after :: rest) ss
  | [], ss => ss
  | [_], ss => ss
  | _ :: _ :: _, [] => []

def removeDeadBlocksOf (n : Nat) (bw : Local → Bool) :
    List BasicBlock → List (List (List Local)) → List BasicBlock
  | blk :: blks, pts :: rest =>
    { stmts := killDead n bw pts blk.stmts, terminator := blk.terminator } ::
      removeDeadBlocksOf n bw blks rest
  | blks, [] => blks
  | [], _ :: _ => []

def removeDeadBody (borrowed : Body → Local → Bool) (body : Body)
    (points : List (List (List Local))) : Body :=
  { name := body.name, args := body.args, locals := body.locals,
    blocks := removeDeadBlocksOf body.locals.length (borrowed body) body.blocks points }

def removeDeadFns (borrowed : Body → Local → Bool) :
    List Body → List (List (List (List Local))) → List Body
  | body :: bodies, pts :: rest =>
    removeDeadBody borrowed body pts :: removeDeadFns borrowed bodies rest
  | bodies, [] => bodies
  | [], _ :: _ => []

/-- The pass: every store the certificate says is dead becomes a `nop`. -/
def removeDead (cert : List (List (List (List Local)))) (borrowed : Body → Local → Bool)
    (prog : Program) : Program :=
  { adts := prog.adts, fns := removeDeadFns borrowed prog.fns cert }

end Blaze.Mir.Opt
