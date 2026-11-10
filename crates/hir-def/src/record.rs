//! The canonical form of blaze's records (`#![blaze::records]`,
//! docs/v2/plan/design/records.md): which fields, in which order, name a record type.
//!
//! `{ a: A, b: B }` is core's `Record<"a,b", (A, B)>`: the field names sorted by their bytes
//! and joined by [`SEPARATOR`], and the field types in that order. Two records are one type
//! exactly when their sets of (name, type) pairs are equal, whatever order the fields were
//! written in; nothing else (a default least of all) is part of the type. hir-ty makes the
//! type from [`canonical_order`] and [`joined`], and reads a field's place back with
//! [`index_of`]. Sorting is by bytes, never by an interned id or a hash, so the form is the
//! same in every crate and every build (rule `deterministic-output`).

/// What separates the names in a record's name list. No identifier holds it, so a list splits
/// back into the names it was joined from.
pub const SEPARATOR: char = ',';

/// The places of a record's fields in canonical order: the indices of `names` sorted by the
/// names' bytes. `Err(at)` when the field at `at` has the name of an earlier one, which no
/// record has.
pub fn canonical_order(names: &[&str]) -> Result<Vec<usize>, usize> {
    // alloc: a record's field order, once per record type lowered or record value checked
    let mut order: Vec<usize> = (0..names.len()).collect();
    // A stable sort by name: two fields of one name stay in the order written, so the later
    // one is the duplicate.
    order.sort_by(|&a, &b| names[a].as_bytes().cmp(names[b].as_bytes()));
    match order.windows(2).filter(|pair| names[pair[0]] == names[pair[1]]).map(|pair| pair[1]).min()
    {
        Some(duplicate) => Err(duplicate),
        None => Ok(order),
    }
}

/// The name list of a record whose field names, in canonical order, are `names`.
pub fn joined<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    let mut list = String::new(); // alloc: a record's name list, once per record type made
    for name in names {
        debug_assert!(!name.is_empty() && !name.contains(SEPARATOR), "{name:?} is no field name");
        if !list.is_empty() {
            list.push(SEPARATOR);
        }
        list.push_str(name);
    }
    list
}

/// The field names of the name list `list`, in canonical order. The empty list is the empty
/// record's.
pub fn names(list: &str) -> impl Iterator<Item = &str> + Clone {
    list.split(SEPARATOR).filter(|name| !name.is_empty())
}

/// The place of the field `name` in the record whose name list is `list`: its index in the
/// tuple of the record's field types.
pub fn index_of(list: &str, name: &str) -> Option<usize> {
    names(list).position(|it| it == name)
}

#[cfg(test)]
mod tests {
    use hegel::TestCase;
    use hegel::generators as gs;

    use super::{canonical_order, index_of, joined, names};

    /// A field as written: its name, its type and whether it has a default (a stand-in for
    /// which default).
    type Field = (&'static str, u8, bool);

    const NAMES: [&str; 6] = ["a", "b", "ab", "a_b", "type", "z9"];

    /// A way to get the canonical form wrong.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Plant {
        None,
        /// The fields are keyed in the order written: `{ a, b }` and `{ b, a }` differ.
        WrittenOrder,
        /// A field's default is part of the key: one record type per set of defaults.
        DefaultInIdentity,
    }

    /// The identity of the record type with `fields`: its name list and its field types in
    /// canonical order, as hir-ty builds `Record<"..", (..)>`. `None` for a repeated name.
    fn key(fields: &[Field], plant: Plant) -> Option<(String, Vec<(u8, bool)>)> {
        let written: Vec<&str> = fields.iter().map(|field| field.0).collect();
        let order = match plant {
            Plant::WrittenOrder => {
                canonical_order(&written).ok()?;
                (0..fields.len()).collect()
            }
            _ => canonical_order(&written).ok()?,
        };
        let list = joined(order.iter().map(|&at| fields[at].0));
        let in_key = |at: &usize| {
            let (_, ty, default) = fields[*at];
            (ty, plant == Plant::DefaultInIdentity && default)
        };
        Some((list, order.iter().map(in_key).collect()))
    }

    /// The oracle: the set of (name, type) pairs, which is what a record type is.
    fn pairs(fields: &[Field]) -> Option<Vec<(&'static str, u8)>> {
        let mut pairs: Vec<_> = fields.iter().map(|&(name, ty, _)| (name, ty)).collect();
        pairs.sort();
        let repeated = pairs.windows(2).any(|pair| pair[0].0 == pair[1].0);
        (!repeated).then_some(pairs)
    }

