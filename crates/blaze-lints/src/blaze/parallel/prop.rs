//! The differential property of `parallelizable`: a loop gives the same result in every order
//! of its iterations iff the lint finds it.
//!
//! The loops are of a small language ([`Body`]) over `xs: &[u64; 8]` and `out: &mut [u64; 8]`:
//! reductions into accumulators of their own, one write of `out` (at the iteration's own index,
//! or from the next one's), a carried hash, a push. Each is rendered to Rust and linted, and
//! run here, by the language's own meaning, in random orders of `0..8` over random inputs.
//! Inputs stay below 16 so that no order overflows.
//!
//! Outside the language, on purpose: an accumulator reduced twice (`a += x; a += x`), which
//! blaze reads as a carried value though any order gives the same; and a write of `out`
//! another write overwrites, whose read is dead. Both are misses the notes name.

use hegel::generators::{self as gs, Generator as _};
use hegel::TestCase;

use super::tests::{FLAGS, MAIN, MINICORE, STD, conf, found};
use crate::tests::lint_with;

const N: usize = 8;

#[derive(Clone, Copy, Debug)]
enum Reduce {
    Add,
    Mul,
    Or,
    And,
    Xor,
    Min,
    Max,
}

impl Reduce {
    fn init(self) -> u64 {
        match self {
            Reduce::Add | Reduce::Or | Reduce::Xor | Reduce::Max => 0,
            Reduce::Mul => 1,
            Reduce::And | Reduce::Min => !0,
        }
    }

    fn apply(self, acc: u64, x: u64) -> u64 {
        match self {
            Reduce::Add => acc + x,
            Reduce::Mul => acc * x,
            Reduce::Or => acc | x,
            Reduce::And => acc & x,
            Reduce::Xor => acc ^ x,
            Reduce::Min => acc.min(x),
            Reduce::Max => acc.max(x),
        }
    }

