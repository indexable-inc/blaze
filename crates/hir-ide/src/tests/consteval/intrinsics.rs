use super::*;

#[test]
fn size_of() {
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn size_of<T>() -> usize;

        const GOAL: usize = size_of::<i32>();
        "#,
        4,
    );
}

#[test]
fn variant_count() {
    let fixture = |goal: &str| {
        format!(
            r#"
        #[rustc_intrinsic]
        pub fn variant_count<T>() -> usize;

        enum Three {{ A, B(u8), C {{ c: i32 }} }}
        enum Never {{}}
        struct One(u8);
        union Both {{ a: u8, b: u16 }}

        {goal}
        "#
        )
    };
    check_number(&fixture("const GOAL: usize = variant_count::<Three>();"), 3);
    check_number(&fixture("const GOAL: usize = variant_count::<Never>();"), 0);
    check_number(&fixture("const GOAL: usize = variant_count::<One>();"), 1);
    check_number(&fixture("const GOAL: usize = variant_count::<Both>();"), 1);
    check_number(&fixture("const GOAL: usize = variant_count::<u8>();"), 0);
}

#[test]
fn size_of_val() {
    check_number(
        r#"
        //- minicore: coerce_unsized
        #[rustc_intrinsic]
        pub fn size_of_val<T: ?Sized>(_: *const T) -> usize;

        struct X(i32, u8);

        const GOAL: usize = size_of_val(&X(1, 2));
        "#,
        8,
    );
    check_number(
        r#"
        //- minicore: coerce_unsized
        #[rustc_intrinsic]
        pub fn size_of_val<T: ?Sized>(_: *const T) -> usize;

        const GOAL: usize = {
            let it: &[i32] = &[1, 2, 3];
            size_of_val(it)
        };
        "#,
        12,
    );
    check_number(
        r#"
        //- minicore: coerce_unsized, transmute
        use core::mem::transmute;

        #[rustc_intrinsic]
        pub fn size_of_val<T: ?Sized>(_: *const T) -> usize;

        struct X {
            x: i64,
            y: u8,
            t: [i32],
        }

        const GOAL: usize = unsafe {
            let y: &X = transmute([0usize, 3]);
            size_of_val(y)
        };
        "#,
        24,
    );
    check_number(
        r#"
        //- minicore: coerce_unsized, transmute
        use core::mem::transmute;

        #[rustc_intrinsic]
        pub fn size_of_val<T: ?Sized>(_: *const T) -> usize;

        struct X {
            x: i32,
            y: i64,
            t: [u8],
        }

        const GOAL: usize = unsafe {
            let y: &X = transmute([0usize, 15]);
            size_of_val(y)
        };
    "#,
        32,
    );
    check_number(
        r#"
        //- minicore: coerce_unsized, fmt, builtin_impls, dispatch_from_dyn
        #[rustc_intrinsic]
        pub fn size_of_val<T: ?Sized>(_: *const T) -> usize;

        const GOAL: usize = {
            let x: &i16 = &5;
            let y: &dyn core::fmt::Debug = x;
            let z: &dyn core::fmt::Debug = &y;
            size_of_val(x) + size_of_val(y) * 10 + size_of_val(z) * 100
        };
        "#,
        1622,
    );
    check_number(
        r#"
        //- minicore: coerce_unsized
        #[rustc_intrinsic]
        pub fn size_of_val<T: ?Sized>(_: *const T) -> usize;

        const GOAL: usize = {
            size_of_val("salam")
        };
        "#,
        5,
    );
}

#[test]
fn align_of_val() {
    check_number(
        r#"
        //- minicore: coerce_unsized
        #[rustc_intrinsic]
        pub fn align_of_val<T: ?Sized>(_: *const T) -> usize;

        struct X(i32, u8);

        const GOAL: usize = align_of_val(&X(1, 2));
        "#,
        4,
    );
    check_number(
        r#"
        //- minicore: coerce_unsized
        #[rustc_intrinsic]
        pub fn align_of_val<T: ?Sized>(_: *const T) -> usize;

        const GOAL: usize = {
            let x: &[i32] = &[1, 2, 3];
            align_of_val(x)
        };
        "#,
        4,
    );
}

#[test]
fn type_name() {
    check_str(
        r#"
        #[rustc_intrinsic]
        pub fn type_name<T: ?Sized>() -> &'static str;

        const GOAL: &str = type_name::<i32>();
        "#,
        "i32",
    );
    check_str(
        r#"
        #[rustc_intrinsic]
        pub fn type_name<T: ?Sized>() -> &'static str;

        mod mod1 {
            pub mod mod2 {
                pub struct Ty;
            }
        }

        const GOAL: &str = type_name::<mod1::mod2::Ty>();
        "#,
        "ra_test_fixture::mod1::mod2::Ty",
    );
}

