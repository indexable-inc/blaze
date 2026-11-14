//! The perturbations as properties over random programs: `f`'s identity survives any layout,
//! any `//` or `/* */` comments, any order of the items around it and any edit of their
//! literals, and changes when any literal of its own does.

use blaze_sim::{Config, Tape};

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
fn operand(tape: &Tape, locals: usize, helpers: usize, out: &mut Vec<Tok>) {
    match tape.int_in(0u8..=3, "operand") {
        1 if locals > 0 => {
            let at = tape.index(locals, "local");
            out.push(word(format!("x{at}")));
        }
        2 if helpers > 0 => {
            let at = tape.index(helpers, "helper");
            out.extend([word(format!("h{at}")), word("("), word(")")]);
        }
        3 => out.extend(["(", "dep", "::", "g", "(", ")", "as", "u64", ")"].map(word)),
        _ => out.push(Tok::Lit(tape.int_in(0u64..=99, "literal"))),
    }
}

/// Operands joined by operators that cannot overflow on these sizes.
fn expr(tape: &Tape, locals: usize, helpers: usize, out: &mut Vec<Tok>) {
    operand(tape, locals, helpers, out);
    for _ in 0..tape.int_in(0u8..=2, "operators") {
        out.push((*tape.one_of(&["+", "^", "|", "&"], "operator")).into());
        operand(tape, locals, helpers, out);
    }
}

impl From<&str> for Tok {
    fn from(text: &str) -> Tok {
        word(text)
    }
}

/// The items of `main`, each as its tokens: `f` first, then helpers, structs and constants.
fn program(tape: &Tape) -> Vec<Vec<Tok>> {
    let helpers = tape.int_in(0usize..=3, "helpers");
    let mut f: Vec<Tok> = ["pub", "fn", "f", "(", ")", "->", "u64", "{"].map(word).to_vec();
    let locals = tape.int_in(0usize..=5, "locals");
    for at in 0..locals {
        f.extend([word("let"), word(format!("x{at}")), word("=")]);
        expr(tape, at, helpers, &mut f);
        f.push(word(";"));
    }
    expr(tape, locals, helpers, &mut f);
    f.push(word("}"));
    let mut items = vec![f];
    for at in 0..helpers {
        let lit = Tok::Lit(tape.int_in(0u64..=99, "helper literal"));
        let mut h = vec![word("pub"), word("fn"), word(format!("h{at}"))];
        h.extend([word("("), word(")"), word("->"), word("u64"), word("{"), lit, word("}")]);
        items.push(h);
    }
    for at in 0..tape.int_in(0usize..=2, "structs") {
        items.push(vec![word("pub"), word("struct"), word(format!("S{at}")), word(";")]);
    }
    for at in 0..tape.int_in(0usize..=2, "consts") {
        let lit = Tok::Lit(tape.int_in(0u64..=99, "const literal"));
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
fn laid_out(tape: &Tape, items: &[Vec<Tok>], order: &[usize]) -> String {
    const GAPS: [&str; 9] = [" ", "  ", "\n", "\n    ", "\t", " /* c */ ", "/**/", " // c\n", "\n// c\n"];
    let mut out = String::new();
    for &at in order {
        for tok in &items[at] {
            out.push_str(&text(tok));
            out.push_str(tape.one_of(&GAPS, "gap"));
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
fn edit_a_literal(tape: &Tape, item: &mut [Tok]) -> bool {
    let places = literals(item);
    if places.is_empty() {
        return false;
    }
    let at = places[tape.index(places.len(), "literal edited")];
    if let Tok::Lit(n) = &mut item[at] {
        *n = (*n + 1 + tape.int_in(0u64..=97, "literal step")) % 100;
    }
    true
}

/// Each case builds four databases.
fn keeps_and_loses_its_identity(tape: &Tape) {
    let mut items = program(tape);
    let mut order: Vec<usize> = (0..items.len()).collect();
    let base = plain(&items, &order);
    tape.shuffle(&mut order, "item order");
    same(&base, &laid_out(tape, &items, &order));
    if items.len() > 1 {
        let other = tape.int_in(1usize..=items.len() - 1, "other item");
        let mut edited = items.clone();
        if edit_a_literal(tape, &mut edited[other]) {
            same(&base, &plain(&edited, &order));
        }
    }
    if edit_a_literal(tape, &mut items[0]) {
        differ(&base, &plain(&items, &order));
    }
}

/// `BLAZE_SIM_SEEDS=<n>` runs it longer.
#[test]
fn layout_comments_order_and_other_items_keep_a_body_and_its_own_literals_do_not() {
    blaze_sim::check(Config::cases(24), keeps_and_loses_its_identity);
}
