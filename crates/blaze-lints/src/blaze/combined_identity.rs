//! `combined_identity`: hashes combined into an identity by XOR or by a wrapping sum, outside
//! `blaze_ids::keyed`, with no `// identity: <why>` comment on the line or the line above.
//!
//! The rule: an identity of many things is a hash of all of them; one kept through edits is
//! `blaze_ids::keyed::Keyed`'s root, and nothing else adds or XORs hashes into an identity. A
//! sum or an XOR updates as cheaply and is not an identity: entries that XOR to a chosen value
//! are found by linear algebra, entries that sum to one by a generalized birthday search, so
//! two different sets get one ID and what is kept under it is used for the wrong set. A
//! build's ID was once the wrapping sum of its functions' entries, and keyed kept state.
//!
//! A finding is `a ^ b`, `a ^= b`, `a.wrapping_add(b)` or `a.wrapping_sub(b)` on integers, in
//! a function whose parameters or return type hold a `blaze_ids::ContentId` (itself, or in a
//! tuple, a slice, an array or a field, two types deep: `BuiltId(ContentId)`,
//! `&[(Key, ContentId)]`), or with an operand that reads one (`id.0[i] ^ other.0[i]`).
//!
//! Not found: `+` and `+=`, which panic on overflow in a build that checks it, so nobody sums
//! hashes with them; an identity that is a bare integer or array with no `ContentId` near it.
//! `blaze_ids::keyed`, the one audited module, and tests are not linted. An XOR that makes no
//! identity (a shard index, a checksum of one buffer) says so with `// identity: <why>`.
//! There is no fix: the set has to move into a `Keyed`.

use base_db::Db;
use hir_def::{
    DefWithBodyId, HasModule as _,
    expr_store::Body,
    hir::{ArithOp, BinaryOp, Expr, ExprId},
};

use super::{Finding, Lint, exempt, facts};
use crate::check::Cx;

const LINT: Lint = Lint::CombinedIdentity;

/// How many ADTs deep a type is looked into for a `ContentId`.
const DEPTH: u32 = 2;

/// How many expressions of an operand are looked at for one that reads an identity.
const MOST_EXPRS: usize = 64;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    if audited(db, hir::Module::from(def.module(db))) || cx.is_in_test(cx.root()) {
        return found;
    }
    let body = analysis.body();
    let infer = analysis.infer();
    let ty_of = |expr: ExprId| analysis.ty(infer.expr_ty(expr));
    // Asked once, at the first combining operation of the body.
    let mut signature = None;
    for (id, expr) in body.exprs() {
        let (how, lhs, rhs) = match expr {
            Expr::BinaryOp {
                lhs,
                rhs,
                op:
                    Some(
                        BinaryOp::ArithOp(ArithOp::BitXor)
                        | BinaryOp::Assignment { op: Some(ArithOp::BitXor) },
                    ),
            } => ("an XOR", lhs, rhs),
            Expr::MethodCall { receiver, method_name, args: [arg], .. }
                if matches!(method_name.as_str(), "wrapping_add" | "wrapping_sub") =>
            {
                ("a wrapping sum", receiver, *arg)
            }
            _ => continue,
        };
        if !ty_of(lhs).strip_references().is_int_or_uint() {
            continue;
        }
        let in_signature = *signature.get_or_insert_with(|| match def {
            DefWithBodyId::FunctionId(f) => {
                let f = hir::Function::from(f);
                f.assoc_fn_params(db).iter().any(|it| identity(db, it.ty()))
                    || identity(db, &f.ret_type(db))
            }
            _ => false,
        });
        let reads = |operand: ExprId| reads_identity(body, operand, &|it| identity(db, &ty_of(it)));
        if !(in_signature || reads(lhs) || reads(rhs)) {
            continue;
        }
        let Some(range) = analysis.expr_range(id) else { continue };
        if exempt::exempt(cx.text(), range, LINT) {
            continue;
        }
        let message = format!(
            "{how} of hashes where an identity is made, outside `blaze_ids::keyed`, with no `// identity: <why>`"
        );
        // alloc: a message's part
        let note = "a sum or an XOR of hashes is not an identity: entries that combine to a chosen value are found \
                    by linear algebra (XOR) or a generalized birthday search (a wrapping sum), so two different \
                    sets get one ID and what is kept under it is used for the wrong set"
            .to_owned();
        facts::push(&mut found, LINT, range, message, Some(note));
    }
    found
}

/// Whether `ty` holds a `blaze_ids::ContentId`, at most [`DEPTH`] types deep.
fn identity<'db>(db: &'db Db, ty: &hir::Type<'db>) -> bool {
    facts::holds(db, ty, DEPTH, &mut |adt| facts::adt_is(db, adt, "blaze_ids", "ContentId"))
        .is_some()
}