const TYPE_INFO: &str = r#"
    //- minicore: option, send, slice, matches
    #![feature(lang_items)]
    #[lang = "type_id"]
    pub struct TypeId { t: (u64, u64) }

    #[rustc_intrinsic]
    pub const fn type_id<T: ?Sized>() -> TypeId;
    #[rustc_intrinsic]
    pub const fn type_id_variants(id: TypeId) -> usize;
    #[rustc_intrinsic]
    pub const fn type_id_fields(id: TypeId, variant: usize) -> usize;
    #[rustc_intrinsic]
    pub const fn type_id_field_representing_type(id: TypeId, variant: usize, field: usize) -> TypeId;
    #[rustc_intrinsic]
    pub const fn field_representing_type_actual_type_id(id: TypeId) -> TypeId;
    #[rustc_intrinsic]
    pub const fn field_representing_type_name(id: TypeId) -> &'static str;
    #[rustc_intrinsic]
    pub const fn field_representing_type_offset(id: TypeId) -> usize;
    #[rustc_intrinsic]
    pub const fn type_id_size_of(id: TypeId) -> Option<usize>;
    #[rustc_intrinsic]
    pub const fn type_id_points_to(id: TypeId) -> Option<TypeId>;
    #[rustc_intrinsic]
    pub const fn type_id_variant_name(id: TypeId, variant: usize) -> &'static str;

    #[rustc_intrinsic]
    pub const fn type_id_type_of(id: TypeId) -> Type;
    #[rustc_intrinsic]
    pub const fn type_id_non_exhaustive(id: TypeId) -> bool;
    #[rustc_intrinsic]
    pub const fn type_id_variant_non_exhaustive(id: TypeId, variant: usize) -> bool;
    #[rustc_intrinsic]
    pub const fn type_id_generics(id: TypeId) -> &'static [Generic];
    #[rustc_intrinsic]
    pub const fn type_id_function_ptr(id: TypeId) -> Option<FnPtr>;

    #[lang = "type_info"]
    pub struct Type { pub kind: TypeKind }
    pub enum TypeKind {
        Tuple, Array, Slice, DynTrait(DynTrait), Struct, Enum, Union, Bool, Char, Int, Float,
        Str(Str), Reference, Pointer, FnPtr, Other,
    }
    pub struct DynTrait { pub predicates: &'static [DynTraitPredicate] }
    pub struct DynTraitPredicate { pub trait_ty: Trait }
    pub struct Trait { pub ty: TypeId, pub is_auto: bool }
    pub struct Str {}
    #[lang = "type_info_generic"]
    pub enum Generic { Lifetime(Lifetime), Type(GenericType), Const(Const) }
    pub struct Lifetime {}
    pub struct GenericType { pub ty: TypeId }
    pub struct Const { pub ty: TypeId }
    #[lang = "FnPtr"]
    pub struct FnPtr {
        is_unsafe: bool, abi: Abi, inputs: &'static [TypeId], output: TypeId, variadic: bool,
        is_splatted: bool, splatted_index: u8,
    }
    pub enum Abi { Named(&'static str), ExternRust, ExternC }

    const fn same(a: TypeId, b: TypeId) -> bool {
        a.t.0 == b.t.0 && a.t.1 == b.t.1
    }
    const fn field(base: TypeId, field: usize) -> TypeId {
        field_representing_type_actual_type_id(type_id_field_representing_type(base, 0, field))
    }

    struct Point<'a> { x: u32, y: u8, name: &'a str }
    #[repr(C)]
    struct Packed { x: u32, y: u8 }
    enum Shape { Dot, Line(u16, u64) }
    #[non_exhaustive]
    enum Open { A, #[non_exhaustive] B { x: u8 } }
    struct Pair<'a, T, const N: usize>(&'a T, [T; N]);
    trait Draw {}
"#;

#[test]
fn type_info_counts_and_measures() {
    check_number(
        &format!(
            "{TYPE_INFO}
            const GOAL: usize = {{
                let point = type_id::<Point<'static>>();
                let shape = type_id::<Shape>();
                let size = match type_id_size_of(field(point, 1)) {{ Some(it) => it, None => 9 }};
                let offset = field_representing_type_offset(
                    type_id_field_representing_type(type_id::<Packed>(), 0, 1),
                );
                type_id_variants(point)
                    + type_id_fields(point, 0) * 10
                    + size * 100
                    + type_id_variants(shape) * 1000
                    + type_id_fields(shape, 1) * 10000
                    + same(field(point, 2), type_id::<&str>()) as usize * 100000
                    + offset * 1000000
            }};"
        ),
        4122131,
    );
    check_number(
        &format!(
            "{TYPE_INFO}
            const GOAL: bool = match type_id_points_to(type_id::<&Shape>()) {{
                Some(it) => same(it, type_id::<Shape>()),
                None => false,
            }};"
        ),
        1,
    );
}

#[test]
fn type_info_names() {
    check_str(
        &format!(
            "{TYPE_INFO}
            const GOAL: &str = field_representing_type_name(
                type_id_field_representing_type(type_id::<Point<'static>>(), 0, 1),
            );"
        ),
        "y",
    );
    check_str(
        &format!(
            "{TYPE_INFO}\n    const GOAL: &str = type_id_variant_name(type_id::<Shape>(), 1);"
        ),
        "Line",
    );
}

#[test]
fn type_info_kinds() {
    check_number(
        &format!(
            "{TYPE_INFO}
            const fn kind(id: TypeId) -> usize {{
                match type_id_type_of(id).kind {{
                    TypeKind::Struct => 1,
                    TypeKind::Enum => 2,
                    TypeKind::Int => 3,
                    TypeKind::Str(_) => 4,
                    TypeKind::Reference => 5,
                    TypeKind::FnPtr => 6,
                    TypeKind::Other => 7,
                    TypeKind::DynTrait(it) => match it.predicates {{
                        [principal, auto] => {{
                            8 + (same(principal.trait_ty.ty, type_id::<dyn Draw>())
                                && !principal.trait_ty.is_auto
                                && same(auto.trait_ty.ty, type_id::<dyn Send>())
                                && auto.trait_ty.is_auto) as usize
                        }}
                        _ => 0,
                    }},
                    _ => 0,
                }}
            }}
            const GOAL: usize = kind(type_id::<Point<'static>>())
                + kind(type_id::<Shape>()) * 10
                + kind(type_id::<u8>()) * 100
                + kind(type_id::<str>()) * 1000
                + kind(type_id::<&u8>()) * 10000
                + kind(type_id::<fn(u8)>()) * 100000
                + kind(type_id::<!>()) * 1000000
                + kind(type_id::<dyn Draw + Send>()) * 10000000;"
        ),
        97654321,
    );
}

#[test]
fn type_info_non_exhaustive() {
    check_number(
        &format!(
            "{TYPE_INFO}
            const GOAL: usize = type_id_non_exhaustive(type_id::<Open>()) as usize
                + type_id_non_exhaustive(type_id::<Shape>()) as usize * 10
                + type_id_variant_non_exhaustive(type_id::<Open>(), 0) as usize * 100
                + type_id_variant_non_exhaustive(type_id::<Open>(), 1) as usize * 1000
                + type_id_non_exhaustive(type_id::<u8>()) as usize * 10000;"
        ),
        1001,
    );
}

#[test]
fn type_info_generics_and_fn_ptrs() {
    check_number(
        &format!(
            "{TYPE_INFO}
            const GOAL: usize = {{
                let generics = match type_id_generics(type_id::<Pair<'static, u16, 3>>()) {{
                    [Generic::Lifetime(_), Generic::Type(t), Generic::Const(c)] => {{
                        1 + same(t.ty, type_id::<u16>()) as usize
                            + same(c.ty, type_id::<usize>()) as usize
                    }}
                    _ => 0,
                }};
                let none = type_id_generics(type_id::<u8>()).len();
                let f = match type_id_function_ptr(type_id::<unsafe extern \"C\" fn(u8, u16) -> u32>()) {{
                    Some(f) => {{
                        f.is_unsafe as usize
                            + matches!(f.abi, Abi::ExternC) as usize
                            + match f.inputs {{
                                [a, b] => (same(*a, type_id::<u8>()) && same(*b, type_id::<u16>())) as usize,
                                _ => 0,
                            }}
                            + same(f.output, type_id::<u32>()) as usize
                            + (!f.variadic && !f.is_splatted && f.splatted_index == 255) as usize
                    }}
                    None => 0,
                }};
                let not_fn = match type_id_function_ptr(type_id::<u8>()) {{ Some(_) => 0, None => 1 }};
                generics + none * 10 + f * 100 + not_fn * 1000
            }};"
        ),
        1503,
    );
}

#[test]
fn type_info_out_of_bounds() {
    check_fail(
        &format!("{TYPE_INFO}\n    const GOAL: usize = type_id_fields(type_id::<Shape>(), 2);"),
        |e| matches!(e, ConstEvalError::MirEvalError(MirEvalError::UndefinedBehavior(_))),
    );
}

#[test]
fn type_id_in_constant() {
    use rustc_type_ir::{TyKind, UintTy, inherent::IntoKind};
    check_answer(&format!("{TYPE_INFO}\n    const GOAL: TypeId = type_id::<u8>();"), |b, mm| {
        let id = u128::from_le_bytes(b.try_into().unwrap());
        let ty = mm.type_id_ty(id).expect("the constant says what type its id is of");
        assert!(matches!(ty.kind(), TyKind::Uint(UintTy::U8)), "{ty:?}");
    });
}

#[test]
fn transmute() {
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn transmute<T, U>(e: T) -> U;

        const GOAL: i32 = transmute((1i16, 1i16));
        "#,
        0x00010001,
    );
}

#[test]
fn read_via_copy() {
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn read_via_copy<T>(e: *const T) -> T;
        #[rustc_intrinsic]
        pub fn volatile_load<T>(e: *const T) -> T;

        const GOAL: i32 = {
            let x = 2;
            read_via_copy(&x) + volatile_load(&x)
        };
        "#,
        4,
    );
}

#[test]
fn const_eval_select() {
    check_number(
        r#"
        //- minicore: fn
        #[rustc_intrinsic]
        pub fn const_eval_select<ARG, F, G, RET>(arg: ARG, called_in_const: F, called_at_rt: G) -> RET
        where
            G: FnOnce<ARG, Output = RET>,
            F: FnOnce<ARG, Output = RET>;

        const fn in_const(x: i32, y: i32) -> i32 {
            x + y
        }

        fn in_rt(x: i32, y: i32) -> i32 {
            x + y
        }

        const GOAL: i32 = const_eval_select((2, 3), in_const, in_rt);
        "#,
        5,
    );
}

#[test]
fn const_allocate() {
    check_number(
        r#"
        //- minicore: fn
        #[rustc_intrinsic]
        pub const unsafe fn const_allocate(size: usize, align: usize) -> *mut u8;
        #[rustc_intrinsic]
        pub const unsafe fn const_deallocate(ptr: *mut u8, size: usize, align: usize);

        const GOAL: u8 = unsafe {
            let ptr = const_allocate(4, 4);
            *ptr = 5;
            let value = *ptr;
            const_deallocate(ptr, 4, 4);
            value
        };
        "#,
        5,
    );
}

#[test]
fn wrapping_add() {
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn wrapping_add<T>(a: T, b: T) -> T;

        const GOAL: u8 = wrapping_add(10, 250);
        "#,
        4,
    );
}

#[test]
fn compare_bytes() {
    check_number(
        r#"
        #[rustc_intrinsic]
        pub const unsafe fn compare_bytes(left: *const u8, right: *const u8, bytes: usize) -> i32;

        const GOAL: i32 = unsafe {
            let a = [1u8, 2, 3, 4];
            let b = [1u8, 2, 4, 0];
            let lt = compare_bytes(&a as *const [u8; 4] as *const u8, &b as *const [u8; 4] as *const u8, 4);
            let eq = compare_bytes(&a as *const [u8; 4] as *const u8, &b as *const [u8; 4] as *const u8, 2);
            let gt = compare_bytes(&b as *const [u8; 4] as *const u8, &a as *const [u8; 4] as *const u8, 3);
            let none = compare_bytes(&a as *const [u8; 4] as *const u8, &b as *const [u8; 4] as *const u8, 0);
            (lt + 2) * 1000 + (eq + 2) * 100 + (gt + 2) * 10 + none + 2
        };
        "#,
        1232,
    );
}

#[test]
fn ptr_offset_from() {
    check_number(
        r#"
        //- minicore: index, slice, coerce_unsized
        #[rustc_intrinsic]
        pub fn ptr_offset_from<T>(ptr: *const T, base: *const T) -> isize;
        #[rustc_intrinsic]
        pub fn ptr_offset_from_unsigned<T>(ptr: *const T, base: *const T) -> usize;

        const GOAL: isize = {
            let x = [1, 2, 3, 4, 5i32];
            let r1 = -ptr_offset_from(&x[0], &x[4]);
            let r2 = ptr_offset_from(&x[3], &x[1]);
            let r3 = ptr_offset_from_unsigned(&x[3], &x[0]) as isize;
            r3 * 100 + r2 * 10 + r1
        };
        "#,
        324,
    );
}

#[test]
fn saturating() {
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn saturating_add<T>(a: T, b: T) -> T;

        const GOAL: u8 = saturating_add(10, 250);
        "#,
        255,
    );
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn saturating_sub<T>(a: T, b: T) -> T;

        const GOAL: bool = saturating_sub(5u8, 7) == 0 && saturating_sub(8u8, 4) == 4;
        "#,
        1,
    );
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn saturating_add<T>(a: T, b: T) -> T;

        const GOAL: i8 = saturating_add(5, 8);
        "#,
        13,
    );
}

#[test]
fn allocator() {
    check_number(
        r#"
        //- minicore: sized
        extern "Rust" {
            #[rustc_allocator]
            fn __rust_alloc(size: usize, align: usize) -> *mut u8;
            #[rustc_deallocator]
            fn __rust_dealloc(ptr: *mut u8, size: usize, align: usize);
            #[rustc_reallocator]
            fn __rust_realloc(ptr: *mut u8, old_size: usize, align: usize, new_size: usize) -> *mut u8;
            #[rustc_allocator_zeroed]
            fn __rust_alloc_zeroed(size: usize, align: usize) -> *mut u8;
        }

        const GOAL: u8 = unsafe {
            let ptr = __rust_alloc(4, 1);
            let ptr2 = ((ptr as usize) + 1) as *mut u8;
            *ptr = 23;
            *ptr2 = 32;
            let ptr = __rust_realloc(ptr, 4, 1, 8);
            let ptr = __rust_realloc(ptr, 8, 1, 3);
            let ptr2 = ((ptr as usize) + 1) as *mut u8;
            *ptr + *ptr2
        };
        "#,
        55,
    );
}

