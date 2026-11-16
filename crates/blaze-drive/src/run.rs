//! blaze-run's compiles and records as drive names (drive.md, "Run" and "Tests"): a version of a
//! function (instance, tier, hooks) is one `Code` name, a run's record key is a drive key, a
//! function's code identity is what a test's read list holds, and a test is one `Test` name.
//! `run` and `test` hand the entry and the units' record to blaze-run's `Parent`, whose
//! `Lookup`/`Compile`/`Publish` effects the caller turns into `Code` jobs of a request.

use blaze_id::ContentId;
use blaze_ids::Hasher;
use blaze_run::{CodeId, InstanceKey, RecordKey, TestId, Tier};

use crate::key::{Key, Kind, Name};

const CODE_DOMAIN: &str = "blaze-drive code name 2";

/// The `Code` name of one version of a function: the record its compile makes. A viewer's
/// hooks are no part of it: a hook is a stub in front of the code (blaze-run `Hooks`: "never
/// code compiled into it"; `Effect::Compile` carries none), so the record's bytes are the same
/// under any hooks and a name that held them would be two keys for equal bytes (rule 21). The
/// function takes no hooks, so no caller can put them in.
pub fn code_name(function: InstanceKey, tier: Tier) -> Name {
    let mut h = Hasher::new(CODE_DOMAIN);
    let tier = match tier {
        Tier::Fast => 0u8,
        Tier::Optimized => 1u8,
    };
    h.update(&function.0).update(&[tier]);
    Name::new(Kind::Code, h.finish())
}

/// The drive key a run's record key is: the same 32 bytes, one identity in both crates.
pub fn record_key(key: RecordKey) -> Key {
    Key(ContentId(key.0))
}

pub fn run_record_key(key: Key) -> RecordKey {
    RecordKey(key.0.0)
}

/// What a test's read list holds for a function it entered: its code identity's digest. A
/// changed body changes its code identity, so a test whose list holds the old one is run again
/// and every other is reported reused (`blaze test`, drive.md "Tests").
pub fn code_read(id: CodeId) -> (Name, ContentId) {
    (Name::new(Kind::Code, ContentId(id.0)), ContentId(id.0))
}

/// The `Test` name of one test.
pub fn test_name(test: TestId) -> Name {
    Name::new(Kind::Test, ContentId(test.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A code record's name is its instance and its tier, nothing else: two instances at two
    /// tiers are four names, and one (instance, tier) is one name (hooks are no input: the
    /// function takes none).
    #[test]
    fn a_code_name_is_its_instance_and_tier() {
        let (a, b) = (InstanceKey([1; 32]), InstanceKey([2; 32]));
        let names = [code_name(a, Tier::Fast), code_name(a, Tier::Optimized), code_name(b, Tier::Fast), code_name(b, Tier::Optimized)];
        for (at, name) in names.iter().enumerate() {
            assert!(names[..at].iter().all(|other| other != name), "two versions under one name: {names:?}");
        }
        assert_eq!(code_name(a, Tier::Fast), names[0]);
    }
}
