//! A RECORDING `FrontEnd`: every question the back half asks of a program, with its answer,
//! written as text (shown.rs) into a log in the test's frame, the program answering as it
//! would with no wrapper. Two logs, one of each program over the same names, are compared
//! question by question (front_equal.rs).
//!
//! A question names what it is about by item key; a row index (`i`) is written as its fn's
//! key, a type as its tree. A question whose answer is the program's own shape rather than a
//! fact of the Rust program is forwarded and not logged: `fn_count`, `types`, `fn_index` (a row
//! index), `check_all` (the whole program's form), `make_asked`, `done`, `last_projection` (the
//! text of a cause), `static_records` (the statics made SO FAR: the lazy program makes them
//! one body at a time; each static's value is logged where the walk takes it, front_equal/
//! over.rs).
//!
//! THE PLANT (the control, rule 23): `plant: Some(key)` flips the ABI `fn_abi` answers for
//! `key` (Rust to C, C and rust-call to Rust). The flipped answer is the one logged and the one
//! the build is given, as a wrong program would give it.

use core::cell::{Cell, Ref, RefCell};
use core::fmt::{Arguments, Write as _};

use blaze_build::layout::{ReprAttrs, ValidRange};
use blaze_build::{Abi, CallerSite, FmtPiece, Instance, InstanceKey, Intrinsic, StaticItem};
use blaze_ids::ContentId;
use blaze_ladder::front::FnLang;
use blaze_ladder::front2::body_reads::ReadsDigest;
use blaze_ladder::front2::builtins_shim::Builtins;
use blaze_ladder::front2::const_at::{ConstAtWhy, ConstNeed};
use blaze_ladder::front2::start::RustStart;
use blaze_ladder::run::{ClosureTable, RunError};
use blaze_ladder::seam::{CheckedBodies, FnFacts, FrontEnd, LangRefusal, PositionGap, SeamData, SeamRecipe};
use blaze_own::{AString, AVec, Alloc};
use blaze_typeck::{Adt, Checked, ExprId, FmtTrait, FnSig, ItemKey, LangItem, List, Name, PTy, PTys, PatId, PortableTys, ResolveError};

use super::shown;

const WRITES: &str = "an answer writes into the test's frame";

/// One question and the answer it was given.
pub struct Entry<'a> {
    pub question: AString<'a>,
    pub answer: AString<'a>,
}

/// `inner` answering, every answer logged.
pub struct Asked<'a, P> {
    pub inner: P,
    log: RefCell<AVec<'a, Entry<'a>>>,
    plant: Option<ItemKey>,
    /// The first fn `fn_abi` answered `Some` for: the control's key.
    first_abi: Cell<Option<ItemKey>>,
    a: Alloc<'a>,
}

impl<'a, P: FrontEnd> Asked<'a, P> {
    pub fn new(inner: P, plant: Option<ItemKey>, a: Alloc<'a>) -> Asked<'a, P> {
        Asked { inner, log: RefCell::new(AVec::new_in(a)), plant, first_abi: Cell::new(None), a }
    }

    /// Every question asked, in the order asked.
    pub fn log(&self) -> Ref<'_, AVec<'a, Entry<'a>>> {
        self.log.borrow()
    }

    pub fn first_abi(&self) -> Option<ItemKey> {
        self.first_abi.get()
    }

    /// A question the walk's maker asks outside the seam (front_equal/over.rs: a static's value).
    pub fn note(&self, question: Arguments<'_>, answer: impl FnOnce(&mut AString<'a>) -> core::fmt::Result) {
        let mut q = AString::new_in(self.a);
        q.write_fmt(question).expect(WRITES);
        let mut o = AString::new_in(self.a);
        answer(&mut o).expect(WRITES);
        self.log.borrow_mut().push(Entry { question: q, answer: o });
    }

    fn text(&self) -> AString<'a> {
        AString::new_in(self.a)
    }

    fn push(&self, question: AString<'a>, answer: AString<'a>) {
        self.log.borrow_mut().push(Entry { question, answer });
    }

    fn key_of(&self, i: usize) -> ItemKey {
        self.inner.function(i).key
    }
}

/// A ground list's answer: the item and its arguments as trees.
fn instance(o: &mut AString<'_>, ground: &PortableTys<'_>, item: ItemKey, args: List) -> core::fmt::Result {
    write!(o, "{item:?}<")?;
    shown::args(o, ground.view(), args)?;
    o.write_str(">")
}

