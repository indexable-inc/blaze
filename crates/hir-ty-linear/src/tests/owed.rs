//! Owed values: a value that stands for something still to be handed over (store puts a
//! session already names, a claim that must be answered) is `#[blaze::linear]`, and every way
//! a body can lose one is an error naming the value, where it was made, the exit that drops it
//! and what consumes it. The fixtures are the ways the bug class happened or could: an early
//! `return` (`store()` dropping `body_puts`), a `?`, one arm of a `match`, a struct the value
//! was moved into, a shadowed binding, `mem::drop`, `mem::forget` and `let _ =`; and the
//! control, the same values consumed, which get nothing.
//!
//! Written by claude-opus-5-5.

use super::{check, diagnostics, expect};

/// `OwedPuts` in miniature: no `Drop` impl, so no exit abandons it (rule 6 of
/// docs/v2/plan/design/linear.md): a `?` with one live is an error too.
const OWED: &str = "
#[blaze::linear]
struct Owed { puts: u32 }

impl Owed {
    #[blaze::consumes]
    fn hand_on(self) -> u32 { self.puts }
    #[blaze::consumes]
    fn discard(self, _why: &'static str) {}
}

struct Carry { owed: Owed, n: u32 }

fn make() -> Owed { Owed { puts: 1 } }
fn step() -> Result<u32, u8> { Ok(0) }
";

/// What the error's note says consumes an `Owed`, read off its impls.
const BY: &str = "a `Owed` is consumed by `Owed::hand_on` or `Owed::discard`";

#[test]
fn an_owed_value_dropped_on_an_early_return_is_an_error_naming_what_consumes_it() {
    let src = format!(
        "{OWED}
fn f(c: bool) -> u32 {{
    let owed = make();
    if c {{
        return 0;
    }}
    owed.hand_on()
}}
"
    );
    expect(&check(&src), &[(&["`owed`", "`Owed`", "`return` leaves early"], "return 0")]);
    let found = diagnostics(&src);
    let (error, _) = &found[0];
    assert!(error.notes.iter().any(|it| it.contains(BY)), "{:#?}", error.notes);
    assert!(
        error.labels.iter().any(|(_, it)| it == "`owed` is declared here"),
        "the error names where the value was made: {:#?}",
        error.labels
    );
}

#[test]
fn an_owed_value_dropped_by_a_question_mark_is_an_error() {
    let found = check(&format!(
        "{OWED}
fn f() -> Result<u32, u8> {{
    let owed = make();
    let n = step()?;
    Ok(owed.hand_on() + n)
}}
"
    ));
    expect(&found, &[(&["`owed`", "`Owed`", "`?` returns early", "before the `?`"], "step()?")]);
}

#[test]
fn an_owed_value_dropped_in_one_match_arm_is_an_error() {
    let found = check(&format!(
        "{OWED}
fn f(n: Option<u32>) -> u32 {{
    let owed = make();
    match n {{
        Some(n) => owed.hand_on() + n,
        None => 0,
    }}
}}
"
    ));
    expect(&found, &[(&["`owed`", "on a path that does not move it"], "}")]);
}

#[test]
fn an_owed_value_moved_into_a_struct_that_is_dropped_is_an_error() {
    // `Carry` is not marked: it holds an `Owed` by value, so dropping it drops the `Owed`.
    let found = check(&format!(
        "{OWED}
fn f() -> u32 {{
    let carry = Carry {{ owed: make(), n: 1 }};
    carry.n
}}
"
    ));
    expect(&found, &[(&["`carry.owed`", "`Owed`", "at the end of its scope"], "}")]);
}

#[test]
fn a_shadowed_owed_value_is_an_error() {
    let found = check(&format!(
        "{OWED}
fn f() -> u32 {{
    let owed = make();
    let owed = make();
    owed.hand_on()
}}
"
    ));
    expect(&found, &[(&["`owed`", "at the end of its scope"], "}")]);
}

#[test]
fn an_owed_value_given_to_drop_or_forget_is_an_error_of_its_own() {
    let found = diagnostics(&format!(
        "{OWED}
fn dropped() {{
    drop(make());
}}
fn forgotten() {{
    core::mem::forget(make());
}}
"
    ));
    assert_eq!(found.len(), 2, "{found:#?}");
    for (name, verb) in [("drop", "drops"), ("forget", "leaks")] {
        let words = [
            format!("`mem::{name}`"),
            format!("which {verb} it without consuming it"),
            "`Owed`".to_owned(),
        ];
        let error = found
            .iter()
            .map(|(error, _)| error)
            .find(|error| words.iter().all(|word| error.message.contains(word.as_str())))
            .unwrap_or_else(|| panic!("no error says {words:?}: {found:#?}"));
        assert!(error.notes.iter().any(|it| it.contains(BY)), "{:#?}", error.notes);
    }
}

#[test]
fn an_owed_value_bound_to_an_underscore_is_still_dropped() {
    // `let _ = owed` moves nothing: `owed` is dropped where its scope ends. `let _ = make()`
    // drops the value the call made at once.
    let found = check(&format!(
        "{OWED}
fn bound() {{
    let owed = make();
    let _ = owed;
}}
fn made() {{
    let _ = make();
}}
"
    ));
    assert_eq!(found.len(), 2, "{found:#?}");
    let bound = found.iter().find(|(message, _)| message.contains("`owed`"));
    let Some((message, at)) = bound else { panic!("`owed` is not named: {found:#?}") };
    assert!(message.contains("at the end of its scope"), "{message}");
    assert_eq!(at, "}", "{message}");
    let made = found.iter().find(|(message, _)| !message.contains("`owed`"));
    let Some((message, _)) = made else { panic!("the call's value is not named: {found:#?}") };
    for word in ["`Owed`", "without being consumed"] {
        assert!(message.contains(word), "`{word}` is not in: {message}");
    }
}

/// The control: the same values, consumed on every path, get no error. Consumed means handed
/// on (`hand_on`), discarded with a reason, returned, moved out of the struct that carries it,
/// or moved into a field of a value that is returned.
#[test]
fn an_owed_value_consumed_on_every_path_gets_no_error() {
    let found = check(&format!(
        "{OWED}
fn handed() -> u32 {{ make().hand_on() }}
fn branches(c: bool) -> u32 {{
    let owed = make();
    if c {{
        owed.discard(\"the test gives it up\");
        0
    }} else {{
        owed.hand_on()
    }}
}}
fn after_the_question_mark() -> Result<u32, u8> {{
    let n = step()?;
    let owed = make();
    Ok(owed.hand_on() + n)
}}
fn returned() -> Owed {{
    let owed = make();
    owed
}}
fn carried_out() -> u32 {{
    let carry = Carry {{ owed: make(), n: 1 }};
    carry.owed.hand_on() + carry.n
}}
fn carried_back() -> Carry {{ Carry {{ owed: make(), n: 0 }} }}
fn every_arm(n: Option<u32>) -> u32 {{
    let owed = make();
    match n {{
        Some(n) => owed.hand_on() + n,
        None => {{
            owed.discard(\"nothing to put\");
            0
        }}
    }}
}}
fn shadowed_and_moved(owed: Owed) -> u32 {{
    let owed = owed;
    owed.hand_on()
}}
"
    ));
    expect(&found, &[]);
}
