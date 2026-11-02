//! Proc-macro expansions kept in the store (docs/v2/plan/lanes/usercode.md), so that no
//! macro is expanded twice for the same inputs, on this machine or, through the store's
//! remote, another.
//!
//! An expansion is a run of user code ([`blaze_usercode::RunKey`]): its code is the content ID
//! of the macro's library, and what it is given ([`given`]) is the macro's name and kind, its
//! input and attribute tokens, the variables of the crate it expands for, the directory it
//! runs in, the toolchain and the proc-macro ABI. Spans are given relative: each as its byte
//! range from the item it is anchored to and which of the call's anchors and syntax contexts
//! it has, numbered as they first appear, so the next call with the same input in another
//! database or revision has the same key. The value is the output with its spans written the
//! same way ([`written`]). What else it read (files, variables of the shell, a listing) the
//! host that ran it traced, and the result is found and kept by exactly those through
//! [`blaze_usercode::index`]. `blaze-macro-eval`, which runs macro code, keys, finds and puts
//! its expansions with what is here ([`keeping`]).
//!
//! Not kept: an expansion that asked the database about a span (its text, file or line),
//! since the key does not say what it was told; one whose output has an anchor or a context
//! its input has not, which the next call could not be given back; one that read what can't
//! be traced (the clock, randomness); and one that failed. `BLAZE_PROC_MACRO_CACHE=check`
//! expands every call it finds kept anyway, and an output other than the kept one is an error
//! naming the macro.
//!
//! An expansion runs inside a query on a worker, many at once, so it writes nothing shared: it
//! reads the store, and what it keeps it leaves in its worker's own buffer for the store's
//! owner ([`puts`]); its statistics are its worker's ([`tally`]).

pub mod env;
pub mod puts;
pub mod tally;
pub mod verdict;

use std::{
    cell::Cell,
    sync::{Arc, OnceLock},
};

use base_db::Env;
use blaze_cas::Reader;
use blaze_ids::ContentId;
use blaze_usercode::{
    Given, RunKind,
    index::{self, FindError},
    inputs,
    trace::Inputs,
};
use hir_expand::proc_macro::ProcMacroKind;
use span::{Span, SpanAnchor, SyntaxContext};

pub use self::env::{Environment, SET};
pub use self::puts::{Published, Put, publish, put, take_puts};
use crate::bridge::wire::{self, Parts};

/// The version of what a macro is given and gives back that expansions are kept under: a
/// change to either bumps it, so nothing kept before is found.
const ABI: u32 = 8;

/// What the content ID of a macro's library is made with.
pub const LIBRARY: &str = "blaze v2 proc-macro library 1";

/// The variable that says to check kept expansions.
pub const CACHE: &str = "BLAZE_PROC_MACRO_CACHE";
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// What is found is used.
    Keep,
    /// What is found is expanded again and compared.
    Check,
}

impl Mode {
    /// The mode `value` of [`CACHE`] says.
    pub fn of(value: Option<&str>) -> Result<Mode, String> {
        match value {
            None => Ok(Mode::Keep),
            Some("check") => Ok(Mode::Check),
            Some(other) => Err(format!( // alloc: error path
                "{CACHE}={other:?} is not a mode: every expansion that can be kept is, and \
                 `check` expands what was kept again and compares"
            )),
        }
    }
}

/// Where expansions are kept, and what they are keyed by besides their call.
pub struct Expansions {
    /// Read to find an expansion, with no lock, under a guard taken for that read alone;
    /// never written by one: what an expansion keeps goes to its worker's buffer
    /// ([`take_puts`]), which the store's owner [`publish`]es.
    pub store: Reader,
    /// The toolchain whose proc-macro ABI and library the macros are built with.
    pub toolchain: ContentId,
    /// What the user's shell passes through to the expansions, which they are checked against.
    pub environment: Arc<Environment>,
}

struct Installed {
    expansions: Expansions,
    mode: Mode,
}

static INSTALLED: OnceLock<Installed> = OnceLock::new();

/// The inputs of an expansion as they are now: the machine's files, and the environment it is
/// sent ([`Environment::seen`]). A variable a shape names is looked up where it is, so a hit
/// allocates nothing; the whole environment is worked out only for a run that read all of it.
struct Seen<'a> {
    passed: &'a [(String, String)],
    call: &'a Env,
    made: Option<inputs::Env>,
    /// What file, directory and listing reads are asked through: no variable is read there.
    none: inputs::Env,
}