const ALLOCATOR: &str = r#"
        extern "Rust" {
            #[rustc_allocator]
            fn __rust_alloc(size: usize, align: usize) -> *mut u8;
            #[rustc_deallocator]
            fn __rust_dealloc(ptr: *mut u8, size: usize, align: usize);
            #[rustc_reallocator]
            fn __rust_realloc(ptr: *mut u8, old_size: usize, align: usize, new_size: usize) -> *mut u8;
            #[rustc_allocator_zeroed]
            fn __rust_alloc_zeroed(size: usize, align: usize) -> *mut u8;
        }
        #[rustc_intrinsic]
        pub const unsafe fn const_allocate(size: usize, align: usize) -> *mut u8;
        #[rustc_intrinsic]
        pub const unsafe fn const_deallocate(ptr: *mut u8, size: usize, align: usize);
        #[rustc_intrinsic]
        pub const unsafe fn const_make_global(ptr: *mut u8) -> *const u8;
"#;

#[test]
fn freed_memory_is_reused() {
    // 1.5 GiB allocated in all, more than the 1 GiB an evaluation may use at once.
    check_number(
        &format!(
            r#"
        //- minicore: sized
        {ALLOCATOR}
        const GOAL: u8 = unsafe {{
            let mut i = 0;
            let mut sum = 0u8;
            while i < 512 {{
                let ptr = __rust_alloc(1 << 20, 8);
                sum = sum + *ptr;
                *ptr = 1;
                let ptr = __rust_realloc(ptr, 1 << 20, 8, (1 << 20) + 1);
                sum = sum + *ptr - 1;
                __rust_dealloc(ptr, (1 << 20) + 1, 8);
                i += 1;
            }}
            sum
        }};
        "#
        ),
        0,
    );
}

#[test]
fn a_grown_block_keeps_its_bytes() {
    check_number(
        &format!(
            r#"
        //- minicore: sized
        {ALLOCATOR}
        const GOAL: u32 = unsafe {{
            let mut cap = 1;
            let mut ptr = __rust_alloc(cap * 4, 4) as *mut u32;
            let mut len = 0;
            while len < 1000 {{
                if len == cap {{
                    ptr = __rust_realloc(ptr as *mut u8, cap * 4, 4, cap * 8) as *mut u32;
                    cap *= 2;
                }}
                *(((ptr as usize) + len * 4) as *mut u32) = len as u32;
                len += 1;
            }}
            let mut sum = 0;
            let mut i = 0;
            while i < len {{
                sum += *(((ptr as usize) + i * 4) as *const u32);
                i += 1;
            }}
            __rust_dealloc(ptr as *mut u8, cap * 4, 4);
            sum
        }};
        "#
        ),
        999 * 1000 / 2,
    );
}

#[test]
fn stack_frames_are_freed() {
    // 2 GiB of locals in all, of 1 MiB a call.
    check_number(
        r#"
        //- minicore: fn
        const fn f(i: usize) -> u8 {
            let a = [7u8; 1 << 20];
            a[i]
        }
        const GOAL: u32 = {
            let g = |i: usize| {
                let a = [1u8; 1 << 20];
                a[i]
            };
            let mut i = 0;
            let mut sum = 0u32;
            while i < 1024 {
                sum = sum + f(i) as u32 + g(i) as u32;
                i += 1;
            }
            sum
        };
        "#,
        1024 * 8,
    );
}

#[test]
fn a_constant_read_again_is_not_copied_again() {
    // 2 GiB if every read of `K` copied it to the heap.
    check_number(
        r#"
        const K: [u32; 65536] = [3; 65536];
        const GOAL: u32 = {
            let mut i = 0;
            let mut sum = 0;
            while i < 8192 {
                sum += K[i];
                i += 1;
            }
            sum
        };
        "#,
        3 * 8192,
    );
}

#[test]
fn a_promoted_temporary_outlives_its_call() {
    check_number(
        r#"
        const fn f() -> &'static [u8; 3] {
            &[1, 2, 3]
        }
        const fn g() -> u8 {
            let a = [9u8; 64];
            a[3]
        }
        const GOAL: u8 = {
            let a = f();
            let b = g();
            let c = f();
            a[2] * 10 + b + c[0]
        };
        "#,
        40,
    );
}

#[test]
fn allocated_memory_may_not_be_in_the_value() {
    for value in ["__rust_alloc(4, 1)", "&*__rust_alloc(4, 1)"] {
        let ty = if value.starts_with('&') { "&u8" } else { "*const u8" };
        check_fail(
            &format!(
                r#"
            //- minicore: sized
            {ALLOCATOR}
            const GOAL: {ty} = unsafe {{ {value} }};
            "#
            ),
            |e| {
                matches!(
                    e,
                    ConstEvalError::MirEvalError(MirEvalError::HeapInValue {
                        of_const_allocate: false
                    })
                )
            },
        );
    }
    // Behind other memory: the data of a vector.
    check_fail(
        &format!(
            r#"
        //- minicore: sized
        {ALLOCATOR}
        struct RawVec {{ ptr: *mut u8, cap: usize }}
        const GOAL: (RawVec, usize) = unsafe {{ (RawVec {{ ptr: __rust_alloc(8, 1), cap: 8 }}, 0) }};
        "#
        ),
        |e| {
            matches!(
                e,
                ConstEvalError::MirEvalError(MirEvalError::HeapInValue {
                    of_const_allocate: false
                })
            )
        },
    );
}

#[test]
fn const_allocate_is_in_the_value_once_made_global() {
    check_fail(
        &format!(
            r#"
        //- minicore: sized
        {ALLOCATOR}
        const GOAL: &u8 = unsafe {{ &*const_allocate(4, 4) }};
        "#
        ),
        |e| {
            matches!(
                e,
                ConstEvalError::MirEvalError(MirEvalError::HeapInValue { of_const_allocate: true })
            )
        },
    );
    check_number(
        &format!(
            r#"
        //- minicore: sized
        {ALLOCATOR}
        const X: &u8 = unsafe {{
            let ptr = const_allocate(4, 4);
            *ptr = 7;
            &*const_make_global(ptr)
        }};
        const GOAL: u8 = *X;
        "#
        ),
        7,
    );
}

#[test]
fn freeing_what_was_not_allocated() {
    for body in [
        "let ptr = __rust_alloc(4, 1); __rust_dealloc(ptr, 4, 1); __rust_dealloc(ptr, 4, 1);",
        "let ptr = __rust_alloc(4, 1); __rust_dealloc(((ptr as usize) + 1) as *mut u8, 3, 1);",
        "let ptr = __rust_alloc(4, 1); const_deallocate(ptr, 4, 1);",
        "let ptr = const_allocate(4, 1); __rust_dealloc(ptr, 4, 1);",
        "let mut x = 0u8; __rust_dealloc(&mut x, 1, 1);",
        "let ptr = __rust_alloc(4, 1); const_make_global(ptr);",
    ] {
        check_fail(
            &format!(
                r#"
            //- minicore: sized
            {ALLOCATOR}
            const GOAL: u8 = unsafe {{ {body} 0 }};
            "#
            ),
            |e| matches!(e, ConstEvalError::MirEvalError(MirEvalError::UndefinedBehavior(_))),
        );
    }
}

#[test]
fn unchecked_arithmetic() {
    let fixture = |goal: &str| {
        format!(
            r#"
        #[rustc_intrinsic]
        pub fn unchecked_add<T>(x: T, y: T) -> T;
        #[rustc_intrinsic]
        pub fn unchecked_mul<T>(x: T, y: T) -> T;
        #[rustc_intrinsic]
        pub fn unchecked_div<T>(x: T, y: T) -> T;
        #[rustc_intrinsic]
        pub fn unchecked_rem<T>(x: T, y: T) -> T;
        #[rustc_intrinsic]
        pub fn exact_div<T>(x: T, y: T) -> T;
        #[rustc_intrinsic]
        pub fn unchecked_shl<T, U>(x: T, y: U) -> T;
        #[rustc_intrinsic]
        pub fn unchecked_shr<T, U>(x: T, y: U) -> T;

        {goal}
        "#
        )
    };
    let undefined = |e: ConstEvalError<'_>| {
        matches!(e, ConstEvalError::MirEvalError(MirEvalError::UndefinedBehavior(_)))
    };
    check_number(&fixture("const GOAL: u8 = unchecked_add(200u8, 55);"), 255);
    check_number(&fixture("const GOAL: i8 = unchecked_mul(-64i8, 2);"), -128);
    check_number(&fixture("const GOAL: i32 = exact_div(-6i32, 3);"), -2);
    check_number(&fixture("const GOAL: u8 = unchecked_shl(1u8, 7u32);"), 128);
    check_number(&fixture("const GOAL: i8 = unchecked_shr(-128i8, 7u32);"), -1);
    check_number(&fixture("const GOAL: u8 = unchecked_rem(7u8, 4);"), 3);
    check_fail(&fixture("const GOAL: u8 = unchecked_add(200u8, 56);"), undefined);
    check_fail(&fixture("const GOAL: i8 = unchecked_mul(-64i8, -2);"), undefined);
    check_fail(&fixture("const GOAL: i8 = unchecked_div(-128i8, -1);"), undefined);
    check_fail(&fixture("const GOAL: i32 = exact_div(7i32, 2);"), undefined);
    check_fail(&fixture("const GOAL: u8 = unchecked_rem(7u8, 0);"), undefined);
    check_fail(&fixture("const GOAL: u8 = unchecked_shl(1u8, 8u32);"), undefined);
    check_fail(&fixture("const GOAL: u8 = unchecked_shr(1u8, -1i32);"), undefined);
}

