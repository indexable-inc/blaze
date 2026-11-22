//! The transcriber's range copies (`rules/copy.rs`) against the oracle, in a test binary of its
//! own: the oracle and the planted bug are switches of the process, so no other test may run
//! beside this one.
//!
//! With the oracle on, every arm a call tries is matched by both matchers and transcribed from
//! each: the linear matcher's values are written as range copies, the position-set matcher's
//! token by token. The calls below take every path of the copy: a `$($t)*` over a run of token
//! trees (one copy), the same under an outer repetition, single fragments of each kind written
//! as they are (`tt`, `ident`, `ty`, `expr`), and repetitions the stack machine still runs.
//!
//! Prints, and asserts:
//! - "rules oracle: N calls: M arms equal, 0 different" with no plant;
//! - "rules oracle with CopyPlant::DropLastToken: N calls: M arms equal, D different" with D > 0.

#![feature(allocator_api)]

use blaze_expand::{
    Call, CallSite, CopyPlant, Ctx, DefAt, DefSite, Edition, ExpandCx, Expanded, Export, HostAsk,
    HostError, HostWhy, IncludeAs, Included, LocalInner, MacroDefinition, Place, PlaceOf,
    ProcCall, Produced, TokenStream, expand, lex, rules_copy_plant, rules_oracle,
    rules_oracle_counts,
};
use blaze_id::{ContentId, CrateItemKey};
use blaze_own::Alloc;

/// Nothing outside: the calls below are of `macro_rules!` alone.
struct NoCx;

const REFUSED: HostError = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Failed };

impl ExpandCx for NoCx {
    fn env(&self, _: &str) -> Result<Option<&str>, HostError> {
        Ok(None)
    }

    fn cfg(&self, _: &TokenStream<'_>) -> Result<bool, HostError> {
        Ok(false)
    }

    fn include(&self, _: &str, _: IncludeAs) -> Result<Included<'_>, HostError> {
        Ok(Included::Missing)
    }

    fn place(&self, _: PlaceOf) -> Result<Place<'_>, HostError> {
        Ok(Place::Number(1))
    }

    fn run_proc_macro<'a>(
        &self,
        _: &MacroDefinition<'_>,
        _: ProcCall<'_>,
        _: Alloc<'a>,
    ) -> Result<Produced<'a>, HostError> {
        Err(REFUSED)
    }
}

fn tokens(text: &str) -> TokenStream<'static> {
    lex(text, Edition::E2021, blaze_own::testing::heap().alloc()).expect("the test's text lexes")
}

fn rules(body: &str) -> MacroDefinition<'static> {
    let at = DefAt {
        site: DefSite { krate: CrateItemKey(ContentId([7; 32])), module: None },
        edition: Edition::E2021,
        export: Export::Local,
        local_inner: LocalInner::No,
        transparency: None,
    };
    MacroDefinition::rules(tokens(body), at)
}

/// Each definition and the calls made of it.
const CASES: &[(&str, &[&str])] = &[
    // `$($t)*` over a run of trees: one copy; joint operators, literals, groups and a lifetime.
    (
        "($($t:tt)*) => { $($t)* }",
        &[
            "a += 1; b <<= 2; f(x, y)[0] .. 'a: loop {} 1.0f32 \"s\" -x",
            "x::<u8>::y && !z || w",
            "{ a } [ b ] ( c )",
            ";",
        ],
    ),
    // The same under an outer repetition: the inner run copied once an outer iteration. Each
    // run is in brackets: a bare `;` would fit both `$b:tt` and the outer separator, a local
    // ambiguity the Reference refuses.
    (
        "($($a:ident: [$($b:tt)*]);*) => { $( fn $a() { $($b)* } )* }",
        &["f: [let x = 1 + 2;]; g: [x . y ( ) ;]; h: []", "only: [a == b]"],
    ),
    // Single fragments, each written as one run: `ident`, `ty` (also before `<`), `expr`.
    (
        "($e:expr, $i:ident, $ty:ty) => { let $i: $ty = $e; let _ = <$ty>::default(); }",
        &["1 + 2, x, Vec<u8>", "-1, y, &'static str", "f(a, b), z, [u8; 4]"],
    ),
    // A repetition with a separator and a definition token in its body: the stack machine.
    (
        "($($x:ident),* $(,)?) => { $(let $x = 1;)* }",
        &["a, b, c", "a,", ""],
    ),
];

/// Expands every case's calls; returns the oracle's counts that the calls added. With `strict`
/// each call must expand with no diagnostic; with a plant its output may be broken (a group
/// left open), so what the call gives is not looked at: the oracle's counts are.
fn run_cases(strict: bool) -> [u64; 5] {
    let before = rules_oracle_counts();
    for &(body, calls) in CASES {
        let def = rules(body);
        for &text in calls {
            let input = tokens(text);
            let site = CallSite {
                edition: Edition::E2021,
                panic_edition: Edition::E2021,
                ctx: Ctx::root(Edition::E2021),
            };
            let expanded = expand(&def, &Call::fn_like(&input, site), &NoCx, blaze_own::testing::heap().alloc());
            if !strict {
                continue;
            }
            match expanded.expect("the call expands") {
                Expanded::Expansion(exp) => {
                    assert!(exp.diagnostics.is_empty(), "`{text}` of `{body}` gave a diagnostic");
                }
                Expanded::NeedsExpanded(_) => panic!("`{text}` of `{body}` has no nested call"),
            }
        }
    }
    let after = rules_oracle_counts();
    core::array::from_fn(|at| after[at] - before[at])
}

/// The oracle sees no difference between the range copies and the token-by-token writer, and
/// sees the planted bug: "rules oracle: N calls: M arms equal, 0 different", then "rules oracle
/// with CopyPlant::DropLastToken: N calls: M arms equal, D different" with D > 0.
#[test]
fn the_oracle_sees_the_range_copy_equal_and_the_plant_different() {
    rules_oracle(true);
    rules_copy_plant(CopyPlant::Off);
    let [equal, different, exact, _, calls] = run_cases(true);
    println!("rules oracle: {calls} calls: {equal} arms equal, {different} different");
    assert!(equal > 0, "the oracle compared no arm");
    assert_eq!(different, 0, "the range copy wrote other tokens than the token-by-token writer");
    assert_eq!(exact, 0, "an arm was left to the exact mode");

    rules_copy_plant(CopyPlant::DropLastToken);
    let [equal, different, _, _, calls] = run_cases(false);
    rules_copy_plant(CopyPlant::Off);
    rules_oracle(false);
    println!(
        "rules oracle with CopyPlant::DropLastToken: {calls} calls: {equal} arms equal, {different} different"
    );
    assert!(different > 0, "the oracle missed the planted bug");
}
