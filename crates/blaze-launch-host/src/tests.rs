//! The daemon's side under every schedule Hegel draws: builds landing while launchers'
//! messages, the daemon's frames and the answers to first calls are in flight, launchers
//! connecting between them, and stubs firing against whatever build is newest when their
//! need arrives.
//!
//! The machine here is the driver's contract written down (blaze-cargo's
//! `serve/unit/run/launch.rs` and `needs.rs`): what the program hands back for a launcher is
//! written to it in the order it was handed back, a launcher's messages reach the program in
//! the order it sent them, and each `Said::Want` is answered once, at any later step and in
//! any order: with a bind key, by the record the store holds under it; without, by what the
//! newest build makes of the function. Nothing else is ordered. A model launcher applies
//! frames as blaze-launch-rt's control thread does: a record is kept until the next edit, an
//! edit binds the slots it names.
//!
//! The model build: workspace function `i` calls `i + 1` (the last calls none) and the sealed
//! function `s0`, and names workspace static `i % statics`; function 0 is `main`. `s0` calls
//! `s1` and names the sealed static `t`; the sealed ones have bind records in the store and
//! are never edited.

use std::collections::VecDeque;

use blaze_cas::Key;
use blaze_codegen::{
    Arch, CompiledFunction, Init, InstanceKey, RelocKind, Relocation, Relocs, Static, StaticId, Target,
};
use blaze_format::SpanRef;
use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemKey};
use blaze_launch::wire::{self, Ask, ProgramId, RecordId, Tell};
use blaze_launch::{Slot, Thread};
use blaze_link::Position;
use hegel::TestCase;
use hegel::generators as gs;

use crate::bind::{self, Bound, Names, Under};
use crate::program::{Conn, Piece, Plant, Program, Said, Told};
use crate::record::{self, Defines, Positions};
use crate::start::{self, Of};

fn pick(tc: &TestCase, max: usize) -> usize {
    tc.draw(gs::integers::<usize>().max_value(max))
}

fn id(domain: u8, a: u64, b: u64) -> ContentId {
    let mut bytes = [domain; 32];
    bytes[1..9].copy_from_slice(&a.to_le_bytes());
    bytes[9..17].copy_from_slice(&b.to_le_bytes());
    ContentId(bytes)
}

/// A maker's view with no span tables, and the symbols of `defines`.
struct Made<'a> {
    defines: &'a [(&'a str, Defines)],
}

impl Positions for Made<'_> {
    fn of(&self, _: &SpanRef) -> Result<(&str, Position), String> {
        Err("this test's functions name no position".to_owned())
    }

    fn defines(&self, name: &str) -> Option<Defines> {
        self.defines.iter().find(|it| it.0 == name).map(|it| it.1)
    }
}

const NONE: Made<'static> = Made { defines: &[] };

/// What every bind key of the model is made under.
const UNDER: Under = Under {
    policy: ContentId([7; 32]),
    symbols: ContentId([8; 32]),
    maker: ContentId([9; 32]),
};

fn naming() -> ContentId {
    id(10, 0, 0)
}

/// The index of the sealed functions `s0` and `s1`, and of the sealed static `t`.
const SEALED: u64 = 100;
const T: u64 = 200;

/// The source of a program: each workspace function's body version and signature version,
/// and each workspace static's value.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Source {
    bodies: Vec<u64>,
    sigs: Vec<u64>,
    statics: Vec<u64>,
}

impl Source {
    fn new(functions: usize) -> Source {
        Source { bodies: vec![1; functions], sigs: vec![1; functions], statics: vec![7, 7] }
    }
}

/// A function's key: its body's version is in it, as a code hash is in an instance's key.
fn key(function: u64, body: u64) -> InstanceKey {
    InstanceKey(id(1, function, body))
}

fn identity(function: u64) -> ContentId {
    id(2, function, 0)
}

/// The slot identity of `function` at signature `sig`: with `Plant::SlotAcrossAbi`, the
/// driver's planted bug, what it is alone.
fn slot(function: u64, sig: u64, plant: Plant) -> ContentId {
    match plant {
        Plant::SlotAcrossAbi => identity(function),
        _ => bind::slot_identity(&identity(function), &id(5, function, sig)),
    }
}

fn static_id(of: u64) -> StaticId {
    (ItemKey(id(3, of, 0)), ContentId::default())
}

fn sealed_bind(of: u64) -> Option<Key> {
    Some(bind::bind_key(&Key(id(6, of, 1)), &naming(), &UNDER))
}

/// A function of `own` whose code reaches each of `targets` through a GOT word, then holds
/// `body`.
fn compiled(own: InstanceKey, targets: &[Target], body: u64) -> CompiledFunction {
    let mut relocs: Vec<Relocation> = Vec::new();
    let mut code = vec![0u8; targets.len() * 8 + 8];
    for (at, target) in targets.iter().enumerate() {
        let at = at * 8;
        // clone (each): a target of a synthetic function, in a test
        relocs.push(Relocation { at: at as u32, kind: RelocKind::GotPage21, target: target.clone(), addend: 0 });
        relocs.push(Relocation { at: at as u32 + 4, kind: RelocKind::GotPageOffset12, target: target.clone(), addend: 0 });
        // `adrp x16; ldr x16, [x16]`.
        code[at..at + 4].copy_from_slice(&0x9000_0010u32.to_le_bytes());
        code[at + 4..at + 8].copy_from_slice(&0xf940_0210u32.to_le_bytes());
    }
    let end = code.len() - 8;
    code[end..].copy_from_slice(&body.to_le_bytes());
    CompiledFunction {
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
    }
}

/// The bound record of a static of `value`.
fn bound_static(of: u64, value: u64) -> Bound {
    let made = Static {
        item: static_id(of).0,
        instance: ContentId::default(),
        align: 8,
        init: Init::Bytes { bytes: value.to_le_bytes().to_vec(), relocs: Relocs::from_iter([]) },
        data: Vec::new(),
        mutable: false,
        thread_local: false,
        section: None,
        imports: Vec::new(),
    };
    let made = record::of_static(&made, &format!("static {of}"), Arch::Aarch64, &NONE).expect("a static's record");
    Bound { made, slot: ContentId::default(), code: CodeHash(ContentId::default()), names: Vec::new() }
}

/// What a callee or a static is to the function that names it.
fn names_of(callees: &[(InstanceKey, ContentId, Option<Key>)], statics: &[(StaticId, Option<Key>)]) -> Vec<Names> {
    callees
        .iter()
        .map(|(key, slot, bind)| Names::Function { key: *key, slot: *slot, bind: *bind, ask: ask_of(slot) })
        .chain(statics.iter().map(|(id, bind)| Names::Static { id: *id, bind: *bind }))
        .collect()
}

/// The model's portable ask of a function: of its slot identity, as an instance is.
fn ask_of(slot: &ContentId) -> Vec<u8> {
    slot.0.to_vec()
}

/// A whole build of a source: every function's and static's bound record, and the store's
/// bind records of the sealed ones. The reference of the refinement: what a build that gives
/// every piece at its start binds.
struct Built {
    /// By slot identity.
    functions: Vec<(ContentId, Bound)>,
    statics: Vec<(StaticId, Bound)>,
    entry: (InstanceKey, ContentId),
    store: Vec<(Key, Vec<u8>)>,
}

impl Built {
    fn of(&self, slot: &ContentId) -> Option<&Bound> {
        self.functions.iter().find(|it| it.0 == *slot).map(|it| &it.1)
    }

    fn get(&self, key: &Key) -> Result<Bound, String> {
        let bytes = &self.store.iter().find(|it| it.0 == *key).ok_or("no record under a bind key")?.1;
        bind::decode(bytes)
    }
}

