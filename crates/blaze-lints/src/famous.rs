//! The well-known items of the standard library the lints compare with: found through the lang
//! items of the crate being linted, or by path in its `core`, `alloc` or `std`.

use base_db::{CrateOrigin, Db, LangCrateOrigin};
use hir_def::{ModuleDefId, ModuleId, lang_item::lang_items};
use hir_expand::name::Name;
use intern::Symbol;

/// Every path of `core`, `alloc` and `std` a lint looks up ([`crate::check::Cx::std_def`]),
/// written `crate:module:...:name`. A crate resolves them all once
/// ([`crate::query::famous_defs`]); a lookup is one perfect hash of the path and one load.
pub(crate) const KNOWN: &[&str] = &[
    "alloc:alloc:Global",
    "alloc:borrow:Cow",
    "alloc:borrow:ToOwned",
    "alloc:collections:BTreeMap",
    "alloc:collections:BTreeSet",
    "alloc:collections:VecDeque",
    "alloc:rc:Rc",
    "alloc:rc:Weak",
    "alloc:string:String",
    "alloc:string:ToString",
    "alloc:sync:Arc",
    "alloc:sync:Weak",
    "alloc:vec:IntoIter",
    "alloc:vec:Vec",
    "core:any:Any",
    "core:array:IntoIter",
    "core:cmp:Ord",
    "core:cmp:max",
    "core:cmp:min",
    "core:convert:AsRef",
    "core:error:Error",
    "core:fmt:Debug",
    "core:fmt:Display",
    "core:hint:spin_loop",
    "core:iter:Chain",
    "core:iter:Cloned",
    "core:iter:Copied",
    "core:iter:Enumerate",
    "core:iter:Filter",
    "core:iter:FilterMap",
    "core:iter:Iterator",
    "core:iter:Map",
    "core:iter:Rev",
    "core:iter:Skip",
    "core:iter:StepBy",
    "core:iter:Take",
    "core:iter:Zip",
    "core:marker:Send",
    "core:marker:Sync",
    "core:mem:ManuallyDrop",
    "core:ops:Deref",
    "core:result:Result",
    "core:slice:Chunks",
    "core:slice:ChunksExact",
    "core:slice:ChunksExactMut",
    "core:slice:ChunksMut",
    "core:slice:Iter",
    "core:slice:IterMut",
    "core:slice:Windows",
    "std:alloc:Global",
    "std:array:IntoIter",
    "std:cmp:Ord",
    "std:cmp:max",
    "std:cmp:min",
    "std:collections:BTreeMap",
    "std:collections:BTreeSet",
    "std:collections:HashMap",
    "std:collections:HashSet",
    "std:collections:VecDeque",
    "std:ffi:OsStr",
    "std:fs",
    "std:fs:File",
    "std:fs:OpenOptions",
    "std:hash:RandomState",
    "std:hint:spin_loop",
    "std:io:Error",
    "std:io:ErrorKind",
    "std:io:Read",
    "std:io:Write",
    "std:iter:Chain",
    "std:iter:Cloned",
    "std:iter:Copied",
    "std:iter:Enumerate",
    "std:iter:Filter",
    "std:iter:FilterMap",
    "std:iter:Map",
    "std:iter:Rev",
    "std:iter:Skip",
    "std:iter:StepBy",
    "std:iter:Take",
    "std:iter:Zip",
    "std:marker:Send",
    "std:marker:Sync",
    "std:net:TcpListener",
    "std:net:TcpStream",
    "std:net:UdpSocket",
    "std:path:Path",
    "std:slice:Chunks",
    "std:slice:ChunksExact",
    "std:slice:ChunksExactMut",
    "std:slice:ChunksMut",
    "std:slice:Iter",
    "std:slice:IterMut",
    "std:slice:Windows",
    "std:string:String",
    "std:sync:Condvar",
    "std:sync:mpsc:Receiver",
    "std:thread:JoinHandle",
    "std:thread:LocalKey",
    "std:thread:park",
    "std:thread:park_timeout",
    "std:thread:sleep",
    "std:vec:IntoIter",
    "std:vec:Vec",
];

blaze_phf::phf_table!(static KNOWN_AT = KNOWN;);

/// The length of the longest path of [`KNOWN`]: a longer path is none of them.
const LONGEST: usize = {
    let (mut longest, mut at) = (0, 0);
    while at < KNOWN.len() {
        if KNOWN[at].len() > longest {
            longest = KNOWN[at].len();
        }
        at += 1;
    }
    longest
};

/// The index of `name` in [`KNOWN`]; a name not there fails to compile.
const fn known_index(name: &str) -> usize {
    let mut at = 0;
    while at < KNOWN.len() {
        if blaze_phf::bytes_eq(KNOWN[at].as_bytes(), name.as_bytes()) {
            return at;
        }
        at += 1;
    }
    panic!("a path not in `KNOWN`");
}

const RESULT: usize = known_index("core:result:Result");

/// The index in [`KNOWN`] of the path of segments `path`: joined by `:` in a buffer on the
/// stack, then one perfect hash.
pub(crate) fn known(path: &[&str]) -> Option<usize> {
    let mut joined = [0u8; LONGEST];
    let mut len = 0;
    for (at, segment) in path.iter().enumerate() {
        let start = len + usize::from(at > 0);
        let end = start + segment.len();
        if end > LONGEST {
            return None;
        }
        if at > 0 {
            joined[len] = b':';
        }
        joined[start..end].copy_from_slice(segment.as_bytes());
        len = end;
    }
    KNOWN_AT.get(std::str::from_utf8(&joined[..len]).ok()?)
}

#[derive(Clone, Copy)]
pub(crate) struct Famous<'db> {
    db: &'db Db,
    krate: base_db::Crate,
}

