use blaze_ids::{ContentId, Hasher};

/// A slice's content hash: BLAKE3 `ContentId` with a domain (`"blaze v2 <what> <n>"`).
///
/// A row is length-prefixed, so rows hash apart however their bytes split; `u32` and `u64` are
/// fixed-width little-endian. BLAKE3 hashes at about 1 GB/s per core on rows this small, so a
/// slice costs its bytes once.
pub struct SliceHasher(Hasher);

impl SliceHasher {
    pub fn new(domain: &'static str) -> Self {
        SliceHasher(Hasher::new(domain))
    }

    pub fn row(&mut self, bytes: &[u8]) {
        self.0.part(bytes);
    }

    pub fn u32(&mut self, v: u32) {
        self.0.update(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.0.update(&v.to_le_bytes());
    }

    pub fn id(&mut self, id: &ContentId) {
        self.0.id(id);
    }

    pub fn finish(self) -> ContentId {
        self.0.finish()
    }
}
