//! Embedding + hybrid-retrieval primitives for the notes library.
//!
//! The [`Embedder`] trait abstracts "text → vector" so the store stays backend-agnostic: a local
//! ONNX model, a cloud API, or (in tests) a deterministic stub all satisfy it. The free functions
//! are the math the store needs once vectors exist — a dot product for cosine similarity, vector
//! (de)serialization for BLOB storage, transcript chunking, and Reciprocal Rank Fusion for blending
//! the full-text and semantic rankings into one hybrid result.

use crate::Result;

/// Turns text into a fixed-length embedding vector.
///
/// Implementations MUST return L2-normalized vectors of length [`Embedder::dim`], so cosine
/// similarity reduces to a plain dot product ([`dot`]). It is `Send + Sync` so the store can hold
/// one behind the same mutex as its connection.
///
/// Passages and queries are embedded separately because asymmetric models (e.g. E5) prepend a
/// different instruction prefix to each; for symmetric models the two simply behave the same.
pub trait Embedder: Send + Sync {
    /// Length of every vector this embedder produces.
    fn dim(&self) -> usize;

    /// Embeds documents/passages for storage, into `dim()`-length L2-normalized vectors, in order.
    fn embed_passages(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    /// Embeds a single search query into a `dim()`-length L2-normalized vector.
    fn embed_query(&self, text: &str) -> Result<Vec<f32>>;

    /// The lowest query–passage similarity semantic search counts as a match. Models differ a lot:
    /// some score unrelated text well above zero, so a zero floor would match everything.
    fn min_score(&self) -> f32 {
        0.0
    }
}

/// Dot product of two equal-length vectors — cosine similarity when both are L2-normalized. Returns
/// `0.0` if the lengths differ, so a vector stored by a different (wrong-dimension) model is simply
/// treated as unrelated rather than panicking.
pub(crate) fn dot(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Serializes a vector to little-endian bytes for BLOB storage.
pub(crate) fn to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Deserializes a little-endian byte blob back into a vector (trailing partial bytes are ignored).
pub(crate) fn from_bytes(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

/// Groups consecutive segment texts into chunks of at most `max_chars` characters, never splitting
/// an individual segment — so each chunk is a coherent unit to embed. A single segment longer than
/// `max_chars` becomes its own (oversized) chunk; blank segments are skipped. Empty input yields no
/// chunks.
#[cfg(test)]
pub(crate) fn chunk_texts(texts: &[&str], max_chars: usize) -> Vec<String> {
    chunk_spans(texts, max_chars)
        .into_iter()
        .map(|c| c.text)
        .collect()
}

/// A chunk of consecutive segments: its text and the positions (in the input) of its first and last
/// segment, so a hit can point back at a transcript line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpanChunk {
    pub text: String,
    pub first: usize,
    pub last: usize,
}

/// Groups segment texts into chunks the same way, keeping each chunk's first and last input position.
pub(crate) fn chunk_spans(texts: &[&str], max_chars: usize) -> Vec<SpanChunk> {
    let mut chunks = Vec::new();
    let mut cur: Option<SpanChunk> = None;

    for (i, t) in texts.iter().enumerate() {
        let t = t.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(c) = &cur {
            if c.text.chars().count() + 1 + t.chars().count() > max_chars {
                chunks.extend(cur.take());
            }
        }
        match &mut cur {
            Some(c) => {
                c.text.push(' ');
                c.text.push_str(t);
                c.last = i;
            }
            None => {
                cur = Some(SpanChunk {
                    text: t.to_owned(),
                    first: i,
                    last: i,
                })
            }
        }
    }
    chunks.extend(cur);
    chunks
}

/// A chunk of a document and the 1-based line it starts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DocChunk {
    pub text: String,
    pub line_start: usize,
}

/// Splits a document into chunks of at most `max_chars` characters. Paragraphs (runs of non-blank
/// lines) are packed together while they fit; a paragraph longer than `max_chars` is cut at a
/// sentence end or a space in the second half of the window, or hard-cut when there is neither.
/// Every chunk records the line it starts on, so a hit can point back into the file.
pub(crate) fn chunk_document(text: &str, max_chars: usize) -> Vec<DocChunk> {
    let max_chars = max_chars.max(1);
    // Paragraphs with their first line number.
    let mut paragraphs: Vec<(usize, String)> = Vec::new();
    let mut cur: Option<(usize, String)> = None;
    for (n, line) in text.lines().enumerate() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            paragraphs.extend(cur.take());
            continue;
        }
        match &mut cur {
            Some((_, p)) => {
                p.push('\n');
                p.push_str(line);
            }
            None => cur = Some((n + 1, line.to_owned())),
        }
    }
    paragraphs.extend(cur);

    // Pieces no longer than `max_chars`.
    let mut pieces: Vec<DocChunk> = Vec::new();
    for (line, para) in paragraphs {
        split_long(&para, line, max_chars, &mut pieces);
    }

    // Pack pieces into chunks.
    let mut chunks: Vec<DocChunk> = Vec::new();
    for piece in pieces {
        if let Some(last) = chunks.last_mut() {
            if last.text.chars().count() + 2 + piece.text.chars().count() <= max_chars {
                last.text.push_str("\n\n");
                last.text.push_str(&piece.text);
                continue;
            }
        }
        chunks.push(piece);
    }
    chunks
}

