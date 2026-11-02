//! A journal built through `blaze_vfs::Vfs` from transactions the input describes (commits,
//! some against older roots, heads moved and removed, files whose bytes are another journal,
//! collections), on a simulated disk whose appends, syncs and rewrites complete or fail where
//! the input says (a failed append leaving part of its bytes, a failed sync dropping what was
//! not synced). Then a crash: the append in flight torn, and the file cut or damaged, where the
//! input says. It is recovered with `Vfs::recover` and checked against the heads after each
//! transaction; the recovered VFS is then used further, settled and recovered again.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - recovery never panics, and asks for at most `blaze_fuzz::allowed(len)` bytes at once;
//! - a ticket is made durable once, only after every byte of its transaction is synced, and
//!   settling makes every ticket durable;
//! - a journal torn or cut anywhere after its last rewrite recovers to the heads after some
//!   prefix of the transactions, one holding at least every transaction whose records are all
//!   before the cut: the whole sequence when every transaction's records are in the file;
//! - a journal cut inside its last rewrite or damaged anywhere recovers each head at a root that
//!   head was at after some transaction;
//! - files with the same paths, modes and bytes make the same root in any VFS;
//! - a recovered VFS, used and settled, recovers to exactly its heads and their sequences.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_core::{Completion, Done, Effect, FileName, IoError};
use blaze_fuzzer::Target;
use blaze_ids::ContentId;
use blaze_vfs::{CommitError, Mode, Op, Ticket, View, Vfs};

const TARGET: Target = Target {
    name: "vfs_journal_model",
    check,
    seeds,
    dictionary: &[&[4], &[5], &[6], &[7], &[8], &[9], &[10], b"BZJ4"],
    max_len: 1 << 12,
    budget: Duration::from_secs(600),
};

const HEADS: [&str; 3] = ["a", "b", "c"];
const PATHS: [&str; 7] = ["a", "b", "d", "d/a", "d/b", "d/e/a", "e/a"];
const MODES: [Mode; 3] = [Mode::Regular, Mode::Executable, Mode::Symlink];

/// Steps before the crash, and after the recovery.
const STEPS: usize = 48;
const STEPS_AFTER: usize = 24;

fn check(data: &[u8]) {
    let mut input = Input { data, at: 0 };
    let mut run = Run::new(Vfs::recover(journal(), 0, &[]).0, Vec::new());
    for _ in 0..STEPS {
        if input.done() || !run.step(&mut input, true) {
            break;
        }
    }
    let (bytes, lower) = run.crash(&mut input);
    let vfs = recover(&bytes);
    run.recovered(&vfs, lower);

    let mut run = Run::new(vfs, bytes);
    for _ in 0..STEPS_AFTER {
        if input.done() {
            break;
        }
        run.step(&mut input, false);
    }
    run.settle();
    let back = recover(&run.file);
    let left = heads(&run.vfs);
    assert_eq!(
        heads(&back),
        left,
        "a recovered journal, used and settled, recovers to the heads it was left with"
    );
    for (head, _) in &left {
        assert_eq!(
            back.sequence(head),
            run.vfs.sequence(head),
            "a recovered journal, used and settled, recovers {head:?}'s sequence number"
        );
    }
}

fn journal() -> FileName {
    FileName::new("vfs/journal").expect("a file name")
}

fn recover(bytes: &[u8]) -> Vfs {
    let (largest, (vfs, _)) =
        blaze_fuzz::largest_allocation(|| Vfs::recover(journal(), 0, bytes));
    let allowed = blaze_fuzz::allowed(bytes.len());
    assert!(
        largest <= allowed,
        "recovering {} bytes asks for at most {allowed} at once, not {largest}",
        bytes.len()
    );
    vfs
}

/// Every head and its root, by name.
fn heads(vfs: &Vfs) -> Vec<(String, ContentId)> {
    vfs.heads().into_iter().map(|(head, root)| (head.to_owned(), root)).collect()
}

