//! Which `Drop` impls only free memory (docs/v2/plan/design/early-drop.md, section 1), over the
//! stand-in toolchain: its `Vec` and `String` free and nothing else; a destructor that reads
//! the environment, calls foreign code or may panic does more; and `#[blaze::drop(..)]` is
//! held to the impl.

use base_db::Db;
use hir_def::{AdtId, ModuleDefId, nameres::crate_def_map};
use hir_expand::name::Name;
use intern::Symbol;

use super::fixture::{crate_named, db_with};
use crate::{
    Rule,
    drop_effects::{check_drop_attr, drop_effects, frees_only},
};

const TYPES: &str = r#"
//- /lib.rs crate:kernel deps:std,alloc
unsafe extern "C" {
    fn my_unlock(lock: usize) -> i32;
}

/// A container of the crate's own: its drop frees its buffer.
#[blaze::drop(early)]
pub struct Stack<T> {
    ptr: *mut T,
    len: usize,
}
impl<T> Drop for Stack<T> {
    fn drop(&mut self) {
        unsafe { alloc::alloc::dealloc(self.ptr as *mut u8, self.len, 1) }
    }
}

/// Holds toolchain containers, with nothing of its own to run.
pub struct Plain {
    names: Vec<String>,
}

pub struct Reader;
impl Drop for Reader {
    fn drop(&mut self) {
        let _ = std::env::var("HOME");
    }
}

/// A guard: its drop unlocks through foreign code.
pub struct Guard {
    lock: usize,
}
impl Drop for Guard {
    fn drop(&mut self) {
        unsafe { my_unlock(self.lock) };
    }
}

pub struct Checked {
    live: bool,
}
impl Drop for Checked {
    fn drop(&mut self) {
        if self.live {
            core::panicking::panic("dropped while live");
        }
    }
}

#[blaze::drop(early)]
pub struct Loud;
impl Drop for Loud {
    fn drop(&mut self) {
        let _ = std::env::var("HOME");
    }
}

#[blaze::drop(early, trusted = "gives its slot back to the pool")]
pub struct Pooled {
    lock: usize,
}
impl Drop for Pooled {
    fn drop(&mut self) {
        unsafe { my_unlock(self.lock) };
    }
}

#[blaze::drop(early)]
pub struct NoImpl {
    names: Vec<String>,
}

#[blaze::drop(scoped)]
pub struct Nothing(u32);

#[blaze::drop(scoped)]
pub struct Token {
    names: Vec<String>,
}

#[blaze::drop(early)]
#[blaze::drop(scoped)]
pub struct Both;
impl Drop for Both {
    fn drop(&mut self) {}
}
"#;

/// The struct, enum or union `name` at the root of `krate`.
#[track_caller]
fn adt_named(db: &Db, krate: base_db::Crate, name: &str) -> AdtId {
    let root = crate_def_map(db, krate).root_module_id();
    let data = root.to_db(db).def_map(db).module(db, root);
    match data.name_in_scope(db, &Name::new_symbol_root(Symbol::intern(name))).take_types() {
        Some(ModuleDefId::AdtId(adt)) => adt,
        _ => panic!("no type `{name}` at the root of the crate"),
    }
}

/// The stand-in `alloc`'s type `name`.
#[track_caller]
fn toolchain_adt(db: &Db, name: &str) -> AdtId {
    let krate = crate_named(db, "alloc");
    let def_map = crate_def_map(db, krate);
    for (_, data) in def_map.modules(db) {
        for def in data.scope(db).declarations() {
            if let ModuleDefId::AdtId(adt) = def
                && hir_ty_mir::mir::early_drop::class::adt_name(db, adt) == name
            {
                return adt;
            }
        }
    }
    panic!("no `{name}` in the stand-in `alloc`")
}

#[test]
fn the_toolchains_containers_only_free() {
    let db = db_with(TYPES);
    blaze_query::db::attach(&db, || {
        for name in ["Vec", "String"] {
            let adt = toolchain_adt(&db, name);
            let kinds = drop_effects(&db, adt).unwrap_or_else(|| panic!("`{name}` has a drop"));
            assert!(kinds.has(Rule::Alloc), "`{name}` frees: {kinds:?}");
            assert_eq!(frees_only(&db, adt), Ok(()), "`{name}`: {kinds:?}");
        }
    });
}

#[test]
fn a_destructor_is_frees_only_by_what_its_body_does() {
    let db = db_with(TYPES);
    let krate = crate_named(&db, "kernel");
    blaze_query::db::attach(&db, || {
        let of = |name: &str| frees_only(&db, adt_named(&db, krate, name));
        assert_eq!(of("Stack"), Ok(()), "a container that frees its buffer");
        assert_eq!(of("Plain"), Ok(()), "a type with no `Drop` impl has nothing to refuse");
        assert_eq!(of("Reader"), Err(Rule::Env), "a destructor that reads the environment");
        assert_eq!(of("Guard"), Err(Rule::Foreign), "a guard that unlocks through foreign code");
        assert_eq!(of("Checked"), Err(Rule::Panic), "a destructor of the crate's that may panic");
    });
}

#[test]
fn the_attribute_is_held_to_the_impl() {
    let db = db_with(TYPES);
    let krate = crate_named(&db, "kernel");
    blaze_query::db::attach(&db, || {
        let of = |name: &str| check_drop_attr(&db, adt_named(&db, krate, name));
        for fine in ["Stack", "Plain", "Guard", "Pooled", "Token"] {
            assert_eq!(of(fine), Ok(()), "`{fine}`");
        }
        let loud = of("Loud").expect_err("`Loud` reads the environment in its drop");
        assert!(
            loud.contains("`Loud` is `#[blaze::drop(early)]`")
                && loud.contains("reads the environment"),
            "{loud}"
        );
        let no_impl = of("NoImpl").expect_err("`NoImpl` has no `Drop` impl to mark");
        assert!(no_impl.contains("has no `Drop` impl"), "{no_impl}");
        let nothing = of("Nothing").expect_err("`Nothing` drops nothing");
        assert!(nothing.contains("dropping it runs nothing"), "{nothing}");
        let both = of("Both").expect_err("`Both` says both words");
        assert!(both.contains("both `#[blaze::drop(early)]` and `#[blaze::drop(scoped)]`"), "{both}");
    });
}
