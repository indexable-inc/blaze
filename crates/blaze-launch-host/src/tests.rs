//! The daemon's side under every schedule Hegel draws: builds landing while launchers'
//! messages and the daemon's frames are in flight, launchers connecting between them, and
//! stubs firing against whatever build is newest when their need arrives.
//!
//! The machine here is the driver's contract written down (blaze-cargo's
//! `serve/unit/run/launch.rs`): what the program hands back for a launcher is written to it
//! in the order it was handed back, and a launcher's messages reach the program in the
//! order it sent them; nothing else is ordered. A model launcher applies frames as
//! blaze-launch-rt's control thread does: a record is kept until the next edit, an edit
//! binds the slots it names.

use std::collections::VecDeque;
use std::sync::Arc;

use blaze_codegen::driver::{Built, Owner};
use blaze_codegen::{
    Arch, CompiledFunction, Init, InstanceKey, Linkage, RelocKind, Relocation, Relocs, Static,
    Target,
};
use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemKey};
use blaze_launch::wire::{self, Ask, ProgramId, RecordId, Tell};
use blaze_launch::{Slot, Thread};
use hegel::TestCase;
use hegel::generators as gs;

use crate::program::{Conn, Plant, Program, Tables, Told};

fn pick(tc: &TestCase, max: usize) -> usize {
    tc.draw(gs::integers::<usize>().max_value(max))
}

fn id(domain: u8, a: u64, b: u64) -> ContentId {
    let mut bytes = [domain; 32];
    bytes[1..9].copy_from_slice(&a.to_le_bytes());
    bytes[9..17].copy_from_slice(&b.to_le_bytes());
    ContentId(bytes)
}

/// The source of a program: each function's body version, and each static's value.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Source {
    bodies: Vec<u64>,
    statics: Vec<u64>,
}

/// A function's key: its body's version is in it, as a code hash is in an instance's key.
fn key(function: usize, body: u64) -> InstanceKey {
    InstanceKey(id(1, function as u64, body))
}

/// The build of `source`: function `i` calls function `i + 1` (the last calls none) and
/// names static `i % statics`; function 0 is `main`. A function nothing changed is the very
/// `Arc` the build before held (`kept`), as the driver shares them.
fn build(source: &Source, kept: &[Arc<CompiledFunction>]) -> Built {
    let count = source.bodies.len();
    let mut functions: Vec<Arc<CompiledFunction>> = (0..count)
        .map(|at| {
            let own = key(at, source.bodies[at]);
            let mut relocs: Vec<Relocation> = Vec::new();
            if at + 1 < count {
                let callee = Target::Function(key(at + 1, source.bodies[at + 1]));
                // clone: a target of a synthetic function, in a test
                relocs.push(Relocation { at: 0, kind: RelocKind::GotPage21, target: callee.clone(), addend: 0 });
                relocs.push(Relocation { at: 4, kind: RelocKind::GotPageOffset12, target: callee, addend: 0 });
            }
            if !source.statics.is_empty() {
                let of = Target::Static {
                    item: ItemKey(id(3, (at % source.statics.len()) as u64, 0)),
                    instance: ContentId::default(),
                };
                relocs.push(Relocation { at: 8, kind: RelocKind::GotPage21, target: of.clone(), addend: 0 }); // clone: as above
                relocs.push(Relocation { at: 12, kind: RelocKind::GotPageOffset12, target: of, addend: 0 });
            }
            let mut code = vec![0u8; 24];
            // `adrp x16; ldr x16, [x16]` twice, then the body's version.
            for (at, word) in [0x9000_0010u32, 0xf940_0210, 0x9000_0010, 0xf940_0210].iter().enumerate() {
                code[at * 4..at * 4 + 4].copy_from_slice(&word.to_le_bytes());
            }
            code[16..24].copy_from_slice(&source.bodies[at].to_le_bytes());
            let made = CompiledFunction {
                key: own,
                arch: Arch::Aarch64,
                align: 4,
                code,
                relocs: Relocs::from_iter(relocs),
                data: Vec::new(),
                inlined: Vec::new(),
                side: None,
                unwind: None,
                lines: Vec::new(),
                coverage: None,
                profile: None,
            };
            match kept.iter().find(|it| ***it == made) {
                Some(same) => Arc::clone(same),
                None => Arc::new(made),
            }
        })
        .collect();
    let mut identities: Vec<(InstanceKey, ContentId)> =
        (0..count).map(|at| (key(at, source.bodies[at]), id(2, at as u64, 0))).collect();
    let mut owners: Vec<(InstanceKey, Owner, CodeHash)> = (0..count)
        .map(|at| (key(at, source.bodies[at]), Owner::Workspace, CodeHash(id(4, at as u64, source.bodies[at]))))
        .collect();
    let mut names: Vec<(InstanceKey, String)> =
        (0..count).map(|at| (key(at, source.bodies[at]), format!("f{at}"))).collect();
    functions.sort_by_key(|it| it.key);
    identities.sort_by_key(|it| it.0);
    owners.sort_by_key(|it| it.0);
    names.sort_by_key(|it| it.0);
    let mut statics: Vec<Static> = source
        .statics
        .iter()
        .enumerate()
        .map(|(at, value)| Static {
            item: ItemKey(id(3, at as u64, 0)),
            instance: ContentId::default(),
            align: 8,
            init: Init::Bytes { bytes: value.to_le_bytes().to_vec(), relocs: Relocs::from_iter([]) },
            data: Vec::new(),
            mutable: false,
            thread_local: false,
            section: None,
            imports: Vec::new(),
        })
        .collect();
    statics.sort_by_key(|it| (it.item, it.instance));
    Built {
        functions,
        statics,
        identities,
        owners,
        debug_names: names,
        symbols: vec![(key(0, source.bodies[0]), "main".to_owned(), Linkage::Strong)],
        ..Built::default()
    }
}

