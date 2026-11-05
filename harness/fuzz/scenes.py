"""scenes.py: statements for gen.py that are each a small scene of their own.

A scene is one block: it declares the types and functions it needs inside itself, uses nothing
of the function around it, and feeds what it computes and what it drops into the checksum. They
cover what the free-form statements of gen.py reach rarely or not at all: unwinding while
values are being dropped, futures, trait objects through upcasts, closures made in loops,
floats at their edges, 128-bit integers, enums with niches, arms no value reaches, statics that
point into each other, representations, and inline assembly.
"""

WIDE = ["0", "1", "2", "3", "7", "10", "255", "65537", "u64::MAX as u128", "(1u128 << 64)", "(1u128 << 64) + 1",
        "(1u128 << 127)", "u128::MAX", "u128::MAX - 1", "0x0123_4567_89ab_cdef_fedc_ba98_7654_3210",
        "340282366920938463463374607431768211297", "18446744073709551557", "(u128::MAX / 3)", "1_000_000_007"]
EDGES = ["0.0", "-0.0", "1.0", "-1.0", "0.5", "-0.5", "1.5", "2.5", "-2.5", "255.0", "255.5", "256.0", "-128.5", "-129.0",
         "65535.9", "2147483647.0", "2147483648.0", "-2147483649.0", "4294967295.0", "4294967296.0", "9223372036854775807.0",
         "1.8446744073709552e19", "-9.223372036854777e18", "1.7014118346046923e38", "3.4028236692093846e38", "1e39", "-1e39",
         "1e-45", "5e-324", "2.2250738585072014e-308", "f64::MAX", "f64::MIN", "f64::EPSILON", "f64::INFINITY",
         "f64::NEG_INFINITY", "f64::NAN", "16777216.0", "16777217.0", "9007199254740993.0", "0.1", "0.30000000000000004"]
INT_NAMES = ["i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize"]


