//! The token row form: the ONE home of how a kept token stream is laid out in a record.
//!
//! Three records keep token streams and all three read this text: the kept expansions set
//! ([`crate::kept_set`]), a crate's proc-macro results table (blaze-proc-macro's
//! `record::table`) and the outline's token entry (blaze-file-items, when it takes tokens).
//!
//! ```text
//! row    one u32, little-endian: an atom id. A stream of n tokens is 4n bytes, any alignment.
//!        Nothing else is per token in the row.
//! atom   12 bytes, each distinct atom once a table:
//!          word        u32  `token_word`: the kind (bits 0..3: ident 0, punct 1, literal 2,
//!                           open 3, close 4), the SPACING (bits 3..5), then an identifier's
//!                           `r#` (bit 5), a punct's character (bits 8..29), a literal's kind
//!                           (5..9) and its `#`s (9..17), a delimiter (5..7)
//!          text end    u32  where its text ends in the table's text section; it starts where
//!                           the atom before ends (the first at 0). The text is what
//!                           [`TokenKind`] says: a name without `r#`, a literal's inside then
//!                           its suffix, nothing for a punct or a delimiter
//!          suffix_len  u32  a literal's suffix length in bytes; 0 for every other kind
//! text   the atoms' texts, end to end, UTF-8, nothing after the last atom's
//! ```
//!
//! - **Spacing** is in the atom's word, never in the row: `a` alone and `a` joint are two atoms.
//! - **Hygiene and origins** are never in the row: a stream's contexts, and where each token of
//!   an expansion came from, are side columns of the record that holds the rows (a proc
//!   answer's tokens are all of one root context; its origins are runs).
//! - **A group's open and close** are ordinary tokens: atoms of kind 3 and 4 with the
//!   delimiter and no text. They carry nothing else: no index of the matching close (the
//!   outline scans a body; it asked for no skips column).
//! - **Order.** The atoms' order is any order that is a function of the table's content, so
//!   two writers of one content write equal bytes (rule 25): [`AtomsBuilder::finish`] sorts by
//!   (word, suffix_len, text), whatever order the atoms were interned in;
//!   [`AtomsBuilder::finish_first_use`] keeps the interning order, for a writer whose visiting
//!   order is itself a function of the content. A reader never relies on the order.
//! - **Proved once (rule 74).** [`Atoms::of`] checks the table whole; an [`Atoms`] is the proof
//!   and a read through it checks nothing of the table again. A stream's rows are checked
//!   where they are read ([`Atoms::extend_rows`]: each id inside the table).
//! - **A later step.** The outline's lane may add a fourth u32 to the atom, the parser's word
//!   (keyword resolved in the token's edition, `JOINT` from the spacing), so its words column
//!   is one gather a token. That is a new layout: [`ATOM`] changes and every record that holds
//!   an atom table steps its own layout word with it; nothing reads both widths.

use core::fmt;

use blaze_own::{AVec, Alloc, Vec};

use crate::{
    error::StreamError,
    tokens::{Ctx, TokenKind, TokenStream},
};

pub use crate::record::{token_of, token_word};

/// An atom's bytes: its word, its text's end, its suffix length.
pub const ATOM: usize = 12;
/// A row's bytes: an atom id.
pub const ROW: usize = 4;

/// The spacing's bits in an atom's word.
const SPACING_BITS: u32 = 0b11 << 3;

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn wide(value: u32) -> usize {
    usize::try_from(value).expect("a u32 fits a usize on every target blaze runs on")
}

/// Why bytes are no atom table, or rows no stream of one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AtomsError {
    /// The atom table is `len` bytes: not a whole number of atoms.
    CutAtom { len: usize },
    /// The atoms' text is not UTF-8.
    TextNotUtf8,
    /// Atom `atom`'s text ends before the atom before it does, past the text, or inside a
    /// character.
    TextEnd { atom: usize },
    /// The last atom's text ends at `end` and the text is `len` bytes.
    TextLeft { end: usize, len: usize },
    /// Atom `atom`'s word is no token kind's.
    Word { atom: usize, word: u32 },
    /// Atom `atom` is no literal and has a suffix length.
    Suffix { atom: usize },
    /// A stream's rows are `len` bytes: not a whole number of rows.
    CutRow { len: usize },
    /// Row `row` names atom `id`, past the table's atoms.
    NoSuchAtom { row: usize, id: u32 },
    /// More atoms, or more bytes of atom text, than 32 bits count.
    TooLarge,
    /// The stream the rows were read into refused a token.
    Stream(StreamError),
}

