//! Socket options: one explicit value passed in at a connect, a listen or a bind
//! ([`crate::Effect::Connect`], [`crate::Effect::Listen`], [`crate::Effect::Bind`]), and the
//! one table of how each is set on each system (rule nothing-ambient: no default is changed
//! behind a caller; the value a caller does not name is [`TcpOptions::DEFAULT`], which is what
//! blaze-core did before it took options: no-delay on, a listen's address reusable).
//!
//! **Asking.** Each option is an [`Ask`]: [`Ask::Leave`] (the system's own), [`Ask::Must`]
//! (set, or the call fails naming it) or [`Ask::WhereAvailable`] (set where the system has it,
//! else skipped and said so in [`Skipped`]). An option the system lacks, asked
//! [`Ask::Must`], is refused by name ([`Unavailable`]) before any socket is made: never skipped
//! quietly (rule no-fallback). Whether a system has an option is [`Opt::on`], a pure function
//! of the option and the [`Platform`], so a caller (bstd) refuses it with a typed error before
//! it asks the driver, and the simulation answers the same.
//!
//! **The table.** [`knob`] is every system call's level and name, per system, from the pinned
//! `libc` crate's constants (Linux and macOS), except two macOS has and `libc` does not
//! (`TCP_ENABLE_ECN`, `TCP_NOTSENT_LOWAT`), taken from the SDK's `netinet/tcp.h` and named
//! where they are defined here. Each [`Opt`]'s doc holds the man page's sentence for it
//! (`man 4 tcp`, `man 4 ip`, `man 4 ip6`, `getsockopt(2)` of macOS's SDK; Linux's `tcp(7)`,
//! `ip(7)`, `socket(7)`).
//!
//! **What was applied.** A buffer size is the kernel's to round: Linux doubles what it is
//! given and caps it at `net.core.rmem_max`/`wmem_max`; macOS caps it at `kern.ipc.maxsockbuf`.
//! [`Buffers`] is what the kernel says after every option is set, read back with
//! `getsockopt`: a datagram socket's arrives in [`crate::Done::Bound`].
//!
//! Production `setsockopt`/`getsockopt` paths are 100% safe via [`crate::pod::bytes_of`] and
//! [`crate::pod::bytes_of_mut`]. The remaining `unsafe` blocks are strictly in socket bind/connect
//! helpers that pass a `sockaddr_storage` written by [`crate::net::sockaddr`].

use std::{alloc::Global, fmt};

/// How one option is asked.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Ask<T> {
    /// Not set: the system's own value.
    #[default]
    Leave,
    /// Set to this, or the call fails naming the option.
    Must(T),
    /// Set to this where the system has the option; where it has not, skipped, and the
    /// option is in the [`Skipped`] the call reports. The caller's explicit, per-option choice.
    WhereAvailable(T),
}

impl<T: Copy> Ask<T> {
    /// The value asked, if any.
    pub fn value(&self) -> Option<T> {
        match *self {
            Ask::Leave => None,
            Ask::Must(value) | Ask::WhereAvailable(value) => Some(value),
        }
    }

    /// Whether the caller said it may go without it.
    pub fn may_skip(&self) -> bool {
        matches!(self, Ask::WhereAvailable(_))
    }
}

/// A count of bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Bytes(pub u32);

/// A count of whole seconds.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Seconds(pub u32);

/// Keep-alive probes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum KeepAlive {
    /// `SO_KEEPALIVE` off.
    Off,
    /// `SO_KEEPALIVE` on: the first probe after `idle` without traffic, then one each
    /// `interval`, the connection dropped after `probes` unanswered. Each at least 1.
    On { idle: Seconds, interval: Seconds, probes: u32 },
}

/// What a hang-up does with bytes not yet sent.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Linger {
    /// `SO_LINGER` off: the close returns at once and the system sends what is left.
    Off,
    /// `SO_LINGER` on with no time: the close drops what is left and resets the connection
    /// (TCP's RST).
    Reset,
    /// `SO_LINGER` on for this long: the close waits until what is left is sent or the time
    /// is out. The wait is the driver's thread's: keep it short.
    Seconds(u16),
}

/// A listen's TCP Fast Open (`TCP_FASTOPEN`): the two systems take different values.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FastOpen {
    /// On, the system's queue: macOS's form (an int 1). Linux refuses it: it takes a queue.
    On,
    /// On with at most this many fast-open requests pending: Linux's form. macOS refuses it.
    Queue(u32),
}

/// The longest congestion-control name this type holds. A name the kernel does not know fails
/// the call naming `TCP_CONGESTION`.
pub const CONGESTION_MOST: usize = 15;

/// A congestion-control algorithm, by the name the kernel knows it by (`cubic`, `bbr`, `reno`).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Congestion {
    len: u8,
    name: [u8; CONGESTION_MOST],
}

impl Congestion {
    /// `name`, if it is 1 to [`CONGESTION_MOST`] bytes of ASCII letters, digits, `_` or `-`.
    pub const fn new(name: &str) -> Option<Congestion> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.len() > CONGESTION_MOST {
            return None;
        }
        let mut out = [0u8; CONGESTION_MOST];
        let mut at = 0;
        while at < bytes.len() {
            let byte = bytes[at];
            if !(byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-') {
                return None;
            }
            out[at] = byte;
            at += 1;
        }
        Some(Congestion { len: bytes.len() as u8, name: out })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.name[..usize::from(self.len)]
    }

    pub fn as_str(&self) -> &str {
        // Only ASCII is admitted by `new`.
        core::str::from_utf8(self.as_bytes()).expect("a congestion name is ASCII")
    }
}

impl fmt::Debug for Congestion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Congestion({})", self.as_str())
    }
}

/// A Differentiated Services codepoint (RFC 2474): the high six bits of IPv4's TOS byte and
/// IPv6's traffic class. The low two are ECN's (RFC 3168), which the kernel keeps for TCP and
/// a datagram's [`crate::dgram::Head::ecn`] says for UDP, so this type cannot set them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Dscp(u8);

impl Dscp {
    /// `codepoint`, if it is below 64.
    pub const fn new(codepoint: u8) -> Option<Dscp> {
        match codepoint < 64 {
            true => Some(Dscp(codepoint)),
            false => None,
        }
    }

    pub const fn codepoint(self) -> u8 {
        self.0
    }

    /// The TOS byte or traffic class it is, its ECN bits zero.
    pub const fn tos(self) -> u8 {
        self.0 << 2
    }
}

/// Path-MTU discovery of a datagram socket (Linux's `IP_PMTUDISC_*`; RFC 8899's DPLPMTUD wants
/// [`Pmtu::Probe`]). macOS has only the don't-fragment bit: [`Pmtu::Do`] sets it, [`Pmtu::Dont`]
/// clears it, and the rest are refused there by name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Pmtu {
    /// Never set DF; fragment locally.
    Dont,
    /// The route's discovery: DF set, fragmented where the path's MTU is known smaller.
    Want,
    /// Always DF; a send larger than the known path MTU fails.
    Do,
    /// DF set, the path MTU ignored: the asker probes it (QUIC).
    Probe,
    /// DF set, the interface's MTU used, the path's ignored.
    Interface,
    /// As `Interface`, but fragment where the interface's MTU is passed.
    Omit,
}

/// An IPv4 multicast group and local interface address (`IP_ADD_MEMBERSHIP`, `ip_mreq`).
/// `interface` is typically `Ipv4Addr::UNSPECIFIED` (`0.0.0.0`) for the default interface.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MulticastV4 {
    pub group: core::net::Ipv4Addr,
    pub interface: core::net::Ipv4Addr,
}

