//! The structural lints: the analysis's properties over drawn bodies, and one fixture per
//! lint that fires on the bug or the hand-written form and stays quiet on the right one.

use std::alloc::Global;

use hegel::generators as gs;
use syntax::TextRange;

use super::uniform::{Shape, Term, classify};
use crate::tests::{lint, offered_in};

/// `n` field names, `f0` to `f{n-1}`.
fn names(n: usize) -> Vec<String> {
    (0..n).map(|it| format!("f{it}")).collect()
}

/// One term per field in `order`, each reading its field through `template`.
fn terms(order: &[usize], template: &str) -> Vec<Term> {
    order
        .iter()
        .map(|&at| {
            let mut reads: Vec<usize, Global> = Vec::new_in(Global);
            reads.push(at);
            Term { owner: None, reads, template: template.to_owned(), range: TextRange::default() }
        })
        .collect()
}

const TEMPLATE: &str = "self . # . hash ( state )";

/// The invariant: a body with one term per field, each the same template over its field, is
/// [`Shape::Uniform`] and covers every field in declaration order.
#[hegel::test(test_cases = 64)]
fn one_template_over_every_field_is_uniform(tc: hegel::TestCase) {
    let n: usize = tc.draw(gs::integers().min_value(2).max_value(12));
    let owned = names(n);
    let fields: Vec<&str> = owned.iter().map(String::as_str).collect();
    let order: Vec<usize> = (0..n).collect();
    let found = classify(&fields, &fields, &[], &terms(&order, TEMPLATE), 2, false);
    assert_eq!(found.shape, Shape::Uniform, "{found:?}");
    assert_eq!(found.template, TEMPLATE);
    assert!(found.in_order(n), "{found:?}");
    assert!(found.missing.is_empty() && found.odd.is_empty() && found.crossed.is_empty());
}

/// The invariant: deleting one field's term from a uniform body of three fields or more gives
/// [`Shape::AlmostUniform`] naming exactly that field missing; leaving the same field out with
/// its reason gives [`Shape::Uniform`] back.
#[hegel::test(test_cases = 64)]
fn one_term_deleted_is_almost_uniform_naming_the_field(tc: hegel::TestCase) {
    let n: usize = tc.draw(gs::integers().min_value(3).max_value(12));
    let gone: usize = tc.draw(gs::integers().min_value(0).max_value(n - 1));
    let owned = names(n);
    let fields: Vec<&str> = owned.iter().map(String::as_str).collect();
    let order: Vec<usize> = (0..n).filter(|it| *it != gone).collect();
    let body = terms(&order, TEMPLATE);
    let found = classify(&fields, &fields, &[], &body, 2, false);
    assert_eq!(found.shape, Shape::AlmostUniform, "{found:?}");
    assert_eq!(found.missing.as_slice(), &[gone], "{found:?}");
    let excused = classify(&fields, &fields, &[gone], &body, 2, false);
    assert_eq!(excused.shape, Shape::Uniform, "a field left out with its reason: {excused:?}");
}

/// The invariant: the order of the terms does not change the verdict; a permuted uniform body
/// is [`Shape::Uniform`] over the same fields, and in declaration order only when unpermuted.
#[hegel::test(test_cases = 64)]
fn permuting_the_terms_keeps_uniform(tc: hegel::TestCase) {
    let n: usize = tc.draw(gs::integers().min_value(2).max_value(8));
    let order: Vec<usize> = tc.draw(gs::permutations((0..n).collect::<Vec<usize>>()));
    let owned = names(n);
    let fields: Vec<&str> = owned.iter().map(String::as_str).collect();
    let found = classify(&fields, &fields, &[], &terms(&order, TEMPLATE), 2, false);
    assert_eq!(found.shape, Shape::Uniform, "{found:?}");
    let mut covered = found.covered.to_vec();
    covered.sort_unstable();
    assert_eq!(covered, (0..n).collect::<Vec<usize>>());
    let sorted = order.iter().enumerate().all(|(at, &it)| at == it);
    assert_eq!(found.in_order(n), sorted, "{order:?}");
}

