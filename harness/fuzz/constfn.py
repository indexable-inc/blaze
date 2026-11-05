"""constfn.py: generated `const fn`s, each evaluated as a constant and called at run time.

A case of gen.py gets a few of them (#862). For each one the case has a constant that calls
it, which the compiler evaluates (blaze: its MIR interpreter), and `run` calls it again on the
same arguments through `black_box`, which is compiled code. Both results go into the
checksum, so a build whose interpreter or code generator an edit or the store reached wrong
prints another than the build it is held to.

What a body may do is what a `const fn` may on the pinned nightly without a feature: integer
and float arithmetic, `as`, `while` and `loop` with a counter, `if`, `match`, arrays indexed
in bounds, tuples, `&mut` to its own locals, calls to the const fns before it, and const
generic parameters. Nothing can panic (division and remainder are checked, shifts wrap,
indices are taken modulo the length), every loop runs at most `LOOP` times, and no NaN's bits
are observed: a NaN is mixed in as one value, and `copysign` never takes the sign of one, as
rustc's const eval and the hardware may give it a different payload and sign.
"""
import random

INTS = {"u8": (8, False), "u16": (16, False), "u32": (32, False), "u64": (64, False), "u128": (128, False),
        "usize": (64, False), "i8": (8, True), "i32": (32, True), "i64": (64, True), "i128": (128, True)}
FLOATS = ["f32", "f64"]
LOOP = 12
DEPTH = 3