/// Every option, by what it does; [`Opt::name`] is the system's name for it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Opt {
    /// `TCP_NODELAY`. macOS tcp(4): "Under most circumstances, TCP sends data when it is
    /// presented; when outstanding data has not yet been acknowledged, it gathers small
    /// amounts of output to be sent in a single packet once an acknowledgement is received.
    /// ... TCP_NODELAY defeats this algorithm."
    NoDelay,
    /// `SO_RCVBUF`. getsockopt(2): "SO_SNDBUF and SO_RCVBUF are options to adjust the normal
    /// buffer sizes allocated for output and input buffers, respectively. ... The system
    /// places an absolute limit on these values." Linux socket(7): the kernel doubles the value
    /// set, and getsockopt returns the doubled value.
    RecvBuffer,
    /// `SO_SNDBUF`: as [`Opt::RecvBuffer`], for output.
    SendBuffer,
    /// `SO_KEEPALIVE` with its three times: Linux `TCP_KEEPIDLE`, macOS `TCP_KEEPALIVE` ("the
    /// amount of time, in seconds, that the connection must be idle before keepalive probes
    /// (if enabled) are sent"); `TCP_KEEPINTVL` ("the amount of time in seconds between
    /// successive keepalives sent to probe an unresponsive peer"); `TCP_KEEPCNT` ("the number
    /// of times a keepalive probe should be repeated if the peer is not responding").
    KeepAlive,
    /// `SO_LINGER` (macOS: `SO_LINGER_SEC`, "the same option as SO_LINGER except the linger
    /// time is in seconds"). getsockopt(2): "SO_LINGER controls the action taken when unsent
    /// messages are queued on socket and a close(2) is performed."
    Linger,
    /// `SO_REUSEADDR`. getsockopt(2): "indicates that the rules used in validating addresses
    /// supplied in a bind(2) call should allow reuse of local addresses."
    ReuseAddress,
    /// `SO_REUSEPORT`. getsockopt(2): "allows completely duplicate bindings by multiple
    /// processes if they all set SO_REUSEPORT before binding the port." On Linux the kernel
    /// spreads connections and datagrams across the sockets; on macOS the last bound gets them.
    ReusePort,
    /// `TCP_NOTSENT_LOWAT`. macOS tcp(4): "This option allows a TCP sender to control the
    /// amount of unsent data kept in the send socket buffer. The value of the option should be
    /// the maximum amount of unsent data in bytes."
    NotSentLowWater,
    /// `TCP_QUICKACK` (Linux): acknowledge at once rather than delayed, until the kernel's own
    /// heuristics leave quick-ack mode. A connect's alone: it is not kept by a listen.
    QuickAck,
    /// `TCP_FASTOPEN_CONNECT` (Linux): a connect's SYN carries the first write's bytes. macOS
    /// has fast open on a connect only through `connectx` with the data, which blaze-core's
    /// connect does not use: refused there.
    FastOpenConnect,
    /// `TCP_FASTOPEN` on a listen. macOS tcp(4): "The TCP listener can set this option to use
    /// TCP Fast Open feature."
    FastOpenListen,
    /// `TCP_CONGESTION` (Linux): the congestion-control algorithm, by name.
    Congestion,
    /// `IP_TOS` (IPv4) or `IPV6_TCLASS` (IPv6): the DSCP. macOS ip(4): "IP_TOS and IP_TTL may
    /// be used to set the type-of-service and time-to-live fields in the IP header for
    /// SOCK_STREAM and SOCK_DGRAM sockets."
    Dscp,
    /// `TCP_ENABLE_ECN` (macOS): "Using Explicit Congestion Notification (ECN) on TCP allows
    /// bi-directional end-to-end notification of congestion without dropping packets." Linux
    /// negotiates TCP's ECN by its `net.ipv4.tcp_ecn` setting, with no option per socket:
    /// refused there.
    TcpEcn,
    /// Linux `IP_MTU_DISCOVER`/`IPV6_MTU_DISCOVER`; macOS `IP_DONTFRAG`/`IPV6_DONTFRAG`
    /// ([`Pmtu`]).
    PathMtu,
    /// `SO_BROADCAST`: permit sending broadcast datagrams on a datagram socket.
    Broadcast,
    /// `IP_MULTICAST_TTL`: hop limit for outgoing IPv4 multicast datagrams.
    MulticastTtl,
    /// `IP_MULTICAST_LOOP`: whether outgoing IPv4 multicast datagrams loop back to local sockets.
    MulticastLoop,
    /// `IP_ADD_MEMBERSHIP`: join an IPv4 multicast group ([`MulticastV4`]).
    MulticastJoin,
}

impl Opt {
    /// Every option, in order.
    pub const ALL: [Opt; 19] = [
        Opt::NoDelay,
        Opt::RecvBuffer,
        Opt::SendBuffer,
        Opt::KeepAlive,
        Opt::Linger,
        Opt::ReuseAddress,
        Opt::ReusePort,
        Opt::NotSentLowWater,
        Opt::QuickAck,
        Opt::FastOpenConnect,
        Opt::FastOpenListen,
        Opt::Congestion,
        Opt::Dscp,
        Opt::TcpEcn,
        Opt::PathMtu,
        Opt::Broadcast,
        Opt::MulticastTtl,
        Opt::MulticastLoop,
        Opt::MulticastJoin,
    ];

    /// The system's name for it.
    pub const fn name(self) -> &'static str {
        match self {
            Opt::NoDelay => "TCP_NODELAY",
            Opt::RecvBuffer => "SO_RCVBUF",
            Opt::SendBuffer => "SO_SNDBUF",
            Opt::KeepAlive => "SO_KEEPALIVE",
            Opt::Linger => "SO_LINGER",
            Opt::ReuseAddress => "SO_REUSEADDR",
            Opt::ReusePort => "SO_REUSEPORT",
            Opt::NotSentLowWater => "TCP_NOTSENT_LOWAT",
            Opt::QuickAck => "TCP_QUICKACK",
            Opt::FastOpenConnect => "TCP_FASTOPEN_CONNECT",
            Opt::FastOpenListen => "TCP_FASTOPEN",
            Opt::Congestion => "TCP_CONGESTION",
            Opt::Dscp => "IP_TOS/IPV6_TCLASS",
            Opt::TcpEcn => "TCP_ENABLE_ECN",
            Opt::PathMtu => "IP_MTU_DISCOVER/IP_DONTFRAG",
            Opt::Broadcast => "SO_BROADCAST",
            Opt::MulticastTtl => "IP_MULTICAST_TTL",
            Opt::MulticastLoop => "IP_MULTICAST_LOOP",
            Opt::MulticastJoin => "IP_ADD_MEMBERSHIP",
        }
    }

    /// Whether `platform` has it. The simulation has every option; a platform that is not
    /// Linux or macOS has none (Windows sets its no-delay itself and takes no options).
    pub const fn on(self, platform: Platform) -> bool {
        match platform {
            Platform::Simulated => true,
            Platform::Other => false,
            Platform::Linux => !matches!(self, Opt::TcpEcn),
            Platform::Macos => !matches!(self, Opt::QuickAck | Opt::FastOpenConnect | Opt::Congestion),
        }
    }

    const fn bit(self) -> u32 {
        1 << (self as u32)
    }
}

/// The system a check is made for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Platform {
    Linux,
    Macos,
    /// [`crate::sim::Sim`]: every option, none with an effect but what the model reads.
    Simulated,
    /// Any other target.
    Other,
}

impl Platform {
    /// The system this was built for.
    pub const HOST: Platform = if cfg!(target_os = "linux") {
        Platform::Linux
    } else if cfg!(target_os = "macos") {
        Platform::Macos
    } else {
        Platform::Other
    };
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Platform::Linux => "Linux",
            Platform::Macos => "macOS",
            Platform::Simulated => "the simulation",
            Platform::Other => "this target (not Linux or macOS)",
        })
    }
}

