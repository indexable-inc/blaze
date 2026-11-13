use std::alloc::Global;
use std::cell::Cell;

use blaze_cas::{Driven, Key, Kind, Mismatch, content_of};
use blaze_ids::ContentId;
use blaze_store_check::{Setting, VerifyError};
use hegel::TestCase;
use hegel::generators as gs;

use super::*;

mod cost;

const FACT: ReadKind = ReadKind(1);
const DIAL: ReadKind = ReadKind(2);
const EXTRA: ReadKind = ReadKind(3);
const HASHED: &str = "blaze v2 kernel test fact 1";

/// Facts by one-byte key, a dial, and ways to misbehave.
#[derive(Default)]
struct Facts {
    facts: blaze_hash::HashMap<u8, u8, Global>,
    /// A fact whose read fails.
    failing: Option<u8>,
    /// A fact that cannot be valued now.
    not_now: Option<u8>,
    /// A fact whose hash changes at each read.
    drifting: Option<u8>,
    drift: Cell<u8>,
    dial: u16,
}

thread_local! {
    /// What [`Salted`] reads behind the kernel's back.
    static SALT: Cell<u8> = const { Cell::new(0) };
}

impl Facts {
    fn of(facts: &[(u8, u8)]) -> Facts {
        let mut world = Facts::default();
        world.facts.extend(facts.iter().copied());
        world
    }
}

impl World for Facts {
    fn hash(&self, read: Read<'_>) -> Result<Seen, WorldError> {
        match read.kind {
            FACT => {
                let &[key] = read.key else {
                    return Err(WorldError::new(read, "a fact's key is one byte"));
                };
                if self.failing == Some(key) {
                    return Err(WorldError::new(read, "the test fails it"));
                }
                if self.not_now == Some(key) {
                    return Err(WorldError::not_now(read, "the test cannot value it now"));
                }
                let salt = if self.drifting == Some(key) {
                    self.drift.replace(self.drift.get() + 1)
                } else {
                    0
                };
                Ok(match self.facts.get(&key) {
                    Some(value) => Seen::Is(ContentId::of(HASHED, &[*value, salt])),
                    None => Seen::Gone,
                })
            }
            DIAL => Ok(Seen::Is(ContentId::of(HASHED, &self.dial.to_le_bytes()))),
            EXTRA => Ok(Seen::Is(ContentId::of(HASHED, read.key))),
            _ => Err(WorldError::new(read, "no such kind")),
        }
    }

    /// The facts, the dial and which fact misbehaves; not the drift, which is how this world
    /// moves without saying so.
    fn pin(&self) -> Pin {
        let mut facts: Vec<(u8, u8)> = self.facts.iter().map(|(key, value)| (*key, *value)).collect();
        facts.sort_unstable();
        let mut bytes = self.dial.to_le_bytes().to_vec();
        bytes.extend([self.failing.is_some() as u8, self.failing.unwrap_or(0)]);
        bytes.extend([self.drifting.is_some() as u8, self.drifting.unwrap_or(0)]);
        bytes.extend([self.not_now.is_some() as u8, self.not_now.unwrap_or(0)]);
        bytes.extend(facts.iter().flat_map(|(key, value)| [*key, *value]));
        Pin(ContentId::of("blaze v2 kernel test pin 1", &bytes))
    }

    fn describe(&self, read: Read<'_>) -> String {
        format!("fact {:?} of kind {}", read.key, read.kind.0)
    }
}

struct Value;

impl Fact<Facts> for Value {
    const KIND: ReadKind = FACT;
    type Key = u8;
    type Value<'w> = Option<u8>;

    fn key(key: &u8, out: &mut Vec<u8>) {
        out.push(*key);
    }

    fn get<'w>(world: &'w Facts, key: &u8) -> Result<(Option<u8>, Seen), WorldError> {
        let seen = world.hash(Read { kind: FACT, key: &[*key] })?;
        Ok((world.facts.get(key).copied(), seen))
    }
}

struct Dial;

