//! `magic_number` and `magic_string` (`blaze::internal`, allowed by default, denied in blaze's
//! own workspace by the ratchet): a literal in code states a fact about something else (a size,
//! a count, a limit, a duration, a protocol field, a tuning choice, a key, a setting's name, a
//! state's name). That fact has one home: derived from what it depends on, or a named `const`
//! whose name says what it is and whose doc comment says why that value, used everywhere the
//! fact is used. Where the set of values is closed, the home is a type: an enum.
//!
//! Both are decided on the typed tree (HIR, inference, name resolution), never on text. A body's
//! literals are read once ([`literals`]); the crate's are merged once ([`magic_index`]) and asked
//! for one use at a time ([`magic_answer`], keyed by [`MagicKey`]), so a body's lint reruns only
//! when the sites of its own literals' uses change.
//!
//! Not looked at, each stated in `docs/lints.md#magic_number`: the bodies of a `const`, a
//! `static` and an enum discriminant, an inline `const { .. }` block (the named home); test code
//! (`#[test]`, `cfg(test)` modules, test, bench and example targets); literals in patterns; what
//! a macro wrote, its arguments included (`format!`, `vec!`, `assert!`: a gap for `vec![0;
//! 4096]`, which the front end maps to the macro call, not to the literal); array lengths in
//! types (`[u8; 32]` in a signature or a field has no body).
//!
//! A number is exempt when it is an identity (`0`, `1`, `-1`, `0.0`, `1.0`), a `2` that doubles
//! or halves (an operand of `*`, `/`, `%`, `<<`, `>>`, `pow`), or an index or range bound of a
//! fixed-size array that its length proves in range. A string is exempt when it is empty, has no
//! letter or digit (punctuation the code writes: `", "`, `"::"`), holds whitespace (prose for a
//! person), or is the message of `expect` or `context`.
//!
//! The stronger findings, each its own message, strongest first:
//!
//! - disagreeing: an array indexed by a fieldless enum's discriminant whose length is not the
//!   enum's variant count; a slice of `N` bytes read by `u32::from_le_bytes` and its siblings,
//!   whose `try_into` fails whenever `N` is not the integer's size;
//! - derivable: the length of the array sliced or looped over (`a[..4]`, `for i in 0..4 {
//!   a[i] }`: `a.len()`, machine-applicable for a place); the variant count of the enum indexing
//!   the array (`core::mem::variant_count::<E>()`, machine-applicable where the crate turns on
//!   `variant_count` and the name resolves); the size of the integer `from_*_bytes` reads
//!   (`size_of::<u32>()`); the value of the one `const` the same parameter is passed elsewhere
//!   in the crate (that `const`). A string that spells the name of the variant its `match` arm
//!   is of, of a field passed beside it, or of the function it is in;
//! - near-miss (strings): the same use elsewhere in the crate with a spelling that differs only
//!   by case, `-` and `_`, a plural `s`, or one non-digit character of a word of six or more;
//! - repeated: a number passed to the same parameter of the same function, or compared with the
//!   same field, `static` or method, with the same type, in two or more places of the crate; a
//!   string written in two or more places of the crate. The message lists the other functions;
//! - a closed set (strings): one place matched or compared against two or more strings: an enum
//!   with these variants;
//! - the rest: a number anywhere, a string where the program branches or looks up by it (a
//!   compare, a map key, an environment variable's name, a path component): name it. A literal
//!   passed to a `Duration` constructor is asked to name the duration.
//!
//! A string has its home already when a `const` of the crate, or of a local dependency, holds
//! it ([`named_index`]): the literal is told that `const`'s path, with a machine-applicable fix
//! where the path names it from here. `const BLAZE_HOME: &str = "BLAZE_HOME"` is the home; the
//! second `"BLAZE_HOME"` is the finding. A string that spells an item's name (the variant of
//! its `match` arm, a field beside it, its function, the type its method is of, its crate) is
//! told the convention that maps the name to the text (`DepInfo` in snake_case is
//! `"dep_info"`); a `match` or an array that spells every variant of an enum is one finding,
//! whose fix is the reflection form (`for const` over the variants) where the crate has blaze's
//! compile-time reflection on, else one `const` table beside the enum. What sibling crates
//! share is [`shared`]'s.
//!
//! `repeated_format`: a format template (the string of a `format_args!` a formatting or error
//! macro wrote: `format!`, `write!`, `panic!`, `anyhow!`, `bail!`, blaze's own) of
//! [`template::TEMPLATE_WORDS_FLOOR`] words or more, its placeholders made one, written in two
//! or more places of the crate. A template that is not a string literal (`concat!(..)`, a
//! `const`) is not read; a macro that formats without `format_args!` (one that takes its
//! template apart itself, as `tracing`'s field syntax does) is a gap.

mod shared;
mod template;

use std::alloc::Global;

use base_db::Db;
use blaze_hash::HashMap;
use hir_def::{
    AdtId, ConstId, DefWithBodyId, EnumId, EnumVariantId, FieldId, FunctionId, StaticId, VariantId,
    expr_store::Body,
    hir::{
        ArithOp, Array, BinaryOp, BindingId, CmpOp, Expr, ExprId, Literal, Pat, PatId, Statement,
        UnaryOp,
    },
    resolver::ValueNs,
    signatures::FunctionSignature,
};
use hir_ty::next_solver::{Ty, TyKind};
use intern::Symbol;
use rustc_type_ir::{FloatTy, IntTy, UintTy, inherent::IntoKind as _};
use syntax::{
    NodeOrToken, SyntaxKind, TextRange,
    view::{self as ast, AstNode as _},
};

use self::shared::{Access, UseClass};
use super::{Finding, Lint, dup, facts, frozen_collection, general_facts, test_code};
use crate::{check::Cx, diag::Applicability};

/// How many `as` casts, borrows and string conversions a literal's use is followed up through.
const USE_DEPTH: u32 = 4;
/// How many borrows and derefs a compared place is followed through.
const PLACE_DEPTH: u32 = 8;
/// How many parents a range bound is followed up to the `from_*_bytes` call that reads it.
const BYTES_DEPTH: usize = 6;
/// How many other functions a repeated literal's message names before it says how many more.
const SHOWN: usize = 4;
/// The shortest strings a one-character difference is a near-miss of: below it, two words one
/// letter apart are as often two words (`"get"`, `"set"`).
const NEAR_LEN: usize = 6;
/// The fewest distinct strings one place is matched against that make a closed set.
const SET_MIN: usize = 2;
/// The one number past the identities that is exempt, as the operand of `*`, `/`, `%`, a shift
/// or `pow`: doubling and halving.
const TWO: i128 = 2;

/// The calls that turn a value into another of the same text or number, followed through to the
/// use: `"x".to_owned()` is used where the `String` is.
const CONVERSIONS: &[&str] = &["to_owned", "to_string", "into", "as_ref", "as_str", "from"];
/// The calls whose string argument is a message for a person.
const MESSAGES: &[&str] = &["expect", "expect_err", "context"];
/// The calls that read an integer from its bytes.
const FROM_BYTES: &[&str] = &["from_le_bytes", "from_be_bytes", "from_ne_bytes"];
/// The methods of std's maps and sets whose first argument is a key.
const KEY_METHODS: &[&str] =
    &["get", "get_mut", "contains_key", "contains", "remove", "insert", "entry"];
/// std's maps and sets.
const MAPS: &[&str] = &["HashMap", "HashSet", "BTreeMap", "BTreeSet"];
/// std's `env` functions whose first argument is a variable's name.
const ENV_FNS: &[&str] = &["var", "var_os", "set_var", "remove_var"];
/// The methods of `Path` and `PathBuf` whose argument is a path or one of its components.
const PATH_METHODS: &[&str] =
    &["new", "join", "push", "with_extension", "with_file_name", "set_extension", "set_file_name"];
/// The calls that fill or set what a name names: the writer's side of a key, a variable or a
/// file ([`Access::Writes`]).
const WRITES: &[&str] = &[
    "insert", "entry", "set_var", "remove_var", "write", "create", "create_new", "create_dir",
    "create_dir_all", "rename", "copy", "hard_link",
];
/// The calls that look a name up: the reader's side ([`Access::Reads`]).
const READS: &[&str] = &[
    "get", "get_mut", "contains_key", "contains", "remove", "var", "var_os", "read", "read_to_string",
    "read_dir", "open", "metadata", "exists", "try_exists",
];
/// How many parents a formatted value is followed up to the `Err(..)` it is the error of:
/// `Err(format!(..))` is two, `bail!`'s `return Err(anyhow!(..).into())` five.
const ERR_DEPTH: usize = 6;

/// A literal's value: what two sites must share to be the same fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Value {
    Int(i128),
    /// An unsigned integer above `i128::MAX`.
    Big(u128),
    /// A float, by its `f64` bits.
    Float(u64),
    /// A string, trimmed.
    Str(Symbol),
}

impl Value {
    fn of(literal: &Literal) -> Option<Value> {
        Some(match literal {
            Literal::Int(it, _) => Value::Int(*it),
            Literal::Uint(it, _) => i128::try_from(*it).map_or(Value::Big(*it), Value::Int),
            Literal::Float(it, _) => {
                // The front end keeps a float's value as its digits, the suffix (`f32`) apart.
                let written = it.as_str().split('f').next().unwrap_or_default();
                // alloc: one float literal's digits
                let digits: String = written.chars().filter(|c| *c != '_').collect();
                Value::Float(digits.parse::<f64>().ok()?.to_bits())
            }
            Literal::String(it) => Value::Str(Symbol::intern(it.as_str().trim())),
            _ => return None,
        })
    }

    fn negated(self) -> Value {
        match self {
            Value::Int(it) => it.checked_neg().map_or(self, Value::Int),
            Value::Float(bits) => Value::Float((-f64::from_bits(bits)).to_bits()),
            other => other,
        }
    }

    fn int(self) -> Option<i128> {
        match self {
            Value::Int(it) => Some(it),
            _ => None,
        }
    }

    /// `0`, `1`, `-1`, `0.0`, `-0.0` and `1.0`: what a number is when it states nothing.
    fn is_identity(self) -> bool {
        match self {
            Value::Int(it) => (-1..=1).contains(&it),
            Value::Float(bits) => {
                [0.0_f64.to_bits(), (-0.0_f64).to_bits(), 1.0_f64.to_bits()].contains(&bits)
            }
            _ => false,
        }
    }

    /// Whether this is `2`, which an operand of `*`, `/`, `%`, a shift or `pow` doubles or halves
    /// with.
    fn is_two(self) -> bool {
        self == Value::Int(TWO)
    }
}

/// A literal's primitive type: what two sites must share, with the value, to be the same fact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Prim {
    Int(IntTy),
    Uint(UintTy),
    Float(FloatTy),
    Str,
}

