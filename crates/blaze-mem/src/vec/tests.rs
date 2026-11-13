//! Every growth path of blaze-mem's collections against a frame with a small limit
//! (docs/v2/plan/design/fallible-bstd.md, decision D29): running out is an `Err` that names
//! the arena and leaves the collection whole, never `alloc`'s `handle_alloc_error`.

use std::{alloc::Allocator, path::Path};

use hegel::TestCase;
use hegel::generators as gs;

use crate::{
    ByteLen, FormatError, Frame, InsertError, Offset, OutOfMemory, Overflow, Tag, TooSmall,
    text::{PathBuf, String},
};

/// The name the frame under test says when it runs out.
const ARENA: &str = "growth test arena";

/// A safeguard turned off to prove the oracle sees the bug (skill `testing`, planted-bug
/// controls).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// A push goes through `alloc`'s own `Vec::push`, the infallible one.
    PushThroughAlloc,
}

#[derive(Clone, Debug)]
enum Op {
    Push(u8),
    ExtendFromSlice(usize),
    /// An iterator that does not say how many it gives.
    Extend(usize),
    Reserve(usize),
    Insert(usize, u8),
    Resize(usize),
    PushStr(usize),
    PushChar(char),
    WriteFmt(u32),
    FormatIn(usize),
    PathPush(usize),
}

#[hegel::composite]
fn op(tc: &TestCase) -> Op {
    let size = |tc: &TestCase| tc.draw(gs::integers::<usize>().max_value(3000));
    match tc.draw(gs::integers::<u8>().max_value(10)) {
        0 => Op::Push(tc.draw(gs::integers())),
        1 => Op::ExtendFromSlice(size(tc)),
        2 => Op::Extend(size(tc)),
        3 => Op::Reserve(size(tc)),
        4 => Op::Insert(size(tc), tc.draw(gs::integers())),
        5 => Op::Resize(size(tc)),
        6 => Op::PushStr(size(tc)),
        7 => Op::PushChar(['a', 'é', '日', '🦀'][tc.draw(gs::integers::<usize>().max_value(3))]),
        8 => Op::WriteFmt(tc.draw(gs::integers())),
        9 => Op::FormatIn(size(tc)),
        _ => Op::PathPush(size(tc).min(200)),
    }
}

/// What a run saw: how many growths were refused, so a test can say its draws reached the
/// limit at all.
#[derive(Default, Debug)]
struct Seen {
    refused: usize,
    grown: usize,
}

/// One push the way `plant` says: blaze-mem's, or `alloc`'s own.
fn push<A: Allocator + crate::Named>(
    bytes: &mut crate::Vec<u8, A>,
    spare: A,
    byte: u8,
    plant: Plant,
) -> Result<(), OutOfMemory> {
    match plant {
        Plant::None => bytes.push(byte),
        Plant::PushThroughAlloc => {
            let mut raw = std::mem::replace(bytes, crate::Vec::new_in(spare)).into_std();
            raw.push(byte);
            *bytes = crate::Vec::from_std(raw);
            Ok(())
        }
    }
}

