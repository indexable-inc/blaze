//! refused: cannot construct `Ancestor` with struct literal syntax due to private fields
// An `Ancestor` is made only by `Fs::ancestors`, from the driver's answer cut to the granted
// top: no literal names a folder further up, and no field of one can be set.
use bstd::fs::Ancestor;

pub fn run() -> Ancestor {
    Ancestor { up: 9 }
}
