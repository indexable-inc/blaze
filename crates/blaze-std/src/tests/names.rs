//! Typed names: the `const fn` checks that `path!`, `var!` and `host!` run as the crate
//! compiles are the checks `new` and `parse` run at run time.
//!
//! - [`Path::new`]'s byte-wise check refuses exactly what the rule by text (std's `split`,
//!   as it was written before the check became a `const fn`) refuses, for the same reason.
//! - [`HostPort::parse`] of `host:port` is [`HostPort::new`] of the host and the port: the
//!   same name, or the same refusal; and a name prints as what parses back to it.

use hegel::TestCase;

use super::{Draw, Drawn};
use crate::{
    env::{VarName, VarNameWhy},
    net::{HostPort, HostPortWhy},
    path::{Path, PathWhy},
};

/// A string of at most `most` bytes, each one of `alphabet`.
fn word(d: &mut impl Draw, alphabet: &[u8], most: u64) -> String {
    let bytes: Vec<u8> = (0..d.int(0, most)).map(|_| d.pick(alphabet)).collect();
    String::from_utf8(bytes).expect("the alphabets are ASCII")
}

/// The path rule by text, as `path.rs` wrote it before its check was a `const fn`.
fn path_by_text(raw: &str) -> Option<PathWhy> {
    if raw.is_empty() {
        return Some(PathWhy::Empty);
    }
    if raw.starts_with('/') || raw.as_bytes().get(1) == Some(&b':') {
        return Some(PathWhy::Rooted);
    }
    if raw.contains(['\\', '\0']) {
        return Some(PathWhy::Forbidden);
    }
    for part in raw.split('/') {
        match part {
            ".." => return Some(PathWhy::Parent),
            "" | "." => return Some(PathWhy::NotCanonical),
            _ => {}
        }
    }
    None
}

#[hegel::test(test_cases = 500)]
fn a_path_is_refused_as_the_rule_by_text_refuses_it(tc: TestCase) {
    let raw = word(&mut Drawn(&tc), b"a./\\:\0", 8);
    let got = Path::new(&raw).err().map(|error| error.why);
    assert_eq!(got, path_by_text(&raw), "Path::new({raw:?})");
}

#[hegel::test(test_cases = 500)]
fn parse_of_host_and_port_is_new_of_them(tc: TestCase) {
    let mut d = Drawn(&tc);
    let host = word(&mut d, b"a0-.:", 10);
    let port = d.int(0, u64::from(u16::MAX)) as u16;
    let joined = format!("{host}:{port}");
    let new = HostPort::new(&host, port).map_err(|error| error.why);
    let parsed = HostPort::parse(&joined).map_err(|error| error.why);
    assert_eq!(new, parsed, "HostPort::new({host:?}, {port}) and HostPort::parse({joined:?})");
    if let Ok(name) = new {
        assert_eq!(HostPort::parse(&name.to_string()).as_ref(), Ok(&name), "{name} prints as it parses");
    }
}

#[test]
fn host_ports_by_example() {
    let why = |raw: &str| HostPort::parse(raw).err().map(|error| error.why);
    assert_eq!(why("api.stripe.com:443"), None);
    assert_eq!(why("a..b:443"), Some(HostPortWhy::BadLabel));
    assert_eq!(why("-a.b:443"), Some(HostPortWhy::BadLabel));
    assert_eq!(why("a_b:443"), Some(HostPortWhy::BadByte));
    assert_eq!(why("a.b"), Some(HostPortWhy::NoPort));
    assert_eq!(why("a.b:0"), Some(HostPortWhy::BadPort));
    assert_eq!(why("a.b:65536"), Some(HostPortWhy::BadPort));
    assert_eq!(why("[::1]"), Some(HostPortWhy::NoPort));
    assert_eq!(why("[::1:443"), Some(HostPortWhy::NoPort));
    assert_eq!(why("[::g]:443"), Some(HostPortWhy::BadBrackets));
    let v6 = HostPort::parse("[::1]:443").expect("an IPv6 host and port");
    assert_eq!((v6.host(), v6.port()), ("[::1]", 443));
    assert_eq!(v6.socket_addr(), Some("[::1]:443".parse().expect("a socket address")));
    let v4 = HostPort::parse("127.0.0.1:8080").expect("an IPv4 host and port");
    assert_eq!(v4.socket_addr(), Some("127.0.0.1:8080".parse().expect("a socket address")));
    assert_eq!(HostPort::parse("localhost:80").expect("a host name").socket_addr(), None);
}

#[test]
fn literals_are_checked_as_the_crate_compiles() {
    assert_eq!(crate::path!("data/log").expect("a test path"), Path::new("data/log").expect("a path"));
    assert_eq!(crate::var!("HOME"), VarName::new("HOME").expect("a variable name"));
    assert_eq!(crate::host!("api.stripe.com:443").expect("a test host"), HostPort::new("api.stripe.com", 443).expect("a host"));
    let why = |raw: &str| VarName::new(raw).err().map(|error| error.why);
    assert_eq!((why(""), why("A=B"), why("A\0")), (Some(VarNameWhy::Empty), Some(VarNameWhy::Equals), Some(VarNameWhy::Nul)));
}