/// Every file of `root`, with its mode and bytes.
fn files(vfs: &mut Vfs, root: ContentId) -> Vec<(String, Mode, Vec<u8>)> {
    let view = vfs.open(View { root }).expect("a root the tree has opens");
    let mut out = Vec::new();
    let mut dirs = vec![String::new()];
    while let Some(dir) = dirs.pop() {
        for (name, is_dir) in vfs.list(view, &dir).expect("a directory of the tree lists") {
            let path = if dir.is_empty() { name } else { format!("{dir}/{name}") };
            if is_dir {
                dirs.push(path);
            } else {
                let file = vfs
                    .read(view, &path)
                    .expect("a file of a recovered tree is not corrupt")
                    .expect("a listed file reads");
                let (mode, bytes) = (file.mode, file.bytes.to_vec());
                out.push((path, mode, bytes));
            }
        }
    }
    vfs.close(view);
    out
}

/// The input, read a byte at a time; past its end every byte is 0.
struct Input<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Input<'a> {
    fn done(&self) -> bool {
        self.at >= self.data.len()
    }

    fn byte(&mut self) -> u8 {
        let byte = self.data.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        byte
    }

    fn u16(&mut self) -> usize {
        usize::from(self.byte()) | usize::from(self.byte()) << 8
    }

    fn bytes(&mut self, len: usize) -> &'a [u8] {
        let start = self.at.min(self.data.len());
        let end = self.at.saturating_add(len).min(self.data.len());
        self.at = self.at.saturating_add(len);
        &self.data[start..end]
    }

    fn pick<T: Copy>(&mut self, of: &[T]) -> T {
        of[usize::from(self.byte()) % of.len()]
    }
}

/// An op on one of [`PATHS`]: a kind byte (mod 4: write, edit, delete, rename), a path byte,
/// then for a write a mode byte, a length byte (mod 16) and the bytes; for an edit its start
/// and length (each mod 16), a length byte (mod 8) and the bytes; for a rename the path to.
fn op(input: &mut Input<'_>) -> Op {
    let kind = input.byte();
    let path = input.pick(&PATHS).to_owned();
    match kind % 4 {
        0 => {
            let mode = input.pick(&MODES);
            let len = usize::from(input.byte() % 16);
            Op::Write { mode, path, bytes: input.bytes(len).to_vec() }
        }
        1 => {
            let at = u64::from(input.byte() % 16);
            let len = u64::from(input.byte() % 16);
            let new = usize::from(input.byte() % 8);
            Op::Edit { at, len, path, bytes: input.bytes(new).to_vec() }
        }
        2 => Op::Delete { path },
        _ => Op::Rename { to: input.pick(&PATHS).to_owned(), from: path },
    }
}

/// A VFS on a simulated disk, and what each of its transactions left.
struct Run {
    vfs: Vfs,
    /// The journal file as the disk holds it, and how much of it a sync made durable.
    file: Vec<u8>,
    synced: usize,
    /// The file's length when its last rewrite completed. A rewrite is atomic, so a crash
    /// never cuts inside one; a cut there is damage.
    rewritten: usize,
    /// The effects asked for and not completed, each with how many transactions were made
    /// when it was asked: an append or a rewrite holds those not in the file before it.
    asked: Vec<(Effect, usize)>,
    /// The heads before the first transaction and after each.
    states: Vec<Vec<(String, ContentId)>>,
    /// By transaction: its ticket, the file length its records are all within, and whether it
    /// was made durable.
    tickets: Vec<Ticket>,
    ends: Vec<Option<usize>>,
    durable: Vec<bool>,
    /// Every root made, for commits against older ones.
    roots: Vec<ContentId>,
    scratch: Vec<Effect>,
}

