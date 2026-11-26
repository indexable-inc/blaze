//! Pluggable dense embedding vector store with strict dimension validation and cosine similarity.
//!
//! Dense retrieval is explicitly opted into by passing a query vector in [`crate::QueryOpts`].
//! Dimension mismatches and queries against an empty store are typed errors, never silent fallbacks.

use core::fmt;

use blaze_data::{Int, Map, Value};
use blaze_own::{ABTreeMap, AString, AVec, Alloc};

use crate::math::sqrt_f64;

/// Errors returned by [`EmbeddingStore`] operations and cosine scoring.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EmbedError {
    /// A vector's length did not match the store's fixed embedding dimension.
    DimensionMismatch { expected: usize, found: usize },
    /// Zero-length vectors cannot be stored or compared.
    EmptyVector,
    /// A vector contained NaN or infinity.
    NonFiniteValue { index: usize },
    /// Malformed serialized embedding data in cache.
    MalformedCache,
}

impl fmt::Display for EmbedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            EmbedError::DimensionMismatch { expected, found } => {
                write!(f, "embedding dimension mismatch: expected {expected}, found {found}")
            }
            EmbedError::EmptyVector => f.write_str("embedding vector cannot be empty"),
            EmbedError::NonFiniteValue { index } => {
                write!(f, "embedding vector contains non-finite float at index {index}")
            }
            EmbedError::MalformedCache => f.write_str("malformed embedding store in cached index"),
        }
    }
}

impl core::error::Error for EmbedError {}

/// Computes exact cosine similarity in `[-1.0, 1.0]` between two equal-length `f32` vectors.
///
/// Returns `0.0` if either vector has zero L2 norm. Refuses empty vectors, mismatched dimensions,
/// and non-finite components with [`EmbedError`].
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> Result<f64, EmbedError> {
    if a.is_empty() {
        return Err(EmbedError::EmptyVector);
    }
    if a.len() != b.len() {
        return Err(EmbedError::DimensionMismatch { expected: a.len(), found: b.len() });
    }
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (i, (&x, &y)) in a.iter().zip(b.iter()).enumerate() {
        if !x.is_finite() || !y.is_finite() {
            return Err(EmbedError::NonFiniteValue { index: i });
        }
        let xf = f64::from(x);
        let yf = f64::from(y);
        dot += xf * yf;
        norm_a += xf * xf;
        norm_b += yf * yf;
    }
    let denom = sqrt_f64(norm_a) * sqrt_f64(norm_b);
    if denom == 0.0 {
        return Ok(0.0);
    }
    Ok((dot / denom).clamp(-1.0, 1.0))
}

/// Optional fixed-dimension dense vector store keyed by canonical node ID (`crate:...`, `doc:...`, `note:...`).
pub struct EmbeddingStore<'a> {
    dim: usize,
    vectors: ABTreeMap<'a, AString<'a>, AVec<'a, f32>>,
    alloc: Alloc<'a>,
}

impl<'a> EmbeddingStore<'a> {
    /// Creates an empty store; the first [`insert`](Self::insert) fixes its dimension.
    pub fn new_in(alloc: Alloc<'a>) -> Self {
        Self { dim: 0, vectors: ABTreeMap::new_in(alloc), alloc }
    }

    /// Creates an empty store pre-configured with an expected vector dimension.
    pub fn with_dim_in(dim: usize, alloc: Alloc<'a>) -> Self {
        Self { dim, vectors: ABTreeMap::new_in(alloc), alloc }
    }

    /// Fixed vector dimension (`0` when empty and unconfigured).
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Number of stored node embeddings.
    pub fn len(&self) -> usize {
        self.vectors.len()
    }

    /// Whether the store holds no vectors.
    pub fn is_empty(&self) -> bool {
        self.vectors.is_empty()
    }

    /// Inserts or replaces the embedding vector for `node_id`.
    pub fn insert(&mut self, node_id: &str, vec: &[f32]) -> Result<(), EmbedError> {
        if vec.is_empty() {
            return Err(EmbedError::EmptyVector);
        }
        if self.dim == 0 {
            self.dim = vec.len();
        } else if vec.len() != self.dim {
            return Err(EmbedError::DimensionMismatch { expected: self.dim, found: vec.len() });
        }
        for (i, &v) in vec.iter().enumerate() {
            if !v.is_finite() {
                return Err(EmbedError::NonFiniteValue { index: i });
            }
        }
        let mut owned = AVec::with_capacity_in(vec.len(), self.alloc);
        owned.extend_from_slice(vec);
        let key = AString::from_str_in(node_id, self.alloc);
        self.vectors.insert(key, owned);
        Ok(())
    }

