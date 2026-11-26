//! Hybrid retrieval engine: identifier-aware BM25 + character 3-grams + pluggable dense embeddings
//! + 1-hop graph expansion + token-budgeted result packing + `outline` and `area` queries.

use core::fmt;

use blaze_own::{ABTreeMap, ABTreeSet, AString, AVec, Alloc, format_in};

use crate::embed::EmbedError;
use crate::format::{OutputFormat, render_value, search_result_to_value};
use crate::graph::{EdgeKind, expand_1hop};
use crate::math::{ln_f64, round3};
use crate::tree::{AgentNoteNode, CrateNode, CrateStatus, CtxIndex, DocNode, DocStatus, NoteKind, format_slice_ptr};

/// Default token budget when none is specified by the caller.
pub const DEFAULT_BUDGET_TOKENS: usize = 2000;
/// Default maximum number of ranked hits considered before budget packing.
pub const DEFAULT_MAX_HITS: usize = 24;

/// BM25 tuning constants (standard Robertson/Okapi values).
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

/// Fusion weights combining lexical BM25, character 3-gram overlap, and dense cosine similarity.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct HybridWeights {
    pub bm25: f64,
    pub trigram: f64,
    pub dense: f64,
}

impl HybridWeights {
    /// Default lexical-only weights (when `QueryOpts::dense` is `None`).
    pub const LEXICAL: HybridWeights = HybridWeights { bm25: 0.70, trigram: 0.30, dense: 0.0 };
    /// Default hybrid lexical + dense weights (when `QueryOpts::dense` is `Some`).
    pub const HYBRID: HybridWeights = HybridWeights { bm25: 0.45, trigram: 0.20, dense: 0.35 };
}

/// Options controlling a [`search`] query.
pub struct QueryOpts<'q> {
    pub query: &'q str,
    pub budget_tokens: usize,
    pub max_hits: usize,
    pub expand_hops: bool,
    /// Optional dense embedding vector for hybrid retrieval.
    /// When `Some`, the index MUST hold matching-dimension embeddings or [`SearchError`] is returned.
    pub dense: Option<&'q [f32]>,
    pub weights: Option<HybridWeights>,
}

impl<'q> QueryOpts<'q> {
    /// Creates standard lexical + 1-hop query options with the given token budget.
    pub fn new(query: &'q str, budget_tokens: usize) -> Self {
        Self {
            query,
            budget_tokens,
            max_hits: DEFAULT_MAX_HITS,
            expand_hops: true,
            dense: None,
            weights: None,
        }
    }
}

/// Typed search errors (Rule 3: no silent fallback on dense query mismatch).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SearchError {
    EmptyQuery,
    NoEmbeddingsLoaded,
    Embed(EmbedError),
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            SearchError::EmptyQuery => f.write_str("search query contains no indexable tokens"),
            SearchError::NoEmbeddingsLoaded => {
                f.write_str("dense query vector provided, but index has no embeddings loaded")
            }
            SearchError::Embed(e) => write!(f, "{e}"),
        }
    }
}

impl core::error::Error for SearchError {}

/// Concept kind of a ranked search hit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    Crate,
    Doc,
    Howto,
    Annoyance,
}

impl NodeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            NodeKind::Crate => "crate",
            NodeKind::Doc => "doc",
            NodeKind::Howto => "howto",
            NodeKind::Annoyance => "annoyance",
        }
    }
}

/// One ranked concept hit returned in a [`SearchResult`].
pub struct SearchHit<'a> {
    pub rank: u32,
    pub kind: NodeKind,
    pub id: AString<'a>,
    pub status: AString<'a>,
    pub domain: AString<'a>,
    pub score: f64,
    pub match_source: AString<'a>,
    pub ptr: AString<'a>,
    pub summary: AString<'a>,
    pub via: Option<AString<'a>>,
}

/// A pinpointed Markdown heading section matching the query (`path#Lstart-Lend`).
pub struct SectionHit<'a> {
    pub doc_path: AString<'a>,
    pub doc_status: AString<'a>,
    pub heading: AString<'a>,
    pub ptr: AString<'a>,
    pub score: f64,
}

/// Token-budgeted search output ready for `blaze-tab` or JSON rendering.
pub struct SearchResult<'a> {
    pub query: AString<'a>,
    pub budget_tokens: usize,
    pub used_tokens: usize,
    pub total_candidates: usize,
    pub omitted: usize,
    pub hits: AVec<'a, SearchHit<'a>>,
    pub sections: AVec<'a, SectionHit<'a>>,
}

/// Heading outline entry for `blaze-ctx outline`.
pub struct OutlineEntry<'a> {
    pub level: u8,
    pub heading: AString<'a>,
    pub ptr: AString<'a>,
    pub lines: u32,
}

/// Full document outline report for `blaze-ctx outline`.
pub struct OutlineReport<'a> {
    pub path: AString<'a>,
    pub status: AString<'a>,
    pub date: Option<AString<'a>>,
    pub title: AString<'a>,
    pub summary: AString<'a>,
    pub line_count: u32,
    pub headings: AVec<'a, OutlineEntry<'a>>,
}

/// Export item row in an [`AreaReport`].
pub struct AreaExport<'a> {
    pub kind: AString<'a>,
    pub name: AString<'a>,
    pub ptr: AString<'a>,
}

/// Linked doc row in an [`AreaReport`].
pub struct AreaDoc<'a> {
    pub status: AString<'a>,
    pub path: AString<'a>,
    pub title: AString<'a>,
    pub ptr: AString<'a>,
    pub relation: AString<'a>,
}

