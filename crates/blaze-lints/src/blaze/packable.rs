//! `packable` (in `blaze::pedantic_perf`, allowed by default): a struct of the crate whose
//! fields need far fewer bits than its layout gives them, stored many at a time. A `bool` takes
//! a byte and needs a bit; a fieldless enum of four variants takes a byte and needs two. Zig's
//! packed structs are the idea: a struct whose fields sit at their bit widths in one integer.
//! blaze's opt-in `#[blaze::bits]` is that for Rust.
//!
//! A field's bits: a `bool` 1; a fieldless enum the bits that hold every discriminant
//! (⌈log2 n⌉ for the default `0..n`), or its full size when it has an integer `#[repr]` (what
//! `#[blaze::bits]` gives it); a struct of the crate without `#[repr]` what its own fields need,
//! by the same rule, when that is less than its size; anything else its full size. The packed
//! size is those bits in whole bytes, rounded up to the largest alignment of a field kept at
//! full width (an element of an array is a multiple of its alignment).
//!
//! Found: a struct of the crate with no `#[repr(..)]` and no `#[blaze::bits]`, outside test
//! code and macro output, with a layout, at least one field narrower than its size, that is
//! the element of a `Vec`, `VecDeque`, array, slice or `ArrayVec`, or the value of a map
//! (`HashMap`, `BTreeMap`, hashbrown's, indexmap's or papaya's), somewhere in the crate: the
//! type of a field or of any expression of a body ([`packable_uses`]); and packing it at least
//! halves it, or brings it from over a cache line (64 bytes) to one or less. A struct that
//! `enum_vec_padded` finds a field of is left to that lint when it is on.
//!
//! With a profile configured ([`crate::profile`]), a finding whose storing functions include
//! one the profile shows hot says so, and by how much, in its first note; one with none says
//! that too. Without a profile it is found all the same.
//!
//! Narrowed: only the crate that defines the struct is searched for collections of it (another
//! crate's `Vec<S>` is that crate's lint run, which can't report on this one's struct); an enum
//! with explicit discriminants whose values don't evaluate counts at its full size; integer
//! fields count at their full width until blaze has `uN` (lane `uint`). No fix: a packed field
//! has no address, so every `&s.f` and `&mut s.f` must be checked first.

use std::alloc::Global;

use base_db::Db;
use blaze_hash::HashMap;
use hir::HirDisplay as _;
use hir_def::{
    AdtId, DefWithBodyId, FieldId, FunctionId, ModuleDefId, StructId, VariantId, expr_store::Body,
    hir::ExprId, nameres::crate_def_map,
};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasAttrs as _, HasName as _},
};

use super::{Lint, enum_vec_padded, frozen_collection};
use crate::{check::Cx, diag, query};

const LINT: Lint = Lint::Packable;

/// A cache line: an element over it that packing brings to it or under is found.
const CACHE_LINE: u64 = 64;

/// How many structs deep a field's bits are followed.
const DEPTH: u32 = 8;

/// How many of a finding's narrowed fields its note names.
const FIELDS_SHOWN: usize = 8;

/// What holds the struct many at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Collection {
    /// The element of this sequence (`Vec`, `VecDeque`, `ArrayVec`).
    Element(&'static str),
    Array,
    Slice,
    /// The value of this map.
    Value(&'static str),
}

impl Collection {
    fn describe(self) -> String {
        match self {
            Collection::Element(name) => format!("the element of a `{name}`"),
            Collection::Array => "the element of an array".to_owned(), // alloc: a message's part
            Collection::Slice => "the element of a slice".to_owned(),  // alloc: a message's part
            Collection::Value(name) => format!("the value of a `{name}`"),
        }
    }
}

/// Where the crate holds the struct in a collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Site {
    /// A field's type names the collection.
    Field(FieldId),
    /// An expression of a body has the collection's type.
    Expr(DefWithBodyId, ExprId),
}

/// The collections of one struct the crate has.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Stored {
    /// The first, in the order of the crate's items and then its bodies.
    pub(crate) first: (Site, Collection),
    /// The functions whose bodies have one, each once, in the order of the crate's bodies.
    pub(crate) fns: Vec<FunctionId>,
}

/// Every struct of the crate the crate holds in a collection.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Uses(HashMap<StructId, Stored, Global>);