#[test]
fn overflowing_add() {
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn add_with_overflow<T>(x: T, y: T) -> (T, bool);

        const GOAL: u8 = add_with_overflow(1, 2).0;
        "#,
        3,
    );
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn add_with_overflow<T>(x: T, y: T) -> (T, bool);

        const GOAL: u8 = add_with_overflow(1, 2).1 as u8;
        "#,
        0,
    );
}

#[test]
fn needs_drop() {
    check_number(
        r#"
        //- minicore: drop, manually_drop, copy, sized, phantom_data
        use core::mem::ManuallyDrop;
        use core::marker::PhantomData;
        #[rustc_intrinsic]
        pub fn needs_drop<T: ?Sized>() -> bool;
        struct X;
        struct NeedsDrop;
        impl Drop for NeedsDrop {
            fn drop(&mut self) {}
        }
        enum Enum<T> {
            A(T),
            B(X),
        }
        const fn val_needs_drop<T>(_v: T) -> bool { needs_drop::<T>() }
        const fn closure_needs_drop() -> bool {
            let a = NeedsDrop;
            let b = X;
            !val_needs_drop(|| &a) && val_needs_drop(move || &a) && !val_needs_drop(move || &b)
        }
        const fn opaque() -> impl Sized {
            || {}
        }
        const fn opaque_copy() -> impl Sized + Copy {
            || {}
        }
        struct RecursiveType(RecursiveType);
        trait Everything {}
        impl<T> Everything for T {}
        const GOAL: bool = !needs_drop::<i32>() && !needs_drop::<X>()
            && needs_drop::<NeedsDrop>() && !needs_drop::<ManuallyDrop<NeedsDrop>>()
            && needs_drop::<[NeedsDrop; 1]>() && !needs_drop::<[NeedsDrop; 0]>()
            && needs_drop::<(X, NeedsDrop)>() && !needs_drop::<PhantomData<NeedsDrop>>()
            && needs_drop::<Enum<NeedsDrop>>() && !needs_drop::<Enum<X>>()
            && closure_needs_drop()
            && !val_needs_drop(opaque()) && !val_needs_drop(opaque_copy())
            && needs_drop::<[NeedsDrop]>() && needs_drop::<dyn Everything>()
            && !needs_drop::<&dyn Everything>() && !needs_drop::<str>()
            && !needs_drop::<RecursiveType>();
        "#,
        1,
    );
}

#[test]
fn discriminant_value() {
    check_number(
        r#"
        //- minicore: discriminant, option
        use core::marker::DiscriminantKind;
        #[rustc_intrinsic]
        pub fn discriminant_value<T>(v: &T) -> <T as DiscriminantKind>::Discriminant;
        const GOAL: bool = {
            discriminant_value(&Some(2i32)) == discriminant_value(&Some(5i32))
                && discriminant_value(&Some(2i32)) != discriminant_value(&None::<i32>)
        };
        "#,
        1,
    );
}

#[test]
fn likely() {
    check_number(
        r#"
        #[rustc_intrinsic]
        pub const fn likely(b: bool) -> bool {
            b
        }

        #[rustc_intrinsic]
        pub const fn unlikely(b: bool) -> bool {
            b
        }

        const GOAL: bool = likely(true) && unlikely(true) && !likely(false) && !unlikely(false);
        "#,
        1,
    );
}

#[test]
fn floating_point() {
    // FIXME(#17451): Add `f16` and `f128` tests once intrinsics are added.
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn sqrtf32(x: f32) -> f32;
        #[rustc_intrinsic]
        pub fn powf32(a: f32, x: f32) -> f32;
        #[rustc_intrinsic]
        pub fn fmaf32(a: f32, b: f32, c: f32) -> f32;

        const GOAL: f32 = sqrtf32(1.2) + powf32(3.4, 5.6) + fmaf32(-7.8, 1.3, 2.4);
        "#,
        i128::from_le_bytes(pad16(
            &f32::to_le_bytes(1.2f32.sqrt() + 3.4f32.powf(5.6) + (-7.8f32).mul_add(1.3, 2.4)),
            IsSigned::Yes,
        )),
    );
    // FIXME: this should be an `expect`, but that currently results in `lint_expectation_unfulfilled`
    #[allow(clippy::unnecessary_min_or_max, reason = "for symmetry with the expression in `GOAL`")]
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn powif64(a: f64, x: i32) -> f64;
        #[rustc_intrinsic]
        pub fn sinf64(x: f64) -> f64;
        #[rustc_intrinsic]
        pub fn minnumf64(x: f64, y: f64) -> f64;

        const GOAL: f64 = powif64(1.2, 5) + sinf64(3.4) + minnumf64(-7.8, 1.3);
        "#,
        i128::from_le_bytes(pad16(
            &f64::to_le_bytes(1.2f64.powi(5) + 3.4f64.sin() + (-7.8f64).min(1.3)),
            IsSigned::Yes,
        )),
    );
}

#[test]
fn atomic() {
    check_number(
        r#"
        //- minicore: copy
        #[rustc_intrinsic]
        pub fn atomic_load_seqcst<T: Copy>(src: *const T) -> T;
        #[rustc_intrinsic]
        pub fn atomic_xchg_acquire<T: Copy>(dst: *mut T, src: T) -> T;
        #[rustc_intrinsic]
        pub fn atomic_cxchg_release_seqcst<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
        #[rustc_intrinsic]
        pub fn atomic_cxchgweak_acquire_acquire<T: Copy>(dst: *mut T, old: T, src: T) -> (T, bool);
        #[rustc_intrinsic]
        pub fn atomic_store_release<T: Copy>(dst: *mut T, val: T);
        #[rustc_intrinsic]
        pub fn atomic_xadd_acqrel<T: Copy>(dst: *mut T, src: T) -> T;
        #[rustc_intrinsic]
        pub fn atomic_xsub_seqcst<T: Copy>(dst: *mut T, src: T) -> T;
        #[rustc_intrinsic]
        pub fn atomic_and_acquire<T: Copy>(dst: *mut T, src: T) -> T;
        #[rustc_intrinsic]
        pub fn atomic_nand_seqcst<T: Copy>(dst: *mut T, src: T) -> T;
        #[rustc_intrinsic]
        pub fn atomic_or_release<T: Copy>(dst: *mut T, src: T) -> T;
        #[rustc_intrinsic]
        pub fn atomic_xor_seqcst<T: Copy>(dst: *mut T, src: T) -> T;
        #[rustc_intrinsic]
        pub fn atomic_fence_seqcst();
        #[rustc_intrinsic]
        pub fn atomic_singlethreadfence_acqrel();

        fn should_not_reach() {
            _ // fails the test if executed
        }

        const GOAL: i32 = {
            let mut x = 5;
            atomic_store_release(&mut x, 10);
            let mut y = atomic_xchg_acquire(&mut x, 100);
            atomic_xadd_acqrel(&mut y, 20);
            if (30, true) != atomic_cxchg_release_seqcst(&mut y, 30, 40) {
                should_not_reach();
            }
            atomic_fence_seqcst();
            if (40, false) != atomic_cxchg_release_seqcst(&mut y, 30, 50) {
                should_not_reach();
            }
            if (40, true) != atomic_cxchgweak_acquire_acquire(&mut y, 40, 30) {
                should_not_reach();
            }
            let mut z = atomic_xsub_seqcst(&mut x, -200);
            atomic_singlethreadfence_acqrel();
            atomic_xor_seqcst(&mut x, 1024);
            atomic_load_seqcst(&x) + z * 3 + atomic_load_seqcst(&y) * 2
        };
        "#,
        660 + 1024,
    );
}

#[test]
fn offset() {
    check_number(
        r#"
        //- minicore: coerce_unsized, index, slice
        #[rustc_intrinsic]
        pub fn offset<Ptr, Delta>(dst: Ptr, offset: Delta) -> Ptr;
        #[rustc_intrinsic]
        pub fn arith_offset<T>(dst: *const T, offset: isize) -> *const T;

        const GOAL: i32 = unsafe {
            let ar: &[(i32, i32, i32)] = &[
                (10, 11, 12),
                (20, 21, 22),
                (30, 31, 32),
                (40, 41, 42),
                (50, 51, 52),
            ];
            let ar: *const [(i32, i32, i32)] = ar;
            let ar = ar as *const (i32, i32, i32);
            let element3 = *offset(ar, 2usize);
            let element4 = *arith_offset(ar, 3);
            element3.1 * 100 + element4.0
        };
        "#,
        3140,
    );
}

#[test]
fn arith_offset() {
    check_number(
        r#"
        //- minicore: coerce_unsized, index, slice
        #[rustc_intrinsic]
        pub fn arith_offset<T>(dst: *const T, offset: isize) -> *const T;

        const GOAL: u8 = unsafe {
            let ar: &[(u8, u8, u8)] = &[
                (10, 11, 12),
                (20, 21, 22),
                (30, 31, 32),
                (40, 41, 42),
                (50, 51, 52),
            ];
            let ar: *const [(u8, u8, u8)] = ar;
            let ar = ar as *const (u8, u8, u8);
            let element = *arith_offset(arith_offset(ar, 102), -100);
            element.1
        };
        "#,
        31,
    );
}

#[test]
fn copy_nonoverlapping() {
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn copy_nonoverlapping<T>(src: *const T, dst: *mut T, count: usize);

        const GOAL: u8 = unsafe {
            let mut x = 2;
            let y = 5;
            copy_nonoverlapping(&y, &mut x, 1);
            x
        };
        "#,
        5,
    );
}