impl Prim {
    fn of(ty: Ty<'_>) -> Option<Prim> {
        match ty.kind() {
            TyKind::Int(it) => Some(Prim::Int(it)),
            TyKind::Uint(it) => Some(Prim::Uint(it)),
            TyKind::Float(it) => Some(Prim::Float(it)),
            _ => None,
        }
    }

    fn name(self) -> String {
        // alloc: a finding's message part
        match self {
            Prim::Int(it) => it.bit_width().map_or_else(|| "isize".to_owned(), |bits| format!("i{bits}")),
            Prim::Uint(it) => it.bit_width().map_or_else(|| "usize".to_owned(), |bits| format!("u{bits}")),
            Prim::Float(it) => format!("f{}", it.bit_width()),
            Prim::Str => "&str".to_owned(),
        }
    }

    /// The bytes of a fixed-width integer.
    fn bytes(self) -> Option<u64> {
        let bits = match self {
            Prim::Int(it) => it.bit_width()?,
            Prim::Uint(it) => it.bit_width()?,
            _ => return None,
        };
        Some(bits / u64::from(u8::BITS))
    }
}

/// A place a literal is compared with that other bodies can compare with too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Place {
    Field(FieldId),
    Static(StaticId),
    /// The value of a method called with no arguments (`v.len()`).
    Method(FunctionId),
}

/// How a literal is used, the same in every body of the crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Role {
    /// The argument at `index` of a call of `f`, `self` being 0 for a method.
    Arg { f: FunctionId, index: u32 },
    /// An operand of a comparison with the place.
    Compare(Place),
}

/// What a body asks of the crate's literals.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Ask {
    /// The bodies with a number of this value and type in this use.
    Use(Role, Value, Prim),
    /// The bodies that write this string.
    Text(Symbol),
    /// The `const`s passed in this use, with their values.
    Consts(Role),
    /// The strings written in this use, and where.
    Spellings(Role),
    /// The enums whose discriminant indexes the array in this field.
    EnumIndex(FieldId),
    /// The string `const`s whose value is this string ([`named_index`]).
    Named(Symbol),
    /// The bodies that write this format template, normalised ([`template::normalise`]).
    Template(Symbol),
    /// The fieldless enums whose variants' names are this set ([`variant_set_key`]).
    VariantSet(Symbol),
}

/// The crate's answer to an [`Ask`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Answer {
    /// Each body, in the crate's walk order, and how many sites it has.
    bodies: Vec<(DefWithBodyId, u32)>,
    consts: Vec<(ConstId, Value, Prim)>,
    spellings: Vec<(Symbol, DefWithBodyId)>,
    enums: Vec<EnumId>,
    /// The `const`s that hold a string, in the crate's walk order.
    named: Vec<ConstId>,
    /// How many sites of a template are not the error of an `Err(..)`.
    outside_err: u32,
    /// The error types of the `Err(..)`s a template's sites are in, distinct.
    errors: Vec<AdtId>,
}

/// A place compared against strings in one body: a [`Place`] or a local.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SetPlace {
    Place(Place),
    Local(BindingId),
}

/// The item a string spells the name of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemName {
    Variant(EnumVariantId),
    Field(FieldId),
    Function(FunctionId),
    /// The type the method it is in is of.
    Type(AdtId),
    /// The crate it is in.
    Crate,
}

/// The convention that maps an item's name to the string spelling it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    AsWritten,
    Lower,
    Snake,
    Kebab,
    ScreamingSnake,
    Camel,
    Pascal,
    /// The same letters in another case, with no word boundary to say which.
    OtherCase,
}

impl Case {
    /// How a message says it: "in snake_case".
    fn said(self) -> &'static str {
        match self {
            Case::AsWritten => "as written",
            Case::Lower => "in lower case",
            Case::Snake => "in snake_case",
            Case::Kebab => "in kebab-case",
            Case::ScreamingSnake => "in SCREAMING_SNAKE_CASE",
            Case::Camel => "in camelCase",
            Case::Pascal => "in PascalCase",
            Case::OtherCase => "in another letter case",
        }
    }

    /// The convention `text` is written in.
    fn of(text: &str) -> Case {
        let lower = text.chars().any(|c| c.is_ascii_lowercase());
        let upper = text.chars().any(|c| c.is_ascii_uppercase());
        if text.contains('-') {
            Case::Kebab
        } else if text.contains('_') || !lower {
            if lower { Case::Snake } else { Case::ScreamingSnake }
        } else if !upper {
            Case::Lower
        } else if text.starts_with(|c: char| c.is_ascii_uppercase()) {
            Case::Pascal
        } else {
            Case::Camel
        }
    }
}

/// What is known of a literal beyond its use, the strongest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Known {
    Nothing,
    /// The argument of a `Duration` constructor.
    Duration,
    /// The length of the array `base` is, sliced or looped over.
    Len { base: ExprId },
    /// The size of the integer a `from_*_bytes` call reads.
    SizeOf(Prim),
    /// A slice of `len` bytes read as the integer `prim`, which is not that size.
    BytesDisagree { prim: Prim, len: u64 },
    /// An array repeat count equal to the variant count of the enum that indexes the array.
    VariantCount(EnumId),
    /// An array repeat count not equal to the variant count of the enum that indexes the array.
    VariantsDisagree(EnumId),
    /// An array repeat count of the field: what indexes it is asked of the crate.
    FieldLen(FieldId),
    ItemName(ItemName, Case),
}

/// One literal of a body that is not exempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Lit {
    /// The literal, or the `-` before it.
    at: ExprId,
    /// Where `at` is in the body's tree ([`hir::BodyAnalysis::expr_range`]).
    range: TextRange,
    value: Value,
    prim: Prim,
    role: Option<Role>,
    known: Known,
    /// What the program looks up or branches by it as (never [`UseClass::Const`] or
    /// [`UseClass::Matched`], which are not literals of a body).
    lookup: Option<UseClass>,
    access: Access,
    /// What a compared literal is compared with.
    other: Option<ExprId>,
    /// The place a compared string is compared with, for a closed set.
    set: Option<SetPlace>,
}

/// A `match` on strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Matched {
    scrutinee: ExprId,
    /// Where the scrutinee is in the body's tree.
    range: TextRange,
    role: Option<Role>,
    values: Vec<Symbol>,
}

/// A format template a macro wrote in the body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Template {
    /// The `format_args!` expression.
    at: ExprId,
    /// Normalised ([`template::normalise`]).
    text: Symbol,
    /// The error type of the `Err(..)` the formatted value is the error of: `Some(None)` where
    /// the type is not an ADT.
    error: Option<Option<AdtId>>,
}

/// A `match` or an array of strings that spells the name of every variant of an enum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WholeSet {
    /// The `match` or the array.
    at: ExprId,
    /// The enum, known for a `match`; an array asks the crate by [`Ask::VariantSet`].
    of: Option<EnumId>,
    /// The array's key for [`Ask::VariantSet`].
    key: Option<Symbol>,
    /// The string literals, each of one variant.
    names: Vec<ExprId>,
    case: Case,
}

/// An index of an array by an enum's discriminant, the array not of the enum's variant count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Indexed {
    index: ExprId,
    base: ExprId,
    of: EnumId,
    len: usize,
}

/// What [`literals`] finds in a body.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Literals {
    lits: Vec<Lit>,
    matched: Vec<Matched>,
    indexed: Vec<Indexed>,
    consts: Vec<(Role, ConstId, Value, Prim)>,
    enum_fields: Vec<(FieldId, EnumId)>,
    templates: Vec<Template>,
    whole_sets: Vec<WholeSet>,
}

/// The literals of the body of `def` that are not exempt, once per body and edit.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn literals(db: &Db, def: DefWithBodyId) -> Literals {
    let mut out = Literals::default();
    let named_home = matches!(
        def,
        DefWithBodyId::ConstId(_) | DefWithBodyId::StaticId(_) | DefWithBodyId::VariantId(_)
    );
    if named_home || test_code::is_test_def(db, def) {
        return out;
    }
    let analysis = hir::BodyAnalysis::new(db, hir::DefWithBody::from(def));
    let krate = hir::DefWithBody::from(def).module(db).krate(db);
    let walk = Walk {
        db,
        def,
        body: analysis.body(),
        analysis: &analysis,
        parents: facts::Parents::of(analysis.body()),
        crate_name: krate.display_name(db).map(|it| it.crate_name().symbol().clone()),
    };
    walk.gather(&mut out);
    out
}

/// The string `const`s of `krate` (not test code), by value, in the crate's walk order: what a
/// literal of the crate, or of a crate that depends on it, is told already names it. Its own
/// query, apart from [`magic_index`], so a dependent crate reads its dependencies' `const`s
/// and never their bodies' literals.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn named_index(db: &Db, krate: base_db::Crate) -> Index {
    let mut index = Index::default();
    if dup::index::is_test_target(db, krate) {
        return index;
    }
    for def in frozen_collection::crate_bodies(db, krate) {
        let DefWithBodyId::ConstId(konst) = def else { continue };
        if test_code::is_test_def(db, def) {
            continue;
        }
        let Some(Value::Str(text)) = const_value(db, konst) else { continue };
        if shared::is_prose(text.as_str()) {
            continue;
        }
        if let Some(answer) = index.answer(Ask::Named(text)) {
            answer.named.push(konst);
        }
    }
    index
}

/// The fieldless enums of `krate` with [`SET_MIN`] variants or more, by the key of their
/// variants' names ([`variant_set_key`]): what an array of strings is asked spells.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn variant_sets(db: &Db, krate: base_db::Crate) -> Index {
    let mut index = Index::default();
    for module in hir::Crate::from(krate).modules(db) {
        for item in module.declarations(db) {
            let hir::ModuleDef::Adt(hir::Adt::Enum(of)) = item else { continue };
            if of.is_data_carrying(db) {
                continue;
            }
            let variants = of.variants(db);
            if variants.len() < SET_MIN {
                continue;
            }
            let Some(key) = variant_set_key(variants.iter().map(|it| it.name(db).symbol().clone())) else {
                continue;
            };
            if let Some(answer) = index.answer(Ask::VariantSet(key))
                && !answer.enums.contains(&EnumId::from(of))
            {
                answer.enums.push(EnumId::from(of));
            }
        }
    }
    index
}

/// The key of a set of names that every spelling of them shares: each name's words in lower
/// case joined by `_`, sorted, joined by `,`. `None` for a set with a name of no words.
fn variant_set_key(names: impl Iterator<Item = Symbol>) -> Option<Symbol> {
    let mut keys: Vec<Vec<u8, Global>, Global> = Vec::new_in(Global);
    for name in names {
        let mut key = Vec::new_in(Global);
        for word in words(name.as_str()) {
            if !key.is_empty() {
                key.push(b'_');
            }
            key.extend(word.bytes().map(|it| it.to_ascii_lowercase()));
        }
        if key.is_empty() {
            return None;
        }
        keys.push(key);
    }
    keys.sort_unstable();
    let mut out: Vec<u8, Global> = Vec::new_in(Global);
    for (at, key) in keys.iter().enumerate() {
        if at > 0 {
            out.push(b',');
        }
        out.extend_from_slice(key);
    }
    Some(Symbol::intern(core::str::from_utf8(&out).ok()?))
}

