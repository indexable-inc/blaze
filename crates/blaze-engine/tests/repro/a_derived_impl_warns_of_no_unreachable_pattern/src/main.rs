#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Ctrl<const SLOTS: usize>(u64);

fn main() {
    let ctrl = Ctrl::<7>(3);
    println!("{:?} {}", ctrl.clone(), ctrl == Ctrl::default());
}
