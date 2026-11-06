//! The flight recorder's names, per thread (docs/v2/plan/design/profile-names.md): a thread
//! interns text into its own table ([`Local`]), whose ids are scoped to the ring it writes, and
//! the first use of a text in a generation writes its definition into that ring. Readers
//! ([`Streams`]) take the definitions back out of the rows and resolve every row's ids, so no
//! thread that records shares anything to name what it records: no lock, no atomic, no map
//! another core writes.
//!
//! The pieces:
//! - A **token** ([`Token`]) is what a [`crate::span::Name`] and a span's slot hold: a kept name
//!   ([`KEPT_TOKEN`] and its number), or `ring << 44 | generation << 12 | index` with the full
//!   32-bit generation, so it names one definition for the life of the process.
//! - A **row sym** is what a row holds: a kept name ([`KEPT_SYM`] and its number), or
//!   `(generation & 0xff) << 12 | index` of the ring the row is in, of a generation at most
//!   [`WINDOW`] before the ring's generation at that row. A token of another ring, or an older
//!   one, is written as a [`LINK_ROW`] binding an index of this generation to the full token.
//! - A **generation** begins with a [`GEN_ROW`] and holds at most [`INDEX_MAX`] names; a thread
//!   begins the next once its generation's indexes are used or [`GEN_ROWS`] rows were written
//!   since it began, dropping its table whole: no eviction while recording.
//!
//! This module is a sans-IO core: it writes rows through [`Out`] and reads them through
//! [`Source`], both the driver's (`flight.rs`).
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::{alloc::Global, sync::Arc};

use blaze_hash::HashMap;

use super::{KIND_PLOT, WORDS};

/// A name as a slot or a [`crate::span::Name`] holds it: 0 none, a kept name, or a name of one
/// ring's generation (module docs).
pub type Token = u64;

/// The tag of a kept name's token; its low 30 bits are the kept name's number.
pub const KEPT_TOKEN: Token = 1 << 63;
/// The class of a kept name as a row and a reader hold it: `0b10 << 30` and its number.
pub const KEPT_SYM: u32 = 0b10 << 30;
/// The bits of a sym that say its class (kept, a reader's, a postmortem's, or a row's own).
pub const CLASS: u32 = 0b11 << 30;
/// The most names a generation gives: the index bits of a row sym, 0 being none.
pub const INDEX_MAX: u16 = (1 << 12) - 1;
/// Rows a generation lasts at most: then the next name its thread gives begins another.
pub const GEN_ROWS: u64 = 4096;
/// How many generations back a row's own ids may reach: older tokens are linked. Under 256, so
/// the low byte of a generation names one of the window.
pub const WINDOW: u32 = 32;
/// Generations a reader keeps of each ring: names of older ones read as forgotten.
pub const RETAIN: usize = 64;
/// The longest text a definition holds; a longer one is cut at a character's boundary.
pub const TEXT_MAX: usize = (1 << 16) - 1;
/// Bytes of text a definition row holds: its seven words other than its tag.
const ROW_TEXT: usize = 56;
/// The words of a definition row that hold text, in order.
const TEXT_WORDS: [usize; 7] = [0, 1, 2, 3, 5, 6, 7];

const _: () = assert!(WINDOW < 256 && (RETAIN as u32) > WINDOW);
const _: () = assert!(TEXT_WORDS.len() * 8 == ROW_TEXT && WORDS == 8);

/// A generation begins: word 0 its full number.
pub(crate) const GEN_ROW: u64 = 0xf3;
/// A name's definition: its index at bits 8..20 of the tag, its whole length at 20..36, and the
/// first [`ROW_TEXT`] bytes of its text.
pub(crate) const NAME_ROW: u64 = 0xf2;
/// The rest of a definition, [`ROW_TEXT`] bytes a row: its index at 8..20, its part (from 1)
/// at 20..
pub(crate) const NAME_MORE_ROW: u64 = 0xf1;
/// An index of this generation bound to a full token (word 0) of another ring or generation.
pub(crate) const LINK_ROW: u64 = 0xf0;

/// Whether `row` is one of this module's: a reader takes it into its [`Streams`], never into a
/// table.
#[inline]
pub(crate) fn is_names_row(row: &[u64; WORDS]) -> bool {
    row[4] >> 62 == KIND_PLOT && (LINK_ROW..=GEN_ROW).contains(&(row[4] & 0xff))
}

