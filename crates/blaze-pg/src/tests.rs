use std::borrow::ToOwned;
use std::format;
use std::string::{String, ToString};
use std::vec;
use std::vec::Vec;

use alloc::alloc::Global;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use blaze_sql::desc::{Storage, ValueRef};
use blaze_sim::Tape;

use crate::codec::{self, EncodeError, ValueError};
use crate::proto::{base64_18, put_base64};
use crate::scram;
use crate::{
    AuthError, Bound, Config, Connection, Error, Event, Expect, Fatal, MESSAGE_MOST, ParamType,
    QueryId, Refused, StatementId, What,
};

mod sim;

// ---------------------------------------------------------------------------------------------
// Values the tests own.

pub(crate) const STORAGES: [Storage; 5] =
    [Storage::Boolean, Storage::Integer, Storage::Real, Storage::Text, Storage::Blob];

/// A value owned by the test, compared bit for bit (a NaN equals itself).
#[derive(Clone, Debug)]
pub(crate) enum Owned {
    Null,
    Boolean(bool),
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl PartialEq for Owned {
    fn eq(&self, other: &Owned) -> bool {
        match (self, other) {
            (Owned::Null, Owned::Null) => true,
            (Owned::Boolean(a), Owned::Boolean(b)) => a == b,
            (Owned::Integer(a), Owned::Integer(b)) => a == b,
            (Owned::Real(a), Owned::Real(b)) => a.to_bits() == b.to_bits(),
            (Owned::Text(a), Owned::Text(b)) => a == b,
            (Owned::Blob(a), Owned::Blob(b)) => a == b,
            _ => false,
        }
    }
}

impl Owned {
    pub(crate) fn as_ref(&self) -> ValueRef<'_> {
        match self {
            Owned::Null => ValueRef::Null,
            Owned::Boolean(it) => ValueRef::Boolean(*it),
            Owned::Integer(it) => ValueRef::Integer(*it),
            Owned::Real(it) => ValueRef::Real(*it),
            Owned::Text(it) => ValueRef::Text(it),
            Owned::Blob(it) => ValueRef::Blob(it),
        }
    }

    pub(crate) fn of(value: ValueRef<'_>) -> Owned {
        match value {
            ValueRef::Null => Owned::Null,
            ValueRef::Boolean(it) => Owned::Boolean(it),
            ValueRef::Integer(it) => Owned::Integer(it),
            ValueRef::Real(it) => Owned::Real(it),
            ValueRef::Text(it) => Owned::Text(it.to_owned()),
            ValueRef::Blob(it) => Owned::Blob(it.to_vec()),
        }
    }
}

pub(crate) fn storage(tape: &Tape) -> Storage {
    *tape.one_of(&STORAGES, "storage")
}

/// A value of `storage`, null sometimes when `nullable`. Text has no NUL, which Postgres text
/// cannot hold.
pub(crate) fn value(tape: &Tape, storage: Storage, nullable: bool) -> Owned {
    if nullable && tape.int_in(0..=4u8, "null") == 0 {
        return Owned::Null;
    }
    match storage {
        Storage::Boolean => Owned::Boolean(tape.bool("boolean")),
        Storage::Integer => Owned::Integer(tape.int("integer")),
        Storage::Real => Owned::Real(tape.f64("real")),
        Storage::Text => Owned::Text(text_without_nul(tape, 0..=10, "text")),
        Storage::Blob => Owned::Blob(tape.bytes(0..=10, "blob")),
    }
}

/// `sizes` characters of `Tape::char`, none of them NUL: a NUL drawn is redrawn, in its own
/// span, at most eight times before the case is discarded.
fn text_without_nul(
    tape: &Tape,
    sizes: core::ops::RangeInclusive<usize>,
    label: &'static str,
) -> String {
    let characters = tape.vec(sizes, label, |tape| {
        for _ in 0..8 {
            match tape.char(label) {
                '\0' => {}
                character => return character,
            }
        }
        tape.discard()
    });
    characters.into_iter().collect()
}

// ---------------------------------------------------------------------------------------------
// The server's side of the protocol, written by the tests: the outside world, not the core.

pub(crate) mod server {
    use super::*;

    pub(crate) fn message(out: &mut Vec<u8>, tag: u8, body: &[u8]) {
        out.push(tag);
        out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
        out.extend_from_slice(body);
    }

    pub(crate) fn authentication(out: &mut Vec<u8>, code: u32, data: &[u8]) {
        let mut body = code.to_be_bytes().to_vec();
        body.extend_from_slice(data);
        message(out, b'R', &body);
    }

    pub(crate) fn parameter_status(out: &mut Vec<u8>, name: &str, value: &str) {
        let body = format!("{name}\0{value}\0");
        message(out, b'S', body.as_bytes());
    }

    /// What follows `AuthenticationOk`: parameters, the backend key, `ReadyForQuery`.
    pub(crate) fn startup_done(out: &mut Vec<u8>) {
        parameter_status(out, "client_encoding", "UTF8");
        parameter_status(out, "server_version", "17.2");
        message(out, b'K', &[0, 0, 0, 7, 1, 2, 3, 4]);
        message(out, b'Z', b"I");
    }

    /// An `ErrorResponse` (`tag` `E`) or `NoticeResponse` (`N`).
    pub(crate) fn report(out: &mut Vec<u8>, tag: u8, severity: &str, code: &str, text: &str) {
        let body = format!("S{severity}\0V{severity}\0C{code}\0M{text}\0Fpostgres.c\0\0");
        message(out, tag, body.as_bytes());
    }