impl Fact<Facts> for Dial {
    const KIND: ReadKind = DIAL;
    type Key = ();
    type Value<'w> = u16;

    fn key(_: &(), _: &mut Vec<u8>) {}

    fn get<'w>(world: &'w Facts, _: &()) -> Result<(u16, Seen), WorldError> {
        Ok((world.dial, world.hash(Read { kind: DIAL, key: &[] })?))
    }
}

fn subject(input: u8) -> Subject {
    Subject {
        of: ContentId::of("blaze v2 kernel test input 1", &[input]),
        flags: ContentId::default(),
    }
}

/// Follows facts from its input, at most four hops: its reads depend on what it read.
struct Chase;

impl Step for Chase {
    const ID: StepId = StepId(0x7f01);
    const NAME: &'static str = "test chase";
    const KIND: Kind = Kind::Query;
    type Input = u8;

    fn subject(input: &u8) -> Subject {
        subject(*input)
    }

    fn label(input: &u8) -> String {
        format!("chase from {input}")
    }
}

impl Compute for Chase {
    type World = Facts;

    fn compute(ctx: &mut Reads<'_, Facts>, input: &u8) -> Result<Vec<u8>, StepError> {
        let mut out = Vec::new();
        let mut at = *input;
        for _ in 0..4 {
            match ctx.get::<Value>(&at)? {
                Some(next) => {
                    out.push(next);
                    at = next % 8;
                }
                None => {
                    out.push(0xff);
                    break;
                }
            }
        }
        Ok(out)
    }
}

/// Reads nothing: its identity is its whole content.
struct Constant;

impl Step for Constant {
    const ID: StepId = StepId(0x7f02);
    const NAME: &'static str = "test constant";
    const KIND: Kind = Kind::Query;
    type Input = u8;

    fn subject(input: &u8) -> Subject {
        subject(*input)
    }

    fn label(input: &u8) -> String {
        format!("constant {input}")
    }
}

impl Compute for Constant {
    type World = Facts;

    fn compute(_: &mut Reads<'_, Facts>, input: &u8) -> Result<Vec<u8>, StepError> {
        Ok(vec![*input; 3])
    }
}

/// Reads a fact, and the world's salt behind the kernel's back: a step that is not pure.
struct Salted;

impl Step for Salted {
    const ID: StepId = StepId(0x7f03);
    const NAME: &'static str = "test salted";
    const KIND: Kind = Kind::Query;
    type Input = u8;

    fn subject(input: &u8) -> Subject {
        subject(*input)
    }

    fn label(input: &u8) -> String {
        format!("salted {input}")
    }
}

impl Compute for Salted {
    type World = Facts;

    fn compute(ctx: &mut Reads<'_, Facts>, input: &u8) -> Result<Vec<u8>, StepError> {
        let value = ctx.get::<Value>(input)?.unwrap_or(0xff);
        Ok(vec![value, SALT.with(Cell::get)])
    }
}

/// Reads the dial, then a key the dial names: one shape for each setting of the dial.
struct Wide;

impl Step for Wide {
    const ID: StepId = StepId(0x7f04);
    const NAME: &'static str = "test wide";
    const KIND: Kind = Kind::Query;
    type Input = u8;

    fn subject(input: &u8) -> Subject {
        subject(*input)
    }

    fn label(input: &u8) -> String {
        format!("wide {input}")
    }
}

impl Compute for Wide {
    type World = Facts;

    fn compute(ctx: &mut Reads<'_, Facts>, _: &u8) -> Result<Vec<u8>, StepError> {
        let dial = ctx.get::<Dial>(&())?.to_le_bytes();
        ctx.hash(Read { kind: EXTRA, key: &dial })?;
        Ok(dial.to_vec())
    }
}

fn kernel() -> Kernel<Facts> {
    let mut kernel = Kernel::new();
    kernel.register::<Chase>(CodeId(ContentId([1; 32]))).unwrap();
    kernel.register::<Constant>(CodeId(ContentId([2; 32]))).unwrap();
    kernel.register::<Salted>(CodeId(ContentId([3; 32]))).unwrap();
    kernel.register::<Wide>(CodeId(ContentId([4; 32]))).unwrap();
    kernel
}

