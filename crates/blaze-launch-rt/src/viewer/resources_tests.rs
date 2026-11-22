//! The answer's bytes against the viewer's old parser of lsof's output for the same facts, the
//! v6 text against `inet_ntop`'s, and the Linux tables read by inode.

use core::fmt::Write as _;

use blaze_own::{AString, AVec, Alloc};

use super::*;

/// The viewer's reading of `lsof -nP -a -p <pid> -F ftPnT` before it asked the kernel itself
/// (viewer.rs's `read_process_resources_json` to 2026-11-21), kept as the oracle: field tags
/// per line, a record flushed at each `f`, a numeric `f` counted, an `IPv4`/`IPv6` record a
/// socket split at `->`, its port the local end's last `:` segment, an empty state `LISTEN`
/// unconnected and `ESTABLISHED` connected, an empty protocol `TCP`; `REG`/`VREG` a file.
fn old_lsof_json<'a>(pid: u32, text: &str, alloc: Alloc<'a>) -> AString<'a> {
    type Row<'t> = (&'t str, &'t str, &'t str, &'t str, u16);
    fn record<'t>(
        ty: &str,
        proto: &'t str,
        name: &'t str,
        state: &'t str,
        sockets: &mut AVec<'_, Row<'t>>,
        files: &mut AVec<'_, &'t str>,
    ) {
        if ty == "IPv4" || ty == "IPv6" {
            let (local, remote) = name.split_once("->").unwrap_or((name, ""));
            let port = local.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()).unwrap_or(0);
            let st = if state.is_empty() {
                if remote.is_empty() { "LISTEN" } else { "ESTABLISHED" }
            } else {
                state
            };
            if sockets.len() < 48 {
                sockets.push((if proto.is_empty() { "TCP" } else { proto }, local, remote, st, port));
            }
        } else if (ty == "REG" || ty == "VREG") && !name.is_empty() && files.len() < 24 {
            files.push(name);
        }
    }
    let mut fd_count: u32 = 0;
    let mut sockets: AVec<'_, Row<'_>> = AVec::new_in(alloc);
    let mut files: AVec<'_, &str> = AVec::new_in(alloc);
    let (mut ty, mut proto, mut name, mut state) = ("", "", "", "");
    for line in text.lines() {
        let mut chars = line.chars();
        let Some(tag) = chars.next() else { continue };
        let rest = chars.as_str();
        match tag {
            'f' => {
                if !ty.is_empty() {
                    record(ty, proto, name, state, &mut sockets, &mut files);
                }
                (ty, proto, name, state) = ("", "", "", "");
                if rest.bytes().all(|b| b.is_ascii_digit()) {
                    fd_count = fd_count.saturating_add(1);
                }
            }
            't' => ty = rest,
            'P' => proto = rest,
            'n' => name = rest,
            'T' => {
                if let Some(st) = rest.strip_prefix("ST=") {
                    state = st;
                }
            }
            _ => {}
        }
    }
    if !ty.is_empty() {
        record(ty, proto, name, state, &mut sockets, &mut files);
    }
    let mut out = AString::new_in(alloc);
    let _ = write!(out, r#"{{"pid":{pid},"fd_count":{fd_count},"sockets":["#);
    for (i, (proto, local, remote, state, port)) in sockets.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(r#"{"proto":""#);
        push_json_text(&mut out, proto);
        out.push_str(r#"","local":""#);
        push_json_text(&mut out, local);
        out.push_str(r#"","remote":""#);
        push_json_text(&mut out, remote);
        out.push_str(r#"","state":""#);
        push_json_text(&mut out, state);
        let _ = write!(out, r#"","port":{port}}}"#);
    }
    out.push_str(r#"],"files":["#);
    for (i, f) in files.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        push_json_text(&mut out, f);
        out.push('"');
    }
    out.push_str("]}");
    out
}

const LSOF: &str = "p4242
f0
tCHR
n/dev/ttys001
f3
tIPv4
PTCP
n127.0.0.1:41937
TST=LISTEN
TQR=0
TQS=0
f4
tIPv6
PTCP
n[::1]:5000->[::1]:61000
TST=ESTABLISHED
f5
tIPv4
PUDP
n*:5353
f6
tREG
n/tmp/held log \"q\".txt
f7
tIPv4
PTCP
n*:*
TST=CLOSED
f8
tIPv4
PUDP
n127.0.0.1:53000->127.0.0.1:53
f9
tIPv6
PTCP
n[2001:db8::5]:22->[2001:db8::1:0:0:1]:50000
TST=TIME_WAIT
";

fn v4(a: u8, b: u8, c: u8, d: u8, port: u16) -> End {
    End { ip: Ip::V4([a, b, c, d]), port }
}

fn v6(words: [u16; 8], port: u16) -> End {
    let mut bytes = [0u8; 16];
    for (into, word) in bytes.chunks_exact_mut(2).zip(words) {
        into.copy_from_slice(&word.to_be_bytes());
    }
    End { ip: Ip::V6(bytes), port }
}

const UNSET: End = End { ip: Ip::V4([0; 4]), port: 0 };

/// The facts `LSOF` says, as the drivers gather them: descriptors 0 (a terminal) and 3..=9.
fn facts(alloc: Alloc<'_>) -> Resources<'_> {
    let mut facts = Resources::new_in(4242, alloc);
    facts.fd_count = 8;
    let tcp = |local, remote, state| Socket { proto: Proto::Tcp, local, remote, state: Some(state) };
    let udp = |local, remote| Socket { proto: Proto::Udp, local, remote, state: None };
    facts.push_socket(tcp(v4(127, 0, 0, 1, 41937), UNSET, TcpState::Listen));
    facts.push_socket(tcp(v6([0, 0, 0, 0, 0, 0, 0, 1], 5000), v6([0, 0, 0, 0, 0, 0, 0, 1], 61000), TcpState::Established));
    facts.push_socket(udp(v4(0, 0, 0, 0, 5353), UNSET));
    facts.push_file(b"/tmp/held log \"q\".txt");
    facts.push_socket(tcp(UNSET, UNSET, TcpState::Closed));
    facts.push_socket(udp(v4(127, 0, 0, 1, 53000), v4(127, 0, 0, 1, 53)));
    facts.push_socket(tcp(
        v6([0x2001, 0xdb8, 0, 0, 0, 0, 0, 5], 22),
        v6([0x2001, 0xdb8, 0, 0, 1, 0, 0, 1], 50000),
        TcpState::TimeWait,
    ));
    facts
}

#[test]
fn the_answer_is_the_old_parsers_for_the_same_facts() {
    let alloc = blaze_own::testing::heap().alloc();
    let old = old_lsof_json(4242, LSOF, alloc);
    let mut new = AString::new_in(alloc);
    facts(alloc).write_json(&mut new);
    assert_eq!(new.as_str(), old.as_str());
    assert!(old.contains(r#"{"proto":"UDP","local":"*:5353","remote":"","state":"LISTEN","port":5353}"#), "{old}");
    assert!(old.contains(r#""local":"*:*","remote":"","state":"CLOSED","port":0"#), "{old}");
}

#[test]
fn no_process_is_the_old_empty_answer() {
    let alloc = blaze_own::testing::heap().alloc();
    let mut out = AString::new_in(alloc);
    Resources::new_in(0, alloc).write_json(&mut out);
    assert_eq!(out.as_str(), r#"{"pid":0,"fd_count":0,"sockets":[],"files":[]}"#);
    assert_eq!(query_pid(""), Ok(0));
    assert_eq!(query_pid("pid=42"), Ok(42));
    assert_eq!(query_pid("x=1&pid=7"), Ok(7));
    assert_eq!(query_pid("pid=4x").map_err(|error| error.refused), Err(Refused::BadQuery));
}

#[test]
fn the_caps_are_the_old_ones() {
    let alloc = blaze_own::testing::heap().alloc();
    let mut facts = Resources::new_in(1, alloc);
    for port in 1..=60 {
        facts.push_socket(Socket { proto: Proto::Tcp, local: v4(127, 0, 0, 1, port), remote: UNSET, state: Some(TcpState::Listen) });
        facts.push_file(b"/f");
    }
    assert_eq!((facts.sockets.len(), facts.files.len()), (MOST_SOCKETS, MOST_FILES));
}

#[test]
fn v6_addresses_are_written_as_inet_ntop_writes_them() {
    let alloc = blaze_own::testing::heap().alloc();
    let cases: [([u16; 8], &str); 8] = [
        ([0, 0, 0, 0, 0, 0, 0, 1], "[::1]:1"),
        ([0x2001, 0xdb8, 0, 0, 1, 0, 0, 1], "[2001:db8::1:0:0:1]:1"),
        ([0x2001, 0xdb8, 0, 1, 0, 0, 0, 1], "[2001:db8:0:1::1]:1"),
        ([0xfe80, 0, 0, 0, 0, 0, 0, 0], "[fe80::]:1"),
        ([0, 0, 0, 0, 0, 0xffff, 0x7f00, 1], "[::ffff:127.0.0.1]:1"),
        ([0, 0, 0, 0, 0, 0, 0x0a00, 2], "[::10.0.0.2]:1"),
        ([1, 0, 2, 3, 4, 5, 6, 7], "[1:0:2:3:4:5:6:7]:1"),
        ([0xabcd, 0, 0, 0xef, 0, 0, 0, 0], "[abcd:0:0:ef::]:1"),
    ];
    for (words, text) in cases {
        let mut out = AString::new_in(alloc);
        write_end(&mut out, v6(words, 1));
        assert_eq!(out.as_str(), text, "{words:x?}");
    }
    let mut out = AString::new_in(alloc);
    write_end(&mut out, v6([0; 8], 443));
    assert_eq!(out.as_str(), "*:443");
}

const TCP: &[u8] = b"  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:A3D1 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 111 1 0000000000000000 100 0 0 10 0
   1: 0100007F:A3D1 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 222 1 0000000000000000 20 4 30 10 -1
   2: 0100007F:0016 0100007F:D432 01 00000000:00000000 00:00000000 00000000     0        0 555 1 0000000000000000 20 4 30 10 -1
";
const TCP6: &[u8] = b"  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:1388 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 444 1 0000000000000000 100 0 0 10 0
";
const UDP: &[u8] = b"   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
  5: 00000000:14E9 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 333 2 0000000000000000 0
";

/// The tables' addresses are native-endian words: these are a little-endian machine's (every
/// target blaze runs on).
#[cfg(target_endian = "little")]
#[test]
fn the_linux_tables_are_read_by_inode_in_descriptor_order() {
    let alloc = blaze_own::testing::heap().alloc();
    let links: [&[u8]; 7] =
        [b"socket:[111]", b"/tmp/x.log", b"pipe:[5]", b"socket:[444]", b"socket:[333]", b"socket:[999]", b"socket:[222]"];
    let mut wanted: AVec<'_, u64> = AVec::new_in(alloc);
    for link in links {
        if let FdLink::Socket(inode) = fd_link(link) {
            wanted.push(inode);
        }
    }
    wanted.sort_unstable();
    let mut sockets = AVec::new_in(alloc);
    proc_net_sockets(TCP, Proto::Tcp, false, &wanted, &mut sockets).unwrap();
    proc_net_sockets(TCP6, Proto::Tcp, true, &wanted, &mut sockets).unwrap();
    proc_net_sockets(UDP, Proto::Udp, false, &wanted, &mut sockets).unwrap();
    assert_eq!(sockets.len(), 4, "inode 555 is not the process's, 999 is no IP socket");
    sockets.sort_unstable_by_key(|&(inode, _)| inode);
    let mut facts = Resources::new_in(7, alloc);
    for link in links {
        facts.add_linux_fd(link, &sockets);
    }
    let mut out = AString::new_in(alloc);
    facts.write_json(&mut out);
    assert_eq!(
        out.as_str(),
        concat!(
            r#"{"pid":7,"fd_count":7,"sockets":["#,
            r#"{"proto":"TCP","local":"127.0.0.1:41937","remote":"","state":"LISTEN","port":41937},"#,
            r#"{"proto":"TCP","local":"[::1]:5000","remote":"","state":"LISTEN","port":5000},"#,
            r#"{"proto":"UDP","local":"*:5353","remote":"","state":"LISTEN","port":5353},"#,
            r#"{"proto":"TCP","local":"127.0.0.1:41937","remote":"127.0.0.1:54321","state":"ESTABLISHED","port":41937}"#,
            r#"],"files":["/tmp/x.log"]}"#,
        )
    );
}

#[test]
fn a_malformed_table_line_is_refused_by_line_and_column() {
    let alloc = blaze_own::testing::heap().alloc();
    let mut sockets = AVec::new_in(alloc);
    let table = b"header\n   0: 0100007F:A3D1 00000000:0000 0A 0:0 0:0 0 1000 0 111\n   1: 0100007G:A3D1 00000000:0000 0A 0:0 0:0 0 1000 0 222\n";
    assert_eq!(
        proc_net_sockets(table, Proto::Tcp, false, &[111, 222], &mut sockets),
        Err(ProcNetError { line: 3, column: "local_address" })
    );
    let table = b"header\n   0: 0100007F:A3D1 00000000:0000 0A 0:0 0:0 0 1000 0\n";
    assert_eq!(
        proc_net_sockets(table, Proto::Tcp, false, &[111], &mut sockets),
        Err(ProcNetError { line: 2, column: "inode" })
    );
}

#[test]
fn a_refusal_names_the_call_the_pid_and_the_errno() {
    let alloc = blaze_own::testing::heap().alloc();
    let error = ResourcesError {
        call: "proc_pidinfo(PROC_PIDLISTFDS)",
        pid: 99,
        errno: 3,
        name: "ESRCH",
        refused: Refused::Gone,
    };
    let mut out = AString::new_in(alloc);
    error.write_json(&mut out);
    assert_eq!(out.as_str(), r#"{"error":{"call":"proc_pidinfo(PROC_PIDLISTFDS)","pid":99,"errno":3,"name":"ESRCH"}}"#);
    assert_eq!(error.status(), "404 Not Found");
}

#[test]
fn a_path_that_is_not_utf8_is_written_lossily() {
    let alloc = blaze_own::testing::heap().alloc();
    let mut out = AString::new_in(alloc);
    push_json_bytes(&mut out, b"/a\xffb\n");
    assert_eq!(out.as_str(), "/a\u{FFFD}b\\n");
}
