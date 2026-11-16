//! The machine's TLS trust anchors: the roots a connection checks a server's chain against,
//! read from the operating system's store when first asked (never at start), each handed on
//! as its certificate's DER.
//!
//! - **macOS** ([`macos`]): Security.framework, opened with `dlopen` on the first read and never
//!   at load, so a program that does not fetch links nothing of it. It reads the system's anchors
//!   (`SecTrustCopyAnchorCertificates`) and the admin and user trust-settings domains: a root an
//!   administrator or the user distrusts for SSL is dropped, and one they add (a TLS-inspecting
//!   proxy's) is kept. The user's decision wins over the admin's.
//! - **Linux** ([`linux`]): `SSL_CERT_FILE`, then the directories of `SSL_CERT_DIR`, as the
//!   driver's environment gives them (rule 75). When neither is set, the distributions' standard
//!   bundles, then `/etc/ssl/certs`. Each is PEM, decoded with blaze-base64 ([`pem`]).
//! - **An override**: a bundle path in the run's configuration replaces the system's store.
//!   The caller reads that file through its own file path and decodes it with
//!   [`pem::certificates`].
//!
//! No root found is a [`TrustError`] that names every place looked, with what each gave. It never
//! falls back to roots built in (rule 3).
//!
//! Sizes are bounded before anything is kept ([`MOST_FILE`], [`MOST_ROOTS`], [`MOST_DER`]).

use std::{
    alloc::Allocator,
    fmt,
    path::PathBuf,
};

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "Security.framework and CoreFoundation calls, opened at first use")]
pub mod macos;
pub mod pem;

/// The most bytes of one bundle file read.
pub const MOST_FILE: u64 = 16 << 20;
/// The most roots kept from all places together.
pub const MOST_ROOTS: u32 = 20_000;
/// The most bytes of one root's DER.
pub const MOST_DER: usize = 64 << 10;

/// Trust anchors: each one's DER, in the order found, with no two equal.
#[derive(Clone, Debug)] // clone: a completion's copy in tests (Done's derive)
pub struct Roots<A: Allocator> {
    bytes: Vec<u8, A>,
    /// The end of each root in `bytes`.
    ends: Vec<u32, A>,
    /// How many times the system's store was read to make these: the count belongs to the
    /// value read, not to the process, so two reads at once do not see each other's.
    reads: u32,
}

// By hand, not derived: a derive would ask `A: PartialEq`, which `Global` is not, and two
// sets of roots are equal by their bytes whatever holds them.
impl<A: Allocator> PartialEq for Roots<A> {
    fn eq(&self, other: &Roots<A>) -> bool {
        self.bytes == other.bytes && self.ends == other.ends
    }
}

impl<A: Allocator> Eq for Roots<A> {}

impl<A: Allocator + Clone> Roots<A> {
    pub fn new_in(alloc: A) -> Roots<A> {
        Roots { bytes: Vec::new_in(alloc.clone()), ends: Vec::new_in(alloc), reads: 0 }
    }
}

impl<A: Allocator> Roots<A> {
    /// How many times the system's store was read to make these (not part of equality).
    pub fn reads(&self) -> u32 {
        self.reads
    }

    /// How many roots.
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Each root's DER, in the order added.
    pub fn each(&self) -> impl Iterator<Item = &[u8]> {
        let mut start = 0usize;
        self.ends.iter().map(move |end| {
            let der = &self.bytes[start..*end as usize];
            start = *end as usize;
            der
        })
    }

    /// Whether `der` is one of the roots.
    pub fn holds(&self, der: &[u8]) -> bool {
        self.each().any(|it| it == der)
    }

    /// Adds `der` unless it is held already. False, and nothing added, past [`MOST_ROOTS`] or
    /// for a DER past [`MOST_DER`]: the caller says so by name.
    pub fn add(&mut self, der: &[u8]) -> Result<(), Over> {
        if der.len() > MOST_DER {
            return Err(Over::Der { len: der.len() });
        }
        if self.holds(der) {
            return Ok(());
        }
        if self.ends.len() as u32 >= MOST_ROOTS {
            return Err(Over::Roots);
        }
        let end = self.bytes.len() + der.len();
        let Ok(end) = u32::try_from(end) else { return Err(Over::Roots) };
        self.bytes.extend_from_slice(der);
        self.ends.push(end);
        Ok(())
    }
}

