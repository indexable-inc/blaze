//! Single flight under many threads: however many threads ask for a function at once, the
//! runner compiles it once and answers every one of them with the one address; a function
//! that fails or panics is answered `Failed` naming it, and the run is stopped once. On a
//! host with stubs, threads call through real entries into a hook that asks a runner.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use blaze_codegen::{CompiledFunction, InstanceKey, plan::Mode, plan::OptLevel};
use blaze_hash::{HashMap, HashSet};
use blaze_inproc::CodeMemory;
use blaze_inproc::frame::Kind;
use blaze_items::ItemKey;
use blaze_stencil::tier::{self, CALLS, Policy, Tier};
use parking_lot::{Condvar, Mutex};

use super::{Rng, compiled, function, key, policy};
use crate::{
    LazyId, Purpose, Session,
    fetch::Priority,
    plan::{Function, Plan},
    runner::{self, Answers, Closed, Compiler, Config, Placer, Runner, TierUp},
    stub::{Abi, Isa, X86Save},
    wire::{self, ASK_KIND, Frame, READY_KIND},
};

/// The answers a runner sent, by client and sequence number, and the stops it asked for.
#[derive(Default)]
pub(crate) struct Mailbox {
    answers: Mutex<HashMap<(u32, u32), (Kind, Vec<u8>)>>,
    arrived: Condvar,
    stops: Mutex<Vec<(Option<(InstanceKey, ItemKey)>, String)>>,
}

impl Mailbox {
    /// The answer to `client`'s frame `seq`, once it comes.
    pub(crate) fn wait(&self, client: u32, seq: u32) -> (Kind, Vec<u8>) {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut answers = self.answers.lock();
        loop {
            if let Some(answer) = answers.remove(&(client, seq)) {
                return answer;
            }
            let left = deadline.checked_duration_since(Instant::now()).expect("an answer within a minute");
            self.arrived.wait_for(&mut answers, left);
        }
    }

    /// Whether `client`'s frame `seq` was answered yet, not waiting.
    pub(crate) fn has(&self, client: u32, seq: u32) -> bool {
        self.answers.lock().contains_key(&(client, seq))
    }

    pub(crate) fn stops(&self) -> Vec<(Option<(InstanceKey, ItemKey)>, String)> {
        self.stops.lock().clone()
    }
}

#[derive(Clone)]
pub(crate) struct Mail(pub(crate) Arc<Mailbox>);

impl Answers for Mail {
    fn send(&self, client: u32, seq: u32, kind: Kind, body: &[u8]) {
        let old = self.0.answers.lock().insert((client, seq), (kind, body.to_vec()));
        assert!(old.is_none(), "frame {seq} of client {client} answered twice");
        self.0.arrived.notify_all();
    }

    fn stop(&self, function: Option<(InstanceKey, ItemKey)>, why: &str) {
        self.0.stops.lock().push((function, why.to_owned()));
    }
}

/// Compiles slowly enough that asks overlap, and records every compile.
#[derive(Clone, Default)]
pub(crate) struct Counting {
    pub(crate) seen: Arc<Mutex<Vec<InstanceKey>>>,
    pub(crate) fail: Vec<InstanceKey>,
    pub(crate) panic: Vec<InstanceKey>,
    pub(crate) delay: Duration,
    /// What its probe finds compiled already; one also in `panic` panics the probe.
    pub(crate) hits: Vec<InstanceKey>,
}

impl Compiler for Counting {
    fn compile(&self, key: InstanceKey) -> Result<CompiledFunction, String> {
        self.seen.lock().push(key);
        blaze_spin::busy(self.delay);
        if self.fail.contains(&key) {
            return Err("its MIR could not be built".to_owned());
        }
        if self.panic.contains(&key) {
            panic!("boom");
        }
        let mut function = compiled(0, &[], &[]);
        function.key = key;
        Ok(function)
    }

    fn hit(&self, key: InstanceKey) -> Option<Result<CompiledFunction, String>> {
        if !self.hits.contains(&key) {
            return None;
        }
        if self.panic.contains(&key) {
            panic!("boom");
        }
        let mut function = compiled(0, &[], &[]);
        function.key = key;
        Some(Ok(function))
    }

