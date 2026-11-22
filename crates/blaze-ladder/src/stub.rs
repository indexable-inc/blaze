//! A function this build could not make, behind its slot: a STUB that stops the program when
//! it is called (docs/v2/plan/design/bottom.md, section 11: "a function not yet compiled is a
//! stub"). The skeleton's first step of "compile on first call": every reached instance has a
//! slot, a refused one's slot holds [`code`] instead of a body, and the program starts. Only a
//! refused function that is CALLED stops the run, with its refusal by name; one nothing calls
//! costs a count in the command's closing line.
//!
//! The stub cannot return (it has no value to give: it would be a guess), so it leaves the
//! program: [`stopped`] puts the thread back where [`Stops::run`] entered the program
//! ([`enter`] kept that state, [`leave`] restores it), and the runner reports the refusal as
//! its own error. The launcher's stubs (blaze-launch-rt `table::resolve`, `Bound::Refused`)
//! replace these when the program runs in a child: the refusal's words are what the parent
//! answers with.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

/// The bytes of one stub.
pub const LEN: usize = 40;
/// Where the stub's two literal words are: the address of the run's [`Stops`], then of
/// [`stopped`]. Placement fills them (place.rs `Target::Stops`, `Target::Stop`).
pub const STOPS_AT: u32 = 24;
pub const STOP_AT: u32 = 32;

/// Stub `stub`'s code (AArch64, position-independent; the same bytes on any machine): a jump
/// to [`stopped`] with the run's `Stops` in `x0` and `stub` in `x1`. Its caller's arguments
/// are dropped: nothing reads them.
pub const fn code(stub: u32) -> [u8; LEN] {
    let words: [u32; 6] = [
        // ldr x0, #24 (the first literal word)
        0x5800_0000 | (6 << 5),
        // movz w1, #(stub & 0xffff)
        0x5280_0000 | ((stub & 0xffff) << 5) | 1,
        // movk w1, #(stub >> 16), lsl #16
        0x72a0_0000 | ((stub >> 16) << 5) | 1,
        // ldr x16, #20 (the second literal word)
        0x5800_0000 | (5 << 5) | 16,
        // br x16
        0xd61f_0200,
        // brk #0: never run; the literal words are 8-aligned after it.
        0xd420_0000,
    ];
    let mut out = [0u8; LEN];
    let mut i = 0;
    while i < words.len() {
        let bytes = words[i].to_le_bytes();
        out[i * 4] = bytes[0];
        out[i * 4 + 1] = bytes[1];
        out[i * 4 + 2] = bytes[2];
        out[i * 4 + 3] = bytes[3];
        i += 1;
    }
    out
}

/// What [`enter`] keeps of its caller for [`leave`]: `x19` to `x28`, `x29`, `x30`, `sp`, `d8`
/// to `d15` (every register a callee preserves in the AArch64 procedure call standard).
#[repr(C, align(16))]
pub struct Jump([u64; 21]);

/// How the program was left: `stopped` is 0 when its entry returned (`value` is then what it
/// returned in `x0`), 1 when a stub was called (`value` is the stub's number).
#[repr(C)]
pub struct Left {
    pub value: u64,
    pub stopped: u64,
}

/// Calls `function(first, second)` (C convention, two integer-class words) after keeping the
/// caller's preserved registers in `jump`, and gives what it returned.
///
/// The registers, by AAPCS64, which arm64 macOS (Apple's arm64 ABI) and arm64 Linux both follow
/// for integer and pointer arguments: the arguments in `x0`, `x1`, .. in order, an integer
/// result in `x0` (`w0` for an `int`), a 16-byte composite result ([`Left`]) in `x0` and `x1`.
/// So this function is entered with `function` in `x0`, `first` in `x1`, `second` in `x2`,
/// `jump` in `x3`, and calls `function` with `first` in `x0` and `second` in `x1`. One `enter`
/// serves every entry: `fn()` reads neither register, bstd's hosted entry reads the table in
/// `x0` only, a C `main(argc, argv)` reads `argc` in `w0` and `argv` in `x1`. An `int` is read
/// from `w0`: AAPCS64 leaves the upper 32 bits of `x0` unspecified, and Apple's ABI asks the
/// caller to extend only values narrower than 32 bits, so a non-negative `argc` passed
/// zero-extended is read right on both.
///
/// # Safety
///
/// `function` is the address of code that follows the C convention for at most two
/// integer-class parameters, each given its word here; `jump` is writable and stays so until
/// this returns. A [`leave`] with the same `jump` may run only while this call has not
/// returned, on the same thread.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
unsafe extern "C" fn enter(function: usize, first: usize, second: usize, jump: *mut Jump) -> Left {
    core::arch::naked_asm!(
        "stp x19, x20, [x3, #0]",
        "stp x21, x22, [x3, #16]",
        "stp x23, x24, [x3, #32]",
        "stp x25, x26, [x3, #48]",
        "stp x27, x28, [x3, #64]",
        "stp x29, x30, [x3, #80]",
        "mov x9, sp",
        "str x9, [x3, #96]",
        "stp d8, d9, [x3, #104]",
        "stp d10, d11, [x3, #120]",
        "stp d12, d13, [x3, #136]",
        "stp d14, d15, [x3, #152]",
        // A frame record of its own, so a walk of the stack by records goes on through it.
        "stp x29, x30, [sp, #-16]!",
        "mov x29, sp",
        // `x9` is a scratch register (AAPCS64: x9-x15 are not preserved), free here: the
        // function's address. The two words move down one register each, `first` before
        // `second` so neither is overwritten before it is read.
        "mov x9, x0",
        "mov x0, x1",
        "mov x1, x2",
        "blr x9",
        // The entry returned: `Left { value: x0, stopped: 0 }`.
        "mov x1, #0",
        "ldp x29, x30, [sp], #16",
        "ret",
    )
}

