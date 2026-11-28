//! Zero-alloc typed linear randomness requests (`blaze_core::request::FillRandom`) over
//! caller-owned [`DmaBuf`] buffers.

#![blaze::sans_io]

pub use blaze_core::request::{
    BufError, ByteLen, DmaBuf, FillRandom, FillRandomOpts, FsError,
};

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use blaze_core::{
        handle::{ExitKind, TaskId},
        request::SimHandler,
        sim::{Faults, Sim},
    };
    use blaze_own::{Erased, Inline, testing::heap, with_brand};

    use super::{ByteLen, DmaBuf, FillRandom, FillRandomOpts};
    use crate::{
        io::{Io, Limits, SimWorld},
        mem::TASK_FRAME,
    };

    fn fill_random_with_seed(seed: u64) -> [u8; 32] {
        let h = heap();
        let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
        let cx = io.cx(Duration::from_secs(10), TASK_FRAME).expect("root cx");
        let mut sim = Sim::new(seed, Faults::default());

        // Caller DMA buffer lives strictly on a stack `Inline<256>` allocator (zero global alloc).
        let inline = Inline::<256>::new();
        let buf_alloc = Erased::new(&inline);
        let mut out = [0u8; 32];

        with_brand(|brand| {
            let mut handler = SimHandler::new(&mut sim, TaskId(1), brand, h.alloc());
            let mut buf = DmaBuf::with_capacity_in(ByteLen(32), buf_alloc.handle());
            buf.resize_zero(32);
            assert_eq!(
                inline.used(),
                32,
                "caller DmaBuf must reside strictly in stack Inline buffer"
            );

            let filled = cx
                .run("bstd.rng.fill", &mut handler, FillRandom::new(FillRandomOpts { buf }))
                .expect("handler perform fill_random")
                .expect("fill_random ok");

            assert_eq!(filled.len(), 32);
            assert_eq!(
                inline.used(),
                32,
                "FillRandom must fill caller DmaBuf in place with zero extra allocation"
            );
            out.copy_from_slice(filled.as_slice());
            filled.release();
            assert_eq!(inline.used(), 0, "releasing DmaBuf reclaims all stack Inline bytes");

            assert_eq!(handler.finish_task(ExitKind::Normal), Ok(0));
        });

        out
    }

    #[test]
    fn cx_run_fill_random_is_deterministic_across_same_seed_sims_without_global_buf_alloc() {
        const SEED: u64 = 9001;
        let first = fill_random_with_seed(SEED);
        let second = fill_random_with_seed(SEED);
        let other = fill_random_with_seed(SEED + 1);

        assert_eq!(first, second, "same seed must yield byte-for-byte identical random bytes");
        assert_ne!(first, [0u8; 32], "random bytes must not stay all zeros");
        assert_ne!(first, other, "distinct seeds must yield distinct random streams");
    }
}
