//! Whose each instance and static is for a dependency image (fastcompile-premap 3,
//! docs/v2/plan/lanes/fastcompile/premap.md): a dependency's code goes in the image linked
//! once per set of dependencies, the workspace's in the link of every edit.
//!
//! The front end decides an instance's owner where it sees its type arguments
//! ([`crate::backend::Described::owner`]): an instance of a dependency's generic made for a
//! workspace type is the workspace's. A static is its crate's. A dependency's code never refers
//! to the workspace's, or the image would change with workspace code: the build checks every
//! reference of the dependency's code and refuses one to the workspace's, naming both.

use blaze_graph::par;
use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemKey};
use serde::{Deserialize, Serialize};

use crate::{CodegenError, InstanceKey, Target, plan::Origin};

/// Whose code an instance or static is, for the dependency image.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Owner {
    /// Linked once into the image of the build's dependencies: of a dependency's or the
    /// standard library's item, and no type argument names a workspace crate's type, closure
    /// or item. Ordered first: of two describers of one instance, the image's wins, since an
    /// instance is its key's code wherever it is linked.
    Dependency,
    /// Linked with every edit.
    Workspace,
}

impl Owner {
    /// The owner of code of a crate of `origin` that no type argument makes another's: a
    /// dependency's and the standard library's are the image's, a member's and build-time
    /// code's the workspace's.
    pub fn of(origin: &Origin) -> Owner {
        match origin {
            Origin::Dependency(_) | Origin::Std => Owner::Dependency,
            Origin::Member(_) | Origin::BuildTime(_) => Owner::Workspace,
        }
    }
}

/// A function of the build as [`owners`] takes it.
#[derive(Clone, Copy)]
pub(super) struct Function<'a> {
    pub(super) key: InstanceKey,
    pub(super) item: ItemKey,
    pub(super) owner: Owner,
    /// The code hash of the generic it is an instance of.
    pub(super) code: CodeHash,
    pub(super) refs: &'a [Target],
    /// What its refusal names it by: rustc's demangled name of the instance.
    pub(super) name: &'a str,
    pub(super) origin: Origin,
}

/// A static of the build as [`owners`] takes it.
#[derive(Clone, Copy)]
pub(super) struct StaticOf<'a> {
    pub(super) id: (ItemKey, ContentId),
    /// What its errors name it by: its value, as the build's other errors of a static do.
    pub(super) unit: InstanceKey,
    pub(super) owner: Owner,
    pub(super) refs: &'a [Target],
    /// What its refusal names it by, with `origin`: its symbol, for one that has one.
    pub(super) symbol: Option<&'a str>,
    pub(super) origin: Origin,
}

/// What [`owners`] gives: [`super::Built::owners`] and [`super::Built::static_owners`].
pub(super) struct Owned {
    pub(super) functions: Vec<(InstanceKey, Owner, CodeHash)>,
    pub(super) statics: Vec<(ItemKey, ContentId, Owner)>,
}

