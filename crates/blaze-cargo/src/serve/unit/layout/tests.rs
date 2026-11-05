use blaze_codegen::InstanceKey;
use blaze_ids::ContentId;
use blaze_link::Heat;
use ix_prof::{
    Profile,
    profile::{CpuSample, Frame, FrameKind, Interner, Thread},
};

use super::{Counts, DIR, Profiles, Weights, hex};

fn key(n: u8) -> InstanceKey {
    InstanceKey(ContentId([n; 32]))
}

fn identity(n: u8) -> ContentId {
    ContentId([n + 100; 32])
}

fn frame(build: &str, address: u64, inline_depth: u16) -> Frame {
    Frame {
        build_id: Some(build.to_owned()),
        address: Some(address),
        inline_depth,
        ..Frame::named("f", FrameKind::Native)
    }
}

/// A profile read through the map of the build it ran: each sample's leaf is the function
/// whose code holds its address, the functions inlined at that address one frame with it, its
/// caller the next machine frame; a sample of another build or of no function is counted and
/// read as nothing. Each sample weighs the CPU time it stands for, so samples taken at two
/// rates add up by time. A later build's instances of those functions take their weight.
#[test]
fn a_sampled_address_names_the_function_its_build_put_there() {
    let target = tempfile::tempdir().unwrap();
    std::fs::create_dir(target.path().join(DIR)).unwrap();
    let image = "/w/target/release/deps/app-0f";
    let id = [7; 16];
    let profiles = Profiles::read(target.path()).unwrap().expect("a layout directory");
    let functions = [(key(1), 0x1000, 0x100), (key(2), 0x1100, 0x80), (key(3), 0x2000, 0x10)];
    let identities = [(key(3), identity(3)), (key(1), identity(1)), (key(2), identity(2))];
    profiles.write_map(image, id, &functions, &identities).unwrap();

    let (ours, theirs) = (hex(&id), "ab".repeat(16));
    let mut interner = Interner::default();
    let caller = interner.frame(frame(&ours, 0x1010, 0));
    let leaf = interner.frame(frame(&ours, 0x1120, 0));
    let inlined = interner.frame(frame(&ours, 0x1120, 1));
    let other = interner.frame(frame(&theirs, 0x1120, 0));
    let nowhere = interner.frame(frame(&ours, 0x3000, 0));
    let deep = interner.stack([caller, leaf, inlined]).unwrap();
    let elsewhere = interner.stack([other]).unwrap();
    let outside = interner.stack([nowhere]).unwrap();
    let sample = |stack, on_cpu, weight_ns| CpuSample {
        process: 1,
        thread: 1,
        t_ns: 0,
        stack,
        span_id: None,
        on_cpu,
        weight_ns,
    };
    let profile = Profile {
        threads: vec![Thread {
            process: 1,
            process_name: "app".into(),
            thread: 1,
            thread_name: None,
            cpu_ns: None,
        }],
        frames: interner.frame_rows,
        stacks: interner.stack_rows,
        cpu_samples: vec![
            // Two at 1 kHz, one at 250 Hz: 6 ms on the CPU, not 3 samples' worth.
            sample(deep, true, 1_000_000),
            sample(deep, true, 1_000_000),
            sample(deep, true, 4_000_000),
            sample(deep, false, 1_000_000),
            sample(elsewhere, true, 1_000_000),
            sample(outside, true, 1_000_000),
        ],
        ..Profile::default()
    };
    std::fs::write(target.path().join(DIR).join("app-1.ixprof"), profile.to_ixprof().unwrap())
        .unwrap();

    let profiles = Profiles::read(target.path()).unwrap().unwrap();
    let weights = profiles.weights(image).expect("the image's weights");
    assert_eq!(
        *weights,
        Weights::new(vec![(identity(2), 6_000_000)], vec![(identity(1), identity(2), 6_000_000)])
    );
    assert_eq!(profiles.counts, Counts { files: 1, samples: 5, unmapped: 1, outside: 1 });
    assert!(profiles.weights("/w/target/release/deps/other").is_none());

    // The next build: 2's body edited (another key, one identity), 1 gone.
    let next = [(key(12), identity(2)), (key(13), identity(3))];
    assert_eq!(weights.heat(&next), Heat::new([(key(12), 6_000_000)], []));
}