/// What `S` makes of `input` in `world`, with no store.
fn fresh<S: Compute<Input = u8, World = Facts>>(world: &Facts, input: u8) -> Vec<u8> {
    let mut ctx = Reads::new(world);
    S::compute(&mut ctx, &input).unwrap()
}

/// A store in memory that keeps the first value of a key, as blaze-cas does. `blind` hides
/// what it has from reads, so a step runs again over an entry it kept. It counts reads.
#[derive(Default)]
struct Memory {
    values: blaze_hash::HashMap<Key, Vec<u8>, Global>,
    blind: bool,
    gets: Cell<usize>,
    probes: Cell<usize>,
}

impl Get for Memory {
    fn get(&self, key: &Key) -> Result<Option<&[u8]>, String> {
        self.gets.set(self.gets.get() + 1);
        Ok(self.values.get(key).filter(|_| !self.blind).map(Vec::as_slice))
    }

    fn has(&self, key: &Key) -> Result<bool, String> {
        self.probes.set(self.probes.get() + 1);
        Ok(!self.blind && self.values.contains_key(key))
    }
}

impl Put for Memory {
    fn put(&mut self, key: Key, _: Kind, bytes: Vec<u8>) -> Result<(), Mismatch> {
        match self.values.get(&key) {
            Some(kept) if *kept != bytes => {
                Err(Mismatch { key, kept: content_of(kept), given: content_of(&bytes) })
            }
            Some(_) => Ok(()),
            None => {
                self.values.insert(key, bytes);
                Ok(())
            }
        }
    }
}

/// What the kernel keeps comes back from the store on disk after the process that kept it is
/// gone, read in place, equal byte for byte to a fresh run (rule 26). A store that opens empty
/// fails here.
#[test]
fn a_kept_entry_comes_back_after_a_restart() {
    let held = blaze_core::temp::TempDir::for_test("blaze-kernel-restart-");
    let root = held.path();
    let kernel = kernel();
    let world = Facts::of(&[(1, 2), (2, 3), (3, 1)]);
    let (chased, constant) = {
        let mut driven = Driven::open(&root).unwrap();
        let chased = kernel.get::<Chase, _>(driven.store(), &world, &1, &Setting::OFF).unwrap();
        assert!(!chased.is_hit());
        let chased = chased.value().to_vec();
        let constant = kernel
            .get::<Constant, _>(driven.store(), &world, &7, &Setting::OFF)
            .unwrap()
            .value()
            .to_vec();
        driven.flush().unwrap();
        (chased, constant)
    };
    let mut driven = Driven::open(&root).unwrap();
    // Every hit is computed again and compared.
    let got = kernel.get::<Chase, _>(driven.store(), &world, &1, &Setting::ALL).unwrap();
    assert!(got.is_hit(), "the store came back without the entry it kept");
    assert_eq!(got.value(), chased);
    assert_eq!(got.value(), fresh::<Chase>(&world, 1));
    let got = kernel.get::<Constant, _>(driven.store(), &world, &7, &Setting::ALL).unwrap();
    assert!(got.is_hit(), "the store came back without the entry with no reads it kept");
    assert_eq!(got.value(), constant);
    assert_eq!(got.value(), fresh::<Constant>(&world, 7));
    drop(driven);
}

/// Two values under one identity and set of reads are refused, naming the step, the input
/// and the reads, and the first value stays.
#[test]
fn two_values_under_one_identity_and_reads_are_refused() {
    let kernel = kernel();
    let mut store = Memory::default();
    let world = Facts::of(&[(5, 1)]);
    let first =
        kernel.get::<Salted, _>(&mut store, &world, &5, &Setting::OFF).unwrap().value().to_vec();
    SALT.with(|it| it.set(1));
    store.blind = true;
    let refused = kernel.get::<Salted, _>(&mut store, &world, &5, &Setting::OFF).unwrap_err();
    assert!(matches!(refused, KernelError::Conflict { .. }), "{refused}");
    let said = refused.to_string();
    for part in ["test salted", "`salted 5`", "fact [5]", "misses an input"] {
        assert!(said.contains(part), "{part} is not in: {said}");
    }
    store.blind = false;
    SALT.with(|it| it.set(0));
    assert_eq!(
        kernel.get::<Salted, _>(&mut store, &world, &5, &Setting::OFF).unwrap().value(),
        first
    );
}

