// Found by edits.py: seed E5-15-s9-cranelift, edition 2021, backend cranelift.
// The edit (regen.constants): c5 from constants
// Again: edits.py check '{"files": ["../edits-open/std-panic-path.rs"]}' (open, see FUZZ.md)
// differs in c5: panic location
// rustc:
// panic: panicked at /home/ubuntu/.rustup/toolchains/1.99.0-x86_64-unknown-linux-gnu/lib/rustlib/src/rust/library/core/src/num/mod.rs:1290:5:
// attempt to exponentiate with overflow
// panic: panicked at src/main.rs:362:9
// panic: panicked at src/main.rs:362:9
// panic: panicked at src/main.rs:362:9
// panic: panicked at src/main.rs:362:9
// panic: panicked at src/main.rs:362:9
// panic: panicked at src/main.rs:362:9
// panic: panicked at src/main.rs:362:9
// panic: panicked at src/main.rs:362:9
// panic: panicked at src/main.rs:362:9
// c5 a6495cd75f834ebc ok
// blaze:
// panic: panicked at /rustc/b940084d7eb6a299eb4bfeb8e34901bc051e7ac4/library/core/src/num/mod.rs:1290:5:
// attempt to exponentiate with overflow
// panic: panicked at src/main.rs:362:9
// panic: panicked at src/main.rs:362:9
// panic: panicked at src/main.rs:362:9
// panic: panicked at src/main.rs:362:9
// panic: panicked at src/main.rs:362:9
// panic: panicked at src/main.rs:362:9
// panic: panicked at src/main.rs:362:9
// panic: panicked at src/main.rs:362:9
// panic: panicked at src/main.rs:362:9
// c5 a6495cd75f834ebc ok
mod c5 {
    use super::*;
    #[derive(Clone, Debug, PartialEq, PartialOrd, Copy, Eq, Ord, Hash)]
    #[repr(align(2))]
    pub struct S1 { pub f0: bool }
    impl Mix for S1 {
        fn mix(&self) {
            feed(1);
            self.f0.mix();
        }
    }
    #[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord, Hash)]
    pub struct S2 { pub f0: S1 }
    impl Mix for S2 {
        fn mix(&self) {
            feed(1);
            self.f0.mix();
        }
    }
    impl Drop for S2 {
        fn drop(&mut self) {
            feed(0x20895f6b);
            self.mix();
        }
    }
    const K3: f64 = (-725.2377f64);
    const K4: f64 = 2.0;
    #[inline(never)]
    pub fn f5(a6: &str, a7: &str, a8: &f64, a9: &mut bool, a10: S1, a11: bool, a12: &str, a13: S2) -> ((bool, &'static str), S1) {
        Ok::<Option<((bool, &'static str), S1)>, bool>(Some(((false, "3.5"), S1 { f0: false }))).unwrap_or_else(|x15: bool| -> Option<((bool, &'static str), S1)> { None::<((bool, &'static str), S1)> }).unwrap()
    }
    pub fn run() {
        (if (false) { S1 { f0: true } } else { S1 { f0: true } }).mix();
        let v16: S2 = S2 { f0: S1 { f0: true } };
        let v17: S2 = (if (matches!([["-7", "0123456789", "a", "line\n"], ["-7", "", "😀", "  pad  "]], ([.., [_, _, _, _]]))) { (match (String::from("héllo").as_str()) { "3.5" if true => S2 { f0: S1 { f0: false } }, _ => S2 { f0: S1 { f0: false } } }) } else { (if (true) { v16.clone() } else { v16.clone() }) });
        let mut v18: S2 = twice::<S2>(&('b19: { if (false) { break 'b19 v16.clone(); } v16.clone() })).1;
        let v20: i8 = (-20i8);
        let mut v24: bool = (sel::<f64>(false, (-9223372036854775808.0f64), 3.25f64) >= (if (true) { K4 } else { f64::MIN }));
        {
            let mut f25 = |a21: [[&'static str; 4]; 2], a22: ((bool, &'static str), S1), a23: bool| -> S2 { v24 = (!Some(S2 { f0: S1 { f0: false } }).is_some()); apply(|x26: f32| -> S2 { (if (a23) { v17.clone() } else { v17.clone() }) }, 255.5f32) };
            (-id::<f64>(K3)).mix();
            S2 { f0: S1 { f0: true } }.mix();
            apply(|x27: u128| -> ((bool, &'static str), S1) { ((false, "42"), S1 { f0: false }) }, 41538374868278621028243970633760767u128).mix();
            f25([["line\n"; 4], ["x y"; 4]], apply(|x30: S2| -> ((bool, &'static str), S1) { ((true, "héllo"), S1 { f0: true }) }, S2 { f0: S1 { f0: true } }), (v20 <= v20)).mix();
        }
        v24.mix();
        v24 = 2u128.is_power_of_two();
        let v31: f64 = (41i8..43i8).step_by(1).fold(twice::<f64>(&0.0f64).1, |acc33: f64, x34: i8| -> f64 { apply(|x35: ((bool, &'static str), S1)| -> f64 { (-9223372036854775808.0f64) }, ((false, "  pad  "), S1 { f0: false })) });
        'l37: for i36 in ((34i8..38i8).step_by(2)) {
            let mut v38: [(f32, bool, f32, bool); 2] = sel::<[(f32, bool, f32, bool); 2]>(matches!(v24, (false)), [(984.5463f32, false, 3.25f32, false), (0.2f32, false, 1e-40f32, false)], Err::<[(f32, bool, f32, bool); 2], u128>(9671406556917033397649408u128).unwrap_or([(f32::NAN, false, 1e10f32, true); 2]));
            'l48: for x47 in ((56i8..57i8).step_by(3).zip(({ let mut items = Vec::<f32>::new(); items.retain(|x46: &f32| -> bool { v24 }); items }))) {
                let v49: [bool; 2] = twice::<[bool; 2]>(&[v24, true]).0;
                let v54: S1 = S1 { f0: i36.overflowing_mul(2i8).1 };
            }
            f64::from(63u16).mix();
            (!("a" > "ff")).mix();
        }
        let mut v55: [(f32, bool, f32, bool); 2] = [(f32::MAX, false, 65535.0f32, true), ((-2.5e-3f32), true, 1e30f32, false)];
        if ((((true, "a"), S1 { f0: true }) > ((false, "Abc Def"), S1 { f0: true }))) {
            let mut v58: f64 = "x".parse::<f64>().unwrap_or((if (v55[0].3) { v31 } else { K3 }));
            let mut v59: [&'static str; 5] = ["3.5", "a,b,,c", "héllo", "Abc Def", "😀"];
        } else {
            <[(f32, bool, f32, bool); 2]>::try_from(Vec::<(f32, bool, f32, bool)>::new()).unwrap_or([(1e-40f32, true, 0.1f32, false); 2]).mix();
        }
        v18 = v16.clone();
        v24 = matches!(K4, (_));
        {
            #[derive(Clone, Copy, Debug, PartialEq)] enum Small { A, B, C }
            #[derive(Clone, Copy, Debug, PartialEq)] enum Outer { One(Small), Two, Three(bool), Four(char) }
            #[derive(Debug, PartialEq)] enum Deep { Ref(&'static u8), Boxed(Box<u16>), Nz(NonZeroU32), Nothing, Other(Option<Option<bool>>) }
            #[derive(Clone, Copy, Debug)] #[repr(u8)] enum Tagged { X(u8) = 3, Y = 9, Z(bool, u16) = 200 }
            enum Never {}
            static BYTE: u8 = 7;
            fn tell(it: &Outer) -> u64 { match *it { Outer::One(Small::A) => 1, Outer::One(Small::B) => 2, Outer::One(Small::C) => 3, Outer::Two => 4, Outer::Three(flag) => 5 + flag as u64, Outer::Four(c) => 7 + c as u64 } }
            fn deep(it: &Deep) -> u64 { match it { Deep::Ref(r) => **r as u64, Deep::Boxed(b) => **b as u64, Deep::Nz(n) => n.get() as u64, Deep::Nothing => 0, Deep::Other(None) => 1, Deep::Other(Some(None)) => 2, Deep::Other(Some(Some(flag))) => 3 + *flag as u64 } }
            { let items: [Option<char>; 3] = black_box([None, Some('z'), Some('\u{10FFFF}')]); for it in items { feed(match it { None => 0, Some(c) => c as u64 }); } let nested: Option<Option<Option<u8>>> = black_box(Some(Some(None))); feed(match nested { None => 1, Some(None) => 2, Some(Some(None)) => 3, Some(Some(Some(v))) => v as u64 }); }
            { let items: Vec<Option<NonZeroU8>> = black_box(vec![NonZeroU8::new(0), NonZeroU8::new(26), NonZeroU8::new(255)]); for it in &items { feed(it.map_or(0, |n| n.get() as u64)); } feed(size_of::<Option<NonZeroU8>>() as u64); }
            feed((size_of::<Option<Small>>() + size_of::<Outer>() * 16 + size_of::<Option<Outer>>() * 256 + size_of::<Option<Option<Outer>>>() * 4096) as u64);
            for it in black_box([Outer::One(Small::C), Outer::Two, Outer::Three(true), Outer::Four('a'), Outer::One(Small::A), Outer::Three(false)]) { feed(tell(&it)); feed(tell(&Some(it).unwrap_or(Outer::Two))); }
            for it in black_box([Deep::Ref(&BYTE), Deep::Boxed(Box::new(26)), Deep::Nz(NonZeroU32::new(48).unwrap()), Deep::Nothing, Deep::Other(None), Deep::Other(Some(None)), Deep::Other(Some(Some(true)))]) { feed(deep(&it)); let wrapped = Some(it); feed(wrapped.as_ref().map_or(99, deep)); }
            { let mut slot = black_box(Outer::Two); slot = if 26 % 2 == 0 { Outer::Three(true) } else { Outer::One(Small::B) }; feed(tell(&slot)); let mut opt = black_box(Some(slot)); if let Some(Outer::Three(flag)) = &mut opt { *flag = false; } feed(opt.map_or(0, |it| tell(&it))); opt = None; feed(opt.is_some() as u64); }
        }
        if ((0u128.is_power_of_two() ^ (34u128 > 3u128))) {
            {
                let (a, b) = (black_box((-129.0) as f64), black_box((-1.0) as f64));
                format!("{a} {a:?} {a:e} {b:.3} {b:10.2} {a:+}").mix();
                let (mut acc, mut k) = (a, 0u32); while acc.is_finite() && acc.abs() < 1e30 && k < 40 { acc = acc * 1.5 + 1.0; k += 1; } feed(k as u64);
                feed(format!("{:?}", a.classify()).len() as u64); if !a.is_nan() { feed(a.to_bits() as u64); a.signum().mix(); a.copysign(b).mix(); }
            }
        }
        let v67: [&'static str; 5] = sel::<[&'static str; 5]>((match ([[true; 5]; 1]) { [[.., p70]] => false, _ => v24 }), ["x y", "", "hello", "a,b,,c", "3.5"], ["", "😀", "😀", "x y", "42"]);
        let v75: [[bool; 5]; 1] = [[Some("-7").is_none(), 144879859275757046229809103401853177813i128.overflowing_add(5i128).1, true, (match (String::from("0123456789").as_str()) { p80 => false, _ => v24 }), (if (v55[1].3) { false } else { true })]];
        0.2f32.recip().mix();
    }
}
//// then
mod c5 {
    use super::*;
    #[derive(Clone, Debug, PartialEq, PartialOrd, Copy, Eq, Ord, Hash)]
    #[repr(align(2))]
    pub struct S1 { pub f0: bool }
    impl Mix for S1 {
        fn mix(&self) {
            feed(1);
            self.f0.mix();
        }
    }
    #[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord, Hash)]
    pub struct S2 { pub f0: S1 }
    impl Mix for S2 {
        fn mix(&self) {
            feed(1);
            self.f0.mix();
        }
    }
    impl Drop for S2 {
        fn drop(&mut self) {
            feed(0x20895f6b);
            self.mix();
        }
    }
    const K3: bool = true;
    static K4: S1 = S1 { f0: true };
    pub trait T5 {
        fn m6(&self) -> f32;
        fn m7(&self, a8: f64) -> bool {
            self.m6().mix();
            vec![(true, 65535.0f32), (true, 4294967296.0f32), (false, 302.6939f32)].into_iter().map(|x10: (bool, f32)| -> char { 'z' }).collect::<String>().contains("line\n")
        }
    }
    impl T5 for f32 {
        fn m6(&self) -> f32 {
            [std::array::from_fn::<u128, 2, _>(|i13: usize| -> u128 { 3u128 }), [10u128, 4503599627370496u128], [37u128, 2u128], [40564819207303340847894502572031u128; 2], sel::<[u128; 2]>(false, [10u128, 327348754437794219382519230952776333586u128], [1u128, 27u128]), [25u128, 36u128], [100u128, 40u128], [8u128, 5u128], std::array::from_fn::<u128, 2, _>(|i27: usize| -> u128 { 0u128 }), [0u128, 590295810358705651712u128], [340282366920938463463374607431768211455u128, 8u128], [227421410334537234037046923663595854575u128, 326185920523986117878869925215194871558u128], apply(|x37: String| -> [u128; 2] { [340282366920938463463374607431768211455u128; 2] }, String::from("a,b,,c")), apply(|x40: [bool; 1]| -> [u128; 2] { [664613997892457936451903530140172288u128, 170141183460469231731687303715884105729u128] }, [true]), [2u128, 156328986558069006728580820732789030614u128], <[u128; 2]>::try_from(vec![313358875812819535616854706051688392905u128, 34u128, 281474976710656u128, 282606677669707870241618705979741087365u128, 8u128]).unwrap_or([288230376151711744u128, 81429181741750389181260072854855152817u128]), [15u128; 2]].mix();
            (*self)
        }
    }
    impl T5 for bool {
        fn m6(&self) -> f32 {
            (255.5f64 as f32).ceil()
        }
    }
    pub fn f45(a46: (Box<bool>, (&'static str, bool, bool, bool, bool, bool)), a47: &S1, a48: Box<String>, a49: S1, a50: &mut S1) -> bool {
        ((if (false) { a46.1.1 } else { K3 }) || (!K3))
    }
    #[inline(never)]
    pub fn f51(a52: f32, a53: f32, a54: &mut String, a55: (Box<bool>, (&'static str, bool, bool, bool, bool, bool))) -> [f32; 3] {
        let mut v56: String = format!("{:5}! {:5}!", a55.1.1, true);
        [(-0.0f32), f32::NAN, (-128.9f32)]
    }
    #[inline(never)]
    pub fn f57(a58: &String) -> String {
        (41i8..41i8).enumerate().map(|x60: (usize, i8)| -> u128 { (unsafe { Bytes4 { signed: 1i32 }.halves[0] } as u128) }).map(|x61: u128| -> bool { K3 }).map(|x62: bool| -> char { (if (K3) { '\'' } else { '😀' }) }).collect::<String>()
    }
    #[inline(never)]
    pub fn f63(a64: (bool, f32)) -> f64 {
        let mut v65: S1 = S1 { f0: sel::<bool>(K3, K3, a64.0), ..S1 { f0: false } };
        let v66: (bool, f32) = a64;
        twice::<f64>(&(-1.0f64)).1.fract()
    }
    pub fn run() {
        {
            let (a, b): (i128, i128) = (black_box((((1u128 << 64)) as u128 as i128).wrapping_neg()), black_box(((18446744073709551557) as u128 as i128)));
            { let (hi, lo) = ((a >> 64) as u64, a as u64); let again = ((hi as u128) << 64) | lo as u128; feed((again == a as u128) as u64); ((lo as u128) * (hi as u128)).mix(); }
            (a & b).mix(); (a | b).mix(); (a ^ b).mix(); feed((a < b) as u64 + 2 * (a == b) as u64); a.max(b).mix(); a.abs_diff(b).mix(); a.cmp(&b).mix();
            a.to_le_bytes().mix(); a.to_be_bytes().mix(); a.to_string().mix(); format!("{a:x} {b:#o} {a:>45} {b:e}").mix();
            a.wrapping_add(b).mix(); a.wrapping_sub(b).mix(); a.wrapping_mul(b).mix(); a.checked_add(b).mix(); a.checked_mul(b).mix(); a.saturating_sub(b).mix();
            { let (sum, carry) = a.overflowing_add(b); let (product, over) = a.overflowing_mul(b); sum.mix(); product.mix(); feed(carry as u64 + 2 * over as u64); }
            (a << 100).mix(); (a >> 100).mix(); a.rotate_left(100).mix(); a.checked_shl(100 + 64).mix(); a.wrapping_shr(100 + 100).mix(); (a >> (b as u32 % 128)).mix();
            { let items = [a, b, a ^ b, a.wrapping_add(b)]; let mut sorted = items; sorted.sort(); sorted.mix(); items.iter().copied().fold(0, |acc: i128, it| acc.wrapping_add(it)).mix(); }
            a.checked_div(b).mix(); a.checked_rem(b).mix(); a.checked_div_euclid(b).mix(); a.checked_rem_euclid(b).mix(); a.wrapping_div(b | 1).mix(); a.wrapping_rem(b | 1).mix();
        }
        let mut v67: ([bool; 4],) = sel::<([bool; 4],)>(K3, ([true; 4],), ([true; 4],));
        let v68: f64 = f64::from((false as i16));
        'l74: for x73 in ([1e30f32, 16777217.0f32].into_iter().zip(sel::<Vec<(bool, (bool, NonZeroU32), bool)>>(false, vec![(true, (true, NonZeroU32::new(4194304u32).unwrap()), false), (false, (true, NonZeroU32::new(1u32).unwrap()), false), (false, (false, NonZeroU32::new(256u32).unwrap()), true)], vec![(false, (true, NonZeroU32::new(1u32).unwrap()), false)])).collect::<Vec<_>>()) {
            let mut v75: String = (match (({ let mut text = ({ let mut text = String::from("x y"); text.push('\u{e000}'); text }); text.push('\''.to_ascii_lowercase()); text }).as_str()) { "😀" if ((match ([(-9223372036854775808.0f32); 3]) { [_, p76 @ .., _] => p76[0], [p77, p78, _] => x73.0, _ => x73.0 }) != sel::<f32>(v67.0[0], 0.1f32, x73.0)) => f57(&String::from("42")), _ => String::from("hello").replace("0123456789", "hello") });
        }
        let r79 = catch_unwind(AssertUnwindSafe(|| -> (bool, (bool, NonZeroU32), bool) {
            let mut o80: Box<dyn T5> = Box::new(apply_dyn_mut(&mut |x81: S2| -> f32 { f32::from(236u8) }, Ok::<S2, f64>(S2 { f0: S1 { f0: false } }).unwrap_or_else(|x82: f64| -> S2 { S2 { f0: S1 { f0: true } } })));
            let ([p83, .., p84],): ([bool; 4],) = larger::<([bool; 4],)>(id::<([bool; 4],)>(v67), ([false, false, true, true],));
            (sel::<bool>(p84, K3, p83), ({ let mut text = String::from("Abc Def"); text.push('\u{d7ff}'); text }), true).mix();
            let v85: bool = ({ let mut m86: S1 = twice::<S1>(&K4).1; let out = f45(id::<(Box<bool>, (&'static str, bool, bool, bool, bool, bool))>((Box::new(false), ("😀", true, true, true, true, false))), &S1 { f0: true }, Box::new(String::from("-7")), S1 { f0: true }, &mut m86); m86.mix(); out });
            if v85 { std::panic::panic_any(21u32.pow(804513931u32 % 70)); }
            (false, (true, NonZeroU32::new(1u32).unwrap()), false)
        }));
        match r79 {
            Ok(value) => value.mix(),
            Err(caught) => payload(caught),
        }
        v67 = sel::<([bool; 4],)>((if (sel::<bool>(false, false, false)) { (!K3) } else { true }), (match (id::<([&'static str; 5], String)>((["-7", "a", "0123456789", "héllo", ""], String::from("42")))) { ([p88, ..], p89) => (if (true) { ([true; 4],) } else { v67 }), _ => (v67.0,) }), sel::<([bool; 4],)>((NonZeroI64::new(9i64).unwrap() > NonZeroI64::new(68719476737i64).unwrap()), ([true; 4],), twice::<([bool; 4],)>(&v67).0));
        v67 = apply(|x90: (bool, f32)| -> ([bool; 4],) { (if (true) { v67 } else { ([true, false, false, false],) }) }, (true, (-2.5e-3f32)));
        let mut n91 = 0u32;
        'l92: loop {
            n91 += 1;
            if n91 > 3 { break; }
            if ((apply(|x94: S1| -> S1 { S1 { f0: true } }, S1 { f0: false }) <= (if (K3) { K4 } else { K4 }))) {
                {
                    for armed in [false, true] { let n = Noisy(10); let f = move |armed: bool| { let _l = Noisy(30); trip(armed, ()); n.0 }; caught(move || feed(f(armed) as u64)); }
                    for armed in [false, true] { let n = Noisy(10); let m = Noisy(30); caught(|| { let by_ref = || n.0 + m.0; feed(by_ref() as u64); let taken = move || { let _own = m; trip(armed, 1u32) }; feed(taken() as u64); }); n.mix(); }
                    for armed in [false, true] { let mut seen = Vec::new(); caught(|| { let mut note = |it: u32| { seen.push(Noisy(it)); trip(armed && it == 2, ()); }; note(1); note(2); note(3); }); feed(seen.len() as u64); }
                }
            }
            "ff".split(' ').nth(1).unwrap_or("line\n").trim().mix();
            if let ([true, true, false, p95],) = (&{ v67 }) {
                let g96: fn((bool, f32)) -> f64 = (|a97: (bool, f32)| -> f64 { sel::<f64>(String::from("42").ends_with("a,b,,c"), (64i8..67i8).fold((-9223372036854775808.0f64), |acc98: f64, x99: i8| -> f64 { 255.5f64 }), 65535.0f64) });
                match &mut v67 {
                    ([false, false, true, false],) => {
                        let v100: Box<NonZeroU32> = Box::new(NonZeroU32::new(1566561162u32).unwrap());
                        let mut v101: bool = (match (String::from("😀").chars().rev().collect::<String>().as_str()) { p102 => true, _ => ({ let mut m104: S1 = K4; let out = f45((Box::new(false), ("line\n", true, true, false, false, true)), &apply(|x103: S1| -> S1 { S1 { f0: true } }, K4), twice::<Box<String>>(&Box::new(String::from("héllo"))).0, S1 { f0: K3, ..K4 }, &mut m104); m104.mix(); out }) });
                    }
                    ([true, p105 @ .., p106],) => {
                        let g107: fn((bool, f32)) -> f64 = (|a108: (bool, f32)| -> f64 { ({ let mut items = Vec::<(bool, String, bool)>::new(); items.extend(vec![(false, String::from("line\n"), true), (true, String::from("42"), false), (true, String::from("a"), true), (true, String::from("0123456789"), false), (false, String::from("héllo"), false)]); items }).into_iter().fold(f64::from(2147483647i32), |acc110: f64, x111: (bool, String, bool)| -> f64 { (if (K3) { acc110 } else { 255.5f64 }) }) });
                    }
                    _ => {}
                }
            } else {
                let mut v113: [bool; 1] = [true];
                {
                    let mut f114 = |a112: (bool, f32)| -> ([i128; 2], Box<bool>) { v113 = [false]; (std::array::from_fn::<i128, 2, _>(|i117: usize| -> i128 { 85070591730234615865843651857942052863i128 }), (match (String::from("hello").as_str()) { "hello" => Box::new(false), p119 => Box::new(true), p120 => Box::new(false), _ => Box::new(false) })) };
                    apply(|x121: Box<String>| -> f64 { f63((true, (-952.0950f32))) }, Box::new(String::from("a,b,,c"))).mix();
                    let mut v122: f64 = f63((true, f32::MIN));
                    f63((true, 330.5861f32)).mix();
                    f114((true, 0.1f32)).mix();
                }
                v113.mix();
            }
        }
        let mut v123: NonZeroI64 = (|x124: ([bool; 4],)| -> NonZeroI64 { apply(|x125: S2| -> NonZeroI64 { ('b126: { if (K3) { break 'b126 NonZeroI64::new((-7206220902646561443i64)).unwrap(); } NonZeroI64::new((-7i64)).unwrap() }) }, S2 { f0: K4 }) })(([true, false, false, false],));
        let ([.., p127],): ([bool; 4],) = v67;
        let mut v128: bool = sel::<String>(p127, String::from("-7"), String::from("")).contains("a");
        let mut n129 = 0u32;
        let v131: [[u128; 2]; 17] = 'l130: loop {
            n129 += 1;
            if n129 > 8 { break sel::<[[u128; 2]; 17]>(true, [[6u128, 339317153082740810836795212588881088595u128], [274877906944u128; 2], [26u128; 2], [93393669872174718095715635179510309069u128, 295160894422952029873588407390087363654u128], [238565085718884277117579816280828008557u128; 2], [20u128; 2], [10u128, 170141183460469231731687303715884105727u128], [1073741824u128, 20282409603651670423947251286017u128], [97238660904217101582590935760616166703u128; 2], [85070591730234615865843651857942052864u128, 2u128], [7u128, 17u128], [258691978947764113400450361754434257396u128, 340282366920938463463374607431768211455u128], [6518916200564562249537916325731225189u128; 2], [100u128; 2], [280413120229473007320597076974876656210u128, 3u128], [23u128, 1u128], [340282366920938463463374607431768211455u128, 308168665342960953619494734321773292840u128]], [[17259243596869604585004518328941291884u128; 2], [1u128, 9u128], [34u128, 95626653015733478117932564223295078260u128], [0u128, 33818493301098906730397056433600450683u128], [1u128; 2], [179045770223294682926216577258698482210u128; 2], [7u128, 8169741450849458301255581835912030303u128], [3u128; 2], [1180591620717411303424u128; 2], [237799944491958465200370365144177349614u128, 7u128], [271724696287825915870711036483036844621u128, 340282366920938463463374607431768211455u128], [664613997892457936451903530140172288u128; 2], [340282366920938463463374607431768211455u128, 10u128], [340282366920938463463374607431768211454u128; 2], [158456325028528675187087900673u128; 2], [8u128, 6u128], [129895848255209836061850371565875484674u128; 2]]); }
            {
                let f135 = move |a134: [[bool; 2]; 2]| -> Box<bool> { sel::<Box<bool>>((!p127), Box::new(v67.0[1]), Box::new(false)) };
                let v136: &'static str = id::<&'static str>("hello");
                sel::<S1>((String::from("") > String::from("line\n")), sel::<S1>(v67.0[2], S1 { f0: true }, S1 { f0: false }), twice::<S1>(&K4).1).mix();
                let mut v137: f64 = twice::<f64>(&0.1f64.abs()).1;
                f135([[true, false], [false, true]]).mix();
            }
            ([Box::new(String::from("  pad  ")), Box::new(String::from("hello")), Box::new(String::from("Abc Def")), Box::new(String::from("😀"))].map(|x143: Box<String>| -> [[bool; 2]; 2] { [[true, false], [true, false]] }).map(|x141: [[bool; 2]; 2]| -> bool { 64i8.is_negative() }),).mix();
            let [.., p146]: [Option<f32>; 5] = [Some((-1.0f32)), None::<f32>, Some(65535.0f32), Some(f32::MIN), None::<f32>];
        };
        v128 = ({ let mut m150: S1 = sel::<S1>(({ let mut m151: S1 = S1 { f0: false }; let out = f45((Box::new(false), ("hello", true, true, true, false, false)), &K4, Box::new(String::from("-7")), K4, &mut m151); m151.mix(); out }), (if (true) { K4 } else { K4 }), sel::<S1>(K3, K4, K4)); let out = f45((match (format!("{} {:?}", v128, K3).as_str()) { "0123456789" => (Box::new(true), ("x y", true, false, false, true, true)), _ => id::<(Box<bool>, (&'static str, bool, bool, bool, bool, bool))>((Box::new(true), ("  pad  ", true, true, false, false, true))) }), &S1 { f0: false }, apply_dyn_mut(&mut |x147: f64| -> Box<String> { apply_boxed(Box::new(|| -> Box<String> { Box::new(String::from("line\n")) })) }, f63((true, 0.2f32))), sel::<S1>(({ let mut m149: S1 = K4; let out = f45((Box::new(false), ("x y", true, true, true, true, false)), &S1 { f0: true }, Box::new(String::from("a,b,,c")), K4, &mut m149); m149.mix(); out }), sel::<S1>(false, K4, K4), S1 { f0: true, ..K4 }), &mut m150); m150.mix(); out });
        {
            { let mut d = Duo { a: 55u32, b: Noisy(39) }; let mut inc = || d.a += 1; inc(); inc(); let take = move || d.b; feed(d.a as u64); take().mix(); }
            { let d = Duo { a: Noisy(55), b: Noisy(39) }; let only_a = move || d.a.0; feed(only_a() as u64); d.b.mix(); }
        }
        matches!(sel::<&'static str>(true, "", "Abc Def"), ("-7")).mix();
        (if (({ let mut m152: S1 = S1 { f0: false }; let out = f45((Box::new(false), ("x y", false, false, true, false, false)), &K4, Box::new(String::from("0123456789")), K4, &mut m152); m152.mix(); out })) { (true, String::from("hello"), true) } else { apply(|x153: [f32; 3]| -> (bool, String, bool) { (true, String::from("x y"), false) }, [255.5f32, 1.0f32, f32::MIN_POSITIVE]) }).mix();
        let mut v154: (bool, (bool, NonZeroU32), bool) = apply(|x155: i8| -> (bool, (bool, NonZeroU32), bool) { (v67.0[1], (false, NonZeroU32::new(2147483647u32).unwrap()), v128) }, (54i8..59i8).fold(63i8, |acc156: i8, x157: i8| -> i8 { acc156 }));
        if (true) {
            (if (v154.2) { [[(-119.4736f32), 65535.0f32, (-2.5e-3f32)]; 17] } else { [[65535.0f32, f32::MIN_POSITIVE, (-9223372036854775808.0f32)]; 17] }).map(|x159: [f32; 3]| -> [u128; 2] { twice::<[u128; 2]>(&[320246050094709496948130130589721925855u128, 0u128]).1 }).mix();
            apply(|x164: NonZeroI64| -> Box<String> { Box::new(String::from("héllo")) }, apply(|x165: [f32; 3]| -> NonZeroI64 { NonZeroI64::new(7740889626850173980i64).unwrap() }, [7.0f32, 0.2f32, f32::NAN])).mix();
            let mut n166 = 0u32;
            let v168: u128 = 'l167: loop {
                n166 += 1;
                if n166 > 5 { break (65535.0f32 as u128); }
                String::from("héllo").starts_with("3.5").mix();
            };
        }
        let mut v169: Box<i128> = (if (v154.1.0) { Box::new(12i128) } else { Box::new((-14i128)) });
    }
}