/// Runs `ops` against collections in a frame of `limit` bytes, with plain global-heap twins
/// as the model, and checks the oracle after each.
///
/// The oracle: every growth returns; an `Ok` leaves the collection equal to the model after
/// the same change; an `Err` names the arena, asked for something, and leaves the collection
/// equal to the model before it (a growth that says it keeps a part, [`crate::Vec::extend`]
/// and `write_fmt`, keeps a prefix of the change); the frame never hands out more than its
/// limit; and after a refusal another task's frame still grows.
fn run(ops: &[Op], limit: usize, plant: Plant) -> Seen {
    let frame = Frame::named(ARENA, limit, Tag::Other).expect("a frame");
    let other = Frame::named("another task", 1 << 20, Tag::Other).expect("a frame");
    let mut bystander = crate::Vec::new_in(&other);
    let mut bytes = crate::Vec::new_in(&frame);
    let mut text = String::new_in(&frame);
    let mut path = PathBuf::new_in(&frame);
    // alloc: the model's twins, on the global heap so the limit is not theirs.
    let (mut bytes_model, mut text_model, mut path_model) =
        (std::vec::Vec::new(), std::string::String::new(), std::path::PathBuf::new());
    let mut seen = Seen::default();
    for (step, op) in ops.iter().enumerate() {
        let said = format!("step {step}, {op:?}, limit {limit}"); // alloc: a test's message
        let refused = |error: OutOfMemory| {
            assert_eq!(error.arena, ARENA, "{said}: the refusal names the arena");
            assert!(error.asked > 0, "{said}: the refusal says what was asked");
        };
        let outcome: Result<(), OutOfMemory> = match op {
            Op::Push(byte) => push(&mut bytes, &frame, *byte, plant).map(|()| {
                bytes_model.push(*byte);
            }),
            Op::ExtendFromSlice(len) => {
                let more = vec![0x5a; *len]; // alloc: a test's input
                bytes.extend_from_slice(&more).map(|()| bytes_model.extend_from_slice(&more))
            }
            Op::Extend(len) => {
                let before = bytes.len();
                let result = bytes.extend((0..*len).map(|at| at as u8).filter(|_| true));
                let kept = bytes.len() - before;
                assert!(result.is_ok() == (kept == *len), "{said}: all of them, or an error");
                bytes_model.extend((0..kept).map(|at| at as u8));
                result
            }
            Op::Reserve(more) => bytes.reserve(*more).map(|()| {
                assert!(bytes.capacity() - bytes.len() >= *more, "{said}: the room was had");
            }),
            Op::Insert(at, byte) => {
                let at = at % (bytes_model.len() + 1);
                match bytes.insert(at, *byte) {
                    Ok(()) => {
                        bytes_model.insert(at, *byte);
                        Ok(())
                    }
                    Err(InsertError::OutOfMemory(error)) => Err(error),
                    Err(error) => panic!("{said}: position {at} is in range, yet {error}"),
                }
            }
            Op::Resize(len) => bytes.resize(*len, 7).map(|()| bytes_model.resize(*len, 7)),
            Op::PushStr(len) => {
                let more = "é".repeat(*len / 2); // alloc: a test's input
                text.push_str(&more).map(|()| text_model.push_str(&more))
            }
            Op::PushChar(ch) => text.push(*ch).map(|()| text_model.push(*ch)),
            Op::WriteFmt(n) => {
                let before = text.len();
                let whole = format!("{n}-{n:05}-{}", "x".repeat(*n as usize % 600)); // alloc: the model
                let result = write!(text, "{n}-{n:05}-{}", "x".repeat(*n as usize % 600)); // alloc: a test's argument
                let kept = text.len() - before;
                assert!(whole.is_char_boundary(kept), "{said}: whole pieces are kept");
                text_model.push_str(&whole[..kept]);
                match result {
                    Ok(()) => {
                        assert_eq!(kept, whole.len(), "{said}: all of it was written");
                        Ok(())
                    }
                    Err(FormatError::Full(error)) => Err(error),
                    Err(FormatError::Trait) => panic!("{said}: no formatting trait here fails"),
                }
            }
            Op::FormatIn(width) => match crate::format_in!(&frame, "{:width$}|{step}", "") {
                Ok(made) => {
                    assert_eq!(made.len(), width + 1 + step.to_string().len(), "{said}"); // alloc: a test's count
                    Ok(())
                }
                Err(FormatError::Full(error)) => Err(error),
                Err(FormatError::Trait) => panic!("{said}: no formatting trait here fails"),
            },
            Op::PathPush(len) => {
                let more = "d".repeat(*len); // alloc: a test's input
                path.push(&more).map(|()| path_model.push(&more))
            }
        };
        match outcome {
            Ok(()) => seen.grown += 1,
            Err(error) => {
                refused(error);
                seen.refused += 1;
                bystander.push(step).expect("another task's frame still grows after a refusal");
            }
        }
        assert_eq!(bytes.as_slice(), bytes_model.as_slice(), "{said}: the bytes are the model's");
        assert_eq!(text.as_str(), text_model.as_str(), "{said}: the text is the model's");
        assert_eq!(path.as_path(), path_model.as_path(), "{said}: the path is the model's");
        assert!(frame.used() <= limit, "{said}: {} used of a limit of {limit}", frame.used());
    }
    assert_eq!(bystander.len(), seen.refused);
    seen
}

