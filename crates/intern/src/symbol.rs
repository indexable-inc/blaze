//! Interned strings: symbols declared at compile time ([`symbols`]) and ones interned at
//! runtime, as `u32` IDs that mean the same in every process that restores one image.
//!
//! A [`Symbol`] is an ID in the symbol table (`text`): `1..=STATIC` are the compiled-in texts of
//! [`symbols::TEXTS`] (the declared symbols, then each list's), every other one a text interned
//! at runtime or restored from an image ([`restore_symbols`]), read in place. The table holds
//! the compiled-in texts first, so interning a declared symbol's text gives the declared
//! symbol. A symbol is never freed while the process runs; the image's GC ([`write_symbols`])
//! keeps only the ones the other sections still name, each at its own ID.

use std::{cmp::Ordering, fmt, num::NonZeroU32, sync::Arc, sync::OnceLock};

use blaze_format::SchemaHash;
use blaze_ids::{ContentId, Hasher};
use blaze_image::{Builder, Image, SectionId, Stable};

use crate::table::{Jobs, Live, RestoreError};
#[cfg(test)]
use crate::table::{Table, serial};

pub mod symbols;

pub use blaze_ids::names::order_key;

/// A compiled-in symbol's text and its order key, read by [`Symbol::cmp_order`] without the
/// table.
pub(crate) struct Text {
    key: u64,
    str: &'static str,
}

impl Text {
    pub(crate) const fn new(str: &'static str) -> Text {
        Text { key: order_key(str.as_bytes()), str }
    }
}

/// # Invariant
///
/// `id` names a text of the symbol table: compiled in (`id <= STATIC`), or made or restored
/// by this process, which never frees it. Two symbols are one string exactly when they are
/// one ID.
#[derive(PartialEq, Eq, Hash, Clone, Copy)]
pub struct Symbol {
    id: NonZeroU32,
}

impl fmt::Debug for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(f)
    }
}

const _: () = assert!(size_of::<Symbol>() == size_of::<u32>());
const _: () = assert!(size_of::<Option<Symbol>>() == size_of::<u32>());

/// The process's symbol table, every value of which is text. Only this module reaches it, so
/// every value in it is a compiled-in `&str`, a copy of a `&str` interned, or bytes of an image
/// that `Table::restore` checked to be UTF-8 (`shape().text`): reading one as a `str` needs
/// no check. That is what its one unsafe operation buys: [`Symbol::as_str`] costs a range check
/// and two loads, where checking the UTF-8 again would scan every byte of the name each time.
#[allow(unsafe_code)]
mod text {
    use std::sync::OnceLock;

    use blaze_image::{Image, SectionId, SectionWriter, Stable};

    use super::{order_key, symbols, symbols_schema};
    use crate::table::{Jobs, Live, RestoreError, Shape, Table};

    static TABLE: OnceLock<Table> = OnceLock::new();

    /// The symbol table's shape: the compiled-in texts first, every value text.
    pub(super) fn shape() -> Shape {
        Shape {
            statics: symbols::STATIC as u32,
            static_bytes: |id| symbols::TEXTS[id as usize - 1].str.as_bytes(),
            key: order_key,
            align: 1,
            size: 1,
            text: true,
        }
    }

