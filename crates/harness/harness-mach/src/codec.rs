//! The bytes of one message, sans IO: what [`encode`] lays out for `mach_msg` to send and what [`RecvBuf::decode`] reads
//! back from a receive. Always a complex message (`MACH_MSGH_BITS_COMPLEX`, even with no ports), so one layout covers
//! every message:
//!
//! ```text
//! mach_msg_header_t      24 bytes  bits, size, remote (the destination, COPY_SEND), local 0, voucher 0, id
//! mach_msg_body_t         4 bytes  descriptor count n
//! mach_msg_port_desc_t   12 bytes  x n: name, pad, pad, disposition COPY_SEND, type PORT
//! u32 len                 4 bytes  the body's length
//! body                  len bytes, then zeros to a 4-byte boundary
//! ```
//!
//! A received message has the kernel's trailer after it, which [`RecvBuf::decode`] ignores; port names in it are the
//! receiver's own, each one send right.
use crate::{MAX_BODY, MAX_PORTS};

/// The message id every harness message carries (`'hbrw'`), checked on receive.
pub const MSG_ID: i32 = 0x6862_7277;

const HEADER: usize = 24;
const BODY_COUNT: usize = 4;
const PORT_DESC: usize = 12;
const LEN: usize = 4;
/// A receive's room for the kernel's trailer (the largest, mach_msg_mac_trailer_t, is 52 bytes).
const TRAILER: usize = 64;

const MACH_MSGH_BITS_COMPLEX: u32 = 0x8000_0000;
const MACH_MSG_TYPE_COPY_SEND: u32 = 19;
const MACH_MSG_TYPE_PORT_SEND: u8 = 17;
const MACH_MSG_PORT_DESCRIPTOR: u8 = 0;

fn pad4(n: usize) -> usize {
    (n + 3) & !3
}

/// The message to send to `dest` carrying `ports` (copied send rights) and `body`, as `u32`s so it is 4-aligned.
#[must_use]
pub fn encode(dest: u32, ports: &[u32], body: &[u8]) -> Vec<u32> {
    let size = HEADER + BODY_COUNT + PORT_DESC * ports.len() + LEN + pad4(body.len());
    let mut bytes = Vec::with_capacity(size); // alloc: one message, sent and dropped
    let bits = MACH_MSG_TYPE_COPY_SEND | MACH_MSGH_BITS_COMPLEX;
    for w in [bits, size as u32, dest, 0, 0, MSG_ID as u32, ports.len() as u32] {
        bytes.extend_from_slice(&w.to_ne_bytes());
    }
    for &p in ports {
        bytes.extend_from_slice(&p.to_ne_bytes());
        bytes.extend_from_slice(&0u32.to_ne_bytes());
        // pad2 (u16), disposition (u8), type (u8), as the struct's bitfields lay out on a little-endian Mac.
        bytes.extend_from_slice(&0u16.to_ne_bytes());
        bytes.push(MACH_MSG_TYPE_COPY_SEND as u8);
        bytes.push(MACH_MSG_PORT_DESCRIPTOR);
    }
    bytes.extend_from_slice(&(body.len() as u32).to_ne_bytes());
    bytes.extend_from_slice(body);
    bytes.resize(size, 0);
    bytes.chunks_exact(4).map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// A decoded message: its body and the port names it carried (send rights the caller now owns).
#[derive(Debug, PartialEq, Eq)]
pub struct Decoded {
    pub body: Vec<u8>,
    pub ports: Vec<u32>,
}

/// A buffer a receive fills: the largest message plus the trailer, 8-aligned.
pub struct RecvBuf(Box<[u64]>);

impl RecvBuf {
    pub const LEN: usize = HEADER + BODY_COUNT + PORT_DESC * MAX_PORTS + LEN + MAX_BODY + TRAILER;

    #[must_use]
    pub fn new() -> Self {
        Self(vec![0u64; Self::LEN.div_ceil(8)].into_boxed_slice()) // alloc: one receive buffer per drain
    }

    pub fn as_mut_ptr(&mut self) -> *mut std::ffi::c_void {
        self.0.as_mut_ptr().cast()
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: a u64 slice viewed as its bytes.
        unsafe { std::slice::from_raw_parts(self.0.as_ptr().cast(), self.0.len() * 8) }
    }

    /// The message the kernel wrote, or why it is not one of ours (naming the field). On an error the ports it carried
    /// are still returned to the caller's ownership through `Err`'s words only: the caller should treat the sender as
    /// broken.
    pub fn decode(&self) -> Result<Decoded, String> {
        decode(self.bytes())
    }
}

impl Default for RecvBuf {
    fn default() -> Self {
        Self::new()
    }
}

fn word(b: &[u8], at: usize) -> Result<u32, String> {
    b.get(at..at + 4).map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]])).ok_or_else(|| format!("cut short at byte {at}"))
}