/// Every [`Ask`] of a crate and its [`Answer`].
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Index(HashMap<Ask, Answer, Global>);

impl Index {
    fn answer(&mut self, ask: Ask) -> Option<&mut Answer> {
        if self.0.get(&ask).is_none() {
            self.0.insert(ask, Answer::default());
        }
        self.0.get_mut(&ask)
    }

    fn body(&mut self, ask: Ask, def: DefWithBodyId) {
        let Some(answer) = self.answer(ask) else { return };
        match answer.bodies.iter_mut().find(|(it, _)| *it == def) {
            Some((_, count)) => *count += 1,
            None => answer.bodies.push((def, 1)),
        }
    }
}

/// The literals of every body of `krate`, merged by [`Ask`], once per crate and edit.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn magic_index(db: &Db, krate: base_db::Crate) -> Index {
    let mut index = Index::default();
    for def in frozen_collection::crate_bodies(db, krate) {
        let found = literals(db, def);
        for lit in &found.lits {
            match (lit.value, lit.role) {
                (Value::Str(text), role) => {
                    index.body(Ask::Text(text), def);
                    if let Some(answer) = role.and_then(|it| index.answer(Ask::Spellings(it))) {
                        answer.spellings.push((text, def));
                    }
                }
                (value, Some(role)) => index.body(Ask::Use(role, value, lit.prim), def),
                (_, None) => {}
            }
        }
        for matched in &found.matched {
            for &text in &matched.values {
                index.body(Ask::Text(text), def);
                if let Some(answer) = matched.role.and_then(|it| index.answer(Ask::Spellings(it)))
                {
                    answer.spellings.push((text, def));
                }
            }
        }
        for &(role, konst, value, prim) in &found.consts {
            let Some(answer) = index.answer(Ask::Consts(role)) else { continue };
            if !answer.consts.contains(&(konst, value, prim)) {
                answer.consts.push((konst, value, prim));
            }
        }
        for &(field, of) in &found.enum_fields {
            let Some(answer) = index.answer(Ask::EnumIndex(field)) else { continue };
            if !answer.enums.contains(&of) {
                answer.enums.push(of);
            }
        }
        for template in &found.templates {
            index.body(Ask::Template(template.text), def);
            let Some(answer) = index.answer(Ask::Template(template.text)) else { continue };
            match template.error {
                None => answer.outside_err += 1,
                Some(Some(error)) if !answer.errors.contains(&error) => answer.errors.push(error),
                Some(_) => {}
            }
        }
    }
    index
}

/// One [`Ask`] of one crate: the key of [`magic_answer`].
#[blaze_query::interned(debug, unsafe(no_lifetime))]
pub(crate) struct MagicKey {
    #[returns(copy)]
    pub(crate) krate: base_db::Crate,
    #[returns(copy)]
    pub(crate) ask: Ask,
}

/// The answer to one [`Ask`]: what a body's lint reads, so an edit elsewhere in the crate that
/// leaves this answer as it was leaves the body's lint as it was.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn magic_answer(db: &Db, key: MagicKey) -> Answer {
    let (krate, ask) = (key.krate(db), key.ask(db));
    let index = match ask {
        Ask::Named(_) => named_index(db, krate),
        Ask::VariantSet(_) => variant_sets(db, krate),
        _ => magic_index(db, krate),
    };
    index.0.get(&ask).cloned().unwrap_or_default()
}

/// Where a literal is used: the expression it is used as (through casts, borrows and
/// conversions) and that expression's parent.
#[derive(Clone, Copy)]
struct Use {
    used: ExprId,
    parent: Option<ExprId>,
}

/// A body being read for its literals.
struct Walk<'a, 'db> {
    db: &'db Db,
    def: DefWithBodyId,
    body: &'db Body,
    analysis: &'a hir::BodyAnalysis<'db>,
    parents: facts::Parents,
    /// The name of the crate the body is in, which a string can spell.
    crate_name: Option<Symbol>,
}

