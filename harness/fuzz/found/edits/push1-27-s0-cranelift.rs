// Found by edits.py: seed push1-27-s0-cranelift, edition 2021, backend cranelift.
// The edit (regen.fn): c3 from fn4
// Again: edits.py check '{"files": ["push1-27-s0-cranelift.rs"]}'
// step 1: refused: Error: daemon: cargo: a build of fz panicked: assertion `left == right` failed: Can't change TLS data object to normal or in the opposite way   left: false  right: true
// At 39d79a5b, x86-64 (#291): the patch numbered a thread-local of its own as the base had numbered
// a static it imports. By e843203e this program's numbers no longer meet, but the clash could still.
mod c0 {
    use super::*;
    #[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord, Hash)]
    pub struct S1<A> { pub f0: [A; 2], pub f1: u16, pub f2: u16, pub f3: (&'static str, (i16, u8), String, bool), pub f4: &'static str, pub f5: u64 }
    impl<A: Mix> Mix for S1<A> {
        fn mix(&self) {
            feed(6);
            self.f0.mix();
            self.f1.mix();
            self.f2.mix();
            self.f3.mix();
            self.f4.mix();
            self.f5.mix();
        }
    }
    pub trait T2 {
        fn m3(&self, a0: char) -> i32;
        fn m4(&self, a0: (&'static str, (i16, u8), String, bool)) -> u64;
        fn m5(&self, a6: i64) -> (String, Vec<i128>) {
            self.m4(("ff", ((-32768i16), 127u8), String::from("héllo"), true)).mix();
            (String::from("x y"), vec![16384i128])
        }
    }
    impl T2 for char {
        fn m3(&self, a7: char) -> i32 {
            let ((_, p8),): (([u8; 4], [u8; 4]),) = (([0u8, 151u8, 22u8, 3u8], [127u8; 4]),);
            (-18i32).signum().wrapping_rem_euclid(7i32 | 1)
        }
        fn m4(&self, a9: (&'static str, (i16, u8), String, bool)) -> u64 {
            let v10: char = '\'';
            let v11: ((isize, char, i16), u32, (isize, i8, usize), char, usize, &'static str) = (match (vec![String::from("😀"), String::from("héllo"), String::from("x y")].join("-7").as_str()) { "hello" => (match (vec![35u64, 18446744073709551614u64, 8388609u64, 32u64, 7u64].as_slice()) { [p13, .., 39u64..140737488355328u64] if a9.3 => ((0isize, 'a', 1i16), 4294967294u32, (10isize, 100i8, 8usize), '😀', 18014398509481984usize, "-7"), [..=10u64, .., p15] => ((4294967297isize, '9', 8i16), 37u32, (256isize, (-17i8), 1usize), 'z', 4usize, "😀"), [(..=15295692752344892009u64 | 8796093022207u64..12460479180062205638u64 | 11253256760294365187u64), ..=16543901845079140037u64, p17] => (((-3682847505030620912isize), '\u{10FFFF}', 1i16), 256u32, ((-5902696808153036263isize), (-128i8), 7usize), '0', 34usize, "Abc Def"), _ => ((7isize, '中', 4i16), 3u32, (15isize, 0i8, 18446744073709551615usize), '\u{d7ff}', 3usize, "x y") }), _ => ((3isize, 'a', (-8i16)), 4294967294u32, ((-4isize), (-23i8), 524287usize), (*self), 0usize, a9.0) });
            (unsafe { std::mem::transmute::<u16, [u8; 2]>(2u16) }[0] as u64)
        }
    }
    impl T2 for S1<u64> {
        fn m3(&self, a18: char) -> i32 {
            if ((((*self).f3.2.clone(), (*self).f3.2.clone(), String::from("hello"), NonZeroI64::new(100i64).unwrap(), Box::new('\n'), (*self).f0[1]) != (String::from("ff"), String::from("42"), String::from("line\n"), NonZeroI64::new(11i64).unwrap(), Box::new('😀'), 13594018022685989798u64))) {
                let v19: (String, Vec<i128>) = (String::from("-7"), vec![10i128, 170141183460469231731687303715884105727i128, 67856937615624230892935940022005278758i128, (-1i128), (-3i128)]);
                let mut v20: [u64; 3] = [18446744073709551614u64, 24u64, 437988416641733513u64];
                v20.mix();
                let (p22, p23, p24, p25, _, _): (String, String, String, NonZeroI64, Box<char>, u64) = (String::from("-7").to_uppercase(), format!("{:>4}|", (*self).f3.0), (if ((*self).f3.3) { (*self).f3.2.clone() } else { (*self).f3.2.clone() }), NonZeroI64::new((-9223372036854775807i64)).unwrap(), Box::new('Z'), v20[0]);
            }
            let mut v26: NonZeroI64 = larger::<NonZeroI64>((if ((*self).f3.3) { NonZeroI64::new(4095i64).unwrap() } else { NonZeroI64::new(256i64).unwrap() }), larger::<NonZeroI64>(NonZeroI64::new(1073741824i64).unwrap(), NonZeroI64::new(1i64).unwrap()));
            728303545i32.wrapping_rem_euclid(1113433600i32 | 1)
        }
        fn m4(&self, a27: (&'static str, (i16, u8), String, bool)) -> u64 {
            (*self).f0[0]
        }
    }
    impl T2 for i32 {
        fn m3(&self, a28: char) -> i32 {
            (!std::iter::repeat((String::from("Abc Def"), vec![7i128, 649037107316853453566312041152512i128, 2i128, 100i128, 70368744177665i128])).take(1).map(|x29: (String, Vec<i128>)| -> u64 { 16620786192457431278u64 }).fold((*self), |acc30: i32, x31: u64| -> i32 { acc30 })).to_be()
        }
        fn m4(&self, a32: (&'static str, (i16, u8), String, bool)) -> u64 {
            let mut v35: (([u8; 4], [u8; 4]),) = (([183u8; 4], [126u8, 11u8, 9u8, 204u8]),);
            {
                let mut f36 = |a33: Box<String>, a34: (([u8; 4], [u8; 4]),)| -> ((isize, char, i16), u32, (isize, i8, usize), char, usize, &'static str) { v35 = v35; (((-7244509230025871881isize), '\n', a32.1.0), (match (vec![("-7", ((-11163i16), 21u8), String::from(""), true), ("line\n", ((-13387i16), 1u8), String::from("ff"), true)].as_slice()) { [p38, .., p39] => 2496654068u32, _ => 524289u32 }), ((-8isize), 3i8, 0usize), (unsafe { *(&'é' as *const char) }), 562949953421312usize, a32.0.get(3..6).unwrap_or(a32.0)) };
                let mut v40: BTreeMap<u64, (([u8; 4], [u8; 4]),)> = BTreeMap::new();
                v40.insert(id::<u64>((64u64.abs_diff(4097u64) as u64)), (([0u8, 1u8, 115u8, 10u8], [3u8, 127u8, 8u8, 2u8]),)).mix();
                for (key, value) in v40.iter().rev() { key.mix(); value.mix(); }
                v40.len().mix();
                {
                    for armed in [false, true] { caught(|| { let _v = match Some(Noisy(14)) { Some(it) => (trip(armed, Noisy(58)), it), None => (Noisy(18), Noisy(44)) }; }); }
                    for armed in [false, true] { caught(|| { take3(first_of(&Noisy(43), &Noisy(24)).0, Noisy(50), trip(armed, Noisy(32))); }); }
                    for armed in [false, true] { caught(|| { let _v = [Noisy(2), Noisy(43), trip(armed, Noisy(28))]; }); }
                }
                f36(twice::<Box<String>>(&Box::new(String::from("-7"))).1, (([64u8; 4], [15u8; 4]),)).mix();
            }
            v35.mix();
            536870913u64.checked_neg().unwrap_or_default()
        }
    }
    impl T2 for u64 {
        fn m3(&self, a42: char) -> i32 {
            larger::<i32>((-2147483647i32), (-2147483647i32))
        }
        fn m4(&self, a43: (&'static str, (i16, u8), String, bool)) -> u64 {
            if ((-911.6277f32).is_finite()) {
                let v44: (String, String, String, NonZeroI64, Box<char>, u64) = (Vec::<String>::new().join("ff"), a43.2.clone().repeat(1), a43.2.clone(), NonZeroI64::new((-1i64)).unwrap(), ({ let t47: (String, Vec<i128>) = (String::from("-7"), vec![170141183460469231731687303715884105726i128]); Box::new('\0') }), ((*self) & 13u64));
                let f46 = move || -> i64 { let v45 = v44; let out = (3i64.abs_diff(1446468724796532763i64) as i64).wrapping_rem(100i64.checked_add((-7192014087112160196i64)).unwrap_or(262143i64) | 1); drop(v45); out };
                f46().mix();
                let mut v48: [i64; 1] = (unsafe { *(&[(-6615100948430746565i64); 1] as *const [i64; 1]) });
                { let value = twice::<i64>(&(-10i64)).1; let at = (String::from("").len() % 1); unsafe { v48.as_mut_ptr().add(at).write(value) }; }
                v48.mix();
                v48 = ({ let mut items = v48; items.swap(0, 0); items });
            } else {
                {
                    for armed in [false, true] { caught(|| { take3(Noisy(31), trip(armed, Noisy(24)), Noisy(57)); }); }
                    for armed in [false, true] { caught(|| { let _v = { let _local = Noisy(7); (Noisy(8), trip(armed, Noisy(21))) }; }); }
                }
            }
            ((*self).rotate_left(521806978u32) & (*self).wrapping_div_euclid(15178758762649430582u64 | 1))
        }
    }
    #[inline(never)]
    pub fn f54(a55: S1<u64>, a56: i64, a57: &[i32], a58: i64, a59: char, a60: (&'static str, (i16, u8), String, bool), a61: u16, a62: &str) -> ((u128, usize, (), &'static str), Vec<char>) {
        {
            let f63 = || -> u64 { a55.f0[1] };
            let mut v64: u16 = (a61.wrapping_rem_euclid(a61 | 1).trailing_zeros() as u16);
            v64 = u16::from(twice::<u8>(&a60.1.1.checked_neg().unwrap_or_default()).0);
            f63().mix();
        }
        if (a56.is_positive()) {
            {
                let mut v65: String = String::from("hello");
                v65 = twice::<String>(&format!("{:>4}| {} {:?}", '\u{10FFFF}', ' ', a59)).0;
            }
            let mut n66 = 0u32;
            let v68: NonZeroI64 = 'l67: loop {
                n66 += 1;
                if n66 > 3 { break id::<NonZeroI64>(NonZeroI64::new((-5i64)).unwrap()); }
                let mut v69: S1<u64> = (if (a60.3) { a55.clone() } else { a55.clone() });
            };
            {
                let f71 = move |a70: char| -> (&'static str, (i128, u16, isize)) { ("".split(' ').nth(1).unwrap_or("line\n"), ((-18i128), a61, 4611686018427387903isize)) };
                let mut v72: (usize, &'static str, i32, u16) = ((a61 as usize), id::<&'static str>(a55.f4.get(1..7).unwrap_or(a55.f3.0)), ("Abc Def".parse::<i32>().unwrap_or(1i32).leading_ones() as i32), a61);
                v72 = (18446744073709551614usize, "a", 19i32, 512u16);
                v72.3 &= std::array::from_fn::<(&'static str, (i16, u8), String, bool), 2, _>(|i74: usize| -> (&'static str, (i16, u8), String, bool) { a60.clone() }).to_vec().into_iter().filter(|x76: &(&'static str, (i16, u8), String, bool)| -> bool { id::<Option<&'static str>>(Some("hello")).is_none() }).fold((if (a60.3) { a61 } else { a61 }).checked_neg().unwrap_or_default(), |acc78: u16, x79: (&'static str, (i16, u8), String, bool)| -> u16 { ((1i32.leading_zeros() as i32) as u16) });
                f71('😀').mix();
            }
        } else {
            match (vec![3i64].as_slice()) {
                [p82, .., (1i64..=4717093961040038007i64 | ..=13i64)] => {
                    let mut v83: S1<(&'static str, (i16, u8), String, bool)> = S1 { f0: (match (vec![("x y", ((-170141183460469231731687303715884105727i128), 2u16, 70368744177664isize)), ("line\n", ((-89128668510826873155604005480215651743i128), 20u16, 4194304isize)), ("42", (10i128, 1u16, 36028797018963967isize)), ("a,b,,c", (39977557776910600607035937011762087897i128, 39u16, 15isize)), ("-7", (1267650600228229401496703205376i128, 511u16, 128isize))].as_slice()) { [p86 @ .., (p87, ..)] if true => [("hello", ((-31460i16), 255u8), String::from(""), true), ("ff", (23455i16, 121u8), String::from("héllo"), false)], _ => [("-7", (9498i16, 2u8), String::from("hello"), false), ("-7", (12272i16, 2u8), String::from("😀"), true)] }), f1: twice::<u16>(&a61).0, f2: a61.checked_mul(a61).unwrap_or(a61), f3: a60.clone(), f4: (if (true) { "😀" } else { a55.f3.0 }), f5: (!a55.f5) };
                }
                [.., (-9i64 | 0i64..7i64 | ..=9223372036854775806i64)] => {
                    {
                        for armed in [false, true] { caught(|| { let t = (Some(Noisy(39)), Duo { a: Noisy(36), b: Noisy(17) }); trip(armed, ()); let _x = t.1.b; let _y = t.0; let _late = Noisy(33); }); }
                        for armed in [false, true] { caught(|| { let t = [Noisy(39), Noisy(36), Noisy(17)]; let [_, _x @ ..] = t; trip(armed, ()); let _late = Noisy(48); }); }
                    }
                }
                [-9223372036854775808i64..=5167007451531145697i64, .., p90] => {
                    let v91: char = vec![String::from("line\n")].join("héllo").chars().rev().nth(1).unwrap_or(twice::<char>(&'_').0);
                }
                _ => twice::<NonZeroI64>(&NonZeroI64::new(9223372036854775806i64).unwrap()).1.mix(),
            }
        }
        let v92: i64 = a58.checked_sub(a58).unwrap_or((-4i64)).reverse_bits();
        ((29u128, 10330236887177769956usize, (), a60.0), ({ let mut items = vec!['\u{d7ff}', '\u{d7ff}']; items.reverse(); items }))
    }
    #[inline(never)]
    pub fn f95<G94: Mix + Clone + Debug + PartialOrd>(a96: G94) -> i16 {
        if let [] = (({ let mut items = vec![((39u128, 8usize, (), "ff"), vec!['中', '\n', 'z', 'Z', '0'])]; items.reverse(); items }).as_slice()) {
            {
                let v99: (&'static str, (i16, u8), String, bool) = ("ff", (1i16, 189u8), String::from("😀"), false);
                let mut v100: BTreeMap<char, ((bool, bool, NonZeroU8, i16), char, String, i128)> = BTreeMap::new();
                v100.insert(twice::<char>(&String::from("héllo").chars().nth(2).unwrap_or('Z')).0, ({ let t101: (&'static str, (i16, u8), String, bool) = (v99.0, v99.1, v99.2.clone(), v99.3); ((false, false, NonZeroU8::new(7u8).unwrap(), 0i16), '\0', String::from("-7"), 17i128) })).mix();
                v100.insert(id::<char>('Z'), ((false, v99.3, NonZeroU8::new(19u8).unwrap(), 7i16), (match (vec!["ff", "😀", ""].as_slice()) { [_, .., p103] if v99.3 => 'ß', _ => 'ß' }), ({ let mut text = v99.2.clone(); text.push('_'); text }), 13345364489612951718363091651945190970i128.checked_neg().unwrap_or_default())).mix();
                for (key, value) in v100.iter().rev() { key.mix(); value.mix(); }
                v100.len().mix();
            }
            let mut v105: (&'static str, u128, char, &'static str, (i8, u32, i32, NonZeroU8), bool) = ((match (({ let mut items = vec![(String::from("0123456789"), String::from("a,b,,c"), String::from("a,b,,c"), NonZeroI64::new(4097i64).unwrap(), Box::new('\u{80}'), 32u64), (String::from("hello"), String::from("x y"), String::from("héllo"), NonZeroI64::new(4611686018427387903i64).unwrap(), Box::new('\n'), 6u64)]; items.retain(|x107: &(String, String, String, NonZeroI64, Box<char>, u64)| -> bool { true }); items }).as_slice()) { [(_, ..), .., (p109, ..)] if sel::<bool>(false, true, true) => (loop { break "3.5"; }), [.., (_, _, _, _, p111, 40u64..=257u64)] => "  pad  ".trim(), [.., (p113, p114, _, p115, p116, 17u64..15436819532105483718u64)] => "a", _ => (match (String::from("3.5").as_str()) { "ff" => "line\n", _ => "line\n" }) }), 7u128.wrapping_sub(230284551709350466226582499436846372059u128).wrapping_shr(536870913u32.checked_add(34u32).unwrap_or(4294967294u32)), id::<String>(String::from("héllo")).chars().last().unwrap_or(id::<char>('Z')), "héllo", larger::<(i8, u32, i32, NonZeroU8)>((6i8, 256u32, (-2147483648i32), NonZeroU8::new(100u8).unwrap()), ((-2i8), 2147483647u32, (-673854350i32), NonZeroU8::new(22u8).unwrap())), 1e30f64.sqrt().is_normal());
            {
                let mut f106 = |a104: ((isize, char, i16), u32, (isize, i8, usize), char, usize, &'static str)| -> (&'static str, (i128, u16, isize)) { v105 = (v105.3, id::<u128>(v105.1), String::from("hello").chars().last().unwrap_or('9'), twice::<&'static str>(&"0123456789").0, (a104.2.1, v105.4.1, v105.4.2, v105.4.3), None::<u64>.is_some()); (v105.3.split(' ').nth(1).unwrap_or(v105.3), (64i128, 1u16, a104.2.0)) };
                let v117: (&'static str, u128, char, &'static str, (i8, u32, i32, NonZeroU8), bool) = twice::<(&'static str, u128, char, &'static str, (i8, u32, i32, NonZeroU8), bool)>(&("a,b,,c", 10u128, '0', "  pad  ", ((-22i8), 9u32, 100i32, NonZeroU8::new(117u8).unwrap()), false)).1;
                {
                    let mut items: Vec<Box<dyn Any>> = vec![Box::new(1.5f64), Box::new(()), Box::new(Some(24i64)), Box::new((24u8, true)), Box::new([31u16; 3])];
                    for item in &items { if let Some(it) = item.downcast_ref::<u32>() { feed(*it as u64); } else if let Some(it) = item.downcast_ref::<&str>() { it.mix(); } else if item.is::<Noisy>() { feed(1); } else if let Some(it) = item.downcast_ref::<(u8, bool)>() { feed(it.0 as u64 + it.1 as u64); } else { feed(2); } }
                    items.retain(|it| !it.is::<f64>()); feed(items.len() as u64);
                }
                f106(id::<((isize, char, i16), u32, (isize, i8, usize), char, usize, &'static str)>((((-7097181891160269502isize), '\u{7f}', 6843i16), 2u32, ((-16isize), 7i8, 27usize), 'é', 13usize, "héllo"))).mix();
            }
            v105.mix();
        }
        (if (false) { (-1i16) } else { 7i16 }).rotate_left(5u32.wrapping_rem(67108865u32 | 1))
    }
    #[inline(never)]
    pub fn f118(a119: (&'static str, (i16, u8), String, bool), a120: i128, a121: bool) -> impl Fn() -> ((&'static str,), u64, u16) {
        move || -> ((&'static str,), u64, u16) { (unsafe { *(&(if (a119.3) { (("Abc Def",), 100u64, 8191u16) } else { (("  pad  ",), 17u64, 35u16) }) as *const ((&'static str,), u64, u16)) }) }
    }
    #[inline(always)]
    pub fn f122(a123: i16, a124: (NonZeroU32, Vec<i16>), a125: u16, a126: bool) -> NonZeroI64 {
        let mut v127: (([u8; 4], [u8; 4]),) = (([40u8, 35u8, 64u8, 57u8], [116u8; 4]),);
        {
            v127 = (([16u8; 4], [33u8; 4]),);
            if ((((159931869944120718353370504580804933103u128, 7288658725784472704usize, (), "hello"), vec!['\0', '\'', '\n']) <= (if (a126) { ((4503599627370495u128, 1024usize, (), "héllo"), Vec::<char>::new()) } else { ((7u128, 10usize, (), "a,b,,c"), vec!['\u{d7ff}', 'a']) }))) {
                v127 = (([24u8, 127u8, 6u8, 255u8], [10u8; 4]),);
                let v128: (([u8; 4], [u8; 4]),) = std::mem::replace(&mut v127, (([64u8, 129u8, 16u8, 128u8], [64u8, 127u8, 8u8, 21u8]),));
                let mut v129: [i32; 1] = ({ let mut items = larger::<[i32; 1]>([19i32], [727582492i32]); items.reverse(); items });
                (unsafe { *v129.as_ptr().add(0) }).mix();
                v129.mix();
            } else {
                v127 = ((v127.0.1, ({ let mut items = v127.0.0; items.rotate_left(3); items })),);
            }
            let v137: S1<(&'static str, (i16, u8), String, bool)> = S1 { f0: (match ([("-7", ((-12550i16), 33u8), String::from("line\n"), false)].to_vec().as_slice()) { [(p144, (p145, p146), p147, true), .., (_, (p148, _), p149, true)] => (loop { break [("  pad  ", (32i16, 1u8), String::from("3.5"), true), ("-7", (19i16, 9u8), String::from("héllo"), false)]; }), [_, .., (p153, (p154, p155), p156, p157)] => ({ let mut items = [("😀", (8193i16, 2u8), String::from("😀"), true), ("ff", (2i16, 11u8), String::from("x y"), false)]; items.sort_unstable(); items }), _ => S1 { f0: [("-7", (4096i16, 236u8), String::from("line\n"), false), ("  pad  ", (16383i16, 8u8), String::from("line\n"), true)], f1: 13217u16, f2: 8388u16, f3: ("a,b,,c", (7i16, 9u8), String::from("  pad  "), false), f4: "ff", f5: 2599572512526005410u64 }.f0 }), f1: 8192u16.checked_add(('b162: { if (true) { break 'b162 60581u16; } a125 })).unwrap_or((a125.leading_zeros() as u16)), f2: (137438953471isize.wrapping_rem_euclid(2199023255552isize | 1) as u16), f3: (if (larger::<bool>(false, a126)) { ("Abc Def", (3i16, 104u8), String::from("Abc Def"), false) } else { ("a,b,,c", ((-5i16), 33u8), String::from("42"), a126) }), f4: "-7".get(0..7).unwrap_or("x y").trim_start(), f5: 10u64.wrapping_rem(6392804031246972977u64 | 1).overflowing_shr((38u32.checked_ilog2().unwrap_or(0) as u32)).0 };
            let f139 = move || -> ((&'static str,), u64, u16) { let v138 = v137; let out = ((v138.f3.0,), (&1024u64 as &dyn T2).m4(("Abc Def", (127i16, 127u8), String::from("x y"), false)), a125.checked_div(28528u16).unwrap_or(v138.f1)); drop(v138); out };
            f139().mix();
            id::<i16>(a123).mix();
        }
        (if ((unsafe { *(&false as *const bool) })) { NonZeroI64::new(70368744177663i64).unwrap_or(NonZeroI64::new(9223372036854775806i64).unwrap()) } else { NonZeroI64::new(34359738369i64).unwrap_or(NonZeroI64::new(9i64).unwrap()) })
    }
    #[inline(never)]
    pub fn f164<G163: Mix + Clone + Debug + PartialOrd>(a165: G163, a166: Box<String>) -> G163 {
        match (vec![(NonZeroU32::new(4294967295u32).unwrap(),), (NonZeroU32::new(33u32).unwrap(),), (NonZeroU32::new(536870913u32).unwrap(),), (NonZeroU32::new(7u32).unwrap(),)].as_slice()) {
            [.., (p169,)] => {
                let mut v172: bool = ((-f32::NAN) - (f64::INFINITY as f32)).is_nan();
                {
                    let mut f173 = |a170: u64, a171: u16| -> char { v172 = ((false ^ v172) < (if (v172) { v172 } else { v172 })); twice::<char>(&(*a166).clone().chars().nth(2).unwrap_or('z')).0 };
                    let v174: u16 = (if ((({ let t175: ((), (bool, i8), String, String, (NonZeroI64, i16, char), (NonZeroU8, i16)) = ((), (true, 19i8), String::from("héllo"), String::from("😀"), (NonZeroI64::new((-5978409721688939101i64)).unwrap(), 14i16, 'z'), (NonZeroU8::new(31u8).unwrap(), 32766i16)); Box::new(()) }) <= id::<Box<()>>(Box::new(())))) { 10u16.wrapping_sub(52599u16).overflowing_add(u16::from_str_radix("x y", 8).unwrap_or(40480u16)).0 } else { (unsafe { std::mem::transmute::<u32, u32>(40u32) } as u16).wrapping_div(26u16 | 1) });
                    let v176: NonZeroU32 = (*p169);
                    f173((unsafe { *(&12269282571099349479u64 as *const u64) }), 38496u16.checked_div(3u16).unwrap_or(11u16)).mix();
                }
                v172.mix();
                let mut v178: usize = (unsafe { *(&3usize as *const usize) }).checked_div(9usize).unwrap_or((*a166).clone().len());
                {
                    let mut f179 = |a177: ((u128, usize, (), &'static str), Vec<char>)| -> String { v178 = ((*a166).clone().len() as usize); ({ let mut items = vec![String::from("3.5"), String::from("x y"), String::from(""), String::from("x y"), String::from("-7")]; items.retain(|x180: &String| -> bool { true }); items }).join("Abc Def") };
                    let v181: Box<String> = larger::<Box<String>>((match (String::from("hello").as_str()) { "" if v172 => a166.clone(), "😀" => a166.clone(), _ => a166.clone() }), ('b182: { if (v172) { break 'b182 a166.clone(); } Box::new(String::from("hello")) }));
                    let mut v183: u16 = 38u16;
                    let mut v184: String = (*a166).clone();
                    f179(((173207536102028857067695845059580438446u128, 8589934591usize, (), "  pad  "), vec!['_'])).mix();
                }
                v178.mix();
            }
            _ => {}
        }
        let (_, _, _, p185, (p186, _, p187, _), p188): (&'static str, u128, char, &'static str, (i8, u32, i32, NonZeroU8), bool) = ("".get(2..2).unwrap_or("a,b,,c"), 1099511627777u128.checked_div(10u128).unwrap_or(22u128), (*a166).clone().chars().rev().nth(1).unwrap_or('中'), "hello".trim(), (8i8, 19u32, 1073741825i32, NonZeroU8::new(12u8).unwrap()), (("42", ((-170141183460469231731687303715884105728i128), 29064u16, 14isize)) >= ("-7", (10i128, 65535u16, (-8isize)))));
        (if (p188) { a165.clone() } else { a165.clone() })
    }
    pub fn f189(a190: u64, a191: bool, a192: String) -> Box<()> {
        match (vec![(170141183460469231731687303715884105727i128, NonZeroI64::new((-4638473290364693070i64)).unwrap()); (511usize.wrapping_rem_euclid(6usize | 1) % 6)].as_slice()) {
            [.., p195] if (unsafe { *(&false as *const bool) }) => {
                {
                    {
                        let mut items: Vec<Box<dyn Any>> = vec![Box::new(String::from("owned")), Box::new(()), Box::new("text"), Box::new([52u16; 3]), Box::new(Noisy(52))];
                        for item in &items { if let Some(it) = item.downcast_ref::<u32>() { feed(*it as u64); } else if let Some(it) = item.downcast_ref::<&str>() { it.mix(); } else if item.is::<Noisy>() { feed(1); } else if let Some(it) = item.downcast_ref::<(u8, bool)>() { feed(it.0 as u64 + it.1 as u64); } else { feed(2); } }
                        while let Some(item) = items.pop() { match item.downcast::<Noisy>() { Ok(it) => it.mix(), Err(other) => feed(other.is::<String>() as u64) } }
                    }
                    let mut v196: [isize; 4] = <[isize; 4]>::try_from(({ let mut items = vec![36028797018963967isize, 18isize, (-1isize), 8isize]; items.sort_by(|a, b| b.cmp(a)); items })).unwrap_or(({ let mut items = (match (vec!["3.5"].as_slice()) { [p207, p206 @ ..] if false => [6isize, 20isize, (-9isize), (-15isize)], [p209, .., _] if true => [(-7isize); 4], _ => [(-14isize); 4] }); items.reverse(); items }));
                    v196.mix();
                    let mut v210: [i64; 1] = ({ let mut items = [5881150332997549328i64]; items.sort_unstable(); items });
                    { let (x, y) = ((3usize.wrapping_add(7usize) % 1), (None::<()>.into_iter().filter_map(|x212: ()| -> Option<u64> { Some(24u64) }).zip(vec![32u64, 5259237838086745439u64, 19u64, 15098908156768477869u64, 2u64]).count() % 1)); let start = v210.as_mut_ptr(); unsafe { std::ptr::swap(start.add(x), start.add(y)) }; }
                    v210.mix();
                    {
                        struct Outer(Noisy, Bomb, Noisy);
                        impl Drop for Outer { fn drop(&mut self) { feed(0x0d0d + self.0.0 as u64); } }
                        caught(|| { let _a = Noisy(16); { let _inner = Box::new((Bomb(37), Noisy(38))); let _late = Noisy(32); } let _z = Noisy(49); });
                    }
                }
                {
                    let ((), ..): ((), (bool, i8), String, String, (NonZeroI64, i16, char), (NonZeroU8, i16)) = ((), (false, 0i8), a192.clone(), a192.clone(), (NonZeroI64::new((-19i64)).unwrap(), 8i16, '\n'), (NonZeroU8::new(1u8).unwrap(), 512i16));
                    let mut v216: bool = ((String::from("3.5"), String::from("héllo"), String::from("a,b,,c"), NonZeroI64::new((-19i64)).unwrap(), Box::new('Z'), 10u64) < (String::from("42"), String::from("  pad  "), String::from(""), NonZeroI64::new(4611686018427387903i64).unwrap(), Box::new('9'), 1u64));
                    let v217: bool = std::mem::replace(&mut v216, Some(((), (false, 43i8), String::from("Abc Def"), String::from("Abc Def"), (NonZeroI64::new(9007199254740992i64).unwrap(), 33i16, 'z'), (NonZeroU8::new(14u8).unwrap(), (-4i16)))).is_some());
                    {
                        struct Outer(Noisy, Bomb, Noisy);
                        impl Drop for Outer { fn drop(&mut self) { feed(0x0d0d + self.0.0 as u64); } }
                        caught(|| { let _a = Noisy(17); let _h0 = Box::new((Bomb(52), Noisy(45))); let _z = Noisy(33); });
                        caught(|| { let _a = Noisy(58); { let _inner = (Noisy(34), [Bomb(32)], Box::new(Noisy(10))); let _late = Noisy(51); } let _z = Noisy(46); });
                    }
                }
            }
            _ => {}
        }
        let [p222, .., p223]: [u8; 4] = (if (a191) { (unsafe { *(&[16u8, 120u8, 2u8, 16u8] as *const [u8; 4]) }) } else { [16u8, 4u8, 4u8, 64u8] });
        f164::<Box<()>>((if (a191) { (match (vec![NonZeroU8::new(39u8).unwrap()].as_slice()) { [_, .., _] => Box::new(()), [p226, .., p227] => Box::new(()), _ => Box::new(()) }) } else { ('b228: { if (false) { break 'b228 Box::new(()); } Box::new(()) }) }), (if (None::<(&'static str,)>.is_some()) { f164::<Box<String>>(Box::new(String::from("42")), Box::new(String::from("a"))) } else { twice::<Box<String>>(&Box::new(String::from("héllo"))).0 }))
    }
    pub fn run() {
        {
            let f230 = |a229: &'static str| -> bool { (unsafe { *(&2i16 as *const i16) }).is_positive() };
            let v231: ((), (bool, i8), String, String, (NonZeroI64, i16, char), (NonZeroU8, i16)) = (if ((match (Vec::<u128>::new().as_slice()) { [] => false, _ => false })) { ((), (true, 1i8), String::from("3.5"), String::from("x y"), (NonZeroI64::new(4i64).unwrap(), (-32768i16), ' '), (NonZeroU8::new(30u8).unwrap(), (-6063i16))) } else { sel::<((), (bool, i8), String, String, (NonZeroI64, i16, char), (NonZeroU8, i16))>(false, ((), (true, (-15i8)), String::from("a,b,,c"), String::from(""), (NonZeroI64::new(1376309402658870273i64).unwrap(), (-25501i16), '0'), (NonZeroU8::new(30u8).unwrap(), 17i16)), ((), (true, (-6i8)), String::from("42"), String::from("hello"), (NonZeroI64::new((-9223372036854775808i64)).unwrap(), 2i16, '\''), (NonZeroU8::new(254u8).unwrap(), 1i16))) });
            let mut n233 = 0u32;
            'l234: loop {
                n233 += 1;
                if n233 > 1 { break; }
                let v236: i64 = (-5630521140727797417i64);
            }
            f230(f164::<&'static str>("hello", Box::new(String::from("  pad  ")))).mix();
        }
        {
            static NUMBERS: [u32; 4] = [31, 29, 13, 4];
            static SECOND: &u32 = &NUMBERS[1];
            static TAIL: &[u32] = match &NUMBERS { [_, _, rest @ ..] => rest };
            static WORDS: [&str; 3] = ["one", "two", "three"];
            static PICKED: &&str = &WORDS[2];
            struct Link { value: u32, next: Option<&'static Link>, call: fn(u32) -> u32, name: &'static str, shown: &'static (dyn Debug + Sync) }
            fn double(x: u32) -> u32 { x.wrapping_mul(2) }
            static END: Link = Link { value: 3, next: None, call: double, name: "end", shown: &NUMBERS };
            static MID: Link = Link { value: 31, next: Some(&END), call: |x| x + 1, name: WORDS[0], shown: &"mid" };
            static HEAD: Link = Link { value: 29, next: Some(&MID), call: double, name: "head", shown: &MID.value };
            static TABLE: [fn(u32) -> u32; 3] = [double, |x| x ^ 5, HEAD.call];
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            static LISTS: (&[u8], &[&[u8]], [Option<&u32>; 2]) = (b"bytes", &[b"a", b"", &[31, 2]], [Some(&NUMBERS[3]), None]);
            const VIEW: &[u32; 4] = &NUMBERS;
            thread_local! { static LOCAL: Cell<u32> = const { Cell::new(5) }; static NOTES: RefCell<Vec<&'static u32>> = const { RefCell::new(Vec::new()) }; }
            { let mut at = Some(&HEAD); while let Some(link) = at { feed((link.call)(link.value) as u64); link.name.mix(); format!("{:?}", link.shown).mix(); at = link.next; } }
            feed(*SECOND as u64 + TAIL.len() as u64 + std::ptr::eq(SECOND, &NUMBERS[1]) as u64 * 16 + std::ptr::eq(TAIL.as_ptr(), &NUMBERS[2]) as u64 * 32);
            { let before = COUNTER.fetch_add(3, Ordering::Relaxed); feed((COUNTER.load(Ordering::Relaxed) - before) as u64); }
            PICKED.mix(); feed(std::ptr::eq(*PICKED, WORDS[2]) as u64);
        }
        let mut n237 = 0u32;
        'l238: loop {
            n237 += 1;
            if n237 > 8 { break; }
            let v240: u16 = 8192u16;
            let mut v241: S1<(&'static str, (i16, u8), String, bool)> = id::<S1<(&'static str, (i16, u8), String, bool)>>(sel::<S1<(&'static str, (i16, u8), String, bool)>>(true, S1 { f0: [("line\n", ((-17i16), 252u8), String::from("line\n"), false), ("hello", (3i16, 29u8), String::from("Abc Def"), false)], f1: 64u16, f2: 65534u16, f3: ("0123456789", (2i16, 14u8), String::from("ff"), false), f4: "  pad  ", f5: 25u64 }, S1 { f0: [("héllo", (32766i16, 13u8), String::from("Abc Def"), false), ("ff", (10i16, 2u8), String::from("42"), true)], f1: 2u16, f2: 4u16, f3: ("hello", (100i16, 1u8), String::from("a"), true), f4: "0123456789", f5: 1580509438637208978u64 }));
        }
        if (((Vec::<i128>::new() <= vec![85070591730234615865843651857942052863i128]) | larger::<bool>(false, false))) {
            if (true) {
                let mut v242: [(); 2] = [(); 2];
                { let v243: [(); 2] = (match (String::from("Abc Def".get(1..5).unwrap_or("Abc Def")).as_str()) { "a,b,,c" => f164::<[(); 2]>([(); 2], larger::<Box<String>>(Box::new(String::from("3.5")), Box::new(String::from("a")))), _ => f164::<[(); 2]>(id::<[(); 2]>([(), ()]), id::<Box<String>>(Box::new(String::from("hello")))) }); unsafe { std::ptr::copy_nonoverlapping(v243.as_ptr(), v242.as_mut_ptr(), 2) }; }
                v242.mix();
                match &mut v242 {
                    [(), .., _] => {
                        {
                            async fn work(seed: u32) -> u64 { let h252 = format!("{seed}-22"); let w253 = later(2, seed as u64 + 42).await; let h254: Option<Box<u16>> = if seed % 2 == 0 { Some(Box::new(7)) } else { None }; let w255 = later(0, [seed as u64; 2]).await[0]; let h256 = [seed as u8; 1]; let w257 = match later(1, seed % 3).await { 0 => later(0, 10u64).await, 1 => 11, _ => later(2, 12u64).await }; (h252.len() as u64).rotate_left(0) ^ (w253).rotate_left(1) ^ (h254.as_deref().copied().unwrap_or(1) as u64).rotate_left(2) ^ (w255).rotate_left(3) ^ (h256[0] as u64).rotate_left(4) ^ (w257).rotate_left(5) }
                            poll_some(work(59), 6).mix();
                            poll_some(work(28), 6).mix();
                        }
                    }
                    _ => {}
                }
            } else {
                {
                    let v258: NonZeroI64 = (match (vec![1152921504606846976u64].as_slice()) { [(18446744073709551614u64 | ..=9u64 | 13833968532029689199u64), (9223372036854775807u64 | 3172153442252196142u64)] => NonZeroI64::new(9223372036854775807i64).unwrap_or(NonZeroI64::new(1i64).unwrap()), _ => NonZeroI64::new(2019519267072283792i64).unwrap() });
                    let v260: NonZeroI64 = NonZeroI64::new((-9i64).checked_add(274877906943i64).unwrap_or(17179869183i64).wrapping_neg()).unwrap_or(larger::<NonZeroI64>((match (vec![Box::new(()), Box::new(()), Box::new(())].as_slice()) { [] => v258, [_, .., _] => v258, _ => v258 }), f122(7325i16, (NonZeroU32::new(9u32).unwrap(), vec![32767i16, 8i16, 7i16, 15452i16, 8i16]), 65535u16, false)));
                }
                let mut n263 = 0u32;
                let v265: S1<(&'static str, (i16, u8), String, bool)> = 'l264: loop {
                    n263 += 1;
                    if n263 > 2 { break S1 { f0: [("3.5", (18i16, 1u8), String::from("-7"), true), ("", ((-17i16), 7u8), String::from("42"), false)], f1: 7u16, f2: 65535u16, f3: ("a", ((-13742i16), 127u8), String::from("😀"), false), f4: "line\n", f5: 1096375833072487577u64 }; }
                    let mut v266: Vec<(u64, u32, u16)> = ({ let mut items = ({ let mut items = vec![(536870912u64, 4294967295u32, 54773u16), (4503599627370496u64, 2005401232u32, 7u16), (15u64, 5u32, 25454u16)]; items.push((37u64, 40u32, 12522u16)); items }); items.sort_unstable(); items });
                    {
                        for armed in [false, true] { caught(|| { take3(first_of(&Noisy(9), &Noisy(13)).0, Noisy(29), trip(armed, Noisy(27))); }); }
                        for armed in [false, true] { caught(|| { let mut held = vec![Noisy(28)]; held.push(trip(armed, Noisy(43))); held.insert(0, Noisy(47)); }); }
                        for armed in [false, true] { caught(|| { let _v = match Some(Noisy(11)) { Some(it) => (trip(armed, Noisy(12)), it), None => (Noisy(28), Noisy(44)) }; }); }
                        for armed in [false, true] { caught(|| { let _v = Some(Noisy(19)).map(|it| (trip(armed, Noisy(31)), it)); }); }
                    }
                };
            }
            {
                match ([vec![8589934593u64], vec![16u64]].concat().as_slice()) {
                    [17u64..8358266131987336983u64, .., ..=11275075181517471667u64] => {
                        {
                            for armed in [false, true] { caught(|| { let mut kept = Vec::new(); let mut i = 0u32; while i < 4 { let n = Noisy(i); if i == 2 { trip(armed, ()); } if i % 2 == 1 { kept.push(n); i += 1; continue; } let _m = Noisy(10 + i); if i == 1 { break; } i += 1; } feed(kept.len() as u64); }); }
                        }
                    }
                    _ => (NonZeroU32::new(25u32).unwrap(),).mix(),
                }
                {
                    trait Shape { fn area(&self) -> u64; fn scaled(&self, by: u64) -> u64 where Self: Sized { self.area() * by } }
                    trait Conv<T> { fn conv(&self) -> T; }
                    struct Sq(u64); struct Rect(u64, u64);
                    impl Shape for Sq { fn area(&self) -> u64 { self.0 * self.0 } }
                    impl Shape for Rect { fn area(&self) -> u64 { self.0 * self.1 } }
                    impl<T: Shape + ?Sized> Shape for Box<T> { fn area(&self) -> u64 { (**self).area() + 1 } }
                    impl Conv<u64> for Sq { fn conv(&self) -> u64 { self.0 } } impl Conv<String> for Sq { fn conv(&self) -> String { self.0.to_string() } }
                    impl dyn Shape { fn doubled(&self) -> u64 { self.area() * 2 } }
                    fn total<T: Shape + ?Sized>(it: &T) -> u64 { it.area() }
                    fn sum(shapes: &[&dyn Shape]) -> u64 { shapes.iter().map(|it| total(*it)).sum() }
                    let (sq, rect) = (Sq(36), Rect(36, 49));
                    feed(sum(&[&sq, &rect, &Sq(2)]));
                    { let mut calls = 0u64; let mut bump: Box<dyn FnMut(u64) -> u64> = Box::new(|by| { calls += by; calls }); bump(2); feed(bump(5)); }
                    feed(total(&sq) + total(&rect as &dyn Shape) + sq.scaled(3));
                    { let nested: Box<Box<dyn Shape>> = Box::new(Box::new(Sq(3))); feed(nested.area()); }
                    { let boxed: Box<dyn Shape> = Box::new(Rect(2, 3)); feed(total(&boxed) + boxed.doubled() + total(&*boxed)); }
                }
                let mut v271: bool = ("ff", (512i16, 13u8), String::from("  pad  "), true).3;
            }
        }
        let v272: (i64, u16) = (9223372036854775806i64.checked_sub(19i64).unwrap_or((5740993855784652940i64, 7u16).0), (86u8 as u16).wrapping_rem_euclid((false as u16) | 1));
        if let [(0u64..=18446744073709551615u64,), p274 @ ..] = (id::<Vec<(u64,)>>(vec![(100u64,), (18446744073709551615u64,), (3u64,), (12435728908878968681u64,), (8388608u64,)]).as_slice()) {
            let mut v275: BTreeMap<(String, Vec<i128>), Box<()>> = BTreeMap::new();
            v275.insert((String::from("line\n"), vec![71082831263855638380421968465703191237i128, 122134876049378784325044968116280703015i128, 7i128, 170141183460469231731687303715884105726i128, 85070591730234615865843651857942052863i128]), id::<Box<()>>(larger::<Box<()>>(Box::new(()), Box::new(())))).mix();
            v275.insert((String::from("x y"), ({ let mut items = Vec::<i128>::new(); items.sort_unstable(); items })), f189(0u64.checked_add(2u64).unwrap_or(3001718226445475805u64), (S1 { f0: [18446744073709551615u64, 14903570190600934292u64], f1: 9u16, f2: 40u16, f3: ("x y", (4096i16, 63u8), String::from("line\n"), true), f4: "line\n", f5: 8117595741544670776u64 } > S1 { f0: [0u64, 37u64], f1: 9u16, f2: 37u16, f3: ("héllo", (6i16, 7u8), String::from("3.5"), false), f4: "Abc Def", f5: 2147483648u64 }), String::from("héllo"))).mix();
            v275.insert((String::from("héllo"), Vec::<i128>::new()), (if ((match (vec![(7u128,)].as_slice()) { [p277 @ .., _] => true, [p278 @ .., (35u128..=267672723882900911535094467749275223774u128,)] => false, _ => true })) { (if (true) { Box::new(()) } else { Box::new(()) }) } else { (match (vec![(21usize, "Abc Def", (-2147483647i32), 8u16)].as_slice()) { [(_, ..), .., (_, _, p280 @ ..=256i32, 24u16..=23558u16)] => Box::new(()), _ => Box::new(()) }) })).mix();
            v275.insert((String::from("ff"), ({ let mut items = vec![(-166953688933191579158138508195638840683i128)]; items.sort_unstable(); items })), (loop { break (if (true) { Box::new(()) } else { Box::new(()) }); })).mix();
            v275.first_key_value().map(|(key, _)| key.clone()).mix();
            v275.len().mix();
            if (None::<(usize, &'static str, i32, u16)>.and_then(|x282: (usize, &'static str, i32, u16)| -> Option<((&'static str,), u64, u16)> { Some((("  pad  ",), 31u64, 512u16)) }).is_some()) {
                let mut v283: i32 = (&(2905061348835499408u64.overflowing_add(4294967297u64).1 as u64) as &dyn T2).m3(String::from("Abc Def").chars().nth(2).unwrap_or('Z'));
                v283 = larger::<i32>(v283.wrapping_div((-1i32) | 1), (unsafe { *(&v283 as *const i32) }));
                if (((true <= false) ^ (21usize != p274.len()))) {
                    v283 |= (unsafe { std::mem::transmute::<i128, [u8; 16]>(74991148259261459136414239905269411549i128.max(0i128)) }[1] as i32).wrapping_rem((0i128 as i32).checked_sub(T2::m3(&S1 { f0: [21u64, 39u64], f1: 8u16, f2: 9u16, f3: ("3.5", (1i16, 127u8), String::from("a,b,,c"), true), f4: "3.5", f5: 12841410297352338411u64 }, '9')).unwrap_or(128i32.wrapping_div_euclid(v283 | 1)) | 1);
                } else {
                    let v284: ((), (bool, i8), String, String, (NonZeroI64, i16, char), (NonZeroU8, i16)) = ((), (false, 17i8), String::from(""), String::from("hello"), (NonZeroI64::new((-1i64)).unwrap(), (-20177i16), 'a'), (NonZeroU8::new(8u8).unwrap(), (-32768i16)));
                }
            }
        } else {
            let mut v285: [i16; 4] = larger::<[i16; 4]>((match (String::from("0123456789").as_str()) { "0123456789" => [7i16, (-1i16), (-32768i16), 6i16], _ => [128i16, 511i16, (-1i16), 14i16] }), twice::<[i16; 4]>(&[(-32767i16); 4]).0);
            { let (x, y) = (0, ((0i32..4i32).fold(100usize, |acc287: usize, x288: i32| -> usize { acc287 }) % 4)); let start = v285.as_mut_ptr(); unsafe { std::ptr::swap(start.add(x), start.add(y)) }; }
            v285.mix();
        }
        match (({ let mut items = Vec::<i64>::new(); items.sort_unstable(); items }).as_slice()) {
            [-5i64..=7025606067693101305i64, .., (-7640591587381565412i64..=7i64 | -11i64..=2199023255553i64 | 3657431934438231099i64)] if true => {
                let mut v297: [u16; 8] = id::<[u16; 8]>([32767u16, 0u16, 4096u16, 7u16, 1u16, 36u16, 22u16, 40u16]);
                (unsafe { *v297.as_ptr().add(3) }).mix();
                v297.mix();
            }
            [p302 @ 9i64..=6043970613535983320i64, p301 @ ..] if (-406.4809f32).is_normal() => {
                let v303: (i64, u16) = (larger::<i64>(1i64, v272.0), v272.1.to_be());
                let v304: char = String::from_utf8_lossy(&({ let mut items = Vec::<u8>::new(); items.reverse(); items })).into_owned().chars().last().unwrap_or(larger::<char>(String::from("  pad  ").chars().next().unwrap_or('\0'), String::from("héllo").chars().rev().nth(1).unwrap_or('\'')));
            }
            _ => ({ let t306: (&'static str, u128, char, &'static str, (i8, u32, i32, NonZeroU8), bool) = ("a,b,,c", 2u128, '\u{80}', "0123456789", ((-23i8), 3917923391u32, 7i32, NonZeroU8::new(1u8).unwrap()), false); ((17179869183u128, 8usize, (), ""), vec!['\u{80}']) }).mix(),
        }
        let v307: (NonZeroU8, usize) = (f164::<NonZeroU8>(NonZeroU8::new(100u8).unwrap(), id::<Box<String>>(Box::new(String::from("a,b,,c")))), ({ let mut items = vec![vec![10i128], vec![(-36413113122033958370856123157311683044i128), 6266291706766194515960911114487061797i128, 85070591730234615865843651857942052863i128], vec![1i128, (-4i128), 1i128], Vec::<i128>::new(), vec![(-18i128)]]; items.sort(); items }).into_iter().zip(vec![(15i64, 4097u16), (10i64, 16u16)]).count());
        let v310: ((u128, usize, (), &'static str), Vec<char>) = ((match (Vec::<char>::new().as_slice()) { ['\u{80}'..='\u{10FFFF}', .., p312] => (123282197747589046115677977870197388350u128, 1054071252395083939usize, (), "-7"), [p314 @ 'A'..='Z', .., p315] => (549755813887u128, 4398046511104usize, (), "3.5"), _ => (340282366920938463463374607431768211455u128, 18446744073709551615usize, (), "  pad  ") }), String::from("").chars().collect::<Vec<char>>());
        let mut v317: [(); 4] = larger::<[(); 4]>(larger::<[(); 4]>([(), (), (), ()], [(); 4]), std::array::from_fn::<(), 4, _>(|i323: usize| -> () { v310.0.2 }));
        (unsafe { *v317.as_ptr().add((v310.0.1 % 4)) }).mix();
        v317.mix();
        v317 = ({ let mut items = twice::<[(); 4]>(&[(), (), (), ()]).1; items.swap(0, 3); items });
        let ((p329,),): (([u8; 4],),) = (([3u8, 28u8, 9u8, 8u8],),);
        if (None::<(usize, &'static str, i32, u16)>.is_some()) {
            let v331: u128 = 9u128;
            let mut v332: [(); 4] = v317;
            {
                for armed in [false, true] { caught(|| { let t = [Noisy(29), Noisy(58), Noisy(16)]; let [_x, ..] = t; trip(armed, ()); }); }
                for armed in [false, true] { caught(|| { let t = (Noisy(29), Box::new(Duo { a: Noisy(58), b: Noisy(16) })); trip(armed, ()); let _x = t.1.a; let _late = Noisy(12); }); }
                for armed in [false, true] { caught(|| { let t = (Some(Noisy(29)), Duo { a: Noisy(58), b: Noisy(16) }); let _x = t.1.b; let _y = t.0; let _w = t.1.a; trip(armed, ()); }); }
            }
        }
        let v333: [(); 4] = std::mem::replace(&mut v317, [(), (), (), ()]);
    }
}

mod c1 {
    use super::*;
    #[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord, Hash)]
    #[repr(i8)]
    pub enum E1 {
        V0 { f0: u128, f1: u32, f2: Vec<Vec<bool>> },
        V1(Result<Result<u8, u16>, Result<bool, bool>>, i8, char),
        V2(char),
    }
    impl Mix for E1 {
        fn mix(&self) {
            match self {
                E1::V0 { f0: a0, f1: a1, f2: a2 } => { feed(10); a0.mix(); a1.mix(); a2.mix(); }
                E1::V1(a0, a1, a2) => { feed(11); a0.mix(); a1.mix(); a2.mix(); }
                E1::V2(a0) => { feed(12); a0.mix(); }
            }
        }
    }
    static mut K2: u128 = (35u128 ^ 134217727u128);
    static K3: Result<Result<bool, f64>, Result<i8, f64>> = Ok::<Result<bool, f64>, Result<i8, f64>>(Ok::<bool, f64>(true));
    static K4: Result<Result<u8, u16>, Result<bool, bool>> = Ok::<Result<u8, u16>, Result<bool, bool>>(Ok::<u8, u16>((if 8u8 >= 2u8 { 8u8 } else { 2u8 })));
    pub trait T5 {
        fn m6(&self, a0: char) -> Result<Result<bool, f64>, Result<i8, f64>>;
        fn m7(&self, a0: i8) -> Result<Vec<u8>, E1>;
        fn m8(&mut self, a0: i8) -> i8;
    }
    impl T5 for Vec<Vec<bool>> {
        fn m6(&self, a9: char) -> Result<Result<bool, f64>, Result<i8, f64>> {
            (match (&{ E1::V0 { f0: 284434544489860383939024773513609867994u128, f1: 4013446316u32, f2: Vec::<Vec<bool>>::new() } }) { E1::V1(Ok(Ok(_)), p11, p12) => K3, _ => Ok::<Result<bool, f64>, Result<i8, f64>>(Ok::<bool, f64>(false)) })
        }
        fn m7(&self, a13: i8) -> Result<Vec<u8>, E1> {
            {
                let v14: Box<E1> = sel::<Box<E1>>((match (&{ Box::new(E1::V2('z')) }) { p15 if false => true, _ => true }), Box::new(E1::V1(Ok::<Result<u8, u16>, Result<bool, bool>>(Err::<u8, u16>(1u16)), 1i8, '\u{7f}')), id::<Box<E1>>(Box::new(E1::V1(Ok::<Result<u8, u16>, Result<bool, bool>>(Err::<u8, u16>(32767u16)), 10i8, 'z'))));
                let mut v16: [char; 2] = [(3u8 as char), 'ß'];
                (unsafe { *v16.as_ptr().add(1) }).mix();
                v16.mix();
            }
            None::<Vec<u8>>.ok_or(E1::V2('\u{80}'))
        }
        fn m8(&mut self, a21: i8) -> i8 {
            let mut v22: i8 = (3.25f32 as i8);
            ((-9223372036854775808.0f64) as i8)
        }
    }
    impl T5 for u128 {
        fn m6(&self, a23: char) -> Result<Result<bool, f64>, Result<i8, f64>> {
            sel::<Result<Result<bool, f64>, Result<i8, f64>>>((id::<Result<Result<bool, f64>, Result<i8, f64>>>(K3) >= sel::<Result<Result<bool, f64>, Result<i8, f64>>>(true, K3, K3)), Err::<u32, Result<bool, f64>>(Ok::<bool, f64>(false)).err().ok_or(sel::<Result<i8, f64>>(false, Ok::<i8, f64>(65i8), Err::<i8, f64>(3.25f64))), (match (apply(|x31: Box<String>| -> String { (*x31).clone() }, Box::new(String::from("ff"))).as_str()) { "a,b,,c" => (unsafe { let mut slot = MaybeUninit::<Result<Result<bool, f64>, Result<i8, f64>>>::uninit(); slot.write(Ok::<Result<bool, f64>, Result<i8, f64>>(Ok::<bool, f64>(false))); slot.assume_init() }), "-7" => Err::<Result<bool, f64>, Vec<Vec<bool>>>(vec![vec![true]]).or_else(|x33: Vec<Vec<bool>>| -> Result<Result<bool, f64>, Result<i8, f64>> { Ok::<Result<bool, f64>, Result<i8, f64>>(Ok::<bool, f64>(false)) }), _ => Ok::<Result<bool, f64>, Result<i8, f64>>(Ok::<bool, f64>(false)) }))
        }
        fn m7(&self, a34: i8) -> Result<Vec<u8>, E1> {
            None::<Result<Box<bool>, Box<i128>>>.ok_or(E1::V0 { f0: 65564692165419172559254419304022083259u128, f1: 35u32, f2: vec![vec![false, false, true, true, false], vec![false, false], vec![true, true, true, false, true]] }).map(|x36: Result<Box<bool>, Box<i128>>| -> Result<Result<u8, u16>, Result<bool, bool>> { Ok::<Result<u8, u16>, i8>(Err::<u8, u16>(26711u16)).or_else(|x38: i8| -> Result<Result<u8, u16>, Result<bool, bool>> { K4 }) }).map(|x35: Result<Result<u8, u16>, Result<bool, bool>>| -> Vec<u8> { (match (Ok::<Result<u8, u16>, Box<Vec<bool>>>(Err::<u8, u16>(65534u16)).or_else(|x40: Box<Vec<bool>>| -> Result<Result<u8, u16>, Result<bool, bool>> { x35 })) { Err(Err(false)) if (100u32 >= 10u32) => vec![36u8, 38u8, 177u8], _ => ({ let mut items = vec![189u8, 8u8, 10u8]; items.dedup(); items }) }) })
        }
        fn m8(&mut self, a42: i8) -> i8 {
            let mut v43: Vec<String> = ({ let mut items = vec![String::from("a,b,,c"), String::from("0123456789")]; items.push(String::from("  pad  ")); items });
            { let value = Some(295736281742822913267537347128435696154u128).map_or((*self), |x45: u128| -> u128 { (unsafe { K2 }) }).max(3u128); unsafe { K2 = value; } }
            100i8.checked_add(a42).unwrap_or(a42)
        }
    }
    #[inline]
    pub fn f46(a47: E1, a48: &i8) -> u16 {
        sel::<u16>(65535.0f32.is_infinite(), 25u16.checked_shr(31u32).unwrap_or(65534u16), 32767u16.checked_shl(2u32).unwrap_or(3u16))
    }
    pub fn f49(a50: &[Vec<Result<bool, f32>>], a51: Box<String>) -> Box<E1> {
        let mut v52: Vec<Box<Result<i8, u128>>> = vec![Box::new(Ok::<i8, u128>(15i8)), Box::new(Err::<i8, u128>(31u128))];
        {
            let mut v54: Result<Result<u8, u16>, Result<bool, bool>> = sel::<Result<Result<u8, u16>, Result<bool, bool>>>((false && false), K4, (match (E1::V2('\u{d7ff}')) { E1::V0 { f1: p57, f2: p58, .. } => twice::<Result<Result<u8, u16>, Result<bool, bool>>>(&Ok::<Result<u8, u16>, Result<bool, bool>>(Err::<u8, u16>(65535u16))).1, E1::V1(p60, p61, p62) => (loop { break K4; }), E1::V2(_) => Err::<Result<u8, u16>, Result<bool, bool>>(Ok::<bool, bool>(true)) }));
            let mut v65: [u32; 4] = <[u32; 4]>::try_from([vec![12u32, 1u32], vec![7u32, 29u32, 25u32]].concat()).unwrap_or([37u32; 4]);
            (unsafe { *v65.as_ptr().add(2) }).mix();
            v65.mix();
            {
                async fn leaf(x: u64) -> u64 { let _g = Noisy(x as u32 % 9); later((x % 3) as u32, x).await }
                let futures: Vec<Pin<Box<dyn Future<Output = u64>>>> = vec![Box::pin(leaf(23)), Box::pin(async { 12 }), Box::pin(later(2, 9u64)), Box::pin(async { leaf(1).await + leaf(2).await })];
                for (i, future) in futures.into_iter().enumerate() { poll_some(future, i as u32 % 1).mix(); }
            }
        }
        {
            #[inline(never)] fn spread(a: u8, b: u128, c: u64, d: u128, e: u32, f: u128, g: u128) -> (u128, u8, u128) { (b.wrapping_add(d) ^ f, a.wrapping_add(c as u8).wrapping_add(e as u8), g.wrapping_sub(b)) }
            #[inline(never)] extern "C" fn c_side(a: u128, b: u8, c: u128) -> u128 { a.wrapping_mul(3).wrapping_add(c) ^ b as u128 }
            #[inline(never)] fn by_ref(a: &u128, b: &mut u128) -> u128 { *b = b.wrapping_add(*a); *a ^ *b }
            #[derive(Clone, Copy)] struct Wide { tag: u8, value: u128, pair: [u128; 2] }
            #[inline(never)] fn pass(w: Wide, extra: Option<u128>) -> Wide { Wide { tag: w.tag.wrapping_add(1), value: w.pair[1] ^ extra.unwrap_or(1), pair: [w.value, w.pair[0]] } }
            let (a, b): (u128, u128) = (black_box((10) as u128), black_box((0x0123_4567_89ab_cdef_fedc_ba98_7654_3210) as u128));
            let got = spread(1, a, 2, b, 3, a ^ b, !a); got.0.mix(); got.1.mix(); got.2.mix();
            c_side(a, 9, b).mix(); { let mut m = b; by_ref(&a, &mut m).mix(); m.mix(); }
            { let w = pass(Wide { tag: 7, value: a, pair: [b, a] }, Some(b)); w.tag.mix(); w.value.mix(); w.pair.mix(); pass(w, None).value.mix(); }
            { let f: fn(u128, u8, u128) -> u128 = |x, y, z| x ^ z ^ y as u128; f(a, 1, b).mix(); let g: extern "C" fn(u128, u8, u128) -> u128 = c_side; g(b, 2, a).mix(); }
        }
        (match (&{ Err::<Result<u8, u16>, Result<bool, bool>>(Ok::<bool, bool>(false)) }) { Err(p73) => (loop { break Box::new(E1::V1(Ok::<Result<u8, u16>, Result<bool, bool>>(Ok::<u8, u16>(64u8)), (-1i8), '\u{10FFFF}')); }), Err(Ok(true)) => apply_once(|| -> Box<E1> { Box::new(E1::V0 { f0: 10u128, f1: 2640320815u32, f2: vec![vec![true, false], vec![true, false, false, true, false]] }) }), _ => id::<Box<E1>>(Box::new(E1::V1(Ok::<Result<u8, u16>, Result<bool, bool>>(Err::<u8, u16>(60906u16)), (-91i8), '\''))) })
    }
    #[inline]
    pub extern "C" fn f75(a76: i8, a77: u32, a78: i8) -> u32 {
        {
            enum Loud { I(i32, Noisy), C(char, Noisy) }
            #[inline(never)] fn items(k: u32, last: char) -> [Loud; 3] { black_box([Loud::I(k as i32 % 4, Noisy(k)), Loud::C('q', Noisy(k + 1)), if k % 3 == 0 { Loud::I(1, Noisy(k + 2)) } else { Loud::C(last, Noisy(k + 2)) }]) }
            #[inline(never)] fn pick0(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { [x @ (Loud::I(..) | Loud::C('a'..='m', _)), _, Loud::C('a', _) | Loud::I(1, _)] => 1, [Loud::C('a', _) | Loud::C(_, _), Loud::I(..), y] if black_box(flag) => 2, [.., _] if black_box(!flag) => 3, _ => 0 } }
            for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick0(k, last, flag) as u64); feed(0xf00d); } } }
            #[inline(never)] fn pick1(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { [Loud::I(..), Loud::I(..), Loud::I(7, _)] => 1, _ if black_box(flag) => 2, _ => 0 } }
            for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick1(k, last, flag) as u64); feed(0xf00d); } } }
            #[inline(never)] fn pick2(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { [x @ (Loud::I(..) | Loud::C('a'..='m', _)), ..] if black_box(flag) => 1, [Loud::C('b', _), Loud::C('a'..='c', _) | Loud::I(_, _), Loud::C(_, _)] if black_box(!flag) => 2, _ => 0 } }
            for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick2(k, last, flag) as u64); feed(0xf00d); } } }
        }
        let mut v79: Result<Result<bool, f64>, Result<i8, f64>> = ('b81: { if (sel::<bool>(true, true, false)) { break 'b81 (match (&{ Ok::<Box<u8>, Result<bool, i64>>(Box::new(163u8)) }) { Ok(_) => Ok::<Result<bool, f64>, Result<i8, f64>>(Ok::<bool, f64>(false)), Ok(p83) => K3, _ => K3 }); } K3 });
        u32::try_from(6u8.wrapping_mul(1u8)).unwrap_or(a77)
    }
    #[inline]
    pub fn f85(a86: Result<String, Result<u16, bool>>, a87: &Vec<String>, a88: Vec<Result<bool, f32>>) -> Box<Vec<bool>> {
        let mut v89: Result<Result<bool, f64>, Result<i8, f64>> = larger::<Result<Result<bool, f64>, Result<i8, f64>>>((Box::new((unsafe { K2 }).checked_shl(22u32).unwrap_or((unsafe { K2 }))) as Box<dyn T5>).m6('\\'), Err::<Option<Result<bool, f64>>, Box<Result<i64, bool>>>(Box::new(Ok::<i64, bool>(17i64))).unwrap_or_else(|x94: Box<Result<i64, bool>>| -> Option<Result<bool, f64>> { Some(Err::<bool, f64>(0.0f64)) }).ok_or(i8::try_from(8u16).map_err(|_| -> f64 { (-9223372036854775808.0f64) })));
        Box::new(vec![true, false, false])
    }
    pub fn run() {
        let mut v96: Result<Result<bool, f64>, Result<i8, f64>> = (match (format!("<{:?}>", sel::<Result<Result<u8, u16>, Result<bool, bool>>>(true, K4, Err::<Result<u8, u16>, Result<bool, bool>>(Err::<bool, bool>(false)))).as_str()) { "😀" => larger::<Result<Result<bool, f64>, Result<i8, f64>>>(sel::<Result<Result<bool, f64>, Result<i8, f64>>>(true, K3, K3), Err::<Result<bool, f64>, Result<i8, f64>>(Err::<i8, f64>(f64::EPSILON))), _ => Err::<Result<bool, f64>, Result<i8, f64>>(Ok::<Result<i8, f64>, u32>(Ok::<i8, f64>(89i8)).unwrap_or_else(|x104: u32| -> Result<i8, f64> { Err::<i8, f64>(1e-40f64) })) });
        (match (&{ Err::<Box<u8>, Result<bool, i64>>((|x107: String| -> Result<bool, i64> { Ok::<bool, i64>(false) })(String::from("Abc Def"))) }) { Err(_) => (match (&{ Ok::<Result<u8, u16>, Box<String>>(Err::<u8, u16>(4u16)).or_else(|x108: Box<String>| -> Result<Result<u8, u16>, Result<bool, bool>> { K4 }) }) { Ok(Ok(_)) => Box::new(Err::<i8, bool>(false)), Err(Ok(true)) => twice::<Box<Result<i8, bool>>>(&Box::new(Ok::<i8, bool>(50i8))).0, _ => (match (&{ v96 }) { Err(Ok(p109)) if true => Box::new(Err::<i8, bool>(false)), _ => Box::new(Err::<i8, bool>(true)) }) }), _ => twice::<Box<Result<i8, bool>>>(&(unsafe { let mut slot = MaybeUninit::<Box<Result<i8, bool>>>::uninit(); slot.write(Box::new(Err::<i8, bool>(false))); slot.assume_init() })).1 }).mix();
        let mut o110: Vec<Box<dyn T5>> = vec![Box::new((match (&{ Some(vec![10u8, 28u8, 129u8, 111u8, 127u8]).ok_or(E1::V2('😀')) }) { Err(E1::V1(Ok(_), ..=9i8, p113)) => ((unsafe { K2 }).abs_diff(5u128) as u128), Err(E1::V2(p114)) => ((unsafe { K2 }) & (unsafe { K2 })), _ => (unsafe { K2 }).wrapping_rem_euclid((unsafe { K2 }) | 1) }).wrapping_div_euclid((unsafe { K2 }) | 1)), Box::new(pick::<[Vec<bool>; 3]>(true, &[vec![false, true], Vec::<bool>::new(), Vec::<bool>::new()], &[vec![false, true], vec![true, false, true], vec![true]]).clone().to_vec()), Box::new((unsafe { K2 }))];
        for o111 in o110.iter() { o111.m7(i8::try_from(100u8).unwrap_or(3i8)).mix(); }
        let [_, p121, _, p122]: [u8; 4] = [(223u8.checked_ilog2().unwrap_or(0) as u8), (127u8 << (8192u32 % 8)), ('b120: { if (true) { break 'b120 63u8; } 107u8 }), (4u8 & 50u8)];
        let mut v123: [u16; 1] = ({ let mut items = <[u16; 1]>::try_from(Vec::<u16>::new()).unwrap_or([21u16; 1]); items.reverse(); items });
        { let (x, y) = ((11usize.wrapping_rem_euclid(9367923078914693757usize | 1) % 1), (6usize.isqrt() % 1)); let start = v123.as_mut_ptr(); unsafe { std::ptr::swap(start.add(x), start.add(y)) }; }
        v123.mix();
        let mut v129: [u128; 1] = (match (apply_once(|| -> Result<E1, Result<u32, bool>> { Err::<E1, Result<u32, bool>>(Ok::<u32, bool>(1345096204u32)) })) { Err(Err(p135)) => <[u128; 1]>::try_from(vec![25u128, 19u128]).unwrap_or([134714927168999507415111719217352891333u128; 1]), _ => (match (vec![Ok::<bool, f32>(true), Err::<bool, f32>(0.5f32), Err::<bool, f32>(16777217.0f32), Ok::<bool, f32>(false), Ok::<bool, f32>(false)]) { _ if true => [174327136769565408333966448766023226420u128], _ => [2047u128; 1] }), Err(Ok(_)) if p122.overflowing_add(20u8).1 => (match (Ok::<Vec<bool>, Box<i128>>(vec![true, false, true, false, false])) { Err(p142) => [337425325722742495336729046071308910343u128; 1], Err(_) => [111995299511118459465320954297340934229u128; 1], _ => [4294967295u128] }), _ => [(-13i8)].map(|x144: i8| -> u128 { 4398046511104u128 }) });
        v129.mix();
        let mut v145: Result<Result<bool, f64>, Result<i8, f64>> = (match (&{ None::<E1>.ok_or(Ok::<u32, bool>(549329198u32)) }) { Err(Ok(p148)) => (match (&{ v123[0] }) { p150 => K3, p151 @ ..55726u16 => K3, _ => v96 }), Err(Err(p152)) => None::<Result<bool, f64>>.ok_or(Ok::<i8, f64>(9i8)), Ok(E1::V2(p154)) => Err::<Result<bool, f64>, Result<i8, f64>>(Ok::<i8, f64>(3i8)), _ => ('b157: { if (false) { break 'b157 v96; } K3 }) });
        v123.swap(0, 0);
        (match (&{ E1::V1(Err::<Result<u8, u16>, Result<bool, bool>>(Ok::<bool, bool>(true)), (-80i8), '\u{7f}') }) { E1::V1(Err(Err(p160)), _, '\u{80}'..='\u{10FFFF}') => v123, E1::V0 { f1: p161, f2: _, .. } => v123, E1::V1(_, p162 @ -128i8.., p163 @ 'a'..='z') => [0u16], _ => v123 }).mix();
        match &mut v129 {
            [.., p164 @ 25u128..=9903520314283042199192993791u128] => {
                let mut o165: Box<dyn T5> = Box::new(({ let mut items = ({ let mut items = vec![vec![true, false, false], vec![false, false, false], Vec::<bool>::new()]; items.retain(|x168: &Vec<bool>| -> bool { true }); items }); items.push([false, false, true].to_vec()); items }));
            }
            [p169 @ 27u128..2417851639229258349412352u128] => {
                let v170: [u16; 1] = std::mem::replace(&mut v123, [29u16]);
            }
            _ => {}
        }
        match &mut v145 {
            Ok(Ok(p171)) => {
                apply_once(|| -> Result<Result<bool, f32>, Vec<f32>> { sel::<Result<Result<bool, f32>, Vec<f32>>>((-1.0f32).is_nan(), (match (&{ Err::<Box<bool>, Box<i128>>(Box::new(1048575i128)) }) { Err(_) => Ok::<Result<bool, f32>, Vec<f32>>(Ok::<bool, f32>(true)), p176 => Ok::<Result<bool, f32>, Vec<f32>>(Ok::<bool, f32>(true)), _ => Err::<Result<bool, f32>, Vec<f32>>(vec![f32::MIN_POSITIVE, 957.0924f32]) }), Err::<Result<bool, f32>, Result<Result<i32, f64>, Result<&'static str, bool>>>(Ok::<Result<i32, f64>, Result<&'static str, bool>>(Ok::<i32, f64>((-1563828986i32)))).or_else(|x177: Result<Result<i32, f64>, Result<&'static str, bool>>| -> Result<Result<bool, f32>, Vec<f32>> { Ok::<Result<bool, f32>, Vec<f32>>(Ok::<bool, f32>(false)) })) }).mix();
            }
            _ => {}
        }
        {
            { let mut kept = Vec::new(); for i in 0..5u32 { let n = Noisy(i); let by_ref = || n.0 * 2; by_ref().mix(); if i % 2 == 0 { kept.push(move || n.0); } } for f in kept.iter().rev() { f().mix(); } }
            { let mut count = 0u32; let mut step = |by: u32| { count += by; count }; for i in 0..5 { step(i).mix(); } count.mix(); }
            { let mut fs: Vec<Box<dyn FnMut() -> u32>> = Vec::new(); for i in 0..5u32 { let mut own = i; fs.push(Box::new(move || { own += 56; own })); } for round in 0..2 { for f in fs.iter_mut() { feed(f() as u64 + round); } } }
            { for i in 0..5u32 { let n = Noisy(i); let once = move || n.0 + 56; if i != 1 { once().mix(); } } }
        }
        let mut v178: [u16; 1] = v123;
        let mut o179: Box<dyn T5> = Box::new([(match (&{ Ok::<Vec<u8>, Vec<u8>>(vec![31u8, 9u8]) }) { Err(_) => Vec::<Vec<bool>>::new(), Ok(_) => vec![vec![true, false]], Ok(p182) if true => vec![vec![true]], _ => vec![vec![false], vec![false, true], Vec::<bool>::new(), vec![true, true, true, false, true], Vec::<bool>::new()] }), vec![vec![true, false, true]]].concat());
    }
}

mod c2 {
    use super::*;
    #[derive(Clone, Debug, PartialEq, PartialOrd, Copy, Eq, Ord, Hash)]
    pub struct S1;
    impl Mix for S1 {
        fn mix(&self) {
            feed(0);
        }
    }
    static K2: NonZeroU8 = NonZeroU8::new(255u8).unwrap();
    static K3: NonZeroU8 = NonZeroU8::new(254u8).unwrap();
    pub trait T4 {
        fn m5(&self, a0: u128) -> S1;
        fn m6(&self, a0: S1, a1: S1) -> Vec<Vec<char>>;
    }
    impl T4 for u128 {
        fn m5(&self, a7: u128) -> S1 {
            twice::<S1>(&S1).0
        }
        fn m6(&self, a8: S1, a9: S1) -> Vec<Vec<char>> {
            {
                async fn leaf(x: u64) -> u64 { let _g = Noisy(x as u32 % 9); later((x % 3) as u32, x).await }
                for armed in [false, true] { caught(|| { let got = block_on(async { let _n = Noisy(5); let a = leaf(2).await; trip(armed, ()); let _m = Noisy(2); a + leaf(4).await }); feed(got); }); }
                caught(|| { let got = poll_some(async { let _b = Bomb(5); leaf(2).await }, 2); feed(got.is_some() as u64); });
            }
            if ((if ((match (&{ a9 }) { S1 if true => false, _ => true })) { (unsafe { *(&true as *const bool) }) } else { true })) {
                let v10: u64 = (unsafe { *(&9223372036854775807u64 as *const u64) });
                let mut v11: Vec<u64> = vec![24u64, 8u64];
            } else {
                let v13: Vec<Vec<char>> = ({ let mut items = Vec::<Vec<char>>::new(); items.reverse(); items });
            }
            larger::<Vec<Vec<char>>>(({ let mut items = ({ let mut items = vec![vec!['9', '\u{10FFFF}']]; items.reverse(); items }); items.insert(0, vec!['😀']); items }), ('b20: { if ((false || true)) { break 'b20 vec![vec!['\u{10FFFF}', '\u{80}']; ((f64::MIN_POSITIVE as usize) % 6)]; } [vec![vec!['\u{7f}'], vec!['Z', '\\', 'ß'], Vec::<char>::new()], vec![vec!['\u{e000}', 'é', '中', '\\', '\0'], vec!['\u{80}', 'ß', '\u{e000}', '\'', '0'], vec!['😀'], Vec::<char>::new(), vec!['😀', '😀']]].concat() }))
        }
    }
    impl T4 for i128 {
        fn m5(&self, a23: u128) -> S1 {
            (match (({ let t24: char = '\0'; S1 })) { S1 if ([Some(1u128), Some(1u128), Some(576460752303423489u128)] >= [None::<u128>; 3]) => twice::<S1>(&S1).0, _ => (match (vec![9i128, 17179869183i128, (-8i128), 4294967295i128, 3i128].as_slice()) { [8i128, p26] => S1, [p28, p29, 17i128] => S1, _ => S1 }) })
        }
        fn m6(&self, a30: S1, a31: S1) -> Vec<Vec<char>> {
            vec![Vec::<char>::new()]
        }
    }
    impl T4 for Box<Box<u16>> {
        fn m5(&self, a32: u128) -> S1 {
            'l34: for i33 in ((20u64..28u64).rev()) {
                (unsafe { *(&larger::<([i128; 1],)>((match (NonZeroU8::new(16u8).unwrap()) { p35 => ([281474976710655i128; 1],), _ => ([(-3i128)],) }), ([1i128; 1],)) as *const ([i128; 1],)) }).mix();
                let mut v36: NonZeroU8 = K2;
                ({ let mut items = [['a']; 1]; items.reverse(); items }).mix();
            }
            ('b39: { if ((vec![Vec::<char>::new()] == vec![Vec::<char>::new(), vec!['0', '\u{7f}']])) { break 'b39 S1; } S1 })
        }
        fn m6(&self, a40: S1, a41: S1) -> Vec<Vec<char>> {
            vec![vec!['9'], vec!['\u{7f}'], vec!['ß']]
        }
    }
    #[inline(never)]
    pub fn f45<G43: Mix + Clone + Debug + PartialOrd + T4, G44: Mix + Clone + Debug + PartialOrd>(a46: G43, a47: G44) -> Box<Box<u16>> {
        (if ((({ let t50: i128 = (-63600508693481826314741712179904569442i128); [Some(10u128); 3] }) != [23u128; 3].map(|x52: u128| -> Option<u128> { None::<u128> }))) { ({ let t53: [Option<u128>; 3] = [Some(158456325028528675187087900671u128); 3]; larger::<Box<Box<u16>>>(Box::new(Box::new(1025u16)), Box::new(Box::new(24u16))) }) } else { (if let Some(x54) = (None::<[[char; 1]; 1]>.and_then(|x55: [[char; 1]; 1]| -> Option<Box<Box<u16>>> { Some(Box::new(Box::new(50418u16))) })) { x54 } else { Box::new(Box::new(8u16)) }) })
    }
    #[inline]
    pub fn f58<G56: Mix + Clone + Debug + PartialOrd, G57: Mix + Clone + Debug + PartialOrd>(a59: G56, a60: G57, a61: Box<G57>) -> G56 {
        if ((!(false && false))) {
            (match (vec![664613997892457936451903530140172288u128, 7u128, 30u128, 1180591620717411303425u128, 32767u128].as_slice()) { [] => [[Some(10u128), Some(16154913052818414817605469184925902512u128), Some(340282366920938463463374607431768211455u128)]].map(|x66: [Option<u128>; 3]| -> [char; 1] { ['\0'].map(|x70: char| -> char { 'z' }) }), _ => ('b73: { if (([[' ']] <= [['Z']; 1])) { break 'b73 [['\u{10FFFF}']]; } [[Box::new(f64::MIN), Box::new(1.5f64)]].map(|x77: [Box<f64>; 2]| -> [char; 1] { ['中'] }) }) }).mix();
        }
        match ((match (&{ [Box::new(f64::NAN), Box::new((-176.0297f64))] }) { [_, p80] => [['\u{80}'; 1]], [p81, ..] => [['0'; 1]; 1], _ => [['中']; 1] })) {
            [['\0'..='\u{7f}', ..], ..] if [vec![vec!['Z', '\0']]].into_iter().all(|x82: Vec<Vec<char>>| -> bool { false }) => {
                let mut v83: G57 = (*a61).clone();
                if (false) {
                    (match (Box::new(Box::new(20u16))) { _ => v83.clone(), p84 if (Box::new([0i128, (-7i128), (-127094543821962587050867749306411961709i128), (-170141183460469231731687303715884105727i128)]) <= Box::new([18790480612043044266812141401202084485i128, 85070591730234615865843651857942052863i128, (-18i128), 7i128])) => v83.clone(), _ => None::<G57>.unwrap_or_else(|| -> G57 { v83.clone() }) }).mix();
                    let mut v86: [i128; 4] = [[S1; 4].map(|x94: S1| -> Vec<char> { vec!['\u{7f}', 'Z'] }), [[Box::new(9223372036854775809u128), Box::new(1u128), Box::new(340282366920938463463374607431768211455u128)], [Box::new(181073816643360395980027758297684112045u128), Box::new(308331633154851925203838085522532052704u128), Box::new(13731597077191453086366943008932613106u128)], [Box::new(100802292787226530386490388677099175010u128), Box::new(188052303810428472674912447926986234404u128), Box::new(18u128)], [Box::new(7u128), Box::new(1u128), Box::new(286874361873049860604331377614659621425u128)]].map(|x96: [Box<u128>; 3]| -> Vec<char> { vec!['a', '\u{7f}', '\u{10FFFF}', '_', '\u{7f}'] }), [vec![' ', '😀'], vec!['a', 'a', '\u{d7ff}'], vec!['9', 'é'], vec!['ß', '9']], [vec!['_', 'é', '\u{10FFFF}'], vec!['\'', '\'', '\u{d7ff}'], Vec::<char>::new(), vec!['\0', '\\', '\u{80}', '\u{80}', '\u{7f}']]].map(|x90: [Vec<char>; 4]| -> i128 { 20i128 });
                    { let value = ((3i128 >> (63u32 % 128)) >> (pick::<u32>(false, &3007306689u32, &27u32).clone() % 128)); let at = (usize::from_str_radix("a,b,,c", 36).unwrap_or(37usize) % 4); unsafe { v86.as_mut_ptr().add(at).write(value) }; }
                    v86.mix();
                    {
                        #[derive(Clone, Copy, Debug, PartialEq)] enum Small { A, B, C }
                        #[derive(Clone, Copy, Debug, PartialEq)] enum Outer { One(Small), Two, Three(bool), Four(char) }
                        #[derive(Debug, PartialEq)] enum Deep { Ref(&'static u8), Boxed(Box<u16>), Nz(NonZeroU32), Nothing, Other(Option<Option<bool>>) }
                        #[derive(Clone, Copy, Debug)] #[repr(u8)] enum Tagged { X(u8) = 3, Y = 9, Z(bool, u16) = 200 }
                        enum Never {}
                        static BYTE: u8 = 7;
                        fn tell(it: &Outer) -> u64 { match *it { Outer::One(Small::A) => 1, Outer::One(Small::B) => 2, Outer::One(Small::C) => 3, Outer::Two => 4, Outer::Three(flag) => 5 + flag as u64, Outer::Four(c) => 7 + c as u64 } }
                        fn deep(it: &Deep) -> u64 { match it { Deep::Ref(r) => **r as u64, Deep::Boxed(b) => **b as u64, Deep::Nz(n) => n.get() as u64, Deep::Nothing => 0, Deep::Other(None) => 1, Deep::Other(Some(None)) => 2, Deep::Other(Some(Some(flag))) => 3 + *flag as u64 } }
                        { let order: [Option<Cmp>; 4] = black_box([None, Some(Cmp::Less), Some(Cmp::Equal), Some(Cmp::Greater)]); for it in order { feed(match it { None => 9, Some(Cmp::Less) => 1, Some(Cmp::Equal) => 2, Some(Cmp::Greater) => 3 }); } }
                        for it in black_box([Outer::One(Small::C), Outer::Two, Outer::Three(true), Outer::Four('x'), Outer::One(Small::A), Outer::Three(false)]) { feed(tell(&it)); feed(tell(&Some(it).unwrap_or(Outer::Two))); }
                        { let items: [Option<char>; 3] = black_box([None, Some('z'), Some('\u{10FFFF}')]); for it in items { feed(match it { None => 0, Some(c) => c as u64 }); } let nested: Option<Option<Option<u8>>> = black_box(Some(Some(None))); feed(match nested { None => 1, Some(None) => 2, Some(Some(None)) => 3, Some(Some(Some(v))) => v as u64 }); }
                        feed((size_of::<Deep>() + size_of::<Option<Deep>>() * 64 + size_of::<Option<Box<u8>>>() * 4096 + size_of::<Option<&u8>>() * 65536 + size_of::<Result<Box<u8>, ()>>() * 1048576) as u64);
                        { let ok: Result<u32, Never> = black_box(Ok(5)); let Ok(value) = ok; feed(value as u64); let none: Option<Never> = None; feed(none.is_none() as u64 + size_of::<Option<Never>>() as u64); }
                        for it in black_box([Deep::Ref(&BYTE), Deep::Boxed(Box::new(49)), Deep::Nz(NonZeroU32::new(39).unwrap()), Deep::Nothing, Deep::Other(None), Deep::Other(Some(None)), Deep::Other(Some(Some(true)))]) { feed(deep(&it)); let wrapped = Some(it); feed(wrapped.as_ref().map_or(99, deep)); }
                    }
                } else {
                    let mut v99: NonZeroU8 = K2;
                    let v100: [Vec<char>; 4] = [vec!['\u{d7ff}', '\\', '中', '\'', '\''], vec!['\u{10FFFF}', 'z', '😀', '0', '\n'], vec!['\\', 'a', 'é', '\n', '\u{10FFFF}'], Vec::<char>::new()];
                }
            }
            _ => {}
        }
        let mut v103: Box<Box<u16>> = (if ([18i8].into_iter().any(|x104: i8| -> bool { false })) { Box::new(Box::new(40800u16)) } else { Box::new(Box::new(3832u16)) });
        a59.clone()
    }
    #[inline]
    pub extern "C" fn f105(a106: i8, a107: u64, a108: i8) -> i8 {
        (-7i8)
    }
    pub fn run() {
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
            for it in black_box([Outer::One(Small::C), Outer::Two, Outer::Three(true), Outer::Four('k'), Outer::One(Small::A), Outer::Three(false)]) { feed(tell(&it)); feed(tell(&Some(it).unwrap_or(Outer::Two))); }
            { let items = black_box([Tagged::X(10), Tagged::Y, Tagged::Z(true, 53)]); for it in items { feed(match it { Tagged::X(v) => v as u64, Tagged::Y => 1000, Tagged::Z(flag, v) => v as u64 + flag as u64 }); feed(unsafe { *(&it as *const Tagged as *const u8) } as u64); } }
            { let pairs: [Option<(bool, u8)>; 3] = black_box([None, Some((true, 10)), Some((false, 0))]); for it in pairs { feed(match it { None => 7, Some((flag, v)) => v as u64 * 2 + flag as u64 }); } let refs: [Option<&str>; 2] = black_box([None, Some("")]); for it in refs { feed(it.map_or(5, str::len) as u64); } }
            for it in black_box([Deep::Ref(&BYTE), Deep::Boxed(Box::new(10)), Deep::Nz(NonZeroU32::new(53).unwrap()), Deep::Nothing, Deep::Other(None), Deep::Other(Some(None)), Deep::Other(Some(Some(true)))]) { feed(deep(&it)); let wrapped = Some(it); feed(wrapped.as_ref().map_or(99, deep)); }
            { let mut slot = black_box(Outer::Two); slot = if 10 % 2 == 0 { Outer::Three(true) } else { Outer::One(Small::B) }; feed(tell(&slot)); let mut opt = black_box(Some(slot)); if let Some(Outer::Three(flag)) = &mut opt { *flag = false; } feed(opt.map_or(0, |it| tell(&it))); opt = None; feed(opt.is_some() as u64); }
        }
        let [p111, p112]: [Box<f64>; 2] = [Box::new(7.0f64), Box::new(4294967296.0f64)];
        if (((false && true) || ([None::<u128>, Some(34u128), Some(2596148429267413814265248164610049u128)] >= [Some(308580785964937187803746842294234257869u128), Some(8u128), Some(10u128)]))) {
            {
                let mut v113: NonZeroU8 = (match (3i128.overflowing_shr(13u32).0) { -145579123486859959783651204563242585363i128.. => K3, 4i128 if (vec![Some('\0'), Some('😀'), Some('\u{d7ff}')] >= vec![Some('\n'), Some('\''), None::<char>]) => NonZeroU8::new(106u8).unwrap_or(K3), 18i128 if true => (if (false) { NonZeroU8::new(1u8).unwrap() } else { K3 }), _ => pick::<NonZeroU8>(false, &K2, &NonZeroU8::new(31u8).unwrap()).clone() });
                let v114: u64 = (match (f58::<[Box<u128>; 3], u128>([Box::new(28u128), Box::new(89316722062820527841169243815971419097u128), Box::new(1u128)], ((-952.6070f64) as u128), f58::<Box<u128>, i128>(Box::new(187760385278891731003010172739333163452u128), 170141183460469231731687303715884105726i128, Box::new(11801157305505555593849567410848357664i128)))) { [_, ..] => 8u64.wrapping_add(9934241966739483673u64).clamp(262145u64, 16600001639038004203u64), [p119, p120, p121] => u64::from_str_radix("x y".trim_end(), 10).unwrap_or(268435457u64.wrapping_mul(2598537980553623399u64)), [p123, p122 @ .., p124] => (32767u16 as u64), _ => ((33u64..40u64).all(|x125: u64| -> bool { false }) as u64) });
                {
                    (-18i8).mix();
                    {
                        enum Loud { I(i32, Noisy), C(char, Noisy) }
                        #[inline(never)] fn items(k: u32, last: char) -> [Loud; 3] { black_box([Loud::I(k as i32 % 4, Noisy(k)), Loud::C('q', Noisy(k + 1)), if k % 3 == 0 { Loud::I(1, Noisy(k + 2)) } else { Loud::C(last, Noisy(k + 2)) }]) }
                        #[inline(never)] fn pick0(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { [Loud::C('a'..='z', _), Loud::C('a', _) | Loud::I(2, _), y @ Loud::C(..)] => 1, [_, m @ .., Loud::C('a', _) | Loud::C(_, _)] => 2, [.., _] if black_box(!flag) => 3, [Loud::C(_, _) | Loud::I(..3, _), Loud::I(..), y] => 4, _ => 0 } }
                        for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick0(k, last, flag) as u64); feed(0xf00d); } } }
                        #[inline(never)] fn pair(k: u32, last: char, flag: bool) -> u8 { match (items(k, last), k % 3, flag) { ([Loud::I(..), ..], 3, true) => 1, ([.., _], _, _) => 2, ([x, m @ ..], 3, false) => 3, (all, _, _) if black_box(flag) => 4, _ => 0 } }
                        for k in 0..4u32 { for last in ['a', 'Z'] { for flag in [false, true] { feed(pair(k, last, flag) as u64); feed(0xf00d); } } }
                    }
                    5i128.overflowing_shl(4294967294u32).0.mix();
                }
                let mut o126: Box<dyn T4> = Box::new(Box::new(Box::new(1023u16)));
            }
        } else {
            (loop { break (if (true) { K2 } else { K2 }); }).mix();
            Box::new([100i128; 4]).mix();
        }
        if (pick::<Option<bool>>(true, &None::<bool>, &Some(true)).clone().map_or((false && false), |x127: bool| -> bool { (true && x127) })) {
            Box::new((*p111)).mix();
            let v129: u64 = (f32::NEG_INFINITY as u64);
            f58::<S1, [Box<u128>; 3]>(f58::<S1, Box<[u128; 1]>>(S1, Box::new([36u128]), f58::<Box<Box<[u128; 1]>>, Vec<[u128; 3]>>(Box::new(Box::new([35u128; 1])), vec![[12u128, 16u128, 340282366920938463463374607431768211455u128], [163884940034143201427516553415137844231u128; 3], [255852462849515668045396775836178807437u128, 170141183460469231731687303715884105727u128, 4u128]], Box::new(Vec::<[u128; 3]>::new()))), [(match (&{ [Box::new(340282366920938463463374607431768211455u128), Box::new(2u128), Box::new(1u128)] }) { [p132, .., p133] => (*p133).clone(), [_, p134 @ .., _] if false => (*p134)[0].clone(), p135 if true => Box::new(136101413938040027363210542187941371338u128), _ => Box::new(36u128) }), f58::<Box<u128>, S1>(Box::new(549755813889u128), S1, Box::new(S1)), (match (&{ v129 }) { 1152921504606846977u64 => Box::new(85319788714576658547855777825119608202u128), _ => Box::new(9u128), _ => Box::new(18u128), _ => Box::new(1u128) })], (match ((match ([['9'; 1]; 1]) { [[.., _], ..] => vec![NonZeroU8::new(100u8).unwrap(), NonZeroU8::new(1u8).unwrap()], _ => vec![NonZeroU8::new(1u8).unwrap(), NonZeroU8::new(8u8).unwrap(), NonZeroU8::new(253u8).unwrap(), NonZeroU8::new(1u8).unwrap(), NonZeroU8::new(2u8).unwrap()] }).as_slice()) { [_, ..] => Box::new([Box::new(32u128), Box::new(100u128), Box::new(170141183460469231731687303715884105727u128)]), _ => ({ let t138: ([i128; 1],) = ([5192296858534827628530496329220096i128],); Box::new([Box::new(38u128), Box::new(268435456u128), Box::new(3u128)]) }) })).mix();
        }
        if (larger::<bool>((Box::new([0u128; 3]) < Box::new([2596148429267413814265248164610047u128, 12u128, 28u128])), (match (vec![([(-15i128); 1],)].as_slice()) { [([_],)] => true, [([.., p141],), ([p142, ..],)] if false => false, _ => true }))) {
            {
                let mut v143: Box<f64> = Box::new((-2.5e-3f64));
                if (true) {
                    let mut v144: u64 = (31u64 >> (4294967295u32 % 64));
                    {
                        let (a, b): (u128, u128) = (black_box((3) as u128), black_box(((u128::MAX / 3)) as u128));
                        { let (sum, carry) = a.overflowing_add(b); let (product, over) = a.overflowing_mul(b); sum.mix(); product.mix(); feed(carry as u64 + 2 * over as u64); }
                        (a & b).mix(); (a | b).mix(); (a ^ b).mix(); feed((a < b) as u64 + 2 * (a == b) as u64); a.max(b).mix(); a.abs_diff(b).mix(); a.cmp(&b).mix();
                        a.wrapping_add(b).mix(); a.wrapping_sub(b).mix(); a.wrapping_mul(b).mix(); a.checked_add(b).mix(); a.checked_mul(b).mix(); a.saturating_sub(b).mix();
                        a.checked_pow(3).mix(); a.wrapping_pow(b as u32 % 7).mix(); a.checked_neg().mix(); a.checked_next_multiple_of(b | 1).mix();
                        (a as u64).mix(); (a as i64).mix(); (a as u8).mix(); ((a >> 64) as u64).mix(); u64::try_from(a).ok().mix(); i32::try_from(b).ok().mix();
                        a.isqrt().mix(); a.checked_ilog2().mix(); a.checked_ilog10().mix();
                    }
                } else {
                    let v145: [Option<u128>; 3] = pick::<[Option<u128>; 3]>(true, &[f58::<Option<u128>, Box<[u128; 3]>>(None::<u128>, Box::new([241275595233701631665964736664362760870u128; 3]), Box::new(Box::new([61480499119620199758281380294270262252u128, 3u128, 10u128]))), Some(340282366920938463463374607431768211455u128), Ok::<u128, Vec<[u128; 3]>>(1u128).ok()], &[Ok::<u128, Box<[u128; 3]>>(8u128).ok(), twice::<Option<u128>>(&Some(37778931862957161709568u128)).1, Some(39u128).filter(|x156: &u128| -> bool { false })]).clone();
                    let v157: [Vec<char>; 4] = twice::<[Vec<char>; 4]>(&[vec!['\\', 'z'], vec!['\'', '\u{d7ff}'], Vec::<char>::new(), vec!['a', '\n', '\u{10FFFF}', '\u{80}', '\u{d7ff}']]).1;
                    let f159 = move || -> Box<[u128; 8]> { let v158 = v157; let out = f58::<Box<[u128; 8]>, [Vec<char>; 4]>(Box::new([7u128, 191320422065717124530029450769589453927u128, 1u128, 36u128, 31u128, 33u128, 26u128, 97984293104079566886058558951555104907u128]), [vec!['\\', '\u{80}', '\u{7f}', '0', '\n'], vec!['中'], Vec::<char>::new(), vec!['a', '\\', '\u{e000}', '\\', 'ß']], f58::<Box<[Vec<char>; 4]>, i8>(Box::new([vec!['_', '\u{80}', 'a', ' ', '0'], vec!['\u{10FFFF}', '\'', '中', '_', 'z'], Vec::<char>::new(), vec!['ß', 'ß', '_', 'a', 'Z']]), 7i8, Box::new(18i8))); drop(v158); out };
                    apply_boxed(Box::new(f159)).mix();
                }
            }
            {
                async fn leaf(x: u64) -> u64 { let _g = Noisy(x as u32 % 9); later((x % 3) as u32, x).await }
                for armed in [false, true] { caught(|| { let got = block_on(async { let _n = Noisy(14); let a = leaf(0).await; trip(armed, ()); let _m = Noisy(2); a + leaf(4).await }); feed(got); }); }
                caught(|| { let got = poll_some(async { let _b = Bomb(14); leaf(2).await }, 0); feed(got.is_some() as u64); });
            }
        } else {
            let mut v164: Vec<[i128; 8]> = twice::<Vec<[i128; 8]>>(&vec![[170141183460469231731687303715884105726i128, 8i128, 3i128, (-170141183460469231731687303715884105728i128), 20i128, 3i128, 20282409603651670423947251286016i128, 170141183460469231731687303715884105727i128]]).1;
        }
        if (false) {
            let mut v169: NonZeroU8 = K2;
            {
                let mut n170 = 0u32;
                'l171: loop {
                    n170 += 1;
                    if n170 > 1 { break; }
                    {
                        async fn work(seed: u32) -> u64 { let h173 = Noisy(seed % 36 + 1); let w174 = later(2, [seed as u64; 9]).await[0]; let h175 = Noisy(seed % 1 + 1); let w176 = later(3, seed as u64 + 58).await; let h177: u128 = (seed as u128) << 94; let w178 = match later(1, seed % 3).await { 0 => later(0, 10u64).await, 1 => 11, _ => later(2, 12u64).await }; let h179 = (seed as f64 * 0.5, seed as u8, seed % 3 == 0); let mut w180 = 0u64; for i in 0..2u64 { let _g = Noisy(i as u32); w180 += later((i % 2) as u32, i).await; } (h173.0 as u64).rotate_left(0) ^ (w174).rotate_left(1) ^ (h175.0 as u64).rotate_left(2) ^ (w176).rotate_left(3) ^ ((h177 >> 3) as u64).rotate_left(4) ^ (w178).rotate_left(5) ^ (h179.0 as u64 + h179.1 as u64 + h179.2 as u64).rotate_left(6) ^ (w180).rotate_left(7) }
                        poll_some(work(28), 1).mix();
                        poll_some(work(69), 5).mix();
                    }
                    {
                        #[derive(Clone, Copy, Debug, PartialEq)] enum Small { A, B, C }
                        #[derive(Clone, Copy, Debug, PartialEq)] enum Outer { One(Small), Two, Three(bool), Four(char) }
                        #[derive(Debug, PartialEq)] enum Deep { Ref(&'static u8), Boxed(Box<u16>), Nz(NonZeroU32), Nothing, Other(Option<Option<bool>>) }
                        #[derive(Clone, Copy, Debug)] #[repr(u8)] enum Tagged { X(u8) = 3, Y = 9, Z(bool, u16) = 200 }
                        enum Never {}
                        static BYTE: u8 = 7;
                        fn tell(it: &Outer) -> u64 { match *it { Outer::One(Small::A) => 1, Outer::One(Small::B) => 2, Outer::One(Small::C) => 3, Outer::Two => 4, Outer::Three(flag) => 5 + flag as u64, Outer::Four(c) => 7 + c as u64 } }
                        fn deep(it: &Deep) -> u64 { match it { Deep::Ref(r) => **r as u64, Deep::Boxed(b) => **b as u64, Deep::Nz(n) => n.get() as u64, Deep::Nothing => 0, Deep::Other(None) => 1, Deep::Other(Some(None)) => 2, Deep::Other(Some(Some(flag))) => 3 + *flag as u64 } }
                        { let order: [Option<Cmp>; 4] = black_box([None, Some(Cmp::Less), Some(Cmp::Equal), Some(Cmp::Greater)]); for it in order { feed(match it { None => 9, Some(Cmp::Less) => 1, Some(Cmp::Equal) => 2, Some(Cmp::Greater) => 3 }); } }
                        feed((size_of::<Option<Small>>() + size_of::<Outer>() * 16 + size_of::<Option<Outer>>() * 256 + size_of::<Option<Option<Outer>>>() * 4096) as u64);
                        { let items: [Option<char>; 3] = black_box([None, Some('z'), Some('\u{10FFFF}')]); for it in items { feed(match it { None => 0, Some(c) => c as u64 }); } let nested: Option<Option<Option<u8>>> = black_box(Some(Some(None))); feed(match nested { None => 1, Some(None) => 2, Some(Some(None)) => 3, Some(Some(Some(v))) => v as u64 }); }
                        { let mut slot = black_box(Outer::Two); slot = if 42 % 2 == 0 { Outer::Three(true) } else { Outer::One(Small::B) }; feed(tell(&slot)); let mut opt = black_box(Some(slot)); if let Some(Outer::Three(flag)) = &mut opt { *flag = false; } feed(opt.map_or(0, |it| tell(&it))); opt = None; feed(opt.is_some() as u64); }
                        feed((size_of::<Deep>() + size_of::<Option<Deep>>() * 64 + size_of::<Option<Box<u8>>>() * 4096 + size_of::<Option<&u8>>() * 65536 + size_of::<Result<Box<u8>, ()>>() * 1048576) as u64);
                        for it in black_box([Deep::Ref(&BYTE), Deep::Boxed(Box::new(42)), Deep::Nz(NonZeroU32::new(29).unwrap()), Deep::Nothing, Deep::Other(None), Deep::Other(Some(None)), Deep::Other(Some(Some(true)))]) { feed(deep(&it)); let wrapped = Some(it); feed(wrapped.as_ref().map_or(99, deep)); }
                    }
                }
                {
                    async fn work(seed: u32) -> u64 { let h181: Option<Box<u16>> = if seed % 2 == 0 { Some(Box::new(36)) } else { None }; let mut w182 = 0u64; for i in 0..3u64 { let _g = Noisy(i as u32); w182 += later((i % 2) as u32, i).await; } let h183 = [seed as u8; 17]; let mut w184 = 0u64; for i in 0..3u64 { let _g = Noisy(i as u32); w184 += later((i % 2) as u32, i).await; } let h185: Option<Box<u16>> = if seed % 2 == 0 { Some(Box::new(52)) } else { None }; let w186 = if seed % 2 == 0 { later(1, 5u64).await } else { 29 }; (h181.as_deref().copied().unwrap_or(1) as u64).rotate_left(0) ^ (w182).rotate_left(1) ^ (h183[0] as u64).rotate_left(2) ^ (w184).rotate_left(3) ^ (h185.as_deref().copied().unwrap_or(1) as u64).rotate_left(4) ^ (w186).rotate_left(5) }
                    block_on(work(86)).mix();
                    block_on(work(98)).mix();
                    block_on(work(66)).mix();
                }
                let mut v187: [char; 3] = [Box::new([11u128]), Box::new([8u128]), Box::new([63u128; 1])].map(|x190: Box<[u128; 1]>| -> char { '\u{80}' });
                { let at = (129usize.wrapping_div_euclid(5usize | 1) % 3); let value = (match ((match (vec![vec![vec!['\u{d7ff}', ' ', ' ', '\'', '\u{e000}'], vec!['a', '\u{10FFFF}'], vec!['中', '\u{7f}', '\0', '\u{e000}', '\u{10FFFF}']], vec![vec![' ', '\\']], Vec::<Vec<char>>::new(), Vec::<Vec<char>>::new(), vec![vec!['\u{e000}', ' ', ' ']]].as_slice()) { [_] => Box::new(Some('中')), [_, ..] => Box::new(Some('\0')), _ => Box::new(None::<char>) })) { p193 => pick::<char>(true, &'\n', &'9').clone(), _ => (match (&{ 10141204801825835211973625643008u128 }) { ..340282366920938463463374607431768211455u128 => '\'', (8u128 | 1u128 | 10u128..35184372088831u128) if false => 'Z', _ => '\n' }) }); unsafe { raw_set(v187.as_mut_ptr(), at, value); raw_get(v187.as_ptr(), at).mix(); } }
                v187.mix();
            }
        }
        let mut o194: Box<dyn T4> = Box::new((f58::<f32, [Box<u128>; 3]>(0.0f32, [Box::new(1u128), Box::new(23u128), Box::new(6u128)], Box::new([Box::new(8u128), Box::new(170141183460469231731687303715884105727u128), Box::new(7u128)])) as u128));
        p111.clone().mix();
        if ((false && false)) {
            'l200: for x199 in ([vec![[10u128, 85070591730234615865843651857942052864u128, 1u128]], vec![[30224258302000066359921906323408650122u128, 29u128, 59165426104318702275864701876968506916u128], [39614081257132168796771975168u128, 7u128, 316884961751050557950899706446160761781u128]], vec![[72927663287046453765225977145069616537u128; 3], [2u128; 3], [10u128; 3]], vec![[1099511627776u128, 141982562388964418771270761492825729436u128, 8u128], [2u128, 2305843009213693952u128, 15u128], [7u128, 3u128, 67108864u128]]].map(|x196: Vec<[u128; 3]>| -> u64 { 6439242900012837776u64 }).into_iter().chain(pick::<Vec<u64>>(true, &vec![0u64, 14539866411839654618u64], &vec![10910562957261823498u64]).clone()).collect::<Vec<_>>()) {
                (match ((match (S1) { S1 => None::<char>, S1 if false => Some('a'), S1 if true => Some('\u{7f}'), _ => None::<char> })) { None => f58::<char, [Vec<char>; 4]>('z', [vec!['é', '9', 'ß', '\\', '\''], vec!['é', '\'', '_'], vec!['\\', 'z', '\''], vec!['\u{7f}', ' ', '9']], Box::new([vec!['\u{e000}', '0', '\n', 'é', '😀'], vec!['0', 'z'], vec!['\n'], Vec::<char>::new()])), Some(x201) => x201 }).mix();
                let v203: [Vec<f32>; 5] = [[946.8765f32].to_vec(), (match (&{ [Vec::<char>::new(), vec!['\0'], vec!['é', 'z', ' '], vec!['_']] }) { [p208, _, _, p209] => vec![(-2.5e-3f32)], [_, .., _] => vec![(-605.8614f32), 255.5f32, 0.5f32, (-2.5e-3f32), 3.25f32], [p210, p211, p212, p213] => vec![f32::EPSILON, 0.5f32, f32::NAN, f32::NEG_INFINITY, 4294967296.0f32], _ => vec![(-1.0f32), 0.1f32, 255.5f32] }), vec![f32::NAN; 1], larger::<Vec<f32>>(vec![(-1.0f32), 1e10f32, (-128.9f32)], vec![f32::NAN, f32::NAN, 7.0f32]), vec![f32::MAX, 1e30f32, 16777217.0f32, f32::MIN_POSITIVE, 566.4595f32]];
                {
                    { let found = 'search: loop { for i in 0..4u32 { let hit = move |at: u32| at * i == 4; for j in 0..3 { if hit(j) { break 'search Some((i, j)); } } } break None; }; found.mix(); }
                    { let mut count = 0u32; let mut step = |by: u32| { count += by; count }; for i in 0..4 { step(i).mix(); } count.mix(); }
                }
            }
        } else {
            [Vec::<f32>::new(), vec![255.5f32, (-340.8487f32), (-9223372036854775808.0f32), 0.2f32, 16777217.0f32], Vec::<f32>::new(), Vec::<f32>::new(), Vec::<f32>::new()].mix();
            {
                let mut v217: Vec<S1> = f58::<Vec<S1>, Vec<Option<char>>>([Vec::<Box<u64>>::new(), vec![Box::new(16742441527678708841u64), Box::new(3824868261517766634u64), Box::new(9223372036854775807u64), Box::new(27u64), Box::new(9223372036854775807u64)], vec![Box::new(7u64)]].into_iter().rev().map(|x221: Vec<Box<u64>>| -> S1 { S1 }).collect::<Vec<S1>>(), ({ let mut items = vec![Some(' '), Some('Z'), Some('ß')]; items.insert(0, Some('_')); items }), f58::<Box<Vec<Option<char>>>, [[u128; 2]; 3]>(Box::new(vec![None::<char>, None::<char>]), [[14u128; 2], [10u128, 328039713378842869076835346590663360075u128], [10141204801825835211973625643008u128, 274877906944u128]], Box::new([[7u128, 10u128], [170141183460469231731687303715884105727u128, 1u128], [10u128; 2]])));
                drop(v217);
                f58::<NonZeroU8, u64>(pick::<NonZeroU8>(true, &NonZeroU8::new(100u8).unwrap(), &K2).clone(), (match ([Some(2u128), None::<u128>, Some(14u128)]) { [None, p224 @ .., Some(40u128..186216609906003102314762199759863276415u128)] => 9223372036854775807u64, [Some(p225), ..] => 10707433218097875047u64, _ => 36u64 }), f58::<Box<u64>, [[u128; 2]; 3]>(Box::new(1u64), [[3u128; 2], [137438953473u128; 2], [2u128; 2]], Box::new([[9u128; 2], [8388609u128; 2], [100u128; 2]]))).mix();
                let mut v226: [([i128; 1],); 2] = [Some(([(-85002137606709354547406671624261767283i128)],)).unwrap_or(pick::<([i128; 1],)>(false, &([125945343063214422439565613921774630447i128; 1],), &([77371252455336267181195264i128],)).clone()), ([8388608i128],)];
                { let words: [u16; 3] = [13u16.isqrt(), 6468u16.clamp(20u16, 2048u16), f58::<u16, [Box<u128>; 3]>(6475u16, [Box::new(100948797431351010666265391019262508507u128), Box::new(1u128), Box::new(309485009821345068724781055u128)], Box::new([Box::new(4u128), Box::new(173474300217886446690403338145466699533u128), Box::new(8796093022208u128)]))]; (unsafe { std::slice::from_raw_parts(words.as_ptr() as *const u8, 6) }).mix(); }
                v226.mix();
            }
        }
        {
            let mut v235: Box<[i128; 4]> = Some(Box::new([513i128, 5070602400912917605986812821505i128, 118782435867388962288023381383570355114i128, 10i128])).ok_or(Box::new(vec![3711075408u32, 15u32, 25u32])).unwrap_or_else(|x236: Box<Vec<u32>>| -> Box<[i128; 4]> { twice::<Box<[i128; 4]>>(&Box::new([(-19i128), 1025i128, 9671406556917033397649408i128, 3i128])).1 });
            let v238: Box<[i128; 4]> = (match (Some(v235.clone())) { None => Box::new([(-157912523038288625058213727126429607081i128); 4]), Some(x239) => x239 });
            let mut v241: [([i128; 1],); 3] = [vec![vec![(-11i16)], vec![20005i16], vec![32766i16]], vec![vec![0i16, 8i16, 5i16]], vec![vec![(-17i16), 7i16, 9329i16], vec![32767i16, (-15813i16)], Vec::<i16>::new(), vec![31i16, 14622i16], vec![(-32768i16), (-12i16)]]].map(|x244: Vec<Vec<i16>>| -> ([i128; 1],) { ([(-15i128); 1],) });
            (unsafe { *v241.as_ptr().add((larger::<usize>(5usize, 7usize) % 3)) }).mix();
            v241.mix();
        }
        match (&{ [vec!['é', '_'], Vec::<char>::new(), vec!['\0', '\u{e000}', '\u{10FFFF}'], vec!['\u{80}', '\0'], vec!['ß', '😀']] }) {
            [p247, ..] => {
                9i8.mix();
            }
            [p249, p248 @ .., _] => {
                let mut v250: Vec<char> = (*p248)[0].clone();
            }
            _ => {}
        }
        let mut v252: Vec<S1> = (unsafe { let mut slot = MaybeUninit::<Vec<S1>>::uninit(); slot.write(vec![S1, S1, S1, S1]); slot.assume_init() });
        'l257: for x256 in ((14i8..19i8).rev().collect::<Vec<_>>()) {
            let v258: [Vec<f32>; 5] = [Vec::<Box<u64>>::new(), vec![Box::new(18446744073709551615u64), Box::new(1u64)], ({ let mut items = vec![Box::new(9007199254740991u64), Box::new(9223372036854775807u64), Box::new(25u64)]; items.dedup(); items }), f58::<Vec<Box<u64>>, [Option<u128>; 3]>(vec![Box::new(12u64), Box::new(35u64), Box::new(30u64), Box::new(0u64), Box::new(10u64)], [None::<u128>, Some(130525666366082550068253169617181467407u128), Some(15666505350531566613021195419989707016u128)], Box::new([Some(10u128); 3])), f58::<Vec<Box<u64>>, [Box<u128>; 3]>(Vec::<Box<u64>>::new(), [Box::new(35184372088831u128), Box::new(562949953421312u128), Box::new(282330958170073676029793657178123403145u128)], Box::new([Box::new(1u128), Box::new(100u128), Box::new(39u128)]))].map(|x260: Vec<Box<u64>>| -> Vec<f32> { pick::<Vec<f32>>((true || true), &({ let mut items = vec![1.5f32, 16777217.0f32, (-1.0f32), (-398.1931f32), 0.0f32]; items.dedup(); items }), &({ let mut items = Vec::<f32>::new(); items.reverse(); items })).clone() });
            let mut v270: [Vec<f32>; 5] = v258.clone();
            let v273: Box<[u128; 3]> = Box::new([7u128, 340282366920938463463374607431768211454u128, 8193u128]);
            (if (f58::<bool, [Vec<char>; 5]>(true, [[Box::new('a'), Box::new('😀'), Box::new('中'), Box::new('\0')], [Box::new('\\'), Box::new('é'), Box::new(' '), Box::new('é')], [Box::new('\n'), Box::new('\n'), Box::new('\u{10FFFF}'), Box::new('\u{7f}')], [Box::new('\u{80}'), Box::new('\u{80}'), Box::new('\u{80}'), Box::new('\u{10FFFF}')], [Box::new('😀'), Box::new('\u{d7ff}'), Box::new('ß'), Box::new('Z')]].map(|x275: [Box<char>; 4]| -> Vec<char> { vec!['z', 'ß', 'ß'] }), (match (&{ [Box::new('😀'), Box::new('0'), Box::new('\u{e000}'), Box::new('9'), Box::new('\0')] }) { [p277, p276 @ .., _] => Box::new([vec!['\\', '\n'], vec!['_', '\0'], vec!['\u{7f}', 'z', '\u{7f}', '\u{10FFFF}', 'z'], vec!['é', 'é'], vec!['\u{d7ff}', 'z', 'é']]), [p279, p278 @ .., _] => Box::new([vec!['\u{10FFFF}', '0', '\u{10FFFF}'], vec!['_', '😀'], vec![' ', '0'], vec!['\u{10FFFF}', '\u{e000}'], vec!['0']]), _ => Box::new([vec!['é', 'Z'], vec!['ß'], Vec::<char>::new(), vec!['\u{10FFFF}', '\u{d7ff}', ' ', '\\', 'a'], Vec::<char>::new()]) }))) { v273.clone() } else { Box::new([170141183460469231731687303715884105727u128; 3]) }).mix();
        }
        if (true) {
            let mut n280 = 0u32;
            let v282: ([i128; 2],) = 'l281: loop {
                n280 += 1;
                if n280 > 3 { break ([145130606591207411097270798211094710435i128; 2],); }
                let mut v283: NonZeroU8 = K3;
                let v284: Vec<[i128; 8]> = (match (vec![Box::new([16777217u128])].as_slice()) { [.., _] => vec![[124550422861466843344339711227620016023i128, 112470155505734277672786591726582498313i128, 170141183460469231731687303715884105727i128, 170141183460469231731687303715884105726i128, 170141183460469231731687303715884105727i128, 13i128, 7i128, (-1i128)]], [_, ..] => vec![[17592186044416i128; 8], [154524767267644589257228089512082676738i128, (-170141183460469231731687303715884105728i128), 295147905179352825857i128, 8i128, 10i128, 5316911983139663491615228241121378303i128, 0i128, (-7i128)], [1208925819614629174706176i128, 85070591730234615865843651857942052863i128, 17179869184i128, 0i128, 3i128, 85070591730234615865843651857942052863i128, 6i128, (-170141183460469231731687303715884105727i128)], [100i128; 8], [(-16776932078627959487435009641957236251i128); 8]], _ => vec![[(-19i128); 8], [(-162994492443309333886273896702771552578i128), (-1i128), 17i128, (-3i128), 110068372377148467413367873054435945796i128, 1i128, 85070591730234615865843651857942052863i128, 2305843009213693951i128], [38685626227668133590597632i128, 0i128, (-70993529988813908066863074256040764715i128), (-2i128), 16777216i128, (-17i128), (-38759841173194659451186835995993378665i128), 18446744073709551615i128]] });
                [vec!['\u{10FFFF}'], vec!['9', '\0', 'z'], vec!['😀', '_', '9'], vec!['_']].mix();
                if (pick::<bool>(f58::<bool, Vec<[u128; 3]>>(true, Vec::<[u128; 3]>::new(), Box::new(Vec::<[u128; 3]>::new())), &[[['😀'; 1]]; 4].into_iter().map(|x288: [[char; 1]; 1]| -> [Vec<f32>; 5] { [vec![0.5f32, 4294967296.0f32, 3.25f32], vec![3.25f32], vec![1.0f32, f32::EPSILON, 4294967296.0f32], vec![1e30f32], vec![f32::MIN_POSITIVE, f32::NEG_INFINITY]] }).any(|x289: [Vec<f32>; 5]| -> bool { false }), &f58::<bool, Box<[i128; 4]>>(true, Box::new([162259276829213363391578010288128i128; 4]), Box::new(Box::new([63344412825377464602390288724045494913i128, 18889465931478580854785i128, 633825300114114700748351602688i128, (-1i128)])))).clone()) {
                    K3.mix();
                    if (({ let mut items = [[Some(20u128), Some(1u128), None::<u128>]; 2]; items.reverse(); items }).into_iter().rev().all(|x293: [Option<u128>; 3]| -> bool { vec![[vec![7.0f32, 3.25f32, (-488.1841f32)], vec![255.5f32, 949.4408f32, f32::MIN_POSITIVE, 0.5f32, f32::NAN], Vec::<f32>::new(), vec![3.25f32, f32::NEG_INFINITY, 65535.0f32], vec![7.0f32, 1e10f32, 255.5f32, (-0.0f32), (-0.0f32)]], [vec![f32::MIN_POSITIVE, (-1.0f32), (-128.9f32), 149.6231f32, (-0.0f32)], vec![f32::NEG_INFINITY, (-2.5e-3f32), 1e10f32], vec![(-2.5e-3f32), 1e10f32, 1.5f32], vec![0.2f32, (-1.0f32)], vec![944.4450f32, f32::INFINITY, (-118.0934f32), 16777217.0f32, 1.0f32]], [vec![4294967296.0f32, 1.0f32, (-2.5e-3f32), 743.5603f32, (-2.5e-3f32)], vec![(-128.9f32), f32::INFINITY, f32::MIN_POSITIVE, 0.5f32, 0.5f32], vec![0.5f32], Vec::<f32>::new(), vec![f32::MAX, f32::NEG_INFINITY, 0.1f32]]].contains(&[vec![388.7610f32], vec![(-9223372036854775808.0f32), 0.1f32], Vec::<f32>::new(), Vec::<f32>::new(), vec![f32::EPSILON, 1e30f32, 255.5f32, 4294967296.0f32, 65535.0f32]]) })) { break 'l281 ([(-141427653942171059278949346193305440698i128), 2097151i128],); }
                }
            };
            let mut n294 = 0u32;
            'l295: loop {
                n294 += 1;
                if n294 > 2 { break; }
                let mut v297: NonZeroU8 = (unsafe { *(&K3 as *const NonZeroU8) });
                let mut n298 = 0u32;
                let v300: [[u128; 2]; 3] = 'l299: loop {
                    n298 += 1;
                    if n298 > 3 { break twice::<[[u128; 2]; 3]>(&[[248886501860325954180491004407135225240u128, 18u128]; 3]).0; }
                    let [_] = (&{ <[Box<f32>; 1]>::try_from(vec![Box::new((-1.0f32)), Box::new((-2.5e-3f32)), Box::new(255.5f32), Box::new(0.5f32), Box::new(255.5f32)]).unwrap_or([Box::new(0.5f32)]) }) else { break 'l299 twice::<[[u128; 2]; 3]>(&[[35184372088832u128, 15u128]; 3]).1; };
                    if ((if (([Box::new(170141183460469231731687303715884105727u128), Box::new(10u128), Box::new(0u128), Box::new(307861535083300558839144405593247093543u128), Box::new(1048575u128), Box::new(9444732965739290427391u128), Box::new(275096114323689885199609170817263226678u128), Box::new(5192296858534827628530496329220097u128)] >= [Box::new(170141183460469231731687303715884105727u128), Box::new(1u128), Box::new(302787219421699579047070586605471528576u128), Box::new(39u128), Box::new(6u128), Box::new(204911377931114011471325323651048818933u128), Box::new(40564819207303340847894502572032u128), Box::new(43752647061623660247170548969745486360u128)])) { ([[NonZeroU8::new(1u8).unwrap(); 3]; 2] <= [[NonZeroU8::new(3u8).unwrap(); 3]; 2]) } else { (!false) })) { continue 'l295; }
                };
            }
            let mut n307 = 0u32;
            let v309: u64 = 'l308: loop {
                n307 += 1;
                if n307 > 8 { break 3375730535756871436u64.clamp(26u64, 29u64); }
                {
                    for armed in [false, true] { let acts: Vec<Box<dyn Fn() -> u32>> = vec![Box::new(|| 24), Box::new(move || trip(armed, 7)), Box::new(|| 3)]; caught(|| { let _g = Noisy(24); for act in &acts { feed(act() as u64); } }); }
                    for armed in [false, true] { let mut seen = Vec::new(); caught(|| { let mut note = |it: u32| { seen.push(Noisy(it)); trip(armed && it == 2, ()); }; note(1); note(2); note(3); }); feed(seen.len() as u64); }
                }
                {
                    async fn leaf(x: u64) -> u64 { let _g = Noisy(x as u32 % 9); later((x % 3) as u32, x).await }
                    let futures: Vec<Pin<Box<dyn Future<Output = u64>>>> = vec![Box::pin(leaf(9)), Box::pin(async { 11 }), Box::pin(later(2, 9u64)), Box::pin(async { leaf(1).await + leaf(2).await })];
                    for (i, future) in futures.into_iter().enumerate() { poll_some(future, i as u32 % 2).mix(); }
                }
            };
        } else {
            let mut v310: Box<[i128; 4]> = (match ([['\'', '\u{7f}', 'a', 'z', '中', '\u{e000}', '\u{7f}', '\'']; 1]) { [[' ', p311 @ .., '9']] => Box::new([34359738369i128, (-94750392029786323653017937235328926046i128), 71283466562627873800360991791028543835i128, (-10i128)]), p312 => Box::new([(-9i128), (-61282340866929454873948509472425524789i128), 85070591730234615865843651857942052863i128, 103495614778620734690589611758598146934i128]), [['\u{80}', ..]] => Box::new([18i128, 7i128, 10i128, (-1i128)]), _ => Box::new([(-8i128), (-15i128), (-170141183460469231731687303715884105728i128), 170141183460469231731687303715884105727i128]) });
            let mut v313: [char; 2] = [Box::new(Box::new(65535u16)), Box::new(Box::new(19u16))].map(|x316: Box<Box<u16>>| -> char { '中' });
            { let bytes: [u8; 17] = [(f64::MAX as u8).wrapping_div((match (vec![[Box::new('_'), Box::new('Z'), Box::new('\u{7f}'), Box::new('_')], [Box::new('z'), Box::new('\''), Box::new('中'), Box::new('é')], [Box::new('é'), Box::new('\u{10FFFF}'), Box::new('\u{80}'), Box::new('z')]].as_slice()) { [[_, ..]] if true => 33u8, [[_, .., _], ..] => 4u8, [[_, p322, _, p323], [p324, p325, _, _]] if true => 7u8, _ => 30u8 }) | 1), (255.5f32 as u8).checked_shr(({ let t326: Box<Option<char>> = Box::new(None::<char>); 1u32 })).unwrap_or(String::from("héllo").as_bytes().first().copied().unwrap_or(1u8)), (!String::from("a").as_bytes().first().copied().unwrap_or(254u8)), (15u8.wrapping_mul(9u8).leading_zeros() as u8), ((-1e10f32) as u8), (if (String::from("42").chars().flat_map(|x327: char| -> Vec<[Box<char>; 4]> { vec![[Box::new('9'), Box::new(' '), Box::new('é'), Box::new('\0')]] }).any(|x328: [Box<char>; 4]| -> bool { true })) { u8::from_str_radix("  pad  ", 2).unwrap_or(25u8) } else { 0u8.wrapping_add(15u8) }), 8u8.clamp(0u8, 10u8).clamp(7u8, 34u8), f58::<String, [Vec<char>; 5]>(String::from("Abc Def"), [Vec::<char>::new(), Vec::<char>::new(), vec!['\'', '\u{80}'], vec!['9', '\'', '中', '_', '\u{7f}'], vec!['a']], Box::new([vec!['ß', '\u{e000}', '\u{10FFFF}'], vec!['中', 'ß', '0', 'Z', 'ß'], vec!['\u{e000}', '\u{d7ff}', 'a'], vec!['\\', '\u{10FFFF}', '\u{7f}', '\'', ' '], Vec::<char>::new()])).as_bytes().first().copied().unwrap_or((2u8.abs_diff(3u8) as u8)), (match (Vec::<Vec<Box<u64>>>::new().as_slice()) { [_] if (match ([Some(68719476736u128); 3]) { [.., p330] => false, [Some(p331), .., Some((..=340282366920938463463374607431768211454u128 | 0u128..=10u128 | 79228162514264337593543950337u128..85070591730234615865843651857942052863u128))] => true, _ => true }) => (unsafe { *(&127u8 as *const u8) }), _ => String::from("42").as_bytes().first().copied().unwrap_or(0u8) }), pick::<u8>(false, &28u8.checked_shl(14u32).unwrap_or(18u8), &(match (vec![[Box::new(1e30f64), Box::new(f64::EPSILON)]].as_slice()) { [[_, p333], ..] => 0u8, _ => 255u8 })).clone(), (({ let t334: Box<[u128; 8]> = Box::new([117177186329333913371048014715602046370u128, 24u128, 28u128, 12u128, 32u128, 2u128, 3u128, 0u128]); 64u8 }) >> (31u32 % 8)), f58::<u8, [Option<u128>; 3]>(3u8, [None::<u128>, None::<u128>, Some(9u128)], Box::new([Some(2475880078570760549798248448u128), None::<u128>, Some(134856587158942976022782019550652346040u128)])).isqrt(), ('\u{d7ff}' as u8), (3u8 ^ (match (([10i128; 2],)) { ([147573952589676412929i128, p335],) => 254u8, _ => 17u8 })), (!(false as u8)), (17592186044415usize as u8), (!171u8.wrapping_add(7u8))]; (unsafe { (bytes.as_ptr().add(6) as *const i32).read_unaligned() }).mix(); }
            v313.mix();
        }
        Box::new(vec![36u32]).mix();
        let mut v336: Box<Box<u16>> = f58::<Box<Box<u16>>, Vec<[char; 2]>>((if (([Some(29u128); 3] == [Some(39u128), None::<u128>, None::<u128>])) { pick::<Box<Box<u16>>>(true, &Box::new(Box::new(5135u16)), &Box::new(Box::new(3u16))).clone() } else { f45::<u128, [[char; 2]; 2]>(1u128, [['😀', '\u{80}'], ['\u{80}', 'a']]) }), vec![['\n'; 2], [' ', '\\'], ['\0', 'ß'], ['\u{80}'; 2], ['é', '\\']], pick::<Box<Vec<[char; 2]>>>((true || false), &(if (true) { Box::new(vec![['a'; 2], ['\u{80}', '\0'], ['\u{d7ff}', '\''], ['\u{e000}'; 2], ['\u{e000}', ' ']]) } else { Box::new(vec![['Z'; 2], ['z', '0'], [' '; 2]]) }), &(match (S1) { S1 => Box::new(vec![['Z'; 2], [' '; 2], ['中', '\\'], ['_', '\\'], ['\u{80}'; 2]]), _ => Box::new(Vec::<[char; 2]>::new()) })).clone());
        K2.mix();
        let mut v337: [u128; 3] = [10u128, (0usize as u128), ((match (&{ ([9i128],) }) { ([14i128],) => (*p112), _ => (*p111), _ => (*p111) }) as u128)];
        { let at = 1; let value = (if (false) { (match (&{ [Box::new(42535295865117307932921825928971026431u128), Box::new(145351998528978059790035618975332891543u128), Box::new(27u128)] }) { [p341, ..] => (*(*p341)), [p342, _, p343] => (*(*p343)), _ => 0u128 }) } else { 0u128 }); unsafe { raw_set(v337.as_mut_ptr(), at, value); raw_get(v337.as_ptr(), at).mix(); } }
        v337.mix();
        [(match ([Box::new(' '), Box::new('中'), Box::new('a'), Box::new('\n'), Box::new('中')]) { [p347, p346 @ .., p348] if false => Box::new(340282366920938463463374607431768211455u128), [_, .., p349] if true => Box::new(340282366920938463463374607431768211455u128), _ => Box::new(162259276829213363391578010288129u128) }), Box::new(v337[1]), pick::<Box<u128>>(false, &Box::new(28u128), &Box::new(9u128)).clone()].mix();
        let mut n350 = 0u32;
        'l351: while n350 < 3 && ((unsafe { *(&true as *const bool) })) {
            n350 += 1;
            match ([['0', '\u{7f}']; 2]) {
                [[p354, '中'], [.., '\u{d7ff}']] if (true & true) => {
                    let v355: NonZeroU8 = K3;
                }
                [.., p356] => {
                    let v357: char = p356[0];
                }
                [.., [p358, ..]] => {
                    if ((match (&{ [Box::new((-0.0f32))] }) { [.., p360] => Some([['_'; 8]; 1]), _ => Some([['😀'; 8]]) }).is_some()) {
                        let v361: Box<Box<u16>> = Box::new(Box::new(16u16));
                        let mut v362: Vec<Option<char>> = ({ let mut items = f58::<Vec<Option<char>>, Box<[u128; 1]>>(vec![None::<char>; 0], (match ([Box::new('a'), Box::new('9'), Box::new('\u{7f}'), Box::new('\u{d7ff}')]) { [_, _, p366, p367] if false => Box::new([340282366920938463463374607431768211455u128; 1]), _ => Box::new([20u128; 1]) }), ({ let t368: Box<Box<u16>> = v361.clone(); Box::new(Box::new([11u128])) })); let at = items.len() / 2; items.split_off(at) });
                        {
                            { let x = black_box((3.4028236692093846e38) as f64); for word in [(x as u8) as i128 as u128 as u64, (x as i128) as i128 as u128 as u64, (x as i64) as i128 as u128 as u64] { feed(word); } }
                            { let x = black_box((-0.5) as f32); for word in [(x as i16) as i128 as u128 as u64, (x as usize) as i128 as u128 as u64, (x as i64) as i128 as u128 as u64, (x as i32) as i128 as u128 as u64, (x as i8) as i128 as u128 as u64, (x as u16) as i128 as u128 as u64] { feed(word); } }
                            { let x = black_box((65535.9) as f64); for word in [(x as i8) as i128 as u128 as u64, (x as u128) as i128 as u128 as u64, (x as u64) as i128 as u128 as u64] { feed(word); } }
                            { let x = black_box((9007199254740993.0) as f32); for word in [(x as u16) as i128 as u128 as u64, (x as u8) as i128 as u128 as u64, (x as isize) as i128 as u128 as u64, (x as i32) as i128 as u128 as u64] { feed(word); } }
                            { let x = black_box((-1.0) as f64); for word in [(x as u64) as i128 as u128 as u64, (x as i8) as i128 as u128 as u64, (x as usize) as i128 as u128 as u64] { feed(word); } }
                            { let x = black_box(isize::MAX / 3 as isize); (x as f32).mix(); (x as f64).mix(); ((x as f32) as isize).mix(); ((x as f64) as isize).mix(); (x as f64 as f32).mix(); }
                        }
                    }
                    if ((f58::<Box<Vec<u32>>, (Option<u128>, [u16; 3])>(Box::new(vec![3292728652u32, 262143u32]), (None::<u128>, [100u16, 25u16, 100u16]), Box::new((Some(340282366920938463463374607431768211455u128), [1u16; 3]))) < f58::<Box<Vec<u32>>, [Vec<char>; 5]>(Box::new(vec![1573306179u32, 3455748037u32, 29u32, 3213641903u32, 32767u32]), [vec!['\u{d7ff}'], Vec::<char>::new(), vec!['_'], vec!['\u{10FFFF}', '\u{80}', '_'], Vec::<char>::new()], Box::new([Vec::<char>::new(), vec!['0', '\u{7f}'], vec!['z', '\u{e000}'], vec!['\u{80}', 'a'], vec!['_', '\u{d7ff}', 'ß']])))) {
                        let v369: Vec<Option<char>> = vec![Some('\'')];
                        twice::<[Option<u128>; 3]>(&({ let mut items = [Some(10u128); 3]; items.sort_by(|a, b| b.cmp(a)); items })).1.mix();
                    } else {
                        [vec![65343167942501977495190489515398338200i128, 87871865194887809920388257661040044689i128, 100i128, 33554432i128, 4951760157141521099596496897i128], (match (({ let mut items = vec![Box::new(Box::new(32769u16))]; items.push(v336.clone()); items }).as_slice()) { [p379] => Vec::<i128>::new(), _ => vec![8i128, (-18747609072885105259066629057119589806i128)] }), ({ let mut items = vec![(-13i128), (-13i128)]; items.insert(0, ({ let t383: Option<[f32; 2]> = None::<[f32; 2]>; (-170141183460469231731687303715884105727i128) })); items })].mix();
                        let mut v385: Box<Box<u16>> = (match (&{ f58::<[Box<u128>; 8], [Option<u128>; 3]>([Box::new(322928091593680519040237708224525692836u128), Box::new(10843976144851532481948132570574035718u128), Box::new(1u128), Box::new(40u128), Box::new(18u128), Box::new(0u128), Box::new(256669772664817598509468048758536531877u128), Box::new(340282366920938463463374607431768211454u128)], [Some(23u128); 3], Box::new([Some(1048576u128), None::<u128>, None::<u128>])) }) { [.., p388] if String::from("").chars().skip_while(|x389: &char| -> bool { false }).map(|x390: char| -> Box<Box<u16>> { v336.clone() }).all(|x391: Box<Box<u16>>| -> bool { false }) => v336.clone(), [_, .., p392] => v336.clone(), [p393, .., p394] => twice::<Box<Box<u16>>>(&v336.clone()).1, _ => Box::new(Box::new(6u16)) });
                    }
                }
                _ => pick::<[[char; 8]; 1]>(false, &[['\0'; 8]; 1], &[['\u{d7ff}'; 8]]).clone().mix(),
            }
            larger::<Box<Vec<u32>>>(f58::<Box<Vec<u32>>, [Box<u128>; 8]>(Box::new(vec![8u32, 30u32]), [Box::new(1u128), Box::new(170141183460469231731687303715884105727u128), Box::new(2199023255552u128), Box::new(25383555970713901046212944167848723756u128), Box::new(23u128), Box::new(0u128), Box::new(8u128), Box::new(1u128)], Box::new([Box::new(6u128), Box::new(305498377633066941823794898867991404868u128), Box::new(23u128), Box::new(312254371149675322088636339864932422046u128), Box::new(21u128), Box::new(8u128), Box::new(2u128), Box::new(5316911983139663491615228241121378303u128)])), Box::new(vec![30u32, 2147483647u32, 268435456u32])).mix();
        }
    }
}

mod c3 {
    use super::*;
    #[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord, Hash)]
    pub struct S1<A, B> { pub f0: B, pub f1: Vec<A> }
    impl<A: Mix, B: Mix> Mix for S1<A, B> {
        fn mix(&self) {
            feed(2);
            self.f0.mix();
            self.f1.mix();
        }
    }
    #[derive(Clone, Debug, PartialEq, PartialOrd, Copy, Eq, Ord, Hash)]
    pub struct S2 { pub f0: i32 }
    impl Mix for S2 {
        fn mix(&self) {
            feed(1);
            self.f0.mix();
        }
    }
    static K3: u64 = 0u64;
    static K4: i32 = 10i32;
    static K5: u32 = 23u32;
    pub trait T6 {
        fn m7(&self, a0: NonZeroU8) -> i32;
        fn m8(&self) -> u32;
    }
    impl T6 for u32 {
        fn m7(&self, a9: NonZeroU8) -> i32 {
            let mut v10: BTreeMap<i32, i32> = BTreeMap::new();
            v10.insert(i32::from_str_radix("Abc Def", 36).unwrap_or((f32::EPSILON as i32)), i32::from_be_bytes(K4.overflowing_shr((*self)).0.to_be_bytes())).mix();
            v10.insert(i32::try_from(3i128).unwrap_or(K4).wrapping_rem(K4.saturating_sub(K4) | 1), Some((-10i32)).unwrap_or_else(|| -> i32 { 8i32.wrapping_neg() })).mix();
            v10.first_key_value().map(|(key, _)| key.clone()).mix();
            v10.len().mix();
            let v13: S1<i32, f64> = twice::<S1<i32, f64>>(&S1 { f0: (-372.3083f64), f1: vec![4096i32, 14i32, 4095i32, 16777217i32, 31i32] }).0;
            i32::from_ne_bytes((Some(1e10f64).unwrap_or(7.0f64) as i32).to_ne_bytes())
        }
        fn m8(&self) -> u32 {
            ((-311.2741f32) as u32).clamp(0u32, 38u32)
        }
    }
    impl T6 for f64 {
        fn m7(&self, a15: NonZeroU8) -> i32 {
            {
                struct Outer(Noisy, Bomb, Noisy);
                impl Drop for Outer { fn drop(&mut self) { feed(0x0d0d + self.0.0 as u64); } }
                caught(|| { let _a = Noisy(11); let _h0 = Outer(Noisy(34), Bomb(53), Noisy(22)); let _z = Noisy(23); });
                caught(|| { let _a = Noisy(9); let _ = Duo { a: Bomb(19), b: Noisy(35) }; take3(Noisy(22), vec![Box::new(Noisy(24)) as Box<dyn Any>, Box::new(Bomb(43)), Box::new(Noisy(26))], Noisy(13)); let _z = Noisy(20); });
                caught(|| { let _a = Noisy(41); let _ = Duo { a: Noisy(18), b: (Bomb(15), Noisy(26)) }; let _z = Noisy(4); });
            }
            (K4.overflowing_mul(K4).1 as i32)
        }
        fn m8(&self) -> u32 {
            if (false) {
                let v16: u64 = K3.overflowing_shr(13u32).0.min(u64::from_str_radix("ff", 16).unwrap_or(K3));
                {
                    #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(C)] struct Plain { a: u8, b: u32, c: u16, d: u8 }
                    #[derive(Clone, Copy, Default)] #[repr(C, packed(2))] struct Tight { a: u8, b: u64, c: u16, d: u128 }
                    #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(C, align(2))] struct Big { a: u8, b: [u16; 3] }
                    #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(align(2))] struct Rusty(u8, u32);
                    #[derive(Clone, Copy, Debug, PartialEq)] #[repr(transparent)] struct Wrap(f32);
                    #[derive(Clone, Copy, Debug, PartialEq)] #[repr(i8)] enum Signed { Low = -128, Mid = -1, High = 127 }
                    #[derive(Clone, Copy, Debug, PartialEq)] #[repr(u16)] enum Wider { A = 1, B = 0x1234, C = 0xffff }
                    #[derive(Clone, Copy, Debug, PartialEq)] #[repr(C)] enum Cee { Unit, One(u8), Two { x: u16, y: u64 } }
                    #[derive(Clone, Copy, Debug, PartialEq)] #[repr(C, u8)] enum Both { P(u32), Q(u8, u8), R }
                    #[derive(Clone, Copy)] #[repr(C)] union Bits { word: u32, bytes: [u8; 4], half: (u16, u16) }
                    #[inline(never)] extern "C" fn c_plain(p: Plain, t: Tight, w: Wrap) -> Plain { Plain { a: p.a.wrapping_add(t.a), b: p.b ^ { t.c } as u32, c: p.c, d: p.d } }
                    #[inline(never)] extern "C" fn c_big(x: u8, big: Big, y: u8, rusty: Rusty) -> Big { Big { a: big.a ^ x ^ y ^ rusty.0, b: [big.b[2], big.b[0], rusty.1 as u16] } }
                    #[inline(never)] fn r_enums(s: Signed, w: Wider, c: Cee, b: Both) -> (i8, u16, Cee, Both) { (s as i8, w as u16, match c { Cee::One(v) => Cee::Two { x: v as u16, y: 1 }, other => other }, match b { Both::Q(x, y) => Both::Q(y, x), other => other }) }
                    { let both = black_box([Both::P(7), Both::Q(1, 2), Both::R]); for it in both { feed(unsafe { *(&it as *const Both as *const u8) } as u64); feed(match it { Both::P(v) => v as u64, Both::Q(x, y) => (x + y) as u64, Both::R => 99 }); } }
                    { let mut t = black_box(Tight { a: 5, b: 20, c: 7, d: u128::MAX }); t.b += 1; t.d = t.d.wrapping_add(5); let (b, c, d) = (t.b, t.c, t.d); feed(b + c as u64); d.mix(); feed(unsafe { std::ptr::addr_of!(t.d).read_unaligned() } as u64); }
                    { let mut bits = Bits { word: black_box(5u32.wrapping_mul(0x01010101)) }; unsafe { bits.bytes[0] ^= 0xff; feed(bits.word.to_le() as u64); feed(u32::from_le_bytes(bits.bytes) as u64); bits.half = (1, 2); feed(u32::from_le(bits.word) as u64); } }
                    feed((std::mem::offset_of!(Plain, b) + std::mem::offset_of!(Plain, c) * 16 + std::mem::offset_of!(Plain, d) * 256 + std::mem::offset_of!(Tight, d) * 4096 + std::mem::offset_of!(Big, b) * 65536) as u64);
                    { let w = black_box(Wrap(2.5)); let raw: f32 = unsafe { std::mem::transmute(w) }; raw.mix(); let items = black_box([Rusty(1, 2), Rusty(3, 4)]); feed(items[1].1 as u64 + size_of_val(&items) as u64 * 16); }
                }
            }
            10u64.checked_shr(27u32).unwrap_or(K3).wrapping_div_euclid(11256217406054894619u64 | 1).mix();
            (match (Ok::<String, [Option<i128>; 4]>(String::from("3.5")).unwrap_or_else(|x17: [Option<i128>; 4]| -> String { String::from("3.5") }).as_str()) { "0123456789" => 2634905826u32, p18 => (if (false) { 1475868306u32 } else { K5 }), _ => K5.overflowing_shl(K5).0 })
        }
    }
    impl T6 for i32 {
        fn m7(&self, a19: NonZeroU8) -> i32 {
            if let [Some(p23), _, Some(p24), Some(p25)] = (({ let t22: u64 = K3; [None::<i128>, Some((-20258099297118698071500252571542405718i128)), None::<i128>, Some(151115727451828646838273i128)] })) {
                let v26: S1<i32, f64> = twice::<S1<i32, f64>>(&S1 { f0: 0.0f64, f1: vec![7i32, 32i32, 2147483646i32] }).0;
                let f28 = move || -> f64 { let v27 = v26; let out = v27.f0; drop(v27); out };
                f28().mix();
                let v29: String = format!("{:#?}", "1e-320".parse::<f64>().unwrap_or(65535.0f64));
            } else {
                let v30: [Option<String>; 3] = (if (matches!(S2 { f0: (-1i32) }, (S2 { .. }))) { <[Option<String>; 3]>::try_from(vec![Some(String::from("")), Some(String::from("-7")), None::<String>, Some(String::from("x y")), Some(String::from("x y"))]).unwrap_or([Some(String::from("  pad  ")), None::<String>, Some(String::from("Abc Def"))]) } else { [String::from("Abc Def"), String::from("héllo"), String::from("hello")].map(|x38: String| -> Option<String> { Some(String::from("line\n")) }) });
                let f32 = move || -> S2 { let v31 = v30; let out = [[S2 { f0: 2147483646i32 }, S2 { f0: 100i32 }, S2 { f0: 616178915i32 }, S2 { f0: (-399524342i32) }]; 17].map(|x40: [S2; 4]| -> S2 { x40[1] })[5]; drop(v31); out };
                f32().mix();
            }
            (*self).checked_shl(262145u32).unwrap_or((*self)).overflowing_shr((262144u32.abs_diff(K5) as u32)).0
        }
        fn m8(&self) -> u32 {
            let v41: String = String::from("héllo");
            let f43 = move || -> NonZeroU8 { let v42 = v41; let out = NonZeroU8::new((true as u8)).unwrap_or(NonZeroU8::new(3u8).unwrap_or(NonZeroU8::new(228u8).unwrap())); drop(v42); out };
            apply_boxed(Box::new(f43)).mix();
            K5.checked_sub(11u32).unwrap_or(K5).wrapping_div(({ let t44: String = String::from("hello"); K5 }) | 1)
        }
    }
    impl T6 for S1<i32, f64> {
        fn m7(&self, a45: NonZeroU8) -> i32 {
            (-13i32)
        }
        fn m8(&self) -> u32 {
            sel::<u32>(true, K5, K5).checked_shl((K5 | K5)).unwrap_or(((-9223372036854775808.0f32) as u32))
        }
    }
    pub fn f46(a47: &mut S1<i32, f64>, a48: isize, a49: i128, a50: S2, a51: isize) -> S2 {
        let mut v52: S1<i32, f64> = S1 { f0: (-(*a47).f0), f1: pick::<Vec<i32>>(false, &(*a47).f1.clone(), &(*a47).f1.clone()).clone() };
        S2 { f0: 2i32 }
    }
    #[inline(never)]
    pub fn f54(a55: i32) -> S1<i32, f64> {
        {
            let f56 = || -> [S2; 17] { larger::<[S2; 17]>(({ let mut items = [S2 { f0: 14i32 }; 17]; items.reverse(); items }), ({ let mut items = [S2 { f0: (-477423691i32) }, S2 { f0: 684573590i32 }, S2 { f0: 11i32 }, S2 { f0: (-1342369408i32) }, S2 { f0: 8191i32 }, S2 { f0: (-1i32) }, S2 { f0: 9i32 }, S2 { f0: (-16i32) }, S2 { f0: (-2i32) }, S2 { f0: 10i32 }, S2 { f0: (-1407250305i32) }, S2 { f0: 100i32 }, S2 { f0: 8i32 }, S2 { f0: 17i32 }, S2 { f0: 14i32 }, S2 { f0: 17i32 }, S2 { f0: 15i32 }]; items.swap(0, 16); items })) };
            let p66: [Option<i128>; 4] = ({ let t65: [S2; 4] = [S2 { f0: 1073741823i32 }, S2 { f0: (-2147483647i32) }, S2 { f0: (-4i32) }, S2 { f0: 7i32 }]; [Some(8i128), Some((-35940331962741815898170949521314507252i128)), Some((-6i128)), Some(48804257853431924277202278319375967250i128)] });
            {
                let f68 = |a67: String| -> S2 { ({ let mut m69: S1<i32, f64> = S1 { f0: (-761.5659f64), f1: Vec::<i32>::new() }; let out = f46(&mut m69, (-6isize), 1i128.saturating_add(2i128), S2 { f0: K4 }, (16isize & 7isize)); m69.mix(); out }) };
                let mut v70: BTreeMap<isize, u64> = BTreeMap::new();
                v70.insert((unsafe { std::mem::transmute::<u16, [u8; 2]>(22582u16.overflowing_add(65535u16).0) }[0] as isize), K3).mix();
                v70.insert(isize::from_ne_bytes(6432465149782071142isize.to_ne_bytes()).checked_rem((unsafe { std::mem::transmute::<u64, u64>(K3) } as isize)).unwrap_or(('z' as isize)), 13257028394372436031u64.overflowing_mul(K3).0.overflowing_sub(hash_of(&[Some(String::from("  pad  ")), Some(String::from("0123456789")), Some(String::from("0123456789"))])).0).mix();
                v70.entry(((-707.6620f32) as isize)).or_insert(K3.overflowing_add(1152921504606846975u64).0).mix();
                v70.len().mix();
                let v71: isize = sel::<isize>((p66 > [Some(1048576i128), None::<i128>, None::<i128>, Some(16i128)]), (-9223372036854775807isize), 4611686018427387903isize.overflowing_mul((-1812694405543665939isize)).0);
                f68(({ let mut text = String::from("Abc Def"); text.pop(); text })).mix();
            }
            f56().mix();
        }
        sel::<S1<i32, f64>>((match (String::from("x y").as_str()) { p73 => None::<[Option<i128>; 4]>, _ => Some([Some(129i128), Some((-77530790197717811991957418778311122199i128)), Some(309485009821345068724781055i128), None::<i128>]) }).is_none(), twice::<S1<i32, f64>>(&S1 { f0: (-9223372036854775808.0f64), f1: vec![2i32, 2147483646i32, (-14i32)] }).0, S1 { f0: S1 { f0: f64::EPSILON, f1: vec![(-13i32)] }.f0, f1: twice::<Vec<i32>>(&vec![0i32, 1048575i32]).0 })
    }
    #[inline(never)]
    pub extern "C" fn f75(a76: i32, a77: i32, a78: f64, a79: u64) -> f32 {
        let v80: i32 = (!a76);
        (-(match ([Some((-108592893955670932820842795669720259270i128)); 4]) { [Some(-4i128), p81 @ .., None] => 4294967296.0f32, _ => 1e10f32 }))
    }
    #[inline(never)]
    pub fn f82(a83: &mut [S2; 3]) -> Option<S2> {
        {
            {
                struct Outer(Noisy, Bomb, Noisy);
                impl Drop for Outer { fn drop(&mut self) { feed(0x0d0d + self.0.0 as u64); } }
                caught(|| { let _a = Noisy(3); let mut v0 = vec![Bomb(21), Bomb(52), Bomb(46)]; v0.truncate(1); let _z = Noisy(18); });
            }
            let mut n84 = 0u32;
            'l85: while n84 < 5 && (([Some(String::from("😀")), Some(String::from("a,b,,c")), Some(String::from("line\n"))] == [Some(String::from("0123456789")), Some(String::from("héllo")), Some(String::from("😀"))])) {
                n84 += 1;
                if (({ let t86: String = String::from("a,b,,c"); false })) { return Some(f32::MIN).map(|x87: f32| -> S2 { ({ let mut m88: S1<i32, f64> = S1 { f0: (-9223372036854775808.0f64), f1: vec![1i32, 11i32, 1i32] }; let out = f46(&mut m88, 1555601566840293926isize, 158611006697760810065859547933165685176i128, (*a83)[0], 7isize); m88.mix(); out }) }); }
            }
            (*a83)[1].f0.overflowing_shl(K5).0.mix();
        }
        ({ let mut items = vec![S2 { f0: (-18i32) }, S2 { f0: (-8i32) }, S2 { f0: 9i32 }, S2 { f0: (-1603519699i32) }, S2 { f0: 1i32 }]; items.truncate(1); items }).first().cloned()
    }
    #[inline(never)]
    pub fn f91(a92: &dyn T6, a93: &mut [NonZeroU8], a94: &[i128], a95: f32, a96: S1<i32, f64>, a97: i128, a98: u64, a99: S1<i32, f64>, a100: &mut [S2; 17], a101: isize, a102: [S2; 3], a103: f64, a104: i32) -> f64 {
        { let value = NonZeroU8::new(32u8.saturating_pow(K5 % 7)).unwrap_or((if (false) { NonZeroU8::new(1u8).unwrap() } else { NonZeroU8::new(4u8).unwrap() })); if let Some(slot) = a93.first_mut() { *slot = value; } }
        'l109: for x108 in ((24i32..25i32).chain(None::<Vec<i32>>.map_or(vec![7i32], |x107: Vec<i32>| -> Vec<i32> { a96.f1.clone() }))) {
            if (None::<[S2; 3]>.is_none()) { return ((if let Some(x110) = (None::<f64>) { x110 } else { a96.f0 }) * f64::from(65i16)); }
            NonZeroU8::new((unsafe { std::mem::transmute::<u64, [u32; 2]>(K3) }[0] as u8)).unwrap_or(NonZeroU8::new(2u8).unwrap()).mix();
        }
        vec![(a97 & a97), a97.overflowing_shl(K5).0].mix();
        let mut n112 = 0u32;
        'l113: loop {
            n112 += 1;
            if n112 > 3 { break; }
            let mut n115 = 0u32;
            let v117: isize = 'l116: loop {
                n115 += 1;
                if n115 > 2 { break isize::try_from(4u16).unwrap_or((-7098461887750444784isize)); }
                a102[2].mix();
                let mut v118: BTreeMap<u64, [S2; 4]> = BTreeMap::new();
                v118.insert((if (false) { Some(12u64).unwrap_or_else(|| -> u64 { K3 }) } else { K3.max(K3) }), ({ let mut items = ({ let mut items = [S2 { f0: 12i32 }, S2 { f0: 2147483647i32 }, S2 { f0: (-8i32) }, S2 { f0: 2i32 }]; items.swap(0, 3); items }); items.sort_unstable(); items })).mix();
                v118.entry(a98).or_insert(({ let mut items = [S2 { f0: 100i32 }, S2 { f0: 19i32 }, S2 { f0: (-15i32) }, S2 { f0: 17i32 }]; items.rotate_left(2); items })).mix();
                v118.len().mix();
                (*a100)[14].f0 = ((if ((true && false)) { i32::try_from(a101).unwrap_or(a102[2].f0) } else { (-1i32).checked_shr(K5).unwrap_or(0i32) }) ^ i32::from(122u8.swap_bytes()));
                let v126: S1<i32, f64> = S1 { f0: 0.1f64, f1: vec![402621856i32, 1087845816i32, 1073741823i32] };
                let f128 = move || -> S2 { let v127 = v126; let out = ({ let mut m129: S1<i32, f64> = S1 { f0: (-799.8641f64), f1: vec![18i32, 2097153i32] }; let out = f46(&mut m129, a101.to_be(), a97.checked_rem(a97).unwrap_or(a97), a102[2], ({ let t130: S2 = a102[2]; a101 })); m129.mix(); out }); drop(v127); out };
                apply_once(f128).mix();
            };
            let v131: [Option<String>; 3] = (unsafe { let mut slot = MaybeUninit::<[Option<String>; 3]>::uninit(); slot.write([Some(String::from("  pad  ")), None::<String>, Some(String::from("ff"))]); slot.assume_init() });
            let f133 = move || -> NonZeroI64 { let v132 = v131; let out = NonZeroI64::new(3i64.min(3113307517972961666i64)).unwrap_or(('b136: { if (true) { break 'b136 NonZeroI64::new(2147483648i64).unwrap(); } NonZeroI64::new(35184372088832i64).unwrap() })); drop(v132); out };
            f133().mix();
            (*a100) = ('b139: { if (false) { break 'b139 [S2 { f0: 5i32 }, S2 { f0: 1853229835i32 }, S2 { f0: (-1598541223i32) }, S2 { f0: 17i32 }, S2 { f0: 100i32 }, S2 { f0: 1048577i32 }, S2 { f0: 4i32 }, S2 { f0: 7i32 }, S2 { f0: 8i32 }, S2 { f0: (-2147483647i32) }, S2 { f0: 18i32 }, S2 { f0: (-2147483647i32) }, S2 { f0: (-17i32) }, S2 { f0: 14i32 }, S2 { f0: 100i32 }, S2 { f0: (-17i32) }, S2 { f0: 1073741823i32 }]; } [S2 { f0: (-2088629358i32) }; 17] });
        }
        let mut n140 = 0u32;
        'l141: while n140 < 2 && (false) {
            n140 += 1;
            let r142 = catch_unwind(AssertUnwindSafe(|| -> S2 {
                let v143: S1<i32, f64> = a96.clone();
                let f145 = move || -> f32 { let v144 = v143; let out = a95; drop(v144); out };
                f145().mix();
                NonZeroU8::new(2u8).unwrap_or(NonZeroU8::new(92u8).unwrap()).mix();
                {
                    { let mut kept = Vec::new(); for i in 0..5u32 { let n = Noisy(i); let by_ref = || n.0 * 2; by_ref().mix(); if i % 2 == 0 { kept.push(move || n.0); } } for f in kept.iter().rev() { f().mix(); } }
                    { let mut fs: Vec<Box<dyn FnMut() -> u32>> = Vec::new(); for i in 0..5u32 { let mut own = i; fs.push(Box::new(move || { own += 57; own })); } for round in 0..2 { for f in fs.iter_mut() { feed(f() as u64 + round); } } }
                    { let found = 'search: loop { for i in 0..5u32 { let hit = move |at: u32| at * i == 3; for j in 0..3 { if hit(j) { break 'search Some((i, j)); } } } break None; }; found.mix(); }
                }
                ({ let mut m146: S1<i32, f64> = f54(K4); let out = f46(&mut m146, 617928902704837771isize.overflowing_shl(K5).0, a97, [S2 { f0: 1073741823i32 }; 3][2], (a101 ^ a101)); m146.mix(); out })
            }));
            match r142 {
                Ok(value) => value.mix(),
                Err(caught) => payload(caught),
            }
        }
        f64::from(i16::try_from(a94.len()).unwrap_or((-32767i16)))
    }
    pub fn run() {
        {
            let f148 = move |a147: i32| -> i128 { (match (String::from("x y").as_str()) { p149 => pick::<i128>(true, &2251799813685249i128, &0i128).clone(), p150 => 7i128, "héllo" if (!false) => (-6i128).rotate_right(1947352702u32), _ => ((-4i128).overflowing_mul(170141183460469231731687303715884105726i128).1 as i128) }) };
            let mut n151 = 0u32;
            'l152: loop {
                n151 += 1;
                if n151 > 5 { break; }
                let mut v154: isize = (-3388889291307358949isize);
                let mut n155 = 0u32;
                let v157: Option<String> = 'l156: loop {
                    n155 += 1;
                    if n155 > 2 { break ({ let t159: NonZeroU8 = NonZeroU8::new(1u8).unwrap(); Some(String::from("-7")) }); }
                    v154.mix();
                    if (format!("{:?}", [S2 { f0: 8i32 }, S2 { f0: 6i32 }, S2 { f0: 1325943105i32 }, S2 { f0: 2147483647i32 }]).ends_with("a")) { break 'l152; }
                    let mut v160: BTreeMap<Option<S2>, u64> = BTreeMap::new();
                    v160.insert(Some(({ let mut m162: S1<i32, f64> = S1 { f0: 1e30f64, f1: vec![2147483647i32] }; let out = f46(&mut m162, 32isize, 57652540383950433765646016462186859571i128, S2 { f0: 1073741823i32 }, v154); m162.mix(); out })), (K3.overflowing_mul(K3).0.checked_ilog2().unwrap_or(0) as u64)).mix();
                    v160.insert(({ let mut items = vec![S2 { f0: 8i32 }, S2 { f0: 2147483647i32 }, S2 { f0: (-20i32) }]; let end = items.len().min(0); items.drain(..end).collect::<Vec<S2>>() }).get(1).cloned(), hash_of(&24u128.reverse_bits())).mix();
                    v160.insert(Err::<NonZeroU8, u32>(100u32).err().map(|x165: u32| -> S2 { ({ let mut m167: S1<i32, f64> = S1 { f0: 7.0f64, f1: vec![2147483647i32, (-2147483647i32), 16385i32, (-2147483647i32), 16i32] }; let out = f46(&mut m167, v154, 18i128, S2 { f0: (-1i32) }, 9203297941195081784isize); m167.mix(); out }) }), (K3.checked_div(18446744073709551615u64).unwrap_or(K3) & K3.saturating_pow(K5 % 7))).mix();
                    v160.insert(('b169: { if (true) { break 'b169 Some(S2 { f0: (-414481695i32) }); } ({ let mut m172: [S2; 3] = [S2 { f0: 6i32 }; 3]; let out = f82(&mut m172); m172.mix(); out }) }), (28u8.wrapping_pow(33u32 % 7) as u64)).mix();
                    v160.entry(Some(S2 { f0: 2i32 })).or_insert(K3).mix();
                    v160.len().mix();
                };
                let mut v177: Vec<i32> = ({ let mut items = vec![K4; (32usize.saturating_mul(6647267401978843080usize) % 6)]; items.insert(0, K4); items });
                v177.truncate(6);
                'l179: while let Some(x178) = v177.pop() {
                    v154 = ((20isize | (-1877149095803009566isize)) & (v154 | v154));
                    S2 { f0: 1073741823i32 }.f0.mix();
                    let v182: [Option<String>; 3] = ('b187: { if (true) { break 'b187 [Some(String::from("ff")), None::<String>, Some(String::from("42"))]; } [Some(String::from("héllo")), None::<String>, Some(String::from("Abc Def"))] });
                    let f184 = move || -> [Option<i128>; 4] { let v183 = v182; let out = ({ let t190: Option<String> = twice::<Option<String>>(&Some(String::from("😀"))).0; [None::<Option<f32>>; 4].map(|x193: Option<Option<f32>>| -> Option<i128> { None::<i128> }) }); drop(v183); out };
                    apply_boxed(Box::new(f184)).mix();
                    f148(K4.checked_div(K4).unwrap_or(K4)).mix();
                }
            }
            let r194 = catch_unwind(AssertUnwindSafe(|| -> S1<i32, f64> {
                if ((6u64..7u64).chain(vec![32u64, 1u64]).skip_while(|x196: &u64| -> bool { true }).skip(3).all(|x198: u64| -> bool { 36u8.overflowing_add(37u8).1 })) {
                    let mut v199: BTreeMap<isize, char> = BTreeMap::new();
                    v199.insert(2086924395092427751isize, Some('中').unwrap_or_else(|| -> char { '\\' }).to_ascii_uppercase()).mix();
                    v199.insert(isize::try_from((-15i16)).unwrap().checked_shr((8u32 ^ K5)).unwrap_or(((-1isize) >> 2590320163u32)), (unsafe { let mut slot = MaybeUninit::<char>::uninit(); slot.write(char::from_digit(K5 % 36, 36).unwrap_or('中')); slot.assume_init() })).mix();
                    v199.insert((14u8.wrapping_rem(7u8 | 1) as isize), '\u{7f}'.to_ascii_uppercase().to_ascii_uppercase()).mix();
                    v199.insert(((-2isize).wrapping_rem_euclid((-9223372036854775807isize) | 1) << 20u32), sel::<char>(true, '😀', '\0').to_ascii_lowercase()).mix();
                    v199.remove(&2isize.checked_shl(K5).unwrap_or(137438953472isize)).mix();
                    v199.len().mix();
                    (K5 | K5).checked_shl(u32::try_from(17u8).unwrap_or(1304948633u32)).unwrap_or(((-101.9031f32) as u32)).checked_shl(u32::from((26u16.abs_diff(7023u16) as u16))).unwrap_or(({ let t201: [S2; 4] = ({ let mut items = [S2 { f0: 2147483647i32 }, S2 { f0: 8i32 }, S2 { f0: 2147483647i32 }, S2 { f0: 15i32 }]; items.reverse(); items }); 0u32 })).mix();
                    let mut v204: BTreeMap<NonZeroI64, u64> = BTreeMap::new();
                    v204.insert((match (Some(String::from("hello")).and_then(|x206: String| -> Option<NonZeroI64> { Some(NonZeroI64::new(8662182174250282850i64).unwrap()) })) { None => NonZeroI64::new(34359738369i64).unwrap(), Some(x205) => x205 }), ((2251799813685248u64 ^ K3) | ('b207: { if (true) { break 'b207 K3; } K3 }))).mix();
                    v204.remove(&NonZeroI64::new((-942509613480918709i64)).unwrap_or(NonZeroI64::new((-13i64)).unwrap())).mix();
                    v204.len().mix();
                }
                {
                    let f211 = move |a208: f64, a209: i32, a210: u8| -> i128 { ((1267650600228229401496703205376i128 / 3i128).overflowing_add((-(-1i128))).1 as i128) };
                    ({ let t214: [Option<char>; 8] = sel::<[Option<char>; 8]>((false || true), [S2 { f0: (-12i32) }, S2 { f0: 3i32 }, S2 { f0: 7i32 }, S2 { f0: (-1i32) }, S2 { f0: 3i32 }, S2 { f0: (-17i32) }, S2 { f0: (-4i32) }, S2 { f0: 8388608i32 }].map(|x218: S2| -> Option<char> { Some('_') }), Some([Some('ß'); 8]).expect("nothing there")); twice::<[Option<i128>; 4]>(&[1i8; 4].map(|x224: i8| -> Option<i128> { None::<i128> })).0 }).mix();
                    let mut v225: [Option<String>; 3] = (match (twice::<[Option<i128>; 4]>(&[Some(0i128), Some(8i128), Some(75111310824307036452731622051115196377i128), Some((-48611095802574097086105277109493248544i128))]).0) { [Some(p230), None, None, None] => [None::<String>, Some(String::from("héllo")), Some(String::from("  pad  "))], _ => [NonZeroI64::new(8344001348163157800i64).unwrap(); 3].map(|x232: NonZeroI64| -> Option<String> { Some(String::from("a")) }) });
                    f211(({ let mut m233: Vec<NonZeroU8> = vec![NonZeroU8::new(1u8).unwrap(), NonZeroU8::new(129u8).unwrap()]; let mut m234: [S2; 17] = [S2 { f0: 3i32 }; 17]; let out = f91(&7.0f64, &mut m233, &Vec::<i128>::new(), 0.1f32, S1 { f0: (-1.0f64), f1: vec![17i32, (-6i32)] }, 3i128, K3, S1 { f0: 20.3597f64, f1: vec![(-16i32), 1073741823i32] }, &mut m234, (-3692408399913173759isize), [S2 { f0: 725556895i32 }, S2 { f0: 16384i32 }, S2 { f0: 2i32 }], (-1.0f64), (-10i32)); m233.mix(); m234.mix(); out }), i32::from(17u8), 100u8.wrapping_neg()).mix();
                }
                let v235: Option<String> = ({ let t241: [S2; 3] = [S2 { f0: (-524108038i32) }, S2 { f0: 0i32 }, S2 { f0: (-4i32) }]; [Some(String::from("-7")), Some(String::from("3.5")), None::<String>] })[1].clone();
                let f237 = move || -> S1<i32, f64> { let v236 = v235; let out = f54(K4.overflowing_shr(K5).0); drop(v236); out };
                f237().mix();
                S1 { f0: ({ let mut m242: Vec<NonZeroU8> = Vec::<NonZeroU8>::new(); let mut m243: [S2; 17] = [S2 { f0: 4194304i32 }, S2 { f0: 19i32 }, S2 { f0: 7i32 }, S2 { f0: (-13i32) }, S2 { f0: 63i32 }, S2 { f0: 65i32 }, S2 { f0: (-4i32) }, S2 { f0: 1073741825i32 }, S2 { f0: 0i32 }, S2 { f0: 3i32 }, S2 { f0: 441757228i32 }, S2 { f0: 1073741823i32 }, S2 { f0: (-1102322752i32) }, S2 { f0: 1159123834i32 }, S2 { f0: 2i32 }, S2 { f0: 2147483647i32 }, S2 { f0: 100i32 }]; let out = f91(&S1 { f0: 1e30f64, f1: vec![1048576i32, 5i32, (-12i32)] }, &mut m242, &vec![2i128, 1180591620717411303425i128, (-18i128)], f32::INFINITY, S1 { f0: (-111.4185f64), f1: vec![33554431i32] }, (-170141183460469231731687303715884105727i128), 9149511117315256470u64, S1 { f0: (-448.1582f64), f1: vec![(-1456749027i32), 4095i32, 134217729i32] }, &mut m243, (-8810844795847473881isize), [S2 { f0: 4i32 }, S2 { f0: 0i32 }, S2 { f0: (-13i32) }], f64::MIN_POSITIVE, K4); m242.mix(); m243.mix(); out }), f1: twice::<Vec<i32>>(&vec![2147483646i32, (-6i32), 10i32]).0 }
            }));
            match r194 {
                Ok(value) => value.mix(),
                Err(caught) => payload(caught),
            }
            let mut n245 = 0u32;
            'l246: while n245 < 3 && (1e30f64.is_finite()) {
                n245 += 1;
                let mut v247: u64 = u64::try_from((-4990752408656814603isize)).unwrap_or(2097153u64);
                'l252: for x251 in (format!("<{:?}>", K5).bytes().step_by(1).flat_map(|x249: u8| -> Vec<char> { ({ let mut items = vec!['\n']; items.insert(0, '\u{80}'); items }) })) {
                    NonZeroI64::new(35184372088833i64).unwrap_or(NonZeroI64::new((-1i64)).unwrap_or(NonZeroI64::new(9223372036854775807i64).unwrap())).mix();
                    (x251 as i8).mix();
                    K4.mix();
                }
            }
            f148(S2 { f0: (-968679595i32) }.f0).mix();
        }
        let mut v253: Vec<i8> = ({ let mut items = vec![(-127i8)]; let at = items.len() / 2; items.split_off(at) });
        v253.truncate(6);
        'l255: while let Some(x254) = v253.pop() {
            {
                struct Outer(Noisy, Bomb, Noisy);
                impl Drop for Outer { fn drop(&mut self) { feed(0x0d0d + self.0.0 as u64); } }
                caught(|| { let _a = Noisy(58); take3(Noisy(26), Rc::new((Noisy(46), Bomb(10))), Noisy(41)); let _z = Noisy(50); });
                caught(|| { let _a = Noisy(52); let _ = Rc::new((Noisy(10), Bomb(16))); let _z = Noisy(13); });
            }
            let mut n257 = 0u32;
            let v259: u32 = 'l258: loop {
                n257 += 1;
                if n257 > 3 { break K5; }
                {
                    #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(C)] struct Plain { a: u8, b: u32, c: u16, d: u64 }
                    #[derive(Clone, Copy, Default)] #[repr(C, packed(2))] struct Tight { a: u8, b: u64, c: u16, d: u128 }
                    #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(C, align(8))] struct Big { a: u8, b: [u16; 3] }
                    #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(align(8))] struct Rusty(u8, u32);
                    #[derive(Clone, Copy, Debug, PartialEq)] #[repr(transparent)] struct Wrap(f32);
                    #[derive(Clone, Copy, Debug, PartialEq)] #[repr(i8)] enum Signed { Low = -128, Mid = -1, High = 127 }
                    #[derive(Clone, Copy, Debug, PartialEq)] #[repr(u16)] enum Wider { A = 1, B = 0x1234, C = 0xffff }
                    #[derive(Clone, Copy, Debug, PartialEq)] #[repr(C)] enum Cee { Unit, One(u8), Two { x: u16, y: u64 } }
                    #[derive(Clone, Copy, Debug, PartialEq)] #[repr(C, u8)] enum Both { P(u32), Q(u8, u8), R }
                    #[derive(Clone, Copy)] #[repr(C)] union Bits { word: u32, bytes: [u8; 4], half: (u16, u16) }
                    #[inline(never)] extern "C" fn c_plain(p: Plain, t: Tight, w: Wrap) -> Plain { Plain { a: p.a.wrapping_add(t.a), b: p.b ^ { t.c } as u32, c: p.c, d: p.d } }
                    #[inline(never)] extern "C" fn c_big(x: u8, big: Big, y: u8, rusty: Rusty) -> Big { Big { a: big.a ^ x ^ y ^ rusty.0, b: [big.b[2], big.b[0], rusty.1 as u16] } }
                    #[inline(never)] fn r_enums(s: Signed, w: Wider, c: Cee, b: Both) -> (i8, u16, Cee, Both) { (s as i8, w as u16, match c { Cee::One(v) => Cee::Two { x: v as u16, y: 1 }, other => other }, match b { Both::Q(x, y) => Both::Q(y, x), other => other }) }
                    { let big = c_big(41, black_box(Big { a: 1, b: [2, 3, 4] }), 36, black_box(Rusty(5, 6))); big.a.mix(); big.b.mix(); feed((&big as *const Big as usize % 8) as u64); }
                    { let mut bits = Bits { word: black_box(41u32.wrapping_mul(0x01010101)) }; unsafe { bits.bytes[0] ^= 0xff; feed(bits.word.to_le() as u64); feed(u32::from_le_bytes(bits.bytes) as u64); bits.half = (1, 2); feed(u32::from_le(bits.word) as u64); } }
                    feed((size_of::<Plain>() + align_of::<Plain>() * 256 + size_of::<Tight>() * 4096 + align_of::<Tight>() * 65536) as u64); feed((size_of::<Big>() + align_of::<Big>() * 256 + size_of::<Rusty>() * 4096 + size_of::<Cee>() * 65536 + size_of::<Both>() * 1048576) as u64);
                    { let both = black_box([Both::P(7), Both::Q(1, 2), Both::R]); for it in both { feed(unsafe { *(&it as *const Both as *const u8) } as u64); feed(match it { Both::P(v) => v as u64, Both::Q(x, y) => (x + y) as u64, Both::R => 99 }); } }
                    { for s in black_box([Signed::Low, Signed::Mid, Signed::High]) { feed((s as i8 as i64 + 200) as u64); feed((s as i32 == -1) as u64); } for w in black_box([Wider::A, Wider::B, Wider::C]) { feed(w as u64); } }
                    { let got = r_enums(black_box(Signed::Low), black_box(Wider::B), black_box(Cee::One(41)), black_box(Both::Q(1, 2))); feed(got.0 as u8 as u64 + got.1 as u64 * 256); format!("{:?} {:?}", got.2, got.3).mix(); }
                }
                let r260 = catch_unwind(AssertUnwindSafe(|| -> i8 {
                    (-((f64::MIN_POSITIVE as i32) % (false as i32))).mix();
                    let v261: bool = (324518553658426726783156020576255u128 / 80931771306105118644976009582526026789u128).overflowing_mul((true as u128)).1;
                    assert!(!v261, "held {}", ({ let t262: u128 = 45585583182812303195804031715135243699u128; K5 }));
                    (x254 >> 1u32)
                }));
                match r260 {
                    Ok(value) => value.mix(),
                    Err(caught) => payload(caught),
                }
            };
        }
        let r264 = catch_unwind(AssertUnwindSafe(|| -> f64 {
            'l266: for x265 in ((52i8..59i8).collect::<Vec<_>>()) {
                let v267: Option<String> = Some(String::from("a,b,,c")).and_then(|x270: String| -> Option<String> { None::<String> });
                let f269 = move || -> [S2; 2] { let v268 = v267; let out = <[S2; 2]>::try_from(({ let mut items = vec![S2 { f0: 2048i32 }, S2 { f0: (-765512775i32) }, S2 { f0: 17i32 }]; items.retain(|x274: &S2| -> bool { true }); items })).unwrap_or(<[S2; 2]>::try_from(vec![S2 { f0: 1956212263i32 }, S2 { f0: 1i32 }, S2 { f0: 940965919i32 }, S2 { f0: (-2147483648i32) }, S2 { f0: 1i32 }]).unwrap_or([S2 { f0: 17i32 }; 2])); drop(v268); out };
                apply_boxed(Box::new(f269)).mix();
                let mut v277: [String; 5] = ('b280: { if ((7isize.overflowing_sub((-4isize)).1 as isize).overflowing_add(15isize.overflowing_shl(3020332612u32).0).1) { break 'b280 ({ let t283: i128 = i128::try_from(K4).unwrap(); <[String; 5]>::try_from(vec![String::from("x y"), String::from("-7")]).unwrap_or([String::from(""), String::from("Abc Def"), String::from("0123456789"), String::from("a,b,,c"), String::from("line\n")]) }); } (match (vec![4294967295u32, 65535u32, 2153734669u32, 65535u32, 34u32].as_slice()) { [.., 31u32] if (-14i32).overflowing_sub(K4).1 => [String::from("Abc Def"), String::from("a,b,,c"), String::from("a,b,,c"), String::from("😀"), String::from("😀")], _ => ({ let mut items = [String::from("3.5"), String::from("ff"), String::from(""), String::from("héllo"), String::from("a,b,,c")]; items.swap(0, 4); items }) }) });
            }
            {
                let f294 = |a293: Option<f32>| -> isize { ((1073741825isize.abs_diff(8120050797479618107isize) as isize) / (-9223372036854775808isize).saturating_pow(18u32 % 7)) };
                'l296: for x295 in ((80u8..80u8)) {
                    let v297: S2 = ({ let mut m298: S1<i32, f64> = f54((K4 << K5)); let out = f46(&mut m298, ((-1682196406787400673isize).abs() % (-4168600512482248132isize)), i128::try_from(i16::from(127i8)).unwrap_or((-20i128)), ({ let mut m299: S1<i32, f64> = S1 { f0: f64::MIN, f1: Vec::<i32>::new() }; let out = f46(&mut m299, 8isize.checked_shr(K5).unwrap_or(10isize), (-75835673394423177842710582329776933976i128).overflowing_add((-1i128)).0, [S2 { f0: (-11i32) }, S2 { f0: (-1647566613i32) }, S2 { f0: 2147483646i32 }, S2 { f0: 634756513i32 }, S2 { f0: (-3i32) }, S2 { f0: 1764465854i32 }, S2 { f0: (-1189115225i32) }, S2 { f0: 2147483647i32 }, S2 { f0: (-726730788i32) }, S2 { f0: 8i32 }, S2 { f0: 199740357i32 }, S2 { f0: 0i32 }, S2 { f0: 2147483646i32 }, S2 { f0: 131073i32 }, S2 { f0: (-1158701365i32) }, S2 { f0: (-3i32) }, S2 { f0: (-20i32) }][8], 9223372036854775806isize.overflowing_shr(1u32).0); m299.mix(); out }), (f32::NEG_INFINITY.mul_add(823.4527f32, f32::MIN) as isize)); m298.mix(); out });
                    let mut v300: u32 = K5.wrapping_rem(K5 | 1).min(K5);
                    (2199023255552isize as i128).abs().mix();
                    {
                        async fn work(seed: u32) -> u64 { let h301 = (seed as f64 * 0.5, seed as u8, seed % 3 == 0); let w302 = later(3, seed as u64 + 20).await; let h303: u128 = (seed as u128) << 45; let w304 = later(2, seed as u64 + 41).await; (h301.0 as u64 + h301.1 as u64 + h301.2 as u64).rotate_left(0) ^ (w302).rotate_left(1) ^ ((h303 >> 3) as u64).rotate_left(2) ^ (w304).rotate_left(3) }
                        block_on(work(28)).mix();
                        block_on(work(79)).mix();
                    }
                }
                let v305: [S2; 3] = ({ let t308: [S2; 17] = ({ let mut items = [S2 { f0: 1i32 }, S2 { f0: (-1671481446i32) }, S2 { f0: 2i32 }, S2 { f0: 1912887168i32 }, S2 { f0: 1981310088i32 }, S2 { f0: 1i32 }, S2 { f0: 7i32 }, S2 { f0: 12i32 }, S2 { f0: 1073741823i32 }, S2 { f0: 8388609i32 }, S2 { f0: 1i32 }, S2 { f0: 421791422i32 }, S2 { f0: 14i32 }, S2 { f0: 7i32 }, S2 { f0: 1i32 }, S2 { f0: (-116141109i32) }, S2 { f0: (-2147483648i32) }]; items.reverse(); items }); ({ let mut items = [S2 { f0: 513i32 }; 3]; items.rotate_left(2); items }) });
                let mut v313: Vec<[Option<i128>; 4]> = vec![[Some(1i128), None::<i128>, None::<i128>, Some((-1i128))], [None::<i128>, None::<i128>, Some(38685626227668133590597632i128), Some(31111377990186045023771127271762248877i128)], [Some(2361183241434822606849i128), Some(17i128), Some(288230376151711745i128), Some(0i128)]];
                v313.truncate(6);
                'l315: while let Some(x314) = v313.pop() {
                    {
                        struct Outer(Noisy, Bomb, Noisy);
                        impl Drop for Outer { fn drop(&mut self) { feed(0x0d0d + self.0.0 as u64); } }
                        caught(|| { let _a = Noisy(30); let mut v0 = vec![Bomb(34), Bomb(58), Bomb(40)]; v0.truncate(1); let _z = Noisy(47); });
                        caught(|| { let _a = Noisy(42); let _h0 = Outer(Noisy(46), Bomb(60), Noisy(28)); let _z = Noisy(18); });
                        caught(|| { let _a = Noisy(59); let _h0 = [Some(Bomb(10)), None, Some(Bomb(38))]; let _h1 = Outer(Noisy(30), Bomb(11), Noisy(3)); let _z = Noisy(56); });
                    }
                    [None::<i128>; 4].mix();
                    {
                        enum Loud { I(i32, Noisy), C(char, Noisy) }
                        #[inline(never)] fn items(k: u32, last: char) -> [Loud; 3] { black_box([Loud::I(k as i32 % 4, Noisy(k)), Loud::C('q', Noisy(k + 1)), if k % 3 == 0 { Loud::I(1, Noisy(k + 2)) } else { Loud::C(last, Noisy(k + 2)) }]) }
                        #[inline(never)] fn pick0(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { [..] => 1, [Loud::C('a', _) | Loud::C(_, _), m @ .., Loud::C('a', _)] => 2, [..] => 3, [Loud::C('Z', _) | Loud::I(_, _), _, Loud::C(..)] => 4, _ => 0 } }
                        for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick0(k, last, flag) as u64); feed(0xf00d); } } }
                        #[inline(never)] fn pick1(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { [m @ .., _] if black_box(flag) => 1, [x, ..] => 2, _ => 0 } }
                        for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick1(k, last, flag) as u64); feed(0xf00d); } } }
                        #[inline(never)] fn pick2(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { [Loud::I(1, _), m @ .., Loud::C('a'..='c', _)] => 1, [Loud::I(..), m @ ..] => 2, _ if black_box(flag) => 3, [_, m @ ..] => 4, _ => 0 } }
                        for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick2(k, last, flag) as u64); feed(0xf00d); } } }
                        #[inline(never)] fn pair(k: u32, last: char, flag: bool) -> u8 { match (items(k, last), k % 3, flag) { ([_, ..], 2, true) => 1, ([.., Loud::C('b', _)], 1, _) => 2, ([x, m @ ..], 2, false) => 3, (all, _, _) if black_box(flag) => 4, _ => 0 } }
                        for k in 0..4u32 { for last in ['a', 'Z'] { for flag in [false, true] { feed(pair(k, last, flag) as u64); feed(0xf00d); } } }
                    }
                }
                f294(({ let mut items = vec![0.1f32, f32::INFINITY]; items.pop() })).mix();
            }
            let r317 = catch_unwind(AssertUnwindSafe(|| -> i8 {
                let mut n318 = 0u32;
                'l319: loop {
                    n318 += 1;
                    if n318 > 2 { break; }
                    let mut v321: i32 = ((K4.abs_diff(K4) as i32) / K4.min(K4));
                }
                ({ let t323: Option<Option<f32>> = Some(None::<[S2; 4]>.map(|x325: [S2; 4]| -> f32 { f32::INFINITY })); Some(1e30f32) }).mix();
                [(((-122i8) % 1i8) % 10i8.overflowing_shr(K5).0), (unsafe { std::mem::transmute::<u32, u32>(K5.saturating_mul(K5)) } as i8), ((-127i8).abs() / 63i8.checked_neg().unwrap_or_default())][std::mem::size_of::<NonZeroU8>()]
            }));
            match r317 {
                Ok(value) => value.mix(),
                Err(caught) => payload(caught),
            }
            let v326: bool = true;
            assert!(!v326, "held {}", (K5 / K5));
            ({ let mut m328: Vec<NonZeroU8> = vec![NonZeroU8::new(184u8).unwrap(); (6usize.checked_add(64usize).unwrap_or(8usize) % 6)]; let mut m332: [S2; 17] = <[S2; 17]>::try_from(Vec::<S2>::new()).unwrap_or([S2 { f0: (-2147483648i32) }, S2 { f0: 8i32 }, S2 { f0: 33554432i32 }, S2 { f0: 2147483647i32 }, S2 { f0: 20i32 }, S2 { f0: 2147483646i32 }, S2 { f0: 0i32 }, S2 { f0: (-1101916958i32) }, S2 { f0: 8i32 }, S2 { f0: 134217729i32 }, S2 { f0: 100i32 }, S2 { f0: 2097151i32 }, S2 { f0: 16384i32 }, S2 { f0: 100i32 }, S2 { f0: 10i32 }, S2 { f0: 416905274i32 }, S2 { f0: 701738220i32 }]); let out = f91(&K4.pow(K5 % 70), &mut m328, ({ let t331: u64 = K3; vec![100i128, 100i128] }).get(1..5).unwrap_or(&[]), f32::NAN, S1 { f0: f64::INFINITY, f1: vec![(-1i32), 128i32, (-592014628i32), 16i32, 3i32] }, (-13119903011799122696111632949766581253i128), (1.0f64 as u64), f54(K4), &mut m332, 20isize.saturating_mul(8isize), [S2 { f0: 4i32 }; 3], ({ let mut m337: Vec<NonZeroU8> = vec![NonZeroU8::new(1u8).unwrap(), NonZeroU8::new(254u8).unwrap(), NonZeroU8::new(179u8).unwrap()]; let mut m338: [S2; 17] = [S2 { f0: 3i32 }; 17]; let out = f91(&S1 { f0: 1.0f64, f1: vec![67108864i32, 8192i32, (-20i32), 2008358121i32, 100i32] }, &mut m337, &Vec::<i128>::new(), 0.1f32, S1 { f0: 0.0f64, f1: Vec::<i32>::new() }, 143743549657865426488149411956178614856i128, 28u64, S1 { f0: f64::INFINITY, f1: vec![15i32, 2i32, 8i32] }, &mut m338, 1isize, [S2 { f0: (-2147483648i32) }; 3], 3.25f64, 276206773i32); m337.mix(); m338.mix(); out }), S2 { f0: 13i32 }.f0); m328.mix(); m332.mix(); out })
        }));
        match r264 {
            Ok(value) => value.mix(),
            Err(caught) => payload(caught),
        }
        'l340: for x339 in ((68i8..=70i8).collect::<Vec<_>>()) {
            let mut n341 = 0u32;
            'l342: while n341 < 8 && (String::from("hello").ends_with("hello")) {
                n341 += 1;
                if let [Some(p346), p345 @ .., Some(p347)] = (({ let mut items = [Some('\u{e000}'), Some('é'), Some('\n'), Some('\n'), None::<char>, Some('é'), None::<char>, Some('Z')]; items.sort_unstable(); items })) {
                    let v348: [String; 5] = [NonZeroU8::new(8u8).unwrap(), NonZeroU8::new(15u8).unwrap(), NonZeroU8::new(1u8).unwrap(), NonZeroU8::new(2u8).unwrap(), NonZeroU8::new(25u8).unwrap()].map(|x352: NonZeroU8| -> String { [String::from("x y"), String::from("42"), String::from("ff"), String::from("a"), String::from("a")][4].clone() });
                    let f350 = move || -> [Option<String>; 3] { let v349 = v348; let out = ('b355: { if (0u64.overflowing_sub(K3).1) { break 'b355 [Some(String::from("x y")), Some(String::from("a")), Some(String::from(""))]; } (match (v349[3].clone().as_str()) { "x y" => [None::<String>, Some(String::from("0123456789")), None::<String>], _ => [Some(String::from("a,b,,c")), Some(String::from("hello")), Some(String::from("42"))] }) }); drop(v349); out };
                    apply_boxed(Box::new(f350)).mix();
                    ({ let mut m358: S1<i32, f64> = f54(K4); let out = f46(&mut m358, (!64isize), i128::from(93i8), ({ let mut m359: S1<i32, f64> = S1 { f0: (-0.0f64), f1: Vec::<i32>::new() }; let out = f46(&mut m359, (-4327406379221955881isize), 1208925819614629174706175i128, S2 { f0: 2090474922i32 }, (-20isize)); m359.mix(); out }), (f64::INFINITY as isize)); m358.mix(); out }).f0.mix();
                }
                let v360: Option<String> = ({ let mut items = vec![String::from("0123456789"), String::from("a,b,,c"), String::from("😀")]; items.pop() });
                let f362 = move || -> i8 { let v361 = v360; let out = pick::<i8>(vec![0.1f32, f32::NEG_INFINITY, 730.9705f32].contains(&0.5f32), &(unsafe { let mut slot = MaybeUninit::<i8>::uninit(); slot.write(8i8); slot.assume_init() }), &({ let t364: char = '9'; x339 })).clone(); drop(v361); out };
                apply_once(f362).mix();
            }
            hash_of(&twice::<NonZeroI64>(&NonZeroI64::new((-585362309959291123i64)).unwrap()).1).mix();
        }
        let mut n365 = 0u32;
        'l366: while n365 < 2 && ((S1 { f0: 65535.0f64, f1: vec![19i32, (-445470135i32), 1i32] } < S1 { f0: 255.5f64, f1: vec![11i32] })) {
            n365 += 1;
            {
                let (a, b) = (black_box((4294967296.0) as f64), black_box((16777216.0) as f64));
                (a + b).mix(); (a - b).mix(); (a * b).mix(); (a / b).mix(); (a % b).mix(); (-a).mix();
                if !a.is_nan() && !b.is_nan() && a != b { a.max(b).mix(); a.min(b).mix(); } a.clamp(-1.0, 1.0).mix();
                a.floor().mix(); a.ceil().mix(); a.round().mix(); a.trunc().mix(); a.fract().mix(); a.abs().mix(); a.round_ties_even().mix();
            }
            if (true) {
                'l371: for x370 in ((53u8..54u8).map(|x367: u8| -> Option<f32> { Some(f32::NAN) }).enumerate().collect::<Vec<_>>()) {
                    let [.., S2 { .. }]: [S2; 3] = Ok::<[S2; 3], Option<S2>>([S2 { f0: 8i32 }; 3]).unwrap_or_else(|x374: Option<S2>| -> [S2; 3] { <[S2; 3]>::try_from(vec![S2 { f0: 3i32 }, S2 { f0: 1048576i32 }]).unwrap_or([S2 { f0: 1234929942i32 }, S2 { f0: 0i32 }, S2 { f0: 8192i32 }]) });
                    {
                        enum Loud { I(i32, Noisy), C(char, Noisy) }
                        #[inline(never)] fn items(k: u32, last: char) -> [Loud; 3] { black_box([Loud::I(k as i32 % 4, Noisy(k)), Loud::C('q', Noisy(k + 1)), if k % 3 == 0 { Loud::I(1, Noisy(k + 2)) } else { Loud::C(last, Noisy(k + 2)) }]) }
                        #[inline(never)] fn pick0(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { [x @ Loud::C(..), Loud::C('A'..='Z', _) | Loud::I(2, _), y @ Loud::C(..)] => 1, [Loud::C('Z', _), m @ .., Loud::C('a', _)] if black_box(flag) => 2, _ => 0 } }
                        for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick0(k, last, flag) as u64); feed(0xf00d); } } }
                        #[inline(never)] fn pick1(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { [Loud::C('a'..='z', _), Loud::C('a', _) | Loud::C(_, _), Loud::C(..)] if black_box(false) => 1, [x, ..] if black_box(flag) => 2, [Loud::C(_, _), m @ ..] => 3, _ => 0 } }
                        for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick1(k, last, flag) as u64); feed(0xf00d); } } }
                        #[inline(never)] fn pick2(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { [.., Loud::C('a', _) | Loud::C(_, _)] => 1, [_, Loud::I(..), _] if black_box(!flag) => 2, [_, m @ .., Loud::C('a'..='c', _) | Loud::I(1, _)] => 3, [Loud::C('A'..='Z', _) | Loud::I(7, _), m @ ..] if black_box(false) => 4, _ => 0 } }
                        for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick2(k, last, flag) as u64); feed(0xf00d); } } }
                        #[inline(never)] fn pair(k: u32, last: char, flag: bool) -> u8 { match (items(k, last), k % 3, flag) { ([Loud::I(..3, _), ..], 1, true) => 1, ([.., Loud::I(..)], 1, _) => 2, ([x, m @ ..], 1, false) => 3, (all, _, _) if black_box(flag) => 4, _ => 0 } }
                        for k in 0..4u32 { for last in ['a', 'Z'] { for flag in [false, true] { feed(pair(k, last, flag) as u64); feed(0xf00d); } } }
                    }
                    let [p379, p378 @ .., p380]: [String; 5] = [String::from("ff"), String::from("x y"), String::from("hello"), String::from("ff"), String::from("😀")];
                }
                let mut v381: Vec<f64> = [({ let t386: i128 = (-4i128); Vec::<f64>::new() }), ({ let mut items = vec![3.25f64]; let end = items.len().min(3); items.drain(..end).collect::<Vec<f64>>() })].concat();
                v381.truncate(6);
                'l383: while let Some(x382) = v381.pop() {
                    ({ let t390: [S2; 3] = ({ let mut items = [S2 { f0: (-11i32) }, S2 { f0: 16i32 }, S2 { f0: 3i32 }]; items.rotate_left(2); items }); ({ let t395: u8 = 2u8; [S2 { f0: (-1i32) }; 17] }) }).mix();
                    let v396: i32 = S2 { f0: (-2147483648i32) }.f0;
                    let mut v397: [Option<String>; 3] = <[Option<String>; 3]>::try_from(vec![Some(String::from(""))]).unwrap_or([Some(String::from("0123456789")), Some(String::from("3.5")), None::<String>]);
                }
            }
            let mut v400: u32 = K5.checked_shl(u32::from(0u8)).unwrap_or(({ let t401: f64 = 1e10f64; K5 }));
            let mut n402 = 0u32;
            'l403: loop {
                n402 += 1;
                if n402 > 3 { break; }
                v400 = (f32::NEG_INFINITY as u32);
                let mut n405 = 0u32;
                'l406: while n405 < 2 && (([Box::new(255.5f32), Box::new(f32::INFINITY)] != [Box::new(0.5f32), Box::new(3.25f32)])) {
                    n405 += 1;
                    let mut v407: BTreeMap<[S2; 4], f64> = BTreeMap::new();
                    v407.insert(('b410: { if ((true || false)) { break 'b410 (unsafe { let mut slot = MaybeUninit::<[S2; 4]>::uninit(); slot.write([S2 { f0: 64i32 }, S2 { f0: 7i32 }, S2 { f0: 2i32 }, S2 { f0: 0i32 }]); slot.assume_init() }); } (match (String::from("line\n").as_str()) { "a" if false => [S2 { f0: 920288295i32 }, S2 { f0: 0i32 }, S2 { f0: (-2147483647i32) }, S2 { f0: 65536i32 }], _ => [S2 { f0: (-1340047876i32) }, S2 { f0: 100i32 }, S2 { f0: 1073741823i32 }, S2 { f0: 257i32 }] }) }), 255.5f64).mix();
                    v407.get(&sel::<[S2; 4]>(false, [S2 { f0: 1048577i32 }, S2 { f0: 8i32 }, S2 { f0: 1i32 }, S2 { f0: (-3i32) }], [S2 { f0: 8i32 }, S2 { f0: 437819615i32 }, S2 { f0: 16i32 }, S2 { f0: (-1i32) }])).cloned().mix();
                    v407.len().mix();
                    let [S2 { .. }, ..] = ([S2 { f0: (-6i32) }, S2 { f0: (-6i32) }, S2 { f0: (-442350648i32) }]) else { break 'l403; };
                }
            }
        }
        let r423 = catch_unwind(AssertUnwindSafe(|| -> [Option<String>; 3] {
            let r424 = catch_unwind(AssertUnwindSafe(|| -> char {
                let r425 = catch_unwind(AssertUnwindSafe(|| -> Result<Option<NonZeroI64>, S1<i32, f64>> {
                    let v426: Result<Option<NonZeroI64>, S1<i32, f64>> = Ok::<Option<NonZeroI64>, S1<i32, f64>>(NonZeroI64::new((-10i64)));
                    let f428 = move || -> [Option<i128>; 4] { let v427 = v426; let out = ({ let mut items = [[S2 { f0: (-2147483648i32) }; 3], [S2 { f0: 1i32 }; 3], [S2 { f0: 1i32 }, S2 { f0: (-12i32) }, S2 { f0: 1509241176i32 }], [S2 { f0: (-14i32) }, S2 { f0: 1073741823i32 }, S2 { f0: 67108864i32 }]].map(|x434: [S2; 3]| -> Option<i128> { None::<i128> }); items.swap(0, 3); items }); drop(v427); out };
                    apply_once(f428).mix();
                    Ok::<Option<NonZeroI64>, S1<i32, f64>>(NonZeroI64::new((0.5f64 as i64)))
                }));
                match r425 {
                    Ok(value) => value.mix(),
                    Err(caught) => payload(caught),
                }
                char::from_digit(((-9223372036854775808.0f64) as u32) % 36, 36).unwrap_or('é'.to_lowercase().next().unwrap_or('9'))
            }));
            match r424 {
                Ok(value) => value.mix(),
                Err(caught) => payload(caught),
            }
            let mut n437 = 0u32;
            'l438: while n437 < 2 && ((false && true)) {
                n437 += 1;
                [Err::<char, S1<i32, f64>>(S1 { f0: f64::MIN_POSITIVE, f1: vec![7i32] }), Ok::<char, S1<i32, f64>>('0')].map(|x440: Result<char, S1<i32, f64>>| -> Box<f32> { Box::new((-1.0f32)) }).mix();
                let mut n441 = 0u32;
                'l442: while n441 < 3 && (({ let t443: u32 = K5; true })) {
                    n441 += 1;
                    let [S2 { f0: _ }, p454 @ .., p455]: [S2; 4] = (if let Some(x446) = (({ let mut items = ({ let mut items = Vec::<[S2; 4]>::new(); items.insert(0, [S2 { f0: 2i32 }, S2 { f0: 18i32 }, S2 { f0: 256i32 }, S2 { f0: (-2147483648i32) }]); items }); items.pop() })) { x446 } else { ({ let mut items = ({ let t453: [Option<String>; 3] = [Some(String::from("😀")), Some(String::from("line\n")), None::<String>]; [S2 { f0: 2147483647i32 }, S2 { f0: 2147483647i32 }, S2 { f0: 131071i32 }, S2 { f0: 248561511i32 }] }); items.swap(0, 3); items }) });
                    (1u8 as char).mix();
                }
                let r456 = catch_unwind(AssertUnwindSafe(|| -> u8 {
                    K4.max((if (twice::<bool>(&true).0) { panic!("gave up at {}", K5.overflowing_shl(3u32).0) } else { (-17i32).checked_mul(K4).expect("out of range") })).mix();
                    let v457: Result<Option<NonZeroI64>, S1<i32, f64>> = (match (Vec::<S2>::new().as_slice()) { [S2 { .. }, ..] => ('b464: { if (false) { break 'b464 Err::<Option<NonZeroI64>, S1<i32, f64>>(S1 { f0: 255.5f64, f1: Vec::<i32>::new() }); } Ok::<Option<NonZeroI64>, S1<i32, f64>>(Some(NonZeroI64::new(16777216i64).unwrap())) }), _ => ('b466: { if (false) { break 'b466 Err::<Option<NonZeroI64>, S1<i32, f64>>(S1 { f0: 0.2f64, f1: Vec::<i32>::new() }); } Ok::<Option<NonZeroI64>, S1<i32, f64>>(None::<NonZeroI64>) }) });
                    let f459 = move || -> [S2; 2] { let v458 = v457; let out = [twice::<S2>(&S2 { f0: 2i32 }).1, S2 { f0: K4 }]; drop(v458); out };
                    f459().mix();
                    ('b469: { if ((S2 { f0: (-1891249139i32) } > S2 { f0: 1839534011i32 })) { break 'b469 8u8.checked_div(33u8).unwrap_or(8u8); } 2u8.overflowing_add(255u8).0 })
                }));
                match r456 {
                    Ok(value) => value.mix(),
                    Err(caught) => payload(caught),
                }
                (match (Ok::<char, S1<i32, f64>>('a')) { p471 => ({ let t473: Option<Option<f32>> = Some([Box::new(f32::MAX), Box::new((-596.6809f32))]).map(|x474: [Box<f32>; 2]| -> Option<f32> { Some(1e30f32) }); ({ let t476: S2 = S2 { f0: 16i32 }; Ok::<Option<NonZeroI64>, S1<i32, f64>>(Some(NonZeroI64::new((-2158774487764455985i64)).unwrap())) }) }), Ok(p477) if true => ({ let t479: u8 = (3u8 / 20u8); Ok::<Option<NonZeroI64>, S1<i32, f64>>(Some(NonZeroI64::new(8388607i64).unwrap())) }), _ => twice::<Result<Option<NonZeroI64>, S1<i32, f64>>>(&('b483: { if (false) { break 'b483 Ok::<Option<NonZeroI64>, S1<i32, f64>>(None::<NonZeroI64>); } Err::<Option<NonZeroI64>, S1<i32, f64>>(S1 { f0: f64::MIN_POSITIVE, f1: Vec::<i32>::new() }) })).0 }).mix();
            }
            let v484: bool = (match ([S2 { f0: 255i32 }; 2]) { [S2 { f0: p485 }, ..] => String::from("42"), [_, .., S2 { f0: 3i32 }] => String::from("-7"), _ => String::from("a") }).ends_with("  pad  ");
            assert!(v484);
            [None::<String>, None::<String>, Some(String::from("3.5"))]
        }));
        match r423 {
            Ok(value) => value.mix(),
            Err(caught) => payload(caught),
        }
        if let "42" = ((String::from("42") + &String::from("  pad  ")).as_str()) {
            ('b495: { if (false) { break 'b495 NonZeroI64::new((-9i64)).unwrap(); } NonZeroI64::new((-7959755647389598133i64)).unwrap() }).mix();
            let mut n496 = 0u32;
            'l497: while n496 < 8 && ((-0.0f32).is_normal()) {
                n496 += 1;
                let r498 = catch_unwind(AssertUnwindSafe(|| -> [Option<String>; 3] {
                    Some(S2 { f0: (-11i32) }).mix();
                    (unsafe { let mut slot = MaybeUninit::<[Option<String>; 3]>::uninit(); slot.write((match (['é', '中'].map(|x505: char| -> Box<f32> { Box::new(0.5f32) })) { [.., _] => ({ let mut items = [Some(String::from("a,b,,c")), Some(String::from("  pad  ")), None::<String>]; items.reverse(); items }), _ => (unsafe { let mut slot = MaybeUninit::<[Option<String>; 3]>::uninit(); slot.write([Some(String::from("x y")), Some(String::from("line\n")), Some(String::from("42"))]); slot.assume_init() }) })); slot.assume_init() })
                }));
                match r498 {
                    Ok(value) => value.mix(),
                    Err(caught) => payload(caught),
                }
                if ((true || ([S2 { f0: (-1659068617i32) }; 2] == [S2 { f0: 7i32 }, S2 { f0: (-1959796689i32) }]))) {
                    (18isize.overflowing_add(9223372036854775806isize).1 as isize).checked_sub((-8isize).wrapping_rem(4294967296isize | 1)).unwrap_or(isize::try_from(K3).unwrap_or(3isize)).mix();
                    let v510: [S2; 4] = (match ([Box::new(0.5f32), Box::new(4294967296.0f32)]) { [p517, p518] => ({ let t520: NonZeroU8 = NonZeroU8::new(219u8).unwrap(); Some([S2 { f0: 1647639891i32 }; 4]) }), _ => Ok::<[S2; 4], i8>([S2 { f0: (-18i32) }; 4]).ok() }).unwrap_or_else(|| -> [S2; 4] { [S2 { f0: (-1638157404i32) }; 4] });
                    {
                        #[cfg(target_arch = "x86_64")] #[inline(never)] fn mul3(mut x: u64, mut y: u64) -> u64 { unsafe { std::arch::asm!("lea {x}, [{x} + {x} * 2]", x = inout(reg) x, options(nostack)); } x }
                        #[cfg(target_arch = "aarch64")] #[inline(never)] fn mul3(mut x: u64, mut y: u64) -> u64 { unsafe { std::arch::asm!("add {x}, {x}, {x}, lsl #1", x = inout(reg) x, options(nostack)); } x }
                        for (x, y) in [(43u64, 60u64), (u64::MAX, 1), (0, 0), (0x8000_0000_0000_0000, 43)] { let _g = Noisy(x as u32 % 7); feed(mul3(black_box(x), black_box(y))); }
                    }
                } else {
                    let v522: f64 = 16777217.0f64.clamp(-1.5f64, 2.5f64);
                    let Some(p524) = (None::<String>) else { break 'l497; };
                }
            }
        }
        if let "42" = (({ let t525: u64 = 3u64; String::from("  pad  ") }).as_str()) {
            let mut v526: BTreeMap<NonZeroU8, S2> = BTreeMap::new();
            v526.insert(('b527: { if ((-128.9f32).is_normal()) { break 'b527 NonZeroU8::new(30u8).unwrap_or(NonZeroU8::new(15u8).unwrap()); } NonZeroU8::new(127u8).unwrap_or(NonZeroU8::new(6u8).unwrap()) }), ({ let mut m528: S1<i32, f64> = (if (false) { S1 { f0: 16777217.0f64, f1: vec![(-1569772028i32), (-2147483647i32), 0i32] } } else { S1 { f0: 255.5f64, f1: Vec::<i32>::new() } }); let out = f46(&mut m528, (-19isize).max(2isize), (65535.0f32 as i128), ({ let mut m529: S1<i32, f64> = S1 { f0: 16777217.0f64, f1: Vec::<i32>::new() }; let out = f46(&mut m529, 274877906943isize, 41538374868278621028243970633760768i128, S2 { f0: 7i32 }, (-16isize)); m529.mix(); out }), (f32::MIN as isize)); m528.mix(); out })).mix();
            v526.get(&('b531: { if (true) { break 'b531 NonZeroU8::new(3u8).unwrap(); } NonZeroU8::new(15u8).unwrap() })).cloned().mix();
            v526.len().mix();
            let mut v532: u64 = twice::<u64>(&2u64).1.checked_next_power_of_two().unwrap_or(K3.saturating_add(K3)).wrapping_rem_euclid((match (f32::EPSILON) { p533 => K3, p534 if false => K3, p535 => 748874441801909055u64, _ => 39u64 }).checked_neg().unwrap_or_default() | 1);
        } else {
            {
                let f536 = || -> [S1<i32, f64>; 3] { [S1 { f0: 255.5f64, f1: vec![16385i32, 131072i32] }, S1 { f0: (-0.0f64), f1: vec![338969258i32, 100i32] }, S1 { f0: 255.5f64, f1: Vec::<i32>::new() }] };
                let mut n539 = 0u32;
                'l540: while n539 < 8 && (2i32.overflowing_mul(K4).1) {
                    n539 += 1;
                    {
                        for armed in [false, true] { let n = Noisy(1); let once = move || { let n = n; trip(armed, ()); drop(n); }; caught(once); }
                    }
                }
                {
                    K3.overflowing_shr(K5).0.overflowing_add((K3.abs_diff(10815594528155571220u64) as u64)).0.mix();
                    let mut v541: BTreeMap<[Option<i128>; 4], S1<i32, f64>> = BTreeMap::new();
                    v541.insert(({ let t544: u128 = (unsafe { std::mem::transmute::<i64, i64>(131072i64) } as u128); [Some(281474976710656i128), Some(524288i128), Some(9i128), None::<i128>] }), S1 { f0: ({ let t547: [S2; 3] = [S2 { f0: 752957244i32 }; 3]; 255.5f64 }), f1: vec![2147483647i32, (-2147483648i32), 9i32, 262144i32, (-2147483647i32)] }).mix();
                    v541.insert([Some(2i128), Some(14i128), None::<i128>, None::<i128>], S1 { f0: f64::EPSILON, f1: vec![240081654i32, 2i32, 8i32] }).mix();
                    v541.insert(twice::<[Option<i128>; 4]>(&(if let Some(x555) = (None::<[Option<i128>; 4]>) { x555 } else { [Some(2147483647i128); 4] })).1, f54(16385i32)).mix();
                    v541.insert(('b558: { if (None::<bool>.unwrap_or_else(|| -> bool { false })) { break 'b558 [Some(4611686018427387903i128), Some(85070591730234615865843651857942052863i128), Some((-18i128)), Some((-2i128))]; } Some([Some(8i128), Some(170141183460469231731687303715884105727i128), None::<i128>, None::<i128>]).unwrap_or([None::<i128>, Some(6i128), None::<i128>, Some((-109240613504419080673079398930580179279i128))]) }), S1 { f0: ({ let mut m563: Vec<NonZeroU8> = vec![NonZeroU8::new(7u8).unwrap(), NonZeroU8::new(100u8).unwrap()]; let mut m564: [S2; 17] = [S2 { f0: (-5i32) }, S2 { f0: (-1807788390i32) }, S2 { f0: 2147483646i32 }, S2 { f0: (-2119027076i32) }, S2 { f0: 727473467i32 }, S2 { f0: 1514597399i32 }, S2 { f0: 8192i32 }, S2 { f0: 18i32 }, S2 { f0: (-1i32) }, S2 { f0: (-1i32) }, S2 { f0: 2i32 }, S2 { f0: 17i32 }, S2 { f0: 4194305i32 }, S2 { f0: 1073741823i32 }, S2 { f0: 16i32 }, S2 { f0: 18i32 }, S2 { f0: 2147483646i32 }]; let out = f91(&K5, &mut m563, &Vec::<i128>::new(), f32::INFINITY, S1 { f0: (-128.9f64), f1: Vec::<i32>::new() }, 0i128, K3, S1 { f0: 65535.0f64, f1: vec![3i32] }, &mut m564, (-9223372036854775808isize), [S2 { f0: (-4i32) }; 3], 0.0f64, 12i32); m563.mix(); m564.mix(); out }), f1: ({ let mut items = vec![(-1809402712i32), (-2147483647i32)]; items.sort_by(|a, b| b.cmp(a)); items }) }).mix();
                    for (key, value) in v541.iter().rev() { key.mix(); value.mix(); }
                    v541.len().mix();
                }
                f536().mix();
            }
        }
        {
            let f571 = |a570: u64| -> Vec<f32> { vec![0.1f32, 1.5f32, f32::MIN_POSITIVE] };
            let v572: [S2; 17] = ({ let mut items = twice::<[S2; 17]>(&[S2 { f0: (-10i32) }, S2 { f0: 2097152i32 }, S2 { f0: 16i32 }, S2 { f0: 128i32 }, S2 { f0: 25800305i32 }, S2 { f0: 19i32 }, S2 { f0: 1073741823i32 }, S2 { f0: (-303621791i32) }, S2 { f0: 2i32 }, S2 { f0: 1331306346i32 }, S2 { f0: 497056223i32 }, S2 { f0: 8192i32 }, S2 { f0: 16777215i32 }, S2 { f0: 67108863i32 }, S2 { f0: (-5i32) }, S2 { f0: 100i32 }, S2 { f0: 8i32 }]).1; items.rotate_left(2); items });
            let mut v577: Vec<[String; 5]> = vec![[String::from("😀"), String::from("hello"), String::from("héllo"), String::from("a,b,,c"), String::from("line\n")], [String::from("x y"), String::from("-7"), String::from("3.5"), String::from("x y"), String::from("42")], [String::from("42"), String::from("3.5"), String::from("a"), String::from("  pad  "), String::from("-7")], [String::from("Abc Def"), String::from("-7"), String::from("x y"), String::from("ff"), String::from("42")], [String::from(""), String::from("x y"), String::from("  pad  "), String::from("😀"), String::from("héllo")]];
            v577.truncate(6);
            'l579: while let Some(x578) = v577.pop() {
                let mut n580 = 0u32;
                'l581: while n580 < 5 && (({ let t582: NonZeroU8 = NonZeroU8::new(126u8).unwrap(); false })) {
                    n580 += 1;
                    {
                        enum Loud { I(i32, Noisy), C(char, Noisy) }
                        #[inline(never)] fn items(k: u32, last: char) -> [Loud; 3] { black_box([Loud::I(k as i32 % 4, Noisy(k)), Loud::C('q', Noisy(k + 1)), if k % 3 == 0 { Loud::I(1, Noisy(k + 2)) } else { Loud::C(last, Noisy(k + 2)) }]) }
                        #[inline(never)] fn pick0(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { [.., Loud::C('a', _)] => 1, [_, m @ .., Loud::C('a', _) | Loud::C(_, _)] if black_box(flag) => 2, [Loud::I(2, _), m @ ..] if black_box(false) => 3, _ => 0 } }
                        for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick0(k, last, flag) as u64); feed(0xf00d); } } }
                        #[inline(never)] fn pick1(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { [Loud::C('a'..='c', _) | Loud::I(1, _), Loud::C(..), Loud::C(..)] if black_box(false) => 1, [Loud::C('a'..='z', _), Loud::C('a', _) | Loud::C(_, _), _] if black_box(false) => 2, _ => 0 } }
                        for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick1(k, last, flag) as u64); feed(0xf00d); } } }
                        #[inline(never)] fn pair(k: u32, last: char, flag: bool) -> u8 { match (items(k, last), k % 3, flag) { ([Loud::C('a', _) | Loud::C(_, _), ..], 3, true) => 1, ([.., Loud::I(..)], 1, _) => 2, ([x, m @ ..], 3, false) => 3, (all, _, _) if black_box(flag) => 4, _ => 0 } }
                        for k in 0..4u32 { for last in ['a', 'Z'] { for flag in [false, true] { feed(pair(k, last, flag) as u64); feed(0xf00d); } } }
                    }
                    S2 { f0: 8i32 }.f0.mix();
                    {
                        #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(C)] struct Plain { a: u8, b: u32, c: u16, d: u8 }
                        #[derive(Clone, Copy, Default)] #[repr(C, packed)] struct Tight { a: u8, b: u64, c: u16, d: u128 }
                        #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(C, align(2))] struct Big { a: u8, b: [u16; 3] }
                        #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(align(2))] struct Rusty(u8, u32);
                        #[derive(Clone, Copy, Debug, PartialEq)] #[repr(transparent)] struct Wrap(f32);
                        #[derive(Clone, Copy, Debug, PartialEq)] #[repr(i8)] enum Signed { Low = -128, Mid = -1, High = 127 }
                        #[derive(Clone, Copy, Debug, PartialEq)] #[repr(u16)] enum Wider { A = 1, B = 0x1234, C = 0xffff }
                        #[derive(Clone, Copy, Debug, PartialEq)] #[repr(C)] enum Cee { Unit, One(u8), Two { x: u16, y: u64 } }
                        #[derive(Clone, Copy, Debug, PartialEq)] #[repr(C, u8)] enum Both { P(u32), Q(u8, u8), R }
                        #[derive(Clone, Copy)] #[repr(C)] union Bits { word: u32, bytes: [u8; 4], half: (u16, u16) }
                        #[inline(never)] extern "C" fn c_plain(p: Plain, t: Tight, w: Wrap) -> Plain { Plain { a: p.a.wrapping_add(t.a), b: p.b ^ { t.c } as u32, c: p.c, d: p.d } }
                        #[inline(never)] extern "C" fn c_big(x: u8, big: Big, y: u8, rusty: Rusty) -> Big { Big { a: big.a ^ x ^ y ^ rusty.0, b: [big.b[2], big.b[0], rusty.1 as u16] } }
                        #[inline(never)] fn r_enums(s: Signed, w: Wider, c: Cee, b: Both) -> (i8, u16, Cee, Both) { (s as i8, w as u16, match c { Cee::One(v) => Cee::Two { x: v as u16, y: 1 }, other => other }, match b { Both::Q(x, y) => Both::Q(y, x), other => other }) }
                        feed((std::mem::offset_of!(Plain, b) + std::mem::offset_of!(Plain, c) * 16 + std::mem::offset_of!(Plain, d) * 256 + std::mem::offset_of!(Tight, d) * 4096 + std::mem::offset_of!(Big, b) * 65536) as u64);
                        { let w = black_box(Wrap(2.5)); let raw: f32 = unsafe { std::mem::transmute(w) }; raw.mix(); let items = black_box([Rusty(1, 2), Rusty(3, 4)]); feed(items[1].1 as u64 + size_of_val(&items) as u64 * 16); }
                        feed((size_of::<Plain>() + align_of::<Plain>() * 256 + size_of::<Tight>() * 4096 + align_of::<Tight>() * 65536) as u64); feed((size_of::<Big>() + align_of::<Big>() * 256 + size_of::<Rusty>() * 4096 + size_of::<Cee>() * 65536 + size_of::<Both>() * 1048576) as u64);
                    }
                }
                let v583: u32 = u32::try_from(K4).unwrap_or(8388608u32);
                {
                    #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(C)] struct Plain { a: u8, b: u32, c: u16, d: f64 }
                    #[derive(Clone, Copy, Default)] #[repr(C, packed(4))] struct Tight { a: u8, b: u64, c: u16, d: u128 }
                    #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(C, align(8))] struct Big { a: u8, b: [u16; 3] }
                    #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(align(8))] struct Rusty(u8, u32);
                    #[derive(Clone, Copy, Debug, PartialEq)] #[repr(transparent)] struct Wrap(f32);
                    #[derive(Clone, Copy, Debug, PartialEq)] #[repr(i8)] enum Signed { Low = -128, Mid = -1, High = 127 }
                    #[derive(Clone, Copy, Debug, PartialEq)] #[repr(u16)] enum Wider { A = 1, B = 0x1234, C = 0xffff }
                    #[derive(Clone, Copy, Debug, PartialEq)] #[repr(C)] enum Cee { Unit, One(u8), Two { x: u16, y: u64 } }
                    #[derive(Clone, Copy, Debug, PartialEq)] #[repr(C, u8)] enum Both { P(u32), Q(u8, u8), R }
                    #[derive(Clone, Copy)] #[repr(C)] union Bits { word: u32, bytes: [u8; 4], half: (u16, u16) }
                    #[inline(never)] extern "C" fn c_plain(p: Plain, t: Tight, w: Wrap) -> Plain { Plain { a: p.a.wrapping_add(t.a), b: p.b ^ { t.c } as u32, c: p.c, d: p.d } }
                    #[inline(never)] extern "C" fn c_big(x: u8, big: Big, y: u8, rusty: Rusty) -> Big { Big { a: big.a ^ x ^ y ^ rusty.0, b: [big.b[2], big.b[0], rusty.1 as u16] } }
                    #[inline(never)] fn r_enums(s: Signed, w: Wider, c: Cee, b: Both) -> (i8, u16, Cee, Both) { (s as i8, w as u16, match c { Cee::One(v) => Cee::Two { x: v as u16, y: 1 }, other => other }, match b { Both::Q(x, y) => Both::Q(y, x), other => other }) }
                    { let big = c_big(1, black_box(Big { a: 1, b: [2, 3, 4] }), 13, black_box(Rusty(5, 6))); big.a.mix(); big.b.mix(); feed((&big as *const Big as usize % 8) as u64); }
                    { for s in black_box([Signed::Low, Signed::Mid, Signed::High]) { feed((s as i8 as i64 + 200) as u64); feed((s as i32 == -1) as u64); } for w in black_box([Wider::A, Wider::B, Wider::C]) { feed(w as u64); } }
                    { let p = c_plain(black_box(Plain { a: 1, b: 13, c: 3, ..Default::default() }), black_box(Tight { a: 1, b: 2, c: 1, d: 4 }), Wrap(1.5)); feed(p.a as u64 + p.b as u64 * 256 + p.c as u64 * 65536); }
                    { let w = black_box(Wrap(2.5)); let raw: f32 = unsafe { std::mem::transmute(w) }; raw.mix(); let items = black_box([Rusty(1, 2), Rusty(3, 4)]); feed(items[1].1 as u64 + size_of_val(&items) as u64 * 16); }
                    { let both = black_box([Both::P(7), Both::Q(1, 2), Both::R]); for it in both { feed(unsafe { *(&it as *const Both as *const u8) } as u64); feed(match it { Both::P(v) => v as u64, Both::Q(x, y) => (x + y) as u64, Both::R => 99 }); } }
                    { let mut bits = Bits { word: black_box(1u32.wrapping_mul(0x01010101)) }; unsafe { bits.bytes[0] ^= 0xff; feed(bits.word.to_le() as u64); feed(u32::from_le_bytes(bits.bytes) as u64); bits.half = (1, 2); feed(u32::from_le(bits.word) as u64); } }
                }
            }
            f571((unsafe { Bytes4 { word: K5 }.signed } as u64)).mix();
        }
        let mut n584 = 0u32;
        'l585: while n584 < 3 && (matches!(K3, (0u64))) {
            n584 += 1;
            let mut n586 = 0u32;
            'l587: while n586 < 3 && (None::<[S2; 17]>.is_some()) {
                n586 += 1;
                let p590: [S2; 3] = [S2 { f0: 1i32 }, S2 { f0: 17i32 }, S2 { f0: 4i32 }];
                {
                    let f592 = move |a591: S2| -> [S2; 4] { (unsafe { let mut slot = MaybeUninit::<[S2; 4]>::uninit(); slot.write([S2 { f0: (-2i32) }, S2 { f0: 18i32 }, S2 { f0: 512i32 }, S2 { f0: 8i32 }]); slot.assume_init() }) };
                    let mut v597: u64 = K3.overflowing_mul(K3).0;
                    let mut v598: NonZeroI64 = NonZeroI64::new(i64::try_from(1u16).unwrap_or(3i64)).unwrap_or_else(|| -> NonZeroI64 { (if ((4294967296.0f64 == 440.8972f64)) { NonZeroI64::new((-3957537824938777796i64)).unwrap_or(NonZeroI64::new((-9223372036854775807i64)).unwrap()) } else { (match (vec![String::from("42"), String::from("héllo"), String::from("hello")].as_slice()) { [p601 @ .., p602] => NonZeroI64::new(6i64).unwrap(), _ => NonZeroI64::new(8i64).unwrap() }) }) });
                    ({ let t603: [S1<i32, f64>; 2] = [S1 { f0: (-9223372036854775808.0f64), f1: vec![100i32, (-18i32)] }, S1 { f0: 1e30f64, f1: vec![2147483647i32, 19i32, 16i32] }]; K4 }).wrapping_shr(({ let t604: Option<S2> = None::<S2>; K5 })).mix();
                    f592(p590[0]).mix();
                }
                (match (3i8.overflowing_shl(39u32).0) { 10i8 => (match (Some(S2 { f0: 1073741824i32 })) { Some(S2 { f0: -894962378i32 }) => [S2 { f0: (-891350329i32) }; 17], None => [S2 { f0: 2i32 }, S2 { f0: (-19i32) }, S2 { f0: (-2147483648i32) }, S2 { f0: (-2147483647i32) }, S2 { f0: (-18i32) }, S2 { f0: 1073741823i32 }, S2 { f0: 1048575i32 }, S2 { f0: 3i32 }, S2 { f0: (-1i32) }, S2 { f0: (-16i32) }, S2 { f0: 537365584i32 }, S2 { f0: (-5i32) }, S2 { f0: 1660557348i32 }, S2 { f0: 13i32 }, S2 { f0: (-11i32) }, S2 { f0: 33554432i32 }, S2 { f0: 2147483646i32 }], _ => [S2 { f0: (-5i32) }; 17] }), _ => <[S2; 17]>::try_from(vec![S2 { f0: 159438242i32 }, S2 { f0: 2048i32 }, S2 { f0: 7i32 }]).unwrap_or([S2 { f0: 2i32 }, S2 { f0: (-1639333862i32) }, S2 { f0: 3i32 }, S2 { f0: (-12i32) }, S2 { f0: 1073741823i32 }, S2 { f0: 2008396436i32 }, S2 { f0: (-15i32) }, S2 { f0: 65536i32 }, S2 { f0: (-593806487i32) }, S2 { f0: 160079032i32 }, S2 { f0: (-741795252i32) }, S2 { f0: (-10i32) }, S2 { f0: 3i32 }, S2 { f0: 10i32 }, S2 { f0: (-983748131i32) }, S2 { f0: (-16i32) }, S2 { f0: 134217728i32 }]) }).mix();
                let mut n611 = 0u32;
                'l612: while n611 < 5 && (sel::<bool>(true, true, true)) {
                    n611 += 1;
                    ('b613: { if (true) { break 'b613 4194304isize; } 18isize }).mix();
                    let v614: S1<i32, f64> = f54(2i32);
                    let f616 = move || -> i128 { let v615 = v614; let out = 92081887130444098094180862360230268903i128; drop(v615); out };
                    f616().mix();
                }
            }
        }
        let mut v617: BTreeMap<NonZeroU8, S2> = BTreeMap::new();
        v617.insert(NonZeroU8::new(1u8).unwrap(), ({ let mut m618: S1<i32, f64> = f54(K4); let out = f46(&mut m618, (if (true) { (-1isize) } else { 4611686018427387903isize }), (-15i128).checked_shl(K5).unwrap_or((-1896996333160026166263137585150543117i128)), ({ let mut m619: S1<i32, f64> = S1 { f0: 1e10f64, f1: vec![(-196388197i32), 209971143i32] }; let out = f46(&mut m619, 142290198562363853isize, 17592186044416i128, S2 { f0: (-2147483647i32) }, 2048isize); m619.mix(); out }), ((-4isize).overflowing_add(7isize).1 as isize)); m618.mix(); out })).mix();
        v617.remove(&({ let t620: S1<i32, f64> = S1 { f0: 1e10f64, f1: vec![100i32, (-9i32), (-16i32), 16i32, (-4i32)] }; NonZeroU8::new(23u8).unwrap() })).mix();
        v617.len().mix();
        let r622 = catch_unwind(AssertUnwindSafe(|| -> f32 {
            if let [Some('9'), p625 @ .., None] = ([37778931862957161709569u128, 0u128, 340282366920938463463374607431768211454u128, 6u128, 7u128, 340282366920938463463374607431768211454u128, 7u128, 2u128].map(|x624: u128| -> Option<char> { Some('0') })) {
                {
                    async fn leaf(x: u64) -> u64 { let _g = Noisy(x as u32 % 9); later((x % 3) as u32, x + 28).await }
                    async fn work(seed: u32) -> u64 { let h626 = (seed as f64 * 0.5, seed as u8, seed % 3 == 0); let w627 = later(1, seed as u64 + 52).await; let h628 = Noisy(seed % 9 + 1); let mut w629 = 0u64; for i in 0..1u64 { let _g = Noisy(i as u32); w629 += later((i % 2) as u32, i).await; } let h630: Option<Box<u16>> = if seed % 2 == 0 { Some(Box::new(41)) } else { None }; let w631 = later(0, [seed as u64; 2]).await[0]; (h626.0 as u64 + h626.1 as u64 + h626.2 as u64).rotate_left(0) ^ (w627).rotate_left(1) ^ (h628.0 as u64).rotate_left(2) ^ (w629).rotate_left(3) ^ (h630.as_deref().copied().unwrap_or(1) as u64).rotate_left(4) ^ (w631).rotate_left(5) }
                    async fn down(n: u32) -> u64 { if n == 0 { leaf(1).await } else { Box::pin(down(n - 1)).await + leaf(n as u64).await } }
                    async fn outer(seed: u32) -> u64 { let n = Noisy(seed); let a = leaf(seed as u64).await; let (b, c) = both(leaf(a), work(seed)).await; let d = async { let _in = Noisy(22); leaf(b ^ c).await }.await; a ^ b ^ c ^ d ^ n.0 as u64 }
                    { let add = async |x: u64| later(1, x).await + 22; block_on(add(3)).mix(); }
                    block_on(outer(23)).mix();
                    poll_some(outer(22), 8).mix();
                }
            }
            f75(10i32.overflowing_sub(4095i32).0, i32::from(16383i16), (-7.0f64), ((-327.3038f64) as u64))
        }));
        match r622 {
            Ok(value) => value.mix(),
            Err(caught) => payload(caught),
        }
        let g632: extern "C" fn(i32, i32, f64, u64) -> f32 = f75;
        let r633 = catch_unwind(AssertUnwindSafe(|| -> S2 {
            if let p634 = (String::from("héllo").as_str()) {
                let g635: fn(i32) -> S1<i32, f64> = (|a636: i32| -> S1<i32, f64> { sel::<S1<i32, f64>>(String::from("héllo").ends_with("ff"), S1 { f0: 4294967296.0f64, f1: Vec::<i32>::new() }, S1 { f0: f64::MIN, f1: vec![10i32, 11i32, 9i32, 2094106963i32, 222088195i32] }) });
            } else {
                (({ let mut m637: Vec<NonZeroU8> = vec![NonZeroU8::new(197u8).unwrap()]; let mut m638: [S2; 17] = [S2 { f0: (-3i32) }; 17]; let out = f91(&K5, &mut m637, &vec![(-164907963058513792885014664903473228322i128), 9i128], f32::MIN_POSITIVE, S1 { f0: (-128.9f64), f1: vec![(-2147483647i32), (-1035524981i32), (-1283134250i32), 0i32, (-1821673732i32)] }, 7i128, K3, S1 { f0: 4294967296.0f64, f1: vec![524289i32] }, &mut m638, 4294967297isize, [S2 { f0: (-1i32) }, S2 { f0: 74824133i32 }, S2 { f0: 536870911i32 }], f64::MAX, K4); m637.mix(); m638.mix(); out }) as u64).overflowing_sub(K3.overflowing_mul(18446744073709551615u64).0.overflowing_shr(u32::try_from((-2694606749656702535isize)).unwrap()).0).0.mix();
            }
            u64::from(K5).mix();
            ({ let mut m639: S1<i32, f64> = f54(K4); let out = f46(&mut m639, 1073741824isize.overflowing_sub(14isize).0, 2i128, ({ let mut m640: S1<i32, f64> = S1 { f0: f64::NEG_INFINITY, f1: vec![513i32, (-1911448561i32), (-196756478i32), (-1i32), (-5i32)] }; let out = f46(&mut m640, (-5029281535278385063isize), 2i128, S2 { f0: 100i32 }, (-16isize)); m640.mix(); out }), isize::try_from(46714271574275544883744117494193319934u128).unwrap_or((-1isize))); m639.mix(); out })
        }));
        match r633 {
            Ok(value) => value.mix(),
            Err(caught) => payload(caught),
        }
        String::from_utf8_lossy(&({ let mut items = vec![127u8]; items.insert(0, 0u8); items })).into_owned().mix();
    }
}

mod c4 {
    use super::*;
    #[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord, Hash)]
    pub struct S1 { pub f0: char }
    impl Mix for S1 {
        fn mix(&self) {
            feed(1);
            self.f0.mix();
        }
    }
    impl Drop for S1 {
        fn drop(&mut self) {
            feed(0x92694393);
            self.mix();
        }
    }
    #[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord, Hash)]
    pub enum E2 {
        V0(String, String, String),
        V1(char),
        V2,
    }
    impl Mix for E2 {
        fn mix(&self) {
            match self {
                E2::V0(a0, a1, a2) => { feed(10); a0.mix(); a1.mix(); a2.mix(); }
                E2::V1(a0) => { feed(11); a0.mix(); }
                E2::V2 => { feed(12); }
            }
        }
    }
    #[derive(Clone, Debug, PartialEq, PartialOrd)]
    pub enum E3 {
        V0 { f0: String },
        V1(i128),
        V2(f32, char),
        V3(S1, f64, String),
        V4(f32),
    }
    impl Mix for E3 {
        fn mix(&self) {
            match self {
                E3::V0 { f0: a0 } => { feed(10); a0.mix(); }
                E3::V1(a0) => { feed(11); a0.mix(); }
                E3::V2(a0, a1) => { feed(12); a0.mix(); a1.mix(); }
                E3::V3(a0, a1, a2) => { feed(13); a0.mix(); a1.mix(); a2.mix(); }
                E3::V4(a0) => { feed(14); a0.mix(); }
            }
        }
    }
    pub fn f4(a5: &S1, a6: &S1) -> String {
        let mut v7: String = String::from_utf8_lossy(&(match (E3::V3(S1 { f0: '_' }, 0.2f64, String::from("Abc Def"))) { E3::V0 { f0: p9 } => vec![64u8, 127u8], E3::V1(_) => vec![197u8, 8u8], E3::V2(p10, _) => vec![255u8], E3::V3(p11, p12, p13) => vec![1u8, 20u8, 1u8, 10u8, 8u8], E3::V4(_) => vec![65u8, 255u8, 13u8, 5u8, 100u8] })).into_owned();
        None::<String>.filter(|x16: &String| -> bool { false }).ok_or(17i128.wrapping_abs()).unwrap_or_else(|x14: i128| -> String { format!("{:10.2}|", (match (vec![String::from("-7"), String::from("-7"), String::from("")].as_slice()) { [p18] if false => f64::NAN, _ => 65535.0f64 })) })
    }
    pub fn f19(a20: &mut [S1]) -> u128 {
        { let value = (unsafe { let mut slot = MaybeUninit::<S1>::uninit(); slot.write((if (true) { S1 { f0: '\u{80}' } } else { S1 { f0: '\u{80}' } })); slot.assume_init() }); if let Some(slot) = a20.first_mut() { *slot = value; } }
        let v21: (Box<f64>,) = (if (true) { (Box::new(4294967296.0f64),) } else { (Box::new(f64::MIN_POSITIVE),) });
        let f23 = move || -> f32 { let v22 = v21; let out = Some(f32::MAX).ok_or(String::from("  pad  ")).unwrap_or_else(|x24: String| -> f32 { ('b26: { if (false) { break 'b26 (-2.5e-3f32); } (-772.6686f32) }) }); drop(v22); out };
        apply_boxed(Box::new(f23)).mix();
        (match (E3::V0 { f0: String::from("-7") }) { E3::V0 { f0: p27 } => 144115188075855873u128, E3::V1(p28) => 0u128, E3::V2(p29, p30) => 126143039215705512621611321656368247232u128, E3::V3(S1 { .. }, _, p31) => 302231454903657293676544u128, E3::V4(p32) => 1073741823u128 })
    }
    #[inline(never)]
    pub fn f33(a34: &String, a35: &S1, a36: &mut f32, a37: &(Box<f64>,), a38: &[(f32, f64, i128, f32, i128, f32); 2], a39: &String, a40: &E3, a41: &str) -> [(f32, f64, i128, f32, i128, f32); 2] {
        {
            let f43 = |a42: f32| -> (Box<f64>,) { (if ((if (true) { true } else { false })) { (unsafe { let mut slot = MaybeUninit::<(Box<f64>,)>::uninit(); slot.write((Box::new(973.3260f64),)); slot.assume_init() }) } else { Ok::<(Box<f64>,), E3>((Box::new(0.1f64),)).unwrap_or((Box::new(f64::MAX),)) }) };
            let mut v45: String = format!("{} {:10.2}|", (*(*a37).0), 809.1881f64).trim().to_string();
            let v46: f32 = (if (true) { (*a36) } else { (*a36) }).floor();
            f43(1e-40f32.div_euclid(v46)).mix();
        }
        let mut v47: Vec<char> = (if let Some(x52) = (None::<Option<Vec<char>>>) { x52 } else { Some(vec!['\u{7f}', 'a', '\'']) }).filter(|x50: &Vec<char>| -> bool { Some(false).expect("nothing there") }).unwrap_or_else(|| -> Vec<char> { vec!['Z', 'ß'] });
        let v54: E3 = (*a40).clone();
        let mut v55: S1 = (*a35).clone();
        <[(f32, f64, i128, f32, i128, f32); 2]>::try_from(({ let mut items = vec![(1.0f32, 0.2f64, 165073588256409205364282012465350426823i128, 65535.0f32, (-12080306183663629875853097237436839488i128), (-0.0f32)), (f32::MAX, 0.2f64, (-19i128), f32::EPSILON, 147573952589676412929i128, f32::MIN), ((-701.7533f32), (-798.7314f64), 32i128, (-842.0971f32), (-105501285188534672169714078467150540709i128), 1e-40f32), (0.1f32, 484.1298f64, 20i128, f32::NAN, 140694703457599670937441056191773787461i128, f32::MIN), ((-9223372036854775808.0f32), f64::INFINITY, (-69371834661357219232999509852543390337i128), 3.25f32, (-3i128), 717.5200f32)]; let at = items.len() / 2; items.split_off(at) })).unwrap_or((match (vec![E2::V1('_'), E2::V2, E2::V0(String::from("42"), String::from("0123456789"), String::from("ff")), E2::V0(String::from("line\n"), String::from("ff"), String::from("0123456789")), E2::V1('\u{7f}')].as_slice()) { [] => (*a38), [] => [((-9223372036854775808.0f32), f64::MIN_POSITIVE, 151115727451828646838273i128, 16777217.0f32, 8i128, (-9223372036854775808.0f32)), (0.0f32, f64::INFINITY, 2i128, f32::MIN, (-1i128), (-1.0f32))], _ => (*a38) }))
    }
    #[inline(never)]
    pub fn f63(a64: &str, a65: &bool, a66: &mut [bool], a67: &E3, a68: &str, a69: &(Box<f64>,)) -> f64 {
        { let value = (!(E3::V2((-9223372036854775808.0f32), '\u{10FFFF}') == E3::V3(S1 { f0: '\\' }, 0.1f64, String::from("hello")))); if let Some(slot) = a66.first_mut() { *slot = value; } }
        let v70: Vec<bool> = Vec::<bool>::new();
        let mut v71: f64 = (loop { break id::<f64>((*(*a69).0)); });
        0.1f64
    }
    #[inline]
    pub fn f72() -> u128 {
        let mut v73: u128 = (match ((if (true) { E2::V1('_') } else { E2::V2 })) { E2::V0(p74, _, _) => 36u128.wrapping_sub(7u128), E2::V1(_) => ((-128.9f64) as u128), E2::V2 => (f64::NEG_INFINITY as u128) });
        {
            async fn work(seed: u32) -> u64 { let h75 = format!("{seed}-58"); let w76 = later(3, seed as u64 + 57).await; let h77 = [seed as u8; 1]; let w78 = later(2, [seed as u64; 2]).await[0]; let h79 = vec![Noisy(31), Noisy(seed % 5)]; let mut w80 = 0u64; for i in 0..2u64 { let _g = Noisy(i as u32); w80 += later((i % 2) as u32, i).await; } let h81 = format!("{seed}-12"); let w82 = later(3, seed as u64 + 28).await; (h75.len() as u64).rotate_left(0) ^ (w76).rotate_left(1) ^ (h77[0] as u64).rotate_left(2) ^ (w78).rotate_left(3) ^ (h79.len() as u64).rotate_left(4) ^ (w80).rotate_left(5) ^ (h81.len() as u64).rotate_left(6) ^ (w82).rotate_left(7) }
            block_on(work(90)).mix();
            block_on(work(70)).mix();
            block_on(work(83)).mix();
        }
        let E3::V4(_) = (pick::<E3>(false, &E3::V0 { f0: String::from("0123456789") }, &E3::V4((-228.1409f32))).clone()) else { return (v73 as u128); };
        (match (Vec::<bool>::new().as_slice()) { [.., p84] => v73, [.., p86] => 32u128, _ => 20u128 }).to_be()
    }
    pub fn run() {
        let mut v87: Vec<f32> = ({ let mut items = ({ let mut items = vec![1e10f32, f32::MIN, (-1.0f32), 255.5f32, f32::EPSILON]; items.push(f32::NAN); items }); items.insert(0, f32::from(16i8)); items });
        let mut v90: [f32; 3] = [(match (sel::<E2>(true, E2::V2, E2::V1('\u{7f}'))) { E2::V0(p94, _, p95) => (match (v87.clone().as_slice()) { [p97, p96 @ .., p98] => f32::NAN, _ => (-0.0f32) }), E2::V1(p99) => f32::EPSILON, E2::V2 => (match (String::from("42").as_str()) { p100 => (-806.2890f32), _ => 0.5f32 }) }), (5i8..8i8).zip(({ let mut items = v87.clone(); items.push((-9223372036854775808.0f32)); items })).map(|x103: (i8, f32)| -> f32 { x103.1 }).enumerate().fold(0.1f32.trunc(), |acc105: f32, x106: (usize, f32)| -> f32 { (if (false) { acc105 } else { x106.1 }) }), 0.2f32];
        { let value = (if ((if (false) { true } else { true })) { "0.1".parse::<f32>().unwrap_or((-1.0f32)) } else { 4294967296.0f32.round() }); let raw = Box::into_raw(Box::new(v90)); unsafe { (*raw)[1] = value; Box::from_raw(raw).mix(); } }
        v90.mix();
        let v107: S1 = id::<S1>((match (pick::<String>(false, &String::from("a,b,,c"), &String::from("42")).clone().as_str()) { p108 if ((-1i8) > (-128i8)) => (if (true) { S1 { f0: '\\' } } else { S1 { f0: '\u{10FFFF}' } }), p109 => (match (vec!['\u{e000}', 'a', '\u{d7ff}', 'z', 'a'].as_slice()) { [.., '0'] => S1 { f0: '😀' }, _ => S1 { f0: 'é' } }), "x y" => (match (v87.clone().as_slice()) { [] => S1 { f0: '\u{7f}' }, _ => S1 { f0: '\u{d7ff}' } }), _ => (match (vec![String::from("3.5"), String::from("hello"), String::from("hello"), String::from("a,b,,c"), String::from("a,b,,c")].as_slice()) { [.., p113] if false => S1 { f0: '\\' }, [.., p115] if true => S1 { f0: '0' }, _ => S1 { f0: '\n' } }) }));
        let g116: fn() -> u128 = f72;
        let mut v117: E2 = E2::V2;
        let v118: f32 = (if (true) { (unsafe { let mut slot = MaybeUninit::<f32>::uninit(); slot.write("x".parse::<f32>().unwrap_or(v90[0])); slot.assume_init() }) } else { v90[0] });
        apply_dyn(&|x119: f64| -> (Box<f64>,) { (Box::new(0.2f64),) }, 1.5f64).mix();
        v117 = (if ((match (Some(false)) { None => false, Some(x120) => x120 })) { v117.clone() } else { E2::V1('ß') });
        let mut v121: [(f32, f64, i128, f32, i128, f32); 2] = (match (String::from("-7").as_str()) { p124 => [(1e-40f32, 16777217.0f64, 100i128, 255.5f32, 664613997892457936451903530140172288i128, 0.5f32), (1.0f32, 1.5f64, 2i128, f32::NAN, 1024i128, (-139.0763f32))], p125 => [(f32::MIN_POSITIVE, (-790.3267f64), 8i128, (-749.4739f32), 170141183460469231731687303715884105726i128, 1e30f32), (785.5322f32, f64::MAX, (-3i128), 1e30f32, (-6i128), (-1.0f32))], "Abc Def" => [((-128.9f32), (-2.5e-3f64), 85070591730234615865843651857942052863i128, f32::MIN, 54660913991656536690361596999850750334i128, f32::NEG_INFINITY), (0.2f32, 1e-40f64, 10i128, f32::MIN, 2i128, 1e-40f32)], _ => [((-1.0f32), f64::EPSILON, 4503599627370497i128, 155.4878f32, 10i128, (-2.5e-3f32)); 2] });
        { let (left, right) = v121.split_at_mut(1); std::mem::swap(&mut left[0], &mut right[0]); }
        if let "ff" = (String::from_utf8_lossy(&vec![34u8, 8u8, 1u8, 127u8, 235u8]).into_owned().as_str()) {
            if let p126 = ((String::from("hello") + &String::from("42"))) {
                match &mut v117 {
                    E2::V2 => {
                        let v127: S1 = (match (v121[0].4) { _ => v107.clone(), _ => S1 { f0: '\u{d7ff}' } });
                    }
                    _ => {}
                }
                if let -7i8 = ((-60i8)) {
                    v87.dedup();
                }
            } else {
                v90 = [1.0f32, (-128.9f32), f32::MAX];
            }
        }
    }
}

mod c5 {
    use super::*;
    #[derive(Clone, Debug, PartialEq, PartialOrd, Copy, Eq, Ord, Hash)]
    pub struct S1;
    impl Mix for S1 {
        fn mix(&self) {
            feed(0);
        }
    }
    const K2: i64 = (-18i64);
    const K3: i128 = (-81925980767927858993723566623778574225i128);
    const K4: i128 = (match 7i128.checked_add((-111231749138148152469490973708616588507i128)) { Some(sum) => sum, None => (-111231749138148152469490973708616588507i128) });
    pub trait T5 {
        fn m6(&mut self, a0: u8) -> i64;
        fn m7(&self, a8: i128) -> u8 {
            ((8589934593i64 as u8).wrapping_add(Err::<u8, u128>(2u128).unwrap_or_else(|x9: u128| -> u8 { 38u8 })).overflowing_add((26u8.overflowing_mul(8u8).1 as u8).saturating_pow((!8u32) % 7)).1 as u8)
        }
    }
    impl T5 for i64 {
        fn m6(&mut self, a10: u8) -> i64 {
            (*self)
        }
    }
    impl T5 for u8 {
        fn m6(&mut self, a11: u8) -> i64 {
            let v12: u8 = std::mem::replace(&mut (*self), sel::<u8>(true, a11, a11));
            (*self) = (*self);
            (loop { break "😀".trim(); }).parse::<i64>().unwrap_or(sel::<i64>((match (Some(false)) { None => true, Some(x13) => x13 }), (-1i64), (1u16 as i64)))
        }
    }
    impl T5 for i128 {
        fn m6(&mut self, a14: u8) -> i64 {
            ((loop { break (K2.trailing_ones() as i64); }).trailing_zeros() as i64)
        }
    }
    impl T5 for isize {
        fn m6(&mut self, a15: u8) -> i64 {
            let mut v16: Vec<i128> = vec![(K3.checked_shr(10u32).unwrap_or((-36144706756089125394246947310184439446i128)).overflowing_sub(K3).1 as i128), (Err::<bool, S1>(S1).unwrap_or(true) as i128), K4];
            (2i64.wrapping_sub(K2).trailing_ones() as i64)
        }
    }
    #[inline(never)]
    pub fn f20<G19: Mix + Clone + Debug + PartialOrd>(a21: G19, a22: &mut Vec<G19>) -> i64 {
        (*a22).insert((String::from("a,b,,c").chars().count() % 4) % ((*a22).len() + 1), (unsafe { let mut slot = MaybeUninit::<G19>::uninit(); slot.write(a21.clone()); slot.assume_init() }));
        let mut v24: S1 = [S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1][16];
        match ((true as i64)) {
            _ => {
                v24 = Some(S1).map_or(v24, |x25: S1| -> S1 { v24 });
                (*a22).pop().mix();
            }
            _ => K2.mix(),
        }
        ((true as u32) as i64)
    }
    #[inline(never)]
    pub fn f27(a28: i128, a29: u8) -> i64 {
        ({ let mut m31: Vec<i128> = vec![K4]; let out = f20::<i128>(('b30: { if (true) { break 'b30 4095i128; } K4 }), &mut m31); m31.mix(); out })
    }
    #[inline(never)]
    pub fn f33(a34: u8, a35: u128, a36: u8) -> impl Fn(isize) -> [S1; 17] {
        move |x37: isize| -> [S1; 17] { [S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1] }
    }
    pub fn run() {
        let mut n38 = 0u32;
        'l39: while n38 < 8 && (Some(Err::<Box<u128>, S1>(S1)).is_none()) {
            n38 += 1;
            let mut v40: [S1; 17] = (unsafe { *(&f33(65u8, 3u128, 22u8)(7isize) as *const [S1; 17]) });
            let mut v45: BTreeMap<u8, u128> = BTreeMap::new();
            v45.insert(Err::<u8, Result<Box<u128>, S1>>(Err::<Box<u128>, S1>(S1)).map_err(|x47: Result<Box<u128>, S1>| -> i64 { 9145416658918156269i64 }).unwrap_or_else(|x46: i64| -> u8 { 37u8 }), (340282366920938463463374607431768211455u128.checked_shr(131072u32).unwrap_or(0u128) | 22u128.checked_neg().unwrap_or_default())).mix();
            v45.insert((Box::new((-11isize)) as Box<dyn T5>).m7(K3).saturating_pow(134217727u32 % 7), (170141183460469231731687303715884105727u128.overflowing_mul(Vec::<u128>::new().into_iter().filter(|x48: &u128| -> bool { true }).fold(10u128, |acc49: u128, x50: u128| -> u128 { acc49 })).1 as u128)).mix();
            v45.insert((Box::new(1i128.wrapping_pow(3501293603u32 % 7)) as Box<dyn T5>).m7(K3.to_be()), (unsafe { let mut slot = MaybeUninit::<u128>::uninit(); slot.write(15u128); slot.assume_init() }).wrapping_add(8u128.wrapping_mul(9671406556917033397649407u128))).mix();
            v45.insert(9u8.checked_shl(2u32).unwrap_or(149u8).m7(K4), 2596148429267413814265248164610049u128.wrapping_pow(2048u32 % 7)).mix();
            for (key, value) in &v45 { key.mix(); value.mix(); }
            v45.len().mix();
        }
        let mut n51 = 0u32;
        'l52: while n51 < 1 && (None::<isize>.is_none()) {
            n51 += 1;
            {
                if ((-f64::NEG_INFINITY).is_finite()) {
                    let S1: S1 = S1;
                } else {
                    {
                        for armed in [false, true] { caught(|| { let t = [Noisy(23), Noisy(29), Noisy(31)]; let [.., _x] = t; trip(armed, ()); }); }
                        for armed in [false, true] { caught(|| { let t = (Noisy(23), Box::new(Duo { a: Noisy(29), b: Noisy(31) })); let _x = t.1.a; let _y = t.0; trip(armed, ()); }); }
                    }
                }
                let mut v53: S1 = S1;
                let mut n54 = 0u32;
                'l55: while n54 < 3 && (f64::INFINITY.is_infinite()) {
                    n54 += 1;
                    if ((if (false) { 13744200898029403983usize } else { 274877906945usize }).overflowing_add((1usize.trailing_ones() as usize)).1) { break 'l52; }
                    v53 = sel::<S1>(matches!(16384isize, (19isize..)), v53, v53);
                }
                let S1: S1 = v53;
            }
            {
                let f58 = |a56: i64, a57: u128| -> S1 { S1 };
                let mut n59 = 0u32;
                'l60: while n59 < 8 && ((unsafe { let mut slot = MaybeUninit::<bool>::uninit(); slot.write(false); slot.assume_init() })) {
                    n59 += 1;
                    let mut v61: Vec<Result<u128, S1>> = (if let Some(x63) = (Some(vec![Err::<u128, S1>(S1), Ok::<u128, S1>(40564819207303340847894502572032u128)])) { x63 } else { vec![Err::<u128, S1>(S1)] });
                    let v65: Option<Vec<Result<u128, S1>>> = Some(v61);
                }
                let r66 = catch_unwind(AssertUnwindSafe(|| -> S1 {
                    {
                        let (a, b) = (black_box((1e39) as f32), black_box((0.0) as f32));
                        a.sqrt().mix(); a.mul_add(b, 1.5).mix(); a.rem_euclid(3.0).mix(); a.div_euclid(3.0).mix(); a.powi(3).mix(); a.recip().mix();
                        let (mut acc, mut k) = (a, 0u32); while acc.is_finite() && acc.abs() < 1e30 && k < 40 { acc = acc * 1.5 + 1.0; k += 1; } feed(k as u64);
                        feed(a.total_cmp(&b) as i8 as u64); feed(a.partial_cmp(&b).map_or(9, |it| it as i8 as u8) as u64); feed((a < b) as u64 + 2 * (a <= b) as u64 + 4 * (a == b) as u64 + 8 * (a != b) as u64);
                        (a + b).mix(); (a - b).mix(); (a * b).mix(); (a / b).mix(); (a % b).mix(); (-a).mix();
                        a.floor().mix(); a.ceil().mix(); a.round().mix(); a.trunc().mix(); a.fract().mix(); a.abs().mix(); a.round_ties_even().mix();
                    }
                    let v67: bool = (if let Some(x68) = (Some(true)) { x68 } else { (loop { break true; }) });
                    assert!(v67);
                    ('b71: { if ((!None::<u128>.is_some())) { break 'b71 f58(K2, (unsafe { std::mem::transmute::<i128, i128>(K3) } as u128)); } S1 })
                }));
                match r66 {
                    Ok(value) => value.mix(),
                    Err(caught) => payload(caught),
                }
                if ((!(!false))) {
                    {
                        trait Base { fn id(&self) -> u32; fn name(&self) -> &'static str { "base" } }
                        trait Mid: Base { fn twice(&self) -> u32 { self.id().wrapping_mul(2) } }
                        trait Top: Mid + Debug { fn all(&self) -> u64 { self.id() as u64 + self.twice() as u64 + self.name().len() as u64 } }
                        #[derive(Debug)] struct P(u32);
                        #[derive(Debug)] struct Q(u8, Noisy);
                        impl Base for P { fn id(&self) -> u32 { self.0 } }
                        impl Base for Q { fn id(&self) -> u32 { self.0 as u32 + self.1.0 } fn name(&self) -> &'static str { "q" } }
                        impl Mid for P {} impl Top for P {}
                        impl Mid for Q { fn twice(&self) -> u32 { 17 } } impl Top for Q {}
                        fn through(it: &dyn Mid) -> u32 { let base: &dyn Base = it; base.id() ^ it.twice() }
                        let mut tops: Vec<Box<dyn Top>> = vec![Box::new(P(10)), Box::new(Q(37, Noisy(17))), Box::new(P(37))];
                        { let last: Box<dyn Base> = tops.pop().unwrap(); feed(last.id() as u64); }
                        { let shared: Rc<dyn Top> = Rc::new(Q(1, Noisy(2))); let mid: Rc<dyn Mid> = shared.clone(); feed(mid.twice() as u64 + Rc::strong_count(&shared) as u64); }
                        { let first: &mut dyn Top = &mut *tops[0]; let base: &mut dyn Base = first; feed(base.id() as u64); }
                    }
                    ({ let mut it = (loop { break K3; }); let out = it.m6("a".parse::<u8>().unwrap_or(22u8)); it.mix(); out }).mix();
                    let mut v72: Result<Box<u128>, S1> = larger::<Result<Box<u128>, S1>>(('b75: { if (true) { break 'b75 Err::<Box<u128>, S1>(S1); } Ok::<Box<u128>, S1>(Box::new(232487637090643041310319870749627641988u128)) }), sel::<Result<Box<u128>, S1>>(false, Err::<Box<u128>, S1>(S1), Err::<Box<u128>, S1>(S1)));
                }
                f58(3i64, 24u128.max(0u128)).mix();
            }
        }
        let mut n77 = 0u32;
        'l78: while n77 < 1 && ((unsafe { let mut slot = MaybeUninit::<bool>::uninit(); slot.write(false); slot.assume_init() })) {
            n77 += 1;
            {
                async fn work(seed: u32) -> u64 { let mut h79 = seed as u64; let h79r = &mut h79; *h79r += 29; let w80 = later(1, [seed as u64; 9]).await[0]; let h81: Option<Box<u16>> = if seed % 2 == 0 { Some(Box::new(18)) } else { None }; let w82 = later(0, seed as u64 + 58).await; (*h79r).rotate_left(0) ^ (w80).rotate_left(1) ^ (h81.as_deref().copied().unwrap_or(1) as u64).rotate_left(2) ^ (w82).rotate_left(3) }
                block_on(work(73)).mix();
            }
            {
                let S1: S1 = Ok::<S1, S1>(S1).unwrap_or(S1);
                let mut o84: Vec<Box<dyn T5>> = vec![Box::new((Box::new(K4) as Box<dyn T5>).m7((K3.overflowing_add(K3).1 as i128))), Box::new(K3.min(K3).wrapping_mul(K4)), Box::new(i128::from_str_radix("line\n", 36).unwrap_or(170141183460469231731687303715884105726i128))];
                for o85 in o84.iter_mut() { o85.m6((if (false) { 5u8 } else { 0u8 })).mix(); }
            }
            if let [.., p89] = (None::<Vec<i32>>.unwrap_or_else(|| -> Vec<i32> { vec![16383i32, 16384i32, 1023i32] }).as_slice()) {
                let [.., S1]: [S1; 3] = Some([S1, S1, S1]).filter(|x93: &[S1; 3]| -> bool { false }).unwrap_or_else(|| -> [S1; 3] { <[S1; 3]>::try_from(vec![S1, S1, S1, S1, S1]).unwrap_or([S1; 3]) });
                let mut v96: [S1; 17] = [(loop { break S1; }); 17];
            }
        }
        let mut n99 = 0u32;
        'l100: while n99 < 5 && (4294967297u128.overflowing_mul(59200545954027203659752714949384120311u128).1) {
            n99 += 1;
            let mut n101 = 0u32;
            'l102: while n101 < 2 && (Err::<bool, Result<u128, S1>>(Err::<u128, S1>(S1)).unwrap_or_else(|x103: Result<u128, S1>| -> bool { true })) {
                n101 += 1;
                match (String::from("3.5").as_str()) {
                    p104 => {
                        let v105: Result<Box<u128>, S1> = Ok::<Box<u128>, S1>(Box::new(340282366920938463463374607431768211455u128));
                        let f107 = move || -> S1 { let v106 = v105; let out = (unsafe { let mut slot = MaybeUninit::<S1>::uninit(); slot.write([S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1][16]); slot.assume_init() }); drop(v106); out };
                        apply_boxed(Box::new(f107)).mix();
                        let [S1, p114 @ .., S1]: [S1; 4] = (match (Err::<[S1; 4], S1>(S1).ok()) { None => <[S1; 4]>::try_from(vec![S1, S1, S1, S1, S1]).unwrap_or([S1, S1, S1, S1]), Some(x110) => x110 });
                    }
                    _ => Err::<Box<u128>, S1>(S1).mix(),
                }
                let mut v115: String = ({ let mut text = format!("{:^7}| {:08} {:b}", 5397293118249662247usize, 1usize, 1usize); text.push_str((loop { break "-7"; })); text });
                let mut v116: Vec<u8> = ({ let mut items = vec![10u8]; items.insert(0, 111u8); items });
                v116.truncate(6);
                'l118: while let Some(x117) = v116.pop() {
                    let S1: S1 = (match ((unsafe { let mut slot = MaybeUninit::<Option<S1>>::uninit(); slot.write(None::<S1>); slot.assume_init() })) { None => S1, Some(x120) => x120 });
                    let mut v122: u8 = x117;
                }
                if (twice::<bool>(&27393i16.overflowing_sub((-1i16)).1).0) { continue 'l102; }
            }
        }
        if (false) {
            let mut n123 = 0u32;
            'l124: while n123 < 2 && (String::from("ff").starts_with("😀")) {
                n123 += 1;
                let S1: S1 = None::<S1>.unwrap_or_else(|| -> S1 { S1 });
                {
                    trait Base { fn id(&self) -> u32; fn name(&self) -> &'static str { "base" } }
                    trait Mid: Base { fn twice(&self) -> u32 { self.id().wrapping_mul(2) } }
                    trait Top: Mid + Debug { fn all(&self) -> u64 { self.id() as u64 + self.twice() as u64 + self.name().len() as u64 } }
                    #[derive(Debug)] struct P(u32);
                    #[derive(Debug)] struct Q(u8, Noisy);
                    impl Base for P { fn id(&self) -> u32 { self.0 } }
                    impl Base for Q { fn id(&self) -> u32 { self.0 as u32 + self.1.0 } fn name(&self) -> &'static str { "q" } }
                    impl Mid for P {} impl Top for P {}
                    impl Mid for Q { fn twice(&self) -> u32 { 6 } } impl Top for Q {}
                    fn through(it: &dyn Mid) -> u32 { let base: &dyn Base = it; base.id() ^ it.twice() }
                    let mut tops: Vec<Box<dyn Top>> = vec![Box::new(P(31)), Box::new(Q(8, Noisy(6))), Box::new(P(8))];
                    { let first: &mut dyn Top = &mut *tops[0]; let base: &mut dyn Base = first; feed(base.id() as u64); }
                    { let shared: Rc<dyn Top> = Rc::new(Q(1, Noisy(2))); let mid: Rc<dyn Mid> = shared.clone(); feed(mid.twice() as u64 + Rc::strong_count(&shared) as u64); }
                    for top in &tops { let mid: &dyn Mid = &**top; let base: &dyn Base = mid; feed(base.id() as u64); base.name().mix(); feed(through(mid) as u64); feed(top.all()); }
                    { let last: Box<dyn Base> = tops.pop().unwrap(); feed(last.id() as u64); }
                    { let mid: Box<dyn Mid> = tops.remove(1); let base: Box<dyn Base> = mid; base.name().mix(); }
                }
                {
                    for armed in [false, true] { caught(|| { let t = Box::new((Noisy(7), Noisy(12), Noisy(27))); let _x = t.2; trip(armed, ()); let _late = Noisy(51); }); }
                }
                let v126: NonZeroI64 = NonZeroI64::new(({ let mut it = K2.wrapping_pow(32u32 % 7); let out = it.m6(u8::from_str_radix("x y", 10).unwrap_or(32u8)); it.mix(); out })).unwrap_or((unsafe { *(&NonZeroI64::new(K2).unwrap_or(NonZeroI64::new(15i64).unwrap()) as *const NonZeroI64) }));
            }
            K3.mix();
            let v127: [(String, String); 5] = (unsafe { let mut slot = MaybeUninit::<[(String, String); 5]>::uninit(); slot.write([(String::from(""), String::from("42")), (String::from("Abc Def"), String::from("-7")), (String::from("hello"), String::from("line\n")), (String::from("  pad  "), String::from("-7")), (String::from("😀"), String::from("x y"))]); slot.assume_init() });
            let f129 = move || -> [(String, String); 5] { let v128 = v127; let out = v128.clone(); drop(v128); out };
            apply_once(f129).mix();
        }
        let r134 = catch_unwind(AssertUnwindSafe(|| -> i16 {
            let mut v135: [S1; 17] = <[S1; 17]>::try_from(({ let mut items = vec![S1, S1, S1, S1, S1]; items.insert(0, S1); items })).unwrap_or(Some([S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1]).unwrap_or_else(|| -> [S1; 17] { [S1; 17] }));
            v135[6] = (if let Some(x142) = (Some(S1)) { x142 } else { false.then(|| -> S1 { v135[5] }).map_or(v135[16], |x143: S1| -> S1 { x143 }) });
            256i16
        }));
        match r134 {
            Ok(value) => value.mix(),
            Err(caught) => payload(caught),
        }
        {
            {
                if let _ = (&{ 24852u16 }) {
                    let [S1, S1, S1, S1]: [S1; 4] = <[S1; 4]>::try_from(vec![S1, S1, S1]).unwrap_or((loop { break [S1; 4]; }));
                    {
                        static NUMBERS: [u32; 4] = [19, 48, 43, 4];
                        static SECOND: &u32 = &NUMBERS[1];
                        static TAIL: &[u32] = match &NUMBERS { [_, _, rest @ ..] => rest };
                        static WORDS: [&str; 3] = ["one", "two", "three"];
                        static PICKED: &&str = &WORDS[2];
                        struct Link { value: u32, next: Option<&'static Link>, call: fn(u32) -> u32, name: &'static str, shown: &'static (dyn Debug + Sync) }
                        fn double(x: u32) -> u32 { x.wrapping_mul(2) }
                        static END: Link = Link { value: 3, next: None, call: double, name: "end", shown: &NUMBERS };
                        static MID: Link = Link { value: 19, next: Some(&END), call: |x| x + 1, name: WORDS[0], shown: &"mid" };
                        static HEAD: Link = Link { value: 48, next: Some(&MID), call: double, name: "head", shown: &MID.value };
                        static TABLE: [fn(u32) -> u32; 3] = [double, |x| x ^ 5, HEAD.call];
                        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                        static LISTS: (&[u8], &[&[u8]], [Option<&u32>; 2]) = (b"bytes", &[b"a", b"", &[19, 2]], [Some(&NUMBERS[3]), None]);
                        const VIEW: &[u32; 4] = &NUMBERS;
                        thread_local! { static LOCAL: Cell<u32> = const { Cell::new(5) }; static NOTES: RefCell<Vec<&'static u32>> = const { RefCell::new(Vec::new()) }; }
                        { let mut at = Some(&HEAD); while let Some(link) = at { feed((link.call)(link.value) as u64); link.name.mix(); format!("{:?}", link.shown).mix(); at = link.next; } }
                        feed(*SECOND as u64 + TAIL.len() as u64 + std::ptr::eq(SECOND, &NUMBERS[1]) as u64 * 16 + std::ptr::eq(TAIL.as_ptr(), &NUMBERS[2]) as u64 * 32);
                        LOCAL.with(|it| { it.set(it.get() + 1); feed(it.get() as u64 % 2 + 40); }); NOTES.with(|it| { it.borrow_mut().push(SECOND); feed(*it.borrow()[0] as u64); });
                    }
                } else {
                    let p150: S1 = S1;
                }
                if (true) {
                    let [S1, S1, S1]: [S1; 3] = ({ let mut items = [S1; 3]; items.sort_by(|a, b| b.cmp(a)); items });
                    let [S1, S1, S1]: [S1; 3] = [S1, S1, S1];
                } else {
                    {
                        let mut items: Vec<Box<dyn Any>> = vec![Box::new([50u16; 3]), Box::new(Noisy(50)), Box::new(String::from("owned")), Box::new(25u32), Box::new(Some(25i64)), Box::new("text")];
                        for item in &items { if let Some(it) = item.downcast_ref::<u32>() { feed(*it as u64); } else if let Some(it) = item.downcast_ref::<&str>() { it.mix(); } else if item.is::<Noisy>() { feed(1); } else if let Some(it) = item.downcast_ref::<(u8, bool)>() { feed(it.0 as u64 + it.1 as u64); } else { feed(2); } }
                        while let Some(item) = items.pop() { match item.downcast::<Noisy>() { Ok(it) => it.mix(), Err(other) => feed(other.is::<String>() as u64) } }
                    }
                    {
                        { let mut count = 0u32; let mut step = |by: u32| { count += by; count }; for i in 0..5 { step(i).mix(); } count.mix(); }
                        { let found = 'search: loop { for i in 0..5u32 { let hit = move |at: u32| at * i == 4; for j in 0..3 { if hit(j) { break 'search Some((i, j)); } } } break None; }; found.mix(); }
                        { for i in 0..5u32 { let n = Noisy(i); let once = move || n.0 + 4; if i != 1 { once().mix(); } } }
                        { let mut made: Vec<Box<dyn Fn(u32) -> u32>> = Vec::new(); for i in 0..5u32 { let k = i.wrapping_mul(4); made.push(Box::new(move |x| x.wrapping_add(k))); } for f in &made { f(1).mix(); } }
                    }
                }
            }
            if ((true & 0isize.overflowing_sub((-11isize)).1)) {
                let v153: Result<Result<u8, isize>, S1> = Ok::<Result<u8, isize>, S1>(Ok::<u8, isize>(95u8));
                let mut n155 = 0u32;
                'l156: while n155 < 8 && (Err::<bool, Result<u128, S1>>(Ok::<u128, S1>(39u128)).unwrap_or(false)) {
                    n155 += 1;
                    let v158: Result<Result<u8, isize>, S1> = ('b161: { if (true) { break 'b161 Ok::<Result<u8, isize>, u32>(Ok::<u8, isize>(5u8)); } Ok::<Result<u8, isize>, u32>(Ok::<u8, isize>(1u8)) }).or_else(|x159: u32| -> Result<Result<u8, isize>, S1> { Some(Err::<Result<u8, isize>, S1>(S1)).unwrap_or(v153) });
                    {
                        for armed in [false, true] { caught(|| { let _v = (Noisy(60), Duo { a: Noisy(51), b: trip(armed, Noisy(56)) }); }); }
                        for armed in [false, true] { caught(|| { let _v = vec![Noisy(24), trip(armed, Noisy(31)), Noisy(47)]; }); }
                        for armed in [false, true] { caught(|| { let _v = Box::new((Noisy(25), trip(armed, Noisy(42)))); }); }
                    }
                }
                let r164 = catch_unwind(AssertUnwindSafe(|| -> [(String, String); 5] {
                    let v165: [(String, String); 5] = [(String::from("ff"), String::from("😀")), (String::from("x y"), String::from("3.5")), (String::from("line\n"), String::from("line\n")), (String::from("42"), String::from("  pad  ")), (String::from("line\n"), String::from("Abc Def"))];
                    let f167 = move || -> Box<S1> { let v166 = v165; let out = Box::new(S1); drop(v166); out };
                    f167().mix();
                    <[(String, String); 5]>::try_from(vec![(match (None::<(String, String)>) { None => (String::from("line\n"), String::from("")), Some(x173) => x173 }), sel::<(String, String)>(true, (String::from("ff"), String::from("-7")), (String::from("a"), String::from("😀"))), ('b174: { if (true) { break 'b174 (String::from(""), String::from("hello")); } (String::from("héllo"), String::from("  pad  ")) })]).unwrap_or([Some((String::from("😀"), String::from("héllo"))).unwrap_or_else(|| -> (String, String) { (String::from("3.5"), String::from("a,b,,c")) }), [(String::from("ff"), String::from("Abc Def")), (String::from("line\n"), String::from("")), (String::from("3.5"), String::from("hello")), (String::from("x y"), String::from("ff")), (String::from("hello"), String::from("0123456789"))][2].clone(), sel::<(String, String)>(false, (String::from("ff"), String::from("  pad  ")), (String::from("x y"), String::from("a,b,,c"))), sel::<(String, String)>(true, (String::from("héllo"), String::from("-7")), (String::from("a,b,,c"), String::from("3.5"))), (loop { break (String::from("42"), String::from("😀")); })])
                }));
                match r164 {
                    Ok(value) => value.mix(),
                    Err(caught) => payload(caught),
                }
            } else {
                {
                    { let mut pair = (Noisy(54), vec![55u8]); let mut grow = || pair.1.push(1); grow(); let first = &pair.0; first.mix(); pair.1.mix(); }
                }
            }
        }
        let mut v178: BTreeMap<u16, usize> = BTreeMap::new();
        v178.insert((K4 as u16).wrapping_mul((9223372036854775808usize as u16)), ((match (Some(12512561170728463032usize)) { None => 18446744073709551614usize, Some(x179) => x179 }) << (37u32 % 64))).mix();
        v178.insert(13610u16.max(512u16).checked_shr((unsafe { Bytes4 { word: 4294967295u32 }.halves[1] } as u32)).unwrap_or(35108u16.wrapping_add(10u16)), 3439602788356294034usize).mix();
        v178.insert(65535u16, (unsafe { Bytes16 { signed: K4.wrapping_neg() }.signed } as usize)).mix();
        v178.insert(31315u16.saturating_mul(256u16).checked_shl(u32::try_from(7i8).unwrap_or(958958128u32)).unwrap_or("42".parse::<u16>().unwrap_or(16272u16)), 25usize).mix();
        for (key, value) in &v178 { key.mix(); value.mix(); }
        v178.len().mix();
        let mut v180: isize = (unsafe { let mut slot = MaybeUninit::<isize>::uninit(); slot.write((4503599627370495u128 as isize)); slot.assume_init() });
        if (true) {
            let r181 = catch_unwind(AssertUnwindSafe(|| -> [S1; 4] {
                let mut n182 = 0u32;
                'l183: while n182 < 8 && (17u128.overflowing_sub(138352275829519035763868480973904817587u128).1) {
                    n182 += 1;
                    {
                        async fn work(seed: u32) -> u64 { let h184 = Noisy(seed % 59 + 1); let w185 = if seed % 2 == 0 { later(1, 5u64).await } else { 32 }; (h184.0 as u64).rotate_left(0) ^ (w185).rotate_left(1) }
                        block_on(work(95)).mix();
                    }
                    let S1: S1 = (loop { break ('b186: { if (false) { break 'b186 S1; } S1 }); });
                }
                let r187 = catch_unwind(AssertUnwindSafe(|| -> isize {
                    let mut v188: Vec<[S1; 3]> = Vec::<[S1; 3]>::new();
                    (!(!v180))
                }));
                match r187 {
                    Ok(value) => value.mix(),
                    Err(caught) => payload(caught),
                }
                (unsafe { *(&[S1; 4] as *const [S1; 4]) })
            }));
            match r181 {
                Ok(value) => value.mix(),
                Err(caught) => payload(caught),
            }
            {
                async fn work(seed: u32) -> u64 { let h191 = Noisy(seed % 28 + 1); let mut w192 = 0u64; for i in 0..2u64 { let _g = Noisy(i as u32); w192 += later((i % 2) as u32, i).await; } (h191.0 as u64).rotate_left(0) ^ (w192).rotate_left(1) }
                block_on(work(61)).mix();
            }
        } else {
            {
                #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(C)] struct Plain { a: u8, b: u32, c: u16, d: u64 }
                #[derive(Clone, Copy, Default)] #[repr(C, packed)] struct Tight { a: u8, b: u64, c: u16, d: u128 }
                #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(C, align(2))] struct Big { a: u8, b: [u16; 3] }
                #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(align(2))] struct Rusty(u8, u32);
                #[derive(Clone, Copy, Debug, PartialEq)] #[repr(transparent)] struct Wrap(f32);
                #[derive(Clone, Copy, Debug, PartialEq)] #[repr(i8)] enum Signed { Low = -128, Mid = -1, High = 127 }
                #[derive(Clone, Copy, Debug, PartialEq)] #[repr(u16)] enum Wider { A = 1, B = 0x1234, C = 0xffff }
                #[derive(Clone, Copy, Debug, PartialEq)] #[repr(C)] enum Cee { Unit, One(u8), Two { x: u16, y: u64 } }
                #[derive(Clone, Copy, Debug, PartialEq)] #[repr(C, u8)] enum Both { P(u32), Q(u8, u8), R }
                #[derive(Clone, Copy)] #[repr(C)] union Bits { word: u32, bytes: [u8; 4], half: (u16, u16) }
                #[inline(never)] extern "C" fn c_plain(p: Plain, t: Tight, w: Wrap) -> Plain { Plain { a: p.a.wrapping_add(t.a), b: p.b ^ { t.c } as u32, c: p.c, d: p.d } }
                #[inline(never)] extern "C" fn c_big(x: u8, big: Big, y: u8, rusty: Rusty) -> Big { Big { a: big.a ^ x ^ y ^ rusty.0, b: [big.b[2], big.b[0], rusty.1 as u16] } }
                #[inline(never)] fn r_enums(s: Signed, w: Wider, c: Cee, b: Both) -> (i8, u16, Cee, Both) { (s as i8, w as u16, match c { Cee::One(v) => Cee::Two { x: v as u16, y: 1 }, other => other }, match b { Both::Q(x, y) => Both::Q(y, x), other => other }) }
                { for s in black_box([Signed::Low, Signed::Mid, Signed::High]) { feed((s as i8 as i64 + 200) as u64); feed((s as i32 == -1) as u64); } for w in black_box([Wider::A, Wider::B, Wider::C]) { feed(w as u64); } }
                { let big = c_big(27, black_box(Big { a: 1, b: [2, 3, 4] }), 6, black_box(Rusty(5, 6))); big.a.mix(); big.b.mix(); feed((&big as *const Big as usize % 2) as u64); }
                feed((size_of::<Plain>() + align_of::<Plain>() * 256 + size_of::<Tight>() * 4096 + align_of::<Tight>() * 65536) as u64); feed((size_of::<Big>() + align_of::<Big>() * 256 + size_of::<Rusty>() * 4096 + size_of::<Cee>() * 65536 + size_of::<Both>() * 1048576) as u64);
                feed((std::mem::offset_of!(Plain, b) + std::mem::offset_of!(Plain, c) * 16 + std::mem::offset_of!(Plain, d) * 256 + std::mem::offset_of!(Tight, d) * 4096 + std::mem::offset_of!(Big, b) * 65536) as u64);
                { let mut t = black_box(Tight { a: 27, b: 6, c: 7, d: u128::MAX }); t.b += 1; t.d = t.d.wrapping_add(27); let (b, c, d) = (t.b, t.c, t.d); feed(b + c as u64); d.mix(); feed(unsafe { std::ptr::addr_of!(t.d).read_unaligned() } as u64); }
                { let w = black_box(Wrap(2.5)); let raw: f32 = unsafe { std::mem::transmute(w) }; raw.mix(); let items = black_box([Rusty(1, 2), Rusty(3, 4)]); feed(items[1].1 as u64 + size_of_val(&items) as u64 * 16); }
            }
        }
        v180 |= ((-1i8).wrapping_add((-96i8)) as isize);
        let mut v193: Vec<NonZeroI64> = sel::<Vec<NonZeroI64>>(false, ({ let mut items = vec![NonZeroI64::new((-9223372036854775808i64)).unwrap(), NonZeroI64::new((-3i64)).unwrap(), NonZeroI64::new(5836632602982584659i64).unwrap(), NonZeroI64::new(6560114569519624363i64).unwrap(), NonZeroI64::new(7456088801619170024i64).unwrap()]; items.reverse(); items }), vec![NonZeroI64::new((-1i64)).unwrap(), NonZeroI64::new((-5i64)).unwrap(), NonZeroI64::new(16384i64).unwrap(), NonZeroI64::new((-1i64)).unwrap()]);
        let v197: [(String, String); 5] = [(151115727451828646838272u128.overflowing_add(100u128).1 as u128), 22u128.saturating_pow(4294967294u32 % 7), 15u128, 25u128.wrapping_neg(), 604462909807314587353088u128].map(|x201: u128| -> (String, String) { twice::<(String, String)>(&(loop { break (String::from("😀"), String::from("3.5")); })).0 });
        let f199 = move || -> i32 { let v198 = v197; let out = 0i32.wrapping_sub(262144i32).saturating_neg(); drop(v198); out };
        f199().mix();
        v180 |= ("x y".parse::<isize>().unwrap_or((-19isize)) >> (13u32 % 64));
        {
            async fn leaf(x: u64) -> u64 { let _g = Noisy(x as u32 % 9); later((x % 3) as u32, x + 19).await }
            async fn work(seed: u32) -> u64 { let h204 = (seed as f64 * 0.5, seed as u8, seed % 3 == 0); let w205 = match later(1, seed % 3).await { 0 => later(0, 10u64).await, 1 => 11, _ => later(2, 12u64).await }; let mut h206 = seed as u64; let h206r = &mut h206; *h206r += 13; let w207 = match later(1, seed % 3).await { 0 => later(0, 10u64).await, 1 => 11, _ => later(2, 12u64).await }; (h204.0 as u64 + h204.1 as u64 + h204.2 as u64).rotate_left(0) ^ (w205).rotate_left(1) ^ (*h206r).rotate_left(2) ^ (w207).rotate_left(3) }
            async fn down(n: u32) -> u64 { if n == 0 { leaf(1).await } else { Box::pin(down(n - 1)).await + leaf(n as u64).await } }
            async fn outer(seed: u32) -> u64 { let n = Noisy(seed); let a = leaf(seed as u64).await; let (b, c) = both(leaf(a), work(seed)).await; let d = async { let _in = Noisy(25); leaf(b ^ c).await }.await; a ^ b ^ c ^ d ^ n.0 as u64 }
            { let add = async |x: u64| later(1, x).await + 25; block_on(add(3)).mix(); }
            { let n = Noisy(25); let by_ref = async { later(1, ()).await; n.0 }; block_on(by_ref).mix(); n.mix(); }
            poll_some(outer(10), 4).mix();
            block_on(outer(45)).mix();
        }
    }
}
//// then
mod c0 {
    use super::*;
    #[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord, Hash)]
    pub struct S1<A> { pub f0: [A; 2], pub f1: u16, pub f2: u16, pub f3: (&'static str, (i16, u8), String, bool), pub f4: &'static str, pub f5: u64 }
    impl<A: Mix> Mix for S1<A> {
        fn mix(&self) {
            feed(6);
            self.f0.mix();
            self.f1.mix();
            self.f2.mix();
            self.f3.mix();
            self.f4.mix();
            self.f5.mix();
        }
    }
    pub trait T2 {
        fn m3(&self, a0: char) -> i32;
        fn m4(&self, a0: (&'static str, (i16, u8), String, bool)) -> u64;
        fn m5(&self, a6: i64) -> (String, Vec<i128>) {
            self.m4(("ff", ((-32768i16), 127u8), String::from("héllo"), true)).mix();
            (String::from("x y"), vec![16384i128])
        }
    }
    impl T2 for char {
        fn m3(&self, a7: char) -> i32 {
            let ((_, p8),): (([u8; 4], [u8; 4]),) = (([0u8, 151u8, 22u8, 3u8], [127u8; 4]),);
            (-18i32).signum().wrapping_rem_euclid(7i32 | 1)
        }
        fn m4(&self, a9: (&'static str, (i16, u8), String, bool)) -> u64 {
            let v10: char = '\'';
            let v11: ((isize, char, i16), u32, (isize, i8, usize), char, usize, &'static str) = (match (vec![String::from("😀"), String::from("héllo"), String::from("x y")].join("-7").as_str()) { "hello" => (match (vec![35u64, 18446744073709551614u64, 8388609u64, 32u64, 7u64].as_slice()) { [p13, .., 39u64..140737488355328u64] if a9.3 => ((0isize, 'a', 1i16), 4294967294u32, (10isize, 100i8, 8usize), '😀', 18014398509481984usize, "-7"), [..=10u64, .., p15] => ((4294967297isize, '9', 8i16), 37u32, (256isize, (-17i8), 1usize), 'z', 4usize, "😀"), [(..=15295692752344892009u64 | 8796093022207u64..12460479180062205638u64 | 11253256760294365187u64), ..=16543901845079140037u64, p17] => (((-3682847505030620912isize), '\u{10FFFF}', 1i16), 256u32, ((-5902696808153036263isize), (-128i8), 7usize), '0', 34usize, "Abc Def"), _ => ((7isize, '中', 4i16), 3u32, (15isize, 0i8, 18446744073709551615usize), '\u{d7ff}', 3usize, "x y") }), _ => ((3isize, 'a', (-8i16)), 4294967294u32, ((-4isize), (-23i8), 524287usize), (*self), 0usize, a9.0) });
            (unsafe { std::mem::transmute::<u16, [u8; 2]>(2u16) }[0] as u64)
        }
    }
    impl T2 for S1<u64> {
        fn m3(&self, a18: char) -> i32 {
            if ((((*self).f3.2.clone(), (*self).f3.2.clone(), String::from("hello"), NonZeroI64::new(100i64).unwrap(), Box::new('\n'), (*self).f0[1]) != (String::from("ff"), String::from("42"), String::from("line\n"), NonZeroI64::new(11i64).unwrap(), Box::new('😀'), 13594018022685989798u64))) {
                let v19: (String, Vec<i128>) = (String::from("-7"), vec![10i128, 170141183460469231731687303715884105727i128, 67856937615624230892935940022005278758i128, (-1i128), (-3i128)]);
                let mut v20: [u64; 3] = [18446744073709551614u64, 24u64, 437988416641733513u64];
                v20.mix();
                let (p22, p23, p24, p25, _, _): (String, String, String, NonZeroI64, Box<char>, u64) = (String::from("-7").to_uppercase(), format!("{:>4}|", (*self).f3.0), (if ((*self).f3.3) { (*self).f3.2.clone() } else { (*self).f3.2.clone() }), NonZeroI64::new((-9223372036854775807i64)).unwrap(), Box::new('Z'), v20[0]);
            }
            let mut v26: NonZeroI64 = larger::<NonZeroI64>((if ((*self).f3.3) { NonZeroI64::new(4095i64).unwrap() } else { NonZeroI64::new(256i64).unwrap() }), larger::<NonZeroI64>(NonZeroI64::new(1073741824i64).unwrap(), NonZeroI64::new(1i64).unwrap()));
            728303545i32.wrapping_rem_euclid(1113433600i32 | 1)
        }
        fn m4(&self, a27: (&'static str, (i16, u8), String, bool)) -> u64 {
            (*self).f0[0]
        }
    }
    impl T2 for i32 {
        fn m3(&self, a28: char) -> i32 {
            (!std::iter::repeat((String::from("Abc Def"), vec![7i128, 649037107316853453566312041152512i128, 2i128, 100i128, 70368744177665i128])).take(1).map(|x29: (String, Vec<i128>)| -> u64 { 16620786192457431278u64 }).fold((*self), |acc30: i32, x31: u64| -> i32 { acc30 })).to_be()
        }
        fn m4(&self, a32: (&'static str, (i16, u8), String, bool)) -> u64 {
            let mut v35: (([u8; 4], [u8; 4]),) = (([183u8; 4], [126u8, 11u8, 9u8, 204u8]),);
            {
                let mut f36 = |a33: Box<String>, a34: (([u8; 4], [u8; 4]),)| -> ((isize, char, i16), u32, (isize, i8, usize), char, usize, &'static str) { v35 = v35; (((-7244509230025871881isize), '\n', a32.1.0), (match (vec![("-7", ((-11163i16), 21u8), String::from(""), true), ("line\n", ((-13387i16), 1u8), String::from("ff"), true)].as_slice()) { [p38, .., p39] => 2496654068u32, _ => 524289u32 }), ((-8isize), 3i8, 0usize), (unsafe { *(&'é' as *const char) }), 562949953421312usize, a32.0.get(3..6).unwrap_or(a32.0)) };
                let mut v40: BTreeMap<u64, (([u8; 4], [u8; 4]),)> = BTreeMap::new();
                v40.insert(id::<u64>((64u64.abs_diff(4097u64) as u64)), (([0u8, 1u8, 115u8, 10u8], [3u8, 127u8, 8u8, 2u8]),)).mix();
                for (key, value) in v40.iter().rev() { key.mix(); value.mix(); }
                v40.len().mix();
                {
                    for armed in [false, true] { caught(|| { let _v = match Some(Noisy(14)) { Some(it) => (trip(armed, Noisy(58)), it), None => (Noisy(18), Noisy(44)) }; }); }
                    for armed in [false, true] { caught(|| { take3(first_of(&Noisy(43), &Noisy(24)).0, Noisy(50), trip(armed, Noisy(32))); }); }
                    for armed in [false, true] { caught(|| { let _v = [Noisy(2), Noisy(43), trip(armed, Noisy(28))]; }); }
                }
                f36(twice::<Box<String>>(&Box::new(String::from("-7"))).1, (([64u8; 4], [15u8; 4]),)).mix();
            }
            v35.mix();
            536870913u64.checked_neg().unwrap_or_default()
        }
    }
    impl T2 for u64 {
        fn m3(&self, a42: char) -> i32 {
            larger::<i32>((-2147483647i32), (-2147483647i32))
        }
        fn m4(&self, a43: (&'static str, (i16, u8), String, bool)) -> u64 {
            if ((-911.6277f32).is_finite()) {
                let v44: (String, String, String, NonZeroI64, Box<char>, u64) = (Vec::<String>::new().join("ff"), a43.2.clone().repeat(1), a43.2.clone(), NonZeroI64::new((-1i64)).unwrap(), ({ let t47: (String, Vec<i128>) = (String::from("-7"), vec![170141183460469231731687303715884105726i128]); Box::new('\0') }), ((*self) & 13u64));
                let f46 = move || -> i64 { let v45 = v44; let out = (3i64.abs_diff(1446468724796532763i64) as i64).wrapping_rem(100i64.checked_add((-7192014087112160196i64)).unwrap_or(262143i64) | 1); drop(v45); out };
                f46().mix();
                let mut v48: [i64; 1] = (unsafe { *(&[(-6615100948430746565i64); 1] as *const [i64; 1]) });
                { let value = twice::<i64>(&(-10i64)).1; let at = (String::from("").len() % 1); unsafe { v48.as_mut_ptr().add(at).write(value) }; }
                v48.mix();
                v48 = ({ let mut items = v48; items.swap(0, 0); items });
            } else {
                {
                    for armed in [false, true] { caught(|| { take3(Noisy(31), trip(armed, Noisy(24)), Noisy(57)); }); }
                    for armed in [false, true] { caught(|| { let _v = { let _local = Noisy(7); (Noisy(8), trip(armed, Noisy(21))) }; }); }
                }
            }
            ((*self).rotate_left(521806978u32) & (*self).wrapping_div_euclid(15178758762649430582u64 | 1))
        }
    }
    #[inline(never)]
    pub fn f54(a55: S1<u64>, a56: i64, a57: &[i32], a58: i64, a59: char, a60: (&'static str, (i16, u8), String, bool), a61: u16, a62: &str) -> ((u128, usize, (), &'static str), Vec<char>) {
        {
            let f63 = || -> u64 { a55.f0[1] };
            let mut v64: u16 = (a61.wrapping_rem_euclid(a61 | 1).trailing_zeros() as u16);
            v64 = u16::from(twice::<u8>(&a60.1.1.checked_neg().unwrap_or_default()).0);
            f63().mix();
        }
        if (a56.is_positive()) {
            {
                let mut v65: String = String::from("hello");
                v65 = twice::<String>(&format!("{:>4}| {} {:?}", '\u{10FFFF}', ' ', a59)).0;
            }
            let mut n66 = 0u32;
            let v68: NonZeroI64 = 'l67: loop {
                n66 += 1;
                if n66 > 3 { break id::<NonZeroI64>(NonZeroI64::new((-5i64)).unwrap()); }
                let mut v69: S1<u64> = (if (a60.3) { a55.clone() } else { a55.clone() });
            };
            {
                let f71 = move |a70: char| -> (&'static str, (i128, u16, isize)) { ("".split(' ').nth(1).unwrap_or("line\n"), ((-18i128), a61, 4611686018427387903isize)) };
                let mut v72: (usize, &'static str, i32, u16) = ((a61 as usize), id::<&'static str>(a55.f4.get(1..7).unwrap_or(a55.f3.0)), ("Abc Def".parse::<i32>().unwrap_or(1i32).leading_ones() as i32), a61);
                v72 = (18446744073709551614usize, "a", 19i32, 512u16);
                v72.3 &= std::array::from_fn::<(&'static str, (i16, u8), String, bool), 2, _>(|i74: usize| -> (&'static str, (i16, u8), String, bool) { a60.clone() }).to_vec().into_iter().filter(|x76: &(&'static str, (i16, u8), String, bool)| -> bool { id::<Option<&'static str>>(Some("hello")).is_none() }).fold((if (a60.3) { a61 } else { a61 }).checked_neg().unwrap_or_default(), |acc78: u16, x79: (&'static str, (i16, u8), String, bool)| -> u16 { ((1i32.leading_zeros() as i32) as u16) });
                f71('😀').mix();
            }
        } else {
            match (vec![3i64].as_slice()) {
                [p82, .., (1i64..=4717093961040038007i64 | ..=13i64)] => {
                    let mut v83: S1<(&'static str, (i16, u8), String, bool)> = S1 { f0: (match (vec![("x y", ((-170141183460469231731687303715884105727i128), 2u16, 70368744177664isize)), ("line\n", ((-89128668510826873155604005480215651743i128), 20u16, 4194304isize)), ("42", (10i128, 1u16, 36028797018963967isize)), ("a,b,,c", (39977557776910600607035937011762087897i128, 39u16, 15isize)), ("-7", (1267650600228229401496703205376i128, 511u16, 128isize))].as_slice()) { [p86 @ .., (p87, ..)] if true => [("hello", ((-31460i16), 255u8), String::from(""), true), ("ff", (23455i16, 121u8), String::from("héllo"), false)], _ => [("-7", (9498i16, 2u8), String::from("hello"), false), ("-7", (12272i16, 2u8), String::from("😀"), true)] }), f1: twice::<u16>(&a61).0, f2: a61.checked_mul(a61).unwrap_or(a61), f3: a60.clone(), f4: (if (true) { "😀" } else { a55.f3.0 }), f5: (!a55.f5) };
                }
                [.., (-9i64 | 0i64..7i64 | ..=9223372036854775806i64)] => {
                    {
                        for armed in [false, true] { caught(|| { let t = (Some(Noisy(39)), Duo { a: Noisy(36), b: Noisy(17) }); trip(armed, ()); let _x = t.1.b; let _y = t.0; let _late = Noisy(33); }); }
                        for armed in [false, true] { caught(|| { let t = [Noisy(39), Noisy(36), Noisy(17)]; let [_, _x @ ..] = t; trip(armed, ()); let _late = Noisy(48); }); }
                    }
                }
                [-9223372036854775808i64..=5167007451531145697i64, .., p90] => {
                    let v91: char = vec![String::from("line\n")].join("héllo").chars().rev().nth(1).unwrap_or(twice::<char>(&'_').0);
                }
                _ => twice::<NonZeroI64>(&NonZeroI64::new(9223372036854775806i64).unwrap()).1.mix(),
            }
        }
        let v92: i64 = a58.checked_sub(a58).unwrap_or((-4i64)).reverse_bits();
        ((29u128, 10330236887177769956usize, (), a60.0), ({ let mut items = vec!['\u{d7ff}', '\u{d7ff}']; items.reverse(); items }))
    }
    #[inline(never)]
    pub fn f95<G94: Mix + Clone + Debug + PartialOrd>(a96: G94) -> i16 {
        if let [] = (({ let mut items = vec![((39u128, 8usize, (), "ff"), vec!['中', '\n', 'z', 'Z', '0'])]; items.reverse(); items }).as_slice()) {
            {
                let v99: (&'static str, (i16, u8), String, bool) = ("ff", (1i16, 189u8), String::from("😀"), false);
                let mut v100: BTreeMap<char, ((bool, bool, NonZeroU8, i16), char, String, i128)> = BTreeMap::new();
                v100.insert(twice::<char>(&String::from("héllo").chars().nth(2).unwrap_or('Z')).0, ({ let t101: (&'static str, (i16, u8), String, bool) = (v99.0, v99.1, v99.2.clone(), v99.3); ((false, false, NonZeroU8::new(7u8).unwrap(), 0i16), '\0', String::from("-7"), 17i128) })).mix();
                v100.insert(id::<char>('Z'), ((false, v99.3, NonZeroU8::new(19u8).unwrap(), 7i16), (match (vec!["ff", "😀", ""].as_slice()) { [_, .., p103] if v99.3 => 'ß', _ => 'ß' }), ({ let mut text = v99.2.clone(); text.push('_'); text }), 13345364489612951718363091651945190970i128.checked_neg().unwrap_or_default())).mix();
                for (key, value) in v100.iter().rev() { key.mix(); value.mix(); }
                v100.len().mix();
            }
            let mut v105: (&'static str, u128, char, &'static str, (i8, u32, i32, NonZeroU8), bool) = ((match (({ let mut items = vec![(String::from("0123456789"), String::from("a,b,,c"), String::from("a,b,,c"), NonZeroI64::new(4097i64).unwrap(), Box::new('\u{80}'), 32u64), (String::from("hello"), String::from("x y"), String::from("héllo"), NonZeroI64::new(4611686018427387903i64).unwrap(), Box::new('\n'), 6u64)]; items.retain(|x107: &(String, String, String, NonZeroI64, Box<char>, u64)| -> bool { true }); items }).as_slice()) { [(_, ..), .., (p109, ..)] if sel::<bool>(false, true, true) => (loop { break "3.5"; }), [.., (_, _, _, _, p111, 40u64..=257u64)] => "  pad  ".trim(), [.., (p113, p114, _, p115, p116, 17u64..15436819532105483718u64)] => "a", _ => (match (String::from("3.5").as_str()) { "ff" => "line\n", _ => "line\n" }) }), 7u128.wrapping_sub(230284551709350466226582499436846372059u128).wrapping_shr(536870913u32.checked_add(34u32).unwrap_or(4294967294u32)), id::<String>(String::from("héllo")).chars().last().unwrap_or(id::<char>('Z')), "héllo", larger::<(i8, u32, i32, NonZeroU8)>((6i8, 256u32, (-2147483648i32), NonZeroU8::new(100u8).unwrap()), ((-2i8), 2147483647u32, (-673854350i32), NonZeroU8::new(22u8).unwrap())), 1e30f64.sqrt().is_normal());
            {
                let mut f106 = |a104: ((isize, char, i16), u32, (isize, i8, usize), char, usize, &'static str)| -> (&'static str, (i128, u16, isize)) { v105 = (v105.3, id::<u128>(v105.1), String::from("hello").chars().last().unwrap_or('9'), twice::<&'static str>(&"0123456789").0, (a104.2.1, v105.4.1, v105.4.2, v105.4.3), None::<u64>.is_some()); (v105.3.split(' ').nth(1).unwrap_or(v105.3), (64i128, 1u16, a104.2.0)) };
                let v117: (&'static str, u128, char, &'static str, (i8, u32, i32, NonZeroU8), bool) = twice::<(&'static str, u128, char, &'static str, (i8, u32, i32, NonZeroU8), bool)>(&("a,b,,c", 10u128, '0', "  pad  ", ((-22i8), 9u32, 100i32, NonZeroU8::new(117u8).unwrap()), false)).1;
                {
                    let mut items: Vec<Box<dyn Any>> = vec![Box::new(1.5f64), Box::new(()), Box::new(Some(24i64)), Box::new((24u8, true)), Box::new([31u16; 3])];
                    for item in &items { if let Some(it) = item.downcast_ref::<u32>() { feed(*it as u64); } else if let Some(it) = item.downcast_ref::<&str>() { it.mix(); } else if item.is::<Noisy>() { feed(1); } else if let Some(it) = item.downcast_ref::<(u8, bool)>() { feed(it.0 as u64 + it.1 as u64); } else { feed(2); } }
                    items.retain(|it| !it.is::<f64>()); feed(items.len() as u64);
                }
                f106(id::<((isize, char, i16), u32, (isize, i8, usize), char, usize, &'static str)>((((-7097181891160269502isize), '\u{7f}', 6843i16), 2u32, ((-16isize), 7i8, 27usize), 'é', 13usize, "héllo"))).mix();
            }
            v105.mix();
        }
        (if (false) { (-1i16) } else { 7i16 }).rotate_left(5u32.wrapping_rem(67108865u32 | 1))
    }
    #[inline(never)]
    pub fn f118(a119: (&'static str, (i16, u8), String, bool), a120: i128, a121: bool) -> impl Fn() -> ((&'static str,), u64, u16) {
        move || -> ((&'static str,), u64, u16) { (unsafe { *(&(if (a119.3) { (("Abc Def",), 100u64, 8191u16) } else { (("  pad  ",), 17u64, 35u16) }) as *const ((&'static str,), u64, u16)) }) }
    }
    #[inline(always)]
    pub fn f122(a123: i16, a124: (NonZeroU32, Vec<i16>), a125: u16, a126: bool) -> NonZeroI64 {
        let mut v127: (([u8; 4], [u8; 4]),) = (([40u8, 35u8, 64u8, 57u8], [116u8; 4]),);
        {
            v127 = (([16u8; 4], [33u8; 4]),);
            if ((((159931869944120718353370504580804933103u128, 7288658725784472704usize, (), "hello"), vec!['\0', '\'', '\n']) <= (if (a126) { ((4503599627370495u128, 1024usize, (), "héllo"), Vec::<char>::new()) } else { ((7u128, 10usize, (), "a,b,,c"), vec!['\u{d7ff}', 'a']) }))) {
                v127 = (([24u8, 127u8, 6u8, 255u8], [10u8; 4]),);
                let v128: (([u8; 4], [u8; 4]),) = std::mem::replace(&mut v127, (([64u8, 129u8, 16u8, 128u8], [64u8, 127u8, 8u8, 21u8]),));
                let mut v129: [i32; 1] = ({ let mut items = larger::<[i32; 1]>([19i32], [727582492i32]); items.reverse(); items });
                (unsafe { *v129.as_ptr().add(0) }).mix();
                v129.mix();
            } else {
                v127 = ((v127.0.1, ({ let mut items = v127.0.0; items.rotate_left(3); items })),);
            }
            let v137: S1<(&'static str, (i16, u8), String, bool)> = S1 { f0: (match ([("-7", ((-12550i16), 33u8), String::from("line\n"), false)].to_vec().as_slice()) { [(p144, (p145, p146), p147, true), .., (_, (p148, _), p149, true)] => (loop { break [("  pad  ", (32i16, 1u8), String::from("3.5"), true), ("-7", (19i16, 9u8), String::from("héllo"), false)]; }), [_, .., (p153, (p154, p155), p156, p157)] => ({ let mut items = [("😀", (8193i16, 2u8), String::from("😀"), true), ("ff", (2i16, 11u8), String::from("x y"), false)]; items.sort_unstable(); items }), _ => S1 { f0: [("-7", (4096i16, 236u8), String::from("line\n"), false), ("  pad  ", (16383i16, 8u8), String::from("line\n"), true)], f1: 13217u16, f2: 8388u16, f3: ("a,b,,c", (7i16, 9u8), String::from("  pad  "), false), f4: "ff", f5: 2599572512526005410u64 }.f0 }), f1: 8192u16.checked_add(('b162: { if (true) { break 'b162 60581u16; } a125 })).unwrap_or((a125.leading_zeros() as u16)), f2: (137438953471isize.wrapping_rem_euclid(2199023255552isize | 1) as u16), f3: (if (larger::<bool>(false, a126)) { ("Abc Def", (3i16, 104u8), String::from("Abc Def"), false) } else { ("a,b,,c", ((-5i16), 33u8), String::from("42"), a126) }), f4: "-7".get(0..7).unwrap_or("x y").trim_start(), f5: 10u64.wrapping_rem(6392804031246972977u64 | 1).overflowing_shr((38u32.checked_ilog2().unwrap_or(0) as u32)).0 };
            let f139 = move || -> ((&'static str,), u64, u16) { let v138 = v137; let out = ((v138.f3.0,), (&1024u64 as &dyn T2).m4(("Abc Def", (127i16, 127u8), String::from("x y"), false)), a125.checked_div(28528u16).unwrap_or(v138.f1)); drop(v138); out };
            f139().mix();
            id::<i16>(a123).mix();
        }
        (if ((unsafe { *(&false as *const bool) })) { NonZeroI64::new(70368744177663i64).unwrap_or(NonZeroI64::new(9223372036854775806i64).unwrap()) } else { NonZeroI64::new(34359738369i64).unwrap_or(NonZeroI64::new(9i64).unwrap()) })
    }
    #[inline(never)]
    pub fn f164<G163: Mix + Clone + Debug + PartialOrd>(a165: G163, a166: Box<String>) -> G163 {
        match (vec![(NonZeroU32::new(4294967295u32).unwrap(),), (NonZeroU32::new(33u32).unwrap(),), (NonZeroU32::new(536870913u32).unwrap(),), (NonZeroU32::new(7u32).unwrap(),)].as_slice()) {
            [.., (p169,)] => {
                let mut v172: bool = ((-f32::NAN) - (f64::INFINITY as f32)).is_nan();
                {
                    let mut f173 = |a170: u64, a171: u16| -> char { v172 = ((false ^ v172) < (if (v172) { v172 } else { v172 })); twice::<char>(&(*a166).clone().chars().nth(2).unwrap_or('z')).0 };
                    let v174: u16 = (if ((({ let t175: ((), (bool, i8), String, String, (NonZeroI64, i16, char), (NonZeroU8, i16)) = ((), (true, 19i8), String::from("héllo"), String::from("😀"), (NonZeroI64::new((-5978409721688939101i64)).unwrap(), 14i16, 'z'), (NonZeroU8::new(31u8).unwrap(), 32766i16)); Box::new(()) }) <= id::<Box<()>>(Box::new(())))) { 10u16.wrapping_sub(52599u16).overflowing_add(u16::from_str_radix("x y", 8).unwrap_or(40480u16)).0 } else { (unsafe { std::mem::transmute::<u32, u32>(40u32) } as u16).wrapping_div(26u16 | 1) });
                    let v176: NonZeroU32 = (*p169);
                    f173((unsafe { *(&12269282571099349479u64 as *const u64) }), 38496u16.checked_div(3u16).unwrap_or(11u16)).mix();
                }
                v172.mix();
                let mut v178: usize = (unsafe { *(&3usize as *const usize) }).checked_div(9usize).unwrap_or((*a166).clone().len());
                {
                    let mut f179 = |a177: ((u128, usize, (), &'static str), Vec<char>)| -> String { v178 = ((*a166).clone().len() as usize); ({ let mut items = vec![String::from("3.5"), String::from("x y"), String::from(""), String::from("x y"), String::from("-7")]; items.retain(|x180: &String| -> bool { true }); items }).join("Abc Def") };
                    let v181: Box<String> = larger::<Box<String>>((match (String::from("hello").as_str()) { "" if v172 => a166.clone(), "😀" => a166.clone(), _ => a166.clone() }), ('b182: { if (v172) { break 'b182 a166.clone(); } Box::new(String::from("hello")) }));
                    let mut v183: u16 = 38u16;
                    let mut v184: String = (*a166).clone();
                    f179(((173207536102028857067695845059580438446u128, 8589934591usize, (), "  pad  "), vec!['_'])).mix();
                }
                v178.mix();
            }
            _ => {}
        }
        let (_, _, _, p185, (p186, _, p187, _), p188): (&'static str, u128, char, &'static str, (i8, u32, i32, NonZeroU8), bool) = ("".get(2..2).unwrap_or("a,b,,c"), 1099511627777u128.checked_div(10u128).unwrap_or(22u128), (*a166).clone().chars().rev().nth(1).unwrap_or('中'), "hello".trim(), (8i8, 19u32, 1073741825i32, NonZeroU8::new(12u8).unwrap()), (("42", ((-170141183460469231731687303715884105728i128), 29064u16, 14isize)) >= ("-7", (10i128, 65535u16, (-8isize)))));
        (if (p188) { a165.clone() } else { a165.clone() })
    }
    pub fn f189(a190: u64, a191: bool, a192: String) -> Box<()> {
        match (vec![(170141183460469231731687303715884105727i128, NonZeroI64::new((-4638473290364693070i64)).unwrap()); (511usize.wrapping_rem_euclid(6usize | 1) % 6)].as_slice()) {
            [.., p195] if (unsafe { *(&false as *const bool) }) => {
                {
                    {
                        let mut items: Vec<Box<dyn Any>> = vec![Box::new(String::from("owned")), Box::new(()), Box::new("text"), Box::new([52u16; 3]), Box::new(Noisy(52))];
                        for item in &items { if let Some(it) = item.downcast_ref::<u32>() { feed(*it as u64); } else if let Some(it) = item.downcast_ref::<&str>() { it.mix(); } else if item.is::<Noisy>() { feed(1); } else if let Some(it) = item.downcast_ref::<(u8, bool)>() { feed(it.0 as u64 + it.1 as u64); } else { feed(2); } }
                        while let Some(item) = items.pop() { match item.downcast::<Noisy>() { Ok(it) => it.mix(), Err(other) => feed(other.is::<String>() as u64) } }
                    }
                    let mut v196: [isize; 4] = <[isize; 4]>::try_from(({ let mut items = vec![36028797018963967isize, 18isize, (-1isize), 8isize]; items.sort_by(|a, b| b.cmp(a)); items })).unwrap_or(({ let mut items = (match (vec!["3.5"].as_slice()) { [p207, p206 @ ..] if false => [6isize, 20isize, (-9isize), (-15isize)], [p209, .., _] if true => [(-7isize); 4], _ => [(-14isize); 4] }); items.reverse(); items }));
                    v196.mix();
                    let mut v210: [i64; 1] = ({ let mut items = [5881150332997549328i64]; items.sort_unstable(); items });
                    { let (x, y) = ((3usize.wrapping_add(7usize) % 1), (None::<()>.into_iter().filter_map(|x212: ()| -> Option<u64> { Some(24u64) }).zip(vec![32u64, 5259237838086745439u64, 19u64, 15098908156768477869u64, 2u64]).count() % 1)); let start = v210.as_mut_ptr(); unsafe { std::ptr::swap(start.add(x), start.add(y)) }; }
                    v210.mix();
                    {
                        struct Outer(Noisy, Bomb, Noisy);
                        impl Drop for Outer { fn drop(&mut self) { feed(0x0d0d + self.0.0 as u64); } }
                        caught(|| { let _a = Noisy(16); { let _inner = Box::new((Bomb(37), Noisy(38))); let _late = Noisy(32); } let _z = Noisy(49); });
                    }
                }
                {
                    let ((), ..): ((), (bool, i8), String, String, (NonZeroI64, i16, char), (NonZeroU8, i16)) = ((), (false, 0i8), a192.clone(), a192.clone(), (NonZeroI64::new((-19i64)).unwrap(), 8i16, '\n'), (NonZeroU8::new(1u8).unwrap(), 512i16));
                    let mut v216: bool = ((String::from("3.5"), String::from("héllo"), String::from("a,b,,c"), NonZeroI64::new((-19i64)).unwrap(), Box::new('Z'), 10u64) < (String::from("42"), String::from("  pad  "), String::from(""), NonZeroI64::new(4611686018427387903i64).unwrap(), Box::new('9'), 1u64));
                    let v217: bool = std::mem::replace(&mut v216, Some(((), (false, 43i8), String::from("Abc Def"), String::from("Abc Def"), (NonZeroI64::new(9007199254740992i64).unwrap(), 33i16, 'z'), (NonZeroU8::new(14u8).unwrap(), (-4i16)))).is_some());
                    {
                        struct Outer(Noisy, Bomb, Noisy);
                        impl Drop for Outer { fn drop(&mut self) { feed(0x0d0d + self.0.0 as u64); } }
                        caught(|| { let _a = Noisy(17); let _h0 = Box::new((Bomb(52), Noisy(45))); let _z = Noisy(33); });
                        caught(|| { let _a = Noisy(58); { let _inner = (Noisy(34), [Bomb(32)], Box::new(Noisy(10))); let _late = Noisy(51); } let _z = Noisy(46); });
                    }
                }
            }
            _ => {}
        }
        let [p222, .., p223]: [u8; 4] = (if (a191) { (unsafe { *(&[16u8, 120u8, 2u8, 16u8] as *const [u8; 4]) }) } else { [16u8, 4u8, 4u8, 64u8] });
        f164::<Box<()>>((if (a191) { (match (vec![NonZeroU8::new(39u8).unwrap()].as_slice()) { [_, .., _] => Box::new(()), [p226, .., p227] => Box::new(()), _ => Box::new(()) }) } else { ('b228: { if (false) { break 'b228 Box::new(()); } Box::new(()) }) }), (if (None::<(&'static str,)>.is_some()) { f164::<Box<String>>(Box::new(String::from("42")), Box::new(String::from("a"))) } else { twice::<Box<String>>(&Box::new(String::from("héllo"))).0 }))
    }
    pub fn run() {
        {
            let f230 = |a229: &'static str| -> bool { (unsafe { *(&2i16 as *const i16) }).is_positive() };
            let v231: ((), (bool, i8), String, String, (NonZeroI64, i16, char), (NonZeroU8, i16)) = (if ((match (Vec::<u128>::new().as_slice()) { [] => false, _ => false })) { ((), (true, 1i8), String::from("3.5"), String::from("x y"), (NonZeroI64::new(4i64).unwrap(), (-32768i16), ' '), (NonZeroU8::new(30u8).unwrap(), (-6063i16))) } else { sel::<((), (bool, i8), String, String, (NonZeroI64, i16, char), (NonZeroU8, i16))>(false, ((), (true, (-15i8)), String::from("a,b,,c"), String::from(""), (NonZeroI64::new(1376309402658870273i64).unwrap(), (-25501i16), '0'), (NonZeroU8::new(30u8).unwrap(), 17i16)), ((), (true, (-6i8)), String::from("42"), String::from("hello"), (NonZeroI64::new((-9223372036854775808i64)).unwrap(), 2i16, '\''), (NonZeroU8::new(254u8).unwrap(), 1i16))) });
            let mut n233 = 0u32;
            'l234: loop {
                n233 += 1;
                if n233 > 1 { break; }
                let v236: i64 = (-5630521140727797417i64);
            }
            f230(f164::<&'static str>("hello", Box::new(String::from("  pad  ")))).mix();
        }
        {
            static NUMBERS: [u32; 4] = [31, 29, 13, 4];
            static SECOND: &u32 = &NUMBERS[1];
            static TAIL: &[u32] = match &NUMBERS { [_, _, rest @ ..] => rest };
            static WORDS: [&str; 3] = ["one", "two", "three"];
            static PICKED: &&str = &WORDS[2];
            struct Link { value: u32, next: Option<&'static Link>, call: fn(u32) -> u32, name: &'static str, shown: &'static (dyn Debug + Sync) }
            fn double(x: u32) -> u32 { x.wrapping_mul(2) }
            static END: Link = Link { value: 3, next: None, call: double, name: "end", shown: &NUMBERS };
            static MID: Link = Link { value: 31, next: Some(&END), call: |x| x + 1, name: WORDS[0], shown: &"mid" };
            static HEAD: Link = Link { value: 29, next: Some(&MID), call: double, name: "head", shown: &MID.value };
            static TABLE: [fn(u32) -> u32; 3] = [double, |x| x ^ 5, HEAD.call];
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            static LISTS: (&[u8], &[&[u8]], [Option<&u32>; 2]) = (b"bytes", &[b"a", b"", &[31, 2]], [Some(&NUMBERS[3]), None]);
            const VIEW: &[u32; 4] = &NUMBERS;
            thread_local! { static LOCAL: Cell<u32> = const { Cell::new(5) }; static NOTES: RefCell<Vec<&'static u32>> = const { RefCell::new(Vec::new()) }; }
            { let mut at = Some(&HEAD); while let Some(link) = at { feed((link.call)(link.value) as u64); link.name.mix(); format!("{:?}", link.shown).mix(); at = link.next; } }
            feed(*SECOND as u64 + TAIL.len() as u64 + std::ptr::eq(SECOND, &NUMBERS[1]) as u64 * 16 + std::ptr::eq(TAIL.as_ptr(), &NUMBERS[2]) as u64 * 32);
            { let before = COUNTER.fetch_add(3, Ordering::Relaxed); feed((COUNTER.load(Ordering::Relaxed) - before) as u64); }
            PICKED.mix(); feed(std::ptr::eq(*PICKED, WORDS[2]) as u64);
        }
        let mut n237 = 0u32;
        'l238: loop {
            n237 += 1;
            if n237 > 8 { break; }
            let v240: u16 = 8192u16;
            let mut v241: S1<(&'static str, (i16, u8), String, bool)> = id::<S1<(&'static str, (i16, u8), String, bool)>>(sel::<S1<(&'static str, (i16, u8), String, bool)>>(true, S1 { f0: [("line\n", ((-17i16), 252u8), String::from("line\n"), false), ("hello", (3i16, 29u8), String::from("Abc Def"), false)], f1: 64u16, f2: 65534u16, f3: ("0123456789", (2i16, 14u8), String::from("ff"), false), f4: "  pad  ", f5: 25u64 }, S1 { f0: [("héllo", (32766i16, 13u8), String::from("Abc Def"), false), ("ff", (10i16, 2u8), String::from("42"), true)], f1: 2u16, f2: 4u16, f3: ("hello", (100i16, 1u8), String::from("a"), true), f4: "0123456789", f5: 1580509438637208978u64 }));
        }
        if (((Vec::<i128>::new() <= vec![85070591730234615865843651857942052863i128]) | larger::<bool>(false, false))) {
            if (true) {
                let mut v242: [(); 2] = [(); 2];
                { let v243: [(); 2] = (match (String::from("Abc Def".get(1..5).unwrap_or("Abc Def")).as_str()) { "a,b,,c" => f164::<[(); 2]>([(); 2], larger::<Box<String>>(Box::new(String::from("3.5")), Box::new(String::from("a")))), _ => f164::<[(); 2]>(id::<[(); 2]>([(), ()]), id::<Box<String>>(Box::new(String::from("hello")))) }); unsafe { std::ptr::copy_nonoverlapping(v243.as_ptr(), v242.as_mut_ptr(), 2) }; }
                v242.mix();
                match &mut v242 {
                    [(), .., _] => {
                        {
                            async fn work(seed: u32) -> u64 { let h252 = format!("{seed}-22"); let w253 = later(2, seed as u64 + 42).await; let h254: Option<Box<u16>> = if seed % 2 == 0 { Some(Box::new(7)) } else { None }; let w255 = later(0, [seed as u64; 2]).await[0]; let h256 = [seed as u8; 1]; let w257 = match later(1, seed % 3).await { 0 => later(0, 10u64).await, 1 => 11, _ => later(2, 12u64).await }; (h252.len() as u64).rotate_left(0) ^ (w253).rotate_left(1) ^ (h254.as_deref().copied().unwrap_or(1) as u64).rotate_left(2) ^ (w255).rotate_left(3) ^ (h256[0] as u64).rotate_left(4) ^ (w257).rotate_left(5) }
                            poll_some(work(59), 6).mix();
                            poll_some(work(28), 6).mix();
                        }
                    }
                    _ => {}
                }
            } else {
                {
                    let v258: NonZeroI64 = (match (vec![1152921504606846976u64].as_slice()) { [(18446744073709551614u64 | ..=9u64 | 13833968532029689199u64), (9223372036854775807u64 | 3172153442252196142u64)] => NonZeroI64::new(9223372036854775807i64).unwrap_or(NonZeroI64::new(1i64).unwrap()), _ => NonZeroI64::new(2019519267072283792i64).unwrap() });
                    let v260: NonZeroI64 = NonZeroI64::new((-9i64).checked_add(274877906943i64).unwrap_or(17179869183i64).wrapping_neg()).unwrap_or(larger::<NonZeroI64>((match (vec![Box::new(()), Box::new(()), Box::new(())].as_slice()) { [] => v258, [_, .., _] => v258, _ => v258 }), f122(7325i16, (NonZeroU32::new(9u32).unwrap(), vec![32767i16, 8i16, 7i16, 15452i16, 8i16]), 65535u16, false)));
                }
                let mut n263 = 0u32;
                let v265: S1<(&'static str, (i16, u8), String, bool)> = 'l264: loop {
                    n263 += 1;
                    if n263 > 2 { break S1 { f0: [("3.5", (18i16, 1u8), String::from("-7"), true), ("", ((-17i16), 7u8), String::from("42"), false)], f1: 7u16, f2: 65535u16, f3: ("a", ((-13742i16), 127u8), String::from("😀"), false), f4: "line\n", f5: 1096375833072487577u64 }; }
                    let mut v266: Vec<(u64, u32, u16)> = ({ let mut items = ({ let mut items = vec![(536870912u64, 4294967295u32, 54773u16), (4503599627370496u64, 2005401232u32, 7u16), (15u64, 5u32, 25454u16)]; items.push((37u64, 40u32, 12522u16)); items }); items.sort_unstable(); items });
                    {
                        for armed in [false, true] { caught(|| { take3(first_of(&Noisy(9), &Noisy(13)).0, Noisy(29), trip(armed, Noisy(27))); }); }
                        for armed in [false, true] { caught(|| { let mut held = vec![Noisy(28)]; held.push(trip(armed, Noisy(43))); held.insert(0, Noisy(47)); }); }
                        for armed in [false, true] { caught(|| { let _v = match Some(Noisy(11)) { Some(it) => (trip(armed, Noisy(12)), it), None => (Noisy(28), Noisy(44)) }; }); }
                        for armed in [false, true] { caught(|| { let _v = Some(Noisy(19)).map(|it| (trip(armed, Noisy(31)), it)); }); }
                    }
                };
            }
            {
                match ([vec![8589934593u64], vec![16u64]].concat().as_slice()) {
                    [17u64..8358266131987336983u64, .., ..=11275075181517471667u64] => {
                        {
                            for armed in [false, true] { caught(|| { let mut kept = Vec::new(); let mut i = 0u32; while i < 4 { let n = Noisy(i); if i == 2 { trip(armed, ()); } if i % 2 == 1 { kept.push(n); i += 1; continue; } let _m = Noisy(10 + i); if i == 1 { break; } i += 1; } feed(kept.len() as u64); }); }
                        }
                    }
                    _ => (NonZeroU32::new(25u32).unwrap(),).mix(),
                }
                {
                    trait Shape { fn area(&self) -> u64; fn scaled(&self, by: u64) -> u64 where Self: Sized { self.area() * by } }
                    trait Conv<T> { fn conv(&self) -> T; }
                    struct Sq(u64); struct Rect(u64, u64);
                    impl Shape for Sq { fn area(&self) -> u64 { self.0 * self.0 } }
                    impl Shape for Rect { fn area(&self) -> u64 { self.0 * self.1 } }
                    impl<T: Shape + ?Sized> Shape for Box<T> { fn area(&self) -> u64 { (**self).area() + 1 } }
                    impl Conv<u64> for Sq { fn conv(&self) -> u64 { self.0 } } impl Conv<String> for Sq { fn conv(&self) -> String { self.0.to_string() } }
                    impl dyn Shape { fn doubled(&self) -> u64 { self.area() * 2 } }
                    fn total<T: Shape + ?Sized>(it: &T) -> u64 { it.area() }
                    fn sum(shapes: &[&dyn Shape]) -> u64 { shapes.iter().map(|it| total(*it)).sum() }
                    let (sq, rect) = (Sq(36), Rect(36, 49));
                    feed(sum(&[&sq, &rect, &Sq(2)]));
                    { let mut calls = 0u64; let mut bump: Box<dyn FnMut(u64) -> u64> = Box::new(|by| { calls += by; calls }); bump(2); feed(bump(5)); }
                    feed(total(&sq) + total(&rect as &dyn Shape) + sq.scaled(3));
                    { let nested: Box<Box<dyn Shape>> = Box::new(Box::new(Sq(3))); feed(nested.area()); }
                    { let boxed: Box<dyn Shape> = Box::new(Rect(2, 3)); feed(total(&boxed) + boxed.doubled() + total(&*boxed)); }
                }
                let mut v271: bool = ("ff", (512i16, 13u8), String::from("  pad  "), true).3;
            }
        }
        let v272: (i64, u16) = (9223372036854775806i64.checked_sub(19i64).unwrap_or((5740993855784652940i64, 7u16).0), (86u8 as u16).wrapping_rem_euclid((false as u16) | 1));
        if let [(0u64..=18446744073709551615u64,), p274 @ ..] = (id::<Vec<(u64,)>>(vec![(100u64,), (18446744073709551615u64,), (3u64,), (12435728908878968681u64,), (8388608u64,)]).as_slice()) {
            let mut v275: BTreeMap<(String, Vec<i128>), Box<()>> = BTreeMap::new();
            v275.insert((String::from("line\n"), vec![71082831263855638380421968465703191237i128, 122134876049378784325044968116280703015i128, 7i128, 170141183460469231731687303715884105726i128, 85070591730234615865843651857942052863i128]), id::<Box<()>>(larger::<Box<()>>(Box::new(()), Box::new(())))).mix();
            v275.insert((String::from("x y"), ({ let mut items = Vec::<i128>::new(); items.sort_unstable(); items })), f189(0u64.checked_add(2u64).unwrap_or(3001718226445475805u64), (S1 { f0: [18446744073709551615u64, 14903570190600934292u64], f1: 9u16, f2: 40u16, f3: ("x y", (4096i16, 63u8), String::from("line\n"), true), f4: "line\n", f5: 8117595741544670776u64 } > S1 { f0: [0u64, 37u64], f1: 9u16, f2: 37u16, f3: ("héllo", (6i16, 7u8), String::from("3.5"), false), f4: "Abc Def", f5: 2147483648u64 }), String::from("héllo"))).mix();
            v275.insert((String::from("héllo"), Vec::<i128>::new()), (if ((match (vec![(7u128,)].as_slice()) { [p277 @ .., _] => true, [p278 @ .., (35u128..=267672723882900911535094467749275223774u128,)] => false, _ => true })) { (if (true) { Box::new(()) } else { Box::new(()) }) } else { (match (vec![(21usize, "Abc Def", (-2147483647i32), 8u16)].as_slice()) { [(_, ..), .., (_, _, p280 @ ..=256i32, 24u16..=23558u16)] => Box::new(()), _ => Box::new(()) }) })).mix();
            v275.insert((String::from("ff"), ({ let mut items = vec![(-166953688933191579158138508195638840683i128)]; items.sort_unstable(); items })), (loop { break (if (true) { Box::new(()) } else { Box::new(()) }); })).mix();
            v275.first_key_value().map(|(key, _)| key.clone()).mix();
            v275.len().mix();
            if (None::<(usize, &'static str, i32, u16)>.and_then(|x282: (usize, &'static str, i32, u16)| -> Option<((&'static str,), u64, u16)> { Some((("  pad  ",), 31u64, 512u16)) }).is_some()) {
                let mut v283: i32 = (&(2905061348835499408u64.overflowing_add(4294967297u64).1 as u64) as &dyn T2).m3(String::from("Abc Def").chars().nth(2).unwrap_or('Z'));
                v283 = larger::<i32>(v283.wrapping_div((-1i32) | 1), (unsafe { *(&v283 as *const i32) }));
                if (((true <= false) ^ (21usize != p274.len()))) {
                    v283 |= (unsafe { std::mem::transmute::<i128, [u8; 16]>(74991148259261459136414239905269411549i128.max(0i128)) }[1] as i32).wrapping_rem((0i128 as i32).checked_sub(T2::m3(&S1 { f0: [21u64, 39u64], f1: 8u16, f2: 9u16, f3: ("3.5", (1i16, 127u8), String::from("a,b,,c"), true), f4: "3.5", f5: 12841410297352338411u64 }, '9')).unwrap_or(128i32.wrapping_div_euclid(v283 | 1)) | 1);
                } else {
                    let v284: ((), (bool, i8), String, String, (NonZeroI64, i16, char), (NonZeroU8, i16)) = ((), (false, 17i8), String::from(""), String::from("hello"), (NonZeroI64::new((-1i64)).unwrap(), (-20177i16), 'a'), (NonZeroU8::new(8u8).unwrap(), (-32768i16)));
                }
            }
        } else {
            let mut v285: [i16; 4] = larger::<[i16; 4]>((match (String::from("0123456789").as_str()) { "0123456789" => [7i16, (-1i16), (-32768i16), 6i16], _ => [128i16, 511i16, (-1i16), 14i16] }), twice::<[i16; 4]>(&[(-32767i16); 4]).0);
            { let (x, y) = (0, ((0i32..4i32).fold(100usize, |acc287: usize, x288: i32| -> usize { acc287 }) % 4)); let start = v285.as_mut_ptr(); unsafe { std::ptr::swap(start.add(x), start.add(y)) }; }
            v285.mix();
        }
        match (({ let mut items = Vec::<i64>::new(); items.sort_unstable(); items }).as_slice()) {
            [-5i64..=7025606067693101305i64, .., (-7640591587381565412i64..=7i64 | -11i64..=2199023255553i64 | 3657431934438231099i64)] if true => {
                let mut v297: [u16; 8] = id::<[u16; 8]>([32767u16, 0u16, 4096u16, 7u16, 1u16, 36u16, 22u16, 40u16]);
                (unsafe { *v297.as_ptr().add(3) }).mix();
                v297.mix();
            }
            [p302 @ 9i64..=6043970613535983320i64, p301 @ ..] if (-406.4809f32).is_normal() => {
                let v303: (i64, u16) = (larger::<i64>(1i64, v272.0), v272.1.to_be());
                let v304: char = String::from_utf8_lossy(&({ let mut items = Vec::<u8>::new(); items.reverse(); items })).into_owned().chars().last().unwrap_or(larger::<char>(String::from("  pad  ").chars().next().unwrap_or('\0'), String::from("héllo").chars().rev().nth(1).unwrap_or('\'')));
            }
            _ => ({ let t306: (&'static str, u128, char, &'static str, (i8, u32, i32, NonZeroU8), bool) = ("a,b,,c", 2u128, '\u{80}', "0123456789", ((-23i8), 3917923391u32, 7i32, NonZeroU8::new(1u8).unwrap()), false); ((17179869183u128, 8usize, (), ""), vec!['\u{80}']) }).mix(),
        }
        let v307: (NonZeroU8, usize) = (f164::<NonZeroU8>(NonZeroU8::new(100u8).unwrap(), id::<Box<String>>(Box::new(String::from("a,b,,c")))), ({ let mut items = vec![vec![10i128], vec![(-36413113122033958370856123157311683044i128), 6266291706766194515960911114487061797i128, 85070591730234615865843651857942052863i128], vec![1i128, (-4i128), 1i128], Vec::<i128>::new(), vec![(-18i128)]]; items.sort(); items }).into_iter().zip(vec![(15i64, 4097u16), (10i64, 16u16)]).count());
        let v310: ((u128, usize, (), &'static str), Vec<char>) = ((match (Vec::<char>::new().as_slice()) { ['\u{80}'..='\u{10FFFF}', .., p312] => (123282197747589046115677977870197388350u128, 1054071252395083939usize, (), "-7"), [p314 @ 'A'..='Z', .., p315] => (549755813887u128, 4398046511104usize, (), "3.5"), _ => (340282366920938463463374607431768211455u128, 18446744073709551615usize, (), "  pad  ") }), String::from("").chars().collect::<Vec<char>>());
        let mut v317: [(); 4] = larger::<[(); 4]>(larger::<[(); 4]>([(), (), (), ()], [(); 4]), std::array::from_fn::<(), 4, _>(|i323: usize| -> () { v310.0.2 }));
        (unsafe { *v317.as_ptr().add((v310.0.1 % 4)) }).mix();
        v317.mix();
        v317 = ({ let mut items = twice::<[(); 4]>(&[(), (), (), ()]).1; items.swap(0, 3); items });
        let ((p329,),): (([u8; 4],),) = (([3u8, 28u8, 9u8, 8u8],),);
        if (None::<(usize, &'static str, i32, u16)>.is_some()) {
            let v331: u128 = 9u128;
            let mut v332: [(); 4] = v317;
            {
                for armed in [false, true] { caught(|| { let t = [Noisy(29), Noisy(58), Noisy(16)]; let [_x, ..] = t; trip(armed, ()); }); }
                for armed in [false, true] { caught(|| { let t = (Noisy(29), Box::new(Duo { a: Noisy(58), b: Noisy(16) })); trip(armed, ()); let _x = t.1.a; let _late = Noisy(12); }); }
                for armed in [false, true] { caught(|| { let t = (Some(Noisy(29)), Duo { a: Noisy(58), b: Noisy(16) }); let _x = t.1.b; let _y = t.0; let _w = t.1.a; trip(armed, ()); }); }
            }
        }
        let v333: [(); 4] = std::mem::replace(&mut v317, [(), (), (), ()]);
    }
}

mod c1 {
    use super::*;
    #[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord, Hash)]
    #[repr(i8)]
    pub enum E1 {
        V0 { f0: u128, f1: u32, f2: Vec<Vec<bool>> },
        V1(Result<Result<u8, u16>, Result<bool, bool>>, i8, char),
        V2(char),
    }
    impl Mix for E1 {
        fn mix(&self) {
            match self {
                E1::V0 { f0: a0, f1: a1, f2: a2 } => { feed(10); a0.mix(); a1.mix(); a2.mix(); }
                E1::V1(a0, a1, a2) => { feed(11); a0.mix(); a1.mix(); a2.mix(); }
                E1::V2(a0) => { feed(12); a0.mix(); }
            }
        }
    }
    static mut K2: u128 = (35u128 ^ 134217727u128);
    static K3: Result<Result<bool, f64>, Result<i8, f64>> = Ok::<Result<bool, f64>, Result<i8, f64>>(Ok::<bool, f64>(true));
    static K4: Result<Result<u8, u16>, Result<bool, bool>> = Ok::<Result<u8, u16>, Result<bool, bool>>(Ok::<u8, u16>((if 8u8 >= 2u8 { 8u8 } else { 2u8 })));
    pub trait T5 {
        fn m6(&self, a0: char) -> Result<Result<bool, f64>, Result<i8, f64>>;
        fn m7(&self, a0: i8) -> Result<Vec<u8>, E1>;
        fn m8(&mut self, a0: i8) -> i8;
    }
    impl T5 for Vec<Vec<bool>> {
        fn m6(&self, a9: char) -> Result<Result<bool, f64>, Result<i8, f64>> {
            (match (&{ E1::V0 { f0: 284434544489860383939024773513609867994u128, f1: 4013446316u32, f2: Vec::<Vec<bool>>::new() } }) { E1::V1(Ok(Ok(_)), p11, p12) => K3, _ => Ok::<Result<bool, f64>, Result<i8, f64>>(Ok::<bool, f64>(false)) })
        }
        fn m7(&self, a13: i8) -> Result<Vec<u8>, E1> {
            {
                let v14: Box<E1> = sel::<Box<E1>>((match (&{ Box::new(E1::V2('z')) }) { p15 if false => true, _ => true }), Box::new(E1::V1(Ok::<Result<u8, u16>, Result<bool, bool>>(Err::<u8, u16>(1u16)), 1i8, '\u{7f}')), id::<Box<E1>>(Box::new(E1::V1(Ok::<Result<u8, u16>, Result<bool, bool>>(Err::<u8, u16>(32767u16)), 10i8, 'z'))));
                let mut v16: [char; 2] = [(3u8 as char), 'ß'];
                (unsafe { *v16.as_ptr().add(1) }).mix();
                v16.mix();
            }
            None::<Vec<u8>>.ok_or(E1::V2('\u{80}'))
        }
        fn m8(&mut self, a21: i8) -> i8 {
            let mut v22: i8 = (3.25f32 as i8);
            ((-9223372036854775808.0f64) as i8)
        }
    }
    impl T5 for u128 {
        fn m6(&self, a23: char) -> Result<Result<bool, f64>, Result<i8, f64>> {
            sel::<Result<Result<bool, f64>, Result<i8, f64>>>((id::<Result<Result<bool, f64>, Result<i8, f64>>>(K3) >= sel::<Result<Result<bool, f64>, Result<i8, f64>>>(true, K3, K3)), Err::<u32, Result<bool, f64>>(Ok::<bool, f64>(false)).err().ok_or(sel::<Result<i8, f64>>(false, Ok::<i8, f64>(65i8), Err::<i8, f64>(3.25f64))), (match (apply(|x31: Box<String>| -> String { (*x31).clone() }, Box::new(String::from("ff"))).as_str()) { "a,b,,c" => (unsafe { let mut slot = MaybeUninit::<Result<Result<bool, f64>, Result<i8, f64>>>::uninit(); slot.write(Ok::<Result<bool, f64>, Result<i8, f64>>(Ok::<bool, f64>(false))); slot.assume_init() }), "-7" => Err::<Result<bool, f64>, Vec<Vec<bool>>>(vec![vec![true]]).or_else(|x33: Vec<Vec<bool>>| -> Result<Result<bool, f64>, Result<i8, f64>> { Ok::<Result<bool, f64>, Result<i8, f64>>(Ok::<bool, f64>(false)) }), _ => Ok::<Result<bool, f64>, Result<i8, f64>>(Ok::<bool, f64>(false)) }))
        }
        fn m7(&self, a34: i8) -> Result<Vec<u8>, E1> {
            None::<Result<Box<bool>, Box<i128>>>.ok_or(E1::V0 { f0: 65564692165419172559254419304022083259u128, f1: 35u32, f2: vec![vec![false, false, true, true, false], vec![false, false], vec![true, true, true, false, true]] }).map(|x36: Result<Box<bool>, Box<i128>>| -> Result<Result<u8, u16>, Result<bool, bool>> { Ok::<Result<u8, u16>, i8>(Err::<u8, u16>(26711u16)).or_else(|x38: i8| -> Result<Result<u8, u16>, Result<bool, bool>> { K4 }) }).map(|x35: Result<Result<u8, u16>, Result<bool, bool>>| -> Vec<u8> { (match (Ok::<Result<u8, u16>, Box<Vec<bool>>>(Err::<u8, u16>(65534u16)).or_else(|x40: Box<Vec<bool>>| -> Result<Result<u8, u16>, Result<bool, bool>> { x35 })) { Err(Err(false)) if (100u32 >= 10u32) => vec![36u8, 38u8, 177u8], _ => ({ let mut items = vec![189u8, 8u8, 10u8]; items.dedup(); items }) }) })
        }
        fn m8(&mut self, a42: i8) -> i8 {
            let mut v43: Vec<String> = ({ let mut items = vec![String::from("a,b,,c"), String::from("0123456789")]; items.push(String::from("  pad  ")); items });
            { let value = Some(295736281742822913267537347128435696154u128).map_or((*self), |x45: u128| -> u128 { (unsafe { K2 }) }).max(3u128); unsafe { K2 = value; } }
            100i8.checked_add(a42).unwrap_or(a42)
        }
    }
    #[inline]
    pub fn f46(a47: E1, a48: &i8) -> u16 {
        sel::<u16>(65535.0f32.is_infinite(), 25u16.checked_shr(31u32).unwrap_or(65534u16), 32767u16.checked_shl(2u32).unwrap_or(3u16))
    }
    pub fn f49(a50: &[Vec<Result<bool, f32>>], a51: Box<String>) -> Box<E1> {
        let mut v52: Vec<Box<Result<i8, u128>>> = vec![Box::new(Ok::<i8, u128>(15i8)), Box::new(Err::<i8, u128>(31u128))];
        {
            let mut v54: Result<Result<u8, u16>, Result<bool, bool>> = sel::<Result<Result<u8, u16>, Result<bool, bool>>>((false && false), K4, (match (E1::V2('\u{d7ff}')) { E1::V0 { f1: p57, f2: p58, .. } => twice::<Result<Result<u8, u16>, Result<bool, bool>>>(&Ok::<Result<u8, u16>, Result<bool, bool>>(Err::<u8, u16>(65535u16))).1, E1::V1(p60, p61, p62) => (loop { break K4; }), E1::V2(_) => Err::<Result<u8, u16>, Result<bool, bool>>(Ok::<bool, bool>(true)) }));
            let mut v65: [u32; 4] = <[u32; 4]>::try_from([vec![12u32, 1u32], vec![7u32, 29u32, 25u32]].concat()).unwrap_or([37u32; 4]);
            (unsafe { *v65.as_ptr().add(2) }).mix();
            v65.mix();
            {
                async fn leaf(x: u64) -> u64 { let _g = Noisy(x as u32 % 9); later((x % 3) as u32, x).await }
                let futures: Vec<Pin<Box<dyn Future<Output = u64>>>> = vec![Box::pin(leaf(23)), Box::pin(async { 12 }), Box::pin(later(2, 9u64)), Box::pin(async { leaf(1).await + leaf(2).await })];
                for (i, future) in futures.into_iter().enumerate() { poll_some(future, i as u32 % 1).mix(); }
            }
        }
        {
            #[inline(never)] fn spread(a: u8, b: u128, c: u64, d: u128, e: u32, f: u128, g: u128) -> (u128, u8, u128) { (b.wrapping_add(d) ^ f, a.wrapping_add(c as u8).wrapping_add(e as u8), g.wrapping_sub(b)) }
            #[inline(never)] extern "C" fn c_side(a: u128, b: u8, c: u128) -> u128 { a.wrapping_mul(3).wrapping_add(c) ^ b as u128 }
            #[inline(never)] fn by_ref(a: &u128, b: &mut u128) -> u128 { *b = b.wrapping_add(*a); *a ^ *b }
            #[derive(Clone, Copy)] struct Wide { tag: u8, value: u128, pair: [u128; 2] }
            #[inline(never)] fn pass(w: Wide, extra: Option<u128>) -> Wide { Wide { tag: w.tag.wrapping_add(1), value: w.pair[1] ^ extra.unwrap_or(1), pair: [w.value, w.pair[0]] } }
            let (a, b): (u128, u128) = (black_box((10) as u128), black_box((0x0123_4567_89ab_cdef_fedc_ba98_7654_3210) as u128));
            let got = spread(1, a, 2, b, 3, a ^ b, !a); got.0.mix(); got.1.mix(); got.2.mix();
            c_side(a, 9, b).mix(); { let mut m = b; by_ref(&a, &mut m).mix(); m.mix(); }
            { let w = pass(Wide { tag: 7, value: a, pair: [b, a] }, Some(b)); w.tag.mix(); w.value.mix(); w.pair.mix(); pass(w, None).value.mix(); }
            { let f: fn(u128, u8, u128) -> u128 = |x, y, z| x ^ z ^ y as u128; f(a, 1, b).mix(); let g: extern "C" fn(u128, u8, u128) -> u128 = c_side; g(b, 2, a).mix(); }
        }
        (match (&{ Err::<Result<u8, u16>, Result<bool, bool>>(Ok::<bool, bool>(false)) }) { Err(p73) => (loop { break Box::new(E1::V1(Ok::<Result<u8, u16>, Result<bool, bool>>(Ok::<u8, u16>(64u8)), (-1i8), '\u{10FFFF}')); }), Err(Ok(true)) => apply_once(|| -> Box<E1> { Box::new(E1::V0 { f0: 10u128, f1: 2640320815u32, f2: vec![vec![true, false], vec![true, false, false, true, false]] }) }), _ => id::<Box<E1>>(Box::new(E1::V1(Ok::<Result<u8, u16>, Result<bool, bool>>(Err::<u8, u16>(60906u16)), (-91i8), '\''))) })
    }
    #[inline]
    pub extern "C" fn f75(a76: i8, a77: u32, a78: i8) -> u32 {
        {
            enum Loud { I(i32, Noisy), C(char, Noisy) }
            #[inline(never)] fn items(k: u32, last: char) -> [Loud; 3] { black_box([Loud::I(k as i32 % 4, Noisy(k)), Loud::C('q', Noisy(k + 1)), if k % 3 == 0 { Loud::I(1, Noisy(k + 2)) } else { Loud::C(last, Noisy(k + 2)) }]) }
            #[inline(never)] fn pick0(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { [x @ (Loud::I(..) | Loud::C('a'..='m', _)), _, Loud::C('a', _) | Loud::I(1, _)] => 1, [Loud::C('a', _) | Loud::C(_, _), Loud::I(..), y] if black_box(flag) => 2, [.., _] if black_box(!flag) => 3, _ => 0 } }
            for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick0(k, last, flag) as u64); feed(0xf00d); } } }
            #[inline(never)] fn pick1(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { [Loud::I(..), Loud::I(..), Loud::I(7, _)] => 1, _ if black_box(flag) => 2, _ => 0 } }
            for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick1(k, last, flag) as u64); feed(0xf00d); } } }
            #[inline(never)] fn pick2(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { [x @ (Loud::I(..) | Loud::C('a'..='m', _)), ..] if black_box(flag) => 1, [Loud::C('b', _), Loud::C('a'..='c', _) | Loud::I(_, _), Loud::C(_, _)] if black_box(!flag) => 2, _ => 0 } }
            for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick2(k, last, flag) as u64); feed(0xf00d); } } }
        }
        let mut v79: Result<Result<bool, f64>, Result<i8, f64>> = ('b81: { if (sel::<bool>(true, true, false)) { break 'b81 (match (&{ Ok::<Box<u8>, Result<bool, i64>>(Box::new(163u8)) }) { Ok(_) => Ok::<Result<bool, f64>, Result<i8, f64>>(Ok::<bool, f64>(false)), Ok(p83) => K3, _ => K3 }); } K3 });
        u32::try_from(6u8.wrapping_mul(1u8)).unwrap_or(a77)
    }
    #[inline]
    pub fn f85(a86: Result<String, Result<u16, bool>>, a87: &Vec<String>, a88: Vec<Result<bool, f32>>) -> Box<Vec<bool>> {
        let mut v89: Result<Result<bool, f64>, Result<i8, f64>> = larger::<Result<Result<bool, f64>, Result<i8, f64>>>((Box::new((unsafe { K2 }).checked_shl(22u32).unwrap_or((unsafe { K2 }))) as Box<dyn T5>).m6('\\'), Err::<Option<Result<bool, f64>>, Box<Result<i64, bool>>>(Box::new(Ok::<i64, bool>(17i64))).unwrap_or_else(|x94: Box<Result<i64, bool>>| -> Option<Result<bool, f64>> { Some(Err::<bool, f64>(0.0f64)) }).ok_or(i8::try_from(8u16).map_err(|_| -> f64 { (-9223372036854775808.0f64) })));
        Box::new(vec![true, false, false])
    }
    pub fn run() {
        let mut v96: Result<Result<bool, f64>, Result<i8, f64>> = (match (format!("<{:?}>", sel::<Result<Result<u8, u16>, Result<bool, bool>>>(true, K4, Err::<Result<u8, u16>, Result<bool, bool>>(Err::<bool, bool>(false)))).as_str()) { "😀" => larger::<Result<Result<bool, f64>, Result<i8, f64>>>(sel::<Result<Result<bool, f64>, Result<i8, f64>>>(true, K3, K3), Err::<Result<bool, f64>, Result<i8, f64>>(Err::<i8, f64>(f64::EPSILON))), _ => Err::<Result<bool, f64>, Result<i8, f64>>(Ok::<Result<i8, f64>, u32>(Ok::<i8, f64>(89i8)).unwrap_or_else(|x104: u32| -> Result<i8, f64> { Err::<i8, f64>(1e-40f64) })) });
        (match (&{ Err::<Box<u8>, Result<bool, i64>>((|x107: String| -> Result<bool, i64> { Ok::<bool, i64>(false) })(String::from("Abc Def"))) }) { Err(_) => (match (&{ Ok::<Result<u8, u16>, Box<String>>(Err::<u8, u16>(4u16)).or_else(|x108: Box<String>| -> Result<Result<u8, u16>, Result<bool, bool>> { K4 }) }) { Ok(Ok(_)) => Box::new(Err::<i8, bool>(false)), Err(Ok(true)) => twice::<Box<Result<i8, bool>>>(&Box::new(Ok::<i8, bool>(50i8))).0, _ => (match (&{ v96 }) { Err(Ok(p109)) if true => Box::new(Err::<i8, bool>(false)), _ => Box::new(Err::<i8, bool>(true)) }) }), _ => twice::<Box<Result<i8, bool>>>(&(unsafe { let mut slot = MaybeUninit::<Box<Result<i8, bool>>>::uninit(); slot.write(Box::new(Err::<i8, bool>(false))); slot.assume_init() })).1 }).mix();
        let mut o110: Vec<Box<dyn T5>> = vec![Box::new((match (&{ Some(vec![10u8, 28u8, 129u8, 111u8, 127u8]).ok_or(E1::V2('😀')) }) { Err(E1::V1(Ok(_), ..=9i8, p113)) => ((unsafe { K2 }).abs_diff(5u128) as u128), Err(E1::V2(p114)) => ((unsafe { K2 }) & (unsafe { K2 })), _ => (unsafe { K2 }).wrapping_rem_euclid((unsafe { K2 }) | 1) }).wrapping_div_euclid((unsafe { K2 }) | 1)), Box::new(pick::<[Vec<bool>; 3]>(true, &[vec![false, true], Vec::<bool>::new(), Vec::<bool>::new()], &[vec![false, true], vec![true, false, true], vec![true]]).clone().to_vec()), Box::new((unsafe { K2 }))];
        for o111 in o110.iter() { o111.m7(i8::try_from(100u8).unwrap_or(3i8)).mix(); }
        let [_, p121, _, p122]: [u8; 4] = [(223u8.checked_ilog2().unwrap_or(0) as u8), (127u8 << (8192u32 % 8)), ('b120: { if (true) { break 'b120 63u8; } 107u8 }), (4u8 & 50u8)];
        let mut v123: [u16; 1] = ({ let mut items = <[u16; 1]>::try_from(Vec::<u16>::new()).unwrap_or([21u16; 1]); items.reverse(); items });
        { let (x, y) = ((11usize.wrapping_rem_euclid(9367923078914693757usize | 1) % 1), (6usize.isqrt() % 1)); let start = v123.as_mut_ptr(); unsafe { std::ptr::swap(start.add(x), start.add(y)) }; }
        v123.mix();
        let mut v129: [u128; 1] = (match (apply_once(|| -> Result<E1, Result<u32, bool>> { Err::<E1, Result<u32, bool>>(Ok::<u32, bool>(1345096204u32)) })) { Err(Err(p135)) => <[u128; 1]>::try_from(vec![25u128, 19u128]).unwrap_or([134714927168999507415111719217352891333u128; 1]), _ => (match (vec![Ok::<bool, f32>(true), Err::<bool, f32>(0.5f32), Err::<bool, f32>(16777217.0f32), Ok::<bool, f32>(false), Ok::<bool, f32>(false)]) { _ if true => [174327136769565408333966448766023226420u128], _ => [2047u128; 1] }), Err(Ok(_)) if p122.overflowing_add(20u8).1 => (match (Ok::<Vec<bool>, Box<i128>>(vec![true, false, true, false, false])) { Err(p142) => [337425325722742495336729046071308910343u128; 1], Err(_) => [111995299511118459465320954297340934229u128; 1], _ => [4294967295u128] }), _ => [(-13i8)].map(|x144: i8| -> u128 { 4398046511104u128 }) });
        v129.mix();
        let mut v145: Result<Result<bool, f64>, Result<i8, f64>> = (match (&{ None::<E1>.ok_or(Ok::<u32, bool>(549329198u32)) }) { Err(Ok(p148)) => (match (&{ v123[0] }) { p150 => K3, p151 @ ..55726u16 => K3, _ => v96 }), Err(Err(p152)) => None::<Result<bool, f64>>.ok_or(Ok::<i8, f64>(9i8)), Ok(E1::V2(p154)) => Err::<Result<bool, f64>, Result<i8, f64>>(Ok::<i8, f64>(3i8)), _ => ('b157: { if (false) { break 'b157 v96; } K3 }) });
        v123.swap(0, 0);
        (match (&{ E1::V1(Err::<Result<u8, u16>, Result<bool, bool>>(Ok::<bool, bool>(true)), (-80i8), '\u{7f}') }) { E1::V1(Err(Err(p160)), _, '\u{80}'..='\u{10FFFF}') => v123, E1::V0 { f1: p161, f2: _, .. } => v123, E1::V1(_, p162 @ -128i8.., p163 @ 'a'..='z') => [0u16], _ => v123 }).mix();
        match &mut v129 {
            [.., p164 @ 25u128..=9903520314283042199192993791u128] => {
                let mut o165: Box<dyn T5> = Box::new(({ let mut items = ({ let mut items = vec![vec![true, false, false], vec![false, false, false], Vec::<bool>::new()]; items.retain(|x168: &Vec<bool>| -> bool { true }); items }); items.push([false, false, true].to_vec()); items }));
            }
            [p169 @ 27u128..2417851639229258349412352u128] => {
                let v170: [u16; 1] = std::mem::replace(&mut v123, [29u16]);
            }
            _ => {}
        }
        match &mut v145 {
            Ok(Ok(p171)) => {
                apply_once(|| -> Result<Result<bool, f32>, Vec<f32>> { sel::<Result<Result<bool, f32>, Vec<f32>>>((-1.0f32).is_nan(), (match (&{ Err::<Box<bool>, Box<i128>>(Box::new(1048575i128)) }) { Err(_) => Ok::<Result<bool, f32>, Vec<f32>>(Ok::<bool, f32>(true)), p176 => Ok::<Result<bool, f32>, Vec<f32>>(Ok::<bool, f32>(true)), _ => Err::<Result<bool, f32>, Vec<f32>>(vec![f32::MIN_POSITIVE, 957.0924f32]) }), Err::<Result<bool, f32>, Result<Result<i32, f64>, Result<&'static str, bool>>>(Ok::<Result<i32, f64>, Result<&'static str, bool>>(Ok::<i32, f64>((-1563828986i32)))).or_else(|x177: Result<Result<i32, f64>, Result<&'static str, bool>>| -> Result<Result<bool, f32>, Vec<f32>> { Ok::<Result<bool, f32>, Vec<f32>>(Ok::<bool, f32>(false)) })) }).mix();
            }
            _ => {}
        }
        {
            { let mut kept = Vec::new(); for i in 0..5u32 { let n = Noisy(i); let by_ref = || n.0 * 2; by_ref().mix(); if i % 2 == 0 { kept.push(move || n.0); } } for f in kept.iter().rev() { f().mix(); } }
            { let mut count = 0u32; let mut step = |by: u32| { count += by; count }; for i in 0..5 { step(i).mix(); } count.mix(); }
            { let mut fs: Vec<Box<dyn FnMut() -> u32>> = Vec::new(); for i in 0..5u32 { let mut own = i; fs.push(Box::new(move || { own += 56; own })); } for round in 0..2 { for f in fs.iter_mut() { feed(f() as u64 + round); } } }
            { for i in 0..5u32 { let n = Noisy(i); let once = move || n.0 + 56; if i != 1 { once().mix(); } } }
        }
        let mut v178: [u16; 1] = v123;
        let mut o179: Box<dyn T5> = Box::new([(match (&{ Ok::<Vec<u8>, Vec<u8>>(vec![31u8, 9u8]) }) { Err(_) => Vec::<Vec<bool>>::new(), Ok(_) => vec![vec![true, false]], Ok(p182) if true => vec![vec![true]], _ => vec![vec![false], vec![false, true], Vec::<bool>::new(), vec![true, true, true, false, true], Vec::<bool>::new()] }), vec![vec![true, false, true]]].concat());
    }
}

mod c2 {
    use super::*;
    #[derive(Clone, Debug, PartialEq, PartialOrd, Copy, Eq, Ord, Hash)]
    pub struct S1;
    impl Mix for S1 {
        fn mix(&self) {
            feed(0);
        }
    }
    static K2: NonZeroU8 = NonZeroU8::new(255u8).unwrap();
    static K3: NonZeroU8 = NonZeroU8::new(254u8).unwrap();
    pub trait T4 {
        fn m5(&self, a0: u128) -> S1;
        fn m6(&self, a0: S1, a1: S1) -> Vec<Vec<char>>;
    }
    impl T4 for u128 {
        fn m5(&self, a7: u128) -> S1 {
            twice::<S1>(&S1).0
        }
        fn m6(&self, a8: S1, a9: S1) -> Vec<Vec<char>> {
            {
                async fn leaf(x: u64) -> u64 { let _g = Noisy(x as u32 % 9); later((x % 3) as u32, x).await }
                for armed in [false, true] { caught(|| { let got = block_on(async { let _n = Noisy(5); let a = leaf(2).await; trip(armed, ()); let _m = Noisy(2); a + leaf(4).await }); feed(got); }); }
                caught(|| { let got = poll_some(async { let _b = Bomb(5); leaf(2).await }, 2); feed(got.is_some() as u64); });
            }
            if ((if ((match (&{ a9 }) { S1 if true => false, _ => true })) { (unsafe { *(&true as *const bool) }) } else { true })) {
                let v10: u64 = (unsafe { *(&9223372036854775807u64 as *const u64) });
                let mut v11: Vec<u64> = vec![24u64, 8u64];
            } else {
                let v13: Vec<Vec<char>> = ({ let mut items = Vec::<Vec<char>>::new(); items.reverse(); items });
            }
            larger::<Vec<Vec<char>>>(({ let mut items = ({ let mut items = vec![vec!['9', '\u{10FFFF}']]; items.reverse(); items }); items.insert(0, vec!['😀']); items }), ('b20: { if ((false || true)) { break 'b20 vec![vec!['\u{10FFFF}', '\u{80}']; ((f64::MIN_POSITIVE as usize) % 6)]; } [vec![vec!['\u{7f}'], vec!['Z', '\\', 'ß'], Vec::<char>::new()], vec![vec!['\u{e000}', 'é', '中', '\\', '\0'], vec!['\u{80}', 'ß', '\u{e000}', '\'', '0'], vec!['😀'], Vec::<char>::new(), vec!['😀', '😀']]].concat() }))
        }
    }
    impl T4 for i128 {
        fn m5(&self, a23: u128) -> S1 {
            (match (({ let t24: char = '\0'; S1 })) { S1 if ([Some(1u128), Some(1u128), Some(576460752303423489u128)] >= [None::<u128>; 3]) => twice::<S1>(&S1).0, _ => (match (vec![9i128, 17179869183i128, (-8i128), 4294967295i128, 3i128].as_slice()) { [8i128, p26] => S1, [p28, p29, 17i128] => S1, _ => S1 }) })
        }
        fn m6(&self, a30: S1, a31: S1) -> Vec<Vec<char>> {
            vec![Vec::<char>::new()]
        }
    }
    impl T4 for Box<Box<u16>> {
        fn m5(&self, a32: u128) -> S1 {
            'l34: for i33 in ((20u64..28u64).rev()) {
                (unsafe { *(&larger::<([i128; 1],)>((match (NonZeroU8::new(16u8).unwrap()) { p35 => ([281474976710655i128; 1],), _ => ([(-3i128)],) }), ([1i128; 1],)) as *const ([i128; 1],)) }).mix();
                let mut v36: NonZeroU8 = K2;
                ({ let mut items = [['a']; 1]; items.reverse(); items }).mix();
            }
            ('b39: { if ((vec![Vec::<char>::new()] == vec![Vec::<char>::new(), vec!['0', '\u{7f}']])) { break 'b39 S1; } S1 })
        }
        fn m6(&self, a40: S1, a41: S1) -> Vec<Vec<char>> {
            vec![vec!['9'], vec!['\u{7f}'], vec!['ß']]
        }
    }
    #[inline(never)]
    pub fn f45<G43: Mix + Clone + Debug + PartialOrd + T4, G44: Mix + Clone + Debug + PartialOrd>(a46: G43, a47: G44) -> Box<Box<u16>> {
        (if ((({ let t50: i128 = (-63600508693481826314741712179904569442i128); [Some(10u128); 3] }) != [23u128; 3].map(|x52: u128| -> Option<u128> { None::<u128> }))) { ({ let t53: [Option<u128>; 3] = [Some(158456325028528675187087900671u128); 3]; larger::<Box<Box<u16>>>(Box::new(Box::new(1025u16)), Box::new(Box::new(24u16))) }) } else { (if let Some(x54) = (None::<[[char; 1]; 1]>.and_then(|x55: [[char; 1]; 1]| -> Option<Box<Box<u16>>> { Some(Box::new(Box::new(50418u16))) })) { x54 } else { Box::new(Box::new(8u16)) }) })
    }
    #[inline]
    pub fn f58<G56: Mix + Clone + Debug + PartialOrd, G57: Mix + Clone + Debug + PartialOrd>(a59: G56, a60: G57, a61: Box<G57>) -> G56 {
        if ((!(false && false))) {
            (match (vec![664613997892457936451903530140172288u128, 7u128, 30u128, 1180591620717411303425u128, 32767u128].as_slice()) { [] => [[Some(10u128), Some(16154913052818414817605469184925902512u128), Some(340282366920938463463374607431768211455u128)]].map(|x66: [Option<u128>; 3]| -> [char; 1] { ['\0'].map(|x70: char| -> char { 'z' }) }), _ => ('b73: { if (([[' ']] <= [['Z']; 1])) { break 'b73 [['\u{10FFFF}']]; } [[Box::new(f64::MIN), Box::new(1.5f64)]].map(|x77: [Box<f64>; 2]| -> [char; 1] { ['中'] }) }) }).mix();
        }
        match ((match (&{ [Box::new(f64::NAN), Box::new((-176.0297f64))] }) { [_, p80] => [['\u{80}'; 1]], [p81, ..] => [['0'; 1]; 1], _ => [['中']; 1] })) {
            [['\0'..='\u{7f}', ..], ..] if [vec![vec!['Z', '\0']]].into_iter().all(|x82: Vec<Vec<char>>| -> bool { false }) => {
                let mut v83: G57 = (*a61).clone();
                if (false) {
                    (match (Box::new(Box::new(20u16))) { _ => v83.clone(), p84 if (Box::new([0i128, (-7i128), (-127094543821962587050867749306411961709i128), (-170141183460469231731687303715884105727i128)]) <= Box::new([18790480612043044266812141401202084485i128, 85070591730234615865843651857942052863i128, (-18i128), 7i128])) => v83.clone(), _ => None::<G57>.unwrap_or_else(|| -> G57 { v83.clone() }) }).mix();
                    let mut v86: [i128; 4] = [[S1; 4].map(|x94: S1| -> Vec<char> { vec!['\u{7f}', 'Z'] }), [[Box::new(9223372036854775809u128), Box::new(1u128), Box::new(340282366920938463463374607431768211455u128)], [Box::new(181073816643360395980027758297684112045u128), Box::new(308331633154851925203838085522532052704u128), Box::new(13731597077191453086366943008932613106u128)], [Box::new(100802292787226530386490388677099175010u128), Box::new(188052303810428472674912447926986234404u128), Box::new(18u128)], [Box::new(7u128), Box::new(1u128), Box::new(286874361873049860604331377614659621425u128)]].map(|x96: [Box<u128>; 3]| -> Vec<char> { vec!['a', '\u{7f}', '\u{10FFFF}', '_', '\u{7f}'] }), [vec![' ', '😀'], vec!['a', 'a', '\u{d7ff}'], vec!['9', 'é'], vec!['ß', '9']], [vec!['_', 'é', '\u{10FFFF}'], vec!['\'', '\'', '\u{d7ff}'], Vec::<char>::new(), vec!['\0', '\\', '\u{80}', '\u{80}', '\u{7f}']]].map(|x90: [Vec<char>; 4]| -> i128 { 20i128 });
                    { let value = ((3i128 >> (63u32 % 128)) >> (pick::<u32>(false, &3007306689u32, &27u32).clone() % 128)); let at = (usize::from_str_radix("a,b,,c", 36).unwrap_or(37usize) % 4); unsafe { v86.as_mut_ptr().add(at).write(value) }; }
                    v86.mix();
                    {
                        #[derive(Clone, Copy, Debug, PartialEq)] enum Small { A, B, C }
                        #[derive(Clone, Copy, Debug, PartialEq)] enum Outer { One(Small), Two, Three(bool), Four(char) }
                        #[derive(Debug, PartialEq)] enum Deep { Ref(&'static u8), Boxed(Box<u16>), Nz(NonZeroU32), Nothing, Other(Option<Option<bool>>) }
                        #[derive(Clone, Copy, Debug)] #[repr(u8)] enum Tagged { X(u8) = 3, Y = 9, Z(bool, u16) = 200 }
                        enum Never {}
                        static BYTE: u8 = 7;
                        fn tell(it: &Outer) -> u64 { match *it { Outer::One(Small::A) => 1, Outer::One(Small::B) => 2, Outer::One(Small::C) => 3, Outer::Two => 4, Outer::Three(flag) => 5 + flag as u64, Outer::Four(c) => 7 + c as u64 } }
                        fn deep(it: &Deep) -> u64 { match it { Deep::Ref(r) => **r as u64, Deep::Boxed(b) => **b as u64, Deep::Nz(n) => n.get() as u64, Deep::Nothing => 0, Deep::Other(None) => 1, Deep::Other(Some(None)) => 2, Deep::Other(Some(Some(flag))) => 3 + *flag as u64 } }
                        { let order: [Option<Cmp>; 4] = black_box([None, Some(Cmp::Less), Some(Cmp::Equal), Some(Cmp::Greater)]); for it in order { feed(match it { None => 9, Some(Cmp::Less) => 1, Some(Cmp::Equal) => 2, Some(Cmp::Greater) => 3 }); } }
                        for it in black_box([Outer::One(Small::C), Outer::Two, Outer::Three(true), Outer::Four('x'), Outer::One(Small::A), Outer::Three(false)]) { feed(tell(&it)); feed(tell(&Some(it).unwrap_or(Outer::Two))); }
                        { let items: [Option<char>; 3] = black_box([None, Some('z'), Some('\u{10FFFF}')]); for it in items { feed(match it { None => 0, Some(c) => c as u64 }); } let nested: Option<Option<Option<u8>>> = black_box(Some(Some(None))); feed(match nested { None => 1, Some(None) => 2, Some(Some(None)) => 3, Some(Some(Some(v))) => v as u64 }); }
                        feed((size_of::<Deep>() + size_of::<Option<Deep>>() * 64 + size_of::<Option<Box<u8>>>() * 4096 + size_of::<Option<&u8>>() * 65536 + size_of::<Result<Box<u8>, ()>>() * 1048576) as u64);
                        { let ok: Result<u32, Never> = black_box(Ok(5)); let Ok(value) = ok; feed(value as u64); let none: Option<Never> = None; feed(none.is_none() as u64 + size_of::<Option<Never>>() as u64); }
                        for it in black_box([Deep::Ref(&BYTE), Deep::Boxed(Box::new(49)), Deep::Nz(NonZeroU32::new(39).unwrap()), Deep::Nothing, Deep::Other(None), Deep::Other(Some(None)), Deep::Other(Some(Some(true)))]) { feed(deep(&it)); let wrapped = Some(it); feed(wrapped.as_ref().map_or(99, deep)); }
                    }
                } else {
                    let mut v99: NonZeroU8 = K2;
                    let v100: [Vec<char>; 4] = [vec!['\u{d7ff}', '\\', '中', '\'', '\''], vec!['\u{10FFFF}', 'z', '😀', '0', '\n'], vec!['\\', 'a', 'é', '\n', '\u{10FFFF}'], Vec::<char>::new()];
                }
            }
            _ => {}
        }
        let mut v103: Box<Box<u16>> = (if ([18i8].into_iter().any(|x104: i8| -> bool { false })) { Box::new(Box::new(40800u16)) } else { Box::new(Box::new(3832u16)) });
        a59.clone()
    }
    #[inline]
    pub extern "C" fn f105(a106: i8, a107: u64, a108: i8) -> i8 {
        (-7i8)
    }
    pub fn run() {
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
            for it in black_box([Outer::One(Small::C), Outer::Two, Outer::Three(true), Outer::Four('k'), Outer::One(Small::A), Outer::Three(false)]) { feed(tell(&it)); feed(tell(&Some(it).unwrap_or(Outer::Two))); }
            { let items = black_box([Tagged::X(10), Tagged::Y, Tagged::Z(true, 53)]); for it in items { feed(match it { Tagged::X(v) => v as u64, Tagged::Y => 1000, Tagged::Z(flag, v) => v as u64 + flag as u64 }); feed(unsafe { *(&it as *const Tagged as *const u8) } as u64); } }
            { let pairs: [Option<(bool, u8)>; 3] = black_box([None, Some((true, 10)), Some((false, 0))]); for it in pairs { feed(match it { None => 7, Some((flag, v)) => v as u64 * 2 + flag as u64 }); } let refs: [Option<&str>; 2] = black_box([None, Some("")]); for it in refs { feed(it.map_or(5, str::len) as u64); } }
            for it in black_box([Deep::Ref(&BYTE), Deep::Boxed(Box::new(10)), Deep::Nz(NonZeroU32::new(53).unwrap()), Deep::Nothing, Deep::Other(None), Deep::Other(Some(None)), Deep::Other(Some(Some(true)))]) { feed(deep(&it)); let wrapped = Some(it); feed(wrapped.as_ref().map_or(99, deep)); }
            { let mut slot = black_box(Outer::Two); slot = if 10 % 2 == 0 { Outer::Three(true) } else { Outer::One(Small::B) }; feed(tell(&slot)); let mut opt = black_box(Some(slot)); if let Some(Outer::Three(flag)) = &mut opt { *flag = false; } feed(opt.map_or(0, |it| tell(&it))); opt = None; feed(opt.is_some() as u64); }
        }
        let [p111, p112]: [Box<f64>; 2] = [Box::new(7.0f64), Box::new(4294967296.0f64)];
        if (((false && true) || ([None::<u128>, Some(34u128), Some(2596148429267413814265248164610049u128)] >= [Some(308580785964937187803746842294234257869u128), Some(8u128), Some(10u128)]))) {
            {
                let mut v113: NonZeroU8 = (match (3i128.overflowing_shr(13u32).0) { -145579123486859959783651204563242585363i128.. => K3, 4i128 if (vec![Some('\0'), Some('😀'), Some('\u{d7ff}')] >= vec![Some('\n'), Some('\''), None::<char>]) => NonZeroU8::new(106u8).unwrap_or(K3), 18i128 if true => (if (false) { NonZeroU8::new(1u8).unwrap() } else { K3 }), _ => pick::<NonZeroU8>(false, &K2, &NonZeroU8::new(31u8).unwrap()).clone() });
                let v114: u64 = (match (f58::<[Box<u128>; 3], u128>([Box::new(28u128), Box::new(89316722062820527841169243815971419097u128), Box::new(1u128)], ((-952.6070f64) as u128), f58::<Box<u128>, i128>(Box::new(187760385278891731003010172739333163452u128), 170141183460469231731687303715884105726i128, Box::new(11801157305505555593849567410848357664i128)))) { [_, ..] => 8u64.wrapping_add(9934241966739483673u64).clamp(262145u64, 16600001639038004203u64), [p119, p120, p121] => u64::from_str_radix("x y".trim_end(), 10).unwrap_or(268435457u64.wrapping_mul(2598537980553623399u64)), [p123, p122 @ .., p124] => (32767u16 as u64), _ => ((33u64..40u64).all(|x125: u64| -> bool { false }) as u64) });
                {
                    (-18i8).mix();
                    {
                        enum Loud { I(i32, Noisy), C(char, Noisy) }
                        #[inline(never)] fn items(k: u32, last: char) -> [Loud; 3] { black_box([Loud::I(k as i32 % 4, Noisy(k)), Loud::C('q', Noisy(k + 1)), if k % 3 == 0 { Loud::I(1, Noisy(k + 2)) } else { Loud::C(last, Noisy(k + 2)) }]) }
                        #[inline(never)] fn pick0(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { [Loud::C('a'..='z', _), Loud::C('a', _) | Loud::I(2, _), y @ Loud::C(..)] => 1, [_, m @ .., Loud::C('a', _) | Loud::C(_, _)] => 2, [.., _] if black_box(!flag) => 3, [Loud::C(_, _) | Loud::I(..3, _), Loud::I(..), y] => 4, _ => 0 } }
                        for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick0(k, last, flag) as u64); feed(0xf00d); } } }
                        #[inline(never)] fn pair(k: u32, last: char, flag: bool) -> u8 { match (items(k, last), k % 3, flag) { ([Loud::I(..), ..], 3, true) => 1, ([.., _], _, _) => 2, ([x, m @ ..], 3, false) => 3, (all, _, _) if black_box(flag) => 4, _ => 0 } }
                        for k in 0..4u32 { for last in ['a', 'Z'] { for flag in [false, true] { feed(pair(k, last, flag) as u64); feed(0xf00d); } } }
                    }
                    5i128.overflowing_shl(4294967294u32).0.mix();
                }
                let mut o126: Box<dyn T4> = Box::new(Box::new(Box::new(1023u16)));
            }
        } else {
            (loop { break (if (true) { K2 } else { K2 }); }).mix();
            Box::new([100i128; 4]).mix();
        }
        if (pick::<Option<bool>>(true, &None::<bool>, &Some(true)).clone().map_or((false && false), |x127: bool| -> bool { (true && x127) })) {
            Box::new((*p111)).mix();
            let v129: u64 = (f32::NEG_INFINITY as u64);
            f58::<S1, [Box<u128>; 3]>(f58::<S1, Box<[u128; 1]>>(S1, Box::new([36u128]), f58::<Box<Box<[u128; 1]>>, Vec<[u128; 3]>>(Box::new(Box::new([35u128; 1])), vec![[12u128, 16u128, 340282366920938463463374607431768211455u128], [163884940034143201427516553415137844231u128; 3], [255852462849515668045396775836178807437u128, 170141183460469231731687303715884105727u128, 4u128]], Box::new(Vec::<[u128; 3]>::new()))), [(match (&{ [Box::new(340282366920938463463374607431768211455u128), Box::new(2u128), Box::new(1u128)] }) { [p132, .., p133] => (*p133).clone(), [_, p134 @ .., _] if false => (*p134)[0].clone(), p135 if true => Box::new(136101413938040027363210542187941371338u128), _ => Box::new(36u128) }), f58::<Box<u128>, S1>(Box::new(549755813889u128), S1, Box::new(S1)), (match (&{ v129 }) { 1152921504606846977u64 => Box::new(85319788714576658547855777825119608202u128), _ => Box::new(9u128), _ => Box::new(18u128), _ => Box::new(1u128) })], (match ((match ([['9'; 1]; 1]) { [[.., _], ..] => vec![NonZeroU8::new(100u8).unwrap(), NonZeroU8::new(1u8).unwrap()], _ => vec![NonZeroU8::new(1u8).unwrap(), NonZeroU8::new(8u8).unwrap(), NonZeroU8::new(253u8).unwrap(), NonZeroU8::new(1u8).unwrap(), NonZeroU8::new(2u8).unwrap()] }).as_slice()) { [_, ..] => Box::new([Box::new(32u128), Box::new(100u128), Box::new(170141183460469231731687303715884105727u128)]), _ => ({ let t138: ([i128; 1],) = ([5192296858534827628530496329220096i128],); Box::new([Box::new(38u128), Box::new(268435456u128), Box::new(3u128)]) }) })).mix();
        }
        if (larger::<bool>((Box::new([0u128; 3]) < Box::new([2596148429267413814265248164610047u128, 12u128, 28u128])), (match (vec![([(-15i128); 1],)].as_slice()) { [([_],)] => true, [([.., p141],), ([p142, ..],)] if false => false, _ => true }))) {
            {
                let mut v143: Box<f64> = Box::new((-2.5e-3f64));
                if (true) {
                    let mut v144: u64 = (31u64 >> (4294967295u32 % 64));
                    {
                        let (a, b): (u128, u128) = (black_box((3) as u128), black_box(((u128::MAX / 3)) as u128));
                        { let (sum, carry) = a.overflowing_add(b); let (product, over) = a.overflowing_mul(b); sum.mix(); product.mix(); feed(carry as u64 + 2 * over as u64); }
                        (a & b).mix(); (a | b).mix(); (a ^ b).mix(); feed((a < b) as u64 + 2 * (a == b) as u64); a.max(b).mix(); a.abs_diff(b).mix(); a.cmp(&b).mix();
                        a.wrapping_add(b).mix(); a.wrapping_sub(b).mix(); a.wrapping_mul(b).mix(); a.checked_add(b).mix(); a.checked_mul(b).mix(); a.saturating_sub(b).mix();
                        a.checked_pow(3).mix(); a.wrapping_pow(b as u32 % 7).mix(); a.checked_neg().mix(); a.checked_next_multiple_of(b | 1).mix();
                        (a as u64).mix(); (a as i64).mix(); (a as u8).mix(); ((a >> 64) as u64).mix(); u64::try_from(a).ok().mix(); i32::try_from(b).ok().mix();
                        a.isqrt().mix(); a.checked_ilog2().mix(); a.checked_ilog10().mix();
                    }
                } else {
                    let v145: [Option<u128>; 3] = pick::<[Option<u128>; 3]>(true, &[f58::<Option<u128>, Box<[u128; 3]>>(None::<u128>, Box::new([241275595233701631665964736664362760870u128; 3]), Box::new(Box::new([61480499119620199758281380294270262252u128, 3u128, 10u128]))), Some(340282366920938463463374607431768211455u128), Ok::<u128, Vec<[u128; 3]>>(1u128).ok()], &[Ok::<u128, Box<[u128; 3]>>(8u128).ok(), twice::<Option<u128>>(&Some(37778931862957161709568u128)).1, Some(39u128).filter(|x156: &u128| -> bool { false })]).clone();
                    let v157: [Vec<char>; 4] = twice::<[Vec<char>; 4]>(&[vec!['\\', 'z'], vec!['\'', '\u{d7ff}'], Vec::<char>::new(), vec!['a', '\n', '\u{10FFFF}', '\u{80}', '\u{d7ff}']]).1;
                    let f159 = move || -> Box<[u128; 8]> { let v158 = v157; let out = f58::<Box<[u128; 8]>, [Vec<char>; 4]>(Box::new([7u128, 191320422065717124530029450769589453927u128, 1u128, 36u128, 31u128, 33u128, 26u128, 97984293104079566886058558951555104907u128]), [vec!['\\', '\u{80}', '\u{7f}', '0', '\n'], vec!['中'], Vec::<char>::new(), vec!['a', '\\', '\u{e000}', '\\', 'ß']], f58::<Box<[Vec<char>; 4]>, i8>(Box::new([vec!['_', '\u{80}', 'a', ' ', '0'], vec!['\u{10FFFF}', '\'', '中', '_', 'z'], Vec::<char>::new(), vec!['ß', 'ß', '_', 'a', 'Z']]), 7i8, Box::new(18i8))); drop(v158); out };
                    apply_boxed(Box::new(f159)).mix();
                }
            }
            {
                async fn leaf(x: u64) -> u64 { let _g = Noisy(x as u32 % 9); later((x % 3) as u32, x).await }
                for armed in [false, true] { caught(|| { let got = block_on(async { let _n = Noisy(14); let a = leaf(0).await; trip(armed, ()); let _m = Noisy(2); a + leaf(4).await }); feed(got); }); }
                caught(|| { let got = poll_some(async { let _b = Bomb(14); leaf(2).await }, 0); feed(got.is_some() as u64); });
            }
        } else {
            let mut v164: Vec<[i128; 8]> = twice::<Vec<[i128; 8]>>(&vec![[170141183460469231731687303715884105726i128, 8i128, 3i128, (-170141183460469231731687303715884105728i128), 20i128, 3i128, 20282409603651670423947251286016i128, 170141183460469231731687303715884105727i128]]).1;
        }
        if (false) {
            let mut v169: NonZeroU8 = K2;
            {
                let mut n170 = 0u32;
                'l171: loop {
                    n170 += 1;
                    if n170 > 1 { break; }
                    {
                        async fn work(seed: u32) -> u64 { let h173 = Noisy(seed % 36 + 1); let w174 = later(2, [seed as u64; 9]).await[0]; let h175 = Noisy(seed % 1 + 1); let w176 = later(3, seed as u64 + 58).await; let h177: u128 = (seed as u128) << 94; let w178 = match later(1, seed % 3).await { 0 => later(0, 10u64).await, 1 => 11, _ => later(2, 12u64).await }; let h179 = (seed as f64 * 0.5, seed as u8, seed % 3 == 0); let mut w180 = 0u64; for i in 0..2u64 { let _g = Noisy(i as u32); w180 += later((i % 2) as u32, i).await; } (h173.0 as u64).rotate_left(0) ^ (w174).rotate_left(1) ^ (h175.0 as u64).rotate_left(2) ^ (w176).rotate_left(3) ^ ((h177 >> 3) as u64).rotate_left(4) ^ (w178).rotate_left(5) ^ (h179.0 as u64 + h179.1 as u64 + h179.2 as u64).rotate_left(6) ^ (w180).rotate_left(7) }
                        poll_some(work(28), 1).mix();
                        poll_some(work(69), 5).mix();
                    }
                    {
                        #[derive(Clone, Copy, Debug, PartialEq)] enum Small { A, B, C }
                        #[derive(Clone, Copy, Debug, PartialEq)] enum Outer { One(Small), Two, Three(bool), Four(char) }
                        #[derive(Debug, PartialEq)] enum Deep { Ref(&'static u8), Boxed(Box<u16>), Nz(NonZeroU32), Nothing, Other(Option<Option<bool>>) }
                        #[derive(Clone, Copy, Debug)] #[repr(u8)] enum Tagged { X(u8) = 3, Y = 9, Z(bool, u16) = 200 }
                        enum Never {}
                        static BYTE: u8 = 7;
                        fn tell(it: &Outer) -> u64 { match *it { Outer::One(Small::A) => 1, Outer::One(Small::B) => 2, Outer::One(Small::C) => 3, Outer::Two => 4, Outer::Three(flag) => 5 + flag as u64, Outer::Four(c) => 7 + c as u64 } }
                        fn deep(it: &Deep) -> u64 { match it { Deep::Ref(r) => **r as u64, Deep::Boxed(b) => **b as u64, Deep::Nz(n) => n.get() as u64, Deep::Nothing => 0, Deep::Other(None) => 1, Deep::Other(Some(None)) => 2, Deep::Other(Some(Some(flag))) => 3 + *flag as u64 } }
                        { let order: [Option<Cmp>; 4] = black_box([None, Some(Cmp::Less), Some(Cmp::Equal), Some(Cmp::Greater)]); for it in order { feed(match it { None => 9, Some(Cmp::Less) => 1, Some(Cmp::Equal) => 2, Some(Cmp::Greater) => 3 }); } }
                        feed((size_of::<Option<Small>>() + size_of::<Outer>() * 16 + size_of::<Option<Outer>>() * 256 + size_of::<Option<Option<Outer>>>() * 4096) as u64);
                        { let items: [Option<char>; 3] = black_box([None, Some('z'), Some('\u{10FFFF}')]); for it in items { feed(match it { None => 0, Some(c) => c as u64 }); } let nested: Option<Option<Option<u8>>> = black_box(Some(Some(None))); feed(match nested { None => 1, Some(None) => 2, Some(Some(None)) => 3, Some(Some(Some(v))) => v as u64 }); }
                        { let mut slot = black_box(Outer::Two); slot = if 42 % 2 == 0 { Outer::Three(true) } else { Outer::One(Small::B) }; feed(tell(&slot)); let mut opt = black_box(Some(slot)); if let Some(Outer::Three(flag)) = &mut opt { *flag = false; } feed(opt.map_or(0, |it| tell(&it))); opt = None; feed(opt.is_some() as u64); }
                        feed((size_of::<Deep>() + size_of::<Option<Deep>>() * 64 + size_of::<Option<Box<u8>>>() * 4096 + size_of::<Option<&u8>>() * 65536 + size_of::<Result<Box<u8>, ()>>() * 1048576) as u64);
                        for it in black_box([Deep::Ref(&BYTE), Deep::Boxed(Box::new(42)), Deep::Nz(NonZeroU32::new(29).unwrap()), Deep::Nothing, Deep::Other(None), Deep::Other(Some(None)), Deep::Other(Some(Some(true)))]) { feed(deep(&it)); let wrapped = Some(it); feed(wrapped.as_ref().map_or(99, deep)); }
                    }
                }
                {
                    async fn work(seed: u32) -> u64 { let h181: Option<Box<u16>> = if seed % 2 == 0 { Some(Box::new(36)) } else { None }; let mut w182 = 0u64; for i in 0..3u64 { let _g = Noisy(i as u32); w182 += later((i % 2) as u32, i).await; } let h183 = [seed as u8; 17]; let mut w184 = 0u64; for i in 0..3u64 { let _g = Noisy(i as u32); w184 += later((i % 2) as u32, i).await; } let h185: Option<Box<u16>> = if seed % 2 == 0 { Some(Box::new(52)) } else { None }; let w186 = if seed % 2 == 0 { later(1, 5u64).await } else { 29 }; (h181.as_deref().copied().unwrap_or(1) as u64).rotate_left(0) ^ (w182).rotate_left(1) ^ (h183[0] as u64).rotate_left(2) ^ (w184).rotate_left(3) ^ (h185.as_deref().copied().unwrap_or(1) as u64).rotate_left(4) ^ (w186).rotate_left(5) }
                    block_on(work(86)).mix();
                    block_on(work(98)).mix();
                    block_on(work(66)).mix();
                }
                let mut v187: [char; 3] = [Box::new([11u128]), Box::new([8u128]), Box::new([63u128; 1])].map(|x190: Box<[u128; 1]>| -> char { '\u{80}' });
                { let at = (129usize.wrapping_div_euclid(5usize | 1) % 3); let value = (match ((match (vec![vec![vec!['\u{d7ff}', ' ', ' ', '\'', '\u{e000}'], vec!['a', '\u{10FFFF}'], vec!['中', '\u{7f}', '\0', '\u{e000}', '\u{10FFFF}']], vec![vec![' ', '\\']], Vec::<Vec<char>>::new(), Vec::<Vec<char>>::new(), vec![vec!['\u{e000}', ' ', ' ']]].as_slice()) { [_] => Box::new(Some('中')), [_, ..] => Box::new(Some('\0')), _ => Box::new(None::<char>) })) { p193 => pick::<char>(true, &'\n', &'9').clone(), _ => (match (&{ 10141204801825835211973625643008u128 }) { ..340282366920938463463374607431768211455u128 => '\'', (8u128 | 1u128 | 10u128..35184372088831u128) if false => 'Z', _ => '\n' }) }); unsafe { raw_set(v187.as_mut_ptr(), at, value); raw_get(v187.as_ptr(), at).mix(); } }
                v187.mix();
            }
        }
        let mut o194: Box<dyn T4> = Box::new((f58::<f32, [Box<u128>; 3]>(0.0f32, [Box::new(1u128), Box::new(23u128), Box::new(6u128)], Box::new([Box::new(8u128), Box::new(170141183460469231731687303715884105727u128), Box::new(7u128)])) as u128));
        p111.clone().mix();
        if ((false && false)) {
            'l200: for x199 in ([vec![[10u128, 85070591730234615865843651857942052864u128, 1u128]], vec![[30224258302000066359921906323408650122u128, 29u128, 59165426104318702275864701876968506916u128], [39614081257132168796771975168u128, 7u128, 316884961751050557950899706446160761781u128]], vec![[72927663287046453765225977145069616537u128; 3], [2u128; 3], [10u128; 3]], vec![[1099511627776u128, 141982562388964418771270761492825729436u128, 8u128], [2u128, 2305843009213693952u128, 15u128], [7u128, 3u128, 67108864u128]]].map(|x196: Vec<[u128; 3]>| -> u64 { 6439242900012837776u64 }).into_iter().chain(pick::<Vec<u64>>(true, &vec![0u64, 14539866411839654618u64], &vec![10910562957261823498u64]).clone()).collect::<Vec<_>>()) {
                (match ((match (S1) { S1 => None::<char>, S1 if false => Some('a'), S1 if true => Some('\u{7f}'), _ => None::<char> })) { None => f58::<char, [Vec<char>; 4]>('z', [vec!['é', '9', 'ß', '\\', '\''], vec!['é', '\'', '_'], vec!['\\', 'z', '\''], vec!['\u{7f}', ' ', '9']], Box::new([vec!['\u{e000}', '0', '\n', 'é', '😀'], vec!['0', 'z'], vec!['\n'], Vec::<char>::new()])), Some(x201) => x201 }).mix();
                let v203: [Vec<f32>; 5] = [[946.8765f32].to_vec(), (match (&{ [Vec::<char>::new(), vec!['\0'], vec!['é', 'z', ' '], vec!['_']] }) { [p208, _, _, p209] => vec![(-2.5e-3f32)], [_, .., _] => vec![(-605.8614f32), 255.5f32, 0.5f32, (-2.5e-3f32), 3.25f32], [p210, p211, p212, p213] => vec![f32::EPSILON, 0.5f32, f32::NAN, f32::NEG_INFINITY, 4294967296.0f32], _ => vec![(-1.0f32), 0.1f32, 255.5f32] }), vec![f32::NAN; 1], larger::<Vec<f32>>(vec![(-1.0f32), 1e10f32, (-128.9f32)], vec![f32::NAN, f32::NAN, 7.0f32]), vec![f32::MAX, 1e30f32, 16777217.0f32, f32::MIN_POSITIVE, 566.4595f32]];
                {
                    { let found = 'search: loop { for i in 0..4u32 { let hit = move |at: u32| at * i == 4; for j in 0..3 { if hit(j) { break 'search Some((i, j)); } } } break None; }; found.mix(); }
                    { let mut count = 0u32; let mut step = |by: u32| { count += by; count }; for i in 0..4 { step(i).mix(); } count.mix(); }
                }
            }
        } else {
            [Vec::<f32>::new(), vec![255.5f32, (-340.8487f32), (-9223372036854775808.0f32), 0.2f32, 16777217.0f32], Vec::<f32>::new(), Vec::<f32>::new(), Vec::<f32>::new()].mix();
            {
                let mut v217: Vec<S1> = f58::<Vec<S1>, Vec<Option<char>>>([Vec::<Box<u64>>::new(), vec![Box::new(16742441527678708841u64), Box::new(3824868261517766634u64), Box::new(9223372036854775807u64), Box::new(27u64), Box::new(9223372036854775807u64)], vec![Box::new(7u64)]].into_iter().rev().map(|x221: Vec<Box<u64>>| -> S1 { S1 }).collect::<Vec<S1>>(), ({ let mut items = vec![Some(' '), Some('Z'), Some('ß')]; items.insert(0, Some('_')); items }), f58::<Box<Vec<Option<char>>>, [[u128; 2]; 3]>(Box::new(vec![None::<char>, None::<char>]), [[14u128; 2], [10u128, 328039713378842869076835346590663360075u128], [10141204801825835211973625643008u128, 274877906944u128]], Box::new([[7u128, 10u128], [170141183460469231731687303715884105727u128, 1u128], [10u128; 2]])));
                drop(v217);
                f58::<NonZeroU8, u64>(pick::<NonZeroU8>(true, &NonZeroU8::new(100u8).unwrap(), &K2).clone(), (match ([Some(2u128), None::<u128>, Some(14u128)]) { [None, p224 @ .., Some(40u128..186216609906003102314762199759863276415u128)] => 9223372036854775807u64, [Some(p225), ..] => 10707433218097875047u64, _ => 36u64 }), f58::<Box<u64>, [[u128; 2]; 3]>(Box::new(1u64), [[3u128; 2], [137438953473u128; 2], [2u128; 2]], Box::new([[9u128; 2], [8388609u128; 2], [100u128; 2]]))).mix();
                let mut v226: [([i128; 1],); 2] = [Some(([(-85002137606709354547406671624261767283i128)],)).unwrap_or(pick::<([i128; 1],)>(false, &([125945343063214422439565613921774630447i128; 1],), &([77371252455336267181195264i128],)).clone()), ([8388608i128],)];
                { let words: [u16; 3] = [13u16.isqrt(), 6468u16.clamp(20u16, 2048u16), f58::<u16, [Box<u128>; 3]>(6475u16, [Box::new(100948797431351010666265391019262508507u128), Box::new(1u128), Box::new(309485009821345068724781055u128)], Box::new([Box::new(4u128), Box::new(173474300217886446690403338145466699533u128), Box::new(8796093022208u128)]))]; (unsafe { std::slice::from_raw_parts(words.as_ptr() as *const u8, 6) }).mix(); }
                v226.mix();
            }
        }
        {
            let mut v235: Box<[i128; 4]> = Some(Box::new([513i128, 5070602400912917605986812821505i128, 118782435867388962288023381383570355114i128, 10i128])).ok_or(Box::new(vec![3711075408u32, 15u32, 25u32])).unwrap_or_else(|x236: Box<Vec<u32>>| -> Box<[i128; 4]> { twice::<Box<[i128; 4]>>(&Box::new([(-19i128), 1025i128, 9671406556917033397649408i128, 3i128])).1 });
            let v238: Box<[i128; 4]> = (match (Some(v235.clone())) { None => Box::new([(-157912523038288625058213727126429607081i128); 4]), Some(x239) => x239 });
            let mut v241: [([i128; 1],); 3] = [vec![vec![(-11i16)], vec![20005i16], vec![32766i16]], vec![vec![0i16, 8i16, 5i16]], vec![vec![(-17i16), 7i16, 9329i16], vec![32767i16, (-15813i16)], Vec::<i16>::new(), vec![31i16, 14622i16], vec![(-32768i16), (-12i16)]]].map(|x244: Vec<Vec<i16>>| -> ([i128; 1],) { ([(-15i128); 1],) });
            (unsafe { *v241.as_ptr().add((larger::<usize>(5usize, 7usize) % 3)) }).mix();
            v241.mix();
        }
        match (&{ [vec!['é', '_'], Vec::<char>::new(), vec!['\0', '\u{e000}', '\u{10FFFF}'], vec!['\u{80}', '\0'], vec!['ß', '😀']] }) {
            [p247, ..] => {
                9i8.mix();
            }
            [p249, p248 @ .., _] => {
                let mut v250: Vec<char> = (*p248)[0].clone();
            }
            _ => {}
        }
        let mut v252: Vec<S1> = (unsafe { let mut slot = MaybeUninit::<Vec<S1>>::uninit(); slot.write(vec![S1, S1, S1, S1]); slot.assume_init() });
        'l257: for x256 in ((14i8..19i8).rev().collect::<Vec<_>>()) {
            let v258: [Vec<f32>; 5] = [Vec::<Box<u64>>::new(), vec![Box::new(18446744073709551615u64), Box::new(1u64)], ({ let mut items = vec![Box::new(9007199254740991u64), Box::new(9223372036854775807u64), Box::new(25u64)]; items.dedup(); items }), f58::<Vec<Box<u64>>, [Option<u128>; 3]>(vec![Box::new(12u64), Box::new(35u64), Box::new(30u64), Box::new(0u64), Box::new(10u64)], [None::<u128>, Some(130525666366082550068253169617181467407u128), Some(15666505350531566613021195419989707016u128)], Box::new([Some(10u128); 3])), f58::<Vec<Box<u64>>, [Box<u128>; 3]>(Vec::<Box<u64>>::new(), [Box::new(35184372088831u128), Box::new(562949953421312u128), Box::new(282330958170073676029793657178123403145u128)], Box::new([Box::new(1u128), Box::new(100u128), Box::new(39u128)]))].map(|x260: Vec<Box<u64>>| -> Vec<f32> { pick::<Vec<f32>>((true || true), &({ let mut items = vec![1.5f32, 16777217.0f32, (-1.0f32), (-398.1931f32), 0.0f32]; items.dedup(); items }), &({ let mut items = Vec::<f32>::new(); items.reverse(); items })).clone() });
            let mut v270: [Vec<f32>; 5] = v258.clone();
            let v273: Box<[u128; 3]> = Box::new([7u128, 340282366920938463463374607431768211454u128, 8193u128]);
            (if (f58::<bool, [Vec<char>; 5]>(true, [[Box::new('a'), Box::new('😀'), Box::new('中'), Box::new('\0')], [Box::new('\\'), Box::new('é'), Box::new(' '), Box::new('é')], [Box::new('\n'), Box::new('\n'), Box::new('\u{10FFFF}'), Box::new('\u{7f}')], [Box::new('\u{80}'), Box::new('\u{80}'), Box::new('\u{80}'), Box::new('\u{10FFFF}')], [Box::new('😀'), Box::new('\u{d7ff}'), Box::new('ß'), Box::new('Z')]].map(|x275: [Box<char>; 4]| -> Vec<char> { vec!['z', 'ß', 'ß'] }), (match (&{ [Box::new('😀'), Box::new('0'), Box::new('\u{e000}'), Box::new('9'), Box::new('\0')] }) { [p277, p276 @ .., _] => Box::new([vec!['\\', '\n'], vec!['_', '\0'], vec!['\u{7f}', 'z', '\u{7f}', '\u{10FFFF}', 'z'], vec!['é', 'é'], vec!['\u{d7ff}', 'z', 'é']]), [p279, p278 @ .., _] => Box::new([vec!['\u{10FFFF}', '0', '\u{10FFFF}'], vec!['_', '😀'], vec![' ', '0'], vec!['\u{10FFFF}', '\u{e000}'], vec!['0']]), _ => Box::new([vec!['é', 'Z'], vec!['ß'], Vec::<char>::new(), vec!['\u{10FFFF}', '\u{d7ff}', ' ', '\\', 'a'], Vec::<char>::new()]) }))) { v273.clone() } else { Box::new([170141183460469231731687303715884105727u128; 3]) }).mix();
        }
        if (true) {
            let mut n280 = 0u32;
            let v282: ([i128; 2],) = 'l281: loop {
                n280 += 1;
                if n280 > 3 { break ([145130606591207411097270798211094710435i128; 2],); }
                let mut v283: NonZeroU8 = K3;
                let v284: Vec<[i128; 8]> = (match (vec![Box::new([16777217u128])].as_slice()) { [.., _] => vec![[124550422861466843344339711227620016023i128, 112470155505734277672786591726582498313i128, 170141183460469231731687303715884105727i128, 170141183460469231731687303715884105726i128, 170141183460469231731687303715884105727i128, 13i128, 7i128, (-1i128)]], [_, ..] => vec![[17592186044416i128; 8], [154524767267644589257228089512082676738i128, (-170141183460469231731687303715884105728i128), 295147905179352825857i128, 8i128, 10i128, 5316911983139663491615228241121378303i128, 0i128, (-7i128)], [1208925819614629174706176i128, 85070591730234615865843651857942052863i128, 17179869184i128, 0i128, 3i128, 85070591730234615865843651857942052863i128, 6i128, (-170141183460469231731687303715884105727i128)], [100i128; 8], [(-16776932078627959487435009641957236251i128); 8]], _ => vec![[(-19i128); 8], [(-162994492443309333886273896702771552578i128), (-1i128), 17i128, (-3i128), 110068372377148467413367873054435945796i128, 1i128, 85070591730234615865843651857942052863i128, 2305843009213693951i128], [38685626227668133590597632i128, 0i128, (-70993529988813908066863074256040764715i128), (-2i128), 16777216i128, (-17i128), (-38759841173194659451186835995993378665i128), 18446744073709551615i128]] });
                [vec!['\u{10FFFF}'], vec!['9', '\0', 'z'], vec!['😀', '_', '9'], vec!['_']].mix();
                if (pick::<bool>(f58::<bool, Vec<[u128; 3]>>(true, Vec::<[u128; 3]>::new(), Box::new(Vec::<[u128; 3]>::new())), &[[['😀'; 1]]; 4].into_iter().map(|x288: [[char; 1]; 1]| -> [Vec<f32>; 5] { [vec![0.5f32, 4294967296.0f32, 3.25f32], vec![3.25f32], vec![1.0f32, f32::EPSILON, 4294967296.0f32], vec![1e30f32], vec![f32::MIN_POSITIVE, f32::NEG_INFINITY]] }).any(|x289: [Vec<f32>; 5]| -> bool { false }), &f58::<bool, Box<[i128; 4]>>(true, Box::new([162259276829213363391578010288128i128; 4]), Box::new(Box::new([63344412825377464602390288724045494913i128, 18889465931478580854785i128, 633825300114114700748351602688i128, (-1i128)])))).clone()) {
                    K3.mix();
                    if (({ let mut items = [[Some(20u128), Some(1u128), None::<u128>]; 2]; items.reverse(); items }).into_iter().rev().all(|x293: [Option<u128>; 3]| -> bool { vec![[vec![7.0f32, 3.25f32, (-488.1841f32)], vec![255.5f32, 949.4408f32, f32::MIN_POSITIVE, 0.5f32, f32::NAN], Vec::<f32>::new(), vec![3.25f32, f32::NEG_INFINITY, 65535.0f32], vec![7.0f32, 1e10f32, 255.5f32, (-0.0f32), (-0.0f32)]], [vec![f32::MIN_POSITIVE, (-1.0f32), (-128.9f32), 149.6231f32, (-0.0f32)], vec![f32::NEG_INFINITY, (-2.5e-3f32), 1e10f32], vec![(-2.5e-3f32), 1e10f32, 1.5f32], vec![0.2f32, (-1.0f32)], vec![944.4450f32, f32::INFINITY, (-118.0934f32), 16777217.0f32, 1.0f32]], [vec![4294967296.0f32, 1.0f32, (-2.5e-3f32), 743.5603f32, (-2.5e-3f32)], vec![(-128.9f32), f32::INFINITY, f32::MIN_POSITIVE, 0.5f32, 0.5f32], vec![0.5f32], Vec::<f32>::new(), vec![f32::MAX, f32::NEG_INFINITY, 0.1f32]]].contains(&[vec![388.7610f32], vec![(-9223372036854775808.0f32), 0.1f32], Vec::<f32>::new(), Vec::<f32>::new(), vec![f32::EPSILON, 1e30f32, 255.5f32, 4294967296.0f32, 65535.0f32]]) })) { break 'l281 ([(-141427653942171059278949346193305440698i128), 2097151i128],); }
                }
            };
            let mut n294 = 0u32;
            'l295: loop {
                n294 += 1;
                if n294 > 2 { break; }
                let mut v297: NonZeroU8 = (unsafe { *(&K3 as *const NonZeroU8) });
                let mut n298 = 0u32;
                let v300: [[u128; 2]; 3] = 'l299: loop {
                    n298 += 1;
                    if n298 > 3 { break twice::<[[u128; 2]; 3]>(&[[248886501860325954180491004407135225240u128, 18u128]; 3]).0; }
                    let [_] = (&{ <[Box<f32>; 1]>::try_from(vec![Box::new((-1.0f32)), Box::new((-2.5e-3f32)), Box::new(255.5f32), Box::new(0.5f32), Box::new(255.5f32)]).unwrap_or([Box::new(0.5f32)]) }) else { break 'l299 twice::<[[u128; 2]; 3]>(&[[35184372088832u128, 15u128]; 3]).1; };
                    if ((if (([Box::new(170141183460469231731687303715884105727u128), Box::new(10u128), Box::new(0u128), Box::new(307861535083300558839144405593247093543u128), Box::new(1048575u128), Box::new(9444732965739290427391u128), Box::new(275096114323689885199609170817263226678u128), Box::new(5192296858534827628530496329220097u128)] >= [Box::new(170141183460469231731687303715884105727u128), Box::new(1u128), Box::new(302787219421699579047070586605471528576u128), Box::new(39u128), Box::new(6u128), Box::new(204911377931114011471325323651048818933u128), Box::new(40564819207303340847894502572032u128), Box::new(43752647061623660247170548969745486360u128)])) { ([[NonZeroU8::new(1u8).unwrap(); 3]; 2] <= [[NonZeroU8::new(3u8).unwrap(); 3]; 2]) } else { (!false) })) { continue 'l295; }
                };
            }
            let mut n307 = 0u32;
            let v309: u64 = 'l308: loop {
                n307 += 1;
                if n307 > 8 { break 3375730535756871436u64.clamp(26u64, 29u64); }
                {
                    for armed in [false, true] { let acts: Vec<Box<dyn Fn() -> u32>> = vec![Box::new(|| 24), Box::new(move || trip(armed, 7)), Box::new(|| 3)]; caught(|| { let _g = Noisy(24); for act in &acts { feed(act() as u64); } }); }
                    for armed in [false, true] { let mut seen = Vec::new(); caught(|| { let mut note = |it: u32| { seen.push(Noisy(it)); trip(armed && it == 2, ()); }; note(1); note(2); note(3); }); feed(seen.len() as u64); }
                }
                {
                    async fn leaf(x: u64) -> u64 { let _g = Noisy(x as u32 % 9); later((x % 3) as u32, x).await }
                    let futures: Vec<Pin<Box<dyn Future<Output = u64>>>> = vec![Box::pin(leaf(9)), Box::pin(async { 11 }), Box::pin(later(2, 9u64)), Box::pin(async { leaf(1).await + leaf(2).await })];
                    for (i, future) in futures.into_iter().enumerate() { poll_some(future, i as u32 % 2).mix(); }
                }
            };
        } else {
            let mut v310: Box<[i128; 4]> = (match ([['\'', '\u{7f}', 'a', 'z', '中', '\u{e000}', '\u{7f}', '\'']; 1]) { [[' ', p311 @ .., '9']] => Box::new([34359738369i128, (-94750392029786323653017937235328926046i128), 71283466562627873800360991791028543835i128, (-10i128)]), p312 => Box::new([(-9i128), (-61282340866929454873948509472425524789i128), 85070591730234615865843651857942052863i128, 103495614778620734690589611758598146934i128]), [['\u{80}', ..]] => Box::new([18i128, 7i128, 10i128, (-1i128)]), _ => Box::new([(-8i128), (-15i128), (-170141183460469231731687303715884105728i128), 170141183460469231731687303715884105727i128]) });
            let mut v313: [char; 2] = [Box::new(Box::new(65535u16)), Box::new(Box::new(19u16))].map(|x316: Box<Box<u16>>| -> char { '中' });
            { let bytes: [u8; 17] = [(f64::MAX as u8).wrapping_div((match (vec![[Box::new('_'), Box::new('Z'), Box::new('\u{7f}'), Box::new('_')], [Box::new('z'), Box::new('\''), Box::new('中'), Box::new('é')], [Box::new('é'), Box::new('\u{10FFFF}'), Box::new('\u{80}'), Box::new('z')]].as_slice()) { [[_, ..]] if true => 33u8, [[_, .., _], ..] => 4u8, [[_, p322, _, p323], [p324, p325, _, _]] if true => 7u8, _ => 30u8 }) | 1), (255.5f32 as u8).checked_shr(({ let t326: Box<Option<char>> = Box::new(None::<char>); 1u32 })).unwrap_or(String::from("héllo").as_bytes().first().copied().unwrap_or(1u8)), (!String::from("a").as_bytes().first().copied().unwrap_or(254u8)), (15u8.wrapping_mul(9u8).leading_zeros() as u8), ((-1e10f32) as u8), (if (String::from("42").chars().flat_map(|x327: char| -> Vec<[Box<char>; 4]> { vec![[Box::new('9'), Box::new(' '), Box::new('é'), Box::new('\0')]] }).any(|x328: [Box<char>; 4]| -> bool { true })) { u8::from_str_radix("  pad  ", 2).unwrap_or(25u8) } else { 0u8.wrapping_add(15u8) }), 8u8.clamp(0u8, 10u8).clamp(7u8, 34u8), f58::<String, [Vec<char>; 5]>(String::from("Abc Def"), [Vec::<char>::new(), Vec::<char>::new(), vec!['\'', '\u{80}'], vec!['9', '\'', '中', '_', '\u{7f}'], vec!['a']], Box::new([vec!['ß', '\u{e000}', '\u{10FFFF}'], vec!['中', 'ß', '0', 'Z', 'ß'], vec!['\u{e000}', '\u{d7ff}', 'a'], vec!['\\', '\u{10FFFF}', '\u{7f}', '\'', ' '], Vec::<char>::new()])).as_bytes().first().copied().unwrap_or((2u8.abs_diff(3u8) as u8)), (match (Vec::<Vec<Box<u64>>>::new().as_slice()) { [_] if (match ([Some(68719476736u128); 3]) { [.., p330] => false, [Some(p331), .., Some((..=340282366920938463463374607431768211454u128 | 0u128..=10u128 | 79228162514264337593543950337u128..85070591730234615865843651857942052863u128))] => true, _ => true }) => (unsafe { *(&127u8 as *const u8) }), _ => String::from("42").as_bytes().first().copied().unwrap_or(0u8) }), pick::<u8>(false, &28u8.checked_shl(14u32).unwrap_or(18u8), &(match (vec![[Box::new(1e30f64), Box::new(f64::EPSILON)]].as_slice()) { [[_, p333], ..] => 0u8, _ => 255u8 })).clone(), (({ let t334: Box<[u128; 8]> = Box::new([117177186329333913371048014715602046370u128, 24u128, 28u128, 12u128, 32u128, 2u128, 3u128, 0u128]); 64u8 }) >> (31u32 % 8)), f58::<u8, [Option<u128>; 3]>(3u8, [None::<u128>, None::<u128>, Some(9u128)], Box::new([Some(2475880078570760549798248448u128), None::<u128>, Some(134856587158942976022782019550652346040u128)])).isqrt(), ('\u{d7ff}' as u8), (3u8 ^ (match (([10i128; 2],)) { ([147573952589676412929i128, p335],) => 254u8, _ => 17u8 })), (!(false as u8)), (17592186044415usize as u8), (!171u8.wrapping_add(7u8))]; (unsafe { (bytes.as_ptr().add(6) as *const i32).read_unaligned() }).mix(); }
            v313.mix();
        }
        Box::new(vec![36u32]).mix();
        let mut v336: Box<Box<u16>> = f58::<Box<Box<u16>>, Vec<[char; 2]>>((if (([Some(29u128); 3] == [Some(39u128), None::<u128>, None::<u128>])) { pick::<Box<Box<u16>>>(true, &Box::new(Box::new(5135u16)), &Box::new(Box::new(3u16))).clone() } else { f45::<u128, [[char; 2]; 2]>(1u128, [['😀', '\u{80}'], ['\u{80}', 'a']]) }), vec![['\n'; 2], [' ', '\\'], ['\0', 'ß'], ['\u{80}'; 2], ['é', '\\']], pick::<Box<Vec<[char; 2]>>>((true || false), &(if (true) { Box::new(vec![['a'; 2], ['\u{80}', '\0'], ['\u{d7ff}', '\''], ['\u{e000}'; 2], ['\u{e000}', ' ']]) } else { Box::new(vec![['Z'; 2], ['z', '0'], [' '; 2]]) }), &(match (S1) { S1 => Box::new(vec![['Z'; 2], [' '; 2], ['中', '\\'], ['_', '\\'], ['\u{80}'; 2]]), _ => Box::new(Vec::<[char; 2]>::new()) })).clone());
        K2.mix();
        let mut v337: [u128; 3] = [10u128, (0usize as u128), ((match (&{ ([9i128],) }) { ([14i128],) => (*p112), _ => (*p111), _ => (*p111) }) as u128)];
        { let at = 1; let value = (if (false) { (match (&{ [Box::new(42535295865117307932921825928971026431u128), Box::new(145351998528978059790035618975332891543u128), Box::new(27u128)] }) { [p341, ..] => (*(*p341)), [p342, _, p343] => (*(*p343)), _ => 0u128 }) } else { 0u128 }); unsafe { raw_set(v337.as_mut_ptr(), at, value); raw_get(v337.as_ptr(), at).mix(); } }
        v337.mix();
        [(match ([Box::new(' '), Box::new('中'), Box::new('a'), Box::new('\n'), Box::new('中')]) { [p347, p346 @ .., p348] if false => Box::new(340282366920938463463374607431768211455u128), [_, .., p349] if true => Box::new(340282366920938463463374607431768211455u128), _ => Box::new(162259276829213363391578010288129u128) }), Box::new(v337[1]), pick::<Box<u128>>(false, &Box::new(28u128), &Box::new(9u128)).clone()].mix();
        let mut n350 = 0u32;
        'l351: while n350 < 3 && ((unsafe { *(&true as *const bool) })) {
            n350 += 1;
            match ([['0', '\u{7f}']; 2]) {
                [[p354, '中'], [.., '\u{d7ff}']] if (true & true) => {
                    let v355: NonZeroU8 = K3;
                }
                [.., p356] => {
                    let v357: char = p356[0];
                }
                [.., [p358, ..]] => {
                    if ((match (&{ [Box::new((-0.0f32))] }) { [.., p360] => Some([['_'; 8]; 1]), _ => Some([['😀'; 8]]) }).is_some()) {
                        let v361: Box<Box<u16>> = Box::new(Box::new(16u16));
                        let mut v362: Vec<Option<char>> = ({ let mut items = f58::<Vec<Option<char>>, Box<[u128; 1]>>(vec![None::<char>; 0], (match ([Box::new('a'), Box::new('9'), Box::new('\u{7f}'), Box::new('\u{d7ff}')]) { [_, _, p366, p367] if false => Box::new([340282366920938463463374607431768211455u128; 1]), _ => Box::new([20u128; 1]) }), ({ let t368: Box<Box<u16>> = v361.clone(); Box::new(Box::new([11u128])) })); let at = items.len() / 2; items.split_off(at) });
                        {
                            { let x = black_box((3.4028236692093846e38) as f64); for word in [(x as u8) as i128 as u128 as u64, (x as i128) as i128 as u128 as u64, (x as i64) as i128 as u128 as u64] { feed(word); } }
                            { let x = black_box((-0.5) as f32); for word in [(x as i16) as i128 as u128 as u64, (x as usize) as i128 as u128 as u64, (x as i64) as i128 as u128 as u64, (x as i32) as i128 as u128 as u64, (x as i8) as i128 as u128 as u64, (x as u16) as i128 as u128 as u64] { feed(word); } }
                            { let x = black_box((65535.9) as f64); for word in [(x as i8) as i128 as u128 as u64, (x as u128) as i128 as u128 as u64, (x as u64) as i128 as u128 as u64] { feed(word); } }
                            { let x = black_box((9007199254740993.0) as f32); for word in [(x as u16) as i128 as u128 as u64, (x as u8) as i128 as u128 as u64, (x as isize) as i128 as u128 as u64, (x as i32) as i128 as u128 as u64] { feed(word); } }
                            { let x = black_box((-1.0) as f64); for word in [(x as u64) as i128 as u128 as u64, (x as i8) as i128 as u128 as u64, (x as usize) as i128 as u128 as u64] { feed(word); } }
                            { let x = black_box(isize::MAX / 3 as isize); (x as f32).mix(); (x as f64).mix(); ((x as f32) as isize).mix(); ((x as f64) as isize).mix(); (x as f64 as f32).mix(); }
                        }
                    }
                    if ((f58::<Box<Vec<u32>>, (Option<u128>, [u16; 3])>(Box::new(vec![3292728652u32, 262143u32]), (None::<u128>, [100u16, 25u16, 100u16]), Box::new((Some(340282366920938463463374607431768211455u128), [1u16; 3]))) < f58::<Box<Vec<u32>>, [Vec<char>; 5]>(Box::new(vec![1573306179u32, 3455748037u32, 29u32, 3213641903u32, 32767u32]), [vec!['\u{d7ff}'], Vec::<char>::new(), vec!['_'], vec!['\u{10FFFF}', '\u{80}', '_'], Vec::<char>::new()], Box::new([Vec::<char>::new(), vec!['0', '\u{7f}'], vec!['z', '\u{e000}'], vec!['\u{80}', 'a'], vec!['_', '\u{d7ff}', 'ß']])))) {
                        let v369: Vec<Option<char>> = vec![Some('\'')];
                        twice::<[Option<u128>; 3]>(&({ let mut items = [Some(10u128); 3]; items.sort_by(|a, b| b.cmp(a)); items })).1.mix();
                    } else {
                        [vec![65343167942501977495190489515398338200i128, 87871865194887809920388257661040044689i128, 100i128, 33554432i128, 4951760157141521099596496897i128], (match (({ let mut items = vec![Box::new(Box::new(32769u16))]; items.push(v336.clone()); items }).as_slice()) { [p379] => Vec::<i128>::new(), _ => vec![8i128, (-18747609072885105259066629057119589806i128)] }), ({ let mut items = vec![(-13i128), (-13i128)]; items.insert(0, ({ let t383: Option<[f32; 2]> = None::<[f32; 2]>; (-170141183460469231731687303715884105727i128) })); items })].mix();
                        let mut v385: Box<Box<u16>> = (match (&{ f58::<[Box<u128>; 8], [Option<u128>; 3]>([Box::new(322928091593680519040237708224525692836u128), Box::new(10843976144851532481948132570574035718u128), Box::new(1u128), Box::new(40u128), Box::new(18u128), Box::new(0u128), Box::new(256669772664817598509468048758536531877u128), Box::new(340282366920938463463374607431768211454u128)], [Some(23u128); 3], Box::new([Some(1048576u128), None::<u128>, None::<u128>])) }) { [.., p388] if String::from("").chars().skip_while(|x389: &char| -> bool { false }).map(|x390: char| -> Box<Box<u16>> { v336.clone() }).all(|x391: Box<Box<u16>>| -> bool { false }) => v336.clone(), [_, .., p392] => v336.clone(), [p393, .., p394] => twice::<Box<Box<u16>>>(&v336.clone()).1, _ => Box::new(Box::new(6u16)) });
                    }
                }
                _ => pick::<[[char; 8]; 1]>(false, &[['\0'; 8]; 1], &[['\u{d7ff}'; 8]]).clone().mix(),
            }
            larger::<Box<Vec<u32>>>(f58::<Box<Vec<u32>>, [Box<u128>; 8]>(Box::new(vec![8u32, 30u32]), [Box::new(1u128), Box::new(170141183460469231731687303715884105727u128), Box::new(2199023255552u128), Box::new(25383555970713901046212944167848723756u128), Box::new(23u128), Box::new(0u128), Box::new(8u128), Box::new(1u128)], Box::new([Box::new(6u128), Box::new(305498377633066941823794898867991404868u128), Box::new(23u128), Box::new(312254371149675322088636339864932422046u128), Box::new(21u128), Box::new(8u128), Box::new(2u128), Box::new(5316911983139663491615228241121378303u128)])), Box::new(vec![30u32, 2147483647u32, 268435456u32])).mix();
        }
    }
}

mod c3 {
    use super::*;
    #[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord, Hash)]
    pub struct S1<A, B> { pub f0: B, pub f1: Vec<A> }
    impl<A: Mix, B: Mix> Mix for S1<A, B> {
        fn mix(&self) {
            feed(2);
            self.f0.mix();
            self.f1.mix();
        }
    }
    #[derive(Clone, Debug, PartialEq, PartialOrd, Copy, Eq, Ord, Hash)]
    pub struct S2 { pub f0: i32 }
    impl Mix for S2 {
        fn mix(&self) {
            feed(1);
            self.f0.mix();
        }
    }
    static K3: u64 = 0u64;
    static K4: i32 = 10i32;
    static K5: u32 = 23u32;
    pub trait T6 {
        fn m7(&self, a0: NonZeroU8) -> i32;
        fn m8(&self) -> u32;
    }
    impl T6 for u32 {
        fn m7(&self, a9: NonZeroU8) -> i32 {
            let mut v10: BTreeMap<i32, i32> = BTreeMap::new();
            v10.insert(i32::from_str_radix("Abc Def", 36).unwrap_or((f32::EPSILON as i32)), i32::from_be_bytes(K4.overflowing_shr((*self)).0.to_be_bytes())).mix();
            v10.insert(i32::try_from(3i128).unwrap_or(K4).wrapping_rem(K4.saturating_sub(K4) | 1), Some((-10i32)).unwrap_or_else(|| -> i32 { 8i32.wrapping_neg() })).mix();
            v10.first_key_value().map(|(key, _)| key.clone()).mix();
            v10.len().mix();
            let v13: S1<i32, f64> = twice::<S1<i32, f64>>(&S1 { f0: (-372.3083f64), f1: vec![4096i32, 14i32, 4095i32, 16777217i32, 31i32] }).0;
            i32::from_ne_bytes((Some(1e10f64).unwrap_or(7.0f64) as i32).to_ne_bytes())
        }
        fn m8(&self) -> u32 {
            ((-311.2741f32) as u32).clamp(0u32, 38u32)
        }
    }
    impl T6 for f64 {
        fn m7(&self, a15: NonZeroU8) -> i32 {
            {
                struct Outer(Noisy, Bomb, Noisy);
                impl Drop for Outer { fn drop(&mut self) { feed(0x0d0d + self.0.0 as u64); } }
                caught(|| { let _a = Noisy(11); let _h0 = Outer(Noisy(34), Bomb(53), Noisy(22)); let _z = Noisy(23); });
                caught(|| { let _a = Noisy(9); let _ = Duo { a: Bomb(19), b: Noisy(35) }; take3(Noisy(22), vec![Box::new(Noisy(24)) as Box<dyn Any>, Box::new(Bomb(43)), Box::new(Noisy(26))], Noisy(13)); let _z = Noisy(20); });
                caught(|| { let _a = Noisy(41); let _ = Duo { a: Noisy(18), b: (Bomb(15), Noisy(26)) }; let _z = Noisy(4); });
            }
            (K4.overflowing_mul(K4).1 as i32)
        }
        fn m8(&self) -> u32 {
            if (false) {
                let v16: u64 = K3.overflowing_shr(13u32).0.min(u64::from_str_radix("ff", 16).unwrap_or(K3));
                {
                    #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(C)] struct Plain { a: u8, b: u32, c: u16, d: u8 }
                    #[derive(Clone, Copy, Default)] #[repr(C, packed(2))] struct Tight { a: u8, b: u64, c: u16, d: u128 }
                    #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(C, align(2))] struct Big { a: u8, b: [u16; 3] }
                    #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(align(2))] struct Rusty(u8, u32);
                    #[derive(Clone, Copy, Debug, PartialEq)] #[repr(transparent)] struct Wrap(f32);
                    #[derive(Clone, Copy, Debug, PartialEq)] #[repr(i8)] enum Signed { Low = -128, Mid = -1, High = 127 }
                    #[derive(Clone, Copy, Debug, PartialEq)] #[repr(u16)] enum Wider { A = 1, B = 0x1234, C = 0xffff }
                    #[derive(Clone, Copy, Debug, PartialEq)] #[repr(C)] enum Cee { Unit, One(u8), Two { x: u16, y: u64 } }
                    #[derive(Clone, Copy, Debug, PartialEq)] #[repr(C, u8)] enum Both { P(u32), Q(u8, u8), R }
                    #[derive(Clone, Copy)] #[repr(C)] union Bits { word: u32, bytes: [u8; 4], half: (u16, u16) }
                    #[inline(never)] extern "C" fn c_plain(p: Plain, t: Tight, w: Wrap) -> Plain { Plain { a: p.a.wrapping_add(t.a), b: p.b ^ { t.c } as u32, c: p.c, d: p.d } }
                    #[inline(never)] extern "C" fn c_big(x: u8, big: Big, y: u8, rusty: Rusty) -> Big { Big { a: big.a ^ x ^ y ^ rusty.0, b: [big.b[2], big.b[0], rusty.1 as u16] } }
                    #[inline(never)] fn r_enums(s: Signed, w: Wider, c: Cee, b: Both) -> (i8, u16, Cee, Both) { (s as i8, w as u16, match c { Cee::One(v) => Cee::Two { x: v as u16, y: 1 }, other => other }, match b { Both::Q(x, y) => Both::Q(y, x), other => other }) }
                    { let both = black_box([Both::P(7), Both::Q(1, 2), Both::R]); for it in both { feed(unsafe { *(&it as *const Both as *const u8) } as u64); feed(match it { Both::P(v) => v as u64, Both::Q(x, y) => (x + y) as u64, Both::R => 99 }); } }
                    { let mut t = black_box(Tight { a: 5, b: 20, c: 7, d: u128::MAX }); t.b += 1; t.d = t.d.wrapping_add(5); let (b, c, d) = (t.b, t.c, t.d); feed(b + c as u64); d.mix(); feed(unsafe { std::ptr::addr_of!(t.d).read_unaligned() } as u64); }
                    { let mut bits = Bits { word: black_box(5u32.wrapping_mul(0x01010101)) }; unsafe { bits.bytes[0] ^= 0xff; feed(bits.word.to_le() as u64); feed(u32::from_le_bytes(bits.bytes) as u64); bits.half = (1, 2); feed(u32::from_le(bits.word) as u64); } }
                    feed((std::mem::offset_of!(Plain, b) + std::mem::offset_of!(Plain, c) * 16 + std::mem::offset_of!(Plain, d) * 256 + std::mem::offset_of!(Tight, d) * 4096 + std::mem::offset_of!(Big, b) * 65536) as u64);
                    { let w = black_box(Wrap(2.5)); let raw: f32 = unsafe { std::mem::transmute(w) }; raw.mix(); let items = black_box([Rusty(1, 2), Rusty(3, 4)]); feed(items[1].1 as u64 + size_of_val(&items) as u64 * 16); }
                }
            }
            10u64.checked_shr(27u32).unwrap_or(K3).wrapping_div_euclid(11256217406054894619u64 | 1).mix();
            (match (Ok::<String, [Option<i128>; 4]>(String::from("3.5")).unwrap_or_else(|x17: [Option<i128>; 4]| -> String { String::from("3.5") }).as_str()) { "0123456789" => 2634905826u32, p18 => (if (false) { 1475868306u32 } else { K5 }), _ => K5.overflowing_shl(K5).0 })
        }
    }
    impl T6 for i32 {
        fn m7(&self, a19: NonZeroU8) -> i32 {
            if let [Some(p23), _, Some(p24), Some(p25)] = (({ let t22: u64 = K3; [None::<i128>, Some((-20258099297118698071500252571542405718i128)), None::<i128>, Some(151115727451828646838273i128)] })) {
                let v26: S1<i32, f64> = twice::<S1<i32, f64>>(&S1 { f0: 0.0f64, f1: vec![7i32, 32i32, 2147483646i32] }).0;
                let f28 = move || -> f64 { let v27 = v26; let out = v27.f0; drop(v27); out };
                f28().mix();
                let v29: String = format!("{:#?}", "1e-320".parse::<f64>().unwrap_or(65535.0f64));
            } else {
                let v30: [Option<String>; 3] = (if (matches!(S2 { f0: (-1i32) }, (S2 { .. }))) { <[Option<String>; 3]>::try_from(vec![Some(String::from("")), Some(String::from("-7")), None::<String>, Some(String::from("x y")), Some(String::from("x y"))]).unwrap_or([Some(String::from("  pad  ")), None::<String>, Some(String::from("Abc Def"))]) } else { [String::from("Abc Def"), String::from("héllo"), String::from("hello")].map(|x38: String| -> Option<String> { Some(String::from("line\n")) }) });
                let f32 = move || -> S2 { let v31 = v30; let out = [[S2 { f0: 2147483646i32 }, S2 { f0: 100i32 }, S2 { f0: 616178915i32 }, S2 { f0: (-399524342i32) }]; 17].map(|x40: [S2; 4]| -> S2 { x40[1] })[5]; drop(v31); out };
                f32().mix();
            }
            (*self).checked_shl(262145u32).unwrap_or((*self)).overflowing_shr((262144u32.abs_diff(K5) as u32)).0
        }
        fn m8(&self) -> u32 {
            let v41: String = String::from("héllo");
            let f43 = move || -> NonZeroU8 { let v42 = v41; let out = NonZeroU8::new((true as u8)).unwrap_or(NonZeroU8::new(3u8).unwrap_or(NonZeroU8::new(228u8).unwrap())); drop(v42); out };
            apply_boxed(Box::new(f43)).mix();
            K5.checked_sub(11u32).unwrap_or(K5).wrapping_div(({ let t44: String = String::from("hello"); K5 }) | 1)
        }
    }
    impl T6 for S1<i32, f64> {
        fn m7(&self, a45: NonZeroU8) -> i32 {
            (-13i32)
        }
        fn m8(&self) -> u32 {
            sel::<u32>(true, K5, K5).checked_shl((K5 | K5)).unwrap_or(((-9223372036854775808.0f32) as u32))
        }
    }
    pub fn f46(a47: &mut S1<i32, f64>, a48: isize, a49: i128, a50: S2, a51: isize) -> S2 {
        let mut v52: S1<i32, f64> = S1 { f0: (-(*a47).f0), f1: pick::<Vec<i32>>(false, &(*a47).f1.clone(), &(*a47).f1.clone()).clone() };
        S2 { f0: 2i32 }
    }
    #[inline(never)]
    pub fn f54(a55: i32) -> S1<i32, f64> {
        {
            let f56 = || -> [S2; 17] { larger::<[S2; 17]>(({ let mut items = [S2 { f0: 14i32 }; 17]; items.reverse(); items }), ({ let mut items = [S2 { f0: (-477423691i32) }, S2 { f0: 684573590i32 }, S2 { f0: 11i32 }, S2 { f0: (-1342369408i32) }, S2 { f0: 8191i32 }, S2 { f0: (-1i32) }, S2 { f0: 9i32 }, S2 { f0: (-16i32) }, S2 { f0: (-2i32) }, S2 { f0: 10i32 }, S2 { f0: (-1407250305i32) }, S2 { f0: 100i32 }, S2 { f0: 8i32 }, S2 { f0: 17i32 }, S2 { f0: 14i32 }, S2 { f0: 17i32 }, S2 { f0: 15i32 }]; items.swap(0, 16); items })) };
            let p66: [Option<i128>; 4] = ({ let t65: [S2; 4] = [S2 { f0: 1073741823i32 }, S2 { f0: (-2147483647i32) }, S2 { f0: (-4i32) }, S2 { f0: 7i32 }]; [Some(8i128), Some((-35940331962741815898170949521314507252i128)), Some((-6i128)), Some(48804257853431924277202278319375967250i128)] });
            {
                let f68 = |a67: String| -> S2 { ({ let mut m69: S1<i32, f64> = S1 { f0: (-761.5659f64), f1: Vec::<i32>::new() }; let out = f46(&mut m69, (-6isize), 1i128.saturating_add(2i128), S2 { f0: K4 }, (16isize & 7isize)); m69.mix(); out }) };
                let mut v70: BTreeMap<isize, u64> = BTreeMap::new();
                v70.insert((unsafe { std::mem::transmute::<u16, [u8; 2]>(22582u16.overflowing_add(65535u16).0) }[0] as isize), K3).mix();
                v70.insert(isize::from_ne_bytes(6432465149782071142isize.to_ne_bytes()).checked_rem((unsafe { std::mem::transmute::<u64, u64>(K3) } as isize)).unwrap_or(('z' as isize)), 13257028394372436031u64.overflowing_mul(K3).0.overflowing_sub(hash_of(&[Some(String::from("  pad  ")), Some(String::from("0123456789")), Some(String::from("0123456789"))])).0).mix();
                v70.entry(((-707.6620f32) as isize)).or_insert(K3.overflowing_add(1152921504606846975u64).0).mix();
                v70.len().mix();
                let v71: isize = sel::<isize>((p66 > [Some(1048576i128), None::<i128>, None::<i128>, Some(16i128)]), (-9223372036854775807isize), 4611686018427387903isize.overflowing_mul((-1812694405543665939isize)).0);
                f68(({ let mut text = String::from("Abc Def"); text.pop(); text })).mix();
            }
            f56().mix();
        }
        sel::<S1<i32, f64>>((match (String::from("x y").as_str()) { p73 => None::<[Option<i128>; 4]>, _ => Some([Some(129i128), Some((-77530790197717811991957418778311122199i128)), Some(309485009821345068724781055i128), None::<i128>]) }).is_none(), twice::<S1<i32, f64>>(&S1 { f0: (-9223372036854775808.0f64), f1: vec![2i32, 2147483646i32, (-14i32)] }).0, S1 { f0: S1 { f0: f64::EPSILON, f1: vec![(-13i32)] }.f0, f1: twice::<Vec<i32>>(&vec![0i32, 1048575i32]).0 })
    }
    #[inline(never)]
    pub extern "C" fn f75(a76: i32, a77: i32, a78: f64, a79: u64) -> f32 {
        let v80: i32 = (!a76);
        (-(match ([Some((-108592893955670932820842795669720259270i128)); 4]) { [Some(-4i128), p81 @ .., None] => 4294967296.0f32, _ => 1e10f32 }))
    }
    #[inline(never)]
    pub fn f82(a83: &mut [S2; 3]) -> Option<S2> {
        {
            {
                struct Outer(Noisy, Bomb, Noisy);
                impl Drop for Outer { fn drop(&mut self) { feed(0x0d0d + self.0.0 as u64); } }
                caught(|| { let _a = Noisy(3); let mut v0 = vec![Bomb(21), Bomb(52), Bomb(46)]; v0.truncate(1); let _z = Noisy(18); });
            }
            let mut n84 = 0u32;
            'l85: while n84 < 5 && (([Some(String::from("😀")), Some(String::from("a,b,,c")), Some(String::from("line\n"))] == [Some(String::from("0123456789")), Some(String::from("héllo")), Some(String::from("😀"))])) {
                n84 += 1;
                if (({ let t86: String = String::from("a,b,,c"); false })) { return Some(f32::MIN).map(|x87: f32| -> S2 { ({ let mut m88: S1<i32, f64> = S1 { f0: (-9223372036854775808.0f64), f1: vec![1i32, 11i32, 1i32] }; let out = f46(&mut m88, 1555601566840293926isize, 158611006697760810065859547933165685176i128, (*a83)[0], 7isize); m88.mix(); out }) }); }
            }
            (*a83)[1].f0.overflowing_shl(K5).0.mix();
        }
        ({ let mut items = vec![S2 { f0: (-18i32) }, S2 { f0: (-8i32) }, S2 { f0: 9i32 }, S2 { f0: (-1603519699i32) }, S2 { f0: 1i32 }]; items.truncate(1); items }).first().cloned()
    }
    #[inline(never)]
    pub fn f91(a92: S1<i32, f64>, a93: f32, a94: [Option<i128>; 4], a95: &mut S1<i32, f64>, a96: fn(i32) -> S1<i32, f64>) -> u32 {
        {
            let f98 = |a97: [Option<String>; 3]| -> NonZeroI64 { (if ((match (a92.f0) { _ => false, _ => false })) { ('b99: { if (false) { break 'b99 NonZeroI64::new(2i64).unwrap(); } NonZeroI64::new((-7i64)).unwrap() }) } else { NonZeroI64::new(17592186044416i64).unwrap() }) };
            let r100 = catch_unwind(AssertUnwindSafe(|| -> i32 {
                if let p101 = (format!("{:?}", [S2 { f0: 64i32 }, S2 { f0: 2097151i32 }, S2 { f0: 47703853i32 }, S2 { f0: 2147483647i32 }, S2 { f0: 64i32 }, S2 { f0: 0i32 }, S2 { f0: 1073741825i32 }, S2 { f0: (-1i32) }, S2 { f0: 63i32 }, S2 { f0: 0i32 }, S2 { f0: (-132689468i32) }, S2 { f0: 20i32 }, S2 { f0: 100i32 }, S2 { f0: 1073741823i32 }, S2 { f0: 8193i32 }, S2 { f0: 131073i32 }, S2 { f0: 3i32 }]).as_str()) {
                    (-10i32).max(K4).overflowing_sub((unsafe { std::mem::transmute::<i32, [u16; 2]>(K4) }[1] as i32)).0.mix();
                } else {
                    let mut v102: f32 = ((if (false) { (-5i128) } else { (-170141183460469231731687303715884105727i128) }).checked_shl((K5 << K5)).unwrap_or(('\u{80}' as i128)) as f32);
                }
                let mut v103: u64 = u64::try_from((255u8 as i128)).unwrap_or(Some(100u64).unwrap_or(1u64)).overflowing_shr(K5).0;
                let v105: bool = ({ let t106: i32 = K4.wrapping_pow(4294967295u32 % 7); ('b107: { if (false) { break 'b107 false; } false }) });
                assert!(!v105, "held {}", twice::<u32>(&K5).1);
                ((a92.f0.mul_add(f64::NEG_INFINITY, 0.2f64) as i32) & (a92.f0.mul_add((*a95).f0, a92.f0) as i32))
            }));
            match r100 {
                Ok(value) => value.mix(),
                Err(caught) => payload(caught),
            }
            let r108 = catch_unwind(AssertUnwindSafe(|| -> u64 {
                {
                    async fn leaf(x: u64) -> u64 { let _g = Noisy(x as u32 % 9); later((x % 3) as u32, x + 17).await }
                    async fn work(seed: u32) -> u64 { let h109 = vec![Noisy(45), Noisy(seed % 5)]; let w110 = match later(1, seed % 3).await { 0 => later(0, 10u64).await, 1 => 11, _ => later(2, 12u64).await }; (h109.len() as u64).rotate_left(0) ^ (w110).rotate_left(1) }
                    async fn down(n: u32) -> u64 { if n == 0 { leaf(1).await } else { Box::pin(down(n - 1)).await + leaf(n as u64).await } }
                    async fn outer(seed: u32) -> u64 { let n = Noisy(seed); let a = leaf(seed as u64).await; let (b, c) = both(leaf(a), work(seed)).await; let d = async { let _in = Noisy(16); leaf(b ^ c).await }.await; a ^ b ^ c ^ d ^ n.0 as u64 }
                    poll_some(outer(35), 4).mix();
                    { let add = async |x: u64| later(1, x).await + 16; block_on(add(3)).mix(); }
                    block_on(down(3)).mix();
                }
                (if (false) { Ok::<[S2; 4], [Option<f64>; 1]>([S2 { f0: (-8i32) }, S2 { f0: 1108405830i32 }, S2 { f0: 257i32 }, S2 { f0: 2i32 }]).map_err(|x116: [Option<f64>; 1]| -> i8 { (-8i8) }).unwrap_or_else(|x115: i8| -> [S2; 4] { [S2 { f0: 2147483647i32 }, S2 { f0: 16777216i32 }, S2 { f0: (-11i32) }, S2 { f0: (-20i32) }] }) } else { <[S2; 4]>::try_from(vec![S2 { f0: (-16i32) }, S2 { f0: 10i32 }, S2 { f0: 1i32 }]).unwrap_or(({ let t122: u64 = K3; [S2 { f0: 1480571062i32 }; 4] })) }).mix();
                {
                    struct Outer(Noisy, Bomb, Noisy);
                    impl Drop for Outer { fn drop(&mut self) { feed(0x0d0d + self.0.0 as u64); } }
                    caught(|| { let _a = Noisy(38); let mut v0 = vec![Bomb(3), Bomb(51), Bomb(12)]; v0.truncate(1); let _z = Noisy(39); });
                    caught(|| { let _a = Noisy(60); let mut v0 = vec![Bomb(41), Bomb(45), Bomb(23)]; v0.truncate(1); let _z = Noisy(44); });
                }
                ('b123: { if ((K4.overflowing_add(K4).1 || (false && true))) { break 'b123 (hash_of(&19i8) % 10u64.max(K3)); } ((*a95).f0 as u64) })
            }));
            match r108 {
                Ok(value) => value.mix(),
                Err(caught) => payload(caught),
            }
            f98([[S2 { f0: (-16i32) }; 3], [S2 { f0: 1905377039i32 }; 3], [S2 { f0: (-8i32) }, S2 { f0: 3i32 }, S2 { f0: 2147483647i32 }]].map(|x125: [S2; 3]| -> Option<String> { Some(String::from("a,b,,c")) })).mix();
        }
        let v126: S1<i32, f64> = a92;
        {
            let f129 = move |a127: i8, a128: Box<[i128; 2]>| -> S2 { ({ let mut m130: S1<i32, f64> = a96(K4); let out = f46(&mut m130, (-15isize).checked_sub(7isize).unwrap_or(64isize), ((-143911962814615589012097539308386189867i128) >> 100u32), (match (S2 { f0: (-15i32) }) { S2 { .. } => S2 { f0: 1817097034i32 }, S2 { f0: p131 } if true => S2 { f0: 256i32 }, S2 { .. } if true => S2 { f0: (-693612624i32) }, _ => S2 { f0: 7i32 } }), ({ let t132: isize = (-2isize); t132 })); m130.mix(); out }) };
            {
                async fn work(seed: u32) -> u64 { let h133 = Noisy(seed % 10 + 1); let w134 = match later(1, seed % 3).await { 0 => later(0, 10u64).await, 1 => 11, _ => later(2, 12u64).await }; let h135: u128 = (seed as u128) << 91; let w136 = later(0, [seed as u64; 2]).await[0]; let h137 = Noisy(seed % 13 + 1); let w138 = if seed % 2 == 0 { later(1, 5u64).await } else { 57 }; let h139: Option<Box<u16>> = if seed % 2 == 0 { Some(Box::new(55)) } else { None }; let w140 = match later(1, seed % 3).await { 0 => later(0, 10u64).await, 1 => 11, _ => later(2, 12u64).await }; (h133.0 as u64).rotate_left(0) ^ (w134).rotate_left(1) ^ ((h135 >> 3) as u64).rotate_left(2) ^ (w136).rotate_left(3) ^ (h137.0 as u64).rotate_left(4) ^ (w138).rotate_left(5) ^ (h139.as_deref().copied().unwrap_or(1) as u64).rotate_left(6) ^ (w140).rotate_left(7) }
                block_on(work(25)).mix();
            }
            {
                enum Loud { I(i32, Noisy), C(char, Noisy) }
                #[inline(never)] fn items(k: u32, last: char) -> [Loud; 3] { black_box([Loud::I(k as i32 % 4, Noisy(k)), Loud::C('q', Noisy(k + 1)), if k % 3 == 0 { Loud::I(1, Noisy(k + 2)) } else { Loud::C(last, Noisy(k + 2)) }]) }
                #[inline(never)] fn pick0(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { [x @ (Loud::I(..) | Loud::C('a'..='m', _)), Loud::I(..), y] => 1, [.., Loud::C('a', _) | Loud::C(_, _)] if black_box(flag) => 2, [Loud::C('a'..='z', _), Loud::I(..), Loud::I(..)] if black_box(false) => 3, _ => 0 } }
                for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick0(k, last, flag) as u64); feed(0xf00d); } } }
                #[inline(never)] fn pick1(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { [.., Loud::C(..)] => 1, [_, ..] if black_box(!flag) => 2, [.., Loud::C('a'..='c', _)] if black_box(!flag) => 3, [Loud::C('a'..='c', _) | Loud::I(0..=5, _), ..] => 4, [_, Loud::C(..), y @ (Loud::I(..) | Loud::C('a'..='m', _))] => 5, _ => 0 } }
                for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick1(k, last, flag) as u64); feed(0xf00d); } } }
            }
            f129((-4i8).min(127i8), (match (v126.clone()) { _ if false => Box::new([(-170141183460469231731687303715884105727i128), 14i128]), S1 { f0: p141, .. } => Box::new([(-10i128), (-170141183460469231731687303715884105728i128)]), _ => Box::new([1i128, 10i128]) })).mix();
        }
        u32::try_from(0isize.wrapping_neg().min((a93 as isize))).unwrap()
    }
    pub fn run() {
        match (Ok::<S2, S2>(S2 { f0: 268435455i32 }).err()) {
            Some(S2 { f0: 4194304i32 }) => {
                {
                    'l145: for x144 in ((46i32..=50i32).take_while(|x143: &i32| -> bool { 15323u16.overflowing_sub(28u16).1 })) {
                        let v146: Option<[f64; 3]> = None::<[f64; 3]>;
                        None::<[f64; 3]>.filter(|x150: &[f64; 3]| -> bool { true }).ok_or(({ let t151: Option<S2> = Some(S2 { f0: 20i32 }); String::from("line\n") })).ok().mix();
                    }
                    let v152: Box<[i128; 2]> = (unsafe { let mut slot = MaybeUninit::<Box<[i128; 2]>>::uninit(); slot.write((unsafe { let mut slot = MaybeUninit::<Box<[i128; 2]>>::uninit(); slot.write(Box::new([60317277639051382812276334194773036424i128; 2])); slot.assume_init() })); slot.assume_init() });
                    let f154 = move || -> u16 { let v153 = v152; let out = u16::try_from((*v153)[1]).unwrap_or(56103u16.checked_div(128u16).unwrap_or(50056u16)); drop(v153); out };
                    apply_once(f154).mix();
                    let v155: String = (String::from("0123456789") + &String::from("  pad  "));
                    let f157 = move || -> i128 { let v156 = v155; let out = ({ let t158: [Option<i128>; 4] = (if (false) { [None::<i128>, Some(85070591730234615865843651857942052863i128), Some(0i128), Some(68719476735i128)] } else { [None::<i128>, Some((-95736117490286718697779447129692078074i128)), Some(114116545998796057772684720659198401245i128), Some((-1i128))] }); 332306998946228968225951765070086143i128.max(85070591730234615865843651857942052863i128) }); drop(v156); out };
                    f157().mix();
                }
                {
                    enum Loud { I(i32, Noisy), C(char, Noisy) }
                    #[inline(never)] fn items(k: u32, last: char) -> [Loud; 3] { black_box([Loud::I(k as i32 % 4, Noisy(k)), Loud::C('q', Noisy(k + 1)), if k % 3 == 0 { Loud::I(1, Noisy(k + 2)) } else { Loud::C(last, Noisy(k + 2)) }]) }
                    #[inline(never)] fn pick0(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { _ if black_box(flag) => 1, [Loud::C('a'..='c', _) | Loud::I(1, _), Loud::C('Z', _), y] if black_box(!flag) => 2, _ => 0 } }
                    for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick0(k, last, flag) as u64); feed(0xf00d); } } }
                    #[inline(never)] fn pick1(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { [.., Loud::C('a', _)] => 1, [.., _] if black_box(!flag) => 2, _ => 0 } }
                    for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick1(k, last, flag) as u64); feed(0xf00d); } } }
                    #[inline(never)] fn pick2(k: u32, last: char, flag: bool) -> u8 { match items(k, last) { _ => 1, [_, m @ .., Loud::C('a'..='z', _)] if black_box(!flag) => 2, _ => 0 } }
                    for k in 0..4u32 { for last in ['a', 'Z', 'q'] { for flag in [false, true] { feed(pick2(k, last, flag) as u64); feed(0xf00d); } } }
                    #[inline(never)] fn pair(k: u32, last: char, flag: bool) -> u8 { match (items(k, last), k % 3, flag) { ([Loud::I(..), ..], 1, true) => 1, ([.., Loud::C(..)], 2, _) => 2, ([x, m @ ..], 1, false) => 3, (all, _, _) if black_box(flag) => 4, _ => 0 } }
                    for k in 0..4u32 { for last in ['a', 'Z'] { for flag in [false, true] { feed(pair(k, last, flag) as u64); feed(0xf00d); } } }
                }
            }
            _ => NonZeroI64::new(100i64).unwrap_or(NonZeroI64::new(6i64).unwrap()).mix(),
        }
        let mut n161 = 0u32;
        'l162: loop {
            n161 += 1;
            if n161 > 8 { break; }
            let [S2 { f0: -1093165251i32 }, S2 { f0: 1i32 }, S2 { f0: -12i32..-2i32 }] = (({ let t166: NonZeroI64 = NonZeroI64::new((-9223372036854775807i64)).unwrap(); [S2 { f0: (-2147483647i32) }, S2 { f0: (-19i32) }, S2 { f0: 2147483646i32 }] })) else { continue 'l162; };
            let Some(p168) = (None::<[f64; 3]>) else { break 'l162; };
        }
        let mut v169: Vec<String> = vec![format!("{}", 1048575isize.overflowing_shr(K5).0)];
        v169.truncate(6);
        'l171: while let Some(x170) = v169.pop() {
            let mut n173 = 0u32;
            'l174: while n173 < 8 && (({ let t175: S1<i32, f64> = S1 { f0: f64::NAN, f1: vec![1048575i32] }; true })) {
                n173 += 1;
                let mut v176: BTreeMap<[Option<i128>; 4], i8> = BTreeMap::new();
                v176.insert(({ let mut items = <[Option<i128>; 4]>::try_from(vec![Some(8i128), Some(42535295865117307932921825928971026431i128)]).unwrap_or([Some(0i128), Some(332306998946228968225951765070086143i128), None::<i128>, Some(15i128)]); items.rotate_left(0); items }), 0i8).mix();
                v176.insert([Some(10i128), Some(5i128), Some(20i128), None::<i128>], (-45i8)).mix();
                v176.remove(&[None::<i128>, Some((-40884184379056262690849822941879911608i128)), Some(110700248049624581021960874135777881092i128), Some((-8i128))]).mix();
                v176.len().mix();
                {
                    let f187 = || -> f32 { f75(((-2.5e-3f64) as i32), twice::<i32>(&(-18i32)).0, (-9223372036854775808.0f64).mul_add(f64::MIN_POSITIVE, 865.3109f64), (K3.overflowing_mul(K3).1 as u64)) };
                    let mut v188: u32 = K5;
                    {
                        for armed in [false, true] { caught(|| { let t = (Noisy(53), Box::new(Duo { a: Noisy(60), b: Noisy(49) })); let _x = t.1.a; trip(armed, ()); let _y = t.0; }); }
                        for armed in [false, true] { caught(|| { let t = Box::new((Noisy(53), Noisy(60), Noisy(49))); let _x = t.2; trip(armed, ()); let _y = t.0; let _w = t.1; }); }
                    }
                    {
                        async fn leaf(x: u64) -> u64 { let _g = Noisy(x as u32 % 9); later((x % 3) as u32, x + 17).await }
                        async fn work(seed: u32) -> u64 { let h189: Option<Box<u16>> = if seed % 2 == 0 { Some(Box::new(13)) } else { None }; let w190 = if seed % 2 == 0 { later(1, 5u64).await } else { 60 }; (h189.as_deref().copied().unwrap_or(1) as u64).rotate_left(0) ^ (w190).rotate_left(1) }
                        async fn down(n: u32) -> u64 { if n == 0 { leaf(1).await } else { Box::pin(down(n - 1)).await + leaf(n as u64).await } }
                        async fn outer(seed: u32) -> u64 { let n = Noisy(seed); let a = leaf(seed as u64).await; let (b, c) = both(leaf(a), work(seed)).await; let d = async { let _in = Noisy(3); leaf(b ^ c).await }.await; a ^ b ^ c ^ d ^ n.0 as u64 }
                        block_on(down(1)).mix();
                        block_on(outer(20)).mix();
                        { let n = Noisy(17); let captured = async move { later(2, ()).await; n.0 }; poll_some(captured, 1).mix(); }
                        { let n = Noisy(3); let by_ref = async { later(1, ()).await; n.0 }; block_on(by_ref).mix(); n.mix(); }
                    }
                    f187().mix();
                }
            }
            1801890350u32.checked_add(K5).unwrap_or(K5).mix();
            let v191: String = ({ let mut text = String::from("😀"); text.push('\u{7f}'); text });
            let f193 = move || -> S2 { let v192 = v191; let out = ('b194: { if (0i8.overflowing_sub(8i8).1) { break 'b194 ({ let mut m195: S1<i32, f64> = S1 { f0: (-309.5093f64), f1: vec![7i32, 9i32, 4i32, 4i32, 1073741825i32] }; let out = f46(&mut m195, 18014398509481984isize, (-67340569782192156799458143258571003075i128), S2 { f0: 524289i32 }, 18isize); m195.mix(); out }); } S2 { f0: K4 } }); drop(v192); out };
            f193().mix();
        }
        {
            let f196 = || -> String { String::from("héllo") };
            let v197: String = ({ let mut text = String::from("0123456789"); text.push_str("  pad  "); text });
            let r198 = catch_unwind(AssertUnwindSafe(|| -> [Option<String>; 3] {
                let mut n199 = 0u32;
                'l200: loop {
                    n199 += 1;
                    if n199 > 2 { break; }
                    [u32::try_from(64u8).unwrap(), ({ let mut m202: S1<i32, f64> = S1 { f0: 0.1f64, f1: vec![1073741823i32, (-5i32), 67108864i32] }; let out = f91(S1 { f0: f64::MIN, f1: vec![0i32, 33554432i32, 15i32] }, 0.2f32, [Some((-10i128)), Some(8i128), Some(85070591730234615865843651857942052863i128), Some(1i128)], &mut m202, f54); m202.mix(); out }), K5][Some(1usize).unwrap_or(40usize)].mix();
                    let mut v204: String = f196();
                }
                {
                    let mut items: Vec<Box<dyn Any>> = vec![Box::new(String::from("owned")), Box::new(()), Box::new([8u16; 3]), Box::new(Some(31i64)), Box::new(31u32), Box::new(1.5f64), Box::new("text")];
                    for item in &items { if let Some(it) = item.downcast_ref::<u32>() { feed(*it as u64); } else if let Some(it) = item.downcast_ref::<&str>() { it.mix(); } else if item.is::<Noisy>() { feed(1); } else if let Some(it) = item.downcast_ref::<(u8, bool)>() { feed(it.0 as u64 + it.1 as u64); } else { feed(2); } }
                    while let Some(item) = items.pop() { match item.downcast::<Noisy>() { Ok(it) => it.mix(), Err(other) => feed(other.is::<String>() as u64) } }
                }
                ({ let mut items = <[Option<String>; 3]>::try_from(({ let mut items = Vec::<Option<String>>::new(); let end = items.len().min(3); items.drain(..end).collect::<Vec<Option<String>>>() })).unwrap_or(Some([Some(String::from("line\n")), None::<String>, Some(String::from(""))]).expect("nothing there")); items.swap(0, 2); items })
            }));
            match r198 {
                Ok(value) => value.mix(),
                Err(caught) => payload(caught),
            }
            let mut n212 = 0u32;
            'l213: loop {
                n212 += 1;
                if n212 > 8 { break; }
                ({ let mut items = ({ let mut items = vec![None::<i32>, Some(67108865i32), None::<i32>, Some(131072i32), Some(2i32)]; items.retain(|x217: &Option<i32>| -> bool { true }); items }); items.insert(0, (-1317837240i32).checked_mul(K4)); items }).mix();
                let v218: Box<[i128; 2]> = (if (true) { Box::new([5i128, 74275280242170100799942021501971752833i128]) } else { Box::new([(-1i128), (-5i128)]) });
                let f220 = move || -> S1<i32, f64> { let v219 = v218; let out = S1 { f0: f64::MIN_POSITIVE, f1: vec![2146640718i32, 100i32] }; drop(v219); out };
                apply_boxed(Box::new(f220)).mix();
                if (true.then(|| -> Result<f64, [f64; 2]> { Ok::<f64, [f64; 2]>(607.5213f64) }).is_none()) {
                    let v222: Box<[i128; 2]> = ({ let t225: Result<f64, [f64; 2]> = Ok::<f64, Box<[i128; 2]>>(1.0f64).map_err(|x226: Box<[i128; 2]>| -> [f64; 2] { [(-0.0f64); 2] }); (match (v197.clone().as_str()) { "" => Box::new([3i128, (-9i128)]), _ => Box::new([6381860361484986569879783976983375544i128, 7i128]) }) });
                    let f224 = move || -> NonZeroU8 { let v223 = v222; let out = NonZeroU8::new(((-787.3315f64) as u8)).unwrap_or(NonZeroU8::new(64u8).unwrap_or(NonZeroU8::new(2u8).unwrap())); drop(v223); out };
                    f224().mix();
                } else {
                    let g227: extern "C" fn(i32, i32, f64, u64) -> f32 = f75;
                    let mut v228: f32 = (if (v197.clone().ends_with("0123456789")) { (-f32::MIN_POSITIVE) } else { (-1.5f32) });
                }
                let r229 = catch_unwind(AssertUnwindSafe(|| -> [Option<String>; 3] {
                    let v230: String = format!("{:?}", 65535u16).trim().to_string();
                    let mut v231: f32 = pick::<f32>(true, &(-9223372036854775808.0f32), &4294967296.0f32).clone().clamp(-1.5f32, 2.5f32);
                    (match (v230.clone().replace("0123456789", "a").as_str()) { "héllo" => [None::<String>, None::<String>, None::<String>], "ff" if None::<f32>.is_none() => ({ let mut items = [Some(String::from("line\n")), None::<String>, None::<String>]; items.reverse(); items }), "-7" => [None::<String>, Some(String::from("line\n")), None::<String>], _ => [Some(String::from("3.5")), Some(String::from("3.5")), Some(String::from("héllo"))] })
                }));
                match r229 {
                    Ok(value) => value.mix(),
                    Err(caught) => payload(caught),
                }
            }
            f196().mix();
        }
        let mut v240: BTreeMap<NonZeroU8, [Option<String>; 3]> = BTreeMap::new();
        v240.insert(NonZeroU8::new((180u8.abs_diff(16u8) as u8)).unwrap_or(NonZeroU8::new(36u8).unwrap()), <[Option<String>; 3]>::try_from((match (String::from("42").as_str()) { "3.5" => vec![Some(String::from("  pad  ")), None::<String>, Some(String::from("line\n")), None::<String>, Some(String::from("  pad  "))], _ => Vec::<Option<String>>::new() })).unwrap_or([Some(String::from("line\n")), None::<String>, None::<String>])).mix();
        v240.insert(NonZeroU8::new(5u8.checked_shl(K5).unwrap_or(21u8)).unwrap_or(NonZeroU8::new(26u8).unwrap_or(NonZeroU8::new(7u8).unwrap())), [Err::<String, S1<i32, f64>>(S1 { f0: 570.9789f64, f1: vec![(-2147483648i32), 13i32, (-2147483648i32), 1i32, 64i32] }).ok(), Some(String::from("3.5")).or(Some(String::from("3.5"))), [Some(String::from("line\n")), Some(String::from("Abc Def")), Some(String::from("ff"))][1].clone()]).mix();
        v240.insert(NonZeroU8::new(128u8).unwrap(), ('b251: { if (4294967296.0f32.is_normal()) { break 'b251 (unsafe { let mut slot = MaybeUninit::<[Option<String>; 3]>::uninit(); slot.write([Some(String::from("")), None::<String>, Some(String::from("  pad  "))]); slot.assume_init() }); } twice::<[Option<String>; 3]>(&[Some(String::from("x y")), None::<String>, None::<String>]).0 })).mix();
        v240.insert(NonZeroU8::new((17u8.abs_diff(1u8) as u8)).unwrap_or(NonZeroU8::new(0u8).unwrap_or(NonZeroU8::new(206u8).unwrap())), [(match ([None::<f64>]) { [Some(p259), ..] => Some(String::from("line\n")), [.., Some(p260)] => Some(String::from("😀")), [Some(_)] if false => None::<String>, _ => Some(String::from("Abc Def")) }), (match (Box::new(Some(0.5f32))) { p262 => None::<String>, _ => Some(String::from("Abc Def")) }), None::<String>.xor(None::<String>)]).mix();
        for (key, value) in v240.iter().rev() { key.mix(); value.mix(); }
        v240.len().mix();
        K4.wrapping_div(K4 | 1).mix();
        match (1.5f32.mul_add(f32::MIN, f32::NEG_INFINITY)) {
            _ if Some((-6i8)).is_some() => {
                let [S2 { .. }, p275 @ .., S2 { .. }]: [S2; 3] = twice::<[S2; 3]>(&<[S2; 3]>::try_from(Vec::<S2>::new()).unwrap_or([S2 { f0: 3i32 }, S2 { f0: (-20i32) }, S2 { f0: (-5i32) }])).0;
            }
            p276 if Ok::<bool, NonZeroU8>(false).unwrap_or(false) => {
                let mut v278: i32 = (262144i32.overflowing_sub(K4).0.abs_diff(i32::try_from(64i64).unwrap_or((-10i32))) as i32).checked_neg().unwrap_or_default();
                (if ((true || 7.0f64.is_infinite())) { (-9223372036854775807isize) } else { ((-128.9f64) as isize).clamp(18isize, 18014398509481984isize) }).mix();
            }
            p279 if twice::<bool>(&false).1 => {
                let mut v280: S1<i32, f64> = ({ let t281: char = 'Z'; S1 { f0: S1 { f0: f64::MAX, f1: vec![(-1i32), (-16i32), 16384i32, (-16i32), (-6i32)] }.f0, f1: ({ let t283: [Option<String>; 3] = [Some(String::from("  pad  ")), None::<String>, Some(String::from("line\n"))]; vec![1073741823i32] }) } });
            }
            _ => {}
        }
        {
            let v284: u16 = ('b285: { if (matches!('0', ('\0'))) { break 'b285 ((-899.2214f64) as u16); } u16::from_str_radix("x y", 2).unwrap_or(256u16) });
            let S2 { f0: _ }: S2 = S2 { f0: 11i32 };
            let mut n286 = 0u32;
            let v288: [S2; 3] = 'l287: loop {
                n286 += 1;
                if n286 > 1 { break [S2 { f0: 1193409320i32 }, S2 { f0: (-1240309076i32) }, S2 { f0: 2147483646i32 }]; }
                {
                    struct Outer(Noisy, Bomb, Noisy);
                    impl Drop for Outer { fn drop(&mut self) { feed(0x0d0d + self.0.0 as u64); } }
                    caught(|| { let _a = Noisy(30); drop(vec![Box::new(Noisy(47)) as Box<dyn Any>, Box::new(Bomb(31)), Box::new(Noisy(55))]); take3(Noisy(1), (Noisy(3), [Bomb(39)], Box::new(Noisy(6))), Noisy(38)); let _z = Noisy(33); });
                    caught(|| { let _a = Noisy(44); let _h0 = (Noisy(28), [Bomb(34)], Box::new(Noisy(44))); let _z = Noisy(60); });
                }
                if (((false ^ true) & 3.25f32.is_infinite())) {
                    let g289: fn(i32) -> S1<i32, f64> = f54;
                    let mut v290: [Option<String>; 3] = [Some(String::from("a")), None::<String>, Some(String::from("a,b,,c"))];
                    if ((twice::<[S2; 4]>(&[S2 { f0: 1073741823i32 }, S2 { f0: 8i32 }, S2 { f0: 2147483646i32 }, S2 { f0: 2147483646i32 }]).1 <= ({ let mut items = [S2 { f0: (-1i32) }, S2 { f0: 1187462286i32 }, S2 { f0: 10i32 }, S2 { f0: 8i32 }]; items.swap(0, 3); items }))) { break 'l287 <[S2; 3]>::try_from(vec![S2 { f0: (-2147483648i32) }, S2 { f0: (-11i32) }, S2 { f0: 7i32 }]).unwrap_or([S2 { f0: 4i32 }; 3]); }
                } else {
                    let [Some(p304)] = ((match (String::from("x y").as_str()) { p302 => [None::<f64>; 1], "-7" => [None::<f64>], p303 if false => [None::<f64>; 1], _ => [Some((-128.9f64)); 1] })) else { break 'l287 ('b299: { if (true) { break 'b299 [S2 { f0: (-1i32) }, S2 { f0: (-16i32) }, S2 { f0: (-2147483647i32) }]; } [S2 { f0: 256i32 }; 3] }); };
                    let mut v305: Option<[f64; 3]> = ({ let t307: NonZeroI64 = (match ([Some((-123.0591f64))]) { _ => NonZeroI64::new(2305843009213693952i64).unwrap(), _ => NonZeroI64::new(2i64).unwrap() }); Ok::<isize, [f64; 3]>(8949558916623354726isize).err() });
                }
                'l312: for x311 in (None::<Vec<Result<f64, [f64; 2]>>>.map_or(Vec::<Result<f64, [f64; 2]>>::new(), |x310: Vec<Result<f64, [f64; 2]>>| -> Vec<Result<f64, [f64; 2]>> { x310.clone() }).into_iter()) {
                    ({ let mut items = sel::<[S2; 17]>(matches!(None::<S1<i32, f64>>, (None)), ({ let mut items = [S2 { f0: 10i32 }, S2 { f0: 0i32 }, S2 { f0: 7i32 }, S2 { f0: (-12i32) }, S2 { f0: 256i32 }, S2 { f0: 2147483647i32 }, S2 { f0: (-2147483648i32) }, S2 { f0: 10i32 }, S2 { f0: 10i32 }, S2 { f0: (-13i32) }, S2 { f0: 5i32 }, S2 { f0: 10i32 }, S2 { f0: 131071i32 }, S2 { f0: 17i32 }, S2 { f0: 2147483646i32 }, S2 { f0: 0i32 }, S2 { f0: 1400775007i32 }]; items.sort_by(|a, b| b.cmp(a)); items }), (if (false) { [S2 { f0: 3i32 }; 17] } else { [S2 { f0: (-2029821917i32) }; 17] })); items.sort_unstable(); items }).mix();
                    [Some(String::from("a")), Some(String::from("a")), Some(String::from("42"))].mix();
                    let v323: Option<S1<i32, f64>> = (match ((match (String::from("42").as_str()) { "x y" => [Some(164287612135635251591565690821209589499i128), None::<i128>, Some(2i128), Some(100i128)], "42" => [Some(4722366482869645213696i128), None::<i128>, Some(1i128), Some((-5i128))], _ => [Some(11i128), None::<i128>, Some(604462909807314587353089i128), Some(11610703149992802726405491995381789073i128)] })) { [None, None, Some(0i128), None] if ('b329: { if (true) { break 'b329 false; } false }) => None::<S1<i32, f64>>, _ => Ok::<i128, S1<i32, f64>>((-13i128)).err() });
                    let f325 = move || -> u16 { let v324 = v323; let out = twice::<u16>(&(v284 & v284)).0; drop(v324); out };
                    apply_boxed(Box::new(f325)).mix();
                    K3.mix();
                }
            };
            let [_, p333 @ .., p334]: [Option<String>; 3] = [Some(String::from("line\n")), Some(String::from("")), None::<String>];
        }
        let mut v335: u16 = (unsafe { std::mem::transmute::<u64, i64>((65535.0f64 as u64)) } as u16);
        let mut n336 = 0u32;
        'l337: loop {
            n336 += 1;
            if n336 > 3 { break; }
            if (matches!(None::<String>, (None))) {
                let [p342]: [String; 1] = [String::from("-7")];
            } else {
                v335 = 39833u16.checked_shr(K5).unwrap_or(v335);
            }
            let r343 = catch_unwind(AssertUnwindSafe(|| -> [u128; 5] {
                let r344 = catch_unwind(AssertUnwindSafe(|| -> NonZeroI64 {
                    u64::try_from(7u8).unwrap_or(34u64).mix();
                    v335 ^= ((18446744073709551614usize as u16) / v335.checked_shl(K5).unwrap_or(0u16));
                    (({ let t345: i16 = 7i16; 0.0f32 }) as u64).mix();
                    Err::<NonZeroI64, f64>(f64::INFINITY).unwrap_or_else(|x346: f64| -> NonZeroI64 { NonZeroI64::new((-11i64)).unwrap() })
                }));
                match r344 {
                    Ok(value) => value.mix(),
                    Err(caught) => payload(caught),
                }
                {
                    for armed in [false, true] { caught(|| { let _v = if armed { (Noisy(51), trip(armed, Noisy(1))) } else { (Noisy(7), Noisy(35)) }; }); }
                    for armed in [false, true] { caught(|| { take3(Noisy(33), trip(armed, Noisy(21)), Noisy(37)); }); }
                }
                v335 = ((match (String::from("a").as_str()) { p347 if false => 45025u16, _ => v335 }).clamp(2u16, 57926u16) << ('\'' as u32));
                None::<[u128; 5]>.filter(|x351: &[u128; 5]| -> bool { 6473176623681768245isize.overflowing_sub(4611686018427387903isize).1 }).unwrap_or_else(|| -> [u128; 5] { sel::<[u128; 5]>(None::<Option<S2>>.is_none(), [Some([(-406.7636f32); 3]); 5].map(|x355: Option<[f32; 3]>| -> u128 { 3u128 }), twice::<[u128; 5]>(&[13u128, 19342813113834066795298816u128, 19342813113834066795298815u128, 20282409603651670423947251286016u128, 0u128]).0) })
            }));
            match r343 {
                Ok(value) => value.mix(),
                Err(caught) => payload(caught),
            }
        }
        (9u32.overflowing_shr(2048u32).0.abs_diff((match ([S2 { f0: 33554431i32 }, S2 { f0: 8192i32 }, S2 { f0: 33i32 }]) { [S2 { .. }, S2 { .. }, S2 { f0: p358 @ 7i32..=255i32 }] => 100u32, [.., S2 { f0: p359 }] => K5, [S2 { f0: ..=2147483646i32 }, S2 { f0: 8i32..9i32 }, S2 { f0: _ }] => K5, _ => K5 })) as u32).mix();
        {
            static NUMBERS: [u32; 4] = [32, 20, 58, 4];
            static SECOND: &u32 = &NUMBERS[1];
            static TAIL: &[u32] = match &NUMBERS { [_, _, rest @ ..] => rest };
            static WORDS: [&str; 3] = ["one", "two", "three"];
            static PICKED: &&str = &WORDS[2];
            struct Link { value: u32, next: Option<&'static Link>, call: fn(u32) -> u32, name: &'static str, shown: &'static (dyn Debug + Sync) }
            fn double(x: u32) -> u32 { x.wrapping_mul(2) }
            static END: Link = Link { value: 3, next: None, call: double, name: "end", shown: &NUMBERS };
            static MID: Link = Link { value: 32, next: Some(&END), call: |x| x + 1, name: WORDS[0], shown: &"mid" };
            static HEAD: Link = Link { value: 20, next: Some(&MID), call: double, name: "head", shown: &MID.value };
            static TABLE: [fn(u32) -> u32; 3] = [double, |x| x ^ 5, HEAD.call];
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            static LISTS: (&[u8], &[&[u8]], [Option<&u32>; 2]) = (b"bytes", &[b"a", b"", &[32, 2]], [Some(&NUMBERS[3]), None]);
            const VIEW: &[u32; 4] = &NUMBERS;
            thread_local! { static LOCAL: Cell<u32> = const { Cell::new(5) }; static NOTES: RefCell<Vec<&'static u32>> = const { RefCell::new(Vec::new()) }; }
            { let mut at = Some(&HEAD); while let Some(link) = at { feed((link.call)(link.value) as u64); link.name.mix(); format!("{:?}", link.shown).mix(); at = link.next; } }
            PICKED.mix(); feed(std::ptr::eq(*PICKED, WORDS[2]) as u64);
            LOCAL.with(|it| { it.set(it.get() + 1); feed(it.get() as u64 % 2 + 40); }); NOTES.with(|it| { it.borrow_mut().push(SECOND); feed(*it.borrow()[0] as u64); });
            { let before = COUNTER.fetch_add(3, Ordering::Relaxed); feed((COUNTER.load(Ordering::Relaxed) - before) as u64); }
            for (i, f) in TABLE.iter().enumerate() { feed(f(i as u32 + 10) as u64); }
        }
        v335 = (v335.abs_diff(v335) as u16).overflowing_mul(v335.checked_shl(2056224352u32).unwrap_or(v335)).0;
        if let None = (None::<[f32; 3]>) {
            v335 ^= (match (Some(v335)) { None => 31u16, Some(x361) => x361 });
            let [p368]: [String; 1] = ({ let t365: f32 = (-122.9174f32); (match (String::from("").as_str()) { "hello" if true => [String::from("ff")], "3.5" => [String::from("42")], "Abc Def" => [String::from("hello")], _ => [String::from("  pad  ")] }) });
        }
        'l370: for x369 in ((14i32..14i32)) {
            'l373: for x372 in ((69i8..75i8).chain(vec![126i8, 9i8, 1i8, 7i8, (-13i8)]).collect::<Vec<_>>()) {
                let S2 { f0: p376 }: S2 = ({ let mut m374: S1<i32, f64> = f54(x369); let out = f46(&mut m374, (-3isize), i128::from_str_radix("😀", 36).unwrap_or(2i128).checked_rem((unsafe { std::mem::transmute::<u128, [i64; 2]>(41538374868278621028243970633760769u128) }[0] as i128)).unwrap_or((-1i128).checked_shr(K5).unwrap_or(7i128)), twice::<S2>(&('b375: { if (false) { break 'b375 S2 { f0: 8i32 }; } S2 { f0: 3i32 } })).0, isize::try_from(2383833748u32).unwrap_or(((-1isize) ^ 14isize))); m374.mix(); out });
            }
            if ((Some(19i128).is_some() && true)) {
                if let Some(S1 { f1: p377, .. }) = (None::<S1<i32, f64>>) {
                    let S1 { f0: _, f1: p378 }: S1<i32, f64> = f54(K4);
                }
            } else {
                ({ let mut text = String::from("line\n").to_uppercase(); text.push((match ([None::<f64>]) { [.., Some(p379)] => 'é', [.., Some(p380)] => '中', _ => 'a' })); text }).mix();
            }
        }
        let S1 { .. }: S1<i32, f64> = S1 { f0: 1e30f64, f1: vec![8i32] };
        if (('b381: { if (matches!([7u128, 29u128, 85070591730234615865843651857942052864u128, 1u128, 1048576u128], ([20769187434139310514121985316880383u128, .., _]))) { break 'b381 false; } false })) {
            'l383: for x382 in (({ let mut text = String::from("ff"); text.push_str("hello"); text }).bytes().collect::<Vec<_>>()) {
                Some((-347.3646f64)).mix();
                let mut n384 = 0u32;
                'l385: while n384 < 8 && ((96u64..98u64).all(|x386: u64| -> bool { false })) {
                    n384 += 1;
                    NonZeroI64::new(1i64.overflowing_sub(1152921504606846975i64).0).unwrap_or(('b387: { if (true) { break 'b387 NonZeroI64::new(10i64).unwrap(); } NonZeroI64::new(9054727411684476544i64).unwrap() })).mix();
                    let v388: Option<S1<i32, f64>> = None::<S1<i32, f64>>;
                    {
                        async fn leaf(x: u64) -> u64 { let _g = Noisy(x as u32 % 9); later((x % 3) as u32, x + 15).await }
                        async fn work(seed: u32) -> u64 { let h389 = Noisy(seed % 1 + 1); let w390 = later(1, [seed as u64; 2]).await[0]; (h389.0 as u64).rotate_left(0) ^ (w390).rotate_left(1) }
                        async fn down(n: u32) -> u64 { if n == 0 { leaf(1).await } else { Box::pin(down(n - 1)).await + leaf(n as u64).await } }
                        async fn outer(seed: u32) -> u64 { let n = Noisy(seed); let a = leaf(seed as u64).await; let (b, c) = both(leaf(a), work(seed)).await; let d = async { let _in = Noisy(27); leaf(b ^ c).await }.await; a ^ b ^ c ^ d ^ n.0 as u64 }
                        { let add = async |x: u64| later(1, x).await + 27; block_on(add(3)).mix(); }
                        block_on(down(4)).mix();
                    }
                }
            }
            {
                let f392 = move |a391: [Option<String>; 3]| -> [S2; 4] { twice::<[S2; 4]>(&({ let t397: String = String::from("x y"); [S2 { f0: 678744612i32 }, S2 { f0: 256i32 }, S2 { f0: 134217729i32 }, S2 { f0: (-11i32) }] })).1 };
                let r398 = catch_unwind(AssertUnwindSafe(|| -> u16 {
                    Err::<f64, NonZeroU8>(NonZeroU8::new(186u8).unwrap()).unwrap_or_else(|x399: NonZeroU8| -> f64 { (match (String::from("-7").replace("a,b,,c", "a").as_str()) { "line\n" => (match ([String::from("42"), String::from("ff")]) { _ => f64::NEG_INFINITY, _ => 862.3946f64 }), "héllo" => f64::from(K5), "a,b,,c" if true => (0.2f64 * 806.4467f64), _ => ({ let t400: Option<String> = Some(String::from("42")); f64::MIN }) }) }).mix();
                    (twice::<u16>(&v335).1 / v335.overflowing_sub(v335).0)
                }));
                match r398 {
                    Ok(value) => value.mix(),
                    Err(caught) => payload(caught),
                }
                let r401 = catch_unwind(AssertUnwindSafe(|| -> Option<f64> {
                    Err::<S1<i32, f64>, S2>(('b404: { if (false) { break 'b404 S2 { f0: 7i32 }; } S2 { f0: 6i32 } })).ok().mix();
                    {
                        static NUMBERS: [u32; 4] = [36, 55, 44, 4];
                        static SECOND: &u32 = &NUMBERS[1];
                        static TAIL: &[u32] = match &NUMBERS { [_, _, rest @ ..] => rest };
                        static WORDS: [&str; 3] = ["one", "two", "three"];
                        static PICKED: &&str = &WORDS[2];
                        struct Link { value: u32, next: Option<&'static Link>, call: fn(u32) -> u32, name: &'static str, shown: &'static (dyn Debug + Sync) }
                        fn double(x: u32) -> u32 { x.wrapping_mul(2) }
                        static END: Link = Link { value: 3, next: None, call: double, name: "end", shown: &NUMBERS };
                        static MID: Link = Link { value: 36, next: Some(&END), call: |x| x + 1, name: WORDS[0], shown: &"mid" };
                        static HEAD: Link = Link { value: 55, next: Some(&MID), call: double, name: "head", shown: &MID.value };
                        static TABLE: [fn(u32) -> u32; 3] = [double, |x| x ^ 5, HEAD.call];
                        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                        static LISTS: (&[u8], &[&[u8]], [Option<&u32>; 2]) = (b"bytes", &[b"a", b"", &[36, 2]], [Some(&NUMBERS[3]), None]);
                        const VIEW: &[u32; 4] = &NUMBERS;
                        thread_local! { static LOCAL: Cell<u32> = const { Cell::new(5) }; static NOTES: RefCell<Vec<&'static u32>> = const { RefCell::new(Vec::new()) }; }
                        for (i, f) in TABLE.iter().enumerate() { feed(f(i as u32 + 10) as u64); }
                        { let mut at = Some(&HEAD); while let Some(link) = at { feed((link.call)(link.value) as u64); link.name.mix(); format!("{:?}", link.shown).mix(); at = link.next; } }
                        { let before = COUNTER.fetch_add(3, Ordering::Relaxed); feed((COUNTER.load(Ordering::Relaxed) - before) as u64); }
                        feed(*SECOND as u64 + TAIL.len() as u64 + std::ptr::eq(SECOND, &NUMBERS[1]) as u64 * 16 + std::ptr::eq(TAIL.as_ptr(), &NUMBERS[2]) as u64 * 32);
                    }
                    (unsafe { let mut slot = MaybeUninit::<Option<f64>>::uninit(); slot.write(vec![f64::MIN_POSITIVE, f64::MIN_POSITIVE, 1e10f64].get(3).cloned()); slot.assume_init() })
                }));
                match r401 {
                    Ok(value) => value.mix(),
                    Err(caught) => payload(caught),
                }
                f392(pick::<[Option<String>; 3]>(false, &[None::<String>, Some(String::from("-7")), None::<String>], &[Some(String::from("😀")), None::<String>, Some(String::from("x y"))]).clone()).mix();
            }
            if let "hello" = (({ let mut text = String::from("héllo"); text.push('9'); text }).as_str()) {
                let mut v409: Vec<Result<f64, [f64; 2]>> = [vec![Err::<f64, [f64; 2]>([0.5f64; 2])], vec![Err::<f64, [f64; 2]>([(-128.9f64); 2]), Ok::<f64, [f64; 2]>((-2.5e-3f64)), Ok::<f64, [f64; 2]>(252.9401f64)]].concat();
                v409.truncate(6);
                'l411: while let Some(x410) = v409.pop() {
                    v335 = (2i8 as u16);
                    let v413: Box<[i128; 2]> = twice::<Box<[i128; 2]>>(&({ let t416: i128 = 79178865674315256406537927648215854740i128; Box::new([(-13i128); 2]) })).1;
                    let f415 = move || -> Option<S1<i32, f64>> { let v414 = v413; let out = twice::<Option<S1<i32, f64>>>(&Some([None::<f64>; 1]).map(|x418: [Option<f64>; 1]| -> S1<i32, f64> { S1 { f0: 65535.0f64, f1: vec![8i32, 7i32] } })).1; drop(v414); out };
                    f415().mix();
                    let g419: fn(i32) -> S1<i32, f64> = (|a420: i32| -> S1<i32, f64> { S1 { f0: S1 { f0: f64::NAN, f1: vec![(-700347403i32), 1646293803i32, 17i32] }.f0, f1: S1 { f0: f64::NEG_INFINITY, f1: Vec::<i32>::new() }.f1 } });
                    sel::<u64>((255.5f64 < 1.5f64), (K3.overflowing_sub(K3).1 as u64), u64::from_ne_bytes(K3.to_be_bytes())).mix();
                }
            } else {
                'l423: for x422 in ((53u32..59u32)) {
                    let mut v424: [u128; 5] = [39u128; 5];
                }
            }
        }
    }
}

mod c4 {
    use super::*;
    #[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord, Hash)]
    pub struct S1 { pub f0: char }
    impl Mix for S1 {
        fn mix(&self) {
            feed(1);
            self.f0.mix();
        }
    }
    impl Drop for S1 {
        fn drop(&mut self) {
            feed(0x92694393);
            self.mix();
        }
    }
    #[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord, Hash)]
    pub enum E2 {
        V0(String, String, String),
        V1(char),
        V2,
    }
    impl Mix for E2 {
        fn mix(&self) {
            match self {
                E2::V0(a0, a1, a2) => { feed(10); a0.mix(); a1.mix(); a2.mix(); }
                E2::V1(a0) => { feed(11); a0.mix(); }
                E2::V2 => { feed(12); }
            }
        }
    }
    #[derive(Clone, Debug, PartialEq, PartialOrd)]
    pub enum E3 {
        V0 { f0: String },
        V1(i128),
        V2(f32, char),
        V3(S1, f64, String),
        V4(f32),
    }
    impl Mix for E3 {
        fn mix(&self) {
            match self {
                E3::V0 { f0: a0 } => { feed(10); a0.mix(); }
                E3::V1(a0) => { feed(11); a0.mix(); }
                E3::V2(a0, a1) => { feed(12); a0.mix(); a1.mix(); }
                E3::V3(a0, a1, a2) => { feed(13); a0.mix(); a1.mix(); a2.mix(); }
                E3::V4(a0) => { feed(14); a0.mix(); }
            }
        }
    }
    pub fn f4(a5: &S1, a6: &S1) -> String {
        let mut v7: String = String::from_utf8_lossy(&(match (E3::V3(S1 { f0: '_' }, 0.2f64, String::from("Abc Def"))) { E3::V0 { f0: p9 } => vec![64u8, 127u8], E3::V1(_) => vec![197u8, 8u8], E3::V2(p10, _) => vec![255u8], E3::V3(p11, p12, p13) => vec![1u8, 20u8, 1u8, 10u8, 8u8], E3::V4(_) => vec![65u8, 255u8, 13u8, 5u8, 100u8] })).into_owned();
        None::<String>.filter(|x16: &String| -> bool { false }).ok_or(17i128.wrapping_abs()).unwrap_or_else(|x14: i128| -> String { format!("{:10.2}|", (match (vec![String::from("-7"), String::from("-7"), String::from("")].as_slice()) { [p18] if false => f64::NAN, _ => 65535.0f64 })) })
    }
    pub fn f19(a20: &mut [S1]) -> u128 {
        { let value = (unsafe { let mut slot = MaybeUninit::<S1>::uninit(); slot.write((if (true) { S1 { f0: '\u{80}' } } else { S1 { f0: '\u{80}' } })); slot.assume_init() }); if let Some(slot) = a20.first_mut() { *slot = value; } }
        let v21: (Box<f64>,) = (if (true) { (Box::new(4294967296.0f64),) } else { (Box::new(f64::MIN_POSITIVE),) });
        let f23 = move || -> f32 { let v22 = v21; let out = Some(f32::MAX).ok_or(String::from("  pad  ")).unwrap_or_else(|x24: String| -> f32 { ('b26: { if (false) { break 'b26 (-2.5e-3f32); } (-772.6686f32) }) }); drop(v22); out };
        apply_boxed(Box::new(f23)).mix();
        (match (E3::V0 { f0: String::from("-7") }) { E3::V0 { f0: p27 } => 144115188075855873u128, E3::V1(p28) => 0u128, E3::V2(p29, p30) => 126143039215705512621611321656368247232u128, E3::V3(S1 { .. }, _, p31) => 302231454903657293676544u128, E3::V4(p32) => 1073741823u128 })
    }
    #[inline(never)]
    pub fn f33(a34: &String, a35: &S1, a36: &mut f32, a37: &(Box<f64>,), a38: &[(f32, f64, i128, f32, i128, f32); 2], a39: &String, a40: &E3, a41: &str) -> [(f32, f64, i128, f32, i128, f32); 2] {
        {
            let f43 = |a42: f32| -> (Box<f64>,) { (if ((if (true) { true } else { false })) { (unsafe { let mut slot = MaybeUninit::<(Box<f64>,)>::uninit(); slot.write((Box::new(973.3260f64),)); slot.assume_init() }) } else { Ok::<(Box<f64>,), E3>((Box::new(0.1f64),)).unwrap_or((Box::new(f64::MAX),)) }) };
            let mut v45: String = format!("{} {:10.2}|", (*(*a37).0), 809.1881f64).trim().to_string();
            let v46: f32 = (if (true) { (*a36) } else { (*a36) }).floor();
            f43(1e-40f32.div_euclid(v46)).mix();
        }
        let mut v47: Vec<char> = (if let Some(x52) = (None::<Option<Vec<char>>>) { x52 } else { Some(vec!['\u{7f}', 'a', '\'']) }).filter(|x50: &Vec<char>| -> bool { Some(false).expect("nothing there") }).unwrap_or_else(|| -> Vec<char> { vec!['Z', 'ß'] });
        let v54: E3 = (*a40).clone();
        let mut v55: S1 = (*a35).clone();
        <[(f32, f64, i128, f32, i128, f32); 2]>::try_from(({ let mut items = vec![(1.0f32, 0.2f64, 165073588256409205364282012465350426823i128, 65535.0f32, (-12080306183663629875853097237436839488i128), (-0.0f32)), (f32::MAX, 0.2f64, (-19i128), f32::EPSILON, 147573952589676412929i128, f32::MIN), ((-701.7533f32), (-798.7314f64), 32i128, (-842.0971f32), (-105501285188534672169714078467150540709i128), 1e-40f32), (0.1f32, 484.1298f64, 20i128, f32::NAN, 140694703457599670937441056191773787461i128, f32::MIN), ((-9223372036854775808.0f32), f64::INFINITY, (-69371834661357219232999509852543390337i128), 3.25f32, (-3i128), 717.5200f32)]; let at = items.len() / 2; items.split_off(at) })).unwrap_or((match (vec![E2::V1('_'), E2::V2, E2::V0(String::from("42"), String::from("0123456789"), String::from("ff")), E2::V0(String::from("line\n"), String::from("ff"), String::from("0123456789")), E2::V1('\u{7f}')].as_slice()) { [] => (*a38), [] => [((-9223372036854775808.0f32), f64::MIN_POSITIVE, 151115727451828646838273i128, 16777217.0f32, 8i128, (-9223372036854775808.0f32)), (0.0f32, f64::INFINITY, 2i128, f32::MIN, (-1i128), (-1.0f32))], _ => (*a38) }))
    }
    #[inline(never)]
    pub fn f63(a64: &str, a65: &bool, a66: &mut [bool], a67: &E3, a68: &str, a69: &(Box<f64>,)) -> f64 {
        { let value = (!(E3::V2((-9223372036854775808.0f32), '\u{10FFFF}') == E3::V3(S1 { f0: '\\' }, 0.1f64, String::from("hello")))); if let Some(slot) = a66.first_mut() { *slot = value; } }
        let v70: Vec<bool> = Vec::<bool>::new();
        let mut v71: f64 = (loop { break id::<f64>((*(*a69).0)); });
        0.1f64
    }
    #[inline]
    pub fn f72() -> u128 {
        let mut v73: u128 = (match ((if (true) { E2::V1('_') } else { E2::V2 })) { E2::V0(p74, _, _) => 36u128.wrapping_sub(7u128), E2::V1(_) => ((-128.9f64) as u128), E2::V2 => (f64::NEG_INFINITY as u128) });
        {
            async fn work(seed: u32) -> u64 { let h75 = format!("{seed}-58"); let w76 = later(3, seed as u64 + 57).await; let h77 = [seed as u8; 1]; let w78 = later(2, [seed as u64; 2]).await[0]; let h79 = vec![Noisy(31), Noisy(seed % 5)]; let mut w80 = 0u64; for i in 0..2u64 { let _g = Noisy(i as u32); w80 += later((i % 2) as u32, i).await; } let h81 = format!("{seed}-12"); let w82 = later(3, seed as u64 + 28).await; (h75.len() as u64).rotate_left(0) ^ (w76).rotate_left(1) ^ (h77[0] as u64).rotate_left(2) ^ (w78).rotate_left(3) ^ (h79.len() as u64).rotate_left(4) ^ (w80).rotate_left(5) ^ (h81.len() as u64).rotate_left(6) ^ (w82).rotate_left(7) }
            block_on(work(90)).mix();
            block_on(work(70)).mix();
            block_on(work(83)).mix();
        }
        let E3::V4(_) = (pick::<E3>(false, &E3::V0 { f0: String::from("0123456789") }, &E3::V4((-228.1409f32))).clone()) else { return (v73 as u128); };
        (match (Vec::<bool>::new().as_slice()) { [.., p84] => v73, [.., p86] => 32u128, _ => 20u128 }).to_be()
    }
    pub fn run() {
        let mut v87: Vec<f32> = ({ let mut items = ({ let mut items = vec![1e10f32, f32::MIN, (-1.0f32), 255.5f32, f32::EPSILON]; items.push(f32::NAN); items }); items.insert(0, f32::from(16i8)); items });
        let mut v90: [f32; 3] = [(match (sel::<E2>(true, E2::V2, E2::V1('\u{7f}'))) { E2::V0(p94, _, p95) => (match (v87.clone().as_slice()) { [p97, p96 @ .., p98] => f32::NAN, _ => (-0.0f32) }), E2::V1(p99) => f32::EPSILON, E2::V2 => (match (String::from("42").as_str()) { p100 => (-806.2890f32), _ => 0.5f32 }) }), (5i8..8i8).zip(({ let mut items = v87.clone(); items.push((-9223372036854775808.0f32)); items })).map(|x103: (i8, f32)| -> f32 { x103.1 }).enumerate().fold(0.1f32.trunc(), |acc105: f32, x106: (usize, f32)| -> f32 { (if (false) { acc105 } else { x106.1 }) }), 0.2f32];
        { let value = (if ((if (false) { true } else { true })) { "0.1".parse::<f32>().unwrap_or((-1.0f32)) } else { 4294967296.0f32.round() }); let raw = Box::into_raw(Box::new(v90)); unsafe { (*raw)[1] = value; Box::from_raw(raw).mix(); } }
        v90.mix();
        let v107: S1 = id::<S1>((match (pick::<String>(false, &String::from("a,b,,c"), &String::from("42")).clone().as_str()) { p108 if ((-1i8) > (-128i8)) => (if (true) { S1 { f0: '\\' } } else { S1 { f0: '\u{10FFFF}' } }), p109 => (match (vec!['\u{e000}', 'a', '\u{d7ff}', 'z', 'a'].as_slice()) { [.., '0'] => S1 { f0: '😀' }, _ => S1 { f0: 'é' } }), "x y" => (match (v87.clone().as_slice()) { [] => S1 { f0: '\u{7f}' }, _ => S1 { f0: '\u{d7ff}' } }), _ => (match (vec![String::from("3.5"), String::from("hello"), String::from("hello"), String::from("a,b,,c"), String::from("a,b,,c")].as_slice()) { [.., p113] if false => S1 { f0: '\\' }, [.., p115] if true => S1 { f0: '0' }, _ => S1 { f0: '\n' } }) }));
        let g116: fn() -> u128 = f72;
        let mut v117: E2 = E2::V2;
        let v118: f32 = (if (true) { (unsafe { let mut slot = MaybeUninit::<f32>::uninit(); slot.write("x".parse::<f32>().unwrap_or(v90[0])); slot.assume_init() }) } else { v90[0] });
        apply_dyn(&|x119: f64| -> (Box<f64>,) { (Box::new(0.2f64),) }, 1.5f64).mix();
        v117 = (if ((match (Some(false)) { None => false, Some(x120) => x120 })) { v117.clone() } else { E2::V1('ß') });
        let mut v121: [(f32, f64, i128, f32, i128, f32); 2] = (match (String::from("-7").as_str()) { p124 => [(1e-40f32, 16777217.0f64, 100i128, 255.5f32, 664613997892457936451903530140172288i128, 0.5f32), (1.0f32, 1.5f64, 2i128, f32::NAN, 1024i128, (-139.0763f32))], p125 => [(f32::MIN_POSITIVE, (-790.3267f64), 8i128, (-749.4739f32), 170141183460469231731687303715884105726i128, 1e30f32), (785.5322f32, f64::MAX, (-3i128), 1e30f32, (-6i128), (-1.0f32))], "Abc Def" => [((-128.9f32), (-2.5e-3f64), 85070591730234615865843651857942052863i128, f32::MIN, 54660913991656536690361596999850750334i128, f32::NEG_INFINITY), (0.2f32, 1e-40f64, 10i128, f32::MIN, 2i128, 1e-40f32)], _ => [((-1.0f32), f64::EPSILON, 4503599627370497i128, 155.4878f32, 10i128, (-2.5e-3f32)); 2] });
        { let (left, right) = v121.split_at_mut(1); std::mem::swap(&mut left[0], &mut right[0]); }
        if let "ff" = (String::from_utf8_lossy(&vec![34u8, 8u8, 1u8, 127u8, 235u8]).into_owned().as_str()) {
            if let p126 = ((String::from("hello") + &String::from("42"))) {
                match &mut v117 {
                    E2::V2 => {
                        let v127: S1 = (match (v121[0].4) { _ => v107.clone(), _ => S1 { f0: '\u{d7ff}' } });
                    }
                    _ => {}
                }
                if let -7i8 = ((-60i8)) {
                    v87.dedup();
                }
            } else {
                v90 = [1.0f32, (-128.9f32), f32::MAX];
            }
        }
    }
}

mod c5 {
    use super::*;
    #[derive(Clone, Debug, PartialEq, PartialOrd, Copy, Eq, Ord, Hash)]
    pub struct S1;
    impl Mix for S1 {
        fn mix(&self) {
            feed(0);
        }
    }
    const K2: i64 = (-18i64);
    const K3: i128 = (-81925980767927858993723566623778574225i128);
    const K4: i128 = (match 7i128.checked_add((-111231749138148152469490973708616588507i128)) { Some(sum) => sum, None => (-111231749138148152469490973708616588507i128) });
    pub trait T5 {
        fn m6(&mut self, a0: u8) -> i64;
        fn m7(&self, a8: i128) -> u8 {
            ((8589934593i64 as u8).wrapping_add(Err::<u8, u128>(2u128).unwrap_or_else(|x9: u128| -> u8 { 38u8 })).overflowing_add((26u8.overflowing_mul(8u8).1 as u8).saturating_pow((!8u32) % 7)).1 as u8)
        }
    }
    impl T5 for i64 {
        fn m6(&mut self, a10: u8) -> i64 {
            (*self)
        }
    }
    impl T5 for u8 {
        fn m6(&mut self, a11: u8) -> i64 {
            let v12: u8 = std::mem::replace(&mut (*self), sel::<u8>(true, a11, a11));
            (*self) = (*self);
            (loop { break "😀".trim(); }).parse::<i64>().unwrap_or(sel::<i64>((match (Some(false)) { None => true, Some(x13) => x13 }), (-1i64), (1u16 as i64)))
        }
    }
    impl T5 for i128 {
        fn m6(&mut self, a14: u8) -> i64 {
            ((loop { break (K2.trailing_ones() as i64); }).trailing_zeros() as i64)
        }
    }
    impl T5 for isize {
        fn m6(&mut self, a15: u8) -> i64 {
            let mut v16: Vec<i128> = vec![(K3.checked_shr(10u32).unwrap_or((-36144706756089125394246947310184439446i128)).overflowing_sub(K3).1 as i128), (Err::<bool, S1>(S1).unwrap_or(true) as i128), K4];
            (2i64.wrapping_sub(K2).trailing_ones() as i64)
        }
    }
    #[inline(never)]
    pub fn f20<G19: Mix + Clone + Debug + PartialOrd>(a21: G19, a22: &mut Vec<G19>) -> i64 {
        (*a22).insert((String::from("a,b,,c").chars().count() % 4) % ((*a22).len() + 1), (unsafe { let mut slot = MaybeUninit::<G19>::uninit(); slot.write(a21.clone()); slot.assume_init() }));
        let mut v24: S1 = [S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1][16];
        match ((true as i64)) {
            _ => {
                v24 = Some(S1).map_or(v24, |x25: S1| -> S1 { v24 });
                (*a22).pop().mix();
            }
            _ => K2.mix(),
        }
        ((true as u32) as i64)
    }
    #[inline(never)]
    pub fn f27(a28: i128, a29: u8) -> i64 {
        ({ let mut m31: Vec<i128> = vec![K4]; let out = f20::<i128>(('b30: { if (true) { break 'b30 4095i128; } K4 }), &mut m31); m31.mix(); out })
    }
    #[inline(never)]
    pub fn f33(a34: u8, a35: u128, a36: u8) -> impl Fn(isize) -> [S1; 17] {
        move |x37: isize| -> [S1; 17] { [S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1] }
    }
    pub fn run() {
        let mut n38 = 0u32;
        'l39: while n38 < 8 && (Some(Err::<Box<u128>, S1>(S1)).is_none()) {
            n38 += 1;
            let mut v40: [S1; 17] = (unsafe { *(&f33(65u8, 3u128, 22u8)(7isize) as *const [S1; 17]) });
            let mut v45: BTreeMap<u8, u128> = BTreeMap::new();
            v45.insert(Err::<u8, Result<Box<u128>, S1>>(Err::<Box<u128>, S1>(S1)).map_err(|x47: Result<Box<u128>, S1>| -> i64 { 9145416658918156269i64 }).unwrap_or_else(|x46: i64| -> u8 { 37u8 }), (340282366920938463463374607431768211455u128.checked_shr(131072u32).unwrap_or(0u128) | 22u128.checked_neg().unwrap_or_default())).mix();
            v45.insert((Box::new((-11isize)) as Box<dyn T5>).m7(K3).saturating_pow(134217727u32 % 7), (170141183460469231731687303715884105727u128.overflowing_mul(Vec::<u128>::new().into_iter().filter(|x48: &u128| -> bool { true }).fold(10u128, |acc49: u128, x50: u128| -> u128 { acc49 })).1 as u128)).mix();
            v45.insert((Box::new(1i128.wrapping_pow(3501293603u32 % 7)) as Box<dyn T5>).m7(K3.to_be()), (unsafe { let mut slot = MaybeUninit::<u128>::uninit(); slot.write(15u128); slot.assume_init() }).wrapping_add(8u128.wrapping_mul(9671406556917033397649407u128))).mix();
            v45.insert(9u8.checked_shl(2u32).unwrap_or(149u8).m7(K4), 2596148429267413814265248164610049u128.wrapping_pow(2048u32 % 7)).mix();
            for (key, value) in &v45 { key.mix(); value.mix(); }
            v45.len().mix();
        }
        let mut n51 = 0u32;
        'l52: while n51 < 1 && (None::<isize>.is_none()) {
            n51 += 1;
            {
                if ((-f64::NEG_INFINITY).is_finite()) {
                    let S1: S1 = S1;
                } else {
                    {
                        for armed in [false, true] { caught(|| { let t = [Noisy(23), Noisy(29), Noisy(31)]; let [.., _x] = t; trip(armed, ()); }); }
                        for armed in [false, true] { caught(|| { let t = (Noisy(23), Box::new(Duo { a: Noisy(29), b: Noisy(31) })); let _x = t.1.a; let _y = t.0; trip(armed, ()); }); }
                    }
                }
                let mut v53: S1 = S1;
                let mut n54 = 0u32;
                'l55: while n54 < 3 && (f64::INFINITY.is_infinite()) {
                    n54 += 1;
                    if ((if (false) { 13744200898029403983usize } else { 274877906945usize }).overflowing_add((1usize.trailing_ones() as usize)).1) { break 'l52; }
                    v53 = sel::<S1>(matches!(16384isize, (19isize..)), v53, v53);
                }
                let S1: S1 = v53;
            }
            {
                let f58 = |a56: i64, a57: u128| -> S1 { S1 };
                let mut n59 = 0u32;
                'l60: while n59 < 8 && ((unsafe { let mut slot = MaybeUninit::<bool>::uninit(); slot.write(false); slot.assume_init() })) {
                    n59 += 1;
                    let mut v61: Vec<Result<u128, S1>> = (if let Some(x63) = (Some(vec![Err::<u128, S1>(S1), Ok::<u128, S1>(40564819207303340847894502572032u128)])) { x63 } else { vec![Err::<u128, S1>(S1)] });
                    let v65: Option<Vec<Result<u128, S1>>> = Some(v61);
                }
                let r66 = catch_unwind(AssertUnwindSafe(|| -> S1 {
                    {
                        let (a, b) = (black_box((1e39) as f32), black_box((0.0) as f32));
                        a.sqrt().mix(); a.mul_add(b, 1.5).mix(); a.rem_euclid(3.0).mix(); a.div_euclid(3.0).mix(); a.powi(3).mix(); a.recip().mix();
                        let (mut acc, mut k) = (a, 0u32); while acc.is_finite() && acc.abs() < 1e30 && k < 40 { acc = acc * 1.5 + 1.0; k += 1; } feed(k as u64);
                        feed(a.total_cmp(&b) as i8 as u64); feed(a.partial_cmp(&b).map_or(9, |it| it as i8 as u8) as u64); feed((a < b) as u64 + 2 * (a <= b) as u64 + 4 * (a == b) as u64 + 8 * (a != b) as u64);
                        (a + b).mix(); (a - b).mix(); (a * b).mix(); (a / b).mix(); (a % b).mix(); (-a).mix();
                        a.floor().mix(); a.ceil().mix(); a.round().mix(); a.trunc().mix(); a.fract().mix(); a.abs().mix(); a.round_ties_even().mix();
                    }
                    let v67: bool = (if let Some(x68) = (Some(true)) { x68 } else { (loop { break true; }) });
                    assert!(v67);
                    ('b71: { if ((!None::<u128>.is_some())) { break 'b71 f58(K2, (unsafe { std::mem::transmute::<i128, i128>(K3) } as u128)); } S1 })
                }));
                match r66 {
                    Ok(value) => value.mix(),
                    Err(caught) => payload(caught),
                }
                if ((!(!false))) {
                    {
                        trait Base { fn id(&self) -> u32; fn name(&self) -> &'static str { "base" } }
                        trait Mid: Base { fn twice(&self) -> u32 { self.id().wrapping_mul(2) } }
                        trait Top: Mid + Debug { fn all(&self) -> u64 { self.id() as u64 + self.twice() as u64 + self.name().len() as u64 } }
                        #[derive(Debug)] struct P(u32);
                        #[derive(Debug)] struct Q(u8, Noisy);
                        impl Base for P { fn id(&self) -> u32 { self.0 } }
                        impl Base for Q { fn id(&self) -> u32 { self.0 as u32 + self.1.0 } fn name(&self) -> &'static str { "q" } }
                        impl Mid for P {} impl Top for P {}
                        impl Mid for Q { fn twice(&self) -> u32 { 17 } } impl Top for Q {}
                        fn through(it: &dyn Mid) -> u32 { let base: &dyn Base = it; base.id() ^ it.twice() }
                        let mut tops: Vec<Box<dyn Top>> = vec![Box::new(P(10)), Box::new(Q(37, Noisy(17))), Box::new(P(37))];
                        { let last: Box<dyn Base> = tops.pop().unwrap(); feed(last.id() as u64); }
                        { let shared: Rc<dyn Top> = Rc::new(Q(1, Noisy(2))); let mid: Rc<dyn Mid> = shared.clone(); feed(mid.twice() as u64 + Rc::strong_count(&shared) as u64); }
                        { let first: &mut dyn Top = &mut *tops[0]; let base: &mut dyn Base = first; feed(base.id() as u64); }
                    }
                    ({ let mut it = (loop { break K3; }); let out = it.m6("a".parse::<u8>().unwrap_or(22u8)); it.mix(); out }).mix();
                    let mut v72: Result<Box<u128>, S1> = larger::<Result<Box<u128>, S1>>(('b75: { if (true) { break 'b75 Err::<Box<u128>, S1>(S1); } Ok::<Box<u128>, S1>(Box::new(232487637090643041310319870749627641988u128)) }), sel::<Result<Box<u128>, S1>>(false, Err::<Box<u128>, S1>(S1), Err::<Box<u128>, S1>(S1)));
                }
                f58(3i64, 24u128.max(0u128)).mix();
            }
        }
        let mut n77 = 0u32;
        'l78: while n77 < 1 && ((unsafe { let mut slot = MaybeUninit::<bool>::uninit(); slot.write(false); slot.assume_init() })) {
            n77 += 1;
            {
                async fn work(seed: u32) -> u64 { let mut h79 = seed as u64; let h79r = &mut h79; *h79r += 29; let w80 = later(1, [seed as u64; 9]).await[0]; let h81: Option<Box<u16>> = if seed % 2 == 0 { Some(Box::new(18)) } else { None }; let w82 = later(0, seed as u64 + 58).await; (*h79r).rotate_left(0) ^ (w80).rotate_left(1) ^ (h81.as_deref().copied().unwrap_or(1) as u64).rotate_left(2) ^ (w82).rotate_left(3) }
                block_on(work(73)).mix();
            }
            {
                let S1: S1 = Ok::<S1, S1>(S1).unwrap_or(S1);
                let mut o84: Vec<Box<dyn T5>> = vec![Box::new((Box::new(K4) as Box<dyn T5>).m7((K3.overflowing_add(K3).1 as i128))), Box::new(K3.min(K3).wrapping_mul(K4)), Box::new(i128::from_str_radix("line\n", 36).unwrap_or(170141183460469231731687303715884105726i128))];
                for o85 in o84.iter_mut() { o85.m6((if (false) { 5u8 } else { 0u8 })).mix(); }
            }
            if let [.., p89] = (None::<Vec<i32>>.unwrap_or_else(|| -> Vec<i32> { vec![16383i32, 16384i32, 1023i32] }).as_slice()) {
                let [.., S1]: [S1; 3] = Some([S1, S1, S1]).filter(|x93: &[S1; 3]| -> bool { false }).unwrap_or_else(|| -> [S1; 3] { <[S1; 3]>::try_from(vec![S1, S1, S1, S1, S1]).unwrap_or([S1; 3]) });
                let mut v96: [S1; 17] = [(loop { break S1; }); 17];
            }
        }
        let mut n99 = 0u32;
        'l100: while n99 < 5 && (4294967297u128.overflowing_mul(59200545954027203659752714949384120311u128).1) {
            n99 += 1;
            let mut n101 = 0u32;
            'l102: while n101 < 2 && (Err::<bool, Result<u128, S1>>(Err::<u128, S1>(S1)).unwrap_or_else(|x103: Result<u128, S1>| -> bool { true })) {
                n101 += 1;
                match (String::from("3.5").as_str()) {
                    p104 => {
                        let v105: Result<Box<u128>, S1> = Ok::<Box<u128>, S1>(Box::new(340282366920938463463374607431768211455u128));
                        let f107 = move || -> S1 { let v106 = v105; let out = (unsafe { let mut slot = MaybeUninit::<S1>::uninit(); slot.write([S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1][16]); slot.assume_init() }); drop(v106); out };
                        apply_boxed(Box::new(f107)).mix();
                        let [S1, p114 @ .., S1]: [S1; 4] = (match (Err::<[S1; 4], S1>(S1).ok()) { None => <[S1; 4]>::try_from(vec![S1, S1, S1, S1, S1]).unwrap_or([S1, S1, S1, S1]), Some(x110) => x110 });
                    }
                    _ => Err::<Box<u128>, S1>(S1).mix(),
                }
                let mut v115: String = ({ let mut text = format!("{:^7}| {:08} {:b}", 5397293118249662247usize, 1usize, 1usize); text.push_str((loop { break "-7"; })); text });
                let mut v116: Vec<u8> = ({ let mut items = vec![10u8]; items.insert(0, 111u8); items });
                v116.truncate(6);
                'l118: while let Some(x117) = v116.pop() {
                    let S1: S1 = (match ((unsafe { let mut slot = MaybeUninit::<Option<S1>>::uninit(); slot.write(None::<S1>); slot.assume_init() })) { None => S1, Some(x120) => x120 });
                    let mut v122: u8 = x117;
                }
                if (twice::<bool>(&27393i16.overflowing_sub((-1i16)).1).0) { continue 'l102; }
            }
        }
        if (false) {
            let mut n123 = 0u32;
            'l124: while n123 < 2 && (String::from("ff").starts_with("😀")) {
                n123 += 1;
                let S1: S1 = None::<S1>.unwrap_or_else(|| -> S1 { S1 });
                {
                    trait Base { fn id(&self) -> u32; fn name(&self) -> &'static str { "base" } }
                    trait Mid: Base { fn twice(&self) -> u32 { self.id().wrapping_mul(2) } }
                    trait Top: Mid + Debug { fn all(&self) -> u64 { self.id() as u64 + self.twice() as u64 + self.name().len() as u64 } }
                    #[derive(Debug)] struct P(u32);
                    #[derive(Debug)] struct Q(u8, Noisy);
                    impl Base for P { fn id(&self) -> u32 { self.0 } }
                    impl Base for Q { fn id(&self) -> u32 { self.0 as u32 + self.1.0 } fn name(&self) -> &'static str { "q" } }
                    impl Mid for P {} impl Top for P {}
                    impl Mid for Q { fn twice(&self) -> u32 { 6 } } impl Top for Q {}
                    fn through(it: &dyn Mid) -> u32 { let base: &dyn Base = it; base.id() ^ it.twice() }
                    let mut tops: Vec<Box<dyn Top>> = vec![Box::new(P(31)), Box::new(Q(8, Noisy(6))), Box::new(P(8))];
                    { let first: &mut dyn Top = &mut *tops[0]; let base: &mut dyn Base = first; feed(base.id() as u64); }
                    { let shared: Rc<dyn Top> = Rc::new(Q(1, Noisy(2))); let mid: Rc<dyn Mid> = shared.clone(); feed(mid.twice() as u64 + Rc::strong_count(&shared) as u64); }
                    for top in &tops { let mid: &dyn Mid = &**top; let base: &dyn Base = mid; feed(base.id() as u64); base.name().mix(); feed(through(mid) as u64); feed(top.all()); }
                    { let last: Box<dyn Base> = tops.pop().unwrap(); feed(last.id() as u64); }
                    { let mid: Box<dyn Mid> = tops.remove(1); let base: Box<dyn Base> = mid; base.name().mix(); }
                }
                {
                    for armed in [false, true] { caught(|| { let t = Box::new((Noisy(7), Noisy(12), Noisy(27))); let _x = t.2; trip(armed, ()); let _late = Noisy(51); }); }
                }
                let v126: NonZeroI64 = NonZeroI64::new(({ let mut it = K2.wrapping_pow(32u32 % 7); let out = it.m6(u8::from_str_radix("x y", 10).unwrap_or(32u8)); it.mix(); out })).unwrap_or((unsafe { *(&NonZeroI64::new(K2).unwrap_or(NonZeroI64::new(15i64).unwrap()) as *const NonZeroI64) }));
            }
            K3.mix();
            let v127: [(String, String); 5] = (unsafe { let mut slot = MaybeUninit::<[(String, String); 5]>::uninit(); slot.write([(String::from(""), String::from("42")), (String::from("Abc Def"), String::from("-7")), (String::from("hello"), String::from("line\n")), (String::from("  pad  "), String::from("-7")), (String::from("😀"), String::from("x y"))]); slot.assume_init() });
            let f129 = move || -> [(String, String); 5] { let v128 = v127; let out = v128.clone(); drop(v128); out };
            apply_once(f129).mix();
        }
        let r134 = catch_unwind(AssertUnwindSafe(|| -> i16 {
            let mut v135: [S1; 17] = <[S1; 17]>::try_from(({ let mut items = vec![S1, S1, S1, S1, S1]; items.insert(0, S1); items })).unwrap_or(Some([S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1, S1]).unwrap_or_else(|| -> [S1; 17] { [S1; 17] }));
            v135[6] = (if let Some(x142) = (Some(S1)) { x142 } else { false.then(|| -> S1 { v135[5] }).map_or(v135[16], |x143: S1| -> S1 { x143 }) });
            256i16
        }));
        match r134 {
            Ok(value) => value.mix(),
            Err(caught) => payload(caught),
        }
        {
            {
                if let _ = (&{ 24852u16 }) {
                    let [S1, S1, S1, S1]: [S1; 4] = <[S1; 4]>::try_from(vec![S1, S1, S1]).unwrap_or((loop { break [S1; 4]; }));
                    {
                        static NUMBERS: [u32; 4] = [19, 48, 43, 4];
                        static SECOND: &u32 = &NUMBERS[1];
                        static TAIL: &[u32] = match &NUMBERS { [_, _, rest @ ..] => rest };
                        static WORDS: [&str; 3] = ["one", "two", "three"];
                        static PICKED: &&str = &WORDS[2];
                        struct Link { value: u32, next: Option<&'static Link>, call: fn(u32) -> u32, name: &'static str, shown: &'static (dyn Debug + Sync) }
                        fn double(x: u32) -> u32 { x.wrapping_mul(2) }
                        static END: Link = Link { value: 3, next: None, call: double, name: "end", shown: &NUMBERS };
                        static MID: Link = Link { value: 19, next: Some(&END), call: |x| x + 1, name: WORDS[0], shown: &"mid" };
                        static HEAD: Link = Link { value: 48, next: Some(&MID), call: double, name: "head", shown: &MID.value };
                        static TABLE: [fn(u32) -> u32; 3] = [double, |x| x ^ 5, HEAD.call];
                        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                        static LISTS: (&[u8], &[&[u8]], [Option<&u32>; 2]) = (b"bytes", &[b"a", b"", &[19, 2]], [Some(&NUMBERS[3]), None]);
                        const VIEW: &[u32; 4] = &NUMBERS;
                        thread_local! { static LOCAL: Cell<u32> = const { Cell::new(5) }; static NOTES: RefCell<Vec<&'static u32>> = const { RefCell::new(Vec::new()) }; }
                        { let mut at = Some(&HEAD); while let Some(link) = at { feed((link.call)(link.value) as u64); link.name.mix(); format!("{:?}", link.shown).mix(); at = link.next; } }
                        feed(*SECOND as u64 + TAIL.len() as u64 + std::ptr::eq(SECOND, &NUMBERS[1]) as u64 * 16 + std::ptr::eq(TAIL.as_ptr(), &NUMBERS[2]) as u64 * 32);
                        LOCAL.with(|it| { it.set(it.get() + 1); feed(it.get() as u64 % 2 + 40); }); NOTES.with(|it| { it.borrow_mut().push(SECOND); feed(*it.borrow()[0] as u64); });
                    }
                } else {
                    let p150: S1 = S1;
                }
                if (true) {
                    let [S1, S1, S1]: [S1; 3] = ({ let mut items = [S1; 3]; items.sort_by(|a, b| b.cmp(a)); items });
                    let [S1, S1, S1]: [S1; 3] = [S1, S1, S1];
                } else {
                    {
                        let mut items: Vec<Box<dyn Any>> = vec![Box::new([50u16; 3]), Box::new(Noisy(50)), Box::new(String::from("owned")), Box::new(25u32), Box::new(Some(25i64)), Box::new("text")];
                        for item in &items { if let Some(it) = item.downcast_ref::<u32>() { feed(*it as u64); } else if let Some(it) = item.downcast_ref::<&str>() { it.mix(); } else if item.is::<Noisy>() { feed(1); } else if let Some(it) = item.downcast_ref::<(u8, bool)>() { feed(it.0 as u64 + it.1 as u64); } else { feed(2); } }
                        while let Some(item) = items.pop() { match item.downcast::<Noisy>() { Ok(it) => it.mix(), Err(other) => feed(other.is::<String>() as u64) } }
                    }
                    {
                        { let mut count = 0u32; let mut step = |by: u32| { count += by; count }; for i in 0..5 { step(i).mix(); } count.mix(); }
                        { let found = 'search: loop { for i in 0..5u32 { let hit = move |at: u32| at * i == 4; for j in 0..3 { if hit(j) { break 'search Some((i, j)); } } } break None; }; found.mix(); }
                        { for i in 0..5u32 { let n = Noisy(i); let once = move || n.0 + 4; if i != 1 { once().mix(); } } }
                        { let mut made: Vec<Box<dyn Fn(u32) -> u32>> = Vec::new(); for i in 0..5u32 { let k = i.wrapping_mul(4); made.push(Box::new(move |x| x.wrapping_add(k))); } for f in &made { f(1).mix(); } }
                    }
                }
            }
            if ((true & 0isize.overflowing_sub((-11isize)).1)) {
                let v153: Result<Result<u8, isize>, S1> = Ok::<Result<u8, isize>, S1>(Ok::<u8, isize>(95u8));
                let mut n155 = 0u32;
                'l156: while n155 < 8 && (Err::<bool, Result<u128, S1>>(Ok::<u128, S1>(39u128)).unwrap_or(false)) {
                    n155 += 1;
                    let v158: Result<Result<u8, isize>, S1> = ('b161: { if (true) { break 'b161 Ok::<Result<u8, isize>, u32>(Ok::<u8, isize>(5u8)); } Ok::<Result<u8, isize>, u32>(Ok::<u8, isize>(1u8)) }).or_else(|x159: u32| -> Result<Result<u8, isize>, S1> { Some(Err::<Result<u8, isize>, S1>(S1)).unwrap_or(v153) });
                    {
                        for armed in [false, true] { caught(|| { let _v = (Noisy(60), Duo { a: Noisy(51), b: trip(armed, Noisy(56)) }); }); }
                        for armed in [false, true] { caught(|| { let _v = vec![Noisy(24), trip(armed, Noisy(31)), Noisy(47)]; }); }
                        for armed in [false, true] { caught(|| { let _v = Box::new((Noisy(25), trip(armed, Noisy(42)))); }); }
                    }
                }
                let r164 = catch_unwind(AssertUnwindSafe(|| -> [(String, String); 5] {
                    let v165: [(String, String); 5] = [(String::from("ff"), String::from("😀")), (String::from("x y"), String::from("3.5")), (String::from("line\n"), String::from("line\n")), (String::from("42"), String::from("  pad  ")), (String::from("line\n"), String::from("Abc Def"))];
                    let f167 = move || -> Box<S1> { let v166 = v165; let out = Box::new(S1); drop(v166); out };
                    f167().mix();
                    <[(String, String); 5]>::try_from(vec![(match (None::<(String, String)>) { None => (String::from("line\n"), String::from("")), Some(x173) => x173 }), sel::<(String, String)>(true, (String::from("ff"), String::from("-7")), (String::from("a"), String::from("😀"))), ('b174: { if (true) { break 'b174 (String::from(""), String::from("hello")); } (String::from("héllo"), String::from("  pad  ")) })]).unwrap_or([Some((String::from("😀"), String::from("héllo"))).unwrap_or_else(|| -> (String, String) { (String::from("3.5"), String::from("a,b,,c")) }), [(String::from("ff"), String::from("Abc Def")), (String::from("line\n"), String::from("")), (String::from("3.5"), String::from("hello")), (String::from("x y"), String::from("ff")), (String::from("hello"), String::from("0123456789"))][2].clone(), sel::<(String, String)>(false, (String::from("ff"), String::from("  pad  ")), (String::from("x y"), String::from("a,b,,c"))), sel::<(String, String)>(true, (String::from("héllo"), String::from("-7")), (String::from("a,b,,c"), String::from("3.5"))), (loop { break (String::from("42"), String::from("😀")); })])
                }));
                match r164 {
                    Ok(value) => value.mix(),
                    Err(caught) => payload(caught),
                }
            } else {
                {
                    { let mut pair = (Noisy(54), vec![55u8]); let mut grow = || pair.1.push(1); grow(); let first = &pair.0; first.mix(); pair.1.mix(); }
                }
            }
        }
        let mut v178: BTreeMap<u16, usize> = BTreeMap::new();
        v178.insert((K4 as u16).wrapping_mul((9223372036854775808usize as u16)), ((match (Some(12512561170728463032usize)) { None => 18446744073709551614usize, Some(x179) => x179 }) << (37u32 % 64))).mix();
        v178.insert(13610u16.max(512u16).checked_shr((unsafe { Bytes4 { word: 4294967295u32 }.halves[1] } as u32)).unwrap_or(35108u16.wrapping_add(10u16)), 3439602788356294034usize).mix();
        v178.insert(65535u16, (unsafe { Bytes16 { signed: K4.wrapping_neg() }.signed } as usize)).mix();
        v178.insert(31315u16.saturating_mul(256u16).checked_shl(u32::try_from(7i8).unwrap_or(958958128u32)).unwrap_or("42".parse::<u16>().unwrap_or(16272u16)), 25usize).mix();
        for (key, value) in &v178 { key.mix(); value.mix(); }
        v178.len().mix();
        let mut v180: isize = (unsafe { let mut slot = MaybeUninit::<isize>::uninit(); slot.write((4503599627370495u128 as isize)); slot.assume_init() });
        if (true) {
            let r181 = catch_unwind(AssertUnwindSafe(|| -> [S1; 4] {
                let mut n182 = 0u32;
                'l183: while n182 < 8 && (17u128.overflowing_sub(138352275829519035763868480973904817587u128).1) {
                    n182 += 1;
                    {
                        async fn work(seed: u32) -> u64 { let h184 = Noisy(seed % 59 + 1); let w185 = if seed % 2 == 0 { later(1, 5u64).await } else { 32 }; (h184.0 as u64).rotate_left(0) ^ (w185).rotate_left(1) }
                        block_on(work(95)).mix();
                    }
                    let S1: S1 = (loop { break ('b186: { if (false) { break 'b186 S1; } S1 }); });
                }
                let r187 = catch_unwind(AssertUnwindSafe(|| -> isize {
                    let mut v188: Vec<[S1; 3]> = Vec::<[S1; 3]>::new();
                    (!(!v180))
                }));
                match r187 {
                    Ok(value) => value.mix(),
                    Err(caught) => payload(caught),
                }
                (unsafe { *(&[S1; 4] as *const [S1; 4]) })
            }));
            match r181 {
                Ok(value) => value.mix(),
                Err(caught) => payload(caught),
            }
            {
                async fn work(seed: u32) -> u64 { let h191 = Noisy(seed % 28 + 1); let mut w192 = 0u64; for i in 0..2u64 { let _g = Noisy(i as u32); w192 += later((i % 2) as u32, i).await; } (h191.0 as u64).rotate_left(0) ^ (w192).rotate_left(1) }
                block_on(work(61)).mix();
            }
        } else {
            {
                #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(C)] struct Plain { a: u8, b: u32, c: u16, d: u64 }
                #[derive(Clone, Copy, Default)] #[repr(C, packed)] struct Tight { a: u8, b: u64, c: u16, d: u128 }
                #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(C, align(2))] struct Big { a: u8, b: [u16; 3] }
                #[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(align(2))] struct Rusty(u8, u32);
                #[derive(Clone, Copy, Debug, PartialEq)] #[repr(transparent)] struct Wrap(f32);
                #[derive(Clone, Copy, Debug, PartialEq)] #[repr(i8)] enum Signed { Low = -128, Mid = -1, High = 127 }
                #[derive(Clone, Copy, Debug, PartialEq)] #[repr(u16)] enum Wider { A = 1, B = 0x1234, C = 0xffff }
                #[derive(Clone, Copy, Debug, PartialEq)] #[repr(C)] enum Cee { Unit, One(u8), Two { x: u16, y: u64 } }
                #[derive(Clone, Copy, Debug, PartialEq)] #[repr(C, u8)] enum Both { P(u32), Q(u8, u8), R }
                #[derive(Clone, Copy)] #[repr(C)] union Bits { word: u32, bytes: [u8; 4], half: (u16, u16) }
                #[inline(never)] extern "C" fn c_plain(p: Plain, t: Tight, w: Wrap) -> Plain { Plain { a: p.a.wrapping_add(t.a), b: p.b ^ { t.c } as u32, c: p.c, d: p.d } }
                #[inline(never)] extern "C" fn c_big(x: u8, big: Big, y: u8, rusty: Rusty) -> Big { Big { a: big.a ^ x ^ y ^ rusty.0, b: [big.b[2], big.b[0], rusty.1 as u16] } }
                #[inline(never)] fn r_enums(s: Signed, w: Wider, c: Cee, b: Both) -> (i8, u16, Cee, Both) { (s as i8, w as u16, match c { Cee::One(v) => Cee::Two { x: v as u16, y: 1 }, other => other }, match b { Both::Q(x, y) => Both::Q(y, x), other => other }) }
                { for s in black_box([Signed::Low, Signed::Mid, Signed::High]) { feed((s as i8 as i64 + 200) as u64); feed((s as i32 == -1) as u64); } for w in black_box([Wider::A, Wider::B, Wider::C]) { feed(w as u64); } }
                { let big = c_big(27, black_box(Big { a: 1, b: [2, 3, 4] }), 6, black_box(Rusty(5, 6))); big.a.mix(); big.b.mix(); feed((&big as *const Big as usize % 2) as u64); }
                feed((size_of::<Plain>() + align_of::<Plain>() * 256 + size_of::<Tight>() * 4096 + align_of::<Tight>() * 65536) as u64); feed((size_of::<Big>() + align_of::<Big>() * 256 + size_of::<Rusty>() * 4096 + size_of::<Cee>() * 65536 + size_of::<Both>() * 1048576) as u64);
                feed((std::mem::offset_of!(Plain, b) + std::mem::offset_of!(Plain, c) * 16 + std::mem::offset_of!(Plain, d) * 256 + std::mem::offset_of!(Tight, d) * 4096 + std::mem::offset_of!(Big, b) * 65536) as u64);
                { let mut t = black_box(Tight { a: 27, b: 6, c: 7, d: u128::MAX }); t.b += 1; t.d = t.d.wrapping_add(27); let (b, c, d) = (t.b, t.c, t.d); feed(b + c as u64); d.mix(); feed(unsafe { std::ptr::addr_of!(t.d).read_unaligned() } as u64); }
                { let w = black_box(Wrap(2.5)); let raw: f32 = unsafe { std::mem::transmute(w) }; raw.mix(); let items = black_box([Rusty(1, 2), Rusty(3, 4)]); feed(items[1].1 as u64 + size_of_val(&items) as u64 * 16); }
            }
        }
        v180 |= ((-1i8).wrapping_add((-96i8)) as isize);
        let mut v193: Vec<NonZeroI64> = sel::<Vec<NonZeroI64>>(false, ({ let mut items = vec![NonZeroI64::new((-9223372036854775808i64)).unwrap(), NonZeroI64::new((-3i64)).unwrap(), NonZeroI64::new(5836632602982584659i64).unwrap(), NonZeroI64::new(6560114569519624363i64).unwrap(), NonZeroI64::new(7456088801619170024i64).unwrap()]; items.reverse(); items }), vec![NonZeroI64::new((-1i64)).unwrap(), NonZeroI64::new((-5i64)).unwrap(), NonZeroI64::new(16384i64).unwrap(), NonZeroI64::new((-1i64)).unwrap()]);
        let v197: [(String, String); 5] = [(151115727451828646838272u128.overflowing_add(100u128).1 as u128), 22u128.saturating_pow(4294967294u32 % 7), 15u128, 25u128.wrapping_neg(), 604462909807314587353088u128].map(|x201: u128| -> (String, String) { twice::<(String, String)>(&(loop { break (String::from("😀"), String::from("3.5")); })).0 });
        let f199 = move || -> i32 { let v198 = v197; let out = 0i32.wrapping_sub(262144i32).saturating_neg(); drop(v198); out };
        f199().mix();
        v180 |= ("x y".parse::<isize>().unwrap_or((-19isize)) >> (13u32 % 64));
        {
            async fn leaf(x: u64) -> u64 { let _g = Noisy(x as u32 % 9); later((x % 3) as u32, x + 19).await }
            async fn work(seed: u32) -> u64 { let h204 = (seed as f64 * 0.5, seed as u8, seed % 3 == 0); let w205 = match later(1, seed % 3).await { 0 => later(0, 10u64).await, 1 => 11, _ => later(2, 12u64).await }; let mut h206 = seed as u64; let h206r = &mut h206; *h206r += 13; let w207 = match later(1, seed % 3).await { 0 => later(0, 10u64).await, 1 => 11, _ => later(2, 12u64).await }; (h204.0 as u64 + h204.1 as u64 + h204.2 as u64).rotate_left(0) ^ (w205).rotate_left(1) ^ (*h206r).rotate_left(2) ^ (w207).rotate_left(3) }
            async fn down(n: u32) -> u64 { if n == 0 { leaf(1).await } else { Box::pin(down(n - 1)).await + leaf(n as u64).await } }
            async fn outer(seed: u32) -> u64 { let n = Noisy(seed); let a = leaf(seed as u64).await; let (b, c) = both(leaf(a), work(seed)).await; let d = async { let _in = Noisy(25); leaf(b ^ c).await }.await; a ^ b ^ c ^ d ^ n.0 as u64 }
            { let add = async |x: u64| later(1, x).await + 25; block_on(add(3)).mix(); }
            { let n = Noisy(25); let by_ref = async { later(1, ()).await; n.0 }; block_on(by_ref).mix(); n.mix(); }
            poll_some(outer(10), 4).mix();
            block_on(outer(45)).mix();
        }
    }
}
