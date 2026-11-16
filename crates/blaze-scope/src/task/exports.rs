//! The `#[macro_export]` `macro_rules!` that macro expansions wrote, made bindings of the crate
//! root once every module of the crate is done.
//!
//! The Reference, "Macros By Example", "Path-based scope": a `#[macro_export]` macro is placed
//! in the crate root, so `use tokio::select;` names one that tokio's own `doc! { .. }` wrapper
//! wrote. In its own crate a macro-expanded export is not reachable by path (rustc's
//! deny-by-default `macro_expanded_macro_exports_accessed_by_absolute_paths`), so the root's
//! scope gains them only after its own modules are resolved: other crates read them there, and
//! `#[macro_use] extern crate` reads the crate root's `macro_exports`.

use blaze_hash::HashMap;
use blaze_id::ItemKey;
use blaze_own::{AVec, Alloc};
use blaze_surface::{Binding, BindingState, CrateRootBuilder, Def, DefKind, MacroShapes, Namespace, Origin, ScopeEntry, Visibility};

use crate::Resolved;

/// Adds every done module's expansion-written exports, and `in_made`, the exports written in
/// the text of a module whose `mod` item an expansion wrote (commonware-runtime's
/// `spawn_cell!` in `utils/cell.rs`, under `stability_scope! { pub mod utils; }`), to `root`'s
/// scope and to `crate_root`'s `macro_exports`. Returns the names exported twice (by two
/// expansions, or by an expansion and the crate's own text): the first by key is kept for
/// each, and the caller notes the rest.
pub fn publish_expanded_exports<'a>(
    done: &mut HashMap<ItemKey, Resolved<'a, &'a str>, Alloc<'a>>,
    in_made: &[(&'a str, ItemKey, MacroShapes)],
    root: ItemKey,
    crate_root: &mut CrateRootBuilder<'a, &'a str>,
    alloc: Alloc<'a>,
) -> AVec<'a, &'a str> {
    let mut written: AVec<'a, (&'a str, ItemKey, MacroShapes, u16)> = AVec::new_in(alloc);
    for r in done.values() {
        written.extend(r.scope.macro_exports.iter().map(|d| (d.name, d.def, d.shapes, d.expansion)));
    }
    // A made module's own text is inside the expansion that wrote its `mod` item: expansion 1,
    // macro-expanded as far as shadowing goes.
    written.extend(in_made.iter().map(|&(name, def, shapes)| (name, def, shapes, 1)));
    let mut twice: AVec<'a, &'a str> = AVec::new_in(alloc);
    if written.is_empty() {
        return twice;
    }
    // Deterministic whatever the map's order: by name, then key.
    written.sort_unstable_by(|x, y| (x.0, x.1).cmp(&(y.0, y.1)));
    let Some(scope) = done.get_mut(&root).map(|r| &mut r.scope) else { return twice };
    let mut last: Option<&'a str> = None;
    for &(name, def, shapes, expansion) in written.iter() {
        let again = last == Some(name) || scope.get(Namespace::Macros, name).is_some();
        last = Some(name);
        if again {
            twice.push(name);
            continue;
        }
        let binding = Binding {
            def: Def { key: def, kind: DefKind::Macro(shapes) },
            vis: Visibility::Public,
            origin: Origin::MacroExport,
            expansion,
            state: BindingState::Determined,
        };
        let at = scope.bindings.partition_point(|e| (e.ns, e.name) < (Namespace::Macros, name));
        scope.bindings.insert(at, ScopeEntry { ns: Namespace::Macros, name, binding });
        let at = crate_root.macro_exports.partition_point(|(n, _, _)| *n < name);
        crate_root.macro_exports.insert(at, (name, def, shapes));
    }
    twice
}
