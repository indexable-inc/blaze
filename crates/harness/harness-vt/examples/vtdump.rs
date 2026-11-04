//! `vtdump COLS ROWS < bytes`: feed bytes into a fresh terminal and print its screen text, then every styled cell,
//! the cursor and modes. For checking attach snapshots by hand.
use std::io::Read;

fn main() {
    let a: Vec<u16> = std::env::args().skip(1).map(|s| s.parse().expect("COLS ROWS")).collect();
    let mut t = harness_vt::Term::new(a[0], a[1], 1000);
    let mut buf = Vec::new();
    std::io::stdin().read_to_end(&mut buf).unwrap();
    t.feed(&buf);
    println!("{}\n----", t.text());
    let (rows, _) = t.rows();
    for (y, r) in rows.iter().enumerate() {
        for (x, c) in r.cells.iter().enumerate() {
            let plain = harness_vt::Cell { text: c.text.clone(), wide: c.wide, spacer: c.spacer, ..Default::default() };
            if *c != plain || c.wide || c.text.chars().count() > 1 {
                println!("{y},{x} {:?} fg={:?} bg={:?} ul={:?}/{:?} b={} i={} inv={} wide={} link={:?}", c.text, c.fg, c.bg, c.underline, c.underline_color, c.bold, c.italic, c.inverse, c.wide, c.link);
            }
        }
    }
    println!("cursor={:?}\nmodes={:?}\ntitle={:?}", t.cursor(), t.modes(), t.title());
}
