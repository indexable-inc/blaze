//! User code (docs/v2/plan/lanes/usercode.md section 6.5; #1450, #1480, #1296, #1297, #1443, #804).
//!
//! - A build script's or proc macro's run happens in a [`blaze_core::Sandbox`] that traces
//!   what it read ([`blaze_core::Traced`]). Its result is keyed by its own code hash and
//!   exactly those inputs ([`RunKey`]), not by "the package changed". A run that read what
//!   can't be traced ([`blaze_core::Untraceable`]) is keyed as cargo keys it.
//! - The daemon hashes and checks every output before the store keeps it.
//! - Tests: each test's reached functions are recorded by code hash ([`Reached`]); an edit
//!   reruns the tests that reach a changed hash.
//!
//! The parts: [`trace`] turns what a sandbox saw into a [`Traced`]; [`index`] finds a run's
//! result in the store by the shape of what it read and then by those inputs' values now;
//! [`inputs`] has what reads are checked against (a view and the machine); [`rule`] is cargo's
//! rule for what can't be traced; [`run`] is a run as a state machine, from the store or a
//! sandboxed spawn to a kept result; [`script`] and [`pool`] are build scripts and the warm
//! processes proc macros run in; [`pure`] keeps a build script proved pure under a key that
//! needs no compile (docs/v2/plan/lanes/pure-scripts/design.md); [`test`] selects and runs tests
//! (docs/v2/plan/lanes/usercode/notes.md).

#![feature(allocator_api, allocator_ext)]
#![forbid(unsafe_code)]

pub mod confine;
pub mod index;
pub mod inputs;
pub mod pool;
pub mod pure;
pub mod rule;
pub mod run;
pub mod script;
pub mod test;
pub mod trace;

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
};

use blaze_core::Traced;
use blaze_ids::{ContentId, Hasher};
use blaze_items::CodeHash;
use serde::{Deserialize, Serialize};

/// What a run of user code is: the code that runs, and what it was given.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RunKind {
    BuildScript,
    ProcMacro,
    Test,
}

impl RunKind {
    fn byte(self) -> u8 {
        match self {
            RunKind::BuildScript => 0,
            RunKind::ProcMacro => 1,
            RunKind::Test => 2,
        }
    }
}

/// The key a run's result is first looked up by: its code and what it was given (arguments,
/// the environment cargo gives it, a proc macro's input tokens). A result found under it is
/// used only if every input [`Traced`] of it is the same in the view ([`RunKey::traced`]).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct RunKey {
    pub code: ContentId,
    pub given: ContentId,
}

impl RunKey {
    pub const DOMAIN: &'static str = trace::os_domain!("blaze v2 run traced 2");

    /// The store's key of the result: this and the hash of what the run read.
    pub fn traced(&self, traced: &Traced) -> ContentId {
        trace::Keyer::new(self).traced(traced)
    }
}

