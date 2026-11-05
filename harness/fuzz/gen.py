#!/usr/bin/env python3
"""gen.py: random Rust programs that are valid, deterministic and terminate.

usage: gen.py SEED [CASES] > main.rs

A program is `prelude.rs` followed by one module per case. A case is a set of types, traits,
helper functions and a `run` function; everything a case computes goes into one checksum,
which the program prints per case. The source of case N of seed S depends on (S, N) only, so
a case can be made again on its own.

What keeps a program free of undefined behaviour and of anything that differs between two
correct compilers:
  - arithmetic is wrapping, checked or saturating, except inside `catch_unwind`, where an
    overflow, a division by zero or an index out of bounds is a panic that is caught;
  - no address, no bit pattern of a NaN, no sign of `min(-0.0, 0.0)` and no hash-map order is
    ever observed;
  - raw pointers stay inside the object they came from and only read what was written;
  - every loop has a counter the generated code cannot assign, calls only go to functions
    defined earlier, recursion counts down, and the work of a case is bounded by an estimate.
"""
import contextlib, os, random, sys
from collections import Counter

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from constfn import ConstFns
from scenes import Scenes

HERE = os.path.dirname(os.path.abspath(__file__))
INTS = {"i8": (8, True), "i16": (16, True), "i32": (32, True), "i64": (64, True), "i128": (128, True), "isize": (64, True),
        "u8": (8, False), "u16": (16, False), "u32": (32, False), "u64": (64, False), "u128": (128, False), "usize": (64, False)}
NONZERO = {"NonZeroU8": "u8", "NonZeroU32": "u32", "NonZeroI64": "i64"}
CHARS = ["'a'", "'Z'", "'0'", "' '", "'\\n'", "'\\0'", "'\\''", "'\\\\'", "'é'", "'ß'", "'中'", "'😀'",
         "'\\u{10FFFF}'", "'\\u{7f}'", "'\\u{80}'", "'\\u{d7ff}'", "'\\u{e000}'", "'9'", "'z'", "'_'"]
STRS = ['""', '"a"', '"hello"', '"héllo"', '"x y"', '"0123456789"', '"line\\n"', '"😀"', '"Abc Def"', '"42"', '"-7"', '"3.5"',
        '"ff"', '"a,b,,c"', '"  pad  "']
FLOATS = ["0.0", "1.0", "-1.0", "0.5", "-0.0", "1.5", "3.25", "1e10", "-2.5e-3", "16777217.0", "4294967296.0",
          "-9223372036854775808.0", "1e-40", "255.5", "-128.9", "65535.0", "1e30", "0.1", "0.2", "7.0"]
FLOAT_TEXTS = ['"2.5"', '"1e3"', '"inf"', '"-0.125"', '"x"', '"1e-320"', '"0.1"', '"340282350000000000000000000000000000000"']
FLOAT_CONSTS = ["MAX", "MIN", "MIN_POSITIVE", "EPSILON", "INFINITY", "NEG_INFINITY", "NAN"]
# The most a case may do, in rough units of one statement.
WORK = 60000


class Ty:
    """A type of the generated program and what the generator may do with a value of it."""

    def __init__(self, kind, name, args=(), copy=True, total=True, heap=False, const=True, **more):
        self.kind, self.name, self.args = kind, name, tuple(args)
        # `total`: has Eq, Ord and Hash. `heap`: can grow without bound. `const`: has a constant literal.
        self.copy, self.total, self.heap, self.const = copy, total, heap, const
        self.leaves = more.pop("leaves", 1)
        self.pod = more.pop("pod", copy)
        self.cabi = more.pop("cabi", False)
        self.__dict__.update(more)

    def __repr__(self):
        return self.name


class Adt:
    """A struct or an enum definition, with the type parameters it is generic over."""

    def __init__(self, kind, name):
        self.kind, self.name = kind, name
        self.params, self.fields, self.variants = [], [], []
        self.tuple = self.packed = self.drop = self.clike = False
        # Whether the definition derives `Copy`, and `Eq`, `Ord` and `Hash`.
        self.copy = self.total = False
        self.repr = ""


class Var:
    """Something in scope: how to name it, its type, and whether it can be assigned."""

    def __init__(self, expr, ty, mut=False, blk=0, loops=0, bounded=False, owned=False):
        self.expr, self.ty, self.mut, self.blk, self.loops = expr, ty, mut, blk, loops
        # `bounded`: a place reached through `&mut`, whose owner may be alive across many calls.
        # `owned`: `expr` makes a new value each time, so it needs no clone.
        self.bounded, self.owned = bounded, owned
        # Read through a `&mut` that is not `Copy`, which a `move` closure would take.
        self.unique = bounded and mut
        self.hid = 0


class Fn:
    """Something that can be called: a function, a closure or a function pointer."""

    def __init__(self, name, params, ret, cost=1, **more):
        # A parameter is (mode, type): "val", "ref", "mut", "slice", "str", "dyn", "depth".
        self.name, self.params, self.ret, self.cost = name, params, ret, cost
        self.tparams = more.get("tparams", [])
        self.panics = more.get("panics", False)
        self.abi = more.get("abi", "")
        self.curried = more.get("curried")
        self.fnmut = more.get("fnmut", False)
        self.pointer = more.get("pointer", True)


class Types:
    """Builds and interns the types of one case."""

    def __init__(self):
        self.known = {}
        self.unit = self.intern(Ty("unit", "()", cabi=False))
        self.bool = self.intern(Ty("bool", "bool", cabi=True))
        self.char = self.intern(Ty("char", "char"))
        self.sstr = self.intern(Ty("sstr", "&'static str", pod=False))
        self.string = self.intern(Ty("string", "String", copy=False, heap=True, const=False))
        self.ints = {n: self.intern(Ty("int", n, cabi=n not in ("i128", "u128"))) for n in INTS}
        self.floats = {n: self.intern(Ty("float", n, total=False, cabi=True)) for n in ("f32", "f64")}
        self.nonzero = {n: self.intern(Ty("nz", n, pod=False, base=self.ints[b])) for n, b in NONZERO.items()}
        self.usize, self.u32, self.u8, self.u64 = (self.ints[n] for n in ("usize", "u32", "u8", "u64"))

    def intern(self, ty):
        return self.known.setdefault(ty.name, ty)

    def composite(self, kind, name, args, **more):
        if name in self.known:
            return self.known[name]
        flags = dict(copy=all(a.copy for a in args), total=all(a.total for a in args), heap=any(a.heap for a in args),
                     const=all(a.const for a in args), pod=all(a.pod for a in args), leaves=sum(a.leaves for a in args))
        flags.update(more)
        return self.intern(Ty(kind, name, args, **flags))

    def tuple(self, args):
        name = "(" + ", ".join(a.name for a in args) + ("," if len(args) == 1 else "") + ")"
        return self.composite("tuple", name, args)

    def array(self, elem, n):
        return self.composite("array", f"[{elem.name}; {n}]", [elem], n=n, leaves=elem.leaves * n)

    def option(self, ty):
        return self.composite("option", f"Option<{ty.name}>", [ty], pod=False)

    def result(self, ok, err):
        return self.composite("result", f"Result<{ok.name}, {err.name}>", [ok, err], pod=False)

    def boxed(self, ty):
        return self.composite("box", f"Box<{ty.name}>", [ty], copy=False, const=False, pod=False)

    def vec(self, ty):
        return self.composite("vec", f"Vec<{ty.name}>", [ty], copy=False, const=False, heap=True, pod=False)

    def param(self, name):
        return self.intern(Ty("param", name, copy=False, total=False, const=False, pod=False))

    def adt(self, d, args=()):
        """The type of a definition at the given type arguments."""
        name = d.name + ("<" + ", ".join(a.name for a in args) + ">" if args else "")
        if name in self.known:
            return self.known[name]
        bound = dict(zip((p.name for p in d.params), args))
        if d.kind == "struct":
            fields = [(n, self.subst(t, bound)) for n, t in d.fields]
            parts, variants = [t for _, t in fields], None
        else:
            variants = [(v, shape, [(n, self.subst(t, bound)) for n, t in fs]) for v, shape, fs in d.variants]
            fields, parts = None, [t for _, _, fs in variants for _, t in fs]
        plain = not d.drop
        ty = Ty(d.kind, name, args, copy=d.copy and all(t.copy for t in parts), total=d.total and all(t.total for t in parts),
                heap=any(t.heap for t in parts), const=plain and all(t.const for t in parts),
                pod=plain and d.kind == "struct" and "align" not in d.repr and all(t.pod for t in parts), leaves=max(1, sum(t.leaves for t in parts)),
                cabi=d.kind == "struct" and d.repr == "#[repr(C)]" and bool(parts) and all(t.cabi for t in parts),
                d=d, fields=fields, variants=variants)
        return self.intern(ty)

    def could(self, ty, flag):
        """Whether a field type has the property once its type parameters do: what a derive needs."""
        if ty.kind == "param":
            return True
        if not ty.args or (flag == "copy" and ty.kind in ("box", "vec")):
            return getattr(ty, flag)
        if ty.kind in ("struct", "enum") and not getattr(ty.d, flag):
            return False
        return all(self.could(a, flag) for a in ty.args)

    def subst(self, ty, bound):
        """`ty` with type parameters replaced by what `bound` gives them."""
        if not bound:
            return ty
        if ty.kind == "param":
            return bound.get(ty.name, ty)
        if not ty.args:
            return ty
        args = [self.subst(a, bound) for a in ty.args]
        if all(a is b for a, b in zip(args, ty.args)):
            return ty
        return self.rebuild(ty, args)

    def rebuild(self, ty, args):
        if ty.kind in ("struct", "enum"):
            return self.adt(ty.d, args)
        if ty.kind == "array":
            return self.array(args[0], ty.n)
        make = {"tuple": self.tuple, "option": lambda a: self.option(*a), "result": lambda a: self.result(*a),
                "box": lambda a: self.boxed(*a), "vec": lambda a: self.vec(*a)}[ty.kind]
        return make(args)

    def unify(self, pattern, ty, bound):
        """Whether `ty` is `pattern` for some types of its parameters, which are added to `bound`."""
        if pattern.kind == "param":
            return bound.setdefault(pattern.name, ty) is ty
        if pattern.kind != ty.kind or len(pattern.args) != len(ty.args):
            return False
        if not pattern.args:
            return pattern is ty
        if pattern.kind in ("struct", "enum") and pattern.d is not ty.d:
            return False
        if pattern.kind == "array" and pattern.n != ty.n:
            return False
        return all(self.unify(p, t, bound) for p, t in zip(pattern.args, ty.args))


