//! Socket options on real sockets on loopback: each option the host has is set and read back
//! with `getsockopt` through the true table (the oracle, [`verify_tcp`]); an option the host
//! lacks is refused naming it and the platform; a table that writes an option at the wrong
//! level (the planted bug) is caught by the read-back; a listen's options reach the
//! connections it accepts; and a batch of datagrams crosses loopback with its marks from a
//! socket bound with tuning.
//!
//! What each man page promises, asserted: a buffer's size reads back as Linux socket(7) says
//! ("The kernel doubles this value ... and this doubled value is returned by getsockopt") and as
//! macOS getsockopt(2) says (the size set, under "an absolute limit"); a boolean option reads
//! back non-zero (macOS answers its flag's bit, not 1); every other value as it was set.

use std::{
    io,
    net::{SocketAddr, TcpListener, TcpStream},
    os::fd::{AsRawFd as _, RawFd},
    time::{Duration, Instant},
};

use blaze_sys::posix::net;

use super::*;
use crate::dgram::{Batch, Ecn, Head, Slots};

/// What every buffer asks: well under Linux's default `rmem_max`/`wmem_max` (212,992) and
/// macOS's `kern.ipc.maxsockbuf` (8 MiB), so no cap applies.
const BUFFER: u32 = 64 << 10;

/// The size a buffer of `asked` reads back as, by the host's man page.
fn buffer_reads(asked: u32) -> u32 {
    match Platform::HOST {
        Platform::Linux => asked * 2,
        _ => asked,
    }
}

/// Every TCP option the host has, asked [`Ask::Must`] for `side`, the rest left.
fn every_tcp(side: Side) -> TcpOptions {
    // A fn, not a closure: it is asked for options of several types.
    fn must<T>(opt: Opt, value: T) -> Ask<T> {
        match opt.on(Platform::HOST) {
            true => Ask::Must(value),
            false => Ask::Leave,
        }
    }
    let fast_open_listen = match (side, Platform::HOST) {
        (Side::Listen, Platform::Linux) => Ask::Must(FastOpen::Queue(16)),
        (Side::Listen, Platform::Macos) => Ask::Must(FastOpen::On),
        _ => Ask::Leave,
    };
    let connect = side == Side::Connect;
    TcpOptions {
        no_delay: Ask::Must(true),
        recv_buffer: Ask::Must(Bytes(BUFFER)),
        send_buffer: Ask::Must(Bytes(BUFFER)),
        keep_alive: Ask::Must(KeepAlive::On { idle: Seconds(30), interval: Seconds(5), probes: 4 }),
        linger: Ask::Must(Linger::Seconds(3)),
        reuse_address: Ask::Must(true),
        reuse_port: Ask::Must(true),
        not_sent_low_water: Ask::Must(Bytes(4096)),
        quick_ack: match connect {
            true => must(Opt::QuickAck, true),
            false => Ask::Leave,
        },
        fast_open_connect: match connect {
            true => must(Opt::FastOpenConnect, true),
            false => Ask::Leave,
        },
        fast_open_listen,
        // `reno` is built into every Linux and allowed to every user.
        congestion: must(Opt::Congestion, Congestion::new("reno").expect("a name")),
        dscp: Ask::Must(Dscp::new(10).expect("AF11")),
        ecn: must(Opt::TcpEcn, true),
    }
}

fn knob_of(call: Call, v6: bool) -> Knob {
    knob(call, v6).expect("the option is on the host")
}