/// A run whose read failed, or whose read saw two hashes, keeps nothing.
#[test]
fn a_run_whose_read_failed_or_moved_keeps_nothing() {
    let kernel = kernel();
    let mut store = Memory::default();
    let mut world = Facts::of(&[(1, 1)]);
    world.failing = Some(1);
    let failed = kernel.get::<Chase, _>(&mut store, &world, &1, &Setting::OFF).unwrap_err();
    assert!(matches!(failed, KernelError::World { .. }), "{failed}");
    world.failing = None;
    // 1 names 1: the chase reads fact 1 four times, and it drifts.
    world.drifting = Some(1);
    let moved = kernel.get::<Chase, _>(&mut store, &world, &1, &Setting::OFF).unwrap_err();
    assert!(matches!(moved, KernelError::Unsteady { .. }), "{moved}");
    assert!(store.values.is_empty(), "a run that failed kept {} values", store.values.len());
}

/// A step with no reads costs one probe and one read on a hit: no chain of shapes.
#[test]
fn a_step_with_no_reads_is_one_probe() {
    let kernel = kernel();
    let mut store = Memory::default();
    let world = Facts::default();
    kernel.get::<Constant, _>(&mut store, &world, &3, &Setting::OFF).unwrap();
    assert_eq!(store.values.len(), 1, "a step with no reads kept a shape");
    store.gets.set(0);
    store.probes.set(0);
    let got = kernel.get::<Constant, _>(&mut store, &world, &3, &Setting::OFF).unwrap();
    let (hit, value) = (got.is_hit(), got.value().to_vec());
    assert!(hit);
    assert_eq!((store.probes.get(), store.gets.get()), (1, 1));
    assert_eq!(value, fresh::<Constant>(&world, 3));
}

/// A changed read misses, a read now gone misses, and going back hits the first entry.
#[test]
fn a_changed_or_gone_read_misses_and_going_back_hits() {
    let kernel = kernel();
    let mut store = Memory::default();
    let mut world = Facts::of(&[(1, 2), (2, 9)]);
    let mut get = |world: &Facts| {
        let got = kernel.get::<Chase, _>(&mut store, world, &1, &Setting::ALL).unwrap();
        (got.is_hit(), got.value().to_vec())
    };
    let first = get(&world);
    assert!(!first.0);
    world.facts.insert(2, 3);
    assert_eq!(get(&world), (false, fresh::<Chase>(&world, 1)));
    world.facts.remove(&2);
    assert_eq!(get(&world), (false, fresh::<Chase>(&world, 1)));
    world.facts.insert(2, 9);
    assert_eq!(get(&world), (true, first.1));
}

/// The chain of shapes under one identity ends at [`MOST_SHAPES`] with a typed error.
#[test]
fn the_chain_of_shapes_is_bounded() {
    let kernel = kernel();
    let mut store = Memory::default();
    let mut world = Facts::default();
    let identity = kernel.identity::<Wide>(&0).unwrap().id();
    for n in 0..MOST_SHAPES {
        world.dial = n as u16;
        let mut ctx = Reads::new(&world);
        Wide::compute(&mut ctx, &0).unwrap();
        let Ok(trace) = ctx.finish() else { panic!("the wide step's reads") };
        let slot = blaze_record::list_key(&identity, blaze_record::Slot { generation: 0, n });
        store.put(Key(slot), Kind::Query, entry::list(&identity, &trace)).unwrap();
    }
    world.dial = MOST_SHAPES as u16;
    let refused = kernel.get::<Wide, _>(&mut store, &world, &0, &Setting::OFF).unwrap_err();
    assert!(matches!(refused, KernelError::TooManyShapes { .. }), "{refused}");
}