impl Run {
    fn new(vfs: Vfs, file: Vec<u8>) -> Run {
        let mut roots: Vec<ContentId> = vfs.heads().into_iter().map(|it| it.1).collect();
        roots.push(vfs.empty());
        let states = vec![heads(&vfs)];
        Run {
            vfs,
            synced: file.len(),
            rewritten: file.len(),
            file,
            asked: Vec::new(),
            states,
            tickets: Vec::new(),
            ends: Vec::new(),
            durable: Vec::new(),
            roots,
            scratch: Vec::new(),
        }
    }

    /// One step, led by a byte whose value mod 16 says what it is: 2 moves a head to a root,
    /// 3 removes a head, 4 commits a file that is a journal, 5 flushes, 6 completes the effect
    /// in flight, 7 fails it, 8 collects, 9 settles, 10 crashes, anything else commits.
    /// Without `faults`, a failure completes and a crash settles. `false` once it crashes.
    fn step(&mut self, input: &mut Input<'_>, faults: bool) -> bool {
        match input.byte() % 16 {
            2 => {
                let head = input.pick(&HEADS);
                let root = self.root(input);
                let ticket = self.vfs.set_head(head, root).expect("a root the tree has");
                self.made(ticket);
            }
            3 => {
                if let Some(ticket) = self.vfs.remove_head(input.pick(&HEADS)) {
                    self.made(ticket);
                }
            }
            4 => self.embed(input),
            5 => self.flush(),
            6 => self.finish(input, true),
            7 => self.finish(input, !faults),
            8 => {
                self.vfs.collect(&[]);
                let vfs = &self.vfs;
                self.roots.retain(|it| vfs.has(*it));
                self.flush();
            }
            9 => self.settle(),
            10 if faults => return false,
            10 => self.settle(),
            _ => {
                let head = input.pick(&HEADS);
                let base = self.root(input);
                let ops: Vec<Op> = (0..=input.byte() % 3).map(|_| op(input)).collect();
                self.commit(head, base, &ops);
            }
        }
        true
    }

    /// A root the input picks with one byte: mod 8, 0 to 2 the root of that head (the empty
    /// root without one), 3 the empty root, else one made before.
    fn root(&self, input: &mut Input<'_>) -> ContentId {
        let pick = usize::from(input.byte());
        let root = match pick % 8 {
            at @ 0..=2 => self.vfs.head(HEADS[at]),
            3 => None,
            _ => Some(self.roots[pick / 8 % self.roots.len()]),
        };
        root.filter(|it| self.vfs.has(*it)).unwrap_or(self.vfs.empty())
    }

    fn commit(&mut self, head: &str, base: ContentId, ops: &[Op]) {
        match self.vfs.commit(head, base, ops) {
            Ok(commit) => {
                self.roots.push(commit.root);
                self.made(commit.ticket);
            }
            Err(CommitError::UnknownBase(root)) => panic!("{root:x} is a root the tree has"),
            Err(CommitError::Conflict { .. } | CommitError::Op(_)) => {}
        }
    }

    /// Commits to a head (path, head) a file whose bytes are another VFS's journal, in which a
    /// side head (byte) is moved to a root of this tree (as [`Run::root`] picks) and moved again
    /// up to 7 times (byte): records that name this tree's roots with larger sequence numbers
    /// than its own, which must stay a file's bytes.
    fn embed(&mut self, input: &mut Input<'_>) {
        let path = input.pick(&PATHS);
        let head = input.pick(&HEADS);
        let side_head = input.pick(&HEADS);
        let root = self.root(input);
        let bumps = input.byte() % 8;
        let writes: Vec<Op> = files(&mut self.vfs, root)
            .into_iter()
            .map(|(path, mode, bytes)| Op::Write { mode, path, bytes })
            .collect();
        let mut side = Vfs::recover(journal(), 0, &[]).0;
        let empty = side.empty();
        let made = side.commit(side_head, empty, &writes).expect("a tree's files make a tree").root;
        assert_eq!(made, root, "files with the same paths, modes and bytes make the same root");
        for _ in 0..bumps {
            side.set_head(side_head, made).expect("a root the side tree has");
        }
        let mut effects = Vec::new();
        side.flush(&mut effects);
        let Some(Effect::Append { bytes, .. }) = effects.pop() else {
            panic!("a new journal's first flush is an append")
        };
        assert!(effects.is_empty(), "a journal asks for one effect at a time: {effects:?}");
        let base = self.vfs.head(head).unwrap_or(self.vfs.empty());
        let write = Op::Write { mode: Mode::Regular, path: path.to_owned(), bytes };
        self.commit(head, base, &[write]);
    }

