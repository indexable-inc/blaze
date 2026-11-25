//! The glyph protocol reply (ghostty's apc/glyph/response.zig): `ESC _ 25a1; ... ESC \`.

use crate::kitty::Wire;

/// Which sources render a code point (`Coverage`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct Coverage {
    pub system: bool,
    pub glossary: bool,
}

impl Coverage {
    /// A comma list of names; unknown names are skipped so later names stay compatible.
    pub fn parse(value: &[u8]) -> Coverage {
        let mut c = Coverage::default();
        for name in value.split(|&b| b == b',') {
            match name {
                b"system" => c.system = true,
                b"glossary" => c.glossary = true,
                _ => {}
            }
        }
        c
    }
}

/// The payload formats a support reply advertises.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct Formats {
    pub glyf: bool,
    pub colrv0: bool,
    pub colrv1: bool,
}

/// A reply's status: 0 is ok, 1 an error; other values pass through.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Status(pub u8);

impl Status {
    pub const OK: Status = Status(0);
    pub const ERR: Status = Status(1);
}

/// Why a register or clear failed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Reason {
    OutOfNamespace,
    CompositeUnsupported,
    HintingUnsupported,
    MalformedPayload,
    PayloadTooLarge,
    /// A name outside the spec's list (ghostty sends `out_of_memory` this way).
    Other(ReasonName),
}

/// A reason name outside the spec's list: 1 to 32 bytes of `a-z`, `0-9` and `_`, so a
/// reply always fits [`Wire::CAP`] and never carries a separator.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ReasonName(&'static str);

impl ReasonName {
    pub const MOST: usize = 32;
    pub const OUT_OF_MEMORY: ReasonName = ReasonName("out_of_memory");

    pub fn new(name: &'static str) -> Option<ReasonName> {
        let ok = (1..=ReasonName::MOST).contains(&name.len())
            && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        ok.then_some(ReasonName(name))
    }

    pub fn as_str(self) -> &'static str {
        self.0
    }
}

impl Reason {
    pub fn name(self) -> &'static str {
        match self {
            Reason::OutOfNamespace => "out_of_namespace",
            Reason::CompositeUnsupported => "composite_unsupported",
            Reason::HintingUnsupported => "hinting_unsupported",
            Reason::MalformedPayload => "malformed_payload",
            Reason::PayloadTooLarge => "payload_too_large",
            Reason::Other(n) => n.as_str(),
        }
    }
}

/// A glyph protocol reply.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Response {
    Support { fmt: Formats },
    Query { cp: u32, status: Coverage },
    Register { cp: u32, status: Status, reason: Option<Reason> },
    Clear { status: Status, reason: Option<Reason> },
}

impl Response {
    /// The reply bytes (`formatWire`).
    pub fn encode(&self) -> Wire {
        let mut w = Wire::new();
        w.put(b"\x1b_25a1;");
        match *self {
            Response::Support { fmt } => {
                w.put(b"s;fmt=");
                let names =
                    [(fmt.glyf, &b"glyf"[..]), (fmt.colrv0, b"colrv0"), (fmt.colrv1, b"colrv1")];
                put_set(&mut w, &names);
            }
            Response::Query { cp, status } => {
                w.put(b"q;cp=");
                put_hex(&mut w, cp);
                w.put(b";status=");
                put_set(&mut w, &[(status.system, &b"system"[..]), (status.glossary, b"glossary")]);
            }
            Response::Register { cp, status, reason } => {
                w.put(b"r;cp=");
                put_hex(&mut w, cp);
                w.put(b";status=");
                w.put_decimal(u64::from(status.0));
                put_reason(&mut w, reason);
            }
            Response::Clear { status, reason } => {
                w.put(b"c;status=");
                w.put_decimal(u64::from(status.0));
                put_reason(&mut w, reason);
            }
        }
        w.put(b"\x1b\\");
        w
    }
}

fn put_set(w: &mut Wire, names: &[(bool, &[u8])]) {
    let mut first = true;
    for &(on, name) in names {
        if on {
            if !first {
                w.put(b",");
            }
            first = false;
            w.put(name);
        }
    }
}

fn put_reason(w: &mut Wire, reason: Option<Reason>) {
    if let Some(r) = reason {
        w.put(b";reason=");
        w.put(r.name().as_bytes());
    }
}