    #[inline]
    fn table() -> &'static Table {
        TABLE.get_or_init(|| Table::new(shape()))
    }

    pub(super) fn intern_hashed(s: &str, hash: u64) -> u32 {
        table().intern_hashed(hash, s.as_bytes())
    }

    pub(super) fn find_hashed(s: &str, hash: u64) -> Option<u32> {
        table().find_hashed(hash, s.as_bytes())
    }

    /// Whether `id` names a symbol of this process.
    pub(super) fn names(id: u32) -> bool {
        table().get(id).is_some()
    }

    pub(super) fn key(id: u32) -> u64 {
        table().key(id)
    }

    /// The text of `id`, which a held symbol names.
    #[inline]
    pub(super) fn str(id: u32) -> &'static str {
        let bytes = table().bytes(id);
        // SAFETY: every value of `TABLE` is text (the module's doc): its compiled-in values are
        // `&str`s, `intern` copies a `&str`, and a restore checks every live value's UTF-8
        // (`Table::validate`, as `shape().text` asks) before any is read. A value that was not
        // would make a `&str` that is not UTF-8.
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }

    /// One past the highest ID made so far.
    #[cfg(test)]
    pub(super) fn end() -> u32 {
        table().end()
    }

    pub(super) fn write(live: &Live, out: &mut SectionWriter<'_>) {
        table().write(live, out);
    }

    pub(super) fn restore<S: Stable>(image: &'static Image<S>, jobs: Jobs<'_>) -> Result<(), RestoreError> {
        Table::restore_into(&TABLE, shape(), image, SectionId::SYMBOLS, symbols_schema(), jobs)
    }
}

#[cfg(test)]
use self::text::shape;

/// The schema of the symbol section: the table's layout and every compiled-in text, so an
/// image is restored only by a blaze whose `STATIC` IDs are the same texts.
pub fn symbols_schema() -> SchemaHash {
    static SCHEMA: OnceLock<SchemaHash> = OnceLock::new();
    *SCHEMA.get_or_init(|| {
        let mut hasher = Hasher::new("blaze intern symbols");
        hasher.update(crate::table::LAYOUT.as_bytes()).update(&crate::table::ROOT.0.0);
        for text in &symbols::TEXTS {
            hasher.update(&(text.str.len() as u64).to_le_bytes()).update(text.str.as_bytes());
        }
        SchemaHash(hasher.finish())
    })
}

/// Makes the symbols of `image`'s [`SectionId::SYMBOLS`] this process's, read in place: every
/// symbol a restored value names means the text it meant when the image was written. Must run
/// before any symbol is interned or read; the image stays mapped until the process exits.
///
/// On an error no symbol was restored, so no other section of `image` that names symbols may
/// be used: every one of them is recomputed from a cold start. The section is checked whole
/// here, since every value depends on it, its CRCs and columns split across `jobs` (the
/// daemon's pool: one core streams a tenth of what the machine's memory can).
pub fn restore_symbols<S: Stable>(image: Arc<Image<S>>, jobs: Jobs<'_>) -> Result<(), RestoreError> {
    // alloc: once a restore, so the image outlives every symbol read from it
    let image: &'static Arc<Image<S>> = Box::leak(Box::new(image));
    text::restore(image, jobs)?;
    RESTORED.set(image.hash()).expect("the symbol table is restored once");
    Ok(())
}

static RESTORED: OnceLock<ContentId> = OnceLock::new();

/// The hash of the image this process's symbols were restored from, `None` for a fresh table:
/// what an owner of other sections checks before using them, since their values name symbol
/// IDs that mean what they meant only in that image's table.
pub fn restored_from() -> Option<ContentId> {
    RESTORED.get().copied()
}

/// A symbol table restored into `cell`, as [`restore_symbols`] restores the process's.
#[cfg(test)]
fn restore_into<S: Stable>(
    cell: &OnceLock<Table>,
    image: &'static Image<S>,
    jobs: Jobs<'_>,
) -> Result<(), RestoreError> {
    Table::restore_into(cell, shape(), image, SectionId::SYMBOLS, symbols_schema(), jobs)
}

/// Writes the symbol section into `builder`: every symbol `live` marks at its ID, every other
/// ID free for the next process. The symbols interned while this runs are not in it.
pub fn write_symbols(live: &Live, builder: &mut Builder) {
    let mut out = builder.section(SectionId::SYMBOLS, symbols_schema());
    text::write(live, &mut out);
}

impl Live {
    /// Keeps `symbol` in the next image.
    #[inline]
    pub fn mark(&mut self, symbol: Symbol) {
        self.mark_id(symbol.id());
    }
}

