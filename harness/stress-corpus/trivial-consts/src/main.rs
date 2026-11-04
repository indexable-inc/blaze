//! libc-style constant load; the constants are generated into consts.rs by ../../gen.py.

mod consts;
mod evaluated;

use consts::*;
use evaluated::*;

fn main() {
    assert_eq!(TABLE.len(), 6_000);
    assert_eq!(TABLE.iter().sum::<i64>(), TABLE_SUM);
    assert_eq!(NAME_LAST, "name_0399");
    assert_eq!(LETTER_25, 'z');
    assert!(FLAG_00 && !FLAG_01);

    assert_eq!(O_ACCMODE, 3);
    assert_eq!(TIOCGWINSZ, 0x8008_5413);
    assert_eq!(FD_WORDS, 16);
    assert_eq!(EMPTY_SET.len(), FD_WORDS);
    assert_eq!(FIB_80, 23_416_728_348_467_685);
    assert_eq!(MASKS[31], 1 << 31);
    assert_eq!(SIG_NAMES[2], "QUIT");
    assert_eq!(SIGRTMAX - SIGRTMIN, 30);
    println!("trivial-consts ok: {} constants, sum {}", TABLE.len(), TABLE_SUM);
}