#[allow(non_snake_case)]
impl<'db> Famous<'db> {
    pub(crate) fn new(db: &'db Db, krate: hir::Crate) -> Famous<'db> {
        Famous { db, krate: krate.base() }
    }

    pub(crate) fn core(&self) -> Option<hir::Crate> {
        self.lang_crate(LangCrateOrigin::Core).map(hir::Crate::from)
    }

    pub(crate) fn alloc(&self) -> Option<hir::Crate> {
        self.lang_crate(LangCrateOrigin::Alloc).map(hir::Crate::from)
    }

    pub(crate) fn std(&self) -> Option<hir::Crate> {
        self.lang_crate(LangCrateOrigin::Std).map(hir::Crate::from)
    }

    pub(crate) fn core_option_Option(&self) -> Option<hir::Enum> {
        lang_items(self.db, self.krate).Option.map(hir::Enum::from)
    }

    pub(crate) fn core_result_Result(&self) -> Option<hir::Enum> {
        match crate::query::famous_defs(self.db, self.krate)[RESULT]? {
            hir::ModuleDef::Adt(hir::Adt::Enum(it)) => Some(it),
            _ => None,
        }
    }

    pub(crate) fn core_default_Default(&self) -> Option<hir::Trait> {
        lang_items(self.db, self.krate).Default.map(hir::Trait::from)
    }

    pub(crate) fn core_iter_Iterator(&self) -> Option<hir::Trait> {
        lang_items(self.db, self.krate).Iterator.map(hir::Trait::from)
    }

    pub(crate) fn core_iter_IntoIterator(&self) -> Option<hir::Trait> {
        lang_items(self.db, self.krate).IntoIterator.map(hir::Trait::from)
    }

    /// The item at `path`, written `crate:module:...:name` with `crate` one of `core`, `alloc`,
    /// `std`, `proc_macro` and `test`.
    pub(crate) fn find(&self, path: &str) -> Option<hir::ModuleDef> {
        let mut segments = path.split(':');
        let name = segments.next_back()?;
        let origin = match LangCrateOrigin::from(segments.next()?) {
            LangCrateOrigin::Other | LangCrateOrigin::Dependency => return None,
            origin => origin,
        };
        let krate = self.lang_crate(origin)?;
        let mut module = hir_def::nameres::crate_def_map(self.db, krate).root_module_id();
        for segment in segments {
            match self.scope_get(module, segment).take_types()? {
                ModuleDefId::ModuleId(it) => module = it,
                _ => return None,
            }
        }
        let found = self.scope_get(module, name);
        let def = match found.take_types().or_else(|| found.take_values()) {
            Some(def) => def,
            None => ModuleDefId::MacroId(found.take_macros()?),
        };
        Some(hir::ModuleDef::from(def))
    }

    fn scope_get(&self, module: ModuleId, name: &str) -> hir_def::per_ns::PerNs {
        let name = Name::new_symbol_root(Symbol::intern(name));
        module.def_map(self.db).module(self.db, module).scope(self.db).get(&name)
    }

    fn lang_crate(&self, origin: LangCrateOrigin) -> Option<base_db::Crate> {
        let wanted = CrateOrigin::Lang(origin);
        let data = self.krate.data(self.db);
        if data.origin == wanted {
            return Some(self.krate);
        }
        data.dependencies
            .iter()
            .map(|dep| dep.crate_id)
            .find(|&dep| dep.data(self.db).origin == wanted)
    }
}

#[cfg(test)]
mod tests {
    use base_db::Db;
    use hir_expand::fixture;

    use super::{Famous, KNOWN, KNOWN_AT, LONGEST, known};

    #[test]
    fn a_path_is_found_by_its_segments_at_its_place() {
        for (at, path) in KNOWN.iter().enumerate() {
            assert_eq!(KNOWN_AT.get(path), Some(at), "{path}");
            let segments: Vec<&str> = path.split(':').collect();
            assert_eq!(known(&segments), Some(at), "{path}");
        }
        // As `Famous::find` reads a path: its segments joined by `:`, then the whole compared.
        let long = "x".repeat(LONGEST);
        for path in [
            &["core", "iter"][..],
            &["core", "iter", "Iterators"],
            &["core:iter", "Iterator"],
            &["core", "iter", "Iterator", "next"],
            &["std", long.as_str()],
            &[],
        ] {
            let joined = path.join(":");
            assert_eq!(known(path), KNOWN.iter().position(|it| *it == joined), "{path:?}");
        }
        assert_eq!(known(&["core", "iter", "Iterators"]), None);
        assert_eq!(known(&["std", long.as_str()]), None);
    }

    /// What a crate's table holds of each path is what resolving the path alone finds.
    #[test]
    fn the_crates_table_finds_what_each_path_alone_does() {
        let fixture = format!(
            r#"
//- minicore: iterators, option, result, fmt, deref, as_ref, clone, copy
//- /src/lib.rs crate:main deps:std
fn f() {{}}
{}"#,
            crate::tests::STD
        );
        let db = fixture::db(&fixture).into_db();
        hir::attach_db(&db, || {
            let krate = crate::tests::main_crate(&db);
            let famous = Famous::new(&db, krate);
            let table = crate::query::famous_defs(&db, krate.base());
            assert_eq!(table.len(), KNOWN.len());
            let mut found = 0;
            for (at, path) in KNOWN.iter().enumerate() {
                assert_eq!(table[at], famous.find(path), "{path}");
                found += usize::from(table[at].is_some());
            }
            assert!(found > 0, "the fixture's `core` has some of them");
            assert_eq!(
                famous.core_result_Result(),
                match famous.find("core:result:Result") {
                    Some(hir::ModuleDef::Adt(hir::Adt::Enum(it))) => Some(it),
                    _ => None,
                }
            );
        });
    }
}