impl<'a> Seen<'a> {
    /// The value [`Environment::seen`] gives `name`: the call's, over what is passed, over
    /// what blaze sets for every expansion ([`SET`]). A name that is not UTF-8 is none of them.
    fn value(&self, name: &std::ffi::OsStr) -> Option<&'a str> {
        let name = name.to_str()?;
        if let Some((_, value)) = self.call.iter().find(|it| it.0 == name) {
            return Some(value);
        }
        if let Some((_, value)) = self.passed.iter().rev().find(|it| it.0 == name) {
            return Some(value.as_str());
        }
        SET.iter().find(|it| it.0 == name).map(|it| it.1)
    }
}

impl Inputs for Seen<'_> {
    type Error = inputs::Unreadable;
    fn entry(&mut self, path: &std::path::Path) -> Result<blaze_usercode::trace::Entry, Self::Error> {
        inputs::Now { files: inputs::Files::shared(), env: &self.none }.entry(path)
    }

    fn file(&mut self, path: &std::path::Path) -> Result<Option<[u8; 32]>, Self::Error> {
        inputs::Now { files: inputs::Files::shared(), env: &self.none }.file(path)
    }

    fn dir(&mut self, path: &std::path::Path) -> Result<Option<[u8; 32]>, Self::Error> {
        inputs::Now { files: inputs::Files::shared(), env: &self.none }.dir(path)
    }

    fn env(&mut self, name: &std::ffi::OsStr) -> Option<&std::ffi::OsStr> {
        self.value(name).map(std::ffi::OsStr::new)
    }

    fn env_all(&mut self) -> &[(std::ffi::OsString, std::ffi::OsString)] {
        let (passed, call) = (self.passed, self.call);
        // alloc: a run that read the whole environment: it is sorted once per call, as its key needs.
        self.made.get_or_insert_with(|| inputs::Env::new(Environment::seen(passed, call.iter()))).vars()
    }

    fn names(
        &mut self,
        path: &std::path::Path,
        out: &mut blaze_usercode::trace::Names<'_>,
    ) -> Result<bool, Self::Error> {
        inputs::Now { files: inputs::Files::shared(), env: &self.none }.names(path, out)
    }
}

/// What `check` compares a kept expansion by, read in place: its bytes' hash.
pub fn checked(bytes: &[u8]) -> ContentId {
    ContentId::of("blaze v2 proc-macro checked expansion 1", bytes)
}

/// Keeps every proc-macro expansion from here on in `expansions`. Says why not when [`CACHE`]
/// has a value that is not a mode, or it was installed before.
pub fn install(expansions: Expansions) -> Result<(), String> {
    let var = |name: &str| match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} is not UTF-8")), // alloc: error path
    };
    let mode = Mode::of(var(CACHE)?.as_deref())?;
    // alloc: error path
    INSTALLED.set(Installed { expansions, mode }).map_err(|_| "proc-macro expansions were installed twice".to_owned())
}

/// Where blaze-macro-eval keeps the expansions it keys, and how.
#[derive(Clone, Copy)]
pub struct Keeping {
    pub expansions: &'static Expansions,
    pub mode: Mode,
}

/// Where expansions are kept: `None` before [`install`], as in a bench, which keeps nothing.
pub fn keeping() -> Option<Keeping> {
    INSTALLED.get().map(|it| Keeping { expansions: &it.expansions, mode: it.mode })
}

/// The hash of the values the names of `shape` (an `ExpansionReads::Traced` shape) have now,
/// for an expansion given the variables `env`: what `hir_def::deps` files a crate's image by.
/// The same hash the expansion reported when its values were these. Says why not before
/// [`install`]: no expansion was kept, so no shape is of one.
pub fn reads_now(env: &Env, shape: &[u8]) -> Result<ContentId, String> {
    let Some(installed) = INSTALLED.get() else {
        return Err("a proc-macro expansion's reads were asked before expansions were installed".to_owned()); // alloc: error path
    };
    reads_in(installed.expansions.environment.passed(), env, shape)
}

/// [`reads_now`] for expansions passed `passed`.
fn reads_in(passed: &[(String, String)], env: &Env, shape: &[u8]) -> Result<ContentId, String> {
    let mut now = Seen { passed, call: env, made: None, none: inputs::Env::default() };
    match index::values(shape, &mut now) {
        Ok(Some(values)) => Ok(values),
        // A directory that is no longer one: no value it had is this.
        Ok(None) => Ok(ContentId::of("blaze v2 expansion reads gone", shape)),
        Err(FindError::NotAShape { why, .. }) => Err(format!("a shape that does not read: {why}")), // alloc: error path
        Err(FindError::Inputs(unreadable)) => Err(format!("{unreadable}")), // alloc: error path
        Err(FindError::Corrupt(corrupt)) => Err(format!("{corrupt}")), // alloc: error path
    }
}