/// A second step of one number or one name is refused, and so is a step never registered.
#[test]
fn one_number_and_one_name_are_one_step() {
    struct Impostor;
    impl Step for Impostor {
        const ID: StepId = Chase::ID;
        const NAME: &'static str = "test impostor";
        const KIND: Kind = Kind::Query;
        type Input = u8;
        fn subject(input: &u8) -> Subject {
            subject(*input)
        }
        fn label(_: &u8) -> String {
            String::new()
        }
    }
    let mut kernel = kernel();
    assert!(matches!(
        kernel.register::<Chase>(CodeId(ContentId([9; 32]))),
        Err(KernelError::Twice { .. })
    ));
    assert!(matches!(
        kernel.register::<Impostor>(CodeId(ContentId([9; 32]))),
        Err(KernelError::Twice { .. })
    ));
    assert!(matches!(
        Kernel::<Facts>::new().identity::<Chase>(&1),
        Err(KernelError::Unregistered { holder: None, .. })
    ));
}

/// A sampled hit is computed again: a step that read outside its context is caught.
#[test]
fn a_sampled_hit_is_computed_again() {
    let kernel = kernel();
    let mut store = Memory::default();
    let world = Facts::of(&[(4, 4)]);
    kernel.get::<Salted, _>(&mut store, &world, &4, &Setting::OFF).unwrap();
    SALT.with(|it| it.set(7));
    assert!(kernel.get::<Salted, _>(&mut store, &world, &4, &Setting::OFF).unwrap().is_hit());
    let caught = kernel.get::<Salted, _>(&mut store, &world, &4, &Setting::ALL).unwrap_err();
    assert!(matches!(caught, KernelError::Verify(VerifyError::Mismatch(_))), "{caught}");
    assert!(caught.to_string().contains("salted 4"), "{caught}");
}

/// Any sequence of edits to the world and gets: each get gives what a fresh run over the
/// world as it is now gives, and each hit, computed again, makes the same reads and bytes.
#[hegel::test(test_cases = 100)]
fn any_edits_and_gets_give_what_a_fresh_run_gives(tc: TestCase) {
    let kernel = kernel();
    let mut store = Memory::default();
    let mut world = Facts::default();
    let steps: usize = tc.draw(gs::integers::<usize>().max_value(40));
    for _ in 0..steps {
        let key = tc.draw(gs::integers::<u8>().max_value(7));
        match tc.draw(gs::integers::<u8>().max_value(2)) {
            0 => {
                world.facts.insert(key, tc.draw(gs::integers::<u8>().max_value(11)));
            }
            1 => {
                world.facts.remove(&key);
            }
            _ => {
                let got = kernel.get::<Chase, _>(&mut store, &world, &key, &Setting::ALL).unwrap();
                assert_eq!(got.value(), fresh::<Chase>(&world, key));
            }
        }
    }
}

/// The key a get computed and kept its entry under; a hit is a failure.
fn computed<S: Compute<Input = u8, World = Facts>>(
    kernel: &Kernel<Facts>,
    store: &mut Memory,
    world: &Facts,
    input: u8,
) -> Key {
    match kernel.get::<S, _>(store, world, &input, &Setting::OFF).unwrap() {
        Got::Computed { key, .. } => key,
        Got::Hit(_) => panic!("{} of {input} hit", S::NAME),
    }
}