/// Linked agent note row in an [`AreaReport`].
pub struct AreaNote<'a> {
    pub kind: AString<'a>,
    pub note_id: AString<'a>,
    pub state: AString<'a>,
    pub ptr: AString<'a>,
    pub summary: AString<'a>,
}

/// Focused subsystem report for `blaze-ctx area <crate-or-topic>`.
pub struct AreaReport<'a> {
    pub target: AString<'a>,
    pub found_crate: bool,
    pub crate_name: Option<AString<'a>>,
    pub status: Option<AString<'a>>,
    pub domain: AString<'a>,
    pub ptr: Option<AString<'a>>,
    pub summary: Option<AString<'a>>,
    pub deps: AVec<'a, AString<'a>>,
    pub dependents: AVec<'a, AString<'a>>,
    pub exports: AVec<'a, AreaExport<'a>>,
    pub design_docs: AVec<'a, AreaDoc<'a>>,
    pub notes: AVec<'a, AreaNote<'a>>,
}

// ---------------------------------------------------------------------------
// Identifier-Aware Tokenization & Character 3-Grams
// ---------------------------------------------------------------------------

/// Splits `text` into lowercase identifier-aware tokens.
///
/// Handles `snake_case`, `CamelCase`, `HTTPServer` (`http`, `server`), `kebab-case`,
/// Rust `::` paths, and POSIX `/` paths.
pub fn tokenize<'a>(text: &str, alloc: Alloc<'a>) -> AVec<'a, AString<'a>> {
    let bytes = text.as_bytes();
    let mut out: AVec<'a, AString<'a>> = AVec::new_in(alloc);
    let mut cur = AString::new_in(alloc);

    let flush = |cur: &mut AString<'a>, out: &mut AVec<'a, AString<'a>>| {
        if cur.len() >= 2 {
            out.push(AString::from_str_in(cur.as_str(), alloc));
        }
        cur.clear();
    };

    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphanumeric() {
            // Split CamelCase boundaries:
            // 1. lower/digit -> UPPER (`fooBar` -> `foo`, `Bar`)
            // 2. acronym -> Word (`HTTPServer` -> `HTTP`, `Server`)
            if !cur.is_empty() && b.is_ascii_uppercase() {
                let prev = bytes[i - 1];
                let next_is_lower = bytes.get(i + 1).is_some_and(u8::is_ascii_lowercase);
                if prev.is_ascii_lowercase() || prev.is_ascii_digit() || (prev.is_ascii_uppercase() && next_is_lower) {
                    flush(&mut cur, &mut out);
                }
            }
            cur.push(b.to_ascii_lowercase() as char);
        } else {
            flush(&mut cur, &mut out);
        }
        i += 1;
    }
    flush(&mut cur, &mut out);
    out
}

/// Extracts unique lowercase ASCII character 3-grams from `tokens`.
pub fn extract_trigrams<'a>(tokens: &[AString<'_>], alloc: Alloc<'a>) -> ABTreeSet<'a, [u8; 3]> {
    let mut tris: ABTreeSet<'a, [u8; 3]> = ABTreeSet::new_in(alloc);
    for tok in tokens {
        let b = tok.as_bytes();
        if b.len() >= 3 {
            for win in b.windows(3) {
                tris.insert([win[0], win[1], win[2]]);
            }
        }
    }
    tris
}

/// Computes query-focused character 3-gram overlap in `[0.0, 1.0]`.
fn trigram_similarity(query_tris: &ABTreeSet<'_, [u8; 3]>, doc_tris: &ABTreeSet<'_, [u8; 3]>) -> f64 {
    if query_tris.is_empty() || doc_tris.is_empty() {
        return 0.0;
    }
    let mut shared = 0usize;
    for tri in query_tris.iter() {
        if doc_tris.contains(tri) {
            shared += 1;
        }
    }
    if shared == 0 {
        return 0.0;
    }
    let coverage = (shared as f64) / (query_tris.len() as f64);
    let dice = (2.0 * shared as f64) / ((query_tris.len() + doc_tris.len()) as f64);
    0.75 * coverage + 0.25 * dice
}

/// Deterministic LLM token count estimator (`ceil(bytes / 4)`).
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

fn is_stopword(tok: &str) -> bool {
    matches!(
        tok,
        "the" | "and" | "for" | "with" | "that" | "this" | "from" | "are" | "was" | "not" | "into" | "have" | "has"
    )
}

// ---------------------------------------------------------------------------
// Internal Search Document Representation
// ---------------------------------------------------------------------------

enum NodeRef<'idx> {
    Crate(&'idx CrateNode<'idx>),
    Doc(&'idx DocNode<'idx>),
    Note(&'idx AgentNoteNode<'idx>),
}

