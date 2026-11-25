//! The glyph protocol, APC `25a1` (ghostty's apc/glyph/*): register private-use glyph
//! outlines, query coverage, clear registrations. [`execute`] runs one request against a
//! [`Glossary`] and gives the reply, if any.

pub mod glossary;
pub mod glyf;
pub mod request;
pub mod response;

use blaze_own::Alloc;

pub use glossary::{Entry, EntryError, Glossary, GlossaryError};
pub use request::{Parser, PayloadError, Request, RequestError};
pub use response::{Coverage, Formats, Reason, ReasonName, Response, Status};

use request::{Clear, Query, Register, Reply};

/// The formats this terminal registers (`supported_formats`).
pub const SUPPORTED_FORMATS: Formats = Formats { glyf: true, colrv0: false, colrv1: false };

/// Run `req` (`execute`). A query sets only glossary coverage; the host adds system coverage.
pub fn execute<'a>(
    glossary: &mut Glossary<'a>,
    req: &Request<'_>,
    alloc: Alloc<'a>,
) -> Option<Response> {
    match req {
        Request::Support => Some(Response::Support { fmt: SUPPORTED_FORMATS }),
        Request::Query(q) => query(glossary, q),
        Request::Register(r) => register(glossary, r, alloc),
        Request::Clear(c) => Some(clear(glossary, c)),
    }
}

fn query(glossary: &Glossary<'_>, q: &Query<'_>) -> Option<Response> {
    let cp = q.cp()?;
    Some(Response::Query {
        cp,
        status: Coverage { system: false, glossary: glossary.contains(cp) },
    })
}

/// Why a register failed, before it maps to a reply reason.
enum RegisterFailed {
    Entry(EntryError),
    Glossary(GlossaryError),
    NoCodepoint,
}

fn register<'a>(
    glossary: &mut Glossary<'a>,
    r: &Register<'_>,
    alloc: Alloc<'a>,
) -> Option<Response> {
    let reply = r.reply();
    let outcome = (|| {
        let cp = r.cp().ok_or(RegisterFailed::NoCodepoint)?;
        // The entry is built before the namespace is checked, as ghostty does.
        let entry = Entry::new(r, alloc).map_err(RegisterFailed::Entry)?;
        glossary.register(cp, entry).map_err(RegisterFailed::Glossary)?;
        Ok(cp)
    })();
    match outcome {
        Ok(cp) => match reply {
            Reply::All => Some(Response::Register { cp, status: Status::OK, reason: None }),
            Reply::None | Reply::Failures => None,
        },
        Err(why) => match reply {
            Reply::None => None,
            Reply::All | Reply::Failures => Some(Response::Register {
                cp: r.cp().unwrap_or(0),
                status: Status::ERR,
                reason: Some(reason_of(why)),
            }),
        },
    }
}

fn reason_of(why: RegisterFailed) -> Reason {
    match why {
        RegisterFailed::Glossary(GlossaryError::OutOfMemory)
        | RegisterFailed::Entry(EntryError::Payload(PayloadError::OutOfMemory)) => {
            Reason::Other(ReasonName::OUT_OF_MEMORY)
        }
        RegisterFailed::Glossary(GlossaryError::OutOfNamespace) => Reason::OutOfNamespace,
        RegisterFailed::Entry(EntryError::Payload(PayloadError::PayloadTooLarge)) => {
            Reason::PayloadTooLarge
        }
        RegisterFailed::Entry(EntryError::Payload(PayloadError::CompositeUnsupported)) => {
            Reason::CompositeUnsupported
        }
        RegisterFailed::Entry(EntryError::Payload(PayloadError::HintingUnsupported)) => {
            Reason::HintingUnsupported
        }
        RegisterFailed::NoCodepoint
        | RegisterFailed::Entry(EntryError::Payload(PayloadError::MalformedPayload))
        | RegisterFailed::Entry(EntryError::InvalidOptions)
        | RegisterFailed::Entry(EntryError::UnsupportedFormat) => Reason::MalformedPayload,
    }
}