/// The token of `index` of generation `generation` of ring `ring`.
pub const fn token(ring: u16, generation: u32, index: u16) -> Token {
    (ring as u64) << 44 | (generation as u64) << 12 | index as u64
}

/// The ring, generation and index of a token of a ring's name; `None` for 0 and kept names.
pub const fn parts(token: Token) -> Option<(u16, u32, u16)> {
    if token == 0 || token & KEPT_TOKEN != 0 {
        return None;
    }
    Some(((token >> 44) as u16, (token >> 12) as u32, (token & 0xfff) as u16))
}

/// A row sym of `index` of the generation whose low byte is `generation`'s.
const fn local(generation: u32, index: u16) -> u32 {
    (generation & 0xff) << 12 | index as u32
}

/// The key the hot sketches keep a token under ([`crate::hot`]): a kept name's number, or
/// `ring << 20 | (generation & 0xff) << 12 | index`, 0 for none or a kept number past 20 bits.
/// Read back with [`Streams::hot_text`].
pub const fn hot_key(token: Token) -> u32 {
    if token & KEPT_TOKEN != 0 {
        let kept = (token & ((1 << 30) - 1)) as u32;
        return if kept < 1 << 20 { kept } else { 0 };
    }
    match parts(token) {
        Some((ring, generation, index)) => (ring as u32) << 20 | local(generation, index),
        None => 0,
    }
}

/// A bug a test plants in the core, for the DST's controls (`testing` skill): each removes one
/// safeguard the oracle must notice is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Plant {
    #[default]
    None,
    /// A token of another ring or an older generation is written as if it were this ring's own,
    /// with no link.
    NoLink,
    /// A generation begins with no [`GEN_ROW`].
    NoGenRow,
    /// A reader takes a row sym's generation as the newest it holds with that low byte, not the
    /// one in the row's window.
    NoWindow,
}

/// Counts a core hands its driver ([`Out::counted`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Count {
    /// A name defined in a ring.
    Given,
    /// A link written: a name another thread or an older generation gave.
    Linked,
    /// A text cut at [`TEXT_MAX`].
    Cut,
    /// A name a row could not hold: more links than a generation has indexes.
    Lost,
}

/// Where a [`Local`] writes: its thread's ring, as the driver holds it.
pub trait Out {
    /// Rows ever written to the ring.
    fn head(&self) -> u64;
    /// Writes `row` as the ring's next; its index.
    fn put(&mut self, row: [u64; WORDS]) -> u64;
    /// The ring's generation is now `generation`: where a ring keeps it for its next thread.
    fn began(&mut self, generation: u32);
    fn counted(&mut self, count: Count);
}

// --- The writer -------------------------------------------------------------------------

/// A thread's names: its ring's current generation and what it gave in it. Only its thread
/// reads or writes it, and the driver looks it up fresh for every record, so it is never held
/// across a wait (`no-wait-across-thread-locals`).
pub struct Local {
    ring: u16,
    generation: u32,
    /// The ring's index of the generation's [`GEN_ROW`].
    from: u64,
    /// The next index to give.
    next: u16,
    texts: HashMap<Box<str>, u16, Global>,
    links: HashMap<Token, u16, Global>,
    /// [`GEN_ROWS`] and [`INDEX_MAX`], smaller in the DST so it reaches many generations.
    gen_rows: u64,
    index_max: u16,
    plant: Plant,
}

impl Local {
    /// The names of a thread that took ring `ring`, whose last generation was `last` (0 for a
    /// new ring): it begins the next as it first gives a name.
    pub fn new(ring: u16, last: u32) -> Local {
        Local::with_limits(ring, last, GEN_ROWS, INDEX_MAX)
    }

    /// As [`Local::new`], its generations at most `gen_rows` rows and `index_max` names.
    pub fn with_limits(ring: u16, last: u32, gen_rows: u64, index_max: u16) -> Local {
        let index_max = index_max.clamp(1, INDEX_MAX);
        Local {
            ring,
            generation: last,
            from: 0,
            // Past the last index: the first name begins a generation.
            next: index_max + 1,
            // alloc: once per thread that names, its table for one generation at a time
            texts: HashMap::new_in(Global),
            // alloc: as above
            links: HashMap::new_in(Global),
            gen_rows: gen_rows.max(1),
            index_max,
            plant: Plant::None,
        }
    }

    pub fn plant(&mut self, plant: Plant) {
        self.plant = plant;
    }