impl Symbol {
    /// Compiled-in symbol `index` of [`symbols::TEXTS`].
    pub(crate) const fn from_index(index: usize) -> Symbol {
        assert!(index < symbols::STATIC);
        match NonZeroU32::new(index as u32 + 1) {
            Some(id) => Symbol { id },
            None => unreachable!(),
        }
    }

    #[inline]
    pub fn intern(s: &str) -> Self {
        Symbol::intern_hashed(s, blaze_phf::name_hash(s.as_bytes()))
    }

    /// [`Symbol::intern`] of `s` whose [`blaze_phf::name_hash`] is `hash`, from a caller that
    /// hashed it while its bytes were in registers. A compiled-in text ([`symbols::TEXTS`]: the
    /// declared symbols, the common names among them, each list's) is its fixed symbol from
    /// [`symbols::COMPILED`], one lookup with no probe of the table; any other is one probe of
    /// the table under `hash`. Neither hashes `s` again.
    #[inline]
    pub fn intern_hashed(s: &str, hash: u64) -> Self {
        debug_assert_eq!(hash, blaze_phf::name_hash(s.as_bytes()), "{s:?}: a hash not its name_hash");
        match symbols::COMPILED.index_by_name_hash(hash, s.as_bytes()) {
            Some(index) => Symbol::from_index(index),
            None => Symbol::from_table(text::intern_hashed(s, hash)),
        }
    }

    /// The symbol of `s`, if one was interned or restored.
    #[inline]
    pub fn find(s: &str) -> Option<Symbol> {
        Symbol::find_hashed(s, blaze_phf::name_hash(s.as_bytes()))
    }

    /// [`Symbol::find`] of `s` whose [`blaze_phf::name_hash`] is `hash`, as
    /// [`Symbol::intern_hashed`].
    #[inline]
    pub fn find_hashed(s: &str, hash: u64) -> Option<Symbol> {
        debug_assert_eq!(hash, blaze_phf::name_hash(s.as_bytes()), "{s:?}: a hash not its name_hash");
        match symbols::COMPILED.index_by_name_hash(hash, s.as_bytes()) {
            Some(index) => Some(Symbol::from_index(index)),
            None => text::find_hashed(s, hash).map(Symbol::from_table),
        }
    }

    fn from_table(id: u32) -> Symbol {
        Symbol { id: NonZeroU32::new(id).expect("the table never gives ID 0") }
    }

    /// The symbol's ID: what a persisted value stores, the same in every process that restored
    /// the image it was written with.
    #[inline]
    pub fn id(self) -> u32 {
        self.id.get()
    }

    /// The symbol of a stored ID, `None` when it names no symbol in this process.
    #[inline]
    pub fn from_id(id: u32) -> Option<Symbol> {
        let id = NonZeroU32::new(id)?;
        let known = id.get() as usize <= symbols::STATIC || text::names(id.get());
        known.then_some(Symbol { id })
    }

    /// The symbol of an ID [`Symbol::id`] gave in this process, with no lookup: for columns of
    /// IDs that were symbols when written (`tt`'s token trees). An ID that names no symbol
    /// panics when its text is read, as a symbol whose value the image's GC freed does.
    #[inline]
    pub fn from_held_id(id: u32) -> Symbol {
        Symbol { id: NonZeroU32::new(id).expect("a symbol's ID is never 0") }
    }

    pub fn empty() -> Self {
        symbols::__empty
    }

