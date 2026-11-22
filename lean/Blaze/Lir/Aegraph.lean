/-!
Soundness of scalar 64-bit algebraic identities and strength reductions performed by Blaze's
acyclic e-graph optimizer (`crates/blaze-lir/src/simplify/egraph.rs`).

Every theorem is proved directly over Lean 4 core's `BitVec 64` using modular and bitwise
`Nat` lemmas (no `bv_decide`, no native evaluation, no extra axioms).
-/

namespace Blaze.Lir.Aegraph

/-- Unary scalar 64-bit LIR integer operations modeled by `aegraph`. -/
inductive UnOp where
  | neg
  | not
  deriving DecidableEq, Repr

/-- Binary scalar 64-bit LIR integer operations modeled by `aegraph`. -/
inductive BinOp where
  | add
  | sub
  | mul
  | and
  | or
  | xor
  | shl
  | lshr
  | udiv
  | urem
  deriving DecidableEq, Repr

/-- Evaluate a unary scalar 64-bit LIR operation. -/
def evalUn : UnOp → BitVec 64 → BitVec 64
  | .neg, x => -x
  | .not, x => ~~~x

/-- Evaluate a binary scalar 64-bit LIR operation; trapping division or remainder by zero
    yields `none` so `aegraph` never folds a trapping instruction away. -/
def evalBin : BinOp → BitVec 64 → BitVec 64 → Option (BitVec 64)
  | .add, a, b => some (a + b)
  | .sub, a, b => some (a - b)
  | .mul, a, b => some (a * b)
  | .and, a, b => some (a &&& b)
  | .or, a, b => some (a ||| b)
  | .xor, a, b => some (a ^^^ b)
  | .shl, a, b => some (a <<< (b.toNat % 64))
  | .lshr, a, b => some (a >>> (b.toNat % 64))
  | .udiv, a, b => if b = 0 then none else some (a / b)
  | .urem, a, b => if b = 0 then none else some (a % b)

/-- Conditional scalar select (`Op::Select` on 64-bit LIR values). -/
def select (cond : Bool) (t f : BitVec 64) : BitVec 64 :=
  if cond then t else f

/-! ### Helper lemmas for powers of two within 64 bits -/

private theorem toNat_one_shl (k : Nat) (hk : k < 64) :
    ((1 : BitVec 64) <<< k).toNat = 2 ^ k := by
  have hlt : 2 ^ k < 2 ^ 64 := Nat.pow_lt_pow_right (by omega) hk
  have h1 : (1 : BitVec 64).toNat = 1 := by decide
  rw [BitVec.toNat_shiftLeft, Nat.shiftLeft_eq, h1, Nat.one_mul, Nat.mod_eq_of_lt hlt]

private theorem toNat_one_shl_sub_one (k : Nat) (hk : k < 64) :
    (((1 : BitVec 64) <<< k) - 1).toNat = 2 ^ k - 1 := by
  have hshl : ((1 : BitVec 64) <<< k).toNat = 2 ^ k := toNat_one_shl k hk
  have h1 : (1 : BitVec 64).toNat = 1 := by decide
  have hpos : 1 ≤ 2 ^ k := Nat.one_le_pow k 2 (by omega)
  have hlt : 2 ^ k < 2 ^ 64 := Nat.pow_lt_pow_right (by omega) hk
  rw [BitVec.toNat_sub, hshl, h1]
  omega

private theorem nat_mod_two_pow_eq_and (x k : Nat) :
    x % 2 ^ k = x &&& (2 ^ k - 1) := by
  apply Nat.eq_of_testBit_eq
  intro i
  rw [Nat.testBit_mod_two_pow, Nat.testBit_and, Nat.testBit_two_pow_sub_one, Bool.and_comm]

/-! ### Algebraic identities -/

/-- Additive right identity: `x + 0 = x`. -/
theorem aegraph_add_zero (x : BitVec 64) :
    x + 0 = x := by
  simp