impl fmt::Display for AtomsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            AtomsError::CutAtom { len } => write!(f, "an atom table of {len} bytes is not a whole number of {ATOM}-byte atoms"),
            AtomsError::TextNotUtf8 => write!(f, "an atom table's text is not UTF-8"),
            AtomsError::TextEnd { atom } => write!(f, "atom {atom}'s text ends before the atom before it, past the text, or inside a character"),
            AtomsError::TextLeft { end, len } => write!(f, "an atom table's last atom ends at byte {end} of a text of {len} bytes"),
            AtomsError::Word { atom, word } => write!(f, "atom {atom}'s word {word:#x} is no token kind's"),
            AtomsError::Suffix { atom } => write!(f, "atom {atom} is no literal and has a suffix length"),
            AtomsError::CutRow { len } => write!(f, "token rows of {len} bytes are not a whole number of {ROW}-byte rows"),
            AtomsError::NoSuchAtom { row, id } => write!(f, "token row {row} names atom {id}, past its table's atoms"),
            AtomsError::TooLarge => write!(f, "an atom table passed 2^32 atoms or bytes of text"),
            AtomsError::Stream(error) => write!(f, "{error}"),
        }
    }
}

impl core::error::Error for AtomsError {}

/// The kind an atom's word and suffix length are; `None` for a word [`token_word`] does not
/// write.
fn kind_of(word: u32, suffix_len: u32) -> Option<TokenKind> {
    Some(match token_of(word)? {
        TokenKind::Literal { kind, spacing, .. } => TokenKind::Literal { kind, suffix_len, spacing },
        other => other,
    })
}

/// An atom's word and suffix length: the two fields of it that are not its text.
fn word_of(kind: TokenKind) -> (u32, u32) {
    let suffix_len = match kind {
        TokenKind::Literal { suffix_len, .. } => suffix_len,
        TokenKind::Ident { .. } | TokenKind::Punct { .. } | TokenKind::Open(_) | TokenKind::Close(_) => 0,
    };
    (token_word(kind), suffix_len)
}

/// An atom table read in place, proved: every atom's word is a kind's, every text a whole
/// `str` inside the text section, the section used to its end. Made only by [`Atoms::of`].
#[derive(Clone, Copy, Debug)]
pub struct Atoms<'t> {
    atoms: &'t [u8],
    /// The text section, proved a whole `str` by [`Atoms::of`] (or, through
    /// [`Checked::atoms`], by an earlier `Atoms::of` of the same bytes): held as one, so an
    /// atom's text is a `str` slice and its push needs no second check.
    text: &'t str,
}

