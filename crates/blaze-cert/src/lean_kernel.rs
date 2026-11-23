//! A self-contained, `#![no_std]`, `#![forbid(unsafe_code)]`, explicit-allocator (`Alloc<'a>`)
//! **Lean 4 Dependent Type Theory Kernel Checker**.
//!
//! This module implements a standalone trusted kernel for Lean 4's Calculus of Inductive
//! Constructions (CIC) with:
//!
//! - **Universe levels** ([`Level`], [`LevelId`]): `Zero`, `Succ`, `Max`, `IMax`, `Param`,
//!   normalization, universe polymorphism substitution, and sound `level_leq` / `level_eq`.
//! - **Core expressions** ([`Expr`], [`ExprId`]): arena-allocated de Bruijn terms (`BVar`, `FVar`,
//!   `Sort`, `Const`, `App`, `Lam`, `ForallE`, `LetE`, `Lit`, `Proj`) with explicit depth and fuel
//!   guards against recursion overflow and non-termination.
//! - **Environment & declarations** ([`Decl`], [`RecRule`]): `Axiom`, `Def` (delta-reducible),
//!   `Theorem` (opaque `Prop` proof), `Inductive`, `Ctor`, `Recursor` (with iota-reduction rules),
//!   and `Quot`.
//! - **Reduction & definitional equality** (`whnf`, `is_def_eq`): beta, zeta, lazy delta, iota
//!   (recursor on constructor and `Nat` literals), structure projection reduction, proof
//!   irrelevance, and eta-equivalence.
//! - **Bidirectional type checking & theorem verification** (`infer_type`, `check_decl`,
//!   `verify_theorem`): universe-stratified typing (`Sort u : Sort (u + 1)`), impredicative `Prop`
//!   (`IMax`), and transitive axiom auditing against an allowlist (catching `sorryAx` and rogue
//!   axioms).
//! - **Lean 4 `.export` certificate loader** (`load_export`): parses standard line-based Lean 4
//!   kernel export certificates (`#NS`, `#NI`, `#US`, `#UM`, `#UIM`, `#UP`, `#EV`, `#ES`, `#EC`,
//!   `#EA`, `#EL`, `#EP`, `#EZ`, `#EJ`, `#AX`, `#DEF`, `#THM`, `#QUOT`) and type-checks every
//!   declaration directly in Rust with zero external Lean installation.

use core::fmt;

use blaze_int::{Int, idx, len32};
use blaze_own::{ABTreeMap, ABTreeSet, AString, AVec, Alloc};

/// Default allowed classical axioms for [`KernelEnv::verify_theorem`].
///
/// Any axiom outside this list (such as `sorryAx` or custom axioms) is rejected.
pub const DEFAULT_ALLOWED_AXIOMS: [&str; 3] = ["propext", "Classical.choice", "Quot.sound"];

/// Default maximum recursion depth for term traversal and reduction.
pub const DEFAULT_MAX_DEPTH: u32 = 256;

/// Default reduction/checking fuel budget per top-level kernel operation.
pub const DEFAULT_FUEL_BUDGET: u32 = 500_000;

/// Interned hierarchical declaration or parameter name in [`KernelEnv`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NameId(pub u32);

/// Arena handle for a universe [`Level`] in [`KernelEnv`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LevelId(pub u32);

/// Arena handle for a core [`Expr`] in [`KernelEnv`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExprId(pub u32);

/// Slice range into [`KernelEnv`]'s level-list pool for [`Expr::Const`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LevelRange {
    pub start: u32,
    pub len: u32,
}

/// Slice range into [`KernelEnv`]'s name-list pool (level parameters or constructor lists).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct NameRange {
    start: u32,
    len: u32,
}

/// Slice range into [`KernelEnv`]'s recursor-rule pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct RuleRange {
    start: u32,
    len: u32,
}

/// Universe level expression in Lean 4's stratified hierarchy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// Universe `0` (`Sort 0 = Prop`).
    Zero,
    /// Successor universe `u + 1`.
    Succ(LevelId),
    /// Maximum universe `max u v`.
    Max(LevelId, LevelId),
    /// Impredicative maximum `imax u v`: `0` when `v = 0`, otherwise `max u v`.
    IMax(LevelId, LevelId),
    /// Universe polymorphism parameter.
    Param(NameId),
}

/// Core Lean 4 literal value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Literal {
    Nat(u64),
    Str(NameId),
}

/// Core Lean 4 expression node stored in [`KernelEnv`]'s arena.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Expr {
    /// Bound variable (de Bruijn index, `0` = innermost binder).
    BVar(u32),
    /// Local context free variable carrying its unique ID and closed type.
    FVar { id: u32, ty: ExprId },
    /// Universe sort (`Sort 0 = Prop`, `Sort 1 = Type 0`, `Sort (Succ u) = Type u`).
    Sort(LevelId),
    /// Environment constant instantiated with universe level arguments.
    Const { name: NameId, levels: LevelRange },
    /// Function application `f a`.
    App(ExprId, ExprId),
    /// Lambda abstraction `fun (_ : binder_ty) => body`.
    Lam { binder_ty: ExprId, body: ExprId },
    /// Dependent product `∀ (_ : binder_ty), body`.
    ForallE { binder_ty: ExprId, body: ExprId },
    /// Local let binding `let (_ : ty) := val; body`.
    LetE { ty: ExprId, val: ExprId, body: ExprId },
    /// Primitive literal (`Nat` or `String`).
    Lit(Literal),
    /// Structure field projection `expr.field_idx` for single-constructor inductive `struct_name`.
    Proj { struct_name: NameId, field_idx: u32, expr: ExprId },
}

/// One iota-reduction rule for a [`Decl::Recursor`].
///
/// `rhs` is a closed lambda taking `(params..., motives..., minors..., ctor_fields...)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecRule {
    pub ctor_name: NameId,
    pub nfields: u32,
    pub rhs: ExprId,
}

/// A Lean 4 environment declaration submitted to [`KernelEnv::check_decl`].
#[derive(Debug)]
pub enum Decl<'a> {
    /// Unproven axiom `axiom name.{level_params} : ty`.
    Axiom {
        name: NameId,
        level_params: AVec<'a, NameId>,
        ty: ExprId,
    },
    /// Definition (`is_theorem == false`, delta-reducible) or theorem (`is_theorem == true`,
    /// opaque and required to inhabit `Prop`).
    Def {
        name: NameId,
        level_params: AVec<'a, NameId>,
        ty: ExprId,
        value: ExprId,
        is_theorem: bool,
    },
    /// Inductive type family.
    Inductive {
        name: NameId,
        level_params: AVec<'a, NameId>,
        num_params: u32,
        ty: ExprId,
        ctors: AVec<'a, NameId>,
    },
    /// Constructor of an inductive type.
    Ctor {
        name: NameId,
        ind_name: NameId,
        ctor_idx: u32,
        num_params: u32,
        num_fields: u32,
        level_params: AVec<'a, NameId>,
        ty: ExprId,
    },
    /// Recursor / eliminator of an inductive type with iota-reduction rules.
    Recursor {
        name: NameId,
        ind_name: NameId,
        num_params: u32,
        num_indices: u32,
        num_motives: u32,
        num_minors: u32,
        level_params: AVec<'a, NameId>,
        ty: ExprId,
        rules: AVec<'a, RecRule>,
    },
    /// Quotient primitive declaration (`Quot`, `Quot.mk`, `Quot.lift`, `Quot.ind`).
    Quot {
        name: NameId,
        level_params: AVec<'a, NameId>,
        ty: ExprId,
    },
}

/// Read-only classification of a stored declaration in [`KernelEnv`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclKind {
    Axiom,
    Def { is_theorem: bool },
    Inductive { num_params: u32, num_ctors: u32 },
    Ctor { ind_name: NameId, ctor_idx: u32, num_params: u32, num_fields: u32 },
    Recursor {
        ind_name: NameId,
        num_params: u32,
        num_indices: u32,
        num_motives: u32,
        num_minors: u32,
    },
    Quot,
}

/// Compact `Copy` representation of a checked declaration stored in [`KernelEnv`].
#[derive(Clone, Copy, Debug)]
enum StoredDecl {
    Axiom {
        level_params: NameRange,
        ty: ExprId,
    },
    Def {
        level_params: NameRange,
        ty: ExprId,
        value: ExprId,
        is_theorem: bool,
        height: u32,
    },
    Inductive {
        level_params: NameRange,
        num_params: u32,
        ty: ExprId,
        ctors: NameRange,
    },
    Ctor {
        ind_name: NameId,
        ctor_idx: u32,
        num_params: u32,
        num_fields: u32,
        level_params: NameRange,
        ty: ExprId,
    },
    Recursor {
        ind_name: NameId,
        num_params: u32,
        num_indices: u32,
        num_motives: u32,
        num_minors: u32,
        level_params: NameRange,
        ty: ExprId,
        rules: RuleRange,
    },
    Quot {
        level_params: NameRange,
        ty: ExprId,
    },
}

impl StoredDecl {
    fn ty(self) -> ExprId {
        match self {
            StoredDecl::Axiom { ty, .. }
            | StoredDecl::Def { ty, .. }
            | StoredDecl::Inductive { ty, .. }
            | StoredDecl::Ctor { ty, .. }
            | StoredDecl::Recursor { ty, .. }
            | StoredDecl::Quot { ty, .. } => ty,
        }
    }

    fn level_params(self) -> NameRange {
        match self {
            StoredDecl::Axiom { level_params, .. }
            | StoredDecl::Def { level_params, .. }
            | StoredDecl::Inductive { level_params, .. }
            | StoredDecl::Ctor { level_params, .. }
            | StoredDecl::Recursor { level_params, .. }
            | StoredDecl::Quot { level_params, .. } => level_params,
        }
    }
}

/// Specific `.export` parsing error category.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportParseKind {
    InvalidInteger,
    MissingToken,
    UnknownTag,
    UnknownNameIndex(u32),
    UnknownLevelIndex(u32),
    UnknownExprIndex(u32),
    InvalidBinderInfo,
    InvalidHexString,
}

/// Typed error returned by the Lean 4 kernel checker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelError {
    /// Expected type and inferred type are not definitionally equal.
    TypeMismatch { expected: ExprId, got: ExprId },
    /// Theorem transitively depends on an axiom not in the caller's allowlist.
    DisallowedAxiom { theorem: NameId, axiom: NameId },
    /// Theorem type does not inhabit `Prop` (`Sort 0`).
    NotAProp { name: NameId, sort: LevelId },
    /// Applied expression does not have a `ForallE` (function) type.
    NotAFunction { expr: ExprId, ty: ExprId },
    /// Expression expected to be a type does not have a `Sort` type.
    NotASort { expr: ExprId, ty: ExprId },
    /// Referenced constant does not exist in the environment.
    UnknownConst(NameId),
    /// Attempted to add a declaration whose name already exists.
    DuplicateDecl(NameId),
    /// Duplicate universe parameter name in a declaration.
    DuplicateLevelParam(NameId),
    /// Universe parameter used in a declaration was not declared in its `level_params`.
    UnknownLevelParam(NameId),
    /// Constant instantiated with the wrong number of universe arguments.
    LevelParamCountMismatch { name: NameId, expected: u32, got: u32 },
    /// Unbound de Bruijn variable encountered during type checking.
    LooseBVar(u32),
    /// Invalid structure projection (missing struct, non-struct, out-of-range field, or
    /// illegal non-`Prop` projection from a `Prop` structure).
    InvalidProjection { struct_name: NameId, field_idx: u32 },
    /// Constructor or recursor references a missing or non-inductive parent type.
    InvalidInductive(NameId),
    /// `verify_theorem` called on a declaration that has no proof body (e.g. an axiom).
    NotATheorem(NameId),
    /// Reduction or checking fuel budget exhausted.
    FuelExhausted,
    /// Recursion depth guard exceeded.
    DepthExceeded,
    /// Malformed line in a Lean 4 `.export` certificate.
    ExportParseError { line: u32, kind: ExportParseKind },
}

impl fmt::Display for KernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lean_kernel error: {self:?}")
    }
}

impl core::error::Error for KernelError {}

/// Proof verification summary returned by [`KernelEnv::verify_theorem`].
#[derive(Debug)]
pub struct VerifiedTheorem<'a> {
    pub name: NameId,
    pub used_axioms: AVec<'a, NameId>,
}

/// Summary of declarations checked while loading a `.export` certificate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExportSummary {
    pub names: u32,
    pub levels: u32,
    pub exprs: u32,
    pub decls: u32,
}

/// Internal recursion budget threaded through kernel operations.
#[derive(Clone, Copy)]
struct Budget {
    fuel: u32,
    max_depth: u32,
}

impl Budget {
    #[inline]
    fn tick(&mut self, depth: u32) -> Result<(), KernelError> {
        if depth > self.max_depth {
            return Err(KernelError::DepthExceeded);
        }
        if self.fuel == 0 {
            return Err(KernelError::FuelExhausted);
        }
        self.fuel -= 1;
        Ok(())
    }
}

/// De Bruijn expression transformation dispatched by [`KernelEnv::subst_expr_rec`].
#[derive(Clone, Copy)]
enum SubstOp {
    /// Lift `BVar(i)` with `i >= bvar_depth` by `amount` (saturating).
    Lift { amount: u32 },
    /// Substitute `BVar(bvar_depth)` with `arg` (lifted by `bvar_depth`), shifting higher `BVar`s down by `1`.
    Instantiate { arg: ExprId },
    /// Abstract `FVar { id == target_fvar }` to `BVar(bvar_depth)`, shifting `BVar(i >= bvar_depth)` up by `1`.
    AbstractFVar { target_fvar: u32 },
}

/// Self-contained Lean 4 Dependent Type Theory kernel environment and arena.
pub struct KernelEnv<'a> {
    alloc: Alloc<'a>,
    names: AVec<'a, AString<'a>>,
    name_intern: ABTreeMap<'a, AString<'a>, NameId>,
    levels: AVec<'a, Level>,
    level_intern: ABTreeMap<'a, Level, LevelId>,
    level_lists: AVec<'a, LevelId>,
    level_list_intern_1: ABTreeMap<'a, LevelId, LevelRange>,
    level_list_intern_2: ABTreeMap<'a, (LevelId, LevelId), LevelRange>,
    name_lists: AVec<'a, NameId>,
    rec_rules: AVec<'a, RecRule>,
    exprs: AVec<'a, Expr>,
    expr_intern: ABTreeMap<'a, Expr, ExprId>,
    decls: ABTreeMap<'a, NameId, StoredDecl>,
    next_fvar: u32,
    next_def_height: u32,
    max_depth: u32,
    fuel_budget: u32,
    zero_level: LevelId,
}

impl<'a> KernelEnv<'a> {
    /// Create a fresh empty kernel environment backed by `alloc`.
    pub fn new_in(alloc: Alloc<'a>) -> Self {
        let mut levels = AVec::new_in(alloc);
        let mut level_intern = ABTreeMap::new_in(alloc);
        levels.push(Level::Zero);
        let zero_level = LevelId(0);
        level_intern.insert(Level::Zero, zero_level);

        let mut env = Self {
            alloc,
            names: AVec::new_in(alloc),
            name_intern: ABTreeMap::new_in(alloc),
            levels,
            level_intern,
            level_lists: AVec::new_in(alloc),
            level_list_intern_1: ABTreeMap::new_in(alloc),
            level_list_intern_2: ABTreeMap::new_in(alloc),
            name_lists: AVec::new_in(alloc),
            rec_rules: AVec::new_in(alloc),
            exprs: AVec::new_in(alloc),
            expr_intern: ABTreeMap::new_in(alloc),
            decls: ABTreeMap::new_in(alloc),
            next_fvar: 0,
            next_def_height: 1,
            max_depth: DEFAULT_MAX_DEPTH,
            fuel_budget: DEFAULT_FUEL_BUDGET,
            zero_level,
        };
        // Reserve NameId(0) for the anonymous root name "".
        let _anon = env.intern_name("");
        env
    }

