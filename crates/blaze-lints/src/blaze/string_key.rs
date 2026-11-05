//! `string_key`: a hash map or set keyed by a string (`String`, `&str`, `Box<str>`, `Arc<str>`,
//! `Rc<str>`, `smol_str::SmolStr`) whose keys are identifiers: every key put into it in the body
//! is a `Name`'s (`hir_expand`'s) or a `Symbol`'s (`intern`'s), through `as_str`, `to_string`,
//! `to_owned`, `to_smolstr`, `into`, `clone`, `&`, `*`, `String::from`, `SmolStr::new`,
//! `Box::from`, `Arc::from`, and up to [`LET_DEPTH`] `let`s. The identifier is already interned:
//! keying by `intern::Symbol` hashes and compares four bytes, where a string key hashes every
//! byte, compares them on each probe and owns a copy.
//!
//! Only in a crate that depends on `intern` (so the fix needs no new dependency), and not
//! `intern` itself. A map one of whose keys comes from anything else (a file's text, a
//! command line, a format) holds user data and is not flagged, nor is one whose keys this body
//! does not all see. Reported once per map, at its first write. The map is a place: a local or a
//! `static` and its fields, through `&` and `*`; writes are `insert`, `entry`,
//! `get_or_insert`, `get_or_insert_with` and papaya's `pin()` of one, the method resolved on the
//! map's impl ([`facts::hash_adt`]'s maps and sets, papaya's).

use hir_def::{
    DefWithBodyId,
    expr_store::Body,
    hir::{BindingId, Expr, ExprId, Pat, Statement, UnaryOp},
    resolver::ValueNs,
    signatures::FunctionSignature,
};
use hir_expand::name::Name;
use syntax::TextRange;

use super::{
    Finding, Lint,
    alloc::written_callee,
    facts,
    per_item::{value_of, written_call},
};
use crate::check::Cx;

const LINT: Lint = Lint::StringKey;

/// How many `let`s a key is followed back through to what made it.
const LET_DEPTH: u32 = 2;
/// How many conversions, borrows and derefs a key is followed through.
const KEY_DEPTH: u32 = 12;
/// How many fields deep a map's place is followed.
const PLACE_DEPTH: u32 = 16;

/// The writes of a key.
const WRITES: &[&str] = &["insert", "entry", "get_or_insert", "get_or_insert_with"];
/// The conversions a key is followed through, each keeping its text.
const CONVERSIONS: &[&str] = &[
    "as_str",
    "to_string",
    "to_owned",
    "to_smolstr",
    "into",
    "clone",
    "as_ref",
    "borrow",
    "symbol",
];
/// The constructors of a string key from another string, called with one argument.
const CONSTRUCTORS: &[&str] = &["from", "new"];
/// papaya's maps and sets and what their `pin()` lends.
const PAPAYA: &[&str] = &["HashMap", "HashSet", "HashMapRef", "HashSetRef"];

/// What a map is keyed by, as the message names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Key {
    String,
    Str,
    BoxStr,
    ArcStr,
    RcStr,
    SmolStr,
}

impl Key {
    fn say(self) -> &'static str {
        match self {
            Key::String => "String",
            Key::Str => "&str",
            Key::BoxStr => "Box<str>",
            Key::ArcStr => "Arc<str>",
            Key::RcStr => "Rc<str>",
            Key::SmolStr => "SmolStr",
        }
    }
}

/// The local or `static` a map is reached from, and its fields.
#[derive(Clone, PartialEq, Debug)]
struct Place {
    root: ValueNs,
    fields: Vec<Name>,
}