impl<'idx> NodeRef<'idx> {
    fn id(&self) -> &'idx str {
        match self {
            NodeRef::Crate(c) => c.id.as_str(),
            NodeRef::Doc(d) => d.id.as_str(),
            NodeRef::Note(n) => n.id.as_str(),
        }
    }

    fn kind(&self) -> NodeKind {
        match self {
            NodeRef::Crate(_) => NodeKind::Crate,
            NodeRef::Doc(_) => NodeKind::Doc,
            NodeRef::Note(n) => match n.kind {
                NoteKind::Howto => NodeKind::Howto,
                NoteKind::Annoyance => NodeKind::Annoyance,
            },
        }
    }

    fn status(&self) -> &'idx str {
        match self {
            NodeRef::Crate(c) => c.status.as_str(),
            NodeRef::Doc(d) => d.status.as_str(),
            NodeRef::Note(n) => n.state.as_deref().unwrap_or(n.kind.as_str()),
        }
    }

    fn domain(&self) -> &'idx str {
        match self {
            NodeRef::Crate(c) => c.domain.as_str(),
            NodeRef::Doc(d) => d.domain.as_str(),
            NodeRef::Note(n) => n.domain.as_str(),
        }
    }

    fn summary(&self) -> &'idx str {
        match self {
            NodeRef::Crate(c) => c.summary.as_str(),
            NodeRef::Doc(d) => d.summary.as_str(),
            NodeRef::Note(n) => n.summary.as_str(),
        }
    }

    fn default_ptr<'a>(&self, alloc: Alloc<'a>) -> AString<'a> {
        match self {
            NodeRef::Crate(c) => format_slice_ptr(c.entry_file.as_str(), 1, c.entry_lines, alloc),
            NodeRef::Doc(d) => format_slice_ptr(d.path.as_str(), 1, d.line_count, alloc),
            NodeRef::Note(n) => format_slice_ptr(n.path.as_str(), 1, n.line_count, alloc),
        }
    }

    fn freshness_multiplier(&self) -> f64 {
        match self {
            NodeRef::Crate(c) => match c.status {
                CrateStatus::Live => 1.05,
                CrateStatus::Experimental => 0.90,
                CrateStatus::Parked => 0.65,
            },
            NodeRef::Doc(d) => match d.status {
                DocStatus::Current => 1.06,
                DocStatus::Plan => 1.04,
                DocStatus::Unmarked => 0.95,
                DocStatus::Record => 0.85,
                DocStatus::Superseded => 0.55,
            },
            NodeRef::Note(n) => match n.state.as_deref() {
                Some("open" | "current") => 1.02,
                _ => 0.96,
            },
        }
    }
}

struct PreparedDoc<'a, 'idx> {
    node: NodeRef<'idx>,
    /// Weighted term frequencies: title/name/exports (3.0), summary/area/tags (2.0), body/detail (1.0).
    tf: ABTreeMap<'a, AString<'a>, f64>,
    weighted_len: f64,
    /// Trigrams of high-signal fields (title, name, headings, summary, tags).
    trigrams: ABTreeSet<'a, [u8; 3]>,
}

fn add_weighted_tokens<'a>(
    text: &str,
    weight: f64,
    tf: &mut ABTreeMap<'a, AString<'a>, f64>,
    len_acc: &mut f64,
    tris_target: Option<&mut ABTreeSet<'a, [u8; 3]>>,
    alloc: Alloc<'a>,
) {
    let toks = tokenize(text, alloc);
    if let Some(tris) = tris_target {
        for tok in toks.as_slice() {
            let b = tok.as_bytes();
            if b.len() >= 3 {
                for win in b.windows(3) {
                    tris.insert([win[0], win[1], win[2]]);
                }
            }
        }
    }
    for tok in toks {
        *len_acc += weight;
        if let Some(v) = tf.get_mut(tok.as_str()) {
            *v += weight;
        } else {
            tf.insert(tok, weight);
        }
    }
}