/// The oracle: every option `options` sets on `side` reads back from `fd` through the true
/// table as it was asked; the first that does not, named with what it read.
fn verify_tcp(fd: RawFd, v6: bool, options: &TcpOptions, side: Side) -> Result<(), String> {
    let read = |call: Call| get_int(fd, knob_of(call, v6)).map_err(|error| format!("{call:?}: {error}"));
    let wrong = |what: &str, want: &str, got: i64| -> Result<(), String> {
        Err(format!("{what} reads back {got}, asked {want}"))
    };
    if let Some(on) = options.no_delay.value() {
        let got = read(Call::NoDelay)?;
        if (got != 0) != on {
            return wrong("TCP_NODELAY", &on.to_string(), got.into());
        }
    }
    for (call, ask) in [(Call::RecvBuffer, options.recv_buffer), (Call::SendBuffer, options.send_buffer)] {
        if let Some(Bytes(n)) = ask.value() {
            let got = read(call)?;
            if got as u32 != buffer_reads(n) {
                return wrong(&format!("{call:?}"), &n.to_string(), got.into());
            }
        }
    }
    if let Some(KeepAlive::On { idle, interval, probes }) = options.keep_alive.value() {
        if read(Call::KeepAliveOn)? == 0 {
            return wrong("SO_KEEPALIVE", "on", 0);
        }
        for (call, want) in [(Call::KeepIdle, idle.0), (Call::KeepInterval, interval.0), (Call::KeepCount, probes)] {
            let got = read(call)?;
            if got as u32 != want {
                return wrong(&format!("{call:?}"), &want.to_string(), got.into());
            }
        }
    }
    if let Some(Linger::Seconds(n)) = options.linger.value() {
        let got = get_linger(fd, knob_of(Call::Linger, v6)).map_err(|error| error.to_string())?;
        if got.l_onoff == 0 || got.l_linger != i32::from(n) {
            return Err(format!("linger reads back on {} for {}, asked {n} s", got.l_onoff, got.l_linger));
        }
    }
    let listen_side = side != Side::Accepted;
    for (call, ask, own) in [
        (Call::ReuseAddress, options.reuse_address, listen_side),
        (Call::ReusePort, options.reuse_port, listen_side),
        (Call::QuickAck, options.quick_ack, side == Side::Connect),
        (Call::FastOpenConnect, options.fast_open_connect, side == Side::Connect),
        (Call::TcpEcn, options.ecn, true),
    ] {
        if let (Some(on), true) = (ask.value(), own) {
            let got = read(call)?;
            if (got != 0) != on {
                return wrong(&format!("{call:?}"), &on.to_string(), got.into());
            }
        }
    }
    if let Some(Bytes(n)) = options.not_sent_low_water.value() {
        let got = read(Call::NotSentLowWater)?;
        if got as u32 != n {
            return wrong("TCP_NOTSENT_LOWAT", &n.to_string(), got.into());
        }
    }
    if let (Some(fast), Side::Listen) = (options.fast_open_listen.value(), side) {
        let got = read(Call::FastOpenListen)?;
        let fine = match fast {
            FastOpen::On => got != 0,
            FastOpen::Queue(n) => got as u32 == n,
        };
        if !fine {
            return wrong("TCP_FASTOPEN", &format!("{fast:?}"), got.into());
        }
    }
    if let Some(name) = options.congestion.value() {
        let mut into = [0u8; 16];
        let len = get_name(fd, knob_of(Call::Congestion, v6), &mut into).map_err(|error| error.to_string())?;
        if &into[..len] != name.as_bytes() {
            return Err(format!("TCP_CONGESTION reads back {:?}, asked {}", String::from_utf8_lossy(&into[..len]), name.as_str()));
        }
    }
    if let Some(dscp) = options.dscp.value() {
        let got = read(Call::Tos)?;
        // The low two bits are ECN's, the kernel's for TCP.
        if (got as u8) & !0b11 != dscp.tos() {
            return wrong("IP_TOS", &dscp.tos().to_string(), got.into());
        }
    }
    Ok(())
}

fn tcp_socket() -> std::os::fd::OwnedFd {
    crate::net::stream_socket(net::AF_INET).expect("a TCP socket")
}

/// Every option the host has, set on a connect's socket and on a listen's, reads back as asked
/// through the true table.
///
/// Prints, on macOS: `sockopt: every TCP option macOS has reads back as set (SO_RCVBUF 65536
/// -> 65536)`; on Linux the same with `Linux` and `-> 131072`.
#[test]
fn every_option_the_host_has_reads_back_as_set() {
    // A connect's options on a fresh socket; a listen's through `listen_tcp`, which sets fast
    // open once the socket listens (XNU refuses it before).
    let options = every_tcp(Side::Connect);
    let fd = tcp_socket();
    let skipped = apply_tcp(fd.as_raw_fd(), false, &options, Side::Connect).expect("every option the host has is set");
    assert!(skipped.is_empty(), "nothing asked Must is skipped: {skipped:?}");
    if let Err(why) = verify_tcp(fd.as_raw_fd(), false, &options, Side::Connect) {
        panic!("on a {}: {why}", Side::Connect);
    }
    let options = every_tcp(Side::Listen);
    let listener = listen_tcp("127.0.0.1:0".parse().expect("an address"), &options).expect("a listen with every option");
    if let Err(why) = verify_tcp(listener.as_raw_fd(), false, &options, Side::Listen) {
        panic!("on a {}: {why}", Side::Listen);
    }
    let fd = tcp_socket();
    apply_tcp(fd.as_raw_fd(), false, &every_tcp(Side::Connect), Side::Connect).expect("set");
    let got = buffers(fd.as_raw_fd(), false).expect("read back");
    assert_eq!(got.recv, Bytes(buffer_reads(BUFFER)));
    println!(
        "sockopt: every TCP option {} has reads back as set (SO_RCVBUF {BUFFER} -> {})",
        Platform::HOST,
        got.recv.0
    );
}