#[test]
fn write_bytes() {
    check_number(
        r#"
        #[rustc_intrinsic]
        fn write_bytes<T>(dst: *mut T, val: u8, count: usize);

        const GOAL: i32 = unsafe {
            let mut x = 2;
            write_bytes(&mut x, 5, 1);
            x
        };
        "#,
        0x05050505,
    );
}

#[test]
fn write_via_move() {
    check_number(
        r#"
        #[rustc_intrinsic]
        fn write_via_move<T>(ptr: *mut T, value: T);

        const GOAL: i32 = unsafe {
            let mut x = 2;
            write_via_move(&mut x, 100);
            x
        };
        "#,
        100,
    );
}

#[test]
fn copy() {
    check_number(
        r#"
        //- minicore: coerce_unsized, index, slice
        #[rustc_intrinsic]
        pub fn copy<T>(src: *const T, dst: *mut T, count: usize);

        const GOAL: i32 = unsafe {
            let mut x = [1i32, 2, 3, 4, 5];
            let y = (&mut x as *mut _) as *mut i32;
            let z = (y as usize + 4) as *const i32;
            copy(z, y, 4);
            x[0] + x[1] + x[2] + x[3] + x[4]
        };
        "#,
        19,
    );
}

#[test]
fn ctpop() {
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn ctpop<T: Copy>(x: T) -> T;

        const GOAL: i64 = ctpop(-29);
        "#,
        61,
    );
}

#[test]
fn ctlz() {
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn ctlz<T: Copy>(x: T) -> T;

        const GOAL: u8 = ctlz(0b0001_1100_u8);
        "#,
        3,
    );
}

#[test]
fn cttz() {
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn cttz<T: Copy>(x: T) -> T;

        const GOAL: i64 = cttz(-24);
        "#,
        3,
    );
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn cttz<T: Copy>(x: T) -> T;

        const GOAL: i8 = cttz(0i8);
        "#,
        8,
    );
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn cttz<T: Copy>(x: T) -> T;

        const GOAL: u32 = cttz(0u32);
        "#,
        32,
    );
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn cttz<T: Copy>(x: T) -> T;

        const GOAL: u64 = cttz(0u64);
        "#,
        64,
    );
}

#[test]
fn rotate() {
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn rotate_left<T: Copy>(x: T, y: T) -> T;

        const GOAL: i64 = rotate_left(0xaa00000000006e1i64, 12);
        "#,
        0x6e10aa,
    );
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn rotate_right<T: Copy>(x: T, y: T) -> T;

        const GOAL: i64 = rotate_right(0x6e10aa, 12);
        "#,
        0xaa00000000006e1,
    );
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn rotate_left<T: Copy>(x: T, y: T) -> T;

        const GOAL: i8 = rotate_left(129, 2);
        "#,
        6,
    );
    check_number(
        r#"
        #[rustc_intrinsic]
        pub fn rotate_right<T: Copy>(x: T, y: T) -> T;

        const GOAL: i32 = rotate_right(10006016, 1020315);
        "#,
        320192512,
    );
}

#[test]
fn simd() {
    check_number(
        r#"
        pub struct i8x16(
            i8,i8,i8,i8,i8,i8,i8,i8,i8,i8,i8,i8,i8,i8,i8,i8,
        );
        #[rustc_intrinsic]
        pub fn simd_bitmask<T, U>(x: T) -> U;
        const GOAL: u16 = simd_bitmask(i8x16(
            0, -1, 0, 0, -1, -1, -1, 0, -1, 0, -1, -1, 0, 0, 0, 0
        ));
        "#,
        0b0000110101110010,
    );
    check_number(
        r#"
        pub struct i8x16(
            i8,i8,i8,i8,i8,i8,i8,i8,i8,i8,i8,i8,i8,i8,i8,i8,
        );
        #[rustc_intrinsic]
        pub fn simd_lt<T, U>(x: T, y: T) -> U;
        #[rustc_intrinsic]
        pub fn simd_bitmask<T, U>(x: T) -> U;
        const GOAL: u16 = simd_bitmask(simd_lt::<i8x16, i8x16>(
            i8x16(
                -105, -4, -3, -2, -1, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10
            ),
            i8x16(
                -4, -3, -2, -1, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11
            ),
        ));
        "#,
        0xFFFF,
    );
}

const SIMD: &str = r#"
    #[repr(simd)] pub struct i8x4([i8; 4]);
    #[repr(simd)] pub struct u8x4([u8; 4]);
    #[repr(simd)] pub struct i8x16([i8; 16]);
    #[repr(simd)] pub struct i32x4([i32; 4]);
    #[repr(simd)] pub struct u32x4([u32; 4]);
    #[repr(simd)] pub struct f32x2([f32; 2]);
    #[repr(simd)] pub struct f32x4([f32; 4]);
    #[repr(simd)] pub struct f64x2([f64; 2]);
    #[repr(simd)] pub struct usizex2([usize; 2]);
    #[repr(simd)] pub struct isizex2([isize; 2]);
    #[repr(simd)] pub struct isizex4([isize; 4]);
    #[repr(simd)] pub struct ptrx2([*const i32; 2]);
    #[repr(simd)] pub struct bptrx2([*const u8; 2]);
    #[repr(simd)] pub struct ptrx4([*const i32; 4]);
    #[repr(simd)] pub struct mptrx4([*mut i32; 4]);
    pub enum SimdAlign { Unaligned = 0, Element = 1, Vector = 2 }

    #[rustc_intrinsic] pub const unsafe fn simd_add<T>(x: T, y: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_sub<T>(x: T, y: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_mul<T>(x: T, y: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_div<T>(x: T, y: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_rem<T>(x: T, y: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_shl<T>(x: T, y: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_shr<T>(x: T, y: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_funnel_shl<T>(a: T, b: T, shift: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_funnel_shr<T>(a: T, b: T, shift: T) -> T;
    #[rustc_intrinsic] pub unsafe fn simd_carryless_mul<T>(a: T, b: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_neg<T>(x: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_saturating_add<T>(x: T, y: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_saturating_sub<T>(x: T, y: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_bswap<T>(x: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_bitreverse<T>(x: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_ctlz<T>(x: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_cttz<T>(x: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_ctpop<T>(x: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_fabs<T>(x: T) -> T;
    #[rustc_intrinsic] pub unsafe fn simd_fsqrt<T>(x: T) -> T;
    #[rustc_intrinsic] pub unsafe fn simd_fsin<T>(x: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_ceil<T>(x: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_floor<T>(x: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_round<T>(x: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_round_ties_even<T>(x: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_trunc<T>(x: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_fma<T>(x: T, y: T, z: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_relaxed_fma<T>(x: T, y: T, z: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_minimum_number_nsz<T>(x: T, y: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_maximum_number_nsz<T>(x: T, y: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_fmin<T>(x: T, y: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_eq<T, U>(x: T, y: T) -> U;
    #[rustc_intrinsic] pub const unsafe fn simd_ne<T, U>(x: T, y: T) -> U;
    #[rustc_intrinsic] pub const unsafe fn simd_lt<T, U>(x: T, y: T) -> U;
    #[rustc_intrinsic] pub const unsafe fn simd_le<T, U>(x: T, y: T) -> U;
    #[rustc_intrinsic] pub const unsafe fn simd_gt<T, U>(x: T, y: T) -> U;
    #[rustc_intrinsic] pub const unsafe fn simd_ge<T, U>(x: T, y: T) -> U;
    #[rustc_intrinsic] pub const unsafe fn simd_cast<T, U>(x: T) -> U;
    #[rustc_intrinsic] pub const unsafe fn simd_as<T, U>(x: T) -> U;
    #[rustc_intrinsic] pub const unsafe fn simd_cast_ptr<T, U>(ptr: T) -> U;
    #[rustc_intrinsic] pub unsafe fn simd_expose_provenance<T, U>(ptr: T) -> U;
    #[rustc_intrinsic] pub const unsafe fn simd_with_exposed_provenance<T, U>(addr: T) -> U;
    #[rustc_intrinsic] pub const unsafe fn simd_arith_offset<T, U>(ptr: T, offset: U) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_select<M, T>(mask: M, yes: T, no: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_select_bitmask<M, T>(m: M, yes: T, no: T) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_bitmask<T, U>(x: T) -> U;
    #[rustc_intrinsic] pub const unsafe fn simd_extract<T, U>(x: T, idx: u32) -> U;
    #[rustc_intrinsic] pub const unsafe fn simd_extract_dyn<T, U>(x: T, idx: u32) -> U;
    #[rustc_intrinsic] pub const unsafe fn simd_insert<T, U>(x: T, idx: u32, val: U) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_insert_dyn<T, U>(x: T, idx: u32, val: U) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_reduce_add_ordered<T, U>(x: T, y: U) -> U;
    #[rustc_intrinsic] pub const unsafe fn simd_reduce_mul_ordered<T, U>(x: T, y: U) -> U;
    #[rustc_intrinsic] pub unsafe fn simd_reduce_add_unordered<T, U>(x: T) -> U;
    #[rustc_intrinsic] pub unsafe fn simd_reduce_mul_unordered<T, U>(x: T) -> U;
    #[rustc_intrinsic] pub const unsafe fn simd_reduce_and<T, U>(x: T) -> U;
    #[rustc_intrinsic] pub const unsafe fn simd_reduce_or<T, U>(x: T) -> U;
    #[rustc_intrinsic] pub const unsafe fn simd_reduce_xor<T, U>(x: T) -> U;
    #[rustc_intrinsic] pub const unsafe fn simd_reduce_min<T, U>(x: T) -> U;
    #[rustc_intrinsic] pub const unsafe fn simd_reduce_max<T, U>(x: T) -> U;
    #[rustc_intrinsic] pub const unsafe fn simd_reduce_all<T>(x: T) -> bool;
    #[rustc_intrinsic] pub const unsafe fn simd_reduce_any<T>(x: T) -> bool;
    #[rustc_intrinsic] pub const unsafe fn simd_shuffle<T, U, V>(x: T, y: T, idx: U) -> V;
    #[rustc_intrinsic] pub const unsafe fn simd_splat<T, U>(value: U) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_gather<T, U, V>(val: T, ptr: U, mask: V) -> T;
    #[rustc_intrinsic] pub const unsafe fn simd_scatter<T, U, V>(val: T, ptr: U, mask: V);
    #[rustc_intrinsic]
    pub const unsafe fn simd_masked_load<V, U, T, const ALIGN: SimdAlign>(mask: V, ptr: U, val: T) -> T;
    #[rustc_intrinsic]
    pub const unsafe fn simd_masked_store<V, U, T, const ALIGN: SimdAlign>(mask: V, ptr: U, val: T);
