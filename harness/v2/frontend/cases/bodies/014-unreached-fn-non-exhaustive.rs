//@ expect: reject E0004
enum Light {
    Red,
    Amber,
    Green,
}

fn unused(light: Light) -> u8 {
    match light { //~ ERROR E0004
        Light::Red => 0,
        Light::Amber => 1,
    }
}

fn main() {
    let _ = Light::Green;
}
