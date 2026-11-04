//! Zig `std.hash.Wyhash` (0.16, streaming) as a `const fn`, byte for byte, so layout hashes computed at compile time
//! in Rust equal `plugin.layoutHash` in Zig for the same type. Source: zig 0.16.0 `lib/std/hash/wyhash.zig`.

const SECRET: [u64; 4] = [0xa0761d6478bd642f, 0xe7037ed1a0b428db, 0x8ebc6af09c88c6e3, 0x589965cc75374cc3];

const fn mix(a: u64, b: u64) -> u64 {
    let x = (a as u128).wrapping_mul(b as u128);
    (x as u64) ^ ((x >> 64) as u64)
}

/// Little-endian read of `n` (at most 8) bytes at `at`.
const fn read(data: &[u8], at: usize, n: usize) -> u64 {
    let mut v = 0u64;
    let mut i = 0;
    while i < n {
        v |= (data[at + i] as u64) << (8 * i);
        i += 1;
    }
    v
}

#[derive(Clone, Copy)]
pub struct Wyhash {
    a: u64,
    b: u64,
    state: [u64; 3],
    total_len: usize,
    buf: [u8; 48],
    buf_len: usize,
}

impl Wyhash {
    pub const fn new(seed: u64) -> Self {
        let s0 = seed ^ mix(seed ^ SECRET[0], SECRET[1]);
        Wyhash { a: 0, b: 0, state: [s0, s0, s0], total_len: 0, buf: [0; 48], buf_len: 0 }
    }

    const fn round(&mut self, input: &[u8], at: usize) {
        let mut i = 0;
        while i < 3 {
            let a = read(input, at + 8 * (2 * i), 8);
            let b = read(input, at + 8 * (2 * i + 1), 8);
            self.state[i] = mix(a ^ SECRET[i + 1], b ^ self.state[i]);
            i += 1;
        }
    }

    /// Same state machine as Zig's `update`, including the "last full 48-byte block" quirk.
    pub const fn update(mut self, input: &[u8]) -> Self {
        self.total_len += input.len();
        if input.len() <= 48 - self.buf_len {
            let mut j = 0;
            while j < input.len() {
                self.buf[self.buf_len + j] = input[j];
                j += 1;
            }
            self.buf_len += input.len();
            return self;
        }
        let mut i = 0;
        if self.buf_len > 0 {
            i = 48 - self.buf_len;
            let mut j = 0;
            while j < i {
                self.buf[self.buf_len + j] = input[j];
                j += 1;
            }
            let b = self.buf;
            self.round(&b, 0);
            self.buf_len = 0;
        }
        while i + 48 < input.len() {
            self.round(input, i);
            i += 48;
        }
        let rem_len = input.len() - i;
        if rem_len < 16 && i >= 48 {
            let rem = 16 - rem_len;
            let mut j = 0;
            while j < rem {
                self.buf[48 - rem + j] = input[i - rem + j];
                j += 1;
            }
        }
        let mut j = 0;
        while j < rem_len {
            self.buf[j] = input[i + j];
            j += 1;
        }
        self.buf_len = rem_len;
        self
    }

    pub const fn update_u64(self, v: u64) -> Self {
        self.update(&v.to_le_bytes())
    }

    pub const fn finish(self) -> u64 {
        let mut s = self; // value semantics: idempotent
        if self.total_len <= 16 {
            let input = self.buf.split_at(self.buf_len).0;
            s.small_key(input);
        } else {
            let mut scratch = [0u8; 16];
            let mut offset = 0;
            let use_scratch = self.buf_len < 16;
            if use_scratch {
                let rem = 16 - self.buf_len;
                let mut j = 0;
                while j < rem {
                    scratch[j] = self.buf[48 - rem + j];
                    j += 1;
                }
                j = 0;
                while j < self.buf_len {
                    scratch[rem + j] = self.buf[j];
                    j += 1;
                }
                offset = rem;
            }
            s.state[0] ^= s.state[1] ^ s.state[2];
            if use_scratch {
                s.final1(&scratch, offset);
            } else {
                s.final1(self.buf.split_at(self.buf_len).0, offset);
            }
        }
        s.a ^= SECRET[1];
        s.b ^= s.state[0];
        let x = (s.a as u128).wrapping_mul(s.b as u128);
        let (a, b) = (x as u64, (x >> 64) as u64);
        mix(a ^ SECRET[0] ^ (s.total_len as u64), b ^ SECRET[1])
    }

    const fn small_key(&mut self, input: &[u8]) {
        let n = input.len();
        if n >= 4 {
            let end = n - 4;
            let quarter = (n >> 3) << 2;
            self.a = (read(input, 0, 4) << 32) | read(input, quarter, 4);
            self.b = (read(input, end, 4) << 32) | read(input, end - quarter, 4);
        } else if n > 0 {
            self.a = ((input[0] as u64) << 16) | ((input[n >> 1] as u64) << 8) | (input[n - 1] as u64);
            self.b = 0;
        } else {
            self.a = 0;
            self.b = 0;
        }
    }

    const fn final1(&mut self, input_lb: &[u8], start: usize) {
        let len = input_lb.len() - start;
        let mut i = 0;
        while i + 16 < len {
            self.state[0] = mix(read(input_lb, start + i, 8) ^ SECRET[1], read(input_lb, start + i + 8, 8) ^ self.state[0]);
            i += 16;
        }
        self.a = read(input_lb, input_lb.len() - 16, 8);
        self.b = read(input_lb, input_lb.len() - 8, 8);
    }
}

/// One-shot `Wyhash.hash(seed, input)` (equal to streaming over the same bytes).
pub const fn hash(seed: u64, input: &[u8]) -> u64 {
    Wyhash::new(seed).update(input).finish()
}
