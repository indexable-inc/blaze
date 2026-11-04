//! `false_sharing`: a struct shared between threads with two fields written apart that can sit
//! in one cache line: two atomics (std's, `crossbeam_utils::atomic::AtomicCell`), or a lock
//! (std's `Mutex` or `RwLock`, `lock_api`'s, which `parking_lot`'s are) and an atomic. A thread
//! writing one takes the line from the core that wrote the other, though they share no data.
//!
//! - **One line**: from the front end's layout of the struct, a field's offset and size, a line
//!   being [`LINE`] bytes. Two fields can share one when the struct's alignment is below a line
//!   and the last byte of the first is less than a line before the second, or, the struct
//!   aligned to a line, when they meet in one. A field whose own alignment is a line or more (a
//!   `#[repr(align(128))]` wrapper, `crossbeam_utils::CachePadded`) is padded and never counts.
//!   A struct whose layout depends on its parameters is not checked.
//! - **Written apart**: a field is written by a method of the struct's impls (inherent or a
//!   trait's) that calls a writing method on `self.field` (a store, swap or read-modify-write
//!   of an atomic, a lock taken). Two fields are written apart when different methods write
//!   them. Writes from outside the struct's impls are not seen.
//! - **Shared**: the struct is held, by value or through fields, arrays and type arguments, in
//!   a static (not a `thread_local!`'s) or an `Arc`, in a field, a static or a function
//!   signature of a workspace crate that can name it. A local `Arc::new` is not seen.
//!
//! The note says to pad to 128 bytes, the pair of lines arm64 and x86-64 prefetch together
//! (`cache-line-size` in `clippy.toml` sets it). Keep one (a read-mostly field, one writer
//! thread) with `#[expect(blaze::false_sharing, reason = "..")]` on the struct.

use base_db::Db;
use hir_def::signatures::FunctionSignature;
use hir_def::{DefWithBodyId, hir::Expr};
use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::{Lint, alloc::written_callee, facts};
use crate::check::Cx;

const LINT: Lint = Lint::FalseSharing;

/// The bytes one write takes exclusive: 128, the pair of 64-byte lines arm64 and x86-64
/// prefetch together (crossbeam's `CachePadded` pads to it on both).
const LINE: u64 = 128;

/// The line: `cache-line-size` in `clippy.toml`, or [`LINE`]; at least one byte.
fn line(cx: &Cx<'_, '_>) -> u64 {
    u64::try_from(cx.conf().int("cache-line-size", LINE as i64)).unwrap_or(LINE).max(1)
}

/// How deep into fields and type arguments a type is looked through for the struct.
const DEPTH: u32 = 4;

/// How many modules of the workspace are looked through for a static or an `Arc` holding it.
const MODULES: usize = 4096;

/// The methods that write an atomic.
const ATOMIC_WRITES: &[&str] = &[
    "store",
    "swap",
    "fetch_add",
    "fetch_sub",
    "fetch_and",
    "fetch_or",
    "fetch_xor",
    "fetch_nand",
    "fetch_max",
    "fetch_min",
    "fetch_update",
    "compare_exchange",
    "compare_exchange_weak",
    "compare_and_swap",
    "take",
    "replace",
];

/// The methods that take a lock, each writing its state.
const LOCK_WRITES: &[&str] =
    &["lock", "try_lock", "write", "try_write", "read", "try_read", "upgradable_read"];

/// What a field is that a thread writes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Atomic,
    Lock,
}

