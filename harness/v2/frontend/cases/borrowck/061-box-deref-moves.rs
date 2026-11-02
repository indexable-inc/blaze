//@ expect: accept
fn main() {
    let b = Box::new(String::from("boxed"));
    let s = *b;
    let bt = Box::new((String::from("x"), String::from("y")));
    let x = bt.0;
    let y = bt.1;
    let mut bb = Box::new(String::from("re"));
    let old = *bb;
    *bb = old + "init";
    println!("{s} {x} {y} {bb}");
}