    /// Return the allocator backing this environment.
    #[inline]
    pub fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }

    /// Configure the maximum recursion depth guard.
    #[inline]
    pub fn set_max_depth(&mut self, max_depth: u32) {
        self.max_depth = max_depth;
    }

    /// Configure the per-operation reduction/type-checking fuel budget.
    #[inline]
    pub fn set_fuel_budget(&mut self, fuel_budget: u32) {
        self.fuel_budget = fuel_budget;
    }

    #[inline]
    fn budget(&self) -> Budget {
        Budget { fuel: self.fuel_budget, max_depth: self.max_depth }
    }

    // -------------------------------------------------------------------------
    // Name interning
    // -------------------------------------------------------------------------

    /// Intern a dotted name (e.g. `"Nat.add"`) and return its [`NameId`].
    pub fn intern_name(&mut self, name: &str) -> NameId {
        if let Some(&id) = self.name_intern.get(name) {
            return id;
        }
        let id = NameId(len32(self.names.len()).unwrap_or_else(|e| e.stop()));
        let owned = AString::from_str_in(name, self.alloc);
        let key = AString::from_str_in(name, self.alloc);
        self.names.push(owned);
        self.name_intern.insert(key, id);
        id
    }

    /// Look up an already-interned name without allocating.
    pub fn lookup_name(&self, name: &str) -> Option<NameId> {
        self.name_intern.get(name).copied()
    }

    /// Borrow the string representation of `id`.
    #[inline]
    pub fn name_str(&self, id: NameId) -> &str {
        self.names[idx(id.0)].as_str()
    }

    // -------------------------------------------------------------------------
    // Universe Levels
    // -------------------------------------------------------------------------

    /// Return `Level::Zero`.
    #[inline]
    pub fn level_zero(&self) -> LevelId {
        self.zero_level
    }

    /// Inspect the [`Level`] stored at `id`.
    #[inline]
    pub fn level(&self, id: LevelId) -> Level {
        self.levels[idx(id.0)]
    }

    fn intern_level(&mut self, lvl: Level) -> LevelId {
        if let Some(&id) = self.level_intern.get(&lvl) {
            return id;
        }
        let id = LevelId(len32(self.levels.len()).unwrap_or_else(|e| e.stop()));
        self.levels.push(lvl);
        self.level_intern.insert(lvl, id);
        id
    }

    /// Construct `Succ(u)`.
    pub fn mk_level_succ(&mut self, u: LevelId) -> LevelId {
        self.intern_level(Level::Succ(u))
    }

    /// Construct `Max(u, v)`.
    pub fn mk_level_max(&mut self, u: LevelId, v: LevelId) -> LevelId {
        self.intern_level(Level::Max(u, v))
    }

    /// Construct `IMax(u, v)`.
    pub fn mk_level_imax(&mut self, u: LevelId, v: LevelId) -> LevelId {
        self.intern_level(Level::IMax(u, v))
    }

    /// Construct `Param(name)`.
    pub fn mk_level_param(&mut self, name: NameId) -> LevelId {
        self.intern_level(Level::Param(name))
    }

    /// Construct concrete numeral universe `Succ^n(Zero)`.
    pub fn mk_level_nat(&mut self, n: u32) -> LevelId {
        let mut cur = self.zero_level;
        for _ in 0..n {
            cur = self.mk_level_succ(cur);
        }
        cur
    }

    /// Peel off outer `Succ` constructors: returns `(base, offset)` with `l == Succ^offset(base)`.
    fn level_to_offset(&self, mut l: LevelId) -> (LevelId, u32) {
        let mut off = 0u32;
        while let Level::Succ(inner) = self.level(l) {
            l = inner;
            off = off.saturating_add(1);
            if off >= self.max_depth.saturating_mul(4) {
                break;
            }
        }
        (l, off)
    }

    fn level_add_offset(&mut self, mut base: LevelId, offset: u32) -> LevelId {
        for _ in 0..offset {
            base = self.mk_level_succ(base);
        }
        base
    }

    /// True if `l` is guaranteed to evaluate to a strictly positive universe (`>= 1`) under
    /// every parameter assignment.
    fn is_level_nonzero(&self, l: LevelId, depth: u32) -> bool {
        if depth > self.max_depth {
            return false;
        }
        match self.level(l) {
            Level::Zero | Level::Param(_) => false,
            Level::Succ(_) => true,
            Level::Max(a, b) => {
                self.is_level_nonzero(a, depth + 1) || self.is_level_nonzero(b, depth + 1)
            }
            Level::IMax(_, b) => self.is_level_nonzero(b, depth + 1),
        }
    }

    /// Normalize a universe level expression into canonical simplified form.
    ///
    /// Implements:
    /// - `IMax(u, Zero) = Zero`
    /// - `IMax(u, Succ(v)) = Max(u, Succ(v))` (and generally when `v` is nonzero)
    /// - `IMax(Zero, v) = v`, `IMax(u, u) = u`
    /// - `Max(Zero, u) = u`, `Max(u, Zero) = u`, `Max(u, u) = u`
    /// - `Max(Succ(u), Succ(v)) = Succ(Max(u, v))`
    pub fn normalize_level(&mut self, l: LevelId) -> LevelId {
        self.normalize_level_rec(l, 0)
    }

    fn normalize_level_rec(&mut self, l: LevelId, depth: u32) -> LevelId {
        if depth > self.max_depth {
            return l;
        }
        match self.level(l) {
            Level::Zero | Level::Param(_) => l,
            Level::Succ(u) => {
                let u_norm = self.normalize_level_rec(u, depth + 1);
                self.mk_level_succ(u_norm)
            }
            Level::Max(u, v) => {
                let u_norm = self.normalize_level_rec(u, depth + 1);
                let v_norm = self.normalize_level_rec(v, depth + 1);
                self.simplify_max(u_norm, v_norm, depth + 1)
            }
            Level::IMax(u, v) => {
                let u_norm = self.normalize_level_rec(u, depth + 1);
                let v_norm = self.normalize_level_rec(v, depth + 1);
                self.simplify_imax(u_norm, v_norm, depth + 1)
            }
        }
    }

    fn simplify_max(&mut self, u: LevelId, v: LevelId, depth: u32) -> LevelId {
        if depth > self.max_depth {
            return self.mk_level_max(u, v);
        }
        if u == v {
            return u;
        }
        if u == self.zero_level {
            return v;
        }
        if v == self.zero_level {
            return u;
        }
        if let (Level::Succ(u1), Level::Succ(v1)) = (self.level(u), self.level(v)) {
            let inner = self.simplify_max(u1, v1, depth + 1);
            return self.mk_level_succ(inner);
        }
        // Absorb concrete vs offset when base matches (e.g. Max(u, Succ(u)) = Succ(u)).
        let (u_base, u_off) = self.level_to_offset(u);
        let (v_base, v_off) = self.level_to_offset(v);
        if u_base == v_base {
            return if u_off >= v_off { u } else { v };
        }
        if u_base == self.zero_level && u_off <= v_off {
            return v;
        }
        if v_base == self.zero_level && v_off <= u_off {
            return u;
        }
        // Canonical ordering so Max(u, v) and Max(v, u) share the same LevelId.
        if u <= v { self.mk_level_max(u, v) } else { self.mk_level_max(v, u) }
    }

    fn simplify_imax(&mut self, u: LevelId, v: LevelId, depth: u32) -> LevelId {
        if depth > self.max_depth {
            return self.mk_level_imax(u, v);
        }
        if v == self.zero_level {
            return self.zero_level;
        }
        if u == self.zero_level || u == v {
            return v;
        }
        if self.is_level_nonzero(v, 0) {
            return self.simplify_max(u, v, depth + 1);
        }
        if let Level::Max(v1, v2) = self.level(v) {
            let a = self.simplify_imax(u, v1, depth + 1);
            let b = self.simplify_imax(u, v2, depth + 1);
            return self.simplify_max(a, b, depth + 1);
        }
        self.mk_level_imax(u, v)
    }

    /// Instantiate universe parameters `params` with `args` inside `l` and normalize.
    pub fn subst_level(&mut self, l: LevelId, params: &[NameId], args: &[LevelId]) -> LevelId {
        if params.is_empty() {
            return self.normalize_level(l);
        }
        let substituted = self.subst_level_rec(l, params, args, 0);
        self.normalize_level(substituted)
    }

    fn subst_level_rec(
        &mut self,
        l: LevelId,
        params: &[NameId],
        args: &[LevelId],
        depth: u32,
    ) -> LevelId {
        if depth > self.max_depth {
            return l;
        }
        match self.level(l) {
            Level::Zero => l,
            Level::Param(p) => {
                for (i, &param) in params.iter().enumerate() {
                    if param == p && i < args.len() {
                        return args[i];
                    }
                }
                l
            }
            Level::Succ(u) => {
                let u2 = self.subst_level_rec(u, params, args, depth + 1);
                if u2 == u { l } else { self.mk_level_succ(u2) }
            }
            Level::Max(u, v) => {
                let u2 = self.subst_level_rec(u, params, args, depth + 1);
                let v2 = self.subst_level_rec(v, params, args, depth + 1);
                if u2 == u && v2 == v { l } else { self.mk_level_max(u2, v2) }
            }
            Level::IMax(u, v) => {
                let u2 = self.subst_level_rec(u, params, args, depth + 1);
                let v2 = self.subst_level_rec(v, params, args, depth + 1);
                if u2 == u && v2 == v { l } else { self.mk_level_imax(u2, v2) }
            }
        }
    }

    /// Find an arbitrary universe parameter appearing in the second argument of an `IMax`,
    /// used for sound case-splitting (`p = 0` vs `p = Succ p`) in [`KernelEnv::level_leq`].
    fn find_imax_param(&self, l: LevelId, depth: u32) -> Option<NameId> {
        if depth > self.max_depth {
            return None;
        }
        match self.level(l) {
            Level::Zero | Level::Param(_) => None,
            Level::Succ(u) => self.find_imax_param(u, depth + 1),
            Level::Max(u, v) => {
                self.find_imax_param(u, depth + 1).or_else(|| self.find_imax_param(v, depth + 1))
            }
            Level::IMax(u, v) => match self.level(v) {
                Level::Param(p) => Some(p),
                _ => self
                    .find_imax_param(v, depth + 1)
                    .or_else(|| self.find_imax_param(u, depth + 1)),
            },
        }
    }

    /// Sound universe inequality check: returns `true` only if `u <= v` holds for all universe
    /// parameter assignments.
    pub fn level_leq(&mut self, u: LevelId, v: LevelId) -> bool {
        let u_norm = self.normalize_level(u);
        let v_norm = self.normalize_level(v);
        self.level_leq_core(u_norm, v_norm, 0)
    }

    fn level_leq_core(&mut self, u: LevelId, v: LevelId, depth: u32) -> bool {
        if u == v || u == self.zero_level {
            return true;
        }
        if depth > 64 {
            return false;
        }

        // Case-split on any parameter controlling an `IMax(_, Param(p))` so that e.g.
        // `v <= IMax(u, v)` and `IMax(u, v) <= Max(u, v)` are decided soundly and completely.
        if let Some(p) = self.find_imax_param(u, 0).or_else(|| self.find_imax_param(v, 0)) {
            let zero = self.zero_level;
            let p_lvl = self.mk_level_param(p);
            let p_succ = self.mk_level_succ(p_lvl);

            let u0 = self.subst_level(u, &[p], &[zero]);
            let v0 = self.subst_level(v, &[p], &[zero]);
            if !self.level_leq_core(u0, v0, depth + 1) {
                return false;
            }
            let u1 = self.subst_level(u, &[p], &[p_succ]);
            let v1 = self.subst_level(v, &[p], &[p_succ]);
            return self.level_leq_core(u1, v1, depth + 1);
        }

        let (u_base, u_off) = self.level_to_offset(u);
        let (v_base, v_off) = self.level_to_offset(v);

        if u_base == v_base {
            return u_off <= v_off;
        }
        if u_base == self.zero_level && u_off <= v_off {
            return true;
        }

        // Distribute offsets into `Max` on the left: `Max(a, b) + k <= v` iff `a + k <= v && b + k <= v`.
        if let Level::Max(a, b) = self.level(u_base) {
            let a_off = self.level_add_offset(a, u_off);
            let b_off = self.level_add_offset(b, u_off);
            return self.level_leq_core(a_off, v, depth + 1)
                && self.level_leq_core(b_off, v, depth + 1);
        }

        // Distribute offsets into `Max` on the right: `u <= Max(a, b) + k` if `u <= a + k || u <= b + k`.
        if let Level::Max(a, b) = self.level(v_base) {
            let a_off = self.level_add_offset(a, v_off);
            let b_off = self.level_add_offset(b, v_off);
            return self.level_leq_core(u, a_off, depth + 1)
                || self.level_leq_core(u, b_off, depth + 1);
        }

        // Peel common `Succ` offsets.
        if u_off > 0 && v_off > 0 {
            let common = u_off.min(v_off);
            let u_rem = self.level_add_offset(u_base, u_off - common);
            let v_rem = self.level_add_offset(v_base, v_off - common);
            return self.level_leq_core(u_rem, v_rem, depth + 1);
        }

        // Sound fallback for residual `IMax` without bare `Param`: `IMax(a, b) <= Max(a, b)`.
        if let Level::IMax(a, b) = self.level(u_base) {
            let m = self.simplify_max(a, b, 0);
            let m_off = self.level_add_offset(m, u_off);
            return self.level_leq_core(m_off, v, depth + 1);
        }

        false
    }

    /// Sound universe equality check (`u <= v && v <= u`).
    pub fn level_eq(&mut self, u: LevelId, v: LevelId) -> bool {
        let u_norm = self.normalize_level(u);
        let v_norm = self.normalize_level(v);
        if u_norm == v_norm {
            return true;
        }
        self.level_leq_core(u_norm, v_norm, 0) && self.level_leq_core(v_norm, u_norm, 0)
    }

    // -------------------------------------------------------------------------
    // Core Expr Arena & Constructors
    // -------------------------------------------------------------------------

    /// Inspect the [`Expr`] node stored at `id`.
    #[inline]
    pub fn expr(&self, id: ExprId) -> Expr {
        self.exprs[idx(id.0)]
    }

    /// Return the universe level arguments slice referenced by a [`LevelRange`].
    #[inline]
    pub fn const_levels(&self, range: LevelRange) -> &[LevelId] {
        let start = idx(range.start);
        let end = start + idx(range.len);
        &self.level_lists[start..end]
    }

    #[inline]
    fn name_slice(&self, range: NameRange) -> &[NameId] {
        let start = idx(range.start);
        let end = start + idx(range.len);
        &self.name_lists[start..end]
    }

    #[inline]
    fn rule_slice(&self, range: RuleRange) -> &[RecRule] {
        let start = idx(range.start);
        let end = start + idx(range.len);
        &self.rec_rules[start..end]
    }

    fn push_level_range(&mut self, lvls: &[LevelId]) -> LevelRange {
        match lvls {
            [] => LevelRange { start: 0, len: 0 },
            &[l0] => {
                if let Some(&r) = self.level_list_intern_1.get(&l0) {
                    return r;
                }
                let start = len32(self.level_lists.len()).unwrap_or_else(|e| e.stop());
                self.level_lists.push(l0);
                let r = LevelRange { start, len: 1 };
                self.level_list_intern_1.insert(l0, r);
                r
            }
            &[l0, l1] => {
                if let Some(&r) = self.level_list_intern_2.get(&(l0, l1)) {
                    return r;
                }
                let start = len32(self.level_lists.len()).unwrap_or_else(|e| e.stop());
                self.level_lists.push(l0);
                self.level_lists.push(l1);
                let r = LevelRange { start, len: 2 };
                self.level_list_intern_2.insert((l0, l1), r);
                r
            }
            _ => {
                if let Some(pos) = self.level_lists.as_slice().windows(lvls.len()).position(|w| w == lvls) {
                    let start = len32(pos).unwrap_or_else(|e| e.stop());
                    let len = len32(lvls.len()).unwrap_or_else(|e| e.stop());
                    return LevelRange { start, len };
                }
                let start = len32(self.level_lists.len()).unwrap_or_else(|e| e.stop());
                for &l in lvls {
                    self.level_lists.push(l);
                }
                let len = len32(lvls.len()).unwrap_or_else(|e| e.stop());
                LevelRange { start, len }
            }
        }
    }

    fn push_name_range(&mut self, names: &[NameId]) -> NameRange {
        if names.is_empty() {
            return NameRange { start: 0, len: 0 };
        }
        let start = len32(self.name_lists.len()).unwrap_or_else(|e| e.stop());
        for &n in names {
            self.name_lists.push(n);
        }
        let len = len32(names.len()).unwrap_or_else(|e| e.stop());
        NameRange { start, len }
    }

    fn push_rule_range(&mut self, rules: &[RecRule]) -> RuleRange {
        if rules.is_empty() {
            return RuleRange { start: 0, len: 0 };
        }
        let start = len32(self.rec_rules.len()).unwrap_or_else(|e| e.stop());
        for &r in rules {
            self.rec_rules.push(r);
        }
        let len = len32(rules.len()).unwrap_or_else(|e| e.stop());
        RuleRange { start, len }
    }

    fn intern_expr(&mut self, e: Expr) -> ExprId {
        if let Some(&id) = self.expr_intern.get(&e) {
            return id;
        }
        let id = ExprId(len32(self.exprs.len()).unwrap_or_else(|e| e.stop()));
        self.exprs.push(e);
        self.expr_intern.insert(e, id);
        id
    }

    /// Construct `BVar(index)`.
    pub fn mk_bvar(&mut self, index: u32) -> ExprId {
        self.intern_expr(Expr::BVar(index))
    }

    /// Construct `FVar { id, ty }`.
    pub fn mk_fvar(&mut self, id: u32, ty: ExprId) -> ExprId {
        self.intern_expr(Expr::FVar { id, ty })
    }

    /// Allocate a fresh local free variable with type `ty`.
    pub fn mk_fresh_fvar(&mut self, ty: ExprId) -> (u32, ExprId) {
        let id = self.next_fvar;
        self.next_fvar = self.next_fvar.wrapping_add(1);
        let expr = self.mk_fvar(id, ty);
        (id, expr)
    }

    /// Construct `Sort(level)`.
    pub fn mk_sort(&mut self, level: LevelId) -> ExprId {
        self.intern_expr(Expr::Sort(level))
    }

    /// Construct `Prop = Sort 0`.
    pub fn mk_prop(&mut self) -> ExprId {
        let z = self.zero_level;
        self.mk_sort(z)
    }

    /// Construct `Type 0 = Sort 1`.
    pub fn mk_type0(&mut self) -> ExprId {
        let one = self.mk_level_succ(self.zero_level);
        self.mk_sort(one)
    }

    /// Construct `Const { name, levels }`.
    pub fn mk_const(&mut self, name: NameId, levels: &[LevelId]) -> ExprId {
        let range = self.push_level_range(levels);
        self.intern_expr(Expr::Const { name, levels: range })
    }

    /// Construct `App(f, a)`.
    pub fn mk_app(&mut self, f: ExprId, a: ExprId) -> ExprId {
        self.intern_expr(Expr::App(f, a))
    }

    /// Construct iterated application `(((f a_0) a_1) ... a_{n-1})`.
    pub fn mk_apps(&mut self, mut f: ExprId, args: &[ExprId]) -> ExprId {
        for &arg in args {
            f = self.mk_app(f, arg);
        }
        f
    }

    /// Construct `Lam { binder_ty, body }`.
    pub fn mk_lam(&mut self, binder_ty: ExprId, body: ExprId) -> ExprId {
        self.intern_expr(Expr::Lam { binder_ty, body })
    }

    /// Construct `ForallE { binder_ty, body }`.
    pub fn mk_forall(&mut self, binder_ty: ExprId, body: ExprId) -> ExprId {
        self.intern_expr(Expr::ForallE { binder_ty, body })
    }

    /// Construct a non-dependent arrow `dom → cod` (lifts loose `BVar`s in `cod` by `1`).
    pub fn mk_arrow(&mut self, dom: ExprId, cod: ExprId) -> Result<ExprId, KernelError> {
        let mut budget = self.budget();
        let lifted = self.subst_expr_rec(cod, SubstOp::Lift { amount: 1 }, 0, 0, &mut budget)?;
        Ok(self.mk_forall(dom, lifted))
    }

    /// Construct `LetE { ty, val, body }`.
    pub fn mk_let(&mut self, ty: ExprId, val: ExprId, body: ExprId) -> ExprId {
        self.intern_expr(Expr::LetE { ty, val, body })
    }

    /// Construct `Lit(Literal::Nat(n))`.
    pub fn mk_lit_nat(&mut self, n: u64) -> ExprId {
        self.intern_expr(Expr::Lit(Literal::Nat(n)))
    }

    /// Construct `Lit(Literal::Str(s))`.
    pub fn mk_lit_str(&mut self, s: NameId) -> ExprId {
        self.intern_expr(Expr::Lit(Literal::Str(s)))
    }

    /// Construct `Proj { struct_name, field_idx, expr }`.
    pub fn mk_proj(&mut self, struct_name: NameId, field_idx: u32, expr: ExprId) -> ExprId {
        self.intern_expr(Expr::Proj { struct_name, field_idx, expr })
    }

    // -------------------------------------------------------------------------
    // De Bruijn Substitution, Lifting, Abstraction & Level Instantiation
    // -------------------------------------------------------------------------

    /// Shared binder-depth-tracking traversal for [`SubstOp::Lift`], [`SubstOp::Instantiate`],
    /// and [`SubstOp::AbstractFVar`].
    fn subst_expr_rec(
        &mut self,
        expr: ExprId,
        op: SubstOp,
        bvar_depth: u32,
        depth: u32,
        budget: &mut Budget,
    ) -> Result<ExprId, KernelError> {
        if let SubstOp::Lift { amount: 0 } = op {
            return Ok(expr);
        }
        budget.tick(depth)?;
        match self.expr(expr) {
            Expr::BVar(i) => match op {
                SubstOp::Lift { amount } => {
                    if i >= bvar_depth {
                        Ok(self.mk_bvar(i.saturating_add(amount)))
                    } else {
                        Ok(expr)
                    }
                }
                SubstOp::Instantiate { arg } => {
                    if i == bvar_depth {
                        if bvar_depth == 0 {
                            Ok(arg)
                        } else {
                            self.subst_expr_rec(
                                arg,
                                SubstOp::Lift { amount: bvar_depth },
                                0,
                                depth + 1,
                                budget,
                            )
                        }
                    } else if i > bvar_depth {
                        Ok(self.mk_bvar(i - 1))
                    } else {
                        Ok(expr)
                    }
                }
                SubstOp::AbstractFVar { .. } => {
                    if i >= bvar_depth {
                        Ok(self.mk_bvar(i + 1))
                    } else {
                        Ok(expr)
                    }
                }
            },
            Expr::FVar { id, .. } => match op {
                SubstOp::AbstractFVar { target_fvar } if id == target_fvar => {
                    Ok(self.mk_bvar(bvar_depth))
                }
                _ => Ok(expr),
            },
            Expr::Sort(_) | Expr::Const { .. } | Expr::Lit(_) => Ok(expr),
            Expr::App(f, a) => {
                let f2 = self.subst_expr_rec(f, op, bvar_depth, depth + 1, budget)?;
                let a2 = self.subst_expr_rec(a, op, bvar_depth, depth + 1, budget)?;
                if f2 == f && a2 == a { Ok(expr) } else { Ok(self.mk_app(f2, a2)) }
            }
            Expr::Lam { binder_ty, body } => {
                let t2 = self.subst_expr_rec(binder_ty, op, bvar_depth, depth + 1, budget)?;
                let b2 = self.subst_expr_rec(body, op, bvar_depth + 1, depth + 1, budget)?;
                if t2 == binder_ty && b2 == body { Ok(expr) } else { Ok(self.mk_lam(t2, b2)) }
            }
            Expr::ForallE { binder_ty, body } => {
                let t2 = self.subst_expr_rec(binder_ty, op, bvar_depth, depth + 1, budget)?;
                let b2 = self.subst_expr_rec(body, op, bvar_depth + 1, depth + 1, budget)?;
                if t2 == binder_ty && b2 == body { Ok(expr) } else { Ok(self.mk_forall(t2, b2)) }
            }
            Expr::LetE { ty, val, body } => {
                let t2 = self.subst_expr_rec(ty, op, bvar_depth, depth + 1, budget)?;
                let v2 = self.subst_expr_rec(val, op, bvar_depth, depth + 1, budget)?;
                let b2 = self.subst_expr_rec(body, op, bvar_depth + 1, depth + 1, budget)?;
                if t2 == ty && v2 == val && b2 == body {
                    Ok(expr)
                } else {
                    Ok(self.mk_let(t2, v2, b2))
                }
            }
            Expr::Proj { struct_name, field_idx, expr: inner } => {
                let x2 = self.subst_expr_rec(inner, op, bvar_depth, depth + 1, budget)?;
                if x2 == inner { Ok(expr) } else { Ok(self.mk_proj(struct_name, field_idx, x2)) }
            }
        }
    }

    /// Substitute `BVar(0)` with `arg` in `expr`, shifting outer de Bruijn indices down by `1`.
    pub fn instantiate(&mut self, expr: ExprId, arg: ExprId) -> Result<ExprId, KernelError> {
        let mut budget = self.budget();
        self.instantiate_impl(expr, arg, 0, &mut budget)
    }

    #[inline]
    fn instantiate_impl(
        &mut self,
        expr: ExprId,
        arg: ExprId,
        depth: u32,
        budget: &mut Budget,
    ) -> Result<ExprId, KernelError> {
        self.subst_expr_rec(expr, SubstOp::Instantiate { arg }, 0, depth, budget)
    }

    /// Abstract free variable `target_fvar` into `BVar(0)` (shifting existing `BVar`s up).
    pub fn abstract_fvar(&mut self, expr: ExprId, target_fvar: u32) -> Result<ExprId, KernelError> {
        let mut budget = self.budget();
        self.subst_expr_rec(expr, SubstOp::AbstractFVar { target_fvar }, 0, 0, &mut budget)
    }

    /// Instantiate universe level parameters `params` with `args` throughout `expr`.
    pub fn instantiate_levels(
        &mut self,
        expr: ExprId,
        params: &[NameId],
        args: &[LevelId],
    ) -> Result<ExprId, KernelError> {
        if params.is_empty() {
            return Ok(expr);
        }
        let mut budget = self.budget();
        self.instantiate_levels_rec(expr, params, args, 0, &mut budget)
    }

    fn instantiate_levels_rec(
        &mut self,
        expr: ExprId,
        params: &[NameId],
        args: &[LevelId],
        depth: u32,
        budget: &mut Budget,
    ) -> Result<ExprId, KernelError> {
        budget.tick(depth)?;
        match self.expr(expr) {
            Expr::BVar(_) | Expr::Lit(_) => Ok(expr),
            Expr::FVar { id, ty } => {
                let ty2 = self.instantiate_levels_rec(ty, params, args, depth + 1, budget)?;
                if ty2 == ty { Ok(expr) } else { Ok(self.mk_fvar(id, ty2)) }
            }
            Expr::Sort(l) => {
                let l2 = self.subst_level(l, params, args);
                if l2 == l { Ok(expr) } else { Ok(self.mk_sort(l2)) }
            }
            Expr::Const { name, levels } => {
                if levels.len == 0 {
                    return Ok(expr);
                }
                let mut new_lvls = AVec::with_capacity_in(idx(levels.len), self.alloc);
                let mut changed = false;
                for i in 0..levels.len {
                    let old = self.level_lists[idx(levels.start) + idx(i)];
                    let new = self.subst_level(old, params, args);
                    if new != old {
                        changed = true;
                    }
                    new_lvls.push(new);
                }
                if !changed { Ok(expr) } else { Ok(self.mk_const(name, new_lvls.as_slice())) }
            }
            Expr::App(f, a) => {
                let f2 = self.instantiate_levels_rec(f, params, args, depth + 1, budget)?;
                let a2 = self.instantiate_levels_rec(a, params, args, depth + 1, budget)?;
                if f2 == f && a2 == a { Ok(expr) } else { Ok(self.mk_app(f2, a2)) }
            }
            Expr::Lam { binder_ty, body } => {
                let t2 = self.instantiate_levels_rec(binder_ty, params, args, depth + 1, budget)?;
                let b2 = self.instantiate_levels_rec(body, params, args, depth + 1, budget)?;
                if t2 == binder_ty && b2 == body { Ok(expr) } else { Ok(self.mk_lam(t2, b2)) }
            }
            Expr::ForallE { binder_ty, body } => {
                let t2 = self.instantiate_levels_rec(binder_ty, params, args, depth + 1, budget)?;
                let b2 = self.instantiate_levels_rec(body, params, args, depth + 1, budget)?;
                if t2 == binder_ty && b2 == body { Ok(expr) } else { Ok(self.mk_forall(t2, b2)) }
            }
            Expr::LetE { ty, val, body } => {
                let t2 = self.instantiate_levels_rec(ty, params, args, depth + 1, budget)?;
                let v2 = self.instantiate_levels_rec(val, params, args, depth + 1, budget)?;
                let b2 = self.instantiate_levels_rec(body, params, args, depth + 1, budget)?;
                if t2 == ty && v2 == val && b2 == body {
                    Ok(expr)
                } else {
                    Ok(self.mk_let(t2, v2, b2))
                }
            }
            Expr::Proj { struct_name, field_idx, expr: inner } => {
                let x2 = self.instantiate_levels_rec(inner, params, args, depth + 1, budget)?;
                if x2 == inner { Ok(expr) } else { Ok(self.mk_proj(struct_name, field_idx, x2)) }
            }
        }
    }

    fn instantiate_levels_from_ranges(
        &mut self,
        expr: ExprId,
        param_range: NameRange,
        arg_range: LevelRange,
        depth: u32,
        budget: &mut Budget,
    ) -> Result<ExprId, KernelError> {
        if param_range.len == 0 {
            return Ok(expr);
        }
        let mut params = AVec::with_capacity_in(idx(param_range.len), self.alloc);
        params.extend_from_slice(self.name_slice(param_range));
        let mut args = AVec::with_capacity_in(idx(arg_range.len), self.alloc);
        args.extend_from_slice(self.const_levels(arg_range));
        self.instantiate_levels_rec(expr, params.as_slice(), args.as_slice(), depth, budget)
    }

    /// Check that `expr` has no unbound de Bruijn variables and all universe parameters
    /// occurring in it are declared in `allowed_params`.
    fn validate_closed_and_levels(
        &self,
        expr: ExprId,
        allowed_params: &[NameId],
        bvar_depth: u32,
        depth: u32,
        budget: &mut Budget,
    ) -> Result<(), KernelError> {
        budget.tick(depth)?;
        match self.expr(expr) {
            Expr::BVar(i) => {
                if i >= bvar_depth {
                    return Err(KernelError::LooseBVar(i - bvar_depth));
                }
                Ok(())
            }
            Expr::FVar { ty, .. } => {
                self.validate_closed_and_levels(ty, allowed_params, 0, depth + 1, budget)
            }
            Expr::Sort(l) => self.validate_level_params(l, allowed_params, 0),
            Expr::Const { levels, .. } => {
                for &l in self.const_levels(levels) {
                    self.validate_level_params(l, allowed_params, 0)?;
                }
                Ok(())
            }
            Expr::App(f, a) => {
                self.validate_closed_and_levels(f, allowed_params, bvar_depth, depth + 1, budget)?;
                self.validate_closed_and_levels(a, allowed_params, bvar_depth, depth + 1, budget)
            }
            Expr::Lam { binder_ty, body } | Expr::ForallE { binder_ty, body } => {
                self.validate_closed_and_levels(
                    binder_ty,
                    allowed_params,
                    bvar_depth,
                    depth + 1,
                    budget,
                )?;
                self.validate_closed_and_levels(
                    body,
                    allowed_params,
                    bvar_depth + 1,
                    depth + 1,
                    budget,
                )
            }
            Expr::LetE { ty, val, body } => {
                self.validate_closed_and_levels(ty, allowed_params, bvar_depth, depth + 1, budget)?;
                self.validate_closed_and_levels(
                    val,
                    allowed_params,
                    bvar_depth,
                    depth + 1,
                    budget,
                )?;
                self.validate_closed_and_levels(
                    body,
                    allowed_params,
                    bvar_depth + 1,
                    depth + 1,
                    budget,
                )
            }
            Expr::Lit(_) => Ok(()),
            Expr::Proj { expr: inner, .. } => {
                self.validate_closed_and_levels(inner, allowed_params, bvar_depth, depth + 1, budget)
            }
        }
    }

    fn validate_level_params(
        &self,
        l: LevelId,
        allowed: &[NameId],
        depth: u32,
    ) -> Result<(), KernelError> {
        if depth > self.max_depth {
            return Err(KernelError::DepthExceeded);
        }
        match self.level(l) {
            Level::Zero => Ok(()),
            Level::Param(p) => {
                if allowed.contains(&p) {
                    Ok(())
                } else {
                    Err(KernelError::UnknownLevelParam(p))
                }
            }
            Level::Succ(u) => self.validate_level_params(u, allowed, depth + 1),
            Level::Max(u, v) | Level::IMax(u, v) => {
                self.validate_level_params(u, allowed, depth + 1)?;
                self.validate_level_params(v, allowed, depth + 1)
            }
        }
    }

    // -------------------------------------------------------------------------
    // WHNF Reduction (beta, zeta, delta, iota, projection)
    // -------------------------------------------------------------------------

    /// Unpack an application spine `(((head a_0) a_1) ... a_{n-1})` into `(head, [a_0, ..., a_{n-1}])`.
    fn unpack_app_spine(&self, mut e: ExprId) -> (ExprId, AVec<'a, ExprId>) {
        let mut rev_args = AVec::new_in(self.alloc);
        while let Expr::App(f, a) = self.expr(e) {
            rev_args.push(a);
            e = f;
        }
        rev_args.reverse();
        (e, rev_args)
    }

    /// Reduce `e` to Weak Head Normal Form (beta, zeta, delta, iota, projection).
    ///
    /// Opaque `Theorem` definitions are never unfolded.
    pub fn whnf(&mut self, e: ExprId) -> Result<ExprId, KernelError> {
        let mut budget = self.budget();
        self.whnf_impl(e, true, 0, &mut budget)
    }

    fn whnf_impl(
        &mut self,
        mut e: ExprId,
        unfold_delta: bool,
        depth: u32,
        budget: &mut Budget,
    ) -> Result<ExprId, KernelError> {
        loop {
            budget.tick(depth)?;
            let (head, args) = self.unpack_app_spine(e);
            match self.expr(head) {
                Expr::Lam { body, .. } if !args.is_empty() => {
                    // Beta-reduction.
                    let reduced = self.instantiate_impl(body, args[0], depth + 1, budget)?;
                    e = self.mk_apps(reduced, &args[1..]);
                }
                Expr::LetE { val, body, .. } => {
                    // Zeta-reduction.
                    let reduced = self.instantiate_impl(body, val, depth + 1, budget)?;
                    e = self.mk_apps(reduced, args.as_slice());
                }
                Expr::Proj { struct_name, field_idx, expr: target } => {
                    // Structure projection reduction.
                    let target_whnf = self.whnf_impl(target, true, depth + 1, budget)?;
                    let (t_head, t_args) = self.unpack_app_spine(target_whnf);
                    if let Expr::Const { name: ctor_name, .. } = self.expr(t_head)
                        && let Some(&StoredDecl::Ctor {
                            ind_name,
                            num_params,
                            num_fields,
                            ..
                        }) = self.decls.get(&ctor_name)
                        && ind_name == struct_name
                        && field_idx < num_fields
                    {
                        let arg_pos = idx(num_params) + idx(field_idx);
                        if arg_pos < t_args.len() {
                            e = self.mk_apps(t_args[arg_pos], args.as_slice());
                            continue;
                        }
                    }
                    return Ok(e);
                }
                Expr::Const { name, levels } => {
                    let Some(&decl) = self.decls.get(&name) else {
                        return Ok(e);
                    };
                    match decl {
                        StoredDecl::Def { level_params, value, is_theorem: false, .. }
                            if unfold_delta =>
                        {
                            // Delta-unfolding (only non-theorem defs).
                            if levels.len != level_params.len {
                                return Err(KernelError::LevelParamCountMismatch {
                                    name,
                                    expected: level_params.len,
                                    got: levels.len,
                                });
                            }
                            let unfolded = self.instantiate_levels_from_ranges(
                                value,
                                level_params,
                                levels,
                                depth + 1,
                                budget,
                            )?;
                            e = self.mk_apps(unfolded, args.as_slice());
                        }
                        StoredDecl::Recursor {
                            ind_name,
                            num_params,
                            num_indices,
                            num_motives,
                            num_minors,
                            level_params,
                            rules,
                            ..
                        } => {
                            // Iota-reduction: recursor applied to a constructor (or Nat literal).
                            let prefix_len = idx(num_params) + idx(num_motives) + idx(num_minors);
                            let major_idx = prefix_len + idx(num_indices);
                            if args.len() <= major_idx {
                                return Ok(e);
                            }
                            if levels.len != level_params.len {
                                return Err(KernelError::LevelParamCountMismatch {
                                    name,
                                    expected: level_params.len,
                                    got: levels.len,
                                });
                            }
                            let major_whnf =
                                self.whnf_impl(args[major_idx], true, depth + 1, budget)?;
                            if let Some(reduced) = self.try_reduce_recursor(
                                ind_name,
                                level_params,
                                levels,
                                rules,
                                prefix_len,
                                major_idx,
                                major_whnf,
                                args.as_slice(),
                                depth + 1,
                                budget,
                            )? {
                                e = reduced;
                                continue;
                            }
                            return Ok(e);
                        }
                        StoredDecl::Quot { .. } => {
                            // Quot.lift f h (Quot.mk r a) ~> f a
                            if self.name_str(name) == "Quot.lift" && args.len() >= 6 {
                                let major_whnf =
                                    self.whnf_impl(args[5], true, depth + 1, budget)?;
                                let (m_head, m_args) = self.unpack_app_spine(major_whnf);
                                if let Expr::Const { name: mk_name, .. } = self.expr(m_head)
                                    && self.name_str(mk_name) == "Quot.mk"
                                    && m_args.len() >= 3
                                {
                                    let f_a = self.mk_app(args[3], m_args[2]);
                                    e = self.mk_apps(f_a, &args[6..]);
                                    continue;
                                }
                            }
                            return Ok(e);
                        }
                        _ => return Ok(e),
                    }
                }
                _ => return Ok(e),
            }
        }
    }

    fn try_reduce_recursor(
        &mut self,
        ind_name: NameId,
        rec_level_params: NameRange,
        rec_levels: LevelRange,
        rules: RuleRange,
        prefix_len: usize,
        major_idx: usize,
        major_whnf: ExprId,
        args: &[ExprId],
        depth: u32,
        budget: &mut Budget,
    ) -> Result<Option<ExprId>, KernelError> {
        // Support Nat literals as virtual `Nat.zero` / `Nat.succ` constructors.
        if let Expr::Lit(Literal::Nat(n)) = self.expr(major_whnf)
            && self.name_str(ind_name) == "Nat"
        {
            let target_ctor = if n == 0 {
                self.lookup_name("Nat.zero")
            } else {
                self.lookup_name("Nat.succ")
            };
            if let Some(ctor_name) = target_ctor {
                let matched_rule =
                    self.rule_slice(rules).iter().copied().find(|r| r.ctor_name == ctor_name);
                if let Some(rule) = matched_rule {
                    let rhs_inst = self.instantiate_levels_from_ranges(
                        rule.rhs,
                        rec_level_params,
                        rec_levels,
                        depth + 1,
                        budget,
                    )?;
                    let mut res = self.mk_apps(rhs_inst, &args[..prefix_len]);
                    if n > 0 && rule.nfields == 1 {
                        let pred = self.mk_lit_nat(n - 1);
                        res = self.mk_app(res, pred);
                    }
                    res = self.mk_apps(res, &args[major_idx + 1..]);
                    return Ok(Some(res));
                }
            }
            return Ok(None);
        }

        let (m_head, m_args) = self.unpack_app_spine(major_whnf);
        let Expr::Const { name: ctor_name, .. } = self.expr(m_head) else {
            return Ok(None);
        };
        let Some(&StoredDecl::Ctor {
            ind_name: ctor_ind,
            num_params: ctor_num_params,
            num_fields: ctor_num_fields,
            ..
        }) = self.decls.get(&ctor_name)
        else {
            return Ok(None);
        };
        if ctor_ind != ind_name {
            return Ok(None);
        }
        let matched_rule =
            self.rule_slice(rules).iter().copied().find(|r| r.ctor_name == ctor_name);
        let Some(rule) = matched_rule else {
            return Ok(None);
        };
        if rule.nfields != ctor_num_fields {
            return Ok(None);
        }
        let field_start = idx(ctor_num_params);
        let field_end = field_start + idx(rule.nfields);
        if m_args.len() < field_end {
            return Ok(None);
        }

        let rhs_inst = self.instantiate_levels_from_ranges(
            rule.rhs,
            rec_level_params,
            rec_levels,
            depth + 1,
            budget,
        )?;
        let mut res = self.mk_apps(rhs_inst, &args[..prefix_len]);
        res = self.mk_apps(res, &m_args[field_start..field_end]);
        res = self.mk_apps(res, &args[major_idx + 1..]);
        Ok(Some(res))
    }

    // -------------------------------------------------------------------------
    // Definitional Equality (proof irrelevance, eta, lazy delta, Nat literals)
    // -------------------------------------------------------------------------

    /// Check whether `a` and `b` are definitionally equal in Lean 4's kernel.
    pub fn is_def_eq(&mut self, a: ExprId, b: ExprId) -> Result<bool, KernelError> {
        let mut budget = self.budget();
        self.is_def_eq_impl(a, b, 0, &mut budget)
    }

    fn is_def_eq_impl(
        &mut self,
        a: ExprId,
        b: ExprId,
        depth: u32,
        budget: &mut Budget,
    ) -> Result<bool, KernelError> {
        budget.tick(depth)?;
        if a == b {
            return Ok(true);
        }

        // Fast check for identical Sort levels before doing any inference.
        if let (Expr::Sort(u), Expr::Sort(v)) = (self.expr(a), self.expr(b)) {
            return Ok(self.level_eq(u, v));
        }

        // Proof irrelevance: if `a : P`, `b : Q`, `P : Prop`, and `P ≡ Q`, then `a ≡ b`.
        if self.is_def_eq_proof_irrel(a, b, depth + 1, budget)? {
            return Ok(true);
        }

        // Reduce both sides using non-delta WHNF first (beta, zeta, iota, proj).
        let a1 = self.whnf_impl(a, false, depth + 1, budget)?;
        let b1 = self.whnf_impl(b, false, depth + 1, budget)?;
        if a1 == b1 {
            return Ok(true);
        }

        // Direct structural match on core constructors.
        match (self.expr(a1), self.expr(b1)) {
            (Expr::Sort(u), Expr::Sort(v)) => return Ok(self.level_eq(u, v)),
            (Expr::BVar(i), Expr::BVar(j)) => return Ok(i == j),
            (Expr::FVar { id: i, .. }, Expr::FVar { id: j, .. }) => return Ok(i == j),
            (Expr::Lit(l1), Expr::Lit(l2)) => return Ok(l1 == l2),
            (
                Expr::Lam { binder_ty: t1, body: e1 },
                Expr::Lam { binder_ty: t2, body: e2 },
            )
            | (
                Expr::ForallE { binder_ty: t1, body: e1 },
                Expr::ForallE { binder_ty: t2, body: e2 },
            ) => {
                if !self.is_def_eq_impl(t1, t2, depth + 1, budget)? {
                    return Ok(false);
                }
                let (_, x) = self.mk_fresh_fvar(t1);
                let e1_x = self.instantiate_impl(e1, x, depth + 1, budget)?;
                let e2_x = self.instantiate_impl(e2, x, depth + 1, budget)?;
                return self.is_def_eq_impl(e1_x, e2_x, depth + 1, budget);
            }
            _ => {}
        }

        // Lazy delta-unfolding and spine comparison.
        if let Some(eq) = self.is_def_eq_delta_or_spine(a1, b1, depth + 1, budget)? {
            return Ok(eq);
        }

        // Full WHNF (including delta) on both sides for eta, Nat-literal expansion, and final compare.
        let a2 = self.whnf_impl(a1, true, depth + 1, budget)?;
        let b2 = self.whnf_impl(b1, true, depth + 1, budget)?;
        if a2 != a1 || b2 != b1 {
            return self.is_def_eq_impl(a2, b2, depth + 1, budget);
        }

        // Nat literal vs `Nat.zero` / `Nat.succ` constructor equivalence.
        if let Some(a_exp) = self.expand_nat_literal(a2) {
            return self.is_def_eq_impl(a_exp, b2, depth + 1, budget);
        }
        if let Some(b_exp) = self.expand_nat_literal(b2) {
            return self.is_def_eq_impl(a2, b_exp, depth + 1, budget);
        }

        // Eta-equivalence: `(fun x : A => body) ≡ f` iff `body ≡ f x` for fresh `x : A`.
        if let Expr::Lam { binder_ty, body } = self.expr(a2) {
            return self.is_def_eq_eta(binder_ty, body, b2, depth + 1, budget);
        }
        if let Expr::Lam { binder_ty, body } = self.expr(b2) {
            return self.is_def_eq_eta(binder_ty, body, a2, depth + 1, budget);
        }

        Ok(false)
    }

    fn is_def_eq_proof_irrel(
        &mut self,
        a: ExprId,
        b: ExprId,
        depth: u32,
        budget: &mut Budget,
    ) -> Result<bool, KernelError> {
        // Sorts and ForallE are types/propositions themselves (their type is `Sort _`, never a
        // proof of a `Prop`).
        match (self.expr(a), self.expr(b)) {
            (Expr::Sort(_), _)
            | (_, Expr::Sort(_))
            | (Expr::ForallE { .. }, _)
            | (_, Expr::ForallE { .. })
            | (Expr::BVar(_), _)
            | (_, Expr::BVar(_)) => return Ok(false),
            _ => {}
        }

        let Ok(ty_a) = self.infer_type_impl(a, true, depth + 1, budget) else {
            return Ok(false);
        };
        if !self.is_prop_type_impl(ty_a, depth + 1, budget)? {
            return Ok(false);
        }
        let Ok(ty_b) = self.infer_type_impl(b, true, depth + 1, budget) else {
            return Ok(false);
        };
        self.is_def_eq_impl(ty_a, ty_b, depth + 1, budget)
    }

    /// Check whether `ty` is a `Prop` (i.e. `ty : Sort 0`).
    fn is_prop_type_impl(
        &mut self,
        ty: ExprId,
        depth: u32,
        budget: &mut Budget,
    ) -> Result<bool, KernelError> {
        let Ok(sort_expr) = self.infer_type_impl(ty, true, depth + 1, budget) else {
            return Ok(false);
        };
        let sort_whnf = self.whnf_impl(sort_expr, true, depth + 1, budget)?;
        match self.expr(sort_whnf) {
            Expr::Sort(lvl) => Ok(self.level_eq(lvl, self.zero_level)),
            _ => Ok(false),
        }
    }

    fn is_def_eq_eta(
        &mut self,
        binder_ty: ExprId,
        body: ExprId,
        other: ExprId,
        depth: u32,
        budget: &mut Budget,
    ) -> Result<bool, KernelError> {
        // Verify `other` has a function type whose domain matches `binder_ty`.
        let Ok(other_ty) = self.infer_type_impl(other, true, depth + 1, budget) else {
            return Ok(false);
        };
        let other_ty_whnf = self.whnf_impl(other_ty, true, depth + 1, budget)?;
        let Expr::ForallE { binder_ty: dom, .. } = self.expr(other_ty_whnf) else {
            return Ok(false);
        };
        if !self.is_def_eq_impl(binder_ty, dom, depth + 1, budget)? {
            return Ok(false);
        }
        let (_, x) = self.mk_fresh_fvar(binder_ty);
        let lhs = self.instantiate_impl(body, x, depth + 1, budget)?;
        let rhs = self.mk_app(other, x);
        self.is_def_eq_impl(lhs, rhs, depth + 1, budget)
    }

    fn expand_nat_literal(&mut self, e: ExprId) -> Option<ExprId> {
        let Expr::Lit(Literal::Nat(n)) = self.expr(e) else {
            return None;
        };
        if n == 0 {
            let zero = self.lookup_name("Nat.zero")?;
            Some(self.mk_const(zero, &[]))
        } else {
            let succ = self.lookup_name("Nat.succ")?;
            let succ_c = self.mk_const(succ, &[]);
            let pred = self.mk_lit_nat(n - 1);
            Some(self.mk_app(succ_c, pred))
        }
    }

    fn def_height(&self, e: ExprId) -> Option<(NameId, LevelRange, NameRange, ExprId, u32)> {
        let Expr::Const { name, levels } = self.expr(e) else {
            return None;
        };
        match self.decls.get(&name) {
            Some(&StoredDecl::Def { level_params, value, is_theorem: false, height, .. }) => {
                Some((name, levels, level_params, value, height))
            }
            _ => None,
        }
    }

    fn is_def_eq_delta_or_spine(
        &mut self,
        a: ExprId,
        b: ExprId,
        depth: u32,
        budget: &mut Budget,
    ) -> Result<Option<bool>, KernelError> {
        let (h1, args1) = self.unpack_app_spine(a);
        let (h2, args2) = self.unpack_app_spine(b);

        let d1 = self.def_height(h1);
        let d2 = self.def_height(h2);

        // If heads are syntactically compatible and arities match, try structural arg comparison
        // before unfolding any definitions.
        let same_head = match (self.expr(h1), self.expr(h2)) {
            (
                Expr::Const { name: n1, levels: l1 },
                Expr::Const { name: n2, levels: l2 },
            ) if n1 == n2 && l1.len == l2.len => {
                let mut ok = true;
                for i in 0..l1.len {
                    let u = self.level_lists[idx(l1.start) + idx(i)];
                    let v = self.level_lists[idx(l2.start) + idx(i)];
                    if !self.level_eq(u, v) {
                        ok = false;
                        break;
                    }
                }
                ok
            }
            (Expr::FVar { id: i1, .. }, Expr::FVar { id: i2, .. }) => i1 == i2,
            (
                Expr::Proj { struct_name: s1, field_idx: f1, expr: e1 },
                Expr::Proj { struct_name: s2, field_idx: f2, expr: e2 },
            ) if s1 == s2 && f1 == f2 => self.is_def_eq_impl(e1, e2, depth + 1, budget)?,
            _ => false,
        };

        if same_head && args1.len() == args2.len() {
            let mut all_eq = true;
            for i in 0..args1.len() {
                if !self.is_def_eq_impl(args1[i], args2[i], depth + 1, budget)? {
                    all_eq = false;
                    break;
                }
            }
            if all_eq {
                return Ok(Some(true));
            }
        }

        // Lazy delta unfolding: unfold whichever definition has the greater definition height
        // (or both if they are the same definition whose arguments differed).
        match (d1, d2) {
            (Some((n1, l1, lp1, v1, ht1)), Some((n2, l2, lp2, v2, ht2))) => {
                if ht1 > ht2 {
                    let a_unf = self.unfold_const_app(n1, l1, lp1, v1, &args1, depth + 1, budget)?;
                    Ok(Some(self.is_def_eq_impl(a_unf, b, depth + 1, budget)?))
                } else if ht2 > ht1 {
                    let b_unf = self.unfold_const_app(n2, l2, lp2, v2, &args2, depth + 1, budget)?;
                    Ok(Some(self.is_def_eq_impl(a, b_unf, depth + 1, budget)?))
                } else {
                    let a_unf = self.unfold_const_app(n1, l1, lp1, v1, &args1, depth + 1, budget)?;
                    let b_unf = self.unfold_const_app(n2, l2, lp2, v2, &args2, depth + 1, budget)?;
                    Ok(Some(self.is_def_eq_impl(a_unf, b_unf, depth + 1, budget)?))
                }
            }
            (Some((n1, l1, lp1, v1, _)), None) => {
                let a_unf = self.unfold_const_app(n1, l1, lp1, v1, &args1, depth + 1, budget)?;
                Ok(Some(self.is_def_eq_impl(a_unf, b, depth + 1, budget)?))
            }
            (None, Some((n2, l2, lp2, v2, _))) => {
                let b_unf = self.unfold_const_app(n2, l2, lp2, v2, &args2, depth + 1, budget)?;
                Ok(Some(self.is_def_eq_impl(a, b_unf, depth + 1, budget)?))
            }
            (None, None) => Ok(None),
        }
    }

    fn unfold_const_app(
        &mut self,
        name: NameId,
        levels: LevelRange,
        level_params: NameRange,
        value: ExprId,
        args: &[ExprId],
        depth: u32,
        budget: &mut Budget,
    ) -> Result<ExprId, KernelError> {
        if levels.len != level_params.len {
            return Err(KernelError::LevelParamCountMismatch {
                name,
                expected: level_params.len,
                got: levels.len,
            });
        }
        let unfolded =
            self.instantiate_levels_from_ranges(value, level_params, levels, depth + 1, budget)?;
        Ok(self.mk_apps(unfolded, args))
    }

    // -------------------------------------------------------------------------
    // Bidirectional Type Checker
    // -------------------------------------------------------------------------

    /// Infer and fully check the type of `e`.
    pub fn infer_type(&mut self, e: ExprId) -> Result<ExprId, KernelError> {
        let mut budget = self.budget();
        self.infer_type_impl(e, false, 0, &mut budget)
    }

    /// Infer the type of `e` and reduce it to `Sort(u)`, returning normalized `u`.
    pub fn infer_sort(&mut self, e: ExprId) -> Result<LevelId, KernelError> {
        let mut budget = self.budget();
        self.infer_sort_impl(e, false, 0, &mut budget)
    }

    /// Infer the type of `e` and reduce it to a `Sort(u)`, returning `u`.
    fn infer_sort_impl(
        &mut self,
        e: ExprId,
        infer_only: bool,
        depth: u32,
        budget: &mut Budget,
    ) -> Result<LevelId, KernelError> {
        let ty = self.infer_type_impl(e, infer_only, depth + 1, budget)?;
        let ty_whnf = self.whnf_impl(ty, true, depth + 1, budget)?;
        match self.expr(ty_whnf) {
            Expr::Sort(lvl) => Ok(self.normalize_level(lvl)),
            _ => Err(KernelError::NotASort { expr: e, ty: ty_whnf }),
        }
    }

    fn infer_type_impl(
        &mut self,
        e: ExprId,
        infer_only: bool,
        depth: u32,
        budget: &mut Budget,
    ) -> Result<ExprId, KernelError> {
        budget.tick(depth)?;
        match self.expr(e) {
            Expr::BVar(i) => Err(KernelError::LooseBVar(i)),
            Expr::FVar { ty, .. } => Ok(ty),
            Expr::Sort(u) => {
                // Sort(u) : Sort(Succ u) — strictly stratified, prevents Girard's paradox.
                let u_norm = self.normalize_level(u);
                let succ = self.mk_level_succ(u_norm);
                Ok(self.mk_sort(succ))
            }
            Expr::Const { name, levels } => {
                let Some(&decl) = self.decls.get(&name) else {
                    return Err(KernelError::UnknownConst(name));
                };
                let level_params = decl.level_params();
                if levels.len != level_params.len {
                    return Err(KernelError::LevelParamCountMismatch {
                        name,
                        expected: level_params.len,
                        got: levels.len,
                    });
                }
                self.instantiate_levels_from_ranges(
                    decl.ty(),
                    level_params,
                    levels,
                    depth + 1,
                    budget,
                )
            }
            Expr::App(f, a) => {
                let f_ty = self.infer_type_impl(f, infer_only, depth + 1, budget)?;
                let f_ty_whnf = self.whnf_impl(f_ty, true, depth + 1, budget)?;
                let Expr::ForallE { binder_ty, body } = self.expr(f_ty_whnf) else {
                    return Err(KernelError::NotAFunction { expr: f, ty: f_ty_whnf });
                };
                if !infer_only {
                    let a_ty = self.infer_type_impl(a, false, depth + 1, budget)?;
                    if !self.is_def_eq_impl(a_ty, binder_ty, depth + 1, budget)? {
                        return Err(KernelError::TypeMismatch { expected: binder_ty, got: a_ty });
                    }
                }
                self.instantiate_impl(body, a, depth + 1, budget)
            }
            Expr::Lam { binder_ty, body } => {
                if !infer_only {
                    let _u = self.infer_sort_impl(binder_ty, false, depth + 1, budget)?;
                }
                let (x_id, x) = self.mk_fresh_fvar(binder_ty);
                let body_open = self.instantiate_impl(body, x, depth + 1, budget)?;
                let body_ty = self.infer_type_impl(body_open, infer_only, depth + 1, budget)?;
                let body_ty_abs = self.subst_expr_rec(
                    body_ty,
                    SubstOp::AbstractFVar { target_fvar: x_id },
                    0,
                    depth + 1,
                    budget,
                )?;
                Ok(self.mk_forall(binder_ty, body_ty_abs))
            }
            Expr::ForallE { binder_ty, body } => {
                let u = self.infer_sort_impl(binder_ty, infer_only, depth + 1, budget)?;
                let (_, x) = self.mk_fresh_fvar(binder_ty);
                let body_open = self.instantiate_impl(body, x, depth + 1, budget)?;
                let v = self.infer_sort_impl(body_open, infer_only, depth + 1, budget)?;
                // Impredicative Prop: Sort(IMax(u, v)) yields Prop whenever v == 0.
                let imax_raw = self.mk_level_imax(u, v);
                let imax = self.normalize_level(imax_raw);
                Ok(self.mk_sort(imax))
            }
            Expr::LetE { ty, val, body } => {
                if !infer_only {
                    let _u = self.infer_sort_impl(ty, false, depth + 1, budget)?;
                    let val_ty = self.infer_type_impl(val, false, depth + 1, budget)?;
                    if !self.is_def_eq_impl(val_ty, ty, depth + 1, budget)? {
                        return Err(KernelError::TypeMismatch { expected: ty, got: val_ty });
                    }
                }
                let body_inst = self.instantiate_impl(body, val, depth + 1, budget)?;
                self.infer_type_impl(body_inst, infer_only, depth + 1, budget)
            }
            Expr::Lit(Literal::Nat(_)) => {
                let nat_name = self
                    .lookup_name("Nat")
                    .ok_or_else(|| KernelError::UnknownConst(self.intern_name("Nat")))?;
                if !self.decls.contains_key(&nat_name) {
                    return Err(KernelError::UnknownConst(nat_name));
                }
                Ok(self.mk_const(nat_name, &[]))
            }
            Expr::Lit(Literal::Str(_)) => {
                let str_name = self
                    .lookup_name("String")
                    .ok_or_else(|| KernelError::UnknownConst(self.intern_name("String")))?;
                if !self.decls.contains_key(&str_name) {
                    return Err(KernelError::UnknownConst(str_name));
                }
                Ok(self.mk_const(str_name, &[]))
            }
            Expr::Proj { struct_name, field_idx, expr: inner } => {
                self.infer_proj_type(struct_name, field_idx, inner, infer_only, depth + 1, budget)
            }
        }
    }

    fn infer_proj_type(
        &mut self,
        struct_name: NameId,
        field_idx: u32,
        inner: ExprId,
        infer_only: bool,
        depth: u32,
        budget: &mut Budget,
    ) -> Result<ExprId, KernelError> {
        let inner_ty = self.infer_type_impl(inner, infer_only, depth + 1, budget)?;
        let inner_ty_whnf = self.whnf_impl(inner_ty, true, depth + 1, budget)?;
        let (ind_head, ind_args) = self.unpack_app_spine(inner_ty_whnf);

        let Expr::Const { name: ind_name, levels: ind_levels } = self.expr(ind_head) else {
            return Err(KernelError::InvalidProjection { struct_name, field_idx });
        };
        if ind_name != struct_name {
            return Err(KernelError::InvalidProjection { struct_name, field_idx });
        }

        let Some(&StoredDecl::Inductive { num_params, ctors, .. }) = self.decls.get(&struct_name)
        else {
            return Err(KernelError::InvalidProjection { struct_name, field_idx });
        };
        // Structure projections are valid only on single-constructor inductive types.
        if ctors.len != 1 {
            return Err(KernelError::InvalidProjection { struct_name, field_idx });
        }
        let ctor_name = self.name_lists[idx(ctors.start)];
        let Some(&StoredDecl::Ctor {
            num_fields,
            level_params: ctor_level_params,
            ty: ctor_ty_raw,
            ..
        }) = self.decls.get(&ctor_name)
        else {
            return Err(KernelError::InvalidProjection { struct_name, field_idx });
        };
        if field_idx >= num_fields || ind_args.len() < idx(num_params) {
            return Err(KernelError::InvalidProjection { struct_name, field_idx });
        }
        if ind_levels.len != ctor_level_params.len {
            return Err(KernelError::LevelParamCountMismatch {
                name: ctor_name,
                expected: ctor_level_params.len,
                got: ind_levels.len,
            });
        }

        let mut cur_ty = self.instantiate_levels_from_ranges(
            ctor_ty_raw,
            ctor_level_params,
            ind_levels,
            depth + 1,
            budget,
        )?;

        // Instantiate inductive parameters.
        for p in 0..idx(num_params) {
            let cur_whnf = self.whnf_impl(cur_ty, true, depth + 1, budget)?;
            let Expr::ForallE { body, .. } = self.expr(cur_whnf) else {
                return Err(KernelError::InvalidProjection { struct_name, field_idx });
            };
            cur_ty = self.instantiate_impl(body, ind_args[p], depth + 1, budget)?;
        }

        // Instantiate preceding dependent fields via `Proj(struct_name, i, inner)`.
        for i in 0..field_idx {
            let cur_whnf = self.whnf_impl(cur_ty, true, depth + 1, budget)?;
            let Expr::ForallE { body, .. } = self.expr(cur_whnf) else {
                return Err(KernelError::InvalidProjection { struct_name, field_idx });
            };
            let prev_proj = self.mk_proj(struct_name, i, inner);
            cur_ty = self.instantiate_impl(body, prev_proj, depth + 1, budget)?;
        }

        let cur_whnf = self.whnf_impl(cur_ty, true, depth + 1, budget)?;
        let Expr::ForallE { binder_ty: field_ty, .. } = self.expr(cur_whnf) else {
            return Err(KernelError::InvalidProjection { struct_name, field_idx });
        };

        // Soundness: prevent projecting non-`Prop` data out of a `Prop` structure (e.g. `Exists`).
        if !infer_only && self.is_prop_type_impl(inner_ty_whnf, depth + 1, budget)? {
            let field_is_prop = self.is_prop_type_impl(field_ty, depth + 1, budget)?;
            if !field_is_prop {
                return Err(KernelError::InvalidProjection { struct_name, field_idx });
            }
        }

        Ok(field_ty)
    }

    // -------------------------------------------------------------------------
    // Declaration Checking & Theorem Verification
    // -------------------------------------------------------------------------

    fn check_unique_level_params(params: &[NameId]) -> Result<(), KernelError> {
        for (i, &p) in params.iter().enumerate() {
            if params[..i].contains(&p) {
                return Err(KernelError::DuplicateLevelParam(p));
            }
        }
        Ok(())
    }

    /// Type-check `decl` and add it to the kernel environment upon success.
    pub fn check_decl(&mut self, decl: Decl<'a>) -> Result<(), KernelError> {
        let mut budget = self.budget();
        match decl {
            Decl::Axiom { name, level_params, ty } => {
                if self.decls.contains_key(&name) {
                    return Err(KernelError::DuplicateDecl(name));
                }
                Self::check_unique_level_params(level_params.as_slice())?;
                self.validate_closed_and_levels(ty, level_params.as_slice(), 0, 0, &mut budget)?;
                let _sort = self.infer_sort_impl(ty, false, 0, &mut budget)?;
                let lp_range = self.push_name_range(level_params.as_slice());
                self.decls.insert(name, StoredDecl::Axiom { level_params: lp_range, ty });
                Ok(())
            }
            Decl::Def { name, level_params, ty, value, is_theorem } => {
                if self.decls.contains_key(&name) {
                    return Err(KernelError::DuplicateDecl(name));
                }
                Self::check_unique_level_params(level_params.as_slice())?;
                self.validate_closed_and_levels(ty, level_params.as_slice(), 0, 0, &mut budget)?;
                self.validate_closed_and_levels(
                    value,
                    level_params.as_slice(),
                    0,
                    0,
                    &mut budget,
                )?;
                let ty_sort = self.infer_sort_impl(ty, false, 0, &mut budget)?;
                if is_theorem && !self.level_eq(ty_sort, self.zero_level) {
                    return Err(KernelError::NotAProp { name, sort: ty_sort });
                }
                let val_ty = self.infer_type_impl(value, false, 0, &mut budget)?;
                if !self.is_def_eq_impl(val_ty, ty, 0, &mut budget)? {
                    return Err(KernelError::TypeMismatch { expected: ty, got: val_ty });
                }
                let lp_range = self.push_name_range(level_params.as_slice());
                let height = self.next_def_height;
                self.next_def_height = self.next_def_height.saturating_add(1);
                self.decls.insert(
                    name,
                    StoredDecl::Def { level_params: lp_range, ty, value, is_theorem, height },
                );
                Ok(())
            }
            Decl::Inductive { name, level_params, num_params, ty, ctors } => {
                if self.decls.contains_key(&name) {
                    return Err(KernelError::DuplicateDecl(name));
                }
                Self::check_unique_level_params(level_params.as_slice())?;
                self.validate_closed_and_levels(ty, level_params.as_slice(), 0, 0, &mut budget)?;
                let _sort = self.infer_sort_impl(ty, false, 0, &mut budget)?;
                let lp_range = self.push_name_range(level_params.as_slice());
                let ctors_range = self.push_name_range(ctors.as_slice());
                self.decls.insert(
                    name,
                    StoredDecl::Inductive {
                        level_params: lp_range,
                        num_params,
                        ty,
                        ctors: ctors_range,
                    },
                );
                Ok(())
            }
            Decl::Ctor { name, ind_name, ctor_idx, num_params, num_fields, level_params, ty } => {
                if self.decls.contains_key(&name) {
                    return Err(KernelError::DuplicateDecl(name));
                }
                if !matches!(self.decls.get(&ind_name), Some(StoredDecl::Inductive { .. })) {
                    return Err(KernelError::InvalidInductive(ind_name));
                }
                Self::check_unique_level_params(level_params.as_slice())?;
                self.validate_closed_and_levels(ty, level_params.as_slice(), 0, 0, &mut budget)?;
                let _sort = self.infer_sort_impl(ty, false, 0, &mut budget)?;
                let lp_range = self.push_name_range(level_params.as_slice());
                self.decls.insert(
                    name,
                    StoredDecl::Ctor {
                        ind_name,
                        ctor_idx,
                        num_params,
                        num_fields,
                        level_params: lp_range,
                        ty,
                    },
                );
                Ok(())
            }
            Decl::Recursor {
                name,
                ind_name,
                num_params,
                num_indices,
                num_motives,
                num_minors,
                level_params,
                ty,
                rules,
            } => {
                if self.decls.contains_key(&name) {
                    return Err(KernelError::DuplicateDecl(name));
                }
                if !matches!(self.decls.get(&ind_name), Some(StoredDecl::Inductive { .. })) {
                    return Err(KernelError::InvalidInductive(ind_name));
                }
                Self::check_unique_level_params(level_params.as_slice())?;
                self.validate_closed_and_levels(ty, level_params.as_slice(), 0, 0, &mut budget)?;
                let _sort = self.infer_sort_impl(ty, false, 0, &mut budget)?;

                // Insert the recursor before checking rule RHS terms because recursive rules
                // (e.g. `Nat.rec` for `Nat.succ`) reference the recursor itself.
                let lp_range = self.push_name_range(level_params.as_slice());
                let rules_range = self.push_rule_range(rules.as_slice());
                self.decls.insert(
                    name,
                    StoredDecl::Recursor {
                        ind_name,
                        num_params,
                        num_indices,
                        num_motives,
                        num_minors,
                        level_params: lp_range,
                        ty,
                        rules: rules_range,
                    },
                );

                for &rule in rules.as_slice() {
                    if let Err(err) = self
                        .validate_closed_and_levels(
                            rule.rhs,
                            level_params.as_slice(),
                            0,
                            0,
                            &mut budget,
                        )
                        .and_then(|()| self.infer_type_impl(rule.rhs, false, 0, &mut budget).map(|_| ()))
                    {
                        self.decls.remove(&name);
                        return Err(err);
                    }
                }
                Ok(())
            }
            Decl::Quot { name, level_params, ty } => {
                if self.decls.contains_key(&name) {
                    return Err(KernelError::DuplicateDecl(name));
                }
                Self::check_unique_level_params(level_params.as_slice())?;
                self.validate_closed_and_levels(ty, level_params.as_slice(), 0, 0, &mut budget)?;
                let _sort = self.infer_sort_impl(ty, false, 0, &mut budget)?;
                let lp_range = self.push_name_range(level_params.as_slice());
                self.decls.insert(name, StoredDecl::Quot { level_params: lp_range, ty });
                Ok(())
            }
        }
    }

    /// Return `true` if a declaration named `name` exists in the environment.
    #[inline]
    pub fn contains_decl(&self, name: NameId) -> bool {
        self.decls.contains_key(&name)
    }

    /// Return the 0-based constructor index if `name` is a declared inductive constructor.
    #[inline]
    pub fn ctor_idx(&self, name: NameId) -> Option<u32> {
        match self.decls.get(&name) {
            Some(StoredDecl::Ctor { ctor_idx, .. }) => Some(*ctor_idx),
            _ => None,
        }
    }

    /// Return the read-only [`DeclKind`] of `name`, if declared.
    #[inline]
    pub fn decl_kind(&self, name: NameId) -> Option<DeclKind> {
        match *self.decls.get(&name)? {
            StoredDecl::Axiom { .. } => Some(DeclKind::Axiom),
            StoredDecl::Def { is_theorem, .. } => Some(DeclKind::Def { is_theorem }),
            StoredDecl::Inductive { num_params, ctors, .. } => {
                Some(DeclKind::Inductive { num_params, num_ctors: ctors.len })
            }
            StoredDecl::Ctor { ind_name, ctor_idx, num_params, num_fields, .. } => {
                Some(DeclKind::Ctor { ind_name, ctor_idx, num_params, num_fields })
            }
            StoredDecl::Recursor {
                ind_name,
                num_params,
                num_indices,
                num_motives,
                num_minors,
                ..
            } => Some(DeclKind::Recursor {
                ind_name,
                num_params,
                num_indices,
                num_motives,
                num_minors,
            }),
            StoredDecl::Quot { .. } => Some(DeclKind::Quot),
        }
    }

    /// Return the declared type of `name`, if present.
    #[inline]
    pub fn decl_ty(&self, name: NameId) -> Option<ExprId> {
        self.decls.get(&name).map(|d| d.ty())
    }

    /// Return the universe level parameter names of `name`.
    #[inline]
    pub fn decl_level_params(&self, name: NameId) -> &[NameId] {
        match self.decls.get(&name) {
            Some(d) => {
                let r = d.level_params();
                let start = idx(r.start);
                let len = idx(r.len);
                &self.name_lists.as_slice()[start..start + len]
            }
            None => &[],
        }
    }

    /// Return the constructor names of inductive `name`, if `name` is an inductive declaration.
    #[inline]
    pub fn inductive_ctors(&self, name: NameId) -> Option<&[NameId]> {
        match self.decls.get(&name) {
            Some(StoredDecl::Inductive { ctors, .. }) => {
                let start = idx(ctors.start);
                let len = idx(ctors.len);
                Some(&self.name_lists.as_slice()[start..start + len])
            }
            _ => None,
        }
    }

    /// Return the body of a transparent (non-theorem) definition `name`, if present.
    #[inline]
    pub fn def_value(&self, name: NameId) -> Option<ExprId> {
        match self.decls.get(&name) {
            Some(StoredDecl::Def { value, is_theorem: false, .. }) => Some(*value),
            _ => None,
        }
    }

    /// Return all declaration names in deterministic [`NameId`] order.
    pub fn decl_names(&self) -> AVec<'a, NameId> {
        let mut out = AVec::new_in(self.alloc);
        for (&name, _) in self.decls.iter() {
            out.push(name);
        }
        out
    }

    /// Verify a theorem in the environment:
    /// 1. Confirms `name` is a checked `Def`/`Theorem` with a proof body.
    /// 2. Confirms its type inhabits `Prop` (`Sort 0`).
    /// 3. Confirms `infer_type(proof) ≡ ty`.
    /// 4. Collects all transitively referenced `Axiom`s and rejects any axiom not in
    ///    `allowed_axioms`.
    pub fn verify_theorem(
        &mut self,
        name: NameId,
        allowed_axioms: &[&str],
    ) -> Result<VerifiedTheorem<'a>, KernelError> {
        let Some(&decl) = self.decls.get(&name) else {
            return Err(KernelError::UnknownConst(name));
        };
        let StoredDecl::Def { ty, value, .. } = decl else {
            return Err(KernelError::NotATheorem(name));
        };

        let mut budget = self.budget();
        let ty_sort = self.infer_sort_impl(ty, false, 0, &mut budget)?;
        if !self.level_eq(ty_sort, self.zero_level) {
            return Err(KernelError::NotAProp { name, sort: ty_sort });
        }
        let val_ty = self.infer_type_impl(value, false, 0, &mut budget)?;
        if !self.is_def_eq_impl(val_ty, ty, 0, &mut budget)? {
            return Err(KernelError::TypeMismatch { expected: ty, got: val_ty });
        }

        // Iteratively collect all transitively referenced declarations.
        let mut visited: ABTreeSet<'a, NameId> = ABTreeSet::new_in(self.alloc);
        let mut worklist: AVec<'a, NameId> = AVec::new_in(self.alloc);
        let mut used_axioms: AVec<'a, NameId> = AVec::new_in(self.alloc);
        let mut expr_stack: AVec<'a, ExprId> = AVec::new_in(self.alloc);

        visited.insert(name);
        self.collect_expr_consts(ty, &mut expr_stack, &mut worklist, &mut visited);
        self.collect_expr_consts(value, &mut expr_stack, &mut worklist, &mut visited);

        while let Some(dep) = worklist.pop() {
            let Some(&dep_decl) = self.decls.get(&dep) else {
                return Err(KernelError::UnknownConst(dep));
            };
            match dep_decl {
                StoredDecl::Axiom { ty: ax_ty, .. } => {
                    let ax_name = self.name_str(dep);
                    if !allowed_axioms.contains(&ax_name) {
                        return Err(KernelError::DisallowedAxiom { theorem: name, axiom: dep });
                    }
                    used_axioms.push(dep);
                    self.collect_expr_consts(ax_ty, &mut expr_stack, &mut worklist, &mut visited);
                }
                StoredDecl::Def { ty: d_ty, value: d_val, .. } => {
                    self.collect_expr_consts(d_ty, &mut expr_stack, &mut worklist, &mut visited);
                    self.collect_expr_consts(d_val, &mut expr_stack, &mut worklist, &mut visited);
                }
                StoredDecl::Inductive { ty: i_ty, ctors, .. } => {
                    self.collect_expr_consts(i_ty, &mut expr_stack, &mut worklist, &mut visited);
                    for i in 0..ctors.len {
                        let c = self.name_lists[idx(ctors.start) + idx(i)];
                        if visited.insert(c) {
                            worklist.push(c);
                        }
                    }
                }
                StoredDecl::Ctor { ind_name, ty: c_ty, .. } => {
                    if visited.insert(ind_name) {
                        worklist.push(ind_name);
                    }
                    self.collect_expr_consts(c_ty, &mut expr_stack, &mut worklist, &mut visited);
                }
                StoredDecl::Recursor { ind_name, ty: r_ty, rules, .. } => {
                    if visited.insert(ind_name) {
                        worklist.push(ind_name);
                    }
                    self.collect_expr_consts(r_ty, &mut expr_stack, &mut worklist, &mut visited);
                    for i in 0..rules.len {
                        let rhs = self.rec_rules[idx(rules.start) + idx(i)].rhs;
                        self.collect_expr_consts(rhs, &mut expr_stack, &mut worklist, &mut visited);
                    }
                }
                StoredDecl::Quot { ty: q_ty, .. } => {
                    self.collect_expr_consts(q_ty, &mut expr_stack, &mut worklist, &mut visited);
                }
            }
        }

        Ok(VerifiedTheorem { name, used_axioms })
    }

    /// Iterative traversal of `root` to enqueue all referenced declaration [`NameId`]s.
    fn collect_expr_consts(
        &self,
        root: ExprId,
        stack: &mut AVec<'a, ExprId>,
        worklist: &mut AVec<'a, NameId>,
        visited: &mut ABTreeSet<'a, NameId>,
    ) {
        let mut seen_exprs: ABTreeSet<'a, ExprId> = ABTreeSet::new_in(self.alloc);
        stack.clear();
        stack.push(root);
        while let Some(cur) = stack.pop() {
            if !seen_exprs.insert(cur) {
                continue;
            }
            match self.expr(cur) {
                Expr::BVar(_) | Expr::Sort(_) | Expr::Lit(_) => {}
                Expr::FVar { ty, .. } => stack.push(ty),
                Expr::Const { name, .. } => {
                    if visited.insert(name) {
                        worklist.push(name);
                    }
                }
                Expr::App(f, a) => {
                    stack.push(f);
                    stack.push(a);
                }
                Expr::Lam { binder_ty, body } | Expr::ForallE { binder_ty, body } => {
                    stack.push(binder_ty);
                    stack.push(body);
                }
                Expr::LetE { ty, val, body } => {
                    stack.push(ty);
                    stack.push(val);
                    stack.push(body);
                }
                Expr::Proj { struct_name, expr: inner, .. } => {
                    if visited.insert(struct_name) {
                        worklist.push(struct_name);
                    }
                    stack.push(inner);
                }
            }
        }
    }

    // -------------------------------------------------------------------------
    // Lean 4 `.export` Certificate Loader
    // -------------------------------------------------------------------------

    /// Parse and type-check a Lean 4 line-based `.export` kernel certificate.
    ///
    /// Supported tags:
    /// - Names: `#NS`, `#NI`
    /// - Universe levels: `#US`, `#UM`, `#UIM`, `#UP`
    /// - Expressions: `#EV`, `#ES`, `#EC`, `#EA`, `#EL`, `#EP`, `#EZ`, `#EJ`, `#ELN`, `#ELS`
    /// - Declarations: `#AX`, `#DEF`, `#THM`, `#QUOT`
    pub fn load_export(&mut self, src: &str) -> Result<ExportSummary, KernelError> {
        let mut exp_names: ABTreeMap<'a, u32, NameId> = ABTreeMap::new_in(self.alloc);
        let mut exp_levels: ABTreeMap<'a, u32, LevelId> = ABTreeMap::new_in(self.alloc);
        let mut exp_exprs: ABTreeMap<'a, u32, ExprId> = ABTreeMap::new_in(self.alloc);

        // Lean 4 export tables pre-seed index 0 as anonymous name "" and Level::Zero.
        exp_names.insert(0, NameId(0));
        exp_levels.insert(0, self.zero_level);

        let mut summary = ExportSummary { names: 0, levels: 0, exprs: 0, decls: 0 };

        for (line_idx, raw_line) in src.lines().enumerate() {
            let line_no = len32(line_idx + 1).unwrap_or(u32::MAX);
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with("--") || line.starts_with("//") {
                continue;
            }
            // Skip optional version header line such as `0.1.0` or `3.0.0`.
            if !line.contains(' ')
                && line.bytes().all(|b| b.is_ascii_digit() || b == b'.')
                && line.contains('.')
            {
                continue;
            }

            let mut tokens = line.split_ascii_whitespace();
            let Some(first) = tokens.next() else {
                continue;
            };

            if first.starts_with('#') {
                // Declaration line: `#AX`, `#DEF`, `#THM`, `#QUOT`.
                match first {
                    "#AX" => {
                        let nidx = parse_u32_tok(&mut tokens, line_no)?;
                        let eidx = parse_u32_tok(&mut tokens, line_no)?;
                        let name = lookup_export_name(&exp_names, nidx, line_no)?;
                        let ty = lookup_export_expr(&exp_exprs, eidx, line_no)?;
                        let level_params =
                            parse_export_name_list(&mut tokens, &exp_names, line_no, self.alloc)?;
                        self.check_decl(Decl::Axiom { name, level_params, ty })?;
                        summary.decls += 1;
                    }
                    "#DEF" => {
                        let nidx = parse_u32_tok(&mut tokens, line_no)?;
                        let ty_idx = parse_u32_tok(&mut tokens, line_no)?;
                        let val_idx = parse_u32_tok(&mut tokens, line_no)?;
                        let name = lookup_export_name(&exp_names, nidx, line_no)?;
                        let ty = lookup_export_expr(&exp_exprs, ty_idx, line_no)?;
                        let value = lookup_export_expr(&exp_exprs, val_idx, line_no)?;

                        // Optional lean4export reducibility hint (`O`, `A`, or `R <height>`).
                        let mut level_params = AVec::new_in(self.alloc);
                        if let Some(next_tok) = tokens.next() {
                            match next_tok {
                                "O" | "A" => {}
                                "R" => {
                                    let _height = parse_u32_tok(&mut tokens, line_no)?;
                                }
                                other => {
                                    let pidx = parse_u32_str(other, line_no)?;
                                    level_params.push(lookup_export_name(
                                        &exp_names, pidx, line_no,
                                    )?);
                                }
                            }
                            for tok in tokens {
                                let pidx = parse_u32_str(tok, line_no)?;
                                level_params.push(lookup_export_name(&exp_names, pidx, line_no)?);
                            }
                        }

                        self.check_decl(Decl::Def {
                            name,
                            level_params,
                            ty,
                            value,
                            is_theorem: false,
                        })?;
                        summary.decls += 1;
                    }
                    "#THM" => {
                        let nidx = parse_u32_tok(&mut tokens, line_no)?;
                        let ty_idx = parse_u32_tok(&mut tokens, line_no)?;
                        let val_idx = parse_u32_tok(&mut tokens, line_no)?;
                        let name = lookup_export_name(&exp_names, nidx, line_no)?;
                        let ty = lookup_export_expr(&exp_exprs, ty_idx, line_no)?;
                        let value = lookup_export_expr(&exp_exprs, val_idx, line_no)?;
                        let level_params =
                            parse_export_name_list(&mut tokens, &exp_names, line_no, self.alloc)?;
                        self.check_decl(Decl::Def {
                            name,
                            level_params,
                            ty,
                            value,
                            is_theorem: true,
                        })?;
                        summary.decls += 1;
                    }
                    "#QUOT" => {
                        let nidx = parse_u32_tok(&mut tokens, line_no)?;
                        let ty_idx = parse_u32_tok(&mut tokens, line_no)?;
                        let name = lookup_export_name(&exp_names, nidx, line_no)?;
                        let ty = lookup_export_expr(&exp_exprs, ty_idx, line_no)?;
                        let level_params =
                            parse_export_name_list(&mut tokens, &exp_names, line_no, self.alloc)?;
                        self.check_decl(Decl::Quot { name, level_params, ty })?;
                        summary.decls += 1;
                    }
                    _ => {
                        return Err(KernelError::ExportParseError {
                            line: line_no,
                            kind: ExportParseKind::UnknownTag,
                        });
                    }
                }
            } else {
                // Indexed item line: `<id> <TAG> <args...>`.
                let id = parse_u32_str(first, line_no)?;
                let tag = tokens.next().ok_or(KernelError::ExportParseError {
                    line: line_no,
                    kind: ExportParseKind::MissingToken,
                })?;

                match tag {
                    "#NS" => {
                        let parent_idx = parse_u32_tok(&mut tokens, line_no)?;
                        let seg = tokens.next().ok_or(KernelError::ExportParseError {
                            line: line_no,
                            kind: ExportParseKind::MissingToken,
                        })?;
                        let parent_id = lookup_export_name(&exp_names, parent_idx, line_no)?;
                        let full = self.join_name_str(parent_id, seg);
                        exp_names.insert(id, full);
                        summary.names += 1;
                    }
                    "#NI" => {
                        let parent_idx = parse_u32_tok(&mut tokens, line_no)?;
                        let num = parse_u64_tok(&mut tokens, line_no)?;
                        let parent_id = lookup_export_name(&exp_names, parent_idx, line_no)?;
                        let full = self.join_name_num(parent_id, num);
                        exp_names.insert(id, full);
                        summary.names += 1;
                    }
                    "#US" => {
                        let uidx = parse_u32_tok(&mut tokens, line_no)?;
                        let u = lookup_export_level(&exp_levels, uidx, line_no)?;
                        let lvl = self.mk_level_succ(u);
                        exp_levels.insert(id, lvl);
                        summary.levels += 1;
                    }
                    "#UM" => {
                        let u1_idx = parse_u32_tok(&mut tokens, line_no)?;
                        let u2_idx = parse_u32_tok(&mut tokens, line_no)?;
                        let u1 = lookup_export_level(&exp_levels, u1_idx, line_no)?;
                        let u2 = lookup_export_level(&exp_levels, u2_idx, line_no)?;
                        let lvl = self.mk_level_max(u1, u2);
                        exp_levels.insert(id, lvl);
                        summary.levels += 1;
                    }
                    "#UIM" => {
                        let u1_idx = parse_u32_tok(&mut tokens, line_no)?;
                        let u2_idx = parse_u32_tok(&mut tokens, line_no)?;
                        let u1 = lookup_export_level(&exp_levels, u1_idx, line_no)?;
                        let u2 = lookup_export_level(&exp_levels, u2_idx, line_no)?;
                        let lvl = self.mk_level_imax(u1, u2);
                        exp_levels.insert(id, lvl);
                        summary.levels += 1;
                    }
                    "#UP" => {
                        let nidx = parse_u32_tok(&mut tokens, line_no)?;
                        let name = lookup_export_name(&exp_names, nidx, line_no)?;
                        let lvl = self.mk_level_param(name);
                        exp_levels.insert(id, lvl);
                        summary.levels += 1;
                    }
                    "#EV" => {
                        let bvar = parse_u32_tok(&mut tokens, line_no)?;
                        let expr = self.mk_bvar(bvar);
                        exp_exprs.insert(id, expr);
                        summary.exprs += 1;
                    }
                    "#ES" => {
                        let uidx = parse_u32_tok(&mut tokens, line_no)?;
                        let lvl = lookup_export_level(&exp_levels, uidx, line_no)?;
                        let expr = self.mk_sort(lvl);
                        exp_exprs.insert(id, expr);
                        summary.exprs += 1;
                    }
                    "#EC" => {
                        let nidx = parse_u32_tok(&mut tokens, line_no)?;
                        let name = lookup_export_name(&exp_names, nidx, line_no)?;
                        let mut lvls = AVec::new_in(self.alloc);
                        for tok in tokens {
                            let uidx = parse_u32_str(tok, line_no)?;
                            lvls.push(lookup_export_level(&exp_levels, uidx, line_no)?);
                        }
                        let expr = self.mk_const(name, lvls.as_slice());
                        exp_exprs.insert(id, expr);
                        summary.exprs += 1;
                    }
                    "#EA" => {
                        let f_idx = parse_u32_tok(&mut tokens, line_no)?;
                        let a_idx = parse_u32_tok(&mut tokens, line_no)?;
                        let f = lookup_export_expr(&exp_exprs, f_idx, line_no)?;
                        let a = lookup_export_expr(&exp_exprs, a_idx, line_no)?;
                        let expr = self.mk_app(f, a);
                        exp_exprs.insert(id, expr);
                        summary.exprs += 1;
                    }
                    "#EL" | "#EP" => {
                        let bi = tokens.next().ok_or(KernelError::ExportParseError {
                            line: line_no,
                            kind: ExportParseKind::MissingToken,
                        })?;
                        if !matches!(bi, "#BD" | "#BI" | "#BS" | "#BC") {
                            return Err(KernelError::ExportParseError {
                                line: line_no,
                                kind: ExportParseKind::InvalidBinderInfo,
                            });
                        }
                        let binder_nidx = parse_u32_tok(&mut tokens, line_no)?;
                        let _binder_name = lookup_export_name(&exp_names, binder_nidx, line_no)?;
                        let ty_idx = parse_u32_tok(&mut tokens, line_no)?;
                        let body_idx = parse_u32_tok(&mut tokens, line_no)?;
                        let binder_ty = lookup_export_expr(&exp_exprs, ty_idx, line_no)?;
                        let body = lookup_export_expr(&exp_exprs, body_idx, line_no)?;
                        let expr = if tag == "#EL" {
                            self.mk_lam(binder_ty, body)
                        } else {
                            self.mk_forall(binder_ty, body)
                        };
                        exp_exprs.insert(id, expr);
                        summary.exprs += 1;
                    }
                    "#EZ" => {
                        let binder_nidx = parse_u32_tok(&mut tokens, line_no)?;
                        let _binder_name = lookup_export_name(&exp_names, binder_nidx, line_no)?;
                        let ty_idx = parse_u32_tok(&mut tokens, line_no)?;
                        let val_idx = parse_u32_tok(&mut tokens, line_no)?;
                        let body_idx = parse_u32_tok(&mut tokens, line_no)?;
                        let ty = lookup_export_expr(&exp_exprs, ty_idx, line_no)?;
                        let val = lookup_export_expr(&exp_exprs, val_idx, line_no)?;
                        let body = lookup_export_expr(&exp_exprs, body_idx, line_no)?;
                        let expr = self.mk_let(ty, val, body);
                        exp_exprs.insert(id, expr);
                        summary.exprs += 1;
                    }
                    "#EJ" => {
                        let struct_nidx = parse_u32_tok(&mut tokens, line_no)?;
                        let field_idx = parse_u32_tok(&mut tokens, line_no)?;
                        let inner_idx = parse_u32_tok(&mut tokens, line_no)?;
                        let struct_name = lookup_export_name(&exp_names, struct_nidx, line_no)?;
                        let inner = lookup_export_expr(&exp_exprs, inner_idx, line_no)?;
                        let expr = self.mk_proj(struct_name, field_idx, inner);
                        exp_exprs.insert(id, expr);
                        summary.exprs += 1;
                    }
                    "#ELN" => {
                        let n = parse_u64_tok(&mut tokens, line_no)?;
                        let expr = self.mk_lit_nat(n);
                        exp_exprs.insert(id, expr);
                        summary.exprs += 1;
                    }
                    "#ELS" => {
                        let mut bytes = AVec::new_in(self.alloc);
                        for hex_tok in tokens {
                            let b = u8::from_str_radix(hex_tok, 16).map_err(|_| {
                                KernelError::ExportParseError {
                                    line: line_no,
                                    kind: ExportParseKind::InvalidHexString,
                                }
                            })?;
                            bytes.push(b);
                        }
                        let text = core::str::from_utf8(bytes.as_slice()).map_err(|_| {
                            KernelError::ExportParseError {
                                line: line_no,
                                kind: ExportParseKind::InvalidHexString,
                            }
                        })?;
                        let s_id = self.intern_name(text);
                        let expr = self.mk_lit_str(s_id);
                        exp_exprs.insert(id, expr);
                        summary.exprs += 1;
                    }
                    _ => {
                        return Err(KernelError::ExportParseError {
                            line: line_no,
                            kind: ExportParseKind::UnknownTag,
                        });
                    }
                }
            }
        }

        Ok(summary)
    }

    fn join_name_str(&mut self, parent: NameId, seg: &str) -> NameId {
        if parent.0 == 0 {
            return self.intern_name(seg);
        }
        let parent_str = self.name_str(parent);
        let mut buf = AString::with_capacity_in(parent_str.len() + 1 + seg.len(), self.alloc);
        buf.push_str(parent_str);
        buf.push('.');
        buf.push_str(seg);
        self.intern_name(buf.as_str())
    }

    fn join_name_num(&mut self, parent: NameId, mut num: u64) -> NameId {
        let mut digits = [0u8; 20];
        let mut pos = digits.len();
        if num == 0 {
            pos -= 1;
            digits[pos] = b'0';
        } else {
            while num > 0 {
                pos -= 1;
                let rem: u8 = (num % 10).narrow_or_stop();
                digits[pos] = b'0' + rem;
                num /= 10;
            }
        }
        let num_str = core::str::from_utf8(&digits[pos..]).unwrap_or("0");
        self.join_name_str(parent, num_str)
    }
}

