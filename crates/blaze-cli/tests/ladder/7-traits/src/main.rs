// Rung 7: a trait, two impls, static dispatch. The shape is reth's `InMemorySize`
// (reth-primitives-traits; implemented by block responses in crates/net/p2p/src/bodies/response.rs):
// `fn size(&self) -> usize` on two block parts, called through a generic bound, by method
// syntax and by path. The checker is blaze-typeck over blaze-ty and blaze-solve
// (`InferCtx`); monomorphization resolves `InMemorySize::size` at `Header` and `Body` to the
// impls' methods (`resolve_instance`).
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for usize {}

trait InMemorySize {
    fn size(&self) -> usize;
}

struct Header {
    number: u64,
    extra_len: usize,
}

struct Body {
    tx_count: usize,
    ommers: usize,
}

impl InMemorySize for Header {
    fn size(&self) -> usize {
        8 + self.extra_len
    }
}

impl InMemorySize for Body {
    fn size(&self) -> usize {
        self.tx_count * 4 + self.ommers
    }
}

fn size_of<T: InMemorySize>(item: &T) -> usize {
    item.size()
}

fn sum<A: InMemorySize, B: InMemorySize>(a: &A, b: &B) -> usize {
    size_of(a) + InMemorySize::size(b)
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let header = Header { number: 1, extra_len: 4 };
    let body = Body { tx_count: 7, ommers: 2 };
    // 12 + 30 = 42.
    sum(&header, &body) as i32
}