impl<'db> Walk<'_, 'db> {
    fn gather(&self, out: &mut Literals) {
        self.templates(out);
        self.name_arrays(out);
        let mut in_pattern = Vec::new_in(Global);
        for (_, pat) in self.body.pats() {
            match pat {
                Pat::Lit(it) => in_pattern.push(it),
                Pat::Range { start, end, .. } => in_pattern.extend(start.into_iter().chain(end)),
                _ => {}
            }
        }
        let indexed = self.enum_indexes(out);
        for (id, expr) in self.body.exprs() {
            match expr {
                Expr::Literal(literal) if !in_pattern.contains(&id) => {
                    if let Some(lit) = self.literal(id, literal, &indexed) {
                        out.lits.push(lit);
                    }
                }
                Expr::Path(_) => self.const_arg(id, out),
                Expr::Match { expr: scrutinee, arms } => {
                    let mut values = Vec::new_in(Global);
                    for arm in arms {
                        self.string_pats(arm.pat, &mut values);
                    }
                    if values.len() >= SET_MIN
                        && self.written(id)
                        && !self.in_const(id)
                        && let Some(range) = self.analysis.expr_range(scrutinee)
                    {
                        let role = self.place(scrutinee).map(Role::Compare);
                        out.matched.push(Matched { scrutinee, range, role, values });
                    }
                    if let Some(whole) = self.match_spells_every_variant(id) {
                        out.whole_sets.push(whole);
                    }
                }
                _ => {}
            }
        }
    }

    /// Whether `id` is written in the body's own file, not by a macro.
    fn written(&self, id: ExprId) -> bool {
        let file = self.analysis.tree().1;
        self.analysis.source_map().expr_syntax(id).is_ok_and(|it| it.file_id == file)
    }

    /// The format templates of the body: each `format_args!` a macro wrote whose template is a
    /// string literal of [`template::TEMPLATE_WORDS_FLOOR`] words or more, normalised, with the
    /// error type of the `Err(..)` it is the error of.
    fn templates(&self, out: &mut Literals) {
        let map = self.analysis.source_map();
        // Lowering one `format_args!` makes several expressions (the `Arguments` call, its
        // pieces, its arguments) that all map back to its syntax: the template is read once,
        // at the outermost of them, so one written template is one site.
        let mut seen: Vec<_, Global> = Vec::new_in(Global);
        for (id, _) in self.body.exprs() {
            let Ok(source) = map.expr_syntax(id) else { continue };
            if source.value.kind() != SyntaxKind::FORMAT_ARGS_EXPR {
                continue;
            }
            if self.parents.up(id, usize::MAX).any(|it| map.expr_syntax(it).is_ok_and(|it| it == source)) {
                continue;
            }
            if seen.contains(&source) {
                continue;
            }
            let root = source.file_id.root(self.db);
            let at = source.value.syntax_node_ptr().text_range();
            let covering = match root.covering_element(at) {
                NodeOrToken::Node(it) => it,
                NodeOrToken::Token(it) => match it.parent() {
                    Some(it) => it,
                    None => continue,
                },
            };
            let Some(node) = covering
                .ancestors()
                .find(|it| it.kind() == SyntaxKind::FORMAT_ARGS_EXPR && it.text_range() == at)
            else {
                continue;
            };
            let Some(written) = ast::FormatArgsExpr::cast(node).and_then(|it| it.template()) else { continue };
            // A template that is not one string token (`concat!(..)`, a `const`) is not read.
            let Some(token) = written.syntax().first_token() else { continue };
            if token.kind() != SyntaxKind::STRING || written.syntax().text_range() != token.text_range() {
                continue;
            }
            let Some(text) = template::normalise(token.text()) else { continue };
            if template::words(text.as_str()) < template::TEMPLATE_WORDS_FLOOR {
                continue;
            }
            seen.push(source);
            out.templates.push(Template { at: id, text, error: self.err_of(id) });
        }
    }

    /// The error type of the `Err(..)` whose error `id` is part of, through [`ERR_DEPTH`]
    /// parents (`Err(format!(..))`, `bail!`'s `return Err(anyhow!(..))`): `Some(None)` where
    /// the error is not an ADT, `None` outside an `Err`.
    fn err_of(&self, id: ExprId) -> Option<Option<AdtId>> {
        for it in self.parents.up(id, ERR_DEPTH) {
            let Expr::Call { callee, .. } = self.body.expr(it) else { continue };
            let Some(ValueNs::EnumVariantId(variant)) =
                general_facts::value_of(self.db, self.def, self.body, callee)
            else {
                continue;
            };
            let variant = hir::EnumVariant::from(variant);
            let result = hir::Adt::Enum(variant.parent_enum(self.db));
            if variant.name(self.db).as_str() != "Err" || !general_facts::std_adt(self.db, result, "result", &["Result"]) {
                continue;
            }
            let error = self.ty(it).type_arguments().nth(1).and_then(|it| it.as_adt()).map(AdtId::from);
            return Some(error);
        }
        None
    }

    /// The `match` `id` when each arm's value is a string literal that spells its pattern's
    /// variant, one arm per variant of the enum: the whole set of names, written by hand.
    fn match_spells_every_variant(&self, id: ExprId) -> Option<WholeSet> {
        let Expr::Match { arms, .. } = self.body.expr(id) else { return None };
        if !self.written(id) || self.in_const(id) {
            return None;
        }
        let mut of = None;
        let mut seen: Vec<EnumVariantId, Global> = Vec::new_in(Global);
        let mut names = Vec::new();
        let mut case = None;
        for arm in arms.iter() {
            let Expr::Literal(Literal::String(text)) = self.body.expr(arm.expr) else { return None };
            let Some(VariantId::EnumVariantId(variant)) = self.analysis.infer().variant_resolution_for_pat(arm.pat) else {
                return None;
            };
            let variant_ = hir::EnumVariant::from(variant);
            let parent = EnumId::from(variant_.parent_enum(self.db));
            if *of.get_or_insert(parent) != parent || seen.contains(&variant) {
                return None;
            }
            let spelled = spelling(variant_.name(self.db).as_str(), text.as_str().trim())?;
            case.get_or_insert(spelled);
            seen.push(variant);
            names.push(arm.expr);
        }
        let of = of?;
        let count = hir::Enum::from(of).variants(self.db).len();
        (count >= SET_MIN && seen.len() == count).then(|| WholeSet { at: id, of: Some(of), key: None, names, case: case.unwrap_or(Case::AsWritten) })
    }

    /// The arrays of [`SET_MIN`] or more string literals the body writes, with the key of the
    /// set their texts spell ([`variant_set_key`]), which the crate is asked the enum of.
    fn name_arrays(&self, out: &mut Literals) {
        for (id, expr) in self.body.exprs() {
            let Expr::Array(Array::ElementList { elements, .. }) = expr else { continue };
            if elements.len() < SET_MIN || !self.written(id) || self.in_const(id) {
                continue;
            }
            let mut texts: Vec<Symbol, Global> = Vec::new_in(Global);
            for &it in elements.iter() {
                let Expr::Literal(Literal::String(text)) = self.body.expr(it) else { break };
                texts.push(Symbol::intern(text.as_str().trim()));
            }
            if texts.len() != elements.len() || texts.iter().any(|it| shared::is_prose(it.as_str())) {
                continue;
            }
            let Some(key) = variant_set_key(texts.iter().cloned()) else { continue };
            let case = Case::of(texts[0].as_str());
            out.whole_sets.push(WholeSet { at: id, of: None, key: Some(key), names: elements.to_vec(), case });
        }
    }

    /// Whether `id` is inside an inline `const { .. }` block.
    fn in_const(&self, id: ExprId) -> bool {
        self.parents.up(id, general_facts::UP).any(|it| matches!(self.body.expr(it), Expr::Const(_)))
    }

    /// The string literals `pat` matches, through `|`.
    fn string_pats(&self, pat: PatId, out: &mut Vec<Symbol, Global>) {
        match self.body.pat(pat) {
            Pat::Lit(it) => {
                if let Expr::Literal(Literal::String(text)) = self.body.expr(it) {
                    let text = Symbol::intern(text.as_str().trim());
                    if !out.contains(&text) {
                        out.push(text);
                    }
                }
            }
            Pat::Or(pats) => {
                for &it in pats {
                    self.string_pats(it, out);
                }
            }
            _ => {}
        }
    }

    /// The type of `expr`.
    fn ty(&self, expr: ExprId) -> hir::Type<'db> {
        self.analysis.ty(self.analysis.infer().expr_ty(expr))
    }

    /// The length of the fixed-size array `expr` is, through references.
    fn array_len(&self, expr: ExprId) -> Option<usize> {
        self.ty(expr).strip_references().as_array(self.db).map(|(_, len)| len)
    }

    /// The fieldless enum `expr` is a value of.
    fn fieldless_enum(&self, expr: ExprId) -> Option<hir::Enum> {
        match self.ty(expr).strip_references().as_adt()? {
            hir::Adt::Enum(it) if !it.is_data_carrying(self.db) => Some(it),
            _ => None,
        }
    }

    /// The locals indexed by a fieldless enum's discriminant (`counts[tag as usize]`), and the
    /// enum; adds the fields indexed so to `out`, and the indexes of an array whose length is
    /// not the enum's variant count.
    fn enum_indexes(&self, out: &mut Literals) -> Vec<(BindingId, EnumId), Global> {
        let mut locals = Vec::new_in(Global);
        for (id, expr) in self.body.exprs() {
            let Expr::Index { base, index } = expr else { continue };
            let Expr::Cast { expr: inner, .. } = self.body.expr(index) else { continue };
            let Some(of) = self.fieldless_enum(inner) else { continue };
            let variants = of.variants(self.db).len();
            match self.body.expr(base) {
                Expr::Path(_) => {
                    if let Some(ValueNs::LocalBinding(it)) =
                        general_facts::value_of(self.db, self.def, self.body, base)
                    {
                        locals.push((it, EnumId::from(of)));
                        continue;
                    }
                }
                Expr::Field { .. } => {
                    if let Some(either::Either::Left(field)) =
                        self.analysis.infer().field_resolution(base)
                    {
                        out.enum_fields.push((field, EnumId::from(of)));
                    }
                }
                _ => {}
            }
            let Some(len) = self.array_len(base) else { continue };
            if len != variants && self.written(id) {
                out.indexed.push(Indexed { index: id, base, of: EnumId::from(of), len });
            }
        }
        locals
    }

    /// The expression a literal at `at` is used as, through `as` casts, borrows and the
    /// [`CONVERSIONS`], and its parent.
    fn user(&self, mut at: ExprId) -> (ExprId, Option<ExprId>) {
        for _ in 0..USE_DEPTH {
            let Some(parent) = self.parents.of_expr(at) else { return (at, None) };
            let through = match self.body.expr(parent) {
                Expr::Cast { .. } | Expr::Ref { .. } => true,
                Expr::MethodCall { receiver, args: [], .. } => {
                    receiver == at && self.callee_name(parent).is_some_and(|it| CONVERSIONS.contains(&it))
                }
                Expr::Call { args: [_], .. } => {
                    self.callee_name(parent).is_some_and(|it| CONVERSIONS.contains(&it))
                }
                _ => false,
            };
            if !through {
                return (at, Some(parent));
            }
            at = parent;
        }
        (at, self.parents.of_expr(at))
    }

    fn callee(&self, call: ExprId) -> Option<general_facts::Callee<'db>> {
        general_facts::callee(self.db, self.analysis, call)
    }

    fn callee_name(&self, call: ExprId) -> Option<&'db str> {
        self.callee(call).map(|it| it.name)
    }

    /// The use of `at` that other bodies can share, with `parent` the expression it is used in.
    fn role(&self, at: ExprId, parent: ExprId) -> Option<Role> {
        match self.body.expr(parent) {
            Expr::Call { args, .. } => {
                let index = args.iter().position(|&it| it == at)?;
                Some(Role::Arg { f: self.callee(parent)?.f, index: u32::try_from(index).ok()? })
            }
            Expr::MethodCall { args, .. } => {
                let index = args.iter().position(|&it| it == at)?;
                // `self` is the method's argument 0, as it is when the method is called as a path.
                let index = u32::try_from(index).ok()?.checked_add(1)?;
                Some(Role::Arg { f: self.callee(parent)?.f, index })
            }
            Expr::BinaryOp { lhs, rhs, op: Some(BinaryOp::CmpOp(_)) } => {
                let other = if lhs == at { rhs } else { lhs };
                self.place(other).map(Role::Compare)
            }
            _ => None,
        }
    }

    /// The place `expr` reads: a field, a `static` or a method called with no arguments,
    /// through borrows and derefs.
    fn place(&self, mut expr: ExprId) -> Option<Place> {
        for _ in 0..PLACE_DEPTH {
            match self.body.expr(expr) {
                Expr::Ref { expr: inner, .. } | Expr::UnaryOp { expr: inner, op: UnaryOp::Deref } => {
                    expr = inner;
                }
                Expr::Field { .. } => {
                    return match self.analysis.infer().field_resolution(expr)? {
                        either::Either::Left(it) => Some(Place::Field(it)),
                        either::Either::Right(_) => None,
                    };
                }
                Expr::Path(_) => {
                    return match general_facts::value_of(self.db, self.def, self.body, expr)? {
                        ValueNs::StaticId(it) => Some(Place::Static(it)),
                        _ => None,
                    };
                }
                Expr::MethodCall { args: [], .. } => return self.callee(expr).map(|it| Place::Method(it.f)),
                _ => return None,
            }
        }
        None
    }

    /// [`Walk::place`], or the local `expr` names: what a body's closed set is of.
    fn set_place(&self, expr: ExprId) -> Option<SetPlace> {
        if let Some(it) = self.place(expr) {
            return Some(SetPlace::Place(it));
        }
        match general_facts::value_of(self.db, self.def, self.body, expr)? {
            ValueNs::LocalBinding(it) => Some(SetPlace::Local(it)),
            _ => None,
        }
    }

    /// The kind of the token the literal `id` is written as (`INT_NUMBER`, `STRING`, `BYTE`,
    /// ..), when the body's own file writes it.
    fn token_kind(&self, id: ExprId) -> Option<SyntaxKind> {
        let (root, file) = self.analysis.tree();
        let source = self.analysis.source_map().expr_syntax(id).ok()?;
        if source.file_id != file {
            return None;
        }
        match root.covering_element(source.value.syntax_node_ptr().text_range()) {
            NodeOrToken::Node(it) => Some(it.first_token()?.kind()),
            NodeOrToken::Token(it) => Some(it.kind()),
        }
    }

    /// The literal `id` when it is not exempt. A byte (`b'0'`) is a number to the front end and
    /// a character to the reader: only a number written as one is a number here.
    fn literal(&self, id: ExprId, literal: &Literal, indexed: &[(BindingId, EnumId)]) -> Option<Lit> {
        let value = match (self.token_kind(id)?, Value::of(literal)?) {
            (
                SyntaxKind::INT_NUMBER | SyntaxKind::FLOAT_NUMBER,
                it @ (Value::Int(_) | Value::Big(_) | Value::Float(_)),
            ) => it,
            (SyntaxKind::STRING, it @ Value::Str(_)) => it,
            _ => return None,
        };
        if self.in_const(id) {
            return None;
        }
        let (at, value) = match self.parents.of_expr(id).map(|it| (it, self.body.expr(it))) {
            Some((parent, Expr::UnaryOp { op: UnaryOp::Neg, .. })) => (parent, value.negated()),
            _ => (id, value),
        };
        let (used, parent) = self.user(at);
        let prim = match value {
            Value::Str(_) => Prim::Str,
            _ => Prim::of(self.analysis.infer().expr_ty(id))?,
        };
        let lit = Lit {
            at,
            range: self.analysis.expr_range(at)?,
            value,
            prim,
            role: parent.and_then(|it| self.role(used, it)),
            known: Known::Nothing,
            lookup: None,
            access: Access::Names,
            other: None,
            set: None,
        };
        let use_ = Use { used, parent };
        match value {
            Value::Str(text) => self.string(lit, text, use_),
            _ => self.number(lit, use_, indexed),
        }
    }

    /// A number when it is not exempt, with what is known of it.
    fn number(&self, lit: Lit, use_: Use, indexed: &[(BindingId, EnumId)]) -> Option<Lit> {
        let Use { used, parent } = use_;
        let value = lit.value;
        if value.is_identity() {
            return None;
        }
        let lit = |known| Lit { known, ..lit };
        let Some(parent) = parent else { return Some(lit(Known::Nothing)) };
        let n = value.int();
        match self.body.expr(parent) {
            Expr::BinaryOp {
                op:
                    Some(
                        BinaryOp::ArithOp(op)
                        | BinaryOp::Assignment { op: Some(op) },
                    ),
                ..
            } if value.is_two()
                && matches!(op, ArithOp::Mul | ArithOp::Div | ArithOp::Rem | ArithOp::Shl | ArithOp::Shr) =>
            {
                None
            }
            Expr::MethodCall { .. } if value.is_two() && self.callee_name(parent) == Some("pow") => None,
            Expr::Index { base, index } if index == used => {
                let len = self.array_len(base).and_then(|it| i128::try_from(it).ok());
                match (n, len) {
                    (Some(n), Some(len)) if n < len => None,
                    _ => Some(lit(Known::Nothing)),
                }
            }
            Expr::RecordLit { fields, .. } if self.is_range(parent) => {
                self.range_bound(used, n, parent, fields.len()).map(lit)
            }
            Expr::Array(Array::Repeat { repeat, .. }) if repeat == used => {
                Some(lit(self.repeat_count(n, parent, indexed)))
            }
            Expr::Call { .. } | Expr::MethodCall { .. } => {
                let duration = self.ty(parent).as_adt().is_some_and(|it| {
                    general_facts::std_adt(self.db, it, "time", &["Duration"])
                });
                Some(lit(if duration { Known::Duration } else { Known::Nothing }))
            }
            _ => Some(lit(Known::Nothing)),
        }
    }

    /// Whether `expr` is a range written as one (`a..b`), which the front end lowers to a
    /// struct literal of `Range`.
    fn is_range(&self, expr: ExprId) -> bool {
        self.analysis
            .source_map()
            .expr_syntax(expr)
            .is_ok_and(|it| it.value.kind() == SyntaxKind::RANGE_EXPR)
    }

    /// What a range bound `used` of value `n` is, the range `range` with `bounds` bounds; `None`
    /// when it is exempt: a bound of a slice of a fixed-size array below its length, which the
    /// length proves in range.
    fn range_bound(&self, used: ExprId, n: Option<i128>, range: ExprId, bounds: usize) -> Option<Known> {
        let Some(n) = n else { return Some(Known::Nothing) };
        let end = self.is_end(used, range);
        let parent = self.parents.of_expr(range);
        if let Some(Expr::Index { base, index }) = parent.map(|it| self.body.expr(it))
            && index == range
            && let Some(len) = self.array_len(base).and_then(|it| i128::try_from(it).ok())
        {
            return match n.cmp(&len) {
                std::cmp::Ordering::Less => None,
                std::cmp::Ordering::Equal if end => Some(Known::Len { base }),
                _ => Some(Known::Nothing),
            };
        }
        if end {
            if let Some(base) = self.looped_array(range, n) {
                return Some(Known::Len { base });
            }
            if let Some(known) = self.read_as_int(used, n, range, bounds) {
                return Some(known);
            }
        }
        Some(Known::Nothing)
    }

    /// Whether `used` is the end bound of `range`: its last field.
    fn is_end(&self, used: ExprId, range: ExprId) -> bool {
        match self.body.expr(range) {
            Expr::RecordLit { fields, .. } => fields.last().is_some_and(|it| it.expr == used),
            _ => false,
        }
    }

    /// The array a `for` loop over `range` indexes with its variable, when its length is `n`:
    /// `for i in 0..n { a[i] }`.
    fn looped_array(&self, range: ExprId, n: i128) -> Option<ExprId> {
        let (root, file) = self.analysis.tree();
        let source = self.analysis.source_map().expr_syntax(range).ok()?;
        if source.file_id != file {
            return None;
        }
        let at = source.value.syntax_node_ptr().text_range();
        let covering = match root.covering_element(at) {
            NodeOrToken::Node(it) => it,
            NodeOrToken::Token(it) => it.parent()?,
        };
        let node = covering
            .ancestors()
            .find(|it| it.kind() == SyntaxKind::RANGE_EXPR && it.text_range() == at)?;
        let for_ = ast::ForExpr::cast(node.parent()?)?;
        if for_.iterable()?.syntax().text_range() != at {
            return None;
        }
        let pat = for_.pat()?.syntax().text_range();
        let binding = self.body.pats().find_map(|(id, it)| match it {
            Pat::Bind { id: binding, .. } => {
                let range = self.analysis.source_map().pat_syntax(id).ok()?.value.syntax_node_ptr().text_range();
                (range == pat).then_some(binding)
            }
            _ => None,
        })?;
        self.body.exprs().find_map(|(_, expr)| {
            let Expr::Index { base, index } = expr else { return None };
            let by_it = matches!(
                general_facts::value_of(self.db, self.def, self.body, index),
                Some(ValueNs::LocalBinding(it)) if it == binding
            );
            let len = self.array_len(base).and_then(|it| i128::try_from(it).ok());
            (by_it && len == Some(n)).then_some(base)
        })
    }

    /// What the end bound `used` of value `end` of a slice that a `from_*_bytes` call reads is:
    /// its integer's size, or a disagreement with it.
    fn read_as_int(&self, used: ExprId, end: i128, range: ExprId, bounds: usize) -> Option<Known> {
        let start = match (bounds, self.body.expr(range)) {
            (1, _) => 0,
            (_, Expr::RecordLit { fields, .. }) => {
                let first = fields.first()?.expr;
                if first == used {
                    return None;
                }
                match self.body.expr(first) {
                    Expr::Literal(literal) => Value::of(literal)?.int()?,
                    _ => return None,
                }
            }
            _ => return None,
        };
        let call = self.parents.up(range, BYTES_DEPTH).find(|&it| {
            matches!(self.body.expr(it), Expr::Call { .. } | Expr::MethodCall { .. })
                && self.callee_name(it).is_some_and(|name| FROM_BYTES.contains(&name))
        })?;
        let prim = Prim::of(self.analysis.infer().expr_ty(call))?;
        let size = i128::from(prim.bytes()?);
        let len = end.checked_sub(start)?;
        match (len == size, start == 0) {
            (true, true) => Some(Known::SizeOf(prim)),
            (true, false) => None,
            (false, _) => Some(Known::BytesDisagree { prim, len: u64::try_from(len).ok()? }),
        }
    }

    /// What an array repeat count of value `n` is, the array `array`: the variant count of the
    /// enum that indexes the local it initializes, or the field it initializes.
    fn repeat_count(&self, n: Option<i128>, array: ExprId, indexed: &[(BindingId, EnumId)]) -> Known {
        let Some(n) = n else { return Known::Nothing };
        if let Some(binding) = self.let_of(array)
            && let Some(&(_, of)) = indexed.iter().find(|(it, _)| *it == binding)
        {
            let variants = i128::try_from(hir::Enum::from(of).variants(self.db).len()).ok();
            return match variants == Some(n) {
                true => Known::VariantCount(of),
                false => Known::VariantsDisagree(of),
            };
        }
        let parent = self.parents.of_expr(array);
        if let Some(record) = parent
            && let Expr::RecordLit { fields, .. } = self.body.expr(record)
            && let Some(field) = fields.iter().find(|it| it.expr == array)
            && let Some(VariantId::StructId(strukt)) =
                self.analysis.infer().variant_resolution_for_expr(record)
        {
            let variant = VariantId::StructId(strukt);
            if let Some(local_id) = variant.fields(self.db).field(&field.name) {
                return Known::FieldLen(FieldId { parent: variant, local_id });
            }
        }
        Known::Nothing
    }

    /// The local a `let` binds to `init` by itself (`let a = init;`).
    fn let_of(&self, init: ExprId) -> Option<BindingId> {
        self.body.exprs().find_map(|(_, expr)| {
            let Expr::Block { statements, .. } = expr else { return None };
            statements.into_iter().find_map(|it| match it {
                Statement::Let { pat, initializer: Some(at), .. } if at == init => {
                    match self.body.pat(pat) {
                        Pat::Bind { id, subpat: None } => Some(id),
                        _ => None,
                    }
                }
                _ => None,
            })
        })
    }

    /// A string when it is not exempt, with what is known of it and what it is looked up as.
    fn string(&self, mut lit: Lit, text: Symbol, use_: Use) -> Option<Lit> {
        let Use { used, parent } = use_;
        let s = text.as_str();
        if shared::is_prose(s) {
            return None;
        }
        let callee = parent.and_then(|it| self.callee(it));
        if callee.as_ref().is_some_and(|it| MESSAGES.contains(&it.name)) {
            return None;
        }
        let role = lit.role;
        lit.known = self.item_name(used, s, parent).map_or(Known::Nothing, |(item, case)| Known::ItemName(item, case));
        if let Some(parent) = parent {
            match self.body.expr(parent) {
                Expr::BinaryOp { lhs, rhs, op: Some(BinaryOp::CmpOp(CmpOp::Eq { .. })) } => {
                    let other = if lhs == used { rhs } else { lhs };
                    lit.lookup = Some(UseClass::Compared);
                    lit.access = Access::Reads;
                    lit.other = Some(other);
                    lit.set = self.set_place(other);
                    return Some(lit);
                }
                // `value["name"]`: a JSON or config object, or any map indexed by a name.
                Expr::Index { index, .. } if index == used => {
                    lit.lookup = Some(UseClass::Key);
                    lit.access = Access::Reads;
                    return Some(lit);
                }
                _ => {}
            }
        }
        let (Some(callee), Some(Role::Arg { index, .. })) = (callee, role) else { return Some(lit) };
        let top = general_facts::std_top(self.db, hir::Function::from(callee.f).module(self.db));
        let top = top.as_ref().map(|it| it.as_str());
        let of = |names: &[&str]| callee.self_adt.is_some_and(|adt| names.contains(&adt.name(self.db).as_str()));
        // A key method of std's maps, or of any type but a string (`str::contains` searches
        // text): blaze_hash's and papaya's maps, a JSON object, a config table.
        let std_map = top == Some("collections") && of(MAPS);
        let other_map = callee.name != "contains"
            && callee.self_adt.is_some_and(|adt| !general_facts::std_adt(self.db, adt, "string", &["String"]));
        lit.lookup = if index == 1 && KEY_METHODS.contains(&callee.name) && (std_map || other_map) {
            Some(UseClass::Key)
        } else if top == Some("env") && callee.self_adt.is_none() && ENV_FNS.contains(&callee.name) {
            Some(UseClass::Env)
        } else if top == Some("fs") || (top == Some("path") && of(&["Path", "PathBuf"]) && PATH_METHODS.contains(&callee.name)) {
            Some(UseClass::Path)
        } else {
            None
        };
        lit.access = if WRITES.contains(&callee.name) {
            Access::Writes
        } else if READS.contains(&callee.name) {
            Access::Reads
        } else {
            Access::Names
        };
        // An environment variable's or a path's name is fixed outside the program: the function,
        // type or crate around it is named after it, not the other way, so a rename of them must
        // not follow it (`fn home() { env::var("HOME") }`). Its finding is the lookup.
        if matches!(lit.lookup, Some(UseClass::Env | UseClass::Path))
            && matches!(lit.known, Known::ItemName(ItemName::Function(_) | ItemName::Type(_) | ItemName::Crate, _))
        {
            lit.known = Known::Nothing;
        }
        Some(lit)
    }

    /// The item `text` spells the name of at `used`, and in what convention: the variant of the
    /// `match` arm it is the value of, a field passed beside it in the same call or tuple, the
    /// function it is in, the type that function is a method of, or the crate.
    fn item_name(&self, used: ExprId, text: &str, parent: Option<ExprId>) -> Option<(ItemName, Case)> {
        let db = self.db;
        if let Some(parent) = parent {
            match self.body.expr(parent) {
                Expr::Match { arms, .. } => {
                    let arm = arms.iter().find(|it| it.expr == used)?;
                    let Some(VariantId::EnumVariantId(variant)) =
                        self.analysis.infer().variant_resolution_for_pat(arm.pat)
                    else {
                        return None;
                    };
                    let name = hir::EnumVariant::from(variant).name(db);
                    return spelling(name.as_str(), text).map(|case| (ItemName::Variant(variant), case));
                }
                Expr::Tuple { exprs: beside } | Expr::Call { args: beside, .. } | Expr::MethodCall { args: beside, .. } => {
                    for &it in beside {
                        let mut at = it;
                        while let Expr::Ref { expr, .. } = self.body.expr(at) {
                            at = expr;
                        }
                        if let Expr::Field { name, .. } = self.body.expr(at)
                            && let Some(case) = spelling(name.as_str(), text)
                            && let Some(either::Either::Left(field)) =
                                self.analysis.infer().field_resolution(at)
                        {
                            return Some((ItemName::Field(field), case));
                        }
                    }
                }
                _ => {}
            }
        }
        if let DefWithBodyId::FunctionId(f) = self.def {
            if let Some(case) = spelling(FunctionSignature::of(db, f).name.as_str(), text) {
                return Some((ItemName::Function(f), case));
            }
            if let Some(assoc) = hir::AsAssocItem::as_assoc_item(hir::Function::from(f), db)
                && let hir::AssocItemContainer::Impl(it) = assoc.container(db)
                && let Some(adt) = it.self_ty(db).as_adt()
                && let Some(case) = spelling(adt.name(db).as_str(), text)
            {
                return Some((ItemName::Type(AdtId::from(adt)), case));
            }
        }
        let krate = self.crate_name.as_ref()?;
        spelling(krate.as_str(), text).map(|case| (ItemName::Crate, case))
    }

    /// A `const` passed as an argument: what [`Ask::Consts`] answers.
    fn const_arg(&self, id: ExprId, out: &mut Literals) {
        let Some(ValueNs::ConstId(konst)) = general_facts::value_of(self.db, self.def, self.body, id) else {
            return;
        };
        let (used, Some(parent)) = self.user(id) else { return };
        let Some(role @ Role::Arg { .. }) = self.role(used, parent) else { return };
        let Some(prim) = Prim::of(self.analysis.infer().expr_ty(id)) else { return };
        let Some(value) = const_value(self.db, konst) else { return };
        out.consts.push((role, konst, value, prim));
    }
}