/// A miss after one narrow read changed names that read, by its world's name, with the hash
/// the entry kept last saw and the one it sees now, and counts it as the one read of the
/// entry's that changed. The read that stayed is not named, and asking writes nothing.
#[test]
fn a_miss_names_the_one_narrow_read_that_changed() {
    let kernel = kernel();
    let mut store = Memory::default();
    // The chase from 1 reads fact 1 (it names 2), then fact 2 (it names 9, so 1 again).
    let mut world = Facts::of(&[(1, 2), (2, 9)]);
    let last = computed::<Chase>(&kernel, &mut store, &world, 1);
    let was = world.hash(Read { kind: FACT, key: &[2] }).unwrap();
    world.facts.insert(2, 3);
    let now = world.hash(Read { kind: FACT, key: &[2] }).unwrap();
    assert_ne!(was, now);
    assert!(kernel.find::<Chase, _>(&store, &world, &1).unwrap().is_none());
    let kept = store.values.len();
    assert_eq!(
        kernel.why::<Chase, _>(&store, &world, &1, Some(last)).unwrap(),
        Missed::Read {
            read: "fact [2] of kind 1".to_owned(),
            names: None,
            was,
            now,
            changed: 1,
            reads: 2
        }
    );
    assert_eq!(store.values.len(), kept, "asking why wrote to the store");
}

/// A miss of an identity nothing was ever kept under is `New`; one with shapes kept but no
/// entry handed in is `Unrecorded`; one whose entry handed in is not in the store is
/// `Collected`, not a guess at what it read.
#[test]
fn a_miss_says_what_it_had_to_compare_against() {
    let kernel = kernel();
    let mut store = Memory::default();
    let world = Facts::of(&[(1, 2)]);
    assert_eq!(kernel.why::<Chase, _>(&store, &world, &1, None).unwrap(), Missed::New);
    assert!(store.values.is_empty(), "asking why wrote to the store");
    let last = computed::<Chase>(&kernel, &mut store, &world, 1);
    assert_eq!(kernel.why::<Chase, _>(&store, &world, &1, None).unwrap(), Missed::Unrecorded);
    // Another input is another identity, with nothing kept under it.
    assert_eq!(kernel.why::<Chase, _>(&store, &world, &2, None).unwrap(), Missed::New);
    let other = computed::<Chase>(&kernel, &mut Memory::default(), &Facts::of(&[(1, 3)]), 1);
    assert_ne!(other, last);
    assert_eq!(kernel.why::<Chase, _>(&store, &world, &1, Some(other)).unwrap(), Missed::Collected);
}

/// A step the kernel cannot run: its value and its reads are made elsewhere (a process).
struct Elsewhere;

impl Step for Elsewhere {
    const ID: StepId = StepId(0x7f05);
    const NAME: &'static str = "test elsewhere";
    const KIND: Kind = Kind::Query;
    type Input = u8;

    fn subject(input: &u8) -> Subject {
        subject(*input)
    }

    fn label(input: &u8) -> String {
        format!("elsewhere {input}")
    }
}

fn fact(world: &Facts, at: u8) -> Seen {
    world.hash(Read { kind: FACT, key: &[at] }).unwrap()
}

/// A value kept with a trace made elsewhere is found while every read of the trace hashes the
/// same, misses once one changed and is found again when it is back; the trace keeps its reads
/// sorted, each once, whatever order they were added in.
#[test]
fn a_step_kept_with_a_trace_made_elsewhere_is_found_while_its_reads_hold() {
    let mut kernel = kernel();
    kernel.register::<Elsewhere>(CodeId(ContentId([6; 32]))).unwrap();
    let mut store = Memory::default();
    let mut world = Facts::of(&[(1, 2), (2, 9)]);
    let mut trace = Trace::new();
    for at in [2, 1, 2] {
        trace.read(Read { kind: FACT, key: &[at] }, fact(&world, at)).unwrap();
    }
    assert_eq!(trace.len(), 2);
    assert_eq!(trace.iter().map(|(read, _)| read.key.to_vec()).collect::<Vec<_>>(), [[1], [2]]);
    let key = kernel.keep::<Elsewhere, _>(&mut store, &5, &trace, b"made elsewhere").unwrap();
    let found = kernel.find::<Elsewhere, _>(&store, &world, &5).unwrap().unwrap();
    assert_eq!((found.key, found.value), (key, &b"made elsewhere"[..]));
    assert_eq!(found.reads().collect::<Vec<_>>(), trace.iter().collect::<Vec<_>>());
    // Another input is another identity.
    assert!(kernel.find::<Elsewhere, _>(&store, &world, &6).unwrap().is_none());
    world.facts.insert(2, 3);
    assert!(kernel.find::<Elsewhere, _>(&store, &world, &5).unwrap().is_none());
    world.facts.insert(2, 9);
    let back = kernel.find::<Elsewhere, _>(&store, &world, &5).unwrap().unwrap();
    assert_eq!(back.value, b"made elsewhere");
    // A trace with no reads is the entry with no reads.
    let bare = kernel.keep::<Elsewhere, _>(&mut store, &7, &Trace::new(), b"whole").unwrap();
    assert_eq!(bare, whole::key(&kernel.identity::<Elsewhere>(&7).unwrap()));
    assert_eq!(kernel.find_whole::<Elsewhere, _>(&store, &7).unwrap().unwrap().value, b"whole");
}