/// Returns from the [`enter`] that filled `jump`, as `Left { value, stopped: 1 }`: every frame
/// above that `enter` is dropped without running.
///
/// # Safety
///
/// The `enter` that filled `jump` has not returned and is on this thread's stack; no frame
/// between it and here holds anything that must be released (the frames are the program's
/// own code, which this run abandons, and [`stopped`]'s, which holds nothing).
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
pub(crate) unsafe extern "C" fn leave(jump: *const Jump, value: u64) -> ! {
    core::arch::naked_asm!(
        "ldp x19, x20, [x0, #0]",
        "ldp x21, x22, [x0, #16]",
        "ldp x23, x24, [x0, #32]",
        "ldp x25, x26, [x0, #48]",
        "ldp x27, x28, [x0, #64]",
        "ldp x29, x30, [x0, #80]",
        "ldr x9, [x0, #96]",
        "mov sp, x9",
        "ldp d8, d9, [x0, #104]",
        "ldp d10, d11, [x0, #120]",
        "ldp d12, d13, [x0, #136]",
        "ldp d14, d15, [x0, #152]",
        "mov x0, x1",
        "mov x1, #1",
        "ret",
    )
}

/// The status of a process a stub ended from a thread that is not the entry's (below).
pub const OTHER_THREAD_STATUS: i32 = 101;

/// One run's stops: where its entry was called from, and each stub's refusal in words.
pub struct Stops {
    jump: UnsafeCell<Jump>,
    /// The thread [`Stops::run`] is on (`pthread_self`, as an integer); 0 before a run.
    thread: AtomicUsize,
    /// By stub number: the refusal, for the one case that cannot go back to the runner.
    said: Box<[Box<str>]>,
}

impl Stops {
    pub fn new(said: Box<[Box<str>]>) -> Stops {
        Stops { jump: UnsafeCell::new(Jump([0; 21])), thread: AtomicUsize::new(0), said }
    }

    /// The buffer [`leave`] returns through: crate::trap's handler resumes a trapped run's
    /// thread at `leave` with it, as a stub's stop does.
    pub(crate) fn jump(&self) -> *mut Jump {
        self.jump.get()
    }

    /// Calls the program's entry `function(first, second)` on this thread (stub.rs `enter`:
    /// `first` in `x0`, `second` in `x1`): what it returned, or the stub that stopped it.
    /// `None` on a machine the stubs' code is not for.
    ///
    /// # Safety
    ///
    /// `function` is the address of placed code following the C convention for at most two
    /// integer-class parameters (none, bstd's host table, or a C `main`'s `argc` and `argv`),
    /// whose stubs were placed with this `Stops`' address; whatever `first` and `second` point
    /// to stays alive until this returns; one run at a time uses a `Stops`.
    pub unsafe fn run(&self, function: usize, first: usize, second: usize) -> Option<Left> {
        #[cfg(target_arch = "aarch64")]
        {
            // SAFETY: `pthread_self` has no precondition.
            self.thread.store(thread_token(), Ordering::Release);
            // SAFETY: the caller's contract for `function` and the two words; `jump` is this
            // `Stops`' own buffer, written only here (one run at a time) and read only by a
            // `leave` under this call.
            let left = unsafe { enter(function, first, second, self.jump.get()) };
            self.thread.store(0, Ordering::Release);
            Some(left)
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            let _ = (function, first, second, &self.jump);
            None
        }
    }
}