impl Uses {
    pub(crate) fn of(&self, id: StructId) -> Option<&Stored> {
        self.0.get(&id)
    }
}

/// The collection ADTs std names by diagnostic item.
struct Std {
    vec: Option<hir::Adt>,
    deque: Option<hir::Adt>,
    hash_map: Option<hir::Adt>,
    btree_map: Option<hir::Adt>,
}

impl Std {
    fn new(db: &Db, krate: hir::Crate) -> Std {
        let item = |name: &str| krate.diagnostic_item_adt(db, &intern::Symbol::intern(name));
        Std {
            vec: item("Vec"),
            deque: item("VecDeque"),
            hash_map: item("HashMap"),
            btree_map: item("BTreeMap"),
        }
    }

    /// Which type argument of `adt` is what it holds many of, and how.
    fn holds(&self, db: &Db, adt: hir::Adt) -> Option<(usize, Collection)> {
        let is = |it: Option<hir::Adt>| it == Some(adt);
        if is(self.vec) {
            return Some((0, Collection::Element("Vec")));
        }
        if is(self.deque) {
            return Some((0, Collection::Element("VecDeque")));
        }
        if is(self.hash_map) {
            return Some((1, Collection::Value("HashMap")));
        }
        if is(self.btree_map) {
            return Some((1, Collection::Value("BTreeMap")));
        }
        // Widely used crates' collections, by their crate and name.
        let display = adt.module(db).krate(db).display_name(db)?;
        let name = adt.name(db);
        match (display.crate_name().as_str(), name.as_str()) {
            ("arrayvec", "ArrayVec") => Some((0, Collection::Element("ArrayVec"))),
            ("hashbrown" | "papaya", "HashMap") => Some((1, Collection::Value("HashMap"))),
            ("indexmap", "IndexMap") => Some((1, Collection::Value("IndexMap"))),
            _ => None,
        }
    }

    /// The structs of `krate` that `ty`, or a type inside it, holds many of.
    fn stored_in(
        &self,
        db: &Db,
        krate: base_db::Crate,
        ty: &hir::Type<'_>,
        out: &mut Vec<(StructId, Collection)>,
    ) {
        ty.walk(db, |inner| {
            let held = if let Some(element) = inner.as_array_element(db) {
                Some((element, Collection::Array))
            } else if let Some(element) = inner.as_slice() {
                Some((element, Collection::Slice))
            } else {
                inner.as_adt().and_then(|adt| {
                    let (at, how) = self.holds(db, adt)?;
                    Some((inner.type_arguments().nth(at)?, how))
                })
            };
            let Some((element, how)) = held else { return };
            if let Some(hir::Adt::Struct(it)) = element.as_adt()
                && it.module(db).krate(db).base() == krate
                && !out.iter().any(|(id, _)| *id == StructId::from(it))
            {
                out.push((StructId::from(it), how));
            }
        });
    }
}

/// Where the crate `krate` holds its own structs in collections, once per crate: the types of
/// its items' fields, then the type of every expression of its bodies.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn packable_uses(db: &Db, krate: base_db::Crate) -> Uses {
    let mut out = Uses(HashMap::new_in(Global));
    let std = Std::new(db, hir::Crate::from(krate));
    // alloc: the structs one type holds, reused for every type of the crate
    let mut found = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        for def in module.scope(db).declarations() {
            match def {
                ModuleDefId::AdtId(AdtId::StructId(it)) => {
                    record_fields(db, krate, &std, VariantId::StructId(it), &mut found, &mut out);
                }
                ModuleDefId::AdtId(AdtId::UnionId(it)) => {
                    record_fields(db, krate, &std, VariantId::UnionId(it), &mut found, &mut out);
                }
                ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                    for &(variant, _) in it.enum_variants(db).variants.values() {
                        record_fields(
                            db,
                            krate,
                            &std,
                            VariantId::EnumVariantId(variant),
                            &mut found,
                            &mut out,
                        );
                    }
                }
                _ => {}
            }
        }
    }
    // What each distinct type holds, found once for the crate.
    let mut seen: HashMap<hir_ty::next_solver::Ty<'_>, (u32, u32), Global> =
        HashMap::new_in(Global);
    // alloc: the structs every distinct type holds, end to end, once per crate
    let mut held: Vec<(StructId, Collection)> = Vec::new();
    for def in frozen_collection::crate_bodies(db, krate) {
        let analysis = hir::BodyAnalysis::new(db, hir::DefWithBody::from(def));
        let infer = analysis.infer();
        for (id, _) in analysis.body().exprs() {
            let ty = infer.expr_ty(id);
            let (start, len) = *seen.entry(ty).or_insert_with(|| {
                found.clear();
                std.stored_in(db, krate, &analysis.ty(ty), &mut found);
                let start = u32::try_from(held.len()).unwrap_or(u32::MAX);
                held.extend_from_slice(&found);
                (start, u32::try_from(found.len()).unwrap_or(0))
            });
            if len > 0 {
                let range = start as usize..(start + len) as usize;
                record(&mut out, &held[range], Site::Expr(def, id));
            }
        }
    }
    out
}