/// The conflict rule is the one of a computed entry: other bytes under one identity and trace
/// are refused, naming the step, the input and the reads, and the first value stays; the same
/// bytes again are kept once.
#[test]
fn two_values_under_one_identity_and_trace_made_elsewhere_are_refused() {
    let mut kernel = kernel();
    kernel.register::<Elsewhere>(CodeId(ContentId([6; 32]))).unwrap();
    let mut store = Memory::default();
    let world = Facts::of(&[(1, 2)]);
    let mut trace = Trace::new();
    trace.read(Read { kind: FACT, key: &[1] }, fact(&world, 1)).unwrap();
    let key = kernel.keep::<Elsewhere, _>(&mut store, &5, &trace, b"first").unwrap();
    assert_eq!(kernel.keep::<Elsewhere, _>(&mut store, &5, &trace, b"first").unwrap(), key);
    let refused = kernel.keep::<Elsewhere, _>(&mut store, &5, &trace, b"second").unwrap_err();
    assert!(matches!(refused, KernelError::Conflict { .. }), "{refused}");
    let said = refused.to_string();
    let identity = format!("{:x}", kernel.identity::<Elsewhere>(&5).unwrap().id());
    let parts =
        ["test elsewhere", "`elsewhere 5`", identity.as_str(), "read 1 of 01", "misses an input"];
    for part in parts {
        assert!(said.contains(part), "{part} is not in: {said}");
    }
    assert_eq!(kernel.find::<Elsewhere, _>(&store, &world, &5).unwrap().unwrap().value, b"first");
    // The same reads seen otherwise are another entry, not a conflict.
    let mut other = Trace::new();
    other.read(Read { kind: FACT, key: &[1] }, Seen::Gone).unwrap();
    assert_ne!(kernel.keep::<Elsewhere, _>(&mut store, &5, &other, b"second").unwrap(), key);
}

/// One read added to a trace with two hashes is refused, naming the read and both.
#[test]
fn a_trace_refuses_one_read_seen_as_two_things() {
    let world = Facts::of(&[(1, 2)]);
    let mut trace = Trace::new();
    trace.read(Read { kind: FACT, key: &[1] }, fact(&world, 1)).unwrap();
    let refused = trace.read(Read { kind: FACT, key: &[1] }, Seen::Gone).unwrap_err();
    assert_eq!(refused.read(), Read { kind: FACT, key: &[1] });
    assert_eq!((refused.first, refused.then), (fact(&world, 1), Seen::Gone));
    assert_eq!(trace.len(), 1);
}

/// The door makes the entry the kernel's own run makes: a computed step's reads and value,
/// kept through `keep` into another store, land under the same key with the same bytes.
#[test]
fn a_trace_made_elsewhere_keeps_the_entry_a_run_keeps() {
    let kernel = kernel();
    let mut store = Memory::default();
    let world = Facts::of(&[(1, 2), (2, 9)]);
    let Got::Computed { key, value, reads } =
        kernel.get::<Chase, _>(&mut store, &world, &1, &Setting::OFF).unwrap()
    else {
        panic!("the first get of an empty store hit");
    };
    let mut trace = Trace::new();
    for (read, seen) in reads.iter().collect::<Vec<_>>().into_iter().rev() {
        trace.read(read, seen).unwrap();
    }
    let mut other = Memory::default();
    assert_eq!(kernel.keep::<Chase, _>(&mut other, &1, &trace, &value).unwrap(), key);
    assert_eq!(other.values.len(), store.values.len());
    for (key, bytes) in &store.values {
        assert_eq!(other.values.get(key), Some(bytes));
    }
}

