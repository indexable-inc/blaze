//! The threads of an evaluation. Each has its own code stack, stack region, thread-local
//! keys, `#[thread_local]` statics and panic being unwound. One runs at a time: the running
//! one's are the evaluator's own fields (`stack`, `code_stack`, ...), the others' wait in
//! [`Thread`], and the machine's `schedule` picks which runs at every terminator of the
//! outermost run.

use std::mem;

use blaze_hash::HashMap;
use hir_def::StaticId;
use smallvec::SmallVec;

use super::{
    Address, Allocated, BasicBlockId, Evaluator, Interval, MirBody, MirEvalError, MirSpan,
    OwnedBytes, Result, Running, StackFrame, Then,
    machine::{EvalThread, Machine},
    not_supported,
    shim::{Resume, TlsData},
    stack_offset,
    unwind::Unwinding,
};

/// What a blocked thread waits for; the evaluator wakes it and finishes the call that blocked.
pub(super) enum Wait {
    /// `pthread_join`: `thread` to end. On waking, its return value (a pointer) is written to
    /// `retval` unless that is null, then `0` to the blocked call's destination.
    Join { thread: EvalThread, retval: Address },
    /// A shim's wait on the object at `addr` (a futex word, a mutex, a condition variable):
    /// woken by a `wake` of `addr` whose bitset meets `bitset` (`u32::MAX` for a plain wait),
    /// or once the virtual clock passes `deadline`, which it moves to only when nothing else
    /// can run. The shim goes on with `resume` ([`Evaluator::resume_shim`]) on waking.
    Shim { addr: usize, bitset: u32, deadline: Option<u64>, resume: Resume },
}

/// The bytes of the stack region of each thread: thread `t`'s starts at `t` times this.
pub(super) fn thread_stack_span() -> usize {
    stack_offset() >> 4
}

/// How many threads the addresses of the stack have room for.
const MAX_THREADS: usize = 240;

/// The thread whose stack region the stack offset `offset` is in, and the position in it.
#[inline]
pub(super) fn stack_position(offset: usize) -> (usize, usize) {
    let span = thread_stack_span();
    (offset / span, offset % span)
}

/// The bytes of thread `t`'s stack: `stack`, the evaluator's own, for the running one.
#[inline]
pub(super) fn region<'a>(
    stack: &'a [u8],
    threads: &'a [Thread<'_>],
    running: EvalThread,
    t: usize,
) -> Option<&'a [u8]> {
    match t == running as usize {
        true => Some(stack),
        false => threads.get(t).map(|it| &it.stack[..]),
    }
}

#[inline]
pub(super) fn region_mut<'a>(
    stack: &'a mut Vec<u8>,
    threads: &'a mut [Thread<'_>],
    running: EvalThread,
    t: usize,
) -> Option<&'a mut Vec<u8>> {
    match t == running as usize {
        true => Some(stack),
        false => threads.get_mut(t).map(|it| &mut it.stack),
    }
}

/// The stack allocations of thread `t`, as [`region`] its bytes.
#[inline]
pub(super) fn region_allocations<'a>(
    allocations: &'a [Allocated],
    threads: &'a [Thread<'_>],
    running: EvalThread,
    t: usize,
) -> &'a [Allocated] {
    match t == running as usize {
        true => allocations,
        false => threads.get(t).map_or(&[], |it| &it.stack_allocations[..]),
    }
}

pub(super) enum Run {
    /// Not started yet: it calls `start` with `arg` when first picked.
    Start { start: usize, arg: Address },
    Runnable,
    /// Waits on `wait`, to finish the call whose destination is `destination`. `since` orders
    /// the waiters of one futex, longest-waiting first.
    Blocked { wait: Wait, destination: Interval, since: u64 },
    /// Its start function returned and its exit destructors ran.
    Ended,
}