    pub(crate) fn parameter_description(out: &mut Vec<u8>, oids: &[u32]) {
        let mut body = (oids.len() as u16).to_be_bytes().to_vec();
        for oid in oids {
            body.extend_from_slice(&oid.to_be_bytes());
        }
        message(out, b't', &body);
    }

    pub(crate) fn row_description(out: &mut Vec<u8>, columns: &[(&str, u32)]) {
        let mut body = (columns.len() as u16).to_be_bytes().to_vec();
        for (index, (name, oid)) in columns.iter().enumerate() {
            body.extend_from_slice(name.as_bytes());
            body.push(0);
            body.extend_from_slice(&16384u32.to_be_bytes());
            body.extend_from_slice(&(index as u16 + 1).to_be_bytes());
            body.extend_from_slice(&oid.to_be_bytes());
            body.extend_from_slice(&(-1i16).to_be_bytes());
            body.extend_from_slice(&(-1i32).to_be_bytes());
            body.extend_from_slice(&0u16.to_be_bytes());
        }
        message(out, b'T', &body);
    }

    pub(crate) fn data_row(out: &mut Vec<u8>, values: &[Owned]) {
        let mut body = (values.len() as u16).to_be_bytes().to_vec();
        for value in values {
            codec::encode(value.as_ref(), &mut body).expect("the test's values encode");
        }
        message(out, b'D', &body);
    }

    pub(crate) fn command_complete(out: &mut Vec<u8>, tag: &str) {
        message(out, b'C', format!("{tag}\0").as_bytes());
    }

    /// The server's side of SCRAM-SHA-256 after a client-first-message: what it sent and what
    /// it needs to check the proof.
    pub(crate) struct ScramServer {
        salt: Vec<u8>,
        iterations: u32,
        nonce: Vec<u8>,
        client_first_bare: Vec<u8>,
        pub(crate) server_first: Vec<u8>,
    }

    impl ScramServer {
        pub(crate) fn new(
            client_first: &[u8],
            salt: &[u8],
            iterations: u32,
        ) -> Result<ScramServer, String> {
            let bare = client_first.strip_prefix(b"n,,").ok_or("no GS2 header n,,")?;
            let nonce = bare
                .split(|&b| b == b',')
                .find_map(|it| it.strip_prefix(b"r="))
                .ok_or("no client nonce")?;
            let mut full = nonce.to_vec();
            full.extend_from_slice(b"+server/nonce");
            let server_first = format!(
                "r={},s={},i={iterations}",
                String::from_utf8_lossy(&full),
                STANDARD.encode(salt)
            )
            .into_bytes();
            Ok(ScramServer {
                salt: salt.to_vec(),
                iterations,
                nonce: full,
                client_first_bare: bare.to_vec(),
                server_first,
            })
        }

        /// Checks the client-final-message's proof; the server-final-message.
        pub(crate) fn finish(
            &self,
            password: &[u8],
            client_final: &[u8],
        ) -> Result<Vec<u8>, String> {
            let text =
                core::str::from_utf8(client_final).map_err(|_| "client-final is not UTF-8")?;
            let (without_proof, proof) =
                text.rsplit_once(",p=").ok_or("client-final has no proof")?;
            let want = format!("c=biws,r={}", String::from_utf8_lossy(&self.nonce));
            if without_proof != want {
                return Err(format!("client-final starts {without_proof:?}, not {want:?}"));
            }
            let proof = STANDARD.decode(proof).map_err(|e| format!("proof is not base64: {e}"))?;
            let salted = scram::pbkdf2(password, &self.salt, self.iterations);
            let client_key = scram::hmac(&salted, b"Client Key");
            let stored_key = blaze_sha256::digest(&client_key);
            let mut auth = self.client_first_bare.clone();
            auth.push(b',');
            auth.extend_from_slice(&self.server_first);
            auth.push(b',');
            auth.extend_from_slice(without_proof.as_bytes());
            let signature = scram::hmac(&stored_key, &auth);
            if proof.len() != 32 {
                return Err(format!("a proof of {} bytes", proof.len()));
            }
            let recovered: Vec<u8> = proof.iter().zip(&signature).map(|(p, s)| p ^ s).collect();
            if blaze_sha256::digest(&recovered) != stored_key {
                return Err("the proof is not the password's".to_string());
            }
            let server_key = scram::hmac(&salted, b"Server Key");
            Ok(format!("v={}", STANDARD.encode(scram::hmac(&server_key, &auth))).into_bytes())
        }
    }

    /// The frontend's messages from a byte stream: the startup message first (tag 0), then
    /// tagged ones.
    #[derive(Default)]
    pub(crate) struct Frontend {
        buffer: Vec<u8>,
        started: bool,
    }

    impl Frontend {
        pub(crate) fn push(&mut self, bytes: &[u8]) {
            self.buffer.extend_from_slice(bytes);
        }

        pub(crate) fn next(&mut self) -> Result<Option<(u8, Vec<u8>)>, String> {
            let (tag, head) = if self.started { (None, 1) } else { (Some(0), 0) };
            let Some(len) = self.buffer.get(head..head + 4) else { return Ok(None) };
            let len = u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize;
            if len < 4 {
                return Err(format!("a frontend message of length {len}"));
            }
            let total = head + len;
            if self.buffer.len() < total {
                return Ok(None);
            }
            let tag = tag.unwrap_or(self.buffer[0]);
            let body = self.buffer[head + 4..total].to_vec();
            self.buffer.drain(..total);
            self.started = true;
            Ok(Some((tag, body)))
        }
    }