/// The owners of `functions` and `statics`, each sorted by key without duplicates, and into
/// `errors` one error for each of the dependency's functions and statics that refers to the
/// workspace's, naming the first such reference, functions first and each in key order. The
/// references are checked on every core: a lookup each, about 1 M for 100 000 functions.
pub(super) fn owners<'a, F, S>(functions: F, statics: S, errors: &mut Vec<CodegenError>) -> Owned
where
    F: Iterator<Item = Function<'a>> + Clone,
    S: Iterator<Item = StaticOf<'a>> + Clone,
{
    let owned = Owned {
        // alloc: per build, what it gives the linker
        functions: functions.clone().map(|it| (it.key, it.owner, it.code)).collect(),
        // alloc: per build, what it gives the linker
        statics: statics.clone().map(|it| (it.id.0, it.id.1, it.owner)).collect(),
    };
    debug_assert!(owned.functions.is_sorted_by(|a, b| a.0 < b.0));
    debug_assert!(owned.statics.is_sorted_by(|a, b| (a.0, a.1) < (b.0, b.1)));
    let owner_of = |target: &Target| match target {
        Target::Function(key) | Target::Direct(key) => {
            let at = owned.functions.binary_search_by(|it| it.0.cmp(key)).ok()?;
            Some(owned.functions[at].1)
        }
        Target::Static { item, instance } => {
            let at = owned.statics.binary_search_by(|it| (it.0, it.1).cmp(&(*item, *instance))).ok()?;
            Some(owned.statics[at].2)
        }
        Target::Data(_)
        | Target::Extern(_)
        | Target::Weak(_)
        | Target::Position(_)
        | Target::Coverage(_)
        | Target::Counter(_)
        | Target::ProfileCounters => None,
    };
    // clone: the columns' iterators, a few pointers each; a refusal reads them again
    let (dependency_functions, dependency_statics) = (functions.clone(), statics.clone());
    let checked: Vec<Checked<'a>> = (dependency_functions.filter(|it| it.owner == Owner::Dependency))
        .map(|it| Checked { unit: it.key, item: it.item, refs: it.refs, named: Named::Function(it.name, it.origin) })
        .chain(dependency_statics.filter(|it| it.owner == Owner::Dependency).map(|it| Checked {
            unit: it.unit,
            item: it.id.0,
            refs: it.refs,
            named: Named::Static(it.symbol, it.origin),
        }))
        // alloc: per build, the dependency's code, a few words each
        .collect();
    // alloc: the refusals, none for a build that has none
    let found: Vec<(&Checked<'a>, &Target)> = par::filter_map(&checked, |it| {
        Some((it, it.refs.iter().find(|target| owner_of(*target) == Some(Owner::Workspace))?))
    });
    for (it, target) in found {
        let to = match target {
            Target::Function(key) | Target::Direct(key) => {
                // clone: an iterator over the build's columns, on an error
                functions.clone().find(|f| f.key == *key).map(|f| Named::Function(f.name, f.origin))
            }
            Target::Static { item, instance } => {
                // clone: an iterator over the build's columns, on an error
                statics.clone().find(|s| s.id == (*item, *instance)).map(|s| Named::Static(s.symbol, s.origin))
            }
            _ => None,
        };
        let to = match to {
            Some(to) => format!("{to}, {target:?}"), // alloc: an error
            None => format!("{target:?}"),           // alloc: an error
        };
        errors.push(CodegenError {
            unit: it.unit,
            item: it.item,
            // alloc: an error
            what: format!(
                "{} is a dependency's, so it goes in the dependency image, and it refers to \
                 {to}, which is the workspace's: the image would change with workspace code",
                it.named,
            ),
        });
    }
    owned
}

/// A dependency's function or static whose references [`owners`] checks.
struct Checked<'a> {
    unit: InstanceKey,
    item: ItemKey,
    refs: &'a [Target],
    named: Named<'a>,
}

/// What a refusal names a function or static by: its name and whose code it is.
#[derive(Clone, Copy)]
enum Named<'a> {
    Function(&'a str, Origin),
    Static(Option<&'a str>, Origin),
}

impl std::fmt::Display for Named<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let origin = match self {
            Named::Function(name, origin) => {
                write!(f, "`{name}`")?;
                origin
            }
            Named::Static(Some(symbol), origin) => {
                write!(f, "the static `{symbol}`")?;
                origin
            }
            Named::Static(None, origin) => {
                write!(f, "a static")?;
                origin
            }
        };
        match origin {
            Origin::Member(package) => write!(f, " (of member `{package}`)"),
            Origin::Dependency(package) => write!(f, " (of dependency `{package}`)"),
            Origin::BuildTime(package) => write!(f, " (of build-time `{package}`)"),
            Origin::Std => write!(f, " (of the standard library)"),
        }
    }
}

#[cfg(test)]
mod tests;
