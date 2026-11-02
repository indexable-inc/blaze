//@ expect: accept
//@ edition: 2021
// One receiver and name probed from bodies with different param envs.
trait Show {
    fn show(&self) -> u32;
}
impl Show for u32 {
    fn show(&self) -> u32 {
        1
    }
}

fn one<T: Show>(t: &T) -> u32 {
    t.show() + 5u32.show()
}

fn two<T>(_t: &T) -> u32 {
    5u32.show()
}

fn three() -> u32 {
    5u32.show() + (&5u32).show()
}

fn main() {
    println!("{}", one(&1u32) + two(&()) + three());
}