    pub fn ring(&self) -> u16 {
        self.ring
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Begins the next generation if this one is spent: its indexes used, or its rows past
    /// [`GEN_ROWS`].
    #[inline]
    fn roll(&mut self, out: &mut impl Out) {
        if self.next > self.index_max || out.head().wrapping_sub(self.from) >= self.gen_rows {
            self.begin(out);
        }
    }

    #[cold]
    fn begin(&mut self, out: &mut impl Out) {
        // 0 is no generation: the 32 bits come round only after 4 billion of one ring.
        self.generation = self.generation.wrapping_add(1).max(1);
        self.texts.clear();
        self.links.clear();
        self.next = 1;
        self.from = if self.plant == Plant::NoGenRow {
            out.head()
        } else {
            out.put([u64::from(self.generation), 0, 0, 0, KIND_PLOT << 62 | GEN_ROW, 0, 0, 0])
        };
        out.began(self.generation);
    }

    /// The token of `text`, defined in the ring the first time this generation names it.
    #[inline]
    pub fn intern(&mut self, text: &str, out: &mut impl Out) -> Token {
        self.roll(out);
        if let Some(&index) = self.texts.get(text) {
            return token(self.ring, self.generation, index);
        }
        self.give(text, out)
    }

    #[cold]
    fn give(&mut self, text: &str, out: &mut impl Out) -> Token {
        let index = self.next;
        self.next += 1;
        let (kept, cut) = cut(text);
        if cut {
            out.counted(Count::Cut);
        }
        define(index, kept.as_bytes(), out);
        out.counted(Count::Given);
        // alloc: once per distinct text a generation, dropped with the generation
        self.texts.insert(text.into(), index);
        token(self.ring, self.generation, index)
    }

    /// Turns `words`, the tokens one row this thread writes next will hold, into that row's
    /// syms, in place: a token's own id while it is this ring's and in the window, else an index
    /// linked to it. All of a row's syms come from one call, which begins a generation (if one
    /// is due, or the links it may write would not fit) before it gives any: a generation begun
    /// between two of them would leave the first out of the window of the row's.
    #[inline]
    pub fn row_syms(&mut self, words: &mut [u64], out: &mut impl Out) {
        if words.iter().all(|it| *it == 0) {
            return;
        }
        self.roll(out);
        if usize::from(self.next) + words.len() > usize::from(self.index_max) + 1 {
            self.begin(out);
        }
        for word in words {
            *word = u64::from(self.row_sym(*word, out));
        }
    }

    #[inline]
    fn row_sym(&mut self, token: Token, out: &mut impl Out) -> u32 {
        if token == 0 {
            return 0;
        }
        if token & KEPT_TOKEN != 0 {
            return KEPT_SYM | (token & ((1 << 30) - 1)) as u32;
        }
        let Some((ring, generation, index)) = parts(token) else { return 0 };
        let own = ring == self.ring
            && generation <= self.generation
            && self.generation - generation < WINDOW;
        if own || self.plant == Plant::NoLink {
            return local(generation, index);
        }
        if let Some(&at) = self.links.get(&token) {
            return local(self.generation, at);
        }
        self.link(token, out)
    }

    #[cold]
    fn link(&mut self, token: Token, out: &mut impl Out) -> u32 {
        // [`Local::row_syms`] made room for a row's links; past it (a row of more tokens than a
        // generation's indexes, which only the DST's limits allow) the name is lost, counted.
        if self.next > self.index_max {
            out.counted(Count::Lost);
            return 0;
        }
        let at = self.next;
        self.next += 1;
        out.put([token, 0, 0, 0, KIND_PLOT << 62 | u64::from(at) << 8 | LINK_ROW, 0, 0, 0]);
        out.counted(Count::Linked);
        // alloc: once per token linked a generation, dropped with the generation
        self.links.insert(token, at);
        local(self.generation, at)
    }
}

/// `text` cut to [`TEXT_MAX`] bytes at a character's boundary, and whether it was.
fn cut(text: &str) -> (&str, bool) {
    if text.len() <= TEXT_MAX {
        return (text, false);
    }
    let mut end = TEXT_MAX;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// Writes the definition of `index` as `bytes`: a [`NAME_ROW`] and the [`NAME_MORE_ROW`]s the
/// rest needs, in a row of the ring each, one after another.
fn define(index: u16, bytes: &[u8], out: &mut impl Out) {
    let mut parts = bytes.chunks(ROW_TEXT);
    let first = parts.next().unwrap_or_default();
    let tag = KIND_PLOT << 62 | (bytes.len() as u64) << 20 | u64::from(index) << 8 | NAME_ROW;
    out.put(text_row(tag, first));
    for (part, chunk) in parts.enumerate() {
        let tag = KIND_PLOT << 62 | (part as u64 + 1) << 20 | u64::from(index) << 8 | NAME_MORE_ROW;
        out.put(text_row(tag, chunk));
    }
}

fn text_row(tag: u64, bytes: &[u8]) -> [u64; WORDS] {
    let mut row = [0; WORDS];
    row[4] = tag;
    for (at, chunk) in TEXT_WORDS.iter().zip(bytes.chunks(8)) {
        let mut word = [0; 8];
        word[..chunk.len()].copy_from_slice(chunk);
        row[*at] = u64::from_le_bytes(word);
    }
    row
}

fn row_text(row: &[u64; WORDS], into: &mut Vec<u8>, len: usize) {
    for at in TEXT_WORDS {
        let take = len.saturating_sub(into.len()).min(8);
        if take == 0 {
            return;
        }
        into.extend_from_slice(&row[at].to_le_bytes()[..take]);
    }
}

// --- The readers ------------------------------------------------------------------------

/// What an index of a generation names.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Def {
    Text(Arc<str>),
    /// A name of another ring or generation, by its full token.
    Link(Token),
}

/// One generation of a ring as a reader holds it: where it began, how far it was read with no
/// row missed, and its definitions by index.
struct Gen {
    generation: u32,
    from: u64,
    /// The index after the last row of it read with every row from `from` read: a row past it
    /// may be of a generation whose [`GEN_ROW`] was written over unread.
    to: u64,
    defs: Vec<Option<Def>>,
}

/// A definition whose rows are being read: its index, its length, the part next, its bytes.
struct Building {
    index: u16,
    len: usize,
    next: u64,
    bytes: Vec<u8>,
}

/// What a reader holds of one ring.
#[derive(Default)]
struct Stream {
    /// The index of the next row to read.
    to: u64,
    /// The generations held, oldest first, at most [`RETAIN`].
    gens: Vec<Gen>,
    building: Option<Building>,
}

/// Where a [`Streams`] reads rows from: the driver's rings, or nothing (a postmortem, which
/// [`Streams::feed`]s every row first).
pub trait Source {
    /// The names rows of ring `ring` from index `from` that are whole now, each with its index,
    /// into `into`; and the first index read and the one after the last: every row between
    /// them was read, and those before the first (past `from`) were written over unread.
    fn names_rows(&mut self, ring: u16, from: u64, into: &mut Vec<(u64, [u64; WORDS])>) -> (u64, u64);
}

/// A [`Source`] with no rows: every row was fed already.
pub struct Fed;

impl Source for Fed {
    fn names_rows(&mut self, _: u16, from: u64, _: &mut Vec<(u64, [u64; WORDS])>) -> (u64, u64) {
        (from, from)
    }
}

/// What a row sym or a token names, as the definitions held say.
enum Found {
    Text(Arc<str>),
    Link(Token),
    Forgotten,
}

/// What readers hold of every ring's names: the readers' side of [`Local`], fed each ring's
/// rows in order ([`Streams::catch_up`], [`Streams::feed`]), which resolves the ids rows hold.
#[derive(Default)]
pub struct Streams {
    rings: Vec<Stream>,
    /// Rows read here, reused.
    scratch: Vec<(u64, [u64; WORDS])>,
    plant: Plant,
    /// Texts of names open spans hold ([`Streams::hold`]), and the read they were last held at:
    /// a span open past [`RETAIN`] generations of its ring still reads its names as it closes.
    held: HashMap<Token, (Arc<str>, u64), Global>,
    /// Reads [`Streams::hold`] was told of.
    reads: u64,
    /// Ids no definition held resolves: written over unread, of a generation let go, or after
    /// rows the reader missed.
    pub forgotten: u64,
}

impl Streams {
    pub fn plant(&mut self, plant: Plant) {
        self.plant = plant;
    }

