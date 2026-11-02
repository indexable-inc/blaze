#!/usr/bin/env python3
"""unwinds.py: a daemon follows edits to functions that a panic unwinds through.

The unwinder has to find the frames of each edited function in the program the daemon builds.
Each step is held to what a fresh build of the same source does (includes.py's `follow`).
"""
from includes import follow

MAIN = "src/main.rs"
MAIN_TEXT = """struct Guard(&'static str);

impl Drop for Guard {
    fn drop(&mut self) {
        println!("dropped {}", self.0);
    }
}

fn inner(n: u32) -> u32 {
    let _guard = Guard("inner");
    if n > 2 {
        panic!("too many: {n}");
    }
    n + 1
}

fn outer(n: u32) -> u32 {
    let _guard = Guard("outer");
    inner(n) + inner(n + 1)
}

fn main() {
    std::panic::set_hook(Box::new(|info| println!("hook: {info}")));
    for n in 0..4 {
        match std::panic::catch_unwind(|| outer(n)) {
            Ok(sum) => println!("{n}: {sum}"),
            Err(_) => println!("{n}: caught"),
        }
    }
}
"""
# (what the step is, [(file, old, new)], whether it does not build)
STEPS = [
    ("nothing edited", [], False),
    ("the function that panics", [(MAIN, "n > 2", "n > 1")], False),
    ("the function unwound through", [(MAIN, "inner(n) + inner(n + 1)", "inner(n) * 2 + inner(n + 1)")], False),
    ("a guard added to it", [
        (MAIN, '    inner(n) * 2', '    let _second = Guard("second");\n    inner(n) * 2'),
    ], False),
    ("the drop that runs while unwinding", [(MAIN, '"dropped {}"', '"let go of {}"')], False),
    ("a function added between them", [
        (MAIN, "fn outer(n: u32) -> u32 {", 'fn middle(n: u32) -> u32 {\n    let _guard = Guard("middle");\n    inner(n)\n}\n\nfn outer(n: u32) -> u32 {'),
        (MAIN, "inner(n) * 2 + inner(n + 1)", "middle(n) * 2 + inner(n + 1)"),
    ], False),
    ("the closure that is caught", [(MAIN, "catch_unwind(|| outer(n))", "catch_unwind(|| outer(n + 1))")], False),
    ("no panic any more", [(MAIN, "n > 1", "n > 100")], False),
]

if __name__ == "__main__":
    follow([(MAIN, MAIN_TEXT)], STEPS)