/// The anchors and syntax contexts of a call's span parts, in the order they first appear: what
/// its output is written against. Its room goes back to the thread's shelf when it is dropped,
/// so the next call's takes no new memory.
#[derive(Default, Debug)]
pub struct Spans {
    anchors: Vec<SpanAnchor>,
    contexts: Vec<SyntaxContext>,
}

thread_local! {
    /// The room of the last [`Spans`] dropped on this thread.
    // alloc: none, an empty shelf; what it keeps is grown by the calls.
    static SHELF: Cell<Option<(Vec<SpanAnchor>, Vec<SyntaxContext>)>> = const { Cell::new(None) };
    /// What a key, a kept value and its reading reuse: taken for one of them, so a call made
    /// inside another (an expansion answering a sub-request) gets fresh ones.
    static SCRATCH: Cell<Option<Box<Scratch>>> = const { Cell::new(None) };
}

impl Spans {
    fn shelved() -> Spans {
        let (mut anchors, mut contexts) = SHELF.take().unwrap_or_default();
        anchors.clear();
        contexts.clear();
        Spans { anchors, contexts }
    }
}

impl Drop for Spans {
    fn drop(&mut self) {
        let room = (std::mem::take(&mut self.anchors), std::mem::take(&mut self.contexts));
        // A thread that is ending keeps nothing.
        _ = SHELF.try_with(|shelf| shelf.set(Some(room)));
    }
}

/// A call's span parts, its trees' relative columns and their shapes, and the room a kept tree
/// off a word is copied into: each as long as the longest it held.
#[derive(Default)]
struct Scratch {
    parts: Parts,
    input: Vec<u8>,
    attrs: Vec<u8>,
    shapes: Vec<u8>,
    room: Vec<u8>,
}

fn with_scratch<R>(f: impl FnOnce(&mut Scratch) -> R) -> R {
    // alloc: the first call on a thread, or one inside another, makes its room.
    let mut scratch = SCRATCH.take().unwrap_or_default();
    let done = f(&mut scratch);
    SCRATCH.set(Some(scratch));
    done
}

/// The number of `it` among `all`, in the order first seen: a new one is pushed. A scan: a
/// call's anchors and contexts are few.
pub fn number<T: PartialEq>(all: &mut Vec<T>, it: T) -> u32 {
    let at = all.iter().position(|known| *known == it).unwrap_or_else(|| {
        all.push(it);
        all.len() - 1
    });
    at as u32
}

/// One call of a macro, as its key is made of it.
pub struct Call<'a> {
    pub name: &'a str,
    pub kind: ProcMacroKind,
    pub subtree: &'a tt::TopSubtree,
    pub attrs: Option<&'a tt::TopSubtree>,
    pub env: &'a Env,
    pub def_site: Span,
    pub call_site: Span,
    pub mixed_site: Span,
    pub current_dir: &'a str,
}

fn kind_byte(kind: ProcMacroKind) -> u8 {
    match kind {
        ProcMacroKind::CustomDerive => 0,
        ProcMacroKind::Attr => 1,
        ProcMacroKind::Bang => 2,
    }
}

/// What `call` gives the macro, for its [`RunKey::given`] with the toolchain `toolchain`, and
/// the anchors and contexts its output is written against.
///
/// The trees are hashed as their own columns made relative ([`wire::relative`]: each span
/// part an index of the call's, the def, call and mixed sites' first), and the parts as the
/// numbers of their anchors and contexts in the order they first appear, then each site's
/// range and part, so the same tokens in another file or item have the same key. One pass over
/// each tree's columns, into buffers kept per thread: a key allocates nothing.
///
/// [`RunKey::given`]: blaze_usercode::RunKey::given
pub fn given(toolchain: &ContentId, call: &Call<'_>) -> (Given, Spans) {
    let mut spans = Spans::shelved();
    let mut given = Given::new(RunKind::ProcMacro);
    given
        .part("name", call.name.as_bytes())
        .part("kind", &[kind_byte(call.kind)])
        .part("abi", &ABI.to_le_bytes())
        .id("toolchain", toolchain)
        .part("dir", call.current_dir.as_bytes());
    with_scratch(|scratch| {
        let Scratch { parts, input, attrs, shapes, .. } = scratch;
        parts.clear();
        input.clear();
        attrs.clear();
        shapes.clear();
        let sites = [call.def_site, call.call_site, call.mixed_site].map(|site| {
            let part = parts.part(site.anchor, site.ctx);
            [site.range.start().into(), site.range.len().into(), part]
        });
        wire::relative(call.subtree, parts, input);
        if let Some(it) = call.attrs {
            wire::relative(it, parts, attrs);
        }
        for &(anchor, ctx) in parts.all() {
            for it in [number(&mut spans.anchors, anchor), number(&mut spans.contexts, ctx)] {
                shapes.extend_from_slice(&it.to_le_bytes());
            }
        }
        for it in sites.as_flattened() {
            shapes.extend_from_slice(&it.to_le_bytes());
        }
        let attrs: &[u8] = if call.attrs.is_some() { attrs } else { b"-" };
        given
            .part("input columns", input.as_slice())
            .part("attrs columns", attrs)
            .part("span parts", shapes.as_slice());
    });
    given.env_keyed(call.env.iter().map(|(name, value)| (name.as_bytes(), value.as_bytes())));
    (given, spans)
}