    /// A reader of a frontend message's body, for the test's server.
    pub(crate) struct Body<'a>(pub(crate) &'a [u8]);

    impl<'a> Body<'a> {
        pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
            if self.0.len() < n {
                return Err(format!("a body that ends {n} bytes early"));
            }
            let (head, rest) = self.0.split_at(n);
            self.0 = rest;
            Ok(head)
        }

        pub(crate) fn u16(&mut self) -> Result<u16, String> {
            let b = self.take(2)?;
            Ok(u16::from_be_bytes([b[0], b[1]]))
        }

        pub(crate) fn i32(&mut self) -> Result<i32, String> {
            let b = self.take(4)?;
            Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        }

        pub(crate) fn cstr(&mut self) -> Result<&'a [u8], String> {
            let end = self.0.iter().position(|&b| b == 0).ok_or("a string with no NUL")?;
            let text = self.take(end)?;
            self.take(1)?;
            Ok(text)
        }

        pub(crate) fn end(&self) -> Result<(), String> {
            if self.0.is_empty() {
                Ok(())
            } else {
                Err(format!("{} bytes after the body", self.0.len()))
            }
        }
    }
}

use server::*;

/// Everything the connection has to send, marked sent.
fn sends(connection: &mut Connection<Global>) -> Vec<u8> {
    let mut all = Vec::new();
    while let Some(bytes) = connection.next_send() {
        let len = bytes.len();
        all.extend_from_slice(bytes);
        connection.sent(len);
    }
    all
}

/// Every event, as its debug text.
fn events(connection: &mut Connection<Global>) -> Vec<String> {
    let mut all = Vec::new();
    while let Some(event) = connection.next_event() {
        all.push(format!("{event:?}"));
    }
    all
}

fn config(password: &str) -> Config<'_> {
    Config { user: "app", database: "db", password, startup_deadline: 1_000_000 }
}

/// A connection the server trusted, past `ReadyForQuery`.
fn ready() -> Connection<Global> {
    let mut connection = Connection::new(config("secret"), [7; 18], Global);
    sends(&mut connection);
    let mut stream = Vec::new();
    authentication(&mut stream, 0, &[]);
    startup_done(&mut stream);
    connection.received(&stream);
    assert!(matches!(connection.next_event(), Some(Event::Ready)));
    assert!(connection.next_event().is_none());
    connection
}

/// The close reason of the last event, as its text.
fn close_text(connection: &mut Connection<Global>) -> String {
    let mut why = None;
    while let Some(event) = connection.next_event() {
        if let Event::Close { why: fatal } = event {
            why = Some(fatal.to_string());
        }
    }
    why.expect("the connection closed")
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// HMAC, PBKDF2 and SCRAM against their RFCs' vectors.

#[test]
fn hmac_is_rfc_4231() {
    let long_key = [0xaa; 131];
    let cases: [(&[u8], &[u8], &str); 6] = [
        (&[0x0b; 20], b"Hi There", "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"),
        (b"Jefe", b"what do ya want for nothing?", "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"),
        (&[0xaa; 20], &[0xdd; 50], "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"),
        (
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25],
            &[0xcd; 50],
            "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b",
        ),
        (
            &long_key,
            b"Test Using Larger Than Block-Size Key - Hash Key First",
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
        ),
        (
            &long_key,
            b"This is a test using a larger than block-size key and a larger than block-size data. \
              The key needs to be hashed before being used by the HMAC algorithm.",
            "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2",
        ),
    ];
    for (number, (key, data, want)) in cases.iter().enumerate() {
        assert_eq!(scram::hmac(key, data).to_vec(), hex(want), "RFC 4231 case {number}");
    }
    // In parts, the same MAC.
    let mac = scram::Hmac::new(b"Jefe");
    assert_eq!(mac.mac(&[b"what do ya ", b"want for nothing?"]).to_vec(), hex(cases[1].2));
}

#[test]
fn pbkdf2_is_rfc_7914() {
    // RFC 7914 section 11's first PBKDF2-HMAC-SHA256 vector, its first 32 bytes, and the
    // widely published 4096-iteration one.
    assert_eq!(
        scram::pbkdf2(b"passwd", b"salt", 1).to_vec(),
        hex("55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc")
    );
    assert_eq!(
        scram::pbkdf2(b"password", b"salt", 4096).to_vec(),
        hex("c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a")
    );
}

#[test]
fn scram_is_rfc_7677() {
    let nonce = b"rOprNGfwEbeRWgbNEkqO";
    let mut first = Vec::new();
    scram::client_first(b"user", nonce, &mut first);
    assert_eq!(first, b"n,,n=user,r=rOprNGfwEbeRWgbNEkqO");
    let server_first =
        b"r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
    let mut last = Vec::new();
    let signature =
        scram::client_final(b"pencil", &first[3..], server_first, nonce, &mut last).unwrap();
    assert_eq!(
        String::from_utf8(last).unwrap(),
        "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="
    );
    assert_eq!(
        scram::server_final(b"v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=", &signature),
        Ok(())
    );
    assert_eq!(
        scram::server_final(b"v=7rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=", &signature),
        Err(AuthError::Signature)
    );
    assert_eq!(scram::server_final(b"e=invalid-proof", &signature), Err(AuthError::ServerRefused));
}

