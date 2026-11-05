//! The key a function is looked up by in a profile: its path as rustc's demangler writes it
//! (`rustc_demangle`, `{:#}`, which leaves out crate disambiguators and legacy hashes), with
//! what tells one instance from another taken out, so every instance of a generic function,
//! and every closure and shim inside a function, folds onto the function written in source.
//!
//! A key is one of:
//! - `krate::module::f`: a free function, an inherent method (`krate::module::Type::f`, the
//!   path of the impl's self type and the method's name; v0 writes `<krate::module::Type>::f`)
//!   or a trait's provided method as legacy symbols name it (`krate::module::Trait::f`);
//! - `<Self as krate::module::Trait>::f`: a method of a trait impl, `Self` the path of the self
//!   type with its references, pointers and `dyn` taken off.
//!
//! Generic arguments (`::<..>`, `<..>` after a name) are dropped wherever they are; a path is
//! cut at its first segment in braces (`{closure#0}`, `{shim:vtable#0}`, `{{closure}}`), which
//! is inside the function before it. The source side ([`of_function`]) writes the same keys
//! from the front end's definitions; a key two functions of a crate write is matched to
//! neither ([`super::Hits`]).

use base_db::Db;
use hir::AsAssocItem as _;

/// The key of a PGO name: a mangled symbol (v0 `_R..`, legacy `_ZN..`, either with the extra
/// `_` Mach-O adds), optionally after the `file;` (LLVM 12 and later) or `file:` (before)
/// that LLVM puts before a function of internal linkage. `None` for a name that does not
/// demangle: a C function, or one of blaze's own `blaze.pgo.` names
/// ([`blaze_items::pgo::parse_name`]).
pub(crate) fn of_profile_name(name: &[u8]) -> Option<String> {
    let name = std::str::from_utf8(name).ok()?;
    let symbol = unprefixed(name);
    let demangled = rustc_demangle::try_demangle(symbol).ok()?;
    // alloc: once per name of the profile, as it is read
    let text = format!("{demangled:#}");
    normalized(&text)
}

/// Whether the PGO name `name` is a symbol rustc mangled ([`of_profile_name`]'s names), told
/// without writing its demangled path: what sorting a profile's records by who made them
/// asks of each, so a profile of many functions is not demangled in full only to be counted.
pub(crate) fn is_mangled(name: &[u8]) -> bool {
    std::str::from_utf8(name)
        .is_ok_and(|name| rustc_demangle::try_demangle(unprefixed(name)).is_ok())
}

/// `name` without the source file LLVM puts before a function of internal linkage.
fn unprefixed(name: &str) -> &str {
    let mangled =
        |it: &str| ["_R", "__R", "_ZN", "__ZN"].iter().any(|prefix| it.starts_with(prefix));
    if mangled(name) {
        return name;
    }
    if let Some((_, rest)) = name.rsplit_once(';')
        && mangled(rest)
    {
        return rest;
    }
    // Before LLVM 12 the separator was `:`, which a path never has alone before a mangled
    // name: the first `:` with a mangled name after it.
    for (at, _) in name.match_indices(':') {
        let rest = &name[at + 1..];
        if mangled(rest) {
            return rest;
        }
    }
    name
}

/// The key of a demangled path ([`of_profile_name`]); `None` for one this module can't read.
pub(crate) fn normalized(text: &str) -> Option<String> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix('<') {
        let close = matching(rest)?;
        let inner = &rest[..close];
        let after = rest[close + 1..].strip_prefix("::")?;
        let item = item_of(after)?;
        return match split_as(inner) {
            Some((self_ty, trait_path)) => {
                let self_ty = type_key(self_ty)?;
                let trait_path = plain(trait_path)?;
                Some(format!("<{self_ty} as {trait_path}>::{item}"))
            }
            None => {
                let self_ty = type_key(inner)?;
                Some(format!("{self_ty}::{item}"))
            }
        };
    }
    let path = plain(text)?;
    let path = cut_at_braces(&path);
    (!path.is_empty() && !path.ends_with("::")).then(|| path.to_owned())
}

