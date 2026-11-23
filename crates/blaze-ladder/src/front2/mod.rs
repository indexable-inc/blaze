//! The new front end of the ladder's runner (README, "For a successor lane (03:30)"): the
//! checker's `World` over every crate's names as the names library (blaze-scope) resolved them.
//! It holds no lookup rule of its own (rule 30): every path goes through [`crate::resolve`],
//! every impl and lang item comes from the library's tables.
//!
//! | module | what |
//! |---|---|
//! | [`texts`] | the one text table: the user's files first, then every crate's files and expansions |
//! | [`keys`] | the one map between library keys (32 bytes) and checker keys (16 bytes); members' keys |
//! | [`index`] | every impl and lang item, from the library's tables |
//! | [`types`] | a written type to a portable one through the resolver; `Why::Need` for a head not lowered yet |

pub mod adt_facts;
pub mod adt_outlives;
pub mod adt_variance;
pub mod anon;
pub mod asks;
mod async_output;
mod back_needs;
pub mod block_crates;
pub mod scope_table;
pub mod expansion_origin;
pub mod block_traits;
pub mod block_type_uses;
pub mod body;
pub mod body_host;
pub mod body_scopes;
#[cfg(test)]
mod body_host_include_tests;
pub mod body_left;
pub mod body_reads;
pub mod borrow_reads;
pub mod borrows;
pub mod borrows_print;
pub mod borrows_nested;
pub mod borrows_unmade;
mod bound_kids;
pub mod buckets;
pub mod call_generics;
pub mod check;
pub mod by_origin;
pub mod const_at;
mod const_args_in_bounds;
mod const_enum;
mod const_open;
mod const_param_ty;
pub mod const_inherent;
pub mod const_project;
pub mod const_fn_fact;
pub mod const_qpath;
pub mod const_select;
mod const_drop;
pub mod consts;
pub(crate) mod coroutine_needs;
pub mod coroutine_recipes;
pub(crate) mod coroutines;
pub mod dyn_ty;
pub mod exact_interior;
pub mod fill;
pub mod fn_attrs;
pub(crate) mod fmt_template;
pub mod gat;
mod gat_bounds;
pub mod glob_uses;
pub mod foreign_closures;
pub mod foreign_locals;
mod foreign_head;
pub mod llvm_intrinsic;
pub mod intrinsic_rule;
pub mod head_record;
pub mod head_text;
pub mod heads;
pub mod rows;
pub mod index;
pub mod inline_for;
pub mod inspect;
pub mod keys;
pub mod linked;
pub mod langs;
pub mod lifetimes;
pub(crate) mod links;
pub mod locals;
mod member_names;
pub mod lower_made;
pub mod on_unimplemented;
pub mod opaque_auto;
pub mod own_trait;
pub mod alloc_shim;
pub mod internal_symbol;
/// The soft-float routines (`__addtf3` ..) bound from the program's compiler_builtins
/// (INTRINSICS.md 3.5, P5b).
pub mod builtins_shim;
pub mod panic_entry;
pub mod pattern_ty;
pub mod nested_exports;
pub mod preds;
pub mod heads_regions;
pub mod program;
pub mod project;
pub mod project_records;
pub mod reach;
pub mod reads;
pub mod receiver;
pub mod rpitit;
pub mod scoped;
// D189 point 4: the surface pass over the finished module names.
pub mod surface_names;
mod self_size;
pub mod shared_keys;
pub mod source;
pub mod spell;
pub mod stalled_goals;
pub mod start;
pub mod statics;
pub mod tables;
pub mod test_entry;
pub mod texts;
pub mod tait;
mod type_expand;
mod type_marks;
pub(crate) mod type_name;
pub mod types;
pub mod world_items;
/// The world's E0425 answer: the segment a body path stopped at.
mod world_path_miss;
/// The note an E0425 carries when the path's written `std` is bstd.
mod std_is_bstd;
pub mod world_paths;
/// An E0425 finding's words.
mod path_miss_shown;
pub use path_miss_shown::MACRO_REFUSED;
/// A written type that names nothing (E0412).
pub mod type_miss;
mod adt_paths;
mod static_shown;
mod written;
/// A token keeps its mark, in an item's head and types (token-mark-F1).
pub mod item_site;
pub mod header_written;
pub mod item_shown;
/// What a body's verdict says, as `check` prints it: one home for both paths.
pub mod said;
pub mod row_path;
/// WHY a census row is left out: the evidence under a build census's `SPLIT` rows.
pub mod census_why;

use blaze_scope::resolve::Names;

use crate::refusal::SourceFile;
use crate::resolve::Resolver;

/// Why the front end could not start.
#[derive(Debug)]
pub enum FrontError {
    Texts(texts::TextsError),
    Key(keys::KeyClash),
    Lang(index::LangTwice),
    /// The program's roots name no crate set (linked.rs).
    Roots(linked::RootsError),
}