/// Cuts `para` (starting on line `line`) into pieces of at most `max` characters.
fn split_long(para: &str, line: usize, max: usize, out: &mut Vec<DocChunk>) {
    let chars: Vec<char> = para.chars().collect();
    let mut start = 0;
    let mut line = line;
    while start < chars.len() {
        let mut end = (start + max).min(chars.len());
        if end < chars.len() {
            let window = &chars[start..end];
            let half = window.len() / 2;
            let cut = (half..window.len())
                .rev()
                .find(|&i| matches!(window[i], '.' | '!' | '?' | '\n'))
                .map(|i| i + 1)
                .or_else(|| {
                    (half..window.len())
                        .rev()
                        .find(|&i| window[i].is_whitespace())
                });
            if let Some(cut) = cut {
                end = start + cut.max(1);
            }
        }
        let piece: String = chars[start..end].iter().collect();
        let trimmed = piece.trim();
        if !trimmed.is_empty() {
            let leading_newlines = piece[..piece.len() - piece.trim_start().len()]
                .matches('\n')
                .count();
            out.push(DocChunk {
                text: trimmed.to_owned(),
                line_start: line + leading_newlines,
            });
        }
        line += piece.matches('\n').count();
        start = end;
    }
}

/// Reciprocal Rank Fusion. Each ranking lists ids in descending relevance; an id's fused score is
/// the sum over the rankings it appears in of `1 / (k + rank)` (0-based rank). `k` (commonly 60)
/// damps the weight of deep ranks. Returns `(id, score)` pairs, highest score first, ties broken by
/// id for determinism.
pub(crate) fn rrf_fuse(rankings: &[Vec<String>], k: f64) -> Vec<(String, f64)> {
    use std::collections::HashMap;

    let mut scores: HashMap<&str, f64> = HashMap::new();
    for ranking in rankings {
        for (rank, id) in ranking.iter().enumerate() {
            *scores.entry(id.as_str()).or_insert(0.0) += 1.0 / (k + rank as f64);
        }
    }

    let mut out: Vec<(String, f64)> = scores
        .into_iter()
        .map(|(id, s)| (id.to_owned(), s))
        .collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_is_cosine_for_normalized_and_zero_on_mismatch() {
        assert!((dot(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(dot(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6); // orthogonal
        assert_eq!(dot(&[1.0, 2.0, 3.0], &[1.0, 2.0]), 0.0); // length mismatch → 0
    }

    #[test]
    fn vector_bytes_round_trip() {
        let v = vec![0.0_f32, 1.5, -2.25, 1e9, -1e-9];
        assert_eq!(from_bytes(&to_bytes(&v)), v);
        assert!(from_bytes(&[]).is_empty());
        // Trailing partial bytes are ignored: 1.0f32 is 00 00 80 3f little-endian, then one stray byte.
        assert_eq!(from_bytes(&[0, 0, 0x80, 0x3f, 7]), vec![1.0]);
    }

    #[test]
    fn chunk_texts_groups_without_splitting_segments() {
        // Three short segments fit in one chunk.
        assert_eq!(chunk_texts(&["aa", "bb", "cc"], 100), vec!["aa bb cc"]);
        // Budget forces a new chunk, but never mid-segment.
        assert_eq!(
            chunk_texts(&["aaaa", "bbbb", "cccc"], 9),
            vec!["aaaa bbbb", "cccc"]
        );
        // A single over-budget segment becomes its own chunk.
        assert_eq!(chunk_texts(&["abcdefghij"], 4), vec!["abcdefghij"]);
        // Blanks are skipped; empty input yields nothing.
        assert_eq!(chunk_texts(&["", "  ", "x"], 100), vec!["x"]);
        assert!(chunk_texts(&[], 100).is_empty());
    }

    #[test]
    fn chunk_texts_is_char_safe_for_cjk() {
        // Budget counts characters, not bytes, so multi-byte segments group correctly.
        let chunks = chunk_texts(&["预算讨论", "排期计划"], 5);
        assert_eq!(chunks, vec!["预算讨论".to_owned(), "排期计划".to_owned()]);
    }

    #[test]
    fn chunk_spans_keep_first_and_last_positions() {
        let spans = chunk_spans(&["aaaa", "", "bbbb", "cccc"], 9);
        assert_eq!(
            spans,
            vec![
                SpanChunk {
                    text: "aaaa bbbb".into(),
                    first: 0,
                    last: 2
                },
                SpanChunk {
                    text: "cccc".into(),
                    first: 3,
                    last: 3
                },
            ]
        );
    }

    #[test]
    fn chunk_document_packs_paragraphs_and_records_lines() {
        let doc = "Title\n\nFirst para line one\nline two\n\n\nSecond para";
        let chunks = chunk_document(doc, 1000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].line_start, 1);
        assert_eq!(
            chunks[0].text,
            "Title\n\nFirst para line one\nline two\n\nSecond para"
        );

        let chunks = chunk_document(doc, 30);
        let lines: Vec<usize> = chunks.iter().map(|c| c.line_start).collect();
        assert_eq!(lines, [1, 3, 7]);
        assert!(chunks.iter().all(|c| c.text.chars().count() <= 30));
    }

    #[test]
    fn chunk_document_cuts_long_paragraphs_at_sentences_then_spaces() {
        let para = "One two three. Four five six. Seven eight nine.";
        let chunks = chunk_document(para, 20);
        assert_eq!(chunks[0].text, "One two three.");
        assert!(chunks.iter().all(|c| c.text.chars().count() <= 20));
        let joined: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(joined.join(" "), para);

        // No spaces at all: hard cut, still bounded, nothing lost.
        let blob = "x".repeat(45);
        let chunks = chunk_document(&blob, 20);
        assert_eq!(chunks.iter().map(|c| c.text.len()).sum::<usize>(), 45);
        assert!(chunks.iter().all(|c| c.text.len() <= 20));
    }

    #[test]
    fn chunk_document_tracks_lines_inside_a_long_paragraph() {
        let para = "alpha beta gamma\ndelta epsilon zeta\neta theta iota";
        let chunks = chunk_document(para, 19);
        let lines: Vec<usize> = chunks.iter().map(|c| c.line_start).collect();
        assert_eq!(lines, [1, 2, 3]);
    }

    #[test]
    fn chunk_document_of_blank_text_is_empty() {
        assert!(chunk_document("", 100).is_empty());
        assert!(chunk_document("  \n\n \t\n", 100).is_empty());
    }

    #[test]
    fn chunk_document_is_char_safe() {
        let chunks = chunk_document(&"预算讨论".repeat(10), 7);
        assert!(chunks.iter().all(|c| c.text.chars().count() <= 7));
        assert_eq!(
            chunks.iter().map(|c| c.text.chars().count()).sum::<usize>(),
            40
        );
    }

    #[test]
    fn rrf_prefers_ids_ranked_high_in_multiple_lists() {
        let a = vec!["x".to_owned(), "y".to_owned(), "z".to_owned()];
        let b = vec!["y".to_owned(), "w".to_owned()];
        let fused = rrf_fuse(&[a, b], 60.0);
        let order: Vec<&str> = fused.iter().map(|(id, _)| id.as_str()).collect();
        // y is in both lists (and ranked high in b) → it wins.
        assert_eq!(order[0], "y");
        // Every id from either list appears exactly once.
        assert_eq!(fused.len(), 4);
    }

    #[test]
    fn rrf_single_list_keeps_its_order_and_empty_is_empty() {
        let single = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let fused = rrf_fuse(&[single], 60.0);
        let order: Vec<&str> = fused.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(order, ["a", "b", "c"]);
        assert!(rrf_fuse(&[], 60.0).is_empty());
    }
}
