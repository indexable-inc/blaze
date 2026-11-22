//! Unit tests for `bstd::gpu`: capability model, hierarchical budget narrowing, explicit-allocator
//! `Buffer` / `DeviceBuffer` / `RenderTarget`, unified vs. discrete dirty tracking, deterministic
//! `Gpu::scope` compute dispatches, and cancellation/deadline aborts.

use std::time::Duration;

use super::{root, world};
use crate::{
    Deadline, Plant,
    gpu::{Backend, Gpu, GpuError, Grid, MemoryKind, ThreadId},
    mem::TASK_FRAME,
};

#[test]
fn gpu_providers_and_offered_preserve_capability_rules() {
    let cpu = Gpu::cpu();
    assert_eq!(cpu.backend(), Backend::Cpu);
    assert_eq!(cpu.memory_kind(), MemoryKind::Unified);
    assert_eq!(cpu.budget_bytes(), None);
    assert_eq!(cpu.used_bytes(), 0);
    assert_eq!(cpu.remaining_bytes(), None);
    assert_eq!(format!("{}", cpu.backend()), "cpu");
    assert_eq!(format!("{}", cpu.memory_kind()), "unified");

    let discrete = Gpu::cpu_discrete();
    assert_eq!(discrete.backend(), Backend::Cpu);
    assert_eq!(discrete.memory_kind(), MemoryKind::Discrete);
    assert_eq!(format!("{}", discrete.memory_kind()), "discrete");

    // Simulation never offers a hardware GPU.
    let sim = world(101, Plant::None);
    assert!(sim.gpu().is_none());
    let missing = sim.need_gpu().expect_err("sim has no hardware gpu");
    assert_eq!(missing.capability, "gpu");
    assert!(Gpu::offered(false).is_none());

    #[cfg(target_os = "macos")]
    {
        let native = Gpu::offered(true).expect("native macOS offers Metal Gpu");
        assert_eq!(native.backend(), Backend::Metal);
        assert_eq!(format!("{}", native.backend()), "metal");
        #[cfg(target_arch = "aarch64")]
        assert_eq!(native.memory_kind(), MemoryKind::Unified);
    }
    #[cfg(not(target_os = "macos"))]
    {
        assert!(Gpu::offered(true).is_none());
    }
}

#[test]
fn budget_narrowing_never_widens_and_accounts_hierarchically() {
    let io = world(102, Plant::None);
    let cx = root(&io);
    let root_gpu = Gpu::cpu();

    let parent = root_gpu.with_budget(1024).expect("narrowing from unlimited");
    assert_eq!(parent.budget_bytes(), Some(1024));
    assert_eq!(parent.remaining_bytes(), Some(1024));

    // Widening is strictly rejected.
    assert_eq!(
        parent.with_budget(1025).unwrap_err(),
        GpuError::BudgetWidened { current: 1024, requested: 1025 }
    );

    // Equal or smaller budget succeeds.
    let same = parent.with_budget(1024).expect("equal budget allowed");
    assert_eq!(same.budget_bytes(), Some(1024));

    let child = parent.with_budget(512).expect("tighter budget allowed");
    assert_eq!(child.budget_bytes(), Some(512));
    assert_eq!(
        child.with_budget(513).unwrap_err(),
        GpuError::BudgetWidened { current: 512, requested: 513 }
    );

    // Parent allocates 800 bytes (200 u32s).
    let p_buf = parent.buffer_zeroed_in::<u32, _>(200, cx.frame()).expect("800 <= 1024");
    assert_eq!(parent.used_bytes(), 800);
    assert_eq!(root_gpu.used_bytes(), 800);
    assert_eq!(child.used_bytes(), 0);
    assert_eq!(child.remaining_bytes(), Some(224));

    // Child asks for 300 bytes (75 u32s): fits child's 512 limit, fails parent's 1024 limit.
    // Child's own reservation must be rolled back cleanly!
    let over = child.buffer_zeroed_in::<u32, _>(75, cx.frame()).unwrap_err();
    assert_eq!(over, GpuError::BudgetExceeded { limit: 1024, used: 800, asked: 300 });
    assert_eq!(child.used_bytes(), 0);
    assert_eq!(parent.used_bytes(), 800);
    assert_eq!(root_gpu.used_bytes(), 800);

    // Child allocates 200 bytes (50 u32s): fits both child (512) and parent (1024).
    let c_buf = child.buffer_zeroed_in::<u32, _>(50, cx.frame()).expect("200 fits both");
    assert_eq!(child.used_bytes(), 200);
    assert_eq!(parent.used_bytes(), 1000);
    assert_eq!(root_gpu.used_bytes(), 1000);
    assert_eq!(child.remaining_bytes(), Some(24));

    // Cloned capability shares accounting and does not widen.
    let child_clone = child.clone();
    assert_eq!(child_clone.used_bytes(), 200);
    assert!(child_clone.buffer_zeroed_in::<u32, _>(7, cx.frame()).is_err());

    // Dropping buffers returns bytes across the whole chain.
    drop(c_buf);
    assert_eq!(child.used_bytes(), 0);
    assert_eq!(parent.used_bytes(), 800);
    assert_eq!(root_gpu.used_bytes(), 800);

    drop(p_buf);
    assert_eq!(parent.used_bytes(), 0);
    assert_eq!(root_gpu.used_bytes(), 0);
    assert_eq!(child.remaining_bytes(), Some(512));

    // Zero-byte budget blocks non-zero allocations but allows empty / ZST buffers.
    let zero = child.with_budget(0).expect("zero budget allowed");
    assert_eq!(
        zero.buffer_zeroed_in::<u8, _>(1, cx.frame()).unwrap_err(),
        GpuError::BudgetExceeded { limit: 0, used: 0, asked: 1 }
    );
    let empty = zero.buffer_zeroed_in::<u32, _>(0, cx.frame()).expect("empty buffer uses 0 bytes");
    assert!(empty.is_empty());
    let zst = zero.buffer_in::<(), _>(128, (), cx.frame()).expect("ZST buffer uses 0 bytes");
    assert_eq!(zst.len(), 128);
    assert_eq!(zst.byte_len(), 0);
}

