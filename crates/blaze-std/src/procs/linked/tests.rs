//! The spawner's provider: none in the simulation, refused by name.

use crate::io::{Io, Missing};

/// The simulation offers no spawner: `Host::spawner` is `Missing { capability: "spawner" }`,
/// never a spawner that would start a real process from a simulated world. Prints
/// "spawner: the simulation's is missing".
#[test]
fn the_simulation_has_no_spawner() {
    let host = Io::sim(7).need_host().expect("the simulation's host");
    let missing = host.spawner().err().expect("no spawner in the simulation");
    assert_eq!(missing, Missing { capability: "spawner" });
    std::println!("spawner: the simulation's is missing");
}

/// Host refusals from `blaze-core` preserve and display their raw OS errno when non-zero,
/// so exhaustion failures (e.g. EMFILE / EAGAIN / ENOMEM) are self-diagnosing.
#[test]
fn host_run_error_displays_os_errno_when_present() {
    use core::fmt::Write as _;
    use super::run::{RunError, RunFault, RunStep};

    struct StackBuf {
        bytes: [u8; 256],
        len: usize,
    }
    impl StackBuf {
        fn new() -> Self {
            Self { bytes: [0; 256], len: 0 }
        }
        fn as_str(&self) -> &str {
            core::str::from_utf8(&self.bytes[..self.len]).expect("utf8")
        }
    }
    impl core::fmt::Write for StackBuf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let end = self.len.checked_add(s.len()).ok_or(core::fmt::Error)?;
            if end > self.bytes.len() {
                return Err(core::fmt::Error);
            }
            self.bytes[self.len..end].copy_from_slice(s.as_bytes());
            self.len = end;
            Ok(())
        }
    }

    let without_os = RunError {
        step: RunStep::Start,
        proc: None,
        fault: RunFault::Host { what: "starting it", os: 0 },
    };
    let mut buf = StackBuf::new();
    write!(buf, "{without_os}").expect("fits");
    assert_eq!(buf.as_str(), "bstd::procs run child, Start: the host refused: starting it");

    let with_os = RunError {
        step: RunStep::Start,
        proc: None,
        fault: RunFault::Host { what: "starting it", os: 24 },
    };
    let mut buf2 = StackBuf::new();
    write!(buf2, "{with_os}").expect("fits");
    let rendered = buf2.as_str();
    assert!(
        rendered.starts_with("bstd::procs run child, Start: the host refused: starting it: ") && rendered.contains("24"),
        "expected OS errno in rendered error, got: {rendered}"
    );
}
