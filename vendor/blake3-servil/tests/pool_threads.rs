//! initialize_multithreaded(Some(n)): the multithreaded calls hash on n
//! threads. A process of its own (the number is process-wide): 1 starts no
//! workers, the calls still hash rightly, a later call with None keeps the
//! number, and another number panics.
#![cfg(feature = "std")]

use core::num::NonZeroUsize;

/// The process's threads (Linux), from /proc.
#[cfg(target_os = "linux")]
fn threads() -> usize {
    std::fs::read_dir("/proc/self/task").expect("/proc/self/task").count()
}

#[test]
fn one_thread_starts_no_workers() {
    blake3_servil::initialize();
    #[cfg(target_os = "linux")]
    let before = threads();
    blake3_servil::initialize_multithreaded(Some(NonZeroUsize::MIN));
    let input: Vec<u8> = (0..8u32 << 20).map(|i| (i * 31 + i / 509) as u8).collect();
    let blocks: Vec<blake3_servil::performance::Block> = input.chunks_exact(64).map(|b| blake3_servil::performance::Block(b.try_into().unwrap())).collect();
    assert_eq!(blake3_servil::performance::hash_multithreaded(blake3_servil::Mode::Plain, &blocks, input.len()), blake3_servil::convenience::hash(&input));
    blake3_servil::initialize_multithreaded(None);
    blake3_servil::initialize_multithreaded(Some(NonZeroUsize::MIN));
    assert_eq!(blake3_servil::performance::hash_multithreaded(blake3_servil::Mode::Plain, &blocks, input.len()), blake3_servil::convenience::hash(&input));
    // No other thread: no workers, no SME2 thread.
    #[cfg(target_os = "linux")]
    assert!(threads() <= before, "{before} threads before, {} after", threads());
    // Where panics unwind.
    #[cfg(panic = "unwind")]
    {
        let other = std::panic::catch_unwind(|| blake3_servil::initialize_multithreaded(NonZeroUsize::new(3)));
        assert!(other.is_err(), "another number panics");
    }
}