    fn stream(&mut self, ring: u16) -> &mut Stream {
        let at = usize::from(ring);
        if self.rings.len() <= at {
            // alloc: once per ring a reader reads, a stream each
            self.rings.resize_with(at + 1, Stream::default);
        }
        &mut self.rings[at]
    }

    /// Reads the names rows ring `ring` wrote since this last did.
    pub fn catch_up(&mut self, ring: u16, source: &mut impl Source) {
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        let from = self.stream(ring).to;
        let (first, head) = source.names_rows(ring, from, &mut scratch);
        self.feed(ring, first, head, scratch.iter().map(|(i, row)| (*i, row)));
        self.scratch = scratch;
    }

    /// Takes ring `ring`'s rows from `first` to `head`, every row between them read: `rows`,
    /// which hold every names row of them (and may hold others), in order. Rows before `first`
    /// not read yet were missed.
    pub fn feed<'a>(
        &mut self,
        ring: u16,
        first: u64,
        head: u64,
        rows: impl IntoIterator<Item = (u64, &'a [u64; WORDS])>,
    ) {
        let stream = self.stream(ring);
        if first > stream.to {
            // Rows missed: the generation held is known only up to them, and a definition
            // being read is torn.
            stream.building = None;
            stream.to = first;
        }
        for (i, row) in rows {
            if i >= stream.to && is_names_row(row) {
                stream.take(i, row);
            }
        }
        stream.advance(head);
    }