impl<'a, P: FrontEnd> FrontEnd for Asked<'a, P> {
    fn fn_count(&self) -> usize {
        self.inner.fn_count()
    }

    fn function(&self, i: usize) -> FnFacts<'_> {
        let f = self.inner.function(i);
        self.note(format_args!("function {:?}", f.key), |o| shown::facts(o, &f));
        f
    }

    fn c_variadic(&self, key: ItemKey) -> Option<u32> {
        let r = self.inner.c_variadic(key);
        self.note(format_args!("c_variadic {key:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn legacy_const_generics(&self, key: ItemKey) -> &[u32] {
        let r = self.inner.legacy_const_generics(key);
        self.note(format_args!("legacy_const_generics {key:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn entry(&self) -> Option<usize> {
        let r = self.inner.entry();
        self.note(format_args!("entry"), |o| write!(o, "{:?}", r.map(|i| self.key_of(i))));
        r
    }

    fn types(&self) -> PTys<'_> {
        self.inner.types()
    }

    fn body_types(&self, i: usize) -> PTys<'_> {
        self.inner.body_types(i)
    }

    fn resolve(&self, i: usize, item: ItemKey, args: List, ground: &mut PortableTys<'_>) -> Result<(ItemKey, List), ResolveError> {
        let mut q = self.text();
        write!(q, "resolve in {:?}: ", self.key_of(i)).expect(WRITES);
        instance(&mut q, ground, item, args).expect(WRITES);
        let r = self.inner.resolve(i, item, args, ground);
        let mut o = self.text();
        match &r {
            Ok((k, l)) => instance(&mut o, ground, *k, *l),
            Err(e) => write!(o, "{e:?}"),
        }
        .expect(WRITES);
        self.push(q, o);
        r
    }

    fn holds(&self, i: usize, trait_: ItemKey, ty: PTy, ground: &mut PortableTys<'_>) -> Result<bool, ResolveError> {
        let mut q = self.text();
        write!(q, "holds in {:?}: {trait_:?} for ", self.key_of(i)).expect(WRITES);
        shown::ty(&mut q, ground.view(), ty).expect(WRITES);
        let r = self.inner.holds(i, trait_, ty, ground);
        let mut o = self.text();
        write!(o, "{r:?}").expect(WRITES);
        self.push(q, o);
        r
    }

    fn adt_drop_impl(&self, i: usize, drop_trait: ItemKey, adt: ItemKey, alloc: Alloc<'_>) -> Result<Option<ItemKey>, ResolveError> {
        let r = self.inner.adt_drop_impl(i, drop_trait, adt, alloc);
        self.note(format_args!("adt_drop_impl in {:?}: {drop_trait:?} {adt:?}", self.key_of(i)), |o| write!(o, "{r:?}"));
        r
    }

    fn make_asked(&mut self, i: usize) -> Result<bool, RunError> {
        self.inner.make_asked(i)
    }

    fn sig(&self, i: usize) -> FnSig<'_> {
        let s = self.inner.sig(i);
        self.note(format_args!("sig {:?}", self.key_of(i)), |o| shown::sig(o, &s));
        s
    }

    fn fn_index(&self, key: ItemKey) -> Option<usize> {
        self.inner.fn_index(key)
    }

    fn specialized_row(&self, i: usize, args: &[PTy], ground: PTys<'_>) -> Option<usize> {
        self.inner.specialized_row(i, args, ground)
    }

    fn specialize_inline_for(&mut self, i: usize, args: &[PTy], ground: PTys<'_>) -> Result<Option<(usize, Option<Checked<'static>>)>, RunError> {
        self.inner.specialize_inline_for(i, args, ground)
    }

    fn soft_float_routines(&self, alloc: Alloc<'_>) -> Builtins {
        let r = self.inner.soft_float_routines(alloc);
        self.note(format_args!("soft_float_routines"), |o| write!(o, "{r:?}"));
        r
    }

    fn fn_abi(&self, key: ItemKey) -> Option<Abi> {
        let mut r = self.inner.fn_abi(key);
        if r.is_some() && self.first_abi.get().is_none() {
            self.first_abi.set(Some(key));
        }
        if self.plant == Some(key) {
            r = r.map(|abi| match abi {
                Abi::Rust => Abi::C,
                Abi::C | Abi::RustCall => Abi::Rust,
            });
        }
        self.note(format_args!("fn_abi {key:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn check_all(&mut self) -> Result<CheckedBodies, RunError> {
        self.inner.check_all()
    }

    fn body_reads(&self, i: usize, checked: &Checked<'static>) -> Option<ReadsDigest> {
        let r = self.inner.body_reads(i, checked);
        self.note(format_args!("body_reads {:?}", self.key_of(i)), |o| write!(o, "{r:?}"));
        r
    }

    fn check_one(&mut self, key: ItemKey) -> Result<Option<(usize, Checked<'static>)>, RunError> {
        let r = self.inner.check_one(key);
        self.note(format_args!("check_one {key:?}"), |o| match &r {
            Ok(Some(_)) => o.write_str("a checked body"),
            Ok(None) => o.write_str("none"),
            Err(e) => write!(o, "refused: {e}"),
        });
        r
    }

    fn head_row(&mut self, key: ItemKey) -> Result<Option<usize>, RunError> {
        let r = self.inner.head_row(key);
        self.note(format_args!("head_row {key:?}"), |o| match &r {
            Ok(Some(_)) => o.write_str("a row"),
            Ok(None) => o.write_str("none"),
            Err(e) => write!(o, "refused: {e}"),
        });
        r
    }

    fn done(&mut self, i: usize) {
        self.inner.done(i)
    }

    fn fn_lang(&self, l: FnLang) -> Option<ItemKey> {
        let r = self.inner.fn_lang(l);
        self.note(format_args!("fn_lang {l:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn drop_trait(&self) -> Option<(ItemKey, Option<ItemKey>)> {
        let r = self.inner.drop_trait();
        self.note(format_args!("drop_trait"), |o| write!(o, "{r:?}"));
        r
    }

    fn lang_item(&self, item: LangItem) -> Result<Option<ItemKey>, LangRefusal> {
        let r = self.inner.lang_item(item);
        self.note(format_args!("lang_item {item:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn lang_trait_fn(&self, trait_: LangItem, name: &str) -> Option<ItemKey> {
        let r = self.inner.lang_trait_fn(trait_, name);
        self.note(format_args!("lang_trait_fn {trait_:?} {name}"), |o| write!(o, "{r:?}"));
        r
    }

    fn object_trait(&self, t: ItemKey) -> Option<blaze_build::vtable::ObjectTrait<'_>> {
        let r = self.inner.object_trait(t);
        self.note(format_args!("object_trait {t:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn object_table(&mut self, t: ItemKey) -> Result<bool, RunError> {
        let r = self.inner.object_table(t);
        self.note(format_args!("object_table {t:?}"), |o| match &r {
            Ok(made) => write!(o, "{made}"),
            Err(e) => write!(o, "refused: {e}"),
        });
        r
    }

    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        let r = self.inner.adt(key);
        self.note(format_args!("adt {key:?}"), |o| shown::adt(o, r));
        r
    }

    fn item_named(&self, key: ItemKey) -> Option<Box<str>> {
        let r = self.inner.item_named(key);
        self.note(format_args!("item_named {key:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn left_out(&self, key: ItemKey) -> Option<&str> {
        let r = self.inner.left_out(key);
        self.note(format_args!("left_out {key:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn nounwind(&self, key: ItemKey) -> Option<bool> {
        let r = self.inner.nounwind(key);
        self.note(format_args!("nounwind {key:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn track_caller(&self, key: ItemKey) -> Option<bool> {
        let r = self.inner.track_caller(key);
        self.note(format_args!("track_caller {key:?}"), |o| write!(o, "{r:?}"));
        r
    }

    // The site only, not the gap: the two fronts give no place by different steps (the old
    // front's `NotLive`, the lazy one's own), and the log compares what was answered.
    fn caller_site(&self, owner: ItemKey, expr: ExprId) -> Result<CallerSite<'_>, blaze_build::SiteGap> {
        let r = self.inner.caller_site(owner, expr);
        self.note(format_args!("caller_site {owner:?} {expr:?}"), |o| write!(o, "{:?}", r.as_ref().ok()));
        r
    }

    fn def_site(&self, item: ItemKey) -> Result<CallerSite<'_>, blaze_build::SiteGap> {
        let r = self.inner.def_site(item);
        self.note(format_args!("def_site {item:?}"), |o| write!(o, "{:?}", r.as_ref().ok()));
        r
    }

    fn site_heads(&mut self, owner: ItemKey) -> Result<(), RunError> {
        let r = self.inner.site_heads(owner);
        self.note(format_args!("site_heads {owner:?}"), |o| match &r {
            Ok(()) => write!(o, "asked"),
            Err(e) => write!(o, "refused: {e}"),
        });
        r
    }

    fn expr_site(&self, owner: ItemKey, expr: ExprId) -> Option<u32> {
        let r = self.inner.expr_site(owner, expr);
        self.note(format_args!("expr_site {owner:?} {expr:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn source_position(&self, item: ItemKey, site: u32) -> Result<(u32, u32), PositionGap> {
        let r = self.inner.source_position(item, site);
        self.note(format_args!("source_position {item:?} {site}"), |o| write!(o, "{r:?}"));
        r
    }

    fn rust_start(&self) -> Option<&RustStart> {
        let r = self.inner.rust_start();
        self.note(format_args!("rust_start"), |o| match r {
            Some(s) => {
                write!(o, "lang_start {:?} main {:?} returns ", s.lang_start, s.main)?;
                shown::ty(o, self.inner.types(), s.returns)
            }
            None => o.write_str("none"),
        });
        r
    }

    fn lang_named(&self, name: &str) -> Option<ItemKey> {
        let r = self.inner.lang_named(name);
        self.note(format_args!("lang_named {name}"), |o| write!(o, "{r:?}"));
        r
    }

    fn coroutine_recipe(&self, owner: ItemKey, ordinal: u32) -> Option<Result<&SeamRecipe, blaze_flow_lower::Refusal>> {
        let r = self.inner.coroutine_recipe(owner, ordinal);
        self.note(format_args!("coroutine_recipe {owner:?} #{ordinal}"), |o| match &r {
            Some(Ok(_)) => o.write_str("a recipe"),
            Some(Err(e)) => write!(o, "refused {e:?}"),
            None => o.write_str("none"),
        });
        r
    }

    fn opaque_of(&self, opaque: ItemKey) -> Option<(ItemKey, u32)> {
        let r = self.inner.opaque_of(opaque);
        self.note(format_args!("opaque_of {opaque:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn alias_definer(&self, alias: ItemKey) -> Result<ItemKey, blaze_ladder::front2::tait::DefinerMiss> {
        let r = self.inner.alias_definer(alias);
        self.note(format_args!("alias_definer {alias:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn last_projection(&self) -> Option<Box<str>> {
        self.inner.last_projection()
    }

    fn const_record(&self, item: ItemKey) -> Option<&SeamData> {
        let r = self.inner.const_record(item);
        self.note(format_args!("const_record {item:?}"), |o| match r {
            Some(d) => shown::data(o, d),
            None => o.write_str("none"),
        });
        r
    }

    fn const_at(&mut self, need: ConstNeed, from: PTys<'_>, args: &[PTy], closures: &ClosureTable) -> Result<SeamData, ConstAtWhy> {
        let mut q = self.text();
        write!(q, "const_at {need:?} <").expect(WRITES);
        shown::types(&mut q, from, args).expect(WRITES);
        q.push_str(">");
        let r = self.inner.const_at(need, from, args, closures);
        let mut o = self.text();
        match &r {
            Ok(d) => shown::data(&mut o, d),
            Err(why) => write!(o, "{why:?}"),
        }
        .expect(WRITES);
        self.push(q, o);
        r
    }

    fn const_fn_instance(&self, key: InstanceKey, ground: &mut PortableTys<'_>, scratch: Alloc<'_>) -> Option<Instance> {
        let r = self.inner.const_fn_instance(key, ground, scratch);
        let mut o = self.text();
        match &r {
            Some(Instance { item, args }) => instance(&mut o, ground, *item, *args),
            None => o.write_str("none"),
        }
        .expect(WRITES);
        let mut q = self.text();
        write!(q, "const_fn_instance {key:?}").expect(WRITES);
        self.push(q, o);
        r
    }

    fn evaluates_at_args(&self) -> bool {
        let r = self.inner.evaluates_at_args();
        self.note(format_args!("evaluates_at_args"), |o| write!(o, "{r}"));
        r
    }

    fn static_records(&self) -> &[(ItemKey, SeamData)] {
        self.inner.static_records()
    }

    fn format_template(&self, owner: ItemKey, e: ExprId) -> Option<&[FmtPiece<'_>]> {
        let r = self.inner.format_template(owner, e);
        self.note(format_args!("format_template {owner:?} {e:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn fmt_argument_new(&self, trait_: FmtTrait) -> Option<ItemKey> {
        let r = self.inner.fmt_argument_new(trait_);
        self.note(format_args!("fmt_argument_new {trait_:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn fmt_argument_adt(&self) -> Option<ItemKey> {
        let r = self.inner.fmt_argument_adt();
        self.note(format_args!("fmt_argument_adt"), |o| write!(o, "{r:?}"));
        r
    }

    fn int_lit(&self, owner: ItemKey, e: ExprId) -> Option<u128> {
        let r = self.inner.int_lit(owner, e);
        self.note(format_args!("int_lit {owner:?} {e:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn int_pat(&self, owner: ItemKey, p: PatId) -> Option<u128> {
        let r = self.inner.int_pat(owner, p);
        self.note(format_args!("int_pat {owner:?} {p:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn string(&self, owner: ItemKey, e: ExprId) -> Option<&[u8]> {
        let r = self.inner.string(owner, e);
        self.note(format_args!("string {owner:?} {e:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn float_lit(&self, owner: ItemKey, e: ExprId) -> Option<(u64, u32)> {
        let r = self.inner.float_lit(owner, e);
        self.note(format_args!("float_lit {owner:?} {e:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn float_pat(&self, owner: ItemKey, p: PatId) -> Option<(u64, u32)> {
        let r = self.inner.float_pat(owner, p);
        self.note(format_args!("float_pat {owner:?} {p:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn str_pat(&self, owner: ItemKey, p: PatId) -> Option<&[u8]> {
        let r = self.inner.str_pat(owner, p);
        self.note(format_args!("str_pat {owner:?} {p:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn normalize(&self, trait_: ItemKey, name: Name, args: &[PTy], ground: &mut PortableTys<'_>) -> Result<Option<PTy>, ResolveError> {
        let mut q = self.text();
        write!(q, "normalize <{trait_:?}>::{name:?} <").expect(WRITES);
        shown::types(&mut q, ground.view(), args).expect(WRITES);
        q.push_str(">");
        let r = self.inner.normalize(trait_, name, args, ground);
        let mut o = self.text();
        match &r {
            Ok(Some(t)) => shown::ty(&mut o, ground.view(), *t),
            Ok(None) => o.write_str("none"),
            Err(e) => write!(o, "{e:?}"),
        }
        .expect(WRITES);
        self.push(q, o);
        r
    }

    fn repr(&self, key: ItemKey) -> Result<ReprAttrs, blaze_build::layout::Missing> {
        let r = self.inner.repr(key);
        self.note(format_args!("repr {key:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn valid_range(&self, key: ItemKey) -> Result<ValidRange, blaze_build::layout::Missing> {
        let r = self.inner.valid_range(key);
        self.note(format_args!("valid_range {key:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn intrinsic(&self, item: ItemKey) -> Option<Intrinsic> {
        let r = self.inner.intrinsic(item);
        self.note(format_args!("intrinsic {item:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn foreign_static(&self, key: ItemKey) -> Option<&str> {
        let r = self.inner.foreign_static(key);
        self.note(format_args!("foreign_static {key:?}"), |o| write!(o, "{r:?}"));
        r
    }
    fn static_item(&self, key: ItemKey) -> Option<StaticItem> {
        let r = self.inner.static_item(key);
        self.note(format_args!("static_item {key:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn discriminant(&self, adt: ItemKey, variant: u32) -> Result<Option<i128>, blaze_build::layout::Missing> {
        let r = self.inner.discriminant(adt, variant);
        self.note(format_args!("discriminant {adt:?} {variant}"), |o| write!(o, "{r:?}"));
        r
    }

    fn ctor_of(&self, key: ItemKey) -> Option<(ItemKey, u32)> {
        let r = self.inner.ctor_of(key);
        self.note(format_args!("ctor_of {key:?}"), |o| write!(o, "{r:?}"));
        r
    }

    fn code_of(&self, i: usize) -> ContentId {
        let r = self.inner.code_of(i);
        self.note(format_args!("code_of {:?}", self.key_of(i)), |o| write!(o, "{r:?}"));
        r
    }
}
