//! What the linker writes a jump-table program and the program answers, through
//! blaze-live's `patch`: `Message::view` (the runtime reads every message in place with it),
//! `Message::decode`, `Answer::decode` and `Descriptor::decode`, each given the whole input.
//! The program trusts no byte of a message: it comes from another process.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - No reader panics, on any bytes, nor does walking every list of a view.
//! - `view` allocates nothing for a message it accepts, and asks for at most
//!   `blaze_fuzz::allowed(len)` bytes at once for one it refuses (its error).
//! - Every write's bytes and every function's name a view gives are bytes of the message.
//! - `decode` is `view` then `to_message`, and a message either reads encodes back to
//!   exactly the bytes read; so does an answer, and a descriptor its first `SIZE` bytes.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_fuzzer::Target;
use blaze_live::patch::{
    ARCH_X86_64, Answer, Code, DESCRIPTOR_MAGIC, DESCRIPTOR_VERSION, Descriptor, Message,
    MessageView, Patch,
};

const TARGET: Target = Target {
    name: "live_message",
    check,
    seeds,
    dictionary: &[
        b"BLZLMSG2",
        b"BLZLANS2",
        b"BLZLIVE2",
        b"\x01\x00\x00\x00",
        b"\x00\x00\x00\x00",
        b"\x02\x00\x00\x00",
        b"\xff\xff\xff\xff",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(300),
};

/// Every entry of every list of `view`, checked to be the message's: how many there are.
fn walk(message: &[u8], view: &MessageView<'_>) -> usize {
    let MessageView::Patch(patch) = *view else { return 0 };
    let inside = message.as_ptr_range();
    let within = |bytes: &[u8]| {
        let range = bytes.as_ptr_range();
        inside.start <= range.start && range.end <= inside.end
    };
    let entries = patch.code.len() + patch.data.len();
    let mut seen = 0;
    for code in patch.code {
        assert!(within(code.name.as_bytes()), "a function's name is the message's");
        seen += 1;
    }
    for (_, bytes) in patch.data {
        assert!(within(bytes), "a write's bytes are the message's");
        seen += 1;
    }
    assert_eq!(seen, entries, "the code and write lists give as many entries as they say");
    let lists =
        [patch.rebase().len(), patch.cells().len(), patch.frames().len(), patch.windows().len()];
    let walked = [
        patch.rebase().count(),
        patch.cells().count(),
        patch.frames().count(),
        patch.windows().count(),
    ];
    assert_eq!(walked, lists, "a list gives as many entries as it says");
    seen + walked.iter().sum::<usize>()
}

fn check(data: &[u8]) {
    let (most, viewed) = blaze_fuzz::largest_allocation(|| {
        Message::view(data).map(|view| (walk(data, &view), view))
    });
    match &viewed {
        Ok(_) => assert_eq!(most, 0, "a message read in place allocates nothing"),
        Err(_) => assert!(
            most <= blaze_fuzz::allowed(data.len()),
            "refusing {} bytes asked for {most} at once",
            data.len()
        ),
    }
    let decoded = Message::decode(data);
    match (&viewed, &decoded) {
        (Ok((_, view)), Ok(message)) => {
            assert_eq!(&view.to_message(), message, "decode is the view's message");
            assert_eq!(message.encode(), data, "a message read encodes to its bytes");
            assert_eq!(message.encoded_len(), data.len());
        }
        (Err(a), Err(b)) => assert_eq!(a, b, "decode refuses as view does"),
        _ => panic!(
            "view and decode disagree: {:?} and {:?}",
            viewed.as_ref().map(|it| it.0),
            decoded
        ),
    }
    if let Ok(answer) = Answer::decode(data) {
        assert_eq!(answer.encode(), data, "an answer read encodes to its bytes");
        assert_eq!(answer.encoded_len(), data.len());
    }
    if let Some(descriptor) = Descriptor::decode(data) {
        assert_eq!(
            descriptor.encode(),
            &data[..Descriptor::SIZE],
            "a descriptor read encodes to its bytes"
        );
    }
}

/// Each kind of message, answer and descriptor as the linker and program write them, and the
/// ways bytes are not one.
fn seeds() -> Vec<Vec<u8>> {
    let patch = Patch {
        id: [7; 16],
        generation: 2,
        base: 1,
        code: vec![Code::of(0x4000, &[0x90; 16], "life::step"), Code::of(0x4010, &[], "ünï")],
        data: vec![(0x8000, 0x4000u64.to_le_bytes().to_vec())],
        rebase: vec![0x8000],
        cells: vec![(0, 0x4000), (3, 0x4008)],
        frames: vec![(0x4000, 16)],
        windows: vec![(0x4000, 0x4010, 0x4008)],
        functions: 2,
        statics: 1,
    };
    let messages = [
        Message::Patch(patch),
        Message::Patch(Patch::default()),
        Message::RollBack { id: [7; 16], to: 1 },
        Message::Restart { id: [7; 16], generation: 2, why: "a layout changed: ünï".into() },
    ];
    let answers = [
        Answer::Applied { generation: 2 },
        Answer::RolledBack { generation: 2, to: 1, why: "panicked".into() },
        Answer::Declined { generation: 3, why: String::new() },
    ];
    let descriptor = Descriptor {
        magic: DESCRIPTOR_MAGIC,
        version: DESCRIPTOR_VERSION,
        arch: ARCH_X86_64,
        id: [7; 16],
        generation: 1,
        this: 0x1000,
        table: 0x2000,
        slots: 4,
        patch_code: [0x4000, 0x8000],
        patch_data: [0x8000, 0xc000],
    };
    let mut seeds: Vec<Vec<u8>> = messages.iter().map(Message::encode).collect();
    seeds.extend(answers.iter().map(Answer::encode));
    seeds.push(descriptor.encode());
    let whole = messages[0].encode();
    seeds.push(whole[..whole.len() - 1].to_vec());
    seeds.push([&whole[..], &[0]].concat());
    // The kind word, after the 8-byte magic and the 4-byte version.
    let mut unknown = messages[2].encode();
    unknown[12] = 9;
    seeds.push(unknown);
    seeds
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }
}