fn prepare_corpus<'a, 'idx>(index: &'idx CtxIndex<'idx>, alloc: Alloc<'a>) -> AVec<'a, PreparedDoc<'a, 'idx>> {
    let total = index.crates.len() + index.docs.len() + index.notes.len();
    let mut corpus = AVec::with_capacity_in(total, alloc);

    for c in index.crates.as_slice() {
        let mut tf = ABTreeMap::new_in(alloc);
        let mut wlen = 0.0f64;
        let mut tris = ABTreeSet::new_in(alloc);
        add_weighted_tokens(c.name.as_str(), 3.0, &mut tf, &mut wlen, Some(&mut tris), alloc);
        add_weighted_tokens(c.path.as_str(), 2.5, &mut tf, &mut wlen, Some(&mut tris), alloc);
        add_weighted_tokens(c.summary.as_str(), 2.0, &mut tf, &mut wlen, Some(&mut tris), alloc);
        add_weighted_tokens(c.domain.as_str(), 1.5, &mut tf, &mut wlen, Some(&mut tris), alloc);
        for ex in c.exports.as_slice() {
            add_weighted_tokens(ex.name.as_str(), 2.2, &mut tf, &mut wlen, Some(&mut tris), alloc);
        }
        for dep in c.deps.as_slice() {
            add_weighted_tokens(dep.as_str(), 0.8, &mut tf, &mut wlen, None, alloc);
        }
        for doc in c.design_docs.as_slice() {
            add_weighted_tokens(doc.as_str(), 1.2, &mut tf, &mut wlen, None, alloc);
        }
        corpus.push(PreparedDoc { node: NodeRef::Crate(c), tf, weighted_len: wlen, trigrams: tris });
    }

    for d in index.docs.as_slice() {
        let mut tf = ABTreeMap::new_in(alloc);
        let mut wlen = 0.0f64;
        let mut tris = ABTreeSet::new_in(alloc);
        add_weighted_tokens(d.path.as_str(), 3.0, &mut tf, &mut wlen, Some(&mut tris), alloc);
        add_weighted_tokens(d.title.as_str(), 3.0, &mut tf, &mut wlen, Some(&mut tris), alloc);
        add_weighted_tokens(d.summary.as_str(), 2.2, &mut tf, &mut wlen, Some(&mut tris), alloc);
        add_weighted_tokens(d.domain.as_str(), 1.2, &mut tf, &mut wlen, None, alloc);
        for h in d.headings.as_slice() {
            add_weighted_tokens(h.title.as_str(), 2.2, &mut tf, &mut wlen, Some(&mut tris), alloc);
            add_weighted_tokens(h.excerpt.as_str(), 1.1, &mut tf, &mut wlen, None, alloc);
        }
        for c in d.mentioned_crates.as_slice() {
            add_weighted_tokens(c.as_str(), 1.2, &mut tf, &mut wlen, None, alloc);
        }
        add_weighted_tokens(d.body_terms.as_str(), 1.0, &mut tf, &mut wlen, None, alloc);
        corpus.push(PreparedDoc { node: NodeRef::Doc(d), tf, weighted_len: wlen, trigrams: tris });
    }

    for n in index.notes.as_slice() {
        let mut tf = ABTreeMap::new_in(alloc);
        let mut wlen = 0.0f64;
        let mut tris = ABTreeSet::new_in(alloc);
        add_weighted_tokens(n.note_id.as_str(), 2.5, &mut tf, &mut wlen, Some(&mut tris), alloc);
        add_weighted_tokens(n.area.as_str(), 2.4, &mut tf, &mut wlen, Some(&mut tris), alloc);
        add_weighted_tokens(n.summary.as_str(), 2.2, &mut tf, &mut wlen, Some(&mut tris), alloc);
        for t in n.tags.as_slice() {
            add_weighted_tokens(t.as_str(), 2.0, &mut tf, &mut wlen, Some(&mut tris), alloc);
        }
        add_weighted_tokens(n.detail.as_str(), 1.0, &mut tf, &mut wlen, None, alloc);
        corpus.push(PreparedDoc { node: NodeRef::Note(n), tf, weighted_len: wlen, trigrams: tris });
    }

    corpus
}

// ---------------------------------------------------------------------------
// Hybrid Search + 1-Hop Graph Expansion + Budget Packing
// ---------------------------------------------------------------------------

/// Runs hybrid retrieval over `index`, expanding 1 hop through the concept graph and packing the
/// highest-signal slice within `opts.budget_tokens`.
pub fn search<'a>(
    index: &CtxIndex<'_>,
    opts: &QueryOpts<'_>,
    alloc: Alloc<'a>,
) -> Result<SearchResult<'a>, SearchError> {
    let raw_tokens = tokenize(opts.query, alloc);
    let mut query_tokens: AVec<'a, AString<'a>> = AVec::new_in(alloc);
    for t in raw_tokens.as_slice() {
        if !is_stopword(t.as_str()) {
            query_tokens.push(AString::from_str_in(t.as_str(), alloc));
        }
    }
    if query_tokens.is_empty() {
        for t in raw_tokens {
            query_tokens.push(t);
        }
    }
    if query_tokens.is_empty() && opts.dense.is_none() {
        return Err(SearchError::EmptyQuery);
    }

    // Strict dense validation (Rule 3: never silently ignore a dense query vector).
    if let Some(qvec) = opts.dense {
        if index.embeddings.is_empty() {
            return Err(SearchError::NoEmbeddingsLoaded);
        }
        index.embeddings.validate_query(qvec).map_err(SearchError::Embed)?;
    }

    let weights = opts.weights.unwrap_or(match opts.dense {
        Some(_) => HybridWeights::HYBRID,
        None => HybridWeights::LEXICAL,
    });

    let query_tris = extract_trigrams(query_tokens.as_slice(), alloc);
    let mut unique_qterms: ABTreeSet<'a, AString<'a>> = ABTreeSet::new_in(alloc);
    for t in query_tokens.as_slice() {
        unique_qterms.insert(AString::from_str_in(t.as_str(), alloc));
    }

    let corpus = prepare_corpus(index, alloc);
    let n_docs = corpus.len();
    if n_docs == 0 {
        return Ok(SearchResult {
            query: AString::from_str_in(opts.query, alloc),
            budget_tokens: opts.budget_tokens,
            used_tokens: 0,
            total_candidates: 0,
            omitted: 0,
            hits: AVec::new_in(alloc),
            sections: AVec::new_in(alloc),
        });
    }

    // Compute corpus average weighted length & document frequencies for query terms.
    let mut total_len = 0.0f64;
    let mut df: ABTreeMap<'a, AString<'a>, usize> = ABTreeMap::new_in(alloc);
    for doc in corpus.as_slice() {
        total_len += doc.weighted_len;
        for qterm in unique_qterms.iter() {
            if doc.tf.contains_key(qterm.as_str()) {
                if let Some(c) = df.get_mut(qterm.as_str()) {
                    *c += 1;
                } else {
                    df.insert(AString::from_str_in(qterm.as_str(), alloc), 1);
                }
            }
        }
    }
    let avgdl = (total_len / (n_docs as f64)).max(1.0);

    // Precompute IDF per query term.
    let mut idf: ABTreeMap<'a, AString<'a>, f64> = ABTreeMap::new_in(alloc);
    for qterm in unique_qterms.iter() {
        let freq = df.get(qterm.as_str()).copied().unwrap_or(0) as f64;
        let num = (n_docs as f64) - freq + 0.5;
        let den = freq + 0.5;
        let val = ln_f64((num / den) + 1.0).max(0.0);
        idf.insert(AString::from_str_in(qterm.as_str(), alloc), val);
    }

    // Score every node in the corpus.
    struct ScoredCand<'b> {
        corpus_idx: usize,
        score: f64,
        match_source: &'static str,
        best_ptr: AString<'b>,
        via: Option<AString<'b>>,
    }

    let mut id_to_idx: ABTreeMap<'a, AString<'a>, usize> = ABTreeMap::new_in(alloc);
    let mut scored: AVec<'a, ScoredCand<'a>> = AVec::new_in(alloc);
    let mut section_hits: AVec<'a, SectionHit<'a>> = AVec::new_in(alloc);

    for (idx, doc) in corpus.as_slice().iter().enumerate() {
        let node_id = doc.node.id();
        id_to_idx.insert(AString::from_str_in(node_id, alloc), idx);

        // 1. BM25 score.
        let mut bm25_raw = 0.0f64;
        let mut matched_terms = 0usize;
        for qterm in unique_qterms.iter() {
            if let Some(&freq) = doc.tf.get(qterm.as_str()) {
                matched_terms += 1;
                let term_idf = idf.get(qterm.as_str()).copied().unwrap_or(0.0);
                let norm = BM25_K1 * (1.0 - BM25_B + BM25_B * (doc.weighted_len / avgdl));
                bm25_raw += term_idf * (freq * (BM25_K1 + 1.0)) / (freq + norm);
            }
        }
        // Normalize BM25 into [0, 1) smoothly while preserving rank differences.
        let bm25_norm = if bm25_raw > 0.0 {
            bm25_raw / (bm25_raw + 3.5)
        } else {
            0.0
        };
        // Bonus when multiple distinct query terms co-occur in the same node.
        let term_cov = if !unique_qterms.is_empty() {
            (matched_terms as f64) / (unique_qterms.len() as f64)
        } else {
            0.0
        };

        // 2. Character 3-gram overlap.
        let tri_sim = trigram_similarity(&query_tris, &doc.trigrams);

        // 3. Optional dense cosine similarity.
        let dense_sim = match opts.dense {
            Some(qvec) => index
                .embeddings
                .cosine(node_id, qvec)
                .map_err(SearchError::Embed)?
                .unwrap_or(0.0)
                .max(0.0),
            None => 0.0,
        };

        // Exact-name boost if query directly names the crate/doc stem (scaled gently so multi-term
        // matches are not drowned out by a single-word filename match).
        let exact_boost = if query_mentions_node_exact(opts.query, &doc.node) { 0.16 } else { 0.0 };

        let combined = (weights.bm25 * (0.60 * bm25_norm + 0.40 * term_cov)
            + weights.trigram * tri_sim
            + weights.dense * dense_sim
            + exact_boost)
            * doc.node.freshness_multiplier();

        // Filter noise (require meaningful lexical or dense match).
        let threshold = if opts.dense.is_some() { 0.08 } else { 0.12 };
        if combined < threshold {
            continue;
        }

        let match_source = match (bm25_raw > 0.0, tri_sim >= 0.25, dense_sim > 0.2) {
            (true, _, true) => "hybrid",
            (false, _, true) => "dense",
            (true, true, false) => "bm25+tri",
            (true, false, false) => "bm25",
            (false, true, false) => "trigram",
            (false, false, false) => "exact",
        };

        // If this is a DocNode, pinpoint its best matching heading slice(s).
        let mut best_ptr = doc.node.default_ptr(alloc);
        if let NodeRef::Doc(d) = doc.node {
            let mut best_h_score = 0.0f64;
            for h in d.headings.as_slice() {
                let hs = score_heading(h.title.as_str(), h.excerpt.as_str(), &unique_qterms, &query_tris, alloc);
                if hs > 0.18 {
                    let ptr = format_slice_ptr(d.path.as_str(), h.start_line, h.end_line, alloc);
                    if hs > best_h_score {
                        best_h_score = hs;
                        best_ptr = AString::from_str_in(ptr.as_str(), alloc);
                    }
                    section_hits.push(SectionHit {
                        doc_path: AString::from_str_in(d.path.as_str(), alloc),
                        doc_status: AString::from_str_in(d.status.as_str(), alloc),
                        heading: AString::from_str_in(h.title.as_str(), alloc),
                        ptr,
                        score: round3(hs * combined),
                    });
                }
            }
        }

        scored.push(ScoredCand {
            corpus_idx: idx,
            score: combined,
            match_source,
            best_ptr,
            via: None,
        });
    }

    scored.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| corpus[a.corpus_idx].node.id().cmp(corpus[b.corpus_idx].node.id()))
    });

    // 4. 1-hop graph expansion from top seeds.
    if opts.expand_hops && !scored.is_empty() {
        let seed_limit = scored.len().min(6);
        let mut seeds: AVec<'a, (&str, f64)> = AVec::with_capacity_in(seed_limit, alloc);
        for c in scored.as_slice().iter().take(seed_limit) {
            seeds.push((corpus[c.corpus_idx].node.id(), c.score));
        }

        let neighbors = expand_1hop(index.edges.as_slice(), seeds.as_slice(), 10, alloc);

        // Track already-scored IDs so we annotate or append neighbors cleanly.
        let mut present: ABTreeSet<'a, AString<'a>> = ABTreeSet::new_in(alloc);
        for c in scored.as_slice() {
            present.insert(AString::from_str_in(corpus[c.corpus_idx].node.id(), alloc));
        }

        for nb in neighbors {
            if present.contains(nb.node_id.as_str()) {
                continue;
            }
            if let Some(&cidx) = id_to_idx.get(nb.node_id.as_str()) {
                let adj = nb.score * corpus[cidx].node.freshness_multiplier();
                let ptr = corpus[cidx].node.default_ptr(alloc);
                scored.push(ScoredCand {
                    corpus_idx: cidx,
                    score: adj,
                    match_source: "graph-1hop",
                    best_ptr: ptr,
                    via: Some(nb.via),
                });
                present.insert(nb.node_id);
            }
        }

        scored.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| corpus[a.corpus_idx].node.id().cmp(corpus[b.corpus_idx].node.id()))
        });
    }

    let total_candidates = scored.len();
    let cap = opts.max_hits.max(1);
    if scored.len() > cap {
        scored.truncate(cap);
    }

    section_hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.ptr.as_str().cmp(b.ptr.as_str()))
    });
    if section_hits.len() > 12 {
        section_hits.truncate(12);
    }

    // 5. Build SearchHit list and pack strictly inside `opts.budget_tokens`.
    let mut all_hits: AVec<'a, SearchHit<'a>> = AVec::with_capacity_in(scored.len(), alloc);
    for (rank_idx, cand) in scored.into_iter().enumerate() {
        let node = &corpus[cand.corpus_idx].node;
        all_hits.push(SearchHit {
            rank: (rank_idx + 1) as u32,
            kind: node.kind(),
            id: AString::from_str_in(node.id(), alloc),
            status: AString::from_str_in(node.status(), alloc),
            domain: AString::from_str_in(node.domain(), alloc),
            score: round3(cand.score),
            match_source: AString::from_str_in(cand.match_source, alloc),
            ptr: cand.best_ptr,
            summary: AString::from_str_in(node.summary(), alloc),
            via: cand.via,
        });
    }

    Ok(pack_to_budget(
        opts.query,
        opts.budget_tokens,
        total_candidates,
        all_hits,
        section_hits,
        alloc,
    ))
}

