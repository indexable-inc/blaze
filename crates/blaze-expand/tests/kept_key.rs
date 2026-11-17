//! The key an expansion is kept under holds the expander's code identity (decision D105,
//! docs/v2/plan/design/producer-id.md): a call's key says what is asked, `kept_key` adds who
//! answers. The causal test (rule 23): the expander's id moved, the kept key moves; every other
//! producer moved, it stays; the call's own key never moves with a producer.
//!
//! Prints "kept key: the expander's id moves it, the call's key does not hold it".

use blaze_expand::{Call, CallSite, Ctx, DefAt, DefSite, Edition, Export, LocalInner, MacroDefinition, TokenStream, call_key, kept_key, lex};
use blaze_id::{ContentId, CrateItemKey};
use blaze_ids::producer::{Expander, Producers, Row};

fn at() -> DefAt {
    DefAt {
        site: DefSite { krate: CrateItemKey(ContentId([1; 32])), module: None },
        edition: Edition::E2021,
        export: Export::Local,
        local_inner: LocalInner::No,
        transparency: None,
    }
}

fn tokens(text: &str) -> TokenStream<'static> {
    lex(text, Edition::E2021, blaze_own::testing::heap().alloc()).expect("the test's text lexes")
}

#[test]
fn the_expanders_id_moves_the_kept_key_alone() {
    let def = MacroDefinition::rules(tokens("($name:ident) => { pub struct $name; };"), at());
    let site = CallSite { edition: Edition::E2021, panic_edition: Edition::E2021, ctx: Ctx::root(Edition::E2021) };
    let input = tokens("Kept");
    let call = call_key(&def, &Call::fn_like(&input, site));

    let today = Producers::for_tests(b"test");
    let kept = kept_key(today.get::<Expander>(), call);
    assert_ne!(kept, call, "the kept key holds more than the call's key");
    assert_eq!(kept, kept_key(Producers::for_tests(b"test").get::<Expander>(), call), "one expander, one key");

    for row in Row::ALL {
        let moved = today.planted_for_tests(row, b"older");
        let again = kept_key(moved.get::<Expander>(), call);
        assert_eq!(again != kept, row == Row::Expander, "{row:?} moved");
    }
    println!("kept key: the expander's id moves it, the call's key does not hold it");
}