/// Records that `site` holds each of `found`.
fn record(out: &mut Uses, found: &[(StructId, Collection)], site: Site) {
    for &(id, how) in found {
        // alloc: one struct's storing functions, once per struct the crate stores
        let stored =
            out.0.entry(id).or_insert_with(|| Stored { first: (site, how), fns: Vec::new() });
        if let Site::Expr(DefWithBodyId::FunctionId(f), _) = site
            && stored.fns.last() != Some(&f)
        {
            stored.fns.push(f);
        }
    }
}

/// Records what the fields of `variant` hold.
fn record_fields(
    db: &Db,
    krate: base_db::Crate,
    std: &Std,
    variant: VariantId,
    found: &mut Vec<(StructId, Collection)>,
    out: &mut Uses,
) {
    for (local_id, _) in variant.fields(db).fields().iter() {
        let id = FieldId { parent: variant, local_id };
        found.clear();
        std.stored_in(db, krate, &hir::Field::from(id).ty(db), found);
        record(out, found, Site::Field(id));
    }
}

/// What packing takes of a value: its bits, the largest alignment of what stays at full width,
/// and whether anything in it is narrower than its size.
#[derive(Clone, Copy, Debug)]
struct Need {
    bits: u64,
    align: u64,
    narrowed: bool,
}

/// The bits of a fieldless enum's discriminants; `None` when it is kept at full size.
fn enum_bits(db: &Db, enum_: hir::Enum) -> Option<u64> {
    if enum_.is_data_carrying(db) || enum_.repr(db).is_some_and(|it| it.int.is_some()) {
        return None;
    }
    let count = enum_.num_variants(db) as u64;
    if count == 0 {
        return None;
    }
    let id = hir_def::EnumId::from(enum_);
    let variants = id.enum_variants(db);
    let explicit =
        variants.variants.values().any(|&(it, _)| variants.has_explicit_discriminant(it));
    let width = |value: u128| u64::from(128 - value.leading_zeros());
    if !explicit {
        // `0..count`.
        return Some(width(u128::from(count - 1)));
    }
    let (mut least, mut most) = (0i128, 0i128);
    for variant in enum_.variants(db) {
        let value = variant.eval(db).ok()?;
        least = least.min(value);
        most = most.max(value);
    }
    if least >= 0 {
        return Some(width(most as u128));
    }
    // Two's complement: a sign bit over the larger of the two magnitudes.
    Some(1 + width(most.max(0) as u128).max(width(!least as u128)))
}

/// What packing takes of a value of `ty`, `depth` structs down.
fn need(db: &Db, krate: hir::Crate, ty: &hir::Type<'_>, depth: u32) -> Option<Need> {
    let layout = ty.layout(db).ok()?;
    let full = Need { bits: layout.size() * 8, align: layout.align().max(1), narrowed: false };
    if ty.is_bool() {
        return Some(Need { bits: 1, align: 1, narrowed: true });
    }
    match ty.as_adt() {
        Some(hir::Adt::Enum(enum_)) => match enum_bits(db, enum_) {
            Some(bits) if bits < full.bits => Some(Need { bits, align: 1, narrowed: true }),
            _ => Some(full),
        },
        Some(hir::Adt::Struct(inner))
            if depth < DEPTH && inner.module(db).krate(db) == krate && inner.repr(db).is_none() =>
        {
            match fields_need(db, krate, ty, depth + 1) {
                Some(it) if it.narrowed && it.bits < full.bits => Some(it),
                _ => Some(full),
            }
        }
        _ => Some(full),
    }
}