/// The invariant: one term of another template among three or more is odd, never covered, and
/// the body is [`Shape::AlmostUniform`]; a term labelled with one field that reads another is
/// crossed.
#[hegel::test(test_cases = 64)]
fn an_odd_term_and_a_crossed_term_are_named(tc: hegel::TestCase) {
    let n: usize = tc.draw(gs::integers().min_value(4).max_value(10));
    let odd: usize = tc.draw(gs::integers().min_value(0).max_value(n - 1));
    let owned = names(n);
    let fields: Vec<&str> = owned.iter().map(String::as_str).collect();
    let order: Vec<usize> = (0..n).collect();
    let mut body = terms(&order, TEMPLATE);
    body[odd].template = "other ( self . # )".to_owned();
    let found = classify(&fields, &fields, &[], &body, 2, false);
    assert_eq!(found.shape, Shape::AlmostUniform, "{found:?}");
    assert_eq!(found.odd.as_slice(), &[odd]);
    assert!(!found.covered.contains(&odd));

    let mut body = terms(&order, TEMPLATE);
    let (label, read) = (odd, (odd + 1) % n);
    body[label].owner = Some(label);
    body[label].reads[0] = read;
    let found = classify(&fields, &fields, &[], &body, 2, false);
    assert_eq!(found.crossed.as_slice(), &[(label, read)], "{found:?}");
    assert_eq!(found.shape, Shape::AlmostUniform);
}

const MAIN: &str = r#"
//- minicore: sized, copy, clone, eq, ord, hash, option, derive, fmt, default, from
//- /src/lib.rs crate:main
use core::hash::{Hash, Hasher};
"#;

fn header() -> String {
    MAIN.trim_start_matches('\n').to_owned()
}

fn flags(lint: &str) -> String {
    format!("-A clippy::all --warn=blaze::{lint}")
}

fn lint_main(main: &str, name: &str) -> String {
    lint(&format!("{}{}", header(), main.trim_start_matches('\n')), &flags(name))
}

/// An `Eq` that ignores a field its `Hash` covers is found; one whose `Hash` covers fewer
/// fields than it compares is legal (equal values still hash alike) and is not.
#[test]
fn an_eq_that_skips_a_field_hash_covers_is_found() {
    let found = lint_main(
        r#"
pub struct Loose {
    a: u32,
    b: u32,
    c: u32,
}
impl PartialEq for Loose {
    fn eq(&self, other: &Self) -> bool {
        self.a == other.a && self.b == other.b
    }
}
impl Eq for Loose {}
impl Hash for Loose {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.a.hash(state);
        self.b.hash(state);
        self.c.hash(state);
    }
}
pub struct Coarse {
    a: u32,
    b: u32,
    c: u32,
}
impl PartialEq for Coarse {
    fn eq(&self, other: &Self) -> bool {
        self.a == other.a && self.b == other.b && self.c == other.c
    }
}
impl Eq for Coarse {}
impl Hash for Coarse {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.a.hash(state);
        self.b.hash(state);
    }
}
"#,
        "impl_misses_field",
    );
    assert!(
        found.contains("`PartialEq` of `Loose` ignores `c`, which its `Hash` covers"),
        "{found}"
    );
    assert!(!found.contains("Coarse"), "a coarser hash is legal:\n{found}");
}

const CLONE: &str = r#"
pub struct Pair {
    a: u32,
    b: u32,
}
impl Clone for Pair {
    fn clone(&self) -> Self {
        Self { a: self.a.clone(), b: self.b.clone() }
    }
}
"#;

/// A `Clone` identical to the derive is found, the fix makes it the derive, and the fixed file
/// lints clean ([`offered_in`] checks that); a `Clone` that resets a field is not the derive.
#[test]
fn a_clone_identical_to_the_derive_is_derived() {
    let found = lint_main(CLONE, "hand_written_derivable");
    assert!(found.contains("Pair"), "{found}");
    let after = offered_in(&header(), CLONE.trim_start_matches('\n'), &flags("hand_written_derivable"));
    assert!(after.contains("#[derive(Clone)]\npub struct Pair"), "{after}");
    assert!(!after.contains("impl Clone"), "{after}");

    let found = lint_main(
        r#"
pub struct Reset {
    a: u32,
    b: u32,
}
impl Clone for Reset {
    fn clone(&self) -> Self {
        Self { a: self.a.clone(), b: 0 }
    }
}
"#,
        "hand_written_derivable",
    );
    assert!(!found.contains("Reset"), "a clone that resets a field is not the derive:\n{found}");
}

/// The 23 variants of the enum the match fixtures use.
const VARIANTS: [&str; 23] = [
    "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R",
    "S", "T", "U", "V", "W",
];

/// An enum of [`VARIANTS`] and a `fn name` naming the first `named` of them, a `_` arm for the
/// rest.
fn named_match(named: usize) -> String {
    let mut out = format!("pub enum Tag {{ {} }}\n", VARIANTS.join(", "));
    out.push_str("impl Tag {\n    pub fn name(self) -> &'static str {\n        match self {\n");
    for it in &VARIANTS[..named] {
        out.push_str(&format!("            Tag::{it} => \"{}\",\n", it.to_lowercase()));
    }
    if named < VARIANTS.len() {
        out.push_str("            _ => \"other\",\n");
    }
    out.push_str("        }\n    }\n}\n");
    out
}