    /// What the property and its controls hold a pair of field lists to: the two record types
    /// are equal exactly when their (name, type) sets are, a repeated name has no type, and
    /// each field is found at its place.
    fn check(a: &[Field], b: &[Field], plant: Plant) -> Result<(), String> {
        let (key_a, key_b) = (key(a, plant), key(b, plant));
        let (set_a, set_b) = (pairs(a), pairs(b));
        if key_a.is_some() != set_a.is_some() || key_b.is_some() != set_b.is_some() {
            return Err(format!("{a:?} or {b:?}: a type for a repeated name, or none without one"));
        }
        let (Some(key_a), Some(key_b)) = (key_a, key_b) else { return Ok(()) };
        if (key_a == key_b) != (set_a == set_b) {
            return Err(format!(
                "{a:?} and {b:?}: keys {key_a:?} and {key_b:?}, sets {set_a:?} and {set_b:?}"
            ));
        }
        for (at, name) in names(&key_a.0).enumerate() {
            if index_of(&key_a.0, name) != Some(at) {
                return Err(format!("{name} of {a:?} is not at {at} in {:?}", key_a.0));
            }
        }
        match names(&key_a.0).count() == a.len() {
            true => Ok(()),
            false => Err(format!("{:?} does not split into the {} names of {a:?}", key_a.0, a.len())),
        }
    }

    fn drawn(tc: &TestCase) -> Vec<Field> {
        let count: usize = tc.draw(gs::integers().max_value(5));
        (0..count)
            .map(|_| {
                let name: usize = tc.draw(gs::integers().max_value(NAMES.len() - 1));
                let ty: u8 = tc.draw(gs::integers().max_value(2));
                let default: bool = tc.draw(gs::booleans());
                (NAMES[name], ty, default)
            })
            .collect()
    }

    /// The invariant: two record types are equal exactly when their (name, type) sets are.
    #[hegel::test]
    fn records_are_equal_exactly_when_their_field_sets_are(tc: TestCase) {
        let a = drawn(&tc);
        // Half the draws are `a` again in another order with other defaults, so that equal
        // sets are as common as different ones.
        let b = match tc.draw(gs::booleans()) {
            false => drawn(&tc),
            true => {
                let turn: usize = tc.draw(gs::integers().max_value(5));
                let mut b: Vec<Field> = a.iter().map(|&(name, ty, d)| (name, ty, !d)).collect();
                if !b.is_empty() {
                    let by = turn % b.len();
                    b.rotate_left(by);
                }
                if tc.draw(gs::booleans()) {
                    b.reverse();
                }
                b
            }
        };
        if let Err(what) = check(&a, &b, Plant::None) {
            panic!("{what}");
        }
    }

    /// Every pair of field lists of up to three fields over two names, two types and both
    /// default states: small enough to walk whole, with every order and every default mix.
    fn small_pairs() -> Vec<(Vec<Field>, Vec<Field>)> {
        let fields: Vec<Field> = ["a", "b"]
            .into_iter()
            .flat_map(|name| [0, 1].into_iter().map(move |ty| (name, ty)))
            .flat_map(|(name, ty)| [false, true].into_iter().map(move |d| (name, ty, d)))
            .collect();
        let mut lists: Vec<Vec<Field>> = vec![Vec::new()];
        let mut last = lists.clone();
        for _ in 0..3 {
            let grown: Vec<Vec<Field>> = last
                .iter()
                .flat_map(|list| {
                    fields.iter().map(move |&field| {
                        let mut longer = list.clone();
                        longer.push(field);
                        longer
                    })
                })
                .collect();
            lists.extend(grown.iter().cloned());
            last = grown;
        }
        lists
            .iter()
            .flat_map(|a| lists.iter().map(move |b| (a.clone(), b.clone())))
            .collect()
    }

    fn control(plant: Plant) {
        let caught = small_pairs().into_iter().find_map(|(a, b)| check(&a, &b, plant).err());
        match caught {
            Some(what) => eprintln!("Plant::{plant:?} caught: {what}"),
            None => panic!("the oracle caught Plant::{plant:?} on no small pair: it is too weak"),
        }
    }

    #[test]
    fn every_small_pair_holds() {
        for (a, b) in small_pairs() {
            if let Err(what) = check(&a, &b, Plant::None) {
                panic!("{what}");
            }
        }
    }

    #[test]
    fn the_oracle_catches_field_order_in_the_key() {
        control(Plant::WrittenOrder);
    }

    #[test]
    fn the_oracle_catches_a_default_in_the_type_identity() {
        control(Plant::DefaultInIdentity);
    }

    #[test]
    fn the_order_is_by_bytes_and_a_repeated_name_is_refused_at_the_later_field() {
        assert_eq!(canonical_order(&["b", "a_b", "ab", "a", "Z"]), Ok(vec![4, 3, 1, 2, 0]));
        assert_eq!(canonical_order(&[]), Ok(vec![]));
        assert_eq!(canonical_order(&["x", "y", "x", "y"]), Err(2));
        assert_eq!(joined(["a", "b"]), "a,b");
        assert_eq!(joined([]), "");
        assert_eq!(names("").count(), 0);
        assert_eq!(index_of("a,ab,b", "ab"), Some(1));
        assert_eq!(index_of("a,ab,b", "c"), None);
    }
}
