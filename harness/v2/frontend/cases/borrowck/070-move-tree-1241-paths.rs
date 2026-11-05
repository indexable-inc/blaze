//@ expect: accept
struct Inner {
    g0: String,
    g1: String,
    g2: String,
    g3: String,
    g4: String,
    g5: String,
    g6: String,
    g7: String,
    g8: String,
    g9: String,
    g10: String,
    g11: String,
    g12: String,
    g13: String,
    g14: String,
    g15: String,
    g16: String,
    g17: String,
    g18: String,
    g19: String,
    g20: String,
    g21: String,
    g22: String,
    g23: String,
    g24: String,
    g25: String,
    g26: String,
    g27: String,
    g28: String,
    g29: String,
}

struct Outer {
    f0: Inner,
    f1: Inner,
    f2: Inner,
    f3: Inner,
    f4: Inner,
    f5: Inner,
    f6: Inner,
    f7: Inner,
    f8: Inner,
    f9: Inner,
    f10: Inner,
    f11: Inner,
    f12: Inner,
    f13: Inner,
    f14: Inner,
    f15: Inner,
    f16: Inner,
    f17: Inner,
    f18: Inner,
    f19: Inner,
    f20: Inner,
    f21: Inner,
    f22: Inner,
    f23: Inner,
    f24: Inner,
    f25: Inner,
    f26: Inner,
    f27: Inner,
    f28: Inner,
    f29: Inner,
    f30: Inner,
    f31: Inner,
    f32: Inner,
    f33: Inner,
    f34: Inner,
    f35: Inner,
    f36: Inner,
    f37: Inner,
    f38: Inner,
    f39: Inner,
}

fn inner(k: usize) -> Inner {
    Inner {
        g0: format!("{k}-0"),
        g1: format!("{k}-1"),
        g2: format!("{k}-2"),
        g3: format!("{k}-3"),
        g4: format!("{k}-4"),
        g5: format!("{k}-5"),
        g6: format!("{k}-6"),
        g7: format!("{k}-7"),
        g8: format!("{k}-8"),
        g9: format!("{k}-9"),
        g10: format!("{k}-10"),
        g11: format!("{k}-11"),
        g12: format!("{k}-12"),
        g13: format!("{k}-13"),
        g14: format!("{k}-14"),
        g15: format!("{k}-15"),
        g16: format!("{k}-16"),
        g17: format!("{k}-17"),
        g18: format!("{k}-18"),
        g19: format!("{k}-19"),
        g20: format!("{k}-20"),
        g21: format!("{k}-21"),
        g22: format!("{k}-22"),
        g23: format!("{k}-23"),
        g24: format!("{k}-24"),
        g25: format!("{k}-25"),
        g26: format!("{k}-26"),
        g27: format!("{k}-27"),
        g28: format!("{k}-28"),
        g29: format!("{k}-29"),
    }
}

fn take(s: String) -> usize {
    s.len()
}