/// A bound a store passed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Over {
    /// More than [`MOST_ROOTS`] roots.
    Roots,
    /// One root of `len` bytes, past [`MOST_DER`].
    Der { len: usize },
}

/// Where roots were looked for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Place {
    /// A bundle file, and the variable that named it, if one did.
    File { path: PathBuf, from: Option<&'static str> },
    /// A directory of certificate files, and the variable that named it, if one did.
    Dir { path: PathBuf, from: Option<&'static str> },
    /// A framework of the system's, or one of its stores.
    System(&'static str),
}

/// What one place gave.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Why {
    /// It does not exist.
    Absent,
    /// It could not be read.
    Unreadable(std::io::ErrorKind),
    /// It is past [`MOST_FILE`].
    TooBig { len: u64 },
    /// It holds no certificate.
    NoCertificate,
    /// Its PEM is broken.
    Pem(pem::PemError),
    /// A bound passed.
    Over(Over),
    /// The framework, or `symbol` in it, would not load.
    Missing { symbol: Option<&'static str> },
    /// A call of the framework failed with this `OSStatus`.
    Status { call: &'static str, status: i32 },
    /// It gave roots: the count.
    Gave(u32),
}

/// One place looked, and what it gave.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Looked {
    pub place: Place,
    pub why: Why,
}

/// Why the machine's roots could not be read: every place looked, in order, and what each
/// gave. Either no place gave a root, or one failed in a way that could hide a distrust (a
/// trust-settings domain that would not read), so nothing is trusted.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TrustError {
    pub looked: Vec<Looked>,
}

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Place::File { path, from: Some(var) } => write!(f, "{} (from {var})", path.display()),
            Place::File { path, from: None } => write!(f, "{}", path.display()),
            Place::Dir { path, from: Some(var) } => write!(f, "{}/ (from {var})", path.display()),
            Place::Dir { path, from: None } => write!(f, "{}/", path.display()),
            Place::System(name) => f.write_str(name),
        }
    }
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Why::Absent => f.write_str("not there"),
            Why::Unreadable(kind) => write!(f, "not readable ({kind})"),
            Why::TooBig { len } => write!(f, "{len} bytes, past the {MOST_FILE} read"),
            Why::NoCertificate => f.write_str("no certificate in it"),
            Why::Pem(e) => write!(f, "broken PEM: {e}"),
            Why::Over(Over::Roots) => write!(f, "more than {MOST_ROOTS} roots"),
            Why::Over(Over::Der { len }) => write!(f, "a root of {len} bytes, past {MOST_DER}"),
            Why::Missing { symbol: None } => f.write_str("would not load"),
            Why::Missing { symbol: Some(symbol) } => write!(f, "has no `{symbol}`"),
            Why::Status { call, status } => write!(f, "`{call}` failed with OSStatus {status}"),
            Why::Gave(n) => write!(f, "{n} roots"),
        }
    }
}

impl fmt::Display for TrustError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the machine's TLS roots cannot be read; looked at:")?;
        for looked in &self.looked {
            write!(f, "\n  {}: {}", looked.place, looked.why)?;
        }
        Ok(())
    }
}

impl std::error::Error for TrustError {}

/// Where the driver's environment points for roots: `SSL_CERT_FILE` and `SSL_CERT_DIR`, as
/// given (rule 75). Read on Linux only; macOS's store is its keychains.
#[derive(Clone, Copy, Default, Debug)]
pub struct Places<'e> {
    pub cert_file: Option<&'e std::ffi::OsStr>,
    pub cert_dir: Option<&'e std::ffi::OsStr>,
}

/// The machine's roots, read now: the effect a driver answers on the first connection's
/// asking, never at start.
#[cfg(target_os = "macos")]
pub fn system<A: Allocator + Clone>(_places: Places<'_>, alloc: A) -> Result<Roots<A>, TrustError> {
    macos::roots(alloc)
}

/// The machine's roots, read now: the effect a driver answers on the first connection's
/// asking, never at start.
#[cfg(target_os = "linux")]
pub fn system<A: Allocator + Clone>(places: Places<'_>, alloc: A) -> Result<Roots<A>, TrustError> {
    linux::roots(places, alloc)
}

#[cfg(test)]
mod tests;