/// What a run is given, hashed into [`RunKey::given`]: its kind, program arguments, the
/// variables blaze sets for it (cargo's, for a build script), its standard input and what
/// else the caller names. The environment the user's shell passes through is not given: what
/// of it the run reads is traced (#805).
#[derive(Debug)]
pub struct Given {
    kind: RunKind,
    args: Vec<OsString>,
    // BTreeMap: hashed in order into the cache key `RunKey::given`, and `vars` hands it out in order.
    env: BTreeMap<OsString, OsString>,
    /// The hash of the standard input: given bytes are hashed once, as they are given, and
    /// never copied.
    stdin: [u8; 32],
    /// Each part's hash by its name, sorted by name, in `parts[..len]`: hashed in order into
    /// `RunKey::given`.
    parts: [(&'static str, [u8; 32]); Given::PARTS],
    len: usize,
}

impl Given {
    pub const DOMAIN: &'static str = trace::os_domain!("blaze v2 run given 2");
    const PART: &'static str = "blaze v2 run given part 1";
    /// The most parts a run is given: they are kept inline, so a key is made with no
    /// allocation.
    pub const PARTS: usize = 16;

    pub fn new(kind: RunKind) -> Given {
        Given {
            kind,
            args: Vec::new(), // alloc: none, an empty list
            env: BTreeMap::new(),
            stdin: Hasher::new(Given::PART).finish().0,
            parts: [("", [0; 32]); Given::PARTS],
            len: 0,
        }
    }

    pub fn arg(&mut self, arg: impl Into<OsString>) -> &mut Given {
        self.args.push(arg.into());
        self
    }

    /// A variable blaze sets; set twice, the last value is the one the run sees.
    pub fn env(&mut self, name: impl Into<OsString>, value: impl Into<OsString>) -> &mut Given {
        self.env.insert(name.into(), value.into());
        self
    }

    pub fn stdin(&mut self, bytes: &[u8]) -> &mut Given {
        self.stdin = Hasher::new(Given::PART).update(bytes).finish().0;
        self
    }

    /// Something else the run is a function of, by a name of the caller's: a macro's input
    /// tokens, its kind and name, the target. Named twice, the last is kept.
    pub fn part(&mut self, name: &'static str, bytes: &[u8]) -> &mut Given {
        let hash = Hasher::new(Given::PART).update(bytes).finish().0;
        self.hashed(name, hash)
    }

    /// Variables the run is given that blaze does not set in its process (a proc macro's,
    /// passed to its server with each expansion): one part, hashed in name order as they are
    /// lent, with no copy. Named twice, the last value is kept. [`Given::sets`] and
    /// [`Given::vars`] don't name them.
    pub fn env_keyed<'a>(&mut self, vars: impl IntoIterator<Item = (&'a [u8], &'a [u8])>) -> &mut Given {
        let hash = blaze_mem::frame::task(blaze_mem::Tag::Usercode, |frame| {
            // A name's first sixteen bytes, big-endian and padded with zeros, order as its bytes
            // do, and most comparisons end there at one compare of two words; a tie goes to
            // the bytes. The sort is stable, so the last value of a name is the one kept.
            let order = |name: &[u8]| {
                let mut first = [0u8; 16];
                let len = name.len().min(16);
                first[..len].copy_from_slice(&name[..len]);
                u128::from_be_bytes(first)
            };
            let mut sorted: Vec<(u128, &[u8], &[u8]), &blaze_mem::Frame> = Vec::new_in(frame);
            sorted.extend(vars.into_iter().map(|(name, value)| (order(name), name, value)));
            sorted.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
            sorted.dedup_by(|later, first| {
                let same = later.1 == first.1;
                if same {
                    *first = *later;
                }
                same
            });
            let mut hasher = Hasher::new(Given::PART);
            hasher.update(&(sorted.len() as u64).to_le_bytes());
            let mut batched = Batched { hasher: &mut hasher, batch: [0; Batched::LEN], len: 0 };
            for (_, name, value) in &sorted {
                batched.part(name);
                batched.part(value);
            }
            batched.flush();
            hasher.finish().0
        });
        self.hashed("env keyed", hash)
    }

    fn hashed(&mut self, name: &'static str, hash: [u8; 32]) -> &mut Given {
        let len = self.len;
        match self.parts[..len].binary_search_by(|it| it.0.cmp(name)) {
            Ok(at) => self.parts[at].1 = hash,
            Err(at) => {
                assert!(len < Given::PARTS, "a run is given at most {} parts; `{name}` is one more", Given::PARTS);
                self.parts.copy_within(at..len, at + 1);
                self.parts[at] = (name, hash);
                self.len = len + 1;
            }
        }
        self
    }

    pub fn id(&mut self, name: &'static str, id: &ContentId) -> &mut Given {
        self.part(name, &id.0)
    }

    pub fn finish(&self) -> ContentId {
        let mut hasher = Hasher::new(Given::DOMAIN);
        hasher.update(&[self.kind.byte()]);
        hasher.update(&(self.args.len() as u64).to_le_bytes());
        for arg in &self.args {
            hasher.part(&trace::os_bytes(arg));
        }
        hasher.update(&(self.env.len() as u64).to_le_bytes());
        for (name, value) in &self.env {
            hasher.part(&trace::os_bytes(name)).part(&trace::os_bytes(value));
        }
        hasher.update(&self.stdin);
        hasher.update(&(self.len as u64).to_le_bytes());
        for (name, hash) in &self.parts[..self.len] {
            hasher.part(name.as_bytes()).update(hash);
        }
        hasher.finish()
    }

    /// Whether `name` is one blaze sets, so not one to trace.
    pub fn sets(&self, name: &OsStr) -> bool {
        self.env.contains_key(name)
    }

    pub fn vars(&self) -> impl Iterator<Item = (&OsString, &OsString)> {
        self.env.iter()
    }
}

/// Length-prefixed parts gathered for one update of `hasher`, as [`trace::Keyer`] gathers a
/// key's: BLAKE3 then hashes eight chunks at once, where an update per name and value compresses
/// one block at a time. The bytes hashed are [`Hasher::part`]'s, so the hash is the same.
struct Batched<'h> {
    hasher: &'h mut Hasher,
    batch: [u8; Batched::LEN],
    len: usize,
}

