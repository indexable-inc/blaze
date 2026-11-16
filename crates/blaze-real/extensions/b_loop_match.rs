//! loop match: nightly's `#[loop_match]` / `#[const_continue]`, which blaze lowers to a jump into the arm. A working layer accepts it.
//! expect: accept
#![feature(loop_match)]

fn run() -> i32 {
    let mut state = 0u8;
    #[loop_match]
    loop {
        state = 'blk: {
            match state {
                0 => {
                    #[const_continue]
                    break 'blk 1;
                }
                _ => return 42,
            }
        };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    run()
}
