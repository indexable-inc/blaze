//! The simulated host behind the capabilities blaze-core has no effect for yet
//! (docs/v2/plan/design/std.md, "The rest of the set"): [`SimHost`], one value a simulated
//! world owns ([`Io::script`]), which a test scripts (the keys a terminal will send, a
//! camera's frame) and reads back (what was written, opened, signalled). Each such capability
//! is a type with its narrowing and its operations; its provider is this simulation alone,
//! so a native [`Io`] does not offer it ([`crate::host::OFFERS`]), and nothing here reaches
//! Rust std's ambient APIs to stand in for one (rule no-fallback).
//!
//! [`Refused`] is how a call on any of them fails, the one shape their per-area errors share.

use alloc::{string::String, vec::Vec};
use core::{fmt, net::IpAddr};

use crate::{
    Stopped,
    audio::SimAudio,
    cx::Cx,
    device::{Position, SimDevice},
    io::Io,
    keys::SimKey,
    locale::Locale,
    name::{AppName, DeviceName, ModuleName, ServiceName},
    power::Battery,
    procs::{ProcId, Signal, SimProc},
    requests::SimHttp,
    roots::Anchor,
    streams::SimStreams,
    term::{KeyEvent, SimTerm},
    url::Url,
    user::UserFacts,
    window::SimWindow,
};

/// How a call on a hosted capability failed.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum Refused {
    /// The capability does not reach the thing: it was narrowed (narrowing never widens).
    NotAllowed,
    /// This world has no provider for the capability: blaze-core has no effect for it yet, and
    /// only the simulation answers it.
    NoProvider,
    /// The host has no such thing (a key, a device, a process, a window).
    NotFound,
    /// The wait stopped first.
    Stopped(Stopped),
    /// The host refused, saying this.
    Host(&'static str),
    /// A stream's inbox already held `held` bytes the peer sent and no receive took, and the
    /// reply would have passed the `most` its opener said: receive, then send again.
    InboxFull { held: usize, most: usize },
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refused::NotAllowed => f.write_str("this capability does not reach it (it was narrowed)"),
            Refused::NoProvider => f.write_str(
                "this world has no provider for it: blaze-core has no effect for it yet, and only \
                 the simulation answers it",
            ),
            Refused::NotFound => f.write_str("the host has no such thing"),
            Refused::Stopped(stopped) => write!(f, "{stopped}"),
            Refused::Host(said) => write!(f, "the host refused: {said}"),
            Refused::InboxFull { held, most } => write!(
                f,
                "the stream's inbox holds {held} bytes not yet received and would pass the {most} \
                 its opener allowed: receive what is there first"
            ),
        }
    }
}

impl From<Stopped> for Refused {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    fn from(stopped: Stopped) -> Refused {
        Refused::Stopped(stopped)
    }
}

/// The simulated host: what its devices will say (scripted) and what was done to them
/// (recorded). Made by [`SimHost::new`] with every simulated world; changed and read by a test
/// through [`Io::script`].
#[derive(Debug)]
pub struct SimHost {
    /// Everything written to standard output ([`crate::out::Out`]), in order.
    pub out: Vec<u8>,
    /// Everything written to standard error, in order.
    pub err: Vec<u8>,
    pub term: SimTerm,
    /// The keychain.
    pub keys: Vec<SimKey>,
    /// The machine's trust anchors: what [`Io::roots`] hands out.
    pub roots: Vec<Anchor>,
    pub locale: Locale,
    pub user: UserFacts,
    /// The other processes.
    pub procs: Vec<SimProc>,
    /// Every signal sent, in order.
    pub signals: Vec<(ProcId, Signal)>,
    /// The modules of native code that can be loaded.
    pub modules: Vec<ModuleName>,
    /// Every module loaded, in order.
    pub loaded: Vec<ModuleName>,
    /// The length of every region made executable, in order.
    pub executable: Vec<usize>,
    pub windows: Vec<SimWindow>,
    /// The applications that can be driven.
    pub apps: Vec<AppName>,
    /// Every key synthesized into an application, in order.
    pub pressed: Vec<(AppName, KeyEvent)>,
    /// Every URL handed to its default application, in order.
    pub opened: Vec<Url>,
    pub battery: Battery,
    /// How many holds keep the machine awake now.
    pub awake: u32,
    pub audio: SimAudio,
    /// Cameras, displays, Bluetooth, USB and serial devices.
    pub devices: Vec<SimDevice>,
    /// Everything written to a Bluetooth device's service, in order.
    pub bluetooth_sent: Vec<(DeviceName, ServiceName, Vec<u8>)>,
    pub clipboard: Vec<u8>,
    /// Every notification posted, title then body, in order.
    pub notices: Vec<(String, String)>,
    /// Where the machine is.
    pub position: Position,
    /// What a name resolves to: the simulation's name service ([`crate::resolver`]).
    pub names: Vec<(String, IpAddr)>,
    /// The requests the host makes for the program ([`Io::http`]).
    pub http: SimHttp,
    /// The streams the host carries for the program ([`Io::streams`]).
    pub streams: SimStreams,
}

impl SimHost {
    /// A quiet host: a terminal of 80 by 24, the user `sim`, the locale of
    /// [`Locale::simulated`], a full battery, and nothing else (no keys, anchors, processes,
    /// windows, devices or names).
    #[blaze::nonblocking]
    pub fn new() -> SimHost {
        SimHost {
            // alloc: none, nothing written yet.
            out: Vec::new(),
            err: Vec::new(),
            term: SimTerm::new(),
            // alloc: none, the simulated host's empty tables (this and each below).
            keys: Vec::new(),
            roots: Vec::new(),
            locale: Locale::simulated(),
            user: UserFacts::simulated(),
            procs: Vec::new(),
            signals: Vec::new(),
            modules: Vec::new(),
            loaded: Vec::new(),
            executable: Vec::new(),
            windows: Vec::new(),
            apps: Vec::new(),
            pressed: Vec::new(),
            opened: Vec::new(),
            battery: Battery { percent: 100, charging: true },
            awake: 0,
            audio: SimAudio::new(),
            devices: Vec::new(),
            bluetooth_sent: Vec::new(),
            clipboard: Vec::new(),
            notices: Vec::new(),
            position: Position { lat_e7: 0, lon_e7: 0 },
            names: Vec::new(),
            http: SimHttp::default(),
            streams: SimStreams::default(),
        }
    }
}

impl Default for SimHost {
    #[blaze::nonblocking]
    fn default() -> SimHost {
        SimHost::new()
    }
}

/// One call on the simulated host under `cx`: `f` on it, at once. A `cx` that stopped already
/// asks nothing; a world with no simulated host is [`Refused::NoProvider`].
pub(crate) fn call<R>(io: &Io, cx: &Cx, f: impl FnOnce(&mut SimHost) -> Result<R, Refused>) -> Result<R, Refused> {
    cx.stopped_by(io.now())?;
    io.script(f).unwrap_or(Err(Refused::NoProvider))
}

/// A wait on the simulated host under `cx`, in the span `span`, which names what it waits for
/// (rule every-wait-says-what): until `ready` gives a value, or `cx`'s deadline or
/// cancellation ([`Cx::wait_for`], the one wait).
pub(crate) fn wait<R>(
    io: &Io,
    cx: &Cx,
    span: &'static str,
    mut ready: impl FnMut(&mut SimHost) -> Option<Result<R, Refused>>,
) -> Result<R, Refused> {
    if io.script(|_| ()).is_none() {
        return Err(Refused::NoProvider);
    }
    let cx = cx.span(span);
    cx.wait_for(io, || io.script(&mut ready).flatten())?
}
