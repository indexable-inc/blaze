// Rung 7a: drops. The shape is reth's `BudgetGuard` (crates/metrics/src/common/mpsc.rs): a guard
// that gives its size back to a budget when dropped, held alone and as a field of an envelope
// (`Budgeted`). What it shows:
// - drops at scope end, in reverse declaration order;
// - a field's drop through its owner's drop glue;
// - a value moved into a call is dropped by the callee, once;
// - a moved-from local is not dropped.
// Each drop prints its label. Nightly needs the `drop` and `drop_in_place` lang items under
// `no_core` for drop glue; whether it also asks for the sized hierarchy's `meta_sized` and
// `pointee_sized` is not checked here (nothing may run rustc).
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impls (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl.
impl Copy for usize {}
impl Copy for u8 {}
impl<T> Copy for *mut T {}

#[lang = "drop"]
trait Drop {
    fn drop(&mut self);
}

#[lang = "drop_in_place"]
unsafe fn drop_in_place<T>(to_drop: *mut T) {
    unsafe { drop_in_place(to_drop) }
}

unsafe extern "C" {
    fn write(fd: i32, buf: *const u8, len: usize) -> isize;
}

struct Budget {
    used: usize,
}

struct BudgetGuard {
    size: usize,
    budget: *mut Budget,
    label: u8,
}

impl Drop for BudgetGuard {
    fn drop(&mut self) {
        unsafe {
            (*self.budget).used -= self.size;
            let line = [b'd', b'r', b'o', b'p', b' ', self.label, b'\n'];
            write(1, &line as *const [u8; 7] as *const u8, 7);
        }
    }
}

struct Budgeted {
    msg: u64,
    guard: BudgetGuard,
}

fn consume(guard: BudgetGuard) {}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut budget = Budget { used: 60 };
    let p = &mut budget as *mut Budget;
    {
        let a = BudgetGuard { size: 10, budget: p, label: b'a' };
        let b = Budgeted { msg: 1, guard: BudgetGuard { size: 5, budget: p, label: b'b' } };
        let c = BudgetGuard { size: 3, budget: p, label: b'c' };
        consume(c);
        let d = BudgetGuard { size: 0, budget: p, label: b'd' };
        let e = d;
    }
    // 60 - 10 - 5 - 3 = 42; printed: c (in `consume`), then d (as `e`), b, a.
    budget.used as i32
}
