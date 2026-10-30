// Found by fuzz.py: seed smoke-0, case c19, edition 2024, backend llvm.
// differs: checksum
// Reduced. Again: fuzz.py check differs-checksum-smoke-0-c19.rs
// rustc:
// c19 49c1fa10c2ec037d ok
// ra-jit:
// c19 0342fed9db0d2dfd ok
mod c19 {
    use super::*;
    pub fn run() {
            #[inline(never)] fn spread(a: u8, b: i128, c: u64, d: i128, e: u32, f: i128, g: i128) -> (i128, u8, i128) { (b.wrapping_add(d) ^ f, a.wrapping_add(c as u8).wrapping_add(e as u8), g.wrapping_sub(b)) }
            #[inline(never)] extern "C" fn c_side(a: i128, b: u8, c: i128) -> i128 { a.wrapping_mul(3).wrapping_add(c) ^ b as i128 }
            #[inline(never)] fn by_ref(a: &i128, b: &mut i128) -> i128 { *b = b.wrapping_add(*a); *a ^ *b }
            #[derive(Clone, Copy)] struct Wide { tag: u8, value: i128, pair: [i128; 2] }
            #[inline(never)] fn pass(w: Wide, extra: Option<i128>) -> Wide { Wide { tag: w.tag.wrapping_add(1), value: w.pair[1] ^ extra.unwrap_or(1), pair: [w.value, w.pair[0]] } }
            let (a, b): (i128, i128) = (black_box(((18446744073709551557) as u128 as i128).wrapping_neg()), black_box((((u128::MAX / 3)) as u128 as i128).wrapping_neg()));
            let got = spread(1, a, 2, b, 3, a ^ b, !a); got.0.mix(); got.1.mix(); got.2.mix();
        let v78: [Box<f64>; 2] = (match (Ok::<Box<i128>, Box<i128>>(Box::new((-18i128)))) { Ok(p81) => [Box::new(f64::MIN_POSITIVE), Box::new(f64::MIN)], _ => [Box::new((-55.8273f64)), Box::new((-9223372036854775808.0f64))] });
    }
}
