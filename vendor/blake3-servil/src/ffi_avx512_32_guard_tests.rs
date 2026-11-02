// Included inside ffi_avx512_32::tests (Linux x86-64 only).
// These mappings make over/under-reads and writes fault, rather than hide
// in allocator slack. Input, key and pointer-table pages become read-only.
unsafe extern "C" {
    fn mmap(addr: *mut core::ffi::c_void, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut core::ffi::c_void;
    fn mprotect(addr: *mut core::ffi::c_void, len: usize, prot: i32) -> i32;
    fn munmap(addr: *mut core::ffi::c_void, len: usize) -> i32;
    fn getpagesize() -> i32;
}

struct Guarded {
    base: *mut u8,
    page: usize,
    size: usize,
    at_end: bool,
}

impl Guarded {
    fn new(size: usize, at_end: bool) -> Self {
        let page = usize::try_from(unsafe { getpagesize() }).unwrap();
        assert!(size > 0 && size <= page);
        let total = page.checked_mul(3).unwrap();
        // Linux: PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS. Only the middle
        // page is accessible; tested objects sit against either guard.
        let base = unsafe { mmap(core::ptr::null_mut(), total, 0, 2 | 0x20, -1, 0) }.cast::<u8>();
        assert_ne!(base as usize, usize::MAX, "mmap failed");
        assert!(!base.is_null());
        let result = Self { base, page, size, at_end };
        assert_eq!(unsafe { mprotect(base.add(page).cast(), page, 1 | 2) }, 0);
        unsafe { core::ptr::write_bytes(result.ptr(), 0xa5, size) };
        result
    }

    fn ptr(&self) -> *mut u8 {
        unsafe { self.base.add(self.page + if self.at_end { self.page - self.size } else { 0 }) }
    }

    fn read_only(&mut self) {
        assert_eq!(unsafe { mprotect(self.base.add(self.page).cast(), self.page, 1) }, 0);
    }

    fn bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.ptr(), self.size) }
    }
}

impl Drop for Guarded {
    fn drop(&mut self) {
        assert_eq!(unsafe { munmap(self.base.cast(), 3 * self.page) }, 0);
    }
}

#[test]
fn guarded_input_key_pointer_table_and_output_bounds() {
    if !crate::platform::avx512_detected() {
        return;
    }
    for width in [16, 32] {
        guarded_kernel_bounds(width);
    }
}

fn guarded_kernel_bounds(width: usize) {
    assert!(crate::platform::avx512_detected() && matches!(width, 16 | 32));
    for at_end in [false, true] {
        for blocks in [1, 16] {
            for last_len in [0, 63, 64] {
                let n = blocks * BLOCK_LEN;
                let inputs: Vec<_> = (0..width)
                    .map(|i| {
                        let mut input = Guarded::new(n, at_end);
                        let bytes = unsafe { core::slice::from_raw_parts_mut(input.ptr(), n) };
                        for (j, b) in bytes.iter_mut().enumerate() {
                            *b = (13 * j + 17 * i) as u8;
                        }
                        bytes[(blocks - 1) * BLOCK_LEN + last_len..].fill(0);
                        input.read_only();
                        input
                    })
                    .collect();
                let mut table = Guarded::new(width * core::mem::size_of::<*const u8>(), at_end);
                for (i, input) in inputs.iter().enumerate() {
                    unsafe { table.ptr().cast::<*const u8>().add(i).write(input.ptr()) };
                }
                table.read_only();
                let mut key = Guarded::new(32, at_end);
                unsafe { core::ptr::copy_nonoverlapping(crate::IV.as_ptr().cast::<u8>(), key.ptr(), 32) };
                key.read_only();
                let out = Guarded::new(width * OUT_LEN, at_end);
                let counter = u64::MAX - 15;
                let packed = 16 | (1 << 8) | (2 << 16) | ((last_len as u64) << 24);
                for inc in [false, true] {
                    if width == 16 && inc {
                        continue;
                    }
                    let mut want = vec![0u8; width * OUT_LEN];
                    for (i, input) in inputs.iter().enumerate() {
                        let mut cv = *crate::IV;
                        let c = counter.wrapping_add(if inc { i as u64 } else { 0 });
                        for (b, block) in input.bytes().chunks_exact(BLOCK_LEN).enumerate() {
                            crate::platform::Platform::SSE41.compress_in_place(
                                &mut cv,
                                block.try_into().unwrap(),
                                if b + 1 == blocks { last_len as u8 } else { BLOCK_LEN as u8 },
                                c,
                                16 | if b == 0 { 1 } else { 0 } | if b + 1 == blocks { 2 } else { 0 },
                            );
                        }
                        for w in 0..8 {
                            want[i * OUT_LEN + 4 * w..][..4].copy_from_slice(&cv[w].to_le_bytes());
                        }
                    }
                    unsafe {
                        core::ptr::write_bytes(out.ptr(), 0xa5, width * OUT_LEN);
                        let kernel = if width == 16 {
                            blake3_servil_avx512_p16
                        } else if inc {
                            blake3_servil_avx512_k32
                        } else {
                            blake3_servil_avx512_p32
                        };
                        kernel(table.ptr().cast(), blocks as u64, key.ptr().cast(), counter, packed, out.ptr());
                    }
                    assert_eq!(out.bytes(), &want, "width={width} at_end={at_end} blocks={blocks} last_len={last_len} increment={inc}");
                }
            }
        }
    }
}