/// Read a message laid out as [`encode`] does (the bytes from its header on).
pub fn decode(b: &[u8]) -> Result<Decoded, String> {
    let bits = word(b, 0)?;
    let size = word(b, 4)? as usize;
    let id = word(b, 20)? as i32;
    if id != MSG_ID {
        return Err(format!("its id is 0x{id:x}, not harness's 0x{MSG_ID:x}"));
    }
    if bits & MACH_MSGH_BITS_COMPLEX == 0 {
        return Err("it is not a complex message".into());
    }
    let n = word(b, HEADER)? as usize;
    if n > MAX_PORTS {
        return Err(format!("it carries {n} descriptors, over {MAX_PORTS}"));
    }
    let mut ports = Vec::with_capacity(n); // alloc: at most MAX_PORTS names
    for i in 0..n {
        let at = HEADER + BODY_COUNT + PORT_DESC * i;
        let name = word(b, at)?;
        let kind = *b.get(at + 11).ok_or("cut short in a descriptor")?;
        let disposition = *b.get(at + 10).ok_or("cut short in a descriptor")?;
        if kind != MACH_MSG_PORT_DESCRIPTOR {
            return Err(format!("descriptor {i} is of type {kind}, not a port"));
        }
        if disposition != MACH_MSG_TYPE_PORT_SEND {
            return Err(format!("descriptor {i} carries disposition {disposition}, not a send right"));
        }
        ports.push(name);
    }
    let at = HEADER + BODY_COUNT + PORT_DESC * n;
    let len = word(b, at)? as usize;
    if len > MAX_BODY || at + LEN + len > size {
        return Err(format!("its body says {len} bytes, past the message's {size}"));
    }
    let body = b.get(at + LEN..at + LEN + len).ok_or("cut short in the body")?.to_vec(); // alloc: the message's bytes, handed to the caller
    Ok(Decoded { body, ports })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the kernel does to a sent message's descriptors on receive: COPY_SEND arrives as PORT_SEND, names renamed.
    fn as_received(sent: &[u32], names: &[u32]) -> Vec<u8> {
        let mut b: Vec<u8> = sent.iter().flat_map(|w| w.to_ne_bytes()).collect();
        for (i, &n) in names.iter().enumerate() {
            let at = HEADER + BODY_COUNT + PORT_DESC * i;
            b[at..at + 4].copy_from_slice(&n.to_ne_bytes());
            b[at + 10] = MACH_MSG_TYPE_PORT_SEND;
        }
        b
    }

    #[test]
    fn round_trips() {
        for (ports, body) in [(vec![], vec![]), (vec![5u32], b"x".to_vec()), (vec![7, 9, 11], vec![1u8; 1001])] {
            let sent = encode(3, &ports, &body);
            assert_eq!(sent.len() * 4, word(&as_received(&sent, &[]), 4).unwrap() as usize);
            let renamed: Vec<u32> = ports.iter().map(|p| p + 100).collect();
            let d = decode(&as_received(&sent, &renamed)).unwrap();
            assert_eq!(d, Decoded { body, ports: renamed });
        }
    }

    #[test]
    fn refuses_what_is_not_ours() {
        let mut b = as_received(&encode(3, &[], b"hi"), &[]);
        b[20] ^= 1;
        assert!(decode(&b).unwrap_err().contains("id"));
        let mut b = as_received(&encode(3, &[], b"hi"), &[]);
        b[HEADER + BODY_COUNT] = 200;
        assert!(decode(&b).unwrap_err().contains("past the message"));
    }
}