/// An entry of the layout before this one (2: no hash of its value) under a key this blaze
/// looks under is refused by name, never read as layout 3 and never computed over (rule 26).
#[test]
fn an_entry_of_layout_2_is_refused_by_name() {
    let kernel = kernel();
    let mut store = Memory::default();
    let world = Facts::of(&[(1, 2)]);
    let (key, value) = match kernel.get::<Chase, _>(&mut store, &world, &1, &Setting::OFF).unwrap() {
        Got::Computed { key, value, .. } => (key, value),
        Got::Hit(_) => panic!("an empty store had an entry"),
    };
    // Layout 2 is layout 3 without the 32 bytes of the value's hash, which keeps the value
    // on its 16-byte boundary.
    let mut old = store.values.get(&key).expect("the entry kept").clone();
    let hash = content_of(&value);
    let at = old.windows(32).position(|it| it == hash.0.as_slice()).expect("the kept hash");
    old.drain(at..at + 32);
    old[15] = b'2';
    store.values.insert(key, old.clone());
    let refused = kernel.get::<Chase, _>(&mut store, &world, &1, &Setting::OFF).unwrap_err();
    let said = refused.to_string();
    assert!(matches!(refused, KernelError::Store { .. }), "{said}");
    assert!(
        said.contains("a kernel entry of layout 2, where this blaze reads layout 3"),
        "{said}"
    );
    assert_eq!(store.values.get(&key), Some(&old), "a refused entry was written over");
    let found = kernel.find::<Chase, _>(&store, &world, &1).unwrap_err().to_string();
    assert!(found.contains("layout 2"), "{found}");
}

/// A read the world cannot value now passes over the read-list that names it, and the lookup
/// goes on to the identity's other lists; a read that fails is still the lookup's error; and
/// in a run a read that cannot be valued now is the run's failure, which keeps nothing.
#[test]
fn a_read_that_cannot_be_valued_now_passes_over_its_list() {
    let kernel = kernel();
    let mut store = Memory::default();
    // A chase from 1 reads fact 1 and the fact it names: facts 1 and 2, then facts 1 and 5.
    let mut world = Facts::of(&[(1, 2)]);
    kernel.get::<Chase, _>(&mut store, &world, &1, &Setting::OFF).unwrap();
    world.facts.insert(1, 5);
    let second = kernel.get::<Chase, _>(&mut store, &world, &1, &Setting::OFF).unwrap();
    let (hit, second) = (second.is_hit(), second.value().to_vec());
    assert!(!hit);
    // Fact 2 is named by the first list alone.
    world.not_now = Some(2);
    let got = kernel.get::<Chase, _>(&mut store, &world, &1, &Setting::OFF).unwrap();
    assert!(got.is_hit(), "a list with a read that cannot be valued now ended the lookup");
    assert_eq!(got.value(), second);
    let found = kernel.find::<Chase, _>(&store, &world, &1).unwrap().expect("the second entry");
    assert_eq!(found.value, second);
    world.not_now = None;
    world.failing = Some(2);
    let failed = kernel.get::<Chase, _>(&mut store, &world, &1, &Setting::OFF).unwrap_err();
    assert!(matches!(failed, KernelError::World { .. }), "{failed}");
    // Fact 5 is named by the second list, and a run now asks for it.
    world.failing = None;
    world.not_now = Some(5);
    let kept = store.values.len();
    let refused = kernel.get::<Chase, _>(&mut store, &world, &1, &Setting::OFF).unwrap_err();
    assert!(matches!(refused, KernelError::World { .. }), "{refused}");
    assert_eq!(store.values.len(), kept, "a run whose read could not be valued kept something");
}