/// The writes of one map in a body.
struct Map {
    place: Place,
    key: Key,
    /// Its first write.
    first: TextRange,
    receiver: ExprId,
    /// Whether every key is an identifier's, and which kind the first was.
    from: Option<&'static str>,
    all: bool,
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

/// The body linted and what its walk needs.
struct Walk<'a, 'db, 't> {
    cx: &'a Cx<'db, 't>,
    def: DefWithBodyId,
    body: &'db Body,
    analysis: &'a hir::BodyAnalysis<'db>,
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let db = cx.db();
    let krate = cx.krate();
    if facts::crate_is(db, krate, "intern")
        || facts::dep_type(db, krate.base(), "intern", &["Symbol"]).is_none()
    {
        return found;
    }
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let walk = Walk { cx, def, body: analysis.body(), analysis };
    // alloc: the maps one body writes, a few
    let mut maps: Vec<Map> = Vec::new();
    for (id, expr) in walk.body.exprs() {
        let Expr::MethodCall { receiver, args, .. } = expr else { continue };
        let Some(&key) = args.first() else { continue };
        if !written_call(analysis, id) {
            continue;
        }
        let Some(key_type) = walk.write_of(id, receiver) else { continue };
        let Some(place) = walk.place_of(receiver, PLACE_DEPTH) else { continue };
        let Some(range) = analysis.expr_range(id) else { continue };
        let from = walk.identifier(key, KEY_DEPTH, LET_DEPTH);
        match maps.iter_mut().find(|it| it.place == place) {
            Some(map) => {
                map.all &= from.is_some();
                if range.start() < map.first.start() {
                    (map.first, map.receiver) = (range, receiver);
                }
            }
            None => maps.push(Map {
                place,
                key: key_type,
                first: range,
                receiver,
                from,
                all: from.is_some(),
            }),
        }
    }
    for map in maps {
        let (true, Some(from)) = (map.all, map.from) else { continue };
        let text = analysis.expr_range(map.receiver).map(|it| cx.snippet(it)).unwrap_or("this map");
        let message = format!(
            "`{text}` is keyed by `{}`, and every key put in it here is a `{from}`'s text: key it by \
             `intern::Symbol`",
            map.key.say(),
        );
        let note = "a `Symbol` is interned once: hashing and comparing it is four bytes, where a string key \
                    hashes every byte, compares the bytes on each probe and owns its copy";
        facts::push(&mut found, LINT, map.first, message, Some(note.to_owned())); // alloc: a finding's note
    }
    found
}

