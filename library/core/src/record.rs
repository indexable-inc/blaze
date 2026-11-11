//! blaze's records: the type of `{ a: A, b: B }` in a crate with `#![blaze::records]`, an
//! extension of blaze's, not the Rust project's (`library/UPSTREAM` lists this module among
//! blaze's changes to the standard library; the design is docs/v2/plan/design/records.md).
//!
//! hir-ty lowers `{ a: A, b: B }` to [`Record`] of the field names, sorted by their bytes and
//! joined by `,`, over the tuple of the field types in that order: `Record<"a,b", (A, B)>`.
//! So `{ a: A, b: B }` and `{ b: B, a: A }` are one type in every crate, with the tuple's
//! layout, and a record has a trait when the tuple of its fields has it. A record value, a
//! field read (`r.a`) and a record pattern are the compiler's; nothing here is called for
//! them.
//!
//! Every item is unstable under `blaze_records`; nothing else in `core` refers to it, so a
//! crate without records sees `core` as the nightly's.

use crate::fmt;

/// The record with the fields named in `FIELDS` (names sorted by their bytes, joined by `,`)
/// whose values are `T`, the tuple of the field types in that order: what `{ a: A, b: B }`
/// names, `Record<"a,b", (A, B)>`.
///
/// It compares, orders and hashes as the tuple does, so in the order of the sorted names.
#[lang = "record"]
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[unstable(feature = "blaze_records", issue = "none")]
pub struct Record<const FIELDS: &'static str, T>(
    /// The fields' values, in the order of the sorted names.
    #[unstable(feature = "blaze_records", issue = "none")]
    pub T,
);

/// The tuple of a record's fields, each of which can be printed: what [`Record`]'s `Debug`
/// walks. Implemented for tuples of up to twelve types, as the tuples' own traits are.
#[unstable(feature = "blaze_records", issue = "none")]
pub trait Fields {
    /// Calls `visit` with each field's value, in the tuple's order.
    #[unstable(feature = "blaze_records", issue = "none")]
    fn each(&self, visit: &mut dyn FnMut(&dyn fmt::Debug));
}

macro_rules! fields {
    ($(($($T:ident $at:tt),*))*) => {$(
        #[unstable(feature = "blaze_records", issue = "none")]
        impl<$($T: fmt::Debug),*> Fields for ($($T,)*) {
            #[inline]
            fn each(&self, visit: &mut dyn FnMut(&dyn fmt::Debug)) {
                // The empty tuple visits nothing.
                let _ = &visit;
                $(visit(&self.$at);)*
            }
        }
    )*};
}

fields! {
    ()
    (A 0)
    (A 0, B 1)
    (A 0, B 1, C 2)
    (A 0, B 1, C 2, D 3)
    (A 0, B 1, C 2, D 3, E 4)
    (A 0, B 1, C 2, D 3, E 4, F 5)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10)
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11)
}

/// A field's name, printed bare.
struct Name<'a>(&'a str);

impl fmt::Debug for Name<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// `{a: 1, b: 2}`: the fields by name, in the order of the sorted names, as a map prints.
#[unstable(feature = "blaze_records", issue = "none")]
impl<const FIELDS: &'static str, T: Fields> fmt::Debug for Record<FIELDS, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names = FIELDS.split(',');
        let mut map = f.debug_map();
        self.0.each(&mut |value| {
            // A name per field: the compiler makes the list and the tuple together.
            let name = names.next().unwrap_or("?");
            map.entry(&Name(name), value);
        });
        map.finish()
    }
}