/// The output as it is kept: its columns made relative ([`wire::relative`]), each span part an
/// index of the output's own, then the numbers of each part's anchor and context among the
/// call's `spans` (two words), then how many parts there are. `None` when it has an anchor or
/// a context the call has not. The tree is first, so the value's start is its start.
pub fn written(output: &tt::TopSubtree, spans: &Spans) -> Option<Vec<u8>> {
    with_scratch(|scratch| {
        let parts = &mut scratch.parts;
        parts.clear();
        // alloc: the value kept, once as an expansion is kept.
        let mut value = Vec::new();
        wire::relative(output, parts, &mut value);
        for &(anchor, ctx) in parts.all() {
            let anchor = spans.anchors.iter().position(|it| *it == anchor)? as u32;
            let context = spans.contexts.iter().position(|it| *it == ctx)? as u32;
            for it in [anchor, context] {
                value.extend_from_slice(&it.to_le_bytes());
            }
        }
        value.extend_from_slice(&(parts.len() as u32).to_le_bytes());
        Some(value)
    })
}

/// What [`written`] wrote, against the call's `spans`: one checked pass over the columns into
/// the tree, its one allocation, with a tree off a word copied into the thread's room.
pub fn read(bytes: &[u8], spans: &Spans) -> Result<tt::TopSubtree, String> {
    // alloc: error path.
    let cut = || "not a kept expansion: its table of span parts is cut short".to_owned();
    let (rest, count) = bytes.split_last_chunk::<4>().ok_or_else(cut)?;
    let table = (u32::from_le_bytes(*count) as usize)
        .checked_mul(8)
        .filter(|&len| len <= rest.len())
        .ok_or_else(cut)?;
    let (tree, table) = rest.split_at(rest.len() - table);
    with_scratch(|scratch| {
        let Scratch { parts, room, .. } = scratch;
        parts.clear();
        for (at, row) in table.as_chunks::<8>().0.iter().enumerate() {
            let field = |i: usize| u32::from_le_bytes(row[i..i + 4].try_into().expect("four bytes"));
            let anchor = spans.anchors.get(field(0) as usize);
            let ctx = spans.contexts.get(field(4) as usize);
            let Some((&anchor, &ctx)) = anchor.zip(ctx) else {
                // alloc: error path.
                return Err(format!("a kept expansion's span part {at} is of an anchor or context its call has not"));
            };
            if parts.part(anchor, ctx) as usize != at {
                // alloc: error path.
                return Err(format!("a kept expansion's span part {at} is an earlier one again"));
            }
        }
        // alloc: error path.
        wire::read_in(tree, parts, room).map_err(|error| format!("a kept expansion: {error}"))
    })
}

/// The first expansion that failed for a reason that is not the macro's own (the store
/// refused what it kept): what is said of it.
static SERVER_FAILED: OnceLock<String> = OnceLock::new();

fn system(said: String) {
    if SERVER_FAILED.set(said.clone()).is_ok() { // clone: error path, said once and kept
        eprintln!("proc macros: {said}");
    }
}

