//! The parse rules (moved from bstd's `tests/names.rs` with the code, 2026-11-16), and one
//! case per hostile URL form a client must refuse by name.
//!
//! - [`HostPort::parse`] of `host:port` is [`HostPort::new`] of the host and the port: the
//!   same name, or the same refusal; and a name prints as what parses back to it.

use std::{format, string::String, vec::Vec};

use blaze_sim::{Config, Tape};

use crate::{HostPort, HostPortWhy, Url, UrlWhy};

/// A string of at most `most` bytes, each one of `alphabet`.
fn word(tape: &Tape, alphabet: &[u8], most: u64) -> String {
    let bytes: Vec<u8> = (0..tape.int_in(0..=most, "word len")).map(|_| *tape.one_of(alphabet, "byte")).collect();
    String::from_utf8(bytes).expect("the alphabets are ASCII")
}

#[test]
fn parse_of_host_and_port_is_new_of_them() {
    blaze_sim::check(Config::cases(500), |tape| {
        let host = word(tape, b"a0-.:", 10);
        let port = tape.int_in(0..=u64::from(u16::MAX), "port") as u16;
        let joined = format!("{host}:{port}");
        let new = HostPort::new(&host, port).map_err(|error| error.why);
        let parsed = HostPort::parse(&joined).map_err(|error| error.why);
        assert_eq!(new, parsed, "HostPort::new({host:?}, {port}) and HostPort::parse({joined:?})");
        if let Ok(name) = new {
            assert_eq!(HostPort::parse(&std::string::ToString::to_string(&name)).as_ref(), Ok(&name), "{name} prints as it parses");
        }
    });
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

/// Each hostile form, refused with the reason that names it.
#[test]
fn hostile_urls_are_refused_by_name() {
    let why = |raw: &str| Url::parse(raw).err().map(|error| error.why);
    // A user before the host: the secret, or the host a reader thinks it is (`a`), is not.
    assert_eq!(why("http://a@b/"), Some(UrlWhy::UserInfo));
    assert_eq!(why("https://user:pass@static.crates.io/x"), Some(UrlWhy::UserInfo));
    // A backslash, which WHATWG parsers read as `/`: `https://a.com\@b.com/` is b.com to them.
    assert_eq!(why("https://a.com\\@b.com/"), Some(UrlWhy::Forbidden));
    assert_eq!(why("https://static.crates.io/a\\b"), Some(UrlWhy::Forbidden));
    // An embedded NUL, CR, LF, tab or space: a request line split, or a name cut short.
    for raw in ["https://a.com/\0x", "https://a.com/x\r\nHost: b", "https://a.com/x\ny", "https://a.\tcom/", "https://a com/"] {
        assert_eq!(why(raw), Some(UrlWhy::Forbidden), "{raw:?}");
    }
    // A percent-escape in the host (`%2F` is `/`), a non-ASCII name (no IDNA), an empty label.
    for raw in ["https://a%2Fb.com/", "https://a%2fb.com/", "https://bücher.example/", "https://a..b/"] {
        assert_eq!(why(raw), Some(UrlWhy::BadAuthority), "{raw:?}");
    }
    // A port out of range, zero, or not a number.
    for raw in ["https://a.com:65536/", "https://a.com:0/", "https://a.com:99999999999/", "https://a.com:4x3/", "https://a.com:-1/"] {
        assert_eq!(why(raw), Some(UrlWhy::BadAuthority), "{raw:?}");
    }
    // IPv6 in brackets only: taken, with or without a port; bare, or with a zone, refused.
    let v6 = Url::parse("https://[2001:db8::1]:8443/x").expect("a bracketed IPv6 address");
    assert_eq!(v6.origin().map(|it| (it.at().host(), it.at().port())), Some(("[2001:db8::1]", 8443)));
    let v6 = Url::parse("https://[::1]/").expect("a bracketed IPv6 address, the scheme's port");
    assert_eq!(v6.origin().map(|it| it.at().port()), Some(443));
    assert_eq!(why("https://[fe80::1%25en0]/"), Some(UrlWhy::BadAuthority));
    assert_eq!(why("https://[::1/"), Some(UrlWhy::BadAuthority));
    // And what a fetch asks is taken as written.
    let ok = Url::parse("https://static.crates.io/crates/serde/serde-1.0.0.crate").expect("a crate's URL");
    assert_eq!((ok.scheme().as_str(), ok.target()), ("https", "/crates/serde/serde-1.0.0.crate"));
    assert_eq!(ok.origin().map(|it| (it.at().host(), it.at().port())), Some(("static.crates.io", 443)));
}