#[test]
fn scram_refuses_a_server_first_message_by_name() {
    let nonce = b"abc";
    let first = |message: &[u8]| scram::server_first(message, nonce).map(|_| ());
    assert_eq!(first(b"r=xyz1,s=c2FsdA==,i=4096"), Err(AuthError::Nonce));
    assert_eq!(first(b"r=abc,s=c2FsdA==,i=4096"), Err(AuthError::Nonce));
    assert_eq!(first(b"r=abcd,s=c2FsdA==,i=0"), Err(AuthError::Iterations { iterations: 0 }));
    assert_eq!(
        first(b"r=abcd,s=c2FsdA==,i=99999999"),
        Err(AuthError::Iterations { iterations: 99_999_999 })
    );
    assert!(matches!(first(b"m=ext,r=abcd,s=c2FsdA==,i=1"), Err(AuthError::ServerFirst { .. })));
    assert!(matches!(first(b"r=abcd,s=c2FsdA==,i=1,x=y"), Err(AuthError::ServerFirst { .. })));
    assert!(matches!(first(b"r=abcd,s=!!,i=1"), Err(AuthError::ServerFirst { .. })));
    assert_eq!(first(b"r=abcd,s=c2FsdA==,i=1"), Ok(()));
}

#[test]
fn base64_is_the_crate_s() {
    blaze_sim::check(blaze_sim::Config::cases(100), |tape| {
        let bytes = tape.bytes(0..=70, "bytes");
        let mut ours = Vec::new();
        put_base64(&mut ours, &bytes);
        assert_eq!(ours, STANDARD.encode(&bytes).into_bytes());
        let nonce: [u8; 18] =
            core::array::from_fn(|at| bytes.get(at).copied().unwrap_or(at as u8));
        assert_eq!(base64_18(&nonce).to_vec(), STANDARD.encode(nonce).into_bytes());
    });
}

// ---------------------------------------------------------------------------------------------
// The codecs.

/// The refinement: decoding what `encode` wrote gives the value back, for every storage.
#[test]
fn a_value_round_trips() {
    blaze_sim::check(blaze_sim::Config::cases(100), |tape| {
        let storage = storage(tape);
        let value = value(tape, storage, true);
        let mut bytes = Vec::new();
        codec::encode(value.as_ref(), &mut bytes).unwrap();
        let mut rest = &bytes[..];
        let body = codec::split_value(&mut rest, "value").unwrap();
        assert!(rest.is_empty());
        assert_eq!(Owned::of(codec::decode(storage, body).unwrap()), value);
    });
}

/// The refinement: decoding what `encode_array` wrote gives the elements back, nulls included.
#[test]
fn an_array_round_trips() {
    blaze_sim::check(blaze_sim::Config::cases(100), |tape| {
        let storage = storage(tape);
        let items: Vec<Owned> = tape.vec(0..=6, "items", |tape| value(tape, storage, true));
        let refs: Vec<ValueRef<'_>> = items.iter().map(Owned::as_ref).collect();
        let mut bytes = Vec::new();
        codec::encode_array(storage, &refs, &mut bytes).unwrap();
        let mut rest = &bytes[..];
        let body =
            codec::split_value(&mut rest, "array").unwrap().expect("an array is not null");
        assert!(rest.is_empty());
        let mut elements = codec::decode_array(storage, body).unwrap();
        let got: Vec<Owned> = elements.by_ref().map(|it| Owned::of(it.unwrap())).collect();
        elements.finish().unwrap();
        assert_eq!(got, items);
    });
}

#[test]
fn codecs_refuse_by_name() {
    assert_eq!(codec::decode(Storage::Boolean, Some(&[2])), Err(ValueError::Boolean { byte: 2 }));
    assert_eq!(
        codec::decode(Storage::Integer, Some(&[0; 7])),
        Err(ValueError::Length { storage: Storage::Integer, len: 7 })
    );
    assert_eq!(
        codec::decode(Storage::Text, Some(b"a\xff")),
        Err(ValueError::NotUtf8 { valid_up_to: 1 })
    );
    let mut out = Vec::new();
    assert_eq!(
        codec::encode(ValueRef::Text("a\0b"), &mut out),
        Err(EncodeError::TextHasNul { at: 1 })
    );
    assert_eq!(
        codec::encode_array(
            Storage::Integer,
            &[ValueRef::Integer(1), ValueRef::Text("x")],
            &mut out
        ),
        Err(EncodeError::ElementStorage {
            index: 1,
            expected: Storage::Integer,
            got: Storage::Text
        })
    );
    let header = |dimensions: i32, flag: i32, oid: u32, bound: i32| {
        let mut bytes = Vec::new();
        for word in [dimensions, flag, oid as i32, 1, bound] {
            bytes.extend_from_slice(&word.to_be_bytes());
        }
        bytes
    };
    let refused =
        |bytes: Vec<u8>| codec::decode_array(Storage::Integer, &bytes).map(|_| ()).unwrap_err();
    assert_eq!(refused(header(2, 0, codec::INT8, 1)), ValueError::Dimensions { dimensions: 2 });
    assert_eq!(refused(header(1, 0, codec::INT8, 0)), ValueError::LowerBound { bound: 0 });
    assert_eq!(refused(header(1, 2, codec::INT8, 1)), ValueError::NullFlag { flag: 2 });
    assert_eq!(
        refused(header(1, 0, codec::TEXT, 1)),
        ValueError::ElementOid { expected: codec::INT8, got: codec::TEXT }
    );
    // A null in an array whose flag says it has none.
    let mut bytes = header(1, 0, codec::INT8, 1);
    bytes.extend_from_slice(&(-1i32).to_be_bytes());
    let first = codec::decode_array(Storage::Integer, &bytes).unwrap().next();
    assert_eq!(first, Some(Err(ValueError::NullNotFlagged { index: 0 })));
}

