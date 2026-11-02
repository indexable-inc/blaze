//! The production oracle's sample (docs/v2/plan/lanes/ci.md section 10): one shared memo table means one
//! wrong result is wrong everywhere, so a sample of views is also built again from nothing
//! ([`blaze_api::Request::Verify`]) and compared. Which views is a hash of the commit, so every
//! run of the service (and a standby restored from a snapshot) samples the same ones.
//!
//! The daemon stops the results a mismatch names; the service only posts what it found as the
//! check run [`CHECK`].

use std::fmt::Write as _;

use blaze_api::Mismatch;
use blaze_ids::ContentId;

pub const DOMAIN: &str = "blaze v2 ci oracle sample 1";

/// The check run a sampled view's verification is posted as.
pub const CHECK: &str = "blaze/oracle";

/// Whether the view of `commit` is verified: one in `one_in` commits, none for 0.
pub fn sampled(commit: &str, one_in: u64) -> bool {
    if one_in == 0 {
        return false;
    }
    let id = ContentId::of(DOMAIN, commit.as_bytes());
    let mut low = [0; 8];
    low.copy_from_slice(&id.0[..8]);
    u64::from_le_bytes(low) % one_in == 0
}

/// What a verification found, for a person and an LLM: each mismatched query and the fact that
/// should have changed its edge and did not.
pub fn summary(mismatches: &[Mismatch]) -> String {
    if mismatches.is_empty() {
        // alloc: once per sampled view verified, its summary
        return "Built again from nothing, every result matched the daemon's.".to_owned();
    }
    let mut text = format!( // alloc: once per sampled view verified, its summary
        "Built again from nothing, {} result{} differed from the daemon's; the daemon stopped \
         them.\n",
        mismatches.len(),
        if mismatches.len() == 1 { "" } else { "s" }
    );
    for mismatch in mismatches {
        writeln!(
            text,
            "\n- query of kind {} on input {:x}: incremental {:?}, from nothing {:?}",
            mismatch.query.kind.0, mismatch.query.input, mismatch.incremental, mismatch.scratch
        )
        .expect("a String takes every byte");
        match &mismatch.fact {
            Some(fact) => writeln!(
                text,
                "  the fact that missed: {} (kind {} on input {:x})",
                fact.what, fact.key.kind.0, fact.key.input
            )
            .expect("a String takes every byte"),
            None => text.push_str("  the oracle found no fact whose edge missed\n"),
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use blaze_api::FactRead;
    use blaze_ids::Fingerprint;

    use super::*;

    #[test]
    fn zero_samples_nothing_and_one_samples_everything() {
        for commit in ["a", "b", "0123456789abcdef0123456789abcdef01234567"] {
            assert!(!sampled(commit, 0));
            assert!(sampled(commit, 1));
        }
    }

    #[test]
    fn the_sample_is_the_hash_of_the_commit() {
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let id = ContentId::of(DOMAIN, commit.as_bytes());
        let low = u64::from_le_bytes(id.0[..8].try_into().unwrap());
        for one_in in 1..64 {
            assert_eq!(sampled(commit, one_in), low % one_in == 0);
            assert_eq!(sampled(commit, one_in), sampled(commit, one_in));
        }
    }

    #[test]
    fn about_one_in_n_is_sampled() {
        let hits = (0..10_000).filter(|n| sampled(&format!("{n:040x}"), 10)).count();
        assert!((800..1200).contains(&hits), "{hits}");
    }

    #[test]
    fn a_mismatch_names_its_query_and_fact() {
        let key = |kind, byte| blaze_query::QueryKey {
            kind: blaze_query::QueryKind(kind),
            input: ContentId([byte; 32]),
        };
        let mismatches = [
            Mismatch {
                query: key(7, 1),
                incremental: Fingerprint(1),
                scratch: Fingerprint(2),
                fact: Some(FactRead {
                    key: key(9, 2),
                    what: "the signature of `parser::Token::new`".to_owned(),
                }),
            },
            Mismatch {
                query: key(8, 3),
                incremental: Fingerprint(3),
                scratch: Fingerprint(4),
                fact: None,
            },
        ];
        let text = summary(&mismatches);
        assert!(text.contains("2 results differed"), "{text}");
        assert!(text.contains("kind 7"), "{text}");
        assert!(text.contains(&format!("{:x}", ContentId([1; 32]))), "{text}");
        assert!(text.contains("the signature of `parser::Token::new`"), "{text}");
        assert!(text.contains("no fact"), "{text}");
        assert!(summary(&[]).contains("matched"));
    }
}