    fn tier_up(&self, key: InstanceKey, to: Tier) -> Result<CompiledFunction, String> {
        Err(format!("{key:?} at {to:?}: these runs are not tiered"))
    }
}

impl Counting {
    fn times(&self, key: InstanceKey) -> usize {
        self.seen.lock().iter().filter(|it| **it == key).count()
    }
}

/// Places each function at the address its key was given.
pub(crate) struct Addresses(pub(crate) HashMap<InstanceKey, u64>);

impl Placer for Addresses {
    fn place(&mut self, function: &CompiledFunction) -> Result<u64, String> {
        self.0.get(&function.key).copied().ok_or_else(|| format!("no address for {:?}", function.key))
    }

    fn cells(&self, _: &InstanceKey) -> Option<u64> {
        None
    }

    fn replace(&mut self, slot: InstanceKey, _: &CompiledFunction) -> Result<u64, String> {
        Err(format!("{slot:?}: these runs are not tiered"))
    }
}

pub(crate) fn session(functions: &[Function]) -> Session {
    let plan = Plan::new(functions, &[], &[]).unwrap();
    let dev = policy(OptLevel::O0, Mode::Live);
    let isa = Isa::X86_64 { abi: Abi::SysV, save: X86Save::Fx };
    Session::new(&Purpose { profile: "dev", policy: &dev, output: None }, plan, &isa, 0).unwrap().0
}

fn target_of(n: u32) -> u64 {
    0x10_0000 + u64::from(n) * 0x40
}

fn addresses(count: u32) -> Addresses {
    Addresses((0..count).map(|n| (key(n), target_of(n))).collect())
}

/// Asks for `lazy` as `client`'s frame `seq` and waits for the answer.
fn ask<C: Compiler, P: Placer>(
    runner: &Runner<C, P, Mail>,
    mail: &Mailbox,
    client: u32,
    seq: u32,
    lazy: u32,
) -> (Kind, Vec<u8>) {
    runner.frame(client, ASK_KIND, seq, &wire::ask(u64::from(seq) << 8, lazy)).unwrap();
    mail.wait(client, seq)
}

#[test]
fn many_threads_asking_at_once_compile_each_function_once() {
    const FUNCTIONS: u32 = 8;
    const THREADS: u32 = 32;
    let functions: Vec<Function> = (0..FUNCTIONS).map(function).collect();
    let compiler = Counting { delay: Duration::from_millis(20), ..Counting::default() };
    let mail = Arc::new(Mailbox::default());
    let runner = Runner::start(
        session(&functions),
        &[],
        compiler.clone(),
        addresses(FUNCTIONS),
        Mail(Arc::clone(&mail)),
        Config { workers: 4, idle: false },
        None,
    )
    .unwrap();
    std::thread::scope(|scope| {
        for client in 0..THREADS {
            let (runner, mail) = (&runner, &mail);
            scope.spawn(move || {
                let mut rng = Rng::new(u64::from(client));
                let mut order: Vec<u32> = (0..FUNCTIONS).collect();
                for at in (1..order.len()).rev() {
                    order.swap(at, rng.below(at as u64 + 1) as usize);
                }
                for (seq, lazy) in order.into_iter().enumerate() {
                    let seq = seq as u32 + 1;
                    let (kind, body) = ask(runner, mail, client, seq, lazy);
                    assert_eq!(kind, READY_KIND);
                    assert_eq!(
                        Frame::decode(&body).unwrap(),
                        Frame::Ready { request: u64::from(seq) << 8, lazy, target: target_of(lazy) }
                    );
                }
            });
        }
    });
    for n in 0..FUNCTIONS {
        assert_eq!(compiler.times(key(n)), 1, "function {n}");
    }
    let closed: Closed = runner.close();
    assert_eq!(closed.compiles, u64::from(FUNCTIONS));
    assert!(closed.failures.is_empty());
    assert!(mail.stops().is_empty());
}