fn score_heading<'a>(
    title: &str,
    excerpt: &str,
    query_terms: &ABTreeSet<'_, AString<'_>>,
    query_tris: &ABTreeSet<'_, [u8; 3]>,
    alloc: Alloc<'a>,
) -> f64 {
    let title_toks = tokenize(title, alloc);
    let exc_toks = tokenize(excerpt, alloc);
    let mut title_hits = 0usize;
    let mut exc_hits = 0usize;
    for q in query_terms.iter() {
        if title_toks.as_slice().iter().any(|t| t.as_str() == q.as_str()) {
            title_hits += 1;
        }
        if exc_toks.as_slice().iter().any(|t| t.as_str() == q.as_str()) {
            exc_hits += 1;
        }
    }
    let denom = query_terms.len().max(1) as f64;
    let lex = 0.65 * (title_hits as f64 / denom) + 0.35 * (exc_hits as f64 / denom);
    let tris = extract_trigrams(title_toks.as_slice(), alloc);
    let tri = trigram_similarity(query_tris, &tris);
    0.70 * lex + 0.30 * tri
}

fn query_mentions_node_exact(query: &str, node: &NodeRef<'_>) -> bool {
    let needle = match node {
        NodeRef::Crate(c) => c.name.as_str(),
        NodeRef::Doc(d) => d
            .path
            .as_str()
            .rsplit('/')
            .next()
            .unwrap_or(d.path.as_str())
            .strip_suffix(".md")
            .unwrap_or(d.path.as_str()),
        NodeRef::Note(n) => n.note_id.as_str(),
    };
    if needle.len() < 3 {
        return false;
    }
    let qb = query.as_bytes();
    let nb = needle.as_bytes();
    if nb.len() > qb.len() {
        return false;
    }
    qb.windows(nb.len()).any(|w| {
        w.iter()
            .zip(nb.iter())
            .all(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase())
    })
}