"#;

fn with_simd(goal: &str) -> String {
    format!("{SIMD}\n    {goal}")
}

/// What `check_number` compares a vector with: the number whose little-endian bytes are its
/// lanes'.
fn lanes<const N: usize, const B: usize>(lanes: [[u8; B]; N]) -> i128 {
    let mut bytes = [0; 16];
    bytes[..N * B].copy_from_slice(lanes.as_flattened());
    i128::from_le_bytes(bytes)
}

fn i8s(it: [i8; 4]) -> i128 {
    lanes(it.map(i8::to_le_bytes))
}

fn u8s(it: [u8; 4]) -> i128 {
    lanes(it.map(u8::to_le_bytes))
}

fn i32s(it: [i32; 4]) -> i128 {
    lanes(it.map(i32::to_le_bytes))
}

fn u32s(it: [u32; 4]) -> i128 {
    lanes(it.map(u32::to_le_bytes))
}

fn f32s(it: [f32; 4]) -> i128 {
    lanes(it.map(f32::to_le_bytes))
}

fn f64s(it: [f64; 2]) -> i128 {
    lanes(it.map(f64::to_le_bytes))
}

const NAN32: f32 = f32::from_bits(0x7fc0_0000);
const NAN64: f64 = f64::from_bits(0x7ff8_0000_0000_0000);

fn is_ub(e: ConstEvalError<'_>) -> bool {
    matches!(e, ConstEvalError::MirEvalError(MirEvalError::UndefinedBehavior(_)))
}

#[test]
fn simd_int_arithmetic() {
    let check = |goal: &str, answer| check_number(&with_simd(goal), answer);
    check(
        "const GOAL: i32x4 = unsafe { simd_add(i32x4([1, 2, 3, 2147483647]), i32x4([4, 5, 6, 1])) };",
        i32s([5, 7, 9, i32::MIN]),
    );
    check(
        "const GOAL: i32x4 = unsafe { simd_sub(i32x4([1, 2, 3, -2147483647 - 1]), i32x4([4, 5, 6, 1])) };",
        i32s([-3, -3, -3, i32::MAX]),
    );
    check(
        "const GOAL: i32x4 = unsafe { simd_mul(i32x4([3, -4, 65536, 7]), i32x4([5, 6, 65536, -1])) };",
        i32s([15, -24, 0, -7]),
    );
    check(
        "const GOAL: i32x4 = unsafe { simd_div(i32x4([7, -7, 7, -7]), i32x4([2, 2, -2, -2])) };",
        i32s([3, -3, -3, 3]),
    );
    check(
        "const GOAL: i32x4 = unsafe { simd_rem(i32x4([7, -7, 7, -7]), i32x4([2, 2, -2, -2])) };",
        i32s([1, -1, 1, -1]),
    );
    check(
        "const GOAL: u32x4 = unsafe { simd_div(u32x4([4294967295, 7, 0, 9]), u32x4([2, 7, 3, 10])) };",
        u32s([2147483647, 1, 0, 0]),
    );
    check(
        "const GOAL: i32x4 = unsafe { simd_neg(i32x4([1, -2, 0, -2147483647 - 1])) };",
        i32s([-1, 2, 0, i32::MIN]),
    );
    check(
        "const GOAL: i32x4 = unsafe { simd_shl(i32x4([1, -1, 3, 1]), i32x4([0, 4, 30, 31])) };",
        i32s([1, -16, -1073741824, i32::MIN]),
    );
    check(
        "const GOAL: i32x4 = unsafe { simd_shr(i32x4([-16, 16, -1, -2147483647 - 1]), i32x4([2, 2, 31, 31])) };",
        i32s([-4, 4, -1, -1]),
    );
    check(
        "const GOAL: u32x4 = unsafe { simd_shr(u32x4([2147483648, 16, 4294967295, 1]), u32x4([31, 2, 28, 0])) };",
        u32s([1, 4, 15, 1]),
    );
    check(
        "const GOAL: i8x4 = unsafe { simd_saturating_add(i8x4([100, -100, 1, 127]), i8x4([100, -100, 2, 0])) };",
        i8s([127, -128, 3, 127]),
    );
    check(
        "const GOAL: i8x4 = unsafe { simd_saturating_sub(i8x4([-100, 100, 5, -127 - 1]), i8x4([100, -100, 7, 1])) };",
        i8s([-128, 127, -2, -128]),
    );
    check(
        "const GOAL: u8x4 = unsafe { simd_saturating_add(u8x4([200, 1, 255, 0]), u8x4([100, 2, 1, 0])) };",
        u8s([255, 3, 255, 0]),
    );
    check(
        "const GOAL: u8x4 = unsafe { simd_saturating_sub(u8x4([5, 1, 0, 255]), u8x4([10, 1, 1, 0])) };",
        u8s([0, 0, 0, 255]),
    );
    check(
        "const GOAL: u8x4 = unsafe { simd_funnel_shl(u8x4([18, 255, 128, 1]), u8x4([52, 0, 1, 128]), u8x4([4, 0, 7, 1])) };",
        u8s([0x23, 0xff, 0x00, 0x03]),
    );
    check(
        "const GOAL: u8x4 = unsafe { simd_funnel_shr(u8x4([18, 255, 128, 1]), u8x4([52, 0, 1, 128]), u8x4([4, 0, 7, 1])) };",
        u8s([0x23, 0x00, 0x00, 0xc0]),
    );
    check(
        "const GOAL: u8x4 = unsafe { simd_carryless_mul(u8x4([3, 255, 128, 5]), u8x4([3, 2, 2, 5])) };",
        u8s([5, 0xfe, 0, 17]),
    );
}

#[test]
fn simd_int_undefined_behavior() {
    let check = |goal: &str| check_fail(&with_simd(goal), is_ub);
    check("const GOAL: i32x4 = unsafe { simd_div(i32x4([1, 1, 1, 1]), i32x4([1, 0, 1, 1])) };");
    check("const GOAL: u32x4 = unsafe { simd_rem(u32x4([1, 1, 1, 1]), u32x4([1, 1, 1, 0])) };");
    check(
        "const GOAL: i32x4 = unsafe { simd_div(i32x4([-2147483647 - 1, 1, 1, 1]), i32x4([-1, 1, 1, 1])) };",
    );
    check(
        "const GOAL: i32x4 = unsafe { simd_rem(i32x4([1, -2147483647 - 1, 1, 1]), i32x4([1, -1, 1, 1])) };",
    );
    check("const GOAL: i32x4 = unsafe { simd_shl(i32x4([1, 1, 1, 1]), i32x4([0, 32, 0, 0])) };");
    check("const GOAL: i32x4 = unsafe { simd_shr(i32x4([1, 1, 1, 1]), i32x4([0, 0, -1, 0])) };");
    check("const GOAL: u8x4 = unsafe { simd_shr(u8x4([1, 1, 1, 1]), u8x4([8, 0, 0, 0])) };");
    check(
        "const GOAL: u8x4 = unsafe { simd_funnel_shl(u8x4([1, 1, 1, 1]), u8x4([1, 1, 1, 1]), u8x4([0, 8, 0, 0])) };",
    );
}

#[test]
fn simd_int_bits() {
    let check = |goal: &str, answer| check_number(&with_simd(goal), answer);
    check(
        "const GOAL: u32x4 = unsafe { simd_bswap(u32x4([0x12345678, 0, 0xff, 1])) };",
        u32s([0x78563412, 0, 0xff000000, 0x01000000]),
    );
    check(
        "const GOAL: u8x4 = unsafe { simd_bitreverse(u8x4([1, 0x80, 0x0f, 0x16])) };",
        u8s([0x80, 1, 0xf0, 0x68]),
    );
    check(
        "const GOAL: u32x4 = unsafe { simd_ctlz(u32x4([0, 1, 0x80000000, 0xffff])) };",
        u32s([32, 31, 0, 16]),
    );
    check(
        "const GOAL: u32x4 = unsafe { simd_cttz(u32x4([0, 1, 0x80000000, 0x100])) };",
        u32s([32, 0, 31, 8]),
    );
    check(
        "const GOAL: i8x4 = unsafe { simd_ctpop(i8x4([-1, 0, 7, -127 - 1])) };",
        i8s([8, 0, 3, 1]),
    );
}

