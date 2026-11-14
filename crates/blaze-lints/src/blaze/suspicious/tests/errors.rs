//! `fallback`, `unseen_target`, `question_mark_converts` and `option_question_mark_hides`: where
//! each fires, where it does not, and for `unseen_target` what its fix writes.

use super::{fixed, fixture_of, found};
use crate::tests::lint;

const ROUTES: &str = r#"
pub struct E;
pub fn fast(x: u32) -> Result<u32, E> { Ok(x) }
pub fn slow(x: u32) -> u32 { x }
pub fn slow_r(x: u32) -> Result<u32, E> { Ok(x) }
pub fn arm(x: u32) -> u32 {
    match fast(x) {
        Ok(v) => v,
        Err(_) => slow(x),
    }
}
pub fn closure(x: u32) -> u32 {
    fast(x).unwrap_or_else(|_| slow(x))
}
pub fn or_else(x: u32) -> Result<u32, E> {
    fast(x).or_else(|_| slow_r(x))
}
pub fn is_err(x: u32) -> u32 {
    let mut v = fast(x);
    if v.is_err() {
        v = slow_r(x);
    }
    match v {
        Ok(v) => v,
        Err(_e) => 0,
    }
}
pub fn if_let(x: u32) -> u32 {
    let mut v = 0;
    if let Err(_) = fast(x) {
        v = slow(x);
    }
    v
}
pub fn handed_on(x: u32) -> Result<u32, E> {
    match fast(x) {
        Ok(v) => Ok(v),
        Err(e) => Err(e),
    }
}
pub fn returns(x: u32) -> Result<u32, E> {
    match fast(x) {
        Ok(v) => Ok(v),
        Err(_) => return Err(E),
    }
}
pub fn reads_it(x: u32) -> Result<u32, E> {
    fast(x).or_else(|e| Err(e))
}
pub fn defaults(x: u32) -> u32 {
    match fast(x) {
        Ok(v) => v,
        Err(_) => 0,
    }
}
"#;

/// The five shapes are found, each once; an error handed on, a branch that returns, a closure
/// that reads the error and a default are not fallbacks.
#[test]
fn fallback_finds_a_second_route_and_leaves_an_error_handed_on() {
    let out = found("fallback", ROUTES);
    assert_eq!(out.matches("this `Err` arm answers the failure by another route, `slow(..)`").count(), 1, "{out}");
    assert_eq!(out.matches("`.unwrap_or_else(|_| ..)` answers every error").count(), 1, "{out}");
    assert_eq!(out.matches("`.or_else(|_| ..)` answers every error").count(), 1, "{out}");
    assert_eq!(out.matches("this branch computes the result again by another route").count(), 2, "{out}");
    assert_eq!(out.matches("never looks at the").count(), 5, "{out}");
}

/// With `dropped_error` on beside it, the places `fallback` takes are said once, by `fallback`,
/// and the default stays `dropped_error`'s.
#[test]
fn fallback_and_dropped_error_say_each_place_once() {
    let out = lint(
        &fixture_of("", ROUTES),
        "-A clippy::all --warn=blaze::fallback --warn=blaze::dropped_error",
    );
    assert_eq!(out.matches("never looks at the").count(), 5, "{out}");
    assert_eq!(out.matches("throws away").count(), 2, "the default's arm and `is_err`'s `Err(_e)` arm:\n{out}");
    let alone = found("dropped_error", ROUTES);
    assert!(alone.matches("throws away").count() >= 4, "alone, `dropped_error` still finds the arm and the closure:\n{alone}");
}

const TARGETS: &str = r#"
pub struct Port(u32);
impl core::str::FromStr for Port {
    type Err = ();
    fn from_str(s: &str) -> Result<Port, ()> { Ok(Port(0)) }
}
pub struct Wide(u64);
impl From<u32> for Wide {
    fn from(n: u32) -> Wide { Wide(n as u64) }
}
pub struct Count;
impl Iterator for Count {
    type Item = u32;
    fn next(&mut self) -> Option<u32> { None }
}
pub fn takes_port(p: Port) {}
pub fn takes_wide(w: Wide) {}
pub fn takes_all(v: Vec<u32>) {}
pub fn takes_string(s: String) {}
pub fn parsed(text: &str) {
    let p = text.parse().unwrap();
    takes_port(p);
}
pub fn widened(n: u32) {
    let w = n.into();
    takes_wide(w);
}
pub fn collected() {
    let all = Count.collect();
    takes_all(all);
}
pub fn defaulted() {
    let s = Default::default();
    takes_string(s);
}
pub fn written(text: &str, n: u32) {
    let p: Port = text.parse().unwrap();
    takes_port(p);
    let q = text.parse::<Port>().unwrap();
    takes_port(q);
    takes_port(text.parse().unwrap());
    let w: Wide = n.into();
    takes_wide(w);
    takes_wide(n.into());
    let all = Count.collect::<Vec<u32>>();
    takes_all(all);
    let s = String::default();
    takes_string(s);
}
"#;