// ---------------------------------------------------------------------------------------------
// The connection.

#[test]
fn startup_is_protocol_3_with_utf8() {
    let mut connection = Connection::new(config("secret"), [7; 18], Global);
    let mut want = Vec::new();
    let body = b"\0\x03\0\0user\0app\0database\0db\0client_encoding\0UTF8\0\0";
    want.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    want.extend_from_slice(body);
    assert_eq!(sends(&mut connection), want);
    assert_eq!(connection.next_deadline(), Some(1_000_000));
}

#[test]
fn a_cleartext_password_is_sent_when_asked() {
    let mut connection = Connection::new(config("secret"), [7; 18], Global);
    sends(&mut connection);
    let mut stream = Vec::new();
    authentication(&mut stream, 3, &[]);
    connection.received(&stream);
    let mut want = Vec::new();
    message(&mut want, b'p', b"secret\0");
    assert_eq!(sends(&mut connection), want);
    let mut stream = Vec::new();
    authentication(&mut stream, 0, &[]);
    startup_done(&mut stream);
    connection.received(&stream);
    assert_eq!(events(&mut connection), vec!["Ready".to_string()]);
    assert_eq!(connection.backend_key().map(|key| key.pid), Some(7));
}

#[test]
fn scram_through_the_connection() {
    let password = "correct horse";
    let mut connection = Connection::new(config(password), [3; 18], Global);
    sends(&mut connection);
    let mut stream = Vec::new();
    authentication(&mut stream, 10, b"SCRAM-SHA-256-PLUS\0SCRAM-SHA-256\0\0");
    connection.received(&stream);
    let sent = sends(&mut connection);
    let mut body = Body(&sent[5..]);
    assert_eq!(sent[0], b'p');
    assert_eq!(body.cstr().unwrap(), b"SCRAM-SHA-256");
    let len = body.i32().unwrap() as usize;
    let client_first = body.take(len).unwrap().to_vec();
    body.end().unwrap();
    let server = ScramServer::new(&client_first, b"pepper and salt", 64).unwrap();
    let mut stream = Vec::new();
    authentication(&mut stream, 11, &server.server_first);
    // Split anywhere, the same.
    for byte in &stream {
        connection.received(&[*byte]);
    }
    let sent = sends(&mut connection);
    assert_eq!(sent[0], b'p');
    let server_final = server.finish(password.as_bytes(), &sent[5..]).unwrap();
    let mut stream = Vec::new();
    authentication(&mut stream, 12, &server_final);
    authentication(&mut stream, 0, &[]);
    startup_done(&mut stream);
    connection.received(&stream);
    assert_eq!(events(&mut connection), vec!["Ready".to_string()]);
}

#[test]
fn a_server_without_the_password_is_refused() {
    let mut connection = Connection::new(config("secret"), [3; 18], Global);
    sends(&mut connection);
    let mut stream = Vec::new();
    authentication(&mut stream, 10, b"SCRAM-SHA-256\0\0");
    connection.received(&stream);
    let sent = sends(&mut connection);
    let client_first = &sent[5 + 14 + 4..];
    let server = ScramServer::new(client_first, b"salt", 8).unwrap();
    let mut stream = Vec::new();
    authentication(&mut stream, 11, &server.server_first);
    connection.received(&stream);
    sends(&mut connection);
    // A signature of another exchange: the server does not hold this password's keys.
    let mut stream = Vec::new();
    authentication(&mut stream, 12, b"v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=");
    connection.received(&stream);
    let why = close_text(&mut connection);
    assert!(why.contains("server signature"), "{why}");
}

#[test]
fn md5_and_the_rest_are_refused_by_name() {
    for (code, name) in [(5u32, "MD5"), (2, "Kerberos V5"), (7, "GSSAPI"), (9, "SSPI")] {
        let mut connection = Connection::new(config("secret"), [7; 18], Global);
        sends(&mut connection);
        let mut stream = Vec::new();
        authentication(&mut stream, code, if code == 5 { &[1, 2, 3, 4] } else { &[] });
        connection.received(&stream);
        let why = close_text(&mut connection);
        assert!(why.contains(name), "{why}");
        assert!(connection.next_send().is_none());
    }
}

#[test]
fn channel_binding_alone_is_refused() {
    let mut connection = Connection::new(config("secret"), [7; 18], Global);
    sends(&mut connection);
    let mut stream = Vec::new();
    authentication(&mut stream, 10, b"SCRAM-SHA-256-PLUS\0\0");
    connection.received(&stream);
    assert!(close_text(&mut connection).contains("SCRAM-SHA-256-PLUS"));
}

#[test]
fn a_password_outside_printable_ascii_is_refused_naming_saslprep() {
    let mut connection = Connection::new(config("p\u{e4}ss"), [7; 18], Global);
    sends(&mut connection);
    let mut stream = Vec::new();
    authentication(&mut stream, 10, b"SCRAM-SHA-256\0\0");
    connection.received(&stream);
    let why = close_text(&mut connection);
    assert!(why.contains("byte 1") && why.contains("SASLprep"), "{why}");
}

#[test]
fn a_refused_login_closes_with_the_server_s_words() {
    let mut connection = Connection::new(config("secret"), [7; 18], Global);
    sends(&mut connection);
    let mut stream = Vec::new();
    report(&mut stream, b'E', "FATAL", "28P01", "password authentication failed for user \"app\"");
    connection.received(&stream);
    let why = close_text(&mut connection);
    assert!(why.contains("28P01") && why.contains("password authentication failed"), "{why}");
}

