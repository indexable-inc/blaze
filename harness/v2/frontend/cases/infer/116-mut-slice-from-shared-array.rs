//@ expect: reject E0308
fn fill(xs: &mut [u32]) {
    xs[0] = 1;
}

fn main() {
    let a = [0u32; 3];
    fill(&a); //~ ERROR E0308
}