class ConstFns:
    """The const fns of one case, from their own random numbers."""

    def __init__(self, key):
        self.r = random.Random(f"{key}/constfn")
        self.fns = []  # (name, [(param, ty)], ret, generic length or None)
        self.count = 0
        self.lines = []
        self.ind = 1
        self.looping = False

    def fresh(self, prefix):
        self.count += 1
        return f"{prefix}{self.count}"

    def emit(self, text):
        self.lines.append("    " * self.ind + text)

    # ---- values ---------------------------------------------------------------------------

    def int_lit(self, ty):
        bits, signed = INTS[ty]
        r = self.r
        lo, hi = (-(1 << (bits - 1)), (1 << (bits - 1)) - 1) if signed else (0, (1 << bits) - 1)
        value = r.choice([0, 1, 2, 3, 7, 100, lo, hi, lo + 1, hi - 1, r.randint(lo, hi), r.randint(-50, 50)])
        value = max(lo, min(hi, value))
        return f"({value}{ty})" if value < 0 else f"{value}{ty}"

    def float_lit(self, ty):
        text = self.r.choice(["0.0", "1.0", "-1.0", "0.5", "2.5", "-7.25", "1e10", "1e-7", "3.0", "-0.0", "100.125"])
        return f"({text}{ty})" if text.startswith("-") else f"{text}{ty}"

    def lit(self, ty):
        if ty == "bool":
            return self.r.choice(["true", "false"])
        if ty in FLOATS:
            return self.float_lit(ty)
        return self.int_lit(ty)

    def pick(self, scope, ty):
        names = [name for name, t in scope if t == ty]
        return self.r.choice(names) if names and self.r.random() < 0.7 else None

    def expr(self, scope, ty, depth=0):
        """An expression of type `ty` that cannot panic."""
        r = self.r
        if depth >= DEPTH or r.random() < 0.25:
            return self.pick(scope, ty) or self.lit(ty)
        sub = lambda t: self.expr(scope, t, depth + 1)
        if ty == "bool":
            other = r.choice(list(INTS))
            options = [
                lambda: f"({sub(other)} {r.choice(['<', '<=', '==', '!=', '>', '>='])} {sub(other)})",
                lambda: f"({sub('bool')} {r.choice(['&&', '||', '^', '&'])} {sub('bool')})",
                lambda: f"(!{sub('bool')})",
                lambda: f"{sub(other)}.{'is_negative' if INTS[other][1] else 'is_power_of_two'}()",
            ]
        elif ty in FLOATS:
            options = [
                lambda: f"({sub(ty)} {r.choice(['+', '-', '*', '/', '%'])} {sub(ty)})",
                lambda: f"({sub(r.choice(list(INTS)))} as {ty})",
                lambda: f"(-{sub(ty)})",
                lambda: f"{sub(ty)}.abs()",
                lambda: f"(if {sub('bool')} {{ {sub(ty)} }} else {{ {sub(ty)} }})",
                lambda: f"{sub(ty)}.copysign({self.not_nan(sub(ty), ty)})",
            ]
        else:
            signed = INTS[ty][1]
            options = [
                lambda: f"{sub(ty)}.wrapping_{r.choice(['add', 'sub', 'mul'])}({sub(ty)})",
                lambda: f"({sub(ty)} {r.choice(['^', '&', '|'])} {sub(ty)})",
                lambda: f"{sub(ty)}.{r.choice(['wrapping_shl', 'wrapping_shr', 'rotate_left', 'rotate_right'])}({sub('u32')})",
                lambda: f"({sub(r.choice(list(INTS) + FLOATS + ['bool']))} as {ty})",
                lambda: f"({sub(ty)}.{r.choice(['count_ones', 'leading_zeros', 'trailing_zeros'])}() as {ty})",
                lambda: f"(match {sub(ty)}.checked_{r.choice(['add', 'sub', 'mul', 'div', 'rem'])}({sub(ty)}) {{ Some(v) => v, None => {sub(ty)} }})",
                lambda: f"{sub(ty)}.saturating_{r.choice(['add', 'sub', 'mul'])}({sub(ty)})",
                lambda: f"{sub(ty)}.{r.choice(['swap_bytes', 'reverse_bits', 'wrapping_neg'])}()",
                lambda: f"(if {sub('bool')} {{ {sub(ty)} }} else {{ {sub(ty)} }})",
                lambda: f"{sub(ty)}.wrapping_pow({r.randrange(6)})",
                lambda: f"(match {sub(ty)}{'.rem_euclid(4)' if signed else ' % 4'} {{ 0 => {sub(ty)}, 1 => {sub(ty)}, _ => {self.lit(ty)} }})",
            ]
            if self.fns and not self.looping:
                options.append(lambda: self.call(scope, ty, depth))
        return r.choice(options)()

    def not_nan(self, value, ty):
        """`value`, or 1 where it is a NaN: a NaN's sign is not what a correct compiler must agree on."""
        return f"{{ let x: {ty} = {value}; if x.is_nan() {{ 1.0 }} else {{ x }} }}"

    def call(self, scope, ty, depth):
        fits = [f for f in self.fns if f[2] == ty]
        if not fits:
            return self.lit(ty)
        name, params, _, length = self.r.choice(fits)
        args = [self.array(scope, length, depth + 1) if t == "array" else self.expr(scope, t, depth + 1) for _, t in params]
        generic = f"::<{length}>" if length is not None else ""
        return f"{name}{generic}({', '.join(args)})"

    def array(self, scope, length, depth):
        return "[" + ", ".join(self.expr(scope, "u32", depth) for _ in range(length)) + "]"

    # ---- bodies ---------------------------------------------------------------------------

    def statements(self, scope, count, depth=0):
        r = self.r
        for _ in range(count):
            kind = r.choice(["let", "let", "assign", "while", "if", "match", "array", "mutref", "loop"])
            muts = [(n, t) for n, t in scope if n.startswith("m")]
            if depth >= 2 and kind in ("while", "loop", "if", "match"):
                kind = "assign"
            ty = r.choice(list(INTS) + FLOATS + ["bool"])
            if kind == "let" or (kind in ("assign", "mutref") and not muts):
                name = self.fresh("m")
                self.emit(f"let mut {name}: {ty} = {self.expr(scope, ty)};")
                scope.append((name, ty))
            elif kind == "assign":
                name, t = r.choice(muts)
                self.emit(f"{name} = {self.expr(scope, t)};")
            elif kind == "mutref":
                name, t = r.choice(muts)
                ref = self.fresh("r")
                self.emit(f"let {ref} = &mut {name};")
                rest = [(n, u) for n, u in scope if n != name] + [(f"(*{ref})", t)]
                self.emit(f"*{ref} = {self.expr(rest, t)};")
            elif kind in ("while", "loop"):
                counter = self.fresh("i")
                bound = r.randint(0, LOOP)
                self.emit(f"let mut {counter}: u32 = 0;")
                if kind == "while":
                    self.emit(f"while {counter} < {bound} {{")
                else:
                    self.emit("loop {")
                    self.ind += 1
                    self.emit(f"if {counter} >= {bound} {{ break; }}")
                    self.ind -= 1
                self.ind += 1
                self.emit(f"{counter} += 1;")
                inner = scope + [(counter, "u32")]
                outer, self.looping = self.looping, True
                self.statements(inner, r.randint(1, 3), depth + 1)
                if r.random() < 0.3:
                    self.emit(f"if {self.expr(inner, 'bool')} {{ {'break' if r.random() < 0.5 else 'continue'}; }}")
                self.looping = outer
                self.ind -= 1
                self.emit("}")
            elif kind == "if":
                self.emit(f"if {self.expr(scope, 'bool')} {{")
                self.ind += 1
                self.statements(list(scope), r.randint(1, 2), depth + 1)
                self.ind -= 1
                self.emit("} else {")
                self.ind += 1
                self.statements(list(scope), r.randint(0, 2), depth + 1)
                self.ind -= 1
                self.emit("}")
            elif kind == "match":
                t = r.choice(["u8", "u32", "i32", "u64"])
                self.emit(f"match {self.expr(scope, t)} {{")
                self.ind += 1
                for arm in ["0", "1..=9", f"{self.lit(t)}"][: r.randint(1, 3)]:
                    self.emit(f"{arm} => {{")
                    self.ind += 1
                    self.statements(list(scope), 1, depth + 1)
                    self.ind -= 1
                    self.emit("}")
                self.emit("_ => {}")
                self.ind -= 1
                self.emit("}")
            elif kind == "array":
                name, length = self.fresh("a"), r.randint(1, 8)
                self.emit(f"let mut {name}: [u32; {length}] = {self.array(scope, length, 1)};")
                at = self.expr(scope, "usize")
                self.emit(f"{name}[({at}) % {length}] = {self.expr(scope, 'u32')};")
                value = self.fresh("m")
                self.emit(f"let mut {value}: u32 = {name}[({self.expr(scope, 'usize')}) % {length}];")
                scope.append((value, "u32"))

    def make_fn(self):
        r = self.r
        name = self.fresh("cf")
        length = r.choice([None, None, None, r.randint(1, 5)])
        params = [(self.fresh("p"), r.choice(list(INTS) + FLOATS + ["bool"])) for _ in range(r.randint(0, 3))]
        if length is not None:
            params.append((self.fresh("p"), "array"))
        ret = r.choice(list(INTS) + FLOATS + ["bool"])
        shown = [f"{p}: {'[u32; N]' if t == 'array' else t}" for p, t in params]
        generic = "<const N: usize>" if length is not None else ""
        self.emit(f"pub const fn {name}{generic}({', '.join(shown)}) -> {ret} {{")
        self.ind += 1
        scope = [(p, t) for p, t in params if t != "array"]
        for p, t in params:
            if t == "array":
                total = self.fresh("m")
                index = self.fresh("i")
                self.emit(f"let mut {total}: u32 = N as u32;")
                self.emit(f"let mut {index}: usize = 0;")
                self.emit(f"while {index} < N {{ {total} = {total}.rotate_left(5) ^ {p}[{index}]; {index} += 1; }}")
                scope.append((total, "u32"))
        self.statements(scope, r.randint(2, 7))
        state = self.fresh("s")
        self.emit(f"let mut {state}: u64 = 0;")
        for value, ty in scope:
            self.emit(f"{state} = {state}.rotate_left(7) ^ {self.mixed(value, ty)};")
        result = self.expr(scope, ret)
        if ret == "bool":
            self.emit(f"{result} ^ ({state} & 1 == 1)")
        elif ret in FLOATS:
            self.emit(f"{result} + ({state} as {ret})")
        else:
            self.emit(f"{result} ^ ({state} as {ret})")
        self.ind -= 1
        self.emit("}")
        self.fns.append((name, params, ret, length))

    def mixed(self, value, ty):
        """`value` as what goes into the checksum: a NaN as one value, whatever its bits."""
        if ty in FLOATS:
            return f"(if {value}.is_nan() {{ 1u64 }} else {{ {value}.to_bits() as u64 }})"
        if ty == "bool":
            return f"({value} as u64)"
        if ty in ("u128", "i128"):
            return f"(({value} as u128) as u64 ^ (({value} as u128) >> 64) as u64)"
        return f"({value} as u64)"

    def case(self):
        """The const fns, a constant of each, and `const_fns()`, which `run` calls first."""
        for _ in range(self.r.randint(1, 4)):
            self.make_fn()
        checks = []
        for name, params, ret, length in self.fns:
            args = [self.array([], length, 1) if t == "array" else self.lit(t) for _, t in params]
            generic = f"::<{length}>" if length is not None else ""
            konst = self.fresh("CF")
            self.emit(f"const {konst}: {ret} = {name}{generic}({', '.join(args)});")
            hidden = [f"black_box({a})" for a in args]
            checks.append((konst, f"{name}{generic}({', '.join(hidden)})", ret))
        self.emit("#[inline(never)]")
        self.emit("fn const_fns() {")
        self.ind += 1
        for konst, call, ret in checks:
            self.emit(f"let ran: {ret} = {call};")
            self.emit(f"feed({self.mixed(konst, ret)});")
            self.emit(f"feed({self.mixed('ran', ret)});")
        self.ind -= 1
        self.emit("}")
        return self.lines