#[test]
fn a_failed_or_panicking_compile_answers_every_asker_and_stops_the_run_once() {
    let functions: Vec<Function> = (0..6).map(function).collect();
    let compiler = Counting {
        delay: Duration::from_millis(10),
        fail: vec![key(3)],
        panic: vec![key(5)],
        ..Counting::default()
    };
    let mail = Arc::new(Mailbox::default());
    let runner = Runner::start(
        session(&functions),
        &[],
        compiler.clone(),
        addresses(6),
        Mail(Arc::clone(&mail)),
        Config { workers: 3, idle: false },
        None,
    )
    .unwrap();
    let seq = AtomicU32::new(1);
    std::thread::scope(|scope| {
        for client in 0..8u32 {
            let (runner, mail, seq) = (&runner, &mail, &seq);
            scope.spawn(move || {
                for (lazy, why) in [(3, "its MIR could not be built"), (5, "the compiler panicked: boom")] {
                    let seq = seq.fetch_add(1, Ordering::Relaxed);
                    let (kind, body) = ask(runner, mail, client, seq, lazy);
                    assert_eq!(kind, wire::FAILED_KIND);
                    let Frame::Failed { lazy: of, why: text, .. } = Frame::decode(&body).unwrap() else {
                        panic!("not a failure");
                    };
                    assert_eq!(of, lazy);
                    assert!(text.contains(why) && text.contains("on its first call"), "{text}");
                }
            });
        }
    });
    assert_eq!((compiler.times(key(3)), compiler.times(key(5))), (1, 1));
    let stops = mail.stops();
    let stopped: Vec<_> = stops.iter().map(|it| it.0.map(|it| it.0)).collect();
    assert_eq!(stopped.len(), 2, "{stops:?}");
    assert!(stopped.contains(&Some(key(3))) && stopped.contains(&Some(key(5))));
    let closed = runner.close();
    assert_eq!(closed.failures.len(), 2);
}

#[test]
fn a_failure_reason_is_cut_to_what_an_ask_slot_carries() {
    let most = blaze_inproc::ask::MOST_BODY - wire::FAILED_LEN;
    assert_eq!(runner::fit("short"), "short");
    let long = "é".repeat(most);
    let cut = runner::fit(&long);
    assert!(cut.len() <= most && cut.len() >= most - 1 && long.starts_with(cut));
    let mut frame = Vec::new();
    Frame::Failed { request: 1, lazy: 2, why: cut }.encode(&mut frame);
    assert!(frame.len() <= blaze_inproc::ask::MOST_BODY);
}

#[test]
fn an_id_past_the_session_is_answered_failed_and_stops_the_run() {
    let mail = Arc::new(Mailbox::default());
    let runner = Runner::start(
        session(&[function(0)]),
        &[],
        Counting::default(),
        addresses(1),
        Mail(Arc::clone(&mail)),
        Config { workers: 1, idle: false },
        None,
    )
    .unwrap();
    let (kind, _) = ask(&runner, &mail, 0, 1, 7);
    assert_eq!(kind, wire::FAILED_KIND);
    assert_eq!(mail.stops().len(), 1);
    assert_eq!(mail.stops()[0].0, None);
    assert_eq!(runner.frame(0, READY_KIND, 2, &wire::ready(1, 0, 8)), Err(wire::WireError::Unasked));
    assert!(matches!(runner.frame(0, READY_KIND, 3, &wire::ask(1, 0)), Err(wire::WireError::Kind(_))));
}