impl<'db> Walk<'_, 'db, '_> {
    fn db(&self) -> &'db base_db::Db {
        self.cx.db()
    }

    fn name(&self, id: ExprId) -> Option<&'db str> {
        let (f, _) = written_callee(self.analysis.infer(), self.body, id)?;
        Some(FunctionSignature::of(self.db(), f).name.as_str())
    }

    /// The key type of the map `receiver` when the call `id` writes a key of it.
    fn write_of(&self, id: ExprId, receiver: ExprId) -> Option<Key> {
        let db = self.db();
        let (f, _) = written_callee(self.analysis.infer(), self.body, id)?;
        if !WRITES.contains(&FunctionSignature::of(db, f).name.as_str()) {
            return None;
        }
        let adt = facts::impl_adt(db, f)?;
        let papaya = facts::crate_is(db, adt.module(db).krate(db), "papaya")
            && PAPAYA.contains(&adt.name(db).as_str());
        if !papaya && !facts::hash_adt(self.cx, adt) {
            return None;
        }
        let map = self.analysis.ty(self.analysis.infer().expr_ty(receiver)).strip_references();
        let key = map.type_arguments().next()?;
        self.key(&key)
    }

    /// What `ty` is as a string key, if it is one.
    fn key(&self, ty: &hir::Type<'db>) -> Option<Key> {
        let db = self.db();
        if ty.is_reference() && ty.strip_references().is_str() {
            return Some(Key::Str);
        }
        let adt = ty.as_adt()?;
        let item = |name: &str| {
            self.cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern(name)) == Some(adt)
        };
        if item("String") {
            return Some(Key::String);
        }
        if facts::adt_is(db, adt, "smol_str", "SmolStr") {
            return Some(Key::SmolStr);
        }
        let of_str = ty.type_arguments().next().is_some_and(|it| it.is_str());
        let pointer = if facts::adt_is(db, adt, "alloc", "Box") {
            Key::BoxStr
        } else if item("Arc") || facts::adt_is(db, adt, "triomphe", "Arc") {
            Key::ArcStr
        } else if item("Rc") {
            Key::RcStr
        } else {
            return None;
        };
        of_str.then_some(pointer)
    }

    /// Whether `ty`, its references peeled, is an identifier: `hir_expand`'s `Name` or
    /// `intern`'s `Symbol`, named as the message says it.
    fn ident(&self, ty: &hir::Type<'db>) -> Option<&'static str> {
        let db = self.db();
        let adt = ty.strip_references().as_adt()?;
        if facts::adt_is(db, adt, "hir_expand", "Name") {
            Some("Name")
        } else if facts::adt_is(db, adt, "intern", "Symbol") {
            Some("Symbol")
        } else {
            None
        }
    }

    /// The identifier `key` is made from, through [`CONVERSIONS`], [`CONSTRUCTORS`], `&`, `*`
    /// and at most `lets` `let` initializers.
    fn identifier(&self, key: ExprId, depth: u32, lets: u32) -> Option<&'static str> {
        let depth = depth.checked_sub(1)?;
        match self.body.expr(key) {
            Expr::Ref { expr, .. } | Expr::UnaryOp { expr, op: UnaryOp::Deref } => {
                self.identifier(expr, depth, lets)
            }
            Expr::MethodCall { receiver, args: [], .. } => {
                if !CONVERSIONS.contains(&self.name(key)?) {
                    return None;
                }
                let ty = self.analysis.ty(self.analysis.infer().expr_ty(receiver));
                self.ident(&ty).or_else(|| self.identifier(receiver, depth, lets))
            }
            Expr::Call { args: [arg], .. } => {
                let arg = *arg;
                if !CONSTRUCTORS.contains(&self.name(key)?) {
                    return None;
                }
                let ty = self.analysis.ty(self.analysis.infer().expr_ty(arg));
                self.ident(&ty).or_else(|| self.identifier(arg, depth, lets))
            }
            Expr::Path(_) => {
                let ValueNs::LocalBinding(binding) = value_of(self.db(), self.def, self.body, key)?
                else {
                    return None;
                };
                let lets = lets.checked_sub(1)?;
                let init = self.initializer(binding)?;
                self.identifier(init, depth, lets)
            }
            _ => None,
        }
    }

    /// The initializer of the `let` that binds `binding` by itself (`let k = ..;`).
    fn initializer(&self, binding: BindingId) -> Option<ExprId> {
        self.body.exprs().find_map(|(_, expr)| {
            let Expr::Block { statements, .. } = expr else { return None };
            statements.into_iter().find_map(|it| match it {
                Statement::Let { pat, initializer: Some(init), .. } => match self.body.pat(pat) {
                    Pat::Bind { id, subpat: None } if id == binding => Some(init),
                    _ => None,
                },
                _ => None,
            })
        })
    }

    /// The place `expr` names: a local or `static` and its fields, through `&`, `*` and
    /// papaya's `pin()`.
    fn place_of(&self, expr: ExprId, depth: u32) -> Option<Place> {
        let depth = depth.checked_sub(1)?;
        match self.body.expr(expr) {
            Expr::Path(_) => {
                let root = value_of(self.db(), self.def, self.body, expr)?;
                matches!(root, ValueNs::LocalBinding(_) | ValueNs::StaticId(_))
                    .then(|| Place { root, fields: Vec::new() }) // alloc: a place's fields, a few
            }
            Expr::Field { expr: inner, name } => {
                let mut place = self.place_of(inner, depth)?;
                place.fields.push(*name);
                Some(place)
            }
            Expr::UnaryOp { expr: inner, op: UnaryOp::Deref } | Expr::Ref { expr: inner, .. } => {
                self.place_of(inner, depth)
            }
            Expr::MethodCall { receiver, args: [], .. } => {
                let (f, _) = written_callee(self.analysis.infer(), self.body, expr)?;
                let db = self.db();
                let adt = facts::impl_adt(db, f)?;
                let pin = matches!(FunctionSignature::of(db, f).name.as_str(), "pin" | "pin_owned")
                    && facts::crate_is(db, adt.module(db).krate(db), "papaya");
                pin.then(|| self.place_of(receiver, depth)).flatten()
            }
            _ => None,
        }
    }
}
