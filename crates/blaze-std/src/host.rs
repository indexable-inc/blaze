//! The host (docs/v2/plan/design/std.md, `Host`): the process id, the cores it may use, the
//! host's name, and exiting with a status.
//!
//! The facts are the world's, read when it was made: the simulation's are chosen by the test,
//! a real world's by its entry (the host's name is handed in, as blaze-core has no effect that
//! asks for it). Signals are left out: blaze-core's only signal effect is `Kill` of a child
//! it spawned, and watching the process's own signals needs an effect it does not have.

use alloc::sync::Arc;
use core::{num::NonZero, time::Duration};

use crate::{
    clock::Born,
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
    /// When the process began against the driver's clock.
    pub born: Born,
    /// The running image's linker id; `None` when it was linked without one.
    pub build_id: Option<BuildId>,
}

/// This program's identity: the id its linker wrote into the running image from the image's
/// content, Mach-O's `LC_UUID` on macOS (16 bytes) and the GNU build-id note on Linux (20 with
/// `--build-id=sha1`). Keys that start from it make a rebuilt program share nothing with the one
/// before it, with nothing bumped by hand (rules 21, 52). A native world reads it in place from
/// the image's mapped header (`rt::image`, no file opened); a simulated one holds what its test
/// set ([`HostFacts::simulated`]'s is fixed).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BuildId {
    bytes: [u8; 32],
    len: u8,
}

impl BuildId {
    /// `bytes` as an id: `None` when empty or longer than 32 bytes.
    #[blaze::nonblocking]
    pub fn of(bytes: &[u8]) -> Option<BuildId> {
        let mut out = [0; 32];
        if bytes.is_empty() {
            return None;
        }
        out.get_mut(..bytes.len())?.copy_from_slice(bytes);
        Some(BuildId { bytes: out, len: u8::try_from(bytes.len()).ok()? })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

impl core::fmt::Debug for BuildId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("BuildId(")?;
        for byte in self.as_bytes() {
            write!(f, "{byte:02x}")?;
        }
        f.write_str(")")
    }
}

/// The running image carries no linker id: it was linked without one. Nothing is put in its
/// place (rule 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NoBuildId;

impl core::fmt::Display for NoBuildId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(
            "bstd::host: this program's image carries no linker build id (Mach-O LC_UUID on macOS; on Linux the GNU \
             build-id note, written when linked with --build-id)",
        )
    }
}

impl core::error::Error for NoBuildId {}

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
            born: Born::Before(Duration::ZERO),
            build_id: BuildId::of(b"bstd simulated!!"),
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

    /// This program's identity: the id its linker wrote into the running image ([`BuildId`]).
    /// A program that keys what it keeps on its own code starts every key from it.
    pub fn build_id(&self) -> Result<BuildId, NoBuildId> {
        self.io.host_facts(|facts| facts.build_id).ok_or(NoBuildId)
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
