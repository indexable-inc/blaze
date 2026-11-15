//! The shape of `crates/blaze-api/src/frames/identity.rs`: `const REQUESTS: usize = 39;` bounds
//! the integer a generator matches on, and the match's arms must number exactly that.

pub enum Request {
    Commit,
    Check,
    Build,
    Status,
    Stop,
}

pub enum Response {
    Committed,
    Built,
    Done,
}

pub const REQUESTS: usize = 5;
pub const RESPONSES: usize = 4;
pub const SHIFT: u32 = 3;

pub struct Gen {
    n: u64,
}

impl Gen {
    fn below(&mut self, n: u64) -> u64 {
        self.n % n
    }

    // Not found: five arms under a bound of 5.
    pub fn request(&mut self) -> Request {
        match self.below(REQUESTS as u64) {
            0 => Request::Commit,
            1 => Request::Check,
            2 => Request::Build,
            3 => Request::Status,
            _ => Request::Stop,
        }
    }

    // Found: three arms under a bound of 4; the fourth integer is the `_` arm's twice.
    pub fn response(&mut self) -> Response {
        match self.below(RESPONSES as u64) {
            0 => Response::Committed,
            1 => Response::Built,
            _ => Response::Done,
        }
    }

    // Not found: the const is a shift, not a bound of the match.
    pub fn shifted(&mut self) -> u64 {
        match self.n >> SHIFT {
            0 => 1,
            _ => 2,
        }
    }
}