/// A field that can be written apart, where it is.
struct Contended {
    field: hir::Field,
    kind: Kind,
    offset: u64,
    size: u64,
    /// The methods of the struct's impls that write it.
    writers: Vec<hir::Name>,
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, item: ast::Struct<'t>) {
    if !cx.on_blaze(LINT) || cx.expansion().is_some() || !cx.is_written(item.syntax()) {
        return;
    }
    let Some(hir::Adt::Struct(def)) = cx.adt_def(ast::Adt::Struct(item)) else { return };
    let db = cx.db();
    let adt = hir::Adt::Struct(def);
    let ty = adt.ty(db);
    let Ok(layout) = ty.layout(db) else { return };
    let line = line(cx);
    // alloc: the few contended fields of one struct
    let mut fields = Vec::new();
    for field in def.fields(db) {
        let field_ty = field.ty(db);
        let Some(kind) = kind(cx, &field_ty) else { continue };
        let Ok(own) = field_ty.layout(db) else { continue };
        if own.align() >= line {
            continue;
        }
        let Some(offset) = layout.field_offset(field) else { continue };
        // alloc: filled once the struct is a candidate
        fields.push(Contended { field, kind, offset, size: own.size(), writers: Vec::new() });
    }
    fields.sort_by_key(|it| it.offset);
    let aligned = layout.align() >= line;
    let near = |a: &Contended, b: &Contended| {
        let last = a.offset + a.size.max(1) - 1;
        match aligned {
            true => last / line == b.offset / line,
            false => b.offset.saturating_sub(last) < line,
        }
    };
    let candidate = |a: &Contended, b: &Contended| {
        (a.kind == Kind::Atomic || b.kind == Kind::Atomic) && near(a, b)
    };
    let any =
        (0..fields.len()).any(|i| (i + 1..fields.len()).any(|j| candidate(&fields[i], &fields[j])));
    if !any {
        return;
    }
    let Some(held) = shared(cx, adt) else { return };
    writers(cx, def, &mut fields);
    let mut pair = None;
    'pairs: for i in 0..fields.len() {
        for j in i + 1..fields.len() {
            let (a, b) = (&fields[i], &fields[j]);
            if !candidate(a, b) {
                continue;
            }
            for m in &a.writers {
                if let Some(n) = b.writers.iter().find(|n| *n != m) {
                    pair = Some((i, j, m.clone(), n.clone())); // clone: two names, once per finding
                    break 'pairs;
                }
            }
        }
    }
    let Some((i, j, m, n)) = pair else { return };
    let (a, b) = (&fields[i], &fields[j]);
    let name = def.name(db);
    let (a_name, b_name) = (a.field.name(db), b.field.name(db));
    let message = format!(
        "the fields `{}` and `{}` of `{}` can share one {line}-byte line (at offsets {} and {}): `{}` writes `{}` and \
         `{}` writes `{}`, so threads calling them take the line from each other's cores",
        a_name.as_str(),
        b_name.as_str(),
        name.as_str(),
        a.offset,
        b.offset,
        m.as_str(),
        a_name.as_str(),
        n.as_str(),
        b_name.as_str(),
    );
    let note = format!(
        "`{}` is shared between threads: it is held {held}. Pad each field written apart to its own {line} bytes \
         (128 is the pair of lines arm64 and x86-64 prefetch together; `cache-line-size` in `clippy.toml` sets it): \
         each in a `#[repr(align({line}))]` wrapper, or in `crossbeam_utils::CachePadded<..>`",
        name.as_str()
    );
    let at = item.name().map_or(item.syntax(), |it| it.syntax());
    let Some(range) = cx.range(at) else { return };
    let span = cx.span(range);
    cx.blaze(LINT, item.syntax(), span, message)
        .note(note)
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}

/// Whether a field of type `ty` is an atomic or a lock a thread writes; `None` for anything
/// else, a padded one (`CachePadded`) included.
fn kind(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> Option<Kind> {
    let db = cx.db();
    let adt = ty.as_adt()?;
    let krate = adt.module(db).krate(db);
    let name = adt.name(db);
    let name = name.as_str();
    if name.starts_with("Atomic") && ["core", "std"].iter().any(|it| facts::crate_is(db, krate, it))
    {
        return Some(Kind::Atomic);
    }
    if facts::adt_is(db, adt, "crossbeam_utils", "AtomicCell") {
        return Some(Kind::Atomic);
    }
    let item =
        |it: &str| cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern(it)) == Some(adt);
    if item("Mutex")
        || item("RwLock")
        || ["Mutex", "RwLock", "ReentrantMutex"]
            .iter()
            .any(|it| facts::adt_is(db, adt, "lock_api", it))
    {
        return Some(Kind::Lock);
    }
    None
}