/-- Self-subtraction cancels to zero: `x - x = 0`. -/
theorem aegraph_sub_self (x : BitVec 64) :
    x - x = 0 := by
  simp

/-- Multiplicative right identity: `x * 1 = x`. -/
theorem aegraph_mul_one (x : BitVec 64) :
    x * 1 = x := by
  simp

/-- Multiplicative right absorbing element: `x * 0 = 0`. -/
theorem aegraph_mul_zero (x : BitVec 64) :
    x * 0 = 0 := by
  simp

/-- Self-XOR cancels to zero: `x ^^^ x = 0`. -/
theorem aegraph_xor_self (x : BitVec 64) :
    x ^^^ x = 0 := by
  simp

/-- Two's-complement negation is an involution: `-(-x) = x`. -/
theorem aegraph_neg_neg (x : BitVec 64) :
    -(-x) = x := by
  simp [BitVec.neg_neg]

/-- Bitwise complement is an involution: `~~~(~~~x) = x`. -/
theorem aegraph_not_not (x : BitVec 64) :
    ~~~(~~~x) = x := by
  simp [BitVec.not_not]

/-- Addition commutes (enabling commutative GVN canonicalization). -/
theorem aegraph_add_comm (x y : BitVec 64) :
    x + y = y + x := by
  rw [BitVec.add_comm]

/-- Multiplication commutes (enabling commutative GVN canonicalization). -/
theorem aegraph_mul_comm (x y : BitVec 64) :
    x * y = y * x := by
  rw [BitVec.mul_comm]

/-- Selecting between identical arms collapses regardless of condition. -/
theorem aegraph_select_same (cond : Bool) (x : BitVec 64) :
    select cond x x = x := by
  cases cond <;> rfl

/-! ### Strength reductions -/

/-- Multiplying by `2 ^ k` (`1 <<< k`, `k < 64`) equals logical left shift by `k`. -/
theorem aegraph_mul_pow2_shl (x : BitVec 64) (k : Nat) (hk : k < 64) :
    x * ((1 : BitVec 64) <<< k) = x <<< k := by
  apply BitVec.eq_of_toNat_eq
  rw [BitVec.toNat_mul, toNat_one_shl k hk, BitVec.toNat_shiftLeft, Nat.shiftLeft_eq]

/-- Unsigned division by `2 ^ k` (`1 <<< k`, `k < 64`) equals logical right shift by `k`. -/
theorem aegraph_udiv_pow2_lshr (x : BitVec 64) (k : Nat) (hk : k < 64) :
    x / ((1 : BitVec 64) <<< k) = x >>> k := by
  apply BitVec.eq_of_toNat_eq
  rw [BitVec.toNat_udiv, toNat_one_shl k hk, BitVec.toNat_ushiftRight, Nat.shiftRight_eq_div_pow]

/-- Unsigned remainder by `2 ^ k` (`1 <<< k`, `k < 64`) equals bitmasking with `(1 <<< k) - 1`. -/
theorem aegraph_urem_pow2_and (x : BitVec 64) (k : Nat) (hk : k < 64) :
    x % ((1 : BitVec 64) <<< k) = x &&& (((1 : BitVec 64) <<< k) - 1) := by
  apply BitVec.eq_of_toNat_eq
  rw [BitVec.toNat_umod, toNat_one_shl k hk, BitVec.toNat_and, toNat_one_shl_sub_one k hk]
  exact nat_mod_two_pow_eq_and x.toNat k

/-- Adding a value to itself equals shifting left by 1: `x + x = x <<< 1`. -/
theorem aegraph_add_self_shl1 (x : BitVec 64) :
    x + x = x <<< 1 := by
  apply BitVec.eq_of_toNat_eq
  rw [BitVec.toNat_add, BitVec.toNat_shiftLeft, Nat.shiftLeft_eq]
  have h : x.toNat + x.toNat = x.toNat * 2 ^ 1 := by omega
  rw [h]

end Blaze.Lir.Aegraph