#[test]
fn explicit_allocator_oom_and_coherence_tracking() {
    let io = world(103, Plant::None);
    let cx = root(&io);
    let mem = io.need_mem().expect("sim has mem");

    // Tiny arena triggers OutOfMemory cleanly and releases GPU budget reservation.
    let tiny = mem.arena("gpu.tiny", 64).expect("tiny arena");
    let gpu = Gpu::cpu().with_budget(4096).expect("budget");
    let oom = gpu.buffer_zeroed_in::<u64, _>(32, tiny.alloc()).unwrap_err();
    assert_eq!(oom, GpuError::OutOfMemory { asked: 256 });
    assert_eq!(gpu.used_bytes(), 0, "budget must roll back on allocator OOM");

    // Overflowing byte length fails cleanly with OutOfMemory.
    let overflow = gpu.buffer_zeroed_in::<u64, _>(usize::MAX, cx.frame()).unwrap_err();
    assert_eq!(overflow, GpuError::OutOfMemory { asked: u64::MAX });
    assert_eq!(gpu.used_bytes(), 0);

    // Unified memory never marks dirty ranges.
    let mut unified = gpu
        .buffer_from_slice_in(&[1u32, 2, 3, 4], cx.frame())
        .expect("unified buffer");
    assert_eq!(unified.memory_kind(), MemoryKind::Unified);
    assert_eq!(unified.byte_len(), 16);
    assert_eq!(unified.dirty_range(), None);
    unified.as_mut_slice()[1] = 20;
    assert_eq!(unified.dirty_range(), None);
    assert_eq!(unified.as_slice(), &[1, 20, 3, 4]);

    // Discrete memory tracks dirty ranges accurately on mutable host writes.
    let dgpu = Gpu::cpu_discrete();
    let mut discrete = dgpu.buffer_in(16, 7u32, cx.frame()).expect("discrete buffer");
    assert_eq!(discrete.memory_kind(), MemoryKind::Discrete);
    assert_eq!(discrete.dirty_range(), None);
    assert_eq!(discrete.as_slice()[0], 7);
    assert_eq!(discrete.dirty_range(), None);

    // Sub-range mutation merges dirty hull.
    discrete.as_mut_range(4..8).expect("in bounds")[0] = 99;
    assert_eq!(discrete.dirty_range(), Some((4, 8)));
    discrete.as_mut_range(10..14).expect("in bounds")[3] = 88;
    assert_eq!(discrete.dirty_range(), Some((4, 14)));
    // Empty or invalid sub-ranges do not widen dirty hull.
    assert!(discrete.as_mut_range(2..2).is_some());
    assert_eq!(discrete.dirty_range(), Some((4, 14)));
    assert!(discrete.as_mut_range(12..10).is_none());
    assert!(discrete.as_mut_range(0..17).is_none());
    assert_eq!(discrete.dirty_range(), Some((4, 14)));

    // Full mutable slice marks entire buffer dirty; mark_synced clears it.
    discrete.as_mut_slice()[0] = 1;
    assert_eq!(discrete.dirty_range(), Some((0, 16)));
    discrete.mark_synced();
    assert_eq!(discrete.dirty_range(), None);

    // Also verify Heap explicit allocator works seamlessly.
    let heap = blaze_own::testing::heap().alloc();
    let heap_buf = dgpu.buffer_from_slice_in(&[10i32, 20, 30], heap).expect("heap buffer");
    assert_eq!(heap_buf.as_slice(), &[10, 20, 30]);
}