/// Where options are set.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Side {
    /// A connect's socket, before it connects.
    Connect,
    /// A listen's socket, before it binds.
    Listen,
    /// A connection a listen accepted: the options that act per connection, set again.
    Accepted,
    /// A datagram socket, before it binds.
    Bind,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Side::Connect => "connect",
            Side::Listen => "listen",
            Side::Accepted => "accepted connection",
            Side::Bind => "datagram socket",
        })
    }
}

/// Why an option was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// The platform has no such option.
    NotOnPlatform,
    /// It does not act on this side (a listen's fast open asked of a connect).
    NotOnThisSide(Side),
    /// The platform has it, but not in this form; says which form it takes.
    Form(&'static str),
    /// It and another asked option say opposite things.
    Conflicts(Opt, &'static str),
}

/// An option refused before any socket was made: which, on which platform, and why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Unavailable {
    pub option: Opt,
    pub platform: Platform,
    pub why: Why,
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (name, platform) = (self.option.name(), self.platform);
        match self.why {
            Why::NotOnPlatform => write!(
                f,
                "socket option {name} is not on {platform}: refused (rule no-fallback); ask it \
                 Ask::WhereAvailable to go on without it"
            ),
            Why::NotOnThisSide(side) => {
                write!(f, "socket option {name} does not act on a {side}: refused (rule no-fallback)")
            }
            Why::Form(form) => write!(f, "socket option {name} on {platform}: {form}"),
            Why::Conflicts(other, said) => {
                write!(f, "socket option {name} with {}: {said}", other.name())
            }
        }
    }
}

impl std::error::Error for Unavailable {}

/// The options asked [`Ask::WhereAvailable`] that the platform lacks, and so were not set.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Skipped(u32);

impl Skipped {
    pub const NONE: Skipped = Skipped(0);

    pub fn contains(self, opt: Opt) -> bool {
        self.0 & opt.bit() != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    fn insert(&mut self, opt: Opt) {
        self.0 |= opt.bit();
    }

    /// Each option skipped, in [`Opt::ALL`]'s order.
    pub fn iter(self) -> impl Iterator<Item = Opt> {
        Opt::ALL.into_iter().filter(move |it| self.contains(*it))
    }
}

impl fmt::Debug for Skipped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter().map(Opt::name)).finish()
    }
}

/// A socket's buffer sizes as the kernel says them after every option was set
/// (`getsockopt` of `SO_RCVBUF` and `SO_SNDBUF`): what a buffer asked became.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Buffers {
    pub recv: Bytes,
    pub send: Bytes,
}

/// What the simulation says a buffer is that was not asked: it has no kernel to ask.
pub const SIMULATED_BUFFER: Bytes = Bytes(1 << 18);

impl Buffers {
    /// The simulation's: the sizes asked, else [`SIMULATED_BUFFER`].
    pub fn simulated(recv: Ask<Bytes>, send: Ask<Bytes>) -> Buffers {
        Buffers {
            recv: recv.value().unwrap_or(SIMULATED_BUFFER),
            send: send.value().unwrap_or(SIMULATED_BUFFER),
        }
    }
}

/// What a caller sees of its options once the socket is made: the buffers the kernel kept and
/// the options it went without.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Applied {
    pub buffers: Buffers,
    pub skipped: Skipped,
}

/// A TCP socket's options: one value for a connect and for a listen. An option that acts on
/// one side only ([`Opt::FastOpenConnect`], [`Opt::QuickAck`] on a connect;
/// [`Opt::FastOpenListen`] on a listen) asked [`Ask::Must`] of the other is refused there.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TcpOptions {
    pub no_delay: Ask<bool>,
    pub recv_buffer: Ask<Bytes>,
    pub send_buffer: Ask<Bytes>,
    pub keep_alive: Ask<KeepAlive>,
    pub linger: Ask<Linger>,
    pub reuse_address: Ask<bool>,
    pub reuse_port: Ask<bool>,
    pub not_sent_low_water: Ask<Bytes>,
    pub quick_ack: Ask<bool>,
    pub fast_open_connect: Ask<bool>,
    pub fast_open_listen: Ask<FastOpen>,
    pub congestion: Ask<Congestion>,
    pub dscp: Ask<Dscp>,
    pub ecn: Ask<bool>,
}

impl TcpOptions {
    /// What blaze-core did before it took options: writes sent at once (`TCP_NODELAY` on) and
    /// a local address reusable (`SO_REUSEADDR` on, as std's `TcpListener::bind` sets it, so a
    /// listen restarted while its old connections wait out TIME_WAIT binds). Nothing else set.
    pub const DEFAULT: TcpOptions = TcpOptions {
        no_delay: Ask::Must(true),
        recv_buffer: Ask::Leave,
        send_buffer: Ask::Leave,
        keep_alive: Ask::Leave,
        linger: Ask::Leave,
        reuse_address: Ask::Must(true),
        reuse_port: Ask::Leave,
        not_sent_low_water: Ask::Leave,
        quick_ack: Ask::Leave,
        fast_open_connect: Ask::Leave,
        fast_open_listen: Ask::Leave,
        congestion: Ask::Leave,
        dscp: Ask::Leave,
        ecn: Ask::Leave,
    };
}

impl Default for TcpOptions {
    fn default() -> TcpOptions {
        TcpOptions::DEFAULT
    }
}

static DEFAULT_TCP: TcpOptions = TcpOptions::DEFAULT;

/// A TCP effect's options: [`TcpOptions::DEFAULT`] unboxed, any other value boxed, so an
/// [`crate::Effect`] grows by one pointer and a connect with the default allocates nothing.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)] // clone: an effect's, as `Effect` is
pub struct Tcp(Option<Box<TcpOptions>>);

impl Tcp {
    pub const DEFAULT: Tcp = Tcp(None);

    pub fn new(options: TcpOptions) -> Tcp {
        match options == TcpOptions::DEFAULT {
            true => Tcp(None),
            // alloc: a connect's or listen's options, once per call that names them.
            false => Tcp(Some(Box::new_in(options, Global))),
        }
    }

    pub fn get(&self) -> &TcpOptions {
        match &self.0 {
            Some(options) => options,
            None => &DEFAULT_TCP,
        }
    }

    pub fn is_default(&self) -> bool {
        self.0.is_none()
    }
}

/// A UDP socket's tuning; the options its datagrams are read and written by
/// ([`crate::dgram::Options`]'s `ecn`, `local_address`, `dont_fragment`, `coalesce`) are its
/// neighbours there, this value one field of it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct UdpOptions {
    pub recv_buffer: Ask<Bytes>,
    pub send_buffer: Ask<Bytes>,
    pub reuse_address: Ask<bool>,
    pub reuse_port: Ask<bool>,
    pub dscp: Ask<Dscp>,
    pub pmtu: Ask<Pmtu>,
    pub broadcast: Ask<bool>,
    pub multicast_ttl: Ask<u8>,
    pub multicast_loop: Ask<bool>,
    pub multicast_join: Ask<MulticastV4>,
}

impl UdpOptions {
    /// Nothing set: what std's `UdpSocket::bind` did, which blaze-core used before.
    pub const DEFAULT: UdpOptions = UdpOptions {
        recv_buffer: Ask::Leave,
        send_buffer: Ask::Leave,
        reuse_address: Ask::Leave,
        reuse_port: Ask::Leave,
        dscp: Ask::Leave,
        pmtu: Ask::Leave,
        broadcast: Ask::Leave,
        multicast_ttl: Ask::Leave,
        multicast_loop: Ask::Leave,
        multicast_join: Ask::Leave,
    };
}

