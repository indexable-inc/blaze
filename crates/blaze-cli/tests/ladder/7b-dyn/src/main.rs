// Rung 7b: a `dyn` call. The shape is reth's `Box<dyn BlobStore>` handle
// (crates/net/network/src/eth_requests.rs), written as `&dyn` because `no_core` has no `Box`:
// sizes summed over trait objects of two types, each call through the object's vtable.
// The unsizing coercion `&T` to `&dyn InMemorySize` needs the `unsize` and `coerce_unsized`
// lang items under `no_core` (as in rustc's minicore); `?Sized` needs the sized hierarchy's
// `meta_sized` and `pointee_sized`. Whether the pinned nightly asks for more is not checked here.
#![feature(no_core, lang_items, unsize, coerce_unsized)]
#![no_core]
#![no_main]

#[lang = "pointee_sized"]
trait PointeeSized {}

#[lang = "meta_sized"]
trait MetaSized: PointeeSized {}

#[lang = "sized"]
trait Sized: MetaSized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for usize {}

#[lang = "unsize"]
trait Unsize<T: PointeeSized>: PointeeSized {}

#[lang = "coerce_unsized"]
trait CoerceUnsized<T: PointeeSized> {}

impl<'a, 'b: 'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<&'a U> for &'b T {}

trait InMemorySize {
    fn size(&self) -> usize;
}

struct Header {
    extra_len: usize,
}

struct Body {
    tx_count: usize,
}

impl InMemorySize for Header {
    fn size(&self) -> usize {
        8 + self.extra_len
    }
}

impl InMemorySize for Body {
    fn size(&self) -> usize {
        self.tx_count * 4
    }
}

fn total(items: &[&dyn InMemorySize; 3]) -> usize {
    let mut sum = 0;
    let mut i = 0;
    while i < 3 {
        sum += items[i].size();
        i += 1;
    }
    sum
}

#[lang = "panic_bounds_check"]
fn panic_bounds_check(index: usize, len: usize) -> ! {
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let header = Header { extra_len: 2 };
    let body = Body { tx_count: 5 };
    let other = Header { extra_len: 4 };
    // 10 + 20 + 12 = 42.
    let items: [&dyn InMemorySize; 3] = [&header, &body, &other];
    total(&items) as i32
}