/// A match naming 22 of 23 variants, the last left to `_`, is found and its fix names the
/// variant; one naming all 23 is not, nor one whose `_` groups two variants.
#[test]
fn a_match_naming_all_but_one_variant_is_found() {
    let found = lint_main(&named_match(22), "variant_list_drift");
    assert!(
        found.contains("`fn name` of `Tag` names 22 of its 23 variants and leaves `W` to `_`"),
        "{found}"
    );
    let after = offered_in(&header(), &named_match(22), &flags("variant_list_drift"));
    assert!(after.contains("Tag::W => \"other\""), "{after}");
    let found = lint_main(&named_match(23), "variant_list_drift");
    assert!(!found.contains("Tag"), "every variant named:\n{found}");
    let found = lint_main(&named_match(21), "variant_list_drift");
    assert!(!found.contains("Tag"), "a `_` over two variants is a grouping:\n{found}");
}

/// A names table, indexed by the enum, with one wrong string.
fn names_table(gamma: &str) -> String {
    format!(
        r#"
pub enum Kind {{ Alpha, Beta, Gamma, Delta, Epsilon }}
impl Kind {{
    pub const COUNT: usize = 5;
}}
pub const NAMES: [&str; Kind::COUNT] = ["alpha", "beta", "{gamma}", "delta", "epsilon"];
"#
    )
}

/// A names table with one misspelled string is found; the right table is not.
#[test]
fn a_names_table_with_one_wrong_string_is_found() {
    let found = lint_main(&names_table("gama"), "variant_list_drift");
    assert!(found.contains("\"gama\""), "{found}");
    assert!(found.contains("\"gamma\""), "{found}");
    let found = lint_main(&names_table("gamma"), "variant_list_drift");
    assert!(!found.contains("NAMES"), "the right table:\n{found}");
}

/// A record of five fields and an `Encode` of it putting the first `put` of them.
fn codec(put: usize) -> String {
    let mut out = String::from(
        r#"
pub struct Out;
impl Out {
    pub fn put(&mut self, _: u32) {}
}
pub trait Encode {
    fn encode(&self, out: &mut Out);
}
pub struct Row {
    a: u32,
    b: u32,
    c: u32,
    d: u32,
    e: u32,
}
impl Encode for Row {
    fn encode(&self, out: &mut Out) {
"#,
    );
    for it in &["a", "b", "c", "d", "e"][..put] {
        out.push_str(&format!("        out.put(self.{it});\n"));
    }
    out.push_str("    }\n}\n");
    out
}

/// A codec uniform over five fields is advice (`hand_written_codec`); one that forgets the
/// fifth is the bug (`impl_misses_field`), which the uniform one is not.
#[test]
fn a_codec_uniform_over_five_fields_and_one_that_forgets_the_fifth() {
    let found = lint_main(&codec(5), "hand_written_codec");
    assert!(found.contains("Row"), "{found}");
    let found = lint_main(&codec(5), "impl_misses_field");
    assert!(!found.contains("Row"), "every field encoded:\n{found}");
    let found = lint_main(&codec(4), "impl_misses_field");
    assert!(found.contains("`encode` of `impl Encode for Row`"), "{found}");
    assert!(found.contains("leaves out `e`"), "{found}");
}

/// A conversion that fills two fields from each other's namesake is found; the straight one
/// is not.
#[test]
fn a_conversion_that_crosses_two_fields_is_found() {
    let types = r#"
pub struct Wire {
    x: u32,
    y: u32,
    z: u32,
}
pub struct Point {
    x: u32,
    y: u32,
    z: u32,
}
"#;
    let crossed = format!(
        "{types}impl From<Wire> for Point {{\n    fn from(w: Wire) -> Self {{\n        Point {{ x: w.y, y: w.x, z: w.z }}\n    }}\n}}\n"
    );
    let found = lint_main(&crossed, "conversion_misses_field");
    assert!(found.contains("fills `x` from `w.y`"), "{found}");
    assert!(found.contains("fills `y` from `w.x`"), "{found}");
    let straight = format!(
        "{types}impl From<Wire> for Point {{\n    fn from(w: Wire) -> Self {{\n        Point {{ x: w.x, y: w.y, z: w.z }}\n    }}\n}}\n"
    );
    let found = lint_main(&straight, "conversion_misses_field");
    assert!(!found.contains("fills"), "the straight conversion:\n{found}");
}
