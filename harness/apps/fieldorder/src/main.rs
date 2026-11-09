//! Field order (docs/v2/plan/design/field-order.md): a table of rows visited at random,
//! the loop reading two fields of each, `key` (declared first) and `weight` (declared last), and
//! never the payload between them.
//!
//! rustc sorts a `Row`'s fields by their size as an alignment (16, 168 and 4 bytes): `key` at 0,
//! `payload` at 16, `weight` at 184, so a visit reads two cache lines of the row. With a field
//! order plan from a profile of this program, blaze lays `weight` out beside `key` (at 16, the
//! payload after it, the row still 192 bytes), so a visit reads one. Everything printed is the
//! same whatever the layout, the row's size included.
//!
//!     fieldorder [ROWS_LOG2 [VISITS]]

use std::process::ExitCode;

/// A row of the table.
struct Row {
    key: [u32; 4],
    payload: [u64; 21],
    weight: u32,
}

/// A stream of numbers that is the same on every machine.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 11
    }
}

/// `rows` rows, every field written once.
#[inline(never)]
fn fill(rows: usize) -> Vec<Row> {
    let mut lcg = Lcg(0x6669_6c6c);
    let mut table = Vec::with_capacity(rows);
    for _ in 0..rows {
        let key = [lcg.next() as u32, lcg.next() as u32, lcg.next() as u32, lcg.next() as u32];
        let seed = lcg.next();
        let mut payload = [0u64; 21];
        for (at, it) in payload.iter_mut().enumerate() {
            *it = seed.rotate_left(at as u32);
        }
        table.push(Row { key, payload, weight: lcg.next() as u32 });
    }
    table
}

/// `visits` rows picked at random, each read for its `key` and `weight`: the hot loop.
#[inline(never)]
fn visit(rows: &[Row], visits: u64) -> u64 {
    let mut lcg = Lcg(0x7669_7369_74);
    // `rows.len()` is a power of two.
    let mask = rows.len() as u64 - 1;
    let mut sum = 0u64;
    for _ in 0..visits {
        let row = &rows[(lcg.next() & mask) as usize];
        sum = sum.wrapping_mul(31).wrapping_add(u64::from(row.key[0] ^ row.weight));
    }
    sum
}

/// The payload of the first rows, read once: the cold path.
#[inline(never)]
fn checksum(rows: &[Row]) -> u64 {
    let mut sum = 0u64;
    for row in rows.iter().take(4096) {
        for &it in &row.payload {
            sum = sum.rotate_left(5) ^ it;
        }
    }
    sum
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = || {
        eprintln!("usage: fieldorder [ROWS_LOG2 (0 to 28) [VISITS]]");
        ExitCode::FAILURE
    };
    let rows_log2: u32 = match args.first().map(|it| it.parse::<u32>()) {
        None => 10,
        Some(Ok(it)) if it <= 28 => it,
        Some(_) => return usage(),
    };
    let visits: u64 = match args.get(1).map(|it| it.parse::<u64>()) {
        None => 10_000,
        Some(Ok(it)) => it,
        Some(Err(_)) => return usage(),
    };
    if args.len() > 2 {
        return usage();
    }
    let rows = fill(1 << rows_log2);
    let sum = visit(&rows, visits);
    println!(
        "{} rows of {} bytes, {visits} visits: sum {sum:016x}, payload {:016x}",
        rows.len(),
        std::mem::size_of::<Row>(),
        checksum(&rows)
    );
    ExitCode::SUCCESS
}