/// What a stub jumps to: the program called a function that is not built. On the entry's
/// thread the program is left and [`Stops::run`] returns the stub's number. On any other
/// thread of the program the entry's thread cannot be stopped from here, so the process
/// ends, after the refusal is written to standard error by name.
pub extern "C" fn stopped(stops: *const Stops, stub: u64) -> ! {
    // SAFETY: `stops` is the first literal word of a stub, which placement filled with the
    // address of the `Stops` the placed program owns (place.rs `Placed`), alive as long as
    // its code is mapped; only its `Sync` parts are read from another thread.
    let stops = unsafe { &*stops };
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: `pthread_self` has no precondition.
        let here = thread_token();
        if here == stops.thread.load(Ordering::Acquire) {
            // SAFETY: this thread is inside the `enter` that filled the buffer (`run` set
            // `thread` to it and clears it after); the frames dropped are the program's and
            // this function's, which holds no value with a destructor.
            unsafe { leave(stops.jump.get(), stub) }
        }
    }
    let said = stops.said.get(stub as usize).map_or("a stub this run did not make", |s| &**s);
    let parts: [&[u8]; 3] = [b"blaze run: the program called a function that is not built yet, on a thread it started: ", said.as_bytes(), b"\n"];
    for part in parts {
        // SAFETY: a write of live bytes to descriptor 2; the process ends below whether or
        // not its last words could be written.
        let _ = blaze_sys::posix::io::write(blaze_sys::Fd::STDERR, part);
    }
    // SAFETY: the process's end, as a signal handler would end it: other threads may hold
    // any lock, so nothing is flushed or unwound.
    blaze_sys::posix::signal::exit_now(OTHER_THREAD_STATUS)
}

fn thread_token() -> usize {
    #[cfg(target_os = "macos")]
    { blaze_sys::posix::thread::pthread_self() as usize }
    #[cfg(target_os = "linux")]
    { unsafe { libc::pthread_self() as usize } }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    { 0 }
}

#[cfg(test)]
mod tests {
    use core::ffi::c_void;

    use super::*;

    /// The words of `code`, decoded: the stub number is split over `movz`/`movk`, and each
    /// literal load points at its word.
    #[test]
    fn a_stub_loads_its_number_and_its_two_words() {
        let bytes = code(0x0012_3456);
        let word = |i: usize| u32::from_le_bytes([bytes[i * 4], bytes[i * 4 + 1], bytes[i * 4 + 2], bytes[i * 4 + 3]]);
        // ldr x0: the literal is 19 bits of words from this instruction.
        assert_eq!(((word(0) >> 5) & 0x7ffff) * 4, STOPS_AT);
        assert_eq!((word(1) >> 5) & 0xffff, 0x3456);
        assert_eq!((word(2) >> 5) & 0xffff, 0x0012);
        assert_eq!(12 + ((word(3) >> 5) & 0x7ffff) * 4, STOP_AT);
        assert_eq!(STOP_AT as usize + 8, LEN);
        println!("STUB: number 0x123456 in movz/movk, literal words at {STOPS_AT} and {STOP_AT}");
    }

    /// `enter` and `leave` on real code: a C-convention function that calls [`stopped`] comes
    /// back as `stopped: 1` with its number; one that returns comes back as `stopped: 0`.
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn a_stopped_entry_comes_back_with_its_stub() {
        extern "C" fn returns(_: *const c_void) -> i32 {
            7
        }
        extern "C" fn stops_at_41(stops: *const c_void) -> i32 {
            stopped(stops.cast::<Stops>(), 41)
        }
        let stops = Stops::new(Box::from([]));
        // SAFETY: both are C-convention functions of one pointer argument (the second word is
        // not read); `stops_at_41` is given this `Stops`, which it passes to `stopped` under
        // this `run`.
        let (plain, stub) = unsafe { (stops.run(returns as usize, 0, 0), stops.run(stops_at_41 as usize, (&raw const stops) as usize, 0)) };
        let (plain, stub) = (plain.expect("an AArch64 host"), stub.expect("an AArch64 host"));
        assert_eq!((plain.value as i32, plain.stopped), (7, 0));
        assert_eq!((stub.value, stub.stopped), (41, 1));
        println!("STOPS: a returning entry gave 7; a stopped one gave stub 41");
    }

    /// `enter` hands its two words to the entry in `x0` and `x1` (AAPCS64), as a C `main` takes
    /// `argc` in `w0` and `argv` in `x1`. Without `enter`'s `mov x1, x2`, `x1` still holds the
    /// first word and this gives 44, not 42; with `mov x0, x1` and `mov x1, x2` swapped, `x0`
    /// would be the second word too and it gives 22.
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn an_entry_of_two_words_gets_both_in_order() {
        extern "C" fn tens_and_ones(argc: i32, argv: usize) -> i32 {
            argc * 10 + argv as i32
        }
        let stops = Stops::new(Box::from([]));
        // SAFETY: a C-convention function of two integer-class parameters that calls no stub.
        let left = unsafe { stops.run(tens_and_ones as usize, 4, 2) }.expect("an AArch64 host");
        assert_eq!((left.value as i32, left.stopped), (42, 0));
        println!("TWO WORDS: 4 and 2 reached the entry in order: 42");
    }
}