/// The value of a `const` whose initializer is a literal or a negated one.
fn const_value(db: &Db, konst: ConstId) -> Option<Value> {
    let body = Body::of(db, DefWithBodyId::ConstId(konst));
    match body.expr(body.root_expr()) {
        Expr::Literal(literal) => Value::of(literal),
        Expr::UnaryOp { expr, op: UnaryOp::Neg } => match body.expr(expr) {
            Expr::Literal(literal) => Value::of(literal).map(Value::negated),
            _ => None,
        },
        _ => None,
    }
}

/// The convention in which `text` spells `name`, if it does: as written, in another letter
/// case, or the same words joined another way (`DepInfo` is `"dep_info"` in snake_case,
/// `"dep-info"` in kebab-case, `"DEP_INFO"` in SCREAMING_SNAKE_CASE, `"depInfo"` in camelCase;
/// `dep_info` is `"DepInfo"` in PascalCase).
fn spelling(name: &str, text: &str) -> Option<Case> {
    let name = name.strip_prefix("r#").unwrap_or(name);
    if text == name {
        return Some(Case::AsWritten);
    }
    if same_words(name, text) {
        return Some(Case::of(text));
    }
    text.eq_ignore_ascii_case(name).then_some(Case::OtherCase)
}

/// Whether `a` and `b` are the same words, letter case and the way they are joined aside.
fn same_words(a: &str, b: &str) -> bool {
    let (mut a, mut b) = (words(a), words(b));
    loop {
        match (a.next(), b.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if x.eq_ignore_ascii_case(y) => {}
            _ => return false,
        }
    }
}