fn parse_u32_str(s: &str, line: u32) -> Result<u32, KernelError> {
    s.parse::<u32>()
        .map_err(|_| KernelError::ExportParseError { line, kind: ExportParseKind::InvalidInteger })
}

fn parse_u32_tok<'s>(
    tokens: &mut impl Iterator<Item = &'s str>,
    line: u32,
) -> Result<u32, KernelError> {
    let tok = tokens.next().ok_or(KernelError::ExportParseError {
        line,
        kind: ExportParseKind::MissingToken,
    })?;
    parse_u32_str(tok, line)
}

fn parse_u64_tok<'s>(
    tokens: &mut impl Iterator<Item = &'s str>,
    line: u32,
) -> Result<u64, KernelError> {
    let tok = tokens.next().ok_or(KernelError::ExportParseError {
        line,
        kind: ExportParseKind::MissingToken,
    })?;
    tok.parse::<u64>()
        .map_err(|_| KernelError::ExportParseError { line, kind: ExportParseKind::InvalidInteger })
}

fn lookup_export_name<'a>(
    map: &ABTreeMap<'a, u32, NameId>,
    id: u32,
    line: u32,
) -> Result<NameId, KernelError> {
    map.get(&id).copied().ok_or(KernelError::ExportParseError {
        line,
        kind: ExportParseKind::UnknownNameIndex(id),
    })
}

