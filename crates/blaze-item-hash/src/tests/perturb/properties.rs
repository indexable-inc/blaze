//! The perturbations as properties over random programs: `f`'s identity survives any layout,
//! any `//` or `/* */` comments, any order of the items around it and any edit of their
//! literals, and changes when any literal of its own does.

use hegel::{Hegel, Settings, TestCase};
use hegel::generators as gs;

use super::{differ, same, with_dep};

#[derive(Clone, Debug)]
enum Tok {
    Lit(u64),
    Word(String),
}

fn word(text: impl Into<String>) -> Tok {
    Tok::Word(text.into())
}

/// One operand: a literal, an earlier local, a helper's result or the dependency's `g`.
fn operand(tc: &TestCase, locals: usize, helpers: usize, out: &mut Vec<Tok>) {
    match tc.draw(gs::integers::<u8>().max_value(3)) {
        1 if locals > 0 => {
            let at: usize = tc.draw(gs::integers().max_value(locals - 1));
            out.push(word(format!("x{at}")));
        }
        2 if helpers > 0 => {
            let at: usize = tc.draw(gs::integers().max_value(helpers - 1));
            out.extend([word(format!("h{at}")), word("("), word(")")]);
        }
        3 => out.extend(["(", "dep", "::", "g", "(", ")", "as", "u64", ")"].map(word)),
        _ => out.push(Tok::Lit(tc.draw(gs::integers().max_value(99)))),
    }
}

/// Operands joined by operators that cannot overflow on these sizes.
fn expr(tc: &TestCase, locals: usize, helpers: usize, out: &mut Vec<Tok>) {
    operand(tc, locals, helpers, out);
    for _ in 0..tc.draw(gs::integers::<u8>().max_value(2)) {
        out.push(tc.draw(gs::sampled_from(vec!["+", "^", "|", "&"])).into());
        operand(tc, locals, helpers, out);
    }
}

impl From<&str> for Tok {
    fn from(text: &str) -> Tok {
        word(text)
    }
}

/// The items of `main`, each as its tokens: `f` first, then helpers, structs and constants.
fn program(tc: &TestCase) -> Vec<Vec<Tok>> {
    let helpers: usize = tc.draw(gs::integers().max_value(3));
    let mut f: Vec<Tok> = ["pub", "fn", "f", "(", ")", "->", "u64", "{"].map(word).to_vec();
    let locals: usize = tc.draw(gs::integers().max_value(5));
    for at in 0..locals {
        f.extend([word("let"), word(format!("x{at}")), word("=")]);
        expr(tc, at, helpers, &mut f);
        f.push(word(";"));
    }
    expr(tc, locals, helpers, &mut f);
    f.push(word("}"));
    let mut items = vec![f];
    for at in 0..helpers {
        let lit = Tok::Lit(tc.draw(gs::integers().max_value(99)));
        let mut h = vec![word("pub"), word("fn"), word(format!("h{at}"))];
        h.extend([word("("), word(")"), word("->"), word("u64"), word("{"), lit, word("}")]);
        items.push(h);
    }
    for at in 0..tc.draw(gs::integers::<usize>().max_value(2)) {
        items.push(vec![word("pub"), word("struct"), word(format!("S{at}")), word(";")]);
    }
    for at in 0..tc.draw(gs::integers::<usize>().max_value(2)) {
        let lit = Tok::Lit(tc.draw(gs::integers().max_value(99)));
        let mut c = vec![word("pub"), word("const"), word(format!("C{at}"))];
        c.extend([word(":"), word("u64"), word("="), lit, word(";")]);
        items.push(c);
    }
    items
}

fn text(tok: &Tok) -> String {
    match tok {
        Tok::Lit(n) => format!("{n}u64"),
        Tok::Word(it) => it.clone(),
    }
}

/// `items` in `order`, a space between tokens.
fn plain(items: &[Vec<Tok>], order: &[usize]) -> String {
    let items: Vec<String> =
        order.iter().map(|&at| items[at].iter().map(text).collect::<Vec<_>>().join(" ")).collect();
    with_dep(&items.join("\n"))
}

/// `items` in `order`, each gap between tokens and items drawn: spaces, line breaks, comments.
fn laid_out(tc: &TestCase, items: &[Vec<Tok>], order: &[usize]) -> String {
    let gaps = vec![" ", "  ", "\n", "\n    ", "\t", " /* c */ ", "/**/", " // c\n", "\n// c\n"];
    let mut out = String::new();
    for &at in order {
        for tok in &items[at] {
            out.push_str(&text(tok));
            out.push_str(tc.draw(gs::sampled_from(gaps.clone())));
        }
        out.push('\n');
    }
    with_dep(&out)
}

/// The literals of `item`, as places in it.
fn literals(item: &[Tok]) -> Vec<usize> {
    (0..item.len()).filter(|&at| matches!(item[at], Tok::Lit(_))).collect()
}

/// Another literal in place of the one at a drawn place of `item`; false if it has none.
fn edit_a_literal(tc: &TestCase, item: &mut [Tok]) -> bool {
    let places = literals(item);
    if places.is_empty() {
        return false;
    }
    let at = places[tc.draw(gs::integers::<usize>().max_value(places.len() - 1))];
    if let Tok::Lit(n) = &mut item[at] {
        *n = (*n + 1 + tc.draw(gs::integers::<u64>().max_value(97))) % 100;
    }
    true
}

/// Each case builds four databases.
fn keeps_and_loses_its_identity(tc: TestCase) {
    let mut items = program(&tc);
    let mut order: Vec<usize> = (0..items.len()).collect();
    let base = plain(&items, &order);
    for at in (1..order.len()).rev() {
        order.swap(at, tc.draw(gs::integers().max_value(at)));
    }
    same(&base, &laid_out(&tc, &items, &order));
    if items.len() > 1 {
        let other: usize = tc.draw(gs::integers().min_value(1).max_value(items.len() - 1));
        let mut edited = items.clone();
        if edit_a_literal(&tc, &mut edited[other]) {
            same(&base, &plain(&edited, &order));
        }
    }
    if edit_a_literal(&tc, &mut items[0]) {
        differ(&base, &plain(&items, &order));
    }
}

/// Runs `property` over `cases` cases, or `HEGEL_TEST_CASES` of them where that is set.
fn run_cases(cases: u64, property: impl FnMut(TestCase)) {
    let cases = std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(cases);
    Hegel::new(property).settings(Settings::new().test_cases(cases).database(None)).run();
}

/// `HEGEL_TEST_CASES=<n>` runs it longer.
#[test]
fn layout_comments_order_and_other_items_keep_a_body_and_its_own_literals_do_not() {
    run_cases(24, keeps_and_loses_its_identity);
}