/// The words of a name or a key: split at `-`, `_` and the start of a capital that follows a
/// lower-case letter or a digit (`"buildId"`, `"build_id"` and `"Build-Id"` are `build`, `id`).
fn words(text: &str) -> impl Iterator<Item = &str> {
    let bytes = text.as_bytes();
    let mut at = 0;
    std::iter::from_fn(move || {
        while at < bytes.len() && matches!(bytes[at], b'-' | b'_') {
            at += 1;
        }
        if at >= bytes.len() {
            return None;
        }
        let start = at;
        at += 1;
        while at < bytes.len() {
            let (before, here) = (bytes[at - 1], bytes[at]);
            if matches!(here, b'-' | b'_')
                || (here.is_ascii_uppercase() && (before.is_ascii_lowercase() || before.is_ascii_digit()))
            {
                break;
            }
            at += 1;
        }
        // Splits are at ASCII bytes, so both ends are on character boundaries.
        text.get(start..at)
    })
}

/// How `a` and `b` differ when they are likely one name spelled twice: only by case, only by
/// `-`, `_` and case, by the convention the words are joined in (`"build_id"`, `"buildId"`), by a
/// plural `s`, or by one character that is not a digit, both [`NEAR_LEN`] long or more.
fn near(a: &str, b: &str) -> Option<&'static str> {
    if a == b {
        return None;
    }
    if a.eq_ignore_ascii_case(b) {
        return Some("case");
    }
    let fold = |it: &str| -> String {
        // alloc: one spelling folded
        it.chars().map(|c| if c == '-' { '_' } else { c.to_ascii_lowercase() }).collect()
    };
    let (fa, fb) = (fold(a), fold(b));
    if fa == fb {
        return Some("`-` and `_`");
    }
    if same_words(a, b) {
        return Some("the case convention (`_`, `-` or capitals between words)");
    }
    if fa.strip_suffix('s') == Some(fb.as_str()) || fb.strip_suffix('s') == Some(fa.as_str()) {
        return Some("a plural `s`");
    }
    let long = a.chars().count() >= NEAR_LEN && b.chars().count() >= NEAR_LEN;
    (long && one_edit(a, b)).then_some("one character")
}

/// Whether one insertion, deletion or substitution of a character that is not a digit turns
/// `a` into `b`.
fn one_edit(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect()); // alloc: two short strings' characters
    let (short, long) = if a.len() <= b.len() { (&a, &b) } else { (&b, &a) };
    if long.len() - short.len() > 1 {
        return false;
    }
    let prefix = short.iter().zip(long.iter()).take_while(|(x, y)| x == y).count();
    let edited = match long.get(prefix) {
        Some(it) => *it,
        None => return false,
    };
    if edited.is_ascii_digit() || short.get(prefix).is_some_and(char::is_ascii_digit) {
        return false;
    }
    let rest = if short.len() == long.len() { prefix + 1 } else { prefix };
    short.get(rest..) == long.get(prefix + 1..)
}

/// A fix of one finding.
struct Fix {
    message: &'static str,
    parts: Vec<(TextRange, String)>,
    applicability: Applicability,
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if ![Lint::MagicNumber, Lint::MagicString, Lint::RepeatedFormat].iter().any(|&it| cx.on_blaze(it)) {
        return;
    }
    let found = find(cx);
    for (finding, fix) in found {
        let parts = fix.map(|it| {
            let parts: Vec<_> = it.parts.into_iter().map(|(at, text)| (cx.span(at), text)).collect(); // alloc: one fix's parts
            (it.message, parts, it.applicability)
        });
        let diagnostic = facts::report_one(cx, finding);
        if let Some((message, parts, applicability)) = parts {
            diagnostic.multipart_suggestion(message, parts, applicability);
        }
    }
}

/// The report of one body.
struct Report<'a, 'c, 't> {
    cx: &'a Cx<'c, 't>,
    db: &'c Db,
    def: DefWithBodyId,
    krate: base_db::Crate,
    analysis: &'a hir::BodyAnalysis<'c>,
    numbers: bool,
    strings: bool,
    formats: bool,
}

fn find(cx: &Cx<'_, '_>) -> Vec<(Finding, Option<Fix>)> {
    // alloc: the findings of one body, reported at the edge
    let mut out = Vec::new();
    let db = cx.db();
    let Some(analysis) = cx.analysis() else { return out };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return out };
    if test_code::is_test_code(cx, cx.root()) {
        return out;
    }
    let report = Report {
        cx,
        db,
        def,
        krate: cx.krate().base(),
        analysis,
        numbers: cx.on_blaze(Lint::MagicNumber),
        strings: cx.on_blaze(Lint::MagicString),
        formats: cx.on_blaze(Lint::RepeatedFormat),
    };
    let found = literals(db, def);
    report.sets(found, &mut out);
    if report.strings {
        for whole in &found.whole_sets {
            out.extend(report.whole_set(whole));
        }
    }
    if report.formats {
        for template in &found.templates {
            out.extend(report.template(template));
        }
    }
    for lit in &found.lits {
        let one = match lit.value {
            Value::Str(text) if report.strings => report.string(lit, text, found),
            Value::Str(_) => None,
            _ if report.numbers => report.number(lit),
            _ => None,
        };
        out.extend(one);
    }
    if report.numbers {
        for indexed in &found.indexed {
            out.extend(report.indexed(indexed));
        }
    }
    out
}