/// What a woken thread does as it goes on, in its own context: the machine sees the
/// synchronization, then the call it blocked in finishes.
#[derive(Default)]
struct Woken {
    /// The object it waited on, which it acquires (`Machine::acquire`) unless it timed out.
    acquire: Option<u64>,
    /// The thread it joined (`Machine::join`).
    joined: Option<EvalThread>,
    /// A pointer to write (`retval` of a join), and the bytes of the call's result.
    pointer: Option<(Address, Address)>,
    result: Option<(Interval, OwnedBytes)>,
    /// The shim's wait to go on with, whether it timed out, and the call's destination.
    shim: Option<(Resume, bool, Interval)>,
}

pub(super) struct Thread<'db> {
    pub(super) run: Run,
    /// While another thread runs: its frames, the block its top frame goes on at, its
    /// stack's bytes and allocations, and the rest of what the evaluator keeps of the
    /// running thread.
    frames: Vec<StackFrame<'db>>,
    block: BasicBlockId,
    stack: Vec<u8>,
    stack_allocations: Vec<Allocated>,
    tls: TlsData,
    statics: HashMap<StaticId, Address>,
    unwinding: Option<Unwinding<'db>>,
    running: Option<Running<'db>>,
    woken: Woken,
    /// Calls to make when its start function returns, last registered first.
    at_exit: Vec<(usize, Address)>,
    /// What its start function returned, once it did.
    result: Option<Address>,
    /// Where its start function's return value is written.
    result_slot: Interval,
    joined: bool,
    detached: bool,
    /// The body that made it and the call's span: its start's types are read in that body.
    origin: (&'db MirBody<'db>, MirSpan),
}

impl<'db> Thread<'db> {
    fn new(run: Run, result_slot: Interval, origin: (&'db MirBody<'db>, MirSpan)) -> Self {
        Thread {
            run,
            frames: Vec::new(), // alloc: empty, allocates nothing; one per spawned thread
            block: origin.0.start_block,
            stack: Vec::new(), // alloc: empty, allocates nothing; one per spawned thread
            stack_allocations: Vec::new(), // alloc: empty, allocates nothing; one per spawned thread
            tls: TlsData::default(),
            statics: HashMap::default(),
            unwinding: None,
            running: None,
            woken: Woken::default(),
            at_exit: Vec::new(), // alloc: empty, allocates nothing; one per spawned thread
            result: None,
            result_slot,
            joined: false,
            detached: false,
            origin,
        }
    }

}