fn build(source: &Source, plant: Plant) -> Built {
    let count = source.bodies.len() as u64;
    let (s0, s1) = (SEALED, SEALED + 1);
    let mut functions = Vec::new();
    for at in 0..count {
        let own = key(at, source.bodies[at as usize]);
        let mut callees = Vec::new();
        if at + 1 < count {
            let next = at + 1;
            callees.push((key(next, source.bodies[next as usize]), slot(next, source.sigs[next as usize], plant), None));
        }
        callees.push((key(s0, 1), slot(s0, 1, plant), sealed_bind(s0)));
        let statics = [(static_id(at % source.statics.len() as u64), None)];
        let targets: Vec<Target> = callees
            .iter()
            .map(|it| Target::Function(it.0))
            .chain(statics.iter().map(|it| Target::Static { item: it.0.0, instance: it.0.1 }))
            .collect();
        // The body's code holds its version and its signature's.
        let compiled = compiled(own, &targets, source.bodies[at as usize] ^ (source.sigs[at as usize] << 32));
        let made = record::function(&compiled, &format!("f{at}"), &NONE).expect("a function's record");
        let slot = slot(at, source.sigs[at as usize], plant);
        let code = CodeHash(id(4, at, source.bodies[at as usize]));
        functions.push((slot, Bound { made, slot, code, names: names_of(&callees, &statics) }));
    }
    // The sealed ones: `s0` calls `s1` and names `t`.
    let t = [(static_id(T), sealed_bind(T))];
    let callees = [(key(s1, 1), slot(s1, 1, plant), sealed_bind(s1))];
    let targets = [Target::Function(key(s1, 1)), Target::Static { item: static_id(T).0, instance: static_id(T).1 }];
    let made = record::function(&compiled(key(s0, 1), &targets, 1), "s0", &NONE).expect("a function's record");
    let code = CodeHash(id(4, s0, 1));
    functions.push((slot(s0, 1, plant), Bound { made, slot: slot(s0, 1, plant), code, names: names_of(&callees, &t) }));
    let made = record::function(&compiled(key(s1, 1), &[], 1), "s1", &NONE).expect("a function's record");
    let code = CodeHash(id(4, s1, 1));
    functions.push((slot(s1, 1, plant), Bound { made, slot: slot(s1, 1, plant), code, names: Vec::new() }));
    let mut statics: Vec<(StaticId, Bound)> =
        source.statics.iter().enumerate().map(|(at, value)| (static_id(at as u64), bound_static(at as u64, *value))).collect();
    statics.push((static_id(T), bound_static(T, 42)));
    let store = vec![
        (sealed_bind(s0).expect("sealed"), bind::encode(&functions[count as usize].1)),
        (sealed_bind(s1).expect("sealed"), bind::encode(&functions[count as usize + 1].1)),
        (sealed_bind(T).expect("sealed"), bind::encode(&statics[statics.len() - 1].1)),
    ];
    Built { functions, statics, entry: (key(0, source.bodies[0]), slot(0, source.sigs[0], plant)), store }
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
    /// Slots whose first call was refused.
    refused: Vec<u32>,
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

    fn hello(conn: u64, page: u32) -> Launcher {
        let mut launcher = Launcher { conn, ..Launcher::default() };
        launcher.say(&Ask::Hello {
            program: ProgramId([0; 32]),
            pid: 1,
            arch: wire::Arch::Aarch64,
            os: wire::Os::Linux,
            page,
            holds: None,
            running: false,
        });
        launcher
    }

    /// Reads and carries out its next whole frame; `false` when none is there.
    fn step(&mut self) -> bool {
        let bytes: Vec<u8> = self.inbound.iter().copied().collect();
        let Some((item, took)) = wire::split(&bytes).expect("a frame of the daemon's") else {
            return false;
        };
        let (_, tell) = Tell::decode(item).expect("a message of the daemon's");
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
                self.say(&Ask::Applied { txn, epoch: 0 });
            }
            Tell::Start { .. } => self.started = true,
            Tell::Restart { txn, .. } => {
                self.restarted = true;
                self.say(&Ask::Restarting { txn });
            }
            Tell::Refused { slot, .. } => {
                self.waiting.retain(|it| *it != slot.0);
                self.refused.push(slot.0);
            }
            Tell::Unheld { why } => panic!("unheld: {why}"),
            // Its whole start in one message: the slots declared, the bodies bound, started.
            Tell::Image { bytes } => {
                let image = blaze_launch::image::Image::decode(bytes).expect("an image of placed code");
                assert_eq!(self.declared, 0, "an image is sent to a launcher that has declared nothing");
                self.declared = image.slots;
                self.bound.resize(image.slots as usize, None);
                for body in &image.bodies {
                    assert!(body.slot.0 < self.declared, "an image binds a slot it does not declare");
                    // A bind's code identity is its record's id (`Program::edit`).
                    self.bound[body.slot.0 as usize] = Some(RecordId(body.code.0));
                }
                self.started = true;
            }
            Tell::Probe { .. } => panic!("a probe, which this daemon does not send"),
        }
        drop(self.inbound.drain(..took));
        true
    }
}

/// How many frames of `out` answer a first call of `slot`: edits that bind it, refusals of it.
fn answers(out: &[u8], slot: u32) -> usize {
    let mut at = 0;
    let mut count = 0;
    while let Some((item, took)) = wire::split(&out[at..]).expect("a frame of the daemon's") {
        match Tell::decode(item).expect("a message of the daemon's").1 {
            Tell::Edit { binds, .. } => count += binds.iter().filter(|it| it.slot.0 == slot).count(),
            Tell::Refused { slot: of, .. } if of.0 == slot => count += 1,
            _ => {}
        }
        at += took;
    }
    count
}

/// A first call the program heard: whose, of which slot, and how many answers it was given.
struct Asked {
    conn: u64,
    slot: u32,
    answers: usize,
}

/// A want the driver has not answered yet: which call it is of.
struct Wanted {
    asked: usize,
    conn: u64,
    slot: u32,
    identity: ContentId,
    bind: Option<Key>,
}

/// A copy of `piece`, for the second program of a [`Twin`].
fn copy(piece: &Piece) -> Piece {
    // clone (each): a piece's copy, in a test
    Piece { functions: piece.functions.clone(), statics: piece.statics.clone() }
}

/// The program the daemon holds and, once it was restored from a start image, the one never
/// restored beside it: every message, piece and build is given to both, and the first thing
/// they say differently is kept for the oracle.
struct Twin {
    program: Program,
    never: Option<Program>,
    diverged: Option<String>,
}

impl Twin {
    fn new(plant: Plant) -> Twin {
        let mut program = Program::new();
        program.plant = plant;
        Twin { program, never: None, diverged: None }
    }

    /// `f` of the program and of the one never restored: what the program said, and the
    /// first difference noted.
    fn both<T: PartialEq + std::fmt::Debug>(
        &mut self,
        what: &str,
        out: &mut Vec<u8>,
        mut f: impl FnMut(&mut Program, &mut Vec<u8>) -> T,
    ) -> T {
        let mut ours = Vec::new();
        let said = f(&mut self.program, &mut ours);
        if let Some(never) = &mut self.never {
            let mut theirs = Vec::new();
            let other = f(never, &mut theirs);
            if self.diverged.is_none() && (said != other || ours != theirs) {
                self.diverged = Some(format!(
                    "{what}: the restored program gives {said:?} and {} bytes, the one never restored {other:?} and {} \
                     bytes (equal: {})",
                    ours.len(),
                    theirs.len(),
                    ours == theirs
                ));
            }
        }
        out.extend_from_slice(&ours);
        said
    }

    fn connected(&mut self, conn: Conn) {
        self.both("connected", &mut Vec::new(), |p, _| p.connected(conn));
    }

    fn disconnected(&mut self, conn: Conn) {
        self.both("disconnected", &mut Vec::new(), |p, _| p.disconnected(conn));
    }

    fn said(&mut self, conn: Conn, item: &[u8], out: &mut Vec<u8>) -> Result<Said, String> {
        self.both("said", out, |p, out| p.said(conn, item, out))
    }

    fn took(&mut self, conn: Conn, slot: u32, piece: Piece, out: &mut Vec<u8>) -> Result<(), String> {
        self.both("took", out, |p, out| p.took(conn, slot, copy(&piece), out))
    }

    fn refused(&mut self, conn: Conn, slot: u32, why: &str, out: &mut Vec<u8>) -> Result<(), String> {
        self.both("refused", out, |p, out| p.refused(conn, slot, why, out))
    }

    fn awaited(&mut self) -> Vec<ContentId> {
        self.both("awaited", &mut Vec::new(), |p, _| p.awaited())
    }

    fn started(
        &mut self,
        entry: ContentId,
        symbols: Vec<(String, Names)>,
        inits: Vec<StaticId>,
        piece: Piece,
        changed: &[ContentId],
    ) -> Result<Vec<(Conn, Vec<u8>, Told)>, String> {
        // clone (each): the build's copy for the second program, in a test
        self.both("started", &mut Vec::new(), |p, _| p.started(entry, symbols.clone(), inits.clone(), copy(&piece), changed))
    }

    fn identity(&self, slot: u32) -> Option<ContentId> {
        self.program.identity(slot)
    }

    /// The program's start image, `plant`ed or not, and the never-restored one's beside it:
    /// the two are the same bytes.
    fn kept(&mut self, of: &Of, plant: Plant) -> Result<Option<(Key, Vec<u8>)>, String> {
        let ours = self.program.kept_planted(of, plant)?;
        if let Some(never) = &mut self.never {
            let theirs = never.kept_planted(of, Plant::None)?;
            if self.diverged.is_none() && ours.as_ref().map(|it| &it.1) != theirs.as_ref().map(|it| &it.1) {
                self.diverged = Some(format!(
                    "kept: the restored program's start image is other bytes than the never-restored one's ({:?} and {:?})",
                    ours.as_ref().map(|it| it.1.len()),
                    theirs.as_ref().map(|it| it.1.len())
                ));
            }
        }
        Ok(ours)
    }

    /// The daemon holds `restored` now; the program it held before is the one never restored,
    /// unless one is held already.
    fn restore(&mut self, restored: Program) {
        let before = std::mem::replace(&mut self.program, restored);
        if self.never.is_none() {
            self.never = Some(before);
        }
    }

    /// The oracle of a restore: the two said nothing differently, and give every slot the
    /// same ask and the same awaited identities.
    fn check(&self) -> Result<(), String> {
        if let Some(why) = &self.diverged {
            return Err(why.clone()); // clone: an error
        }
        let Some(never) = &self.never else { return Ok(()) };
        if self.program.awaited() != never.awaited() {
            return Err("the restored program awaits other identities than the one never restored".to_owned());
        }
        for slot in 0.. {
            let Some(identity) = never.identity(slot) else { break };
            if self.program.identity(slot) != Some(identity) {
                return Err(format!("slot {slot} is of another identity in the restored program"));
            }
            if self.program.ask_of(&identity) != never.ask_of(&identity) {
                return Err(format!(
                    "slot {slot}'s ask is {:?} in the restored program and {:?} in the one never restored",
                    self.program.ask_of(&identity).map(<[u8]>::len),
                    never.ask_of(&identity).map(<[u8]>::len)
                ));
            }
        }
        Ok(())
    }
}