/// A launcher, as the daemon's side sees one: what it has been sent and has not yet read,
/// what it has said and the daemon has not yet read, and what it holds.
#[derive(Default)]
struct Launcher {
    conn: u64,
    /// Frames written to its socket, not yet read by it.
    inbound: VecDeque<u8>,
    /// Its messages, not yet read by the daemon.
    outbound: VecDeque<Vec<u8>>,
    /// The records given since the last edit, with their bytes.
    held: Vec<(RecordId, Vec<u8>)>,
    declared: u32,
    /// By slot: the record bound.
    bound: Vec<Option<RecordId>>,
    /// Slots whose stub fired and waits.
    waiting: Vec<u32>,
    started: bool,
    restarted: bool,
    /// Set by the test when a build it must not take live landed while it ran.
    must_restart: bool,
}

impl Launcher {
    /// Holds a bind or a definition to what a real launcher refuses it for: its record was
    /// given, it has a number for each numbered place, and each function it names has a slot
    /// this launcher was told to declare.
    fn named(&self, record: RecordId, refs: impl Iterator<Item = u32>) {
        use blaze_launch::record::{Place, Record};
        let Some((_, bytes)) = self.held.iter().find(|it| it.0 == record) else {
            panic!("a record that was not given is named");
        };
        let record = Record::decode(bytes).expect("a record");
        let refs: Vec<u32> = refs.collect();
        let numbered: Vec<Place<'_>> = record.places.iter().filter(Place::numbered).collect();
        assert_eq!(refs.len(), numbered.len(), "`{}`: numbers for its places", record.name);
        for (place, number) in numbered.iter().zip(&refs) {
            if *place == Place::Function {
                assert!(
                    *number < self.declared,
                    "`{}` names slot {number}, and this launcher declared {}",
                    record.name,
                    self.declared
                );
            }
        }
    }

    fn say(&mut self, ask: &Ask<'_>) {
        let mut frame = Vec::new();
        ask.encode(1, &mut frame).expect("fits a frame");
        self.outbound.push_back(frame[4..].to_vec());
    }

    /// Reads and carries out its next whole frame; `false` when none is there.
    fn step(&mut self) -> bool {
        let bytes: Vec<u8> = self.inbound.iter().copied().collect();
        let Some((item, took)) = wire::split(&bytes).expect("a frame of the daemon's") else {
            return false;
        };
        let (id, tell) = Tell::decode(item).expect("a message of the daemon's");
        match tell {
            Tell::Welcome { slots } => {
                self.declared = slots;
                self.bound.resize(slots as usize, None);
            }
            Tell::Record { id, bytes } => self.held.push((id, bytes.to_vec())),
            Tell::Statics { defs } => {
                for def in defs.iter() {
                    self.named(def.record, def.refs.iter());
                }
                self.held.clear();
            }
            Tell::Edit { txn, binds, declare, .. } => {
                self.declared += declare;
                self.bound.resize(self.declared as usize, None);
                for bind in binds.iter() {
                    assert!(bind.slot.0 < self.declared, "an edit binds a slot not declared");
                    self.named(bind.record, bind.refs.iter());
                    self.bound[bind.slot.0 as usize] = Some(bind.record);
                    self.waiting.retain(|it| *it != bind.slot.0);
                }
                self.held.clear();
                let _ = id;
                self.say(&Ask::Applied { txn, epoch: 0 });
            }
            Tell::Start { .. } => self.started = true,
            Tell::Restart { txn, .. } => {
                self.restarted = true;
                self.say(&Ask::Restarting { txn });
            }
            Tell::Refused { slot, diagnostic } => panic!("slot {} refused: {diagnostic}", slot.0),
            Tell::Unheld { why } => panic!("unheld: {why}"),
            Tell::Probe { .. } => panic!("a probe, which this daemon does not send"),
        }
        drop(self.inbound.drain(..took));
        true
    }
}

