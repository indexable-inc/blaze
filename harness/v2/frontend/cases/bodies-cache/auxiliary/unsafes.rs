//@ edition: 2021
pub fn swap_raw(a: &mut i32, b: &mut i32) { unsafe { let pa: *mut i32 = a; let pb: *mut i32 = b; let t = *pa; *pa = *pb; *pb = t; } }
pub fn sum_ptr(xs: &[u16]) -> u32 { let mut p = xs.as_ptr(); let mut t = 0u32; for _ in 0..xs.len() { unsafe { t += *p as u32; p = p.add(1); } } t }