impl Batched<'_> {
    /// Eight BLAKE3 chunks: as many as AVX2 hashes at once.
    const LEN: usize = 8 * 1024;

    fn put(&mut self, bytes: &[u8]) {
        if self.len + bytes.len() > Batched::LEN {
            self.flush();
            if bytes.len() > Batched::LEN {
                self.hasher.update(bytes);
                return;
            }
        }
        self.batch[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }

    fn part(&mut self, bytes: &[u8]) {
        self.put(&(bytes.len() as u64).to_le_bytes());
        self.put(bytes);
    }

    fn flush(&mut self) {
        self.hasher.update(&self.batch[..self.len]);
        self.len = 0;
    }
}

/// The functions one test reached, by code hash.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
pub struct Reached {
    pub test: String,
    pub binary: ContentId,
    pub reached: Vec<CodeHash>,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn key() -> RunKey {
        RunKey { code: ContentId::of("blaze v2 test 1", b"script"), given: Given::new(RunKind::BuildScript).finish() }
    }

    #[test]
    fn a_traced_key_ignores_order_and_sees_every_value() {
        let a = Traced {
            files: vec![(PathBuf::from("/b"), Some([1; 32])), (PathBuf::from("/a"), None)],
            env: vec![("CC".into(), Some("cc".into()))],
            ..Traced::default()
        };
        let mut b = a.clone();
        b.files.reverse();
        assert_eq!(key().traced(&a), key().traced(&b));
        let mut c = a.clone();
        c.files[0].1 = Some([2; 32]);
        assert_ne!(key().traced(&a), key().traced(&c));
        let mut d = a.clone();
        d.env[0].1 = None;
        assert_ne!(key().traced(&a), key().traced(&d));
        let mut e = a.clone();
        e.untraceable.push(blaze_core::Untraceable::Clock);
        assert_ne!(key().traced(&a), key().traced(&e));
    }

    #[test]
    fn given_is_framed_and_kind_counts() {
        let mut one = Given::new(RunKind::BuildScript);
        one.arg("ab").arg("c");
        let mut two = Given::new(RunKind::BuildScript);
        two.arg("a").arg("bc");
        assert_ne!(one.finish(), two.finish());
        assert_ne!(Given::new(RunKind::ProcMacro).finish(), Given::new(RunKind::Test).finish());
        let mut env_one = Given::new(RunKind::BuildScript);
        env_one.env("A", "1").env("B", "2");
        let mut env_two = Given::new(RunKind::BuildScript);
        env_two.env("B", "2").env("A", "1");
        assert_eq!(env_one.finish(), env_two.finish());
    }

    #[test]
    fn keyed_variables_are_one_part_in_name_order_the_last_value_kept() {
        let keyed = |vars: &[(&str, &str)]| {
            let mut given = Given::new(RunKind::ProcMacro);
            given.env_keyed(vars.iter().map(|(name, value)| (name.as_bytes(), value.as_bytes())));
            given.finish()
        };
        assert_eq!(keyed(&[("A", "1"), ("B", "2")]), keyed(&[("B", "2"), ("A", "1")]));
        assert_eq!(keyed(&[("A", "0"), ("B", "2"), ("A", "1")]), keyed(&[("A", "1"), ("B", "2")]));
        assert_ne!(keyed(&[("AB", "")]), keyed(&[("A", "B")]));
        assert_ne!(keyed(&[("A", "1")]), keyed(&[]));
        let mut set = Given::new(RunKind::ProcMacro);
        set.env("A", "1");
        assert_ne!(set.finish(), keyed(&[("A", "1")]));
        let mut passed = Given::new(RunKind::ProcMacro);
        passed.env_keyed([(b"A".as_slice(), b"1".as_slice())]);
        assert!(!passed.sets(OsStr::new("A")));
    }