/// What packing takes of the fields of the struct type `ty`.
fn fields_need(db: &Db, krate: hir::Crate, ty: &hir::Type<'_>, depth: u32) -> Option<Need> {
    let mut sum = Need { bits: 0, align: 1, narrowed: false };
    for (_, field) in ty.fields(db) {
        let it = need(db, krate, &field, depth)?;
        sum.bits += it.bits;
        sum.align = sum.align.max(it.align);
        sum.narrowed |= it.narrowed;
    }
    Some(sum)
}

/// `bytes` with its unit.
fn bytes(n: u64) -> String {
    match n {
        1 => "1 byte".to_owned(), // alloc: a message's part
        _ => format!("{n} bytes"),
    }
}

/// How a message names `file`: its path from the crate's package (the directory above the
/// root file's `src`, or the root file's own directory), or the whole path.
pub(super) fn shown_path(db: &Db, krate: hir::Crate, file: span::FileId) -> String {
    let Some(path) = db.path_for_file(file) else { return format!("file {}", file.index()) };
    let base = db.path_for_file(krate.root_file(db)).and_then(|root| {
        let dir = root.parent()?;
        match dir.name_and_extension() {
            Some(("src", None)) => dir.parent(),
            _ => Some(dir),
        }
    });
    match base.as_ref().and_then(|base| path.strip_prefix(base)) {
        Some(relative) => relative.as_str().to_owned(), // alloc: a message's part
        None => path.to_string(),
    }
}

