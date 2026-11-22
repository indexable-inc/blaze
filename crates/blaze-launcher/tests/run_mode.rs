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
            let (_, up) = Up::decode(item).expect("a run-wire message");
            // A fault's report is no ask: the program dies after it, and its end is the test's.
            let Up::Need { need, .. } = up else {
                used += took;
                continue;
            };
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

/// Real end-to-end zero-touch telemetry proof across the actual `blaze-launcher` child process:
/// - Places two plain AArch64 code records (`main` and `work`) with ZERO instrumentation in
///   their machine code.
/// - `main` calls `work()` twice and calls `malloc(256)` + `free(ptr)` via `Space::Outside`.
/// - The parent pre-publishes `work` and sends `Down::Hooks { slot: work, kind: HookKind::Span }`
///   over fd 3, while passing `BLAZE_LAUNCH_TELEMETRY=<shared ring file>`.
/// - After the child exits 0, the parent drains the shared-memory ring and verifies:
///   1. 2 `SpanEnter` + 2 `SpanExit` events for `work` (matching `SpanId`s, durations)
///   2. 1 `Alloc` (`Malloc`, 256 bytes, non-null ptr, non-zero caller frame) + 1 matching `Free`
#[cfg(target_arch = "aarch64")]
#[test]
fn real_launcher_emits_zero_touch_spans_and_alloc_telemetry_over_shared_memory() {
    use blaze_code::record::{CodeRecord, Site, SiteKind, Space, Symbol as CodeSymbol, Target};
    use blaze_code::unwind::Frame;
    use blaze_launch::wire::run::HookKind;
    use blaze_launch_rt::telemetry::{AllocOp, EventKind, MappedRing};

    const ADRP_X16: u32 = 0x9000_0010;
    const LDR_X16: u32 = 0xF940_0210;
    const BLR_X16: u32 = 0xD63F_0200;
    const RET: u32 = 0xD65F_03C0;
    const STP_FP_LR_PRE32: u32 = 0xA9BE_7BFD;
    const MOV_FP_SP: u32 = 0x9100_03FD;
    const LDP_FP_LR_POST32: u32 = 0xA8C2_7BFD;
    const MOVZ_W0_42: u32 = 0x5280_0540;
    const MOVZ_X0_256: u32 = 0xD280_2000;

    let alloc = blaze_own::SharedAlloc::global().local();
    let main_sym = CodeSymbol([0x11; 32]);
    let work_sym = CodeSymbol([0x22; 32]);
    let malloc_sym = CodeSymbol::of_name("malloc");
    let free_sym = CodeSymbol::of_name("free");

    // `work`: `mov w0, #42; ret`
    let mut work_rec = CodeRecord::new_in(alloc);
    for inst in [MOVZ_W0_42, RET] {
        work_rec.code.extend_from_slice(&inst.to_le_bytes());
    }
    work_rec.frame = Frame::PLAIN.word().expect("plain frame");
    let mut work_bytes = blaze_own::Vec::new_in(alloc);
    work_rec.seal().expect("work seals").encode(&mut work_bytes);
    let work_id = blaze_blake3::hash(&work_bytes);

    // `main`: calls `work()` twice, then `malloc(256)` and `free(ptr)`
    let mut main_rec = CodeRecord::new_in(alloc);
    let insts: [u32; 17] = [
        STP_FP_LR_PRE32, // 0
        MOV_FP_SP,       // 4
        ADRP_X16,        // 8  (call work #1)
        LDR_X16,         // 12
        BLR_X16,         // 16
        ADRP_X16,        // 20 (call work #2)
        LDR_X16,         // 24
        BLR_X16,         // 28
        MOVZ_X0_256,     // 32
        ADRP_X16,        // 36 (call malloc(256))
        LDR_X16,         // 40
        BLR_X16,         // 44
        ADRP_X16,        // 48 (call free(x0))
        LDR_X16,         // 52
        BLR_X16,         // 56
        LDP_FP_LR_POST32,// 60
        RET,             // 64
    ];
    for inst in insts {
        main_rec.code.extend_from_slice(&inst.to_le_bytes());
    }
    let t_work = main_rec.target(Target { space: Space::Func, symbol: work_sym });
    let t_malloc = main_rec.outside("malloc");
    let t_free = main_rec.outside("free");
    assert_eq!(malloc_sym, CodeSymbol::of_name("malloc"));
    assert_eq!(free_sym, CodeSymbol::of_name("free"));
    main_rec.sites.push(Site { at: 8, kind: SiteKind::Call, target: t_work, addend: 0 });
    main_rec.sites.push(Site { at: 20, kind: SiteKind::Call, target: t_work, addend: 0 });
    main_rec.sites.push(Site { at: 36, kind: SiteKind::Call, target: t_malloc, addend: 0 });
    main_rec.sites.push(Site { at: 48, kind: SiteKind::Call, target: t_free, addend: 0 });
    main_rec.frame = Frame::PLAIN.word().expect("plain frame");
    let mut main_bytes = blaze_own::Vec::new_in(alloc);
    main_rec.seal().expect("main seals").encode(&mut main_bytes);
    let main_id = blaze_blake3::hash(&main_bytes);

    let tmp = std::env::temp_dir().join(format!("blaze-launcher-telem-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("tmp dir");
    let hex_of = |id: &[u8; 32]| -> String { id.iter().map(|b| format!("{b:02x}")).collect() };
    std::fs::write(tmp.join(hex_of(&work_id)), &work_bytes).expect("write work record");
    std::fs::write(tmp.join(hex_of(&main_id)), &main_bytes).expect("write main record");

    let ring_path = tmp.join("telemetry.ring");
    let ring_cpath = std::ffi::CString::new(ring_path.to_str().expect("utf8")).expect("cstr");
    let ring = MappedRing::create(&ring_cpath, 256).expect("create telemetry ring");

    let (mut ours, theirs) = UnixStream::pair().expect("socket pair");
    ours.set_read_timeout(Some(Duration::from_secs(15))).expect("read timeout");
    let theirs_fd = theirs.as_raw_fd();
    let entry_line = format!("unit:{}", hex_of(&main_sym.0));

    let mut command = Command::new(env!("CARGO_BIN_EXE_blaze-launcher"));
    command
        .env("BLAZE_LAUNCH_FD", "3")
        .env("BLAZE_LAUNCH_STORE", &tmp)
        .env("BLAZE_LAUNCH_ENTRY", &entry_line)
        .env(blaze_launch::telemetry::VAR, &ring_path)
        .env("BLAZE_ALLOC_SAMPLE_BYTES", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(move || match libc::dup2(theirs_fd, 3) {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
    let child = command.spawn().expect("spawn blaze-launcher");
    drop(theirs);

    // Pre-declare main (slot 0) and work (slot 1), publish work, and attach HookKind::Span to
    // work BEFORE main runs!
    let mut ahead = Vec::new();
    let pairs = [
        Declared { symbol: Symbol(main_sym.0), slot: Slot(0) },
        Declared { symbol: Symbol(work_sym.0), slot: Slot(1) },
    ];
    Down::Declare { pairs: List::of(&pairs) }.encode(1, &mut ahead).expect("Declare");
    Down::Publish { slot: Slot(1), tier: Tier::Zero, record: RecordId(work_id) }
        .encode(2, &mut ahead)
        .expect("Publish work");
    Down::Hooks { slot: Slot(1), kind: HookKind::Span }
        .encode(3, &mut ahead)
        .expect("Hooks work");
    Down::Publish { slot: Slot(0), tier: Tier::Zero, record: RecordId(main_id) }
        .encode(4, &mut ahead)
        .expect("Publish main");
    ours.write_all(&ahead).expect("write ahead frames");

    // Drain any fallback needs (if main fired before ahead arrived) until child exits.
    let mut buf = [0u8; 4096];
    loop {
        match ours.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }

    let output = child.wait_with_output().expect("child exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "launcher failed: {:?}; stderr: {stderr}", output.status);

    let mut events = Vec::new();
    ring.ring().drain(256, |e| events.push(e));

    let span_enters: Vec<_> = events.iter().filter(|e| e.event_kind() == Some(EventKind::SpanEnter)).collect();
    let span_exits: Vec<_> = events.iter().filter(|e| e.event_kind() == Some(EventKind::SpanExit)).collect();
    let allocs: Vec<_> = events
        .iter()
        .filter(|e| e.event_kind() == Some(EventKind::Alloc) && e.slot_or_op == AllocOp::Malloc as u32 && e.b == 256)
        .collect();
    let frees: Vec<_> = events.iter().filter(|e| e.event_kind() == Some(EventKind::Free)).collect();

    assert_eq!(span_enters.len(), 2, "work() called twice must emit 2 SpanEnter events; got {events:?}");
    assert_eq!(span_exits.len(), 2, "work() called twice must emit 2 SpanExit events; got {events:?}");
    assert_eq!(span_enters[0].b, span_exits[0].b, "matching SpanId for call 1");
    assert_eq!(span_enters[1].b, span_exits[1].b, "matching SpanId for call 2");
    assert_eq!(allocs.len(), 1, "malloc(256) must emit 1 Alloc(Malloc, 256) event; got {events:?}");
    assert_ne!(allocs[0].a, 0, "malloc returned non-null ptr");
    assert_ne!(allocs[0].c, 0, "malloc captured caller return address for YourKit-style attribution");
    assert!(
        frees.iter().any(|f| f.a == allocs[0].a),
        "free(ptr) must emit Free matching malloc ptr {:#x}; got {frees:?}",
        allocs[0].a
    );

    drop(ring);
    let _ = std::fs::remove_dir_all(&tmp);
    println!("real launcher e2e: 2 zero-touch SpanEnter/SpanExit + 1 Malloc(256)+Free over shared memory");
}
