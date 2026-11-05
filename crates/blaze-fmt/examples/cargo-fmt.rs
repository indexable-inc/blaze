//! blaze's `cargo fmt` as a binary taking the arguments after `fmt`, for the parity harness to
//! run before `blaze fmt` dispatches to it.

fn main() {
    std::process::exit(blaze_fmt::cargo_fmt::main(
        std::env::args().skip(1).collect(),
    ));
}