/// The invariant: a growth of a collection in an arena with no room returns an error naming
/// that arena and leaves the collection as it was, whatever the growth path; the process and
/// every other arena go on.
#[hegel::test]
fn a_growth_past_the_arenas_limit_is_an_error_naming_it(tc: TestCase) {
    let limit = tc.draw(gs::integers::<usize>().min_value(64).max_value(64 << 10));
    let ops = tc.draw(gs::vecs(op()).min_size(1).max_size(60));
    run(&ops, limit, Plant::None);
}

/// The script the two process tests below run: pushes one at a time until well past the limit.
fn pushes_past(limit: usize) -> std::vec::Vec<Op> {
    (0..limit * 4).map(|at| Op::Push(at as u8)).collect() // alloc: a test's script
}

/// Set for the child process the control below starts: which [`Plant`] its run has.
const PLANT_ENV: &str = "BLAZE_MEM_GROWTH_PLANT";
const THIS_TEST: &str = "vec::tests::pushes_past_the_limit_are_refused_and_the_process_goes_on";

/// Run as itself it is the sound case: four times the limit in pushes, each refused once the
/// arena is full, the process alive at the end. The control below runs it again in a child
/// process with the plant.
#[test]
fn pushes_past_the_limit_are_refused_and_the_process_goes_on() {
    let plant = match std::env::var(PLANT_ENV).as_deref() {
        Ok("push-through-alloc") => Plant::PushThroughAlloc,
        Ok(other) => panic!("{PLANT_ENV}={other}: not a plant of this test"),
        Err(_) => Plant::None,
    };
    let limit = 4096;
    let seen = run(&pushes_past(limit), limit, plant);
    assert!(seen.grown > 0 && seen.refused >= limit * 2, "the script passes the limit: {seen:?}");
    println!("survived: {seen:?}");
}

/// The cause (rule test-the-cause) and the control in one: with the push routed back to
/// `alloc`'s infallible `Vec::push`, the same script ends the process in
/// `handle_alloc_error` (SIGABRT, "memory allocation of .. bytes failed"); without the plant
/// the same child survives. A real process, because only the kernel shows an abort: nothing
/// in the process that aborted can report it.
#[cfg(unix)]
#[test]
fn the_oracle_catches_a_push_routed_to_allocs_infallible_one() {
    use std::os::unix::process::ExitStatusExt;

    let child = |plant: Option<&str>| {
        #[expect(clippy::disallowed_methods, reason = "the test binary run again as its own child")]
        let mut command = std::process::Command::new(std::env::current_exe().expect("this binary"));
        command.args([THIS_TEST, "--exact", "--nocapture", "--test-threads=1"]);
        match plant {
            Some(plant) => command.env(PLANT_ENV, plant),
            None => command.env_remove(PLANT_ENV),
        };
        command.output().expect("the child ran")
    };

    let sound = child(None);
    let said = std::string::String::from_utf8_lossy(&sound.stdout); // alloc: a test's output
    assert!(sound.status.success(), "without the plant the child survives: {:?}", sound.status);
    assert!(said.contains("survived: Seen"), "the child ran the script: {said}");

    let planted = child(Some("push-through-alloc"));
    let said = std::string::String::from_utf8_lossy(&planted.stderr); // alloc: a test's output
    assert_eq!(
        planted.status.signal(),
        Some(libc::SIGABRT),
        "with the plant the process is aborted: {:?}\n{said}",
        planted.status
    );
    assert!(
        said.contains("memory allocation of") && said.contains("failed"),
        "by alloc's handle_alloc_error: {said}"
    );
}

#[test]
fn an_insert_past_the_end_is_refused_by_position() {
    let frame = Frame::named(ARENA, 1 << 16, Tag::Other).expect("a frame");
    let mut bytes = crate::Vec::from_slice_in(&[1u8, 2, 3], &frame).expect("room");
    assert_eq!(bytes.insert(4, 9), Err(InsertError::PastEnd { index: 4, len: 3 }));
    assert_eq!(bytes.insert(3, 9), Ok(()));
    assert_eq!(bytes.insert(0, 0), Ok(()));
    assert_eq!(bytes, [0u8, 1, 2, 3, 9]);
    assert_eq!((bytes.remove(9), bytes.remove(1), bytes.swap_remove(0)), (None, Some(1), Some(0)));
    assert_eq!(bytes, [9u8, 2, 3]);
}

