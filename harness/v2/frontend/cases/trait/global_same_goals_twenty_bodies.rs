//@ expect: accept
//@ edition: 2021
// The same global goals (`String: Display`, `Vec<u8>: Clone`, `u32: Copy`, `String: Sized`)
// asked from twenty bodies: every body after the first is a global-goal table hit.
macro_rules! bodies {
    ($($name:ident),*) => {
        $(
            fn $name(s: &String, v: &Vec<u8>, k: u32) -> u32 {
                let t = format!("{}", s);
                let w = v.clone();
                let j = k;
                t.len() as u32 + w.len() as u32 + j + k
            }
        )*
        fn all(s: &String, v: &Vec<u8>) -> u32 {
            0 $(+ $name(s, v, 1))*
        }
    };
}

bodies!(b00, b01, b02, b03, b04, b05, b06, b07, b08, b09, b10, b11, b12, b13, b14, b15, b16, b17, b18, b19);

fn main() {
    let s = String::from("ab");
    let v = vec![1u8, 2, 3];
    println!("{}", all(&s, &v));
}