#[test]
fn idle_workers_compile_the_rest_and_close_reports_what_failed_unasked() {
    let functions: Vec<Function> = (0..10).map(function).collect();
    let compiler = Counting { fail: vec![key(2)], ..Counting::default() };
    let mail = Arc::new(Mailbox::default());
    let runner = Runner::start(
        session(&functions),
        &[(LazyId(4), Priority::History)],
        compiler.clone(),
        addresses(10),
        Mail(Arc::clone(&mail)),
        Config { workers: 2, idle: true },
        None,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while (0..10).any(|n| runner.target(&key(n)).is_none() && n != 2) || compiler.times(key(2)) == 0 {
        assert!(Instant::now() < deadline, "idle compiles within a minute");
        std::thread::yield_now();
    }
    assert_eq!(compiler.seen.lock().first(), Some(&key(4)), "history before idle");
    let closed = runner.close();
    assert_eq!(closed.failures.len(), 1);
    assert_eq!(closed.failures[0].0, key(2));
    assert!(mail.stops().is_empty(), "nothing called it, so nothing stops");
    for n in 0..10 {
        assert!(compiler.times(key(n)) <= 1);
    }
}

/// Compiles at tier 0, and records every tier-up; with `fail`, a tier-up fails.
#[derive(Clone, Default)]
struct Tiers {
    ups: Arc<Mutex<Vec<(InstanceKey, Tier)>>>,
    fail: bool,
}

impl Compiler for Tiers {
    fn compile(&self, key: InstanceKey) -> Result<CompiledFunction, String> {
        Counting::default().compile(key)
    }

    fn hit(&self, _: InstanceKey) -> Option<Result<CompiledFunction, String>> {
        None
    }

    fn tier_up(&self, key: InstanceKey, to: Tier) -> Result<CompiledFunction, String> {
        self.ups.lock().push((key, to));
        if self.fail {
            return Err("its MIR could not be built".to_owned());
        }
        Counting::default().compile(key)
    }
}

const TIERED: u64 = 0x1000_0000;

/// Where blaze-inproc's [`CodeMemory`] is made.
const CODE_MEMORY: bool = cfg!(any(
    all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")),
    all(target_os = "macos", target_arch = "aarch64"),
    all(windows, target_arch = "x86_64"),
));

/// Places function `n` at [`target_of`], its tier-up [`TIERED`] past it; its tier-0 cells are
/// `n`'s pair of cells from `cells`, its tier-up's the pair after them.
struct Slots {
    addresses: Addresses,
    cells: u64,
    replaced: HashSet<InstanceKey>,
}

impl Slots {
    fn of(&self, key: &InstanceKey) -> u64 {
        let n = (0..64).find(|&n| super::key(n) == *key).unwrap();
        self.cells + u64::from(n) * 2 * u64::from(tier::CELLS)
    }
}

impl Placer for Slots {
    fn place(&mut self, function: &CompiledFunction) -> Result<u64, String> {
        self.addresses.place(function)
    }

    fn cells(&self, key: &InstanceKey) -> Option<u64> {
        let tiered = if self.replaced.contains(key) { u64::from(tier::CELLS) } else { 0 };
        Some(self.of(key) + tiered)
    }

    fn replace(&mut self, slot: InstanceKey, function: &CompiledFunction) -> Result<u64, String> {
        assert_eq!(slot, function.key);
        self.replaced.insert(slot);
        Ok(self.addresses.place(function)? + TIERED)
    }
}

fn wait_for(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "{what} within a minute");
        std::thread::yield_now();
    }
}