#[test]
fn device_buffer_explicit_transfers_and_budget() {
    let io = world(104, Plant::None);
    let cx = root(&io);
    let gpu = Gpu::cpu_discrete().with_budget(128).expect("budget");

    let mut dev_a = gpu.device_buffer_zeroed_in::<u32, _>(8, cx.frame()).expect("32 bytes");
    let mut dev_b = gpu.device_buffer_in(8, 5u32, cx.frame()).expect("32 bytes");
    let dev_c = gpu
        .device_buffer_from_slice_in(&[1u32, 2, 3, 4], cx.frame())
        .expect("16 bytes");
    assert_eq!(gpu.used_bytes(), 80);
    assert_eq!(dev_a.len(), 8);
    assert_eq!(dev_a.byte_len(), 32);
    assert_eq!(dev_a.memory_kind(), MemoryKind::Discrete);

    // Upload, partial upload, device-to-device copy, download, partial download.
    dev_a.upload_from(&[10, 20, 30, 40, 50, 60, 70, 80]).expect("upload");
    dev_a.upload_range(2, &[300, 400]).expect("upload range");

    let mut host = [0u32; 8];
    dev_a.download_into(&mut host).expect("download");
    assert_eq!(host, [10, 20, 300, 400, 50, 60, 70, 80]);

    let mut sub = [0u32; 3];
    dev_a.download_range(4, &mut sub).expect("download range");
    assert_eq!(sub, [50, 60, 70]);

    dev_b.copy_from_device(&dev_a).expect("device copy");
    let mut host_b = [0u32; 8];
    dev_b.download_into(&mut host_b).expect("download b");
    assert_eq!(host_b, host);

    let mut host_c = [0u32; 4];
    dev_c.download_into(&mut host_c).expect("download c");
    assert_eq!(host_c, [1, 2, 3, 4]);

    // Mismatched lengths are typed errors, never panics.
    assert_eq!(
        dev_a.upload_from(&[1, 2, 3]).unwrap_err(),
        GpuError::LengthMismatch { expected: 8, actual: 3 }
    );
    assert_eq!(
        dev_a.download_into(&mut [0u32; 2]).unwrap_err(),
        GpuError::LengthMismatch { expected: 8, actual: 2 }
    );
    assert_eq!(
        dev_a.upload_range(7, &[1, 2]).unwrap_err(),
        GpuError::LengthMismatch { expected: 1, actual: 2 }
    );
    assert_eq!(
        dev_a.download_range(9, &mut [0u32; 1]).unwrap_err(),
        GpuError::LengthMismatch { expected: 0, actual: 1 }
    );

    drop(dev_a);
    drop(dev_b);
    drop(dev_c);
    assert_eq!(gpu.used_bytes(), 0);
}

