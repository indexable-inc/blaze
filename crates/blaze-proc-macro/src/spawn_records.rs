//! Spawn answers are records (READINESS.md 1a, S4; rule 77: a spawn is never run twice for one
//! key). [`Recorded`] wraps an executor: it looks the answer up by the request's key in the
//! build scripts' records (`spawn/<64 hex of the key>`, the answer's own encoding, layout word
//! `bzspawn1`, sealed as every record of the store is), returns it decoded when present, and
//! else asks the executor it wraps and keeps the encoded answer, written once. A record of the
//! key already holding other bytes is refused naming the key (rule 21): two answers under one
//! key mean the key misses an input, and the fix is the key.

use blaze_own::Alloc;
use blaze_spawn::{Executor, Files, Seen, SpawnAnswer, SpawnRefused, SpawnRequest, Written};

use crate::record::DirStore;
use crate::spawn_route::hex;

/// `inner` with its answers kept in `records` by request key.
pub struct Recorded<'s, E> {
    pub inner: E,
    pub records: &'s DirStore,
}

impl<E: Executor> Executor for Recorded<'_, E> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn answer<'a>(&self, request: &SpawnRequest<'_>, files: &dyn Files, alloc: Alloc<'a>) -> Result<SpawnAnswer<'a>, SpawnRefused<'a>> {
        let key = request.key(alloc);
        let program = request.program.name.as_str();
        let refused = |what: String| SpawnRefused::Refused {
            program: blaze_own::String::from_str_in(program, alloc),
            what: blaze_own::String::from_str_in(&what, alloc),
        };
        match self.records.get_spawn(&key) {
            Ok(Some(bytes)) => {
                return match SpawnAnswer::decode(&bytes, alloc) {
                    Ok(decoded) => Ok(rehome(&decoded, alloc)),
                    Err(why) => Err(refused(format!("the spawn record {} is not an answer: {why:?}", hex(&key.0)))),
                };
            }
            Ok(None) => {}
            Err(error) => return Err(refused(format!("the spawn record {}: {error}", hex(&key.0)))),
        }
        let answer = self.inner.answer(request, files, alloc)?;
        let mut encoded = blaze_own::Vec::new_in(alloc);
        answer.encode(&mut encoded);
        self.records
            .put_spawn(&key, encoded.as_slice())
            .map_err(|error| refused(format!("the spawn record {}: {error}", hex(&key.0))))?;
        Ok(answer)
    }
}

/// `answer` copied into `alloc`. `SpawnAnswer::decode` ties its answer to the bytes it read
/// (`decode<'b>(bytes: &'b [u8], alloc: Alloc<'b>)`), and the record's bytes are this call's
/// own, so the answer handed back is made again in the caller's allocator; the copy is of an
/// answer already proven canonical by `decode`, so it is built field by field, not re-sorted.
fn rehome<'a>(answer: &SpawnAnswer<'_>, alloc: Alloc<'a>) -> SpawnAnswer<'a> {
    let bytes = |from: &[u8]| {
        let mut out = blaze_own::Vec::new_in(alloc);
        out.extend_from_slice(from);
        out
    };
    let text = |from: &str| blaze_own::String::from_str_in(from, alloc);
    let mut written = blaze_own::Vec::new_in(alloc);
    for file in answer.written.iter() {
        written.push(Written { path: text(file.path.as_str()), bytes: file.bytes.as_ref().map(|held| bytes(held.as_slice())) });
    }
    let mut seen = blaze_own::Vec::new_in(alloc);
    for row in answer.seen.iter() {
        seen.push(match row {
            Seen::Read { path, digest } => Seen::Read { path: text(path.as_str()), digest: *digest },
            Seen::Miss { path } => Seen::Miss { path: text(path.as_str()) },
            Seen::List { path, digest } => Seen::List { path: text(path.as_str()), digest: *digest },
        });
    }
    SpawnAnswer { status: answer.status, stdout: bytes(answer.stdout.as_slice()), stderr: bytes(answer.stderr.as_slice()), written, seen }
}
