//! Names as tasks (docs/v2/plan/design/bottom.md, section 6, "Names"): the functions a driver
//! runs as its task kinds, and the one in-memory scheduler that runs the same functions to a
//! fixpoint for the tests and the examples.
//!
//! - [`answer`]: what the records held answer one read of the machine, or the module it must
//!   wait on. Pure: the records come in through [`Records`].
//! - [`solve`]: one task of the `ModuleScope` kind (one module) or the `Group` kind (a wait
//!   cycle of modules). Entered from the top each time; it returns [`Outcome::Done`] with one
//!   [`crate::Resolved`] per member, or [`Outcome::Need`] naming the records it lacks
//!   ([`Want`]). How a group is solved is the solver's, behind the type parameter `S`.
//! - [`Fill`]: the in-memory scheduler. It holds the finished scopes of the modules it fills
//!   and nothing else that lasts. It runs every ready task, waits a task on what it named, and
//!   when nothing is ready it widens each sink component of the wait graph into one group
//!   task. The driver does not use it: it runs [`solve`] as its own task kinds over its store.
//!
//! The configuration is not read here: it is inside the records the tasks read (a module's
//! declarations are lowered under one cfg and keyed by it). When a record holds an answer for
//! every configuration at once (docs/v2/plan/design/conditions.md), [`Records`] answers with
//! the entry's condition and [`Own`] carries it: the signatures here do not change.

#![blaze::sans_io]

mod answer;
mod attr_value;
pub use attr_value::{AttrCall, AttrValueRefusal, attr_string, read_call};
mod exports;
mod fill;
mod lower;
mod prefetch;
mod solve;
#[cfg(test)]
mod tool_attr_spaced;

pub use answer::{Held, Lack, Note, Own, Records, Variant, answer, helpers_digest};
pub use exports::publish_expanded_exports;
pub use fill::{DecidePlant, ExpandPart, Fill, FillEnv, FillReport, First, NamesPart, Par, Scratch, ScratchHook, Serial, Stage};
pub use lower::{AttrOrder, Attrs, BodySrc, CallSrc, Cfg, Files, Fragment, ImplSrc, ItemText, KNOWN_FALSE, LangSrc, Loc, LowerCx, LowerNote, LowerNoteKind, Lowered, MOST_CFG_DEPTH, MOST_MODULES, MacroBody, MacroSrc, Member, ModMade, PROC_MACRO_DECLS, ProcAttr, ProcDecls, ProcExport, Module, prelude_attr, PreludeImport, RootInput, RootNote, RootPlan, SplitTop, Target, Tree, UseLeaf, WalkNote, WalkRow, Walked, Where, args, crate_root, crate_tools, file_attrs, first_line, head, inline_child, inside, is_builtin_attr, item_attrs, key_of, lower, make_key, parse_path, parse_path_marked, proc_macro_decls_key, root_key, rules_shapes, string_value, strip_attr, to_path, to_path_head, to_path_marked, unraw, use_leaves, walk, walk_rows};
// resolve.rs: a made module's paths joined as the walk joins them.
pub(crate) use lower::{joined, pop_dir};
pub use prefetch::{MOST_HOPS, Prefetch, prefetch};
pub use solve::{Expand, Expanded, MOST_STEPS, Outcome, TaskError, Tally, Want, solve};
