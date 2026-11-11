//! The key of an extension's build: every input its code depends on (rule
//! `keys-cover-every-input`).
//!
//! In it: blaze's own code identity (so a new blaze builds every extension again, and an
//! extension built against another blaze's types is never found, let alone loaded), the
//! toolchain and the host triple, the API's version, the extension's manifest and every file
//! of its source, and the capabilities granted. Not in it: where the extension's directory
//! is, the workspace that uses it, the job limits (a build is the same code under any limit).
//!
//! The same key is the extension's code identity in the key of every fact it computes
//! ([`crate::facts`]): coarser than one identity per fact function, never wrong.

use blaze_ids::ContentId;

use crate::source::Sources;

/// What this blaze is: what an extension is built by and against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The code identity of this blaze: of the build of its own executable. The daemon gives
    /// it; nothing here derives it from a version number, which two builds can share.
    pub blaze: ContentId,
    /// The pinned nightly (`harness/toolchain`).
    pub toolchain: String,
    /// The host triple the extension is compiled for: the daemon's own.
    pub target: String,
}

const DOMAIN: &str = "blaze v2 extension build 1";

/// The key of the build of `sources` with `grants` (sorted) by the blaze `identity` is.
pub fn build_key(identity: &Identity, sources: &Sources, grants: &[String]) -> ContentId {
    let mut bytes = Vec::new(); // alloc: a key's preimage, once per extension per build
    part(&mut bytes, &identity.blaze.0);
    part(&mut bytes, identity.toolchain.as_bytes());
    part(&mut bytes, identity.target.as_bytes());
    part(&mut bytes, blaze_ext::VERSION.as_bytes());
    // Metered: the code counts fuel. A build without it would be other code.
    part(&mut bytes, b"fuel");
    part(&mut bytes, sources.name.as_bytes());
    part(&mut bytes, sources.manifest.as_bytes());
    count(&mut bytes, sources.files.len());
    for (path, text) in &sources.files {
        part(&mut bytes, path.as_bytes());
        part(&mut bytes, text.as_bytes());
    }
    count(&mut bytes, grants.len());
    for grant in grants {
        part(&mut bytes, grant.as_bytes());
    }
    ContentId::of(DOMAIN, &bytes)
}

/// One part, its length first, so no two lists of parts are one string of bytes.
pub(crate) fn part(out: &mut Vec<u8>, bytes: &[u8]) {
    count(out, bytes.len());
    out.extend_from_slice(bytes);
}

pub(crate) fn count(out: &mut Vec<u8>, count: usize) {
    out.extend_from_slice(&(count as u64).to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources() -> Sources {
        Sources {
            name: "x".to_owned(),
            manifest: "[package]\nname = \"x\"\n".to_owned(),
            files: vec![("src/lib.rs".to_owned(), "#![no_std]\n".to_owned())],
        }
    }

    fn identity() -> Identity {
        Identity {
            blaze: ContentId::of("test blaze", b"1"),
            toolchain: "nightly-2026-10-27".to_owned(),
            target: "aarch64-apple-darwin".to_owned(),
        }
    }

    /// The invariant: a build's key changes with each of its inputs, and with nothing else.
    #[test]
    fn the_key_covers_blaze_the_source_and_the_grants() {
        let key = build_key(&identity(), &sources(), &[]);
        assert_eq!(key, build_key(&identity(), &sources(), &[]), "the same inputs, the same key");

        let mut other_blaze = identity();
        other_blaze.blaze = ContentId::of("test blaze", b"2");
        assert_ne!(key, build_key(&other_blaze, &sources(), &[]), "another blaze builds it again");
        let mut other_target = identity();
        other_target.target = "x86_64-unknown-linux-gnu".to_owned();
        assert_ne!(key, build_key(&other_target, &sources(), &[]));
        let mut other_toolchain = identity();
        other_toolchain.toolchain = "nightly-2026-11-01".to_owned();
        assert_ne!(key, build_key(&other_toolchain, &sources(), &[]));

        let mut edited = sources();
        edited.files[0].1.push_str("// an edit\n");
        assert_ne!(key, build_key(&identity(), &edited, &[]), "an edit of its source");
        let mut manifest = sources();
        manifest.manifest.push_str("edition = \"2024\"\n");
        assert_ne!(key, build_key(&identity(), &manifest, &[]));
        assert_ne!(key, build_key(&identity(), &sources(), &["clock".to_owned()]), "a grant");

        // A file's path and its text do not run together.
        let mut split = sources();
        split.files = vec![("src/lib.r".to_owned(), "s#![no_std]\n".to_owned())];
        assert_ne!(key, build_key(&identity(), &split, &[]));
    }

    #[test]
    fn the_oracle_catches_a_key_that_leaves_blaze_out() {
        // The planted bug: the key of a build that does not hash blaze's identity.
        let without = |identity: &Identity| {
            let mut same = identity.clone();
            same.blaze = ContentId::of("test blaze", b"left out");
            build_key(&same, &sources(), &[])
        };
        let mut other = identity();
        other.blaze = ContentId::of("test blaze", b"2");
        assert_eq!(
            without(&identity()),
            without(&other),
            "with blaze left out, two blazes share a key: what the invariant's `assert_ne` refuses"
        );
    }
}
