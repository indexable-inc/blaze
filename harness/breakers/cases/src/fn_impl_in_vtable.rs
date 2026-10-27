//! Found in blaze's self-test: hir-def's `lower_function` hands `&mut impl_trait_lower_fn`, a
//! `&mut &mut dyn FnMut(&mut ExprCollector, TypePtr, ThinVec<TypeBound>)`, on as a `&mut dyn
//! FnMut`. The vtable slot of that `&mut dyn FnMut` pointed at the body of core's
//! `impl FnMut<A> for &mut F`, which takes the arguments as one tuple, while a call through
//! the vtable passes them one by one: the method read the collector out of the first word of
//! the collector and the tests segfaulted. A tuple of one has the ABI of its field, so only
//! two arguments or more showed it. The same held for `call` and `call_once` of any type
//! with its own `Fn` impls in a vtable, as `Box<Box<dyn FnOnce>>` as `Box<dyn FnOnce>`.

#[derive(Debug, Clone, Copy)]
pub struct Ptr {
    pub kind: u16,
    pub start: u32,
    pub end: u32,
}

fn lower(f: &mut dyn FnMut(&mut Vec<u32>, Ptr, String) -> usize, store: &mut Vec<u32>) -> usize {
    f(store, Ptr { kind: 3, start: 10, end: 20 }, "bound".to_string())
}

fn shared(f: &dyn Fn(u64, Ptr, &str) -> String) -> String {
    f(9, Ptr { kind: 1, start: 2, end: 3 }, "x")
}

pub fn run() -> (usize, Vec<u32>, Vec<String>, String, String) {
    let mut bounds = Vec::new();
    let mut alloc = |store: &mut Vec<u32>, ptr: Ptr, bound: String| {
        store.push(ptr.kind as u32 + ptr.start + ptr.end);
        bounds.push(bound);
        store.len()
    };
    let mut inner: &mut dyn FnMut(&mut Vec<u32>, Ptr, String) -> usize = &mut alloc;
    let mut store = vec![1];
    // `&mut &mut dyn FnMut` as `&mut dyn FnMut`.
    let len = lower(&mut inner, &mut store);
    let len = len + lower(&mut inner, &mut store);
    let by_ref = |n: u64, p: Ptr, s: &str| format!("{n}/{}/{}/{}/{s}", p.kind, p.start, p.end);
    let inner_ref: &dyn Fn(u64, Ptr, &str) -> String = &by_ref;
    // `&&dyn Fn` as `&dyn Fn`.
    let called = shared(&inner_ref);
    let tail = String::from("!");
    let once: Box<dyn FnOnce(String, Ptr, u8) -> String> =
        Box::new(move |s, p, n| format!("{s}{}{n}{tail}", p.end));
    // `Box<Box<dyn FnOnce>>` as `Box<dyn FnOnce>`.
    let boxed: Box<dyn FnOnce(String, Ptr, u8) -> String> = Box::new(once);
    let once = boxed("once".to_string(), Ptr { kind: 0, start: 0, end: 5 }, 6);
    drop(alloc);
    (len, store, bounds, called, once)
}