impl<'t> Atoms<'t> {
    /// The table whose atoms are `atoms` ([`ATOM`] bytes each) over the text `text`, checked
    /// once: any alignment, nothing copied.
    pub fn of(atoms: &'t [u8], text: &'t [u8]) -> Result<Atoms<'t>, AtomsError> {
        if atoms.len() % ATOM != 0 {
            return Err(AtomsError::CutAtom { len: atoms.len() });
        }
        let whole = core::str::from_utf8(text).map_err(|_| AtomsError::TextNotUtf8)?;
        let mut start = 0usize;
        for (atom, row) in atoms.chunks_exact(ATOM).enumerate() {
            let word = u32_at(row, 0);
            let end = wide(u32_at(row, 4));
            if end < start || end > text.len() || !whole.is_char_boundary(end) {
                return Err(AtomsError::TextEnd { atom });
            }
            let kind = token_of(word).ok_or(AtomsError::Word { atom, word })?;
            if !matches!(kind, TokenKind::Literal { .. }) && u32_at(row, 8) != 0 {
                return Err(AtomsError::Suffix { atom });
            }
            start = end;
        }
        if start != text.len() {
            return Err(AtomsError::TextLeft { end: start, len: text.len() });
        }
        Ok(Atoms { atoms, text: whole })
    }

    /// How many atoms the table holds.
    pub fn len(&self) -> usize {
        self.atoms.len() / ATOM
    }

    pub fn is_empty(&self) -> bool {
        self.atoms.is_empty()
    }

    /// Atom `id`'s word, its text's start and end, its suffix length. `id` is below
    /// [`Atoms::len`].
    fn part(&self, id: usize) -> (u32, usize, usize, u32) {
        let at = id * ATOM;
        let start = if id == 0 { 0 } else { wide(u32_at(self.atoms, at - ATOM + 4)) };
        (u32_at(self.atoms, at), start, wide(u32_at(self.atoms, at + 4)), u32_at(self.atoms, at + 8))
    }

    /// Atom `id`: its kind (a literal's with its suffix length) and its text; `None` past the
    /// table.
    pub fn atom(&self, id: u32) -> Option<(TokenKind, &'t str)> {
        let id = wide(id);
        if id >= self.len() {
            return None;
        }
        let (word, start, end, suffix_len) = self.part(id);
        let kind = kind_of(word, suffix_len).expect("`Atoms::of` checked every atom's word");
        // `Atoms::of` checked every atom's end is a character boundary of the text.
        let text = &self.text[start..end];
        Some((kind, text))
    }

    /// Appends the tokens `rows` name ([`ROW`] bytes each) to `out`, token `i` in the context
    /// `ctx_of(i)` (a context of `out`). Room is made once, for all of them; a row past the
    /// table is refused before any token is appended.
    pub fn extend_rows(&self, out: &mut TokenStream<'_>, rows: &[u8], mut ctx_of: impl FnMut(usize) -> Ctx) -> Result<(), AtomsError> {
        if rows.len() % ROW != 0 {
            return Err(AtomsError::CutRow { len: rows.len() });
        }
        let atoms = self.len();
        let mut text = 0usize;
        for (row, id) in rows.chunks_exact(ROW).enumerate() {
            let id = u32_at(id, 0);
            if wide(id) >= atoms {
                return Err(AtomsError::NoSuchAtom { row, id });
            }
            let (_, start, end, _) = self.part(wide(id));
            text += end - start;
        }
        out.reserve(rows.len() / ROW, text);
        for (row, id) in rows.chunks_exact(ROW).enumerate() {
            let (word, start, end, suffix_len) = self.part(wide(u32_at(id, 0)));
            let kind = kind_of(word, suffix_len).expect("`Atoms::of` checked every atom's word");
            let text = crate::tokens::WholeText::of(&self.text[start..end]);
            out.push_whole(kind, text, ctx_of(row)).map_err(AtomsError::Stream)?;
        }
        Ok(())
    }
}

/// That a table's two sections were proved by [`Atoms::of`]: what an owner of the bytes keeps
/// (it cannot hold the borrow beside them) to read them again as [`Atoms`] with no second
/// check. It holds the sections' lengths and nothing of their content: the owner gives back
/// the SAME bytes, unchanged since they were proved, and sections of other lengths are
/// refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Checked {
    atoms: usize,
    text: usize,
}

impl Checked {
    /// The proof of the table with no atom and no text.
    pub const EMPTY: Checked = Checked { atoms: 0, text: 0 };

