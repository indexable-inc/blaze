//@ expect: accept
struct D(String);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn main() {
    let mut found = (9, 9);
    'outer: for i in 0..3 {
        let _i = D(format!("i{i}"));
        for j in 0..3 {
            let _j = D(format!("j{j}"));
            if i * j == 2 {
                found = (i, j);
                break 'outer;
            }
            if j == i {
                continue 'outer;
            }
        }
    }
    println!("{found:?}");
    let mut n = 0;
    let looped = 'looped: loop {
        n += 1;
        let _n = D(format!("n{n}"));
        loop {
            let _k = D(format!("k{n}"));
            if n == 2 {
                break 'looped n * 100;
            }
            break;
        }
    };
    println!("{looped}");
    let value = 'block: {
        let _b = D("block".to_string());
        if found.0 == 2 {
            break 'block 1;
        }
        2
    };
    println!("{value}");
}
