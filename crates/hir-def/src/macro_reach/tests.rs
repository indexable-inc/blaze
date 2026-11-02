use base_db::all_crates;
use blaze_hash::HashMap;
use hir_expand::fixture;

use super::{MacroReachable, Reached, macro_reach_module};
use crate::{
    DefWithBodyId, MacroId, ModuleDefId, ModuleId,
    nameres::{DefMap, crate_def_map, demand::body_blocks},
    test_db::TestDB,
};

/// A public `macro` three modules down, with names of each kind around it: private ones, a
/// nested module private to the macro's module, a struct with a private field, an item private
/// to a module the macro can't see into, a name of another crate, and macros of every
/// transparency, in modules and in blocks.
const CRATE: &str = r#"
//- /dep.rs crate:dep
pub struct Thing;
//- /main.rs crate:main deps:dep
#![feature(decl_macro, rustc_attrs)]
mod outer {
    pub fn helper() {}
    fn outer_private() {}
    pub(crate) mod m {
        pub use dep::Thing;
        pub fn f() {}
        fn private() {}
        pub struct S { pub a: u8, b: u16 }
        pub(super) mod inner { pub fn deep() {} }
        mod hidden {
            pub struct T { pub x: u8, y: u8 }
            pub fn h() {}
            fn secret() {}
        }
        pub macro mac() {}
    }
    mod sibling { fn unseen() {} }
}
pub use outer::m::mac;
pub fn top() {}
macro_rules! rules { () => {} }
#[rustc_macro_transparency = "opaque"]
macro_rules! opaque_rules { () => {} }
#[rustc_macro_transparency = "semitransparent"]
pub macro semi() {}
fn g() {
    pub macro in_block() {}
    mod nested { pub macro in_nested() {} }
}
"#;

/// The names of what the def maps declare, by definition, and their macros by name.
struct Names {
    of: HashMap<ModuleDefId, String>,
    macros: HashMap<String, MacroId>,
}

impl Names {
    fn of(db: &TestDB, def_maps: &[&DefMap]) -> Names {
        let mut names = Names { of: HashMap::default(), macros: HashMap::default() };
        for def_map in def_maps {
            for (_, data) in def_map.modules(db) {
                for (name, per_ns) in data.scope(db).entries() {
                    let name = name.as_str().to_owned();
                    let types = per_ns.types.filter(|it| it.import.is_none()).map(|it| it.def);
                    let values = per_ns.values.filter(|it| it.import.is_none()).map(|it| it.def);
                    for def in types.into_iter().chain(values) {
                        names.of.insert(def, name.clone());
                    }
                    if let Some(it) = per_ns.macros.filter(|it| it.import.is_none()) {
                        names.of.insert(ModuleDefId::MacroId(it.def), name.clone());
                        names.macros.insert(name, it.def);
                    }
                }
                // A `macro_rules!` is in the module's textual scope, not its entries.
                for (name, defs) in data.scope(db).legacy_macros() {
                    let Some(&def) = defs.last() else { continue };
                    let name = name.as_str().to_owned();
                    names.of.entry(ModuleDefId::MacroId(def)).or_insert_with(|| name.clone());
                    names.macros.entry(name).or_insert(def);
                }
            }
        }
        names
    }