    /// The table these two sections were proved to be; `None` for sections of other lengths
    /// than the proved ones.
    ///
    /// # Safety
    ///
    /// `atoms` and `text` are the very bytes this proof was taken of ([`Atoms::checked`] of an
    /// [`Atoms::of`] that returned `Ok`), unchanged since. Only their lengths are checked here;
    /// the text is then read as a `str` with no UTF-8 check, and its atoms' ends as character
    /// boundaries, so other bytes of the same lengths would put non-UTF-8 into a token
    /// stream's text table, which [`crate::tokens::TokenStream::text_of`] reads unchecked.
    pub unsafe fn atoms<'t>(&self, atoms: &'t [u8], text: &'t [u8]) -> Option<Atoms<'t>> {
        if atoms.len() != self.atoms || text.len() != self.text {
            return None;
        }
        // SAFETY: by this function's contract `text` is the text section `Atoms::of` proved
        // with `core::str::from_utf8` (the proof's only maker), unchanged since, so it is
        // valid UTF-8; and `atoms` is the section whose every end it proved a character
        // boundary of that text.
        let text = unsafe { core::str::from_utf8_unchecked(text) };
        Some(Atoms { atoms, text })
    }
}

impl Atoms<'_> {
    /// The proof this table is, without its borrow: see [`Checked`].
    pub fn checked(&self) -> Checked {
        Checked { atoms: self.atoms.len(), text: self.text.len() }
    }
}

impl<'a> TokenStream<'a> {
    /// The stream `rows` name in `atoms`, every token in the context `ctx` (a root: the new
    /// stream has no context of its own), in `alloc`: its tables are sized once.
    pub fn from_rows(atoms: &Atoms<'_>, rows: &[u8], ctx: Ctx, alloc: Alloc<'a>) -> Result<TokenStream<'a>, AtomsError> {
        let mut out = TokenStream::new_in(alloc);
        atoms.extend_rows(&mut out, rows, |_| ctx)?;
        Ok(out)
    }
}

/// A bug planted in the writer, for the control of this form's test.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AtomPlant {
    None,
    /// The spacing's bits are left out of an atom's word: `a` joint is kept as `a` alone.
    SpacingDropped,
}

/// One interned atom while a table is written.
#[derive(Clone, Copy)]
struct Made {
    word: u32,
    suffix_len: u32,
    /// Its text in the builder's text, in interning order.
    start: u32,
    end: u32,
}

/// A table being written: each distinct (kind, text) interned once.
pub struct AtomsBuilder<'b> {
    made: AVec<'b, Made>,
    text: AVec<'b, u8>,
    /// Open addressing over `made`: an atom's index plus one, 0 for an empty slot; a power of
    /// two long, at most half full.
    slots: AVec<'b, u32>,
    /// The hash's seed, from the caller (rule 75). No output depends on it: an id is the
    /// atom's interning order, and [`AtomsBuilder::finish`] sorts by content.
    seed: u64,
    plant: AtomPlant,
}

/// What a builder wrote.
pub struct Built<'b> {
    /// The atom table: [`ATOM`] bytes an atom.
    pub atoms: AVec<'b, u8>,
    /// The atoms' texts.
    pub text: AVec<'b, u8>,
    /// For each id [`AtomsBuilder::intern`] gave, the atom's id in `atoms`.
    pub ids: AVec<'b, u32>,
}

impl Built<'_> {
    /// Appends to `out` the rows of a stream whose tokens were interned as `interned` (the
    /// ids [`AtomsBuilder::intern`] gave, in the stream's order).
    pub fn rows(&self, interned: &[u32], out: &mut AVec<'_, u8>) {
        out.reserve(interned.len() * ROW);
        for &id in interned {
            out.extend_from_slice(&self.ids[wide(id)].to_le_bytes());
        }
    }
}

fn hash(seed: u64, word: u32, suffix_len: u32, text: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h = (seed ^ (u64::from(word) | u64::from(suffix_len) << 32)).wrapping_mul(K);
    let mut chunks = text.chunks_exact(8);
    for chunk in &mut chunks {
        let mut word = [0u8; 8];
        word.copy_from_slice(chunk);
        h = (h.rotate_left(27) ^ u64::from_le_bytes(word)).wrapping_mul(K);
    }
    let rest = chunks.remainder();
    let mut last = [0u8; 8];
    last[..rest.len()].copy_from_slice(rest);
    // The length's low byte beside the last bytes: `a` and `a\0` differ.
    last[7] ^= text.len().to_le_bytes()[0];
    h = (h.rotate_left(27) ^ u64::from_le_bytes(last)).wrapping_mul(K);
    h ^ h >> 32
}