    /// A transaction was made.
    fn made(&mut self, ticket: Ticket) {
        self.tickets.push(ticket);
        self.ends.push(None);
        self.durable.push(false);
        self.states.push(heads(&self.vfs));
    }

    fn flush(&mut self) {
        self.vfs.flush(&mut self.scratch);
        self.take();
    }

    fn take(&mut self) {
        let made = self.tickets.len();
        self.asked.extend(self.scratch.drain(..).map(|effect| (effect, made)));
    }

    /// Completes the effect in flight, as the disk would or as a failure (a failed append
    /// leaving as many of its bytes as two input bytes say, a failed sync dropping what was not
    /// synced when an input byte is odd).
    fn finish(&mut self, input: &mut Input<'_>, ok: bool) {
        if self.asked.is_empty() {
            return;
        }
        let (effect, made) = self.asked.remove(0);
        let tag = effect.tag();
        let failed = || Done::Failed(IoError("injected".to_owned()));
        let done = match effect {
            Effect::Append { bytes, .. } if ok => {
                self.file.extend_from_slice(&bytes);
                let end = self.file.len();
                for it in self.ends[..made].iter_mut().filter(|it| it.is_none()) {
                    *it = Some(end);
                }
                Done::Appended
            }
            Effect::Append { bytes, .. } => {
                let kept = input.u16() % (bytes.len() + 1);
                self.file.extend_from_slice(&bytes[..kept]);
                failed()
            }
            Effect::Sync { .. } if ok => {
                self.synced = self.file.len();
                Done::Synced
            }
            Effect::Sync { .. } => {
                if input.byte() & 1 == 1 {
                    self.file.truncate(self.synced);
                }
                failed()
            }
            Effect::Truncate { len, .. } if ok => {
                self.file.truncate(usize::try_from(len).expect("a length within the file"));
                Done::Truncated
            }
            Effect::Write { bytes, .. } if ok => {
                self.file = bytes;
                self.synced = self.file.len();
                self.rewritten = self.file.len();
                for it in &mut self.ends[..made] {
                    *it = Some(self.rewritten);
                }
                Done::Written
            }
            Effect::Truncate { .. } | Effect::Write { .. } => failed(),
            other => panic!("the journal asks only for appends, syncs, cuts and rewrites: {other:?}"),
        };
        let durable = self.vfs.complete(Completion { tag, done }, &mut self.scratch);
        self.take();
        for ticket in durable {
            let at = self
                .tickets
                .iter()
                .position(|it| *it == ticket)
                .unwrap_or_else(|| panic!("a ticket made durable was handed out: {ticket:?}"));
            assert!(!self.durable[at], "a ticket is made durable once: {ticket:?}");
            assert!(
                self.ends[at].is_some_and(|end| end <= self.synced),
                "a ticket is made durable only once its records are synced: {ticket:?}"
            );
            self.durable[at] = true;
        }
    }

    /// Flushes and completes every effect until none is asked for.
    fn settle(&mut self) {
        self.flush();
        while !self.asked.is_empty() {
            self.finish(&mut Input { data: &[], at: 0 }, true);
        }
        assert!(self.durable.iter().all(|it| *it), "settling makes every ticket durable");
    }

