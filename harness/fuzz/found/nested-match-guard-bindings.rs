// Reduced from edits.py seed blaze-edit-holdout-20261027-27, edition 2024.
mod c0 {
    use super::*;

    fn pick(value: bool, pair: (u8, u8)) -> bool {
        match value {
            outer if match pair {
                (1, inner) if inner == 1 => outer,
                (0, inner) | (inner, 0) if inner == 1 => !outer,
                _ => outer,
            } => true,
            _ => false,
        }
    }

    pub fn run() {
        for pair in [(1, 1), (0, 1), (1, 0), (3, 3)] {
            for value in [false, true] {
                feed(pick(value, pair) as u64);
            }
        }
    }
}
