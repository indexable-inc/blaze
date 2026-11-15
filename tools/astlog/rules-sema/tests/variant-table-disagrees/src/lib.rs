//! The shape of `crates/blaze-api/src/frames/identity.rs`: a generator picks a variant by a
//! hand-kept integer, and an index maps each variant back to its integer.

pub enum Verb {
    Compiling,
    Checking,
}

pub enum Request {
    Commit { base: u64 },
    Check,
    Build,
    Status,
    Stop,
    Query(u32),
}

pub enum Response {
    Committed { root: u64 },
    Unit(Verb),
    Done,
}

pub const REQUESTS: usize = 6;
pub const RESPONSES: usize = 3;

pub struct Gen {
    n: u64,
}

impl Gen {
    fn below(&mut self, n: u64) -> u64 {
        self.n % n
    }

    // Found: `Status` and `Build` swapped against `request_index`.
    pub fn request(&mut self) -> Request {
        match self.below(REQUESTS as u64) {
            0 => Request::Commit { base: self.below(9) },
            1 => Request::Check,
            2 => Request::Status,
            3 => {
                Request::Build
            }
            5 => Request::Query(self.below(3) as u32),
            _ => Request::Stop,
        }
    }

    // Not found: agrees with `response_index`; `Verb::Checking` inside arm 1 is no arm's variant.
    pub fn response(&mut self) -> Response {
        match self.below(RESPONSES as u64) {
            0 => Response::Committed { root: self.below(7) },
            1 => Response::Unit(match self.below(2) {
                0 => Verb::Compiling,
                _ => Verb::Checking,
            }),
            _ => Response::Done,
        }
    }
}

pub fn request_index(request: &Request) -> usize {
    match request {
        Request::Commit { .. } => 0,
        Request::Check => 1,
        Request::Build => 2,
        Request::Status => 3,
        Request::Stop => 4,
        Request::Query(_) => 5,
    }
}

pub fn response_index(response: &Response) -> usize {
    match response {
        Response::Committed { .. } => 0,
        Response::Unit(_) => 1,
        Response::Done => 2,
    }
}

// Not found: an integer table of plain numbers, no variants.
pub fn width(n: u64) -> u32 {
    match n {
        0 => 8,
        1 => 16,
        _ => 32,
    }
}