/// Where `site` is written: its file and its range there; `None` for one a macro wrote.
fn place_of(cx: &Cx<'_, '_>, site: Site) -> Option<(span::FileId, TextRange)> {
    let db = cx.db();
    match site {
        Site::Field(id) => {
            let field = hir::Field::from(id);
            let adt = match field.parent_def(db) {
                hir::Variant::Struct(it) => hir::Adt::Struct(it),
                hir::Variant::Union(it) => hir::Adt::Union(it),
                hir::Variant::EnumVariant(it) => hir::Adt::Enum(it.parent_enum(db)),
            };
            let file = match AdtId::from(adt) {
                AdtId::StructId(it) => hir_def::Lookup::lookup(&it, db).id.file_id,
                AdtId::UnionId(it) => hir_def::Lookup::lookup(&it, db).id.file_id,
                AdtId::EnumId(it) => hir_def::Lookup::lookup(&it, db).id.file_id,
            };
            let file = file.file_id()?.file_id(db);
            let source = cx.adt_source(adt)?;
            let node = match source {
                ast::Adt::Struct(item) => {
                    frozen_collection::field_node(item, &field.name(db), field.index())
                        .unwrap_or(item.syntax())
                }
                other => other.syntax(),
            };
            Some((file, node.text_range()))
        }
        Site::Expr(def, expr) => {
            let analysis = hir::BodyAnalysis::new(db, hir::DefWithBody::from(def));
            let file = analysis.tree().1.file_id()?.file_id(db);
            let offset = Body::anchor_offset(db, def)?;
            Some((file, analysis.expr_range(expr)? + offset))
        }
    }
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, item: ast::Struct<'t>) {
    if !cx.on_blaze(LINT)
        || cx.expansion().is_some()
        || !cx.is_written(item.syntax())
        || cx.is_in_test(item.syntax())
    {
        return;
    }
    // `#[blaze::bits]` packs it already.
    if item.attrs().any(|it| it.path().is_some_and(|path| path.syntax().text() == "blaze::bits")) {
        return;
    }
    let Some(hir::Adt::Struct(def)) = cx.adt_def(ast::Adt::Struct(item)) else { return };
    let db = cx.db();
    if def.repr(db).is_some() {
        return;
    }
    let Some(stored) = packable_uses(db, cx.krate().base()).of(StructId::from(def)) else { return };
    let ty = def.ty(db);
    let Ok(layout) = ty.layout(db) else { return };
    let size = layout.size();
    let Some(total) = fields_need(db, cx.krate(), &ty, 0) else { return };
    if !total.narrowed || size == 0 {
        return;
    }
    let packed = total.bits.div_ceil(8).next_multiple_of(total.align);
    let halves = size >= 2 * packed;
    let fits_line = size > CACHE_LINE && packed <= CACHE_LINE;
    if packed >= size || !(halves || fits_line) {
        return;
    }
    // `enum_vec_padded` names what to do with this struct's `Vec` of a lopsided enum.
    if enum_vec_padded::covers(cx, def) {
        return;
    }
    let Some(name_node) = item.name().map(|it| it.syntax()) else { return };
    let Some(range) = cx.range(name_node) else { return };
    let (site, how) = stored.first;
    let place = place_of(cx, site);
    let at = match place {
        Some((file, range)) => match db.line_column(file, range.start()) {
            Ok((line, _)) => format!(" ({}:{})", shown_path(db, cx.krate(), file), line + 1),
            Err(()) => format!(" ({})", shown_path(db, cx.krate(), file)),
        },
        None => String::new(),
    };
    let name = def.name(db);
    let message = format!(
        "`{}` takes {} but its fields need {} bits: packed, it would take {}, and it is {}{at}",
        name.as_str(),
        bytes(size),
        total.bits,
        bytes(packed),
        how.describe(),
    );
    // The fields narrower than their size, and how many bits each needs.
    let target = cx.krate().to_display_target(db);
    let (mut shown, mut listed, mut bools, mut more) = (String::new(), 0usize, 0usize, 0usize);
    for (field, field_ty) in ty.fields(db) {
        let Some(it) = need(db, cx.krate(), &field_ty, 0) else { continue };
        if !it.narrowed {
            continue;
        }
        bools += usize::from(field_ty.is_bool());
        if listed == FIELDS_SHOWN {
            more += 1;
            continue;
        }
        listed += 1;
        if !shown.is_empty() {
            shown.push_str(", ");
        }
        let bits = if it.bits == 1 { "1 bit".to_owned() } else { format!("{} bits", it.bits) }; // alloc: a message's part
        shown.push_str(&format!(
            "`{}` (`{}`, {bits})",
            field.name(db).as_str(),
            field_ty.display(db, target)
        ));
    }
    if more > 0 {
        shown.push_str(&format!(" and {more} more"));
    }
    let hot = hot_note(cx, stored);
    let span = cx.span(range);
    let diagnostic = cx.blaze(LINT, name_node, span, message);
    if let Some(note) = hot {
        diagnostic.note(note);
    }
    if let Some((file, range)) = place {
        diagnostic.span_note(
            diag::of_file(file, range),
            format!("`{}` is {} here", name.as_str(), how.describe()),
        );
    }
    diagnostic.note(format!(
        "{shown} take whole bytes; every element pays the difference. Zig's packed structs store each field at its bit width"
    ));
    if bools >= 2 {
        diagnostic.help(format!("or gather its {bools} `bool`s into one `bitflags` set"));
    }
    diagnostic.help(LINT.help()).help(format!("for further information visit {}", LINT.docs_url()));
}

/// The note of how the configured profile has the functions that store the struct: the
/// hottest of them when one is hot; `None` with no profile.
fn hot_note(cx: &Cx<'_, '_>, stored: &Stored) -> Option<String> {
    let loaded = cx.conf().profile.as_ref()?;
    let db = cx.db();
    // clone: the profile's settings and a refcount, once per finding with a profile
    let hits =
        query::profile_hits(db, cx.krate().base(), query::ProfileKey::new(db, loaded.clone()));
    let file = loaded.file_name();
    let hottest = stored
        .fns
        .iter()
        .filter_map(|&f| Some((f, hits.of(f)?)))
        .filter(|(_, hit)| hit.hot)
        // The last of the hottest, in the crate's body order: the same every run.
        .max_by_key(|(_, hit)| hit.share_ppb);
    Some(match hottest {
        Some((f, hit)) => format!(
            "it is hot per profile `{file}`: `fn {}` stores it, {} of the profile's counts",
            hir::Function::from(f).name(db).as_str(),
            hit.percent()
        ),
        None => format!(
            "profile `{file}` shows none of the {} functions that store it hot",
            stored.fns.len()
        ),
    })
}
