//@ expect: accept
fn capitalize(s: &mut [char]) {
    for c in s.iter_mut() {
        *c = c.to_ascii_uppercase();
    }
}

fn main() {
    let mut data = vec!['a', 'b', 'c'];
    let slice = &mut data[..];
    capitalize(slice);
    data.push('d');
    let first = &data[0];
    println!("{first}");
    data.push('e');
    println!("{:?}", data);
}