fn clear(glossary: &mut Glossary<'_>, c: &Clear<'_>) -> Response {
    if let Some(cp) = c.cp() {
        if glossary.delete(cp).is_err() {
            return Response::Clear { status: Status::ERR, reason: Some(Reason::OutOfNamespace) };
        }
    } else if c.has_cp() {
        return Response::Clear { status: Status::ERR, reason: Some(Reason::MalformedPayload) };
    } else {
        glossary.clear();
    }
    Response::Clear { status: Status::OK, reason: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kitty::storage::tests::alloc;

    fn parse(data: &[u8]) -> Request<'static> {
        let mut p = Parser::new(1024 * 1024, alloc());
        for &b in data {
            p.feed(b).unwrap();
        }
        p.complete().unwrap()
    }

    fn run(g: &mut Glossary<'static>, data: &[u8]) -> Option<Response> {
        execute(g, &parse(data), alloc())
    }

    const ZERO: &[u8] = b"AAAAAAAAAAAAAA==";

    fn reg(cp: &str) -> std::vec::Vec<u8> {
        let mut v = std::format!("r;cp={cp};").into_bytes();
        v.extend_from_slice(ZERO);
        v
    }

    fn err(cp: u32, reason: Reason) -> Option<Response> {
        Some(Response::Register { cp, status: Status::ERR, reason: Some(reason) })
    }

    fn clear_err(reason: Reason) -> Option<Response> {
        Some(Response::Clear { status: Status::ERR, reason: Some(reason) })
    }

    const CLEAR_OK: Option<Response> = Some(Response::Clear { status: Status::OK, reason: None });

    // ghostty execute.zig:138 "execute register stores glyph and returns success"
    #[test]
    fn execute_register_stores_glyph_and_returns_success() {
        let mut g = Glossary::new(alloc());
        assert_eq!(
            run(&mut g, &reg("e0a0")),
            Some(Response::Register { cp: 0xE0A0, status: Status::OK, reason: None })
        );
        assert!(g.contains(0xE0A0));
    }

    // ghostty execute.zig:154 "execute register reply failures suppresses success"
    #[test]
    fn execute_register_reply_failures_suppresses_success() {
        let mut g = Glossary::new(alloc());
        assert_eq!(run(&mut g, b"r;cp=e0a0;reply=2;AAAAAAAAAAAAAA=="), None);
        assert!(g.contains(0xE0A0));
    }

    // ghostty execute.zig:168 "execute register reply none suppresses failure"
    #[test]
    fn execute_register_reply_none_suppresses_failure() {
        let mut g = Glossary::new(alloc());
        assert_eq!(run(&mut g, b"r;cp=41;reply=0;%%%not-base64%%%"), None);
        assert!(!g.contains(0x41));
    }

    // ghostty execute.zig:182 "execute register rejects non-PUA"
    #[test]
    fn execute_register_rejects_non_pua() {
        let mut g = Glossary::new(alloc());
        assert_eq!(run(&mut g, &reg("41")), err(0x41, Reason::OutOfNamespace));
        assert!(!g.contains(0x41));
    }

    // ghostty execute.zig:202 "execute register reports malformed payload"
    #[test]
    fn execute_register_reports_malformed_payload() {
        let mut g = Glossary::new(alloc());
        assert_eq!(
            run(&mut g, b"r;cp=e0a0;%%%not-base64%%%"),
            err(0xE0A0, Reason::MalformedPayload)
        );
        assert!(!g.contains(0xE0A0));
        // The payload is checked before the namespace; a missing cp replies with cp 0.
        assert_eq!(run(&mut g, b"r;cp=41;%%%"), err(0x41, Reason::MalformedPayload));
        assert_eq!(run(&mut g, &reg("zz")), err(0, Reason::MalformedPayload));
        assert_eq!(
            run(&mut g, b"r;cp=e0a0;fmt=colrv0;AAAA"),
            err(0xE0A0, Reason::MalformedPayload)
        );
        assert_eq!(
            run(&mut g, b"r;cp=e0a0;AAEAAAAAAAAAAAAAAAI="),
            err(0xE0A0, Reason::HintingUnsupported)
        );
        assert_eq!(
            run(&mut g, b"r;cp=e0a0;//8AAAAAAAAAAA=="),
            err(0xE0A0, Reason::CompositeUnsupported)
        );
    }

    // ghostty execute.zig:222 "execute clear removes all glyphs"
    #[test]
    fn execute_clear_removes_all_glyphs() {
        let mut g = Glossary::new(alloc());
        run(&mut g, &reg("e0a0"));
        run(&mut g, &reg("e0a1"));
        assert_eq!(run(&mut g, b"c"), CLEAR_OK);
        assert!(!g.contains(0xE0A0) && !g.contains(0xE0A1));
    }

    // ghostty execute.zig:245 "execute clear removes one glyph"
    #[test]
    fn execute_clear_removes_one_glyph() {
        let mut g = Glossary::new(alloc());
        run(&mut g, &reg("e0a0"));
        run(&mut g, &reg("e0a1"));
        assert_eq!(run(&mut g, b"c;cp=e0a0"), CLEAR_OK);
        assert!(!g.contains(0xE0A0) && g.contains(0xE0A1));
    }

    // ghostty execute.zig:268 "execute clear rejects non-PUA"
    #[test]
    fn execute_clear_rejects_non_pua() {
        let mut g = Glossary::new(alloc());
        assert_eq!(run(&mut g, b"c;cp=41"), clear_err(Reason::OutOfNamespace));
    }

    // ghostty execute.zig:286 "execute clear rejects malformed cp without clearing glossary"
    #[test]
    fn execute_clear_rejects_malformed_cp_without_clearing_glossary() {
        let mut g = Glossary::new(alloc());
        run(&mut g, &reg("e0a0"));
        run(&mut g, &reg("e0a1"));
        for data in [&b"c;cp=zz"[..], b"c;cp=", b"c;cp=200000"] {
            assert_eq!(run(&mut g, data), clear_err(Reason::MalformedPayload));
            assert!(g.contains(0xE0A0) && g.contains(0xE0A1));
        }
    }

    // ghostty execute.zig:316 "execute query reports no coverage"
    #[test]
    fn execute_query_reports_no_coverage() {
        let mut g = Glossary::new(alloc());
        assert_eq!(
            run(&mut g, b"q;cp=e0a0"),
            Some(Response::Query { cp: 0xE0A0, status: Coverage::default() })
        );
    }

    // ghostty execute.zig:334 "execute query reports glossary coverage"
    #[test]
    fn execute_query_reports_glossary_coverage() {
        let mut g = Glossary::new(alloc());
        run(&mut g, &reg("e0a0"));
        let status = Coverage { system: false, glossary: true };
        assert_eq!(run(&mut g, b"q;cp=e0a0"), Some(Response::Query { cp: 0xE0A0, status }));
    }

    // ghostty execute.zig:356 "execute query without cp returns no response"
    #[test]
    fn execute_query_without_cp_returns_no_response() {
        let mut g = Glossary::new(alloc());
        assert_eq!(run(&mut g, b"q;foo=bar"), None);
    }

    #[test]
    fn execute_support() {
        let mut g = Glossary::new(alloc());
        let r = run(&mut g, b"s").unwrap();
        assert_eq!(r.encode().as_bytes(), b"\x1b_25a1;s;fmt=glyf\x1b\\");
    }
}