    /// Looks up the embedding vector for `node_id`.
    pub fn get(&self, node_id: &str) -> Option<&[f32]> {
        self.vectors.get(node_id).map(AVec::as_slice)
    }

    /// Validates a dense query vector against this store's dimension.
    pub fn validate_query(&self, query: &[f32]) -> Result<(), EmbedError> {
        if query.is_empty() {
            return Err(EmbedError::EmptyVector);
        }
        if self.dim != 0 && query.len() != self.dim {
            return Err(EmbedError::DimensionMismatch { expected: self.dim, found: query.len() });
        }
        for (i, &v) in query.iter().enumerate() {
            if !v.is_finite() {
                return Err(EmbedError::NonFiniteValue { index: i });
            }
        }
        Ok(())
    }

    /// Computes cosine similarity between `node_id`'s stored vector and `query`.
    /// Returns `Ok(None)` if `node_id` has no stored vector (after validating `query`).
    pub fn cosine(&self, node_id: &str, query: &[f32]) -> Result<Option<f64>, EmbedError> {
        self.validate_query(query)?;
        match self.get(node_id) {
            Some(stored) => cosine_similarity(stored, query).map(Some),
            None => Ok(None),
        }
    }

    /// Serializes this store to a [`blaze_data::Value`] (little-endian `f32` byte blobs for compactness).
    pub fn to_value(&self, alloc: Alloc<'a>) -> Value<Alloc<'a>> {
        let mut root = Map::with_capacity_in(2, alloc);
        root.insert(
            AString::from_str_in("dim", alloc),
            Value::Int(Int::from_u64(self.dim as u64)),
        );
        let mut vecs = Map::with_capacity_in(self.vectors.len(), alloc);
        for (k, v) in self.vectors.iter() {
            let mut bytes = AVec::with_capacity_in(v.len() * 4, alloc);
            for &f in v.as_slice() {
                bytes.extend_from_slice(&f.to_le_bytes());
            }
            vecs.insert(AString::from_str_in(k.as_str(), alloc), Value::Bytes(bytes));
        }
        root.insert(AString::from_str_in("vectors", alloc), Value::Map(vecs));
        Value::Map(root)
    }

    /// Deserializes an [`EmbeddingStore`] from a [`blaze_data::Value`].
    pub fn from_value(value: &Value<Alloc<'a>>, alloc: Alloc<'a>) -> Result<Self, EmbedError> {
        let Value::Map(map) = value else {
            return Err(EmbedError::MalformedCache);
        };
        let dim = match map.get("dim") {
            Some(Value::Int(n)) => usize::try_from(n.as_u64().ok_or(EmbedError::MalformedCache)?)
                .map_err(|_| EmbedError::MalformedCache)?,
            _ => return Err(EmbedError::MalformedCache),
        };
        let Some(Value::Map(vecs_map)) = map.get("vectors") else {
            return Err(EmbedError::MalformedCache);
        };
        let mut store = Self::with_dim_in(dim, alloc);
        for (k, v) in vecs_map.iter() {
            let Value::Bytes(raw) = v else {
                return Err(EmbedError::MalformedCache);
            };
            if raw.len() % 4 != 0 {
                return Err(EmbedError::MalformedCache);
            }
            let count = raw.len() / 4;
            let mut floats = AVec::with_capacity_in(count, alloc);
            for chunk in raw.as_slice().chunks_exact(4) {
                floats.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            }
            store.insert(k, floats.as_slice())?;
        }
        Ok(store)
    }

    /// Clones this store into another allocator.
    pub fn clone_in<'b>(&self, alloc: Alloc<'b>) -> EmbeddingStore<'b> {
        let mut out = EmbeddingStore::with_dim_in(self.dim, alloc);
        for (k, v) in self.vectors.iter() {
            let mut owned = AVec::with_capacity_in(v.len(), alloc);
            owned.extend_from_slice(v.as_slice());
            out.vectors.insert(AString::from_str_in(k.as_str(), alloc), owned);
        }
        out
    }
}