/// The program id of the model's program, and the checkout it is built in.
const PROGRAM: [u8; 32] = [3; 32];
const ROOT: &str = "/checkout";

/// The model's record key of a build of `source`: of every input of it.
fn record_of(source: &Source) -> ContentId {
    let mut hasher = blaze_ids::Hasher::new("blaze-launch-host tests record");
    for list in [&source.bodies, &source.sigs, &source.statics] {
        hasher.update(&(list.len() as u64).to_le_bytes());
        for it in list {
            hasher.update(&it.to_le_bytes());
        }
    }
    hasher.finish()
}

/// The daemon (the program and the driver's contract), its launchers, and the newest build.
struct Sim {
    program: Twin,
    /// The store's start images by key: other bytes under a key are refused, the first kept.
    images: Vec<(Key, Vec<u8>)>,
    /// The journal's mark of the program: the key of its newest image.
    mark: Option<Key>,
    /// The driver's planted bug, if any: how it keys slots.
    plant: Plant,
    source: Source,
    built: Built,
    launchers: Vec<Launcher>,
    asked: Vec<Asked>,
    wants: Vec<Wanted>,
    conns: u64,
}

impl Sim {
    fn new(source: Source, plant: Plant) -> Result<Sim, String> {
        let built = build(&source, plant);
        let mut sim = Sim {
            program: Twin::new(plant),
            images: Vec::new(),
            mark: None,
            plant,
            source,
            built: Built { functions: Vec::new(), statics: Vec::new(), entry: built.entry, store: Vec::new() },
            launchers: Vec::new(),
            asked: Vec::new(),
            wants: Vec::new(),
            conns: 0,
        };
        sim.land(built)?;
        Ok(sim)
    }

    /// A build lands, as the driver gives it: its start (the entry and every static) and,
    /// of the functions the program awaits, those it made again. Gives what each launcher
    /// was told.
    fn land(&mut self, built: Built) -> Result<Vec<Told>, String> {
        let before = &self.built;
        let changed: Vec<ContentId> = built
            .functions
            .iter()
            .filter(|(slot, bound)| before.of(slot).is_some_and(|old| old.made.id != bound.made.id))
            .map(|it| it.0)
            .collect();
        let entry = built.entry.1;
        let mut functions = vec![built.of(&entry).expect("the entry").clone()]; // clone: a piece's copy, in a test
        for identity in self.program.awaited() {
            if identity != entry
                && changed.contains(&identity)
                && let Some(bound) = built.of(&identity)
            {
                functions.push(bound.clone()); // clone: as above
            }
        }
        let piece = Piece { functions, statics: built.statics.clone() }; // clone: as above
        let symbols =
            vec![("main".to_owned(), Names::Function { key: built.entry.0, slot: entry, bind: None, ask: ask_of(&entry) })];
        let told = self.program.started(entry, symbols, Vec::new(), piece, &changed)?;
        self.built = built;
        let mut what = Vec::new();
        for (conn, frames, said) in told {
            let Some(launcher) = self.launchers.iter_mut().find(|it| it.conn == conn.0) else {
                return Err(format!("the program told launcher {} something, and it is gone", conn.0));
            };
            launcher.inbound.extend(frames);
            what.push(said);
        }
        Ok(what)
    }

    /// The source changes and its build lands; `fatal` when what changed cannot be taken
    /// live by a launcher that runs.
    fn edit(&mut self, source: Source, fatal: bool) -> Result<Vec<Told>, String> {
        // A launcher that was welcomed before this build runs code of the build before.
        for launcher in self.launchers.iter_mut().filter(|it| it.declared > 0 && !it.restarted) {
            launcher.must_restart |= fatal;
        }
        let built = build(&source, self.plant);
        self.source = source;
        self.land(built)
    }

    fn connect(&mut self, page: u32) -> usize {
        self.conns += 1;
        self.program.connected(Conn(self.conns));
        self.launchers.push(Launcher::hello(self.conns, page));
        self.launchers.len() - 1
    }

    /// The daemon reads launcher `which`'s next message.
    fn hear(&mut self, which: usize) -> Result<bool, String> {
        let launcher = &mut self.launchers[which];
        let Some(message) = launcher.outbound.pop_front() else { return Ok(false) };
        let (_, ask) = Ask::decode(&message).map_err(|why| format!("{why}"))?;
        let need = match ask {
            Ask::Need { slot, .. } => Some(slot.0),
            _ => None,
        };
        let mut out = Vec::new();
        let said = self.program.said(Conn(launcher.conn), &message, &mut out)?;
        if let Some(slot) = need {
            self.asked.push(Asked { conn: launcher.conn, slot, answers: answers(&out, slot) });
        }
        match (said, need) {
            (Said::Told, _) => {}
            (Said::Want { slot, identity, bind }, Some(of)) if slot == of => {
                let asked = self.asked.len() - 1;
                self.wants.push(Wanted { asked, conn: launcher.conn, slot, identity, bind });
            }
            (Said::Want { slot, .. }, _) => return Err(format!("slot {slot} is wanted for a message that is no call of it")),
        }
        launcher.inbound.extend(out);
        Ok(true)
    }

    /// The driver answers the `at`th want still open: from the store under its bind key,
    /// with the statics it names, or with what the newest build makes of its function.
    fn answer(&mut self, at: usize) -> Result<bool, String> {
        if self.wants.is_empty() {
            return Ok(false);
        }
        let want = self.wants.remove(at % self.wants.len());
        let mut out = Vec::new();
        let conn = Conn(want.conn);
        match want.bind {
            Some(key) => {
                let bound = self.built.get(&key)?;
                let mut statics = Vec::new();
                for names in &bound.names {
                    if let Names::Static { id, bind: Some(key) } = names {
                        statics.push((*id, self.built.get(key)?));
                    }
                }
                self.program.took(conn, want.slot, Piece { functions: vec![bound], statics }, &mut out)?;
            }
            None => match self.built.of(&want.identity) {
                Some(bound) => {
                    let piece = Piece { functions: vec![bound.clone()], statics: Vec::new() }; // clone: a piece's copy
                    self.program.took(conn, want.slot, piece, &mut out)?;
                }
                None => self.program.refused(conn, want.slot, "its function is in no build now", &mut out)?,
            },
        }
        self.asked[want.asked].answers += answers(&out, want.slot);
        if let Some(launcher) = self.launchers.iter_mut().find(|it| it.conn == want.conn) {
            launcher.inbound.extend(out);
        }
        Ok(true)
    }

    /// Everything in flight arrives.
    fn settle(&mut self) -> Result<(), String> {
        for _ in 0..10_000 {
            let mut moved = false;
            for which in 0..self.launchers.len() {
                moved |= self.launchers[which].step();
                moved |= self.hear(which)?;
            }
            moved |= self.answer(0)?;
            if !moved {
                return Ok(());
            }
        }
        Err("the daemon and its launchers did not settle in 10,000 rounds".to_owned())
    }

    /// The slot launcher `which` has declared for `identity`.
    fn slot_of(&self, which: usize, identity: &ContentId) -> Option<u32> {
        (0..self.launchers[which].declared).find(|slot| self.program.identity(*slot).as_ref() == Some(identity))
    }

    /// Launcher `which` calls the function of `identity`, and everything settles.
    fn call(&mut self, which: usize, identity: &ContentId) -> Result<(), String> {
        let slot = self.slot_of(which, identity).ok_or("the function is not declared to the launcher")?;
        let launcher = &mut self.launchers[which];
        if launcher.bound[slot as usize].is_none() {
            launcher.waiting.push(slot);
            launcher.say(&Ask::Need { slot: Slot(slot), thread: Thread(1) });
        }
        self.settle()
    }

    /// What the program's start image is of, at the newest build.
    fn of(&self) -> Of {
        Of { program: PROGRAM, record: record_of(&self.source), under: UNDER, root: ROOT.to_owned() }
    }

    /// The daemon stops and starts again, as blaze-cargo's driver does it: its launchers'
    /// sockets end and the wants in flight are answered to no one; the program's image, when
    /// it gives a new one, is put in the store and marked; and the daemon that starts holds
    /// the program the mark's image holds (with `plant` in the keying and the reading), or,
    /// with no mark, the one it held.
    fn restart(&mut self, plant: Plant) -> Result<(), String> {
        for launcher in &self.launchers {
            self.program.disconnected(Conn(launcher.conn));
        }
        self.launchers.clear();
        self.wants.clear();
        let of = self.of();
        if let Some((key, bytes)) = self.program.kept(&of, plant)? {
            if !self.images.iter().any(|it| it.0 == key) {
                self.images.push((key, bytes));
            }
            self.mark = Some(key);
        }
        let Some(mark) = self.mark else { return Ok(()) };
        let bytes = &self.images.iter().find(|it| it.0 == mark).ok_or("the mark's image is not in the store")?.1;
        let (restored, read) = start::decode(bytes, &PROGRAM, plant)?;
        if read != of {
            return Err(format!("the image marked is of {read:?}, and the newest build of {of:?}"));
        }
        self.program.restore(restored);
        Ok(())
    }

