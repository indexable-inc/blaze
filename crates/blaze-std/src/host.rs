//! The host (docs/v2/plan/design/std.md, `Host`): the process id, the cores it may use, the
//! host's name, and exiting with a status.
//!
//! The facts are the world's, read when it was made: the simulation's are chosen by the test,
//! a real world's by its entry (the host's name is handed in, as blaze-core has no effect that
//! asks for it). Signals are left out: blaze-core's only signal effect is `Kill` of a child
//! it spawned, and watching the process's own signals needs an effect it does not have.

use alloc::sync::Arc;
use core::num::NonZero;

use crate::{
    io::{Io, Missing, Offers},
    procs::Procs,
};

/// What a native world offers: everything its target's driver has an effect for
/// ([`Offers::NATIVE`]). The wasi driver keeps no sockets (blaze-core's `threads` driver runs
/// them on unix only), so there `net` is `None`.
pub const OFFERS: Offers = Offers { net: !cfg!(target_os = "wasi"), ..Offers::NATIVE };

/// What a world says about the machine it runs on.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a test's description of a world
pub struct HostFacts {
    pub pid: u32,
    /// The cores the process may use.
    pub cores: NonZero<usize>,
    pub name: Arc<str>,
}

impl HostFacts {
    /// The simulation's machine unless a test says otherwise: process 1 on a four-core host
    /// named `sim`.
    #[blaze::pure]
    #[blaze::nonblocking]
    pub fn simulated() -> HostFacts {
        HostFacts {
            pid: 1,
            cores: NonZero::<usize>::MIN.saturating_add(3),
            // alloc: once per simulated world, its host's name.
            name: Arc::from("sim"),
        }
    }
}

/// The process's exit, as the simulation unwinds it: a test's `catch_unwind` sees this payload
/// and [`Io::exited`] the status. A real world's exit ends the process.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Exit {
    pub status: i32,
}

/// The host: from [`Io::host`].
#[derive(Clone)]
pub struct Host {
    io: Io,
}

impl Host {
    #[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn new(io: Io) -> Host {
        Host { io }
    }

    pub fn pid(&self) -> u32 {
        self.io.host_facts(|facts| facts.pid)
    }

    pub fn cores(&self) -> NonZero<usize> {
        self.io.host_facts(|facts| facts.cores)
    }

    pub fn name(&self) -> Arc<str> {
        self.io.host_facts(|facts| Arc::clone(&facts.name))
    }

    /// Other processes, if the host offers them (the simulation does): its own entry of
    /// [`Offers`], apart from this process's own facts and exit.
    pub fn procs(&self) -> Option<Procs> {
        self.io.offered(|it| it.procs).then(|| Procs::new(self.io.clone()))
    }

    /// Other processes, which `main` requires.
    pub fn need_procs(&self) -> Result<Procs, Missing> {
        self.procs().ok_or(Missing { capability: "procs" })
    }

    /// The process ends with `status`. In the simulation the calling task unwinds with an
    /// [`Exit`] payload (no panic message) and the world remembers the status; a scope passes
    /// the unwind on rather than failing a task with it.
    pub fn exit(&self, status: i32) -> ! {
        self.io.exit(status)
    }
}

impl core::fmt::Debug for Host {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.io.host_facts(|facts| write!(f, "Host({facts:?})"))
    }
}
