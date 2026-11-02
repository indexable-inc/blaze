//! A child forked after the pool started its threads has none of them:
//! its multithreaded calls panic, saying so, while its single-threaded
//! calls and the parent's calls go on (Zooko, October 26: a forked child
//! that looped on a stream's `collect` waited forever). A binary of its
//! own: the pool is the process's.

use blake3_servil::performance::{self, Block};
use blake3_servil::{Mode, convenience};

#[test]
fn a_forked_childs_multithreaded_calls_panic_and_its_single_threaded_ones_work() {
    blake3_servil::initialize_multithreaded(core::num::NonZeroUsize::new(2));
    let blocks = vec![Block([7u8; 64]); (1 << 20) / 64];
    let bytes: Vec<u8> = blocks.iter().flat_map(|b| b.0).collect();
    let want = convenience::hash(&bytes);
    assert_eq!(performance::hash_multithreaded(Mode::Plain, &blocks, bytes.len()), want, "the parent, before the fork");
    // Sound: the child only hashes, catches a panic and exits.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork");
    if pid == 0 {
        let single = convenience::hash(&bytes) == want;
        let multi = std::panic::catch_unwind(|| performance::hash_multithreaded(Mode::Plain, &blocks, bytes.len()));
        let says = match &multi {
            Err(e) => e.downcast_ref::<&str>().map(|m| m.to_string()).or_else(|| e.downcast_ref::<String>().cloned()).is_some_and(|m| m.contains("forked")),
            Ok(_) => false,
        };
        // Sound: _exit ends the child at once, with no handlers.
        unsafe {
            libc::_exit(if !single {
                2
            } else if multi.is_ok() {
                3
            } else if says {
                0
            } else {
                1
            })
        }
    }
    let mut status = 0;
    // Sound: waits for the child above.
    assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
    assert!(libc::WIFEXITED(status), "the child exited (status {status})");
    assert_eq!(libc::WEXITSTATUS(status), 0, "1: a panic without the message, 2: the single-threaded hash wrong, 3: no panic");
    assert_eq!(performance::hash_multithreaded(Mode::Plain, &blocks, bytes.len()), want, "the parent, after the fork");
}
