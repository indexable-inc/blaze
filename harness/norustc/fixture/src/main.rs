use std::arch::asm;

fn twice(x: u64) -> u64 {
    let out: u64;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        asm!("lea {out}, [{x} + {x}]", x = in(reg) x, out = lateout(reg) out)
    };
    #[cfg(target_arch = "aarch64")]
    unsafe {
        asm!("add {out}, {x}, {x}", x = in(reg) x, out = lateout(reg) out)
    };
    out
}

fn report(text: &str) -> String {
    let words = lines::count(text);
    format!("{} words, level {}, {}", words, outer::LEVEL, twice(words as u64))
}

fn main() {
    println!("{}", report("the quick brown fox"));
    println!("{}", shout::shout!(done));
}

#[cfg(test)]
mod tests {
    #[test]
    fn reports() {
        assert!(super::report("a b").starts_with("2 words"));
    }
}