/// One operation of a schedule.
#[derive(Clone, Copy, Debug)]
enum Op {
    /// An edit lands: of one body, of two at once, of a static, or a function added.
    OneBody(usize),
    TwoBodies(usize),
    Static,
    Added,
    /// A launcher connects and says hello.
    Connect,
    /// A launcher reads its next frame.
    Read(usize),
    /// The daemon reads a launcher's next message.
    Heard(usize),
    /// A launcher's stub fires for a slot.
    Need(usize, usize),
}

fn op(tc: &TestCase) -> Op {
    match pick(tc, 13) {
        0 | 1 => Op::OneBody(pick(tc, 7)),
        2 => Op::TwoBodies(pick(tc, 7)),
        3 => Op::Static,
        4 => Op::Added,
        5 => Op::Connect,
        6..=8 => Op::Read(pick(tc, 3)),
        9 | 10 => Op::Heard(pick(tc, 3)),
        _ => Op::Need(pick(tc, 3), pick(tc, 9)),
    }
}

/// Runs `ops` against a program with `plant`, then lets everything in flight arrive, and
/// checks the invariant. Gives what broke, when something did.
fn run(ops: &[Op], plant: Plant) -> Result<(), String> {
    let mut program = Program::new();
    program.plant = plant;
    let mut source = Source { bodies: vec![1, 1, 1], statics: vec![7, 7] };
    let mut version = 1u64;
    let mut kept: Vec<Arc<CompiledFunction>> = Vec::new();
    let mut launchers: Vec<Launcher> = Vec::new();
    let mut conns = 0u64;

    let rebuild = |program: &mut Program,
                       source: &Source,
                       kept: &mut Vec<Arc<CompiledFunction>>,
                       launchers: &mut Vec<Launcher>,
                       fatal: bool|
     -> Result<(), String> {
        let built = build(source, kept);
        *kept = built.functions.clone(); // clone: counts, in a test
        // A launcher that was welcomed before this build runs code of the build before.
        for launcher in launchers.iter_mut().filter(|it| it.declared > 0 && !it.restarted) {
            launcher.must_restart |= fatal;
        }
        let told = program.built(&built, "main", Tables::Whole(&[]))?;
        for (conn, frames, what) in told {
            let Some(launcher) = launchers.iter_mut().find(|it| it.conn == conn.0) else {
                return Err(format!("the program told launcher {} something, and it is gone", conn.0));
            };
            if let Told::Restart { .. } = what {
                // Whoever was told to start again is no longer this build's concern.
            }
            launcher.inbound.extend(frames);
        }
        Ok(())
    };
    rebuild(&mut program, &source, &mut kept, &mut launchers, false)?;

    let hear = |program: &mut Program, launcher: &mut Launcher| -> Result<bool, String> {
        let Some(message) = launcher.outbound.pop_front() else { return Ok(false) };
        let mut out = Vec::new();
        program.said(Conn(launcher.conn), &message, &mut out)?;
        launcher.inbound.extend(out);
        Ok(true)
    };

    for op in ops {
        match *op {
            Op::OneBody(function) => {
                version += 1;
                let at = function % source.bodies.len();
                source.bodies[at] = version;
                rebuild(&mut program, &source, &mut kept, &mut launchers, false)?;
            }
            Op::TwoBodies(function) => {
                version += 1;
                // Two functions that are not `main` (whose shim would hide one).
                let count = source.bodies.len();
                let (a, b) = (1 + function % (count - 1), 1 + (function + 1) % (count - 1));
                source.bodies[a] = version;
                source.bodies[b] = version;
                rebuild(&mut program, &source, &mut kept, &mut launchers, a != b)?;
            }
            Op::Static => {
                version += 1;
                source.statics[0] = version;
                rebuild(&mut program, &source, &mut kept, &mut launchers, true)?;
            }
            Op::Added => {
                version += 1;
                // The last function's callee is new: its body changes with the addition.
                let last = source.bodies.len() - 1;
                source.bodies[last] = version;
                source.bodies.push(version);
                rebuild(&mut program, &source, &mut kept, &mut launchers, false)?;
            }
            Op::Connect => {
                if launchers.len() < 4 {
                    conns += 1;
                    program.connected(Conn(conns));
                    let mut launcher = Launcher { conn: conns, ..Launcher::default() };
                    launcher.say(&Ask::Hello {
                        program: ProgramId([0; 32]),
                        pid: 1,
                        arch: wire::Arch::Aarch64,
                        os: wire::Os::Linux,
                        page: 4096,
                    });
                    launchers.push(launcher);
                }
            }
            Op::Read(which) => {
                if let Some(launcher) = launchers.get_mut(which) {
                    launcher.step();
                }
            }
            Op::Heard(which) => {
                if let Some(launcher) = launchers.get_mut(which) {
                    hear(&mut program, launcher)?;
                }
            }
            Op::Need(which, slot) => {
                if let Some(launcher) = launchers.get_mut(which)
                    && launcher.started
                    && !launcher.restarted
                    && (slot as u32) < launcher.declared
                    && launcher.bound[slot].is_none()
                    && !launcher.waiting.contains(&(slot as u32))
                {
                    launcher.waiting.push(slot as u32);
                    launcher.say(&Ask::Need { slot: Slot(slot as u32), thread: Thread(1) });
                }
            }
        }
    }
    // Everything in flight arrives.
    for _ in 0..10_000 {
        let mut moved = false;
        for launcher in &mut launchers {
            moved |= launcher.step();
            moved |= hear(&mut program, launcher)?;
        }
        if !moved {
            break;
        }
    }
    // The invariant.
    for launcher in &launchers {
        if launcher.restarted {
            continue;
        }
        if launcher.must_restart {
            return Err(format!(
                "launcher {} runs on after a build it cannot take live (two bodies or a static changed)",
                launcher.conn
            ));
        }
        if !launcher.waiting.is_empty() {
            return Err(format!("launcher {} still waits for slots {:?}", launcher.conn, launcher.waiting));
        }
        for (slot, bound) in launcher.bound.iter().enumerate() {
            let Some(bound) = bound else { continue };
            let newest = program.newest(slot as u32);
            if newest != Some(*bound) {
                return Err(format!(
                    "launcher {}'s slot {slot} holds a record that is not the newest build's",
                    launcher.conn
                ));
            }
        }
    }
    Ok(())
}

