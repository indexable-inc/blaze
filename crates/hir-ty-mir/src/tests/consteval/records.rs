//! blaze's records (`#![blaze::records]`, docs/v2/plan/design/records.md) evaluated: a record
//! value is the same value whatever order its fields are written in, a field is read by its
//! name, a functional update takes the rest from its base, a pattern binds by name, and a
//! record argument's `..` takes the callee's defaults, which are constants of the callee and
//! so the same at every call (their order and count are unobservable).

use super::check_number;

#[test]
fn a_record_is_the_same_value_in_any_field_order() {
    check_number(
        r#"
//- minicore: sized, record
#![blaze::records]
const fn weigh(p: { x: i32, y: i32 }) -> i32 { p.x * 10 + p.y }
const GOAL: i32 = {
    let a: { x: i32, y: i32 } = { y: 2, x: 1 };
    let b: { y: i32, x: i32 } = a;
    let c = { y: 4, x: 3 };
    weigh(a) + 100 * weigh(b) + 10000 * weigh(c) + 1000000 * weigh({ y: 6, x: 5 })
};
"#,
        12 + 100 * 12 + 10000 * 34 + 1000000 * 56,
    );
}

#[test]
fn shorthand_nested_records_and_field_writes() {
    check_number(
        r#"
//- minicore: sized, record
#![blaze::records]
const GOAL: i32 = {
    let x = 7;
    let one = { x, };
    let mut pair = { x, y: 2 };
    pair.y = 5;
    let nested = { at: { x: 1, y: 3 }, scale: 2 };
    one.x + 10 * pair.y + 100 * nested.at.y * nested.scale
};
"#,
        7 + 10 * 5 + 100 * 3 * 2,
    );
}

#[test]
fn a_functional_update_takes_the_rest_from_its_base() {
    check_number(
        r#"
//- minicore: sized, record
#![blaze::records]
const GOAL: i32 = {
    let old = { x: 1, y: 2, z: 3 };
    let new: { x: i32, y: i32, z: i32 } = { y: 5, ..old };
    new.x + 10 * new.y + 100 * new.z + 1000 * old.y
};
"#,
        1 + 10 * 5 + 100 * 3 + 1000 * 2,
    );
}

#[test]
fn a_pattern_binds_by_field_name() {
    check_number(
        r#"
//- minicore: sized, record
#![blaze::records]
const fn pick(p: { x: i32, y: i32 }) -> i32 {
    match p {
        { x: 0, y } => y,
        { x, .. } => x * 100,
    }
}
const GOAL: i32 = {
    let { y: second, x: first } = { x: 1, y: 2 };
    let r = &{ a: 3, b: 4 };
    let { b, .. } = r;
    first + 10 * second + 100 * *b + 1000 * pick({ x: 0, y: 9 }) + pick({ y: 9, x: 5 })
};
"#,
        1 + 10 * 2 + 100 * 4 + 1000 * 9 + 500,
    );
}

/// A field left out takes the callee's default: a literal, a named constant, a `const fn`
/// call. Each call that leaves it out gets the same value, and a field that is written is not
/// overridden.
#[test]
fn a_record_argument_takes_the_callees_defaults() {
    check_number(
        r#"
//- minicore: sized, record
#![blaze::records]
const SCALE: i32 = 2;
const fn double(x: i32) -> i32 { x * 2 }
const fn area(o: { w: i32 = 3, h: i32 = double(2), scale: i32 = SCALE }) -> i32 {
    o.w * o.h * o.scale
}
const GOAL: i32 = area({ w: 5, .. })
    + 1000 * area({ scale: 1, .. })
    + 100000 * area({ h: 1, w: 1, scale: 1 })
    + 1000000 * area({ w: 5, .. });
"#,
        5 * 4 * 2 + 1000 * (3 * 4) + 100000 + 1000000 * (5 * 4 * 2),
    );
}

/// A method's record parameter, after the receiver, and a trait method's defaults, which are
/// its declaration's whichever impl runs.
#[test]
fn a_methods_record_argument_takes_the_declarations_defaults() {
    check_number(
        r#"
//- minicore: sized, record
#![blaze::records]
struct Disk(i32);
impl Disk {
    const fn put(&self, key: i32, opts: { retries: i32 = 3, sync: bool = true }) -> i32 {
        if opts.sync { self.0 + key * 10 + opts.retries * 100 } else { 0 }
    }
}
const GOAL: i32 = {
    let disk = Disk(1);
    disk.put(2, { retries: 4, .. }) + 1000 * disk.put(2, { sync: true, .. })
};
"#,
        1 + 20 + 400 + 1000 * (1 + 20 + 300),
    );
}