/// What follows a qualified path's `>::`: the method, without its arguments or the closures
/// and shims inside it.
fn item_of(after: &str) -> Option<String> {
    let path = plain(after)?;
    let item = cut_at_braces(&path);
    let item = item.split("::").next()?;
    (!item.is_empty()).then(|| item.to_owned())
}

/// `path` up to its first segment in braces, which is inside the function before it.
fn cut_at_braces(path: &str) -> &str {
    let mut at = 0;
    for segment in path.split("::") {
        if segment.starts_with('{') {
            return path[..at].trim_end_matches("::");
        }
        at += segment.len() + 2;
    }
    path
}

/// The byte after `text`'s opening `<` (already taken) where its `>` closes it, counting
/// nested ones; a `->` is no bracket.
fn matching(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    for (at, &byte) in bytes.iter().enumerate() {
        match byte {
            b'<' => depth += 1,
            b'>' if at > 0 && bytes[at - 1] == b'-' => {}
            b'>' if depth == 0 => return Some(at),
            b'>' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// `Self as Trait` split at its ` as ` outside every bracket.
fn split_as(inner: &str) -> Option<(&str, &str)> {
    let bytes = inner.as_bytes();
    let mut depth = 0i32;
    for (at, &byte) in bytes.iter().enumerate() {
        match byte {
            b'<' | b'(' | b'[' => depth += 1,
            b'>' if at > 0 && bytes[at - 1] == b'-' => {}
            b'>' | b')' | b']' => depth -= 1,
            b' ' if depth == 0 && inner[at..].starts_with(" as ") => {
                return Some((&inner[..at], &inner[at + 4..]));
            }
            _ => {}
        }
    }
    None
}

/// A self type's key: its path without generic arguments, references, pointers or `dyn`.
fn type_key(ty: &str) -> Option<String> {
    let mut ty = ty.trim();
    loop {
        let before = ty;
        for prefix in ["&mut ", "&", "*const ", "*mut ", "dyn ", "mut "] {
            if let Some(rest) = ty.strip_prefix(prefix) {
                ty = rest.trim_start();
            }
        }
        // A lifetime after `&`: `&'a T`.
        if let Some(rest) = ty.strip_prefix('\'') {
            ty = rest.split_once(' ').map_or("", |it| it.1);
        }
        if ty == before {
            break;
        }
    }
    let ty = plain(ty)?;
    // A slice, array, tuple, function pointer or `impl` type has no path to match.
    let named = !ty.is_empty()
        && ty.bytes().all(|it| it.is_ascii_alphanumeric() || it == b'_' || it == b':');
    named.then_some(ty)
}

/// `path` with every generic argument list taken out: `a::B<C>::f::<D>` is `a::B::f`.
fn plain(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    // alloc: once per name of the profile, as it is read
    let mut out = String::with_capacity(path.len());
    let mut depth = 0usize;
    for (at, &byte) in bytes.iter().enumerate() {
        match byte {
            b'<' => {
                if depth == 0 && out.ends_with("::") {
                    out.truncate(out.len() - 2);
                }
                depth += 1;
            }
            b'>' if at > 0 && bytes[at - 1] == b'-' => {}
            b'>' => depth = depth.checked_sub(1)?,
            _ if depth == 0 => out.push(char::from(byte)),
            _ => {}
        }
    }
    (depth == 0 && path.is_ascii()).then(|| out.trim().to_owned())
}

/// The key of `f` as [`of_profile_name`] writes the names of its instances, or `None` for a
/// function this module can't name soundly: one in a block (its symbol's path has the
/// enclosing function, which the module path does not), one whose crate has no name, an
/// impl's method whose self type is no path (a slice, a tuple, a closure).
pub(crate) fn of_function(db: &Db, f: hir::Function) -> Option<String> {
    let krate = f.module(db).krate(db);
    // alloc: once per function of the crate linted with a profile
    let crate_name = krate.display_name(db)?.crate_name().to_string();
    let name = f.name(db);
    let name = name.as_str();
    match f.as_assoc_item(db).map(|it| it.container(db)) {
        None => Some(format!("{}::{name}", module_path(db, &crate_name, f.module(db))?)),
        Some(hir::AssocItemContainer::Trait(t)) => Some(format!(
            "{}::{}::{name}",
            module_path(db, &crate_name, t.module(db))?,
            t.name(db).as_str()
        )),
        Some(hir::AssocItemContainer::Impl(imp)) => {
            let self_ty = self_key(db, imp.self_ty(db))?;
            match imp.trait_(db) {
                None => Some(format!("{self_ty}::{name}")),
                Some(t) => {
                    let t_crate = t.module(db).krate(db).display_name(db)?.crate_name().to_string();
                    let trait_path = format!(
                        "{}::{}",
                        module_path(db, &t_crate, t.module(db))?,
                        t.name(db).as_str()
                    );
                    Some(format!("<{self_ty} as {trait_path}>::{name}"))
                }
            }
        }
    }
}

/// An impl's self type as [`type_key`] writes it, references taken off.
fn self_key(db: &Db, ty: hir::Type<'_>) -> Option<String> {
    let ty = ty.strip_references();
    if let Some(adt) = ty.as_adt() {
        let module = adt.module(db);
        let krate = module.krate(db).display_name(db)?.crate_name().to_string();
        return Some(format!("{}::{}", module_path(db, &krate, module)?, adt.name(db).as_str()));
    }
    if let Some(builtin) = ty.as_builtin() {
        return Some(builtin.name().as_str().to_owned());
    }
    None
}

/// `module`'s path from its crate's root, `krate` first; `None` for a module in a block.
fn module_path(db: &Db, krate: &str, module: hir::Module) -> Option<String> {
    let modules = module.path_to_root(db);
    // alloc: once per function of the crate linted with a profile
    let mut path = krate.to_owned();
    for module in modules.iter().rev().skip(1) {
        // A module with no name below the root is a block's.
        let name = module.name(db)?;
        path.push_str("::");
        path.push_str(name.as_str());
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instances_closures_and_hashes_fold_onto_the_function() {
        for (demangled, key) in [
            ("app::run", "app::run"),
            ("app::util::sum::<u32>", "app::util::sum"),
            ("app::util::sum::<alloc::vec::Vec<u8>>", "app::util::sum"),
            ("app::util::sum::{closure#0}", "app::util::sum"),
            ("app::util::sum::{{closure}}", "app::util::sum"),
            ("<app::Grid>::get", "app::Grid::get"),
            ("<app::Grid<f32>>::get::<u8>", "app::Grid::get"),
            ("app::Grid<T>::get", "app::Grid::get"),
            (
                "<app::Grid as core::ops::drop::Drop>::drop",
                "<app::Grid as core::ops::drop::Drop>::drop",
            ),
            (
                "<&mut app::Grid<u8> as app::Walk<fn() -> u8>>::step::{closure#1}",
                "<app::Grid as app::Walk>::step",
            ),
            ("<u32 as app::Walk>::step", "<u32 as app::Walk>::step"),
        ] {
            assert_eq!(normalized(demangled).as_deref(), Some(key), "{demangled}");
        }
        assert_eq!(normalized("<[u8] as app::Walk>::step"), None);
        assert_eq!(normalized("<(u8, u16)>::f"), None);
    }

    #[test]
    fn a_mangled_name_after_its_file_demangles() {
        // `_ZN3app3run17h0123456789abcdefE` is legacy `app::run`.
        let legacy = "_ZN3app3run17h0123456789abcdefE";
        assert_eq!(of_profile_name(legacy.as_bytes()).as_deref(), Some("app::run"));
        assert_eq!(
            of_profile_name(format!("src/main.rs;{legacy}").as_bytes()).as_deref(),
            Some("app::run")
        );
        assert_eq!(
            of_profile_name(format!("src/main.rs:{legacy}").as_bytes()).as_deref(),
            Some("app::run")
        );
        assert_eq!(of_profile_name(b"main"), None);
        assert_eq!(of_profile_name(b"blaze.pgo.00112233445566778899aabbccddeeff"), None);
    }
}