/// Greedily packs hits and sections so the rendered `blaze-tab` output fits inside `budget_tokens`.
fn pack_to_budget<'a>(
    query: &str,
    budget_tokens: usize,
    total_candidates: usize,
    all_hits: AVec<'a, SearchHit<'a>>,
    all_sections: AVec<'a, SectionHit<'a>>,
    alloc: Alloc<'a>,
) -> SearchResult<'a> {
    let budget = budget_tokens.max(64);
    let mut kept_hits = all_hits;
    let mut kept_sections = all_sections;

    loop {
        let probe = SearchResult {
            query: AString::from_str_in(query, alloc),
            budget_tokens: budget,
            used_tokens: 0,
            total_candidates,
            omitted: total_candidates.saturating_sub(kept_hits.len()),
            hits: clone_hits(&kept_hits, alloc),
            sections: clone_sections(&kept_sections, alloc),
        };
        let val = search_result_to_value(&probe, alloc);
        let rendered = render_value(&val, OutputFormat::Tab, alloc).unwrap_or_else(|_| AString::new_in(alloc));
        let tokens = estimate_tokens(rendered.as_str());

        if tokens <= budget {
            return SearchResult {
                query: probe.query,
                budget_tokens: budget,
                used_tokens: tokens,
                total_candidates,
                omitted: total_candidates.saturating_sub(kept_hits.len()),
                hits: kept_hits,
                sections: kept_sections,
            };
        }

        // Trim sections first once we have more than 4, then alternate trimming sections and lowest-ranked hits.
        if kept_sections.len() > 4 {
            kept_sections.pop();
        } else if kept_hits.len() > 1 {
            kept_hits.pop();
        } else if !kept_sections.is_empty() {
            kept_sections.pop();
        } else {
            // Even a single hit exceeded a tiny budget: truncate its summary so we still stay within budget.
            if let Some(first) = kept_hits.first_mut()
                && first.summary.len() > 40
            {
                let cut = floor_char_boundary(first.summary.as_str(), first.summary.len() / 2);
                let shortened = format_in!(alloc, "{}...", &first.summary.as_str()[..cut]);
                first.summary = shortened;
            } else {
                return SearchResult {
                    query: probe.query,
                    budget_tokens: budget,
                    used_tokens: tokens,
                    total_candidates,
                    omitted: total_candidates.saturating_sub(kept_hits.len()),
                    hits: kept_hits,
                    sections: kept_sections,
                };
            }
        }
    }
}

