//! D112 interim, brief N7: the multi-call fact shim a native build script finds first on its
//! `PATH` as `sw_vers`, `pkg-config` and `pkgconf` (blaze_build_script::fact_shim::PROGRAMS).
//! It answers only from the pins file blaze's native runner wrote for the run and refuses
//! anything else with exit 127 naming the program and its arguments; it never runs the
//! machine's tool. Shipped beside `blaze`; `blaze` copies it into its native work folder at the
//! command's start (blaze_proc_macro::native_facts::place_shim). Goes with D112's interim.
#![forbid(unsafe_code)]

fn main() {
    // The program's entry: the one place this process's heap is taken (D102).
    let code = blaze_proc_macro::native_facts::shim_main(blaze_own::Heap::process());
    std::process::exit(code);
}