impl<'c> Report<'_, 'c, '_> {
    fn ask(&self, ask: Ask) -> &'c Answer {
        magic_answer(self.db, MagicKey::new(self.db, self.krate, ask))
    }

    fn range(&self, expr: ExprId) -> Option<TextRange> {
        self.analysis.expr_range(expr)
    }

    fn text(&self, expr: ExprId) -> &str {
        self.range(expr).map_or("", |it| self.cx.snippet(it))
    }

    /// The node of the linted tree at `range`, where a name is resolved for a fix.
    fn node_at(&self, range: TextRange) -> ast::Node<'_> {
        let root = self.cx.root();
        match root.covering_element(range) {
            NodeOrToken::Node(it) => it,
            NodeOrToken::Token(it) => it.parent().unwrap_or(root),
        }
    }

    fn finding(&self, lint: Lint, at: ExprId, message: String, note: &str) -> Option<Finding> {
        let range = self.range(at)?;
        Some(Finding { lint, range, message, note: Some(note.to_owned()) }) // alloc: a finding's note
    }

    /// The other bodies of an answer, as a message lists them: "`a`, `b` and 3 more".
    fn others(&self, answer: &Answer) -> (String, u32) {
        let total = answer.bodies.iter().map(|(_, it)| *it).sum();
        let named: Vec<String> = answer // alloc: a finding's message part
            .bodies
            .iter()
            .filter(|(it, _)| *it != self.def)
            .map(|(it, _)| body_name(self.db, *it))
            .collect();
        let mut said = named.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", "); // alloc: a finding's message part
        if named.len() > SHOWN {
            said = format!("{said} and {} more", named.len() - SHOWN);
        }
        (said, total)
    }

    fn number(&self, lit: &Lit) -> Option<(Finding, Option<Fix>)> {
        const LINT: Lint = Lint::MagicNumber;
        const NOTE: &str = "a number that states a fact about something else (a size, a count, a limit, a duration, a protocol field, a tuning choice) has one home: derived from what it depends on, or a named `const` used everywhere the fact is used";
        let text = self.text(lit.at).to_owned(); // alloc: a finding's message part
        let range = self.range(lit.at)?;
        let derived = |message: String, with: String, applicability| {
            let fix = Fix { message: "derive it", parts: vec![(range, with)], applicability }; // alloc: one part
            Some((self.finding(LINT, lit.at, message, NOTE)?, Some(fix)))
        };
        match lit.known {
            Known::BytesDisagree { prim, len } => {
                let name = prim.name();
                let size = prim.bytes()?;
                let message = format!(
                    "this slice is {len} bytes and a `{name}` is {size}: reading it with `{name}::from_*_bytes` always fails; derive the length, `size_of::<{name}>()`"
                );
                return Some((self.finding(LINT, lit.at, message, NOTE)?, None));
            }
            Known::VariantsDisagree(of) => return self.variants(lit, &text, of, false),
            Known::FieldLen(field) => {
                let enums = &self.ask(Ask::EnumIndex(field)).enums;
                if let [of] = enums.as_slice() {
                    let count = hir::Enum::from(*of).variants(self.db).len();
                    let agrees = lit.value.int().and_then(|it| usize::try_from(it).ok()) == Some(count);
                    return self.variants(lit, &text, *of, agrees);
                }
            }
            Known::VariantCount(of) => return self.variants(lit, &text, of, true),
            Known::Len { base } => {
                let base_text = self.text(base);
                let message = format!("`{text}` is the length of `{base_text}`: derive it, `{base_text}.len()`");
                let applicability = match self.is_place(base) {
                    true => Applicability::MachineApplicable,
                    false => Applicability::MaybeIncorrect,
                };
                return derived(message, format!("{base_text}.len()"), applicability);
            }
            Known::SizeOf(prim) => {
                let name = prim.name();
                let message = format!("`{text}` is the size of `{name}`: derive it, `size_of::<{name}>()`");
                return derived(message, format!("size_of::<{name}>()"), Applicability::MaybeIncorrect);
            }
            Known::Nothing | Known::Duration | Known::ItemName(..) => {}
        }
        if let Some(role) = lit.role {
            if let Some((resolves, name)) = self.one_const(lit, role, range) {
                let message = format!(
                    "`{text}` is the value of `{name}`, {} elsewhere in the crate: derive it, `{name}`",
                    self.role_said(role)
                );
                let applicability = Applicability::MaybeIncorrect;
                return match resolves {
                    true => derived(message, name, applicability),
                    false => Some((self.finding(LINT, lit.at, message, NOTE)?, None)),
                };
            }
            let answer = self.ask(Ask::Use(role, lit.value, lit.prim));
            let (others, total) = self.others(answer);
            if total > 1 {
                let name = proposed(self.db, role, lit.known == Known::Duration);
                let message = format!(
                    "`{text}` is {} here and in {others} ({total} places): name it once, `const {name}: {} = {text};`, with a doc comment saying why {text}",
                    self.role_said(role),
                    match lit.known == Known::Duration {
                        true => "Duration".to_owned(), // alloc: a finding's message part
                        false => lit.prim.name(),
                    },
                );
                return Some((self.finding(LINT, lit.at, message, NOTE)?, None));
            }
        }
        let message = match lit.known {
            Known::Duration => format!(
                "`{text}` is a duration chosen here: name this duration, a `const` of type `Duration` whose doc comment says why {text}"
            ),
            _ => format!("`{text}` is a magic number: name it, a `const` whose doc comment says why {text}, or derive it from what it depends on"),
        };
        Some((self.finding(LINT, lit.at, message, NOTE)?, None))
    }

    /// Whether `expr` is a place that can be read again with no effect: a path, or fields and
    /// derefs of one.
    fn is_place(&self, mut expr: ExprId) -> bool {
        let body = self.analysis.body();
        for _ in 0..PLACE_DEPTH {
            match body.expr(expr) {
                Expr::Path(_) => return true,
                Expr::Field { expr: inner, .. }
                | Expr::UnaryOp { expr: inner, op: UnaryOp::Deref } => expr = inner,
                _ => return false,
            }
        }
        false
    }

    /// The one `const` of the same value and type passed in `role` elsewhere in the crate, and
    /// whether its name resolves to it at `range`.
    fn one_const(&self, lit: &Lit, role: Role, range: TextRange) -> Option<(bool, String)> {
        let mut same = self.ask(Ask::Consts(role)).consts.iter().filter(|(_, value, prim)| *value == lit.value && *prim == lit.prim);
        let (konst, _, _) = *same.next()?;
        if same.next().is_some() {
            return None;
        }
        let konst = hir::Const::from(konst);
        let name = konst.name(self.db)?.as_str().to_owned(); // alloc: a finding's message part
        let resolves = matches!(
            self.cx.resolve_name(self.node_at(range), &name),
            Some(hir::PathResolution::Def(hir::ModuleDef::Const(it))) if it == konst
        );
        Some((resolves, name))
    }

    /// The finding of an array length beside the enum indexing the array, `agrees` when it is
    /// the enum's variant count.
    fn variants(&self, lit: &Lit, text: &str, of: EnumId, agrees: bool) -> Option<(Finding, Option<Fix>)> {
        const LINT: Lint = Lint::MagicNumber;
        let of = hir::Enum::from(of);
        let name = of.name(self.db).as_str().to_owned(); // alloc: a finding's message part
        let count = of.variants(self.db).len();
        let derivation = format!("core::mem::variant_count::<{name}>()");
        let note = "an array indexed by an enum's discriminant is as long as the enum has variants: derived, a variant added or removed changes both";
        if !agrees {
            let message = format!(
                "`{text}` is the length of an array indexed by `{name}`, which has {count} variants: derive it, `{derivation}`, so the two cannot disagree"
            );
            return Some((self.finding(LINT, lit.at, message, note)?, None));
        }
        let message = format!("`{text}` is the number of variants of `{name}`: derive it, `{derivation}`");
        let range = self.range(lit.at)?;
        let feature = self.cx.features().is_enabled(&Symbol::intern("variant_count"));
        let resolves = matches!(
            self.cx.resolve_name(self.node_at(range), &name),
            Some(hir::PathResolution::Def(hir::ModuleDef::Adt(hir::Adt::Enum(it)))) if it == of
        );
        let fix = (feature && resolves).then(|| Fix {
            message: "derive it",
            parts: vec![(range, derivation)], // alloc: one part
            applicability: Applicability::MachineApplicable,
        });
        Some((self.finding(LINT, lit.at, message, note)?, fix))
    }

    /// An index by an enum's discriminant of an array that is not of its variant count.
    fn indexed(&self, indexed: &Indexed) -> Option<(Finding, Option<Fix>)> {
        let of = hir::Enum::from(indexed.of);
        let name = of.name(self.db);
        let message = format!(
            "`{}` has {} elements and is indexed by `{}`, which has {} variants: derive its length from `{}`, `core::mem::variant_count::<{}>()`",
            self.text(indexed.base),
            indexed.len,
            name.as_str(),
            of.variants(self.db).len(),
            name.as_str(),
            name.as_str(),
        );
        let note = "an array indexed by an enum's discriminant is as long as the enum has variants: derived, a variant added or removed changes both";
        Some((self.finding(Lint::MagicNumber, indexed.index, message, note)?, None))
    }

    /// How a role reads in a message: "passed to `with_capacity`'s `capacity`", "compared with
    /// `len`".
    fn role_said(&self, role: Role) -> String {
        let db = self.db;
        // alloc: a finding's message part
        match role {
            Role::Arg { f, index } => {
                let name = FunctionSignature::of(db, f).name.as_str().to_owned();
                match param_name(db, f, index) {
                    Some(param) => format!("passed to `{name}`'s `{param}`"),
                    None => format!("passed to `{name}`"),
                }
            }
            Role::Compare(Place::Field(it)) => format!("compared with the field `{}`", hir::Field::from(it).name(db).as_str()),
            Role::Compare(Place::Static(it)) => {
                format!("compared with `{}`", hir::Static::from(it).name(db).as_str())
            }
            Role::Compare(Place::Method(it)) => {
                format!("compared with `{}()`", FunctionSignature::of(db, it).name.as_str())
            }
        }
    }

    fn string(&self, lit: &Lit, text: Symbol, found: &Literals) -> Option<(Finding, Option<Fix>)> {
        const LINT: Lint = Lint::MagicString;
        const NOTE: &str = "a string the program branches or looks up by names a thing: its home is one `const`, or a type (an enum where the set is closed, a newtype over an interned name) the compiler checks every use of";
        let s = text.as_str();
        let db = self.db;
        if found.whole_sets.iter().any(|it| it.names.contains(&lit.at) && self.whole_enum(it).is_some()) {
            // The `match` or array is the finding, once.
            return None;
        }
        if let Some(named) = self.named(lit, text) {
            return Some(named);
        }
        if let Known::ItemName(item, case) = lit.known {
            let (what, name, how) = match item {
                ItemName::Variant(it) => {
                    let variant = hir::EnumVariant::from(it);
                    let name = variant.name(db).as_str().to_owned(); // alloc: a finding's message part
                    (format!("the variant `{}::{name}`", variant.parent_enum(db).name(db).as_str()), name, "reflection over the enum's variants")
                }
                ItemName::Field(it) => {
                    let name = hir::Field::from(it).name(db).as_str().to_owned(); // alloc: a finding's message part
                    (format!("the field `{name}` beside it"), name, "reflection over the type's fields")
                }
                ItemName::Function(it) => {
                    let name = FunctionSignature::of(db, it).name.as_str().to_owned(); // alloc: a finding's message part
                    (format!("the function it is in, `{name}`"), name, "`core::any::type_name` or reflection")
                }
                ItemName::Type(it) => {
                    let name = hir::Adt::from(it).name(db).as_str().to_owned(); // alloc: a finding's message part
                    (format!("the type its method is of, `{name}`"), name, "`core::any::type_name::<Self>()` or reflection")
                }
                ItemName::Crate => {
                    let name = self.cx.krate().display_name(db).map(|it| it.to_string()).unwrap_or_default();
                    (format!("its crate, `{name}`"), name, "`env!(\"CARGO_PKG_NAME\")`")
                }
            };
            let convention = match case {
                Case::AsWritten => String::new(),
                other => format!(" {} (`{name}` is `\"{s}\"`)", other.said()),
            };
            let message = format!("`\"{s}\"` spells the name of {what}{convention}: derive it ({how}), so a rename follows it");
            return Some((self.finding(LINT, lit.at, message, NOTE)?, None));
        }
        if let Some(role) = lit.role {
            for &(other, at) in &self.ask(Ask::Spellings(role)).spellings {
                if at == self.def {
                    continue;
                }
                let Some(how) = near(s, other.as_str()) else { continue };
                let message = format!(
                    "`\"{s}\"` differs from `\"{}\"`, {} in {}, only by {how}: one name spelled twice or a typo; make them one `const`, or an enum where the set is closed",
                    other.as_str(),
                    self.role_said(role),
                    body_name(db, at),
                );
                return Some((self.finding(LINT, lit.at, message, NOTE)?, None));
            }
        }
        let answer = self.ask(Ask::Text(text));
        let (others, total) = self.others(answer);
        if total > 1 {
            let message = match others.is_empty() {
                true => format!("`\"{s}\"` is written {total} times here: name it once, a `const` (or a variant, where it is one of a closed set)"),
                false => format!("`\"{s}\"` is written here and in {others} ({total} places): name it once, a `const` (or a variant, where it is one of a closed set)"),
            };
            return Some((self.finding(LINT, lit.at, message, NOTE)?, None));
        }
        let in_set = lit.set.is_some_and(|place| self.set_values(found, place).len() >= SET_MIN);
        let message = match lit.lookup? {
            UseClass::Compared if in_set => return None,
            UseClass::Compared => format!(
                "`\"{s}\"` is compared with `{}`: name it, a `const`; where that value is one of a closed set, make it an enum",
                lit.other.map_or("", |it| self.text(it))
            ),
            UseClass::Key => format!("`\"{s}\"` is a map key: name it, a `const`; a fixed set of keys is a struct's fields or an enum"),
            UseClass::Env => format!("`\"{s}\"` names an environment variable: name it, a `const` beside the code that owns the setting, or read it once into a field of the typed settings"),
            UseClass::Path => format!("`\"{s}\"` is a file name or path component: name it, a `const` beside the code that owns the layout"),
            // Not a literal of a body: a `const`'s value and a `match`'s patterns are rows of
            // the crate's, never here.
            UseClass::Const | UseClass::Matched => return None,
        };
        Some((self.finding(LINT, lit.at, message, NOTE)?, None))
    }

    /// The `const` of the crate, or else of a local dependency (in the order a walk from the
    /// crate reaches them), that already holds `text`: the literal is a second copy of it. A
    /// dependency's is asked of only for a string the program looks up or branches by, or one
    /// that spells the `const`'s own name (`"BLAZE_HOME"` for `BLAZE_HOME`): a short word two
    /// unrelated crates both happen to hold is not one fact. The fix writes the path that
    /// names it from here, machine-applicable, where there is one.
    fn named(&self, lit: &Lit, text: Symbol) -> Option<(Finding, Option<Fix>)> {
        const NOTE: &str = "a string a `const` already holds has its home there: a second literal is a copy that a change to the `const` leaves behind";
        let db = self.db;
        let own = self.ask(Ask::Named(text)).named.first().copied();
        let konst = match own {
            Some(it) => it,
            None => dup::index::local_deps(db, self.krate).iter().find_map(|&dep| {
                let answer = magic_answer(db, MagicKey::new(db, dep, Ask::Named(text)));
                answer.named.iter().copied().find(|&it| {
                    lit.lookup.is_some()
                        || hir::Const::from(it).name(db).is_some_and(|name| spelling(name.as_str(), text.as_str()).is_some())
                })
            })?,
        };
        let konst = hir::Const::from(konst);
        let module = self.analysis.def().module(db);
        let config = hir::FindPathConfig {
            prefer_no_std: false,
            prefer_prelude: false,
            prefer_absolute: false,
            allow_unstable: true,
        };
        let item = hir::ItemInNs::Values(hir::ModuleDef::Const(konst));
        let path = module.find_path(db, item, config).map(|it| it.display(db, self.cx.edition()).to_string());
        let shown = match &path {
            Some(it) => it.clone(), // clone: the message's copy of the fix's text
            None => hir::ModuleDef::Const(konst).canonical_path(db, self.cx.edition())?,
        };
        let message = match path {
            Some(_) => format!("`{shown}` already names this; use it"),
            None => format!("`{shown}` already names this, but is not visible here: make it `pub` (or move it where both can see it) and use it"),
        };
        let range = self.range(lit.at)?;
        let fix = path.map(|with| Fix {
            message: "use the `const`",
            parts: vec![(range, with)], // alloc: one part
            applicability: Applicability::MachineApplicable,
        });
        Some((self.finding(Lint::MagicString, lit.at, message, NOTE)?, fix))
    }

    /// The enum a whole set of names spells: a `match`'s own, or the one enum of the crate
    /// whose variants an array's strings are.
    fn whole_enum(&self, whole: &WholeSet) -> Option<EnumId> {
        if let Some(of) = whole.of {
            return Some(of);
        }
        match self.ask(Ask::VariantSet(whole.key?)).enums.as_slice() {
            [one] => Some(*one),
            _ => None,
        }
    }

    /// A `match` or an array that spells every variant's name: one finding, whose fix is the
    /// reflection form where the crate has blaze's compile-time reflection on, else one `const`
    /// table beside the enum.
    fn whole_set(&self, whole: &WholeSet) -> Option<(Finding, Option<Fix>)> {
        const NOTE: &str = "names written once per variant by hand are a second list of the variants: a variant added or renamed leaves the list behind, and nothing but a reader checks they agree";
        let of = hir::Enum::from(self.whole_enum(whole)?);
        let name = of.name(self.db);
        let name = name.as_str();
        let what = match whole.of {
            Some(_) => "this `match`",
            None => "this array",
        };
        let case = match whole.case {
            Case::AsWritten => String::new(),
            other => format!(" {}", other.said()),
        };
        let message = match self.cx.features().blaze_comptime {
            true => format!(
                "{what} spells the name of every variant of `{name}`{case}: derive the names, `for const` over `{name}`'s variants, so a variant added or renamed is named with it"
            ),
            false => format!(
                "{what} spells the name of every variant of `{name}`{case}: keep the names in one `const` table beside `{name}`, indexed by its discriminant, so a variant added or renamed is named in the same place"
            ),
        };
        Some((self.finding(Lint::MagicString, whole.at, message, NOTE)?, None))
    }

    /// A format template written in two or more places of the crate.
    fn template(&self, template: &Template) -> Option<(Finding, Option<Fix>)> {
        const NOTE: &str = "one sentence written in several places is one message kept in several copies: a change to one leaves the others saying the old thing, and nothing finds every place that reports this case";
        let answer = self.ask(Ask::Template(template.text));
        let (others, total) = self.others(answer);
        if total < 2 {
            return None;
        }
        let owner = match (answer.outside_err, answer.errors.as_slice()) {
            (0, [one]) => format!(
                "a variant of `{}`, the error every place returns, whose `Display` writes it",
                hir::Adt::from(*one).name(self.db).as_str()
            ),
            (0, _) => "a variant of a typed error whose `Display` writes it".to_owned(),
            _ => "one function that formats it (or a variant of a typed error, where it reports a failure)".to_owned(),
        };
        let places = match others.is_empty() {
            true => format!("{total} times here"),
            false => format!("here and in {others} ({total} places)"),
        };
        let message = format!(
            "the message `\"{}\"` is written {places}: give the sentence one owner, {owner}",
            template.text.as_str()
        );
        Some((self.finding(Lint::RepeatedFormat, template.at, message, NOTE)?, None))
    }

    /// The strings compared with `place` in the body, distinct, in order.
    fn set_values(&self, found: &Literals, place: SetPlace) -> Vec<Symbol, Global> {
        let mut values = Vec::new_in(Global);
        for lit in &found.lits {
            if let (Some(at), Value::Str(text)) = (lit.set, lit.value)
                && at == place
                && !values.contains(&text)
            {
                values.push(text);
            }
        }
        values
    }

    /// The closed sets of the body: a `match` on strings, and a place compared against two or
    /// more strings.
    fn sets(&self, found: &Literals, out: &mut Vec<(Finding, Option<Fix>)>) {
        if !self.strings {
            return;
        }
        const NOTE: &str = "a value matched against a fixed list of names is one of a closed set: an enum names each once, the compiler checks every match is exhaustive, and the text is parsed once where it comes in (`FromStr`)";
        let quoted = |values: &[Symbol]| {
            // alloc: a finding's message part
            values.iter().map(|it| format!("\"{}\"", it.as_str())).collect::<Vec<_>>().join(", ")
        };
        for matched in &found.matched {
            let message = format!(
                "`{}` is matched against {}: the set is closed, make it an enum with these variants",
                self.text(matched.scrutinee),
                quoted(&matched.values),
            );
            out.extend(self.finding(Lint::MagicString, matched.scrutinee, message, NOTE).map(|it| (it, None)));
        }
        // alloc: the places of one body's compares, a few
        let mut seen: Vec<SetPlace, Global> = Vec::new_in(Global);
        for lit in &found.lits {
            let (Some(place), Some(other)) = (lit.set, lit.other) else { continue };
            if seen.contains(&place) {
                continue;
            }
            seen.push(place);
            let values = self.set_values(found, place);
            if values.len() < SET_MIN {
                continue;
            }
            let message = format!(
                "`{}` is compared against {}: the set is closed, make it an enum with these variants",
                self.text(other),
                quoted(&values),
            );
            out.extend(self.finding(Lint::MagicString, lit.at, message, NOTE).map(|it| (it, None)));
        }
    }
}