fn floor_char_boundary(s: &str, max_len: usize) -> usize {
    let mut idx = max_len.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn clone_hits<'a>(src: &[SearchHit<'_>], alloc: Alloc<'a>) -> AVec<'a, SearchHit<'a>> {
    let mut out = AVec::with_capacity_in(src.len(), alloc);
    for h in src {
        out.push(SearchHit {
            rank: h.rank,
            kind: h.kind,
            id: AString::from_str_in(h.id.as_str(), alloc),
            status: AString::from_str_in(h.status.as_str(), alloc),
            domain: AString::from_str_in(h.domain.as_str(), alloc),
            score: h.score,
            match_source: AString::from_str_in(h.match_source.as_str(), alloc),
            ptr: AString::from_str_in(h.ptr.as_str(), alloc),
            summary: AString::from_str_in(h.summary.as_str(), alloc),
            via: h.via.as_ref().map(|v| AString::from_str_in(v.as_str(), alloc)),
        });
    }
    out
}

fn clone_sections<'a>(src: &[SectionHit<'_>], alloc: Alloc<'a>) -> AVec<'a, SectionHit<'a>> {
    let mut out = AVec::with_capacity_in(src.len(), alloc);
    for s in src {
        out.push(SectionHit {
            doc_path: AString::from_str_in(s.doc_path.as_str(), alloc),
            doc_status: AString::from_str_in(s.doc_status.as_str(), alloc),
            heading: AString::from_str_in(s.heading.as_str(), alloc),
            ptr: AString::from_str_in(s.ptr.as_str(), alloc),
            score: s.score,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Outline & Area Queries
// ---------------------------------------------------------------------------

/// Looks up a Markdown document by exact repo path or unambiguous suffix and returns its heading tree.
pub fn doc_outline<'a>(index: &CtxIndex<'_>, doc_path: &str, alloc: Alloc<'a>) -> Option<OutlineReport<'a>> {
    let norm = doc_path.trim_start_matches("./");
    let doc = index
        .docs
        .as_slice()
        .iter()
        .find(|d| d.path.as_str() == norm)
        .or_else(|| {
            index
                .docs
                .as_slice()
                .iter()
                .find(|d| d.path.as_str().ends_with(norm))
        })?;

    let mut headings = AVec::with_capacity_in(doc.headings.len(), alloc);
    for h in doc.headings.as_slice() {
        let lines = h.end_line.saturating_sub(h.start_line).saturating_add(1);
        headings.push(OutlineEntry {
            level: h.level,
            heading: AString::from_str_in(h.title.as_str(), alloc),
            ptr: format_slice_ptr(doc.path.as_str(), h.start_line, h.end_line, alloc),
            lines,
        });
    }

    Some(OutlineReport {
        path: AString::from_str_in(doc.path.as_str(), alloc),
        status: AString::from_str_in(doc.status.as_str(), alloc),
        date: doc.date.as_ref().map(|s| AString::from_str_in(s.as_str(), alloc)),
        title: AString::from_str_in(doc.title.as_str(), alloc),
        summary: AString::from_str_in(doc.summary.as_str(), alloc),
        line_count: doc.line_count,
        headings,
    })
}

/// Builds a focused subsystem card (`blaze-ctx area <crate-or-topic>`) combining the crate node,
/// dependencies, reverse dependents, top exports, linked design docs, and related agent notes.
pub fn area_report<'a>(index: &CtxIndex<'_>, target: &str, alloc: Alloc<'a>) -> AreaReport<'a> {
    let clean = target.trim().trim_start_matches("crates/").trim_end_matches('/');

    let crate_match = index
        .crates
        .as_slice()
        .iter()
        .find(|c| c.name.as_str() == clean || c.path.as_str() == target.trim())
        .or_else(|| {
            index
                .crates
                .as_slice()
                .iter()
                .find(|c| c.name.as_str().eq_ignore_ascii_case(clean))
        });

    let mut deps = AVec::new_in(alloc);
    let mut dependents = AVec::new_in(alloc);
    let mut exports = AVec::new_in(alloc);
    let mut design_docs = AVec::new_in(alloc);
    let mut notes = AVec::new_in(alloc);

    let mut seen_docs: ABTreeSet<'a, AString<'a>> = ABTreeSet::new_in(alloc);
    let mut seen_notes: ABTreeSet<'a, AString<'a>> = ABTreeSet::new_in(alloc);

    if let Some(c) = crate_match {
        for d in c.deps.as_slice() {
            deps.push(AString::from_str_in(d.as_str(), alloc));
        }
        for ex in c.exports.as_slice().iter().take(24) {
            exports.push(AreaExport {
                kind: AString::from_str_in(ex.kind.as_str(), alloc),
                name: AString::from_str_in(ex.name.as_str(), alloc),
                ptr: format_slice_ptr(c.entry_file.as_str(), ex.line, ex.line, alloc),
            });
        }

        // Traverse graph edges connected to this crate.
        for edge in index.edges.as_slice() {
            if edge.kind == EdgeKind::DependsOn && edge.to.as_str() == c.id.as_str() {
                if let Some(dep_name) = edge.from.as_str().strip_prefix("crate:") {
                    dependents.push(AString::from_str_in(dep_name, alloc));
                }
            } else if edge.kind == EdgeKind::DesignedBy && edge.from.as_str() == c.id.as_str() {
                if let Some(d) = index.docs.as_slice().iter().find(|d| d.id.as_str() == edge.to.as_str())
                    && seen_docs.insert(AString::from_str_in(d.path.as_str(), alloc))
                {
                    design_docs.push(AreaDoc {
                        status: AString::from_str_in(d.status.as_str(), alloc),
                        path: AString::from_str_in(d.path.as_str(), alloc),
                        title: AString::from_str_in(d.title.as_str(), alloc),
                        ptr: format_slice_ptr(d.path.as_str(), 1, d.line_count, alloc),
                        relation: AString::from_str_in("designed_by", alloc),
                    });
                }
            } else if edge.kind == EdgeKind::AnnoyancesFor && edge.to.as_str() == c.id.as_str() {
                if let Some(n) = index.notes.as_slice().iter().find(|n| n.id.as_str() == edge.from.as_str())
                    && seen_notes.insert(AString::from_str_in(n.path.as_str(), alloc))
                {
                    notes.push(AreaNote {
                        kind: AString::from_str_in(n.kind.as_str(), alloc),
                        note_id: AString::from_str_in(n.note_id.as_str(), alloc),
                        state: AString::from_str_in(n.state.as_deref().unwrap_or("-"), alloc),
                        ptr: format_slice_ptr(n.path.as_str(), 1, n.line_count, alloc),
                        summary: AString::from_str_in(n.summary.as_str(), alloc),
                    });
                }
            }
        }
        dependents.sort_by(|a, b| a.as_str().cmp(b.as_str()));

        // Also pull notes mentioning the crate name in area/summary if not already linked.
        for n in index.notes.as_slice() {
            if seen_notes.len() >= 15 {
                break;
            }
            if !seen_notes.contains(n.path.as_str())
                && (contains_ci(n.area.as_str(), c.name.as_str()) || contains_ci(n.summary.as_str(), c.name.as_str()))
            {
                seen_notes.insert(AString::from_str_in(n.path.as_str(), alloc));
                notes.push(AreaNote {
                    kind: AString::from_str_in(n.kind.as_str(), alloc),
                    note_id: AString::from_str_in(n.note_id.as_str(), alloc),
                    state: AString::from_str_in(n.state.as_deref().unwrap_or("-"), alloc),
                    ptr: format_slice_ptr(n.path.as_str(), 1, n.line_count, alloc),
                    summary: AString::from_str_in(n.summary.as_str(), alloc),
                });
            }
        }

        return AreaReport {
            target: AString::from_str_in(target, alloc),
            found_crate: true,
            crate_name: Some(AString::from_str_in(c.name.as_str(), alloc)),
            status: Some(AString::from_str_in(c.status.as_str(), alloc)),
            domain: AString::from_str_in(c.domain.as_str(), alloc),
            ptr: Some(format_slice_ptr(c.entry_file.as_str(), 1, c.entry_lines, alloc)),
            summary: Some(AString::from_str_in(c.summary.as_str(), alloc)),
            deps,
            dependents,
            exports,
            design_docs,
            notes,
        };
    }

    // Topic / domain fallback: gather matching docs and agent notes for the topic string.
    let domain = crate::tree::classify_domain(clean);
    for d in index.docs.as_slice() {
        if contains_ci(d.path.as_str(), clean)
            || contains_ci(d.title.as_str(), clean)
            || d.domain.as_str().eq_ignore_ascii_case(clean)
        {
            if design_docs.len() < 12 {
                design_docs.push(AreaDoc {
                    status: AString::from_str_in(d.status.as_str(), alloc),
                    path: AString::from_str_in(d.path.as_str(), alloc),
                    title: AString::from_str_in(d.title.as_str(), alloc),
                    ptr: format_slice_ptr(d.path.as_str(), 1, d.line_count, alloc),
                    relation: AString::from_str_in("topic", alloc),
                });
            }
        }
    }
    for n in index.notes.as_slice() {
        if contains_ci(n.area.as_str(), clean)
            || contains_ci(n.summary.as_str(), clean)
            || n.domain.as_str().eq_ignore_ascii_case(clean)
        {
            if notes.len() < 12 {
                notes.push(AreaNote {
                    kind: AString::from_str_in(n.kind.as_str(), alloc),
                    note_id: AString::from_str_in(n.note_id.as_str(), alloc),
                    state: AString::from_str_in(n.state.as_deref().unwrap_or("-"), alloc),
                    ptr: format_slice_ptr(n.path.as_str(), 1, n.line_count, alloc),
                    summary: AString::from_str_in(n.summary.as_str(), alloc),
                });
            }
        }
    }

    AreaReport {
        target: AString::from_str_in(target, alloc),
        found_crate: false,
        crate_name: None,
        status: None,
        domain: AString::from_str_in(domain, alloc),
        ptr: None,
        summary: None,
        deps,
        dependents,
        exports,
        design_docs,
        notes,
    }
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    let hb = haystack.as_bytes();
    let nb = needle.as_bytes();
    if nb.is_empty() || nb.len() > hb.len() {
        return false;
    }
    hb.windows(nb.len()).any(|w| {
        w.iter()
            .zip(nb.iter())
            .all(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase())
    })
}