/// The invariant: once a launcher has carried out everything it was sent, each of its bound
/// slots holds the record the newest build makes of that slot's function, or the launcher
/// was told to start again; and a launcher that ran through a build it cannot take live
/// (two bodies changed at once, a static's value) was told to start again. So an edit that
/// lands while another is in flight, or while a stub's need is, never leaves code of an old
/// input bound.
#[hegel::test(test_cases = 300)]
fn no_schedule_leaves_a_launcher_on_code_of_an_old_build(tc: TestCase) {
    let ops: Vec<Op> = (0..pick(&tc, 60)).map(|_| op(&tc)).collect();
    if let Err(why) = run(&ops, Plant::None) {
        panic!("{why}\nops: {ops:?}");
    }
}

/// The schedules that show each planted bug: a launcher whose entry (slot 0, the one slot a
/// launcher has before it calls anything) is bound, then an edit.
fn bound_then(edit: Op) -> Vec<Op> {
    vec![Op::Connect, Op::Heard(0), Op::Read(0), Op::Read(0), Op::Read(0), Op::Read(0), Op::Read(0), Op::Need(0, 0), Op::Heard(0), edit]
}

/// The control: a daemon that rebinds nothing at a build leaves a launcher on the old body,
/// and the oracle says so.
#[test]
fn the_oracle_catches_a_build_that_rebinds_nothing() {
    let ops = bound_then(Op::OneBody(0));
    assert_eq!(run(&ops, Plant::None), Ok(()));
    let caught = run(&ops, Plant::NoRebind).unwrap_err();
    assert!(caught.contains("not the newest build's"), "{caught}");
}

