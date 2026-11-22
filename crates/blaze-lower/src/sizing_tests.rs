//! The machine's stacks sized once per parse (lane RP4, floor C3): [`crate::feed::Sizes`]
//! counts a parse's values and nesting from its event words before the first step, and no
//! step of the parse grows the value or frame stack by reallocation after.

use blaze_own::{Alloc, Erased, Inline};

use crate::expand_tests::Unix;
use crate::feed::{self, Sizes};
use crate::machine::Machine;
use crate::{Edition, Mode, NoModule, parser_edition};

/// The bytes a test's machines take, on its own stack: a named buffer, no heap. A bump that
/// takes back only its newest block, so it is sized for every parse's machines at once.
const BUFFER: usize = 256 << 10;

/// Signatures (an outline: bodies lazy) and bodies (a full parse), with deep nesting, float
/// splits (`x.0.1`, `y.0.`), closures, generics, where clauses and attributes.
const SIGNATURES: &[&str] = &[
    "fn f<T: Clone + Into<u8>, const N: usize>(a: &[T; N], b: impl Fn(u8) -> [u8; { 1 + 2 }]) -> Option<Vec<T>> where T: Copy { a.len() }",
    "#[cfg(unix)] #[doc = \"{\"] pub(crate) unsafe extern \"C\" fn g(x: *const u8, ...) -> i32 { 0 }",
    "impl<T, const N: usize> Trait for [T; { N + 1 }] where T: Default { const C: u8 = 3; fn h(&self) -> u8 { Self::C } }",
    "trait Tr { const X: [u8; { 2 * 3 }]; fn d(&self) -> u8 { 1 } fn e(); }",
    "struct S<'a, T: ?Sized + 'a> { a: &'a T, b: (u8, (u16, (u32, (u64, [u8; 4])))) }",
];

const BODIES: &[&str] = &[
    "fn f(x: (u8, (u8, u8))) -> u8 { let y = x.1.0; let z = (x.0, x.1.1); y + z.0 + (((((1))))) }",
    "fn g(v: &[u8]) -> usize { v.iter().map(|a| { let b = *a as usize; if b > 1 { b * 2 } else { match b { 0 => 0, n => n } } }).sum() }",
    "fn h() { let t = ((1, 2), 3); let _ = t.0.; for i in 0..3 { while i > 4 { loop { break; } } } }",
];

/// The capacities a machine's stacks have once sized for `output` and before any step.
fn sized(text: &str, mode: Mode, sizes: Sizes, alloc: Alloc<'_>) -> (usize, usize) {
    let (mut cfg, mut module) = (Unix, NoModule);
    let values: Option<&mut dyn crate::ValueNames> = match mode {
        Mode::Body => Some(&mut module),
        _ => None,
    };
    let mut m = Machine::new(text, mode, Edition::E2024, &mut cfg, values, alloc);
    m.size_for(sizes);
    m.stack_capacities()
}

/// Expected line: `test sizing_tests::a_parse_grows_no_stack_after_its_sizing ... ok`,
/// printing `MACHINE SIZING: N parses, M steps, reallocations after sizing 0 (floor 0)`.
#[test]
fn a_parse_grows_no_stack_after_its_sizing() {
    let buffer: Inline<BUFFER> = Inline::new();
    let erased = Erased::new(&buffer);
    let alloc = erased.handle();
    let pe = parser_edition(Edition::E2024);
    let (mut parses, mut steps, mut grown) = (0usize, 0usize, 0usize);
    let all = SIGNATURES.iter().map(|t| (*t, Mode::Signature)).chain(BODIES.iter().map(|t| (*t, Mode::Body)));
    for (text, mode) in all {
        let lexed = parser::LexedStr::new(pe, text);
        let input = lexed.to_input(pe);
        let output = match mode {
            Mode::Signature => parser::Reparser::top_level_item().parse_outline(input.tokens()),
            _ => parser::Reparser::top_level_item().parse(input.tokens()),
        };
        let sizes = Sizes::of(&output);
        let before = sized(text, mode, sizes, alloc);
        let (mut cfg, mut module) = (Unix, NoModule);
        let values: Option<&mut dyn crate::ValueNames> = match mode {
            Mode::Body => Some(&mut module),
            _ => None,
        };
        let mut m = Machine::new(text, mode, Edition::E2024, &mut cfg, values, alloc);
        feed::feed(&mut m, &lexed, &output, 0);
        let after = m.stack_capacities();
        parses += 1;
        steps += output.steps().count();
        if after != before {
            grown += 1;
            std::println!("MACHINE SIZING: {text:?} grew from {before:?} to {after:?} (sizes {sizes:?})");
        }
        assert!(sizes.vals > 0 && sizes.depth > 0, "no sizes for {text:?}");
    }
    std::println!("MACHINE SIZING: {parses} parses, {steps} steps, reallocations after sizing {grown} (floor 0)");
    assert_eq!(grown, 0, "a stack grew after its parse sized it");
}

/// The control (rule 23): sized for a parse one level shallower and with half its values, the
/// stacks do grow, so the test above sees a sizing that falls short.
#[test]
fn a_short_sizing_is_seen() {
    let buffer: Inline<BUFFER> = Inline::new();
    let erased = Erased::new(&buffer);
    let alloc = erased.handle();
    let pe = parser_edition(Edition::E2024);
    let text = BODIES[0];
    let lexed = parser::LexedStr::new(pe, text);
    let input = lexed.to_input(pe);
    let output = parser::Reparser::top_level_item().parse(input.tokens());
    let full = Sizes::of(&output);
    let short = Sizes { vals: full.vals / 4, depth: full.depth / 4 };
    let (mut cfg, mut module) = (Unix, NoModule);
    let mut m = Machine::new(text, Mode::Body, Edition::E2024, &mut cfg, Some(&mut module), alloc);
    m.size_for(short);
    let before = m.stack_capacities();
    // The feed sizes again for the parse; here the steps go in through `feed_steps`, which
    // does not, as a short sizing would leave them.
    feed::feed_steps(&mut m, &lexed, output.steps(), 0);
    assert_ne!(m.stack_capacities(), before, "a sizing a quarter of the parse's did not grow");
}