/// An option the host lacks, asked `Must`, is refused naming the option and the platform before
/// any call; asked `WhereAvailable` it is skipped and said skipped.
///
/// Prints, on macOS: `sockopt: refused: socket option TCP_CONGESTION is not on macOS: refused
/// (rule no-fallback); ...`; on Linux the same of `TCP_ENABLE_ECN` on `Linux`.
#[test]
fn an_option_the_host_lacks_is_refused_by_name() {
    let (opt, mut options) = (
        match Platform::HOST {
            Platform::Linux => Opt::TcpEcn,
            _ => Opt::Congestion,
        },
        TcpOptions::DEFAULT,
    );
    let reno = Congestion::new("reno").expect("a name");
    match opt {
        Opt::TcpEcn => options.ecn = Ask::Must(true),
        _ => options.congestion = Ask::Must(reno),
    }
    let fd = tcp_socket();
    let error = apply_tcp(fd.as_raw_fd(), false, &options, Side::Connect).expect_err("refused");
    let said = error.to_string();
    assert!(said.contains(opt.name()), "{said}");
    assert!(said.contains(&Platform::HOST.to_string()), "{said}");
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    match opt {
        Opt::TcpEcn => options.ecn = Ask::WhereAvailable(true),
        _ => options.congestion = Ask::WhereAvailable(reno),
    }
    let skipped = apply_tcp(fd.as_raw_fd(), false, &options, Side::Connect).expect("skipped, not refused");
    assert!(skipped.contains(opt), "{skipped:?}");
    // The pure check says the same for each platform, whatever the host.
    let mut ecn = TcpOptions::DEFAULT;
    ecn.ecn = Ask::Must(true);
    assert_eq!(check_tcp(&ecn, Side::Connect, Platform::Linux).unwrap_err().option, Opt::TcpEcn);
    assert!(check_tcp(&ecn, Side::Connect, Platform::Macos).is_ok());
    assert!(check_tcp(&ecn, Side::Connect, Platform::Simulated).is_ok());
    let mut fast = TcpOptions::DEFAULT;
    fast.fast_open_listen = Ask::Must(FastOpen::Queue(8));
    let wrong_side = check_tcp(&fast, Side::Connect, Platform::Linux).unwrap_err();
    assert_eq!(wrong_side.why, Why::NotOnThisSide(Side::Connect));
    assert!(matches!(check_tcp(&fast, Side::Listen, Platform::Macos).unwrap_err().why, Why::Form(_)));
    println!("sockopt: refused: {said}");
}

/// The table every option is written through, with `TCP_NODELAY` at the socket level: the
/// planted bug.
fn planted(call: Call, v6: bool) -> Option<Knob> {
    match call {
        Call::NoDelay => Some(Knob { level: net::SOL_SOCKET, name: net::TCP_NODELAY, text: "TCP_NODELAY (planted)" }),
        call => knob(call, v6),
    }
}

/// An option written at the wrong level is caught: the call fails (Linux: that name at the
/// socket level is `SO_DEBUG`, which needs a capability), or the read-back through the true
/// table does not show it (macOS sets `SO_DEBUG` and `TCP_NODELAY` stays off). The true table
/// passes the same oracle (test-the-cause on the test itself).
///
/// Prints: `sockopt: the planted wrong level is caught: <how>`.
#[test]
fn a_planted_wrong_level_is_caught_by_the_read_back() {
    let options = TcpOptions { no_delay: Ask::Must(true), reuse_address: Ask::Leave, ..TcpOptions::DEFAULT };
    let fd = tcp_socket();
    apply_tcp_in(knob, fd.as_raw_fd(), false, &options, Side::Connect).expect("the true table sets it");
    verify_tcp(fd.as_raw_fd(), false, &options, Side::Connect).expect("the true table reads back");
    let fd = tcp_socket();
    let how = match apply_tcp_in(planted, fd.as_raw_fd(), false, &options, Side::Connect) {
        Err(error) => format!("the call failed: {error}"),
        Ok(_) => match verify_tcp(fd.as_raw_fd(), false, &options, Side::Connect) {
            Err(why) => format!("the read-back: {why}"),
            Ok(()) => panic!("TCP_NODELAY written at SOL_SOCKET read back as set: the oracle missed the plant"),
        },
    };
    println!("sockopt: the planted wrong level is caught: {how}");
}