#[test]
fn format_into_writes_into_the_buffer_it_is_lent_and_says_what_a_small_one_needed() {
    let mut buffer = [0u8; 16];
    let at = buffer.as_ptr();
    let text = crate::format_into!(&mut buffer, "{}-{:03}", "unit", 7).expect("16 bytes hold it");
    assert_eq!((&*text, text.as_ptr()), ("unit-007", at), "the text is the buffer's own bytes");
    assert_eq!(&*crate::format_into!(&mut buffer, "plain").expect("room"), "plain");
    assert_eq!(&*crate::format_into!(&mut buffer, "{:16}", "").expect("an exact fit"), " ".repeat(16)); // alloc: a test's expectation
    assert_eq!(
        crate::format_into!(&mut buffer, "{}{}", "0123456789", "é日🦀本").map(|text| text.len()),
        Err(FormatError::Full(TooSmall { needed: 10 + "é日🦀本".len(), room: 16 })),
        "the count goes on past the end, so the caller knows what to lend"
    );
    assert_eq!(
        crate::format_into!(&mut [], "{}", 1).map(|text| text.len()),
        Err(FormatError::Full(TooSmall { needed: 1, room: 0 }))
    );
    let (_, used) = blaze_alloc_count::measure(|| {
        crate::format_into!(&mut buffer, "{}@{}", "syn", 2).map(|text| text.len())
    });
    assert_eq!(used.allocations, 0, "format_into! calls no allocator");
}

/// A `Display` that returns an error though its writer took everything.
struct Liar;

impl std::fmt::Display for Liar {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Err(std::fmt::Error)
    }
}

#[test]
fn a_formatting_trait_that_fails_on_its_own_is_an_error_not_a_panic() {
    let frame = Frame::named(ARENA, 1 << 16, Tag::Other).expect("a frame");
    assert_eq!(crate::format_in!(&frame, "a{}", Liar).map(|text| text.len()), Err(FormatError::Trait));
    assert_eq!(
        crate::format_into!(&mut [0u8; 8], "a{}", Liar).map(|text| text.len()),
        Err(FormatError::Trait)
    );
}

#[test]
fn sizes_and_offsets_are_checked_and_say_which_operation_did_not_fit() {
    let (page, file_end) = (ByteLen::new(4096), Offset::new(10_000));
    assert_eq!(page.plus(page), Ok(ByteLen::new(8192)));
    assert_eq!(page.times(3).and_then(|len| len.minus(ByteLen::new(1))), Ok(ByteLen::new(12_287)));
    assert_eq!(Offset::START.after(page), Ok(Offset::new(4096)));
    assert_eq!(file_end.since(Offset::new(4096)), Ok(ByteLen::new(5904)));
    assert_eq!(ByteLen::of(b"abc").in_memory(), Ok(3));
    // The planted overflows: each is an error naming the operation and its operands.
    assert_eq!(
        ByteLen::new(u64::MAX).plus(ByteLen::new(1)),
        Err(Overflow { what: "ByteLen::plus", left: u64::MAX, right: 1 })
    );
    assert_eq!(
        ByteLen::new(1).minus(ByteLen::new(2)),
        Err(Overflow { what: "ByteLen::minus", left: 1, right: 2 })
    );
    assert_eq!(
        ByteLen::new(1 << 40).times(1 << 40),
        Err(Overflow { what: "ByteLen::times", left: 1 << 40, right: 1 << 40 })
    );
    assert_eq!(
        Offset::new(u64::MAX - 1).after(ByteLen::new(2)),
        Err(Overflow { what: "Offset::after", left: u64::MAX - 1, right: 2 })
    );
    assert_eq!(
        Offset::new(5).since(Offset::new(6)),
        Err(Overflow { what: "Offset::since", left: 5, right: 6 })
    );
}

#[cfg(unix)]
#[test]
fn a_path_pushed_past_the_limit_is_left_whole() {
    let frame = Frame::named(ARENA, 256, Tag::Other).expect("a frame");
    let mut path = PathBuf::from_path_in("/work", &frame).expect("room");
    let long = "x".repeat(4096); // alloc: a test's input
    let refused = path.push(&long).expect_err("4096 bytes do not fit in 256");
    assert_eq!(refused.arena, ARENA);
    assert_eq!(path.as_path(), Path::new("/work"), "as it was");
    let refused = path.join(&long).map(|joined| joined.len()).expect_err("nor does the join");
    assert_eq!(refused.arena, ARENA);
}