#[test]
fn hot_cells_tier_a_function_up_and_its_late_asks_get_the_new_code() {
    let Ok(mut memory) = CodeMemory::with_cells("lazy-tier", 1, 1, 1) else {
        assert!(!CODE_MEMORY);
        return;
    };
    let cells = memory.cells().offset;
    let functions: Vec<Function> = (0..3).map(function).collect();
    let compiler = Tiers::default();
    let mail = Arc::new(Mailbox::default());
    let up = TierUp {
        cells: memory.cell_view().unwrap(),
        policy: Policy { warm: 10, hot: u64::MAX, back_edge_weight: 1, highest: Tier::Cranelift },
        config: tier::Config { period: Duration::from_millis(1), workers: 1 },
    };
    let runner = Runner::start(
        session(&functions),
        &[],
        compiler.clone(),
        Slots { addresses: addresses(3), cells, replaced: HashSet::default() },
        Mail(Arc::clone(&mail)),
        Config { workers: 1, idle: false },
        Some(up),
    )
    .unwrap();
    runner.remembered(LazyId(2), Tier::Cranelift).unwrap();
    assert!(runner.remembered(LazyId(3), Tier::Cranelift).is_err());
    for lazy in 0..3 {
        let (kind, _) = ask(&runner, &mail, 0, lazy + 1, lazy);
        assert_eq!(kind, READY_KIND);
    }
    memory.store_word(cells + CALLS as u64, 100).unwrap();
    wait_for("the tier-ups", || {
        [0, 2].iter().all(|&n| runner.target(&key(n)) == Some(target_of(n) + TIERED))
    });
    let (_, body) = ask(&runner, &mail, 0, 4, 0);
    assert_eq!(Frame::decode(&body).unwrap(), Frame::Ready { request: 4 << 8, lazy: 0, target: target_of(0) + TIERED });
    assert_eq!(runner.target(&key(1)), Some(target_of(1)), "nothing counted in function 1");
    assert_eq!(runner.tier_stopped(), None);
    let closed = runner.close();
    let tiered = closed.tier.unwrap();
    assert_eq!(tiered.tiers, [Tier::Cranelift, Tier::Stencil, Tier::Cranelift]);
    assert_eq!((tiered.promotions, tiered.landed), (2, 2));
    assert!(tiered.failures.is_empty() && tiered.stopped.is_none(), "{tiered:?}");
    assert!(closed.unbound.is_empty() && closed.failures.is_empty());
    let mut ups = compiler.ups.lock().clone();
    ups.sort_by_key(|it| (0..3).position(|n| key(n) == it.0));
    assert_eq!(ups, [(key(0), Tier::Cranelift), (key(2), Tier::Cranelift)]);
    assert!(mail.stops().is_empty());
}

#[test]
fn a_failed_tier_up_leaves_the_first_code_and_is_reported() {
    let Ok(memory) = CodeMemory::with_cells("lazy-tier-fail", 1, 1, 1) else {
        assert!(!CODE_MEMORY);
        return;
    };
    let cells = memory.cells().offset;
    let compiler = Tiers { fail: true, ..Tiers::default() };
    let mail = Arc::new(Mailbox::default());
    let up = TierUp {
        cells: memory.cell_view().unwrap(),
        policy: Policy { highest: Tier::Cranelift, ..Policy::default() },
        config: tier::Config { period: Duration::from_millis(1), workers: 1 },
    };
    let runner = Runner::start(
        session(&[function(0)]),
        &[],
        compiler.clone(),
        Slots { addresses: addresses(1), cells, replaced: HashSet::default() },
        Mail(Arc::clone(&mail)),
        Config { workers: 1, idle: false },
        Some(up),
    )
    .unwrap();
    runner.remembered(LazyId(0), Tier::Cranelift).unwrap();
    let (kind, _) = ask(&runner, &mail, 0, 1, 0);
    assert_eq!(kind, READY_KIND);
    wait_for("the tier-up", || !compiler.ups.lock().is_empty());
    assert_eq!(runner.target(&key(0)), Some(target_of(0)));
    let closed = runner.close();
    let tiered = closed.tier.unwrap();
    assert_eq!(tiered.tiers, [Tier::Stencil]);
    assert_eq!(tiered.failures.len(), 1, "{tiered:?}");
    let failure = &tiered.failures[0];
    assert_eq!((failure.function, failure.to), (0, Tier::Cranelift));
    assert!(failure.why.contains("its MIR could not be built") && failure.why.contains("recompiling"), "{failure:?}");
    assert!(closed.failures.is_empty() && mail.stops().is_empty());
}

#[test]
fn an_untiered_run_refuses_remembered_tiers() {
    let mail = Arc::new(Mailbox::default());
    let runner = Runner::start(
        session(&[function(0)]),
        &[],
        Counting::default(),
        addresses(1),
        Mail(Arc::clone(&mail)),
        Config { workers: 1, idle: false },
        None,
    )
    .unwrap();
    assert!(runner.remembered(LazyId(0), Tier::Cranelift).unwrap_err().contains("not tiered"));
    assert_eq!(runner.tier_stopped(), None);
    let closed = runner.close();
    assert!(closed.tier.is_none() && closed.unbound.is_empty());
}