impl<'db, M: Machine<'db>> Evaluator<'db, M> {
    /// The running thread.
    pub(super) fn thread(&self) -> EvalThread {
        self.thread
    }

    /// Where the running thread's stack region starts, in the stack's offsets.
    pub(super) fn stack_base(&self) -> usize {
        self.thread as usize * thread_stack_span()
    }

    /// A new thread calling `start` (the bytes of a function pointer, `extern "C" fn(*mut u8)
    /// -> *mut u8`) with `arg`; it first runs when the scheduler picks it. Tells the machine.
    pub(super) fn spawn_thread(
        &mut self,
        start: usize,
        arg: Address,
        span: MirSpan,
    ) -> Result<'db, EvalThread> {
        let Some(origin) = self.body_running else {
            not_supported!("spawning a thread outside of a function's body");
        };
        if self.threads.is_empty() {
            // The main thread, which runs: its state is the evaluator's own fields.
            let slot = Interval::new(Address::invalid(1), 0);
            self.threads.push(Thread::new(Run::Runnable, slot, (origin, span)));
        }
        if self.threads.len() >= MAX_THREADS {
            not_supported!("more than {MAX_THREADS} threads");
        }
        let id = self.threads.len() as EvalThread;
        let size = self.ptr_size();
        let result_slot = Interval::new(self.heap_allocate(size, size)?, size);
        self.machine.spawn(id)?;
        self.threads.push(Thread::new(Run::Start { start, arg }, result_slot, (origin, span)));
        Ok(id)
    }

    /// Blocks the running thread on `wait` once the shim returns (the shim writes nothing to
    /// the destination): its call completes when it wakes. A wait that is already satisfied
    /// (`Join` of an ended thread) completes at once.
    pub(super) fn block_on(&mut self, wait: Wait) {
        self.waiting = Some(wait);
    }

    /// Wakes up to `count` threads waiting on the futex `addr` whose bitset meets `bitset`,
    /// longest-waiting first; how many woke. Tells the machine (`release`, then `acquire` as
    /// each woken one goes on).
    pub(super) fn wake(&mut self, addr: usize, bitset: u32, count: usize) -> Result<'db, usize> {
        self.machine.release(addr as u64)?;
        let mut waiters: SmallVec<[(u64, usize); 8]> = self
            .threads
            .iter()
            .enumerate()
            .filter_map(|(i, thread)| match &thread.run {
                Run::Blocked { wait: Wait::Shim { addr: at, bitset: bits, .. }, since, .. }
                    if *at == addr && bits & bitset != 0 =>
                {
                    Some((*since, i))
                }
                _ => None,
            })
            .collect(); // alloc: inline `SmallVec`, on the stack for up to eight waiters
        waiters.sort_unstable();
        let woken = waiters.len().min(count);
        for &(_, i) in &waiters[..woken] {
            self.wake_thread(i, false);
        }
        Ok(woken)
    }

    /// `pthread_detach`: nobody joins it; an error if it was joined or detached.
    pub(super) fn detach_thread(&mut self, thread: EvalThread) -> Result<'db, ()> {
        if !self.joinable(thread) {
            // alloc: error path
            return Err(MirEvalError::UndefinedBehavior(format!(
                "detaching thread {thread}, which was joined or detached, or never made"
            )));
        }
        self.machine.detach(thread)?;
        self.threads[thread as usize].detached = true;
        Ok(())
    }

    /// Whether `thread` was made and is not yet joined or detached.
    pub(super) fn joinable(&self, thread: EvalThread) -> bool {
        match self.threads.get(thread as usize) {
            Some(it) => !it.joined && !it.detached,
            // The main thread of a run that made no other.
            None => thread == 0 && self.threads.is_empty(),
        }
    }

    /// `sched_yield`: the machine's `yield_now`, then a switch where the call ends, to another
    /// thread if one can run.
    pub(super) fn yield_thread(&mut self) {
        self.machine.yield_now();
        self.yielded = true;
    }

    /// The running thread's pthread key values and the rest the shims keep per thread.
    pub(super) fn tls(&mut self) -> &mut TlsData {
        &mut self.thread_local_storage
    }

    /// A destructor to call (`func`: a function pointer's bytes, `unsafe extern "C"
    /// fn(*mut u8)`) with `arg` when the running thread's start function returns, last
    /// registered first. The main thread's are not run: the run ends when it returns.
    pub(super) fn at_thread_exit(&mut self, func: usize, arg: Address) {
        self.at_exit.push((func, arg));
    }

    /// The stack region of `thread`: its lowest address and its size.
    pub(super) fn thread_stack(&self, thread: EvalThread) -> (usize, usize) {
        let span = thread_stack_span();
        (Address::stack(thread as usize * span).to_usize(), span)
    }

    /// The virtual clock, in nanoseconds since the run started.
    pub(super) fn now(&self) -> u64 {
        self.clock
    }

    /// Moves the virtual clock on by `nanos`.
    pub(super) fn advance_clock(&mut self, nanos: u64) {
        self.clock = self.clock.saturating_add(nanos);
    }

    /// Whether the running thread may switch: there are others, and the evaluator is in its
    /// outermost run (a nested one is a Rust call of its own, which can't be suspended).
    pub(super) fn may_switch(&self) -> bool {
        self.threads.len() > 1 && self.nesting == 1
    }

    /// At a terminator: the thread the machine picks to run next, if not the running one.
    pub(super) fn pick_at_terminator(&mut self) -> Result<'db, Option<EvalThread>> {
        if !self.may_switch() {
            self.yielded = false;
            return Ok(None);
        }
        let yielded = mem::take(&mut self.yielded);
        let mut runnable = self.runnable();
        if yielded && runnable.len() > 1 {
            runnable.retain(|it| *it != self.thread);
        }
        let next = self.machine.schedule(self.thread, &runnable)?;
        if next == self.thread {
            return Ok(None);
        }
        if !runnable.contains(&next) {
            return Err(MirEvalError::InternalError(
                // alloc: error path
                format!("the machine scheduled thread {next}, which can't run").into(),
            ));
        }
        Ok(Some(next))
    }

    /// The threads that can run: the running one, those runnable or not started.
    fn runnable(&self) -> SmallVec<[EvalThread; 8]> {
        self.threads
            .iter()
            .enumerate()
            .filter(|(i, thread)| {
                *i as EvalThread == self.thread
                    || matches!(thread.run, Run::Runnable | Run::Start { .. })
            })
            .map(|(i, _)| i as EvalThread)
            .collect() // alloc: inline `SmallVec`, on the stack for up to eight threads
    }

    /// The call that just returned asked to block on `wait` (`block_on`): finished at once if
    /// the wait is satisfied already, else the running thread blocks. Whether it blocked.
    pub(super) fn block_call(&mut self, wait: Wait, destination: Interval) -> Result<'db, bool> {
        if let Wait::Join { thread, retval } = wait {
            let Some(joined) = self.threads.get(thread as usize) else {
                // alloc: error path
                return Err(MirEvalError::UndefinedBehavior(format!(
                    "joining thread {thread}, which was never made"
                )));
            };
            if joined.joined || joined.detached || thread == self.thread {
                // alloc: error path
                return Err(MirEvalError::UndefinedBehavior(format!(
                    "joining thread {thread}, which is the running one, or joined or detached"
                )));
            }
            self.threads[thread as usize].joined = true;
            if let Run::Ended = self.threads[thread as usize].run {
                self.machine.join(thread)?;
                let result = self.threads[thread as usize].result;
                self.finish_join(retval, result, destination)?;
                return Ok(false);
            }
        }
        if self.nesting > 1 {
            let name = self.running_name();
            not_supported!("blocking in {name}, which the evaluator runs nested");
        }
        let others_run = self.threads.iter().enumerate().any(|(i, thread)| {
            i != self.thread as usize && matches!(thread.run, Run::Runnable | Run::Start { .. })
        });
        if !others_run && let Wait::Shim { deadline: Some(deadline), resume, .. } = wait {
            // Nothing else can run, so the wait times out now.
            self.clock = self.clock.max(deadline);
            self.resume_shim(resume, true, destination)?;
            return match self.waiting.take() {
                Some(wait) => self.block_call(wait, destination),
                None => Ok(false),
            };
        }
        if self.threads.len() <= 1 {
            return Err(MirEvalError::UndefinedBehavior(
                // alloc: error path
                "deadlock: the only thread blocks".to_owned(),
            ));
        }
        self.wait_counter += 1;
        let since = self.wait_counter;
        let me = self.thread as usize;
        self.threads[me].run = Run::Blocked { wait, destination, since };
        Ok(true)
    }

    fn finish_join(
        &mut self,
        retval: Address,
        result: Option<Address>,
        destination: Interval,
    ) -> Result<'db, ()> {
        if retval.to_usize() != 0 {
            let result = result.unwrap_or(Address::invalid(0));
            self.write_pointer(retval, result)?;
        }
        destination.write_from_bytes(self, &0i32.to_le_bytes()[..destination.size])
    }

    /// Makes the blocked thread `i` runnable: its call finishes as it goes on, with its wait's
    /// result (`timed_out`: the timeout's).
    fn wake_thread(&mut self, i: usize, timed_out: bool) {
        let thread = &mut self.threads[i];
        let Run::Blocked { wait, destination, .. } = mem::replace(&mut thread.run, Run::Runnable)
        else {
            return;
        };
        match wait {
            Wait::Shim { addr, resume, .. } => {
                thread.woken.acquire = (!timed_out).then_some(addr as u64);
                thread.woken.shim = Some((resume, timed_out, destination));
            }
            Wait::Join { thread: joined, retval } => {
                thread.woken.joined = Some(joined);
                if retval.to_usize() != 0 {
                    let result = self.threads[joined as usize].result;
                    let thread = &mut self.threads[i];
                    thread.woken.pointer = Some((retval, result.unwrap_or(Address::invalid(0))));
                }
                self.threads[i].woken.result = Some((destination, OwnedBytes::from_slice(&0i32.to_le_bytes())));
            }
        }
    }

    /// The running thread's start function (or an exit destructor) returned, with its return
    /// place `returned`: the next exit destructor runs, else the thread ends, wakes its
    /// joiners and another thread runs. Where the thread to run goes on.
    pub(super) fn thread_returned(&mut self, returned: Interval) -> Result<'db, BasicBlockId> {
        let me = self.thread as usize;
        if self.threads[me].result.is_none() {
            let result = self.read_pointer(returned.addr)?;
            self.threads[me].result = Some(result);
        }
        if let Some(block) = self.next_exit_call()? {
            return Ok(block);
        }
        self.machine.exit()?;
        self.threads[me].run = Run::Ended;
        let joiners: SmallVec<[usize; 8]> = (0..self.threads.len())
            .filter(|&i| {
                matches!(
                    self.threads[i].run,
                    Run::Blocked { wait: Wait::Join { thread, .. }, .. } if thread as usize == me
                )
            })
            .collect(); // alloc: inline `SmallVec`, on the stack for up to eight joiners
        for i in joiners {
            self.wake_thread(i, false);
        }
        let block = self.threads[me].origin.0.start_block;
        self.switch_thread(None, block)
    }

    /// The main thread's start (`main`, a test) returned: its `at_thread_exit` destructors
    /// run, last registered first, as glibc and dyld run them at exit; key destructors don't.
    pub(super) fn main_returned(&mut self, body: &'db MirBody<'db>) -> Result<'db, ()> {
        let unit = Interval::new(Address::invalid(1), 0);
        while let Some((func, arg)) = self.at_exit.pop() {
            self.call_fn_pointer_word(func, &[arg], unit, body, MirSpan::Unknown, Then::Nested)?;
        }
        Ok(())
    }

    /// The next exit call of the running thread, made a frame: an `at_thread_exit`
    /// destructor, else a pthread key's destructor of a key whose value is not null (which
    /// is set to null first, as pthread does). Where it starts, if there is one.
    fn next_exit_call(&mut self) -> Result<'db, Option<BasicBlockId>> {
        let next = match self.at_exit.pop() {
            Some(it) => Some(it),
            None => self.next_key_destructor(),
        };
        let Some((func, arg)) = next else { return Ok(None) };
        let me = self.thread as usize;
        let (body, span) = self.threads[me].origin;
        let slot = self.threads[me].result_slot;
        let frame = self.call_fn_pointer_word(func, &[arg], slot, body, span, Then::Thread)?;
        Ok(frame.map(|frame| {
            let block = frame.locals.body.start_block;
            self.code_stack.push(frame);
            block
        }))
    }

    /// Switches to another thread: `next` if the machine picked it, else (the running thread
    /// blocked or ended) the one the machine picks of those that can run. The running thread,
    /// unless it ended, goes on later at `block` with its frames on [`Self::code_stack`].
    /// Where the thread switched to goes on.
    pub(super) fn switch_thread(
        &mut self,
        next: Option<EvalThread>,
        block: BasicBlockId,
    ) -> Result<'db, BasicBlockId> {
        let me = self.thread as usize;
        let next = match next {
            Some(it) => it,
            None => self.pick_after_block()?,
        };
        self.park(me, block);
        self.unpark(next as usize)
    }

    /// Of the threads that can run, the one the machine picks; when none can, the timed wait
    /// that ends first times out, and with none of those either the program deadlocked.
    fn pick_after_block(&mut self) -> Result<'db, EvalThread> {
        let runnable: SmallVec<[EvalThread; 8]> = self
            .threads
            .iter()
            .enumerate()
            .filter(|(_, thread)| matches!(thread.run, Run::Runnable | Run::Start { .. }))
            .map(|(i, _)| i as EvalThread)
            .collect(); // alloc: inline `SmallVec`, on the stack for up to eight threads
        if !runnable.is_empty() {
            let running = self.thread;
            return Ok(self.machine.schedule(running, &runnable)?);
        }
        // A scan, not a timing wheel: a few interpreted threads, and the order (deadline,
        // then how long blocked, then index) is the schedule's, which must stay exact.
        let timed = self
            .threads
            .iter()
            .enumerate()
            .filter_map(|(i, thread)| match &thread.run {
                Run::Blocked { wait: Wait::Shim { deadline: Some(deadline), .. }, since, .. } => {
                    Some((*deadline, *since, i))
                }
                _ => None,
            })
            .min();
        let Some((deadline, _, i)) = timed else {
            return Err(MirEvalError::UndefinedBehavior(
                // alloc: error path
                "deadlock: every thread is blocked".to_owned(),
            ));
        };
        self.clock = self.clock.max(deadline);
        self.wake_thread(i, true);
        Ok(i as EvalThread)
    }

    /// Moves the running thread's state into its entry (all of it at once, so the evaluator's
    /// own fields are then the next thread's to fill).
    fn park(&mut self, me: usize, block: BasicBlockId) {
        let thread = &mut self.threads[me];
        thread.frames = mem::take(&mut self.code_stack);
        thread.block = block;
        thread.stack = mem::take(&mut self.stack);
        thread.stack_allocations = mem::take(&mut self.stack_allocations);
        thread.tls = mem::take(&mut self.thread_local_storage);
        thread.statics = mem::take(&mut self.thread_statics);
        thread.unwinding = self.unwinding.take();
        thread.running = self.running.take();
        thread.at_exit = mem::take(&mut self.at_exit);
    }

    /// Makes thread `i` the running one, starting it if it is new. Where it goes on.
    fn unpark(&mut self, i: usize) -> Result<'db, BasicBlockId> {
        self.thread = i as EvalThread;
        let thread = &mut self.threads[i];
        self.code_stack = mem::take(&mut thread.frames);
        self.stack = mem::take(&mut thread.stack);
        self.stack_allocations = mem::take(&mut thread.stack_allocations);
        self.thread_local_storage = mem::take(&mut thread.tls);
        self.thread_statics = mem::take(&mut thread.statics);
        self.unwinding = thread.unwinding.take();
        self.running = thread.running.take();
        self.at_exit = mem::take(&mut thread.at_exit);
        let block = thread.block;
        let woken = mem::take(&mut thread.woken);
        if let Run::Start { start, arg } = mem::replace(&mut thread.run, Run::Runnable) {
            let (body, span) = thread.origin;
            let slot = thread.result_slot;
            self.stack.push(0);
            let Some(frame) = self.call_fn_pointer_word(start, &[arg], slot, body, span, Then::Thread)?
            else {
                not_supported!("a thread whose start function is a shim");
            };
            let block = frame.locals.body.start_block;
            self.code_stack.push(frame);
            return Ok(block);
        }
        if let Some(addr) = woken.acquire {
            self.machine.acquire(addr)?;
        }
        if let Some(joined) = woken.joined {
            self.machine.join(joined)?;
        }
        if let Some((at, to)) = woken.pointer {
            self.write_pointer(at, to)?;
        }
        if let Some((destination, bytes)) = woken.result {
            destination.write_from_bytes(self, &bytes[..destination.size.min(bytes.len())])?;
        }
        if let Some((resume, timed_out, destination)) = woken.shim {
            self.resume_shim(resume, timed_out, destination)?;
            if let Some(wait) = self.waiting.take()
                && self.block_call(wait, destination)?
            {
                return self.switch_thread(None, block);
            }
        }
        Ok(block)
    }

    /// What the running frame is, for errors.
    fn running_name(&self) -> String {
        match self.running.and_then(|(callee, _)| callee) {
            Some(func) => {
                hir_def::signatures::FunctionSignature::of(self.db, func).name.as_str().to_owned() // alloc: error path
            }
            None => "a closure or shim".to_owned(), // alloc: error path
        }
    }
}
