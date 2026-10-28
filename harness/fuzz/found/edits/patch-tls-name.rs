// Found by edits.py, seed blaze-real-edit-20261026-22, edition 2024, backend cranelift.
// Removing two strings changes the data numbering. A new thread-local used to get the
// symbol of K0 in the base object while the edited function still imported K0.
mod c0 {
    use super::*;
    use std::cell::Cell;
    static K0: u32 = 1;
    pub fn run() {
        std::hint::black_box("pad 0");
        std::hint::black_box("pad 1");
        let mut x = 0u32;
        x += unsafe { std::ptr::read_volatile(&K0) };
        feed(x.into());
    }
}
//// then
mod c0 {
    use super::*;
    use std::cell::Cell;
    static K0: u32 = 1;
    thread_local! { static T0: Cell<u32> = const { Cell::new(1) }; }
    thread_local! { static T1: Cell<u32> = const { Cell::new(2) }; }
    pub fn run() {
        let mut x = 0u32;
        x += unsafe { std::ptr::read_volatile(&K0) };
        x += T0.with(|v| v.get());
        x += T1.with(|v| v.get());
        feed(x.into());
    }
}