/// The control: a daemon that sends two bodies changed at once as an edit leaves a launcher
/// running through it, and the oracle says so.
#[test]
fn the_oracle_catches_two_changed_bodies_sent_as_an_edit() {
    let ops = bound_then(Op::TwoBodies(0));
    assert_eq!(run(&ops, Plant::None), Ok(()));
    let caught = run(&ops, Plant::NoRestart).unwrap_err();
    assert!(caught.contains("cannot take live"), "{caught}");
}

/// The control: a daemon that counts the slots an edit declares before its records name
/// their callees binds a record that names a slot the launcher has not declared (found on a
/// real program, 2026-11-12, when slots began to be given on first reference), and the model
/// launcher refuses it as a real one does.
#[test]
fn the_oracle_catches_an_edit_that_declares_before_its_records_name() {
    let ops = bound_then(Op::Need(0, 0));
    assert_eq!(run(&ops, Plant::None), Ok(()));
    let caught = std::panic::catch_unwind(|| run(&ops, Plant::DeclareFirst));
    assert!(caught.is_err(), "a bind that names an undeclared slot was taken");
}

/// What a build is classified as, one case each (the table in `program`'s documentation).
#[test]
fn a_build_is_an_edit_a_declaration_or_a_restart() {
    let told = |edit: Op| -> Vec<Told> {
        let mut program = Program::new();
        let mut source = Source { bodies: vec![1, 1, 1], statics: vec![7, 7] };
        let built = build(&source, &[]);
        let kept = built.functions.clone(); // clone: counts, in a test
        program.built(&built, "main", Tables::Whole(&[])).expect("the first build");
        program.connected(Conn(1));
        let mut launcher = Launcher { conn: 1, ..Launcher::default() };
        launcher.say(&Ask::Hello {
            program: ProgramId([0; 32]),
            pid: 1,
            arch: wire::Arch::Aarch64,
            os: wire::Os::Linux,
            page: 4096,
        });
        let mut exchange = |program: &mut Program, launcher: &mut Launcher| {
            while let Some(message) = launcher.outbound.pop_front() {
                let mut out = Vec::new();
                program.said(Conn(1), &message, &mut out).expect("served");
                launcher.inbound.extend(out);
                while launcher.step() {}
            }
        };
        exchange(&mut program, &mut launcher);
        for slot in [0u32, 1, 2] {
            launcher.say(&Ask::Need { slot: Slot(slot), thread: Thread(1) });
        }
        exchange(&mut program, &mut launcher);
        match edit {
            Op::OneBody(at) => source.bodies[at] = 2,
            Op::TwoBodies(_) => (source.bodies[1], source.bodies[2]) = (2, 2),
            Op::Static => source.statics[0] = 8,
            Op::Added => {
                source.bodies[2] = 2;
                source.bodies.push(2);
            }
            _ => {}
        }
        let built = build(&source, &kept);
        program.built(&built, "main", Tables::Whole(&[])).expect("built").into_iter().map(|it| it.2).collect()
    };
    // One body: its slot, switched together. (Its caller's record names it by place: the
    // same record, not bound again.)
    assert_eq!(told(Op::OneBody(2)), [Told::Edit { binds: 1, together: true }]);
    // The entry itself (here function 0 is the entry: the shim the front end makes around
    // `fn main`) is not counted as a changed body: bound again, with nothing to gate.
    assert_eq!(told(Op::OneBody(0)), [Told::Edit { binds: 1, together: false }]);
    // A function added: the caller that now calls it is rebound, and the slot declared.
    assert_eq!(told(Op::Added), [Told::Edit { binds: 1, together: true }]);
    assert!(matches!(&told(Op::TwoBodies(0))[..], [Told::Restart { why }] if why.contains("f1, f2")));
    assert!(matches!(&told(Op::Static)[..], [Told::Restart { why }] if why.contains("static")));
    // Nothing changed: nothing is said.
    assert_eq!(told(Op::Connect), [Told::Nothing]);
}