impl<'b> AtomsBuilder<'b> {
    /// An empty table in `alloc`, its lookups hashed under `seed`.
    pub fn new_in(alloc: Alloc<'b>, seed: u64) -> AtomsBuilder<'b> {
        AtomsBuilder::planted(alloc, seed, AtomPlant::None)
    }

    /// [`AtomsBuilder::new_in`] with a planted bug: what the form's control runs.
    pub fn planted(alloc: Alloc<'b>, seed: u64, plant: AtomPlant) -> AtomsBuilder<'b> {
        AtomsBuilder { made: Vec::new_in(alloc), text: Vec::new_in(alloc), slots: Vec::new_in(alloc), seed, plant }
    }

    /// How many distinct atoms were interned.
    pub fn len(&self) -> usize {
        self.made.len()
    }

    pub fn is_empty(&self) -> bool {
        self.made.is_empty()
    }

    fn text_of(&self, made: Made) -> &[u8] {
        &self.text[wide(made.start)..wide(made.end)]
    }

    /// The slot `word`, `suffix_len` and `text` are in, or the empty one they go in.
    fn slot(&self, word: u32, suffix_len: u32, text: &[u8]) -> usize {
        let mask = self.slots.len() - 1;
        // The hash's low bits, cut to the table: a truncation is what is wanted.
        let mut at = usize::try_from(hash(self.seed, word, suffix_len, text) & u64::try_from(mask).unwrap_or(u64::MAX)).unwrap_or(0);
        loop {
            match self.slots[at] {
                0 => return at,
                held => {
                    let made = self.made[wide(held - 1)];
                    if made.word == word && made.suffix_len == suffix_len && self.text_of(made) == text {
                        return at;
                    }
                }
            }
            at = (at + 1) & mask;
        }
    }

    /// Twice the slots (16 at first), every atom placed again.
    fn grow(&mut self) {
        let slots = (self.slots.len() * 2).max(16);
        self.slots.clear();
        self.slots.resize(slots, 0);
        for index in 0..self.made.len() {
            let made = self.made[index];
            let at = self.slot(made.word, made.suffix_len, self.text_of(made));
            self.slots[at] = u32::try_from(index + 1).expect("`intern` refused an atom past 32 bits");
        }
    }

    /// The id of the atom `kind` with `text` is, interned now when it is new: ids count from 0
    /// in interning order ([`Built::ids`] gives each its place in the table written).
    pub fn intern(&mut self, kind: TokenKind, text: &str) -> Result<u32, AtomsError> {
        self.intern_bytes(kind, text.as_bytes())
    }

    /// [`AtomsBuilder::intern`] of text given as bytes that are a whole `str` (a stream's own
    /// [`TokenStream::bytes_at`]).
    fn intern_bytes(&mut self, kind: TokenKind, text: &[u8]) -> Result<u32, AtomsError> {
        let (word, suffix_len) = word_of(kind);
        let word = match self.plant {
            AtomPlant::None => word,
            AtomPlant::SpacingDropped => word & !SPACING_BITS,
        };
        if (self.made.len() + 1) * 2 > self.slots.len() {
            self.grow();
        }
        let at = self.slot(word, suffix_len, text);
        if let Some(index) = self.slots[at].checked_sub(1) {
            return Ok(index);
        }
        // The id plus one is a slot's value, so the last id is `u32::MAX - 1`.
        let id = u32::try_from(self.made.len()).ok().filter(|id| *id < u32::MAX).ok_or(AtomsError::TooLarge)?;
        let start = u32::try_from(self.text.len()).map_err(|_| AtomsError::TooLarge)?;
        let end = u32::try_from(text.len()).ok().and_then(|len| start.checked_add(len)).ok_or(AtomsError::TooLarge)?;
        self.text.extend_from_slice(text);
        self.made.push(Made { word, suffix_len, start, end });
        self.slots[at] = id + 1;
        Ok(id)
    }

    /// Interns every token of `stream`, appending each one's id to `interned` in order.
    pub fn intern_stream(&mut self, stream: &TokenStream<'_>, interned: &mut AVec<'_, u32>) -> Result<(), AtomsError> {
        interned.reserve(stream.len());
        for (at, token) in stream.tokens().iter().enumerate() {
            interned.push(self.intern_bytes(token.kind, stream.bytes_at(at))?);
        }
        Ok(())
    }

    /// The table, its atoms sorted by (word, suffix_len, text): the same bytes whatever order
    /// the atoms were interned in.
    pub fn finish(self) -> Built<'b> {
        let alloc = *self.made.allocator();
        let mut order: AVec<'b, u32> = Vec::new_in(alloc);
        order.reserve(self.made.len());
        for index in 0..self.made.len() {
            order.push(u32::try_from(index).expect("`intern` refused an atom past 32 bits"));
        }
        order.sort_unstable_by(|&a, &b| {
            let (a, b) = (self.made[wide(a)], self.made[wide(b)]);
            (a.word, a.suffix_len).cmp(&(b.word, b.suffix_len)).then_with(|| self.text_of(a).cmp(self.text_of(b)))
        });
        self.built(&order)
    }

    /// The table, its atoms in the order they were interned: equal bytes for two writers only
    /// when their interning order is itself a function of the content.
    pub fn finish_first_use(self) -> Built<'b> {
        let alloc = *self.made.allocator();
        let mut order: AVec<'b, u32> = Vec::new_in(alloc);
        order.reserve(self.made.len());
        for index in 0..self.made.len() {
            order.push(u32::try_from(index).expect("`intern` refused an atom past 32 bits"));
        }
        self.built(&order)
    }