impl core::fmt::Display for FrontError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FrontError::Texts(e) => write!(f, "{e}"),
            FrontError::Key(e) => write!(f, "{e}"),
            FrontError::Lang(e) => write!(f, "{e}"),
            FrontError::Roots(e) => write!(f, "{e}"),
        }
    }
}

/// The front end over one program's names, from the source `S` ([`source::NamesSource`]): the
/// names this command solved (`Front2<'n>`, made by [`Front2::new`]) or each crate's records
/// (`Front2<'n, &source::Records>`, made by [`Front2::from_records`]).
pub struct Front2<'n, S = &'n Names<'n>> {
    pub names: S,
    pub resolver: crate::resolve::Paths<S>,
    pub texts: texts::Texts<'n>,
    pub keys: keys::Keys,
    pub index: index::Index<'n>,
    /// The command's frame: what a body's lowering keeps for the command (its local items'
    /// rows and an expansion's text, [`locals`]); a [`heads::Store`] is made over it.
    pub frame: &'n blaze_mem::Frame,
}

impl<'n> Front2<'n> {
    /// The front end over `names`, the program's crate at index 0 and its files `user` as the
    /// caller gave them: [`Front2::for_program`] for the program crate 0 links. `frame` is the
    /// caller's, alive for the command; `alloc` holds the link closure, a bitmap for this call.
    pub fn new(names: &'n Names<'n>, frame: &'n blaze_mem::Frame, user: &[SourceFile<'_>], alloc: blaze_own::Alloc<'_>) -> Result<Front2<'n>, FrontError> {
        let linked = linked::Linked::of(names, &[0], alloc).map_err(FrontError::Roots)?;
        Self::for_program(names, frame, user, &linked)
    }

    /// The front end over `names` for ONE program, the crates `linked` links (linked.rs), and
    /// its files `user`. Its lang rows are the linked crates' alone (the per-program ones,
    /// `#[global_allocator]` and `#[panic_handler]`, are one per program, not per workspace);
    /// a library-only survey passes the library crates as its roots. Every item, variant and
    /// impl the library keys is filed in the key map here, so a clash stops the program before
    /// any item is lowered. `frame` is the caller's, alive for the command.
    pub fn for_program(names: &'n Names<'n>, frame: &'n blaze_mem::Frame, user: &[SourceFile<'_>], linked: &linked::Linked<'_>) -> Result<Front2<'n>, FrontError> {
        let texts = texts::Texts::new(names, user).map_err(FrontError::Texts)?;
        let index = index::Index::new(names, linked).map_err(FrontError::Lang)?;
        let keys = keys::Keys::new();
        for (krate, c) in names.crates().iter().enumerate() {
            for (key, _, _) in names.items(krate) {
                keys.item(key).map_err(FrontError::Key)?;
            }
            for vs in c.tables.enums.values() {
                for v in vs.iter() {
                    keys.item(v.key).map_err(FrontError::Key)?;
                }
            }
            for m in names.modules(krate) {
                keys.item(m.key).map_err(FrontError::Key)?;
            }
        }
        for i in index.impls.iter() {
            keys.item(i.key).map_err(FrontError::Key)?;
        }
        Ok(Front2 { names, resolver: Resolver { names }, texts, keys, index, frame })
    }
}

impl<'n> Front2<'n, &'n source::Records<'n>> {
    /// The front end over each crate's records, nothing solved in this command. Per crate it
    /// does NOTHING up front: no key is filed (a key is filed in `keys` when a head or a body
    /// first meets it, where a clash is then reported), no text is listed (`texts` starts
    /// empty), no impl is filed (`buckets::Buckets::lazy` files a crate's impls the first time
    /// a question's home admits that crate).
    ///
    /// The one thing read here: the lang rows of `lang_crates` (the crates that can declare a
    /// lang item: core, alloc, std and the program's own, as the driver's crate graph names
    /// them), from their items records, which this opens. A lang item declared in a crate not
    /// listed is not found; two crates declaring one name differently is `FrontError::Lang`.
    ///
    /// A crate with no record, a record that does not read and a question no record answers
    /// are in `records.errors()` (see [`source`]): read it after each body.
    pub fn from_records(records: &'n source::Records<'n>, frame: &'n blaze_mem::Frame, lang_crates: &[usize]) -> Result<Front2<'n, &'n source::Records<'n>>, FrontError> {
        let index = index::Index::from_source(records, lang_crates).map_err(FrontError::Lang)?;
        // The same crates' records (opened just above) say which may write incoherent impls:
        // one fact for the whole graph, so no inherent question opens every crate's record.
        records.name_lang_crates(lang_crates);
        Ok(Front2 { names: records, resolver: crate::resolve::Paths { names: records }, texts: texts::Texts::empty(), keys: keys::Keys::new(), index, frame })
    }
}