fn lookup_export_level<'a>(
    map: &ABTreeMap<'a, u32, LevelId>,
    id: u32,
    line: u32,
) -> Result<LevelId, KernelError> {
    map.get(&id).copied().ok_or(KernelError::ExportParseError {
        line,
        kind: ExportParseKind::UnknownLevelIndex(id),
    })
}

fn lookup_export_expr<'a>(
    map: &ABTreeMap<'a, u32, ExprId>,
    id: u32,
    line: u32,
) -> Result<ExprId, KernelError> {
    map.get(&id).copied().ok_or(KernelError::ExportParseError {
        line,
        kind: ExportParseKind::UnknownExprIndex(id),
    })
}

fn parse_export_name_list<'a, 's>(
    tokens: &mut impl Iterator<Item = &'s str>,
    map: &ABTreeMap<'a, u32, NameId>,
    line: u32,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, NameId>, KernelError> {
    let mut out = AVec::new_in(alloc);
    for tok in tokens {
        let idx_val = parse_u32_str(tok, line)?;
        out.push(lookup_export_name(map, idx_val, line)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alloc() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    fn avec<T>(items: &[T]) -> AVec<'static, T>
    where
        T: Copy,
    {
        let mut out = AVec::with_capacity_in(items.len(), alloc());
        out.extend_from_slice(items);
        out
    }

    /// Populate `Eq`, `Eq.refl`, `Eq.rec`, `True`, `True.intro`, `False`, `False.rec`.
    fn add_core_logic(env: &mut KernelEnv<'static>) {
        let u_name = env.intern_name("u");
        let v_name = env.intern_name("v");
        let u_lvl = env.mk_level_param(u_name);
        let v_lvl = env.mk_level_param(v_name);
        let prop = env.mk_prop();
        let sort_u = env.mk_sort(u_lvl);
        let sort_v = env.mk_sort(v_lvl);

        // True : Prop, True.intro : True
        let true_n = env.intern_name("True");
        let true_intro_n = env.intern_name("True.intro");
        let true_c = env.mk_const(true_n, &[]);
        env.check_decl(Decl::Inductive {
            name: true_n,
            level_params: avec(&[]),
            num_params: 0,
            ty: prop,
            ctors: avec(&[true_intro_n]),
        })
        .unwrap();
        env.check_decl(Decl::Ctor {
            name: true_intro_n,
            ind_name: true_n,
            ctor_idx: 0,
            num_params: 0,
            num_fields: 0,
            level_params: avec(&[]),
            ty: true_c,
        })
        .unwrap();

        // False : Prop, False.rec.{u} : ∀ (C : Sort u), False → C
        let false_n = env.intern_name("False");
        let false_rec_n = env.intern_name("False.rec");
        let false_c = env.mk_const(false_n, &[]);
        env.check_decl(Decl::Inductive {
            name: false_n,
            level_params: avec(&[]),
            num_params: 0,
            ty: prop,
            ctors: avec(&[]),
        })
        .unwrap();
        // ∀ (motive : False → Sort u) (t : False), motive t
        let false_motive_ty = env.mk_arrow(false_c, sort_u).unwrap();
        let b0 = env.mk_bvar(0);
        let b1 = env.mk_bvar(1);
        let m_t = env.mk_app(b1, b0);
        let t_to_mt = env.mk_forall(false_c, m_t);
        let false_rec_ty = env.mk_forall(false_motive_ty, t_to_mt);
        env.check_decl(Decl::Recursor {
            name: false_rec_n,
            ind_name: false_n,
            num_params: 0,
            num_indices: 0,
            num_motives: 1,
            num_minors: 0,
            level_params: avec(&[u_name]),
            ty: false_rec_ty,
            rules: avec(&[]),
        })
        .unwrap();

        // Eq.{u} : ∀ (α : Sort u), α → α → Prop
        let eq_n = env.intern_name("Eq");
        let eq_refl_n = env.intern_name("Eq.refl");
        let eq_rec_n = env.intern_name("Eq.rec");

        let b0 = env.mk_bvar(0);
        let b1 = env.mk_bvar(1);
        let eq_ty_inner = env.mk_forall(b1, prop);
        let eq_ty_mid = env.mk_forall(b0, eq_ty_inner);
        let eq_ty = env.mk_forall(sort_u, eq_ty_mid);
        env.check_decl(Decl::Inductive {
            name: eq_n,
            level_params: avec(&[u_name]),
            num_params: 2,
            ty: eq_ty,
            ctors: avec(&[eq_refl_n]),
        })
        .unwrap();

        // Eq.refl.{u} : ∀ (α : Sort u) (a : α), Eq.{u} α a a
        let eq_u = env.mk_const(eq_n, &[u_lvl]);
        let eq_a_a = env.mk_apps(eq_u, &[b1, b0, b0]);
        let eq_refl_mid = env.mk_forall(b0, eq_a_a);
        let eq_refl_ty = env.mk_forall(sort_u, eq_refl_mid);
        env.check_decl(Decl::Ctor {
            name: eq_refl_n,
            ind_name: eq_n,
            ctor_idx: 0,
            num_params: 2,
            num_fields: 0,
            level_params: avec(&[u_name]),
            ty: eq_refl_ty,
        })
        .unwrap();

        // Eq.rec.{v, u} :
        //   ∀ (α : Sort u) (a : α)
        //     (motive : ∀ (x : α), Eq.{u} α a x → Sort v)
        //     (minor : motive a (Eq.refl.{u} α a))
        //     (b : α) (h : Eq.{u} α a b), motive b h
        // Under (α=BVar 5, a=BVar 4, motive=BVar 3, minor=BVar 2, b=BVar 1, h=BVar 0):
        let eq_refl_u = env.mk_const(eq_refl_n, &[u_lvl]);
        let b0 = env.mk_bvar(0);
        let b1 = env.mk_bvar(1);
        let b2 = env.mk_bvar(2);
        let b3 = env.mk_bvar(3);
        let b4 = env.mk_bvar(4);
        let b5 = env.mk_bvar(5);

        // motive : ∀ (x : α [b1]), Eq α [b2] a [b1] x [b0] → Sort v
        let eq_a_x = env.mk_apps(eq_u, &[b2, b1, b0]);
        let motive_ty_inner = env.mk_forall(eq_a_x, sort_v);
        let motive_ty = env.mk_forall(b1, motive_ty_inner);

        // minor : under (α=b2, a=b1, motive=b0) -> motive a (Eq.refl α a)
        let refl_a = env.mk_apps(eq_refl_u, &[b2, b1]);
        let minor_ty = env.mk_apps(b0, &[b1, refl_a]);

        // h : under (α=b4, a=b3, motive=b2, minor=b1, b=b0) -> Eq α a b
        let h_ty = env.mk_apps(eq_u, &[b4, b3, b0]);
        // body : under (..., h=b0) -> motive [b3] b [b1] h [b0]
        let ret_ty = env.mk_apps(b3, &[b1, b0]);

        let t5 = env.mk_forall(h_ty, ret_ty);
        let t4 = env.mk_forall(b3, t5); // b : α (α=b3)
        let t3 = env.mk_forall(minor_ty, t4);
        let t2 = env.mk_forall(motive_ty, t3);
        let t1 = env.mk_forall(b0, t2); // a : α (α=b0)
        let eq_rec_ty = env.mk_forall(sort_u, t1);

        // Rule RHS for Eq.refl: λ α a motive minor => minor (BVar 0)
        let rhs_l3 = env.mk_lam(minor_ty, b0);
        let rhs_l2 = env.mk_lam(motive_ty, rhs_l3);
        let rhs_l1 = env.mk_lam(b0, rhs_l2);
        let rhs = env.mk_lam(sort_u, rhs_l1);
        let _ = b5;

        env.check_decl(Decl::Recursor {
            name: eq_rec_n,
            ind_name: eq_n,
            num_params: 2,
            num_indices: 1,
            num_motives: 1,
            num_minors: 1,
            level_params: avec(&[v_name, u_name]),
            ty: eq_rec_ty,
            rules: avec(&[RecRule { ctor_name: eq_refl_n, nfields: 0, rhs }]),
        })
        .unwrap();
    }

    /// Populate `Nat`, `Nat.zero`, `Nat.succ`, `Nat.rec`, and `Nat.add`.
    fn add_nat(env: &mut KernelEnv<'static>) {
        let u_name = env.intern_name("u");
        let u_lvl = env.mk_level_param(u_name);
        let sort_u = env.mk_sort(u_lvl);
        let type0 = env.mk_type0();

        let nat_n = env.intern_name("Nat");
        let zero_n = env.intern_name("Nat.zero");
        let succ_n = env.intern_name("Nat.succ");
        let rec_n = env.intern_name("Nat.rec");
        let add_n = env.intern_name("Nat.add");

        let nat_c = env.mk_const(nat_n, &[]);
        let zero_c = env.mk_const(zero_n, &[]);
        let succ_c = env.mk_const(succ_n, &[]);

        env.check_decl(Decl::Inductive {
            name: nat_n,
            level_params: avec(&[]),
            num_params: 0,
            ty: type0,
            ctors: avec(&[zero_n, succ_n]),
        })
        .unwrap();

        env.check_decl(Decl::Ctor {
            name: zero_n,
            ind_name: nat_n,
            ctor_idx: 0,
            num_params: 0,
            num_fields: 0,
            level_params: avec(&[]),
            ty: nat_c,
        })
        .unwrap();

        let succ_ty = env.mk_arrow(nat_c, nat_c).unwrap();
        env.check_decl(Decl::Ctor {
            name: succ_n,
            ind_name: nat_n,
            ctor_idx: 1,
            num_params: 0,
            num_fields: 1,
            level_params: avec(&[]),
            ty: succ_ty,
        })
        .unwrap();

        // Nat.rec.{u} :
        //   ∀ (motive : Nat → Sort u)
        //     (zero : motive Nat.zero)
        //     (succ : ∀ (n : Nat), motive n → motive (Nat.succ n))
        //     (t : Nat), motive t
        let b0 = env.mk_bvar(0);
        let b1 = env.mk_bvar(1);
        let b2 = env.mk_bvar(2);
        let b3 = env.mk_bvar(3);

        let motive_ty = env.mk_arrow(nat_c, sort_u).unwrap();
        let zero_case_ty = env.mk_app(b0, zero_c); // under (motive=b0)

        // Under (motive=b1, zero=b0): ∀ (n : Nat), motive [b2] n [b0] → motive [b3] (succ n [b1])
        let m_n = env.mk_app(b2, b0);
        let succ_n_var = env.mk_app(succ_c, b1);
        let m_succ_n = env.mk_app(b3, succ_n_var);
        let ih_to_step = env.mk_forall(m_n, m_succ_n);
        let succ_case_ty = env.mk_forall(nat_c, ih_to_step);

        // Under (motive=b2, zero=b1, succ=b0): ∀ (t : Nat), motive [b3] t [b0]
        let m_t = env.mk_app(b3, b0);
        let t_to_mt = env.mk_forall(nat_c, m_t);

        let r2 = env.mk_forall(succ_case_ty, t_to_mt);
        let r1 = env.mk_forall(zero_case_ty, r2);
        let nat_rec_ty = env.mk_forall(motive_ty, r1);

        // Rule for Nat.zero: λ motive z s => z (BVar 1)
        let z_rhs_2 = env.mk_lam(succ_case_ty, b1);
        let z_rhs_1 = env.mk_lam(zero_case_ty, z_rhs_2);
        let z_rhs = env.mk_lam(motive_ty, z_rhs_1);

        // Rule for Nat.succ: λ motive z s n => s n (Nat.rec.{u} motive z s n)
        // Under (motive=b3, z=b2, s=b1, n=b0):
        let rec_u = env.mk_const(rec_n, &[u_lvl]);
        let ih = env.mk_apps(rec_u, &[b3, b2, b1, b0]);
        let s_app = env.mk_apps(b1, &[b0, ih]);
        let s_rhs_3 = env.mk_lam(nat_c, s_app);
        let s_rhs_2 = env.mk_lam(succ_case_ty, s_rhs_3);
        let s_rhs_1 = env.mk_lam(zero_case_ty, s_rhs_2);
        let s_rhs = env.mk_lam(motive_ty, s_rhs_1);

        env.check_decl(Decl::Recursor {
            name: rec_n,
            ind_name: nat_n,
            num_params: 0,
            num_indices: 0,
            num_motives: 1,
            num_minors: 2,
            level_params: avec(&[u_name]),
            ty: nat_rec_ty,
            rules: avec(&[
                RecRule { ctor_name: zero_n, nfields: 0, rhs: z_rhs },
                RecRule { ctor_name: succ_n, nfields: 1, rhs: s_rhs },
            ]),
        })
        .unwrap();

        // Nat.add : Nat → Nat → Nat :=
        //   λ m n => Nat.rec.{1} (λ _ : Nat => Nat) m (λ _ ih => Nat.succ ih) n
        let one_lvl = env.mk_level_nat(1);
        let rec_1 = env.mk_const(rec_n, &[one_lvl]);
        let add_ty_inner = env.mk_arrow(nat_c, nat_c).unwrap();
        let add_ty = env.mk_arrow(nat_c, add_ty_inner).unwrap();

        let const_nat_motive = env.mk_lam(nat_c, nat_c);
        // step := λ (_ : Nat) (ih : Nat) => Nat.succ ih
        let succ_b0 = env.mk_app(succ_c, b0);
        let step_inner = env.mk_lam(nat_c, succ_b0);
        let step_lam = env.mk_lam(nat_c, step_inner);

        // Under (m=b1, n=b0): Nat.rec.{1} (λ _ => Nat) m step n
        let add_body = env.mk_apps(rec_1, &[const_nat_motive, b1, step_lam, b0]);
        let add_lam_n = env.mk_lam(nat_c, add_body);
        let add_val = env.mk_lam(nat_c, add_lam_n);

        env.check_decl(Decl::Def {
            name: add_n,
            level_params: avec(&[]),
            ty: add_ty,
            value: add_val,
            is_theorem: false,
        })
        .unwrap();
    }

    fn mk_nat_church(env: &mut KernelEnv<'static>, n: u32) -> ExprId {
        let zero_n = env.lookup_name("Nat.zero").unwrap();
        let succ_n = env.lookup_name("Nat.succ").unwrap();
        let mut cur = env.mk_const(zero_n, &[]);
        let succ_c = env.mk_const(succ_n, &[]);
        for _ in 0..n {
            cur = env.mk_app(succ_c, cur);
        }
        cur
    }

    #[test]
    fn universe_normalization_and_girard_paradox_rejected() {
        let mut env = KernelEnv::new_in(alloc());
        let z = env.level_zero();
        let u_name = env.intern_name("u");
        let v_name = env.intern_name("v");
        let u = env.mk_level_param(u_name);
        let v = env.mk_level_param(v_name);
        let su = env.mk_level_succ(u);
        let sv = env.mk_level_succ(v);

        // IMax(u, Zero) = Zero
        let imax_u_0 = env.mk_level_imax(u, z);
        assert_eq!(env.normalize_level(imax_u_0), z);

        // IMax(u, Succ(v)) = Max(u, Succ(v))
        let imax_u_sv = env.mk_level_imax(u, sv);
        let max_u_sv = env.mk_level_max(u, sv);
        assert!(env.level_eq(imax_u_sv, max_u_sv));

        // Max(Zero, u) = u, Max(u, u) = u
        let max_0_u = env.mk_level_max(z, u);
        assert_eq!(env.normalize_level(max_0_u), u);
        let max_u_u = env.mk_level_max(u, u);
        assert_eq!(env.normalize_level(max_u_u), u);

        // Max(Succ(u), Succ(v)) = Succ(Max(u, v))
        let max_su_sv = env.mk_level_max(su, sv);
        let max_uv = env.mk_level_max(u, v);
        let s_max_uv = env.mk_level_succ(max_uv);
        assert!(env.level_eq(max_su_sv, s_max_uv));

        // Ordering: u <= Succ(u), u <= Max(u, v), !(Succ(u) <= u), !(u <= IMax(u, v))
        let imax_uv = env.mk_level_imax(u, v);
        assert!(env.level_leq(u, su));
        assert!(!env.level_leq(su, u));
        assert!(env.level_leq(u, max_uv));
        assert!(env.level_leq(v, imax_uv));
        assert!(!env.level_leq(u, imax_uv));

        // Sort 0 : Sort 1
        let prop = env.mk_prop();
        let type0 = env.mk_type0();
        let prop_ty = env.infer_type(prop).unwrap();
        assert!(env.is_def_eq(prop_ty, type0).unwrap());

        // Reject Girard's paradox: `def bad : Type 0 := Type 0`
        let bad_n = env.intern_name("girard_bad");
        let err = env
            .check_decl(Decl::Def {
                name: bad_n,
                level_params: avec(&[]),
                ty: type0,
                value: type0,
                is_theorem: false,
            })
            .unwrap_err();
        assert!(matches!(err, KernelError::TypeMismatch { .. }));
    }

    #[test]
    fn polymorphic_id_prop_modus_ponens_transitivity_and_eta() {
        let mut env = KernelEnv::new_in(alloc());
        let u_name = env.intern_name("u");
        let u = env.mk_level_param(u_name);
        let su = env.mk_level_succ(u);
        let type_u = env.mk_sort(su);
        let prop = env.mk_prop();
        let b0 = env.mk_bvar(0);
        let b1 = env.mk_bvar(1);
        let b2 = env.mk_bvar(2);

        // id.{u} : ∀ (α : Type u), α → α := λ α x => x
        let id_n = env.intern_name("id");
        let a_to_a = env.mk_forall(b0, b1);
        let id_ty = env.mk_forall(type_u, a_to_a);
        let id_inner = env.mk_lam(b0, b0);
        let id_val = env.mk_lam(type_u, id_inner);
        env.check_decl(Decl::Def {
            name: id_n,
            level_params: avec(&[u_name]),
            ty: id_ty,
            value: id_val,
            is_theorem: false,
        })
        .unwrap();

        // Check `id.{0} Prop P ≡ P` for a free `P : Prop`.
        let (_, p_var) = env.mk_fresh_fvar(prop);
        let id_0 = env.mk_const(id_n, &[env.level_zero()]);
        let id_prop_p = env.mk_apps(id_0, &[prop, p_var]);
        assert!(env.is_def_eq(id_prop_p, p_var).unwrap());

        // Theorem `modus_ponens : ∀ (P Q : Prop), P → (P → Q) → Q`
        let mp_n = env.intern_name("modus_ponens");
        // Under (P=b3, Q=b2, hp=b1, hpq=b0): body = hpq hp = App(b0, b1)
        let p_to_q = env.mk_forall(b2, b2); // under (P=b2, Q=b1, hp=b0): P=b2, inside binder Q=b2
        let mp_ty_3 = env.mk_forall(p_to_q, b2); // inside binder Q=b2
        let mp_ty_2 = env.mk_forall(b1, mp_ty_3); // hp : P (P=b1)
        let mp_ty_1 = env.mk_forall(prop, mp_ty_2); // Q : Prop
        let mp_ty = env.mk_forall(prop, mp_ty_1); // P : Prop

        let mp_body = env.mk_app(b0, b1);
        let mp_l3 = env.mk_lam(p_to_q, mp_body);
        let mp_l2 = env.mk_lam(b1, mp_l3);
        let mp_l1 = env.mk_lam(prop, mp_l2);
        let mp_val = env.mk_lam(prop, mp_l1);

        env.check_decl(Decl::Def {
            name: mp_n,
            level_params: avec(&[]),
            ty: mp_ty,
            value: mp_val,
            is_theorem: true,
        })
        .unwrap();
        let report = env.verify_theorem(mp_n, &DEFAULT_ALLOWED_AXIOMS).unwrap();
        assert!(report.used_axioms.is_empty());

        // Theorem `imp_trans : ∀ (P Q R : Prop), (P → Q) → (Q → R) → (P → R)`
        let trans_n = env.intern_name("imp_trans");
        // Under (P=b2, Q=b1, R=b0):
        //   P → Q = ForallE(b2, b2)
        // Under (P=b3, Q=b2, R=b1, hpq=b0):
        //   Q → R = ForallE(b2, b2)
        // Under (P=b4, Q=b3, R=b2, hpq=b1, hqr=b0):
        //   P → R = ForallE(b4, b3)
        // Under (P=b5, Q=b4, R=b3, hpq=b2, hqr=b1, hp=b0):
        //   body = hqr (hpq hp) = App(b1, App(b2, b0))
        let pq_ty = env.mk_forall(b2, b2);
        let qr_ty = env.mk_forall(b2, b2);
        let b3 = env.mk_bvar(3);
        let b4 = env.mk_bvar(4);
        let pr_ty = env.mk_forall(b4, b3);

        let t_ty_4 = env.mk_forall(qr_ty, pr_ty);
        let t_ty_3 = env.mk_forall(pq_ty, t_ty_4);
        let t_ty_2 = env.mk_forall(prop, t_ty_3);
        let t_ty_1 = env.mk_forall(prop, t_ty_2);
        let trans_ty = env.mk_forall(prop, t_ty_1);

        let hpq_hp = env.mk_app(b2, b0);
        let hqr_hpq_hp = env.mk_app(b1, hpq_hp);
        let t_l5 = env.mk_lam(b4, hqr_hpq_hp);
        let t_l4 = env.mk_lam(qr_ty, t_l5);
        let t_l3 = env.mk_lam(pq_ty, t_l4);
        let t_l2 = env.mk_lam(prop, t_l3);
        let t_l1 = env.mk_lam(prop, t_l2);
        let trans_val = env.mk_lam(prop, t_l1);

        env.check_decl(Decl::Def {
            name: trans_n,
            level_params: avec(&[]),
            ty: trans_ty,
            value: trans_val,
            is_theorem: true,
        })
        .unwrap();
        let report_t = env.verify_theorem(trans_n, &DEFAULT_ALLOWED_AXIOMS).unwrap();
        assert!(report_t.used_axioms.is_empty());

        // Eta-equivalence: `(λ x : P => f x) ≡ f` for opaque `f : P → P`.
        let p_to_p = env.mk_arrow(p_var, p_var).unwrap();
        let (_, f_var) = env.mk_fresh_fvar(p_to_p);
        let f_b0 = env.mk_app(f_var, b0);
        let eta_lam = env.mk_lam(p_var, f_b0);
        assert!(env.is_def_eq(eta_lam, f_var).unwrap());
        assert!(env.is_def_eq(f_var, eta_lam).unwrap());
    }

    #[test]
    fn proof_irrelevance_positive_and_negative() {
        let mut env = KernelEnv::new_in(alloc());
        let prop = env.mk_prop();
        let type0 = env.mk_type0();

        // Two distinct proofs `p, q : P` where `P : Prop` must be definitionally equal.
        let (_, big_p) = env.mk_fresh_fvar(prop);
        let (_, big_q) = env.mk_fresh_fvar(prop);
        let (_, proof1) = env.mk_fresh_fvar(big_p);
        let (_, proof2) = env.mk_fresh_fvar(big_p);
        assert!(env.is_def_eq(proof1, proof2).unwrap());

        // Two distinct propositions `P, Q : Prop` are NOT equal (they live in `Type 0`, not `Prop`).
        assert!(!env.is_def_eq(big_p, big_q).unwrap());

        // Two distinct values `a, b : A` where `A : Type 0` are NOT equal.
        let (_, big_a) = env.mk_fresh_fvar(type0);
        let (_, a1) = env.mk_fresh_fvar(big_a);
        let (_, a2) = env.mk_fresh_fvar(big_a);
        assert!(!env.is_def_eq(a1, a2).unwrap());
    }

    #[test]
    fn nat_iota_reduction_add_two_two_equals_four() {
        let mut env = KernelEnv::new_in(alloc());
        add_core_logic(&mut env);
        add_nat(&mut env);

        let one_lvl = env.mk_level_nat(1);
        let nat_c = env.mk_const(env.lookup_name("Nat").unwrap(), &[]);
        let add_c = env.mk_const(env.lookup_name("Nat.add").unwrap(), &[]);
        let eq_c = env.mk_const(env.lookup_name("Eq").unwrap(), &[one_lvl]);
        let eq_refl_c = env.mk_const(env.lookup_name("Eq.refl").unwrap(), &[one_lvl]);

        // Prove `Nat.add 2 2 = 4` using unary constructors.
        let two = mk_nat_church(&mut env, 2);
        let four = mk_nat_church(&mut env, 4);
        let add_2_2 = env.mk_apps(add_c, &[two, two]);
        assert!(env.is_def_eq(add_2_2, four).unwrap());

        let thm_n = env.intern_name("add_two_two");
        let thm_ty = env.mk_apps(eq_c, &[nat_c, add_2_2, four]);
        let thm_pf = env.mk_apps(eq_refl_c, &[nat_c, four]);
        env.check_decl(Decl::Def {
            name: thm_n,
            level_params: avec(&[]),
            ty: thm_ty,
            value: thm_pf,
            is_theorem: true,
        })
        .unwrap();
        let verified = env.verify_theorem(thm_n, &DEFAULT_ALLOWED_AXIOMS).unwrap();
        assert!(verified.used_axioms.is_empty());

        // Also verify with Lean 4 `Literal::Nat` numerals: `Nat.add 2 2 = 4`.
        let lit2 = env.mk_lit_nat(2);
        let lit4 = env.mk_lit_nat(4);
        let lit_add_2_2 = env.mk_apps(add_c, &[lit2, lit2]);
        let lit_thm_n = env.intern_name("add_two_two_lit");
        let lit_thm_ty = env.mk_apps(eq_c, &[nat_c, lit_add_2_2, lit4]);
        let lit_thm_pf = env.mk_apps(eq_refl_c, &[nat_c, lit4]);
        env.check_decl(Decl::Def {
            name: lit_thm_n,
            level_params: avec(&[]),
            ty: lit_thm_ty,
            value: lit_thm_pf,
            is_theorem: true,
        })
        .unwrap();
        let verified_lit = env.verify_theorem(lit_thm_n, &DEFAULT_ALLOWED_AXIOMS).unwrap();
        assert!(verified_lit.used_axioms.is_empty());
    }

    #[test]
    fn blaze_domain_theorems_dir_capability_and_typeset_commutativity() {
        let mut env = KernelEnv::new_in(alloc());
        add_core_logic(&mut env);
        add_nat(&mut env);

        let u_name = env.intern_name("u");
        let u_lvl = env.mk_level_param(u_name);
        let sort_u = env.mk_sort(u_lvl);
        let prop = env.mk_prop();
        let type0 = env.mk_type0();
        let one_lvl = env.mk_level_nat(1);
        let b0 = env.mk_bvar(0);
        let b1 = env.mk_bvar(1);
        let b2 = env.mk_bvar(2);
        let b3 = env.mk_bvar(3);

        let true_c = env.mk_const(env.lookup_name("True").unwrap(), &[]);
        let true_intro_c = env.mk_const(env.lookup_name("True.intro").unwrap(), &[]);
        let false_c = env.mk_const(env.lookup_name("False").unwrap(), &[]);
        let false_rec_0 = env.mk_const(env.lookup_name("False.rec").unwrap(), &[env.level_zero()]);

        // -----------------------------------------------------------------
        // 1. Blaze `Dir` capability narrowing monotonicity (`Flat ⊆ Subtree`)
        // -----------------------------------------------------------------
        let dir_n = env.intern_name("Dir");
        let flat_n = env.intern_name("Dir.Flat");
        let subtree_n = env.intern_name("Dir.Subtree");
        let dir_rec_n = env.intern_name("Dir.rec");
        let dir_c = env.mk_const(dir_n, &[]);
        let flat_c = env.mk_const(flat_n, &[]);
        let subtree_c = env.mk_const(subtree_n, &[]);

        env.check_decl(Decl::Inductive {
            name: dir_n,
            level_params: avec(&[]),
            num_params: 0,
            ty: type0,
            ctors: avec(&[flat_n, subtree_n]),
        })
        .unwrap();
        env.check_decl(Decl::Ctor {
            name: flat_n,
            ind_name: dir_n,
            ctor_idx: 0,
            num_params: 0,
            num_fields: 0,
            level_params: avec(&[]),
            ty: dir_c,
        })
        .unwrap();
        env.check_decl(Decl::Ctor {
            name: subtree_n,
            ind_name: dir_n,
            ctor_idx: 1,
            num_params: 0,
            num_fields: 0,
            level_params: avec(&[]),
            ty: dir_c,
        })
        .unwrap();

        // Dir.rec.{u} : ∀ (m : Dir → Sort u), m Flat → m Subtree → ∀ (d : Dir), m d
        let dir_motive_ty = env.mk_arrow(dir_c, sort_u).unwrap();
        let m_flat = env.mk_app(b0, flat_c);
        let m_sub = env.mk_app(b1, subtree_c);
        let m_d = env.mk_app(b3, b0);
        let d_to_md = env.mk_forall(dir_c, m_d);
        let dr2 = env.mk_forall(m_sub, d_to_md);
        let dr1 = env.mk_forall(m_flat, dr2);
        let dir_rec_ty = env.mk_forall(dir_motive_ty, dr1);

        let flat_rhs_2 = env.mk_lam(m_sub, b1);
        let flat_rhs_1 = env.mk_lam(m_flat, flat_rhs_2);
        let flat_rhs = env.mk_lam(dir_motive_ty, flat_rhs_1);

        let sub_rhs_2 = env.mk_lam(m_sub, b0);
        let sub_rhs_1 = env.mk_lam(m_flat, sub_rhs_2);
        let sub_rhs = env.mk_lam(dir_motive_ty, sub_rhs_1);

        env.check_decl(Decl::Recursor {
            name: dir_rec_n,
            ind_name: dir_n,
            num_params: 0,
            num_indices: 0,
            num_motives: 1,
            num_minors: 2,
            level_params: avec(&[u_name]),
            ty: dir_rec_ty,
            rules: avec(&[
                RecRule { ctor_name: flat_n, nfields: 0, rhs: flat_rhs },
                RecRule { ctor_name: subtree_n, nfields: 0, rhs: sub_rhs },
            ]),
        })
        .unwrap();

        // Dir.sub : Dir → Dir → Prop
        //   Flat    ⊆ _       := True
        //   Subtree ⊆ Flat    := False
        //   Subtree ⊆ Subtree := True
        let dir_sub_n = env.intern_name("Dir.sub");
        let dir_rec_1 = env.mk_const(dir_rec_n, &[one_lvl]);
        let dir_to_prop = env.mk_arrow(dir_c, prop).unwrap();
        let dir_sub_ty = env.mk_arrow(dir_c, dir_to_prop).unwrap();

        let const_prop_motive = env.mk_lam(dir_c, prop);
        let sub_when_subtree = env.mk_apps(dir_rec_1, &[const_prop_motive, false_c, true_c, b0]);
        let dir_sub_body =
            env.mk_apps(dir_rec_1, &[const_prop_motive, true_c, sub_when_subtree, b1]);
        let dir_sub_l1 = env.mk_lam(dir_c, dir_sub_body);
        let dir_sub_val = env.mk_lam(dir_c, dir_sub_l1);

        env.check_decl(Decl::Def {
            name: dir_sub_n,
            level_params: avec(&[]),
            ty: dir_sub_ty,
            value: dir_sub_val,
            is_theorem: false,
        })
        .unwrap();

        let dir_sub_c = env.mk_const(dir_sub_n, &[]);

        // Theorem 1a: `Dir.flat_sub_subtree : Dir.sub Dir.Flat Dir.Subtree`
        let flat_sub_n = env.intern_name("Dir.flat_sub_subtree");
        let flat_sub_ty = env.mk_apps(dir_sub_c, &[flat_c, subtree_c]);
        env.check_decl(Decl::Def {
            name: flat_sub_n,
            level_params: avec(&[]),
            ty: flat_sub_ty,
            value: true_intro_c,
            is_theorem: true,
        })
        .unwrap();
        assert!(env.verify_theorem(flat_sub_n, &DEFAULT_ALLOWED_AXIOMS).unwrap().used_axioms.is_empty());

        // Theorem 1b: `Dir.sub_refl : ∀ (d : Dir), Dir.sub d d`
        let dir_rec_0 = env.mk_const(dir_rec_n, &[env.level_zero()]);
        let dir_refl_n = env.intern_name("Dir.sub_refl");
        let sub_d_d = env.mk_apps(dir_sub_c, &[b0, b0]);
        let dir_refl_ty = env.mk_forall(dir_c, sub_d_d);
        let refl_motive = env.mk_lam(dir_c, sub_d_d);
        let dir_refl_body = env.mk_apps(dir_rec_0, &[refl_motive, true_intro_c, true_intro_c, b0]);
        let dir_refl_val = env.mk_lam(dir_c, dir_refl_body);
        env.check_decl(Decl::Def {
            name: dir_refl_n,
            level_params: avec(&[]),
            ty: dir_refl_ty,
            value: dir_refl_val,
            is_theorem: true,
        })
        .unwrap();
        assert!(env.verify_theorem(dir_refl_n, &DEFAULT_ALLOWED_AXIOMS).unwrap().used_axioms.is_empty());

        // Theorem 1c: `Dir.sub_trans : ∀ (a b c : Dir), Dir.sub a b → Dir.sub b c → Dir.sub a c`
        let dir_trans_n = env.intern_name("Dir.sub_trans");
        // Under (a=b2, b=b1, c=b0):
        let b4 = env.mk_bvar(4);
        let sub_a_b = env.mk_apps(dir_sub_c, &[b2, b1]);
        let sub_b_c_shifted = env.mk_apps(dir_sub_c, &[b2, b1]); // under (a=b3, b=b2, c=b1, hab=b0)
        let sub_a_c_shifted = env.mk_apps(dir_sub_c, &[b4, b2]); // under (a=b4, b=b3, c=b2, hab=b1, hbc=b0)
        let trans_inner = env.mk_forall(sub_b_c_shifted, sub_a_c_shifted);
        let trans_hab = env.mk_forall(sub_a_b, trans_inner);
        let trans_c = env.mk_forall(dir_c, trans_hab);
        let trans_b = env.mk_forall(dir_c, trans_c);
        let trans_ty = env.mk_forall(dir_c, trans_b);

        // Proof by case split on `a`, then `b`, then `c`:
        // - `a = Flat`: `Dir.sub Flat c` reduces to `True`, proved by `λ b c hab hbc => True.intro`.
        let sub_flat_b = env.mk_apps(dir_sub_c, &[flat_c, b1]);
        let sub_b_c_2 = env.mk_apps(dir_sub_c, &[b2, b1]);
        let case_flat_l3 = env.mk_lam(sub_b_c_2, true_intro_c);
        let case_flat_l2 = env.mk_lam(sub_flat_b, case_flat_l3);
        let case_flat_l1 = env.mk_lam(dir_c, case_flat_l2);
        let case_flat = env.mk_lam(dir_c, case_flat_l1);

        // - `a = Subtree`: case split on `b`:
        //   - `b = Flat`: `hab : Dir.sub Subtree Flat` reduces to `False`, eliminate via `False.rec`!
        let sub_sub_flat = env.mk_apps(dir_sub_c, &[subtree_c, flat_c]);
        let sub_flat_c_1 = env.mk_apps(dir_sub_c, &[flat_c, b1]);
        let goal_sub_c = env.mk_apps(dir_sub_c, &[subtree_c, b2]); // under (c=b2, hab=b1, hbc=b0)
        let sub_sub_c_3 = env.mk_apps(dir_sub_c, &[subtree_c, b3]);
        let false_elim_motive = env.mk_lam(false_c, sub_sub_c_3);
        let elim_hab = env.mk_apps(false_rec_0, &[false_elim_motive, b1]);
        let case_sub_flat_l2 = env.mk_lam(sub_flat_c_1, elim_hab);
        let case_sub_flat_l1 = env.mk_lam(sub_sub_flat, case_sub_flat_l2);
        let case_sub_flat = env.mk_lam(dir_c, case_sub_flat_l1);
        let _ = goal_sub_c;

        //   - `b = Subtree`: `hbc : Dir.sub Subtree c`, which IS the goal `Dir.sub Subtree c`!
        let sub_sub_sub = env.mk_apps(dir_sub_c, &[subtree_c, subtree_c]);
        let sub_sub_c_1 = env.mk_apps(dir_sub_c, &[subtree_c, b1]);
        let case_sub_sub_l2 = env.mk_lam(sub_sub_c_1, b0);
        let case_sub_sub_l1 = env.mk_lam(sub_sub_sub, case_sub_sub_l2);
        let case_sub_sub = env.mk_lam(dir_c, case_sub_sub_l1);

        // Motive for `b` when `a = Subtree`:
        //   λ b : Dir => ∀ c : Dir, Dir.sub Subtree b → Dir.sub b c → Dir.sub Subtree c
        let m_b_sub_a_b = env.mk_apps(dir_sub_c, &[subtree_c, b1]);
        let m_b_sub_b_c = env.mk_apps(dir_sub_c, &[b2, b1]);
        let m_b_goal = env.mk_apps(dir_sub_c, &[subtree_c, b2]);
        let m_b_t2 = env.mk_forall(m_b_sub_b_c, m_b_goal);
        let m_b_t1 = env.mk_forall(m_b_sub_a_b, m_b_t2);
        let m_b_t0 = env.mk_forall(dir_c, m_b_t1);
        let m_b_motive = env.mk_lam(dir_c, m_b_t0);
        let case_subtree = env.mk_apps(dir_rec_0, &[m_b_motive, case_sub_flat, case_sub_sub]);

        let m_a_motive = env.mk_lam(dir_c, trans_b);
        let trans_val = env.mk_apps(dir_rec_0, &[m_a_motive, case_flat, case_subtree]);

        env.check_decl(Decl::Def {
            name: dir_trans_n,
            level_params: avec(&[]),
            ty: trans_ty,
            value: trans_val,
            is_theorem: true,
        })
        .unwrap();
        assert!(env.verify_theorem(dir_trans_n, &DEFAULT_ALLOWED_AXIOMS).unwrap().used_axioms.is_empty());

        // -----------------------------------------------------------------
        // 2. Blaze `TypeSet` union commutativity (pointwise, 0 axioms)
        // -----------------------------------------------------------------
        let bool_n = env.intern_name("Bool");
        let bfalse_n = env.intern_name("Bool.false");
        let btrue_n = env.intern_name("Bool.true");
        let bool_rec_n = env.intern_name("Bool.rec");
        let bool_c = env.mk_const(bool_n, &[]);
        let bfalse_c = env.mk_const(bfalse_n, &[]);
        let btrue_c = env.mk_const(btrue_n, &[]);

        env.check_decl(Decl::Inductive {
            name: bool_n,
            level_params: avec(&[]),
            num_params: 0,
            ty: type0,
            ctors: avec(&[bfalse_n, btrue_n]),
        })
        .unwrap();
        env.check_decl(Decl::Ctor {
            name: bfalse_n,
            ind_name: bool_n,
            ctor_idx: 0,
            num_params: 0,
            num_fields: 0,
            level_params: avec(&[]),
            ty: bool_c,
        })
        .unwrap();
        env.check_decl(Decl::Ctor {
            name: btrue_n,
            ind_name: bool_n,
            ctor_idx: 1,
            num_params: 0,
            num_fields: 0,
            level_params: avec(&[]),
            ty: bool_c,
        })
        .unwrap();

        let bool_motive_ty = env.mk_arrow(bool_c, sort_u).unwrap();
        let m_bf = env.mk_app(b0, bfalse_c);
        let m_bt = env.mk_app(b1, btrue_c);
        let m_bv = env.mk_app(b3, b0);
        let bv_to_mbv = env.mk_forall(bool_c, m_bv);
        let br2 = env.mk_forall(m_bt, bv_to_mbv);
        let br1 = env.mk_forall(m_bf, br2);
        let bool_rec_ty = env.mk_forall(bool_motive_ty, br1);

        let bf_rhs_2 = env.mk_lam(m_bt, b1);
        let bf_rhs_1 = env.mk_lam(m_bf, bf_rhs_2);
        let bf_rhs = env.mk_lam(bool_motive_ty, bf_rhs_1);

        let bt_rhs_2 = env.mk_lam(m_bt, b0);
        let bt_rhs_1 = env.mk_lam(m_bf, bt_rhs_2);
        let bt_rhs = env.mk_lam(bool_motive_ty, bt_rhs_1);

        env.check_decl(Decl::Recursor {
            name: bool_rec_n,
            ind_name: bool_n,
            num_params: 0,
            num_indices: 0,
            num_motives: 1,
            num_minors: 2,
            level_params: avec(&[u_name]),
            ty: bool_rec_ty,
            rules: avec(&[
                RecRule { ctor_name: bfalse_n, nfields: 0, rhs: bf_rhs },
                RecRule { ctor_name: btrue_n, nfields: 0, rhs: bt_rhs },
            ]),
        })
        .unwrap();

        // Bool.or : Bool → Bool → Bool := λ a b => Bool.rec.{1} (λ _ => Bool) b Bool.true a
        let bor_n = env.intern_name("Bool.or");
        let bool_rec_1 = env.mk_const(bool_rec_n, &[one_lvl]);
        let bool_rec_0 = env.mk_const(bool_rec_n, &[env.level_zero()]);
        let bool_to_bool = env.mk_arrow(bool_c, bool_c).unwrap();
        let bor_ty = env.mk_arrow(bool_c, bool_to_bool).unwrap();
        let const_bool_m = env.mk_lam(bool_c, bool_c);
        let bor_body = env.mk_apps(bool_rec_1, &[const_bool_m, b0, btrue_c, b1]);
        let bor_l1 = env.mk_lam(bool_c, bor_body);
        let bor_val = env.mk_lam(bool_c, bor_l1);
        env.check_decl(Decl::Def {
            name: bor_n,
            level_params: avec(&[]),
            ty: bor_ty,
            value: bor_val,
            is_theorem: false,
        })
        .unwrap();

        // Bool.or_comm : ∀ (a b : Bool), Eq Bool (Bool.or a b) (Bool.or b a)
        let bor_c = env.mk_const(bor_n, &[]);
        let eq_1 = env.mk_const(env.lookup_name("Eq").unwrap(), &[one_lvl]);
        let eq_refl_1 = env.mk_const(env.lookup_name("Eq.refl").unwrap(), &[one_lvl]);

        let bor_comm_n = env.intern_name("Bool.or_comm");
        let or_ab = env.mk_apps(bor_c, &[b1, b0]);
        let or_ba = env.mk_apps(bor_c, &[b0, b1]);
        let eq_or_ab_ba = env.mk_apps(eq_1, &[bool_c, or_ab, or_ba]);
        let bor_comm_b = env.mk_forall(bool_c, eq_or_ab_ba);
        let bor_comm_ty = env.mk_forall(bool_c, bor_comm_b);

        // Case a = false: Bool.rec (λ b => Eq Bool (or false b) (or b false)) (refl false) (refl true)
        let or_fb = env.mk_apps(bor_c, &[bfalse_c, b0]);
        let or_bf = env.mk_apps(bor_c, &[b0, bfalse_c]);
        let eq_fb_bf = env.mk_apps(eq_1, &[bool_c, or_fb, or_bf]);
        let m_f = env.mk_lam(bool_c, eq_fb_bf);
        let refl_f = env.mk_apps(eq_refl_1, &[bool_c, bfalse_c]);
        let refl_t = env.mk_apps(eq_refl_1, &[bool_c, btrue_c]);
        let case_f = env.mk_apps(bool_rec_0, &[m_f, refl_f, refl_t]);

        // Case a = true: Bool.rec (λ b => Eq Bool (or true b) (or b true)) (refl true) (refl true)
        let or_tb = env.mk_apps(bor_c, &[btrue_c, b0]);
        let or_bt = env.mk_apps(bor_c, &[b0, btrue_c]);
        let eq_tb_bt = env.mk_apps(eq_1, &[bool_c, or_tb, or_bt]);
        let m_t = env.mk_lam(bool_c, eq_tb_bt);
        let case_t = env.mk_apps(bool_rec_0, &[m_t, refl_t, refl_t]);

        let m_a = env.mk_lam(bool_c, bor_comm_b);
        let bor_comm_val = env.mk_apps(bool_rec_0, &[m_a, case_f, case_t]);
        env.check_decl(Decl::Def {
            name: bor_comm_n,
            level_params: avec(&[]),
            ty: bor_comm_ty,
            value: bor_comm_val,
            is_theorem: true,
        })
        .unwrap();
        assert!(env.verify_theorem(bor_comm_n, &DEFAULT_ALLOWED_AXIOMS).unwrap().used_axioms.is_empty());

        // TypeSet := Nat → Bool
        // TypeSet.union (s1 s2 : TypeSet) (t : Nat) : Bool := Bool.or (s1 t) (s2 t)
        let nat_c = env.mk_const(env.lookup_name("Nat").unwrap(), &[]);
        let ts_n = env.intern_name("TypeSet");
        let ts_val = env.mk_arrow(nat_c, bool_c).unwrap();
        env.check_decl(Decl::Def {
            name: ts_n,
            level_params: avec(&[]),
            ty: type0,
            value: ts_val,
            is_theorem: false,
        })
        .unwrap();
        let ts_c = env.mk_const(ts_n, &[]);

        let union_n = env.intern_name("TypeSet.union");
        let ts_to_ts = env.mk_arrow(ts_c, ts_c).unwrap();
        let union_ty = env.mk_arrow(ts_c, ts_to_ts).unwrap();
        // λ (s1 : TypeSet) (s2 : TypeSet) (t : Nat) => Bool.or (s1 t) (s2 t)
        let s1_t = env.mk_app(b2, b0);
        let s2_t = env.mk_app(b1, b0);
        let union_body = env.mk_apps(bor_c, &[s1_t, s2_t]);
        let u_l2 = env.mk_lam(nat_c, union_body);
        let u_l1 = env.mk_lam(ts_c, u_l2);
        let union_val = env.mk_lam(ts_c, u_l1);
        env.check_decl(Decl::Def {
            name: union_n,
            level_params: avec(&[]),
            ty: union_ty,
            value: union_val,
            is_theorem: false,
        })
        .unwrap();

        // Theorem `TypeSet.union_comm : ∀ (s1 s2 : TypeSet) (t : Nat),
        //   Eq Bool (TypeSet.union s1 s2 t) (TypeSet.union s2 s1 t)`
        let union_c = env.mk_const(union_n, &[]);
        let bor_comm_c = env.mk_const(bor_comm_n, &[]);
        let ts_comm_n = env.intern_name("TypeSet.union_comm");

        let lhs = env.mk_apps(union_c, &[b2, b1, b0]);
        let rhs = env.mk_apps(union_c, &[b1, b2, b0]);
        let eq_union = env.mk_apps(eq_1, &[bool_c, lhs, rhs]);
        let tc2 = env.mk_forall(nat_c, eq_union);
        let tc1 = env.mk_forall(ts_c, tc2);
        let ts_comm_ty = env.mk_forall(ts_c, tc1);

        let pf_body = env.mk_apps(bor_comm_c, &[s1_t, s2_t]);
        let tp2 = env.mk_lam(nat_c, pf_body);
        let tp1 = env.mk_lam(ts_c, tp2);
        let ts_comm_val = env.mk_lam(ts_c, tp1);

        env.check_decl(Decl::Def {
            name: ts_comm_n,
            level_params: avec(&[]),
            ty: ts_comm_ty,
            value: ts_comm_val,
            is_theorem: true,
        })
        .unwrap();
        let report = env.verify_theorem(ts_comm_n, &DEFAULT_ALLOWED_AXIOMS).unwrap();
        assert!(report.used_axioms.is_empty());
    }

    #[test]
    fn projection_reduction_and_let_zeta_reduction() {
        let mut env = KernelEnv::new_in(alloc());
        add_core_logic(&mut env);
        add_nat(&mut env);

        let type0 = env.mk_type0();
        let nat_c = env.mk_const(env.lookup_name("Nat").unwrap(), &[]);
        let b0 = env.mk_bvar(0);
        let b1 = env.mk_bvar(1);
        let b2 = env.mk_bvar(2);
        let b3 = env.mk_bvar(3);

        // Pair : Type 0 → Type 0 → Type 0, Pair.mk : ∀ (α β : Type 0), α → β → Pair α β
        let pair_n = env.intern_name("Pair");
        let pair_mk_n = env.intern_name("Pair.mk");
        let pair_c = env.mk_const(pair_n, &[]);
        let t0_to_t0 = env.mk_arrow(type0, type0).unwrap();
        let pair_ty = env.mk_arrow(type0, t0_to_t0).unwrap();

        env.check_decl(Decl::Inductive {
            name: pair_n,
            level_params: avec(&[]),
            num_params: 2,
            ty: pair_ty,
            ctors: avec(&[pair_mk_n]),
        })
        .unwrap();

        // Pair.mk : ∀ (α : Type 0) (β : Type 0) (a : α) (b : β), Pair α β
        let pair_a_b = env.mk_apps(pair_c, &[b3, b2]);
        let mk_t3 = env.mk_forall(b1, pair_a_b); // b : β (β=b1)
        let mk_t2 = env.mk_forall(b1, mk_t3); // a : α (α=b1)
        let mk_t1 = env.mk_forall(type0, mk_t2);
        let mk_ty = env.mk_forall(type0, mk_t1);
        env.check_decl(Decl::Ctor {
            name: pair_mk_n,
            ind_name: pair_n,
            ctor_idx: 0,
            num_params: 2,
            num_fields: 2,
            level_params: avec(&[]),
            ty: mk_ty,
        })
        .unwrap();

        let pair_mk_c = env.mk_const(pair_mk_n, &[]);
        let one = mk_nat_church(&mut env, 1);
        let two = mk_nat_church(&mut env, 2);
        let p = env.mk_apps(pair_mk_c, &[nat_c, nat_c, one, two]);

        let fst = env.mk_proj(pair_n, 0, p);
        let snd = env.mk_proj(pair_n, 1, p);
        assert!(env.is_def_eq(fst, one).unwrap());
        assert!(env.is_def_eq(snd, two).unwrap());
        let fst_ty = env.infer_type(fst).unwrap();
        assert!(env.is_def_eq(fst_ty, nat_c).unwrap());

        // LetE zeta-reduction: `let x : Nat := 1; Pair.mk Nat Nat x 2` projected at 0 == 1.
        let pair_let_body = env.mk_apps(pair_mk_c, &[nat_c, nat_c, b0, two]);
        let let_expr = env.mk_let(nat_c, one, pair_let_body);
        let let_fst = env.mk_proj(pair_n, 0, let_expr);
        assert!(env.is_def_eq(let_fst, one).unwrap());
    }

    #[test]
    fn export_certificate_round_trip_and_rejection() {
        let mut env = KernelEnv::new_in(alloc());

        // Pre-register a simple `Box` structure so `#EJ` projection can be tested in `.export` too.
        let type0 = env.mk_type0();
        let box_n = env.intern_name("Box");
        let box_mk_n = env.intern_name("Box.mk");
        let box_c = env.mk_const(box_n, &[]);
        let b0 = env.mk_bvar(0);
        let b1 = env.mk_bvar(1);
        let box_ty = env.mk_arrow(type0, type0).unwrap();
        env.check_decl(Decl::Inductive {
            name: box_n,
            level_params: avec(&[]),
            num_params: 1,
            ty: box_ty,
            ctors: avec(&[box_mk_n]),
        })
        .unwrap();
        // Box.mk : ∀ (α : Type 0), α → Box α
        let box_a = env.mk_app(box_c, b1);
        let mk_inner = env.mk_forall(b0, box_a);
        let box_mk_ty = env.mk_forall(type0, mk_inner);
        env.check_decl(Decl::Ctor {
            name: box_mk_n,
            ind_name: box_n,
            ctor_idx: 0,
            num_params: 1,
            num_fields: 1,
            level_params: avec(&[]),
            ty: box_mk_ty,
        })
        .unwrap();

        // Valid Lean 4 `.export` certificate exercising:
        // #NS, #NI, #US, #UM, #UIM, #UP, #EV, #ES, #EC, #EA, #EL, #EP, #EZ, #EJ, #AX, #DEF, #THM.
        let cert = "\
0.1.0
1 #NS 0 u
2 #NS 0 v
3 #NS 0 Exported
4 #NS 3 id
5 #NS 3 mp
6 #NS 3 let_id
7 #NS 3 unbox
8 #NS 3 P
9 #NS 3 Q
10 #NS 3 a
11 #NS 3 x
12 #NI 3 0
13 #NS 0 Box
14 #NS 0 propext
1 #US 0
2 #UP 1
3 #UP 2
4 #US 2
5 #UM 2 3
6 #UIM 2 0
0 #EV 0
1 #EV 1
2 #EV 2
3 #ES 0
4 #ES 1
5 #ES 4
6 #EP #BD 11 0 1
7 #EP #BI 10 5 6
8 #EL #BD 11 0 0
9 #EL #BI 10 5 8
#DEF 4 7 9 A 1
10 #EC 4 0
11 #EP #BD 10 3 3
12 #EL #BD 10 3 0
13 #EZ 11 11 12 0
#DEF 6 11 13 R 1
14 #EC 13
15 #EA 14 0
16 #EJ 13 0 0
17 #EL #BD 11 15 16
18 #EL #BI 10 4 17
19 #EP #BD 11 15 1
20 #EP #BI 10 4 19
#DEF 7 20 18 O
21 #EP #BD 11 2 2
22 #EP #BD 11 21 2
23 #EP #BD 11 1 22
24 #EP #BD 9 3 23
25 #EP #BD 8 3 24
26 #EA 0 1
27 #EL #BD 11 21 26
28 #EL #BD 11 1 27
29 #EL #BD 9 3 28
30 #EL #BD 8 3 29
#THM 5 25 30
#AX 14 3
";

        let summary = env.load_export(cert).unwrap();
        assert_eq!(summary.decls, 5);
        assert_eq!(summary.names, 14);
        assert_eq!(summary.levels, 6);
        assert_eq!(summary.exprs, 31);

        let mp_id = env.lookup_name("Exported.mp").unwrap();
        let report = env.verify_theorem(mp_id, &DEFAULT_ALLOWED_AXIOMS).unwrap();
        assert!(report.used_axioms.is_empty());

        // Negative control: ill-typed `#THM` in an `.export` file must be rejected.
        let bad_cert = "\
0.1.0
1 #NS 0 Bad
2 #NS 1 fake
3 #NS 0 P
0 #EV 0
1 #ES 0
2 #EP #BD 3 1 0
3 #EL #BD 3 1 0
#THM 2 2 3
";
        let err = env.load_export(bad_cert).unwrap_err();
        assert!(matches!(err, KernelError::TypeMismatch { .. }));
    }

    #[test]
    fn negative_soundness_controls_ill_typed_sorry_ax_and_not_a_prop() {
        let mut env = KernelEnv::new_in(alloc());
        let prop = env.mk_prop();
        let type0 = env.mk_type0();
        let b0 = env.mk_bvar(0);
        let b1 = env.mk_bvar(1);

        // 1. Ill-typed proof rejected with `KernelError::TypeMismatch`.
        // Attempt `theorem bad_proof : ∀ (P Q : Prop), P → Q := λ P Q hp => hp`
        let bad_pf_n = env.intern_name("bad_proof");
        let p_to_q = env.mk_forall(b1, b1); // under (P=b1, Q=b0): P -> Q (inside: Q=b1)
        let bad_ty_1 = env.mk_forall(prop, p_to_q);
        let bad_ty = env.mk_forall(prop, bad_ty_1);
        let bad_l2 = env.mk_lam(b1, b0);
        let bad_l1 = env.mk_lam(prop, bad_l2);
        let bad_val = env.mk_lam(prop, bad_l1);
        let err = env
            .check_decl(Decl::Def {
                name: bad_pf_n,
                level_params: avec(&[]),
                ty: bad_ty,
                value: bad_val,
                is_theorem: true,
            })
            .unwrap_err();
        assert!(matches!(err, KernelError::TypeMismatch { .. }));

        // 2. Non-Prop theorem rejected with `KernelError::NotAProp`.
        let non_prop_n = env.intern_name("not_a_prop_thm");
        let id_type0_ty = env.mk_arrow(type0, type0).unwrap();
        let id_type0_val = env.mk_lam(type0, b0);
        let err = env
            .check_decl(Decl::Def {
                name: non_prop_n,
                level_params: avec(&[]),
                ty: id_type0_ty,
                value: id_type0_val,
                is_theorem: true,
            })
            .unwrap_err();
        assert!(matches!(err, KernelError::NotAProp { name, .. } if name == non_prop_n));

        // 3. `sorryAx` rejected by `verify_theorem` (both directly and transitively!).
        let u_name = env.intern_name("u");
        let u_lvl = env.mk_level_param(u_name);
        let sort_u = env.mk_sort(u_lvl);
        let sorry_n = env.intern_name("sorryAx");
        // sorryAx.{u} : ∀ (α : Sort u), α
        let sorry_ty = env.mk_forall(sort_u, b0);
        env.check_decl(Decl::Axiom { name: sorry_n, level_params: avec(&[u_name]), ty: sorry_ty })
            .unwrap();

        let helper_n = env.intern_name("helper_with_sorry");
        let false_prop_ty = env.mk_forall(prop, b0); // ∀ (P : Prop), P
        let sorry_0 = env.mk_const(sorry_n, &[env.level_zero()]);
        let sorry_app = env.mk_app(sorry_0, b0);
        let helper_val = env.mk_lam(prop, sorry_app);
        env.check_decl(Decl::Def {
            name: helper_n,
            level_params: avec(&[]),
            ty: false_prop_ty,
            value: helper_val,
            is_theorem: true,
        })
        .unwrap();

        // Direct use of `sorryAx` rejected.
        let err = env.verify_theorem(helper_n, &DEFAULT_ALLOWED_AXIOMS).unwrap_err();
        assert_eq!(err, KernelError::DisallowedAxiom { theorem: helper_n, axiom: sorry_n });

        // Transitive use of `sorryAx` through `helper_with_sorry` also rejected!
        let outer_n = env.intern_name("outer_uses_helper");
        let helper_c = env.mk_const(helper_n, &[]);
        env.check_decl(Decl::Def {
            name: outer_n,
            level_params: avec(&[]),
            ty: false_prop_ty,
            value: helper_c,
            is_theorem: true,
        })
        .unwrap();
        let err = env.verify_theorem(outer_n, &DEFAULT_ALLOWED_AXIOMS).unwrap_err();
        assert_eq!(err, KernelError::DisallowedAxiom { theorem: outer_n, axiom: sorry_n });

        // 4. Allowed axiom (`propext`) passes when allowlisted, fails when allowlist is empty.
        let propext_n = env.intern_name("propext");
        let propext_inner = env.mk_forall(prop, b1);
        let propext_ty = env.mk_forall(prop, propext_inner); // simplified Prop axiom
        env.check_decl(Decl::Axiom { name: propext_n, level_params: avec(&[]), ty: propext_ty })
            .unwrap();
        let uses_propext_n = env.intern_name("uses_propext");
        let propext_c = env.mk_const(propext_n, &[]);
        env.check_decl(Decl::Def {
            name: uses_propext_n,
            level_params: avec(&[]),
            ty: propext_ty,
            value: propext_c,
            is_theorem: true,
        })
        .unwrap();
        let report = env.verify_theorem(uses_propext_n, &DEFAULT_ALLOWED_AXIOMS).unwrap();
        assert_eq!(report.used_axioms.as_slice(), &[propext_n]);
        let err_strict = env.verify_theorem(uses_propext_n, &[]).unwrap_err();
        assert_eq!(
            err_strict,
            KernelError::DisallowedAxiom { theorem: uses_propext_n, axiom: propext_n }
        );
    }

    #[test]
    fn eq_rec_iota_theorem_opacity_and_depth_fuel_guards() {
        let mut env = KernelEnv::new_in(alloc());
        add_core_logic(&mut env);
        add_nat(&mut env);

        let one_lvl = env.mk_level_nat(1);
        let nat_c = env.mk_const(env.lookup_name("Nat").unwrap(), &[]);
        let eq_1 = env.mk_const(env.lookup_name("Eq").unwrap(), &[one_lvl]);
        let eq_refl_1 = env.mk_const(env.lookup_name("Eq.refl").unwrap(), &[one_lvl]);
        let eq_rec_01 =
            env.mk_const(env.lookup_name("Eq.rec").unwrap(), &[env.level_zero(), one_lvl]);
        let zero = mk_nat_church(&mut env, 0);
        let b0 = env.mk_bvar(0);
        let b1 = env.mk_bvar(1);
        let b2 = env.mk_bvar(2);
        let b3 = env.mk_bvar(3);

        // Prove `Eq.symm_nat : ∀ (a b : Nat), Eq Nat a b → Eq Nat b a` via `Eq.rec`.
        let symm_n = env.intern_name("Eq.symm_nat");
        let eq_a_b = env.mk_apps(eq_1, &[nat_c, b1, b0]);
        let eq_b_a_shifted = env.mk_apps(eq_1, &[nat_c, b1, b2]);
        let symm_t2 = env.mk_forall(eq_a_b, eq_b_a_shifted);
        let symm_t1 = env.mk_forall(nat_c, symm_t2);
        let symm_ty = env.mk_forall(nat_c, symm_t1);

        // Inside (a=b2, b=b1, h=b0):
        //   motive := λ (x : Nat) (_ : Eq Nat a [b3] x [b0]) => Eq Nat x [b1] a [b4]
        let b4 = env.mk_bvar(4);
        let eq_a_x = env.mk_apps(eq_1, &[nat_c, b3, b0]);
        let eq_x_a = env.mk_apps(eq_1, &[nat_c, b1, b4]);
        let motive_inner = env.mk_lam(eq_a_x, eq_x_a);
        let motive = env.mk_lam(nat_c, motive_inner);
        let minor = env.mk_apps(eq_refl_1, &[nat_c, b2]);
        let symm_body = env.mk_apps(eq_rec_01, &[nat_c, b2, motive, minor, b1, b0]);
        let symm_l2 = env.mk_lam(eq_a_b, symm_body);
        let symm_l1 = env.mk_lam(nat_c, symm_l2);
        let symm_val = env.mk_lam(nat_c, symm_l1);

        env.check_decl(Decl::Def {
            name: symm_n,
            level_params: avec(&[]),
            ty: symm_ty,
            value: symm_val,
            is_theorem: true,
        })
        .unwrap();
        assert!(env.verify_theorem(symm_n, &DEFAULT_ALLOWED_AXIOMS).unwrap().used_axioms.is_empty());

        // Check `Eq.rec` iota-reduction directly on `Eq.refl Nat 0`.
        let refl_0 = env.mk_apps(eq_refl_1, &[nat_c, zero]);
        let eq_0_x = env.mk_apps(eq_1, &[nat_c, zero, b0]);
        let eq_x_0 = env.mk_apps(eq_1, &[nat_c, b1, zero]);
        let m0_inner = env.mk_lam(eq_0_x, eq_x_0);
        let m0 = env.mk_lam(nat_c, m0_inner);
        let rec_app = env.mk_apps(eq_rec_01, &[nat_c, zero, m0, refl_0, zero, refl_0]);
        assert_eq!(env.whnf(rec_app).unwrap(), refl_0);

        // Opaque theorem stays folded under `whnf`.
        let symm_c = env.mk_const(symm_n, &[]);
        assert_eq!(env.whnf(symm_c).unwrap(), symm_c);

        // Depth and fuel guards fail cleanly with typed errors instead of panicking.
        let mut deep = zero;
        let succ_c = env.mk_const(env.lookup_name("Nat.succ").unwrap(), &[]);
        for _ in 0..40 {
            deep = env.mk_app(succ_c, deep);
        }
        env.set_max_depth(8);
        assert_eq!(env.infer_type(deep), Err(KernelError::DepthExceeded));

        env.set_max_depth(DEFAULT_MAX_DEPTH);
        env.set_fuel_budget(3);
        assert_eq!(env.infer_type(deep), Err(KernelError::FuelExhausted));
    }
}