#[test]
fn simd_float_arithmetic() {
    let check = |goal: &str, answer| check_number(&with_simd(goal), answer);
    let args = "f32x4([1.5, 0.1, 1e30, 0.0]), f32x4([2.25, 0.2, 1e30, 0.0])";
    check(
        &format!("const GOAL: f32x4 = unsafe {{ simd_add({args}) }};"),
        f32s([1.5 + 2.25, 0.1 + 0.2, 1e30 + 1e30, 0.0]),
    );
    check(
        &format!("const GOAL: f32x4 = unsafe {{ simd_sub({args}) }};"),
        f32s([1.5 - 2.25, 0.1 - 0.2, 0.0, 0.0]),
    );
    check(
        &format!("const GOAL: f32x4 = unsafe {{ simd_mul({args}) }};"),
        f32s([1.5 * 2.25, 0.1 * 0.2, f32::INFINITY, 0.0]),
    );
    check(
        &format!("const GOAL: f32x4 = unsafe {{ simd_div({args}) }};"),
        f32s([1.5 / 2.25, 0.1 / 0.2, 1.0, NAN32]),
    );
    check(
        &format!("const GOAL: f32x4 = unsafe {{ simd_rem({args}) }};"),
        f32s([1.5, 0.1, 0.0, NAN32]),
    );
    check(
        "const GOAL: f64x2 = unsafe { simd_add(f64x2([0.1, 1e308]), f64x2([0.2, 1e308])) };",
        f64s([0.1 + 0.2, f64::INFINITY]),
    );
    check(
        "const GOAL: f32x4 = unsafe { simd_fsqrt(f32x4([2.0, 4.0, -1.0, 0.0])) };",
        f32s([f32::from_bits(0x3fb5_04f3), 2.0, NAN32, 0.0]),
    );
    check(
        "const GOAL: f64x2 = unsafe { simd_fsqrt(f64x2([2.0, -0.0])) };",
        f64s([2f64.sqrt(), -0.0]),
    );
    let fma =
        "f32x4([0.1, 2.0, 1e30, 1.0]), f32x4([10.0, 3.0, 1e30, 0.0]), f32x4([-1.0, 1.0, 1.0, 5.0])";
    let fused = f32s([0.1f32.mul_add(10.0, -1.0), 7.0, f32::INFINITY, 5.0]);
    check(&format!("const GOAL: f32x4 = unsafe {{ simd_fma({fma}) }};"), fused);
    check(&format!("const GOAL: f32x4 = unsafe {{ simd_relaxed_fma({fma}) }};"), fused);
    let nans = "f32x4([0.0 / 0.0, 1.0, -3.0, 0.0 / 0.0]), f32x4([1.0, 0.0 / 0.0, 2.0, 0.0 / 0.0])";
    check(
        &format!("const GOAL: f32x4 = unsafe {{ simd_minimum_number_nsz({nans}) }};"),
        f32s([1.0, 1.0, -3.0, NAN32]),
    );
    check(
        &format!("const GOAL: f32x4 = unsafe {{ simd_maximum_number_nsz({nans}) }};"),
        f32s([1.0, 1.0, 2.0, NAN32]),
    );
    check(
        &format!("const GOAL: f32x4 = unsafe {{ simd_fmin({nans}) }};"),
        f32s([1.0, 1.0, -3.0, NAN32]),
    );
    check(
        "const GOAL: f32x4 = unsafe { simd_neg(f32x4([1.0, -0.0, 2.5, 1.0 / 0.0])) };",
        f32s([-1.0, 0.0, -2.5, f32::NEG_INFINITY]),
    );
    check(
        "const GOAL: f32x4 = unsafe { simd_fabs(f32x4([-1.5, -0.0, 2.0, -1.0 / 0.0])) };",
        f32s([1.5, 0.0, 2.0, f32::INFINITY]),
    );
}

#[test]
fn simd_float_rounding() {
    let check = |name: &str, answer| {
        check_number(
            &with_simd(&format!(
                "const GOAL: f32x4 = unsafe {{ simd_{name}(f32x4([2.5, -2.5, 1.4, -0.5])) }};"
            )),
            f32s(answer),
        )
    };
    check("ceil", [3.0, -2.0, 2.0, -0.0]);
    check("floor", [2.0, -3.0, 1.0, -1.0]);
    check("round", [3.0, -3.0, 1.0, -1.0]);
    check("round_ties_even", [2.0, -2.0, 1.0, -0.0]);
    check("trunc", [2.0, -2.0, 1.0, -0.0]);
}

#[test]
fn simd_transcendental_is_refused() {
    check_fail(
        &with_simd("const GOAL: f32x4 = unsafe { simd_fsin(f32x4([1.0, 2.0, 3.0, 4.0])) };"),
        |e| matches!(e, ConstEvalError::MirEvalError(MirEvalError::NotSupported(_))),
    );
}

#[test]
fn simd_compare() {
    let check = |name: &str, args: &str, answer| {
        check_number(
            &with_simd(&format!("const GOAL: i32x4 = unsafe {{ simd_{name}({args}) }};")),
            i32s(answer),
        )
    };
    let floats = "f32x4([1.0, 0.0 / 0.0, -0.0, 3.0]), f32x4([2.0, 1.0, 0.0, 3.0])";
    check("lt", floats, [-1, 0, 0, 0]);
    check("le", floats, [-1, 0, -1, -1]);
    check("gt", floats, [0, 0, 0, 0]);
    check("ge", floats, [0, 0, -1, -1]);
    check("eq", floats, [0, 0, -1, -1]);
    check("ne", floats, [-1, -1, 0, 0]);
    check(
        "lt",
        "i32x4([-1, 5, -2147483647 - 1, 0]), i32x4([0, -5, 2147483647, 0])",
        [-1, 0, -1, 0],
    );
    check("lt", "u32x4([4294967295, 1, 0, 5]), u32x4([0, 2, 0, 6])", [0, -1, 0, -1]);
    check("ge", "u32x4([4294967295, 1, 0, 5]), u32x4([0, 2, 0, 6])", [-1, 0, -1, 0]);
}

#[test]
fn simd_cast() {
    let check = |goal: &str, answer| check_number(&with_simd(goal), answer);
    check(
        "const GOAL: i32x4 = unsafe { simd_as::<f32x4, i32x4>(f32x4([1e10, 0.0 / 0.0, -1e10, -1.9])) };",
        i32s([i32::MAX, 0, i32::MIN, -1]),
    );
    check(
        "const GOAL: u8x4 = unsafe { simd_as::<f32x4, u8x4>(f32x4([300.0, -5.0, 255.9, 0.0 / 0.0])) };",
        u8s([255, 0, 255, 0]),
    );
    check(
        "const GOAL: i32x4 = unsafe { simd_cast::<f32x4, i32x4>(f32x4([1.9, -1.9, 0.5, -2147483648.0])) };",
        i32s([1, -1, 0, i32::MIN]),
    );
    check(
        "const GOAL: f32x4 = unsafe { simd_cast::<i32x4, f32x4>(i32x4([16777217, -1, 0, 3])) };",
        f32s([16777216.0, -1.0, 0.0, 3.0]),
    );
    check(
        "const GOAL: i8x4 = unsafe { simd_cast::<i32x4, i8x4>(i32x4([300, -1, 127, -129])) };",
        i8s([44, -1, 127, 127]),
    );
    check(
        "const GOAL: u32x4 = unsafe { simd_cast::<i8x4, u32x4>(i8x4([-1, 1, -127 - 1, 0])) };",
        u32s([0xffffffff, 1, 0xffffff80, 0]),
    );
    check(
        "const GOAL: i32x4 = unsafe { simd_as::<u8x4, i32x4>(u8x4([255, 0, 128, 1])) };",
        i32s([255, 0, 128, 1]),
    );
    check(
        "const GOAL: f64x2 = unsafe { simd_cast::<f32x2, f64x2>(f32x2([0.1, 0.0 / 0.0])) };",
        f64s([f64::from(0.1f32), NAN64]),
    );
    check(
        "const GOAL: f32x2 = unsafe { simd_cast::<f64x2, f32x2>(f64x2([0.1, 1e300])) };",
        lanes([0.1f64 as f32, f32::INFINITY].map(f32::to_le_bytes)),
    );
    check_fail(
        &with_simd(
            "const GOAL: i32x4 = unsafe { simd_cast::<f32x4, i32x4>(f32x4([1.0, 1e10, 0.0, 0.0])) };",
        ),
        is_ub,
    );
    check_fail(
        &with_simd(
            "const GOAL: u8x4 = unsafe { simd_cast::<f32x4, u8x4>(f32x4([0.0 / 0.0, 1.0, 0.0, 0.0])) };",
        ),
        is_ub,
    );
}

#[test]
fn simd_pointers() {
    let check = |goal: &str, answer| check_number(&with_simd(goal), answer);
    let ptrs = "simd_with_exposed_provenance::<usizex2, ptrx2>(usizex2([8, 16]))";
    check(
        &format!(
            "const GOAL: usizex2 = unsafe {{ simd_expose_provenance::<ptrx2, usizex2>({ptrs}) }};"
        ),
        lanes([8usize, 16].map(usize::to_le_bytes)),
    );
    check(
        &format!(
            "const GOAL: usizex2 = unsafe {{
                simd_expose_provenance::<ptrx2, usizex2>(simd_arith_offset({ptrs}, isizex2([1, -2])))
            }};"
        ),
        lanes([12usize, 8].map(usize::to_le_bytes)),
    );
    check(
        &format!(
            "const GOAL: usizex2 = unsafe {{
                let bytes = simd_cast_ptr::<ptrx2, bptrx2>({ptrs});
                simd_expose_provenance::<bptrx2, usizex2>(simd_arith_offset(bytes, isizex2([1, 1])))
            }};"
        ),
        lanes([9usize, 17].map(usize::to_le_bytes)),
    );
}