    #[inline]
    pub fn as_str(&self) -> &'static str {
        if let Some(text) = self.text() {
            return text.str;
        }
        text::str(self.id())
    }

    /// The symbol's [`order_key`], stored beside it when it was interned.
    #[inline]
    pub fn order_key(&self) -> u64 {
        match self.text() {
            Some(text) => text.key,
            None => text::key(self.id()),
        }
    }

    /// The order of `self.as_str().cmp(other.as_str())`: the order keys first, the bytes only
    /// when they tie.
    #[inline]
    pub fn cmp_order(&self, other: &Symbol) -> Ordering {
        if self == other {
            return Ordering::Equal;
        }
        match self.order_key().cmp(&other.order_key()) {
            Ordering::Equal => self.as_str().as_bytes().cmp(other.as_str().as_bytes()),
            unequal => unequal,
        }
    }

    /// The symbol's index in [`symbols::Declared`] when it is a declared one, from one compare;
    /// `None` for one interned at run time or of a list such as [`symbols::Integer`].
    /// Interning a declared symbol's text gives the declared symbol, so a name read from source
    /// has its index too.
    #[inline]
    pub fn declared_index(&self) -> Option<u16> {
        let index = self.id() as usize - 1;
        (index < symbols::DECLARED).then_some(index as u16)
    }

    /// The symbol's index in [`symbols::TEXTS`] when it is compiled in.
    #[inline]
    pub(crate) fn static_index(&self) -> Option<usize> {
        let index = self.id() as usize - 1;
        (index < symbols::STATIC).then_some(index)
    }

    #[inline]
    fn text(&self) -> Option<&'static Text> {
        symbols::TEXTS.get(self.id() as usize - 1)
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(f)
    }
}

/// A fixed set of declared symbols (lang items, builtin attributes, target features) as the
/// `u16` index of each in the set: a table by [`Symbol::declared_index`], built by `const fn`,
/// so recognizing a member is one compare and one load, with no hash and no string compared.
/// 2 bytes a declared symbol.
pub struct KnownNames {
    by_declared: [u16; symbols::DECLARED],
}

impl KnownNames {
    const NONE: u16 = u16::MAX;

    /// The set whose member `i` is `members[i]`. A symbol named twice is the first member
    /// that names it.
    pub const fn new(members: &[symbols::Declared]) -> KnownNames {
        assert!(members.len() < KnownNames::NONE as usize, "fewer than 65535 members");
        let mut by_declared = [KnownNames::NONE; symbols::DECLARED];
        let mut i = members.len();
        while i > 0 {
            i -= 1;
            by_declared[members[i] as usize] = i as u16;
        }
        KnownNames { by_declared }
    }

    /// The index in the set of `symbol`, if it is a member.
    #[inline]
    pub fn get(&self, symbol: &Symbol) -> Option<u16> {
        let at = *self.by_declared.get(usize::from(symbol.declared_index()?))?;
        (at != KnownNames::NONE).then_some(at)
    }

