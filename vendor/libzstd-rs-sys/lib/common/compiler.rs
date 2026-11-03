pub(crate) const CACHELINE_SIZE: core::ffi::c_int = 64;

#[expect(dead_code)]
pub enum Locality {
    L3 = 1,
    L2 = 2,
    L1 = 3,
}

#[inline(always)]
pub(crate) fn prefetch_area<T>(ptr: *const T, bytes: usize) {
    for pos in (0..bytes).step_by(CACHELINE_SIZE as usize) {
        prefetch_read_data(ptr.wrapping_byte_add(pos), Locality::L2);
    }
}

#[inline(always)]
pub(crate) fn prefetch_val<T>(ptr: *const T) {
    prefetch_area(ptr, size_of::<T>())
}

pub fn prefetch_read_data<T>(data: *const T, locality: Locality) {
    // The match ensures that the locality argument is a constant value, as required.

    // SAFETY: the prefetch intrinsics do not modify the behavior of the program. They cannot trap
    // and do not produce a value. Hence it is safe to provide an arbitrary pointer.
    unsafe {
        match locality {
            Locality::L3 => prefetch_read_data_internal::<_, 1>(data),
            Locality::L2 => prefetch_read_data_internal::<_, 2>(data),
            Locality::L1 => prefetch_read_data_internal::<_, 3>(data),
        }
    }
}

unsafe fn prefetch_read_data_internal<T, const LOCALITY: i32>(ptr: *const T) {
    cfg_select! {
        feature = "no-prefetch" => {
            let _ = ptr;
        }
        feature = "nightly" => {
            use core::hint::Locality;

            core::hint::prefetch_read(
                ptr,
                const {
                    match LOCALITY {
                        1 => Locality::L3,
                        2 => Locality::L2,
                        3 => Locality::L1,
                        _ => panic!(),
                    }
                },
            )
        }
        target_arch = "x86_64" => {
            use core::arch::x86_64;
            x86_64::_mm_prefetch(ptr as *const i8, LOCALITY)
        }
        target_arch = "x86" => {
            use core::arch::x86;
            unsafe { x86::_mm_prefetch(ptr as *const i8, LOCALITY) };
        }
        target_arch = "aarch64" => {
            core::arch::asm!(
                "prfm {op}, [{addr}]",
                op = const {
                    match LOCALITY {
                        0 => 0b00000, // pldl1strm
                        1 => 0b00001, // pldl1keep
                        2 => 0b00010, // pldl2keep
                        3 => 0b00011, // pldl3keep
                        _ => panic!(),
                    }
                },
                addr = in(reg) ptr,
                options(nostack, preserves_flags)
            );
        }
        _ => {
            let _ = ptr;
        }
    }
}