#[test]
fn startup_past_its_deadline_closes() {
    let mut connection = Connection::new(config("secret"), [7; 18], Global);
    sends(&mut connection);
    connection.advance_clock(999_999);
    assert!(connection.next_event().is_none());
    connection.advance_clock(1_000_000);
    assert!(close_text(&mut connection).contains("startup did not end by its deadline 1000000"));
    assert_eq!(connection.next_deadline(), None);
}

#[test]
fn a_message_the_state_does_not_allow_names_its_type_and_the_state() {
    let mut connection = ready();
    let mut stream = Vec::new();
    data_row(&mut stream, &[Owned::Integer(1)]);
    connection.received(&stream);
    let why = close_text(&mut connection);
    assert!(why.contains("DataRow") && why.contains("no query is in flight"), "{why}");
}

#[test]
fn a_length_past_the_bound_is_refused_before_its_body() {
    let mut connection = ready();
    let mut stream = vec![b'D'];
    stream.extend_from_slice(&(MESSAGE_MOST as u32 + 1).to_be_bytes());
    connection.received(&stream);
    let mut closed = None;
    while let Some(event) = connection.next_event() {
        if let Event::Close { why: Fatal::Protocol(error) } = event {
            closed = Some(error.what);
        }
    }
    assert_eq!(closed, Some(What::TooLong { len: MESSAGE_MOST as u32 + 1, most: MESSAGE_MOST }));
}

/// The server's answer to a prepare: `ParseComplete`, the parameters' types, the columns (or
/// `NoData`), `ReadyForQuery`.
fn prepare_answer(stream: &mut Vec<u8>, params: &[u32], columns: Option<&[(&str, u32)]>) {
    message(stream, b'1', &[]);
    parameter_description(stream, params);
    match columns {
        Some(columns) => row_description(stream, columns),
        None => message(stream, b'n', &[]),
    }
    message(stream, b'Z', b"I");
}

/// The server's answer to a query of one integer column: `BindComplete`, the rows, the tag,
/// `ReadyForQuery`.
fn query_answer(stream: &mut Vec<u8>, rows: &[i64]) {
    message(stream, b'2', &[]);
    for row in rows {
        data_row(stream, &[Owned::Integer(*row)]);
    }
    command_complete(stream, &format!("SELECT {}", rows.len()));
    message(stream, b'Z', b"I");
}

const INTEGER_N: [(&str, Storage); 1] = [("n", Storage::Integer)];

/// A ready connection holding `statement`, `select n from t where k = $1` of one integer
/// parameter and one integer column, prepared.
fn prepared(statement: StatementId) -> Connection<Global> {
    let mut connection = ready();
    let params = [ParamType::Scalar(Storage::Integer)];
    connection
        .prepare(statement, "select n from t where k = $1", &params, Expect::Rows(&INTEGER_N), 100)
        .unwrap();
    sends(&mut connection);
    let mut stream = Vec::new();
    prepare_answer(&mut stream, &[codec::INT8], Some(&[("n", codec::INT8)]));
    connection.received(&stream);
    assert_eq!(events(&mut connection), vec![format!("Prepared {{ statement: {statement:?} }}")]);
    connection
}

/// The frontend messages in `bytes`, after startup: each tag and body.
fn frontend(bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut parsed = Frontend::default();
    // A started stream: a startup message of length 8 first, which is dropped.
    parsed.push(&[0, 0, 0, 8, 0, 3, 0, 0]);
    parsed.push(bytes);
    let mut all = Vec::new();
    while let Some(message) = parsed.next().unwrap() {
        all.push(message);
    }
    all.remove(0);
    all
}

#[test]
fn a_statement_is_prepared_by_name_and_queried_without_a_describe() {
    let statement = StatementId(7);
    let mut connection = ready();
    let params = [ParamType::Scalar(Storage::Integer)];
    connection
        .prepare(statement, "select n from t where k = $1", &params, Expect::Rows(&INTEGER_N), 100)
        .unwrap();
    let sent = frontend(&sends(&mut connection));
    let tags: Vec<u8> = sent.iter().map(|(tag, _)| *tag).collect();
    assert_eq!(tags, b"PDS");
    assert_eq!(sent[0].1, b"blaze_7\0select n from t where k = $1\0\0\x01\0\0\0\x14");
    assert_eq!(sent[1].1, b"Sblaze_7\0");
    let mut stream = Vec::new();
    prepare_answer(&mut stream, &[codec::INT8], Some(&[("n", codec::INT8)]));
    connection.received(&stream);
    assert_eq!(events(&mut connection), vec!["Prepared { statement: StatementId(7) }".to_string()]);

    connection.query(QueryId(1), statement, &[Bound::Value(ValueRef::Integer(3))], 200).unwrap();
    let sent = frontend(&sends(&mut connection));
    let tags: Vec<u8> = sent.iter().map(|(tag, _)| *tag).collect();
    assert_eq!(tags, b"BES");
    let mut bind = Vec::new();
    bind.extend_from_slice(b"\0blaze_7\0\0\x01\0\x01\0\x01");
    codec::encode(ValueRef::Integer(3), &mut bind).unwrap();
    bind.extend_from_slice(b"\0\x01\0\x01");
    assert_eq!(sent[0].1, bind);
    let mut stream = Vec::new();
    query_answer(&mut stream, &[5, 6]);
    connection.received(&stream);
    assert_eq!(
        events(&mut connection),
        [
            "Row { id: QueryId(1), columns: [Integer(5)] }",
            "Row { id: QueryId(1), columns: [Integer(6)] }",
            "Done { id: QueryId(1), changed: Some(2) }",
        ]
    );
}