    /// The index in the set of the declared symbol `declared`, if it is a member.
    pub const fn get_declared(&self, declared: symbols::Declared) -> Option<u16> {
        match self.by_declared[declared as usize] {
            KnownNames::NONE => None,
            at => Some(at),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sym;

    #[test]
    fn smoke_test() {
        let hello = Symbol::intern("hello");
        let world = Symbol::intern("world");
        let bang = Symbol::intern("!");
        let q = Symbol::intern("?");
        assert_eq!(Symbol::intern("!"), bang);
        assert_eq!(Symbol::intern("hello"), hello);
        assert!(text::end() as usize > symbols::STATIC + 1);
        assert_ne!(hello, world);
        assert_ne!(bang, q);
        let default = Symbol::intern("default");
        assert_eq!(
            "hello default world!",
            format!("{} {} {}{}", hello.as_str(), default.as_str(), world.as_str(), bang.as_str())
        );
        assert_eq!(format!("{hello:?} {world}"), "\"hello\" world");
    }

    #[test]
    fn round_trips() {
        for s in ["self", "Self", "", "fn", "0", "255", "256", "a new symbol", "ünïcödé", "x"] {
            assert_eq!(Symbol::intern(s).as_str(), s);
            assert_eq!(Symbol::intern(s), Symbol::intern(&s.to_owned()));
            assert_eq!(Symbol::find(s), Some(Symbol::intern(s)));
            assert_eq!(Symbol::from_id(Symbol::intern(s).id()), Some(Symbol::intern(s)));
            assert_eq!(Symbol::from_held_id(Symbol::intern(s).id()), Symbol::intern(s));
            assert_eq!(Symbol::from_held_id(Symbol::intern(s).id()).as_str(), s);
        }
        let long = "y".repeat(1 << 20);
        assert_eq!(Symbol::intern(&long).as_str(), long);
        assert_eq!(Symbol::find("never interned anywhere"), None);
        assert_eq!(Symbol::from_id(0), None);
        assert_eq!(Symbol::from_id(u32::MAX - 1), None);
    }

    #[test]
    fn declared_symbols_are_interned() {
        assert_eq!(Symbol::intern("self"), sym::self_);
        assert_eq!(Symbol::intern("Self"), sym::Self_);
        assert_eq!(Symbol::intern("..."), sym::dotdotdot);
        assert_eq!(Symbol::intern(""), Symbol::empty());
        assert_eq!(Symbol::empty(), sym::__empty);
        assert_eq!(sym::self_.as_str(), "self");
        assert_ne!(sym::self_, sym::Self_);
        const DECLARED: Symbol = sym::self_;
        assert_eq!(DECLARED, Symbol::intern("self"));
    }

    #[test]
    fn declared_indices() {
        use crate::sym::Declared;
        assert_eq!(sym::self_.declared_index(), Some(Declared::self_ as u16));
        assert_eq!(Symbol::intern("self").declared_index(), Some(Declared::self_ as u16));
        assert_eq!(sym::Self_.declared_index(), Some(Declared::Self_ as u16));
        assert_eq!(Symbol::empty().declared_index(), Some(Declared::__empty as u16));
        assert_eq!(Symbol::intern("not a declared symbol").declared_index(), None);
        assert_eq!(sym::Integer::get(3).declared_index(), None);
        assert_eq!(sym::RaGeneratedName::get(3).declared_index(), None);
        assert!(sym::DECLARED <= u16::MAX as usize);
    }

    /// No two declared symbols share a text, and none is a list's: interning each text gives
    /// its own declared symbol, which tables by declared index and compares with `sym::` rely
    /// on. A list's text gives its list symbol.
    #[test]
    fn every_compiled_in_text_interns_to_itself() {
        assert_eq!(symbols::TEXTS.len(), sym::STATIC);
        for (at, text) in symbols::TEXTS.iter().enumerate() {
            let symbol = Symbol::intern(text.str);
            assert_eq!(symbol, Symbol::from_index(at), "{:?}", text.str);
            assert_eq!(symbol.declared_index(), (at < sym::DECLARED).then_some(at as u16));
            assert_eq!(symbol.order_key(), order_key(text.str.as_bytes()));
        }
    }

    /// Every compiled-in text is its own index in `COMPILED`, by its bytes and by a hash
    /// handed over, so interning one never probes the table.
    #[test]
    fn every_compiled_in_text_is_found_without_the_table() {
        for (at, text) in symbols::TEXTS.iter().enumerate() {
            let bytes = text.str.as_bytes();
            assert_eq!(symbols::COMPILED.get(text.str), Some(at), "{:?}", text.str);
            assert_eq!(symbols::COMPILED.index_by_name_hash(blaze_phf::name_hash(bytes), bytes), Some(at));
        }
        for other in ["selff", "Self_", "a new symbol", "ünïcödé", "256", "<ra@gennew>", "c_int "] {
            let bytes = other.as_bytes();
            assert_eq!(symbols::COMPILED.index_by_name_hash(blaze_phf::name_hash(bytes), bytes), None, "{other:?}");
        }
    }

    /// The names the coordinator named, and the most frequent of `library/` and `harness/`,
    /// are fixed symbols.
    #[test]
    fn common_names_are_compiled_in() {
        for name in ["self", "Some", "Ok", "new", "iter", "std", "None", "Err", "a", "T", "unwrap", "len"] {
            let symbol = Symbol::intern(name);
            assert!(symbol.static_index().is_some(), "{name} is not compiled in");
            assert!(symbol.declared_index().is_some(), "{name} is not declared");
        }
        assert_eq!(Symbol::intern("c_int"), sym::c_int);
        assert_eq!(Symbol::intern("__m128i"), sym::__m128i);
    }

    #[test]
    fn a_hash_handed_over_is_the_same_as_hashing() {
        let mut names: Vec<String> = symbols::TEXTS.iter().map(|text| text.str.to_owned()).collect();
        names.extend((0..500).map(|n| format!("hashed name {n}")));
        names.extend(["", "x_", "ünïcödé", "a much longer name than one word"].map(String::from));
        for name in &names {
            let hash = blaze_phf::name_hash(name.as_bytes());
            assert_eq!(Symbol::intern_hashed(name, hash), Symbol::intern(name), "{name:?}");
            assert_eq!(Symbol::find_hashed(name, hash), Symbol::find(name), "{name:?}");
            assert_eq!(Symbol::find_hashed(name, hash), Some(Symbol::intern(name)), "{name:?}");
        }
        assert_eq!(Symbol::find_hashed("never interned by any test", blaze_phf::name_hash(b"never interned by any test")), None);
    }

    #[test]
    fn known_names() {
        use crate::sym::Declared;
        static SET: KnownNames = KnownNames::new(&[Declared::fn_, Declared::self_, Declared::fn_]);
        assert_eq!(SET.get(&sym::fn_), Some(0));
        assert_eq!(SET.get(&Symbol::intern("fn")), Some(0));
        assert_eq!(SET.get(&sym::self_), Some(1));
        assert_eq!(SET.get(&sym::Self_), None);
        assert_eq!(SET.get(&Symbol::intern("a runtime name")), None);
        assert_eq!(SET.get(&sym::Integer::get(0)), None);
        assert_eq!(SET.get_declared(Declared::self_), Some(1));
        assert_eq!(SET.get_declared(Declared::crate_), None);
    }

    #[test]
    fn integers() {
        for n in [0usize, 1, 9, 10, 255, 256, 1_000, 123_456_789] {
            let declared = sym::Integer::get(n);
            assert_eq!(declared.as_str(), n.to_string());
            assert_eq!(declared, Symbol::intern(&n.to_string()));
            assert_eq!(sym::Integer::as_uint(&declared), Some(n));
            assert_eq!(sym::Integer::as_uint(&Symbol::intern(&n.to_string())), Some(n));
        }
        assert_eq!(sym::Integer::as_uint(&sym::self_), None);
        assert_eq!(sym::Integer::as_uint(&Symbol::intern("12a")), None);
        assert_eq!(sym::Integer::as_uint(&sym::RaGeneratedName::get(3)), None);
        assert_eq!(sym::RaGeneratedName::get(3).as_str(), "<ra@gennew>3");
        assert_eq!(sym::RaGeneratedName::get(300).as_str(), "<ra@gennew>300");
        assert_eq!(sym::RaGeneratedName::get(300), Symbol::intern("<ra@gennew>300"));
    }

    #[test]
    fn symbols_from_many_threads() {
        let seen: Vec<Vec<Symbol>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..16u32)
                .map(|worker| {
                    scope.spawn(move || {
                        (0..10_000u32)
                            .map(|n| (n + worker * 613) % 10_000)
                            .map(|n| Symbol::intern(&format!("racing {n}")))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers.into_iter().map(|it| it.join().unwrap()).collect()
        });
        for (worker, symbols) in seen.iter().enumerate() {
            for (i, &symbol) in symbols.iter().enumerate() {
                let n = (i as u32 + worker as u32 * 613) % 10_000;
                assert_eq!(symbol.as_str(), format!("racing {n}"));
                assert_eq!(symbol, seen[0][n as usize]);
            }
        }
    }

    #[test]
    fn order_keys() {
        assert_eq!(order_key(b""), 0);
        assert_eq!(order_key(b"a"), 0x61 << 56);
        assert_eq!(order_key(b"abcdefgh"), u64::from_be_bytes(*b"abcdefgh"));
        assert_eq!(order_key(b"abcdefghij"), u64::from_be_bytes(*b"abcdefgh"));
        assert_eq!(order_key(b"a\0"), order_key(b"a"));
        for s in ["", "a", "self", "a much longer runtime symbol", "ünïcödé"] {
            assert_eq!(Symbol::intern(s).order_key(), order_key(s.as_bytes()));
        }
        assert_eq!(sym::self_.order_key(), order_key(b"self"));
        assert_eq!(sym::Integer::get(42).order_key(), order_key(b"42"));
        assert_eq!(sym::RaGeneratedName::get(7).order_key(), order_key(b"<ra@gennew>7"));
    }

    fn check_order(names: &[&str]) {
        let symbols: Vec<Symbol> = names.iter().map(|s| Symbol::intern(s)).collect();
        for a in &symbols {
            for b in &symbols {
                assert_eq!(a.cmp_order(b), a.as_str().cmp(b.as_str()), "{a:?} against {b:?}");
            }
        }
        let mut by_key = symbols.clone();
        by_key.sort_by(Symbol::cmp_order);
        let mut by_str = symbols;
        by_str.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(by_key, by_str);
    }

    #[test]
    fn cmp_order_is_str_order() {
        check_order(&[
            "",
            "\0",
            "a",
            "a\0",
            "a\0b",
            "a\u{1}",
            "ab",
            "abcdefgh",
            "abcdefgh\0",
            "abcdefgha",
            "abcdefghb",
            "abcdefgi",
            "self",
            "Self",
            "_",
            "~",
            "\u{7f}",
            "ünïcödé",
            "ü",
            "0",
            "255",
            "256",
            "<ra@gennew>3",
            "<ra@gennew>30",
        ]);
    }

    #[test]
    fn cmp_order_matches_str_order_on_random_names() {
        let mut rng = blaze_ids::rng::SplitMix64::new(0x5eed_0f0d_e5);
        // Few letters and lengths around 8 so that many names share an order key.
        let alphabet = ['\0', 'a', 'b', 'é', '_'];
        let names: Vec<String> = (0..400)
            .map(|_| {
                let len = rng.index(13);
                (0..len).map(|_| alphabet[rng.index(alphabet.len())]).collect()
            })
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        check_order(&names);
    }

    /// Restoring into a table that already interned is refused: its IDs may be the image's.
    #[test]
    fn a_restore_after_interning_is_refused() {
        let image = image_of(&Table::new(shape()), &Live::new());
        let cell = OnceLock::new();
        let _ = cell.get_or_init(|| Table::new(shape()));
        assert_eq!(restore_into(&cell, image, &serial), Err(RestoreError::Started { section: SectionId::SYMBOLS }));
        let fresh = OnceLock::new();
        assert_eq!(restore_into(&fresh, image, &serial), Ok(()));
        assert_eq!(fresh.get().unwrap().find(b"self"), Some(sym::self_.id()));
    }

    /// A symbol section written by a blaze with other compiled-in texts is refused by schema.
    #[test]
    fn another_symbol_list_is_refused() {
        let mut builder = Builder::new();
        let other = SchemaHash(Hasher::new("blaze intern symbols").finish());
        Table::new(shape()).write(&Live::new(), &mut builder.section(SectionId::SYMBOLS, other));
        let image: &'static Image<Vec<u8>> =
            Box::leak(Box::new(Image::open(builder.finish().unwrap()).unwrap()));
        assert!(matches!(
            restore_into(&OnceLock::new(), image, &serial),
            Err(RestoreError::Refused(blaze_image::Refused::Schema { .. }))
        ));
    }

    fn image_of(table: &Table, live: &Live) -> &'static Image<Vec<u8>> {
        let mut builder = Builder::new();
        table.write(live, &mut builder.section(SectionId::SYMBOLS, symbols_schema()));
        Box::leak(Box::new(Image::open(builder.finish().unwrap()).unwrap()))
    }
}