    /// The table with atom `order[i]` (an interning id) at place `i`.
    fn built(&self, order: &[u32]) -> Built<'b> {
        let alloc = *self.made.allocator();
        let mut atoms: AVec<'b, u8> = Vec::new_in(alloc);
        let mut text: AVec<'b, u8> = Vec::new_in(alloc);
        let mut ids: AVec<'b, u32> = Vec::new_in(alloc);
        atoms.reserve(order.len() * ATOM);
        text.reserve(self.text.len());
        ids.resize(order.len(), 0);
        for (place, &index) in order.iter().enumerate() {
            let made = self.made[wide(index)];
            text.extend_from_slice(self.text_of(made));
            // The whole text is the builder's, which `intern` kept within 32 bits.
            let end = u32::try_from(text.len()).expect("`intern` refused text past 32 bits");
            atoms.extend_from_slice(&made.word.to_le_bytes());
            atoms.extend_from_slice(&end.to_le_bytes());
            atoms.extend_from_slice(&made.suffix_len.to_le_bytes());
            ids[wide(index)] = u32::try_from(place).expect("`intern` refused an atom past 32 bits");
        }
        Built { atoms, text, ids }
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::{AVec, Vec};
    use blaze_sim::{Broke, Config, Tape};

    use super::{AtomPlant, Atoms, AtomsBuilder, AtomsError, Built};
    use crate::tokens::{Ctx, Delim, Edition, LitKind, Raw, Spacing, TokenKind, TokenStream};

    const SPACINGS: [Spacing; 3] = [Spacing::Alone, Spacing::Joint, Spacing::JointHidden];
    const DELIMS: [Delim; 4] = [Delim::Paren, Delim::Brace, Delim::Bracket, Delim::Invisible];
    const LITS: [LitKind; 5] = [LitKind::Integer, LitKind::Str, LitKind::StrRaw(2), LitKind::Char, LitKind::Float];
    const NAMES: [&str; 5] = ["a", "self", "caf\u{e9}", "x1", "a_long_identifier_past_one_word"];
    /// A literal's text (its inside then its suffix) and its suffix's length: the same text
    /// with two suffix lengths is two atoms.
    const LIT_TEXTS: [(&str, u32); 6] = [("1", 0), ("1u8", 2), ("1u8", 0), ("h\u{e9}llof64", 3), ("", 0), ("0x1f", 0)];
    const PUNCTS: [char; 5] = ['+', ':', '#', '\'', '$'];