/// Threads calling through real entries, whose hook asks a runner.
#[cfg(all(unix, any(target_arch = "x86_64", target_arch = "aarch64")))]
#[allow(unsafe_code)]
mod through_stubs {
    use std::collections::VecDeque;
    use std::sync::OnceLock;

    use super::*;
    use crate::stub;
    use crate::tests::exec::Code;

    const FUNCTIONS: u32 = 4;

    extern "C" fn f0(x: u64) -> u64 {
        x.wrapping_mul(3)
    }
    extern "C" fn f1(x: u64) -> u64 {
        x ^ 0x5555
    }
    extern "C" fn f2(x: u64) -> u64 {
        x.rotate_left(13)
    }
    extern "C" fn f3(x: u64) -> u64 {
        !x
    }

    const DIRECT: [extern "C" fn(u64) -> u64; FUNCTIONS as usize] = [f0, f1, f2, f3];

    struct World {
        runner: Runner<Counting, Addresses, Mail>,
        mail: Arc<Mailbox>,
        compiler: Counting,
        seq: AtomicU32,
        code: Code,
        layout: stub::Layout,
    }

    static WORLD: OnceLock<World> = OnceLock::new();

    extern "C" fn hook(lazy: u32) -> u64 {
        let Some(world) = WORLD.get() else { return 0 };
        let seq = world.seq.fetch_add(1, Ordering::Relaxed);
        if world.runner.frame(0, ASK_KIND, seq, &wire::ask(u64::from(seq), lazy)).is_err() {
            return 0;
        }
        match Frame::decode(&world.mail.wait(0, seq).1) {
            Ok(Frame::Ready { target, .. }) => target,
            _ => 0,
        }
    }

    fn world() -> &'static World {
        WORLD.get_or_init(|| {
            let functions: Vec<Function> = (0..FUNCTIONS).map(function).collect();
            let compiler = Counting { delay: Duration::from_millis(20), ..Counting::default() };
            let mail = Arc::new(Mailbox::default());
            let places = (0..FUNCTIONS).map(|n| (key(n), DIRECT[n as usize] as usize as u64)).collect();
            let runner = Runner::start(
                session(&functions),
                &[],
                compiler.clone(),
                Addresses(places),
                Mail(Arc::clone(&mail)),
                Config { workers: 2, idle: false },
                None,
            )
            .unwrap();
            let isa = Isa::host().unwrap();
            let stubs = stub::generate(&isa, FUNCTIONS, hook as *const () as u64).unwrap();
            World { runner, mail, compiler, seq: AtomicU32::new(1), code: Code::new(&stubs.bytes), layout: stubs.layout }
        })
    }

    #[test]
    fn concurrent_first_calls_through_entries_compile_once_and_all_continue() {
        let world = world();
        let entries: Vec<extern "C" fn(u64) -> u64> = (0..FUNCTIONS)
            .map(|n| {
                let at = world.code.address(world.layout.entry(LazyId(n))) as usize;
                // SAFETY: the entry forwards to the function its hook answers with, which is
                // `DIRECT[n]`, of this type.
                unsafe { std::mem::transmute::<usize, extern "C" fn(u64) -> u64>(at) }
            })
            .collect();
        std::thread::scope(|scope| {
            for thread in 0..16u64 {
                let entries = &entries;
                scope.spawn(move || {
                    let mut queue: VecDeque<usize> = (0..FUNCTIONS as usize).collect();
                    queue.rotate_left(thread as usize % FUNCTIONS as usize);
                    for n in queue {
                        let x = thread * 0x1_0001 + n as u64;
                        assert_eq!(entries[n](x), DIRECT[n](x), "thread {thread}, function {n}");
                    }
                });
            }
        });
        for n in 0..FUNCTIONS {
            assert_eq!(world.compiler.times(key(n)), 1, "function {n}");
        }
    }
}