/// Fills each field's writers: the methods of `def`'s impls that call a writing method on it.
fn writers(cx: &Cx<'_, '_>, def: hir::Struct, fields: &mut [Contended]) {
    let db = cx.db();
    let adt = hir::Adt::Struct(def);
    for imp in hir::Impl::all_for_type(db, cx.krate(), adt.ty(db)) {
        for item in imp.items(db) {
            let hir::AssocItem::Function(f) = item else { continue };
            let owner = hir::DefWithBody::from(f);
            if DefWithBodyId::try_from(owner).is_err() {
                continue;
            }
            let analysis = hir::BodyAnalysis::new(db, owner);
            let body = analysis.body();
            let infer = analysis.infer();
            for (id, expr) in body.exprs() {
                let Expr::MethodCall { receiver, .. } = expr else { continue };
                let Expr::Field { expr: of, name: field_name } = body.expr(receiver) else {
                    continue;
                };
                if analysis.ty(infer.expr_ty(of)).strip_references().as_adt() != Some(adt) {
                    continue;
                }
                let Some((callee, _)) = written_callee(infer, body, id) else { continue };
                let signature = FunctionSignature::of(db, callee);
                let method = signature.name.as_str();
                let Some(field) = fields.iter_mut().find(|it| it.field.name(db) == *field_name)
                else {
                    continue;
                };
                let writes = match field.kind {
                    Kind::Atomic => ATOMIC_WRITES,
                    Kind::Lock => LOCK_WRITES,
                };
                let name = f.name(db);
                if writes.contains(&method) && !field.writers.contains(&name) {
                    field.writers.push(name);
                }
            }
        }
    }
}

/// Where the workspace holds `target` so that threads share it: the item named, or `None`.
fn shared(cx: &Cx<'_, '_>, target: hir::Adt) -> Option<String> {
    let db = cx.db();
    let arc = cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern("Arc"));
    let home = target.module(db).krate(db);
    let mut seen = 0;
    for krate in home.transitive_reverse_dependencies(db) {
        if !krate.origin(db).is_local() {
            continue;
        }
        // alloc: the worklist of one crate's modules
        let mut modules = vec![krate.root_module(db)];
        while let Some(module) = modules.pop() {
            seen += 1;
            if seen > MODULES {
                return None;
            }
            modules.extend(module.children(db));
            let holds =
                |ty: &hir::Type<'_>, shared: bool| reaches(db, ty, target, arc, shared, DEPTH);
            let in_fn = |f: hir::Function| {
                f.assoc_fn_params(db).iter().any(|it| holds(it.ty(), false))
                    || holds(&f.ret_type(db), false)
            };
            for def in module.declarations(db) {
                let held = match def {
                    hir::ModuleDef::Static(it) => holds(&it.ty(db), true)
                        .then(|| format!("in the static `{}`", it.name(db).as_str())),
                    hir::ModuleDef::Adt(it) => {
                        let fields = match it {
                            hir::Adt::Struct(s) => s.fields(db),
                            hir::Adt::Union(u) => u.fields(db),
                            // alloc: one enum's fields
                            hir::Adt::Enum(e) => {
                                e.variants(db).into_iter().flat_map(|v| v.fields(db)).collect()
                            }
                        };
                        fields.iter().any(|field| holds(&field.ty(db), false)).then(|| {
                            format!("in an `Arc` in the fields of `{}`", it.name(db).as_str())
                        })
                    }
                    hir::ModuleDef::Function(f) => in_fn(f).then(|| {
                        format!("in an `Arc` in the signature of `fn {}`", f.name(db).as_str())
                    }),
                    _ => None,
                };
                if held.is_some() {
                    return held;
                }
            }
            for imp in module.impl_defs(db) {
                for item in imp.items(db) {
                    if let hir::AssocItem::Function(f) = item
                        && in_fn(f)
                    {
                        return Some(format!(
                            "in an `Arc` in the signature of `fn {}`",
                            f.name(db).as_str()
                        ));
                    }
                }
            }
        }
    }
    None
}

/// Whether `ty` holds `target` where threads share it: inside an `Arc`, or anywhere when
/// `shared` (a static's type), through references, tuples, arrays, slices, type arguments and
/// struct fields, at most `depth` types deep.
fn reaches(
    db: &Db,
    ty: &hir::Type<'_>,
    target: hir::Adt,
    arc: Option<hir::Adt>,
    shared: bool,
    depth: u32,
) -> bool {
    if depth == 0 {
        return false;
    }
    let ty = ty.strip_references();
    if let Some(inner) = ty.as_slice() {
        return reaches(db, &inner, target, arc, shared, depth - 1);
    }
    if let Some((inner, _)) = ty.as_array(db) {
        return reaches(db, &inner, target, arc, shared, depth - 1);
    }
    if ty.is_tuple() {
        return ty
            .tuple_fields(db)
            .iter()
            .any(|it| reaches(db, it, target, arc, shared, depth - 1));
    }
    let Some(adt) = ty.as_adt() else { return false };
    if shared && adt == target {
        return true;
    }
    // A `thread_local!`'s static is one value per thread.
    if facts::adt_is(db, adt, "std", "LocalKey") {
        return false;
    }
    let shared = shared || arc == Some(adt);
    ty.type_arguments().any(|it| reaches(db, &it, target, arc, shared, depth - 1))
        || (matches!(adt, hir::Adt::Struct(_))
            && ty.fields(db).iter().any(|(_, it)| reaches(db, it, target, arc, shared, depth - 1)))
}

