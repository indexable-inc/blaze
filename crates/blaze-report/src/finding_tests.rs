use super::{Cause, CauseKind, Finding, Subject, decode, encode};
use crate::frame::words::{Area, Kind, wording};

/// One cause of each kind, with arguments that are easy to find in the words.
fn sample(kind: CauseKind) -> Cause<'static> {
    match kind {
        CauseKind::Task => Cause::Task { error: "<error>" },
        CauseKind::Panicked => Cause::Panicked,
        CauseKind::StretchFilled => Cause::StretchFilled { reserve: 4096 },
        CauseKind::FrameUsedUp => Cause::FrameUsedUp { what: "<what>", reserve: 8388608 },
        CauseKind::Refused => Cause::Refused { why: "<why>" },
        CauseKind::PlanRefused => Cause::PlanRefused { why: "<why>" },
        CauseKind::Silent => Cause::Silent,
        CauseKind::NotWhole => Cause::NotWhole { why: "<why>" },
        CauseKind::RefusedCall => Cause::RefusedCall { call: "<call>" },
        CauseKind::BorrowsRefused => Cause::BorrowsRefused { why: "<why>" },
    }
}

const BODY: Subject<'static> = Subject::Item { file: "src/lib.rs", item: "target" };

fn words(finding: &Finding<'_>) -> String {
    let mut out = String::new();
    finding.words(&mut out).expect("a cause's finding has words");
    out
}

#[test]
fn every_cause_has_a_rule_and_words() {
    for (n, kind) in CauseKind::ALL.into_iter().enumerate() {
        assert_eq!(kind as usize, n, "{kind:?}: ALL is in tag order");
        let cause = sample(kind);
        assert_eq!(cause.kind(), kind);
        let row = wording(Area::Cli, kind.rule()).unwrap_or_else(|| panic!("{kind:?}: rule {} has no row", kind.rule()));
        assert!(!matches!(row.kind, Kind::Error | Kind::Warning | Kind::Told), "{kind:?}: a body's note is a note's kind, not {:?}", row.kind);
        let said = words(&Finding::of_cause(BODY, cause));
        assert!(!said.contains('{'), "{kind:?}: a placeholder left in {said:?}");
        assert!(said.contains("`src/lib.rs`") && said.contains("`target`"), "{kind:?}: the subject is said: {said:?}");
        let (args, given) = cause.args();
        for arg in &args[..given] {
            let arg = match arg {
                super::Arg::Text(text) => (*text).to_owned(),
                super::Arg::Count(n) => n.to_string(),
            };
            assert!(said.contains(&arg), "{kind:?}: its argument {arg:?} is said in {said:?}");
        }
    }
    let rules: Vec<u16> = CauseKind::ALL.into_iter().map(CauseKind::rule).collect();
    let mut distinct = rules.clone();
    distinct.dedup();
    assert_eq!(rules, distinct, "a rule for each cause");
}

/// The equality oracle of the switch (D164 step 6): each cause's words are, byte for byte, the
/// sentence blaze-cli engine_bodies.rs `note` built in place before its sentences moved here
/// (the format strings below are copied from it), and the `--each` form engine_lines.rs
/// `Under`'s Display wrote.
#[test]
fn the_table_says_what_the_old_sentences_said() {
    let (path, item) = ("src/lib.rs", "target");
    let old: [(CauseKind, String); 10] = [
        (CauseKind::Task, format!("`{path}`: the body of `{item}` was not checked: {} (engine_bodies.rs)", "<error>")),
        (CauseKind::Panicked, format!("`{path}`: the check of the body of `{item}` panicked (Panicked); that body alone failed (engine_bodies.rs)")),
        (
            CauseKind::StretchFilled,
            format!("`{path}`: the shared stretch of {} bytes filled while checking `{item}` (FrameUsedUp); the body is owed because its available space depends on the stretch (engine_bodies.rs)", 4096),
        ),
        (CauseKind::FrameUsedUp, format!("`{path}`: the body of `{item}` used up its frame (FrameUsedUp) for {}, limit {} bytes; that body alone failed (engine_bodies.rs)", "<what>", 8388608)),
        (CauseKind::Refused, format!("`{path}`: the body of `{item}` was not checked: {} (engine_bodies.rs)", "<why>")),
        // RefusedAtPlan's `--each` form, which Under's Display wrote.
        (CauseKind::PlanRefused, format!("`{path}`: body `{item}`: not checked because {}", "<why>")),
        (CauseKind::Silent, format!("`{path}`: body `{item}`: not checked whole: its verdict failed and said no cause (front2 tables/task.rs `said_of`; engine_bodies.rs)")),
        (CauseKind::NotWhole, format!("`{path}`: body `{item}`: {}", "<why>")),
        (CauseKind::RefusedCall, format!("`{path}`: body `{item}`: not checked because {}", "<call>")),
        (CauseKind::BorrowsRefused, format!("`{path}`: body `{item}`: types checked; {}", "<why>")),
    ];
    for (kind, sentence) in old {
        assert_eq!(words(&Finding::of_cause(BODY, sample(kind))), sentence, "{kind:?}");
    }
    // The planted bug: a sentence one byte off is caught.
    assert_ne!(words(&Finding::of_cause(BODY, Cause::Panicked)), format!("`{path}`: the check of the body of `{item}` panicked (Panicked); that body alone failed (engine_bodies.rs) "));
}

#[test]
fn a_stored_finding_reads_back_equal() {
    let subjects = [BODY, Subject::File { path: "src/a b.rs" }, Subject::Crate { name: "alpha" }, Subject::Item { file: "", item: "" }];
    for subject in subjects {
        for kind in CauseKind::ALL {
            let cause = sample(kind);
            let mut bytes = Vec::new();
            encode(subject, cause, &mut |piece: &[u8]| bytes.extend_from_slice(piece));
            let read = decode(&bytes).unwrap_or_else(|why| panic!("{subject:?} {kind:?}: {why}"));
            assert_eq!(read, Finding::of_cause(subject, cause));
            bytes.push(0);
            assert!(decode(&bytes).is_err(), "{subject:?} {kind:?}: a byte after the cause is refused");
            bytes.pop();
            bytes.pop();
            assert!(decode(&bytes).is_err(), "{subject:?} {kind:?}: a cut finding is refused");
        }
    }
    let mut bytes = Vec::new();
    encode(Subject::File { path: "ab" }, Cause::Silent, &mut |piece: &[u8]| bytes.extend_from_slice(piece));
    bytes[5] = 0xff;
    assert!(decode(&bytes).is_err(), "a path that is not UTF-8 is refused");
    assert!(decode(&[9]).is_err(), "a subject tag of nothing is refused");
}

#[test]
fn a_cause_key_is_its_words_then_its_kind() {
    let a = Cause::RefusedCall { call: "the call of `a`" }.key();
    let b = Cause::PlanRefused { why: "the call of `b`" }.key();
    assert!(a < b, "keys sort by their words first");
    assert_eq!(Cause::Panicked.key().of, "");
    assert_eq!(Finding::of_cause(BODY, Cause::Silent).key(), Some(Cause::Silent.key()));
}