fn main() {
    let o = Outer {
        f0: inner(0),
        f1: inner(1),
        f2: inner(2),
        f3: inner(3),
        f4: inner(4),
        f5: inner(5),
        f6: inner(6),
        f7: inner(7),
        f8: inner(8),
        f9: inner(9),
        f10: inner(10),
        f11: inner(11),
        f12: inner(12),
        f13: inner(13),
        f14: inner(14),
        f15: inner(15),
        f16: inner(16),
        f17: inner(17),
        f18: inner(18),
        f19: inner(19),
        f20: inner(20),
        f21: inner(21),
        f22: inner(22),
        f23: inner(23),
        f24: inner(24),
        f25: inner(25),
        f26: inner(26),
        f27: inner(27),
        f28: inner(28),
        f29: inner(29),
        f30: inner(30),
        f31: inner(31),
        f32: inner(32),
        f33: inner(33),
        f34: inner(34),
        f35: inner(35),
        f36: inner(36),
        f37: inner(37),
        f38: inner(38),
        f39: inner(39),
    };
    let mut total = 0;
    total += take(o.f0.g0);
    total += take(o.f0.g1);
    total += take(o.f0.g2);
    total += take(o.f0.g3);
    total += take(o.f0.g4);
    total += take(o.f0.g5);
    total += take(o.f0.g6);
    total += take(o.f0.g7);
    total += take(o.f0.g8);
    total += take(o.f0.g9);
    total += take(o.f0.g10);
    total += take(o.f0.g11);
    total += take(o.f0.g12);
    total += take(o.f0.g13);
    total += take(o.f0.g14);
    total += take(o.f0.g15);
    total += take(o.f0.g16);
    total += take(o.f0.g17);
    total += take(o.f0.g18);
    total += take(o.f0.g19);
    total += take(o.f0.g20);
    total += take(o.f0.g21);
    total += take(o.f0.g22);
    total += take(o.f0.g23);
    total += take(o.f0.g24);
    total += take(o.f0.g25);
    total += take(o.f0.g26);
    total += take(o.f0.g27);
    total += take(o.f0.g28);
    total += take(o.f0.g29);
    total += take(o.f1.g0);
    total += take(o.f1.g1);
    total += take(o.f1.g2);
    total += take(o.f1.g3);
    total += take(o.f1.g4);
    total += take(o.f1.g5);
    total += take(o.f1.g6);
    total += take(o.f1.g7);
    total += take(o.f1.g8);
    total += take(o.f1.g9);
    total += take(o.f1.g10);
    total += take(o.f1.g11);
    total += take(o.f1.g12);
    total += take(o.f1.g13);
    total += take(o.f1.g14);
    total += take(o.f1.g15);
    total += take(o.f1.g16);
    total += take(o.f1.g17);
    total += take(o.f1.g18);
    total += take(o.f1.g19);
    total += take(o.f1.g20);
    total += take(o.f1.g21);
    total += take(o.f1.g22);
    total += take(o.f1.g23);
    total += take(o.f1.g24);
    total += take(o.f1.g25);
    total += take(o.f1.g26);
    total += take(o.f1.g27);
    total += take(o.f1.g28);
    total += take(o.f1.g29);
    total += take(o.f2.g0);
    total += take(o.f2.g1);
    total += take(o.f2.g2);
    total += take(o.f2.g3);
    total += take(o.f2.g4);
    total += take(o.f2.g5);
    total += take(o.f2.g6);
    total += take(o.f2.g7);
    total += take(o.f2.g8);
    total += take(o.f2.g9);
    total += take(o.f2.g10);
    total += take(o.f2.g11);
    total += take(o.f2.g12);
    total += take(o.f2.g13);
    total += take(o.f2.g14);
    total += take(o.f2.g15);
    total += take(o.f2.g16);
    total += take(o.f2.g17);
    total += take(o.f2.g18);
    total += take(o.f2.g19);
    total += take(o.f2.g20);
    total += take(o.f2.g21);
    total += take(o.f2.g22);
    total += take(o.f2.g23);
    total += take(o.f2.g24);
    total += take(o.f2.g25);
    total += take(o.f2.g26);
    total += take(o.f2.g27);
    total += take(o.f2.g28);
    total += take(o.f2.g29);
    total += take(o.f3.g0);
    total += take(o.f3.g1);
    total += take(o.f3.g2);
    total += take(o.f3.g3);
    total += take(o.f3.g4);
    total += take(o.f3.g5);
    total += take(o.f3.g6);
    total += take(o.f3.g7);
    total += take(o.f3.g8);
    total += take(o.f3.g9);
    total += take(o.f3.g10);
    total += take(o.f3.g11);
    total += take(o.f3.g12);
    total += take(o.f3.g13);
    total += take(o.f3.g14);
    total += take(o.f3.g15);
    total += take(o.f3.g16);
    total += take(o.f3.g17);
    total += take(o.f3.g18);
    total += take(o.f3.g19);
    total += take(o.f3.g20);
    total += take(o.f3.g21);
    total += take(o.f3.g22);
    total += take(o.f3.g23);
    total += take(o.f3.g24);
    total += take(o.f3.g25);
    total += take(o.f3.g26);
    total += take(o.f3.g27);
    total += take(o.f3.g28);
    total += take(o.f3.g29);
    total += take(o.f4.g0);
    total += take(o.f4.g1);
    total += take(o.f4.g2);
    total += take(o.f4.g3);
    total += take(o.f4.g4);
    total += take(o.f4.g5);
    total += take(o.f4.g6);
    total += take(o.f4.g7);
    total += take(o.f4.g8);
    total += take(o.f4.g9);
    total += take(o.f4.g10);
    total += take(o.f4.g11);
    total += take(o.f4.g12);
    total += take(o.f4.g13);
    total += take(o.f4.g14);
    total += take(o.f4.g15);
    total += take(o.f4.g16);
    total += take(o.f4.g17);
    total += take(o.f4.g18);
    total += take(o.f4.g19);
    total += take(o.f4.g20);
    total += take(o.f4.g21);
    total += take(o.f4.g22);
    total += take(o.f4.g23);
    total += take(o.f4.g24);
    total += take(o.f4.g25);
    total += take(o.f4.g26);
    total += take(o.f4.g27);
    total += take(o.f4.g28);
    total += take(o.f4.g29);
    total += take(o.f5.g0);
    total += take(o.f5.g1);
    total += take(o.f5.g2);
    total += take(o.f5.g3);
    total += take(o.f5.g4);
    total += take(o.f5.g5);
    total += take(o.f5.g6);
    total += take(o.f5.g7);
    total += take(o.f5.g8);
    total += take(o.f5.g9);
    total += take(o.f5.g10);
    total += take(o.f5.g11);
    total += take(o.f5.g12);
    total += take(o.f5.g13);
    total += take(o.f5.g14);
    total += take(o.f5.g15);
    total += take(o.f5.g16);
    total += take(o.f5.g17);
    total += take(o.f5.g18);
    total += take(o.f5.g19);
    total += take(o.f5.g20);
    total += take(o.f5.g21);
    total += take(o.f5.g22);
    total += take(o.f5.g23);
    total += take(o.f5.g24);
    total += take(o.f5.g25);
    total += take(o.f5.g26);
    total += take(o.f5.g27);
    total += take(o.f5.g28);
    total += take(o.f5.g29);
    total += take(o.f6.g0);
    total += take(o.f6.g1);
    total += take(o.f6.g2);
    total += take(o.f6.g3);
    total += take(o.f6.g4);
    total += take(o.f6.g5);
    total += take(o.f6.g6);
    total += take(o.f6.g7);
    total += take(o.f6.g8);
    total += take(o.f6.g9);
    total += take(o.f6.g10);
    total += take(o.f6.g11);
    total += take(o.f6.g12);
    total += take(o.f6.g13);
    total += take(o.f6.g14);
    total += take(o.f6.g15);
    total += take(o.f6.g16);
    total += take(o.f6.g17);
    total += take(o.f6.g18);
    total += take(o.f6.g19);
    total += take(o.f6.g20);
    total += take(o.f6.g21);
    total += take(o.f6.g22);
    total += take(o.f6.g23);
    total += take(o.f6.g24);
    total += take(o.f6.g25);
    total += take(o.f6.g26);
    total += take(o.f6.g27);
    total += take(o.f6.g28);
    total += take(o.f6.g29);
    total += take(o.f7.g0);
    total += take(o.f7.g1);
    total += take(o.f7.g2);
    total += take(o.f7.g3);
    total += take(o.f7.g4);
    total += take(o.f7.g5);
    total += take(o.f7.g6);
    total += take(o.f7.g7);
    total += take(o.f7.g8);
    total += take(o.f7.g9);
    total += take(o.f7.g10);
    total += take(o.f7.g11);
    total += take(o.f7.g12);
    total += take(o.f7.g13);
    total += take(o.f7.g14);
    total += take(o.f7.g15);
    total += take(o.f7.g16);
    total += take(o.f7.g17);
    total += take(o.f7.g18);
    total += take(o.f7.g19);
    total += take(o.f7.g20);
    total += take(o.f7.g21);
    total += take(o.f7.g22);
    total += take(o.f7.g23);
    total += take(o.f7.g24);
    total += take(o.f7.g25);
    total += take(o.f7.g26);
    total += take(o.f7.g27);
    total += take(o.f7.g28);
    total += take(o.f7.g29);
    total += take(o.f8.g0);
    total += take(o.f8.g1);
    total += take(o.f8.g2);
    total += take(o.f8.g3);
    total += take(o.f8.g4);
    total += take(o.f8.g5);
    total += take(o.f8.g6);
    total += take(o.f8.g7);
    total += take(o.f8.g8);
    total += take(o.f8.g9);
    total += take(o.f8.g10);
    total += take(o.f8.g11);
    total += take(o.f8.g12);
    total += take(o.f8.g13);
    total += take(o.f8.g14);
    total += take(o.f8.g15);
    total += take(o.f8.g16);
    total += take(o.f8.g17);
    total += take(o.f8.g18);
    total += take(o.f8.g19);
    total += take(o.f8.g20);
    total += take(o.f8.g21);
    total += take(o.f8.g22);
    total += take(o.f8.g23);
    total += take(o.f8.g24);
    total += take(o.f8.g25);
    total += take(o.f8.g26);
    total += take(o.f8.g27);
    total += take(o.f8.g28);
    total += take(o.f8.g29);
    total += take(o.f9.g0);
    total += take(o.f9.g1);
    total += take(o.f9.g2);
    total += take(o.f9.g3);
    total += take(o.f9.g4);
    total += take(o.f9.g5);
    total += take(o.f9.g6);
    total += take(o.f9.g7);
    total += take(o.f9.g8);
    total += take(o.f9.g9);
    total += take(o.f9.g10);
    total += take(o.f9.g11);
    total += take(o.f9.g12);
    total += take(o.f9.g13);
    total += take(o.f9.g14);
    total += take(o.f9.g15);
    total += take(o.f9.g16);
    total += take(o.f9.g17);
    total += take(o.f9.g18);
    total += take(o.f9.g19);
    total += take(o.f9.g20);
    total += take(o.f9.g21);
    total += take(o.f9.g22);
    total += take(o.f9.g23);
    total += take(o.f9.g24);
    total += take(o.f9.g25);
    total += take(o.f9.g26);
    total += take(o.f9.g27);
    total += take(o.f9.g28);
    total += take(o.f9.g29);
    total += take(o.f10.g0);
    total += take(o.f10.g1);
    total += take(o.f10.g2);
    total += take(o.f10.g3);
    total += take(o.f10.g4);
    total += take(o.f10.g5);
    total += take(o.f10.g6);
    total += take(o.f10.g7);
    total += take(o.f10.g8);
    total += take(o.f10.g9);
    total += take(o.f10.g10);
    total += take(o.f10.g11);
    total += take(o.f10.g12);
    total += take(o.f10.g13);
    total += take(o.f10.g14);
    total += take(o.f10.g15);
    total += take(o.f10.g16);
    total += take(o.f10.g17);
    total += take(o.f10.g18);
    total += take(o.f10.g19);
    total += take(o.f10.g20);
    total += take(o.f10.g21);
    total += take(o.f10.g22);
    total += take(o.f10.g23);
    total += take(o.f10.g24);
    total += take(o.f10.g25);
    total += take(o.f10.g26);
    total += take(o.f10.g27);
    total += take(o.f10.g28);
    total += take(o.f10.g29);
    total += take(o.f11.g0);
    total += take(o.f11.g1);
    total += take(o.f11.g2);
    total += take(o.f11.g3);
    total += take(o.f11.g4);
    total += take(o.f11.g5);
    total += take(o.f11.g6);
    total += take(o.f11.g7);
    total += take(o.f11.g8);
    total += take(o.f11.g9);
    total += take(o.f11.g10);
    total += take(o.f11.g11);
    total += take(o.f11.g12);
    total += take(o.f11.g13);
    total += take(o.f11.g14);
    total += take(o.f11.g15);
    total += take(o.f11.g16);
    total += take(o.f11.g17);
    total += take(o.f11.g18);
    total += take(o.f11.g19);
    total += take(o.f11.g20);
    total += take(o.f11.g21);
    total += take(o.f11.g22);
    total += take(o.f11.g23);
    total += take(o.f11.g24);
    total += take(o.f11.g25);
    total += take(o.f11.g26);
    total += take(o.f11.g27);
    total += take(o.f11.g28);
    total += take(o.f11.g29);
    total += take(o.f12.g0);
    total += take(o.f12.g1);
    total += take(o.f12.g2);
    total += take(o.f12.g3);
    total += take(o.f12.g4);
    total += take(o.f12.g5);
    total += take(o.f12.g6);
    total += take(o.f12.g7);
    total += take(o.f12.g8);
    total += take(o.f12.g9);
    total += take(o.f12.g10);
    total += take(o.f12.g11);
    total += take(o.f12.g12);
    total += take(o.f12.g13);
    total += take(o.f12.g14);
    total += take(o.f12.g15);
    total += take(o.f12.g16);
    total += take(o.f12.g17);
    total += take(o.f12.g18);
    total += take(o.f12.g19);
    total += take(o.f12.g20);
    total += take(o.f12.g21);
    total += take(o.f12.g22);
    total += take(o.f12.g23);
    total += take(o.f12.g24);
    total += take(o.f12.g25);
    total += take(o.f12.g26);
    total += take(o.f12.g27);
    total += take(o.f12.g28);
    total += take(o.f12.g29);
    total += take(o.f13.g0);
    total += take(o.f13.g1);
    total += take(o.f13.g2);
    total += take(o.f13.g3);
    total += take(o.f13.g4);
    total += take(o.f13.g5);
    total += take(o.f13.g6);
    total += take(o.f13.g7);
    total += take(o.f13.g8);
    total += take(o.f13.g9);
    total += take(o.f13.g10);
    total += take(o.f13.g11);
    total += take(o.f13.g12);
    total += take(o.f13.g13);
    total += take(o.f13.g14);
    total += take(o.f13.g15);
    total += take(o.f13.g16);
    total += take(o.f13.g17);
    total += take(o.f13.g18);
    total += take(o.f13.g19);
    total += take(o.f13.g20);
    total += take(o.f13.g21);
    total += take(o.f13.g22);
    total += take(o.f13.g23);
    total += take(o.f13.g24);
    total += take(o.f13.g25);
    total += take(o.f13.g26);
    total += take(o.f13.g27);
    total += take(o.f13.g28);
    total += take(o.f13.g29);
    total += take(o.f14.g0);
    total += take(o.f14.g1);
    total += take(o.f14.g2);
    total += take(o.f14.g3);
    total += take(o.f14.g4);
    total += take(o.f14.g5);
    total += take(o.f14.g6);
    total += take(o.f14.g7);
    total += take(o.f14.g8);
    total += take(o.f14.g9);
    total += take(o.f14.g10);
    total += take(o.f14.g11);
    total += take(o.f14.g12);
    total += take(o.f14.g13);
    total += take(o.f14.g14);
    total += take(o.f14.g15);
    total += take(o.f14.g16);
    total += take(o.f14.g17);
    total += take(o.f14.g18);
    total += take(o.f14.g19);
    total += take(o.f14.g20);
    total += take(o.f14.g21);
    total += take(o.f14.g22);
    total += take(o.f14.g23);
    total += take(o.f14.g24);
    total += take(o.f14.g25);
    total += take(o.f14.g26);
    total += take(o.f14.g27);
    total += take(o.f14.g28);
    total += take(o.f14.g29);
    total += take(o.f15.g0);
    total += take(o.f15.g1);
    total += take(o.f15.g2);
    total += take(o.f15.g3);
    total += take(o.f15.g4);
    total += take(o.f15.g5);
    total += take(o.f15.g6);
    total += take(o.f15.g7);
    total += take(o.f15.g8);
    total += take(o.f15.g9);
    total += take(o.f15.g10);
    total += take(o.f15.g11);
    total += take(o.f15.g12);
    total += take(o.f15.g13);
    total += take(o.f15.g14);
    total += take(o.f15.g15);
    total += take(o.f15.g16);
    total += take(o.f15.g17);
    total += take(o.f15.g18);
    total += take(o.f15.g19);
    total += take(o.f15.g20);
    total += take(o.f15.g21);
    total += take(o.f15.g22);
    total += take(o.f15.g23);
    total += take(o.f15.g24);
    total += take(o.f15.g25);
    total += take(o.f15.g26);
    total += take(o.f15.g27);
    total += take(o.f15.g28);
    total += take(o.f15.g29);
    total += take(o.f16.g0);
    total += take(o.f16.g1);
    total += take(o.f16.g2);
    total += take(o.f16.g3);
    total += take(o.f16.g4);
    total += take(o.f16.g5);
    total += take(o.f16.g6);
    total += take(o.f16.g7);
    total += take(o.f16.g8);
    total += take(o.f16.g9);
    total += take(o.f16.g10);
    total += take(o.f16.g11);
    total += take(o.f16.g12);
    total += take(o.f16.g13);
    total += take(o.f16.g14);
    total += take(o.f16.g15);
    total += take(o.f16.g16);
    total += take(o.f16.g17);
    total += take(o.f16.g18);
    total += take(o.f16.g19);
    total += take(o.f16.g20);
    total += take(o.f16.g21);
    total += take(o.f16.g22);
    total += take(o.f16.g23);
    total += take(o.f16.g24);
    total += take(o.f16.g25);
    total += take(o.f16.g26);
    total += take(o.f16.g27);
    total += take(o.f16.g28);
    total += take(o.f16.g29);
    total += take(o.f17.g0);
    total += take(o.f17.g1);
    total += take(o.f17.g2);
    total += take(o.f17.g3);
    total += take(o.f17.g4);
    total += take(o.f17.g5);
    total += take(o.f17.g6);
    total += take(o.f17.g7);
    total += take(o.f17.g8);
    total += take(o.f17.g9);
    total += take(o.f17.g10);
    total += take(o.f17.g11);
    total += take(o.f17.g12);
    total += take(o.f17.g13);
    total += take(o.f17.g14);
    total += take(o.f17.g15);
    total += take(o.f17.g16);
    total += take(o.f17.g17);
    total += take(o.f17.g18);
    total += take(o.f17.g19);
    total += take(o.f17.g20);
    total += take(o.f17.g21);
    total += take(o.f17.g22);
    total += take(o.f17.g23);
    total += take(o.f17.g24);
    total += take(o.f17.g25);
    total += take(o.f17.g26);
    total += take(o.f17.g27);
    total += take(o.f17.g28);
    total += take(o.f17.g29);
    total += take(o.f18.g0);
    total += take(o.f18.g1);
    total += take(o.f18.g2);
    total += take(o.f18.g3);
    total += take(o.f18.g4);
    total += take(o.f18.g5);
    total += take(o.f18.g6);
    total += take(o.f18.g7);
    total += take(o.f18.g8);
    total += take(o.f18.g9);
    total += take(o.f18.g10);
    total += take(o.f18.g11);
    total += take(o.f18.g12);
    total += take(o.f18.g13);
    total += take(o.f18.g14);
    total += take(o.f18.g15);
    total += take(o.f18.g16);
    total += take(o.f18.g17);
    total += take(o.f18.g18);
    total += take(o.f18.g19);
    total += take(o.f18.g20);
    total += take(o.f18.g21);
    total += take(o.f18.g22);
    total += take(o.f18.g23);
    total += take(o.f18.g24);
    total += take(o.f18.g25);
    total += take(o.f18.g26);
    total += take(o.f18.g27);
    total += take(o.f18.g28);
    total += take(o.f18.g29);
    total += take(o.f19.g0);
    total += take(o.f19.g1);
    total += take(o.f19.g2);
    total += take(o.f19.g3);
    total += take(o.f19.g4);
    total += take(o.f19.g5);
    total += take(o.f19.g6);
    total += take(o.f19.g7);
    total += take(o.f19.g8);
    total += take(o.f19.g9);
    total += take(o.f19.g10);
    total += take(o.f19.g11);
    total += take(o.f19.g12);
    total += take(o.f19.g13);
    total += take(o.f19.g14);
    total += take(o.f19.g15);
    total += take(o.f19.g16);
    total += take(o.f19.g17);
    total += take(o.f19.g18);
    total += take(o.f19.g19);
    total += take(o.f19.g20);
    total += take(o.f19.g21);
    total += take(o.f19.g22);
    total += take(o.f19.g23);
    total += take(o.f19.g24);
    total += take(o.f19.g25);
    total += take(o.f19.g26);
    total += take(o.f19.g27);
    total += take(o.f19.g28);
    total += take(o.f19.g29);
    total += take(o.f20.g0);
    total += take(o.f20.g1);
    total += take(o.f20.g2);
    total += take(o.f20.g3);
    total += take(o.f20.g4);
    total += take(o.f20.g5);
    total += take(o.f20.g6);
    total += take(o.f20.g7);
    total += take(o.f20.g8);
    total += take(o.f20.g9);
    total += take(o.f20.g10);
    total += take(o.f20.g11);
    total += take(o.f20.g12);
    total += take(o.f20.g13);
    total += take(o.f20.g14);
    total += take(o.f20.g15);
    total += take(o.f20.g16);
    total += take(o.f20.g17);
    total += take(o.f20.g18);
    total += take(o.f20.g19);
    total += take(o.f20.g20);
    total += take(o.f20.g21);
    total += take(o.f20.g22);
    total += take(o.f20.g23);
    total += take(o.f20.g24);
    total += take(o.f20.g25);
    total += take(o.f20.g26);
    total += take(o.f20.g27);
    total += take(o.f20.g28);
    total += take(o.f20.g29);
    total += take(o.f21.g0);
    total += take(o.f21.g1);
    total += take(o.f21.g2);
    total += take(o.f21.g3);
    total += take(o.f21.g4);
    total += take(o.f21.g5);
    total += take(o.f21.g6);
    total += take(o.f21.g7);
    total += take(o.f21.g8);
    total += take(o.f21.g9);
    total += take(o.f21.g10);
    total += take(o.f21.g11);
    total += take(o.f21.g12);
    total += take(o.f21.g13);
    total += take(o.f21.g14);
    total += take(o.f21.g15);
    total += take(o.f21.g16);
    total += take(o.f21.g17);
    total += take(o.f21.g18);
    total += take(o.f21.g19);
    total += take(o.f21.g20);
    total += take(o.f21.g21);
    total += take(o.f21.g22);
    total += take(o.f21.g23);
    total += take(o.f21.g24);
    total += take(o.f21.g25);
    total += take(o.f21.g26);
    total += take(o.f21.g27);
    total += take(o.f21.g28);
    total += take(o.f21.g29);
    total += take(o.f22.g0);
    total += take(o.f22.g1);
    total += take(o.f22.g2);
    total += take(o.f22.g3);
    total += take(o.f22.g4);
    total += take(o.f22.g5);
    total += take(o.f22.g6);
    total += take(o.f22.g7);
    total += take(o.f22.g8);
    total += take(o.f22.g9);
    total += take(o.f22.g10);
    total += take(o.f22.g11);
    total += take(o.f22.g12);
    total += take(o.f22.g13);
    total += take(o.f22.g14);
    total += take(o.f22.g15);
    total += take(o.f22.g16);
    total += take(o.f22.g17);
    total += take(o.f22.g18);
    total += take(o.f22.g19);
    total += take(o.f22.g20);
    total += take(o.f22.g21);
    total += take(o.f22.g22);
    total += take(o.f22.g23);
    total += take(o.f22.g24);
    total += take(o.f22.g25);
    total += take(o.f22.g26);
    total += take(o.f22.g27);
    total += take(o.f22.g28);
    total += take(o.f22.g29);
    total += take(o.f23.g0);
    total += take(o.f23.g1);
    total += take(o.f23.g2);
    total += take(o.f23.g3);
    total += take(o.f23.g4);
    total += take(o.f23.g5);
    total += take(o.f23.g6);
    total += take(o.f23.g7);
    total += take(o.f23.g8);
    total += take(o.f23.g9);
    total += take(o.f23.g10);
    total += take(o.f23.g11);
    total += take(o.f23.g12);
    total += take(o.f23.g13);
    total += take(o.f23.g14);
    total += take(o.f23.g15);
    total += take(o.f23.g16);
    total += take(o.f23.g17);
    total += take(o.f23.g18);
    total += take(o.f23.g19);
    total += take(o.f23.g20);
    total += take(o.f23.g21);
    total += take(o.f23.g22);
    total += take(o.f23.g23);
    total += take(o.f23.g24);
    total += take(o.f23.g25);
    total += take(o.f23.g26);
    total += take(o.f23.g27);
    total += take(o.f23.g28);
    total += take(o.f23.g29);
    total += take(o.f24.g0);
    total += take(o.f24.g1);
    total += take(o.f24.g2);
    total += take(o.f24.g3);
    total += take(o.f24.g4);
    total += take(o.f24.g5);
    total += take(o.f24.g6);
    total += take(o.f24.g7);
    total += take(o.f24.g8);
    total += take(o.f24.g9);
    total += take(o.f24.g10);
    total += take(o.f24.g11);
    total += take(o.f24.g12);
    total += take(o.f24.g13);
    total += take(o.f24.g14);
    total += take(o.f24.g15);
    total += take(o.f24.g16);
    total += take(o.f24.g17);
    total += take(o.f24.g18);
    total += take(o.f24.g19);
    total += take(o.f24.g20);
    total += take(o.f24.g21);
    total += take(o.f24.g22);
    total += take(o.f24.g23);
    total += take(o.f24.g24);
    total += take(o.f24.g25);
    total += take(o.f24.g26);
    total += take(o.f24.g27);
    total += take(o.f24.g28);
    total += take(o.f24.g29);
    total += take(o.f25.g0);
    total += take(o.f25.g1);
    total += take(o.f25.g2);
    total += take(o.f25.g3);
    total += take(o.f25.g4);
    total += take(o.f25.g5);
    total += take(o.f25.g6);
    total += take(o.f25.g7);
    total += take(o.f25.g8);
    total += take(o.f25.g9);
    total += take(o.f25.g10);
    total += take(o.f25.g11);
    total += take(o.f25.g12);
    total += take(o.f25.g13);
    total += take(o.f25.g14);
    total += take(o.f25.g15);
    total += take(o.f25.g16);
    total += take(o.f25.g17);
    total += take(o.f25.g18);
    total += take(o.f25.g19);
    total += take(o.f25.g20);
    total += take(o.f25.g21);
    total += take(o.f25.g22);
    total += take(o.f25.g23);
    total += take(o.f25.g24);
    total += take(o.f25.g25);
    total += take(o.f25.g26);
    total += take(o.f25.g27);
    total += take(o.f25.g28);
    total += take(o.f25.g29);
    total += take(o.f26.g0);
    total += take(o.f26.g1);
    total += take(o.f26.g2);
    total += take(o.f26.g3);
    total += take(o.f26.g4);
    total += take(o.f26.g5);
    total += take(o.f26.g6);
    total += take(o.f26.g7);
    total += take(o.f26.g8);
    total += take(o.f26.g9);
    total += take(o.f26.g10);
    total += take(o.f26.g11);
    total += take(o.f26.g12);
    total += take(o.f26.g13);
    total += take(o.f26.g14);
    total += take(o.f26.g15);
    total += take(o.f26.g16);
    total += take(o.f26.g17);
    total += take(o.f26.g18);
    total += take(o.f26.g19);
    total += take(o.f26.g20);
    total += take(o.f26.g21);
    total += take(o.f26.g22);
    total += take(o.f26.g23);
    total += take(o.f26.g24);
    total += take(o.f26.g25);
    total += take(o.f26.g26);
    total += take(o.f26.g27);
    total += take(o.f26.g28);
    total += take(o.f26.g29);
    total += take(o.f27.g0);
    total += take(o.f27.g1);
    total += take(o.f27.g2);
    total += take(o.f27.g3);
    total += take(o.f27.g4);
    total += take(o.f27.g5);
    total += take(o.f27.g6);
    total += take(o.f27.g7);
    total += take(o.f27.g8);
    total += take(o.f27.g9);
    total += take(o.f27.g10);
    total += take(o.f27.g11);
    total += take(o.f27.g12);
    total += take(o.f27.g13);
    total += take(o.f27.g14);
    total += take(o.f27.g15);
    total += take(o.f27.g16);
    total += take(o.f27.g17);
    total += take(o.f27.g18);
    total += take(o.f27.g19);
    total += take(o.f27.g20);
    total += take(o.f27.g21);
    total += take(o.f27.g22);
    total += take(o.f27.g23);
    total += take(o.f27.g24);
    total += take(o.f27.g25);
    total += take(o.f27.g26);
    total += take(o.f27.g27);
    total += take(o.f27.g28);
    total += take(o.f27.g29);
    total += take(o.f28.g0);
    total += take(o.f28.g1);
    total += take(o.f28.g2);
    total += take(o.f28.g3);
    total += take(o.f28.g4);
    total += take(o.f28.g5);
    total += take(o.f28.g6);
    total += take(o.f28.g7);
    total += take(o.f28.g8);
    total += take(o.f28.g9);
    total += take(o.f28.g10);
    total += take(o.f28.g11);
    total += take(o.f28.g12);
    total += take(o.f28.g13);
    total += take(o.f28.g14);
    total += take(o.f28.g15);
    total += take(o.f28.g16);
    total += take(o.f28.g17);
    total += take(o.f28.g18);
    total += take(o.f28.g19);
    total += take(o.f28.g20);
    total += take(o.f28.g21);
    total += take(o.f28.g22);
    total += take(o.f28.g23);
    total += take(o.f28.g24);
    total += take(o.f28.g25);
    total += take(o.f28.g26);
    total += take(o.f28.g27);
    total += take(o.f28.g28);
    total += take(o.f28.g29);
    total += take(o.f29.g0);
    total += take(o.f29.g1);
    total += take(o.f29.g2);
    total += take(o.f29.g3);
    total += take(o.f29.g4);
    total += take(o.f29.g5);
    total += take(o.f29.g6);
    total += take(o.f29.g7);
    total += take(o.f29.g8);
    total += take(o.f29.g9);
    total += take(o.f29.g10);
    total += take(o.f29.g11);
    total += take(o.f29.g12);
    total += take(o.f29.g13);
    total += take(o.f29.g14);
    total += take(o.f29.g15);
    total += take(o.f29.g16);
    total += take(o.f29.g17);
    total += take(o.f29.g18);
    total += take(o.f29.g19);
    total += take(o.f29.g20);
    total += take(o.f29.g21);
    total += take(o.f29.g22);
    total += take(o.f29.g23);
    total += take(o.f29.g24);
    total += take(o.f29.g25);
    total += take(o.f29.g26);
    total += take(o.f29.g27);
    total += take(o.f29.g28);
    total += take(o.f29.g29);
    total += take(o.f30.g0);
    total += take(o.f30.g1);
    total += take(o.f30.g2);
    total += take(o.f30.g3);
    total += take(o.f30.g4);
    total += take(o.f30.g5);
    total += take(o.f30.g6);
    total += take(o.f30.g7);
    total += take(o.f30.g8);
    total += take(o.f30.g9);
    total += take(o.f30.g10);
    total += take(o.f30.g11);
    total += take(o.f30.g12);
    total += take(o.f30.g13);
    total += take(o.f30.g14);
    total += take(o.f30.g15);
    total += take(o.f30.g16);
    total += take(o.f30.g17);
    total += take(o.f30.g18);
    total += take(o.f30.g19);
    total += take(o.f30.g20);
    total += take(o.f30.g21);
    total += take(o.f30.g22);
    total += take(o.f30.g23);
    total += take(o.f30.g24);
    total += take(o.f30.g25);
    total += take(o.f30.g26);
    total += take(o.f30.g27);
    total += take(o.f30.g28);
    total += take(o.f30.g29);
    total += take(o.f31.g0);
    total += take(o.f31.g1);
    total += take(o.f31.g2);
    total += take(o.f31.g3);
    total += take(o.f31.g4);
    total += take(o.f31.g5);
    total += take(o.f31.g6);
    total += take(o.f31.g7);
    total += take(o.f31.g8);
    total += take(o.f31.g9);
    total += take(o.f31.g10);
    total += take(o.f31.g11);
    total += take(o.f31.g12);
    total += take(o.f31.g13);
    total += take(o.f31.g14);
    total += take(o.f31.g15);
    total += take(o.f31.g16);
    total += take(o.f31.g17);
    total += take(o.f31.g18);
    total += take(o.f31.g19);
    total += take(o.f31.g20);
    total += take(o.f31.g21);
    total += take(o.f31.g22);
    total += take(o.f31.g23);
    total += take(o.f31.g24);
    total += take(o.f31.g25);
    total += take(o.f31.g26);
    total += take(o.f31.g27);
    total += take(o.f31.g28);
    total += take(o.f31.g29);
    total += take(o.f32.g0);
    total += take(o.f32.g1);
    total += take(o.f32.g2);
    total += take(o.f32.g3);
    total += take(o.f32.g4);
    total += take(o.f32.g5);
    total += take(o.f32.g6);
    total += take(o.f32.g7);
    total += take(o.f32.g8);
    total += take(o.f32.g9);
    total += take(o.f32.g10);
    total += take(o.f32.g11);
    total += take(o.f32.g12);
    total += take(o.f32.g13);
    total += take(o.f32.g14);
    total += take(o.f32.g15);
    total += take(o.f32.g16);
    total += take(o.f32.g17);
    total += take(o.f32.g18);
    total += take(o.f32.g19);
    total += take(o.f32.g20);
    total += take(o.f32.g21);
    total += take(o.f32.g22);
    total += take(o.f32.g23);
    total += take(o.f32.g24);
    total += take(o.f32.g25);
    total += take(o.f32.g26);
    total += take(o.f32.g27);
    total += take(o.f32.g28);
    total += take(o.f32.g29);
    total += take(o.f33.g0);
    total += take(o.f33.g1);
    total += take(o.f33.g2);
    total += take(o.f33.g3);
    total += take(o.f33.g4);
    total += take(o.f33.g5);
    total += take(o.f33.g6);
    total += take(o.f33.g7);
    total += take(o.f33.g8);
    total += take(o.f33.g9);
    total += take(o.f33.g10);
    total += take(o.f33.g11);
    total += take(o.f33.g12);
    total += take(o.f33.g13);
    total += take(o.f33.g14);
    total += take(o.f33.g15);
    total += take(o.f33.g16);
    total += take(o.f33.g17);
    total += take(o.f33.g18);
    total += take(o.f33.g19);
    total += take(o.f33.g20);
    total += take(o.f33.g21);
    total += take(o.f33.g22);
    total += take(o.f33.g23);
    total += take(o.f33.g24);
    total += take(o.f33.g25);
    total += take(o.f33.g26);
    total += take(o.f33.g27);
    total += take(o.f33.g28);
    total += take(o.f33.g29);
    total += take(o.f34.g0);
    total += take(o.f34.g1);
    total += take(o.f34.g2);
    total += take(o.f34.g3);
    total += take(o.f34.g4);
    total += take(o.f34.g5);
    total += take(o.f34.g6);
    total += take(o.f34.g7);
    total += take(o.f34.g8);
    total += take(o.f34.g9);
    total += take(o.f34.g10);
    total += take(o.f34.g11);
    total += take(o.f34.g12);
    total += take(o.f34.g13);
    total += take(o.f34.g14);
    total += take(o.f34.g15);
    total += take(o.f34.g16);
    total += take(o.f34.g17);
    total += take(o.f34.g18);
    total += take(o.f34.g19);
    total += take(o.f34.g20);
    total += take(o.f34.g21);
    total += take(o.f34.g22);
    total += take(o.f34.g23);
    total += take(o.f34.g24);
    total += take(o.f34.g25);
    total += take(o.f34.g26);
    total += take(o.f34.g27);
    total += take(o.f34.g28);
    total += take(o.f34.g29);
    total += take(o.f35.g0);
    total += take(o.f35.g1);
    total += take(o.f35.g2);
    total += take(o.f35.g3);
    total += take(o.f35.g4);
    total += take(o.f35.g5);
    total += take(o.f35.g6);
    total += take(o.f35.g7);
    total += take(o.f35.g8);
    total += take(o.f35.g9);
    total += take(o.f35.g10);
    total += take(o.f35.g11);
    total += take(o.f35.g12);
    total += take(o.f35.g13);
    total += take(o.f35.g14);
    total += take(o.f35.g15);
    total += take(o.f35.g16);
    total += take(o.f35.g17);
    total += take(o.f35.g18);
    total += take(o.f35.g19);
    total += take(o.f35.g20);
    total += take(o.f35.g21);
    total += take(o.f35.g22);
    total += take(o.f35.g23);
    total += take(o.f35.g24);
    total += take(o.f35.g25);
    total += take(o.f35.g26);
    total += take(o.f35.g27);
    total += take(o.f35.g28);
    total += take(o.f35.g29);
    total += take(o.f36.g0);
    total += take(o.f36.g1);
    total += take(o.f36.g2);
    total += take(o.f36.g3);
    total += take(o.f36.g4);
    total += take(o.f36.g5);
    total += take(o.f36.g6);
    total += take(o.f36.g7);
    total += take(o.f36.g8);
    total += take(o.f36.g9);
    total += take(o.f36.g10);
    total += take(o.f36.g11);
    total += take(o.f36.g12);
    total += take(o.f36.g13);
    total += take(o.f36.g14);
    total += take(o.f36.g15);
    total += take(o.f36.g16);
    total += take(o.f36.g17);
    total += take(o.f36.g18);
    total += take(o.f36.g19);
    total += take(o.f36.g20);
    total += take(o.f36.g21);
    total += take(o.f36.g22);
    total += take(o.f36.g23);
    total += take(o.f36.g24);
    total += take(o.f36.g25);
    total += take(o.f36.g26);
    total += take(o.f36.g27);
    total += take(o.f36.g28);
    total += take(o.f36.g29);
    total += take(o.f37.g0);
    total += take(o.f37.g1);
    total += take(o.f37.g2);
    total += take(o.f37.g3);
    total += take(o.f37.g4);
    total += take(o.f37.g5);
    total += take(o.f37.g6);
    total += take(o.f37.g7);
    total += take(o.f37.g8);
    total += take(o.f37.g9);
    total += take(o.f37.g10);
    total += take(o.f37.g11);
    total += take(o.f37.g12);
    total += take(o.f37.g13);
    total += take(o.f37.g14);
    total += take(o.f37.g15);
    total += take(o.f37.g16);
    total += take(o.f37.g17);
    total += take(o.f37.g18);
    total += take(o.f37.g19);
    total += take(o.f37.g20);
    total += take(o.f37.g21);
    total += take(o.f37.g22);
    total += take(o.f37.g23);
    total += take(o.f37.g24);
    total += take(o.f37.g25);
    total += take(o.f37.g26);
    total += take(o.f37.g27);
    total += take(o.f37.g28);
    total += take(o.f37.g29);
    total += take(o.f38.g0);
    total += take(o.f38.g1);
    total += take(o.f38.g2);
    total += take(o.f38.g3);
    total += take(o.f38.g4);
    total += take(o.f38.g5);
    total += take(o.f38.g6);
    total += take(o.f38.g7);
    total += take(o.f38.g8);
    total += take(o.f38.g9);
    total += take(o.f38.g10);
    total += take(o.f38.g11);
    total += take(o.f38.g12);
    total += take(o.f38.g13);
    total += take(o.f38.g14);
    total += take(o.f38.g15);
    total += take(o.f38.g16);
    total += take(o.f38.g17);
    total += take(o.f38.g18);
    total += take(o.f38.g19);
    total += take(o.f38.g20);
    total += take(o.f38.g21);
    total += take(o.f38.g22);
    total += take(o.f38.g23);
    total += take(o.f38.g24);
    total += take(o.f38.g25);
    total += take(o.f38.g26);
    total += take(o.f38.g27);
    total += take(o.f38.g28);
    total += take(o.f38.g29);
    total += take(o.f39.g0);
    total += take(o.f39.g1);
    total += take(o.f39.g2);
    total += take(o.f39.g3);
    total += take(o.f39.g4);
    total += take(o.f39.g5);
    total += take(o.f39.g6);
    total += take(o.f39.g7);
    total += take(o.f39.g8);
    total += take(o.f39.g9);
    total += take(o.f39.g10);
    total += take(o.f39.g11);
    total += take(o.f39.g12);
    total += take(o.f39.g13);
    total += take(o.f39.g14);
    total += take(o.f39.g15);
    total += take(o.f39.g16);
    total += take(o.f39.g17);
    total += take(o.f39.g18);
    total += take(o.f39.g19);
    total += take(o.f39.g20);
    total += take(o.f39.g21);
    total += take(o.f39.g22);
    total += take(o.f39.g23);
    total += take(o.f39.g24);
    total += take(o.f39.g25);
    total += take(o.f39.g26);
    total += take(o.f39.g27);
    total += take(o.f39.g28);
    total += take(o.f39.g29);
    println!("{total}");
}