#[test]
fn calls_are_refused_by_name_with_nothing_sent() {
    let statement = StatementId(1);
    let mut connection = Connection::new(config("secret"), [7; 18], Global);
    assert_eq!(
        connection.prepare(statement, "select 1", &[], Expect::Count, 10),
        Err(Refused::NotReady)
    );
    assert_eq!(connection.query(QueryId(0), statement, &[], 10), Err(Refused::NotReady));
    assert_eq!(connection.close_statement(statement), Err(Refused::NotReady));

    let mut connection = ready();
    assert_eq!(
        connection.query(QueryId(0), statement, &[], 10),
        Err(Refused::NotPrepared { statement })
    );
    assert_eq!(connection.close_statement(statement), Err(Refused::NotPrepared { statement }));
    assert_eq!(
        connection.prepare(statement, "select 1\0", &[], Expect::Count, 10),
        Err(Refused::TextHasNul { at: 8 })
    );
    assert!(connection.next_send().is_none());

    let params = [ParamType::Scalar(Storage::Integer), ParamType::Array(Storage::Integer)];
    connection
        .prepare(statement, "update t set x = $1 where k = any($2)", &params, Expect::Count, 100)
        .unwrap();
    sends(&mut connection);
    assert_eq!(
        connection.prepare(statement, "select 2", &[], Expect::Count, 100),
        Err(Refused::StatementExists { statement })
    );
    assert_eq!(
        connection.query(QueryId(0), statement, &[], 100),
        Err(Refused::StatementPreparing { statement })
    );
    let mut stream = Vec::new();
    prepare_answer(&mut stream, &[codec::INT8, codec::INT8_ARRAY], None);
    connection.received(&stream);
    events(&mut connection);

    let items = [ValueRef::Integer(1), ValueRef::Real(2.0)];
    let one = Bound::Value(ValueRef::Integer(1));
    assert_eq!(
        connection.query(QueryId(0), statement, &[one], 100),
        Err(Refused::ValueCount { expected: 2, got: 1 })
    );
    assert_eq!(
        connection.query(
            QueryId(0),
            statement,
            &[Bound::Value(ValueRef::Text("x")), Bound::Array(&[])],
            100
        ),
        Err(Refused::ValueType { index: 0, expected: codec::INT8, got: codec::TEXT })
    );
    assert_eq!(
        connection.query(QueryId(0), statement, &[one, Bound::Array(&items)], 100),
        Err(Refused::Param {
            index: 1,
            why: EncodeError::ElementStorage {
                index: 1,
                expected: Storage::Integer,
                got: Storage::Real
            }
        })
    );
    assert!(connection.next_send().is_none());
    connection.advance_clock(10);
    assert_eq!(
        connection.query(QueryId(0), statement, &[one, Bound::Array(&[])], 10),
        Err(Refused::DeadlinePassed { deadline: 10, now: 10 })
    );
    // A null binds to a parameter of either kind.
    let nulls = [Bound::Value(ValueRef::Null), Bound::Value(ValueRef::Null)];
    assert_eq!(connection.query(QueryId(0), statement, &nulls, 20), Ok(()));
    assert_eq!(
        connection.query(QueryId(0), statement, &nulls, 20),
        Err(Refused::IdInFlight { id: QueryId(0) })
    );

    // Closed: queries after it are refused, and the id may be prepared again at once.
    connection.close_statement(statement).unwrap();
    assert_eq!(
        connection.query(QueryId(1), statement, &nulls, 20),
        Err(Refused::NotPrepared { statement })
    );
    assert_eq!(connection.close_statement(statement), Err(Refused::NotPrepared { statement }));
    assert_eq!(connection.prepare(statement, "select 2", &[], Expect::Count, 100), Ok(()));
    let refused = Refused::Param { index: 0, why: EncodeError::TextHasNul { at: 3 } };
    assert!(refused.to_string().starts_with("parameter $1:"));
}