    /// The statement that reduces `x` into the accumulator `acc`.
    fn written(self, acc: &str) -> String {
        match self {
            Reduce::Add => format!("{acc} += x;"),
            Reduce::Mul => format!("{acc} *= x;"),
            Reduce::Or => format!("{acc} |= x;"),
            Reduce::And => format!("{acc} &= x;"),
            Reduce::Xor => format!("{acc} ^= x;"),
            Reduce::Min => format!("{acc} = std::cmp::min({acc}, x);"),
            Reduce::Max => format!("{acc} = std::cmp::max({acc}, x);"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Write {
    /// `out[i] = x * c`.
    Own(u64),
    /// `out[i] = out[(i + 1) % 8]`.
    Next,
}

/// One loop's body.
#[derive(Clone, Debug)]
struct Body {
    reduce: Vec<Reduce>,
    write: Option<Write>,
    /// `hash = hash * 31 + x`.
    carry: bool,
    /// `pushed.push(x)`.
    push: bool,
}

/// What a run of the loop leaves.
#[derive(Debug, PartialEq, Eq)]
struct State {
    accs: Vec<u64>,
    out: [u64; N],
    hash: u64,
    pushed: Vec<u64>,
}

impl Body {
    /// The crate `main` with the loop as the function `run`, and the line its `for` is on.
    fn rendered(&self) -> (String, u32) {
        let mut lines = vec!["pub fn run(xs: &[u64; 8], out: &mut [u64; 8]) -> u64 {".to_owned()];
        for (k, reduce) in self.reduce.iter().enumerate() {
            lines.push(format!("    let mut a{k}: u64 = {};", reduce.init()));
        }
        lines.push("    let mut hash: u64 = 0;".to_owned());
        if self.push {
            lines.push("    let mut pushed = Vec::new();".to_owned());
        }
        lines.push("    for i in 0..8 {".to_owned());
        let at = lines.len() as u32;
        lines.push("        let x = xs[i];".to_owned());
        for (k, reduce) in self.reduce.iter().enumerate() {
            lines.push(format!("        {}", reduce.written(&format!("a{k}"))));
        }
        match self.write {
            Some(Write::Own(c)) => lines.push(format!("        out[i] = x * {c};")),
            Some(Write::Next) => lines.push("        out[i] = out[(i + 1) % 8];".to_owned()),
            None => {}
        }
        if self.carry {
            lines.push("        hash = hash * 31 + x;".to_owned());
        }
        if self.push {
            lines.push("        pushed.push(x);".to_owned());
        }
        lines.push("    }".to_owned());
        let accs: String = (0..self.reduce.len()).map(|k| format!(" ^ a{k}")).collect();
        lines.push(format!("    hash{accs}"));
        lines.push("}".to_owned());
        (lines.join("\n") + "\n", at)
    }

    /// The loop run over `order`.
    fn run(&self, xs: &[u64; N], out: [u64; N], order: &[usize]) -> State {
        let mut state = State {
            accs: self.reduce.iter().map(|it| it.init()).collect(),
            out,
            hash: 0,
            pushed: Vec::new(),
        };
        for &i in order {
            let x = xs[i];
            for (acc, reduce) in state.accs.iter_mut().zip(&self.reduce) {
                *acc = reduce.apply(*acc, x);
            }
            match self.write {
                Some(Write::Own(c)) => state.out[i] = x * c,
                Some(Write::Next) => state.out[i] = state.out[(i + 1) % N],
                None => {}
            }
            if self.carry {
                state.hash = state.hash * 31 + x;
            }
            if self.push {
                state.pushed.push(x);
            }
        }
        state
    }

    /// Whether every order sampled gives what the order written gives, over inputs sampled
    /// from `seed`.
    fn order_free(&self, seed: u64) -> bool {
        let mut rng = Rng(seed | 1);
        let written: Vec<usize> = (0..N).collect();
        for _ in 0..16 {
            let xs: [u64; N] = std::array::from_fn(|_| rng.below(16));
            let out: [u64; N] = std::array::from_fn(|_| 100 + rng.below(16));
            let expected = self.run(&xs, out, &written);
            for _ in 0..16 {
                let mut order = written.clone();
                for at in (1..N).rev() {
                    order.swap(at, rng.below(at as u64 + 1) as usize);
                }
                if self.run(&xs, out, &order) != expected {
                    return false;
                }
            }
        }
        true
    }

    /// Whether `parallelizable` finds the loop.
    fn found(&self) -> bool {
        let (main, at) = self.rendered();
        let fixture = format!("{MINICORE}{STD}{MAIN}{main}");
        let out = lint_with(&fixture, FLAGS, conf("parallel-threshold = 1\n"));
        found(&out).contains(&(at, "parallelizable"))
    }
}

/// xorshift64: inputs and orders from a seed Hegel shrinks.
struct Rng(u64);

impl Rng {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % bound
    }
}

fn draw_body(tc: &TestCase) -> Body {
    let reduces = [Reduce::Add, Reduce::Mul, Reduce::Or, Reduce::And, Reduce::Xor, Reduce::Min, Reduce::Max];
    let reduce = tc.draw(gs::vecs(gs::sampled_from(reduces.to_vec())).max_size(3));
    let write = tc.draw(gs::optional(hegel::one_of!(
        gs::integers::<u64>().min_value(1).max_value(15).map(Write::Own),
        gs::just(Write::Next),
    )));
    let carry = tc.draw(gs::booleans());
    let push = tc.draw(gs::booleans());
    Body { reduce, write, carry, push }
}

#[hegel::test(test_cases = 64)]
#[ignore = "a front end per case: run once compiling is allowed (lane lints-parallel)"]
fn any_order_gives_the_same_iff_the_loop_is_found(tc: TestCase) {
    let body = draw_body(&tc);
    let seed: u64 = tc.draw(gs::integers());
    assert_eq!(body.found(), body.order_free(seed), "{}", body.rendered().0);
}
