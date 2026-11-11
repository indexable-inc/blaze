import Blaze.Mir.Opt.Reach

/-!
Two CFG passes that renumber blocks, proved through `Opt/Rename.lean` and `Opt/Reach.lean`.

`renumber orders prog`: function `fn` gets the blocks `orders fn` lists, in that order, each
with its targets renamed to their positions in the list. What the list must be is checked,
by a function the kernel evaluates, and the theorems take the check's `true`:

- `checkPermutation`: every block is listed, the entry block first. Then the renumbered
  program runs as the program does from every state (`permuteBlocks_correct`), and every call
  gives the same outcome (`permuteBlocks_call`). No invariant.
- `checkKept`: the entry block is first, and every target of a listed block is listed.
  Blocks not listed are removed. Then the runs agree from every state the reachability
  invariant `Reaches (Kept orders)` holds of (`renumber_kept_correct`), and every call gives
  the same outcome (`renumber_kept_call`), because a call starts in such a state.

`removeDeadBlocks` is the second with the order it computes itself: the blocks block 0
reaches, in their old order, as `remove_dead_blocks` of
crates/hir-ty-mir/src/mir/optimize/cfg.rs keeps them. The Rust pass also merges every empty
`unreachable` block into one; this does not, so no pair of bodies is checked against it yet.
-/

namespace Blaze.Mir.Opt

open Blaze.Mir

/-! Lists by index. -/

theorem nth?_lt {α : Type} :
    ∀ (l : List α) (k : Nat) (x : α), nth? l k = some x → k < l.length
  | [], _, _, h => none_eq_some_elim h
  | _ :: l, 0, _, _ => Nat.zero_lt_succ l.length
  | _ :: l, k + 1, x, h => Nat.succ_lt_succ (nth?_lt l k x h)

theorem nth?_of_lt {α : Type} : ∀ (l : List α) (k : Nat), k < l.length → ∃ x, nth? l k = some x
  | [], k, h => absurd h (Nat.not_lt_zero k)
  | a :: _, 0, _ => ⟨a, rfl⟩
  | _ :: l, k + 1, h => nth?_of_lt l k (Nat.lt_of_succ_lt_succ h)

theorem nth?_ge {α : Type} : ∀ (l : List α) (k : Nat), l.length ≤ k → nth? l k = none
  | [], _, _ => rfl
  | _ :: l, 0, h => absurd h (Nat.not_succ_le_zero l.length)
  | _ :: l, k + 1, h => nth?_ge l k (Nat.le_of_succ_le_succ h)