/// What is said of the first expansion in this process that failed for a reason that is not
/// the macro's own, when one did. What is built from it is not the program: a build fails
/// with this, at the point it is first seen (no-fallback).
pub fn server_failed() -> Option<&'static str> {
    SERVER_FAILED.get().map(String::as_str)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod budget {
    use blaze_cas::{Config, Store};
    use blaze_core::{
        FileName, Traced, run_until_idle,
        sim::{Faults, Sim},
    };
    use blaze_usercode::{RunKey, index::Found};

    use super::*;

    /// A call whose expansion is kept is found through [`Seen`], each variable its shape names
    /// looked up where it is and each file stamped, with no allocation: the hit is read in
    /// place.
    #[cfg(unix)]
    #[test]
    fn a_kept_expansion_is_found_with_no_allocation() {
        let mut sim = Sim::new(5, Faults::default());
        let mut effects = Vec::new();
        let config = Config { dir: FileName::new("store").unwrap(), layers: Vec::new(), tags: 1 << 48 };
        let mut store = Store::open(config, &mut effects);
        run_until_idle(&mut sim, &mut effects, |completion, effects| {
            store.complete(completion, effects);
        });
        assert!(store.ready());
        let lib = std::env::temp_dir().join(format!("blaze-kept-budget-{}", std::process::id()));
        std::fs::write(&lib, b"a macro's library").unwrap();
        let hash = inputs::hash_reader(b"a macro's library".as_slice()).unwrap();
        let run = RunKey { code: ContentId::of("blaze v2 test 1", b"macro"), given: ContentId::default() };
        let traced = Traced {
            files: vec![(lib.clone(), Some(hash))],
            env: vec![
                ("CARGO_PKG_NAME".into(), Some("a".into())),
                ("PATH".into(), Some("/bin".into())),
                ("UNSET".into(), None),
            ],
            ..Traced::default()
        };
        index::keep(&mut store, &run, &traced, b"expansion".to_vec()).unwrap();
        let passed = [("PATH".to_owned(), "/bin".to_owned())];
        let call: Env = [("CARGO_PKG_NAME".to_owned(), "a".to_owned())].into_iter().collect();
        let mut seen = Seen { passed: &passed, call: &call, made: None, none: inputs::Env::default() };
        let reader = store.reader();
        let guard = blaze_cas::enter();
        let values = index::VALUES.traced(&traced);
        let mut find = || {
            matches!(
                index::find_reads(&reader, &guard, &run, &mut seen),
                Ok(Found::Hit { value: b"expansion", values: Some(found), .. }) if found == values
            )
        };
        assert!(find());
        assert!(blaze_alloc_count::assert_at_most(0, "a kept expansion found through Seen", &mut find));
        drop(guard);
        _ = std::fs::remove_file(&lib);
    }

    /// What an expansion that read a file reports is what [`reads_in`] works out of its shape
    /// while the file is as it was, found again on a hit, and not once the file has changed.
    #[cfg(unix)]
    #[test]
    fn the_reads_an_expansion_reports_are_worked_out_again_until_a_file_changes() {
        let mut sim = Sim::new(5, Faults::default());
        let mut effects = Vec::new();
        let config = Config { dir: FileName::new("store").unwrap(), layers: Vec::new(), tags: 1 << 48 };
        let mut store = Store::open(config, &mut effects);
        run_until_idle(&mut sim, &mut effects, |completion, effects| {
            store.complete(completion, effects);
        });
        assert!(store.ready());
        let read = std::env::temp_dir().join(format!("blaze-kept-reads-{}", std::process::id()));
        std::fs::write(&read, b"included").unwrap();
        let hash = inputs::hash_reader(b"included".as_slice()).unwrap();
        let traced = Traced {
            files: vec![(read.clone(), Some(hash))],
            env: vec![("CARGO_PKG_NAME".into(), Some("a".into())), ("UNSET".into(), None)],
            ..Traced::default()
        };
        let reported = index::VALUES.traced(&traced);
        let shape = index::Shape::of(&traced).encode();
        let call: Env = [("CARGO_PKG_NAME".to_owned(), "a".to_owned())].into_iter().collect();
        assert_eq!(reads_in(&[], &call, &shape), Ok(reported));

        let run = RunKey { code: ContentId::of("blaze v2 test 1", b"reads"), given: ContentId::default() };
        index::keep(&mut store, &run, &traced, b"expansion".to_vec()).unwrap();
        let reader = store.reader();
        let guard = blaze_cas::enter();
        let mut seen = Seen { passed: &[], call: &call, made: None, none: inputs::Env::default() };
        match index::find_reads(&reader, &guard, &run, &mut seen) {
            Ok(Found::Hit { shape: found, values, .. }) => {
                assert_eq!(found, shape.as_slice());
                assert_eq!(values, Some(reported));
            }
            other => panic!("{other:?}"),
        }
        drop(guard);

        let other: Env = [("CARGO_PKG_NAME".to_owned(), "b".to_owned())].into_iter().collect();
        assert_ne!(reads_in(&[], &other, &shape), Ok(reported));
        std::fs::write(&read, b"included, and changed").unwrap();
        assert_ne!(reads_in(&[], &call, &shape), Ok(reported));
        _ = std::fs::remove_file(&read);
    }
}