class Scenes:
    """The scene statements. The class that has them provides `r`, `emit`, `choose`, `cov`, `fresh`."""

    def s_scene(self):
        self.spend(40)
        lines = self.choose([
            (4, "scene.unwind.drop_panics", self.sc_drop_panics),
            (4, "scene.unwind.partial_move", self.sc_partial_move),
            (3, "scene.unwind.operands", self.sc_unwind_operands),
            (2, "scene.unwind.loop", self.sc_unwind_loop),
            (2, "scene.unwind.closure", self.sc_unwind_closure),
            (2, "scene.unwind.replace", self.sc_unwind_replace),
            (3, "scene.async.across_await", self.sc_async_locals),
            (3, "scene.async.dropped_pending", self.sc_async_dropped),
            (2, "scene.async.nested", self.sc_async_nested),
            (1.5, "scene.async.panic_in_poll", self.sc_async_panic),
            (1.5, "scene.async.boxed_futures", self.sc_async_boxed),
            (3, "scene.dyn.upcast", self.sc_upcast),
            (2, "scene.dyn.any", self.sc_any),
            (2, "scene.dyn.unsized_generic", self.sc_unsized_generic),
            (2.5, "scene.closure.made_in_loop", self.sc_closure_loop),
            (2, "scene.closure.fields", self.sc_closure_fields),
            (2.5, "scene.float.casts", self.sc_float_casts),
            (2, "scene.float.operations", self.sc_float_ops),
            (2.5, "scene.wide.operations", self.sc_wide),
            (1.5, "scene.wide.arguments", self.sc_wide_args),
            (2.5, "scene.niche.enums", self.sc_niche),
            (3, "scene.match.arms_in_the_way", self.sc_arms),
            (2, "scene.static.pointers", self.sc_statics),
            (2, "scene.repr.variety", self.sc_repr),
            (1, "scene.asm", self.sc_asm),
        ])
        self.emit("{")
        for line in lines:
            self.emit("    " + line)
        self.emit("}")

    def num(self, least=1, most=60):
        return self.r.randint(least, most)

    def some(self, items, least, most):
        return self.r.sample(items, min(len(items), self.r.randint(least, most)))

    # ---- unwinding ------------------------------------------------------------------------

    def holder(self, bomb):
        """A value that owns `bomb` and other noisy values, in some shape."""
        a, b = self.num(), self.num()
        return self.r.choice([
            f"(Noisy({a}), {bomb}, Noisy({b}))",
            f"Duo {{ a: {bomb}, b: Noisy({a}) }}",
            f"Duo {{ a: Noisy({a}), b: ({bomb}, Noisy({b})) }}",
            f"Some({bomb})",
            f"Box::new(({bomb}, Noisy({a})))",
            f"vec![Box::new(Noisy({a})) as Box<dyn Any>, Box::new({bomb}), Box::new(Noisy({b}))]",
            f"[Some({bomb}), None, Some(Bomb({a}))]",
            f"Rc::new((Noisy({a}), {bomb}))",
            f"(Noisy({a}), [{bomb}], Box::new(Noisy({b})))",
        ])

    def sc_drop_panics(self):
        lines = ["struct Outer(Noisy, Bomb, Noisy);", "impl Drop for Outer { fn drop(&mut self) { feed(0x0d0d + self.0.0 as u64); } }"]
        for _ in range(self.r.choice([1, 2, 3])):
            body = [f"let _a = Noisy({self.num()});"]
            for i in range(self.r.choice([1, 1, 2])):
                bomb = f"Bomb({self.num()})"
                body.append(self.r.choice([
                    f"let _h{i} = {self.holder(bomb)};",
                    f"let _h{i} = Outer(Noisy({self.num()}), {bomb}, Noisy({self.num()}));",
                    f"{{ let _inner = {self.holder(bomb)}; let _late = Noisy({self.num()}); }}",
                    f"let mut v{i} = vec![{bomb}, Bomb({self.num()}), Bomb({self.num()})]; v{i}.truncate(1);",
                    f"drop({self.holder(bomb)});",
                    f"let _ = {self.holder(bomb)};",
                    f"take3(Noisy({self.num()}), {self.holder(bomb)}, Noisy({self.num()}));",
                ]))
            body.append(f"let _z = Noisy({self.num()});")
            lines.append("caught(|| { " + " ".join(body) + " });")
        return lines

    def sc_partial_move(self):
        a, b, c = self.num(), self.num(), self.num()
        shapes = [
            (f"let t = (Noisy({a}), Noisy({b}), Noisy({c}));", ["let _x = t.1;", "let _y = t.0;", "let _w = t.2;"]),
            (f"let t = Duo {{ a: Noisy({a}), b: (Noisy({b}), Noisy({c})) }};", ["let _x = t.b.0;", "let _y = t.a;", "let _w = t.b.1;"]),
            (f"let t = Box::new((Noisy({a}), Noisy({b}), Noisy({c})));", ["let _x = t.2;", "let _y = t.0;", "let _w = t.1;"]),
            (f"let t = [Noisy({a}), Noisy({b}), Noisy({c})];", ["let [_x, ..] = t;"]),
            (f"let t = [Noisy({a}), Noisy({b}), Noisy({c})];", ["let [.., _x] = t;"]),
            (f"let t = [Noisy({a}), Noisy({b}), Noisy({c})];", ["let [_, _x @ ..] = t;"]),
            (f"let t = (Some(Noisy({a})), Duo {{ a: Noisy({b}), b: Noisy({c}) }});", ["let _x = t.1.b;", "let _y = t.0;", "let _w = t.1.a;"]),
            (f"let t = (Noisy({a}), Box::new(Duo {{ a: Noisy({b}), b: Noisy({c}) }}));", ["let _x = t.1.a;", "let _y = t.0;"]),
        ]
        lines = []
        for make, moves in self.some(shapes, 1, 3):
            moves = moves[:self.r.randint(1, len(moves))]
            at = self.r.randint(0, len(moves))
            body = [make] + moves[:at] + ["trip(armed, ());"] + moves[at:]
            if self.r.random() < 0.4:
                body.append(f"let _late = Noisy({self.num()});")
            lines.append("for armed in [false, true] { caught(|| { " + " ".join(body) + " }); }")
        return lines

    def sc_unwind_operands(self):
        n = lambda: f"Noisy({self.num()})"
        t = lambda: f"trip(armed, {n()})"
        forms = [
            lambda: f"take3({n()}, {t()}, {n()});",
            lambda: f"take3(first_of(&{n()}, &{n()}).0, {n()}, {t()});",
            lambda: f"let _v = vec![{n()}, {t()}, {n()}];",
            lambda: f"let _v = [{n()}, {n()}, {t()}];",
            lambda: f"let _v = ({n()}, Duo {{ a: {n()}, b: {t()} }});",
            lambda: f"let _v = Duo {{ a: {t()}, b: {n()} }};",
            lambda: f"let _v = Some({n()}).map(|it| ({t()}, it));",
            lambda: f"let _v = Box::new(({n()}, {t()}));",
            lambda: f"let _v = {{ let _local = {n()}; ({n()}, {t()}) }};",
            lambda: f"let _v = {{ take3({n()}, &{n()}, {t()}) }};",
            lambda: f"first_of(&{n()}, &{t()}).mix();",
            lambda: f"let _v = if armed {{ ({n()}, {t()}) }} else {{ ({n()}, {n()}) }};",
            lambda: f"let _v = match Some({n()}) {{ Some(it) => ({t()}, it), None => ({n()}, {n()}) }};",
            lambda: f"let mut held = {n()}; held = {t()};",
            lambda: f"let mut held = vec![{n()}]; held.push({t()}); held.insert(0, {n()});",
        ]
        lines = []
        for form in self.some(forms, 2, 4):
            lines.append("for armed in [false, true] { caught(|| { " + form() + " }); }")
        return lines

    def sc_unwind_loop(self):
        k, j, top = self.r.randint(0, 5), self.r.randint(0, 5), self.r.randint(3, 6)
        kind = self.r.choice(["for i in 0..%du32" % top, "let mut i = 0u32; while i < %d" % top, "let mut i = 0u32; 'out: loop"])
        step = "" if kind.startswith("for") else " i += 1;"
        stop = "break 'out;" if "loop" in kind else "break;"
        guard = f"if i >= {top} {{ {stop} }} " if "loop" in kind else ""
        body = (f"{guard}let n = Noisy(i); if i == {k} {{ trip(armed, ()); }} "
                f"if i % 2 == {self.r.randint(0, 1)} {{ kept.push(n);{step} continue; }} "
                f"let _m = Noisy(10 + i); if i == {j} {{ {stop} }}{step}")
        return [f"for armed in [false, true] {{ caught(|| {{ let mut kept = Vec::new(); {kind} {{ {body} }} feed(kept.len() as u64); }}); }}"]

    def sc_unwind_closure(self):
        a, b = self.num(), self.num()
        return self.some([
            f"for armed in [false, true] {{ let n = Noisy({a}); let f = move |armed: bool| {{ let _l = Noisy({b}); trip(armed, ()); n.0 }}; caught(move || feed(f(armed) as u64)); }}",
            f"for armed in [false, true] {{ let n = Noisy({a}); let once = move || {{ let n = n; trip(armed, ()); drop(n); }}; caught(once); }}",
            f"for armed in [false, true] {{ let mut seen = Vec::new(); caught(|| {{ let mut note = |it: u32| {{ seen.push(Noisy(it)); trip(armed && it == 2, ()); }}; note(1); note(2); note(3); }}); feed(seen.len() as u64); }}",
            f"for armed in [false, true] {{ let acts: Vec<Box<dyn Fn() -> u32>> = vec![Box::new(|| {a}), Box::new(move || trip(armed, {b})), Box::new(|| 3)]; caught(|| {{ let _g = Noisy({a}); for act in &acts {{ feed(act() as u64); }} }}); }}",
            f"for armed in [false, true] {{ let n = Noisy({a}); let m = Noisy({b}); caught(|| {{ let by_ref = || n.0 + m.0; feed(by_ref() as u64); let taken = move || {{ let _own = m; trip(armed, 1u32) }}; feed(taken() as u64); }}); n.mix(); }}",
        ], 1, 3)

    def sc_unwind_replace(self):
        a, b = self.num(), self.num()
        return self.some([
            f"caught(|| {{ let mut slot = Some(Bomb({a})); let _n = Noisy({b}); slot = None; feed(slot.is_none() as u64); }});",
            f"caught(|| {{ let mut pair = (Noisy({a}), Bomb({b})); pair.1 = Bomb({a}); feed(7); }});",
            f"caught(|| {{ let mut items = vec![Noisy({a})]; let old = std::mem::replace(&mut items, vec![Noisy({b})]); trip(true, ()); drop(old); }});",
            f"caught(|| {{ let mut boxed = Box::new(Bomb({a})); *boxed = Bomb({b}); feed(9); }});",
            f"for armed in [false, true] {{ caught(|| {{ let mut held = Duo {{ a: Noisy({a}), b: Noisy({b}) }}; held.a = trip(armed, Noisy(1)); held.b = Noisy(2); }}); }}",
            f"caught(|| {{ let cell = RefCell::new(vec![Noisy({a})]); let _g = Noisy({b}); cell.borrow_mut().push(trip(true, Noisy(3))); }});",
        ], 1, 3)

    # ---- futures --------------------------------------------------------------------------

    def held(self):
        """A local for a future to keep across an `.await`, and an expression that uses it."""
        name, k = self.fresh("h"), self.num()
        return self.r.choice([
            (f"let {name} = Noisy(seed % {k} + 1);", f"{name}.0 as u64"),
            (f"let {name}: u128 = (seed as u128) << {self.r.randint(0, 96)};", f"({name} >> 3) as u64"),
            (f"let {name} = [seed as u8; {self.r.choice([1, 3, 17, 40])}];", f"{name}[0] as u64"),
            (f"let {name} = (seed as f64 * 0.5, seed as u8, seed % 3 == 0);", f"{name}.0 as u64 + {name}.1 as u64 + {name}.2 as u64"),
            (f"let {name} = vec![Noisy({k}), Noisy(seed % 5)];", f"{name}.len() as u64"),
            (f"let {name} = format!(\"{{seed}}-{k}\");", f"{name}.len() as u64"),
            (f"let {name}: Option<Box<u16>> = if seed % 2 == 0 {{ Some(Box::new({k})) }} else {{ None }};", f"{name}.as_deref().copied().unwrap_or(1) as u64"),
            (f"let mut {name} = seed as u64; let {name}r = &mut {name}; *{name}r += {k};", f"*{name}r"),
        ])

    def async_fn(self, name):
        parts, uses = [], []
        for _ in range(self.r.randint(1, 4)):
            decl, use = self.held()
            parts.append(decl)
            uses.append(use)
            value = self.fresh("w")
            parts.append(self.r.choice([
                f"let {value} = later({self.r.randint(0, 3)}, seed as u64 + {self.num()}).await;",
                f"let {value} = later({self.r.randint(0, 2)}, [seed as u64; {self.r.choice([2, 9])}]).await[0];",
                f"let {value} = if seed % 2 == 0 {{ later(1, 5u64).await }} else {{ {self.num()} }};",
                f"let mut {value} = 0u64; for i in 0..{self.r.randint(1, 3)}u64 {{ let _g = Noisy(i as u32); {value} += later((i % 2) as u32, i).await; }}",
                f"let {value} = match later(1, seed % 3).await {{ 0 => later(0, 10u64).await, 1 => 11, _ => later(2, 12u64).await }};",
            ]))
            uses.append(value)
        total = " ^ ".join(f"({use}).rotate_left({i})" for i, use in enumerate(uses))
        return f"async fn {name}(seed: u32) -> u64 {{ {' '.join(parts)} {total} }}"

    def sc_async_locals(self):
        lines = [self.async_fn("work")]
        for _ in range(self.r.randint(1, 3)):
            lines.append(f"block_on(work({self.num(0, 99)})).mix();")
        return lines

    def sc_async_dropped(self):
        lines = [self.async_fn("work")]
        for _ in range(self.r.randint(2, 4)):
            lines.append(f"poll_some(work({self.num(0, 99)}), {self.r.randint(0, 6)}).mix();")
        return lines

    def sc_async_nested(self):
        a, b = self.num(), self.num()
        lines = [
            f"async fn leaf(x: u64) -> u64 {{ let _g = Noisy(x as u32 % 9); later((x % 3) as u32, x + {a}).await }}",
            self.async_fn("work"),
            "async fn down(n: u32) -> u64 { if n == 0 { leaf(1).await } else { Box::pin(down(n - 1)).await + leaf(n as u64).await } }",
            f"async fn outer(seed: u32) -> u64 {{ let n = Noisy(seed); let a = leaf(seed as u64).await; let (b, c) = both(leaf(a), work(seed)).await; let d = async {{ let _in = Noisy({b}); leaf(b ^ c).await }}.await; a ^ b ^ c ^ d ^ n.0 as u64 }}",
        ]
        uses = [
            f"block_on(outer({self.num(0, 50)})).mix();",
            f"poll_some(outer({self.num(0, 50)}), {self.r.randint(0, 8)}).mix();",
            f"block_on(down({self.r.randint(0, 4)})).mix();",
            f"{{ let n = Noisy({a}); let captured = async move {{ later(2, ()).await; n.0 }}; poll_some(captured, {self.r.randint(0, 3)}).mix(); }}",
            f"{{ let add = async |x: u64| later(1, x).await + {b}; block_on(add(3)).mix(); }}",
            f"{{ let n = Noisy({b}); let by_ref = async {{ later(1, ()).await; n.0 }}; block_on(by_ref).mix(); n.mix(); }}",
        ]
        return lines + self.some(uses, 2, 4)

    def sc_async_panic(self):
        a, k = self.num(), self.r.randint(0, 2)
        return [
            f"async fn leaf(x: u64) -> u64 {{ let _g = Noisy(x as u32 % 9); later((x % 3) as u32, x).await }}",
            f"for armed in [false, true] {{ caught(|| {{ let got = block_on(async {{ let _n = Noisy({a}); let a = leaf({k}).await; trip(armed, ()); let _m = Noisy(2); a + leaf(4).await }}); feed(got); }}); }}",
            f"caught(|| {{ let got = poll_some(async {{ let _b = Bomb({a}); leaf(2).await }}, {k}); feed(got.is_some() as u64); }});",
        ]

    def sc_async_boxed(self):
        a, b = self.num(), self.num()
        return [
            f"async fn leaf(x: u64) -> u64 {{ let _g = Noisy(x as u32 % 9); later((x % 3) as u32, x).await }}",
            f"let futures: Vec<Pin<Box<dyn Future<Output = u64>>>> = vec![Box::pin(leaf({a})), Box::pin(async {{ {b} }}), Box::pin(later(2, 9u64)), Box::pin(async {{ leaf(1).await + leaf(2).await }})];",
            self.r.choice([
                "for future in futures { block_on(future).mix(); }",
                f"for (i, future) in futures.into_iter().enumerate() {{ poll_some(future, i as u32 % {self.r.randint(1, 3)}).mix(); }}",
                "let mut futures = futures; futures.truncate(2); for future in futures.into_iter().rev() { block_on(future).mix(); }",
            ]),
        ]

    # ---- trait objects --------------------------------------------------------------------

    def sc_upcast(self):
        a, b, c = self.num(), self.num(), self.num()
        lines = [
            "trait Base { fn id(&self) -> u32; fn name(&self) -> &'static str { \"base\" } }",
            "trait Mid: Base { fn twice(&self) -> u32 { self.id().wrapping_mul(2) } }",
            "trait Top: Mid + Debug { fn all(&self) -> u64 { self.id() as u64 + self.twice() as u64 + self.name().len() as u64 } }",
            "#[derive(Debug)] struct P(u32);",
            "#[derive(Debug)] struct Q(u8, Noisy);",
            "impl Base for P { fn id(&self) -> u32 { self.0 } }",
            "impl Base for Q { fn id(&self) -> u32 { self.0 as u32 + self.1.0 } fn name(&self) -> &'static str { \"q\" } }",
            "impl Mid for P {} impl Top for P {}",
            f"impl Mid for Q {{ fn twice(&self) -> u32 {{ {c} }} }} impl Top for Q {{}}",
            "fn through(it: &dyn Mid) -> u32 { let base: &dyn Base = it; base.id() ^ it.twice() }",
            f"let mut tops: Vec<Box<dyn Top>> = vec![Box::new(P({a})), Box::new(Q({b % 200}, Noisy({c}))), Box::new(P({b}))];",
        ]
        uses = [
            "for top in &tops { let mid: &dyn Mid = &**top; let base: &dyn Base = mid; feed(base.id() as u64); base.name().mix(); feed(through(mid) as u64); feed(top.all()); }",
            "{ let last: Box<dyn Base> = tops.pop().unwrap(); feed(last.id() as u64); }",
            "{ let mid: Box<dyn Mid> = tops.remove(1); let base: Box<dyn Base> = mid; base.name().mix(); }",
            "{ let shared: Rc<dyn Top> = Rc::new(Q(1, Noisy(2))); let mid: Rc<dyn Mid> = shared.clone(); feed(mid.twice() as u64 + Rc::strong_count(&shared) as u64); }",
            "format!(\"{:?}\", tops).mix();",
            "{ let first: &mut dyn Top = &mut *tops[0]; let base: &mut dyn Base = first; feed(base.id() as u64); }",
            "{ let sync: Arc<dyn Base + Send + Sync> = Arc::new(P(5)); feed(sync.id() as u64); }",
        ]
        return lines + self.some(uses, 2, 5)

    def sc_any(self):
        a, b = self.num(), self.num()
        values = self.some([f"Box::new({a}u32)", "Box::new(\"text\")", f"Box::new(Noisy({b}))", "Box::new(1.5f64)", f"Box::new(({a}u8, true))",
                            "Box::new(String::from(\"owned\"))", f"Box::new([{b}u16; 3])", "Box::new(())", f"Box::new(Some({a}i64))"], 3, 7)
        return [
            f"let mut items: Vec<Box<dyn Any>> = vec![{', '.join(values)}];",
            "for item in &items { if let Some(it) = item.downcast_ref::<u32>() { feed(*it as u64); } else if let Some(it) = item.downcast_ref::<&str>() { it.mix(); } "
            "else if item.is::<Noisy>() { feed(1); } else if let Some(it) = item.downcast_ref::<(u8, bool)>() { feed(it.0 as u64 + it.1 as u64); } else { feed(2); } }",
            self.r.choice([
                "while let Some(item) = items.pop() { match item.downcast::<Noisy>() { Ok(it) => it.mix(), Err(other) => feed(other.is::<String>() as u64) } }",
                "{ let first = items.swap_remove(0); match first.downcast::<u32>() { Ok(it) => feed(*it as u64), Err(_) => feed(3) } }",
                "items.retain(|it| !it.is::<f64>()); feed(items.len() as u64);",
            ]),
        ]

    def sc_unsized_generic(self):
        a, b = self.num(), self.num()
        lines = [
            "trait Shape { fn area(&self) -> u64; fn scaled(&self, by: u64) -> u64 where Self: Sized { self.area() * by } }",
            "trait Conv<T> { fn conv(&self) -> T; }",
            "struct Sq(u64); struct Rect(u64, u64);",
            "impl Shape for Sq { fn area(&self) -> u64 { self.0 * self.0 } }",
            "impl Shape for Rect { fn area(&self) -> u64 { self.0 * self.1 } }",
            "impl<T: Shape + ?Sized> Shape for Box<T> { fn area(&self) -> u64 { (**self).area() + 1 } }",
            "impl Conv<u64> for Sq { fn conv(&self) -> u64 { self.0 } } impl Conv<String> for Sq { fn conv(&self) -> String { self.0.to_string() } }",
            "impl dyn Shape { fn doubled(&self) -> u64 { self.area() * 2 } }",
            "fn total<T: Shape + ?Sized>(it: &T) -> u64 { it.area() }",
            "fn sum(shapes: &[&dyn Shape]) -> u64 { shapes.iter().map(|it| total(*it)).sum() }",
            f"let (sq, rect) = (Sq({a}), Rect({a}, {b}));",
        ]
        uses = [
            "feed(total(&sq) + total(&rect as &dyn Shape) + sq.scaled(3));",
            "feed(sum(&[&sq, &rect, &Sq(2)]));",
            "{ let boxed: Box<dyn Shape> = Box::new(Rect(2, 3)); feed(total(&boxed) + boxed.doubled() + total(&*boxed)); }",
            "{ let number: &dyn Conv<u64> = &sq; let text: &dyn Conv<String> = &sq; feed(number.conv()); text.conv().mix(); }",
            "{ let mut counts: Box<dyn Iterator<Item = u64>> = Box::new((1..4u64).map(|it| it * 3)); feed(counts.next().unwrap_or(0)); feed(counts.sum()); }",
            "{ let mut calls = 0u64; let mut bump: Box<dyn FnMut(u64) -> u64> = Box::new(|by| { calls += by; calls }); bump(2); feed(bump(5)); }",
            "{ let nested: Box<Box<dyn Shape>> = Box::new(Box::new(Sq(3))); feed(nested.area()); }",
        ]
        return lines + self.some(uses, 2, 5)

    # ---- closures -------------------------------------------------------------------------

    def sc_closure_loop(self):
        a, n = self.num(), self.r.randint(2, 5)
        return self.some([
            f"{{ let mut made: Vec<Box<dyn Fn(u32) -> u32>> = Vec::new(); for i in 0..{n}u32 {{ let k = i.wrapping_mul({a}); made.push(Box::new(move |x| x.wrapping_add(k))); }} for f in &made {{ f(1).mix(); }} }}",
            f"{{ let data = [{a}u32, 2, 3, 4, 5]; let mut total = 0u32; for i in 0..{n} {{ let add = |x: u32| x.wrapping_add(data[i]); total = total.wrapping_add(add(i as u32)); let mut bump = || total = total.wrapping_mul(3); bump(); }} total.mix(); }}",
            f"{{ let mut count = 0u32; let mut step = |by: u32| {{ count += by; count }}; for i in 0..{n} {{ step(i).mix(); }} count.mix(); }}",
            f"{{ for i in 0..{n}u32 {{ let n = Noisy(i); let once = move || n.0 + {a}; if i != 1 {{ once().mix(); }} }} }}",
            f"{{ let mut kept = Vec::new(); for i in 0..{n}u32 {{ let n = Noisy(i); let by_ref = || n.0 * 2; by_ref().mix(); if i % 2 == 0 {{ kept.push(move || n.0); }} }} for f in kept.iter().rev() {{ f().mix(); }} }}",
            f"{{ let mut log = Vec::new(); let mut i = 0u32; while i < {n} {{ let mut push = |it: u32| log.push(it * {a}); push(i); push(i + 1); i += 1; }} log.mix(); }}",
            f"{{ let found = 'search: loop {{ for i in 0..{n}u32 {{ let hit = move |at: u32| at * i == {a % 6}; for j in 0..3 {{ if hit(j) {{ break 'search Some((i, j)); }} }} }} break None; }}; found.mix(); }}",
            f"{{ let mut fs: Vec<Box<dyn FnMut() -> u32>> = Vec::new(); for i in 0..{n}u32 {{ let mut own = i; fs.push(Box::new(move || {{ own += {a}; own }})); }} for round in 0..2 {{ for f in fs.iter_mut() {{ feed(f() as u64 + round); }} }} }}",
            f"{{ let text = String::from(\"abc\"); let mut lens = Vec::new(); for i in 0..{n} {{ let view = || text.len() + i; lens.push(view()); let owned = text.clone(); let eat = move || owned.len(); lens.push(eat()); }} lens.mix(); text.mix(); }}",
        ], 2, 4)

    def sc_closure_fields(self):
        a, b = self.num(), self.num()
        return self.some([
            f"{{ let mut d = Duo {{ a: {a}u32, b: Noisy({b}) }}; let mut inc = || d.a += 1; inc(); inc(); let take = move || d.b; feed(d.a as u64); take().mix(); }}",
            f"{{ let mut pair = (Noisy({a}), vec![{b}u8]); let mut grow = || pair.1.push(1); grow(); let first = &pair.0; first.mix(); pair.1.mix(); }}",
            f"{{ let d = Duo {{ a: Noisy({a}), b: Noisy({b}) }}; let only_a = move || d.a.0; feed(only_a() as u64); d.b.mix(); }}",
            f"{{ let mut t = ({a}u64, ({b}u64, String::from(\"s\"))); let mut c = || {{ t.0 += 1; t.1.0 *= 2; }}; c(); c(); let s = &t.1.1; s.mix(); feed(t.0 + t.1.0); }}",
            f"{{ let nested = Duo {{ a: Duo {{ a: Noisy({a}), b: 2u8 }}, b: Noisy({b}) }}; let c = move || {{ let Duo {{ a: inner, .. }} = nested; inner.b }}; feed(c() as u64); }}",
            f"{{ let boxed = Box::new((Noisy({a}), Noisy({b}))); let c = move || boxed.0.0; feed(c() as u64); }}",
        ], 1, 3)

    # ---- floats ---------------------------------------------------------------------------

    def sc_float_casts(self):
        lines = []
        for _ in range(self.r.randint(2, 5)):
            value, fty = self.r.choice(EDGES), self.r.choice(["f32", "f64"])
            ints = self.some(INT_NAMES, 2, 6)
            casts = ", ".join(f"(x as {ty}) as i128 as u128 as u64" for ty in ints)
            lines.append(f"{{ let x = black_box(({value}) as {fty}); for word in [{casts}] {{ feed(word); }} }}")
        for _ in range(self.r.randint(1, 3)):
            ty = self.r.choice(INT_NAMES)
            value = self.r.choice([f"{ty}::MAX", f"{ty}::MIN", f"{ty}::MAX - 1", f"({ty}::MAX >> 1) + 1", f"{ty}::MAX / 3", "77", "1"])
            lines.append(f"{{ let x = black_box({value} as {ty}); (x as f32).mix(); (x as f64).mix(); ((x as f32) as {ty}).mix(); ((x as f64) as {ty}).mix(); (x as f64 as f32).mix(); }}")
        return lines

    def sc_float_ops(self):
        fty = self.r.choice(["f32", "f64"])
        a, b = (f"black_box(({self.r.choice(EDGES)}) as {fty})" for _ in range(2))
        ops = [
            "(a + b).mix(); (a - b).mix(); (a * b).mix(); (a / b).mix(); (a % b).mix(); (-a).mix();",
            "a.floor().mix(); a.ceil().mix(); a.round().mix(); a.trunc().mix(); a.fract().mix(); a.abs().mix(); a.round_ties_even().mix();",
            "a.sqrt().mix(); a.mul_add(b, 1.5).mix(); a.rem_euclid(3.0).mix(); a.div_euclid(3.0).mix(); a.powi(3).mix(); a.recip().mix();",
            "feed(a.is_nan() as u64 + 2 * a.is_infinite() as u64 + 4 * a.is_finite() as u64 + 8 * a.is_sign_negative() as u64 + 16 * a.is_normal() as u64);",
            "feed(a.total_cmp(&b) as i8 as u64); feed(a.partial_cmp(&b).map_or(9, |it| it as i8 as u8) as u64); feed((a < b) as u64 + 2 * (a <= b) as u64 + 4 * (a == b) as u64 + 8 * (a != b) as u64);",
            "feed(format!(\"{:?}\", a.classify()).len() as u64); if !a.is_nan() { feed(a.to_bits() as u64); a.signum().mix(); a.copysign(b).mix(); }",
            "if !a.is_nan() && !b.is_nan() && a != b { a.max(b).mix(); a.min(b).mix(); } a.clamp(-1.0, 1.0).mix();",
            "format!(\"{a} {a:?} {a:e} {b:.3} {b:10.2} {a:+}\").mix();",
            "let both = [a, b, a + b, a * 2.0]; both.iter().copied().fold(0.0, |acc, it| if it.is_nan() { acc } else { acc + it }).mix(); both.iter().filter(|it| it.is_finite()).count().mix();",
            f"(a as {'f64' if fty == 'f32' else 'f32'}).mix(); {fty}::from_bits(b.to_bits() ^ 1).mix(); a.to_le_bytes().mix(); feed(a.to_string().parse::<{fty}>().map_or(0, |it| it.is_nan() as u64 + 1));",
            "let (mut acc, mut k) = (a, 0u32); while acc.is_finite() && acc.abs() < 1e30 && k < 40 { acc = acc * 1.5 + 1.0; k += 1; } feed(k as u64);",
        ]
        return [f"let (a, b) = ({a}, {b});"] + self.some(ops, 3, 7)

    # ---- 128-bit integers -----------------------------------------------------------------

    def wide_value(self, ty):
        value = self.r.choice(WIDE)
        if ty == "u128":
            return f"black_box(({value}) as u128)"
        return f"black_box((({value}) as u128 as i128){self.r.choice(['', '.wrapping_neg()', ''])})"

    def sc_wide(self):
        ty = self.r.choice(["u128", "i128"])
        shift = self.r.choice([0, 1, 31, 32, 63, 64, 65, 100, 127])
        ops = [
            "a.wrapping_add(b).mix(); a.wrapping_sub(b).mix(); a.wrapping_mul(b).mix(); a.checked_add(b).mix(); a.checked_mul(b).mix(); a.saturating_sub(b).mix();",
            "a.checked_div(b).mix(); a.checked_rem(b).mix(); a.checked_div_euclid(b).mix(); a.checked_rem_euclid(b).mix(); a.wrapping_div(b | 1).mix(); a.wrapping_rem(b | 1).mix();",
            "{ let (sum, carry) = a.overflowing_add(b); let (product, over) = a.overflowing_mul(b); sum.mix(); product.mix(); feed(carry as u64 + 2 * over as u64); }",
            f"(a << {shift}).mix(); (a >> {shift}).mix(); a.rotate_left({shift}).mix(); a.checked_shl({shift} + 64).mix(); a.wrapping_shr({shift} + 100).mix(); (a >> (b as u32 % 128)).mix();",
            "feed(a.count_ones() as u64 + ((a.leading_zeros() as u64) << 8) + ((a.trailing_zeros() as u64) << 16)); a.swap_bytes().mix(); a.reverse_bits().mix(); (!a).mix();",
            "(a & b).mix(); (a | b).mix(); (a ^ b).mix(); feed((a < b) as u64 + 2 * (a == b) as u64); a.max(b).mix(); a.abs_diff(b).mix(); a.cmp(&b).mix();",
            "a.to_le_bytes().mix(); a.to_be_bytes().mix(); a.to_string().mix(); format!(\"{a:x} {b:#o} {a:>45} {b:e}\").mix();",
            f"{ty}::from_str_radix(&format!(\"{{:x}}\", (a as u128) >> 1), 16).ok().mix(); (a as f64).mix(); (a as f32).mix(); ((a as f64) as {ty}).mix();",
            "(a as u64).mix(); (a as i64).mix(); (a as u8).mix(); ((a >> 64) as u64).mix(); u64::try_from(a).ok().mix(); i32::try_from(b).ok().mix();",
            "a.checked_pow(3).mix(); a.wrapping_pow(b as u32 % 7).mix(); a.checked_neg().mix(); a.checked_next_multiple_of(b | 1).mix();" if ty == "u128" else
            "a.checked_pow(3).mix(); a.wrapping_pow(b as u32 % 7).mix(); a.checked_neg().mix(); a.checked_abs().mix(); a.signum().mix(); a.unsigned_abs().mix();",
            "{ let (hi, lo) = ((a >> 64) as u64, a as u64); let again = ((hi as u128) << 64) | lo as u128; feed((again == a as u128) as u64); ((lo as u128) * (hi as u128)).mix(); }",
            "{ let mut x = a; let mut steps = 0u32; while x != 0 && steps < 200 { x /= 10; steps += 1; } feed(steps as u64); }",
            "{ let items = [a, b, a ^ b, a.wrapping_add(b)]; let mut sorted = items; sorted.sort(); sorted.mix(); items.iter().copied().fold(0, |acc: " + ty + ", it| acc.wrapping_add(it)).mix(); }",
            "a.isqrt().mix(); a.checked_ilog2().mix(); a.checked_ilog10().mix();" if ty == "u128" else "a.checked_isqrt().mix(); a.checked_ilog2().mix(); a.checked_ilog10().mix();",
        ]
        return [f"let (a, b): ({ty}, {ty}) = ({self.wide_value(ty)}, {self.wide_value(ty)});"] + self.some(ops, 3, 8)

    def sc_wide_args(self):
        ty = self.r.choice(["u128", "i128"])
        a, b = self.wide_value(ty), self.wide_value(ty)
        return [
            f"#[inline(never)] fn spread(a: u8, b: {ty}, c: u64, d: {ty}, e: u32, f: {ty}, g: {ty}) -> ({ty}, u8, {ty}) {{ (b.wrapping_add(d) ^ f, a.wrapping_add(c as u8).wrapping_add(e as u8), g.wrapping_sub(b)) }}",
            f"#[inline(never)] extern \"C\" fn c_side(a: {ty}, b: u8, c: {ty}) -> {ty} {{ a.wrapping_mul(3).wrapping_add(c) ^ b as {ty} }}",
            f"#[inline(never)] fn by_ref(a: &{ty}, b: &mut {ty}) -> {ty} {{ *b = b.wrapping_add(*a); *a ^ *b }}",
            f"#[derive(Clone, Copy)] struct Wide {{ tag: u8, value: {ty}, pair: [{ty}; 2] }}",
            f"#[inline(never)] fn pass(w: Wide, extra: Option<{ty}>) -> Wide {{ Wide {{ tag: w.tag.wrapping_add(1), value: w.pair[1] ^ extra.unwrap_or(1), pair: [w.value, w.pair[0]] }} }}",
            f"let (a, b): ({ty}, {ty}) = ({a}, {b});",
            "let got = spread(1, a, 2, b, 3, a ^ b, !a); got.0.mix(); got.1.mix(); got.2.mix();",
            "c_side(a, 9, b).mix(); { let mut m = b; by_ref(&a, &mut m).mix(); m.mix(); }",
            "{ let w = pass(Wide { tag: 7, value: a, pair: [b, a] }, Some(b)); w.tag.mix(); w.value.mix(); w.pair.mix(); pass(w, None).value.mix(); }",
            f"{{ let f: fn({ty}, u8, {ty}) -> {ty} = |x, y, z| x ^ z ^ y as {ty}; f(a, 1, b).mix(); let g: extern \"C\" fn({ty}, u8, {ty}) -> {ty} = c_side; g(b, 2, a).mix(); }}",
        ]

    # ---- niches ---------------------------------------------------------------------------

    def sc_niche(self):
        a, b = self.num(), self.num()
        lines = [
            "#[derive(Clone, Copy, Debug, PartialEq)] enum Small { A, B, C }",
            "#[derive(Clone, Copy, Debug, PartialEq)] enum Outer { One(Small), Two, Three(bool), Four(char) }",
            "#[derive(Debug, PartialEq)] enum Deep { Ref(&'static u8), Boxed(Box<u16>), Nz(NonZeroU32), Nothing, Other(Option<Option<bool>>) }",
            "#[derive(Clone, Copy, Debug)] #[repr(u8)] enum Tagged { X(u8) = 3, Y = 9, Z(bool, u16) = 200 }",
            "enum Never {}",
            "static BYTE: u8 = 7;",
            "fn tell(it: &Outer) -> u64 { match *it { Outer::One(Small::A) => 1, Outer::One(Small::B) => 2, Outer::One(Small::C) => 3, Outer::Two => 4, Outer::Three(flag) => 5 + flag as u64, Outer::Four(c) => 7 + c as u64 } }",
            "fn deep(it: &Deep) -> u64 { match it { Deep::Ref(r) => **r as u64, Deep::Boxed(b) => **b as u64, Deep::Nz(n) => n.get() as u64, Deep::Nothing => 0, Deep::Other(None) => 1, Deep::Other(Some(None)) => 2, Deep::Other(Some(Some(flag))) => 3 + *flag as u64 } }",
        ]
        uses = [
            f"for it in black_box([Outer::One(Small::C), Outer::Two, Outer::Three(true), Outer::Four('{chr(97 + a % 26)}'), Outer::One(Small::A), Outer::Three(false)]) {{ feed(tell(&it)); feed(tell(&Some(it).unwrap_or(Outer::Two))); }}",
            f"for it in black_box([Deep::Ref(&BYTE), Deep::Boxed(Box::new({a})), Deep::Nz(NonZeroU32::new({b}).unwrap()), Deep::Nothing, Deep::Other(None), Deep::Other(Some(None)), Deep::Other(Some(Some(true)))]) {{ feed(deep(&it)); let wrapped = Some(it); feed(wrapped.as_ref().map_or(99, deep)); }}",
            "feed((size_of::<Option<Small>>() + size_of::<Outer>() * 16 + size_of::<Option<Outer>>() * 256 + size_of::<Option<Option<Outer>>>() * 4096) as u64);",
            "feed((size_of::<Deep>() + size_of::<Option<Deep>>() * 64 + size_of::<Option<Box<u8>>>() * 4096 + size_of::<Option<&u8>>() * 65536 + size_of::<Result<Box<u8>, ()>>() * 1048576) as u64);",
            f"{{ let items: [Option<char>; 3] = black_box([None, Some('z'), Some('\\u{{10FFFF}}')]); for it in items {{ feed(match it {{ None => 0, Some(c) => c as u64 }}); }} let nested: Option<Option<Option<u8>>> = black_box(Some(Some(None))); feed(match nested {{ None => 1, Some(None) => 2, Some(Some(None)) => 3, Some(Some(Some(v))) => v as u64 }}); }}",
            f"{{ let items = black_box([Tagged::X({a}), Tagged::Y, Tagged::Z(true, {b})]); for it in items {{ feed(match it {{ Tagged::X(v) => v as u64, Tagged::Y => 1000, Tagged::Z(flag, v) => v as u64 + flag as u64 }}); feed(unsafe {{ *(&it as *const Tagged as *const u8) }} as u64); }} }}",
            "{ let ok: Result<u32, Never> = black_box(Ok(5)); let Ok(value) = ok; feed(value as u64); let none: Option<Never> = None; feed(none.is_none() as u64 + size_of::<Option<Never>>() as u64); }",
            f"{{ let items: Vec<Option<NonZeroU8>> = black_box(vec![NonZeroU8::new(0), NonZeroU8::new({a}), NonZeroU8::new(255)]); for it in &items {{ feed(it.map_or(0, |n| n.get() as u64)); }} feed(size_of::<Option<NonZeroU8>>() as u64); }}",
            "{ let order: [Option<Cmp>; 4] = black_box([None, Some(Cmp::Less), Some(Cmp::Equal), Some(Cmp::Greater)]); for it in order { feed(match it { None => 9, Some(Cmp::Less) => 1, Some(Cmp::Equal) => 2, Some(Cmp::Greater) => 3 }); } }",
            f"{{ let pairs: [Option<(bool, u8)>; 3] = black_box([None, Some((true, {a})), Some((false, 0))]); for it in pairs {{ feed(match it {{ None => 7, Some((flag, v)) => v as u64 * 2 + flag as u64 }}); }} let refs: [Option<&str>; 2] = black_box([None, Some(\"\")]); for it in refs {{ feed(it.map_or(5, str::len) as u64); }} }}",
            f"{{ let mut slot = black_box(Outer::Two); slot = if {a} % 2 == 0 {{ Outer::Three(true) }} else {{ Outer::One(Small::B) }}; feed(tell(&slot)); let mut opt = black_box(Some(slot)); if let Some(Outer::Three(flag)) = &mut opt {{ *flag = false; }} feed(opt.map_or(0, |it| tell(&it))); opt = None; feed(opt.is_some() as u64); }}",
        ]
        return lines + self.some(uses, 3, 7)

    # ---- match arms -----------------------------------------------------------------------

    def element(self, bind=None):
        """A pattern for one element of an array of `Loud`, which is an enum of `I(i32, _)` and `C(char, _)`."""
        letter = self.r.choice(["'a'", "'b'", "'Z'", "'a'..='c'", "'a'..='z'", "'A'..='Z'", "_"])
        number = self.r.choice(["1", "2", "0..=5", "..3", "_", "7"])
        options = ["_", "_", f"Loud::C({letter}, _)", f"Loud::C({letter}, _)", f"Loud::I({number}, _)", "Loud::C(..)", "Loud::I(..)",
                   f"Loud::C({letter}, _) | Loud::I({number}, _)", f"Loud::C('a', _) | Loud::C(_, _)"]
        if bind:
            options += [bind, bind, f"{bind} @ Loud::C(..)", f"{bind} @ (Loud::I(..) | Loud::C('a'..='m', _))"]
        return self.r.choice(options)

    def array_arm(self, index):
        """An arm for `[Loud; 3]` that may move elements out, and may be behind a guard."""
        shape = self.r.choice(["three", "three", "front", "back", "middle", "rest", "all"])
        e = self.element
        pattern = {
            "three": lambda: f"[{e('x')}, {e()}, {e('y')}]",
            "front": lambda: f"[{e('x')}, ..]",
            "back": lambda: f"[.., {e('y')}]",
            "middle": lambda: f"[{e()}, m @ .., {e()}]",
            "rest": lambda: f"[{e()}, m @ ..]",
            "all": lambda: self.r.choice(["all", "[..]", "[m @ .., _]", "_"]),
        }[shape]()
        guard = self.r.choice(["", "", "", " if black_box(flag)", " if black_box(!flag)", " if black_box(false)"])
        return f"{pattern}{guard} => {index},"

    def sc_arms(self):
        lines = [
            "enum Loud { I(i32, Noisy), C(char, Noisy) }",
            "#[inline(never)] fn items(k: u32, last: char) -> [Loud; 3] { black_box([Loud::I(k as i32 % 4, Noisy(k)), Loud::C('q', Noisy(k + 1)), if k % 3 == 0 { Loud::I(1, Noisy(k + 2)) } else { Loud::C(last, Noisy(k + 2)) }]) }",
        ]
        for which in range(self.r.randint(1, 3)):
            arms = " ".join(self.array_arm(i + 1) for i in range(self.r.randint(2, 5)))
            lines.append(f"#[inline(never)] fn pick{which}(k: u32, last: char, flag: bool) -> u8 {{ match items(k, last) {{ {arms} _ => 0 }} }}")
            lines.append(f"for k in 0..4u32 {{ for last in ['a', 'Z', 'q'] {{ for flag in [false, true] {{ feed(pick{which}(k, last, flag) as u64); feed(0xf00d); }} }} }}")
        if self.r.random() < 0.6:
            first, second = self.element(), self.element()
            number = self.r.choice(["1", "2", "_", "0..=1", "3"])
            lines.append(f"#[inline(never)] fn pair(k: u32, last: char, flag: bool) -> u8 {{ match (items(k, last), k % 3, flag) {{ ([{first}, ..], {number}, true) => 1, "
                         f"([.., {second}], {self.r.choice(['1', '_', '2'])}, _) => 2, ([x, m @ ..], {number}, false) => 3, (all, _, _) if black_box(flag) => 4, _ => 0 }} }}")
            lines.append("for k in 0..4u32 { for last in ['a', 'Z'] { for flag in [false, true] { feed(pair(k, last, flag) as u64); feed(0xf00d); } } }")
        return lines

    # ---- statics --------------------------------------------------------------------------

    def sc_statics(self):
        a, b, c = self.num(), self.num(), self.num()
        lines = [
            f"static NUMBERS: [u32; 4] = [{a}, {b}, {c}, 4];",
            "static SECOND: &u32 = &NUMBERS[1];",
            "static TAIL: &[u32] = match &NUMBERS { [_, _, rest @ ..] => rest };",
            "static WORDS: [&str; 3] = [\"one\", \"two\", \"three\"];",
            "static PICKED: &&str = &WORDS[2];",
            "struct Link { value: u32, next: Option<&'static Link>, call: fn(u32) -> u32, name: &'static str, shown: &'static (dyn Debug + Sync) }",
            "fn double(x: u32) -> u32 { x.wrapping_mul(2) }",
            "static END: Link = Link { value: 3, next: None, call: double, name: \"end\", shown: &NUMBERS };",
            f"static MID: Link = Link {{ value: {a}, next: Some(&END), call: |x| x + 1, name: WORDS[0], shown: &\"mid\" }};",
            f"static HEAD: Link = Link {{ value: {b}, next: Some(&MID), call: double, name: \"head\", shown: &MID.value }};",
            "static TABLE: [fn(u32) -> u32; 3] = [double, |x| x ^ 5, HEAD.call];",
            "static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);",
            f"static LISTS: (&[u8], &[&[u8]], [Option<&u32>; 2]) = (b\"bytes\", &[b\"a\", b\"\", &[{a % 256}, 2]], [Some(&NUMBERS[3]), None]);",
            "const VIEW: &[u32; 4] = &NUMBERS;",
            "thread_local! { static LOCAL: Cell<u32> = const { Cell::new(5) }; static NOTES: RefCell<Vec<&'static u32>> = const { RefCell::new(Vec::new()) }; }",
        ]
        uses = [
            "feed(*SECOND as u64 + TAIL.len() as u64 + std::ptr::eq(SECOND, &NUMBERS[1]) as u64 * 16 + std::ptr::eq(TAIL.as_ptr(), &NUMBERS[2]) as u64 * 32);",
            "PICKED.mix(); feed(std::ptr::eq(*PICKED, WORDS[2]) as u64);",
            "{ let mut at = Some(&HEAD); while let Some(link) = at { feed((link.call)(link.value) as u64); link.name.mix(); format!(\"{:?}\", link.shown).mix(); at = link.next; } }",
            "for (i, f) in TABLE.iter().enumerate() { feed(f(i as u32 + 10) as u64); }",
            "feed(std::ptr::eq(HEAD.next.unwrap(), &MID) as u64 + std::ptr::eq(MID.next.unwrap(), &END) as u64 * 2 + std::ptr::eq(VIEW, &NUMBERS) as u64 * 4);",
            "{ let before = COUNTER.fetch_add(3, Ordering::Relaxed); feed((COUNTER.load(Ordering::Relaxed) - before) as u64); }",
            "LISTS.0.mix(); for part in LISTS.1 { part.mix(); } feed(LISTS.2[0].map_or(0, |it| *it) as u64 + LISTS.2[1].is_none() as u64);",
            "LOCAL.with(|it| { it.set(it.get() + 1); feed(it.get() as u64 % 2 + 40); }); NOTES.with(|it| { it.borrow_mut().push(SECOND); feed(*it.borrow()[0] as u64); });",
        ]
        return lines + self.some(uses, 3, 7)

    # ---- representations ------------------------------------------------------------------

    def sc_repr(self):
        a, b = self.num(), self.num()
        align = self.r.choice([2, 4, 8, 16])
        pack = self.r.choice(["packed", "packed(2)", "packed(4)"])
        lines = [
            f"#[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(C)] struct Plain {{ a: u8, b: u32, c: u16, d: {self.r.choice(['u64', 'f64', 'i128', 'u8'])} }}",
            f"#[derive(Clone, Copy, Default)] #[repr(C, {pack})] struct Tight {{ a: u8, b: u64, c: u16, d: u128 }}",
            f"#[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(C, align({align}))] struct Big {{ a: u8, b: [u16; 3] }}",
            f"#[derive(Clone, Copy, Debug, Default, PartialEq)] #[repr(align({align}))] struct Rusty(u8, u32);",
            "#[derive(Clone, Copy, Debug, PartialEq)] #[repr(transparent)] struct Wrap(f32);",
            "#[derive(Clone, Copy, Debug, PartialEq)] #[repr(i8)] enum Signed { Low = -128, Mid = -1, High = 127 }",
            "#[derive(Clone, Copy, Debug, PartialEq)] #[repr(u16)] enum Wider { A = 1, B = 0x1234, C = 0xffff }",
            "#[derive(Clone, Copy, Debug, PartialEq)] #[repr(C)] enum Cee { Unit, One(u8), Two { x: u16, y: u64 } }",
            "#[derive(Clone, Copy, Debug, PartialEq)] #[repr(C, u8)] enum Both { P(u32), Q(u8, u8), R }",
            "#[derive(Clone, Copy)] #[repr(C)] union Bits { word: u32, bytes: [u8; 4], half: (u16, u16) }",
            "#[inline(never)] extern \"C\" fn c_plain(p: Plain, t: Tight, w: Wrap) -> Plain { Plain { a: p.a.wrapping_add(t.a), b: p.b ^ { t.c } as u32, c: p.c, d: p.d } }",
            "#[inline(never)] extern \"C\" fn c_big(x: u8, big: Big, y: u8, rusty: Rusty) -> Big { Big { a: big.a ^ x ^ y ^ rusty.0, b: [big.b[2], big.b[0], rusty.1 as u16] } }",
            "#[inline(never)] fn r_enums(s: Signed, w: Wider, c: Cee, b: Both) -> (i8, u16, Cee, Both) { (s as i8, w as u16, match c { Cee::One(v) => Cee::Two { x: v as u16, y: 1 }, other => other }, match b { Both::Q(x, y) => Both::Q(y, x), other => other }) }",
        ]
        uses = [
            f"{{ let p = c_plain(black_box(Plain {{ a: {a % 256}, b: {b}, c: 3, ..Default::default() }}), black_box(Tight {{ a: 1, b: 2, c: {a}, d: 4 }}), Wrap(1.5)); feed(p.a as u64 + p.b as u64 * 256 + p.c as u64 * 65536); }}",
            f"{{ let big = c_big({a % 256}, black_box(Big {{ a: 1, b: [2, 3, 4] }}), {b % 256}, black_box(Rusty(5, 6))); big.a.mix(); big.b.mix(); feed((&big as *const Big as usize % {align}) as u64); }}",
            f"{{ let mut t = black_box(Tight {{ a: {a % 256}, b: {b}, c: 7, d: u128::MAX }}); t.b += 1; t.d = t.d.wrapping_add({a}); let (b, c, d) = (t.b, t.c, t.d); feed(b + c as u64); d.mix(); feed(unsafe {{ std::ptr::addr_of!(t.d).read_unaligned() }} as u64); }}",
            "feed((size_of::<Plain>() + align_of::<Plain>() * 256 + size_of::<Tight>() * 4096 + align_of::<Tight>() * 65536) as u64); feed((size_of::<Big>() + align_of::<Big>() * 256 + size_of::<Rusty>() * 4096 + size_of::<Cee>() * 65536 + size_of::<Both>() * 1048576) as u64);",
            "feed((std::mem::offset_of!(Plain, b) + std::mem::offset_of!(Plain, c) * 16 + std::mem::offset_of!(Plain, d) * 256 + std::mem::offset_of!(Tight, d) * 4096 + std::mem::offset_of!(Big, b) * 65536) as u64);",
            f"{{ let got = r_enums(black_box(Signed::Low), black_box(Wider::B), black_box(Cee::One({a % 256})), black_box(Both::Q(1, 2))); feed(got.0 as u8 as u64 + got.1 as u64 * 256); format!(\"{{:?}} {{:?}}\", got.2, got.3).mix(); }}",
            "{ for s in black_box([Signed::Low, Signed::Mid, Signed::High]) { feed((s as i8 as i64 + 200) as u64); feed((s as i32 == -1) as u64); } for w in black_box([Wider::A, Wider::B, Wider::C]) { feed(w as u64); } }",
            f"{{ let mut bits = Bits {{ word: black_box({a}u32.wrapping_mul(0x01010101)) }}; unsafe {{ bits.bytes[0] ^= 0xff; feed(bits.word.to_le() as u64); feed(u32::from_le_bytes(bits.bytes) as u64); bits.half = (1, 2); feed(u32::from_le(bits.word) as u64); }} }}",
            "{ let w = black_box(Wrap(2.5)); let raw: f32 = unsafe { std::mem::transmute(w) }; raw.mix(); let items = black_box([Rusty(1, 2), Rusty(3, 4)]); feed(items[1].1 as u64 + size_of_val(&items) as u64 * 16); }",
            "{ let both = black_box([Both::P(7), Both::Q(1, 2), Both::R]); for it in both { feed(unsafe { *(&it as *const Both as *const u8) } as u64); feed(match it { Both::P(v) => v as u64, Both::Q(x, y) => (x + y) as u64, Both::R => 99 }); } }",
        ]
        return lines + self.some(uses, 3, 7)

    # ---- inline assembly ------------------------------------------------------------------

    def sc_asm(self):
        a, b = self.num(), self.num()
        forms = [
            ("add", "\"add {x}, {y}\", x = inout(reg) x, y = in(reg) y", "\"add {x}, {x}, {y}\", x = inout(reg) x, y = in(reg) y"),
            ("sub", "\"sub {x}, {y}\", x = inout(reg) x, y = in(reg) y", "\"sub {x}, {x}, {y}\", x = inout(reg) x, y = in(reg) y"),
            ("xor", "\"xor {x}, {y}\", x = inout(reg) x, y = in(reg) y", "\"eor {x}, {x}, {y}\", x = inout(reg) x, y = in(reg) y"),
            ("mul3", "\"lea {x}, [{x} + {x} * 2]\", x = inout(reg) x", "\"add {x}, {x}, {x}, lsl #1\", x = inout(reg) x"),
            ("neg", "\"neg {x}\", \"not {y}\", \"add {x}, {y}\", x = inout(reg) x, y = inout(reg) y => _",
             "\"neg {x}, {x}\", \"mvn {y}, {y}\", \"add {x}, {x}, {y}\", x = inout(reg) x, y = inout(reg) y => _"),
            ("swap", "\"mov {t}, {x}\", \"mov {x}, {y}\", \"add {x}, {t}\", x = inout(reg) x, y = in(reg) y, t = out(reg) _",
             "\"mov {t}, {x}\", \"mov {x}, {y}\", \"add {x}, {x}, {t}\", x = inout(reg) x, y = in(reg) y, t = out(reg) _"),
        ]
        name, x86, arm = self.r.choice(forms)
        options = self.r.choice(["", ", options(pure, nomem, nostack)", ", options(nomem, nostack)", ", options(nostack)"])
        return [
            f"#[cfg(target_arch = \"x86_64\")] #[inline(never)] fn {name}(mut x: u64, mut y: u64) -> u64 {{ unsafe {{ std::arch::asm!({x86}{options}); }} x }}",
            f"#[cfg(target_arch = \"aarch64\")] #[inline(never)] fn {name}(mut x: u64, mut y: u64) -> u64 {{ unsafe {{ std::arch::asm!({arm}{options}); }} x }}",
            f"for (x, y) in [({a}u64, {b}u64), (u64::MAX, 1), (0, 0), (0x8000_0000_0000_0000, {a})] {{ let _g = Noisy(x as u32 % 7); feed({name}(black_box(x), black_box(y))); }}",
        ]