/// Each of the four untyped `let`s is found; the type written on the `let`, in a turbofish, at
/// the call's own type (`String::default()`) or by the parameter the value is handed to is not.
#[test]
fn unseen_target_finds_a_type_fixed_by_a_later_use() {
    let out = found("unseen_target", TARGETS);
    assert_eq!(out.matches("is written nowhere").count(), 4, "{out}");
    assert!(out.contains("the type `.parse()` makes is written nowhere: only how `p` is used later fixes it"), "{out}");
    assert!(out.contains("the type `Default::default()` makes"), "{out}");
}

/// The fix writes the turbofish for `parse` and `collect` and the `let`'s type for `into` and
/// `Default::default()`, and the fixed file lints clean.
#[test]
fn unseen_target_fix_writes_the_type() {
    let after = fixed("unseen_target", TARGETS);
    assert!(after.contains("let p = text.parse::<Port>().unwrap();"), "{after}");
    assert!(after.contains("let w: Wide = n.into();"), "{after}");
    assert!(after.contains("let all = Count.collect::<Vec<u32>>();"), "{after}");
    assert!(after.contains("let s: String = Default::default();"), "{after}");
}

const CONVERSIONS: &str = r#"
pub struct Typed;
impl core::fmt::Debug for Typed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { Ok(()) }
}
impl core::fmt::Display for Typed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { Ok(()) }
}
impl std::error::Error for Typed {}
pub enum Error {
    Io(std::io::Error),
    Read { path: String, source: std::io::Error },
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error { Error::Io(e) }
}
pub fn typed(x: u32) -> Result<u32, Typed> { Ok(x) }
pub fn converted(path: &str) -> Result<Vec<u8>, Error> {
    let bytes = std::io::read(path)?;
    Ok(bytes)
}
pub fn boxed(text: &str) -> Result<u32, Box<dyn std::error::Error>> {
    let n = std::num::parse_u32(text)?;
    Ok(n)
}
pub fn erased(x: u32) -> Result<u32, Box<dyn std::error::Error>> {
    let n = typed(x)?;
    Ok(n)
}
pub fn named(path: &str) -> Result<Vec<u8>, Error> {
    let bytes = std::io::read(path).map_err(|source| Error::Read { path: String::from(path), source })?;
    Ok(bytes)
}
pub fn same(path: &str) -> std::io::Result<Vec<u8>> {
    let bytes = std::io::read(path)?;
    Ok(bytes)
}
pub fn kept(x: u32) -> Result<u32, Typed> {
    let n = typed(x)?;
    Ok(n)
}
pub fn in_closure(path: &str) -> Option<u32> {
    let f = |p: &str| -> Result<u32, Error> { std::io::read(p)?; Ok(0) };
    f(path).ok()
}
"#;

/// A subjectless std error converted (into a typed error, into a `Box<dyn Error>`, in a
/// closure) and a typed error erased are found; an error already the function's, by
/// `.map_err(..)` or by type, is not.
#[test]
fn question_mark_converts_finds_a_lost_subject_and_an_erased_type() {
    let out = found("question_mark_converts", CONVERSIONS);
    assert_eq!(out.matches("this `?` converts").count(), 4, "{out}");
    assert_eq!(out.matches("the error names no subject").count(), 3, "{out}");
    assert_eq!(out.matches("the target is an erased error").count(), 1, "{out}");
}

const LOOKUPS: &str = r#"
pub struct Map;
impl Map {
    pub fn get(&self, key: u32) -> Option<&u32> { None }
}
pub struct Table { names: Map, items: Vec<u32> }
pub fn item(t: &Table, key: u32) -> Option<u32> {
    let at = t.names.get(key)?;
    let first = t.items.first()?;
    Some(*at + *first)
}
pub fn single(t: &Table, key: u32) -> Option<u32> {
    let at = t.names.get(key)?;
    Some(*at)
}
pub fn closure_alone(t: &Table, key: u32) -> Option<u32> {
    let at = t.names.get(key)?;
    let f = |m: &Map| -> Option<u32> { Some(*m.get(1)?) };
    f(&t.names).map(|it| it + *at)
}
pub fn checked(t: &Table, key: u32) -> Result<u32, ()> {
    let at = t.names.get(key).ok_or(())?;
    let first = t.items.first().ok_or(())?;
    Ok(*at + *first)
}
"#;

/// The second lookup's `?` is found, naming the first; one lookup per body, a closure's own
/// body and a function that returns a `Result` are not.
#[test]
fn option_question_mark_hides_finds_a_second_lookup() {
    let out = found("option_question_mark_hides", LOOKUPS);
    assert_eq!(out.matches("this `?` answers a missing").count(), 1, "{out}");
    assert!(out.contains("a missing `.first(..)` with the same `None` as the `.get(..)?` on line"), "{out}");
}