/// Lowercase hexadecimal, no leading zeros (Zig's `{x}`).
fn put_hex(w: &mut Wire, mut n: u32) {
    let mut digits = [0u8; 8];
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b"0123456789abcdef"[(n % 16) as usize];
        n /= 16;
        if n == 0 {
            break;
        }
    }
    w.put(&digits[at..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(r: Response) -> std::vec::Vec<u8> {
        r.encode().as_bytes().to_vec()
    }

    // ghostty response.zig:218 "support formats default to no advertised formats"
    #[test]
    fn support_formats_default_to_no_advertised_formats() {
        assert_eq!(Formats::default(), Formats { glyf: false, colrv0: false, colrv1: false });
    }

    // ghostty response.zig:225 "response support formatWire"
    #[test]
    fn response_support_format_wire() {
        let r = Response::Support { fmt: Formats { glyf: true, colrv0: true, colrv1: false } };
        assert_eq!(wire(r), b"\x1b_25a1;s;fmt=glyf,colrv0\x1b\\");
        let r = Response::Support { fmt: Formats { glyf: false, colrv0: false, colrv1: true } };
        assert_eq!(wire(r), b"\x1b_25a1;s;fmt=colrv1\x1b\\");
    }

    // ghostty response.zig:236 "response support formatWire with no formats"
    #[test]
    fn response_support_format_wire_with_no_formats() {
        assert_eq!(wire(Response::Support { fmt: Formats::default() }), b"\x1b_25a1;s;fmt=\x1b\\");
    }

    // ghostty response.zig:247 "response query formatWire"
    #[test]
    fn response_query_format_wire() {
        let r = Response::Query { cp: 0xE0A0, status: Coverage { system: true, glossary: true } };
        assert_eq!(wire(r), b"\x1b_25a1;q;cp=e0a0;status=system,glossary\x1b\\");
        let r =
            Response::Query { cp: 0x10FFFD, status: Coverage { system: false, glossary: true } };
        assert_eq!(wire(r), b"\x1b_25a1;q;cp=10fffd;status=glossary\x1b\\");
    }

    // ghostty response.zig:258 "response query formatWire with no coverage"
    #[test]
    fn response_query_format_wire_with_no_coverage() {
        let r = Response::Query { cp: 0xE0A0, status: Coverage::default() };
        assert_eq!(wire(r), b"\x1b_25a1;q;cp=e0a0;status=\x1b\\");
    }

    // ghostty response.zig:269 "coverage parses comma-separated names"
    #[test]
    fn coverage_parses_comma_separated_names() {
        let c = |system, glossary| Coverage { system, glossary };
        assert_eq!(Coverage::parse(b""), c(false, false));
        assert_eq!(Coverage::parse(b"system"), c(true, false));
        assert_eq!(Coverage::parse(b"glossary"), c(false, true));
        assert_eq!(Coverage::parse(b"system,glossary"), c(true, true));
        assert_eq!(Coverage::parse(b"glossary,system"), c(true, true));
        assert_eq!(Coverage::parse(b"system,future"), c(true, false));
        assert_eq!(Coverage::parse(b"future"), c(false, false));
    }

    // ghostty response.zig:281 "response register success formatWire"
    #[test]
    fn response_register_success_format_wire() {
        let r = Response::Register { cp: 0xE0A0, status: Status::OK, reason: None };
        assert_eq!(wire(r), b"\x1b_25a1;r;cp=e0a0;status=0\x1b\\");
    }

    // ghostty response.zig:292 "response register error formatWire"
    #[test]
    fn response_register_error_format_wire() {
        let r = Response::Register {
            cp: 0xE0A0,
            status: Status::ERR,
            reason: Some(Reason::OutOfNamespace),
        };
        assert_eq!(wire(r), b"\x1b_25a1;r;cp=e0a0;status=1;reason=out_of_namespace\x1b\\");
    }

    // ghostty response.zig:303 "response register arbitrary status formatWire"
    #[test]
    fn response_register_arbitrary_status_format_wire() {
        let r = Response::Register {
            cp: 0xE0A0,
            status: Status(37),
            reason: Some(Reason::PayloadTooLarge),
        };
        assert_eq!(wire(r), b"\x1b_25a1;r;cp=e0a0;status=37;reason=payload_too_large\x1b\\");
    }

    // ghostty response.zig:314 "register reason names"
    #[test]
    fn register_reason_names() {
        assert_eq!(Reason::OutOfNamespace.name(), "out_of_namespace");
        assert_eq!(Reason::CompositeUnsupported.name(), "composite_unsupported");
        assert_eq!(Reason::HintingUnsupported.name(), "hinting_unsupported");
        assert_eq!(Reason::MalformedPayload.name(), "malformed_payload");
        assert_eq!(Reason::PayloadTooLarge.name(), "payload_too_large");
        assert_eq!(
            Reason::Other(ReasonName::new("future_reason").unwrap()).name(),
            "future_reason"
        );
        assert_eq!(ReasonName::new(""), None);
        assert_eq!(ReasonName::new("has;semicolon"), None);
        assert_eq!(ReasonName::new("abcdefghijklmnopqrstuvwxyz0123456"), None);
    }

    // ghostty response.zig:326 "response clear formatWire"
    #[test]
    fn response_clear_format_wire() {
        assert_eq!(
            wire(Response::Clear { status: Status::OK, reason: None }),
            b"\x1b_25a1;c;status=0\x1b\\"
        );
    }

    // ghostty response.zig:337 "response clear error formatWire"
    #[test]
    fn response_clear_error_format_wire() {
        let r = Response::Clear { status: Status::ERR, reason: Some(Reason::OutOfNamespace) };
        assert_eq!(wire(r), b"\x1b_25a1;c;status=1;reason=out_of_namespace\x1b\\");
    }

    /// The longest reply fits the wire.
    #[test]
    fn longest_reply_fits() {
        let name = ReasonName::new("abcdefghijklmnopqrstuvwxyz012345").unwrap();
        let r = Response::Register {
            cp: 0x1F_FFFF,
            status: Status(255),
            reason: Some(Reason::Other(name)),
        };
        assert!(r.encode().as_bytes().len() <= Wire::CAP);
    }
}