class Gen(Scenes):
    """Generates one case. Methods named `x_KIND` list the ways to make a value of that kind."""

    def __init__(self, seed, index):
        self.key = f"{seed}/{index}"
        self.r = random.Random(self.key)
        self.name = f"c{index}"
        self.t = Types()
        self.cov = Counter()
        self.lines, self.ind = [], 1
        self.scope, self.mut_floor = [], 0
        self.fns, self.traits, self.adts, self.pool = [], [], [], []
        self.count = 0
        self.loops, self.ret, self.question = [], None, None
        self.mult, self.work = 1, 0
        self.panicky = 0
        self.blk, self.blocks, self.ldepth = 0, 0, 0
        self.scale = {}
        self.functions = 0
        self.phases, self.cut = [], None

    # ---- plumbing -------------------------------------------------------------------------

    def weight(self, name):
        """Swarm testing: each case turns a random fifth of the constructs off and favours others."""
        if name not in self.scale:
            self.scale[name] = random.Random(f"{self.key}/{name}").choice([0, 0.3, 1, 1, 3])
        return self.scale[name]

    def choose(self, options):
        """Runs one of `(weight, name, thunk)`, by weight, and counts the name."""
        live = [(w * self.weight(n), n, f) for w, n, f in options if w > 0]
        live = [o for o in live if o[0] > 0] or [(1, n, f) for w, n, f in options if w > 0]
        at = self.r.random() * sum(w for w, _, _ in live)
        for w, name, thunk in live:
            at -= w
            if at <= 0:
                break
        self.cov[name] += 1
        return thunk()

    def emit(self, text):
        self.lines.append("    " * self.ind + text)

    def fresh(self, prefix="v"):
        self.count += 1
        return f"{prefix}{self.count}"

    def chance(self, p):
        return self.r.random() < p

    def visible(self):
        return [v for v in self.scope if not v.hid]

    def assignable(self, v):
        """Whether generated code may give `v` a new value of any size here."""
        at = self.scope.index(v)
        return v.mut and not v.hid and at >= self.mut_floor and (not v.ty.heap or (v.loops == self.ldepth and not v.bounded))

    def mutable(self, v):
        """Whether generated code may change `v` in place by a bounded amount."""
        return v.mut and not v.hid and self.scope.index(v) >= self.mut_floor

    def use(self, v):
        return v.expr if v.ty.copy or v.owned else f"{v.expr}.clone()"

    def enter(self):
        self.blocks += 1
        state = (len(self.scope), self.mut_floor, self.blk, [v for v in self.scope if v.hid])
        self.blk = self.blocks
        return state

    def leave(self, state):
        size, self.mut_floor, self.blk, hidden = state
        del self.scope[size:]
        for v in self.scope:
            v.hid = 1 if v in hidden else 0

    def declare(self, expr, ty, mut=False, bounded=False):
        var = Var(expr, ty, mut, self.blk, self.ldepth, bounded)
        self.scope.append(var)
        return var

    def spend(self, cost=1):
        self.work += cost * self.mult

    # ---- types ----------------------------------------------------------------------------

    def scalar(self):
        t, r = self.t, self.r
        return self.choose([
            (10, "type.int", lambda: t.ints[r.choice(list(INTS))]),
            (3, "type.int128", lambda: t.ints[r.choice(["i128", "u128"])]),
            (3, "type.float", lambda: t.floats[r.choice(["f32", "f64"])]),
            (2, "type.bool", lambda: t.bool),
            (2, "type.char", lambda: t.char),
            (1, "type.str", lambda: t.sstr),
            (0.7, "type.nonzero", lambda: t.nonzero[r.choice(list(NONZERO))]),
            (0.4, "type.unit", lambda: t.unit),
        ])

    def fresh_ty(self, depth=2, want=lambda ty: True):
        """A type made on the spot. `want` says which types the place it is for can hold."""
        t, r = self.t, self.r
        for _ in range(20):
            if depth <= 0:
                ty = self.scalar()
            else:
                sub = lambda: self.fresh_ty(depth - 1, want)
                adts = [a for a in self.adt_types() if want(a)]
                ty = self.choose([
                    (9, "type.scalar", self.scalar),
                    (2, "type.tuple", lambda: t.tuple([sub() for _ in range(r.choice([1, 2, 2, 2, 3, 3, 4, 6]))])),
                    (2, "type.array", lambda: t.array(sub(), r.choice([1, 2, 2, 3, 3, 4, 5, 8, 17]))),
                    (5 if adts else 0, "type.adt", lambda: r.choice(adts)),
                    (1.5, "type.option", lambda: t.option(sub())),
                    (0.7, "type.result", lambda: t.result(sub(), sub())),
                    (0.8, "type.box", lambda: t.boxed(sub())),
                    (1, "type.vec", lambda: t.vec(sub())),
                    (0.8, "type.string", lambda: t.string),
                ])
            if ty.leaves <= 40 and want(ty):
                return ty
        for ty in list(t.ints.values()) + [t.string, t.vec(t.u8), t.array(t.u8, 4), t.tuple([t.u8, t.u32])]:
            if want(ty):
                return ty
        raise NoValue("no type wanted here")

    def adt_types(self):
        return [ty for ty in self.pool if ty.kind in ("struct", "enum")]

    def ty(self, want=lambda ty: True):
        """A type for a value: mostly one the case already uses, so that values meet."""
        pool = [ty for ty in self.pool if want(ty)]
        if pool and self.chance(0.8):
            return self.r.choice(pool)
        ty = self.fresh_ty(2, want)
        if ty.kind != "param" and ty not in self.pool and len(self.pool) < 40:
            self.pool.append(ty)
        return ty

    def int_ty(self):
        return self.ty(lambda ty: ty.kind == "int")

    # ---- literals -------------------------------------------------------------------------

    def int_value(self, ty):
        bits, signed = INTS[ty.name]
        lo, hi = (-(1 << bits - 1), (1 << bits - 1) - 1) if signed else (0, (1 << bits) - 1)
        r = self.r
        pick = r.randrange(10)
        if pick < 3:
            value = r.choice([0, 1, 2, 3, 7, 8, 10, 100, lo, hi, lo + 1, hi - 1, hi // 2, -1 if signed else hi])
        elif pick < 6:
            value = r.randint(-20, 20) if signed else r.randint(0, 40)
        elif pick < 8:
            value = (1 << r.randrange(bits)) + r.choice([-1, 0, 0, 1])
        else:
            value = r.randint(lo, hi)
        return max(lo, min(hi, value))

    def int_lit(self, ty, value=None, bare=False):
        value = self.int_value(ty) if value is None else value
        text = f"{value}{ty.name}"
        return text if value >= 0 or bare else f"({text})"

    def float_lit(self, ty):
        if self.chance(0.25):
            return f"{ty.name}::{self.r.choice(FLOAT_CONSTS)}"
        if self.chance(0.2):
            text = f"{self.r.uniform(-1000, 1000):.4f}"
        else:
            text = self.r.choice(FLOATS)
        return f"({text}{ty.name})" if text.startswith("-") else f"{text}{ty.name}"

    def lit(self, ty):
        """A constant expression of the type, or one that only allocates."""
        t, r, k = self.t, self.r, ty.kind
        if k == "int":
            return self.const_int(ty) if self.constant and self.chance(0.5) else self.int_lit(ty)
        if k == "float":
            return self.float_lit(ty)
        if k == "bool":
            return r.choice(["true", "false"])
        if k == "char":
            return r.choice(CHARS)
        if k == "unit":
            return "()"
        if k == "sstr":
            return r.choice(STRS)
        if k == "nz":
            value = self.int_value(ty.base) or 1
            return f"{ty.name}::new({self.int_lit(ty.base, value)}).unwrap()"
        if k == "tuple":
            return "(" + ", ".join(self.lit(a) for a in ty.args) + ("," if len(ty.args) == 1 else "") + ")"
        if k == "array":
            if ty.args[0].copy and ty.args[0].const and self.chance(0.4):
                return f"[{self.lit(ty.args[0])}; {ty.n}]"
            return "[" + ", ".join(self.lit(ty.args[0]) for _ in range(ty.n)) + "]"
        if k == "struct":
            return self.construct(ty.d.name, ty.d.tuple, ty.fields, self.lit)
        if k == "enum":
            variant, shape, fields = r.choice(ty.variants)
            return self.construct(f"{ty.d.name}::{variant}", shape == "tuple", fields, self.lit, ty)
        if k == "option":
            return f"Some({self.lit(ty.args[0])})" if self.chance(0.7) else f"None::<{ty.args[0].name}>"
        if k == "result":
            ok = self.chance(0.6)
            return f"{'Ok' if ok else 'Err'}::<{ty.args[0].name}, {ty.args[1].name}>({self.lit(ty.args[0 if ok else 1])})"
        if k == "box":
            return f"Box::new({self.lit(ty.args[0])})"
        if k == "vec":
            n = r.choice([0, 1, 2, 3, 5])
            if n == 0:
                return f"Vec::<{ty.args[0].name}>::new()"
            return "vec![" + ", ".join(self.lit(ty.args[0]) for _ in range(n)) + "]"
        if k == "string":
            return f"String::from({r.choice(STRS)})"
        raise NoValue(ty.name)

    def construct(self, path, tuple_like, fields, make, ty=None):
        """A struct or variant expression with each field made by `make`."""
        if ty is not None and ty.args:
            # A variant that names no parameter leaves it to be said.
            head, _, variant = path.rpartition("::")
            path = f"{head}::<{', '.join(a.name for a in ty.args)}>::{variant}"
        if not fields:
            return path
        if tuple_like:
            return f"{path}(" + ", ".join(make(t) for _, t in fields) + ")"
        return f"{path} {{ " + ", ".join(f"{n}: {make(t)}" for n, t in fields) + " }"

    # ---- type definitions -----------------------------------------------------------------

    BOUND = "Mix + Clone + Debug + PartialOrd"

    def make_adts(self):
        for _ in range(self.r.choice([1, 2, 3, 3, 4, 5, 6])):
            d = self.choose([
                (5, "adt.struct", self.struct_def),
                (2, "adt.struct_generic", lambda: self.struct_def(generic=True)),
                (2, "adt.enum_fieldless", self.fieldless_enum),
                (5, "adt.enum_data", self.enum_def),
                (1.5, "adt.enum_generic", lambda: self.enum_def(generic=True)),
            ])
            self.adts.append(d)
            self.emit_adt(d)
            if not d.params:
                self.pool.append(self.t.adt(d))
                continue
            for _ in range(self.r.choice([1, 2])):
                args = [self.ty(lambda ty: ty.leaves <= 8) for _ in d.params]
                ty = self.t.adt(d, args)
                if ty not in self.pool and ty.leaves <= 40:
                    self.pool.append(ty)

    def field_types(self, d, count, want):
        """Field types of a definition; a generic one names each parameter at least once."""
        t, r = self.t, self.r
        types = [self.ty(lambda ty: want(ty) and ty.leaves <= 16) for _ in range(count)]
        for p in d.params:
            wrapped = r.choice([p, p, t.option(p), t.array(p, 2), t.tuple([p, t.u8]), t.vec(p), t.boxed(p)])
            types.insert(r.randrange(len(types) + 1), wrapped)
        return types

    def struct_def(self, generic=False):
        r = self.r
        d = Adt("struct", self.fresh("S"))
        want = lambda ty: True
        if generic:
            d.params = [self.t.param(n) for n in r.choice([["A"], ["A", "B"]])]
            self.cov["generic.struct"] += 1
        else:
            d.repr, want = self.choose([
                (6, "repr.rust", lambda: ("", want)),
                (2, "repr.C", lambda: ("#[repr(C)]", want)),
                (3, "repr.C_scalars", lambda: ("#[repr(C)]", lambda ty: ty.cabi)),
                (1.5, "repr.packed", lambda: (r.choice(["#[repr(packed)]", "#[repr(C, packed)]", "#[repr(packed(2))]"]), lambda ty: ty.pod)),
                (1, "repr.align", lambda: (r.choice(["#[repr(align(16))]", "#[repr(C, align(8))]", "#[repr(align(2))]"]), want)),
            ])
            d.packed = "packed" in d.repr
            d.drop = not d.repr and self.chance(0.3)
            if d.drop:
                self.cov["drop.struct"] += 1
        d.tuple = self.chance(0.3)
        count = r.choice([0, 1, 1, 2, 2, 2, 3, 3, 4, 5, 7]) if not d.repr.startswith("#[repr(C)") else r.choice([1, 2, 2, 3, 4, 6])
        types = self.field_types(d, count, want)
        d.fields = [(str(i) if d.tuple else f"f{i}", ty) for i, ty in enumerate(types)]
        return d

    def fieldless_enum(self):
        r = self.r
        d = Adt("enum", self.fresh("E"))
        d.clike = True
        d.int = r.choice(["isize", "u8", "u8", "i8", "u16", "i16", "u32", "i32", "i64", "u64"])
        d.repr = "" if d.int == "isize" and self.chance(0.7) else f"#[repr({d.int})]"
        bits, signed = INTS[d.int]
        bits = min(bits, 31) if not d.repr else bits
        lo, hi = (-(1 << bits - 1), (1 << bits - 1) - 1) if signed else (0, (1 << bits) - 1)
        count = r.choice([1, 2, 2, 3, 4, 5, 8])
        if self.chance(0.5):
            d.discr = [None] * count
        else:
            values = sorted(r.sample(range(max(lo, -200), min(hi, 300) + 1), count) if self.chance(0.7)
                            else {r.choice([lo, hi, lo + 1, hi - 1, 0, 1, r.randint(lo, hi)]) for _ in range(count)})
            r.shuffle(values)
            d.discr = values
        d.variants = [(f"V{i}", "unit", []) for i in range(len(d.discr))]
        self.cov["repr.enum_int" if d.repr else "repr.rust"] += 1
        return d

    def enum_def(self, generic=False):
        r = self.r
        d = Adt("enum", self.fresh("E"))
        if generic:
            d.params = [self.t.param(n) for n in r.choice([["A"], ["A", "B"]])]
            self.cov["generic.enum"] += 1
        else:
            d.repr = self.choose([
                (7, "repr.rust", lambda: ""),
                (1.5, "repr.enum_int", lambda: f"#[repr({r.choice(['u8', 'i8', 'u16', 'i32', 'u64'])})]"),
                (1, "repr.C", lambda: "#[repr(C)]"),
                (0.5, "repr.C_int", lambda: "#[repr(C, u8)]"),
            ])
            d.drop = not d.repr and self.chance(0.12)
            if d.drop:
                self.cov["drop.enum"] += 1
        count = r.choice([1, 2, 2, 3, 3, 4, 5])
        counts = [r.choice([0, 0, 1, 1, 1, 2, 2, 3]) for _ in range(count)]
        if (generic or d.repr == "#[repr(C, u8)]") and not any(counts):
            counts[0] = 1
        # Types that leave values unused are where a variant of the outer type can be stored.
        types = self.field_types(d, sum(counts), lambda ty: True)
        r.shuffle(types)
        for i in range(count):
            mine, types = types[:counts[i]], types[counts[i]:]
            if i == count - 1:
                mine += types
            shape = "unit" if not mine else r.choice(["tuple", "tuple", "struct"])
            d.variants.append((f"V{i}", shape, [(str(j) if shape == "tuple" else f"f{j}", ty) for j, ty in enumerate(mine)]))
        return d

    def emit_adt(self, d):
        parts = [t for _, t in d.fields] + [t for _, _, fs in d.variants for _, t in fs]
        derives = ["Clone", "Debug", "PartialEq", "PartialOrd"]
        d.copy = not d.drop and all(self.t.could(t, "copy") for t in parts)
        d.total = all(self.t.could(t, "total") for t in parts)
        if d.copy:
            derives.append("Copy")
        if d.total:
            derives += ["Eq", "Ord", "Hash"]
        generics = "<" + ", ".join(p.name for p in d.params) + ">" if d.params else ""
        self.emit(f"#[derive({', '.join(derives)})]")
        if d.repr:
            self.emit(d.repr)
        if d.kind == "struct":
            self.emit_struct(d, generics)
        else:
            self.emit(f"pub enum {d.name}{generics} {{")
            for i, (variant, shape, fields) in enumerate(d.variants):
                tail = f" = {d.discr[i]}" if d.clike and d.discr[i] is not None else ""
                self.emit(f"    {variant}{self.field_list(shape == 'tuple', fields, False)}{tail},")
            self.emit("}")
        self.emit_mix(d, generics)
        if d.drop:
            self.emit(f"impl Drop for {d.name} {{")
            self.emit("    fn drop(&mut self) {")
            self.emit(f"        feed({self.r.randrange(1 << 32):#x});")
            self.emit("        self.mix();")
            self.emit("    }")
            self.emit("}")

    def mentions_param(self, ty):
        return ty.kind == "param" or any(self.mentions_param(a) for a in ty.args)

    def field_list(self, tuple_like, fields, public):
        pub = "pub " if public else ""
        if not fields:
            return ""
        if tuple_like:
            return "(" + ", ".join(pub + t.name for _, t in fields) + ")"
        return " { " + ", ".join(f"{pub}{n}: {t.name}" for n, t in fields) + " }"

    def emit_struct(self, d, generics):
        body = self.field_list(d.tuple, d.fields, True)
        self.emit(f"pub struct {d.name}{generics}{body}{';' if d.tuple or not d.fields else ''}")

    def emit_mix(self, d, generics):
        bounds = "<" + ", ".join(f"{p.name}: Mix" for p in d.params) + ">" if d.params else ""
        self.emit(f"impl{bounds} Mix for {d.name}{generics} {{")
        self.emit("    fn mix(&self) {")
        if d.kind == "struct":
            self.emit(f"        feed({len(d.fields)});")
            for name, _ in d.fields:
                # A field of a packed struct is copied out: it may not be borrowed.
                self.emit(f"        {{ let field = self.{name}; field.mix(); }}" if d.packed else f"        self.{name}.mix();")
        else:
            self.emit("        match self {")
            for i, (variant, shape, fields) in enumerate(d.variants):
                names = [f"a{j}" for j in range(len(fields))]
                if shape == "unit":
                    pattern = ""
                elif shape == "tuple":
                    pattern = "(" + ", ".join(names) + ")"
                else:
                    pattern = " { " + ", ".join(f"{n}: {a}" for (n, _), a in zip(fields, names)) + " }"
                feeds = "".join(f" {a}.mix();" for a in names)
                self.emit(f"            {d.name}::{variant}{pattern} => {{ feed({i + 10});{feeds} }}")
            self.emit("        }")
        self.emit("    }")
        self.emit("}")

    # ---- expressions ----------------------------------------------------------------------
    #
    # Every expression is returned as text that can be followed by `.method()`: anything that is
    # not a path, a literal, a call or a macro is in parentheses.

    def expr(self, ty, d=None):
        """An expression of the type, at most `d` levels of operators deep."""
        if d is None:
            d = self.r.choice([1, 2, 2, 3])
        self.spend()
        if d <= 0 or self.chance(0.12) or self.work > self.cap:
            return self.leaf(ty)
        return self.choose(self.common(ty, d) + getattr(self, "x_" + ty.kind)(ty, d))

    def leaf(self, ty):
        reads = self.reads(ty)
        if reads and (ty.kind == "param" or self.chance(0.65)):
            self.cov["expr.read"] += 1
            return self.r.choice(reads)
        self.cov["expr.literal"] += 1
        return self.lit(ty)

    def reads(self, ty):
        """Every way to read a value of the type out of what is in scope."""
        direct, inside = [], []
        for v in self.scope:
            if v.hid or v.ty.kind in ("fn", "dyn", "slice"):
                continue
            if v.ty is ty:
                direct.append(self.use(v))
            elif v.ty.leaves > 1 or v.ty.args:
                self.walk(v.expr, v.ty, ty, 2, inside)
        return direct if direct and (not inside or self.chance(0.7)) else inside or direct

    def walk(self, place, have, want, depth, out, packed=False):
        if have is want:
            out.append(f"({{ {place} }})" if packed else place if have.copy else f"{place}.clone()")
            return
        if depth == 0:
            return
        k = have.kind
        if k == "struct":
            for name, ty in have.fields:
                self.walk(f"{place}.{name}", ty, want, depth - 1, out, packed or have.d.packed)
        elif k == "tuple":
            for i, ty in enumerate(have.args):
                self.walk(f"{place}.{i}", ty, want, depth - 1, out, packed)
        elif k == "array":
            self.walk(f"{place}[{self.r.randrange(have.n)}]", have.args[0], want, depth - 1, out, packed)
        elif k == "box":
            self.walk(f"(*{place})", have.args[0], want, depth - 1, out, packed)

    def cond(self, d=2):
        return self.expr(self.t.bool, d)

    def small(self, bound, d=1):
        """A `usize` below `bound`, which is at least 1."""
        if self.chance(0.4):
            return str(self.r.randrange(bound))
        return f"({self.expr(self.t.usize, d)} % {bound})"

    def closure(self, params, ret, d, refs=(), times=1, body=None):
        """A closure with typed parameters; those named in `refs` are taken by reference.
        What it captures it only reads, so that it can be called while its captures are in use."""
        state, saved = self.enter(), (self.loops, self.ret, self.question, self.mult)
        self.loops, self.ret, self.question, self.mult = [], None, None, self.mult * times
        self.mut_floor = len(self.scope)
        for name, ty in params:
            self.declare(f"(*{name})" if name in refs else name, ty)
        text = (body or (lambda: self.expr(ret, d)))()
        self.loops, self.ret, self.question, self.mult = saved
        self.leave(state)
        sig = ", ".join(f"{n}: {'&' if n in refs else ''}{t.name}" for n, t in params)
        return f"|{sig}| -> {ret.name} {{ {text} }}"

    def containers(self, ty):
        """Types of the pool that hold a value of `ty`, each with how to take it out of an rvalue."""
        out = []
        for c in self.pool:
            if c.leaves > 24:
                continue
            clone = "" if ty.copy else ".clone()"
            if c.kind == "tuple":
                out += [(c, "expr.tuple_field", f"{{}}.{i}") for i, a in enumerate(c.args) if a is ty]
            elif c.kind == "array" and c.args[0] is ty:
                out.append((c, "expr.array_index", f"{{}}[{self.r.randrange(c.n)}]{clone}"))
            elif c.kind == "struct":
                for name, a in c.fields:
                    if a is not ty:
                        continue
                    if c.d.packed:
                        out.append((c, "expr.packed_field", f"({{{{ let whole = {{}}; whole.{name} }}}})"))
                    else:
                        out.append((c, "expr.struct_field", f"{{}}.{name}{clone if c.d.drop else ''}"))
            elif c.kind == "box" and c.args[0] is ty:
                out.append((c, "expr.box_deref", "(*{})"))
        return out

    def common(self, ty, d):
        """Ways to make a value that do not depend on the kind of its type."""
        t, r = self.t, self.r
        e = lambda ty2=ty: self.expr(ty2, d - 1)
        reads = self.reads(ty)
        calls = self.callables(ty)
        methods = self.methods(ty)
        holders = self.containers(ty)
        options = [
            (4 if reads else 0, "expr.read", lambda: r.choice(reads)),
            (1 if ty.kind != "param" else 0, "expr.literal", lambda: self.lit(ty)),
            (1.5, "expr.if", lambda: f"(if ({self.cond(d - 1)}) {{ {e()} }} else {{ {e()} }})"),
            (1.2, "expr.match", lambda: self.match_expr(ty, d)),
            (0.6, "expr.block", lambda: self.block_expr(ty, d)),
            (5 if calls else 0, "call.function", lambda: self.call(*r.choice(calls), d)),
            (3 if methods else 0, "call.method", lambda: self.method_call(*r.choice(methods), d)),
            (1.5 if holders else 0, "expr.project", lambda: self.project(r.choice(holders), d)),
            (0.8, "option.unwrap_or", lambda: self.unwrap(ty, d)),
            (0.5, "generic.id", lambda: f"id::<{ty.name}>({e()})"),
            (0.5, "generic.sel", lambda: f"sel::<{ty.name}>({self.cond(d - 1)}, {e()}, {e()})"),
            (0.4, "generic.pick", lambda: f"pick::<{ty.name}>({self.cond(d - 1)}, &{e()}, &{e()}).clone()"),
            (0.3, "generic.twice", lambda: f"twice::<{ty.name}>(&{e()}).{r.randrange(2)}"),
            (0.4, "generic.larger", lambda: f"larger::<{ty.name}>({e()}, {e()})"),
            (0.5, "closure.call_inline", lambda: self.apply_closure(ty, d)),
            (0.3, "loop.break_value", lambda: f"(loop {{ break {e()}; }})"),
            (0.3, "loop.labelled_block", lambda: self.labelled_block(ty, d)),
            (0.3 if ty.copy and ty.kind != "param" else 0, "raw.read_cast",
             lambda: f"(unsafe {{ *(&{e()} as *const {ty.name}) }})"),
            (0.3, "raw.maybe_uninit", lambda: f"(unsafe {{ let mut slot = MaybeUninit::<{ty.name}>::uninit(); slot.write({e()}); slot.assume_init() }})"),
        ]
        if self.question is not None and self.question.kind == "option":
            options.append((1.5, "option.question", lambda: f"({self.expr(t.option(ty), d - 1)}?)"))
        if self.question is not None and self.question.kind == "result":
            options.append((1.5, "result.question", lambda: f"({self.expr(t.result(ty, self.question.args[1]), d - 1)}?)"))
        if self.panicky:
            options += [
                (0.6, "panic.unwrap_none", lambda: f"{self.expr(t.option(ty), d - 1)}.unwrap()"),
                (0.3, "panic.expect", lambda: f"{self.expr(t.option(ty), d - 1)}.expect(\"nothing there\")"),
                (0.3, "panic.unwrap_err", lambda: f"{self.expr(t.result(ty, self.ty(lambda x: x.kind != 'param')), d - 1)}.unwrap()"),
                (0.3, "panic.index_vec", lambda: f"{self.expr(t.vec(ty), d - 1)}[{self.expr(t.usize, d - 1)}].clone()"),
                (0.2, "panic.explicit", lambda: f"(if ({self.cond(d - 1)}) {{ panic!(\"gave up at {{}}\", {self.expr(t.u32, d - 1)}) }} else {{ {e()} }})"),
            ]
        return options

    def project(self, holder, d):
        container, name, form = holder
        self.cov[name] += 1
        return form.format(self.expr(container, d - 1))

    def unwrap(self, ty, d):
        t, r = self.t, self.r
        e = lambda ty2=ty: self.expr(ty2, d - 1)
        option = lambda: self.expr(t.option(ty), d - 1)
        err = self.ty(lambda x: x.kind != "param" and x.leaves <= 8)
        binder = self.fresh("x")
        return r.choice([
            lambda: f"{option()}.unwrap_or({e()})",
            lambda: f"{option()}.unwrap_or_else({self.closure([], ty, d - 1)})",
            lambda: f"{option()}.map_or({e()}, {self.closure([(binder, ty)], ty, d - 1)})",
            lambda: f"(if let Some({binder}) = ({option()}) {{ {binder} }} else {{ {e()} }})",
            lambda: f"(match ({option()}) {{ None => {e()}, Some({binder}) => {binder} }})",
            lambda: f"{self.expr(t.result(ty, err), d - 1)}.unwrap_or({e()})",
            lambda: f"{self.expr(t.result(ty, err), d - 1)}.unwrap_or_else({self.closure([(binder, err)], ty, d - 1)})",
        ])()

    def block_expr(self, ty, d):
        other = self.ty(lambda x: x.kind != "param")
        name = self.fresh("t")
        value = self.expr(other, d - 1)
        state = self.enter()
        self.declare(name, other)
        tail = self.expr(ty, d - 1)
        self.leave(state)
        return f"({{ let {name}: {other.name} = {value}; {tail} }})"

    def labelled_block(self, ty, d):
        label = self.fresh("'b")
        return f"({label}: {{ if ({self.cond(d - 1)}) {{ break {label} {self.expr(ty, d - 1)}; }} {self.expr(ty, d - 1)} }})"

    def apply_closure(self, ty, d):
        """A closure made and called in one expression, through each of the ways to pass one."""
        arg = self.ty(lambda x: x.kind != "param" and x.leaves <= 8)
        name = self.fresh("x")
        unary = lambda: self.closure([(name, arg)], ty, d - 1)
        nullary = lambda: self.closure([], ty, d - 1)

        def value():
            # The closure is made first and borrows what is in scope: the argument changes none of it.
            state = self.enter()
            self.mut_floor = len(self.scope)
            text = self.expr(arg, d - 1)
            self.leave(state)
            return text

        return self.choose([
            (2, "closure.impl_fn", lambda: f"apply({unary()}, {value()})"),
            (1, "closure.dyn_fn", lambda: f"apply_dyn(&{unary()}, {value()})"),
            (1, "closure.dyn_fn_mut", lambda: f"apply_dyn_mut(&mut {unary()}, {value()})"),
            (1, "closure.impl_fn_once", lambda: f"apply_once({nullary()})"),
            (1, "closure.boxed_fn_once", lambda: f"apply_boxed(Box::new({nullary()}))"),
            (1, "closure.impl_fn_mut", lambda: f"apply_mut({unary()}, {value()}).{self.r.randrange(2)}"),
            (1, "closure.called_in_place", lambda: f"({unary()})({value()})"),
        ])

    # ---- calls ----------------------------------------------------------------------------

    def callables(self, ty):
        """The functions and closures in reach that return `ty`, with the types of their parameters."""
        out = []
        local = [(v.ty.sig, v) for v in self.scope if v.ty.kind == "fn" and not v.hid]
        for f, var in [(f, None) for f in self.fns] + local:
            if (f.panics and not self.panicky) or f.cost * self.mult + self.work > self.cap:
                continue
            if var is not None and f.fnmut and self.scope.index(var) < self.mut_floor:
                continue
            bound = {}
            if self.t.unify(f.curried.ret if f.curried else f.ret, ty, bound):
                out.append((f, bound, var))
        return out

    def call(self, f, bound, var, d):
        t = self.t
        for param, trait in f.tparams:
            if param.name not in bound:
                bound[param.name] = self.r.choice(trait.impls) if trait else self.ty(lambda x: x.kind != "param" and x.leaves <= 8)
            elif trait and bound[param.name] not in trait.impls:
                return self.leaf(t.subst(f.curried.ret if f.curried else f.ret, bound))
        if var is not None and f.fnmut:
            var.hid += 1
        before, after = [], []
        args = [self.argument(mode, t.subst(ty, bound) if isinstance(ty, Ty) else ty, d, before, after) for mode, ty in f.params]
        text = f.name
        if f.tparams:
            text += "::<" + ", ".join(bound[p.name].name for p, _ in f.tparams) + ">"
        text += "(" + ", ".join(args) + ")"
        if f.curried:
            text += "(" + ", ".join(self.argument(m, ty, d, before, after) for m, ty in f.curried.params) + ")"
        if var is not None and f.fnmut:
            var.hid -= 1
        self.spend(f.cost)
        self.cov[f"abi.args_{min(len(f.params), 8)}{'+' if len(f.params) >= 8 else ''}"] += 1
        if before:
            return f"({{ {' '.join(before)} let out = {text}; {' '.join(after)} out }})"
        return text

    def argument(self, mode, ty, d, before, after):
        t, r = self.t, self.r
        if mode == "val":
            return self.expr(ty, d - 1)
        if mode == "ref":
            return f"&{self.expr(ty, d - 1)}"
        if mode in ("mut", "mutslice"):
            name = self.fresh("m")
            ty = t.vec(ty) if mode == "mutslice" else ty
            before.append(f"let mut {name}: {ty.name} = {self.expr(ty, d - 1)};")
            after.append(f"{name}.mix();")
            return f"&mut {name}"
        if mode == "slice":
            return self.slice_arg(ty, d)
        if mode == "str":
            return r.choice([lambda: r.choice(STRS), lambda: f"{self.expr(t.string, d - 1)}.as_str()", lambda: self.expr(t.sstr, d - 1)])()
        if mode == "dyn":
            return f"&{self.expr(r.choice(ty.impls), d - 1)}"
        if mode == "depth":
            return f"({self.expr(t.u32, d - 1)} % 9)"
        assert mode == "fn"
        return self.function_value(ty, d)

    def slice_arg(self, elem, d):
        t, r = self.t, self.r
        return self.choose([
            (2, "slice.of_literal", lambda: "&[" + ", ".join(self.expr(elem, d - 1) for _ in range(r.randrange(5))) + "]"),
            (2, "slice.of_vec", lambda: f"&{self.expr(t.vec(elem), d - 1)}"),
            (1, "slice.of_array", lambda: f"&{self.expr(t.array(elem, r.choice([1, 2, 3, 5])), d - 1)}"),
            (1, "slice.range", lambda: f"{self.expr(t.vec(elem), d - 1)}.get({r.randrange(3)}..{r.randrange(6)}).unwrap_or(&[])"),
        ])

    def function_value(self, sig, d):
        """Something of a function pointer type: a function of that signature or a closure that captures nothing."""
        same = [f for f in self.fns if self.pointer_type(f) == self.pointer_type(sig) and f.cost <= sig.cost and not f.panics]
        if same and self.chance(0.6):
            self.cov["fnptr.function"] += 1
            return self.r.choice(same).name
        if sig.abi:
            return sig.name
        self.cov["fnptr.closure"] += 1
        params = [(self.fresh("a"), ty) for _, ty in sig.params]
        scope, self.scope = self.scope, [v for v in self.scope if v.blk == 0 and v.ty.kind != "fn"]
        fns, self.fns = self.fns, []
        traits, self.traits = self.traits, []
        text = self.closure(params, sig.ret, min(d, 2))
        self.scope, self.fns, self.traits = scope, fns, traits
        return f"({text})"

    def pointer_type(self, f):
        if not f.pointer or f.tparams or f.curried or any(mode != "val" for mode, _ in f.params):
            return None
        return f"{f.abi}fn({', '.join(ty.name for _, ty in f.params)}) -> {f.ret.name}"

    def methods(self, ty):
        """Trait methods that return `ty`, each with the trait."""
        if self.work > self.cap:
            return []
        return [(trait, m) for trait in self.traits for m in trait.methods if m.ret is ty and (self.panicky or not m.panics)]

    def method_call(self, trait, m, d):
        t, r = self.t, self.r
        self.spend(m.cost)
        args = lambda: ", ".join(self.expr(ty, d - 1) for _, ty in m.params)
        comma = ", " if m.params else ""
        owner = lambda: self.expr(r.choice(trait.impls), d - 1)
        held = [v for v in self.scope if not v.hid and ((v.ty.kind == "dyn" and v.ty.trait is trait) or
                                                        (v.ty.kind == "param" and getattr(v.ty, "trait", None) is trait))]
        held = [v for v in held if m.self_mode == "ref" or (v.ty.kind == "dyn" and self.mutable(v))]
        if held and (self.chance(0.7) or trait.generic_only):
            v = r.choice(held)
            self.cov["dyn.call_held" if v.ty.kind == "dyn" else "generic.bound_call"] += 1
            if m.self_mode == "ref" and v.ty.kind == "param":
                return f"{v.expr}.{m.name}({args()})"
            if m.self_mode == "ref":
                # The arguments may not call a `&mut self` method of the object they are passed to.
                with self.hidden(v):
                    return f"{v.expr}.{m.name}({args()})"
            v.hid += 1
            text = f"({{ let args = ({args()}{comma}); {v.expr}.{m.name}({', '.join(f'args.{i}' for i in range(len(m.params)))}) }})"
            v.hid -= 1
            return text
        if trait.generic_only:
            return self.leaf(m.ret)
        if m.self_mode == "mut":
            self.cov["trait.mut_self"] += 1
            return f"({{ let mut it = {owner()}; let out = it.{m.name}({args()}); it.mix(); out }})"
        options = [
            (3, "trait.static_call", lambda: f"{owner()}.{m.name}({args()})"),
            (1, "trait.qualified_call", lambda: f"{trait.name}::{m.name}(&{owner()}{comma}{args()})"),
        ]
        if trait.dyn:
            options += [
                (2, "dyn.ref", lambda: f"(&{owner()} as &dyn {trait.name}).{m.name}({args()})"),
                (2, "dyn.box", lambda: f"(Box::new({owner()}) as Box<dyn {trait.name}>).{m.name}({args()})"),
            ]
        return self.choose(options)

    # ---- values by kind -------------------------------------------------------------------

    def x_int(self, ty, d):
        t, r, n = self.t, self.r, ty.name
        bits, signed = INTS[n]
        e = lambda: self.expr(ty, d - 1)
        u32 = lambda: self.expr(t.u32, d - 1)
        other_int = lambda: self.expr(t.ints[r.choice(list(INTS))], d - 1)
        op = lambda names: r.choice(names.split())
        lo, hi = sorted([self.int_value(ty), self.int_value(ty)])
        wider = [m for m, (b, s) in INTS.items() if "size" not in m and "size" not in n and (b < bits and (signed or not s))]
        fieldless = [x for x in self.pool if x.kind == "enum" and x.d.clike]
        total = lambda: self.ty(lambda x: x.total and x.kind != "param" and x.leaves <= 8)
        options = [
            (4, "int.wrapping", lambda: f"{e()}.wrapping_{op('add sub mul')}({e()})"),
            (3, "int.bitwise", lambda: f"({e()} {op('^ & |')} {e()})"),
            (1, "int.not", lambda: f"(!{e()})"),
            (2, "int.shift_wrapping", lambda: f"{e()}.{op('wrapping_shl wrapping_shr rotate_left rotate_right')}({u32()})"),
            (1.5, "int.shift_masked", lambda: f"({e()} {op('<< >>')} ({u32()} % {bits}))"),
            (1, "int.shift_checked", lambda: f"{e()}.{op('checked_shl checked_shr')}({u32()}).unwrap_or({e()})"),
            (0.7, "int.shift_overflowing", lambda: f"{e()}.{op('overflowing_shl overflowing_shr')}({u32()}).0"),
            (2.5, "int.saturating", lambda: f"{e()}.saturating_{op('add sub mul')}({e()})"),
            (0.7, "int.pow", lambda: f"{e()}.{op('wrapping_pow saturating_pow')}({u32()} % 7)"),
            (2.5, "int.checked", lambda: f"{e()}.checked_{op('add sub mul div rem')}({e()}).unwrap_or({e()})"),
            (1.5, "int.overflowing", lambda: f"{e()}.overflowing_{op('add sub mul')}({e()}).0"),
            (0.7, "int.overflow_flag", lambda: f"({e()}.overflowing_{op('add sub mul')}({e()}).1 as {n})"),
            (2, "int.division", lambda: f"{e()}.{op('wrapping_div wrapping_rem wrapping_div_euclid wrapping_rem_euclid')}({e()} | 1)"),
            (1, "int.negate", lambda: f"{e()}.{op('wrapping_neg checked_neg().unwrap_or_default')}()"),
            (1.5, "int.bit_count", lambda: f"({e()}.{op('count_ones count_zeros leading_zeros trailing_zeros leading_ones trailing_ones')}() as {n})"),
            (1.5, "int.bytes", lambda: f"{e()}.{op('swap_bytes reverse_bits to_be to_le')}()"),
            (0.7, "int.byte_arrays", lambda: f"{n}::{op('from_be_bytes from_le_bytes from_ne_bytes')}({e()}.{op('to_le_bytes to_be_bytes to_ne_bytes')}())"),
            (1.5, "int.min_max", lambda: f"{e()}.{op('min max')}({e()})"),
            (0.7, "int.clamp", lambda: f"{e()}.clamp({self.int_lit(ty, lo)}, {self.int_lit(ty, hi)})"),
            (0.7, "int.abs_diff", lambda: f"({e()}.abs_diff({e()}) as {n})"),
            (4, "cast.int_to_int", lambda: f"({other_int()} as {n})"),
            (1.5, "cast.float_to_int", lambda: f"({self.expr(t.floats[op('f32 f64')], d - 1)} as {n})"),
            (0.7, "cast.bool_to_int", lambda: f"({self.cond(d - 1)} as {n})"),
            (0.7, "cast.char_to_int", lambda: f"({self.expr(t.char, d - 1)} as {n})"),
            (1.5 if fieldless else 0, "cast.enum_to_int", lambda: f"({self.expr(r.choice(fieldless), d - 1)} as {n})"),
            (1, "cast.try_from", lambda: f"{n}::try_from({other_int()}).unwrap_or({e()})"),
            (1 if wider else 0, "cast.from", lambda: f"{n}::from({self.expr(t.ints[r.choice(wider)], d - 1)})"),
            (0.5, "string.parse_int", lambda: f"{self.expr(t.sstr, d - 1)}.parse::<{n}>().unwrap_or({e()})"),
            (0.3, "string.parse_radix", lambda: f"{n}::from_str_radix({self.expr(t.sstr, d - 1)}, {r.choice([2, 8, 10, 16, 36])}).unwrap_or({e()})"),
            (1, "iter.fold", lambda: self.fold(ty, d)),
            (0.5, "raw.union", lambda: self.union_read(ty, d)),
            (0.5, "raw.transmute", lambda: self.transmute(ty, d)),
        ]
        if signed:
            options += [
                (1, "int.signed_abs", lambda: f"{e()}.{op('wrapping_abs signum saturating_abs saturating_neg')}()"),
                (0.5, "int.unsigned_abs", lambda: f"({e()}.unsigned_abs() as {n})"),
            ]
        else:
            options += [
                (0.7, "int.isqrt", lambda: f"{e()}.isqrt()"),
                (0.5, "int.power_of_two", lambda: f"{e()}.checked_next_power_of_two().unwrap_or({e()})"),
                (0.5, "int.ilog", lambda: f"({e()}.checked_ilog2().unwrap_or(0) as {n})"),
            ]
        if n == "usize":
            sized = self.ty(lambda x: x.kind != "param")
            options += [
                (2, "vec.len", lambda: f"{self.expr(self.ty(lambda x: x.kind in ('vec', 'string') and not self.mentions_param(x)), d - 1)}.len()"),
                (1, "string.char_count", lambda: f"{self.expr(t.string, d - 1)}.chars().count()"),
                (1.5, "layout.size_of", lambda: f"std::mem::size_of::<{sized.name}>()"),
                (1, "layout.align_of", lambda: f"std::mem::align_of::<{sized.name}>()"),
                (0.7, "layout.size_of_val", lambda: f"std::mem::size_of_val(&{self.expr(sized, d - 1)})"),
                (1, "iter.count", lambda: f"{self.chain(d)[0]}.count()"),
                (0.5, "generic.const_array", lambda: f"fold_array({self.expr(self.ty(lambda x: x.kind == 'array' and not self.mentions_param(x)), d - 1)})"),
            ]
        if n == "u64":
            options.append((2, "trait.derived_hash", lambda: f"hash_of(&{self.expr(total(), d - 1)})"))
        if n == "i8":
            options.append((2, "trait.derived_ord", lambda: self.compare(total(), d)))
        if n == "u32":
            options.append((1, "char.to_digit", lambda: f"{self.expr(t.char, d - 1)}.to_digit({r.choice([2, 10, 16, 36])}).unwrap_or({e()})"))
        if n == "u8":
            options.append((1, "string.byte", lambda: f"{self.expr(t.string, d - 1)}.as_bytes().first().copied().unwrap_or({e()})"))
        if self.panicky:
            options += [
                (5, "panic.overflow", lambda: f"({e()} {op('+ - *')} {e()})"),
                (2, "panic.divide", lambda: f"({e()} {op('/ %')} {e()})"),
                (1.5, "panic.shift", lambda: f"({e()} {op('<< >>')} {u32()})"),
                (1, "panic.pow", lambda: f"{e()}.pow({u32()} % 70)"),
                (1, "panic.try_from", lambda: f"{n}::try_from({other_int()}).unwrap()"),
                (1, "panic.checked_expect", lambda: f"{e()}.checked_{op('add sub mul div')}({e()}).expect(\"out of range\")"),
                (1.5, "panic.index_array", lambda: f"[{e()}, {e()}, {e()}][{self.expr(t.usize, d - 1)}]"),
            ]
            if signed:
                options.append((1, "panic.negate", lambda: f"(-{e()})"))
                options.append((0.5, "panic.abs", lambda: f"{e()}.abs()"))
        return options

    def compare(self, ty, d):
        return f"({self.expr(ty, d - 1)}.cmp(&{self.expr(ty, d - 1)}) as i8)"

    def fold(self, ty, d):
        chain, elem = self.chain(d)
        acc, item = self.fresh("acc"), self.fresh("x")
        return f"{chain}.fold({self.expr(ty, d - 1)}, {self.closure([(acc, ty), (item, elem)], ty, d - 1, times=6)})"

    def union_read(self, ty, d):
        """Writes one field of a union of integers and reads another: all of them cover every byte."""
        bits = self.r.choice([32, 64, 128])
        fields = {"word": f"u{bits}", "signed": f"i{bits}", "halves": f"[u{bits // 2}; 2]", "bytes": f"[u8; {bits // 8}]"}
        source, read = self.r.choice(["word", "signed"]), self.r.choice(list(fields))
        value = self.expr(self.t.ints[fields[source]], d - 1)
        index = "" if read in ("word", "signed") else f"[{self.r.randrange(2)}]"
        return f"(unsafe {{ Bytes{bits // 8} {{ {source}: {value} }}.{read}{index} }} as {ty.name})"

    def transmute(self, ty, d):
        bits = self.r.choice([16, 32, 64, 128])
        shapes = [f"u{bits}", f"i{bits}", f"[u8; {bits // 8}]"] + ([f"[u{bits // 2}; 2]", f"[i{bits // 2}; 2]"] if bits > 16 else [])
        source, target = self.r.choice(shapes[:2]), self.r.choice(shapes)
        value = self.expr(self.t.ints[source], d - 1)
        index = f"[{self.r.randrange(2)}]" if target.startswith("[") else ""
        return f"(unsafe {{ std::mem::transmute::<{source}, {target}>({value}) }}{index} as {ty.name})"

    def x_bool(self, ty, d):
        t, r = self.t, self.r
        e = lambda: self.expr(ty, d - 1)
        op = lambda names: r.choice(names.split())
        compared = self.ty(lambda x: x.leaves <= 12 and (x.kind != "param" or self.reads(x)))
        integer = self.int_ty()
        signed = INTS[integer.name][1]
        number = lambda: self.expr(integer, d - 1)
        float_ = lambda: self.expr(t.floats[op("f32 f64")], d - 1)
        char = lambda: self.expr(t.char, d - 1)
        some = lambda: self.expr(t.option(self.ty(lambda x: x.kind != "param")), d - 1)
        return [
            (6, "cmp.ordering", lambda: f"({self.expr(compared, d - 1)} {op('< <= > >= == !=')} {self.expr(compared, d - 1)})"),
            (1, "bool.not", lambda: f"(!{e()})"),
            (2, "bool.short_circuit", lambda: f"({e()} {op('&& ||')} {e()})"),
            (1, "bool.bitwise", lambda: f"({e()} {op('^ & |')} {e()})"),
            (1.5, "option.is_some", lambda: f"{some()}.{op('is_some is_none')}()"),
            (1, "pattern.matches_macro", lambda: self.matches_macro(d)),
            (1, "int.predicate", lambda: f"{number()}.{op('is_positive is_negative') if signed else 'is_power_of_two'}()"),
            (1, "int.overflow_flag", lambda: f"{number()}.overflowing_{op('add sub mul')}({number()}).1"),
            (1, "float.classify", lambda: f"{float_()}.{op('is_nan is_finite is_infinite is_normal')}()"),
            (1, "char.classify", lambda: f"{char()}.{op('is_alphabetic is_ascii_digit is_uppercase is_whitespace is_alphanumeric is_ascii is_control')}()"),
            (1, "vec.contains", lambda: self.contains(d)),
            (1, "string.search", lambda: f"{self.expr(t.string, d - 1)}.{op('starts_with ends_with contains')}({r.choice(STRS)})"),
            (0.7, "iter.any_all", lambda: self.any_all(d)),
        ]

    def matches_macro(self, d):
        ty = self.ty(lambda x: x.kind != "param" and x.leaves <= 12)
        pattern = self.pattern(ty, 2, [], "none")
        return f"matches!({self.expr(ty, d - 1)}, ({pattern}))"

    def contains(self, d):
        elem = self.ty(lambda x: x.kind != "param" and x.leaves <= 6)
        return f"{self.expr(self.t.vec(elem), d - 1)}.contains(&{self.expr(elem, d - 1)})"

    def any_all(self, d):
        chain, elem = self.chain(d)
        return f"{chain}.{self.r.choice(['any', 'all'])}({self.closure([(self.fresh('x'), elem)], self.t.bool, d - 1, times=6)})"

    def x_char(self, ty, d):
        t, r = self.t, self.r
        e = lambda: self.expr(ty, d - 1)
        text = lambda: self.expr(t.string, d - 1)
        return [
            (2, "cast.u8_to_char", lambda: f"({self.expr(t.u8, d - 1)} as char)"),
            (2, "char.from_u32", lambda: f"char::from_u32({self.expr(t.u32, d - 1)}).unwrap_or({e()})"),
            (1, "char.from_digit", lambda: f"char::from_digit({self.expr(t.u32, d - 1)} % 36, 36).unwrap_or({e()})"),
            (1, "char.case", lambda: f"{e()}.{r.choice(['to_ascii_uppercase', 'to_ascii_lowercase'])}()"),
            (1, "char.unicode_case", lambda: f"{e()}.{r.choice(['to_uppercase', 'to_lowercase'])}().next().unwrap_or({e()})"),
            (2, "string.chars", lambda: f"{text()}.chars().{r.choice(['next()', 'last()', 'rev().nth(1)', 'nth(2)', 'max()'])}.unwrap_or({e()})"),
        ]

    def x_float(self, ty, d):
        t, r, n = self.t, self.r, ty.name
        e = lambda: self.expr(ty, d - 1)
        op = lambda names: r.choice(names.split())
        other = t.floats["f64" if n == "f32" else "f32"]
        exact = ["u8", "i8", "u16", "i16"] + (["u32", "i32"] if n == "f64" else [])
        return [
            (6, "float.arithmetic", lambda: f"({e()} {op('+ - * / %')} {e()})"),
            (1, "float.negate", lambda: f"(-{e()})"),
            (3, "float.rounding", lambda: f"{e()}.{op('abs sqrt floor ceil round trunc fract recip')}()"),
            (1, "float.mul_add", lambda: f"{e()}.mul_add({e()}, {e()})"),
            (0.7, "float.clamp", lambda: f"{e()}.clamp(-1.5{n}, 2.5{n})"),
            (0.7, "float.euclid", lambda: f"{e()}.{op('rem_euclid div_euclid')}({e()})"),
            (3, "cast.int_to_float", lambda: f"({self.expr(t.ints[r.choice(list(INTS))], d - 1)} as {n})"),
            (2, "cast.float_to_float", lambda: f"({self.expr(other, d - 1)} as {n})"),
            (1, "cast.from", lambda: f"{n}::from({self.expr(t.ints[r.choice(exact)], d - 1)})"),
            (0.7, "string.parse_float", lambda: f"{r.choice(FLOAT_TEXTS)}.parse::<{n}>().unwrap_or({e()})"),
            (0.7, "iter.fold", lambda: self.fold(ty, d)),
        ]

    def x_unit(self, ty, d):
        any_ty = self.ty(lambda x: x.kind != "param")
        return [
            (2, "unit.mix", lambda: f"{self.expr(any_ty, d - 1)}.mix()"),
            (1, "drop.explicit", lambda: f"drop({self.expr(any_ty, d - 1)})"),
        ]

    def x_sstr(self, ty, d):
        r = self.r
        return [
            (2, "string.static_trim", lambda: f"{self.expr(ty, d - 1)}.{r.choice(['trim', 'trim_start', 'trim_end'])}()"),
            (2, "string.static_get", lambda: f"{self.expr(ty, d - 1)}.get({r.randrange(4)}..{r.randrange(8)}).unwrap_or({self.expr(ty, d - 1)})"),
            (1, "string.static_split", lambda: f"{self.expr(ty, d - 1)}.split({r.choice([chr(39) + ',' + chr(39), chr(39) + ' ' + chr(39)])}).{r.choice(['next()', 'last()', 'nth(1)'])}.unwrap_or({self.expr(ty, d - 1)})"),
        ]

    def x_nz(self, ty, d):
        return [(3, "nonzero.new", lambda: f"{ty.name}::new({self.expr(ty.base, d - 1)}).unwrap_or({self.expr(ty, d - 1)})")]

    def x_param(self, ty, d):
        return []

    def x_tuple(self, ty, d):
        return [(6, "aggregate.tuple", lambda: "(" + ", ".join(self.expr(a, d - 1) for a in ty.args) + ("," if len(ty.args) == 1 else "") + ")")]

    def x_array(self, ty, d):
        t, r = self.t, self.r
        elem, n = ty.args[0], ty.n
        e = lambda: self.expr(ty, d - 1)
        index, item = self.fresh("i"), self.fresh("x")
        source = self.ty(lambda x: x.kind != "param" and x.leaves <= 4)
        options = [
            (4, "aggregate.array", lambda: "[" + ", ".join(self.expr(elem, d - 1) for _ in range(n)) + "]"),
            (1.5, "array.from_fn", lambda: f"std::array::from_fn::<{elem.name}, {n}, _>({self.closure([(index, t.usize)], elem, d - 1, times=n)})"),
            (1.5, "array.map", lambda: f"{self.expr(t.array(source, n), d - 1)}.map({self.closure([(item, source)], elem, d - 1, times=n)})"),
            (1, "array.reverse", lambda: f"({{ let mut items = {e()}; items.{r.choice(['reverse()', f'rotate_left({r.randrange(n)})', f'swap(0, {n - 1})'])}; items }})"),
            (1, "array.try_from_vec", lambda: f"<{ty.name}>::try_from({self.expr(t.vec(elem), d - 1)}).unwrap_or({e()})"),
        ]
        if elem.copy:
            options.append((2, "aggregate.array_repeat", lambda: f"[{self.expr(elem, d - 1)}; {n}]"))
        if elem.total:
            options.append((1, "slice.sort", lambda: f"({{ let mut items = {e()}; items.{r.choice(['sort()', 'sort_unstable()', 'sort_by(|a, b| b.cmp(a))'])}; items }})"))
        return options

    def x_struct(self, ty, d):
        make = lambda field: self.expr(field, d - 1)
        options = [(6, "aggregate.struct", lambda: self.construct(ty.d.name, ty.d.tuple, ty.fields, make))]
        if ty.fields and not ty.d.tuple and not ty.d.drop:
            name, field = self.r.choice(ty.fields)
            options.append((1.5, "aggregate.struct_update", lambda: f"{ty.d.name} {{ {name}: {self.expr(field, d - 1)}, ..{self.expr(ty, d - 1)} }}"))
        return options

    def x_enum(self, ty, d):
        def variant():
            name, shape, fields = self.r.choice(ty.variants)
            return self.construct(f"{ty.d.name}::{name}", shape == "tuple", fields, lambda field: self.expr(field, d - 1), ty)
        return [(6, "aggregate.enum", variant)]

    def x_option(self, ty, d):
        t, r = self.t, self.r
        inner = ty.args[0]
        e = lambda: self.expr(ty, d - 1)
        value = lambda: self.expr(inner, d - 1)
        other = self.ty(lambda x: x.kind != "param" and x.leaves <= 8)
        x = self.fresh("x")
        options = [
            (4, "option.some", lambda: f"Some({value()})"),
            (1, "option.none", lambda: f"None::<{inner.name}>"),
            (2, "option.map", lambda: f"{self.expr(t.option(other), d - 1)}.map({self.closure([(x, other)], inner, d - 1)})"),
            (1, "option.and_then", lambda: f"{self.expr(t.option(other), d - 1)}.and_then({self.closure([(x, other)], ty, d - 1)})"),
            (1, "option.or", lambda: f"{e()}.{r.choice(['or', 'xor'])}({e()})"),
            (1, "option.filter", lambda: f"{e()}.filter({self.closure([(x, inner)], t.bool, d - 1, refs=[x])})"),
            (1, "result.ok", lambda: f"{self.expr(t.result(inner, other), d - 1)}.ok()"),
            (1, "result.err", lambda: f"{self.expr(t.result(other, inner), d - 1)}.err()"),
            (1.5, "vec.get", lambda: f"{self.expr(t.vec(inner), d - 1)}.{r.choice(['first()', 'last()', f'get({r.randrange(4)})'])}.cloned()"),
            (1, "vec.pop", lambda: f"({{ let mut items = {self.expr(t.vec(inner), d - 1)}; items.pop() }})"),
            (1, "option.then", lambda: f"{self.cond(d - 1)}.then({self.closure([], inner, d - 1)})"),
            (1, "iter.last", lambda: self.iter_option(inner, d)),
        ]
        if inner.kind == "int":
            n = inner.name
            options += [
                (3, "int.checked", lambda: f"{value()}.checked_{r.choice('add sub mul div rem'.split())}({value()})"),
                (1, "int.checked", lambda: f"{value()}.checked_{r.choice('shl shr pow'.split())}({self.expr(t.u32, d - 1)})"),
                (1, "cast.try_from", lambda: f"{n}::try_from({self.expr(t.ints[r.choice(list(INTS))], d - 1)}).ok()"),
            ]
        if inner.kind == "char":
            options.append((2, "char.from_u32", lambda: f"char::from_u32({self.expr(t.u32, d - 1)})"))
        if inner.kind == "nz":
            options.append((3, "nonzero.new", lambda: f"{inner.name}::new({self.expr(inner.base, d - 1)})"))
        if inner is t.usize:
            options.append((2, "string.find", lambda: f"{self.expr(t.string, d - 1)}.find({r.choice(CHARS)})"))
        return options

    def iter_option(self, inner, d):
        chain, elem = self.chain(d, inner)
        sinks = ["last()", "next()", f"nth({self.r.randrange(4)})"]
        if inner.total:
            sinks += ["max()", "min()"]
        x = self.fresh("x")
        sinks.append(f"find({self.closure([(x, elem)], self.t.bool, d - 1, refs=[x], times=6)})")
        return f"{chain}.{self.r.choice(sinks)}"

    def x_result(self, ty, d):
        t, r = self.t, self.r
        ok, err = ty.args
        other = self.ty(lambda x: x.kind != "param" and x.leaves <= 8)
        x = self.fresh("x")
        options = [
            (3, "result.ok", lambda: f"Ok::<{ok.name}, {err.name}>({self.expr(ok, d - 1)})"),
            (2, "result.err", lambda: f"Err::<{ok.name}, {err.name}>({self.expr(err, d - 1)})"),
            (2, "result.ok_or", lambda: f"{self.expr(t.option(ok), d - 1)}.ok_or({self.expr(err, d - 1)})"),
            (1.5, "result.map", lambda: f"{self.expr(t.result(other, err), d - 1)}.map({self.closure([(x, other)], ok, d - 1)})"),
            (1.5, "result.map_err", lambda: f"{self.expr(t.result(ok, other), d - 1)}.map_err({self.closure([(x, other)], err, d - 1)})"),
            (1, "result.and_then", lambda: f"{self.expr(t.result(other, err), d - 1)}.and_then({self.closure([(x, other)], ty, d - 1)})"),
            (1, "result.or_else", lambda: f"{self.expr(t.result(ok, other), d - 1)}.or_else({self.closure([(x, other)], ty, d - 1)})"),
        ]
        if ok.kind == "int":
            options += [
                (2, "string.parse_int", lambda: f"{self.expr(t.sstr, d - 1)}.parse::<{ok.name}>().map_err(|_{self.closure([], err, d - 1)[1:]})"),
                (2, "cast.try_from", lambda: f"{ok.name}::try_from({self.expr(t.ints[r.choice(list(INTS))], d - 1)}).map_err(|_{self.closure([], err, d - 1)[1:]})"),
            ]
        return options

    def x_box(self, ty, d):
        return [(5, "box.new", lambda: f"Box::new({self.expr(ty.args[0], d - 1)})")]

    def x_vec(self, ty, d):
        t, r = self.t, self.r
        elem = ty.args[0]
        e = lambda: self.expr(ty, d - 1)
        value = lambda: self.expr(elem, d - 1)
        x = self.fresh("x")
        edit = lambda change: f"({{ let mut items = {e()}; {change}; items }})"
        options = [
            (4, "vec.macro", lambda: "vec![" + ", ".join(value() for _ in range(r.randrange(1, 5))) + "]"),
            (1.5, "vec.repeat", lambda: f"vec![{value()}; {self.small(6)}]"),
            (0.7, "vec.new", lambda: f"Vec::<{elem.name}>::new()"),
            (1.5, "vec.from_array", lambda: f"{self.expr(t.array(elem, r.choice([1, 2, 3, 4])), d - 1)}.to_vec()"),
            (2.5, "iter.collect", lambda: f"{self.chain(d, elem)[0]}.collect::<{ty.name}>()"),
            (2, "vec.push", lambda: edit(f"items.push({value()})")),
            (1, "vec.insert", lambda: edit(f"items.insert(0, {value()})")),
            (1, "vec.truncate", lambda: edit(f"items.truncate({r.randrange(4)})")),
            (1, "vec.reverse", lambda: edit("items.reverse()")),
            (1, "vec.retain", lambda: edit(f"items.retain({self.closure([(x, elem)], t.bool, d - 1, refs=[x], times=4)})")),
            (1, "vec.dedup", lambda: edit("items.dedup()")),
            (1, "vec.extend", lambda: edit(f"items.extend({e()})")),
            (0.7, "vec.concat", lambda: f"[{e()}, {e()}].concat()"),
            (0.7, "vec.drain", lambda: f"({{ let mut items = {e()}; let end = items.len().min({r.randrange(4)}); items.drain(..end).collect::<{ty.name}>() }})"),
            (0.7, "vec.split_off", lambda: f"({{ let mut items = {e()}; let at = items.len() / 2; items.split_off(at) }})"),
        ]
        if elem.total:
            options.append((2, "slice.sort", lambda: edit(r.choice(["items.sort()", "items.sort_unstable()", "items.sort_by(|a, b| b.cmp(a))"]))))
        if elem is t.char:
            options.append((2, "string.chars", lambda: f"{self.expr(t.string, d - 1)}.chars().collect::<Vec<char>>()"))
        if elem is t.u8:
            options.append((2, "string.bytes", lambda: f"{self.expr(t.string, d - 1)}.into_bytes()"))
        if elem is t.string:
            options.append((2, "string.split", lambda: f"{self.expr(t.string, d - 1)}.split({r.choice([chr(39) + ' ' + chr(39), chr(39) + ',' + chr(39), chr(34) + 'l' + chr(34)])}).map(String::from).collect::<Vec<String>>()"))
        return options

    def x_string(self, ty, d):
        t, r = self.t, self.r
        e = lambda: self.expr(ty, d - 1)
        shown = self.ty(lambda x: x.kind in ("int", "float", "char", "bool", "sstr"))
        debugged = self.ty(lambda x: x.kind != "param" or self.reads(x))
        edit = lambda change: f"({{ let mut text = {e()}; {change}; text }})"
        return [
            (2, "string.from", lambda: f"String::from({self.expr(t.sstr, d - 1)})"),
            (2, "string.to_string", lambda: f"{self.expr(shown, d - 1)}.to_string()"),
            (3, "format.display", lambda: self.format(shown, d)),
            (3, "format.debug", lambda: f"format!(\"{r.choice(['{:?}', '{:?}', '{:#?}', '<{:?}>'])}\", {self.expr(debugged, d - 1)})"),
            (1, "string.case", lambda: f"{e()}.{r.choice(['to_uppercase', 'to_lowercase', 'to_ascii_uppercase'])}()"),
            (0.7, "string.repeat", lambda: f"{e()}.repeat({r.randrange(3)})"),
            (1.5, "string.concat", lambda: f"({e()} + &{e()})"),
            (1, "string.reverse", lambda: f"{e()}.chars().rev().collect::<String>()"),
            (1, "string.trim", lambda: f"{e()}.trim().to_string()"),
            (1, "string.replace", lambda: f"{e()}.replace({r.choice(STRS[1:])}, {r.choice(STRS[:3])})"),
            (1.5, "string.push", lambda: edit(f"text.push({self.expr(t.char, d - 1)})")),
            (1, "string.push_str", lambda: edit(f"text.push_str({self.expr(t.sstr, d - 1)})")),
            (0.7, "string.pop", lambda: edit("text.pop()")),
            (1, "string.join", lambda: f"{self.expr(t.vec(t.string), d - 1)}.join({r.choice(STRS)})"),
            (1, "string.from_utf8_lossy", lambda: f"String::from_utf8_lossy(&{self.expr(t.vec(t.u8), d - 1)}).into_owned()"),
            (1, "string.collect", lambda: f"{self.chain(d, t.char)[0]}.collect::<String>()"),
        ]

    def format(self, ty, d):
        specs = {"int": ["{}", "{:x}", "{:X}", "{:o}", "{:b}", "{:#x}", "{:08}", "{:+}", "{:>6}", "{:<6}|", "{:^7}|", "{:#010b}", "{:e}", "{:?}"],
                 "float": ["{}", "{:?}", "{:.3}", "{:e}", "{:10.2}|", "{:+.1}", "{:08.3}", "{:.0}", "{:E}"]}.get(ty.kind, ["{}", "{:?}", "{:>4}|", "{:5}!"])
        count = self.r.choice([1, 1, 2, 3])
        text = " ".join(self.r.choice(specs) for _ in range(count))
        return f"format!(\"{text}\", " + ", ".join(self.expr(ty, d - 1) for _ in range(count)) + ")"

    # ---- iterators ------------------------------------------------------------------------

    def chain(self, d, want=None):
        """An iterator over owned values and the type of its items; of `want` when that is given."""
        t, r = self.t, self.r
        elem = self.ty(lambda x: x.kind != "param" and x.leaves <= 6)
        ranged = self.ty(lambda x: x.kind == "int" and x.name not in ("i128", "u128", "isize", "usize"))
        start = self.r.randrange(0, 100)
        text, elem, both_ends = self.choose([
            (4, "iter.vec", lambda: (f"{self.expr(t.vec(elem), d - 1)}.into_iter()", elem, True)),
            (2, "iter.array", lambda: (f"{self.expr(t.array(elem, r.choice([1, 2, 3, 4])), d - 1)}.into_iter()", elem, True)),
            (3, "iter.range", lambda: (f"({start}{ranged.name}..{start + r.randrange(8)}{ranged.name})", ranged, True)),
            (1, "iter.range_inclusive", lambda: (f"({start}{ranged.name}..={start + r.randrange(6)}{ranged.name})", ranged, True)),
            (1.5, "iter.chars", lambda: (f"{self.expr(t.string, d - 1)}.chars()", t.char, True)),
            (1, "iter.bytes", lambda: (f"{self.expr(t.string, d - 1)}.bytes()", t.u8, True)),
            (1, "iter.option", lambda: (f"{self.expr(t.option(elem), d - 1)}.into_iter()", elem, True)),
            (1, "iter.repeat", lambda: (f"std::iter::repeat({self.expr(elem, d - 1)}).take({r.randrange(5)})", elem, False)),
            (0.7, "iter.successors", lambda: (f"std::iter::successors(Some({self.expr(ranged, d - 1)}), |x| Some(x.wrapping_mul(3).wrapping_add(1))).take({r.randrange(6)})", ranged, False)),
        ])
        for _ in range(r.choice([0, 0, 1, 1, 2, 3])):
            x = self.fresh("x")
            mapped = self.ty(lambda x: x.kind != "param" and x.leaves <= 6)
            test = lambda: self.closure([(x, elem)], t.bool, d - 1, refs=[x], times=6)
            step, elem, both_ends = self.choose([
                (3, "iter.map", lambda: (f".map({self.closure([(x, elem)], mapped, d - 1, times=6)})", mapped, both_ends)),
                (2, "iter.filter", lambda: (f".filter({test()})", elem, both_ends)),
                (1.5 if both_ends else 0, "iter.rev", lambda: (".rev()", elem, True)),
                (1, "iter.skip_take", lambda: (f".{r.choice(['skip', 'take'])}({r.randrange(4)})", elem, False)),
                (1, "iter.step_by", lambda: (f".step_by({r.randrange(1, 4)})", elem, False)),
                (1, "iter.enumerate", lambda: (".enumerate()", t.tuple([t.usize, elem]), False)),
                (1, "iter.take_while", lambda: (f".{r.choice(['take_while', 'skip_while'])}({test()})", elem, False)),
                (1, "iter.inspect", lambda: (f".inspect(|{x}| {x}.mix())", elem, both_ends)),
                (1, "iter.chain", lambda: (f".chain({self.expr(t.vec(elem), d - 1)})", elem, False)),
                (1, "iter.zip", lambda: (f".zip({self.expr(t.vec(mapped), d - 1)})", t.tuple([elem, mapped]), False)),
                (1, "iter.filter_map", lambda: (f".filter_map({self.closure([(x, elem)], t.option(mapped), d - 1, times=6)})", mapped, False)),
                (0.7, "iter.flat_map", lambda: (f".flat_map({self.closure([(x, elem)], t.vec(mapped), d - 1, times=6)})", mapped, False)),
            ])
            text += step
        if want is not None and elem is not want:
            x = self.fresh("x")
            text += f".map({self.closure([(x, elem)], want, d - 1, times=6)})"
            elem = want
        return text, elem

    # ---- patterns -------------------------------------------------------------------------

    def binding(self, ty, binds, mode, nomove):
        """A name bound by a pattern. `mode` is how the scrutinee is matched: "val", "ref", "mut", or "none"."""
        if mode == "none":
            return "_"
        name = self.fresh("p")
        if mode == "val" and not (nomove and not ty.copy):
            binds.append(Var(name, ty))
            return name
        binds.append(Var(f"(*{name})", ty, mode == "mut", bounded=True))
        return f"ref {name}" if mode == "val" else name

    def range_pattern(self, ty):
        bits, signed = INTS[ty.name]
        lowest = -(1 << bits - 1) if signed else 0
        lo, hi = sorted([self.int_value(ty), self.int_value(ty)])
        a, b = self.int_lit(ty, lo, bare=True), self.int_lit(ty, hi, bare=True)
        forms = [f"{a}..={b}", f"..={b}"]
        if lo < hi:
            forms.append(f"{a}..{b}")
        if not self.in_slice:
            forms.append(f"{a}..")
            if hi > lowest:
                forms.append(f"..{b}")
        return self.r.choice(forms)

    in_slice = 0

    def pattern(self, ty, d, binds, mode, nomove=False, total=False):
        """A pattern for a value of `ty`. One that is `total` matches every value."""
        r, k = self.r, ty.kind
        bind = lambda: self.binding(ty, binds, mode, nomove)
        named = 2 if mode != "none" else 0
        sub = lambda part, nomove=nomove: self.pattern(part, d - 1, binds, mode, nomove, total)
        if d <= 0 or self.chance(0.1):
            return bind() if self.chance(0.6) else "_"
        if k == "int" and not total:
            atom = lambda: r.choice([self.int_lit(ty, bare=True), self.range_pattern(ty)])
            return self.choose([
                (3, "pattern.literal", lambda: self.int_lit(ty, bare=True)),
                (3, "pattern.range", lambda: self.range_pattern(ty)),
                (1.5, "pattern.or", lambda: "(" + " | ".join(atom() for _ in range(r.choice([2, 3]))) + ")"),
                (named, "pattern.binding", bind),
                (named * 0.7, "pattern.at_binding", lambda: f"{bind()} @ {self.range_pattern(ty)}"),
                (1, "pattern.wildcard", lambda: "_"),
            ])
        if k == "char" and not total:
            lo, hi = r.choice([("'a'", "'z'"), ("'0'", "'9'"), ("'A'", "'Z'"), ("'\\0'", "'\\u{7f}'"), ("'\\u{80}'", "'\\u{10FFFF}'")])
            return self.choose([
                (2, "pattern.literal", lambda: r.choice(CHARS)),
                (3, "pattern.range", lambda: f"{lo}..={hi}"),
                (named, "pattern.binding", bind),
                (named * 0.5, "pattern.at_binding", lambda: f"{bind()} @ {lo}..={hi}"),
            ])
        if k == "bool" and not total:
            return r.choice([lambda: "true", lambda: "false", bind])()
        if k == "sstr" and not total and mode in ("val", "none") and self.chance(0.6):
            self.cov["pattern.literal"] += 1
            return r.choice(STRS)
        if k == "unit":
            return "()"
        if k == "tuple":
            self.cov["pattern.tuple"] += 1
            if len(ty.args) > 1 and self.chance(0.2):
                return f"({sub(ty.args[0])}, ..)"
            return "(" + ", ".join(sub(a) for a in ty.args) + ("," if len(ty.args) == 1 else "") + ")"
        if k == "array":
            return self.array_pattern(ty, d, binds, mode, nomove, total)
        if k == "struct" and not ty.d.packed:
            self.cov["pattern.struct"] += 1
            return self.fields_pattern(ty.d.name, ty.d.tuple, ty.fields, lambda part: sub(part, nomove or ty.d.drop))
        if k == "enum" and (not total or len(ty.variants) == 1):
            self.cov["pattern.enum"] += 1
            return self.variant_pattern(ty, r.choice(ty.variants), lambda part: sub(part, nomove or ty.d.drop))
        if k == "option" and not total:
            self.cov["pattern.option"] += 1
            return f"Some({sub(ty.args[0])})" if self.chance(0.7) else "None"
        if k == "result" and not total:
            self.cov["pattern.result"] += 1
            return f"Ok({sub(ty.args[0])})" if self.chance(0.5) else f"Err({sub(ty.args[1])})"
        if named:
            self.cov["pattern.binding"] += 1
        return bind() if self.chance(0.7) else "_"

    def fields_pattern(self, path, tuple_like, fields, sub):
        if not fields:
            return path
        if tuple_like:
            return f"{path}(" + ", ".join(sub(ty) for _, ty in fields) + ")"
        kept = [f for f in fields if self.chance(0.7)]
        parts = [f"{name}: {sub(ty)}" for name, ty in kept] + ([".."] if len(kept) < len(fields) else [])
        return f"{path} {{ {', '.join(parts)} }}"

    def variant_pattern(self, ty, variant, sub):
        name, shape, fields = variant
        return self.fields_pattern(f"{ty.d.name}::{name}", shape == "tuple", fields, sub)

    def array_pattern(self, ty, d, binds, mode, nomove, total):
        elem, n = ty.args[0], ty.n
        self.in_slice += 1
        sub = lambda: self.pattern(elem, d - 1, binds, mode, nomove, total)
        self.cov["pattern.array"] += 1
        if n <= 4 and self.chance(0.5):
            text = "[" + ", ".join(sub() for _ in range(n)) + "]"
        elif n >= 3 and mode != "none" and self.chance(0.5):
            rest = self.fresh("p")
            inner = self.t.array(elem, n - 2)
            self.cov["pattern.rest_binding"] += 1
            if mode == "val" and not (nomove and not elem.copy):
                binds.append(Var(rest, inner))
                text = f"[{sub()}, {rest} @ .., {sub()}]"
            else:
                binds.append(Var(f"(*{rest})", inner, mode == "mut", bounded=True))
                text = f"[{sub()}, {'ref ' if mode == 'val' else ''}{rest} @ .., {sub()}]"
        else:
            text = self.r.choice([lambda: f"[{sub()}, ..]", lambda: f"[.., {sub()}]", lambda: f"[{sub()}, .., {sub()}]" if n >= 2 else f"[{sub()}]"])()
        self.in_slice -= 1
        return text

    def slice_pattern(self, elem, d, binds):
        """A pattern for a `&[elem]`. What `rest @ ..` binds is put in scope as a vector."""
        self.in_slice += 1
        sub = lambda: self.pattern(elem, d - 1, binds, "ref")
        rest = self.fresh("p")

        def with_rest(text):
            self.cov["pattern.rest_binding"] += 1
            binds.append(Var(f"{rest}.to_vec()", self.t.vec(elem), owned=True))
            binds.append(Var(f"{rest}.len()", self.t.usize, owned=True))
            return text
        text = self.choose([
            (1, "pattern.slice_empty", lambda: "[]"),
            (2, "pattern.slice_exact", lambda: "[" + ", ".join(sub() for _ in range(self.r.choice([1, 2, 3]))) + "]"),
            (2, "pattern.slice_head", lambda: f"[{sub()}, ..]"),
            (1.5, "pattern.slice_tail", lambda: f"[.., {sub()}]"),
            (1.5, "pattern.slice_ends", lambda: f"[{sub()}, .., {sub()}]"),
            (2, "pattern.slice_rest", lambda: with_rest(f"[{sub()}, {rest} @ ..]")),
            (1, "pattern.slice_rest", lambda: with_rest(f"[{rest} @ .., {sub()}]")),
            (1.5, "pattern.slice_rest", lambda: with_rest(f"[{sub()}, {rest} @ .., {sub()}]")),
        ])
        self.in_slice -= 1
        return text

    def scrutinee(self, d, temporaries=True):
        """Something to match on: its text and what makes a pattern for it into a list of bindings.
        Without `temporaries` it borrows nothing that dies at the end of the statement."""
        t, r = self.t, self.r
        shaped = [x for x in self.pool if x.kind in ("struct", "enum", "tuple", "option", "result", "array") and x.leaves <= 12]
        ty = r.choice(shaped) if shaped and self.chance(0.6) else self.ty(lambda x: x.kind != "param" and x.leaves <= 12)
        elem = self.ty(lambda x: x.kind != "param" and x.leaves <= 6)

        def text_pattern(binds):
            if self.chance(0.7):
                return r.choice(STRS)
            name = self.fresh("p")
            binds.append(Var(f"{name}.to_string()", t.string, owned=True))
            return name
        return self.choose([
            (4, "match.by_value", lambda: (self.expr(ty, d - 1), ty, lambda binds, total=False: self.pattern(ty, 3, binds, "val", total=total))),
            (2, "match.by_reference", lambda: (f"&{{ {self.expr(ty, d - 1)} }}", ty, lambda binds, total=False: self.pattern(ty, 3, binds, "ref", total=total))),
            (2.5 if temporaries else 0, "match.slice", lambda: (f"{self.expr(t.vec(elem), d - 1)}.as_slice()", None, lambda binds, total=False: self.slice_pattern(elem, 3, binds))),
            (1 if temporaries else 0, "match.str", lambda: (f"{self.expr(t.string, d - 1)}.as_str()", None, lambda binds, total=False: text_pattern(binds))),
        ])

    def arm(self, make, d, guard=0.3, total=False):
        """The head of a match arm, with its bindings put in scope. The caller leaves the scope."""
        binds = []
        text = make(binds, total) if total else make(binds)
        state = self.enter()
        for var in binds:
            var.blk, var.loops = self.blk, self.ldepth
            self.scope.append(var)
        if self.chance(guard):
            self.cov["pattern.guard"] += 1
            text += f" if {self.cond(d - 1)}"
        return text, state

    def heads(self, d):
        """The scrutinee of a match and the heads of its arms but the last, which is `_` unless it is None."""
        text, ty, make = self.scrutinee(d)
        if ty is not None and ty.kind == "enum" and "&" not in text and self.chance(0.5):
            self.cov["match.exhaustive"] += 1
            makers = [(lambda binds, v=v: self.variant_pattern(ty, v, lambda part: self.pattern(part, 2, binds, "val", ty.d.drop, True)), 0)
                      for v in ty.variants]
            return text, makers, None
        return text, [(make, 0.3)] * self.r.choice([1, 1, 2, 3]), "_"

    def match_expr(self, ty, d):
        text, makers, last = self.heads(d)
        arms = []
        for make, guard in makers:
            head, state = self.arm(make, d, guard)
            arms.append(f"{head} => {self.expr(ty, d - 1)}")
            self.leave(state)
        if last:
            arms.append(f"{last} => {self.expr(ty, d - 1)}")
        return f"(match ({text}) {{ " + ", ".join(arms) + " })"

    # ---- statements -----------------------------------------------------------------------

    def block(self, count, d):
        for _ in range(count):
            if self.work > self.cap:
                break
            self.statement(d)

    def nested(self, count, d, loop=False, trips=1):
        """A block of statements one level in."""
        state, mult = self.enter(), self.mult
        self.ind += 1
        self.mult *= trips
        self.ldepth += loop
        self.block(count, d - 1)
        self.ldepth -= loop
        self.mult = mult
        self.ind -= 1
        self.leave(state)

    def statement(self, d):
        r = self.r
        deeper = 1 if d > 0 else 0
        loops = deeper if self.mult <= 40 else 0
        self.spend()
        self.choose([
            (6, "stmt.let", self.s_let),
            (1.5, "stmt.let_pattern", self.s_let_pattern),
            (5, "stmt.mix", self.s_mix),
            (4, "stmt.assign", self.s_assign),
            (2.5, "stmt.mutate", self.s_mutate),
            (2.5 * deeper, "stmt.if", lambda: self.s_if(d)),
            (2 * deeper, "stmt.match", lambda: self.s_match(d)),
            (1 * deeper, "match.by_mut_reference", lambda: self.s_match_mut(d)),
            (1 * deeper, "stmt.if_let", lambda: self.s_if_let(d)),
            (0.7, "stmt.let_else", lambda: self.s_let_else(d)),
            (1.5 * loops, "loop.for_range", lambda: self.s_for_range(d)),
            (1.2 * loops, "loop.for_iter", lambda: self.s_for_iter(d)),
            (1 * loops, "loop.while", lambda: self.s_while(d)),
            (1 * loops, "loop.loop", lambda: self.s_loop(d)),
            (0.6 * loops, "loop.while_let", lambda: self.s_while_let(d)),
            (2 if self.loops else 0, "loop.break_continue", self.s_jump),
            (0.6 if self.ret is not None else 0, "stmt.return", self.s_return),
            (1 * deeper, "stmt.block", lambda: self.s_block(d)),
            (1.2 * deeper, "closure.fn", lambda: self.s_closure(d)),
            (1 * deeper, "closure.fn_mut", lambda: self.s_closure_mut(d)),
            (1, "closure.fn_once", lambda: self.s_closure_once(d)),
            (1.5 * deeper, "panic.catch_unwind", lambda: self.s_catch(d)),
            (2, "raw.statement", self.s_raw),
            (1, "dyn.boxed", self.s_dyn),
            (0.8, "fnptr.local", self.s_fnptr),
            (0.8, "drop.move", self.s_move),
            (0.5, "static.mut_write", self.s_static),
            (0.5, "map.btree", self.s_map),
            (3, "scene", self.s_scene),
        ])

    def some_ty(self):
        """A type for a new value: sometimes that of something already in scope."""
        have = [v.ty for v in self.visible() if v.ty.kind not in ("fn", "dyn", "slice") and v.ty.name != "Self"]
        if have and self.chance(0.3):
            return self.r.choice(have)
        return self.ty(lambda x: x.kind != "param")

    def s_let(self):
        ty = self.some_ty()
        name, mut = self.fresh(), self.chance(0.55)
        self.emit(f"let {'mut ' if mut else ''}{name}: {ty.name} = {self.expr(ty)};")
        self.declare(name, ty, mut)

    def s_let_pattern(self):
        ty = self.ty(lambda x: x.kind in ("tuple", "struct", "array") and x.leaves <= 12) if self.chance(0.8) else self.some_ty()
        if ty.kind not in ("tuple", "struct", "array"):
            return self.s_let()
        value = self.expr(ty)
        binds = []
        self.emit(f"let {self.pattern(ty, 2, binds, 'val', total=True)}: {ty.name} = {value};")
        for var in binds:
            var.blk, var.loops = self.blk, self.ldepth
            self.scope.append(var)

    def s_mix(self):
        self.emit(f"{self.expr(self.some_ty())}.mix();")

    @contextlib.contextmanager
    def hidden(self, var):
        """Hides a variable while code is generated that must not name it."""
        var.hid += 1
        yield
        var.hid -= 1

    def s_assign(self):
        t, r = self.t, self.r
        targets = [v for v in self.scope if self.assignable(v) and v.ty.kind not in ("fn", "dyn", "slice")]
        if not targets:
            return self.s_let()
        v = r.choice(targets)
        place, ty = self.subplace(v.expr, v.ty)
        if ty.kind == "int" and self.chance(0.5):
            op = r.choice(["^=", "|=", "&=", "+=", "-=", "*=", "<<=", ">>="] if self.panicky else ["^=", "|=", "&="])
            rhs = self.expr(t.u32 if "<" in op or ">" in op else ty)
            return self.emit(f"{place} {op} {rhs};")
        others = [w for w in targets if w is not v and w.ty is v.ty]
        if others and self.chance(0.2):
            self.cov["stmt.swap"] += 1
            return self.emit(f"std::mem::swap(&mut {v.expr}, &mut {r.choice(others).expr});")
        if self.chance(0.2):
            # The old value is taken out in a statement of its own: nothing else of it is borrowed here.
            self.cov["stmt.mem_replace"] += 1
            old, place, ty = self.fresh(), v.expr, v.ty
            with self.hidden(v):
                taken = "std::mem::take(&mut {})".format(place) if ty.kind in ("vec", "string", "option", "int") and self.chance(0.5) \
                    else f"std::mem::replace(&mut {place}, {self.expr(ty)})"
            self.emit(f"let {old}: {ty.name} = {taken};")
            return self.declare(old, ty)
        self.emit(f"{place} = {self.expr(ty)};")

    def subplace(self, place, ty):
        """The place itself or a part of it that can be assigned on its own."""
        if self.chance(0.6):
            return place, ty
        if ty.kind == "struct" and ty.fields:
            name, field = self.r.choice(ty.fields)
            return self.subplace(f"{place}.{name}", field)
        if ty.kind == "tuple":
            i = self.r.randrange(len(ty.args))
            return self.subplace(f"{place}.{i}", ty.args[i])
        if ty.kind == "array":
            return self.subplace(f"{place}[{self.r.randrange(ty.n)}]", ty.args[0])
        if ty.kind == "box":
            return self.subplace(f"(*{place})", ty.args[0])
        return place, ty

    def s_mutate(self):
        """Changes a collection, a string or an option in place."""
        t, r = self.t, self.r
        targets = [v for v in self.scope if self.mutable(v) and v.ty.kind in ("vec", "string", "option", "array")]
        if not targets:
            ty = self.choose([(3, "type.vec", lambda: t.vec(self.ty(lambda x: x.kind != "param" and x.leaves <= 8))),
                              (1, "type.string", lambda: t.string)])
            name = self.fresh()
            self.emit(f"let mut {name}: {ty.name} = {self.expr(ty)};")
            return self.declare(name, ty, True)
        v = r.choice(targets)
        grow = self.assignable(v)
        with self.hidden(v):
            getattr(self, "m_" + v.ty.kind)(v, v.expr, grow)

    def m_vec(self, v, x, grow):
        t, r = self.t, self.r
        elem = v.ty.args[0]
        grow = grow or not elem.heap
        value = lambda: self.expr(elem, 2)
        item = self.fresh("x")
        self.choose([
            (4 if grow else 0, "vec.push", lambda: self.emit(f"{x}.push({value()});")),
            (2, "vec.pop", lambda: self.emit(f"{x}.pop().mix();")),
            (1.5 if grow else 0, "vec.insert", lambda: self.emit(f"{x}.insert({self.small(4)} % ({x}.len() + 1), {value()});")),
            (1.5, "vec.remove", lambda: self.emit(f"if !{x}.is_empty() {{ {x}.remove({self.small(4)} % {x}.len()).mix(); }}")),
            (1, "vec.swap_remove", lambda: self.emit(f"if !{x}.is_empty() {{ {x}.swap_remove({self.small(4)} % {x}.len()).mix(); }}")),
            (1, "vec.truncate", lambda: self.emit(f"{x}.truncate({r.randrange(6)});")),
            (1, "vec.reverse", lambda: self.emit(f"{x}.reverse();")),
            (1, "vec.dedup", lambda: self.emit(f"{x}.dedup();")),
            (1.5 if elem.total else 0, "slice.sort", lambda: self.emit(f"{x}.{r.choice(['sort()', 'sort_unstable()', 'sort_by(|a, b| b.cmp(a))'])};")),
            (1, "vec.retain", lambda: self.emit(f"{x}.retain({self.closure([(item, elem)], t.bool, 2, refs=[item], times=6)});")),
            (1 if grow else 0, "vec.extend_from_slice", lambda: self.emit(f"{x}.extend_from_slice(&[{value()}, {value()}]);")),
            (1, "vec.swap", lambda: self.emit(f"if {x}.len() > 1 {{ let last = {x}.len() - 1; {x}.swap(0, last); }}")),
            (1.5 if grow else 0, "vec.get_mut", lambda: self.emit(f"if let Some(slot) = {x}.{r.choice(['first_mut()', 'last_mut()', f'get_mut({r.randrange(4)})'])} {{ *slot = {value()}; }}")),
            (1, "vec.drain", lambda: self.emit(f"for taken in {x}.drain(..{x}.len().min({r.randrange(3)})) {{ taken.mix(); }}")),
            (1, "slice.rotate", lambda: self.emit(f"if !{x}.is_empty() {{ {x}.rotate_left(1); }}")),
            (1.5 if grow else 0, "iter.iter_mut", lambda: self.each_mut(x, elem)),
            (1, "slice.windows", lambda: self.emit(f"for part in {x}.{r.choice(['windows', 'chunks', 'rchunks'])}({r.randrange(1, 4)}) {{ part.mix(); }}")),
            (0.7, "slice.split_at", lambda: self.emit(f"{{ let (left, right) = {x}.split_at({x}.len() / 2); right.mix(); left.mix(); }}")),
            (0.7 if elem.total else 0, "slice.binary_search", lambda: self.emit(f"{{ let found = {x}.binary_search(&{value()}); found.is_ok().mix(); }}")),
        ])

    def each_mut(self, x, elem):
        item = self.fresh("x")
        self.emit(f"for {item} in {x}.iter_mut() {{")
        state = self.enter()
        self.declare(f"(*{item})", elem)
        self.emit(f"    *{item} = {self.expr(elem, 2)};")
        self.leave(state)
        self.emit("}")

    def m_array(self, v, x, grow):
        r = self.r
        elem, n = v.ty.args[0], v.ty.n
        grow = grow or not elem.heap
        self.choose([
            (2 if grow else 0, "array.fill", lambda: self.emit(f"{x}.fill({self.expr(elem, 2)});")),
            (2, "array.reverse", lambda: self.emit(f"{x}.{r.choice(['reverse()', f'swap(0, {n - 1})', f'rotate_right({r.randrange(n)})'])};")),
            (2 if grow else 0, "iter.iter_mut", lambda: self.each_mut(x, elem)),
            (1.5 if elem.total else 0, "slice.sort", lambda: self.emit(f"{x}.sort();")),
            (1 if n >= 2 else 0, "slice.split_at_mut", lambda: self.emit(f"{{ let (left, right) = {x}.split_at_mut({n // 2}); std::mem::swap(&mut left[0], &mut right[0]); }}")),
        ])

    def m_string(self, v, x, grow):
        t, r = self.t, self.r
        self.choose([
            (3, "string.push", lambda: self.emit(f"{x}.push({self.expr(t.char, 2)});")),
            (2, "string.push_str", lambda: self.emit(f"{x}.push_str({r.choice(STRS)});")),
            (2, "string.pop", lambda: self.emit(f"{x}.pop().mix();")),
            (1, "string.insert", lambda: self.emit(f"{x}.insert(0, {self.expr(t.char, 2)});")),
            (1, "string.remove", lambda: self.emit(f"if !{x}.is_empty() {{ {x}.remove(0).mix(); }}")),
            (1, "string.case", lambda: self.emit(f"{x}.make_ascii_uppercase();")),
            (1, "string.retain", lambda: self.emit(f"{x}.retain(|c| c != {r.choice(CHARS)});")),
            (0.5, "string.clear", lambda: self.emit(f"{x}.clear();")),
        ])

    def m_option(self, v, x, grow):
        inner = v.ty.args[0]
        grow = grow or not inner.heap
        value = lambda: self.expr(inner, 2)
        self.choose([
            (2, "option.take", lambda: self.emit(f"{x}.take().mix();")),
            (2 if grow else 0, "option.replace", lambda: self.emit(f"{x}.replace({value()}).mix();")),
            (2 if grow else 0, "option.get_or_insert", lambda: self.emit(f"{x}.get_or_insert({value()}).mix();")),
            (2 if grow else 0, "option.as_mut", lambda: self.emit(f"if let Some(slot) = {x}.as_mut() {{ *slot = {value()}; }}")),
            (1 if grow else 0, "option.insert", lambda: self.emit(f"{x}.insert({value()}).mix();")),
        ])

    def s_if(self, d):
        self.emit(f"if ({self.cond()}) {{")
        self.nested(self.r.choice([1, 2, 3]), d)
        if self.chance(0.5):
            self.emit("} else {")
            self.nested(self.r.choice([1, 2]), d)
        self.emit("}")

    def s_block(self, d):
        self.emit("{")
        self.nested(self.r.choice([2, 3, 4]), d)
        self.emit("}")

    def arm_body(self, head, state, d, count=None):
        """The block of an arm whose bindings `arm` put in scope."""
        self.emit(f"    {head} => {{")
        self.ind += 1
        self.nested(count or self.r.choice([1, 1, 2]), d)
        self.ind -= 1
        self.emit("    }")
        self.leave(state)

    def s_match(self, d):
        text, makers, last = self.heads(2)
        self.emit(f"match ({text}) {{")
        for make, guard in makers:
            head, state = self.arm(make, 2, guard)
            self.arm_body(head, state, d)
        if last:
            self.emit(f"    {last} => {{}}" if self.chance(0.5) else f"    {last} => {self.expr(self.some_ty(), 1)}.mix(),")
        self.emit("}")

    def s_match_mut(self, d):
        """Matches a variable through `&mut`, so that the arms can assign what the pattern binds."""
        targets = [v for v in self.scope if self.assignable(v) and v.ty.kind in ("struct", "enum", "tuple", "option", "result", "array")
                   and not self.mentions_param(v.ty)]
        if not targets:
            return self.s_match(d)
        v = self.r.choice(targets)
        make = lambda binds: self.pattern(v.ty, 3, binds, "mut")
        self.emit(f"match &mut {v.expr} {{")
        with self.hidden(v):
            for _ in range(self.r.choice([1, 2])):
                head, state = self.arm(make, 2, 0)
                self.arm_body(head, state, d)
        self.emit("    _ => {}")
        self.emit("}")

    def s_if_let(self, d):
        text, _, make = self.scrutinee(2)
        head, state = self.arm(make, 2, 0)
        self.emit(f"if let {head} = ({text}) {{")
        self.nested(self.r.choice([1, 2]), d)
        self.leave(state)
        if self.chance(0.5):
            self.emit("} else {")
            self.nested(1, d)
        self.emit("}")

    def diverge(self):
        """A statement that leaves the enclosing loop or function, when there is one to leave."""
        if self.loops:
            label, ty = self.r.choice(self.loops)
            if ty is None and self.chance(0.5):
                return f"continue {label};"
            return f"break {label}{'' if ty is None else ' ' + self.expr(ty, 1)};"
        if self.ret is not None:
            return f"return {self.expr(self.ret, 1)};"
        return None

    def s_let_else(self, d):
        leave = self.diverge()
        if leave is None:
            return self.s_if_let(d)
        text, _, make = self.scrutinee(2, temporaries=False)
        binds = []
        pattern = make(binds)
        self.emit(f"let {pattern} = ({text}) else {{ {leave} }};")
        for var in binds:
            var.blk, var.loops = self.blk, self.ldepth
            self.scope.append(var)

    def s_jump(self):
        self.emit(f"if ({self.cond()}) {{ {self.diverge()} }}")

    def s_return(self):
        self.emit(f"if ({self.cond()}) {{ return {self.expr(self.ret, 2)}; }}")

    def loop_body(self, label, value, d, trips, declare=(), frozen=False):
        """The statements of a loop. `frozen` when the loop borrows what is in scope for as long as it runs."""
        self.loops.append((label, value))
        state, mult = self.enter(), self.mult
        if frozen:
            self.mut_floor = len(self.scope)
        self.ind += 1
        self.mult *= trips
        self.ldepth += 1
        for var in declare:
            var.blk, var.loops = self.blk, self.ldepth
            self.scope.append(var)
        self.block(self.r.choice([1, 2, 2, 3, 4]), d - 1)
        self.ldepth -= 1
        self.mult = mult
        self.ind -= 1
        self.leave(state)
        self.loops.pop()

    def label(self):
        return self.fresh("'l")

    def s_for_range(self, d):
        r = self.r
        ty = self.ty(lambda x: x.kind == "int" and x.name not in ("i128", "u128"))
        start, trips = r.randrange(0, 50), r.choice([1, 2, 3, 4, 6, 8])
        name, label = self.fresh("i"), self.label()
        lo, hi = f"{start}{ty.name}", f"{start + trips}{ty.name}"
        span = r.choice([f"{lo}..{hi}", f"{lo}..={hi}", f"({lo}..{hi}).rev()", f"({lo}..{hi}).step_by(2)", f"{lo}..({hi} + ({self.expr(ty, 1)} % 3))"])
        if "%" in span and INTS[ty.name][1]:
            span = f"{lo}..{hi}"
        self.emit(f"{label}: for {name} in ({span}) {{")
        self.loop_body(label, None, d, trips + 2, [Var(name, ty)])
        self.emit("}")

    def s_for_iter(self, d):
        chain, elem = self.chain(2)
        name, label = self.fresh("x"), self.label()
        collected = self.chance(0.4)
        self.emit(f"{label}: for {name} in ({chain}{'.collect::<Vec<_>>()' if collected else ''}) {{")
        self.loop_body(label, None, d, 6, [Var(name, elem)], frozen=not collected)
        self.emit("}")

    def s_while(self, d):
        counter, label, trips = self.fresh("n"), self.label(), self.r.choice([1, 2, 3, 5, 8])
        self.emit(f"let mut {counter} = 0u32;")
        self.emit(f"{label}: while {counter} < {trips} && ({self.cond(1)}) {{")
        self.emit(f"    {counter} += 1;")
        self.loop_body(label, None, d, trips)
        self.emit("}")

    def s_loop(self, d):
        """A `loop`, which every second time is an expression whose `break`s carry its value."""
        counter, label, trips = self.fresh("n"), self.label(), self.r.choice([1, 2, 3, 5, 8])
        value = self.some_ty() if self.chance(0.5) else None
        name = self.fresh()
        self.emit(f"let mut {counter} = 0u32;")
        self.emit(f"let {name}: {value.name} = {label}: loop {{" if value else f"{label}: loop {{")
        self.emit(f"    {counter} += 1;")
        self.emit(f"    if {counter} > {trips} {{ break{' ' + self.expr(value, 1) if value else ''}; }}")
        self.loop_body(label, value, d, trips)
        self.emit("};" if value else "}")
        if value:
            self.declare(name, value)

    def s_while_let(self, d):
        elem = self.ty(lambda x: x.kind != "param" and x.leaves <= 8)
        stack, item, label = self.fresh(), self.fresh("x"), self.label()
        self.emit(f"let mut {stack}: Vec<{elem.name}> = {self.expr(self.t.vec(elem))};")
        self.emit(f"{stack}.truncate(6);")
        self.emit(f"{label}: while let Some({item}) = {stack}.pop() {{")
        self.loop_body(label, None, d, 6, [Var(item, elem)])
        self.emit("}")

    # ---- closures that outlive the statement that makes them ---------------------------------

    def closure_var(self, name, params, ret, fnmut=False):
        sig = Fn(name, [("val", ty) for _, ty in params], ret, cost=4, fnmut=fnmut, pointer=False)
        return Var(name, Ty("fn", f"closure {name}", sig=sig, copy=not fnmut), blk=self.blk, loops=self.ldepth)

    def unmovable(self):
        """What a `move` closure must not name: it would take the value, or the `&mut` it is behind."""
        return [v for v in self.scope if not v.hid and (not v.ty.copy or v.unique)]

    def closure_params(self):
        return [(self.fresh("a"), self.ty(lambda x: x.kind != "param" and x.leaves <= 12)) for _ in range(self.r.choice([0, 1, 1, 2, 3]))]

    def s_closure(self, d):
        """A closure that reads what it captures, or copies it in with `move`, and the code that calls it."""
        params, ret = self.closure_params(), self.ty(lambda x: x.kind != "param")
        name, move = self.fresh("f"), self.chance(0.35)
        self.emit("{")
        self.ind += 1
        state = self.enter()
        if move:
            self.cov["closure.move_copies"] += 1
            moved = self.unmovable()
            for v in moved:
                v.hid += 1
        self.emit(f"let {name} = {'move ' if move else ''}{self.closure(params, ret, 2)};")
        if move:
            for v in moved:
                v.hid -= 1
        else:
            self.cov["closure.by_reference"] += 1
            self.mut_floor = len(self.scope)
        self.scope.append(self.closure_var(name, params, ret))
        self.block(self.r.choice([2, 3]), d - 1)
        self.emit(f"{self.call(self.scope[state[0]].ty.sig, {}, None, 2)}.mix();")
        self.leave(state)
        self.ind -= 1
        self.emit("}")

    def s_closure_mut(self, d):
        """A closure that changes a variable it borrows, called several times, then the variable is read."""
        t = self.t
        held = self.ty(lambda x: x.kind != "param" and not x.heap)
        params, ret = self.closure_params(), self.ty(lambda x: x.kind != "param")
        state_name, name = self.fresh(), self.fresh("f")
        self.emit(f"let mut {state_name}: {held.name} = {self.expr(held)};")
        var = self.declare(state_name, held, True)
        self.emit("{")
        self.ind += 1
        state = self.enter()
        body = lambda: f"{state_name} = {self.expr(held, 2)}; {self.expr(ret, 2)}"
        self.emit(f"let mut {name} = {self.closure(params, ret, 2, body=body)};")
        var.hid += 1
        self.mut_floor = len(self.scope)
        self.scope.append(self.closure_var(name, params, ret, fnmut=True))
        self.block(self.r.choice([2, 3]), d - 1)
        self.emit(f"{self.call(self.scope[state[0]].ty.sig, {}, self.scope[state[0]], 2)}.mix();")
        self.leave(state)
        var.hid = 0
        self.ind -= 1
        self.emit("}")
        self.emit(f"{state_name}.mix();")

    def s_closure_once(self, d):
        """A closure that owns a value and gives it up when called, so it can be called once."""
        owned = self.ty(lambda x: x.kind != "param" and not x.copy)
        ret = self.ty(lambda x: x.kind != "param")
        outer, inner, name = self.fresh(), self.fresh(), self.fresh("f")
        self.emit(f"let {outer}: {owned.name} = {self.expr(owned)};")
        moved = self.unmovable()
        for v in moved:
            v.hid += 1

        def body():
            self.declare(inner, owned)
            return f"let {inner} = {outer}; let out = {self.expr(ret, 2)}; drop({inner}); out"
        self.emit(f"let {name} = move {self.closure([], ret, 2, body=body)};")
        for v in moved:
            v.hid -= 1
        self.emit(self.choose([
            (2, "closure.called_once", lambda: f"{name}().mix();"),
            (1, "closure.impl_fn_once", lambda: f"apply_once({name}).mix();"),
            (1, "closure.boxed_fn_once", lambda: f"apply_boxed(Box::new({name})).mix();"),
        ]))

    # ---- panics ---------------------------------------------------------------------------

    def s_catch(self, d):
        """Code that may panic, inside `catch_unwind`. What it dropped on the way out is in the checksum."""
        ty = self.ty(lambda x: x.kind != "param")
        name = self.fresh("r")
        self.emit(f"let {name} = catch_unwind(AssertUnwindSafe(|| -> {ty.name} {{")
        saved = (self.loops, self.ret, self.question)
        self.loops, self.ret, self.question = [], None, None
        self.panicky += 1
        state = self.enter()
        self.ind += 1
        self.block(self.r.choice([1, 2, 3]), d - 1)
        if self.chance(0.25):
            self.cov["panic.assert"] += 1
            compared = self.ty(lambda x: x.kind != "param" and x.leaves <= 6)
            # The message of `assert!` is the condition as rustc prints it back, which breaks
            # lines where blaze does not (found/assert-message.rs): the condition is kept short.
            held = self.fresh()
            self.emit(f"let {held}: bool = {self.cond()};")
            self.emit(self.r.choice([f"assert!({held});", f"assert!(!{held}, \"held {{}}\", {self.expr(self.t.u32, 1)});",
                                     f"assert_eq!({self.expr(compared, 2)}, {self.expr(compared, 2)});",
                                     f"if {held} {{ std::panic::panic_any({self.expr(self.t.u32, 1)}); }}"]))
        self.emit(self.expr(ty))
        self.ind -= 1
        self.leave(state)
        self.panicky -= 1
        self.loops, self.ret, self.question = saved
        self.emit("}));")
        self.emit(f"match {name} {{")
        self.emit("    Ok(value) => value.mix(),")
        self.emit("    Err(caught) => payload(caught),")
        self.emit("}")

    # ---- raw pointers ---------------------------------------------------------------------

    def s_raw(self):
        """Reads and writes through raw pointers that stay inside one local object."""
        t, r = self.t, self.r
        elem = self.ty(lambda x: x.pod and x.kind != "param" and x.leaves <= 8)
        n = r.choice([1, 2, 3, 4, 8])
        array = t.array(elem, n)
        a, b = self.fresh(), self.fresh()
        i, j = self.small(n), self.small(n)
        integer = t.ints[r.choice(["u16", "u32", "u64", "u128", "i32", "i64"])]
        size = INTS[integer.name][0] // 8
        # `offset_from` panics for a zero-sized element.
        distance = "(start.add(x).offset_from(start.add(y)) as i64).mix(); " if self.scalars(elem) else ""
        self.emit(f"let mut {a}: {array.name} = {self.expr(array)};")
        self.choose([
            (2, "raw.read", lambda: self.emit(f"(unsafe {{ *{a}.as_ptr().add({i}) }}).mix();")),
            (2, "raw.write", lambda: self.emit(f"{{ let value = {self.expr(elem, 2)}; let at = {i}; unsafe {{ {a}.as_mut_ptr().add(at).write(value) }}; }}")),
            (1.5, "raw.swap", lambda: self.emit(f"{{ let (x, y) = ({i}, {j}); let start = {a}.as_mut_ptr(); unsafe {{ std::ptr::swap(start.add(x), start.add(y)) }}; }}")),
            (1.5, "raw.copy", lambda: self.emit(f"{{ let {b}: {array.name} = {self.expr(array)}; unsafe {{ std::ptr::copy_nonoverlapping({b}.as_ptr(), {a}.as_mut_ptr(), {n}) }}; }}")),
            (1.5, "raw.helper_call", lambda: self.emit(f"{{ let at = {i}; let value = {self.expr(elem, 2)}; unsafe {{ raw_set({a}.as_mut_ptr(), at, value); raw_get({a}.as_ptr(), at).mix(); }} }}")),
            (1.5, "raw.offset", lambda: self.emit(f"{{ let (x, y) = ({i}, {j}); let start = {a}.as_ptr(); unsafe {{ {distance}(start.add(x) < start.add(y)).mix(); }} }}")),
            (1.5, "raw.from_raw_parts", lambda: self.emit(f"(unsafe {{ std::slice::from_raw_parts({a}.as_ptr().add({n - 1}), 1) }}).mix();")),
            (1.5, "raw.mut_reference", lambda: self.emit(f"{{ let value = {self.expr(elem, 2)}; let slot = &mut {a}[{r.randrange(n)}] as *mut {elem.name}; unsafe {{ *slot = value; (*slot).mix(); }} }}")),
            (1.5, "raw.bytes", lambda: self.emit(f"{{ let words: [{integer.name}; 3] = {self.expr(t.array(integer, 3))}; (unsafe {{ std::slice::from_raw_parts(words.as_ptr() as *const u8, {3 * size}) }}).mix(); }}")),
            (1.5, "raw.read_unaligned", lambda: self.emit(f"{{ let bytes: [u8; 17] = {self.expr(t.array(t.u8, 17))}; (unsafe {{ (bytes.as_ptr().add({r.randrange(17 - size + 1)}) as *const {integer.name}).read_unaligned() }}).mix(); }}")),
            (1.5, "raw.box_round_trip", lambda: self.emit(f"{{ let value = {self.expr(elem, 2)}; let raw = Box::into_raw(Box::new({a})); unsafe {{ (*raw)[{r.randrange(n)}] = value; Box::from_raw(raw).mix(); }} }}")),
            (1.5, "raw.linked_nodes", lambda: self.emit(f"{{ let last = Node {{ value: {a}[0], next: std::ptr::null() }}; let first = Node {{ value: {a}[{n - 1}], next: &last }}; (unsafe {{ walk(&first) }}).mix(); }}")),
            (2, "raw.packed_field", lambda: self.packed_field()),
        ])
        self.emit(f"{a}.mix();")
        self.declare(a, array, True)

    def packed_field(self):
        packed = [x for x in self.pool if x.kind == "struct" and x.d.packed and x.fields]
        if not packed:
            return
        ty = self.r.choice(packed)
        name, field = self.r.choice(ty.fields)
        whole = self.fresh()
        self.emit(f"let mut {whole}: {ty.name} = {self.expr(ty)};")
        self.emit(f"{{ let value = {self.expr(field, 2)}; unsafe {{ std::ptr::addr_of_mut!({whole}.{name}).write_unaligned(value); std::ptr::addr_of!({whole}.{name}).read_unaligned().mix(); }} }}")
        self.declare(whole, ty, True)

    # ---- trait objects, function pointers, moves, statics, maps --------------------------------

    def s_dyn(self):
        traits = [tr for tr in self.traits if tr.dyn]
        if not traits:
            return self.s_let()
        tr = self.r.choice(traits)
        name = self.fresh("o")
        make = lambda: f"Box::new({self.expr(self.r.choice(tr.impls))})"
        if self.chance(0.6):
            self.emit(f"let mut {name}: Box<dyn {tr.name}> = {make()};")
            self.scope.append(Var(name, Ty("dyn", f"Box<dyn {tr.name}>", trait=tr, copy=False), True, self.blk, self.ldepth))
            return
        self.cov["dyn.vec_of_boxes"] += 1
        each = self.fresh("o")
        method = self.r.choice(tr.methods)
        self.emit(f"let mut {name}: Vec<Box<dyn {tr.name}>> = vec![{', '.join(make() for _ in range(self.r.choice([1, 2, 3])))}];")
        args = ", ".join(self.expr(ty, 1) for _, ty in method.params)
        self.emit(f"for {each} in {name}.{'iter_mut' if method.self_mode == 'mut' else 'iter'}() {{ {each}.{method.name}({args}).mix(); }}")

    def s_fnptr(self):
        pointed = [f for f in self.fns if self.pointer_type(f) and not f.panics]
        if not pointed:
            return self.s_let()
        f = self.r.choice(pointed)
        name = self.fresh("g")
        kind = self.pointer_type(f)
        sig = Fn(name, f.params, f.ret, cost=f.cost, abi=f.abi)
        self.emit(f"let {name}: {kind} = {self.function_value(f, 2)};")
        self.scope.append(Var(name, Ty("fn", kind, sig=sig), blk=self.blk, loops=self.ldepth))

    def s_move(self):
        """Gives a value another owner or drops it early; its old name is not used again."""
        t = self.t
        movable = [v for v in self.scope if not v.hid and not v.ty.copy and v.blk == self.blk and self.scope.index(v) >= self.mut_floor
                   and v.expr.isidentifier() and v.ty.kind not in ("fn", "dyn") and not self.mentions_param(v.ty)]
        if not movable:
            return self.s_let()
        v = self.r.choice(movable)
        name = self.fresh()
        ty, text = self.choose([
            (3, "drop.move_let", lambda: (v.ty, v.expr)),
            (1, "drop.move_into_box", lambda: (t.boxed(v.ty), f"Box::new({v.expr})")),
            (1, "drop.move_into_option", lambda: (t.option(v.ty), f"Some({v.expr})")),
            (1, "drop.move_into_tuple", lambda: (t.tuple([t.u8, v.ty]), f"(7u8, {v.expr})")),
            (2, "drop.explicit", lambda: (None, v.expr)),
        ])
        v.hid = 1
        if ty is None:
            return self.emit(f"drop({text});")
        self.emit(f"let {name}: {ty.name} = {text};")
        self.declare(name, ty)

    def s_static(self):
        if not self.statics:
            return self.s_mix()
        name, ty = self.r.choice(self.statics)
        self.emit(f"{{ let value = {self.expr(ty)}; unsafe {{ {name} = value; }} }}")

    def s_map(self):
        t, r = self.t, self.r
        key = self.ty(lambda x: x.total and x.kind != "param" and x.leaves <= 4)
        value = self.ty(lambda x: x.kind != "param" and x.leaves <= 8)
        name = self.fresh()
        self.emit(f"let mut {name}: BTreeMap<{key.name}, {value.name}> = BTreeMap::new();")
        for _ in range(r.choice([1, 2, 4])):
            self.emit(f"{name}.insert({self.expr(key, 2)}, {self.expr(value, 2)}).mix();")
        self.emit(r.choice([
            f"for (key, value) in &{name} {{ key.mix(); value.mix(); }}",
            f"for (key, value) in {name}.iter().rev() {{ key.mix(); value.mix(); }}",
            f"{name}.remove(&{self.expr(key, 1)}).mix();",
            f"{name}.get(&{self.expr(key, 1)}).cloned().mix();",
            f"{name}.entry({self.expr(key, 1)}).or_insert({self.expr(value, 1)}).mix();",
            f"{name}.first_key_value().map(|(key, _)| key.clone()).mix();",
        ]))
        self.emit(f"{name}.len().mix();")

    # ---- items ----------------------------------------------------------------------------

    def make_constants(self):
        """Constants and statics, which the compiler under test has to evaluate itself."""
        self.constant = True
        for _ in range(self.r.choice([0, 1, 2, 3])):
            ty = self.ty(lambda x: x.const and x.kind != "param" and x.leaves <= 12)
            kind = self.choose([(3, "const.item", lambda: "const"), (2, "static.item", lambda: "static"), (1.5, "static.mut_item", lambda: "static mut")])
            name = self.fresh("K")
            if kind == "static mut":
                ty = self.ty(lambda x: x.kind in ("int", "bool", "float"))
                self.statics.append((name, ty))
            self.emit(f"{kind} {name}: {ty.name} = {self.lit(ty)};")
            self.scope.append(Var(f"(unsafe {{ {name} }})" if kind == "static mut" else name, ty))
        self.constant = False

    def const_int(self, ty):
        """An integer expression a constant may be initialised with."""
        r, n = self.r, ty.name
        bits = INTS[n][0]
        a, b = self.int_lit(ty), self.int_lit(ty)
        other = self.t.ints[r.choice(list(INTS))]
        self.cov["const.arithmetic"] += 1
        return r.choice([
            f"{a}.wrapping_{r.choice(['add', 'sub', 'mul'])}({b})",
            f"({a} {r.choice(['^', '&', '|'])} {b})",
            f"{a}.{r.choice(['rotate_left', 'rotate_right', 'wrapping_shl', 'wrapping_shr'])}({r.randrange(bits + 3)})",
            f"({self.int_lit(other)} as {n})",
            f"({a}.{r.choice(['count_ones', 'leading_zeros', 'trailing_zeros'])}() as {n})",
            f"(if {a} {r.choice(['<', '>=', '=='])} {b} {{ {a} }} else {{ {b} }})",
            f"(match {a}.checked_{r.choice(['add', 'mul', 'sub'])}({b}) {{ Some(sum) => sum, None => {b} }})",
            f"{a}.saturating_{r.choice(['add', 'sub', 'mul'])}({b})",
            f"{a}.{r.choice(['swap_bytes', 'reverse_bits', 'wrapping_neg'])}()",
            f"({r.choice(CHARS)} as u32 as {n})",
            f"(std::mem::size_of::<{self.ty(lambda x: x.kind != 'param').name}>() as {n})",
        ])

    def make_traits(self):
        for _ in range(self.r.choice([0, 1, 1, 2])):
            self.make_trait()

    def make_trait(self):
        t, r = self.t, self.r
        tr = Trait(self.fresh("T"))
        small = lambda: self.ty(lambda x: x.kind != "param" and x.leaves <= 12)
        for _ in range(r.choice([1, 1, 2, 3])):
            m = Fn(self.fresh("m"), [("val", small()) for _ in range(r.choice([0, 1, 1, 2]))], small(), cost=6)
            m.self_mode = "mut" if self.chance(0.25) else "ref"
            tr.methods.append(m)
        candidates = [x for x in self.pool if x.leaves <= 16]
        tr.impls = r.sample(candidates, min(len(candidates), r.choice([1, 2, 3, 4])))
        self.cov["trait.definition"] += 1
        self.emit(f"pub trait {tr.name} {{")
        for m in tr.methods:
            self.emit(f"    {self.method_head(m)};")
        default = None
        if self.chance(0.5):
            default = Fn(self.fresh("m"), [("val", small())], small(), cost=16)
            default.self_mode = "ref"
            self.cov["trait.default_method"] += 1
            self.method_body(tr, default, None)
        self.emit("}")
        for ty in tr.impls:
            self.cov["trait.impl"] += 1
            self.emit(f"impl {tr.name} for {ty.name} {{")
            for m in tr.methods:
                self.method_body(tr, m, ty)
            self.emit("}")
        if default:
            tr.methods.append(default)
        self.traits.append(tr)

    def method_head(self, m, names=None):
        names = names or [f"a{i}" for i in range(len(m.params))]
        receiver = "&mut self" if m.self_mode == "mut" else "&self"
        return f"fn {m.name}({', '.join([receiver] + [f'{n}: {ty.name}' for n, (_, ty) in zip(names, m.params)])}) -> {m.ret.name}"

    def method_body(self, tr, m, ty):
        """A method of an impl for `ty`, or the trait's default method when `ty` is None."""
        names = [self.fresh("a") for _ in m.params]
        self.ind += 1
        self.emit(f"{self.method_head(m, names)} {{")
        state, work, self.work, self.cap = self.enter(), self.work, 0, 400
        self.ind += 1
        for name, (_, param) in zip(names, m.params):
            self.declare(name, param)
        if ty is None:
            required = self.r.choice(tr.methods)
            args = ", ".join(self.expr(param, 1) for _, param in required.params)
            if required.self_mode == "ref":
                self.emit(f"self.{required.name}({args}).mix();")
        else:
            self.declare("(*self)", ty, m.self_mode == "mut", bounded=True)
            self.block(self.r.choice([0, 0, 1, 2]), 1)
        self.emit(self.expr(m.ret))
        self.ind -= 1
        self.leave(state)
        m.cost, self.work = max(m.cost, self.work), work
        self.emit("}")
        self.ind -= 1
        self.functions += 1

    def parameter(self, pointed):
        """A parameter of a helper function: how it is passed, and what."""
        t, r = self.t, self.r
        value = lambda: self.ty(lambda x: x.kind != "param")
        dyn = [tr for tr in self.traits if tr.dyn and any(m.self_mode == "ref" for m in tr.methods)]
        return self.choose([
            (14, "abi.by_value", lambda: ("val", value())),
            (2, "abi.by_reference", lambda: ("ref", value())),
            (1.5, "abi.by_mut_reference", lambda: ("mut", value())),
            (1.2, "abi.slice", lambda: ("slice", self.ty(lambda x: x.kind != "param" and x.leaves <= 8))),
            (0.6, "abi.mut_slice", lambda: ("mutslice", self.ty(lambda x: x.kind != "param" and x.leaves <= 8 and not x.heap))),
            (0.8, "abi.str", lambda: ("str", None)),
            (1 if dyn else 0, "dyn.parameter", lambda: ("dyn", r.choice(dyn))),
            (1 if pointed else 0, "fnptr.parameter", lambda: ("fn", r.choice(pointed))),
        ])

    def parameter_text(self, name, mode, ty):
        return {"val": lambda: f"{name}: {ty.name}", "ref": lambda: f"{name}: &{ty.name}", "mut": lambda: f"{name}: &mut {ty.name}",
                "slice": lambda: f"{name}: &[{ty.name}]", "mutslice": lambda: f"{name}: &mut [{ty.name}]", "str": lambda: f"{name}: &str",
                "dyn": lambda: f"{name}: &dyn {ty.name}", "fn": lambda: f"{name}: {self.pointer_type(ty)}", "depth": lambda: f"{name}: u32"}[mode]()

    def declare_parameter(self, name, mode, ty):
        t = self.t
        if mode in ("val", "depth"):
            self.declare(name, t.u32 if mode == "depth" else ty)
        elif mode in ("ref", "mut"):
            self.declare(f"(*{name})", ty, mode == "mut", bounded=True)
        elif mode in ("slice", "mutslice"):
            if mode == "mutslice":
                self.emit(f"{{ let value = {self.expr(ty, 2)}; if let Some(slot) = {name}.first_mut() {{ *slot = value; }} }}")
            self.scope.append(Var(f"{name}.to_vec()", t.vec(ty), blk=self.blk, owned=True))
            self.scope.append(Var(f"{name}.len()", t.usize, blk=self.blk, owned=True))
            self.scope[-1].unique = self.scope[-2].unique = mode == "mutslice"
        elif mode == "str":
            self.scope.append(Var(f"{name}.to_string()", t.string, blk=self.blk, owned=True))
        elif mode == "dyn":
            self.scope.append(Var(name, Ty("dyn", f"&dyn {ty.name}", trait=ty), blk=self.blk))
        else:
            sig = Fn(name, ty.params, ty.ret, cost=ty.cost, abi=ty.abi)
            self.scope.append(Var(name, Ty("fn", self.pointer_type(ty), sig=sig), blk=self.blk))

    def make_fn(self):
        self.choose([
            (8, "fn.plain", self.plain_fn),
            (2.5, "generic.function", self.generic_fn),
            (2, "abi.extern_c", self.extern_fn),
            (1, "fn.recursive", self.recursive_fn),
            (1, "closure.returned", self.closure_fn),
        ])

    def fn_item(self, f, head, body, statements, cap=1500):
        """Emits a function: `head {`, its parameters in scope, some statements, then `body`'s lines."""
        self.emit(self.r.choice(["#[inline(never)]"] * 5 + ["#[inline]", "#[inline(always)]", "", "", ""]))
        if not self.lines[-1].strip():
            self.lines.pop()
        self.emit(f"{head} {{")
        state, work, self.work, self.cap = self.enter(), self.work, 0, cap
        self.ind += 1
        for name, mode, ty in f.names:
            self.declare_parameter(name, mode, ty)
        self.ret = f.ret
        self.question = f.ret if f.ret.kind in ("option", "result") else None
        self.panicky += f.panics
        self.block(statements, 2)
        body()
        self.panicky -= f.panics
        self.ret = self.question = None
        self.ind -= 1
        self.leave(state)
        f.cost, self.work = self.work + 2, work
        self.emit("}")
        self.fns.append(f)
        self.functions += 1

    def scalars(self, ty):
        """The scalar kinds a value of the type is made of, as far as the calling convention cares."""
        k = ty.kind
        if k in ("int", "bool", "char", "nz", "box", "param"):
            return ["int"]
        if k == "float":
            return ["float"]
        if k == "unit":
            return []
        if k == "sstr":
            return ["int", "int"]
        if k in ("vec", "string"):
            return ["int"] * 3
        if k == "array":
            return self.scalars(ty.args[0]) * ty.n
        if k == "struct":
            return [s for _, field in ty.fields for s in self.scalars(field)]
        if k == "enum":
            return ["int"] + [s for _, _, fields in ty.variants for _, field in fields for s in self.scalars(field)]
        if k in ("option", "result"):
            return ["int"] + [s for a in ty.args for s in self.scalars(a)]
        return [s for a in ty.args for s in self.scalars(a)]

    def count_abi(self, f):
        """Counts how each value parameter and the result of a function are passed."""
        for where, ty in [("argument", ty) for mode, ty in f.params if mode == "val"] + [("return", f.ret)]:
            parts = self.scalars(ty)
            kind = ("zero_sized" if not parts else "scalar_float" if parts == ["float"] else "scalar_int" if len(parts) == 1
                    else "in_memory" if len(parts) > 2 else "float_int_pair" if len(set(parts)) == 2 else "float_pair" if parts[0] == "float" else "int_pair")
            self.cov[f"abi.{where}_{kind}"] += 1

    def signature(self, f, generics=""):
        self.count_abi(f)
        params = ", ".join(self.parameter_text(name, mode, ty) for name, mode, ty in f.names)
        return f"pub {f.abi}fn {f.name}{generics}({params}) -> {f.ret.name}"

    def plain_fn(self):
        r = self.r
        pointed = [f for f in self.fns if self.pointer_type(f) and not f.panics]
        params = [self.parameter(pointed) for _ in range(r.choice([0, 1, 1, 2, 2, 3, 3, 4, 5, 6, 8, 10, 13]))]
        f = Fn(self.fresh("f"), params, self.ty(lambda x: x.kind != "param"), panics=self.chance(0.2))
        f.names = [(self.fresh("a"), mode, ty) for mode, ty in params]
        self.fn_item(f, self.signature(f), lambda: self.emit(self.expr(f.ret)), r.choice([0, 0, 1, 1, 2, 3, 4]))

    def extern_fn(self):
        """A function with the C calling convention, which takes and returns what C can."""
        r, t = self.r, self.t
        c_type = lambda: self.ty(lambda x: x.cabi)
        params = [("val", c_type()) for _ in range(r.choice([0, 1, 2, 2, 3, 4, 6, 9]))]
        f = Fn(self.fresh("f"), params, t.unit if self.chance(0.1) else c_type(), abi='extern "C" ')
        f.names = [(self.fresh("a"), mode, ty) for mode, ty in params]
        self.fn_item(f, self.signature(f), lambda: self.emit(self.expr(f.ret)), r.choice([0, 1, 2]))

    def generic_fn(self):
        """A function over type parameters, some bound by a generated trait whose methods it calls."""
        t, r = self.t, self.r
        tparams = []
        for _ in range(r.choice([1, 1, 2])):
            param = t.param(self.fresh("G"))
            bounded = [tr for tr in self.traits if any(m.self_mode == "ref" for m in tr.methods)]
            param.trait = r.choice(bounded) if bounded and self.chance(0.4) else None
            tparams.append((param, param.trait))
        shapes = lambda p: r.choice([p, p, p, t.option(p), t.vec(p), t.array(p, 2), t.tuple([p, t.u8]), t.boxed(p)])
        some = lambda: shapes(r.choice(tparams)[0]) if self.chance(0.6) else self.ty(lambda x: x.kind != "param")
        params = [(r.choice(["val", "val", "ref"]), p) for p, _ in tparams] + [(r.choice(["val", "val", "val", "ref", "mut"]), some()) for _ in range(r.choice([0, 1, 2, 4]))]
        f = Fn(self.fresh("f"), params, some(), tparams=tparams)
        f.names = [(self.fresh("a"), mode, ty) for mode, ty in params]
        bounds = ", ".join(f"{p.name}: {self.BOUND}{' + ' + tr.name if tr else ''}" for p, tr in tparams)
        self.fn_item(f, self.signature(f, f"<{bounds}>"), lambda: self.emit(self.expr(f.ret)), r.choice([0, 1, 2, 3]))

    def recursive_fn(self):
        """A function that calls itself with a depth that counts down to zero."""
        r = self.r
        carried = self.ty(lambda x: x.kind != "param")
        extra = [("val", self.ty(lambda x: x.kind != "param")) for _ in range(r.choice([0, 0, 1, 2]))]
        f = Fn(self.fresh("f"), [("depth", None), ("val", carried)] + extra, carried)
        f.names = [(self.fresh("a"), mode, ty) for mode, ty in f.params]
        depth = f.names[0][0]

        def body():
            self.emit(f"if {depth} == 0 {{ return {self.expr(carried)}; }}")
            again = lambda: f"{f.name}({', '.join([f'{depth} - 1'] + [self.expr(ty, 2) for _, ty in f.params[1:]])})"
            self.emit(again() if self.chance(0.8) else f"sel::<{carried.name}>({self.cond()}, {again()}, {self.expr(carried, 1)})")
        self.fn_item(f, self.signature(f), body, r.choice([0, 1, 2]), cap=150)
        f.cost *= 9

    def closure_fn(self):
        """A function that returns a closure over its arguments."""
        r = self.r
        value = lambda: self.ty(lambda x: x.kind != "param" and x.leaves <= 12)
        params = [("val", value()) for _ in range(r.choice([1, 1, 2, 3]))]
        inner = Fn("", [("val", value()) for _ in range(r.choice([0, 1, 1, 2]))], self.ty(lambda x: x.kind != "param"))
        f = Fn(self.fresh("f"), params, self.t.unit, curried=inner, pointer=False)
        f.names = [(self.fresh("a"), mode, ty) for mode, ty in params]
        kind = f"Fn({', '.join(ty.name for _, ty in inner.params)}) -> {inner.ret.name}"
        boxed = self.chance(0.4)
        self.cov["closure.boxed_dyn_fn" if boxed else "closure.impl_fn_returned"] += 1
        head = self.signature(f).rsplit(" -> ", 1)[0] + (f" -> Box<dyn {kind}>" if boxed else f" -> impl {kind}")

        def body():
            text = "move " + self.closure([(self.fresh("x"), ty) for _, ty in inner.params], inner.ret, 2)
            self.emit(f"Box::new({text})" if boxed else text)
        self.fn_item(f, head, body, 0)

    def make_run(self):
        self.emit("pub fn run() {")
        state = self.enter()
        self.ind += 1
        self.work, self.cap = 0, WORK
        self.block(self.r.randint(6, 22), 3)
        self.ind -= 1
        self.leave(state)
        self.emit("}")
        self.functions += 1

    def case(self):
        t = self.t
        self.statics, self.constant, self.cap = [], False, WORK
        self.pool = [self.scalar() for _ in range(6)]
        self.pool = list(dict.fromkeys(self.pool))
        self.phase("adts")
        self.make_adts()
        for _ in range(self.r.choice([2, 4, 6])):
            self.ty(lambda x: x.kind != "param")
        self.phase("constants")
        self.make_constants()
        self.phase("traits")
        self.make_traits()
        for index in range(self.r.choice([1, 2, 3, 4, 5, 6, 8])):
            self.phase(f"fn{index}")
            self.make_fn()
        self.phase("run")
        self.make_run()
        const_fns = ConstFns(self.key)
        self.lines += const_fns.case()
        self.lines.insert(self.lines.index("    pub fn run() {") + 1, "        const_fns();")
        self.cov["const.fn"] += len(const_fns.fns)
        return self.lines

    def phase(self, name):
        """Notes that a part of the case starts. From the part named by `cut` on, the case is
        generated from other random numbers: the same case with that part and the rest edited."""
        self.phases.append(name)
        if self.cut and self.cut[0] == name:
            self.r = random.Random(f"{self.key}/{self.cut[1]}")


class Trait:
    """A generated trait: its methods and the types that implement it."""

    def __init__(self, name):
        self.name, self.methods, self.impls = name, [], []
        self.dyn, self.generic_only = True, False


class NoValue(Exception):
    """The generator was asked for a value it has no way to make here."""


def case(seed, index):
    """The lines of case `index` of `seed`, and what was generated in it."""
    for attempt in range(8):
        gen = Gen(seed, index if attempt == 0 else f"{index}/{attempt}")
        gen.name = f"c{index}"
        try:
            lines = gen.case()
        except NoValue:
            continue
        gen.index = index if attempt == 0 else f"{index}/{attempt}"
        return lines, gen
    raise SystemExit(f"case {index} of seed {seed}: no program after 8 attempts")


def variant(seed, index, name, cut):
    """The case `index` of `seed` generated anew from the part `cut` names, or None when that gave no program."""
    gen = Gen(seed, index)
    gen.name, gen.cut = name, cut
    try:
        return gen.case()
    except NoValue:
        return None


def module(name, lines):
    return "\n".join([f"mod {name} {{", "    use super::*;", *lines, "}"])


def render(modules):
    """A whole program from `(name, lines)` per case."""
    table = "const CASES: &[(&str, fn())] = &[" + ", ".join(f'("{name}", {name}::run)' for name, _ in modules) + "];"
    return "\n\n".join([open(os.path.join(HERE, "prelude.rs"), encoding="utf-8").read(), table, *(module(name, lines) for name, lines in modules)]) + "\n"


def program(seed, cases):
    """`cases` cases of `seed`: the modules, the number of functions, and the constructs generated."""
    modules, functions, coverage = [], 0, Counter()
    for index in range(cases):
        lines, gen = case(seed, index)
        modules.append((f"c{index}", lines))
        functions += gen.functions
        coverage.update(gen.cov)
    return modules, functions, coverage


if __name__ == "__main__":
    modules, _, _ = program(sys.argv[1], int(sys.argv[2]) if len(sys.argv) > 2 else 20)
    sys.stdout.write(render(modules))
