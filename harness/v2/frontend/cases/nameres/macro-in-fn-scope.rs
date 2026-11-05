//@ expect: accept
//@ edition: 2021
fn main() {
    macro_rules! local {
        ($x:expr) => {
            $x * 7
        };
    }
    let n = local!(2);
    {
        macro_rules! local {
            ($x:expr) => {
                $x + 1
            };
        }
        println!("{}", local!(n));
    }
    println!("{}", local!(n));
}
