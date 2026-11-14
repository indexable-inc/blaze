//! `skipped_pair`, `half_done` and `hidden_exit`: the exits that leave something undone, each
//! where it fires and where the right form or a plain reading leaves it quiet.

use super::found;

#[test]
fn skipped_pair_finds_a_pop_a_question_mark_skips() {
    let out = found(
        "skipped_pair",
        r#"
pub struct Scope { names: Vec<u32> }
fn check(x: u32) -> Result<u32, ()> { if x > 3 { Err(()) } else { Ok(x) } }
impl Scope {
    pub fn skipped(&mut self, x: u32) -> Result<u32, ()> {
        self.names.push(x);
        let y = check(x)?;
        self.names.pop();
        Ok(y)
    }
    pub fn returned(&mut self, x: u32) -> u32 {
        self.names.push(x);
        if x == 0 {
            return 0;
        }
        self.names.pop();
        x
    }
    pub fn paired(&mut self, x: u32) -> Result<u32, ()> {
        self.names.push(x);
        let y = check(x);
        self.names.pop();
        y
    }
    pub fn other_place(&mut self, other: &mut Vec<u32>, x: u32) -> Result<u32, ()> {
        self.names.push(x);
        let y = check(x)?;
        other.pop();
        Ok(y)
    }
    pub fn never_popped(&mut self, x: u32) -> Result<u32, ()> {
        self.names.push(x);
        check(x)
    }
}
"#,
    );
    assert_eq!(out.matches("is not undone by its `pop` on every path").count(), 2, "{out}");
    assert!(out.contains("docs/lints.md#skipped_pair"), "{out}");
    assert!(out.contains("`check(x)?`"), "{out}");
    assert!(out.contains("`return 0`"), "{out}");
}

#[test]
fn skipped_pair_leaves_a_guard_alone_and_reads_a_mark() {
    let out = found(
        "skipped_pair",
        r#"
pub struct Span;
pub struct Entered<'a> { span: &'a mut Span }
impl Drop for Entered<'_> {
    fn drop(&mut self) {}
}
fn check(x: u32) -> Result<u32, ()> { if x > 3 { Err(()) } else { Ok(x) } }
impl Span {
    pub fn enter(&mut self) -> Entered<'_> { Entered { span: self } }
    pub fn exit(&mut self) {}
    #[blaze::pairs_with(finish)]
    pub fn run(&mut self) {}
    pub fn finish(&mut self) {}
}
pub fn guarded(span: &mut Span, x: u32) -> Result<u32, ()> {
    let entered = span.enter();
    let y = check(x)?;
    drop(entered);
    span.exit();
    Ok(y)
}
pub fn marked(span: &mut Span, x: u32) -> Result<u32, ()> {
    span.run();
    let y = check(x)?;
    span.finish();
    Ok(y)
}
"#,
    );
    assert_eq!(out.matches("is not undone").count(), 1, "{out}");
    assert!(out.contains("this `run` is not undone by its `finish`"), "{out}");
}

#[test]
fn half_done_finds_a_write_an_exit_leaves_half_made() {
    let out = found(
        "half_done",
        r#"
pub struct Pool { len: usize, slots: Vec<u32> }
fn alloc(n: usize) -> Result<u32, ()> { if n > 3 { Err(()) } else { Ok(0) } }
impl Pool {
    pub fn half(&mut self) -> Result<(), ()> {
        self.len += 1;
        let slot = alloc(self.len)?;
        self.slots.push(slot);
        Ok(())
    }
    pub fn committed(&mut self) -> Result<(), ()> {
        let slot = alloc(self.len + 1)?;
        self.len += 1;
        self.slots.push(slot);
        Ok(())
    }
    pub fn before_only(&mut self) -> Result<u32, ()> {
        self.len += 1;
        alloc(self.len)
    }
    pub fn consumed(mut self) -> Result<Pool, ()> {
        self.len += 1;
        let slot = alloc(self.len)?;
        self.slots.push(slot);
        Ok(self)
    }
    pub fn infallible(&mut self, slot: u32) {
        self.len += 1;
        if slot == 0 {
            return;
        }
        self.slots.push(slot);
    }
}
"#,
    );
    assert_eq!(out.matches("half changed").count(), 1, "{out}");
    assert!(out.contains("`self.len += 1` wrote it before"), "{out}");
    assert!(out.contains("docs/lints.md#half_done"), "{out}");
}

#[test]
fn hidden_exit_finds_a_macro_that_returns_from_its_caller() {
    let out = found(
        "hidden_exit",
        r#"
macro_rules! check {
    ($e:expr) => { if !$e { return Err(()); } };
}
macro_rules! open {
    ($e:expr) => { $e? };
}
macro_rules! value {
    ($e:expr) => { $e + 1 };
}
macro_rules! looped {
    () => { loop { break; } };
}
macro_rules! inner {
    () => { (|| -> Result<u32, ()> { Err(())?; Ok(0) })() };
}
fn g() -> Result<u32, ()> { Ok(1) }
pub fn f(x: u32) -> Result<u32, ()> {
    check!(x > 1);
    let a = open!(g());
    let b = value!(x);
    looped!();
    let _ = inner!();
    let c = value!(g()?);
    Ok(a + b + c)
}
"#,
    );
    assert_eq!(out.matches("leaves the function it is called in").count(), 2, "{out}");
    assert!(out.contains("`check!` leaves the function it is called in: its expansion holds a `return`"), "{out}");
    assert!(out.contains("`open!` leaves the function it is called in: its expansion holds a `?`"), "{out}");
    assert!(out.contains("docs/lints.md#hidden_exit"), "{out}");
}
