//! blaze-run's compiles and records as drive names (drive.md, "Run" and "Tests"): a version of a
//! function (instance, tier, hooks) is one `Code` name, a run's record key is a drive key, a
//! function's code identity is what a test's read list holds, and a test is one `Test` name.
//! `run` and `test` hand the entry and the units' record to blaze-run's `Parent`, whose
//! `Lookup`/`Compile`/`Publish` effects the caller turns into `Code` jobs of a request.

use blaze_id::ContentId;
use blaze_ids::Hasher;
use blaze_run::{CodeId, Hooks, InstanceKey, RecordKey, TestId, Tier};

use crate::key::{Key, Kind, Name};

const CODE_DOMAIN: &str = "blaze-drive code name 1";

/// The `Code` name of one version of a function: the record its compile makes.
pub fn code_name(function: InstanceKey, tier: Tier, hooks: Hooks) -> Name {
    let mut h = Hasher::new(CODE_DOMAIN);
    let tier = match tier {
        Tier::Fast => 0u8,
        Tier::Optimized => 1u8,
    };
    h.update(&function.0).update(&[tier]).update(&hooks.0.to_le_bytes());
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