/// A listen's per-connection options reach a connection it accepts: on Linux by the kernel's
/// copy of the listen (the ring accepts with no descriptor to set them on), read back before
/// anything is set on it; on every host after [`accepted`] sets them again.
///
/// Prints: `sockopt: an accepted connection carries the listen's options (<platform>)`.
#[test]
fn an_accepted_connection_carries_the_listen_s_options() {
    let mut options = TcpOptions::DEFAULT;
    options.keep_alive = Ask::Must(KeepAlive::On { idle: Seconds(40), interval: Seconds(7), probes: 3 });
    options.dscp = Ask::Must(Dscp::new(46).expect("EF"));
    options.not_sent_low_water = Ask::Must(Bytes(8192));
    let at: SocketAddr = "127.0.0.1:0".parse().expect("an address");
    let listener: TcpListener = listen_tcp(at, &options).expect("a listen with options");
    let to = listener.local_addr().expect("its address");
    let _client = TcpStream::connect_timeout(&to, Duration::from_secs(5)).expect("a connect on loopback");
    let (stream, _) = listener.accept().expect("the connection");
    if Platform::HOST == Platform::Linux {
        verify_tcp(stream.as_raw_fd(), false, &options, Side::Accepted).expect("Linux copies the listen's options");
    }
    accepted(&stream, &Tcp::new(options)).expect("set again");
    verify_tcp(stream.as_raw_fd(), false, &options, Side::Accepted).expect("the accepted connection's options");
    println!("sockopt: an accepted connection carries the listen's options ({})", Platform::HOST);
}

/// A UDP socket bound with tuning: its buffers read back as asked, and a batch crosses loopback
/// with its marks and the address each arrived at. A run of three datagrams (10 bytes cut at 4)
/// arrives as its three datagrams: on Linux sent as one `UDP_SEGMENT` message, on macOS as
/// three.
///
/// Prints: `sockopt: 4 datagrams crossed loopback with their marks (SO_RCVBUF 262144 -> N)`.
#[test]
fn a_batch_of_datagrams_crosses_loopback_with_its_marks() {
    let asked = 1u32 << 18;
    let tuning = UdpOptions {
        recv_buffer: Ask::Must(Bytes(asked)),
        reuse_address: Ask::Must(true),
        ..UdpOptions::DEFAULT
    };
    let options = crate::dgram::Options { ecn: true, local_address: true, tuning, ..crate::dgram::Options::default() };
    let at: SocketAddr = "127.0.0.1:0".parse().expect("an address");
    let sender = crate::net::udp::bind(at, options).expect("a socket with tuning");
    let receiver = crate::net::udp::bind(at, options).expect("a socket with tuning");
    let got = buffers(receiver.as_raw_fd(), false).expect("read back");
    // Each host may cap it (Linux at twice rmem_max, macOS at maxsockbuf): never more than
    // the host's man page's reading of what was asked.
    assert!(got.recv.0 <= buffer_reads(asked), "{got:?}");
    let reuse = get_int(receiver.as_raw_fd(), knob_of(Call::ReuseAddress, false)).expect("read back");
    assert_ne!(reuse, 0, "SO_REUSEADDR set before the bind");
    let to = receiver.local_addr().expect("its address");
    let mut batch = Batch::new();
    batch.push(to, Ecn::Ect0, b"first");
    batch.bytes.extend_from_slice(b"aaaabbbbcc");
    batch.heads.push(Head { peer: to, local: None, ecn: Ecn::Ect1, len: 10, segment: 4, truncated: false });
    let mut sent = crate::net::udp::At::default();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !sent.done(&batch) {
        assert!(Instant::now() < deadline, "the send did not end in 5 s");
        sent = crate::net::udp::send(&sender, false, &batch, sent).expect("a send on loopback");
    }
    let want: [(&[u8], Ecn); 4] = [(b"first", Ecn::Ect0), (b"aaaa", Ecn::Ect1), (b"bbbb", Ecn::Ect1), (b"cc", Ecn::Ect1)];
    let (mut into, mut seen) = (Batch::new(), 0);
    while seen < want.len() {
        assert!(Instant::now() < deadline, "{seen} of {} datagrams arrived in 5 s", want.len());
        match crate::net::udp::recv(&receiver, &mut into, Slots { most: 8, size: 2048 }) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::yield_now();
                continue;
            }
            Err(error) => panic!("receiving on {to}: {error}"),
        }
        for (head, bytes) in into.each() {
            let (bytes_sent, ecn) = want[seen];
            assert_eq!(bytes, bytes_sent, "datagram {seen}");
            assert_eq!(head.ecn, ecn, "datagram {seen}'s mark");
            assert_eq!(head.local, Some(to.ip()), "the address datagram {seen} arrived at");
            seen += 1;
        }
    }
    println!("sockopt: 4 datagrams crossed loopback with their marks (SO_RCVBUF {asked} -> {})", got.recv.0);
}

