//! The launcher's run mode as a process (docs/v2/plan/design/run.md, piece 5): a fake parent on
//! a socket pair serves a folder of code records, and the program prints.
//!
//! Ignored until the folder exists: `BLAZE_RUN_MODE_RECORDS` names a folder written for the
//! hello-world rung (pieces 3 and 7 write code records; nothing writes this folder yet), with:
//!
//! - `entry`: one line, what the parent sets as `BLAZE_LAUNCH_ENTRY` (`unit:<symbol hex>` or
//!   `c-main:<symbol hex>`);
//! - `index`: one line a function, `<symbol hex> <record hex>`;
//! - each record, named by its id's 64 hex digits (the BLAKE3 of its bytes).
//!
//! TO VERIFY BY A TEST (this one): the two-view code area runs under the launcher's current
//! signature (run.md, "Executable memory": ad hoc signed it runs; the hardened runtime kills it).

#![cfg(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "aarch64"),
))]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use blaze_launch::wire::run::{Declared, Down, Failure, Needed, Symbol, Up};
use blaze_launch::wire::{List, RecordId, split};
use blaze_launch::{Slot, Tier};

fn hex32(text: &str) -> [u8; 32] {
    assert_eq!(text.len(), 64, "`{text}` is not 64 hex digits");
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).expect("hex digits");
    }
    out
}

/// The parent's side: answers each need from the index, numbering slots as symbols first come.
struct Parent {
    records: BTreeMap<[u8; 32], [u8; 32]>,
    slots: BTreeMap<[u8; 32], u32>,
    by_slot: BTreeMap<u32, [u8; 32]>,
    next_id: u64,
}

impl Parent {
    fn answer(&mut self, need: Needed, out: &mut Vec<u8>) {
        let symbol = match need {
            Needed::Symbol(symbol) => symbol.0,
            Needed::Slot(slot) => *self.by_slot.get(&slot.0).expect("a need of a slot this parent declared"),
        };
        let Some(&record) = self.records.get(&symbol) else {
            Down::Failed { need, why: Failure::Unresolved }.encode(self.next_id, out).expect("Failed encodes");
            self.next_id += 1;
            return;
        };
        let next = self.slots.len() as u32;
        let slot = *self.slots.entry(symbol).or_insert(next);
        self.by_slot.insert(slot, symbol);
        let pairs = [Declared { symbol: Symbol(symbol), slot: Slot(slot) }];
        Down::Declare { pairs: List::of(&pairs) }.encode(self.next_id, out).expect("Declare encodes");
        Down::Publish { slot: Slot(slot), tier: Tier::Zero, record: RecordId(record) }
            .encode(self.next_id + 1, out)
            .expect("Publish encodes");
        self.next_id += 2;
    }
}

/// The hello-world rung under the launcher's run mode, served on first call, prints. Prints
/// "run_mode process: Hello, world! from first calls".
#[test]
#[ignore = "needs BLAZE_RUN_MODE_RECORDS, a folder of the hello-world rung's code records (pieces 3 and 7)"]
fn hello_world_runs_from_first_calls() {
    let folder = PathBuf::from(std::env::var_os("BLAZE_RUN_MODE_RECORDS").expect("BLAZE_RUN_MODE_RECORDS names the records folder"));
    let entry = std::fs::read_to_string(folder.join("entry")).expect("the folder's `entry`");
    let index = std::fs::read_to_string(folder.join("index")).expect("the folder's `index`");
    let mut parent = Parent { records: BTreeMap::new(), slots: BTreeMap::new(), by_slot: BTreeMap::new(), next_id: 1 };
    for line in index.lines().filter(|line| !line.is_empty()) {
        let (symbol, record) = line.split_once(' ').expect("`<symbol hex> <record hex>`");
        parent.records.insert(hex32(symbol), hex32(record));
    }

    let (mut ours, theirs) = UnixStream::pair().expect("a socket pair");
    ours.set_read_timeout(Some(Duration::from_secs(60))).expect("a read deadline");
    let theirs_fd = theirs.as_raw_fd();
    let mut command = Command::new(env!("CARGO_BIN_EXE_blaze-launcher"));
    command
        .env("BLAZE_LAUNCH_FD", "3")
        .env("BLAZE_LAUNCH_STORE", &folder)
        .env("BLAZE_LAUNCH_ENTRY", entry.trim())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: between fork and exec only `dup2`, which is async-signal-safe; the new descriptor
    // 3 is the child's end of the pair, open across the exec (dup2 clears close-on-exec).
    unsafe {
        command.pre_exec(move || match libc::dup2(theirs_fd, 3) {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
    let child = command.spawn().expect("the launcher starts");
    // The child holds its end now: the parent's copy goes, so the end of the child closes it.
    drop(theirs);

    let mut bytes = Vec::new();
    let mut buffer = [0u8; 1 << 16];
    loop {
        let got = match ours.read(&mut buffer) {
            Ok(got) => got,
            Err(why) => panic!("the launcher said nothing within the deadline, or the read failed: {why}"),
        };
        if got == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..got]);
        let mut used = 0;
        let mut answer = Vec::new();
        while let Some((item, took)) = split(&bytes[used..]).expect("a frame of the launcher's") {
            let (_, Up::Need { need, .. }) = Up::decode(item).expect("a run-wire message");
            parent.answer(need, &mut answer);
            used += took;
        }
        bytes.drain(..used);
        ours.write_all(&answer).expect("the answers are written");
    }
    let output = child.wait_with_output().expect("the launcher ends");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "the launcher ended {:?}; it said: {stderr}", output.status);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "Hello, world!\n", "its stderr: {stderr}");
    println!("run_mode process: Hello, world! from first calls");
}
