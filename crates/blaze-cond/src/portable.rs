//! A condition's portable form: what a record holds, so two builders write equal bytes (rule 25)
//! and another task's arena can take it back.
//!
//! Little-endian: `root: u32`, `rows: u32`, then `rows` rows of `key: u64, low: u32, high: u32`,
//! 16 bytes each. A reference is `row << 1 | complemented`, rows counting from 1 and 0 the
//! constant true, so `0` is true and `1` false. The rows are the diagram's nodes in its one
//! canonical order: a depth-first walk from the root, low edge first, each node after its
//! children; the root is the last row. A variable is named by its [`VarKey`](crate::VarKey),
//! never its place in a request.
//!
//! [`import`] re-interns the rows into the task's arena and refuses, by row, anything that is
//! not exactly what [`export`] writes: a reference past the rows before it, an unknown key, a
//! child at or before its parent's variable, equal edges, a complemented high edge, a repeated
//! row, rows out of the canonical order or unreachable. So a record's bytes are canonical, and
//! equal conditions are equal bytes.

#![blaze::sans_io]

use blaze_int::{Int, idx};
use blaze_own::{AVec, planted::Planted};

use crate::bdd::{Bdd, Cond, PlantedBdd, Var};
use crate::error::{CondError, ImportError};
use crate::plant::CondPlant;
use crate::space::{Space, VarKey};

/// Bytes of the header.
pub const HEADER: usize = 8;
/// Bytes of one row.
pub const ROW: usize = 16;

/// Writes `cond`'s portable form into `out` (cleared first).
pub fn export(
    bdd: &mut PlantedBdd<'_, impl Planted<CondPlant>>,
    space: &Space<'_>,
    cond: Cond,
    out: &mut AVec<'_, u8>,
) -> Result<(), CondError> {
    out.clear();
    bdd.post_order(cond)?;
    let rows = bdd.order.len();
    out.try_reserve_exact(HEADER + rows * ROW)?;
    out.extend_from_slice(&bdd.slot_ref(cond).to_le_bytes());
    out.extend_from_slice(&rows.narrow_or_stop::<u32>().to_le_bytes());
    for &n in bdd.order.iter() {
        let (var, low, high) = bdd.node_of(n);
        out.extend_from_slice(&space.key(Var::new(var)).0.to_le_bytes());
        out.extend_from_slice(&bdd.slot_ref(low).to_le_bytes());
        out.extend_from_slice(&bdd.slot_ref(high).to_le_bytes());
    }
    Ok(())
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    let mut w = [0u8; 4];
    w.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(w)
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(w)
}

/// Takes a portable form back into `bdd`, whose variables are `space`'s.
pub fn import(
    bdd: &mut PlantedBdd<'_, impl Planted<CondPlant>>,
    space: &Space<'_>,
    bytes: &[u8],
) -> Result<Cond, ImportError> {
    let len = bytes.len();
    if len < HEADER || (len - HEADER) % ROW != 0 {
        return Err(ImportError::Length { len });
    }
    let rows = (len - HEADER) / ROW;
    let root = u32_at(bytes, 0);
    if idx(u32_at(bytes, 4)) != rows {
        return Err(ImportError::Length { len });
    }
    let Ok(rows32) = u32::try_from(rows) else { return Err(ImportError::Length { len }) };
    // Each row's condition and variable; index 0 is the constant.
    let mut made: AVec<'_, (Cond, u16)> = AVec::new_in(bdd.alloc());
    made.try_reserve_exact(rows + 1).map_err(CondError::from)?;
    made.push((Cond::TRUE, u16::MAX));
    for i in 1..=rows32 {
        let at = HEADER + idx(i - 1) * ROW;
        let key = u64_at(bytes, at);
        let (low, high) = (u32_at(bytes, at + 8), u32_at(bytes, at + 12));
        for r in [low, high] {
            if r >> 1 >= i {
                return Err(ImportError::Bounds { row: i, refers: r >> 1 });
            }
        }
        if high & 1 == 1 {
            return Err(ImportError::Complemented { row: i });
        }
        if low == high {
            return Err(ImportError::Redundant { row: i });
        }
        let Some(var) = space.by_key(VarKey(key)) else { return Err(ImportError::UnknownVariable { row: i, key }) };
        let (lc, lv) = made[idx(low >> 1)];
        let (hc, hv) = made[idx(high >> 1)];
        if lv <= var.index() || hv <= var.index() {
            return Err(ImportError::Order { row: i });
        }
        let lo = if low & 1 == 1 { lc.not() } else { lc };
        let node = bdd.mk(var.index(), lo, hc)?;
        made.push((node, var.index()));
    }
    if root >> 1 > rows32 || (rows32 > 0 && root >> 1 != rows32) {
        return Err(ImportError::NotCanonical { row: rows32 });
    }
    let (rc, _) = made[idx(root >> 1)];
    let cond = if root & 1 == 1 { rc.not() } else { rc };
    // The canonical order: the arena's walk from the root visits exactly the rows, in order.
    bdd.post_order(cond)?;
    let order_len = bdd.order.len();
    for i in 1..=rows {
        let expect = Bdd::node_raw(made[i].0);
        if bdd.order.get(i - 1).copied() != Some(expect) {
            let row = i.narrow_or_stop::<u32>();
            if made[1..i].iter().any(|(c, _)| Bdd::node_raw(*c) == expect) {
                return Err(ImportError::Duplicate { row });
            }
            return Err(ImportError::NotCanonical { row });
        }
    }
    if order_len != rows {
        return Err(ImportError::NotCanonical { row: rows32 });
    }
    Ok(cond)
}