#[test]
fn simd_memory() {
    let check = |goal: &str, answer| check_number(&with_simd(goal), answer);
    check(
        "const GOAL: i32x4 = unsafe {
            let arr = [10, 20, 30, 40];
            let base = simd_splat::<ptrx4, *const i32>(&arr as *const [i32; 4] as *const i32);
            let ptrs = simd_arith_offset(base, isizex4([3, 0, 1000000, 2]));
            simd_gather(i32x4([-1, -1, -1, -1]), ptrs, i32x4([-1, -1, 0, -1]))
        };",
        i32s([40, 10, -1, 30]),
    );
    check(
        "const GOAL: [i32; 4] = unsafe {
            let mut arr = [0, 0, 0, 0];
            let base = simd_splat::<mptrx4, *mut i32>(&mut arr as *mut [i32; 4] as *mut i32);
            let ptrs = simd_arith_offset(base, isizex4([3, 2, 1, 0]));
            simd_scatter(i32x4([1, 2, 3, 4]), ptrs, i32x4([-1, -1, 0, -1]));
            arr
        };",
        i32s([4, 0, 2, 1]),
    );
    check(
        "const GOAL: i32x4 = unsafe {
            let arr = [10, 20, 30, 40];
            simd_masked_load::<i32x4, *const i32, i32x4, { SimdAlign::Element }>(
                i32x4([-1, 0, -1, 0]),
                &arr as *const [i32; 4] as *const i32,
                i32x4([7, 7, 7, 7]),
            )
        };",
        i32s([10, 7, 30, 7]),
    );
    check(
        "const GOAL: [i32; 4] = unsafe {
            let mut arr = [0, 0, 0, 0];
            simd_masked_store::<i32x4, *mut i32, i32x4, { SimdAlign::Unaligned }>(
                i32x4([0, -1, -1, 0]),
                &mut arr as *mut [i32; 4] as *mut i32,
                i32x4([1, 2, 3, 4]),
            );
            arr
        };",
        i32s([0, 2, 3, 0]),
    );
}

#[test]
fn simd_masks() {
    let check = |goal: &str, answer| check_number(&with_simd(goal), answer);
    let pair = "i32x4([1, 2, 3, 4]), i32x4([5, 6, 7, 8])";
    check(
        &format!("const GOAL: i32x4 = unsafe {{ simd_select(i32x4([-1, 0, -1, 0]), {pair}) }};"),
        i32s([1, 6, 3, 8]),
    );
    check(
        &format!("const GOAL: i32x4 = unsafe {{ simd_select(i8x4([0, -1, 0, -1]), {pair}) }};"),
        i32s([5, 2, 7, 4]),
    );
    check(
        &format!("const GOAL: i32x4 = unsafe {{ simd_select_bitmask(0b0101u8, {pair}) }};"),
        i32s([1, 6, 3, 8]),
    );
    check(
        &format!("const GOAL: i32x4 = unsafe {{ simd_select_bitmask([0b1010u8], {pair}) }};"),
        i32s([5, 2, 7, 4]),
    );
    check("const GOAL: u8 = unsafe { simd_bitmask::<i32x4, u8>(i32x4([-1, 0, -1, -1])) };", 0b1101);
    check(
        "const GOAL: [u8; 2] = unsafe {
            simd_bitmask::<i8x16, [u8; 2]>(i8x16([-1, 0, 0, 0, 0, 0, 0, 0, -1, -1, 0, 0, 0, 0, -1, 0]))
        };",
        0b0100001100000001,
    );
    check_fail(
        &with_simd(&format!(
            "const GOAL: i32x4 = unsafe {{ simd_select(i32x4([1, 0, 0, 0]), {pair}) }};"
        )),
        is_ub,
    );
    check_fail(
        &with_simd("const GOAL: u8 = unsafe { simd_bitmask::<i32x4, u8>(i32x4([0, 2, 0, 0])) };"),
        is_ub,
    );
}

#[test]
fn simd_lanes() {
    let check = |goal: &str, answer| check_number(&with_simd(goal), answer);
    check("const GOAL: i32 = unsafe { simd_extract::<i32x4, i32>(i32x4([1, 2, 3, 4]), 2) };", 3);
    check(
        "const GOAL: i32 = unsafe { simd_extract_dyn::<i32x4, i32>(i32x4([1, 2, 3, 4]), 3) };",
        4,
    );
    check(
        "const GOAL: i32x4 = unsafe { simd_insert::<i32x4, i32>(i32x4([1, 2, 3, 4]), 1, 9) };",
        i32s([1, 9, 3, 4]),
    );
    check(
        "const GOAL: i32x4 = unsafe { simd_insert_dyn::<i32x4, i32>(i32x4([1, 2, 3, 4]), 3, -5) };",
        i32s([1, 2, 3, -5]),
    );
    check("const GOAL: i32x4 = unsafe { simd_splat::<i32x4, i32>(7) };", i32s([7, 7, 7, 7]));
    let pair = "i32x4([1, 2, 3, 4]), i32x4([5, 6, 7, 8])";
    check(
        &format!(
            "const GOAL: i32x4 = unsafe {{ simd_shuffle::<i32x4, [u32; 4], i32x4>({pair}, [7, 0, 5, 2]) }};"
        ),
        i32s([8, 1, 6, 3]),
    );
    check(
        &format!(
            "const GOAL: i32x4 = unsafe {{ simd_shuffle::<i32x4, u32x4, i32x4>({pair}, u32x4([3, 3, 4, 4])) }};"
        ),
        i32s([4, 4, 5, 5]),
    );
    let fail = |goal: &str| check_fail(&with_simd(goal), is_ub);
    fail("const GOAL: i32 = unsafe { simd_extract::<i32x4, i32>(i32x4([1, 2, 3, 4]), 4) };");
    fail(
        "const GOAL: i32x4 = unsafe { simd_insert_dyn::<i32x4, i32>(i32x4([1, 2, 3, 4]), 7, 0) };",
    );
    fail(&format!(
        "const GOAL: i32x4 = unsafe {{ simd_shuffle::<i32x4, [u32; 4], i32x4>({pair}, [0, 8, 0, 0]) }};"
    ));
}

#[test]
fn simd_reduce() {
    let check = |goal: &str, answer| check_number(&with_simd(goal), answer);
    check(
        "const GOAL: i32 = unsafe { simd_reduce_add_ordered(i32x4([1, 2, 3, 2147483647]), 10) };",
        -2147483633,
    );
    check("const GOAL: i32 = unsafe { simd_reduce_mul_ordered(i32x4([2, 3, 4, 5]), -1) };", -120);
    check(
        "const GOAL: i32 = unsafe { simd_reduce_add_unordered::<i32x4, i32>(i32x4([1, 2, 3, 4])) };",
        10,
    );
    check(
        "const GOAL: i32 = unsafe { simd_reduce_mul_unordered::<i32x4, i32>(i32x4([1, 2, 3, 4])) };",
        24,
    );
    check(
        "const GOAL: f32 = unsafe { simd_reduce_add_ordered(f32x4([1e8, 1.0, -1e8, 1.0]), 0.5) };",
        i128::from(((((0.5f32 + 1e8) + 1.0) + -1e8) + 1.0).to_bits()),
    );
    check(
        "const GOAL: f32 = unsafe { simd_reduce_add_unordered::<f32x4, f32>(f32x4([1e8, 1.0, -1e8, 1.0])) };",
        i128::from((((1e8f32 + 1.0) + -1e8) + 1.0).to_bits()),
    );
    check(
        "const GOAL: f32 = unsafe { simd_reduce_mul_ordered(f32x4([2.0, 0.5, 3.0, 1.0]), 1.5) };",
        i128::from(4.5f32.to_bits()),
    );
    let bits = "u32x4([0xff, 0x0f, 0xf3, 0x1f])";
    check(&format!("const GOAL: u32 = unsafe {{ simd_reduce_and::<u32x4, u32>({bits}) }};"), 0x03);
    check(&format!("const GOAL: u32 = unsafe {{ simd_reduce_or::<u32x4, u32>({bits}) }};"), 0xff);
    check(&format!("const GOAL: u32 = unsafe {{ simd_reduce_xor::<u32x4, u32>({bits}) }};"), 0x1c);
    let signed = "i32x4([3, -7, 2147483647, 0])";
    check(&format!("const GOAL: i32 = unsafe {{ simd_reduce_min::<i32x4, i32>({signed}) }};"), -7);
    check(
        &format!("const GOAL: i32 = unsafe {{ simd_reduce_max::<i32x4, i32>({signed}) }};"),
        2147483647,
    );
    let unsigned = "u32x4([3, 4294967295, 1, 7])";
    check(&format!("const GOAL: u32 = unsafe {{ simd_reduce_min::<u32x4, u32>({unsigned}) }};"), 1);
    check(
        &format!("const GOAL: u32 = unsafe {{ simd_reduce_max::<u32x4, u32>({unsigned}) }};"),
        0xffffffff,
    );
    let floats = "f32x4([0.0 / 0.0, 2.0, -1.0, 5.0])";
    check(
        &format!("const GOAL: f32 = unsafe {{ simd_reduce_min::<f32x4, f32>({floats}) }};"),
        i128::from((-1.0f32).to_bits()),
    );
    check(
        &format!("const GOAL: f32 = unsafe {{ simd_reduce_max::<f32x4, f32>({floats}) }};"),
        i128::from(5.0f32.to_bits()),
    );
    check("const GOAL: bool = unsafe { simd_reduce_all(i32x4([-1, -1, -1, 0])) };", 0);
    check("const GOAL: bool = unsafe { simd_reduce_all(i32x4([-1, -1, -1, -1])) };", 1);
    check("const GOAL: bool = unsafe { simd_reduce_any(i32x4([0, 0, -1, 0])) };", 1);
    check("const GOAL: bool = unsafe { simd_reduce_any(i32x4([0, 0, 0, 0])) };", 0);
    check_fail(
        &with_simd("const GOAL: bool = unsafe { simd_reduce_any(i32x4([2, 0, 0, 0])) };"),
        is_ub,
    );
}