impl Default for UdpOptions {
    fn default() -> UdpOptions {
        UdpOptions::DEFAULT
    }
}

/// How one option was asked, without its value.
#[derive(Clone, Copy, PartialEq, Eq)]
enum How {
    Leave,
    Must,
    MaySkip,
}

fn how<T: Copy>(ask: &Ask<T>) -> How {
    match ask {
        Ask::Leave => How::Leave,
        Ask::Must(_) => How::Must,
        Ask::WhereAvailable(_) => How::MaySkip,
    }
}

/// One option's verdict: set, skipped, or refused.
fn judge(
    skipped: &mut Skipped,
    platform: Platform,
    option: Opt,
    how: How,
    refusal: Option<Why>,
) -> Result<(), Unavailable> {
    let why = match (how, refusal) {
        (How::Leave, _) => return Ok(()),
        (_, Some(why)) => why,
        (_, None) if option.on(platform) => return Ok(()),
        (_, None) => Why::NotOnPlatform,
    };
    match (how, why) {
        // A conflict is the caller's two asks; skipping one would choose for it.
        (How::MaySkip, Why::Conflicts(..)) | (How::Must, _) => Err(Unavailable { option, platform, why }),
        (How::MaySkip, _) => {
            skipped.insert(option);
            Ok(())
        }
        (How::Leave, _) => Ok(()),
    }
}

/// The most a byte count may be: the system takes an `int`.
const INT_MOST: u32 = i32::MAX as u32;

fn bytes_form(ask: &Ask<Bytes>) -> Option<Why> {
    match ask.value() {
        Some(Bytes(n)) if n == 0 || n > INT_MOST => Some(Why::Form("a byte count is 1 to 2^31 - 1")),
        _ => None,
    }
}

/// Which of `options` are set on `side` of `platform`, or the first refused, in [`Opt::ALL`]'s
/// order. Pure: bstd calls it before it asks, every driver before it makes a socket.
pub fn check_tcp(options: &TcpOptions, side: Side, platform: Platform) -> Result<Skipped, Unavailable> {
    let mut skipped = Skipped::NONE;
    let o = options;
    let side_is = |want: Side| match side == want || (side == Side::Accepted && want == Side::Listen) {
        true => None,
        false => Some(Why::NotOnThisSide(side)),
    };
    let keep_alive_form = match o.keep_alive.value() {
        Some(KeepAlive::On { idle, interval, probes }) if idle.0 == 0 || interval.0 == 0 || probes == 0 => {
            Some(Why::Form("a keep-alive's idle, interval and probes are each at least 1"))
        }
        Some(KeepAlive::On { idle, interval, probes }) if idle.0 > INT_MOST || interval.0 > INT_MOST || probes > INT_MOST => {
            Some(Why::Form("a keep-alive's times and probes are each at most 2^31 - 1"))
        }
        _ => None,
    };
    let fast_open_form = match (o.fast_open_listen.value(), platform) {
        (Some(FastOpen::On), Platform::Linux) => Some(Why::Form("Linux takes a queue length: FastOpen::Queue(n)")),
        (Some(FastOpen::Queue(_)), Platform::Macos) => Some(Why::Form("macOS takes no queue length: FastOpen::On")),
        (Some(FastOpen::Queue(0)), _) => Some(Why::Form("a queue of 0 is fast open off: leave the option")),
        (Some(FastOpen::Queue(n)), _) if n > INT_MOST => Some(Why::Form("a queue is at most 2^31 - 1")),
        _ => None,
    };
    let s = &mut skipped;
    judge(s, platform, Opt::NoDelay, how(&o.no_delay), None)?;
    judge(s, platform, Opt::RecvBuffer, how(&o.recv_buffer), bytes_form(&o.recv_buffer))?;
    judge(s, platform, Opt::SendBuffer, how(&o.send_buffer), bytes_form(&o.send_buffer))?;
    judge(s, platform, Opt::KeepAlive, how(&o.keep_alive), keep_alive_form)?;
    judge(s, platform, Opt::Linger, how(&o.linger), None)?;
    judge(s, platform, Opt::ReuseAddress, how(&o.reuse_address), None)?;
    judge(s, platform, Opt::ReusePort, how(&o.reuse_port), None)?;
    judge(s, platform, Opt::NotSentLowWater, how(&o.not_sent_low_water), bytes_form(&o.not_sent_low_water))?;
    judge(s, platform, Opt::QuickAck, how(&o.quick_ack), side_is(Side::Connect))?;
    judge(s, platform, Opt::FastOpenConnect, how(&o.fast_open_connect), side_is(Side::Connect))?;
    judge(s, platform, Opt::FastOpenListen, how(&o.fast_open_listen), side_is(Side::Listen).or(fast_open_form))?;
    judge(s, platform, Opt::Congestion, how(&o.congestion), None)?;
    judge(s, platform, Opt::Dscp, how(&o.dscp), None)?;
    judge(s, platform, Opt::TcpEcn, how(&o.ecn), None)?;
    Ok(skipped)
}

/// As [`check_tcp`], for a datagram socket's options, its tuning and its datagram options
/// together: `dont_fragment` and a [`UdpOptions::pmtu`] both name the path-MTU mode, and a
/// [`UdpOptions::dscp`] with `ecn` would be cleared by each datagram's ECN mark (a send's
/// `IP_TOS` message carries the whole byte): each pair is refused, not resolved for the caller.
pub fn check_udp(options: &crate::dgram::Options, platform: Platform) -> Result<Skipped, Unavailable> {
    let mut skipped = Skipped::NONE;
    let t = &options.tuning;
    let s = &mut skipped;
    let pmtu_form = match (t.pmtu.value(), platform) {
        (Some(_), _) if options.dont_fragment => Some(Why::Conflicts(
            Opt::PathMtu,
            "dgram::Options::dont_fragment sets the path-MTU mode too (IP_PMTUDISC_PROBE on Linux): ask one",
        )),
        (Some(Pmtu::Do | Pmtu::Dont), Platform::Macos) | (Some(_), Platform::Linux | Platform::Simulated | Platform::Other) => None,
        (Some(_), Platform::Macos) => {
            Some(Why::Form("macOS sets only the don't-fragment bit: Pmtu::Do or Pmtu::Dont"))
        }
        (None, _) => None,
    };
    let dscp_conflict = match options.ecn {
        true => Some(Why::Conflicts(
            Opt::Dscp,
            "dgram::Options::ecn marks each datagram with an IP_TOS/IPV6_TCLASS message of the whole byte, which \
             would clear the DSCP: a DSCP carried in each datagram's message is not built",
        )),
        false => None,
    };
    let multicast_join_form = match t.multicast_join.value() {
        Some(m) if !m.group.is_multicast() => Some(Why::Form("multicast group must be in 224.0.0.0/4")),
        _ => None,
    };
    judge(s, platform, Opt::RecvBuffer, how(&t.recv_buffer), bytes_form(&t.recv_buffer))?;
    judge(s, platform, Opt::SendBuffer, how(&t.send_buffer), bytes_form(&t.send_buffer))?;
    judge(s, platform, Opt::ReuseAddress, how(&t.reuse_address), None)?;
    judge(s, platform, Opt::ReusePort, how(&t.reuse_port), None)?;
    judge(s, platform, Opt::Dscp, how(&t.dscp), dscp_conflict)?;
    judge(s, platform, Opt::PathMtu, how(&t.pmtu), pmtu_form)?;
    judge(s, platform, Opt::Broadcast, how(&t.broadcast), None)?;
    judge(s, platform, Opt::MulticastTtl, how(&t.multicast_ttl), None)?;
    judge(s, platform, Opt::MulticastLoop, how(&t.multicast_loop), None)?;
    judge(s, platform, Opt::MulticastJoin, how(&t.multicast_join), multicast_join_form)?;
    Ok(skipped)
}

