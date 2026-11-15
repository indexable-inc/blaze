//! Builtin attributes resolved by nameres, by symbol.
//!
//! The table (names and templates, rustc's `builtin_attrs.rs`) is `syntax::builtin_attrs`: its
//! one home. Here is the old front end's lookup of an interned name in it: each row's declared
//! symbol, built from the table's own rows ([`syntax::builtin_attributes!`]) in its order, so an
//! index found here is an index of `syntax::builtin_attrs::INERT_ATTRIBUTES`.

use intern::{KnownNames, Symbol, sym::Declared};
use syntax::builtin_attrs::INERT_ATTRIBUTES;

/// The index in [`INERT_ATTRIBUTES`] of the attribute `name`: its declared index and one load,
/// no hash.
#[inline]
pub fn find_builtin_attr_idx(name: &Symbol) -> Option<usize> {
    static BY_SYMBOL: KnownNames = KnownNames::new(&INERT_ATTRIBUTE_SYMBOLS);
    BY_SYMBOL.get(name).map(usize::from)
}

/// The declared symbol of each row of the table: the row's name, after `TEST,` if it has one.
macro_rules! declared {
    (@name TEST, $attr:ident $($rest:tt)*) => { Declared::$attr };
    (@name $attr:ident $($rest:tt)*) => { Declared::$attr };
    ($( $kind:ident ! ( $($args:tt)* ) ),* $(,)?) => {
        [ $( declared!(@name $($args)*) ),* ]
    };
}

/// Each [`INERT_ATTRIBUTES`] entry's declared symbol, at its index. A row whose name is not a
/// declared symbol (`intern::sym`) fails the build here, and a row count other than the
/// table's fails the array's type.
const INERT_ATTRIBUTE_SYMBOLS: [Declared; INERT_ATTRIBUTES.len()] =
    syntax::builtin_attributes!(declared);

#[cfg(test)]
mod tests {
    use intern::{Symbol, sym};

    use super::{INERT_ATTRIBUTE_SYMBOLS, INERT_ATTRIBUTES, find_builtin_attr_idx};

    /// The lookup as it was: the last attribute of a name, by its interned text.
    fn by_text(name: &Symbol) -> Option<usize> {
        INERT_ATTRIBUTES.iter().rposition(|it| Symbol::intern(it.name) == *name)
    }

    /// The invariant: the lookup by symbol and the table's lookup by text
    /// (`syntax::builtin_attrs::find`) give the same row for every name of the table.
    #[test]
    fn every_builtin_attribute_is_found_at_its_index() {
        for (at, attr) in INERT_ATTRIBUTES.iter().enumerate() {
            let name = Symbol::intern(attr.name);
            let declared = INERT_ATTRIBUTE_SYMBOLS[at];
            assert_eq!(name.declared_index(), Some(declared as u16), "{}", attr.name);
            assert_eq!(find_builtin_attr_idx(&name), Some(at), "{}", attr.name);
            assert_eq!(find_builtin_attr_idx(&Symbol::intern(&attr.name.to_owned())), Some(at));
            assert_eq!(syntax::builtin_attrs::find(attr.name), Some(at), "{}", attr.name);
        }
    }

    #[test]
    fn other_names_are_found_as_by_text() {
        let mut names: Vec<Symbol> = Vec::new();
        for attr in INERT_ATTRIBUTES {
            let text = attr.name;
            for near in [
                format!("{text}_"),
                format!("r#{text}"),
                text.to_uppercase(),
                text[..text.len() - 1].to_owned(),
                text[1..].to_owned(),
            ] {
                names.push(Symbol::intern(&near));
            }
        }
        names.extend(
            ["", "derive", "macro_rules", "rustc_", "not_an_attribute"].map(Symbol::intern),
        );
        names.extend([sym::self_, sym::fn_, sym::sized, sym::Integer::get(7)]);
        for name in &names {
            assert_eq!(find_builtin_attr_idx(name), by_text(name), "{name:?}");
            assert_eq!(find_builtin_attr_idx(name), syntax::builtin_attrs::find(name.as_str()));
        }
        assert_eq!(find_builtin_attr_idx(&sym::cfg), by_text(&sym::cfg));
        assert!(find_builtin_attr_idx(&sym::cfg).is_some());
        assert_eq!(find_builtin_attr_idx(&Symbol::intern("not_an_attribute")), None);
    }
}