#[cfg(test)]
mod tests {
    use super::super::perf_tests::perf_only;

    const LINT: &str = "false_sharing";
    const FOUND: &str = "can share one 128-byte line";

    #[test]
    fn two_atomics_written_apart_in_a_shared_struct_are_found() {
        let found = perf_only(
            LINT,
            r#"
use std::sync::{Arc, Mutex, atomic::{AtomicU64, AtomicBool, Ordering}};
pub struct Stats {
    hits: AtomicU64,
    misses: AtomicU64,
}
impl Stats {
    pub fn hit(&self) { self.hits.fetch_add(1, Ordering::Relaxed); }
    pub fn miss(&self) { self.misses.fetch_add(1, Ordering::Relaxed); }
}
pub struct Pool { stats: Arc<Stats> }
pub struct Guarded {
    lock: Mutex<u32>,
    flag: AtomicBool,
}
impl Guarded {
    pub fn take(&self) -> u32 { *self.lock.lock() }
    pub fn raise(&self) { self.flag.store(true, Ordering::Release); }
}
pub static GUARDED: Guarded = Guarded { lock: Mutex::new(0), flag: AtomicBool::new(false) };
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 2, "{found}");
        assert!(
            found.contains("the fields `hits` and `misses` of `Stats` can share one 128-byte line (at offsets 0 and 8): `hit` writes `hits` and `miss` writes `misses`"),
            "{found}"
        );
        assert!(found.contains("in an `Arc` in the fields of `Pool`"), "{found}");
        assert!(found.contains("in the static `GUARDED`"), "{found}");
        assert!(
            found.contains("`#[repr(align(128))]` wrapper"),
            "the note says how to pad:\n{found}"
        );
    }

    /// Padded fields, one writer, a struct no thread shares, and two locks: none is found.
    #[test]
    fn padded_single_writer_unshared_or_lock_only_structs_are_left_alone() {
        let found = perf_only(
            LINT,
            r#"
use std::sync::{Arc, Mutex, atomic::{AtomicU64, Ordering}};
use crossbeam_utils::CachePadded;
pub struct Padded {
    hits: CachePadded<AtomicU64>,
    misses: CachePadded<AtomicU64>,
}
impl Padded {
    pub fn hit(&self) { self.hits.fetch_add(1, Ordering::Relaxed); }
    pub fn miss(&self) { self.misses.fetch_add(1, Ordering::Relaxed); }
}
pub struct OneWriter {
    hits: AtomicU64,
    misses: AtomicU64,
}
impl OneWriter {
    pub fn both(&self) {
        self.hits.fetch_add(1, Ordering::Relaxed);
        self.misses.fetch_add(1, Ordering::Relaxed);
    }
    pub fn read(&self) -> u64 { self.hits.load(Ordering::Relaxed) }
}
pub struct Alone {
    hits: AtomicU64,
    misses: AtomicU64,
}
impl Alone {
    pub fn hit(&self) { self.hits.fetch_add(1, Ordering::Relaxed); }
    pub fn miss(&self) { self.misses.fetch_add(1, Ordering::Relaxed); }
}
pub struct Locks { a: Mutex<u32>, b: Mutex<u32> }
impl Locks {
    pub fn a(&self) -> u32 { *self.a.lock() }
    pub fn b(&self) -> u32 { *self.b.lock() }
}
pub struct Shared { p: Arc<Padded>, o: Arc<OneWriter>, l: Arc<Locks> }
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 0, "{found}");
    }

    #[test]
    fn an_expect_with_a_reason_keeps_it() {
        let found = perf_only(
            LINT,
            r#"
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
#[expect(blaze::false_sharing, reason = "read-mostly: `misses` is written once a minute")]
pub struct Stats {
    hits: AtomicU64,
    misses: AtomicU64,
}
impl Stats {
    pub fn hit(&self) { self.hits.fetch_add(1, Ordering::Relaxed); }
    pub fn miss(&self) { self.misses.fetch_add(1, Ordering::Relaxed); }
}
pub static STATS: Stats = Stats { hits: AtomicU64::new(0), misses: AtomicU64::new(0) };
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 0, "{found}");
        assert!(!found.contains("unfulfilled"), "the expectation is met:\n{found}");
    }
}