    /// The oracle.
    fn check(&self) -> Result<(), String> {
        self.program.check()?;
        for launcher in &self.launchers {
            if launcher.restarted {
                continue;
            }
            if launcher.must_restart {
                return Err(format!(
                    "launcher {} runs on after a build it cannot take live (a static it defined changed)",
                    launcher.conn
                ));
            }
            if !launcher.waiting.is_empty() {
                return Err(format!("launcher {} still waits for slots {:?}", launcher.conn, launcher.waiting));
            }
            for (slot, bound) in launcher.bound.iter().enumerate() {
                let Some(bound) = bound else { continue };
                let identity = self.program.identity(slot as u32).ok_or("a bound slot the program never gave")?;
                // A slot whose function left the build is named by no code any more.
                let Some(reference) = self.built.of(&identity) else { continue };
                if reference.made.id != *bound {
                    return Err(format!(
                        "launcher {}'s slot {slot} holds a record that is not the newest build's",
                        launcher.conn
                    ));
                }
            }
        }
        for asked in &self.asked {
            let restarted = self.launchers.iter().find(|it| it.conn == asked.conn).is_none_or(|it| it.restarted);
            if !restarted && asked.answers != 1 {
                return Err(format!(
                    "launcher {}'s first call of slot {} was answered {} times",
                    asked.conn, asked.slot, asked.answers
                ));
            }
        }
        Ok(())
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
    /// The driver answers one of the wants still open.
    Answer(usize),
}

fn op(tc: &TestCase) -> Op {
    match pick(tc, 15) {
        0 | 1 => Op::OneBody(pick(tc, 7)),
        2 => Op::TwoBodies(pick(tc, 7)),
        3 => Op::Static,
        4 => Op::Added,
        5 => Op::Connect,
        6..=8 => Op::Read(pick(tc, 3)),
        9 | 10 => Op::Heard(pick(tc, 3)),
        11 | 12 => Op::Need(pick(tc, 3), pick(tc, 9)),
        _ => Op::Answer(pick(tc, 7)),
    }
}

/// Runs `ops` against a program with `plant`, then lets everything in flight arrive, and
/// checks the oracle. Gives what broke, when something did.
fn run(ops: &[Op], plant: Plant) -> Result<(), String> {
    let mut sim = Sim::new(Source::new(3), plant)?;
    let mut version = 1u64;
    for op in ops {
        step(&mut sim, *op, &mut version)?;
    }
    sim.settle()?;
    sim.check()
}

/// Carries out one operation of a schedule; `version` is the last source version given.
fn step(sim: &mut Sim, op: Op, version: &mut u64) -> Result<(), String> {
    match op {
        Op::OneBody(function) => {
            *version += 1;
            let mut source = sim.source.clone(); // clone: the next source, in a test
            let at = function % source.bodies.len();
            source.bodies[at] = *version;
            sim.edit(source, false)?;
        }
        Op::TwoBodies(function) => {
            *version += 1;
            // Two functions that are not `main` (whose shim would hide one), switched
            // together: one edit.
            let mut source = sim.source.clone(); // clone: as above
            let count = source.bodies.len();
            let (a, b) = (1 + function % (count - 1), 1 + (function + 1) % (count - 1));
            source.bodies[a] = *version;
            source.bodies[b] = *version;
            sim.edit(source, false)?;
        }
        Op::Static => {
            *version += 1;
            let mut source = sim.source.clone(); // clone: as above
            source.statics[0] = *version;
            sim.edit(source, true)?;
        }
        Op::Added => {
            *version += 1;
            // The last function's callee is new: its body changes with the addition.
            let mut source = sim.source.clone(); // clone: as above
            let last = source.bodies.len() - 1;
            source.bodies[last] = *version;
            source.bodies.push(*version);
            source.sigs.push(1);
            sim.edit(source, false)?;
        }
        Op::Connect => {
            if sim.launchers.len() < 4 {
                sim.connect(4096);
            }
        }
        Op::Read(which) => {
            if let Some(launcher) = sim.launchers.get_mut(which) {
                launcher.step();
            }
        }
        Op::Heard(which) => {
            if which < sim.launchers.len() {
                sim.hear(which)?;
            }
        }
        Op::Need(which, slot) => {
            if let Some(launcher) = sim.launchers.get_mut(which)
                && launcher.started
                && !launcher.restarted
                && (slot as u32) < launcher.declared
                && launcher.bound[slot].is_none()
                && !launcher.waiting.contains(&(slot as u32))
                && !launcher.refused.contains(&(slot as u32))
            {
                launcher.waiting.push(slot as u32);
                launcher.say(&Ask::Need { slot: Slot(slot as u32), thread: Thread(1) });
            }
        }
        Op::Answer(at) => {
            sim.answer(at)?;
        }
    }
    Ok(())
}

/// The invariant: once a launcher has carried out everything it was sent, each of its bound
/// slots holds the record the newest build makes of that slot's function, or the launcher
/// was told to start again; and a launcher that ran through a build it cannot take live (a
/// static it defined changed) was told to start again. So an edit that lands while another
/// is in flight, or while a stub's need or the driver's answer to it is, never leaves code of
/// an old input bound.
#[hegel::test(test_cases = 300)]
fn no_schedule_leaves_a_launcher_on_code_of_an_old_build(tc: TestCase) {
    let ops: Vec<Op> = (0..pick(&tc, 60)).map(|_| op(&tc)).collect();
    if let Err(why) = run(&ops, Plant::None) {
        panic!("{why}\nops: {ops:?}");
    }
}

/// The refinement: for any program, call order and interleaving of builds, each slot a
/// launcher has bound holds the record id the whole-build reference (`Built`, every piece of
/// the build given at its start) holds for it: a program given only its start, and each
/// first call's piece when it is asked for (by bind key from the store, or made by the newest
/// build), binds what one given everything binds.
#[hegel::test(test_cases = 200)]
fn a_lazy_program_binds_what_the_whole_build_binds(tc: TestCase) {
    let functions = 2 + pick(&tc, 3);
    let ops: Vec<Op> = (0..pick(&tc, 60)).map(|_| op(&tc)).collect();
    let checked = (|| {
        let mut sim = Sim::new(Source::new(functions), Plant::None)?;
        let mut version = 1;
        for op in &ops {
            match *op {
                Op::OneBody(at) => {
                    version += 1;
                    let mut source = sim.source.clone(); // clone: the next source, in a test
                    let at = at % source.bodies.len();
                    source.bodies[at] = version;
                    sim.edit(source, false)?;
                }
                Op::Connect if sim.launchers.len() < 3 => {
                    sim.connect(4096);
                }
                Op::Read(which) | Op::Heard(which) if which < sim.launchers.len() => {
                    sim.launchers[which].step();
                    sim.hear(which)?;
                }
                Op::Need(which, at) if which < sim.launchers.len() && sim.launchers[which].started => {
                    // A call of a function the launcher has declared, by the build's order.
                    let identity = sim.built.functions[at % sim.built.functions.len()].0;
                    if let Some(slot) = sim.slot_of(which, &identity) {
                        let launcher = &mut sim.launchers[which];
                        if !launcher.restarted && launcher.bound[slot as usize].is_none() && !launcher.waiting.contains(&slot) {
                            launcher.waiting.push(slot);
                            launcher.say(&Ask::Need { slot: Slot(slot), thread: Thread(1) });
                        }
                    }
                }
                Op::Answer(at) => {
                    sim.answer(at)?;
                }
                _ => {}
            }
        }
        sim.settle()?;
        sim.check()
    })();
    if let Err(why) = checked {
        panic!("{why}\nops: {ops:?}");
    }
}

/// The invariant: every first call a launcher that runs on makes is answered exactly once,
/// by an edit that binds it or by a refusal of it, whether the program held its record, the
/// driver read it from the store or the newest build made it, and however the answers
/// interleave with builds and other launchers' calls.
#[hegel::test(test_cases = 200)]
fn every_need_is_answered_once(tc: TestCase) {
    let ops: Vec<Op> = (0..pick(&tc, 60)).map(|_| op(&tc)).collect();
    if let Err(why) = run(&ops, Plant::None) {
        panic!("{why}\nops: {ops:?}");
    }
}

/// The schedules that show each planted bug: a launcher whose entry is bound by its start,
/// which then calls `f1` (wanted, answered by the driver), then an edit.
fn bound_then(edit: Op) -> Vec<Op> {
    vec![Op::Connect, Op::Heard(0), Op::Read(0), Op::Need(0, 1), Op::Heard(0), Op::Answer(0), Op::Read(0), Op::Read(0), edit]
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

/// The control: a daemon that sends a changed static as an edit leaves a launcher running on
/// the old value, and the oracle says so.
#[test]
fn the_oracle_catches_a_changed_static_sent_as_an_edit() {
    let ops = bound_then(Op::Static);
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
    let ops = bound_then(Op::Need(0, 2));
    assert_eq!(run(&ops, Plant::None), Ok(()));
    let caught = std::panic::catch_unwind(|| run(&ops, Plant::DeclareFirst));
    assert!(caught.is_err(), "a bind that names an undeclared slot was taken");
}

/// The control of [`a_lazy_program_binds_what_the_whole_build_binds`]: a program that keeps
/// the record it had of a function where a piece gives it the newest build's leaves the
/// launcher on the old body, and the oracle says so.
#[test]
fn the_oracle_catches_a_piece_that_does_not_replace_the_record_held() {
    let ops = bound_then(Op::OneBody(0));
    assert_eq!(run(&ops, Plant::None), Ok(()));
    let caught = run(&ops, Plant::StaleSlot).unwrap_err();
    assert!(caught.contains("not the newest build's"), "{caught}");
}

/// The control of [`every_need_is_answered_once`]: a program that answers a call of a slot
/// it holds and also wants it has it answered twice, and the oracle says so. Two launchers:
/// the first's call of `f1` makes it held, the second then calls it.
#[test]
fn the_oracle_catches_a_need_answered_twice() {
    let ops = vec![
        Op::Connect,
        Op::Connect,
        Op::Heard(0),
        Op::Heard(1),
        Op::Read(0),
        Op::Read(1),
        Op::Need(0, 1),
        Op::Heard(0),
        Op::Answer(0),
        Op::Need(1, 1),
        Op::Heard(1),
    ];
    assert_eq!(run(&ops, Plant::None), Ok(()));
    let caught = run(&ops, Plant::WantTwice).unwrap_err();
    assert!(caught.contains("answered 2 times"), "{caught}");
}

/// A launcher that has started and called every workspace function of the build.
fn running(sim: &mut Sim) -> usize {
    let which = sim.connect(4096);
    sim.settle().expect("started");
    for at in 1..sim.source.bodies.len() as u64 {
        let identity = slot(at, sim.source.sigs[at as usize], sim.plant);
        sim.call(which, &identity).expect("called");
    }
    which
}

/// What a build is classified as, one case each (the table in `program`'s documentation).
#[test]
fn a_build_is_an_edit_a_declaration_or_a_restart() {
    let told = |edit: Op| -> Vec<Told> {
        let mut sim = Sim::new(Source::new(3), Plant::None).expect("the first build");
        let which = running(&mut sim);
        assert!(sim.launchers[which].bound.iter().filter(|it| it.is_some()).count() >= 3);
        let mut source = sim.source.clone(); // clone: the next source, in a test
        match edit {
            Op::OneBody(at) => source.bodies[at] = 2,
            Op::TwoBodies(_) => (source.bodies[1], source.bodies[2]) = (2, 2),
            Op::Static => source.statics[0] = 8,
            Op::Added => {
                source.bodies[2] = 2;
                source.bodies.push(2);
                source.sigs.push(1);
            }
            _ => {}
        }
        sim.edit(source, false).expect("built")
    };
    // One body: its slot, switched together. (Its caller's record names it by place: the
    // same record, not bound again.)
    assert_eq!(told(Op::OneBody(2)), [Told::Edit { binds: 1, together: true }]);
    // The entry itself (here function 0 is the entry: the shim the front end makes around
    // `fn main`) is not counted as a changed body: bound again, with nothing to gate.
    assert_eq!(told(Op::OneBody(0)), [Told::Edit { binds: 1, together: false }]);
    // A function added: the caller that now calls it is rebound, and the slot declared.
    assert_eq!(told(Op::Added), [Told::Edit { binds: 1, together: true }]);
    // Two bodies whose slots are keyed by their ABIs: one edit, switched together.
    assert_eq!(told(Op::TwoBodies(0)), [Told::Edit { binds: 2, together: true }]);
    assert!(matches!(&told(Op::Static)[..], [Told::Restart { why }] if why.contains("static")));
    // Nothing changed: nothing is said.
    assert_eq!(told(Op::Connect), [Told::Nothing]);
}

/// A record made of a compiled function (the table in `record`'s documentation): a callee is
/// a function place the bind numbers; a symbol the program defines is a function place the
/// program's own symbol fills, by name; a symbol nothing of the program defines is an
/// outside symbol by name; a `Direct` of the function itself is its own bytes; a datum keeps
/// its place; and a position becomes two data, the file's name and the `Location` that
/// points at it, with the span table it was read from noted.
#[test]
fn a_record_names_callees_by_place_and_carries_its_positions_as_data() {
    use blaze_codegen::Data;
    use blaze_launch::record::{Kind, Place, Record, To};
    use blaze_link::{Span, SpanTable};

    use crate::record::Named;

    struct Build {
        anchor: ContentId,
        table: SpanTable,
    }
    impl Positions for Build {
        fn of(&self, span: &SpanRef) -> Result<(&str, Position), String> {
            if span.anchor != self.anchor {
                return Err("no table of that anchor".to_owned());
            }
            let position = self.table.spans[span.index as usize]
                .resolve(&self.table.bases)
                .ok_or_else(|| "a span of no base".to_owned())?;
            Ok((&self.table.files[position.file as usize], position))
        }

        fn defines(&self, name: &str) -> Option<Defines> {
            (name == "__rust_enable_lse").then_some(Defines::Function)
        }
    }
    let (own, callee) = (key(0, 1), key(1, 1));
    let anchor = id(9, 1, 0);
    let build = Build {
        anchor,
        table: SpanTable {
            files: vec!["src/main.rs".to_owned()],
            spans: vec![Span::From { base: 0, line: 0, column: 0 }],
            bases: vec![Position { file: 0, line: 4, column: 17 }],
        },
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
    assert_eq!(made.named, [Named::Function(callee)]);
    assert_eq!(made.symbols, [(1, "__rust_enable_lse".to_owned())]);
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

/// What a running launcher holds after two edits of a program (`main` calls `f1`, which
/// calls `f2`; the launcher has run all three): first the bodies of `f1` and `f2` change
/// together, then `f2`'s signature changes, and with it its body and the body of `f1`, its
/// caller.
struct Edited {
    /// Whether either build told the launcher to start again.
    restarted: bool,
    /// Whether the first build's edit switched its bodies together, and how many it bound.
    first: Option<(usize, bool)>,
    /// The record in `f2`'s slot before the signature changed and after.
    before: Option<RecordId>,
    after: Option<RecordId>,
    /// How many slots it had declared before the signature changed and after.
    declared: (u32, u32),
    /// Whether `f1`'s slot holds the newest build's record at the end.
    caller_is_newest: bool,
}

fn edited(plant: Plant) -> Edited {
    let mut sim = Sim::new(Source::new(3), plant).expect("built");
    let which = running(&mut sim);
    let f2 = sim.slot_of(which, &slot(2, 1, plant)).expect("f2 has a slot");
    assert!(sim.launchers[which].waiting.is_empty() && sim.launchers[which].bound[f2 as usize].is_some());
    let land = |sim: &mut Sim, source: Source| {
        let told = sim.edit(source, false).expect("built");
        sim.settle().expect("settled");
        told.into_iter().find_map(|it| match it {
            Told::Edit { binds, together } => Some((binds, together)),
            _ => None,
        })
    };
    // Two bodies, no signature.
    let mut source = sim.source.clone(); // clone: the next source, in a test
    (source.bodies[1], source.bodies[2]) = (2, 2);
    let first = land(&mut sim, source);
    let (before, declared_before) = (sim.launchers[which].bound[f2 as usize], sim.launchers[which].declared);
    // A signature, with its body and its caller's.
    let mut source = sim.source.clone(); // clone: as above
    (source.sigs[2], source.bodies[1], source.bodies[2]) = (2, 3, 3);
    land(&mut sim, source);
    let f1 = sim.slot_of(which, &slot(1, 1, plant)).expect("f1 has a slot");
    let launcher = &sim.launchers[which];
    let caller_is_newest = sim.built.of(&slot(1, 1, plant)).map(|it| it.made.id) == launcher.bound[f1 as usize];
    Edited {
        restarted: launcher.restarted,
        first,
        before,
        after: launcher.bound[f2 as usize],
        declared: (declared_before, launcher.declared),
        caller_is_newest,
    }
}

/// The invariant: a slot holds bodies of one ABI. Bodies that change together are one edit
/// of a running program, switched together, and a function whose signature changes is
/// another slot: the slot of the old signature keeps the body the frames of its old callers
/// call, the caller is rebound to code that names the new slot, and the program is not
/// started again for either.
#[test]
fn a_changed_signature_is_another_slot_and_changed_bodies_are_one_edit() {
    let edited = edited(Plant::None);
    assert!(!edited.restarted, "neither edit starts the program again");
    assert!(matches!(edited.first, Some((binds, true)) if binds >= 2), "{:?}", edited.first);
    assert!(edited.before.is_some());
    assert_eq!(edited.after, edited.before, "the old signature's slot keeps its body");
    assert_eq!(edited.declared.1, edited.declared.0 + 1, "the new signature is one more slot");
    assert!(edited.caller_is_newest, "the caller runs the code that names the new slot");
}

/// A control of [`a_changed_signature_is_another_slot_and_changed_bodies_are_one_edit`]: a
/// slot keyed by what its function is alone is given the body of the new signature, which
/// the frames of its old callers then call.
#[test]
fn the_oracle_catches_a_slot_kept_across_a_changed_signature() {
    let edited = edited(Plant::SlotAcrossAbi);
    assert!(edited.before.is_some());
    assert_ne!(edited.after, edited.before);
    assert_eq!(edited.declared.1, edited.declared.0);
}

/// A launcher's start is one message, an image of placed code, that binds the start (the
/// entry) and every function the runs before it called, each to the newest build's record;
/// and the next launcher of the same build and set is sent the same bytes.
#[test]
fn a_launcher_starts_from_one_message_that_binds_what_the_runs_before_called() {
    let mut sim = Sim::new(Source::new(3), Plant::None).expect("built");
    // A first run, which calls the entry and what it calls.
    let first = sim.connect(4096);
    sim.settle().expect("started");
    let entry = sim.built.entry.1;
    let bound: Vec<usize> = (0..sim.launchers[first].bound.len()).filter(|it| sim.launchers[first].bound[*it].is_some()).collect();
    assert!(sim.launchers[first].started && bound == [0], "only the entry is bound before anything ran: {bound:?}");
    let f1 = slot(1, 1, Plant::None);
    sim.call(first, &f1).expect("called");
    let answer = |sim: &mut Sim| {
        let which = sim.connect(4096);
        let said = sim.launchers[which].outbound.pop_front().expect("its hello");
        let mut out = Vec::new();
        let conn = Conn(sim.launchers[which].conn);
        sim.program.said(conn, &said, &mut out).expect("heard");
        let (item, took) = wire::split(&out).expect("a frame").expect("a whole frame");
        assert_eq!(took, out.len(), "one message");
        assert!(matches!(Tell::decode(item), Ok((_, Tell::Image { .. }))));
        sim.launchers[which].inbound.extend(out.iter().copied());
        sim.settle().expect("settled");
        (which, out)
    };
    let (second, sent) = answer(&mut sim);
    assert!(sim.launchers[second].started);
    for identity in [entry, f1] {
        let slot = sim.slot_of(second, &identity).expect("declared");
        assert!(sim.launchers[second].bound[slot as usize].is_some(), "slot {slot} is bound before the program starts");
        assert_eq!(sim.launchers[second].bound[slot as usize], sim.built.of(&identity).map(|it| it.made.id));
    }
    let (third, again) = answer(&mut sim);
    assert_eq!(sim.launchers[third].bound, sim.launchers[second].bound);
    // The same image; the frame differs in nothing but would in its id were it made again.
    assert_eq!(again, sent);
}

/// A program an image cannot hold is not started another way: the launcher's hello is
/// refused, naming the case, what of the program it is and what to do. Here a launcher in
/// pages of 4 bytes, past which every static of the program is aligned.
#[test]
fn a_program_an_image_cannot_hold_is_refused_by_name_with_what_to_do() {
    let mut sim = Sim::new(Source::new(3), Plant::None).expect("built");
    let which = sim.connect(4);
    let said = sim.launchers[which].outbound.pop_front().expect("its hello");
    let mut out = Vec::new();
    let why = sim.program.said(Conn(sim.launchers[which].conn), &said, &mut out).expect_err("refused");
    assert!(out.is_empty(), "nothing is sent of a start that is refused");
    assert!(why.contains("aligned past a page"), "{why}");
    assert!(why.contains("aligned to 8 bytes"), "{why}");
    assert!(why.contains("Run it without the launcher"), "{why}");
}

/// A bound record of a function that names `callees` functions, `statics` statics, the
/// program's own function `lse` and static `tls` when asked, and the outside `write`; with
/// the bind keys `binds` draws.
fn drawn(callees: usize, statics: usize, symbols: (bool, bool), outside: bool, binds: &[bool], anchors: &[u8]) -> Bound {
    let callee = |at: usize| (key(300 + at as u64, 1), id(11, at as u64, 0));
    let mut targets: Vec<Target> = (0..callees).map(|at| Target::Function(callee(at).0)).collect();
    targets.extend((0..statics).map(|at| Target::Static { item: static_id(at as u64).0, instance: static_id(at as u64).1 }));
    if symbols.0 {
        targets.push(Target::Extern("lse".to_owned()));
    }
    if symbols.1 {
        targets.push(Target::Extern("tls".to_owned()));
    }
    if outside {
        targets.push(Target::Extern("write".to_owned()));
    }
    let positions = Made { defines: &[("lse", Defines::Function), ("tls", Defines::Static)] };
    let mut made = record::function(&compiled(key(400, 1), &targets, 9), "drawn", &positions).expect("a record");
    made.anchors = anchors.iter().map(|it| id(12, u64::from(*it), 0)).collect();
    made.anchors.sort_unstable();
    made.anchors.dedup();
    let bind = |at: usize| binds.get(at).copied().unwrap_or(false).then(|| Key(id(13, at as u64, 0)));
    let mut names: Vec<Names> = (0..callees)
        // A callee's ask: none for the first, as for one with no portable ask.
        .map(|at| Names::Function { key: callee(at).0, slot: callee(at).1, bind: bind(at), ask: vec![at as u8; at * 3] })
        .collect();
    names.extend((0..statics).map(|at| Names::Static { id: static_id(at as u64), bind: bind(callees + at) }));
    if symbols.0 {
        names.push(Names::Symbol("lse".to_owned()));
    }
    if symbols.1 {
        names.push(Names::Symbol("tls".to_owned()));
    }
    Bound { made, slot: id(14, 0, 0), code: CodeHash(id(15, 0, 0)), names }
}

/// What a launcher that calls `bound`'s function is told when the driver answers its want
/// with `bound`: the frames, and the record its slot holds after it read them.
fn bound_by(bound: Bound) -> (Result<(), String>, Vec<u8>, Option<RecordId>) {
    let mut program = Program::new();
    let kb = key(400, 1);
    let entry = compiled(key(0, 1), &[Target::Function(kb)], 1);
    let made = record::function(&entry, "main", &NONE).expect("a record");
    let entry_slot = id(14, 1, 0);
    let bind = Some(Key(id(16, 0, 0)));
    let entry = Bound {
        made,
        slot: entry_slot,
        code: CodeHash(id(15, 1, 0)),
        names: vec![Names::Function { key: kb, slot: bound.slot, bind, ask: ask_of(&bound.slot) }],
    };
    let statics: Vec<(StaticId, Bound)> = (0..2).map(|at| (static_id(at), bound_static(at, at))).chain([(static_id(50), bound_static(50, 5))]).collect();
    let symbols = vec![
        ("main".to_owned(), Names::Function { key: key(0, 1), slot: entry_slot, bind: None, ask: ask_of(&entry_slot) }),
        ("lse".to_owned(), Names::Function { key: key(77, 1), slot: id(14, 77, 0), bind: None, ask: Vec::new() }),
        ("tls".to_owned(), Names::Static { id: static_id(50), bind: None }),
    ];
    program.started(entry_slot, symbols, Vec::new(), Piece { functions: vec![entry], statics }, &[]).expect("started");
    program.connected(Conn(1));
    let mut launcher = Launcher::hello(1, 4096);
    let mut out = Vec::new();
    program.said(Conn(1), &launcher.outbound.pop_front().expect("hello"), &mut out).expect("welcomed");
    launcher.inbound.extend(out);
    while launcher.step() {}
    let slot = (0..launcher.declared).find(|it| program.identity(*it) == Some(bound.slot)).expect("declared");
    launcher.say(&Ask::Need { slot: Slot(slot), thread: Thread(1) });
    let mut out = Vec::new();
    let said = program.said(Conn(1), &launcher.outbound.pop_front().expect("its call"), &mut out).expect("heard");
    assert_eq!(said, Said::Want { slot, identity: bound.slot, bind }, "the call is wanted with its bind key");
    let mut out = Vec::new();
    let took = program.took(Conn(1), slot, Piece { functions: vec![bound], statics: Vec::new() }, &mut out);
    launcher.inbound.extend(out.iter().copied());
    if took.is_ok() {
        while launcher.step() {}
    }
    (took, out, launcher.bound.get(slot as usize).copied().flatten())
}

/// The invariant: `decode(encode(b)) == b` for any bound record (byte for byte: its encoding
/// again is the same bytes), and `took` of the decoded record leaves the launcher as `took` of
/// `b`: the same frames, and its slot bound to `b`'s record.
#[hegel::test(test_cases = 200)]
fn a_bind_record_reads_back_and_binds_as_its_piece(tc: TestCase) {
    let callees = pick(&tc, 3);
    let statics = pick(&tc, 2);
    let symbols = (tc.draw(gs::booleans()), tc.draw(gs::booleans()));
    let outside = tc.draw(gs::booleans());
    let binds: Vec<bool> = (0..callees + statics).map(|_| tc.draw(gs::booleans())).collect();
    let anchors: Vec<u8> = (0..pick(&tc, 3)).map(|_| tc.draw(gs::integers::<u8>())).collect();
    let bound = drawn(callees, statics, symbols, outside, &binds, &anchors);
    let bytes = bind::encode(&bound);
    let read = bind::decode(&bytes).expect("a bind record reads back");
    assert_eq!(read, bound);
    assert_eq!(bind::encode(&read), bytes);
    let id = bound.made.id;
    let (took, frames, held) = bound_by(bound);
    let (took_read, frames_read, held_read) = bound_by(read);
    assert_eq!(took, Ok(()));
    assert_eq!((took_read, frames_read, held_read), (took, frames, held));
    assert_eq!(held, Some(id), "the launcher's slot holds the record");
}

/// The controls of [`a_bind_record_reads_back_and_binds_as_its_piece`]: a bind record cut
/// short anywhere, or of another layout, is refused naming the offset; and one read without
/// its static names (`Plant::DropsStaticNames`) leaves a static undefined, which `took`
/// refuses by name.
#[test]
fn a_bind_record_that_is_cut_short_of_another_layout_or_without_its_statics_is_refused() {
    let bound = drawn(1, 1, (true, true), true, &[true, true], &[3]);
    let bytes = bind::encode(&bound);
    for cut in [0, 7, 8, 9, 13, bytes.len() / 2, bytes.len() - 33, bytes.len() - 1] {
        let refused = bind::decode(&bytes[..cut]).expect_err("cut short");
        assert!(refused.contains("offset"), "{cut}: {refused}");
    }
    let mut other = bytes.clone(); // clone: a test's copy
    other[..8].copy_from_slice(b"bzbind00");
    let refused = bind::decode(&other).expect_err("another layout");
    assert!(refused.contains("offset 0") && refused.contains("layout"), "{refused}");
    let mut longer = bytes.clone(); // clone: as above
    longer.push(0);
    assert!(bind::decode(&longer).expect_err("a byte past its end").contains("offset"));

    let (took, _, _) = bound_by(bind::read(&bytes, Plant::None).expect("reads"));
    assert_eq!(took, Ok(()));
    let dropped = bind::read(&bytes, Plant::DropsStaticNames).expect("reads, planted");
    let (took, frames, _) = bound_by(dropped);
    let refused = took.expect_err("a record whose static is not named");
    assert!(refused.contains("static") && refused.contains("`drawn`"), "{refused}");
    assert!(frames.is_empty(), "nothing is sent of a piece that is refused");
}

/// Runs `ops` with the daemon stopping and starting again before each op marked so, then a
/// last start and a launcher of it, and checks the oracle with the restore's: what the
/// program the daemon holds says equals, byte for byte, what the one never restored says.
fn restarted(ops: &[(bool, Op)], plant: Plant) -> Result<(), String> {
    let mut sim = Sim::new(Source::new(3), Plant::None)?;
    let mut version = 1u64;
    for (restart, op) in ops {
        if *restart {
            sim.restart(plant)?;
        }
        step(&mut sim, *op, &mut version)?;
    }
    sim.settle()?;
    sim.restart(plant)?;
    sim.connect(4096);
    sim.settle()?;
    sim.check()
}

/// The refinement: for any program, builds, launchers and call order, a program made by
/// `restored(kept())` at any point where `kept` gives an image (and, where it gives none,
/// from the image last marked), then given the same later messages, pieces and builds as the
/// one never restored, appends byte for byte the same frames and returns the same `Said`s,
/// `awaited()` and `ask_of`; and each image it keeps is byte for byte the one the never
/// restored one keeps at the same point. The controls are
/// [`the_oracle_catches_a_restore_that_drops_needed_forgets_asks_or_keys_by_identity_alone`].
#[hegel::test(test_cases = 200)]
fn a_restored_program_tells_what_the_kept_one_tells(tc: TestCase) {
    let ops: Vec<(bool, Op)> = (0..pick(&tc, 60)).map(|_| (pick(&tc, 7) == 0, op(&tc))).collect();
    if let Err(why) = restarted(&ops, Plant::None) {
        panic!("{why}\nops: {ops:?}");
    }
}

/// A launcher that started and called `f1`.
fn ran_f1(sim: &mut Sim) -> Result<(), String> {
    let which = sim.connect(4096);
    sim.settle()?;
    sim.call(which, &slot(1, 1, Plant::None))
}

/// The controls of [`a_restored_program_tells_what_the_kept_one_tells`], on one schedule: a
/// run starts, the daemon starts again, a run calls `f1`, the daemon starts again, a run
/// starts. A restore that leaves out the slots the runs called starts the last run without
/// `f1` bound; one that reads no ask gives the front end nothing to make a first call from;
/// and an image keyed without the hash of its body is keyed as the first image of the same
/// build, which the store keeps, so the daemon restores the program before `f1` was called.
#[test]
fn the_oracle_catches_a_restore_that_drops_needed_forgets_asks_or_keys_by_identity_alone() {
    let schedule = |plant: Plant| -> Result<(), String> {
        let mut sim = Sim::new(Source::new(3), Plant::None)?;
        sim.connect(4096);
        sim.settle()?;
        sim.restart(plant)?;
        ran_f1(&mut sim)?;
        sim.restart(plant)?;
        sim.connect(4096);
        sim.settle()?;
        sim.check()
    };
    assert_eq!(schedule(Plant::None), Ok(()));
    let caught = schedule(Plant::DropsNeeded).expect_err("a restore without the slots the runs called");
    assert!(caught.contains("said"), "{caught}");
    // Caught where its next image is kept (other asks are other bytes) or at the end.
    let caught = schedule(Plant::ForgetsAsks).expect_err("a restore without asks");
    assert!(caught.contains("ask") || caught.contains("kept"), "{caught}");
    let caught = schedule(Plant::KeyOfIdentityAlone).expect_err("a stale image under the newer one's key");
    assert!(caught.contains("said"), "{caught}");
}

/// The invariant (rule 26): a start image reads back as the program that was kept, whose own
/// image is the same bytes, and a daemon holding it then says what the program never
/// restored says; an image of another layout (`bzstart0`, or holding a bind record of
/// `bzbind01`), cut anywhere, with a byte flipped, or of another program is refused, naming
/// the offset or both programs' ids.
#[test]
fn a_start_image_round_trips_and_another_layout_is_refused_by_name() {
    let mut sim = Sim::new(Source::new(3), Plant::None).expect("built");
    ran_f1(&mut sim).expect("ran");
    // The daemon starts again from its image.
    sim.restart(Plant::None).expect("restarted");
    let of = sim.of();
    let (key, bytes) = sim.images.last().cloned().expect("an image"); // clone: a test's copy
    assert_eq!(sim.mark, Some(key));
    assert_eq!(key, start::start_key(&of, &ContentId(bytes[8..40].try_into().expect("32 bytes"))));
    let (restored, read) = Program::restored(&bytes, &PROGRAM).expect("reads back");
    assert_eq!(read, of);
    assert_eq!(start::encode(&restored, &of).expect("encodes").0, bytes, "its image again is the same bytes");
    assert_eq!(sim.program.kept(&of, Plant::None).expect("kept"), None, "nothing changed: no image");

    // A run goes further, and the daemon starts again.
    let which = sim.connect(4096);
    sim.settle().expect("started");
    sim.call(which, &slot(1, 1, Plant::None)).expect("f1, bound in its start");
    sim.call(which, &slot(2, 1, Plant::None)).expect("f2, a first call");
    sim.restart(Plant::None).expect("restarted");
    sim.connect(4096);
    sim.settle().expect("started");
    assert_eq!(sim.check(), Ok(()));

    let mut other = bytes.clone(); // clone: a test's copy
    other[..8].copy_from_slice(b"bzstart0");
    let refused = Program::restored(&other, &PROGRAM).err().expect("another layout");
    assert!(refused.contains("offset 0") && refused.contains("bzstart0") && refused.contains("bzstart1"), "{refused}");
    for cut in 0..bytes.len() {
        let mut cut_short = bytes[..cut].to_vec();
        if cut >= 40 {
            start::seal(&mut cut_short);
        }
        let refused = Program::restored(&cut_short, &PROGRAM).err().expect("cut short");
        assert!(refused.contains("offset"), "{cut}: {refused}");
    }
    let mut flipped = bytes.clone(); // clone: as above
    flipped[bytes.len() / 2] ^= 1;
    let refused = Program::restored(&flipped, &PROGRAM).err().expect("a byte flipped");
    assert!(refused.contains("offset 8") && refused.contains("check"), "{refused}");
    let another = [4; 32];
    let refused = Program::restored(&bytes, &another).err().expect("another program's");
    assert!(
        refused.contains(&format!("{:?}", ContentId(PROGRAM))) && refused.contains(&format!("{:?}", ContentId(another))),
        "{refused}"
    );
    let at = bytes.windows(8).position(|it| it == b"bzbind02").expect("a bind record inside");
    let mut old = bytes.clone(); // clone: as above
    old[at..at + 8].copy_from_slice(b"bzbind01");
    start::seal(&mut old);
    let refused = Program::restored(&old, &PROGRAM).err().expect("a bind record of layout 1 inside");
    assert!(refused.contains("bzbind01") && refused.contains("offset"), "{refused}");
}

/// The invariant: a reader with no decoder finds a start image's head and frame where the
/// layout says: `"bzstart1"` at 0..8, the check at 8..40, the program at 40..72, the record
/// at 72..[`start::HEAD`], and the trailer `u64 at | u64 len | "bzstartE"` naming the frame
/// [`start::table_of`] reads, which lies between the head and the trailer. The control: a
/// trailer that names another offset, resealed, is refused by name.
#[test]
fn the_head_and_the_trailer_are_where_a_reader_with_no_decoder_finds_them() {
    let (_, bytes) = image_of(&[1], 1, Plant::None);
    let (restored, of) = Program::restored(&bytes, &PROGRAM).expect("reads back");
    assert_eq!(start::HEAD, 104);
    assert_eq!(&bytes[..8], b"bzstart1");
    assert_eq!(&bytes[40..72], &of.program);
    assert_eq!(&bytes[72..start::HEAD], &of.record.0);
    let end = bytes.len();
    assert_eq!(&bytes[end - 8..], b"bzstartE");
    let at = u64::from_le_bytes(bytes[end - 24..end - 16].try_into().expect("8 bytes")) as usize;
    let len = u64::from_le_bytes(bytes[end - 16..end - 8].try_into().expect("8 bytes")) as usize;
    assert!(at >= start::HEAD && at + len == end - 24, "the frame [{at}, {}) lies between the head and the trailer", at + len);
    let frame = &bytes[at..at + len];
    assert!(
        matches!(wire::split(frame), Ok(Some((item, took))) if took == len && matches!(Tell::decode(item), Ok((_, Tell::Image { .. })))),
        "the trailer names one whole Tell::Image"
    );
    let table = start::table_of(&bytes).expect("its table");
    let welcome = restored.welcome.as_ref().expect("a kept answer");
    assert_eq!(table.record, of.record);
    assert_eq!((table.declared, table.statics), (welcome.declared, welcome.statics));
    assert_eq!(table.bound, welcome.bound);

    let mut moved = bytes.clone(); // clone: a test's copy
    moved[end - 24..end - 16].copy_from_slice(&((at + 1) as u64).to_le_bytes());
    start::seal(&mut moved);
    let refused = start::table_of(&moved).expect_err("a trailer naming another offset");
    assert!(refused.contains("trailer") && refused.contains("offset"), "{refused}");
}

/// Launcher `conn`, connected, says `Hello` holding the start image of check `holds` (or
/// none), `running` or not: what it is told.
fn say_hello(program: &mut Program, conn: u64, holds: Option<[u8; 32]>, running: bool) -> Result<Vec<u8>, String> {
    let mut frame = Vec::new();
    let hello = Ask::Hello {
        program: ProgramId(PROGRAM),
        pid: conn,
        arch: wire::Arch::Aarch64,
        os: wire::Os::Linux,
        page: 4096,
        holds,
        running,
    };
    hello.encode(1, &mut frame).expect("fits a frame");
    let mut out = Vec::new();
    program.said(Conn(conn), &frame[4..], &mut out)?;
    Ok(out)
}

/// What each frame of `out` is: `welcome N`, `image`, or `other`.
fn tells(out: &[u8]) -> Vec<String> {
    let mut at = 0;
    let mut said = Vec::new();
    while let Some((item, took)) = wire::split(&out[at..]).expect("a frame of the daemon's") {
        said.push(match Tell::decode(item).expect("a message of the daemon's").1 {
            Tell::Welcome { slots } => format!("welcome {slots}"),
            Tell::Image { .. } => "image".to_owned(),
            _ => "other".to_owned(),
        });
        at += took;
    }
    said
}

/// The invariant: a launcher that says it holds the program's newest start image (its check
/// word, `Program::check`) is told `Tell::Welcome` of the image's declared slots and no frame;
/// one that holds another image, not running, is sent the whole frame as today. The control
/// is that second launcher: the frame is what a holder is spared.
#[test]
fn a_launcher_that_holds_the_programs_image_is_welcomed_with_no_frame() {
    let mut sim = Sim::new(Source::new(3), Plant::None).expect("built");
    sim.connect(4096);
    sim.settle().expect("started");
    let of = sim.of();
    let program = &mut sim.program.program;
    let (_, bytes) = program.kept(&of).expect("kept").expect("an image");
    let check: [u8; 32] = bytes[8..40].try_into().expect("32 bytes");
    assert_eq!(program.kept_check(), Some(check));
    let table = start::table_of(&bytes).expect("its table");
    program.connected(Conn(90));
    let told = say_hello(program, 90, Some(check), false).expect("welcomed");
    assert_eq!(tells(&told), [format!("welcome {}", table.declared)]);
    program.connected(Conn(91));
    let told = say_hello(program, 91, Some([7; 32]), false).expect("sent its start");
    assert_eq!(tells(&told), ["image"]);
}

/// The refinement: a running launcher that started from an older image of the program's
/// build, joined with that image's table (`Program::join`), is bound as a launcher welcomed
/// with that image: it is told `Tell::Welcome` of the image's slots, its `Hello` then adds
/// nothing, and a later first call is answered by an edit that binds only slots it declared
/// (the model launcher holds the table's binds and refuses any other), binding the slot.
#[test]
fn a_launcher_joined_from_an_older_image_of_the_same_build_is_bound_as_one_welcomed_then() {
    let mut sim = Sim::new(Source::new(3), Plant::None).expect("built");
    sim.connect(4096);
    sim.settle().expect("started");
    let of = sim.of();
    let (_, older) = sim.program.program.kept(&of).expect("kept").expect("an image");
    let f1 = slot(1, 1, Plant::None);
    sim.call(0, &f1).expect("f1 called: the image is older now");
    let slot_f1 = sim.slot_of(0, &f1).expect("f1 is declared");
    let held: [u8; 32] = older[8..40].try_into().expect("32 bytes");
    let program = &mut sim.program.program;
    assert_ne!(program.kept_check(), Some(held), "what the program holds moved past the older image");
    let table = start::table_of(&older).expect("its table");
    program.connected(Conn(90));
    let mut told = Vec::new();
    program.join(Conn(90), &table, &of.record, &mut told).expect("joined");
    assert_eq!(tells(&told), [format!("welcome {}", table.declared)]);
    assert_eq!(say_hello(program, 90, Some(held), true).expect("its Hello, after the join"), Vec::<u8>::new());

    let mut launcher =
        Launcher { conn: 90, declared: table.declared, bound: table.bound.clone(), ..Launcher::default() }; // clone: the model's own table
    assert!(slot_f1 < launcher.declared, "the image declares f1's slot");
    launcher.say(&Ask::Need { slot: Slot(slot_f1), thread: Thread(1) });
    let need = launcher.outbound.pop_front().expect("its need");
    let mut out = Vec::new();
    let said = program.said(Conn(90), &need, &mut out).expect("answered");
    assert!(matches!(said, Said::Told), "the program holds f1's record and answers at once");
    launcher.inbound.extend(out);
    while launcher.step() {}
    assert!(launcher.bound[slot_f1 as usize].is_some(), "f1 is bound in the joined launcher");
}

/// The invariant: a launcher whose start image is of another build than the program's is
/// refused at its join naming both builds, and a running launcher whose image is not the
/// newest and that was not joined is refused at its `Hello` naming the image. The control:
/// the same join with the program's own build succeeds.
#[test]
fn a_launcher_of_another_build_is_refused_naming_both() {
    let mut sim = Sim::new(Source::new(3), Plant::None).expect("built");
    sim.connect(4096);
    sim.settle().expect("started");
    let of = sim.of();
    let (_, older) = sim.program.program.kept(&of).expect("kept").expect("an image");
    sim.call(0, &slot(1, 1, Plant::None)).expect("f1 called");
    let held: [u8; 32] = older[8..40].try_into().expect("32 bytes");
    let program = &mut sim.program.program;
    let mut table = start::table_of(&older).expect("its table");
    program.connected(Conn(90));
    let refused = say_hello(program, 90, Some(held), true).expect_err("running, not joined");
    assert!(refused.contains(&format!("{:?}", ContentId(held))), "{refused}");
    let other = ContentId([9; 32]);
    let build = table.record;
    table.record = other;
    let refused = program.join(Conn(90), &table, &of.record, &mut Vec::new()).expect_err("another build");
    assert!(
        refused.contains(&format!("{other:?}")) && refused.contains(&format!("{:?}", of.record)),
        "{refused}"
    );
    table.record = build;
    program.join(Conn(90), &table, &of.record, &mut Vec::new()).expect("the program's own build");
}

/// The start image of a program whose last function's body is at `body`, after a run that
/// called `order` (each a function's index), keyed `plant`ed or not.
fn image_of(order: &[u64], body: u64, plant: Plant) -> (Key, Vec<u8>) {
    let mut source = Source::new(3);
    source.bodies[2] = body;
    let mut sim = Sim::new(source, Plant::None).expect("built");
    let which = sim.connect(4096);
    sim.settle().expect("started");
    for function in order {
        sim.call(which, &slot(*function, 1, Plant::None)).expect("called");
    }
    let of = sim.of();
    sim.program.kept(&of, plant).expect("kept").expect("an image")
}

/// The invariant: two start images of other bytes have two keys: of one build after runs
/// that called other slots or the same slots in another order, and of two builds. The
/// control: a key that leaves the body's hash out (`Plant::KeyOfIdentityAlone`) gives two
/// images of one build one key, so the store would hand a daemon the stale one for the newer.
#[test]
fn images_that_differ_have_keys_that_differ() {
    let runs: [(&[u64], u64); 5] = [(&[], 1), (&[1], 1), (&[1, SEALED], 1), (&[SEALED, 1], 1), (&[1, SEALED], 2)];
    let collide = |plant: Plant| -> Option<String> {
        let images: Vec<(Key, Vec<u8>)> = runs.iter().map(|(order, body)| image_of(order, *body, plant)).collect();
        for (a, first) in images.iter().enumerate() {
            for (b, second) in images.iter().enumerate().skip(a + 1) {
                if first.1 != second.1 && first.0 == second.0 {
                    return Some(format!("the images of runs {a} and {b} differ and have one key"));
                }
            }
        }
        None
    };
    // Other bytes, every one: the runs are told apart.
    let images: Vec<Vec<u8>> = runs.iter().map(|(order, body)| image_of(order, *body, Plant::None).1).collect();
    for (a, first) in images.iter().enumerate() {
        assert!(images.iter().skip(a + 1).all(|it| it != first), "run {a}'s image is another run's");
    }
    assert_eq!(collide(Plant::None), None);
    let caught = collide(Plant::KeyOfIdentityAlone).expect("a key that misses an input");
    assert!(caught.contains("one key"), "{caught}");
}

/// The invariant (rule 26): a bind record of the layout before asks were in its names
/// (`bzbind01`) is refused by name, never read as this layout: its function is made again.
#[test]
fn a_bind_record_of_layout_1_is_refused_by_name() {
    let bound = drawn(2, 1, (true, false), true, &[true, false, true], &[1]);
    let mut bytes = bind::encode(&bound);
    assert_eq!(&bytes[..8], b"bzbind02");
    bytes[..8].copy_from_slice(b"bzbind01");
    let refused = bind::decode(&bytes).expect_err("layout 1");
    assert!(refused.contains("offset 0") && refused.contains("bzbind01") && refused.contains("bzbind02"), "{refused}");
}