    fn module(&self, name: &str) -> ModuleId {
        self.of
            .iter()
            .find_map(|(def, it)| match def {
                ModuleDefId::ModuleId(module) if it == name => Some(*module),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no module {name}"))
    }

    fn function(&self, name: &str) -> DefWithBodyId {
        self.of
            .iter()
            .find_map(|(def, it)| match def {
                ModuleDefId::FunctionId(function) if it == name => Some((*function).into()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no function {name}"))
    }

    fn reached(&self, db: &TestDB, reached: Reached) -> String {
        match reached {
            Reached::Def(def) => self.of[&def].clone(),
            Reached::FieldType(field) => {
                let owner = match field.parent {
                    crate::VariantId::StructId(it) => ModuleDefId::AdtId(it.into()),
                    crate::VariantId::UnionId(it) => ModuleDefId::AdtId(it.into()),
                    crate::VariantId::EnumVariantId(it) => ModuleDefId::EnumVariantId(it),
                };
                let fields = field.parent.fields(db).fields();
                format!("{}.{}", self.of[&owner], fields[field.local_id].name.as_str())
            }
        }
    }
}

fn main_def_map(db: &TestDB) -> &DefMap {
    let main = *all_crates(db)
        .iter()
        .find(|it| {
            it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == "main")
        })
        .expect("the fixture has a crate `main`");
    crate_def_map(db, main)
}

#[test]
fn only_an_opaque_macro_of_a_module_takes_part() {
    let db = fixture::db(CRATE).into_db();
    let def_map = main_def_map(&db);
    let names = Names::of(&db, &[def_map]);
    let blocks = body_blocks(&db, names.function("g"));
    let block_maps: Vec<&DefMap> = blocks.blocks.iter().map(|&(_, it)| it).collect();
    let in_blocks = Names::of(&db, &block_maps);
    let reach = |names: &Names, name: &str| macro_reach_module(&db, names.macros[name]);

    assert_eq!(reach(&names, "mac"), Some(names.module("m")));
    assert_eq!(reach(&names, "opaque_rules"), Some(def_map.root_module_id()));
    assert_eq!(reach(&names, "semi"), None);
    assert_eq!(reach(&in_blocks, "in_block"), None);
    assert_eq!(reach(&in_blocks, "in_nested"), Some(in_blocks.module("nested")));
}

#[test]
fn a_plain_macro_rules_takes_no_part() {
    let db = fixture::db(CRATE).into_db();
    let def_map = main_def_map(&db);
    let rules = def_map
        .module(&db, def_map.root_module_id())
        .scope(&db)
        .legacy_macros()
        .find(|(name, _)| name.as_str() == "rules")
        .map(|(_, ids)| ids[0])
        .expect("`rules` is in the root's textual scope");
    assert_eq!(macro_reach_module(&db, rules), None);
}

/// rustc's passes: the first walks the macro's module and the modules in it it can see into,
/// each later one the next ancestor, until the crate root has been walked.
#[test]
fn each_pass_walks_one_module_further_up() {
    let db = fixture::db(CRATE).into_db();
    let def_map = main_def_map(&db);
    let names = Names::of(&db, &[def_map]);
    let defining = macro_reach_module(&db, names.macros["mac"]).unwrap();
    blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| {
        let mut reachable = MacroReachable::new_in(frame);
        let mut pass = || {
            let mut reached = Vec::new();
            reachable.update_from_macro(&db, defining, |it| reached.push(names.reached(&db, it)));
            reached.sort();
            reached
        };
        assert_eq!(
            pass(),
            ["S", "S.a", "S.b", "T", "T.x", "deep", "f", "h", "hidden", "inner", "mac", "private"]
        );
        assert_eq!(pass(), ["helper", "m", "outer_private", "sibling"]);
        assert_eq!(pass(), ["g", "mac", "outer", "semi", "top"]);
        assert!(pass().is_empty());
    });
}

/// The walked pairs are kept per macro's module: a second macro of another module walks the
/// modules the first did again, asking what its own module can see.
#[test]
fn walked_modules_are_kept_per_macros_module() {
    let db = fixture::db(CRATE).into_db();
    let def_map = main_def_map(&db);
    let names = Names::of(&db, &[def_map]);
    let m = names.module("m");
    let root = def_map.root_module_id();
    blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| {
        let mut reachable = MacroReachable::new_in(frame);
        let mut walk = |module: ModuleId, defining: ModuleId| {
            let mut reached = Vec::new();
            let new = reachable.update_macro_reachable(&db, module, defining, &mut |it| {
                reached.push(names.reached(&db, it))
            });
            reached.sort();
            (new, reached)
        };
        let (new, from_m) = walk(m, m);
        assert!(new);
        assert!(from_m.contains(&"private".to_owned()));
        assert_eq!(walk(m, m), (false, Vec::new()));
        let (new, from_root) = walk(m, root);
        assert!(new);
        assert_eq!(from_root, ["S", "S.a", "f", "mac"]);
    });
}
