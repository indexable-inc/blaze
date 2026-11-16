//! x-match-11: `loop` returning a value, labelled `break` with a value, labelled `continue`.
//!
//! The Reference, "Loops and other breakable expressions": "`break` expressions ... When
//! associated with a `loop`, a break expression may be used to return a value from that loop";
//! "labelled block expressions ... `break 'label value`"; "`continue 'label`" continues the
//! labelled loop.
//! 1. i counts 1, 2, .. and the loop breaks with i * 2 when i == 5: 10.
//! 2. nested loops: the inner `break 'outer i * 100 + j` at the first i * j == 6 with i, j in
//!    1..=4 by rows: i = 2, j = 3: 203.
//! 3. a labelled block: `break 'b 1` when its flag is set gives 1, else the tail 2.
//! 4. `continue 'rows` skips the rest of a row at its first j > i: pairs with j <= i in a
//!    4 x 4 grid: 1 + 2 + 3 + 4 = 10.
//! A wrong builder that breaks only the inner loop exits 2.

fn block(flag: bool) -> u32 {
    'b: {
        if flag {
            break 'b 1;
        }
        2
    }
}

fn main() -> std::entry::Status {
    let mut i = 0;
    let doubled = loop {
        i += 1;
        if i == 5 {
            break i * 2;
        }
    };
    let found = 'outer: loop {
        for a in 1..=4u32 {
            for b in 1..=4u32 {
                if a * b == 6 {
                    break 'outer a * 100 + b;
                }
            }
        }
        break 0;
    };
    let mut pairs = 0;
    'rows: for a in 1..=4u32 {
        for b in 1..=4u32 {
            if b > a {
                continue 'rows;
            }
            pairs += 1;
        }
    }
    let status = if doubled != 10 {
        1
    } else if found != 203 {
        2
    } else if block(true) != 1 || block(false) != 2 {
        3
    } else if pairs != 10 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