    /// What `sym` names, a row's own id (not a kept name's) of the row at index `at` of ring
    /// `ring`, a link followed (catching its ring up through `source`); `None`, counted as
    /// forgotten, when no definition held says.
    pub fn resolve(
        &mut self,
        ring: u16,
        at: u64,
        sym: u32,
        source: &mut impl Source,
    ) -> Option<Arc<str>> {
        let found = match self.lookup(ring, at, sym) {
            Found::Text(text) => Some(text),
            Found::Link(token) => self.text(token, source),
            Found::Forgotten => None,
        };
        if found.is_none() {
            self.forgotten += 1;
        }
        found
    }

    /// The text of `token`, a ring's name, its ring caught up through `source` when the
    /// definitions held don't say; `None` when none does. Kept names are the driver's.
    pub fn text(&mut self, token: Token, source: &mut impl Source) -> Option<Arc<str>> {
        if let Found::Text(text) = self.of_token(token) {
            return Some(text);
        }
        let (ring, ..) = parts(token)?;
        self.catch_up(ring, source);
        match self.of_token(token) {
            Found::Text(text) => Some(text),
            _ => None,
        }
    }

    fn lookup(&self, ring: u16, at: u64, sym: u32) -> Found {
        let Some(stream) = self.rings.get(usize::from(ring)) else { return Found::Forgotten };
        let (low, index) = (sym >> 12 & 0xff, usize::from((sym & 0xfff) as u16));
        let held = if self.plant == Plant::NoWindow {
            stream.gens.iter().rev().find(|it| it.generation & 0xff == low)
        } else {
            // The generation the row is in, then the one of its window with the sym's low byte.
            stream.gens.iter().rev().find(|it| it.from <= at && at < it.to).and_then(|now| {
                let generation = (0..WINDOW)
                    .map(|back| now.generation.wrapping_sub(back))
                    .find(|it| it & 0xff == low)?;
                stream.gens.iter().rev().find(|it| it.generation == generation)
            })
        };
        match held.and_then(|it| it.defs.get(index)).and_then(Option::as_ref) {
            // clone: a refcount
            Some(Def::Text(text)) => Found::Text(text.clone()),
            Some(Def::Link(token)) => Found::Link(*token),
            None => Found::Forgotten,
        }
    }

    fn of_token(&self, token: Token) -> Found {
        let Some((ring, generation, index)) = parts(token) else { return Found::Forgotten };
        let Some(stream) = self.rings.get(usize::from(ring)) else { return Found::Forgotten };
        let held = stream.gens.iter().rev().find(|it| it.generation == generation);
        match held.and_then(|it| it.defs.get(usize::from(index))).and_then(Option::as_ref) {
            Some(Def::Text(text)) => Found::Text(Arc::clone(text)),
            // A token is a text's: a link to a link is not one a writer writes.
            Some(Def::Link(_)) | None => match self.held.get(&token) {
                Some((text, _)) => Found::Text(Arc::clone(text)),
                None => Found::Forgotten,
            },
        }
    }