/// The name of the parameter at `index` of `f`, `self` being 0 for a method.
fn param_name(db: &Db, f: FunctionId, index: u32) -> Option<String> {
    let params = hir::Function::from(f).assoc_fn_params(db);
    let param = params.get(usize::try_from(index).ok()?)?;
    Some(param.name(db)?.as_str().to_owned()) // alloc: a finding's message part
}

/// A body as a message names it: "`fn name`", "`const NAME`".
fn body_name(db: &Db, def: DefWithBodyId) -> String {
    let name = hir::DefWithBody::from(def).name(db);
    let name = name.as_ref().map_or("_", |it| it.as_str());
    // alloc: a finding's message part
    match def {
        DefWithBodyId::FunctionId(_) => format!("`fn {name}`"),
        DefWithBodyId::ConstId(_) => format!("`const {name}`"),
        DefWithBodyId::StaticId(_) => format!("`static {name}`"),
        DefWithBodyId::VariantId(_) => format!("`{name}`"),
    }
}

/// The name a repeated number's `const` is proposed under: the parameter's name, or the
/// compared field's or method's, in upper case; `TIMEOUT` for a duration.
fn proposed(db: &Db, role: Role, duration: bool) -> String {
    if duration {
        return "TIMEOUT".to_owned(); // alloc: a finding's message part
    }
    let name = match role {
        Role::Arg { f, index } => param_name(db, f, index),
        Role::Compare(Place::Field(it)) => Some(format!("{}_BOUND", hir::Field::from(it).name(db).as_str())),
        Role::Compare(Place::Method(it)) => {
            Some(format!("{}_BOUND", FunctionSignature::of(db, it).name.as_str()))
        }
        Role::Compare(Place::Static(_)) => None,
    };
    name.map_or_else(|| "NAME".to_owned(), |it| it.to_ascii_uppercase())
}

#[cfg(test)]
#[path = "magic_tests.rs"]
mod tests;