/// Whether `module` is `blaze_ids::keyed` or under it: the one module where hashes are
/// combined into an identity kept through edits.
fn audited(db: &Db, module: hir::Module) -> bool {
    facts::crate_is(db, module.krate(db), "blaze_ids")
        && module
            .path_to_root(db)
            .iter()
            .any(|it| it.name(db).is_some_and(|it| it.as_str() == "keyed"))
}

/// Whether `operand`, or an expression inside it, is of a type `is` says is an identity; at
/// most [`MOST_EXPRS`] expressions are looked at.
fn reads_identity(body: &Body, operand: ExprId, is: &dyn Fn(ExprId) -> bool) -> bool {
    // alloc: the walk of one operand, a few expressions deep
    let mut stack = vec![operand];
    let mut seen = 0;
    while let Some(expr) = stack.pop() {
        if is(expr) {
            return true;
        }
        seen += 1;
        if seen >= MOST_EXPRS {
            return false;
        }
        body.walk_child_exprs(expr, |child| stack.push(child));
    }
    false
}

#[cfg(test)]
mod tests {
    use crate::tests::lint;

    const FOUND: &str = "of hashes where an identity is made";

    /// A `blaze_ids` with `ContentId`, a `Hasher` and the audited `keyed` module.
    const CRATES: &str = r#"
//- minicore: sized, copy, clone, derive, option, fn, index, slice, builtin_impls
//- /blaze_ids/lib.rs crate:blaze_ids
#[derive(Clone, Copy)]
pub struct ContentId(pub [u8; 32]);
pub struct Hasher(u64);
impl Hasher {
    pub fn new(_domain: &'static str) -> Hasher { Hasher(0) }
    pub fn id(&mut self, _id: &ContentId) -> &mut Hasher { self }
    pub fn finish(&self) -> ContentId { ContentId([0; 32]) }
}
pub trait Words {
    fn wrapping_add(self, other: Self) -> Self;
}
impl Words for u64 {
    fn wrapping_add(self, other: u64) -> u64 { other }
}
pub fn word(_id: &ContentId, _at: usize) -> u64 { 0 }
pub mod keyed {
    use crate::ContentId;
    pub fn mix(a: &ContentId, b: &ContentId) -> u8 {
        a.0[0] ^ b.0[0]
    }
}
//- /src/lib.rs crate:main deps:blaze_ids
"#;

    fn found(main: &str) -> String {
        let fixture = format!("{}{}", CRATES.trim_start_matches('\n'), main.trim_start_matches('\n'));
        lint(&fixture, "-A clippy::all --warn=blaze::combined_identity")
    }

    /// The bug in miniature: a build's ID as the wrapping sum of its entries' words, and the
    /// same by XOR.
    #[test]
    fn an_id_made_as_a_wrapping_sum_or_an_xor_of_hashes_is_found() {
        let out = found(
            r#"
use blaze_ids::{ContentId, Hasher, Words as _, word};
pub struct BuiltId(pub ContentId);
fn add(sum: &mut [u64; 4], entry: &ContentId) {
    let mut at = 0;
    while at < 4 {
        sum[at] = sum[at].wrapping_add(word(entry, at));
        at += 1;
    }
}
pub fn id_of(entries: &[ContentId], n: usize) -> BuiltId {
    let mut sum = [0u64; 4];
    let mut at = 0;
    while at < n {
        add(&mut sum, &entries[at]);
        at += 1;
    }
    BuiltId(Hasher::new("built").finish())
}
pub fn folded(a: ContentId, b: ContentId) -> u8 {
    let mut out = a.0[0];
    out ^= b.0[1];
    out
}
pub fn folded_words(a: u64, b: u64, entry: &BuiltId) -> u64 {
    let _ = entry;
    a ^ b
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 3, "{out}");
        assert!(out.contains("a wrapping sum of hashes"), "{out}");
        assert!(out.contains("an XOR of hashes"), "{out}");
        assert!(out.contains("generalized birthday search"), "{out}");
    }

    /// The fix: the set's ID as a hash of every entry is not a finding; neither is an XOR
    /// that says what it is, one with no identity near it, or a call into the audited module.
    #[test]
    fn a_hash_of_the_whole_set_a_stated_reason_and_plain_integers_are_left_alone() {
        let out = found(
            r#"
use blaze_ids::{ContentId, Hasher};
pub struct BuiltId(pub ContentId);
pub fn id_of(entries: &[ContentId], n: usize) -> BuiltId {
    let mut hasher = Hasher::new("built");
    let mut at = 0;
    while at < n {
        hasher.id(&entries[at]);
        at += 1;
    }
    BuiltId(hasher.finish())
}
pub fn shard(id: &ContentId, shards: u8) -> u8 {
    // identity: a shard index, not an identity: both bytes stay in the key
    (id.0[0] ^ id.0[1]) % shards
}
pub fn parity(a: u64, b: u64) -> u64 {
    a ^ b
}
pub fn mixed(a: &ContentId, b: &ContentId) -> u8 {
    blaze_ids::keyed::mix(a, b)
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
    }
}