    fn harness(what: &str) -> Broke {
        Broke::Harness(what.into())
    }

    /// One random token: its kind and its text.
    fn token(tape: &Tape) -> Result<(TokenKind, &'static str), Broke> {
        let spacing = *tape.pick(&SPACINGS, "spacing").ok_or_else(|| harness("no spacing"))?;
        Ok(match tape.below(5, "kind") {
            0 => {
                let raw = if tape.below(2, "raw") == 1 { Raw::Yes } else { Raw::No };
                (TokenKind::Ident { raw, spacing }, *tape.pick(&NAMES, "name").ok_or_else(|| harness("no name"))?)
            }
            1 => (TokenKind::Punct { char: *tape.pick(&PUNCTS, "punct").ok_or_else(|| harness("no punct"))?, spacing }, ""),
            2 => {
                let kind = *tape.pick(&LITS, "literal kind").ok_or_else(|| harness("no literal kind"))?;
                let (text, suffix_len) = *tape.pick(&LIT_TEXTS, "literal text").ok_or_else(|| harness("no literal text"))?;
                (TokenKind::Literal { kind, suffix_len, spacing }, text)
            }
            3 => (TokenKind::Open(*tape.pick(&DELIMS, "open").ok_or_else(|| harness("no delimiter"))?), ""),
            _ => (TokenKind::Close(*tape.pick(&DELIMS, "close").ok_or_else(|| harness("no delimiter"))?), ""),
        })
    }