/// The pairs a datagram socket's options refuse, and a platform's form, by the pure check.
///
/// Prints: `sockopt: datagram conflicts refused by name`.
#[test]
fn datagram_options_that_contradict_are_refused() {
    let mut options = crate::dgram::Options::default();
    options.dont_fragment = true;
    options.tuning.pmtu = Ask::WhereAvailable(Pmtu::Probe);
    let refused = check_udp(&options, Platform::Linux).unwrap_err();
    assert!(matches!(refused.why, Why::Conflicts(Opt::PathMtu, _)), "a conflict is refused even where-available");
    options.dont_fragment = false;
    assert!(matches!(check_udp(&options, Platform::Macos), Ok(skipped) if skipped.contains(Opt::PathMtu)));
    options.tuning.pmtu = Ask::Must(Pmtu::Probe);
    assert!(matches!(check_udp(&options, Platform::Macos).unwrap_err().why, Why::Form(_)));
    options.tuning.pmtu = Ask::Leave;
    options.ecn = true;
    options.tuning.dscp = Ask::Must(Dscp::new(46).expect("EF"));
    let refused = check_udp(&options, Platform::Simulated).unwrap_err();
    assert_eq!(refused.option, Opt::Dscp);
    options.ecn = false;
    options.tuning.dscp = Ask::Leave;
    options.tuning.multicast_join = Ask::Must(MulticastV4 {
        group: core::net::Ipv4Addr::new(10, 0, 0, 1),
        interface: core::net::Ipv4Addr::UNSPECIFIED,
    });
    let refused = check_udp(&options, Platform::Simulated).unwrap_err();
    assert_eq!(refused.option, Opt::MulticastJoin);
    assert!(matches!(refused.why, Why::Form(_)));
    println!("sockopt: datagram conflicts refused by name");
}

#[test]
fn udp_broadcast_and_multicast_options_apply_and_read_back() {
    let at: SocketAddr = "0.0.0.0:0".parse().expect("bind address");
    let mut options = crate::dgram::Options::default();
    options.tuning.reuse_address = Ask::Must(true);
    options.tuning.reuse_port = Ask::Must(true);
    options.tuning.broadcast = Ask::Must(true);
    options.tuning.multicast_ttl = Ask::Must(1);
    options.tuning.multicast_loop = Ask::Must(true);
    options.tuning.multicast_join = Ask::WhereAvailable(MulticastV4 {
        group: core::net::Ipv4Addr::new(239, 255, 77, 77),
        interface: core::net::Ipv4Addr::UNSPECIFIED,
    });
    let sock = crate::net::udp::bind(at, options).expect("bind udp with broadcast+multicast");
    let fd = sock.as_raw_fd();
    assert_ne!(get_int(fd, knob_of(Call::Broadcast, false)).expect("SO_BROADCAST"), 0);
    assert_eq!(get_multicast_u8(fd, knob_of(Call::MulticastTtl, false)).expect("IP_MULTICAST_TTL"), 1);
    assert_ne!(get_multicast_u8(fd, knob_of(Call::MulticastLoop, false)).expect("IP_MULTICAST_LOOP"), 0);
}
