//! A stream on a process with one CPU (a container's quota, a pinned
//! process): the pool then has no worker, and on a CPU without SME2 no
//! thread at all to take tasks, so the caller's thread hashes every task
//! while it waits. Each kind of message completes with the one-message
//! digests. The test pins its process to one CPU
//! before the pool starts, so it is a binary of its own, with one test
//! (Linux, where the pool counts the process's affinity).
#![cfg(target_os = "linux")]

use blake3_servil::{Hash, Mode};

unsafe extern "C" {
    fn sched_setaffinity(pid: i32, cpusetsize: usize, mask: *const core::ffi::c_ulong) -> i32;
}

/// Input byte i is i % 251, as the official vectors define their inputs.
fn input(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// The one-message digest, which the official vectors and the library's
/// fixed values hold to official BLAKE3's.
fn reference(data: &[u8]) -> Hash {
    blake3_servil::convenience::hash(data)
}

#[test]
fn every_stream_completes_on_one_cpu() {
    // Pinned first, so that the pool starts with no worker.
    // A cpu_set_t is an array of unsigned longs, CPU n at bit n % 64 of
    // long n / 64 (a byte array would set CPU 56 on big-endian targets).
    let mask: [core::ffi::c_ulong; 1] = [1];
    // Sound: one long with CPU 0's bit, for this process.
    assert_eq!(unsafe { sched_setaffinity(0, core::mem::size_of_val(&mask), mask.as_ptr()) }, 0, "pin the process to CPU 0");
    assert_eq!(std::thread::available_parallelism().unwrap().get(), 1);
    blake3_servil::initialize_multithreaded(None);
    // Messages short (gathered into tasks) and long (pieces), borrowed,
    // written, and read; and batches small and large.
    let lens = [0, 64, 1000, 16 << 10, 64 << 10, (1 << 20) + 1, 5 << 20];
    let messages: Vec<Vec<u8>> = lens.iter().map(|&len| input(len)).collect();
    let in_blocks: Vec<Vec<blake3_servil::performance::Block>> = messages
        .iter()
        .map(|m| {
            let mut b = vec![blake3_servil::performance::Block::ZERO; m.len().div_ceil(64).max(1)];
            blake3_servil::performance::Block::bytes_mut(&mut b)[..m.len()].copy_from_slice(m);
            b
        })
        .collect();
    let batch = input(64 * 4096);
    let blocks: Vec<blake3_servil::performance::Block> = batch.chunks_exact(64).map(|b| blake3_servil::performance::Block(b.try_into().unwrap())).collect();
    let batch_inputs = [16, 4096].map(|count| blake3_servil::performance::Batch::from_blocks(&blocks[..count], 64));
    let mut want = Vec::new();
    let mut got = vec![Hash::from_bytes([0; 32]); 3 * messages.len() + 16 + 4096];
    let handed = std::cell::Cell::new(0);
    let (singles, batches) = got.split_at_mut(3 * messages.len());
    let (small, large) = batches.split_at_mut(16);
    let mut singles = singles.iter_mut();
    blake3_servil::performance::stream(
        Mode::Plain,
        |d: &mut [Hash]| handed.set(handed.get() + d.len()),
        |s| {
            for (message, blocks) in messages.iter().zip(&in_blocks) {
                s.submit(blocks, message.len(), singles.next().unwrap());
                s.submit_with(
                    message.len(),
                    |at, room| {
                        room.copy_from_slice(&message[at..][..room.len()]);
                        room.len()
                    },
                    singles.next().unwrap(),
                );
                s.submit_reader(&message[..], None, singles.next().unwrap()).unwrap();
                want.extend([reference(message); 3]);
            }
            for (count, input, out) in [(16, &batch_inputs[0], small), (4096, &batch_inputs[1], large)] {
                s.submit_batch(input, out);
                want.extend(batch[..64 * count].chunks(64).map(reference));
            }
            s.finish();
        },
    );
    assert_eq!(handed.get(), want.len(), "every place handed back");
    assert!(got == want, "every digest, in its place, on one CPU");

    // A caller with a few places that collects until one comes back (once,
    // collect only handed back what was done, and with no other thread to
    // hash, nothing ever was: it waited forever, fork issue #86).
    let mut places = vec![Hash::from_bytes([0; 32]); 4];
    let free = std::cell::RefCell::new(places.iter_mut().collect::<Vec<_>>());
    let cycled = std::cell::RefCell::new(Vec::new());
    blake3_servil::performance::stream(
        Mode::Plain,
        |d: &mut [Hash]| {
            cycled.borrow_mut().push(d[0]);
            free.borrow_mut().push(&mut d[0]);
        },
        |s| {
            for (message, blocks) in messages[..4].iter().zip(&in_blocks[..4]) {
                for _ in 0..8 {
                    let place = loop {
                        if let Some(place) = free.borrow_mut().pop() {
                            break place;
                        }
                        s.collect();
                    };
                    s.submit(blocks, message.len(), place);
                }
            }
            s.finish();
        },
    );
    let want: Vec<Hash> = messages[..4].iter().flat_map(|m| [reference(m); 8]).collect();
    assert!(cycled.into_inner() == want, "a few places cycled by collect, on one CPU");
}
