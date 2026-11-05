//! blaze's rustfmt as a binary taking rustfmt's arguments, for the parity harness to run before
//! `blaze fmt` dispatches to it.

fn main() {
    std::process::exit(blaze_fmt::cli::main(std::env::args().skip(1).collect()));
}
