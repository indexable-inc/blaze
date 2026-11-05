//@ expect: accept
//@ edition: 2021
// A method of a type parameter's bound.
trait Area {
    fn area(&self) -> u32;
}

struct Sq(u32);
impl Area for Sq {
    fn area(&self) -> u32 {
        self.0 * self.0
    }
}

fn total<T: Area>(xs: &[T]) -> u32 {
    xs.iter().map(|x| x.area()).sum()
}

fn main() {
    println!("{}", total(&[Sq(2), Sq(3)]));
}