#[test]
fn grid_validation_and_deterministic_scoped_compute_dispatch() {
    let io = world(105, Plant::None);
    let cx = root(&io);
    let gpu = Gpu::cpu();

    // Grid construction helpers & validation.
    let g1 = Grid::cover_1d(100, 32).expect("1d cover");
    assert_eq!(g1.groups, [4, 1, 1]);
    assert_eq!(g1.threads, [32, 1, 1]);
    assert_eq!(g1.total_groups(), 4);
    assert_eq!(g1.threads_per_group(), 32);
    assert_eq!(g1.total_threads(), 128);

    let g2 = Grid::cover_2d(10, 7, 4, 4).expect("2d cover");
    assert_eq!(g2.groups, [3, 2, 1]);
    assert_eq!(g2.threads, [4, 4, 1]);
    assert_eq!(g2.total_threads(), 96);

    assert!(matches!(Grid::new([0, 1, 1], [1, 1, 1]), Err(GpuError::InvalidGrid { .. })));
    assert!(matches!(Grid::new([1, 1, 1], [1, 0, 1]), Err(GpuError::InvalidGrid { .. })));
    assert!(matches!(Grid::new([u32::MAX, 1, 1], [2, 1, 1]), Err(GpuError::InvalidGrid { .. })));
    assert!(matches!(Grid::new([1 << 20, 1 << 20, 1 << 20], [16, 16, 16]), Err(GpuError::InvalidGrid { .. })));
    assert!(matches!(Grid::cover_1d(0, 8), Err(GpuError::InvalidGrid { .. })));
    assert!(matches!(Grid::cover_1d(8, 0), Err(GpuError::InvalidGrid { .. })));
    assert!(matches!(Grid::cover_2d(4, 0, 2, 2), Err(GpuError::InvalidGrid { .. })));

    // Deterministic 3D dispatch order + SAXPY compute kernel.
    let mut visited = Vec::new();
    let grid_3d = Grid::new([2, 2, 2], [2, 1, 1]).expect("3d grid");
    let stats = gpu
        .scope(&cx, |s| {
            // Manually built invalid grid is rejected by dispatch.
            let bad = Grid { groups: [0, 1, 1], threads: [1, 1, 1] };
            assert!(matches!(s.dispatch(bad, |_| {}), Err(GpuError::InvalidGrid { .. })));

            let st = s.dispatch(grid_3d, |tid| visited.push(tid))?;
            assert_eq!(s.dispatches(), 1);
            assert_eq!(s.total_threads_executed(), 16);
            Ok(st)
        })
        .expect("3d dispatch succeeds");

    assert_eq!(stats.groups_executed, 8);
    assert_eq!(stats.threads_executed, 16);
    assert_eq!(stats.backend, Backend::Cpu);
    assert_eq!(visited.len(), 16);
    assert_eq!(
        visited[0],
        ThreadId { group: [0, 0, 0], local: [0, 0, 0], global: [0, 0, 0] }
    );
    assert_eq!(
        visited[1],
        ThreadId { group: [0, 0, 0], local: [1, 0, 0], global: [1, 0, 0] }
    );
    assert_eq!(
        visited[2],
        ThreadId { group: [1, 0, 0], local: [0, 0, 0], global: [2, 0, 0] }
    );
    assert_eq!(
        visited[15],
        ThreadId { group: [1, 1, 1], local: [1, 0, 0], global: [3, 1, 1] }
    );

    // SAXPY: y[i] = 3 * x[i] + y[i] on explicit-allocator buffers.
    let n = 50usize;
    let x = gpu.buffer_in(n, 4u32, cx.frame()).expect("x");
    let mut y = gpu.buffer_in(n, 10u32, cx.frame()).expect("y");
    let grid = Grid::cover_1d(n as u32, 16).expect("saxpy grid");
    let saxpy_stats = gpu
        .scope(&cx, |s| {
            let x_slice = x.as_slice();
            let y_slice = y.as_mut_slice();
            s.dispatch(grid, |tid| {
                let i = tid.global[0] as usize;
                if i < n {
                    y_slice[i] = 3 * x_slice[i] + y_slice[i];
                }
            })
        })
        .expect("saxpy");
    assert_eq!(saxpy_stats.groups_executed, 4);
    assert_eq!(saxpy_stats.threads_executed, 64);
    assert!(y.as_slice().iter().all(|&v| v == 22));
}

#[test]
fn scope_dispatch_and_render_target_honor_cancellation_and_deadlines() {
    let io = world(106, Plant::None);
    let cx = root(&io);
    let gpu = Gpu::cpu();

    // 1. Pre-cancelled scope never enters closure.
    let mut entered = false;
    let _ = cx.scope_with(|s| {
        s.cancel();
        let t = s.may_overlap("gpu-pre-cancel", TASK_FRAME, |tcx| {
            let res = gpu.scope(tcx, |_| {
                entered = true;
                Ok(())
            });
            assert_eq!(res, Err(GpuError::Cancelled));
            Ok::<_, std::convert::Infallible>(())
        });
        let _ = t.join(s.cx());
    });
    assert!(!entered);

    // 2. Mid-dispatch cancellation stops between workgroups cleanly.
    let mut executed_groups = 0u32;
    let _ = cx.scope_with(|s| {
        let grid = Grid::new([100, 1, 1], [4, 1, 1]).expect("grid");
        let res = gpu.scope(s.cx(), |gs| {
            gs.dispatch(grid, |tid| {
                if tid.local[0] == 0 {
                    executed_groups += 1;
                }
                if tid.group[0] == 2 && tid.local[0] == 3 {
                    s.cancel();
                }
            })
        });
        assert_eq!(res, Err(GpuError::Cancelled));
    });
    assert_eq!(executed_groups, 3, "only workgroups 0, 1, 2 should have executed");

    // 3. Expired deadline aborts dispatch with GpuError::Deadline.
    let expired = cx.until(Deadline::at(Duration::ZERO));
    io.with_sim(|sim| {
        sim.advance(Duration::from_millis(10));
    });
    let deadline_err = gpu.scope(&expired, |_| Ok(())).unwrap_err();
    assert_eq!(deadline_err, GpuError::Deadline);
}