#[cfg(unix)]
pub(crate) use native::*;

/// The system calls: the table, the setters, the read-back and the sockets made with them.
#[cfg(unix)]
pub(crate) mod native {
    use std::{
        io,
        net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
        os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd},
        path::Path,
    };

    use blaze_sys::{
        Fd,
        posix::{net, types::{c_int, socklen_t}},
    };

    use super::{
        Buffers, Bytes, FastOpen, KeepAlive, Linger, MulticastV4, Opt, Platform, Pmtu, Side, Skipped, Tcp, TcpOptions,
        check_tcp, check_udp,
    };

    /// macOS's `TCP_ENABLE_ECN`: the SDK's `netinet/tcp.h`, `#define TCP_ENABLE_ECN 0x104`;
    /// not in the `libc` crate.
    #[cfg(target_os = "macos")]
    const MACOS_TCP_ENABLE_ECN: c_int = 0x104;

    /// macOS's `TCP_NOTSENT_LOWAT`: the SDK's `netinet/tcp.h`, `#define TCP_NOTSENT_LOWAT
    /// 0x201`; not in the `libc` crate.
    #[cfg(target_os = "macos")]
    const MACOS_TCP_NOTSENT_LOWAT: c_int = 0x201;

    /// One `setsockopt` call's level and name, and the name printed in its errors.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Knob {
        pub level: c_int,
        pub name: c_int,
        pub text: &'static str,
    }

    /// Each system call an option is made of.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Call {
        NoDelay,
        RecvBuffer,
        SendBuffer,
        KeepAliveOn,
        KeepIdle,
        KeepInterval,
        KeepCount,
        Linger,
        ReuseAddress,
        ReusePort,
        NotSentLowWater,
        QuickAck,
        FastOpenConnect,
        FastOpenListen,
        Congestion,
        Tos,
        TcpEcn,
        PathMtu,
        Broadcast,
        MulticastTtl,
        MulticastLoop,
        MulticastJoin,
    }

    /// The table a setter reads: [`knob`], or a test's planted one.
    pub type Table = fn(Call, bool) -> Option<Knob>;

    const fn k(level: c_int, name: c_int, text: &'static str) -> Option<Knob> {
        Some(Knob { level, name, text })
    }

    /// The host's level and name of `call` on a socket of IPv6 (`v6`) or IPv4; `None` where the
    /// host has no such call. The one home of every option's encoding.
    #[cfg(target_os = "linux")]
    pub fn knob(call: Call, v6: bool) -> Option<Knob> {
        use net::{IPPROTO_IP, IPPROTO_IPV6, IPPROTO_TCP, SOL_SOCKET};
        match (call, v6) {
            (Call::NoDelay, _) => k(IPPROTO_TCP, net::TCP_NODELAY, "TCP_NODELAY"),
            (Call::RecvBuffer, _) => k(SOL_SOCKET, net::SO_RCVBUF, "SO_RCVBUF"),
            (Call::SendBuffer, _) => k(SOL_SOCKET, net::SO_SNDBUF, "SO_SNDBUF"),
            (Call::KeepAliveOn, _) => k(SOL_SOCKET, net::SO_KEEPALIVE, "SO_KEEPALIVE"),
            (Call::KeepIdle, _) => k(IPPROTO_TCP, net::TCP_KEEPIDLE, "TCP_KEEPIDLE"),
            (Call::KeepInterval, _) => k(IPPROTO_TCP, net::TCP_KEEPINTVL, "TCP_KEEPINTVL"),
            (Call::KeepCount, _) => k(IPPROTO_TCP, net::TCP_KEEPCNT, "TCP_KEEPCNT"),
            (Call::Linger, _) => k(SOL_SOCKET, net::SO_LINGER, "SO_LINGER"),
            (Call::ReuseAddress, _) => k(SOL_SOCKET, net::SO_REUSEADDR, "SO_REUSEADDR"),
            (Call::ReusePort, _) => k(SOL_SOCKET, net::SO_REUSEPORT, "SO_REUSEPORT"),
            (Call::NotSentLowWater, _) => k(IPPROTO_TCP, net::TCP_NOTSENT_LOWAT, "TCP_NOTSENT_LOWAT"),
            (Call::QuickAck, _) => k(IPPROTO_TCP, net::TCP_QUICKACK, "TCP_QUICKACK"),
            (Call::FastOpenConnect, _) => k(IPPROTO_TCP, net::TCP_FASTOPEN_CONNECT, "TCP_FASTOPEN_CONNECT"),
            (Call::FastOpenListen, _) => k(IPPROTO_TCP, net::TCP_FASTOPEN, "TCP_FASTOPEN"),
            (Call::Congestion, _) => k(IPPROTO_TCP, net::TCP_CONGESTION, "TCP_CONGESTION"),
            (Call::Tos, false) => k(IPPROTO_IP, net::IP_TOS, "IP_TOS"),
            (Call::Tos, true) => k(IPPROTO_IPV6, net::IPV6_TCLASS, "IPV6_TCLASS"),
            (Call::TcpEcn, _) => None,
            (Call::PathMtu, false) => k(IPPROTO_IP, net::IP_MTU_DISCOVER, "IP_MTU_DISCOVER"),
            (Call::PathMtu, true) => k(IPPROTO_IPV6, net::IPV6_MTU_DISCOVER, "IPV6_MTU_DISCOVER"),
            (Call::Broadcast, _) => k(SOL_SOCKET, net::SO_BROADCAST, "SO_BROADCAST"),
            (Call::MulticastTtl, false) => k(IPPROTO_IP, net::IP_MULTICAST_TTL, "IP_MULTICAST_TTL"),
            (Call::MulticastTtl, true) => None,
            (Call::MulticastLoop, false) => k(IPPROTO_IP, net::IP_MULTICAST_LOOP, "IP_MULTICAST_LOOP"),
            (Call::MulticastLoop, true) => None,
            (Call::MulticastJoin, false) => k(IPPROTO_IP, net::IP_ADD_MEMBERSHIP, "IP_ADD_MEMBERSHIP"),
            (Call::MulticastJoin, true) => None,
        }
    }

    /// As Linux's [`knob`], for macOS.
    #[cfg(target_os = "macos")]
    pub fn knob(call: Call, v6: bool) -> Option<Knob> {
        use net::{IPPROTO_IP, IPPROTO_IPV6, IPPROTO_TCP, SOL_SOCKET};
        match (call, v6) {
            (Call::NoDelay, _) => k(IPPROTO_TCP, net::TCP_NODELAY, "TCP_NODELAY"),
            (Call::RecvBuffer, _) => k(SOL_SOCKET, net::SO_RCVBUF, "SO_RCVBUF"),
            (Call::SendBuffer, _) => k(SOL_SOCKET, net::SO_SNDBUF, "SO_SNDBUF"),
            (Call::KeepAliveOn, _) => k(SOL_SOCKET, net::SO_KEEPALIVE, "SO_KEEPALIVE"),
            (Call::KeepIdle, _) => k(IPPROTO_TCP, net::TCP_KEEPALIVE, "TCP_KEEPALIVE"),
            (Call::KeepInterval, _) => k(IPPROTO_TCP, net::TCP_KEEPINTVL, "TCP_KEEPINTVL"),
            (Call::KeepCount, _) => k(IPPROTO_TCP, net::TCP_KEEPCNT, "TCP_KEEPCNT"),
            // SO_LINGER's time is in ticks on macOS; SO_LINGER_SEC's in seconds.
            (Call::Linger, _) => k(SOL_SOCKET, net::SO_LINGER_SEC, "SO_LINGER_SEC"),
            (Call::ReuseAddress, _) => k(SOL_SOCKET, net::SO_REUSEADDR, "SO_REUSEADDR"),
            (Call::ReusePort, _) => k(SOL_SOCKET, net::SO_REUSEPORT, "SO_REUSEPORT"),
            (Call::NotSentLowWater, _) => k(IPPROTO_TCP, MACOS_TCP_NOTSENT_LOWAT, "TCP_NOTSENT_LOWAT"),
            (Call::QuickAck | Call::FastOpenConnect | Call::Congestion, _) => None,
            (Call::FastOpenListen, _) => k(IPPROTO_TCP, net::TCP_FASTOPEN, "TCP_FASTOPEN"),
            (Call::Tos, false) => k(IPPROTO_IP, net::IP_TOS, "IP_TOS"),
            (Call::Tos, true) => k(IPPROTO_IPV6, net::IPV6_TCLASS, "IPV6_TCLASS"),
            (Call::TcpEcn, _) => k(IPPROTO_TCP, MACOS_TCP_ENABLE_ECN, "TCP_ENABLE_ECN"),
            (Call::PathMtu, false) => k(IPPROTO_IP, net::IP_DONTFRAG, "IP_DONTFRAG"),
            (Call::PathMtu, true) => k(IPPROTO_IPV6, net::IPV6_DONTFRAG, "IPV6_DONTFRAG"),
            (Call::Broadcast, _) => k(SOL_SOCKET, net::SO_BROADCAST, "SO_BROADCAST"),
            (Call::MulticastTtl, false) => k(IPPROTO_IP, net::IP_MULTICAST_TTL, "IP_MULTICAST_TTL"),
            (Call::MulticastTtl, true) => None,
            (Call::MulticastLoop, false) => k(IPPROTO_IP, net::IP_MULTICAST_LOOP, "IP_MULTICAST_LOOP"),
            (Call::MulticastLoop, true) => None,
            (Call::MulticastJoin, false) => k(IPPROTO_IP, net::IP_ADD_MEMBERSHIP, "IP_ADD_MEMBERSHIP"),
            (Call::MulticastJoin, true) => None,
        }
    }

    /// Any other Unix: no option ([`Platform::Other`]).
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn knob(_: Call, _: bool) -> Option<Knob> {
        None
    }

    fn knob_in(table: Table, call: Call, v6: bool) -> io::Result<Knob> {
        table(call, v6).ok_or_else(|| {
            // alloc: an error path.
            io::Error::new(io::ErrorKind::Unsupported, format!("{call:?}: this system has no such socket option"))
        })
    }

    /// `bytes` written as `knob` of `fd`.
    fn set_bytes(fd: RawFd, knob: Knob, value: &[u8]) -> io::Result<()> {
        let len = (value.len() as socklen_t as usize).min(value.len());
        net::setsockopt_bytes(Fd::new(fd), knob.level, knob.name, &value[..len]).map_err(|errno| {
            let error = io::Error::from_raw_os_error(i32::from(errno.raw()));
            // alloc: an error path.
            io::Error::new(error.kind(), format!("setting socket option {}: {error}", knob.text))
        })
    }

    pub(crate) fn set_int(fd: RawFd, knob: Knob, value: c_int) -> io::Result<()> {
        set_bytes(fd, knob, crate::pod::bytes_of(&value))
    }

    fn set_linger(fd: RawFd, knob: Knob, value: net::linger) -> io::Result<()> {
        const {
            assert!(size_of::<net::linger>() == size_of::<[c_int; 2]>());
            assert!(align_of::<net::linger>() == align_of::<[c_int; 2]>());
        }
        let pair = [value.l_onoff, value.l_linger];
        set_bytes(fd, knob, crate::pod::bytes_of(&pair))
    }

    fn set_mreq(fd: RawFd, knob: Knob, value: MulticastV4) -> io::Result<()> {
        const {
            assert!(size_of::<net::ip_mreq>() == size_of::<[u32; 2]>());
            assert!(align_of::<net::ip_mreq>() == align_of::<[u32; 2]>());
        }
        let pair = [
            u32::from_ne_bytes(value.group.octets()),
            u32::from_ne_bytes(value.interface.octets()),
        ];
        set_bytes(fd, knob, crate::pod::bytes_of(&pair))
    }

    /// `IP_MULTICAST_TTL` / `IP_MULTICAST_LOOP`: macOS takes `u_char` (`u8`), Linux takes `int`.
    fn set_multicast_u8(fd: RawFd, knob: Knob, value: u8) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            set_bytes(fd, knob, &[value])
        }
        #[cfg(not(target_os = "macos"))]
        {
            set_int(fd, knob, c_int::from(value))
        }
    }

    #[cfg(test)]
    pub(crate) fn get_multicast_u8(fd: RawFd, knob: Knob) -> io::Result<u8> {
        #[cfg(target_os = "macos")]
        {
            let mut value = [0u8; 1];
            get_bytes(fd, knob, &mut value)?;
            Ok(value[0])
        }
        #[cfg(not(target_os = "macos"))]
        {
            let value = get_int(fd, knob)?;
            Ok(u8::try_from(value).unwrap_or(0))
        }
    }

    /// `knob` of `fd` read back into `into`: how many bytes the kernel wrote.
    fn get_bytes(fd: RawFd, knob: Knob, into: &mut [u8]) -> io::Result<usize> {
        let len = (into.len() as socklen_t as usize).min(into.len());
        net::getsockopt_bytes(Fd::new(fd), knob.level, knob.name, &mut into[..len]).map_err(|errno| {
            let error = io::Error::from_raw_os_error(i32::from(errno.raw()));
            // alloc: an error path.
            io::Error::new(error.kind(), format!("reading socket option {}: {error}", knob.text))
        })
    }

    pub(crate) fn get_int(fd: RawFd, knob: Knob) -> io::Result<c_int> {
        let mut value: c_int = 0;
        get_bytes(fd, knob, crate::pod::bytes_of_mut(&mut value))?;
        Ok(value)
    }

    #[cfg(test)]
    pub(crate) fn get_linger(fd: RawFd, knob: Knob) -> io::Result<net::linger> {
        const {
            assert!(size_of::<net::linger>() == size_of::<[c_int; 2]>());
            assert!(align_of::<net::linger>() == align_of::<[c_int; 2]>());
        }
        let mut pair = [0 as c_int; 2];
        get_bytes(fd, knob, crate::pod::bytes_of_mut(&mut pair))?;
        Ok(net::linger { l_onoff: pair[0], l_linger: pair[1] })
    }

    /// A name option's bytes, up to its first NUL.
    #[cfg(test)]
    pub(crate) fn get_name(fd: RawFd, knob: Knob, into: &mut [u8; 16]) -> io::Result<usize> {
        let got = get_bytes(fd, knob, into)?;
        Ok(into[..got].iter().position(|it| *it == 0).unwrap_or(got))
    }

    fn int(on: bool) -> c_int {
        c_int::from(on)
    }

    /// The `int` a count of at most 2^31 - 1 is ([`super::check_tcp`] refused larger).
    fn count(n: u32) -> c_int {
        c_int::try_from(n).expect("a count was checked to fit an int")
    }

    fn refused(refusal: super::Unavailable) -> io::Error {
        // alloc: an error path.
        io::Error::new(io::ErrorKind::Unsupported, refusal.to_string())
    }

    /// `options` set on the TCP socket `fd` (`v6`: of IPv6) for `side`, through `table`; the
    /// options skipped where unavailable. A [`Side::Accepted`] sets only what acts per
    /// connection: the listen's own (reuse, fast open) were set on the listen.
    pub(crate) fn apply_tcp_in(
        table: Table,
        fd: RawFd,
        v6: bool,
        options: &TcpOptions,
        side: Side,
    ) -> io::Result<Skipped> {
        let skipped = check_tcp(options, side, Platform::HOST).map_err(refused)?;
        let o = options;
        let wanted = |opt: Opt| !skipped.contains(opt);
        let listen_only = side == Side::Accepted;
        let set = |call: Call, value: c_int| knob_in(table, call, v6).and_then(|knob| set_int(fd, knob, value));
        if let (Some(on), true) = (o.no_delay.value(), wanted(Opt::NoDelay)) {
            set(Call::NoDelay, int(on))?;
        }
        if let (Some(Bytes(n)), true) = (o.recv_buffer.value(), wanted(Opt::RecvBuffer)) {
            set(Call::RecvBuffer, count(n))?;
        }
        if let (Some(Bytes(n)), true) = (o.send_buffer.value(), wanted(Opt::SendBuffer)) {
            set(Call::SendBuffer, count(n))?;
        }
        match (o.keep_alive.value(), wanted(Opt::KeepAlive)) {
            (Some(KeepAlive::Off), true) => set(Call::KeepAliveOn, 0)?,
            (Some(KeepAlive::On { idle, interval, probes }), true) => {
                set(Call::KeepAliveOn, 1)?;
                set(Call::KeepIdle, count(idle.0))?;
                set(Call::KeepInterval, count(interval.0))?;
                set(Call::KeepCount, count(probes))?;
            }
            _ => {}
        }
        if let (Some(linger), true) = (o.linger.value(), wanted(Opt::Linger)) {
            let value = match linger {
                Linger::Off => net::linger { l_onoff: 0, l_linger: 0 },
                Linger::Reset => net::linger { l_onoff: 1, l_linger: 0 },
                Linger::Seconds(n) => net::linger { l_onoff: 1, l_linger: c_int::from(n) },
            };
            set_linger(fd, knob_in(table, Call::Linger, v6)?, value)?;
        }
        if let (Some(on), true, false) = (o.reuse_address.value(), wanted(Opt::ReuseAddress), listen_only) {
            set(Call::ReuseAddress, int(on))?;
        }
        if let (Some(on), true, false) = (o.reuse_port.value(), wanted(Opt::ReusePort), listen_only) {
            set(Call::ReusePort, int(on))?;
        }
        if let (Some(Bytes(n)), true) = (o.not_sent_low_water.value(), wanted(Opt::NotSentLowWater)) {
            set(Call::NotSentLowWater, count(n))?;
        }
        if let (Some(on), true, Side::Connect) = (o.quick_ack.value(), wanted(Opt::QuickAck), side) {
            set(Call::QuickAck, int(on))?;
        }
        if let (Some(on), true, Side::Connect) = (o.fast_open_connect.value(), wanted(Opt::FastOpenConnect), side) {
            set(Call::FastOpenConnect, int(on))?;
        }
        // Fast open on a listen is not set here: XNU takes `TCP_FASTOPEN` only on a socket
        // already listening (EINVAL before), so `listen_tcp` sets it after listen(2)
        // (`fast_open_in`).
        if let (Some(name), true) = (o.congestion.value(), wanted(Opt::Congestion)) {
            set_bytes(fd, knob_in(table, Call::Congestion, v6)?, name.as_bytes())?;
        }
        if let (Some(dscp), true) = (o.dscp.value(), wanted(Opt::Dscp)) {
            set(Call::Tos, c_int::from(dscp.tos()))?;
        }
        if let (Some(on), true) = (o.ecn.value(), wanted(Opt::TcpEcn)) {
            set(Call::TcpEcn, int(on))?;
        }
        Ok(skipped)
    }

    /// [`apply_tcp_in`] through the host's [`knob`] table.
    pub(crate) fn apply_tcp(fd: RawFd, v6: bool, options: &TcpOptions, side: Side) -> io::Result<Skipped> {
        apply_tcp_in(knob, fd, v6, options, side)
    }

    /// A listen's fast open (`TCP_FASTOPEN`) set on `fd`, which is listening already, through
    /// `table`, unless `skipped` holds it. XNU's `tcp_ctloutput` refuses it with EINVAL on a
    /// socket not in `TCPS_LISTEN`; Linux takes it either side of listen(2): so it is set after.
    pub(crate) fn fast_open_in(table: Table, fd: RawFd, v6: bool, options: &TcpOptions, skipped: Skipped) -> io::Result<()> {
        if let (Some(fast), false) = (options.fast_open_listen.value(), skipped.contains(Opt::FastOpenListen)) {
            let value = match fast {
                FastOpen::On => 1,
                FastOpen::Queue(n) => count(n),
            };
            set_int(fd, knob_in(table, Call::FastOpenListen, v6)?, value)?;
        }
        Ok(())
    }

    /// A Unix socket takes no TCP option: only the default value, which asks nothing of it.
    pub(crate) fn unix_refused(options: &Tcp) -> io::Result<()> {
        match options.is_default() {
            true => Ok(()),
            false => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "TCP socket options asked of a Unix socket: a Unix address takes TcpOptions::DEFAULT alone",
            )),
        }
    }

    /// `options` set on the stream socket `fd` of `domain` before it connects: a Unix socket's
    /// as [`unix_refused`] says.
    pub(crate) fn for_stream(fd: RawFd, domain: c_int, options: &Tcp, side: Side) -> io::Result<()> {
        match domain {
            net::AF_UNIX => unix_refused(options),
            _ => apply_tcp(fd, domain == net::AF_INET6, options.get(), side).map(drop),
        }
    }

    /// The options that act per connection set again on a connection a listen accepted:
    /// macOS's and the threads driver's accept call; Linux's ring accepts straight into its
    /// table, and its connections keep the listen's (the test
    /// `an_accepted_connection_carries_the_listen_s_options` reads them back).
    pub(crate) fn accepted(stream: &TcpStream, options: &Tcp) -> io::Result<()> {
        let v6 = stream.local_addr()?.is_ipv6();
        apply_tcp(stream.as_raw_fd(), v6, options.get(), Side::Accepted).map(drop)
    }

    fn sockaddr(at: SocketAddr) -> io::Result<crate::net::SockAddr> {
        // The root is read only for a Unix address.
        crate::net::sockaddr(Path::new(""), &crate::Address::Tcp(at))
    }

    fn os(what: &'static str, error: io::Error) -> io::Error {
        // alloc: an error path.
        io::Error::new(error.kind(), format!("{what}: {error}"))
    }

    /// A TCP socket listening at `at` with `options` set before its bind; nonblocking is the
    /// driver's to set, as for std's.
    pub(crate) fn listen_tcp(at: SocketAddr, options: &TcpOptions) -> io::Result<TcpListener> {
        let address = sockaddr(at)?;
        let fd = crate::net::stream_socket(address.domain)?;
        let skipped = apply_tcp(fd.as_raw_fd(), at.is_ipv6(), options, Side::Listen)?;
        #[cfg(target_os = "macos")]
        net::bind(Fd::new(fd.as_raw_fd()), &address.storage, address.len)
            .map_err(|errno| os("bind", io::Error::from_raw_os_error(i32::from(errno.raw()))))?;
        #[cfg(not(target_os = "macos"))]
        // SAFETY: an address of `len` bytes, of the socket's domain (`crate::net::sockaddr`).
        if unsafe { libc::bind(fd.as_raw_fd(), (&raw const address.storage).cast(), address.len) } != 0 {
            return Err(os("bind", io::Error::last_os_error()));
        }
        #[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
        let backlog = net::SOMAXCONN;
        #[cfg(not(any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
        let backlog = libc::SOMAXCONN;
        net::listen(Fd::new(fd.as_raw_fd()), backlog)
            .map_err(|errno| os("listen", io::Error::from_raw_os_error(i32::from(errno.raw()))))?;
        fast_open_in(knob, fd.as_raw_fd(), at.is_ipv6(), options, skipped)?;
        Ok(TcpListener::from(fd))
    }

    /// A TCP connection to `at` with `options` set before it connects, blocking until it is:
    /// the threads driver's, which connects on a thread of its own as std's
    /// `TcpStream::connect` did. An interrupted connect fails (rule no-fallback: no retry).
    pub(crate) fn connect_tcp(at: SocketAddr, options: &Tcp) -> io::Result<TcpStream> {
        let address = sockaddr(at)?;
        let fd = crate::net::stream_socket(address.domain)?;
        apply_tcp(fd.as_raw_fd(), at.is_ipv6(), options.get(), Side::Connect)?;
        #[cfg(target_os = "macos")]
        net::connect(Fd::new(fd.as_raw_fd()), &address.storage, address.len)
            .map_err(|errno| os("connect", io::Error::from_raw_os_error(i32::from(errno.raw()))))?;
        #[cfg(not(target_os = "macos"))]
        // SAFETY: an address of `len` bytes, of the socket's domain.
        if unsafe { libc::connect(fd.as_raw_fd(), (&raw const address.storage).cast(), address.len) } != 0 {
            return Err(os("connect", io::Error::last_os_error()));
        }
        Ok(TcpStream::from(fd))
    }

    /// A UDP socket bound at `at`, close-on-exec, with `options.tuning` set before its bind
    /// (reuse must be); the options its datagrams are read by are `net::udp::bind`'s to set
    /// after. Refused, naming the option, where [`check_udp`] refuses; the options skipped are
    /// [`check_udp`]'s to say, which the asker calls itself (pure).
    pub(crate) fn bind_udp(
        at: SocketAddr,
        options: &crate::dgram::Options,
    ) -> Result<UdpSocket, (&'static str, io::Error)> {
        let skipped = check_udp(options, Platform::HOST).map_err(|refusal| ("its options", refused(refusal)))?;
        let address = sockaddr(at).map_err(|error| ("its address", error))?;
        let raw = net::socket(address.domain, net::SOCK_DGRAM, 0)
            .map_err(|errno| ("socket", io::Error::from_raw_os_error(i32::from(errno.raw()))))?;
        // SAFETY: the call returned a descriptor of its own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw.raw()) };
        crate::net::set_cloexec(fd.as_raw_fd()).map_err(|error| ("FD_CLOEXEC", error))?;
        let (raw, v6, t) = (fd.as_raw_fd(), at.is_ipv6(), &options.tuning);
        let wanted = |opt: Opt| !skipped.contains(opt);
        let set = |call: Call, value: c_int| knob_in(knob, call, v6).and_then(|knob| set_int(raw, knob, value));
        let named = |error: io::Error| ("an option", error);
        if let (Some(on), true) = (t.reuse_address.value(), wanted(Opt::ReuseAddress)) {
            set(Call::ReuseAddress, int(on)).map_err(named)?;
        }
        if let (Some(on), true) = (t.reuse_port.value(), wanted(Opt::ReusePort)) {
            set(Call::ReusePort, int(on)).map_err(named)?;
        }
        if let (Some(Bytes(n)), true) = (t.recv_buffer.value(), wanted(Opt::RecvBuffer)) {
            set(Call::RecvBuffer, count(n)).map_err(named)?;
        }
        if let (Some(Bytes(n)), true) = (t.send_buffer.value(), wanted(Opt::SendBuffer)) {
            set(Call::SendBuffer, count(n)).map_err(named)?;
        }
        if let (Some(dscp), true) = (t.dscp.value(), wanted(Opt::Dscp)) {
            set(Call::Tos, c_int::from(dscp.tos())).map_err(named)?;
        }
        if let (Some(mode), true) = (t.pmtu.value(), wanted(Opt::PathMtu)) {
            set(Call::PathMtu, pmtu(mode)).map_err(named)?;
        }
        if let (Some(on), true) = (t.broadcast.value(), wanted(Opt::Broadcast)) {
            set(Call::Broadcast, int(on)).map_err(named)?;
        }
        if let (Some(ttl), true) = (t.multicast_ttl.value(), wanted(Opt::MulticastTtl)) {
            match knob(Call::MulticastTtl, v6) {
                Some(k) => set_multicast_u8(raw, k, ttl).map_err(named)?,
                None if t.multicast_ttl.may_skip() => {}
                None => return Err(named(io::Error::new(io::ErrorKind::Unsupported, "IP_MULTICAST_TTL: IPv4-only socket option"))),
            }
        }
        if let (Some(on), true) = (t.multicast_loop.value(), wanted(Opt::MulticastLoop)) {
            match knob(Call::MulticastLoop, v6) {
                Some(k) => set_multicast_u8(raw, k, u8::from(on)).map_err(named)?,
                None if t.multicast_loop.may_skip() => {}
                None => return Err(named(io::Error::new(io::ErrorKind::Unsupported, "IP_MULTICAST_LOOP: IPv4-only socket option"))),
            }
        }
        #[cfg(target_os = "macos")]
        net::bind(Fd::new(raw), &address.storage, address.len)
            .map_err(|errno| ("the bind", io::Error::from_raw_os_error(i32::from(errno.raw()))))?;
        #[cfg(not(target_os = "macos"))]
        // SAFETY: an address of `len` bytes, of the socket's domain.
        if unsafe { libc::bind(raw, (&raw const address.storage).cast(), address.len) } != 0 {
            return Err(("the bind", io::Error::last_os_error()));
        }
        if let (Some(mreq), true) = (t.multicast_join.value(), wanted(Opt::MulticastJoin)) {
            match knob(Call::MulticastJoin, v6) {
                Some(k) => set_mreq(raw, k, mreq).map_err(named)?,
                None if t.multicast_join.may_skip() => {}
                None => return Err(named(io::Error::new(io::ErrorKind::Unsupported, "IP_ADD_MEMBERSHIP: IPv4-only socket option"))),
            }
        }
        Ok(UdpSocket::from(fd))
    }

    /// The value `IP_MTU_DISCOVER` (Linux) or `IP_DONTFRAG` (macOS) takes for `mode`.
    #[cfg(target_os = "linux")]
    fn pmtu(mode: Pmtu) -> c_int {
        match mode {
            Pmtu::Dont => net::IP_PMTUDISC_DONT,
            Pmtu::Want => net::IP_PMTUDISC_WANT,
            Pmtu::Do => net::IP_PMTUDISC_DO,
            Pmtu::Probe => net::IP_PMTUDISC_PROBE,
            Pmtu::Interface => net::IP_PMTUDISC_INTERFACE,
            Pmtu::Omit => net::IP_PMTUDISC_OMIT,
        }
    }

    /// macOS's don't-fragment bit: [`super::check_udp`] admits only `Do` and `Dont` here.
    #[cfg(not(target_os = "linux"))]
    fn pmtu(mode: Pmtu) -> c_int {
        c_int::from(mode != Pmtu::Dont)
    }

    /// `fd`'s buffers as the kernel says them now.
    pub(crate) fn buffers(fd: RawFd, v6: bool) -> io::Result<Buffers> {
        let recv = get_int(fd, knob_in(knob, Call::RecvBuffer, v6)?)?;
        let send = get_int(fd, knob_in(knob, Call::SendBuffer, v6)?)?;
        let bytes = |n: c_int| Bytes(u32::try_from(n).expect("the kernel says a buffer's size as a positive int"));
        Ok(Buffers { recv: bytes(recv), send: bytes(send) })
    }
}

#[cfg(all(test, unix))]
mod tests;