    /// The table and rows of `tokens`, interned from the last to the first when `backward`,
    /// the rows in the stream's own order.
    fn written(tokens: &[(TokenKind, &'static str)], backward: bool, seed: u64, plant: AtomPlant) -> Result<(Built<'static>, AVec<'static, u8>), Broke> {
        let alloc = blaze_own::testing::heap().alloc();
        let mut builder = AtomsBuilder::planted(alloc, seed, plant);
        let mut interned: AVec<'static, u32> = Vec::new_in(alloc);
        interned.resize(tokens.len(), 0);
        for step in 0..tokens.len() {
            let at = if backward { tokens.len() - 1 - step } else { step };
            let (kind, text) = tokens[at];
            interned[at] = builder.intern(kind, text).map_err(|_| harness("an atom was not interned"))?;
        }
        let built = builder.finish();
        let mut rows: AVec<'static, u8> = Vec::new_in(alloc);
        built.rows(&interned, &mut rows);
        Ok((built, rows))
    }

    /// A random stream's rows read back are the stream, and its table's bytes do not depend on
    /// the order its atoms were interned in or on the hash's seed.
    fn rows_read_back(tape: &Tape, plant: AtomPlant) -> Result<(), Broke> {
        let alloc = blaze_own::testing::heap().alloc();
        let ctx = Ctx::root(Edition::E2021);
        let count = usize::try_from(tape.below(40, "tokens")).map_err(|_| harness("a count past the machine's word"))?;
        let mut tokens: AVec<'static, (TokenKind, &'static str)> = Vec::new_in(alloc);
        let mut stream = TokenStream::new_in(alloc);
        for _ in 0..count {
            let (kind, text) = token(tape)?;
            stream.push(kind, text, ctx).map_err(|_| harness("a token was not pushed"))?;
            tokens.push((kind, text));
        }
        let (one, one_rows) = written(&tokens, false, 1, plant)?;
        let (two, two_rows) = written(&tokens, true, tape.below(u64::MAX, "seed"), plant)?;
        let atoms = Atoms::of(&one.atoms, &one.text).map_err(|_| Broke::Oracle("the written table is refused".into()))?;
        let back = TokenStream::from_rows(&atoms, &one_rows, ctx, alloc).map_err(|_| Broke::Oracle("the written rows are refused".into()))?;
        if back != stream {
            return Err(Broke::Oracle("a stream read back from its rows is not the stream".into()));
        }
        if one.atoms[..] != two.atoms[..] || one.text[..] != two.text[..] || one_rows[..] != two_rows[..] {
            return Err(Broke::Oracle("two interning orders of one stream wrote unequal bytes".into()));
        }
        Ok(())
    }

    /// Expected line: `test atoms::tests::rows_read_back_are_the_stream_whatever_the_interning_order ... ok`.
    #[test]
    fn rows_read_back_are_the_stream_whatever_the_interning_order() {
        blaze_sim::check(Config::cases(256), |tape: &Tape| rows_read_back(tape, AtomPlant::None));
    }

    /// The control: with the spacing's bits left out of the word the round trip is caught.
    #[test]
    fn control_a_dropped_spacing_is_caught() {
        blaze_sim::control(Config::cases(256), AtomPlant::None, AtomPlant::SpacingDropped, "a stream read back from its rows is not the stream", rows_read_back);
    }

    /// A table cut, or changed in one field, is refused by name; a row past the table too.
    #[test]
    fn a_wrong_table_is_refused_by_name() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut builder = AtomsBuilder::new_in(alloc, 7);
        let a = builder.intern(TokenKind::Ident { raw: Raw::No, spacing: Spacing::Alone }, "caf\u{e9}").expect("interned");
        let b = builder.intern(TokenKind::Punct { char: '+', spacing: Spacing::Joint }, "").expect("interned");
        assert_eq!(builder.intern(TokenKind::Ident { raw: Raw::No, spacing: Spacing::Alone }, "caf\u{e9}"), Ok(a));
        let built = builder.finish();
        let mut rows: AVec<'static, u8> = Vec::new_in(alloc);
        built.rows(&[a, b, a], &mut rows);
        let atoms = Atoms::of(&built.atoms, &built.text).expect("the table is one");
        assert_eq!(atoms.len(), 2);
        assert_eq!(atoms.atom(built.ids[0]), Some((TokenKind::Ident { raw: Raw::No, spacing: Spacing::Alone }, "caf\u{e9}")));
        assert_eq!(atoms.atom(2), None);
        assert_eq!(Atoms::of(&built.atoms[..built.atoms.len() - 1], &built.text).err(), Some(AtomsError::CutAtom { len: 23 }));
        assert_eq!(Atoms::of(&built.atoms, &built.text[..built.text.len() - 1]).err(), Some(AtomsError::TextNotUtf8));
        let copy = |bytes: &[u8]| {
            let mut out: AVec<'static, u8> = Vec::new_in(alloc);
            out.extend_from_slice(bytes);
            out
        };
        let mut longer = copy(&built.text);
        longer.push(b'x');
        assert_eq!(Atoms::of(&built.atoms, &longer).err(), Some(AtomsError::TextLeft { end: 5, len: 6 }));
        let ident = usize::try_from(built.ids[0]).expect("an id fits") * super::ATOM;
        let mut word = copy(&built.atoms);
        word[ident] = 7;
        assert!(matches!(Atoms::of(&word, &built.text).err(), Some(AtomsError::Word { .. })));
        let mut suffix = copy(&built.atoms);
        suffix[ident + 8] = 1;
        assert!(matches!(Atoms::of(&suffix, &built.text).err(), Some(AtomsError::Suffix { .. })));
        let mut inside = copy(&built.atoms);
        inside[ident + 4] = 4;
        assert!(matches!(Atoms::of(&inside, &built.text).err(), Some(AtomsError::TextEnd { .. })));
        let ctx = Ctx::root(Edition::E2021);
        assert_eq!(TokenStream::from_rows(&atoms, &rows[..5], ctx, alloc).err(), Some(AtomsError::CutRow { len: 5 }));
        assert_eq!(TokenStream::from_rows(&atoms, &[2, 0, 0, 0], ctx, alloc).err(), Some(AtomsError::NoSuchAtom { row: 0, id: 2 }));
        assert_eq!(TokenStream::from_rows(&atoms, &rows, ctx, alloc).map(|stream| stream.len()), Ok(3));
    }
}