#[test]
fn offscreen_render_target_operations_budget_and_cancellation() {
    let io = world(107, Plant::None);
    let cx = root(&io);
    let gpu = Gpu::cpu().with_budget(256).expect("256-byte budget");

    // Invalid dimensions rejected.
    assert_eq!(
        gpu.render_target_in(0, 8, cx.frame()).unwrap_err(),
        GpuError::InvalidDimensions { width: 0, height: 8 }
    );
    assert_eq!(
        gpu.render_target_in(8, 0, cx.frame()).unwrap_err(),
        GpuError::InvalidDimensions { width: 8, height: 0 }
    );
    assert_eq!(
        gpu.render_target_in(u32::MAX, u32::MAX, cx.frame()).unwrap_err(),
        GpuError::InvalidDimensions { width: u32::MAX, height: u32::MAX }
    );

    // Explicit allocator OOM rolls back render target budget reservation.
    let mem = io.need_mem().expect("sim has mem");
    let tiny = mem.arena("gpu.rt.tiny", 32).expect("tiny arena");
    assert_eq!(
        gpu.render_target_in(4, 4, tiny.alloc()).unwrap_err(),
        GpuError::OutOfMemory { asked: 64 }
    );
    assert_eq!(gpu.used_bytes(), 0);

    // Budget enforcement: 8x8 RGBA8 = 256 bytes fits; 9x8 = 288 exceeds.
    assert_eq!(
        gpu.render_target_in(9, 8, cx.frame()).unwrap_err(),
        GpuError::BudgetExceeded { limit: 256, used: 0, asked: 288 }
    );

    let mut rt = gpu.render_target_in(8, 8, cx.frame()).expect("8x8 target");
    assert_eq!(rt.width(), 8);
    assert_eq!(rt.height(), 8);
    assert_eq!(rt.byte_len(), 256);
    assert_eq!(rt.pixels().len(), 256);
    assert_eq!(gpu.used_bytes(), 256);
    assert_eq!(rt.get_pixel(0, 0), Some([0, 0, 0, 0]));
    assert_eq!(rt.get_pixel(8, 0), None);
    assert_eq!(rt.get_pixel(0, 8), None);

    rt.clear([10, 20, 30, 255]);
    assert_eq!(rt.get_pixel(5, 7), Some([10, 20, 30, 255]));
    assert!(rt.set_pixel(3, 4, [255, 0, 128, 255]));
    assert!(!rt.set_pixel(8, 4, [1, 2, 3, 4]));
    assert_eq!(rt.get_pixel(3, 4), Some([255, 0, 128, 255]));

    // Deterministic fill_shader pass.
    rt.fill_shader(&cx, |x, y| [(x * 16) as u8, (y * 16) as u8, 42, 255])
        .expect("fill_shader");
    assert_eq!(rt.get_pixel(3, 5), Some([48, 80, 42, 255]));
    assert_eq!(rt.pixels_mut().len(), 256);

    // Mid-shader cancellation stops between rows.
    let mut rows_touched = 0u32;
    let _ = cx.scope_with(|s| {
        let res = rt.fill_shader(s.cx(), |x, y| {
            if x == 0 {
                rows_touched += 1;
            }
            if y == 1 && x == 7 {
                s.cancel();
            }
            [1, 2, 3, 4]
        });
        assert_eq!(res, Err(GpuError::Cancelled));
    });
    assert_eq!(rows_touched, 2, "only rows 0 and 1 should run before cancel takes effect");

    drop(rt);
    assert_eq!(gpu.used_bytes(), 0);
}

#[test]
fn gpu_error_display_and_debug_formatting() {
    let errors = [
        GpuError::BudgetExceeded { limit: 100, used: 80, asked: 40 },
        GpuError::BudgetWidened { current: 100, requested: 200 },
        GpuError::OutOfMemory { asked: 4096 },
        GpuError::InvalidGrid { groups: [0, 1, 1], threads: [1, 1, 1] },
        GpuError::InvalidDimensions { width: 0, height: 10 },
        GpuError::LengthMismatch { expected: 8, actual: 3 },
        GpuError::Cancelled,
        GpuError::Deadline,
    ];
    for err in errors {
        let msg = format!("{err}");
        assert!(!msg.is_empty());
        let dbg = format!("{err:?}");
        assert!(!dbg.is_empty());
    }
    let gpu = Gpu::cpu();
    assert!(format!("{gpu:?}").contains("Cpu"));
}