theorem length_map' {α β : Type} (g : α → β) : ∀ (l : List α), (l.map g).length = l.length
  | [] => rfl
  | _ :: l => congrTo (fun n => n + 1) (length_map' g l)

/-- A map that knows each element's index, from `i` up. -/
def mapFrom {α β : Type} (f : Nat → α → β) : Nat → List α → List β
  | _, [] => []
  | i, a :: rest => f i a :: mapFrom f (i + 1) rest

theorem nth?_mapFrom {α β : Type} (f : Nat → α → β) :
    ∀ (l : List α) (i k : Nat), nth? (mapFrom f i l) k = (nth? l k).map (f (i + k))
  | [], _, _ => rfl
  | _ :: _, _, 0 => rfl
  | _ :: l, i, k + 1 => by
    have h := nth?_mapFrom f l (i + 1) k
    have e : i + 1 + k = i + (k + 1) := by omega
    rw [e] at h
    exact h

theorem nth?_mapFrom_zero {α β : Type} (f : Nat → α → β) (l : List α) (k : Nat) :
    nth? (mapFrom f 0 l) k = (nth? l k).map (f k) := by
  have h := nth?_mapFrom f l 0 k
  rw [Nat.zero_add] at h
  exact h

/-! An order: the old numbers of the blocks kept, in their new order. -/

/-- Where `b` is in the order: its new number. The order's length when it is not there. -/
def position (b : BlockId) : List BlockId → Nat
  | [] => 0
  | a :: rest => if a = b then 0 else position b rest + 1

/-- Whether the order lists `b`. -/
def has (b : BlockId) : List BlockId → Bool
  | [] => false
  | a :: rest => if a = b then true else has b rest

theorem position_cons (b a : BlockId) (rest : List BlockId) :
    position b (a :: rest) = if a = b then 0 else position b rest + 1 := rfl

theorem has_cons (b a : BlockId) (rest : List BlockId) :
    has b (a :: rest) = if a = b then true else has b rest := rfl

/-- A listed block is found at its position. -/
theorem nth?_position {β : Type} (g : BlockId → β) (b : BlockId) :
    ∀ (order : List BlockId), has b order = true →
      nth? (order.map g) (position b order) = some (g b) := by
  intro order
  induction order with
  | nil =>
    intro h
    exact false_eq_true_elim h
  | cons a rest ih =>
    intro h
    by_cases e : a = b
    · have p : position b (a :: rest) = 0 := (position_cons b a rest).trans (if_pos e)
      exact (congrTo (nth? (List.map g (a :: rest))) p).trans
        (congrTo (fun x => some (g x)) e)
    · have p : position b (a :: rest) = position b rest + 1 :=
        (position_cons b a rest).trans (if_neg e)
      have h' : has b rest = true := ((has_cons b a rest).trans (if_neg e)).symm.trans h
      exact (congrTo (nth? (List.map g (a :: rest))) p).trans (ih h')

/-- A block not listed has the order's length as its position. -/
theorem position_not_has (b : BlockId) :
    ∀ (order : List BlockId), has b order = false → position b order = order.length := by
  intro order
  induction order with
  | nil =>
    intro _
    rfl
  | cons a rest ih =>
    intro h
    by_cases e : a = b
    · have t : has b (a :: rest) = true := (has_cons b a rest).trans (if_pos e)
      exact true_eq_false_elim (t.symm.trans h)
    · have p : position b (a :: rest) = position b rest + 1 :=
        (position_cons b a rest).trans (if_neg e)
      have h' : has b rest = false := ((has_cons b a rest).trans (if_neg e)).symm.trans h
      exact p.trans (congrTo (fun n => n + 1) (ih h'))

/-- So nothing is there in the renumbered blocks. -/
theorem nth?_position_none {β : Type} (g : BlockId → β) (b : BlockId) (order : List BlockId)
    (h : has b order = false) : nth? (order.map g) (position b order) = none :=
  nth?_ge (order.map g) (position b order)
    (Nat.le_of_eq ((length_map' g order).trans (position_not_has b order h).symm))

/-- Every block the order lists is one of the body's `n`. -/
def allBelow (n : Nat) : List BlockId → Bool
  | [] => true
  | a :: rest => decide (a < n) && allBelow n rest

theorem has_lt (n : Nat) (b : BlockId) :
    ∀ (order : List BlockId), allBelow n order = true → has b order = true → b < n := by
  intro order
  induction order with
  | nil =>
    intro _ h
    exact false_eq_true_elim h
  | cons a rest ih =>
    intro ha hh
    by_cases e : a = b
    · have below : a < n := of_decide_eq_true (and_true_left ha)
      rw [← e]
      exact below
    · exact ih (and_true_right ha) (((has_cons b a rest).trans (if_neg e)).symm.trans hh)

/-- The order lists every block below `n`. -/
def covers (order : List BlockId) : Nat → Bool
  | 0 => true
  | n + 1 => has n order && covers order n

theorem covers_has (order : List BlockId) (b : BlockId) :
    ∀ (n : Nat), covers order n = true → b < n → has b order = true := by
  intro n
  induction n with
  | zero =>
    intro _ h
    exact absurd h (Nat.not_lt_zero b)
  | succ n ih =>
    intro hc hb
    by_cases e : b = n
    · rw [e]
      exact and_true_left hc
    · have hb' : b < n + 1 := hb
      have e' : ¬ b = n := e
      have hlt : b < n := Nat.lt_of_le_of_ne (Nat.le_of_lt_succ hb') e'
      exact ih (and_true_right hc) hlt

/-- The order starts with the entry block, so the entry block stays block 0. -/
def startsZero : List BlockId → Bool
  | [] => false
  | a :: _ => decide (a = 0)

theorem position_zero : ∀ (order : List BlockId), startsZero order = true → position 0 order = 0
  | [], h => false_eq_true_elim h
  | a :: rest, h => (position_cons 0 a rest).trans (if_pos (of_decide_eq_true h))

theorem has_zero : ∀ (order : List BlockId), startsZero order = true → has 0 order = true
  | [], h => false_eq_true_elim h
  | a :: rest, h => (has_cons 0 a rest).trans (if_pos (of_decide_eq_true h))

/-! The targets of listed blocks are listed. -/

def armsIn (order : List BlockId) : List (Int × BlockId) → Bool
  | [] => true
  | arm :: rest => has arm.2 order && armsIn order rest

def optIn (order : List BlockId) : Option BlockId → Bool
  | none => true
  | some t => has t order

/-- Every target of the terminator is listed. -/
def termIn (order : List BlockId) : Terminator → Bool
  | Terminator.goto t => has t order
  | Terminator.switchInt _ arms otherwise => armsIn order arms && has otherwise order
  | Terminator.call _ _ _ target => optIn order target
  | Terminator.ret => true
  | Terminator.assert _ _ _ t => has t order
  | Terminator.drop _ t => has t order
  | Terminator.unreachable => true

theorem armsIn_live (order : List BlockId) :
    ∀ (arms : List (Int × BlockId)), armsIn order arms = true →
      ArmsLive (fun b => has b order = true) arms
  | [], _ => True.intro
  | _ :: rest, h => And.intro (and_true_left h) (armsIn_live order rest (and_true_right h))

theorem optIn_live (order : List BlockId) :
    ∀ (target : Option BlockId), optIn order target = true →
      OptLive (fun b => has b order = true) target
  | none, _ => True.intro
  | some _, h => h

theorem termIn_live (order : List BlockId) :
    ∀ (term : Terminator), termIn order term = true →
      TermLive (fun b => has b order = true) term
  | Terminator.goto _, h => h
  | Terminator.switchInt _ arms _, h =>
    And.intro (armsIn_live order arms (and_true_left h)) (and_true_right h)
  | Terminator.call _ _ _ target, h => optIn_live order target h
  | Terminator.ret, _ => True.intro
  | Terminator.assert _ _ _ _, h => h
  | Terminator.drop _ _, h => h
  | Terminator.unreachable, _ => True.intro

def orUnreachable : Option BasicBlock → BasicBlock
  | some blk => blk
  | none => { stmts := [], terminator := Terminator.unreachable }

/-- Block `b`; a block that is not there never is asked for (`allBelow`). -/
def blockAt (blocks : List BasicBlock) (b : BlockId) : BasicBlock := orUnreachable (nth? blocks b)

/-- Every target of every block in `l` is listed in `order`. -/
def closedIn (order : List BlockId) (blocks : List BasicBlock) : List BlockId → Bool
  | [] => true
  | b :: rest => termIn order (blockAt blocks b).terminator && closedIn order blocks rest

theorem closedIn_has (order : List BlockId) (blocks : List BasicBlock) (b : BlockId) :
    ∀ (l : List BlockId), closedIn order blocks l = true → has b l = true →
      termIn order (blockAt blocks b).terminator = true := by
  intro l
  induction l with
  | nil =>
    intro _ h
    exact false_eq_true_elim h
  | cons a rest ih =>
    intro hc hh
    by_cases e : a = b
    · rw [← e]
      exact and_true_left hc
    · exact ih (and_true_right hc) (((has_cons b a rest).trans (if_neg e)).symm.trans hh)

/-! The pass. -/

/-- The body with the blocks `order` lists, each with its targets renamed to their positions. -/
def renumberBody (order : List BlockId) (body : Body) : Body :=
  { name := body.name, args := body.args, locals := body.locals,
    blocks :=
      order.map (fun old => (blockAt body.blocks old).rename (fun b => position b order)) }

/-- The program with function `fn`'s blocks renumbered by `orders fn`. -/
def renumber (orders : FnId → List BlockId) (prog : Program) : Program :=
  { adts := prog.adts, fns := mapFrom (fun i body => renumberBody (orders i) body) 0 prog.fns }

/-- The new number of each block. -/
def renumbering (orders : FnId → List BlockId) : FnId → BlockId → BlockId :=
  fun fn b => position b (orders fn)

theorem sig_renumber (orders : FnId → List BlockId) (prog : Program) :
    (renumber orders prog).sig = prog.sig := by
  funext id
  show (nth? (mapFrom (fun i body => renumberBody (orders i) body) 0 prog.fns) id).map _ =
    (nth? prog.fns id).map _
  rw [nth?_mapFrom_zero]
  cases nth? prog.fns id with
  | none => rfl
  | some body => rfl

theorem block_renumber (orders : FnId → List BlockId) (prog : Program) (fn : FnId)
    (k : BlockId) :
    (renumber orders prog).block fn k =
      blockOf k ((nth? prog.fns fn).map (fun body => renumberBody (orders fn) body)) :=
  congrTo (blockOf k)
    (nth?_mapFrom_zero (fun i body => renumberBody (orders i) body) prog.fns fn)

theorem block_of_none (prog : Program) (fn : FnId) (b : BlockId)
    (h : nth? prog.fns fn = none) : prog.block fn b = none :=
  congrTo (blockOf b) h

theorem block_of_some (prog : Program) (fn : FnId) (b : BlockId) (body : Body)
    (h : nth? prog.fns fn = some body) : prog.block fn b = nth? body.blocks b :=
  congrTo (blockOf b) h

theorem block_renumber_none (orders : FnId → List BlockId) (prog : Program) (fn : FnId)
    (k : BlockId) (h : nth? prog.fns fn = none) : (renumber orders prog).block fn k = none :=
  (block_renumber orders prog fn k).trans
    (congrTo (fun o => blockOf k (Option.map (fun body => renumberBody (orders fn) body) o)) h)

theorem block_renumber_some (orders : FnId → List BlockId) (prog : Program) (fn : FnId)
    (k : BlockId) (body : Body) (h : nth? prog.fns fn = some body) :
    (renumber orders prog).block fn k =
      nth? ((orders fn).map (fun old =>
        (blockAt body.blocks old).rename (fun b => position b (orders fn)))) k :=
  (block_renumber orders prog fn k).trans
    (congrTo (fun o => blockOf k (Option.map (fun body => renumberBody (orders fn) body) o)) h)

/-- A listed block of the program is, renamed, at its position in the renumbered program. -/
theorem block_kept (orders : FnId → List BlockId) (prog : Program) (fn : FnId) (b : BlockId)
    (body : Body) (hbody : nth? prog.fns fn = some body)
    (hbelow : allBelow body.blocks.length (orders fn) = true)
    (hb : has b (orders fn) = true) :
    (renumber orders prog).block fn (renumbering orders fn b) =
      (prog.block fn b).map (BasicBlock.rename (renumbering orders fn)) := by
  have hlt : b < body.blocks.length := has_lt body.blocks.length b (orders fn) hbelow hb
  have ⟨blk, hblk⟩ := nth?_of_lt body.blocks b hlt
  have left : (renumber orders prog).block fn (renumbering orders fn b) =
      some (blk.rename (renumbering orders fn)) :=
    (block_renumber_some orders prog fn (renumbering orders fn b) body hbody).trans
      ((nth?_position
          (fun old => (blockAt body.blocks old).rename (fun x => position x (orders fn))) b
          (orders fn) hb).trans
        (congrTo
          (fun o => some ((orUnreachable o).rename (fun x => position x (orders fn)))) hblk))
  have right : (prog.block fn b).map (BasicBlock.rename (renumbering orders fn)) =
      some (blk.rename (renumbering orders fn)) :=
    congrTo (Option.map (BasicBlock.rename (renumbering orders fn)))
      ((block_of_some prog fn b body hbody).trans hblk)
  exact left.trans right.symm

/-- A function the program does not have has no block in either program. -/
theorem block_no_fn (orders : FnId → List BlockId) (prog : Program) (fn : FnId) (b : BlockId)
    (hbody : nth? prog.fns fn = none) :
    (renumber orders prog).block fn (renumbering orders fn b) =
      (prog.block fn b).map (BasicBlock.rename (renumbering orders fn)) :=
  (block_renumber_none orders prog fn (renumbering orders fn b) hbody).trans
    (congrTo (Option.map (BasicBlock.rename (renumbering orders fn)))
      (block_of_none prog fn b hbody)).symm

/-- `c i body` holds of each function `body`, `i` its index from `i` up. -/
def checkFrom (c : FnId → Body → Bool) : Nat → List Body → Bool
  | _, [] => true
  | i, body :: rest => c i body && checkFrom c (i + 1) rest

theorem checkFrom_nth (c : FnId → Body → Bool) :
    ∀ (l : List Body) (i k : Nat) (body : Body), checkFrom c i l = true →
      nth? l k = some body → c (i + k) body = true
  | [], _, _, _, _, h => none_eq_some_elim h
  | b :: _, i, 0, body, hc, h => by
    have e : b = body := Option.some.inj h
    rw [← e]
    exact and_true_left hc
  | _ :: rest, i, k + 1, body, hc, h => by
    have r := checkFrom_nth c rest (i + 1) k body (and_true_right hc) h
    have e : i + 1 + k = i + (k + 1) := by omega
    rw [e] at r
    exact r

theorem checkFrom_zero (c : FnId → Body → Bool) (l : List Body) (k : Nat) (body : Body)
    (hc : checkFrom c 0 l = true) (h : nth? l k = some body) : c k body = true := by
  have r := checkFrom_nth c l 0 k body hc h
  rw [Nat.zero_add] at r
  exact r

/-- A function with a signature is a function the program has. -/
theorem sig_some (prog : Program) (id : FnId) (s : FnSig) (h : prog.sig id = some s) :
    ∃ body, nth? prog.fns id = some body := by
  cases hb : nth? prog.fns id with
  | some body => exact ⟨body, rfl⟩
  | none =>
    have e : prog.sig id = none :=
      congrTo
        (Option.map (fun body => ({ args := body.args, locals := body.locals.length } : FnSig)))
        hb
    exact none_eq_some_elim (e.symm.trans h)

/-! Every block kept: a permutation. -/

/-- What `checkPermutation` asks of a function's order: it lists only blocks the function
has, the entry block first, and misses none. -/
def permCheck (orders : FnId → List BlockId) (fn : FnId) (body : Body) : Bool :=
  allBelow body.blocks.length (orders fn) &&
    (startsZero (orders fn) && covers (orders fn) body.blocks.length)

def checkPermutation (orders : FnId → List BlockId) (prog : Program) : Bool :=
  checkFrom (permCheck orders) 0 prog.fns

theorem renumbers_total (orders : FnId → List BlockId) (prog : Program)
    (h : checkPermutation orders prog = true) :
    Renumbers prog (renumber orders prog) (renumbering orders) (fun _ _ => True) where
  adts := rfl
  sig := sig_renumber orders prog
  entry := by
    intro id s hs
    have ⟨body, hbody⟩ := sig_some prog id s hs
    have hc : permCheck orders id body = true :=
      checkFrom_zero (permCheck orders) prog.fns id body h hbody
    exact position_zero (orders id) (and_true_left (and_true_right hc))
  block := by
    intro fn b _
    cases hbody : nth? prog.fns fn with
    | none => exact block_no_fn orders prog fn b hbody
    | some body =>
      have hc : permCheck orders fn body = true :=
        checkFrom_zero (permCheck orders) prog.fns fn body h hbody
      cases hh : has b (orders fn) with
      | true => exact block_kept orders prog fn b body hbody (and_true_left hc) hh
      | false =>
        have left : (renumber orders prog).block fn (renumbering orders fn b) = none :=
          (block_renumber_some orders prog fn (renumbering orders fn b) body hbody).trans
            (nth?_position_none
              (fun old => (blockAt body.blocks old).rename (fun x => position x (orders fn))) b
              (orders fn) hh)
        cases hsrc : nth? body.blocks b with
        | none =>
          have right : prog.block fn b = none := (block_of_some prog fn b body hbody).trans hsrc
          exact left.trans
            (congrTo (Option.map (BasicBlock.rename (renumbering orders fn))) right).symm
        | some blk =>
          have hlt : b < body.blocks.length := nth?_lt body.blocks b blk hsrc
          have listed : has b (orders fn) = true :=
            covers_has (orders fn) b body.blocks.length (and_true_right (and_true_right hc)) hlt
          exact true_eq_false_elim (listed.symm.trans hh)

/-- A permutation of blocks is correct: for every fuel and every state, the renumbered
program from the renamed state runs as the program does. -/
theorem permuteBlocks_correct (orders : FnId → List BlockId) (prog : Program)
    (h : checkPermutation orders prog = true) (fuel : Nat) (st : State) :
    run (renumber orders prog) fuel (st.rename (renumbering orders)) = run prog fuel st :=
  (renumbers_total orders prog h).sim_total.run fuel st True.intro

/-! Calls: a call of the renumbered program starts in the renamed start state. -/

theorem start_rename {src tgt : Program} {ρ : FnId → BlockId → BlockId}
    {L : FnId → BlockId → Prop} (h : Renumbers src tgt ρ L) (id : FnId) (base : List Slot)
    (args : List Value) :
    tgt.start id base args = Outcome.map (State.rename ρ) (src.start id base args) := by
  unfold Program.start
  rw [h.sig]
  cases hs : src.sig id with
  | none => rfl
  | some s =>
    exact Outcome.bind_commutes_map
      (State.enter_rename ρ { base := base, stack := [], next := 1 } 0 rfl id s args
        { frame := 0, slot := 0, path := [] } none (h.entry id s hs))
      (fun next _ => by
        cases next with
        | state st => rfl
        | done v => rfl)

/-- A permutation of blocks gives every call the same outcome. -/
theorem permuteBlocks_call (orders : FnId → List BlockId) (prog : Program)
    (h : checkPermutation orders prog = true) (fuel : Nat) (id : FnId) (base : List Slot)
    (args : List Value) :
    (renumber orders prog).call fuel id base args = prog.call fuel id base args :=
  Outcome.bind_map_congr (start_rename (renumbers_total orders prog h) id base args)
    (fun st _ => permuteBlocks_correct orders prog h fuel st)

/-! Blocks dropped: only what control reaches is kept. -/

/-- What `checkKept` asks of a function's order: it lists only blocks the function has, the
entry block first, and every target of a listed block. -/
def keptCheck (orders : FnId → List BlockId) (fn : FnId) (body : Body) : Bool :=
  allBelow body.blocks.length (orders fn) &&
    (startsZero (orders fn) && closedIn (orders fn) body.blocks (orders fn))

def checkKept (orders : FnId → List BlockId) (prog : Program) : Bool :=
  checkFrom (keptCheck orders) 0 prog.fns

/-- The blocks the orders keep. -/
def Kept (orders : FnId → List BlockId) : FnId → BlockId → Prop :=
  fun fn b => has b (orders fn) = true

theorem renumbers_kept (orders : FnId → List BlockId) (prog : Program)
    (h : checkKept orders prog = true) :
    Renumbers prog (renumber orders prog) (renumbering orders) (Kept orders) where
  adts := rfl
  sig := sig_renumber orders prog
  entry := by
    intro id s hs
    have ⟨body, hbody⟩ := sig_some prog id s hs
    have hc : keptCheck orders id body = true :=
      checkFrom_zero (keptCheck orders) prog.fns id body h hbody
    exact position_zero (orders id) (and_true_left (and_true_right hc))
  block := by
    intro fn b hb
    cases hbody : nth? prog.fns fn with
    | none => exact block_no_fn orders prog fn b hbody
    | some body =>
      have hc : keptCheck orders fn body = true :=
        checkFrom_zero (keptCheck orders) prog.fns fn body h hbody
      exact block_kept orders prog fn b body hbody (and_true_left hc) hb

/-- The kept blocks are closed: control at a kept block stays at kept blocks. -/
theorem closed_kept (orders : FnId → List BlockId) (prog : Program)
    (h : checkKept orders prog = true) : Closed prog (Kept orders) where
  entry := by
    intro id s hs
    have ⟨body, hbody⟩ := sig_some prog id s hs
    have hc : keptCheck orders id body = true :=
      checkFrom_zero (keptCheck orders) prog.fns id body h hbody
    exact has_zero (orders id) (and_true_left (and_true_right hc))
  targets := by
    intro fn b blk hb hblk
    cases hbody : nth? prog.fns fn with
    | none => exact none_eq_some_elim ((block_of_none prog fn b hbody).symm.trans hblk)
    | some body =>
      have hc : keptCheck orders fn body = true :=
        checkFrom_zero (keptCheck orders) prog.fns fn body h hbody
      have ht : termIn (orders fn) (blockAt body.blocks b).terminator = true :=
        closedIn_has (orders fn) body.blocks b (orders fn)
          (and_true_right (and_true_right hc)) hb
      have hn : nth? body.blocks b = some blk :=
        (block_of_some prog fn b body hbody).symm.trans hblk
      have hat : blockAt body.blocks b = blk := congrTo orUnreachable hn
      rw [hat] at ht
      exact termIn_live (orders fn) blk.terminator ht

/-- Removing the blocks an order does not list is correct on every state the reachability
invariant holds of: the running frame is at a kept block, and every frame returns to one. -/
theorem renumber_kept_correct (orders : FnId → List BlockId) (prog : Program)
    (h : checkKept orders prog = true) (fuel : Nat) (st : State)
    (hr : Reaches (Kept orders) st) :
    run (renumber orders prog) fuel (st.rename (renumbering orders)) = run prog fuel st :=
  ((renumbers_kept orders prog h).sim_live (closed_kept orders prog h)).run fuel st hr

/-- A call starts in a state the reachability invariant holds of. -/
theorem start_reaches {prog : Program} {L : FnId → BlockId → Prop} (hc : Closed prog L)
    (id : FnId) (base : List Slot) (args : List Value) :
    Outcome.All (Reaches L) (prog.start id base args) := by
  unfold Program.start
  cases hs : prog.sig id with
  | none => exact Outcome.all_undefined _ _
  | some s =>
    exact Outcome.all_bind
      (State.enter_reaches L { base := base, stack := [], next := 1 } 0 rfl True.intro id s args
        { frame := 0, slot := 0, path := [] } none (hc.entry id s hs) True.intro)
      (fun next hn => by
        cases next with
        | state st => exact Outcome.all_ok hn
        | done v => exact Outcome.all_undefined _ _)

/-- Removing the blocks an order does not list gives every call the same outcome. -/
theorem renumber_kept_call (orders : FnId → List BlockId) (prog : Program)
    (h : checkKept orders prog = true) (fuel : Nat) (id : FnId) (base : List Slot)
    (args : List Value) :
    (renumber orders prog).call fuel id base args = prog.call fuel id base args :=
  Outcome.bind_map_congr (start_rename (renumbers_kept orders prog h) id base args)
    (fun st hst =>
      renumber_kept_correct orders prog h fuel st
        (start_reaches (closed_kept orders prog h) id base args st hst))

/-! `removeDeadBlocks`: the order is the blocks block 0 reaches. -/

def optList : Option BlockId → List BlockId
  | some t => [t]
  | none => []

def successors : Terminator → List BlockId
  | Terminator.goto t => [t]
  | Terminator.switchInt _ arms otherwise => arms.map (fun arm => arm.2) ++ [otherwise]
  | Terminator.call _ _ _ target => optList target
  | Terminator.ret => []
  | Terminator.assert _ _ _ t => [t]
  | Terminator.drop _ t => [t]
  | Terminator.unreachable => []

def addNew (seen : List BlockId) (t : BlockId) : List BlockId :=
  if has t seen = true then seen else seen ++ [t]

/-- `seen` with the targets of its blocks. -/
def grow (blocks : List BasicBlock) (seen : List BlockId) : List BlockId :=
  seen.foldl (fun acc b => (successors (blockAt blocks b).terminator).foldl addNew acc) seen

/-- `grow`, `k` times: with as many rounds as blocks, everything `seen` reaches. -/
def reachSet (blocks : List BasicBlock) : Nat → List BlockId → List BlockId
  | 0, seen => seen
  | k + 1, seen => reachSet blocks k (grow blocks seen)

/-- The blocks block 0 reaches, in their old order. -/
def reachableOrder (body : Body) : List BlockId :=
  (List.range body.blocks.length).filter
    (fun b => has b (reachSet body.blocks body.blocks.length [0]))

def orderOf : Option Body → List BlockId
  | some body => reachableOrder body
  | none => []

def reachableOrders (prog : Program) : FnId → List BlockId :=
  fun fn => orderOf (nth? prog.fns fn)

/-- The pass: every block that block 0 of its function does not reach is removed, and the
rest keep their order. -/
def removeDeadBlocks (prog : Program) : Program := renumber (reachableOrders prog) prog

/-- Whether what `removeDeadBlocks` keeps is closed: the kernel evaluates it per program. -/
def checkDead (prog : Program) : Bool := checkKept (reachableOrders prog) prog

/-- Dead-block removal is correct on every state the reachability invariant holds of. -/
theorem removeDeadBlocks_correct (prog : Program) (h : checkDead prog = true) (fuel : Nat)
    (st : State) (hr : Reaches (Kept (reachableOrders prog)) st) :
    run (removeDeadBlocks prog) fuel (st.rename (renumbering (reachableOrders prog))) =
      run prog fuel st :=
  renumber_kept_correct (reachableOrders prog) prog h fuel st hr

/-- Dead-block removal gives every call the same outcome. -/
theorem removeDeadBlocks_call (prog : Program) (h : checkDead prog = true) (fuel : Nat)
    (id : FnId) (base : List Slot) (args : List Value) :
    (removeDeadBlocks prog).call fuel id base args = prog.call fuel id base args :=
  renumber_kept_call (reachableOrders prog) prog h fuel id base args

end Blaze.Mir.Opt