    /// The text of a hot sketch's key ([`hot_key`]) of a ring's name: the newest generation held
    /// with its low byte. Approximate as the sketches are: a key whose generation came round
    /// reads as the newer one's.
    pub fn hot_text(&self, key: u32) -> Option<Arc<str>> {
        let stream = self.rings.get((key >> 20) as usize)?;
        let (low, index) = (key >> 12 & 0xff, (key & 0xfff) as usize);
        let held = stream.gens.iter().rev().find(|it| it.generation & 0xff == low)?;
        match held.defs.get(index)?.as_ref()? {
            // clone: a refcount
            Def::Text(text) => Some(text.clone()),
            Def::Link(token) => match self.of_token(*token) {
                Found::Text(text) => Some(text),
                _ => None,
            },
        }
    }

    /// Keeps the text of each of `tokens`, the names the spans open now hold, while a read
    /// still resolves it, until the read after the last that was told of it: a name a long span
    /// took when it opened is written as a link when it closes, maybe hundreds of generations
    /// later, and reads back from here. Called once a read, before its rows are resolved.
    pub fn hold(&mut self, tokens: &[Token], source: &mut impl Source) {
        self.reads += 1;
        let reads = self.reads;
        for &token in tokens {
            if let Some(it) = self.held.get_mut(&token) {
                it.1 = reads;
                continue;
            }
            if let Some(text) = self.text(token, source) {
                // alloc: a name an open span holds, kept while it does
                self.held.insert(token, (text, reads));
            }
        }
        // Kept through the next read: a span that closed since this one wrote its row then.
        self.held.retain(|_, it| it.1 + 1 >= reads);
    }

    /// Forgets every ring, as a reader that starts again.
    pub fn clear(&mut self) {
        self.rings.clear();
        self.held.clear();
    }
}

impl Stream {
    /// Every row before `to` was read, none missed since the last: the newest generation held
    /// runs to it, if it ran to the rows before.
    fn advance(&mut self, to: u64) {
        if to <= self.to {
            return;
        }
        if let Some(held) = self.gens.last_mut()
            && held.to == self.to
        {
            held.to = to;
        }
        self.to = to;
    }

    /// Takes the names row `row` at index `i`, every row before it read.
    fn take(&mut self, i: u64, row: &[u64; WORDS]) {
        self.advance(i);
        let tag = row[4];
        let index = (tag >> 8 & 0xfff) as u16;
        match tag & 0xff {
            GEN_ROW => {
                self.building = None;
                if self.gens.len() == RETAIN {
                    self.gens.remove(0);
                }
                // alloc: a generation's definitions, a reader's, kept RETAIN generations a ring
                let held = Gen { generation: row[0] as u32, from: i, to: i + 1, defs: Vec::new() };
                self.gens.push(held);
                self.to = i + 1;
                return;
            }
            NAME_ROW => {
                let len = (tag >> 20 & 0xffff) as usize;
                // alloc: a definition's bytes while its rows are read
                let mut bytes = Vec::with_capacity(len);
                row_text(row, &mut bytes, len);
                self.building = Some(Building { index, len, next: 1, bytes });
                self.finish();
            }
            NAME_MORE_ROW => {
                // The part's bits only: the tag's top bits are the row's kind.
                let part = tag >> 20 & 0xffff;
                match &mut self.building {
                    Some(building) if building.index == index && building.next == part => {
                        building.next += 1;
                        let len = building.len;
                        row_text(row, &mut building.bytes, len);
                        self.finish();
                    }
                    _ => self.building = None,
                }
            }
            LINK_ROW => {
                self.building = None;
                self.define(index, Def::Link(row[0]));
            }
            _ => {}
        }
        self.advance(i + 1);
    }

    /// Ends the definition being read if it is whole.
    fn finish(&mut self) {
        let Some(building) = &self.building else { return };
        if building.bytes.len() < building.len {
            return;
        }
        let Some(building) = self.building.take() else { return };
        // alloc: a name a reader holds, kept with its generation
        let text: Arc<str> = String::from_utf8_lossy(&building.bytes).into();
        self.define(building.index, Def::Text(text));
    }

    /// Defines `index` of the generation the row just read is in: none when that generation's
    /// start was missed.
    fn define(&mut self, index: u16, def: Def) {
        let to = self.to;
        let Some(held) = self.gens.last_mut().filter(|it| it.to == to) else { return };
        let at = usize::from(index);
        if held.defs.len() <= at {
            held.defs.resize(at + 1, None);
        }
        held.defs[at] = Some(def);
    }
}

#[cfg(test)]
mod tests;