    /// The file a crash leaves: the append in flight torn after as many bytes as two input
    /// bytes say, then (two bytes for a place, one for what) left whole, cut after the last
    /// rewrite, cut anywhere, or one bit flipped (a third byte says which). With it, for a file
    /// whole or cut after its last rewrite, how many transactions have all their records
    /// before the cut.
    fn crash(&mut self, input: &mut Input<'_>) -> (Vec<u8>, Option<usize>) {
        let mut file = std::mem::take(&mut self.file);
        if let Some((Effect::Append { bytes, .. }, _)) = self.asked.first() {
            let kept = input.u16() % (bytes.len() + 1);
            file.extend_from_slice(&bytes[..kept]);
        }
        let len = file.len();
        let at = input.u16();
        let cut = match input.byte() % 4 {
            0 => Some(len),
            1 => {
                let after = len.saturating_sub(self.rewritten);
                Some(len - after + at % (after + 1))
            }
            2 => Some(at % (len + 1)),
            _ => {
                if len > 0 {
                    file[at % len] ^= 1 << (input.byte() % 8);
                }
                None
            }
        };
        let Some(cut) = cut else { return (file, None) };
        file.truncate(cut);
        let whole = self.ends.iter().take_while(|it| it.is_some_and(|end| end <= cut)).count();
        (file, (cut >= self.rewritten).then_some(whole))
    }

    /// Checks the heads recovered from the crashed file: with `whole`, those after a prefix of
    /// at least that many transactions; else each head at a root it was at.
    fn recovered(&self, vfs: &Vfs, whole: Option<usize>) {
        let now = heads(vfs);
        match whole {
            Some(whole) => assert!(
                self.states[whole..].contains(&now),
                "a journal cut after its last rewrite recovers the heads after a prefix of its \
                 {} transactions, holding at least the first {whole}: recovered {now:?}, heads \
                 after each {:?}",
                self.states.len() - 1,
                self.states
            ),
            None => {
                for (head, root) in &now {
                    assert!(
                        self.states.iter().any(|state| state.iter().any(|it| it.0 == *head && it.1 == *root)),
                        "a damaged journal recovers {head:?} at {root:x}, where no transaction left it"
                    );
                }
            }
        }
    }
}

/// Programs of steps (see [`Run::step`]) that a correct journal passes: one commit settled
/// and recovered whole; one torn in flight; a group commit whose sync fails and drops its
/// pages, made good by a rewrite; a collection's rewrite followed by an append cut in half; a
/// branch made and removed, the file damaged in its first record.
fn seeds() -> Vec<Vec<u8>> {
    // Commit to `a` (from its root, none yet), one op: write `a`, regular, 3 bytes.
    let commit_a: [u8; 11] = [0, 0, 0, 0, 0, 0, 0, 3, b'x', b'y', b'z'];
    // Commit to `b` from the empty root, one op: write `d/a`, executable, 1 byte.
    let commit_b: [u8; 9] = [0, 1, 3, 0, 0, 3, 1, 1, b'q'];
    // Commit to `c` from the empty root, one op: write `b`, regular, 2 bytes.
    let commit_c: [u8; 10] = [0, 2, 3, 0, 0, 1, 0, 2, b'h', b'i'];
    let mut out = Vec::new();

    let mut whole = commit_a.to_vec();
    whole.extend([9, 10, 0, 0, 0]);
    out.push(whole);

    let mut torn = commit_a.to_vec();
    torn.extend([5, 10, 5, 0, 0, 0, 0]);
    out.push(torn);

    let mut group = commit_a.to_vec();
    group.extend([5]);
    group.extend(commit_b);
    group.extend([6, 7, 1, 6, 9, 10, 0, 0, 0]);
    out.push(group);

    let mut collected = commit_a.to_vec();
    collected.extend([9]);
    collected.extend(commit_b);
    collected.extend([8, 6]);
    collected.extend(commit_c);
    collected.extend([5, 6, 10, 0x20, 0, 1]);
    out.push(collected);

    let mut damaged = commit_a.to_vec();
    damaged.extend([2, 2, 0, 9, 3, 2, 9, 10, 12, 0, 3, 4]);
    damaged.extend(commit_b);
    out.push(damaged);
    out
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }
}