/// A record made of a compiled function (the table in `record`'s documentation): a callee
/// and a symbol the program defines are function places the bind numbers; a symbol nothing
/// of the program defines is an outside symbol by name; a `Direct` of the function itself
/// is its own bytes; a datum keeps its place; and a position becomes two data, the file's
/// name and the `Location` that points at it, with the span table it was read from noted.
#[test]
fn a_record_names_callees_by_place_and_carries_its_positions_as_data() {
    use blaze_codegen::Data;
    use blaze_format::SpanRef;
    use blaze_launch::record::{Kind, Place, Record, To};
    use blaze_link::{Position, SpanTable};

    use crate::record::{self, Named, Positions};

    struct Build {
        anchor: ContentId,
        table: SpanTable,
        defined: InstanceKey,
    }
    impl Positions for Build {
        fn of(&self, span: &SpanRef) -> Result<(&str, Position), String> {
            if span.anchor != self.anchor {
                return Err("no table of that anchor".to_owned());
            }
            let position = self.table.spans[span.index as usize];
            Ok((&self.table.files[position.file as usize], position))
        }

        fn defined(&self, name: &str) -> Option<Named> {
            (name == "__rust_enable_lse").then_some(Named::Function(self.defined))
        }
    }
    let (own, callee, defined) = (key(0, 1), key(1, 1), key(2, 1));
    let anchor = id(9, 1, 0);
    let build = Build {
        anchor,
        table: SpanTable {
            files: vec!["src/main.rs".to_owned()],
            spans: vec![Position { file: 0, line: 4, column: 17 }],
        },
        defined,
    };
    let datum = Data { id: id(8, 1, 0), align: 1, bytes: b"boom".to_vec(), relocs: Relocs::from_iter([]), writable: false };
    let reloc = |at: u32, kind: RelocKind, target: Target| Relocation { at, kind, target, addend: 0 };
    let function = CompiledFunction {
        key: own,
        arch: Arch::Aarch64,
        align: 4,
        code: vec![0; 40],
        relocs: Relocs::from_iter([
            reloc(0, RelocKind::GotPage21, Target::Function(callee)),
            reloc(4, RelocKind::GotPage21, Target::Extern("__rust_enable_lse".to_owned())),
            reloc(8, RelocKind::GotPage21, Target::Extern("write".to_owned())),
            reloc(12, RelocKind::Call26, Target::Direct(own)),
            reloc(16, RelocKind::GotPage21, Target::Data(datum.id)),
            reloc(20, RelocKind::GotPage21, Target::Position(SpanRef { anchor, index: 0 })),
        ]),
        data: vec![datum],
        inlined: Vec::new(),
        side: None,
        unwind: None,
        lines: Vec::new(),
        coverage: None,
        profile: None,
    };
    let made = record::function(&function, "hello::main", &build).expect("made");
    assert_eq!(made.named, [Named::Function(callee), Named::Function(defined)]);
    assert_eq!(made.anchors, [anchor]);
    assert_eq!(made.id.0, *blake3::hash(&made.bytes).as_bytes());
    let read = Record::decode(&made.bytes).expect("a record");
    let places: Vec<Place<'_>> = read.places.iter().collect();
    assert_eq!(places, [Place::Function, Place::Function, Place::Extern("write")]);
    let to: Vec<To> = read.fixups.iter().map(|it| it.to).collect();
    // The function's own datum is first; the position's file name and `Location` follow.
    assert_eq!(to, [To::Place(0), To::Place(1), To::Place(2), To::Own, To::Data(0), To::Data(2)]);
    let data: Vec<_> = read.data.iter().collect();
    assert_eq!(data.len(), 3);
    assert_eq!((data[0].bytes, data[1].bytes), (&b"boom"[..], &b"src/main.rs\0"[..]));
    // The `Location`: the name's pointer (a fixup to the name's datum), its length, the line
    // and the column.
    let location = data[2].bytes;
    assert_eq!(location.len(), 24);
    assert_eq!(u64::from_le_bytes(location[8..16].try_into().expect("8 bytes")), 11);
    assert_eq!(u32::from_le_bytes(location[16..20].try_into().expect("4 bytes")), 4);
    assert_eq!(u32::from_le_bytes(location[20..24].try_into().expect("4 bytes")), 17);
    let pointer: Vec<_> = data[2].fixups.iter().collect();
    assert_eq!((pointer[0].kind, pointer[0].to, pointer[0].at), (Kind::Abs64, To::Data(1), 0));
    // The placed length: the code, then a cell for each thing reached through a word that
    // is no function (`write`, the datum, the location).
    assert_eq!(made.len, 40 + 3 * 8);

    // What a launcher does not take is refused by name: a relocation kind with no record kind.
    let mut bad = function.clone(); // clone: a test's copy
    bad.relocs = Relocs::from_iter([reloc(0, RelocKind::TlsGd32, Target::Function(callee))]);
    let refused = record::function(&bad, "hello::main", &build).unwrap_err();
    assert!(refused.contains("`hello::main`") && refused.contains("TlsGd32"), "{refused}");
}