    /// The batched hash of keyed variables is the hash of one [`Hasher::part`] per name and
    /// value, across the batch's end and past its size.
    #[test]
    fn keyed_variables_batched_hash_as_one_part_each() {
        let long = vec![b'x'; Batched::LEN + 3];
        let edge = vec![b'y'; Batched::LEN - 20];
        let names: Vec<String> = (0..200).map(|it| format!("VAR_{it:03}")).collect();
        let mut vars: Vec<(&[u8], &[u8])> = names.iter().map(|it| (it.as_bytes(), b"value".as_slice())).collect();
        vars.push((b"WIDE".as_slice(), long.as_slice()));
        vars.push((b"ZEDGE".as_slice(), edge.as_slice()));
        vars.push((b"ZZ".as_slice(), b"".as_slice()));
        // Names that tie on their first sixteen bytes, one a prefix of others, and a short one
        // that ties with a zero byte after it: ordered as their bytes are.
        for name in [&b"CARGO_CFG_TARGET_OS"[..], b"CARGO_CFG_TARGET_ARCH", b"CARGO_CFG_TARGET_", b"A", b"A\0"] {
            vars.push((name, b"v".as_slice()));
        }
        vars.sort_by(|a, b| a.0.cmp(b.0));
        let mut by_piece = Hasher::new(Given::PART);
        by_piece.update(&(vars.len() as u64).to_le_bytes());
        for (name, value) in &vars {
            by_piece.part(name).part(value);
        }
        let mut expected = Given::new(RunKind::ProcMacro);
        expected.hashed("env keyed", by_piece.finish().0);
        let mut given = Given::new(RunKind::ProcMacro);
        given.env_keyed(vars.iter().rev().copied());
        assert_eq!(given.finish(), expected.finish());
    }

    #[test]
    fn parts_are_in_name_order_and_the_last_of_a_name_is_kept() {
        let mut one = Given::new(RunKind::ProcMacro);
        one.part("b", b"2").part("a", b"0").part("c", b"3").part("a", b"1");
        let mut two = Given::new(RunKind::ProcMacro);
        two.part("c", b"3").part("a", b"1").part("b", b"2");
        assert_eq!(one.finish(), two.finish());
        two.part("d", b"");
        assert_ne!(one.finish(), two.finish());
    }

    /// A proc macro's key, as `proc_macro_api::kept` makes one for each expansion it looks up.
    fn macro_given() -> Given {
        let vars = [("CARGO_PKG_NAME", "a"), ("A", "1"), ("CARGO_PKG_NAME", "b")];
        let mut given = Given::new(RunKind::ProcMacro);
        given.part("name", b"derive").part("kind", &[0]).part("input", b"tokens").part("dir", b"/w");
        given.env_keyed(vars.iter().map(|(name, value)| (name.as_bytes(), value.as_bytes())));
        given
    }

    #[test]
    fn a_macros_given_and_its_finish_allocate_nothing() {
        let warm = macro_given().finish();
        let again = blaze_alloc_count::assert_at_most(0, "a proc macro's given, made and finished", || {
            macro_given().finish()
        });
        assert_eq!(warm, again);
        let mut script = Given::new(RunKind::BuildScript);
        script.arg("build").env("OUT_DIR", "/o").stdin(b"").part("target", b"x86_64");
        blaze_alloc_count::assert_at_most(0, "a build script's given, finished", || script.finish());
    }

    #[test]
    fn a_traced_key_allocates_nothing_normal_or_not() {
        let normal = Traced {
            files: vec![(PathBuf::from("/a"), None), (PathBuf::from("/b"), Some([1; 32]))],
            dirs: vec![(PathBuf::from("/d"), [2; 32])],
            env: vec![("CC".into(), Some("cc".into()))],
            untraceable: vec![blaze_core::Untraceable::Clock],
        };
        assert!(trace::normal(&normal));
        let mut unsorted = normal.clone();
        unsorted.files.reverse();
        unsorted.files.push(unsorted.files[0].clone());
        let key = key();
        let warm = (key.traced(&normal), key.traced(&unsorted));
        let normal_key =
            blaze_alloc_count::assert_at_most(0, "the key of a normal Traced", || key.traced(&normal));
        let unsorted_key = blaze_alloc_count::assert_at_most(0, "the key of a Traced not normal", || {
            key.traced(&unsorted)
        });
        assert_eq!(warm, (normal_key, unsorted_key));
        assert_eq!(normal_key, unsorted_key);
    }
}