#[test]
fn a_prepare_whose_description_differs_fails_and_its_statement_is_never_executed() {
    let (bad, good) = (StatementId(1), StatementId(2));
    let mut connection = ready();
    connection.prepare(bad, "select n from t", &[], Expect::Rows(&INTEGER_N), 100).unwrap();
    connection.prepare(good, "select n from u", &[], Expect::Rows(&INTEGER_N), 100).unwrap();
    sends(&mut connection);
    let mut stream = Vec::new();
    // The first: the server says the column is text.
    prepare_answer(&mut stream, &[], Some(&[("n", codec::TEXT)]));
    prepare_answer(&mut stream, &[], Some(&[("n", codec::INT8)]));
    connection.received(&stream);
    let mut seen = Vec::new();
    while let Some(event) = connection.next_event() {
        match event {
            Event::PrepareFailed { statement, error } => {
                assert!(matches!(error, Error::ColumnType { index: 0, .. }));
                seen.push(format!("{statement} failed: {error}"));
            }
            Event::Prepared { statement } => seen.push(format!("{statement} prepared")),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(
        seen,
        [
            "statement 1 failed: column 0, `n`: it expects bigint (OID 20), and the server describes text (OID 25)",
            "statement 2 prepared",
        ]
    );
    assert_eq!(
        connection.query(QueryId(1), bad, &[], 200),
        Err(Refused::StatementFailed { statement: bad })
    );
    assert!(connection.next_send().is_none());
    assert_eq!(connection.query(QueryId(2), good, &[], 200), Ok(()));
}

#[test]
fn each_difference_is_named() {
    let columns = [("a", Storage::Integer), ("b", Storage::Text)];
    let cases: [(&[u32], Option<&[(&str, u32)]>, Expect<'_>, &str); 5] = [
        (
            &[codec::TEXT],
            Some(&[("a", codec::INT8), ("b", codec::TEXT)]),
            Expect::Rows(&columns),
            "parameter $1: it is prepared as bigint (OID 20), and the server takes text (OID 25)",
        ),
        (
            &[],
            Some(&[("a", codec::INT8), ("b", codec::TEXT)]),
            Expect::Rows(&columns),
            "it is prepared with 1 parameters, and the server's description of it takes 0",
        ),
        (
            &[codec::INT8],
            Some(&[("a", codec::INT8), ("c", codec::TEXT)]),
            Expect::Rows(&columns),
            "column 1: it expects `b`, and the server names it `c`",
        ),
        (
            &[codec::INT8],
            Some(&[("a", codec::INT8)]),
            Expect::Rows(&columns),
            "it expects 2 columns, and the server describes 1",
        ),
        (
            &[codec::INT8],
            None,
            Expect::Rows(&columns),
            "it expects rows of 2 columns, and the server describes a count of changed rows",
        ),
    ];
    for (params, described, returns, want) in cases {
        let mut connection = ready();
        connection
            .prepare(
                StatementId(1),
                "select a, b from t where k = $1",
                &[ParamType::Scalar(Storage::Integer)],
                returns,
                100,
            )
            .unwrap();
        sends(&mut connection);
        let mut stream = Vec::new();
        prepare_answer(&mut stream, params, described);
        connection.received(&stream);
        let mut got = None;
        while let Some(event) = connection.next_event() {
            if let Event::PrepareFailed { error, .. } = event {
                got = Some(error.to_string());
            }
        }
        assert_eq!(got.as_deref(), Some(want));
    }
}

#[test]
fn done_waits_for_the_commit_at_ready_for_query() {
    let statement = StatementId(1);
    let mut connection = prepared(statement);
    connection.query(QueryId(1), statement, &[Bound::Value(ValueRef::Integer(3))], 100).unwrap();
    sends(&mut connection);
    let mut stream = Vec::new();
    message(&mut stream, b'2', &[]);
    data_row(&mut stream, &[Owned::Integer(4)]);
    command_complete(&mut stream, "SELECT 1");
    connection.received(&stream);
    assert_eq!(events(&mut connection), ["Row { id: QueryId(1), columns: [Integer(4)] }"]);
    let mut stream = Vec::new();
    report(&mut stream, b'E', "ERROR", "40001", "could not serialize access");
    message(&mut stream, b'Z', b"I");
    connection.received(&stream);
    let seen = events(&mut connection);
    assert_eq!(seen.len(), 1);
    assert!(seen[0].contains("Failed") && seen[0].contains("40001"), "{seen:?}");
}

#[test]
fn a_close_sends_close_and_sync_and_lets_the_statement_go() {
    let statement = StatementId(3);
    let mut connection = prepared(statement);
    connection.query(QueryId(1), statement, &[Bound::Value(ValueRef::Integer(1))], 100).unwrap();
    sends(&mut connection);
    connection.close_statement(statement).unwrap();
    let sent = frontend(&sends(&mut connection));
    assert_eq!(sent, [(b'C', b"Sblaze_3\0".to_vec()), (b'S', Vec::new())]);
    let mut stream = Vec::new();
    // The query sent before the close still runs, its rows read against the statement.
    query_answer(&mut stream, &[9]);
    message(&mut stream, b'3', &[]);
    message(&mut stream, b'Z', b"I");
    connection.received(&stream);
    assert_eq!(
        events(&mut connection),
        [
            "Row { id: QueryId(1), columns: [Integer(9)] }",
            "Done { id: QueryId(1), changed: Some(1) }"
        ]
    );
    assert_eq!(connection.next_deadline(), None);
    // An error to a close closes the connection.
    connection.prepare(statement, "select 1", &[], Expect::Count, 100).unwrap();
    connection.close_statement(statement).unwrap();
    let mut stream = Vec::new();
    prepare_answer(&mut stream, &[], None);
    report(&mut stream, b'E', "ERROR", "XX000", "could not close");
    connection.received(&stream);
    assert!(close_text(&mut connection).contains("XX000"));
}

#[test]
fn a_late_round_fails_and_so_does_every_one_behind_it() {
    let statement = StatementId(1);
    let mut connection = prepared(statement);
    let one = [Bound::Value(ValueRef::Integer(1))];
    connection.query(QueryId(1), statement, &one, 50).unwrap();
    connection.prepare(StatementId(2), "select 2", &[], Expect::Count, 40).unwrap();
    assert_eq!(connection.next_deadline(), Some(40));
    connection.advance_clock(40);
    let seen = events(&mut connection);
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert!(seen[0].contains("QueryId(1)") && seen[0].contains("Closed"), "{seen:?}");
    assert!(seen[1].contains("StatementId(2)") && seen[1].contains("Deadline"), "{seen:?}");
    assert!(seen[2].starts_with("Close"), "{seen:?}");
    assert_eq!(connection.next_deadline(), None);
    assert_eq!(connection.query(QueryId(3), statement, &one, 100), Err(Refused::Closed));
}
