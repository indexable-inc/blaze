//! The world's stream buffer ([`crate::out`], "Buffering"): how many writes the host is
//! handed for what a program says, that the bytes are exactly what was said, in order, that a
//! text larger than the buffer goes through whole without being copied in, that nothing is
//! lost when `main` fails, and that the integer writer says what `core::fmt` says.

use crate::{
    entry::{Error, Status, run_in},
    io::{Io, Limits, SimWorld},
    out::{BUFFER, Stream},
    term::Tty,
};

/// What the simulated host was handed on standard output, and in how many writes. Reading it
/// hands over what the buffer still holds, as one write ([`Io::script`]).
fn handed(io: &Io) -> (String, u64) {
    io.script(|host| (String::from_utf8(host.out.clone()).expect("UTF-8"), host.writes[0])).expect("a simulated world")
}

/// 1,000 lines of ten bytes to a pipe: the host is handed `ceil(10,000 / BUFFER)` writes, the
/// first exactly when the buffer filled (what is left waits in it), and the bytes are the
/// lines in order. Without the buffer it is 1,000 writes. Prints
/// `1000 lines, 10000 bytes, to a pipe: 2 writes`.
#[test]
fn lines_to_a_pipe_are_written_a_buffer_at_a_time() {
    let io = Io::sim(41);
    let cx = super::root(&io);
    let out = io.out().expect("Io::sim offers out");
    let mut expected = String::new();
    for n in 0..1000u32 {
        out.line(&cx, format_args!("line {n:04}")).unwrap();
        expected.push_str(&format!("line {n:04}\n"));
    }
    assert_eq!(expected.len(), 10_000);
    assert_eq!(io.held_out(Stream::Out), 10_000 - BUFFER, "one buffer written when full, the rest held");
    let (bytes, writes) = handed(&io);
    println!("1000 lines, {} bytes, to a pipe: {writes} writes", bytes.len());
    assert_eq!(bytes, expected, "exactly the lines, in order");
    assert!(writes <= expected.len().div_ceil(BUFFER) as u64, "{writes} writes");
    assert_eq!(writes, 2);
}

/// The same 1,000 lines to a terminal: each call's bytes are on the screen when it returns,
/// so the host is handed exactly 1,000 writes, one a line, and nothing is left held. Prints
/// `1000 lines to a terminal: 1000 writes`.
#[test]
fn lines_to_a_terminal_are_written_one_a_line() {
    let mut world = SimWorld::new(42, Limits::USUAL);
    world.ttys = [Tty::Terminal(None), Tty::NotTerminal];
    let io = Io::simulated(world);
    let cx = super::root(&io);
    let out = io.out().expect("Io::sim offers out");
    let mut expected = String::new();
    for n in 0..1000u32 {
        out.line(&cx, format_args!("line {n:04}")).unwrap();
        expected.push_str(&format!("line {n:04}\n"));
    }
    assert_eq!(io.held_out(Stream::Out), 0, "a terminal's line is written when its call returns");
    let (bytes, writes) = handed(&io);
    println!("1000 lines to a terminal: {writes} writes");
    assert_eq!((bytes, writes), (expected, 1000));
}

/// A text three times the buffer, after two short lines: it arrives whole and in order, in one
/// write with the two lines before it (what the buffer held and the text, vectored, not
/// copied through the buffer a buffer at a time, which would be four writes), and its line
/// feed is held after it. Prints `3 x BUFFER text: 2 writes`.
#[test]
fn a_text_larger_than_the_buffer_goes_through_whole() {
    let io = Io::sim(43);
    let cx = super::root(&io);
    let out = io.out().expect("Io::sim offers out");
    let big: String = (0..3 * BUFFER).map(|at| char::from(b'a' + (at % 26) as u8)).collect();
    out.line(&cx, "a").unwrap();
    out.line(&cx, "b").unwrap();
    out.line(&cx, big.as_str()).unwrap();
    assert_eq!(io.held_out(Stream::Out), 1, "the line feed after the text waits");
    let (bytes, writes) = handed(&io);
    println!("3 x BUFFER text: {writes} writes");
    assert_eq!(bytes, format!("a\nb\n{big}\n"), "whole and in order");
    assert_eq!(writes, 2);
}

/// A `main` that says three lines and returns `Err`: the entry writes what the buffer held
/// before it hands back the status, so the lines are with the host (nothing is left held)
/// and the run says its error and fails. Without the entry's flush the lines would still be
/// held when `run_in` returns. Prints `held after main: 0`.
#[test]
fn nothing_is_lost_when_main_returns_err() {
    let io = Io::sim(44);
    let mut said = String::new();
    let status = run_in(io.clone(), &mut said, |cx, io| -> Result<(), Error> {
        let out = io.need_out()?;
        for n in 0..3u32 {
            out.line(cx, n)?;
        }
        Err(Error::new("broke after three lines"))
    });
    let held = io.held_out(Stream::Out);
    println!("held after main: {held}");
    assert_eq!((status, said.as_str(), held), (Status::FAILED, "broke after three lines", 0));
    assert_eq!(handed(&io), (String::from("0\n1\n2\n"), 1), "the three lines, in the one write at the end");
}

/// The integer writer (blaze-text's two digits at a time, which `Out::line` of an integer and
/// `Writes::int` use, with no `core::fmt`) says what `core::fmt`'s `{}` says, at the edges.
/// Prints `integers agree with core::fmt: 8 values`.
#[test]
fn the_integer_writer_agrees_with_core_fmt() {
    use blaze_text::Writes as _;
    let io = Io::sim(45);
    let cx = super::root(&io);
    let out = io.out().expect("Io::sim offers out");
    let mut expected = String::new();
    let mut values = 0;
    macro_rules! each {
        ($($value:expr),* $(,)?) => {$(
            let value = $value;
            out.line(&cx, value).unwrap();
            out.line_with(&cx, |o| o.int(value)).unwrap();
            expected.push_str(&format!("{value}\n{value}\n"));
            values += 1;
        )*};
    }
    each!(0u64, 9u64, 10u64, 99u64, 100u64, u64::MAX, i64::MIN, u128::MAX);
    let (bytes, _) = handed(&io);
    println!("integers agree with core::fmt: {values} values");
    assert_eq!(bytes, expected);
}
