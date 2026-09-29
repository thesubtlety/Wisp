//! Retrieval for reasoning: small, addressable snippets from a project's sources and meetings.
//!
//! [`Library::retrieve`] runs full-text and (with an embedder) semantic search over one project's
//! source chunks and meeting transcripts, fuses the rankings, and returns a bounded list of
//! [`Snippet`]s. Each snippet carries a ref (`S12:C3` for chunk 3 of source 12, `M<id>:T41` for
//! segment 41 of a meeting) that [`Library::snippet_for_ref`] resolves back to its text while the
//! text still exists. Nothing here reads files: only what ingestion stored is searched.
//!
//! The project is the privacy boundary. A query scoped to a project sees only that project's
//! sources and meetings; a query with no project sees only meetings that belong to no project, and
//! no sources.

use std::collections::HashMap;

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::embed;
use crate::store::{truncate_chars, Library};
use crate::Result;

/// Longest snippet text returned, in characters.
pub const SNIPPET_CHARS: usize = 700;

/// Candidates taken from each ranking before fusion.
const POOL: usize = 40;

/// Reciprocal Rank Fusion damping constant (the conventional default).
const RRF_K: f64 = 60.0;

/// Most query terms sent to the full-text index.
const MAX_TERMS: usize = 16;

/// Common words that would match nearly every chunk under trigram search.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "but", "not", "you", "all", "any", "can", "had", "her", "was",
    "one", "our", "out", "has", "have", "him", "his", "how", "its", "may", "who", "did", "does",
    "yes", "get", "got", "let", "say", "she", "too", "use", "that", "this", "with", "from", "they",
    "them", "then", "than", "what", "when", "where", "which", "while", "will", "would", "could",
    "should", "there", "their", "about", "into", "just", "been", "were", "your", "some", "more",
    "also", "only", "very", "like", "want", "need", "know", "think",
];

/// What to retrieve.
#[derive(Debug, Clone)]
pub struct RetrievalQuery<'a> {
    /// Natural-language text: a question, or recent meeting material.
    pub text: &'a str,
    /// The project to search. `None` searches only meetings outside every project.
    pub project_id: Option<&'a str>,
    /// Whether to search meeting transcripts as well as sources.
    pub include_meetings: bool,
    /// A meeting to leave out, typically the one in progress (its text is sent separately).
    pub exclude_meeting_id: Option<&'a str>,
    /// Most snippets returned.
    pub limit: usize,
    /// Most characters of snippet text returned in total.
    pub max_total_chars: usize,
}

/// Where a snippet came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SnippetOrigin {
    /// A chunk of a project source.
    #[serde(rename_all = "camelCase")]
    Source {
        source_id: i64,
        chunk_idx: i64,
        label: String,
        /// The 1-based line of the source's text the chunk starts on.
        line_start: Option<i64>,
    },
    /// A stretch of a stored meeting's transcript.
    #[serde(rename_all = "camelCase")]
    Meeting {
        meeting_id: String,
        title: String,
        started_at_ms: i64,
        /// The segment the ref points at.
        segment_idx: i64,
        /// Offset of that segment into the meeting.
        start_ms: i64,
    },
}

/// A retrieved piece of text and where it came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snippet {
    /// Stable address of the evidence: `S<source>:C<chunk>` or `M<meeting>:T<segment>`.
    pub ref_id: String,
    pub origin: SnippetOrigin,
    pub text: String,
    /// Fused relevance; higher is better. Only meaningful within one result list.
    pub score: f64,
}

impl Snippet {
    /// The meeting a meeting snippet comes from.
    fn meeting_id(&self) -> Option<&str> {
        match &self.origin {
            SnippetOrigin::Meeting { meeting_id, .. } => Some(meeting_id),
            SnippetOrigin::Source { .. } => None,
        }
    }
}

/// A candidate before fusion: the snippet plus the segment range it covers (meetings only).
struct Candidate {
    snippet: Snippet,
    covers: Option<(i64, i64)>,
}

/// The ref for chunk `idx` of source `id`.
pub fn source_ref(source_id: i64, chunk_idx: i64) -> String {
    format!("S{source_id}:C{chunk_idx}")
}

/// The ref for segment `idx` of meeting `id`.
pub fn meeting_ref(meeting_id: &str, segment_idx: i64) -> String {
    format!("M{meeting_id}:T{segment_idx}")
}

/// Query terms for the full-text index: words of 3+ characters, lower-cased, stopwords and
/// duplicates dropped, capped at [`MAX_TERMS`].
fn fts_terms(text: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for word in text.split(|c: char| !c.is_alphanumeric()) {
        let word = word.to_lowercase();
        if word.chars().count() < 3 || STOPWORDS.contains(&word.as_str()) || terms.contains(&word) {
            continue;
        }
        terms.push(word);
        if terms.len() == MAX_TERMS {
            break;
        }
    }
    terms
}

/// An FTS5 query matching any of `terms` (each quoted, so punctuation can't break the syntax).
fn fts_any(terms: &[String]) -> String {
    terms
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

impl Library {
    /// Retrieves the snippets most relevant to `query.text` within its scope, best first, capped by
    /// `limit` and `max_total_chars`. Full-text search always runs; semantic search joins in when an
    /// embedder is set. Meeting hits that overlap an earlier one are dropped. Meetings whose
    /// transcript was pruned have nothing left to return.
    pub fn retrieve(&self, query: &RetrievalQuery) -> Result<Vec<Snippet>> {
        if query.text.trim().is_empty() || query.limit == 0 {
            return Ok(Vec::new());
        }
        let terms = fts_terms(query.text);
        let qvec = match &self.embedder {
            Some(e) => Some(e.embed_query(query.text.trim())?),
            None => None,
        };

        let mut rankings: Vec<Vec<Candidate>> = Vec::new();
        if let Some(project) = query.project_id {
            if !terms.is_empty() {
                rankings.push(self.source_fts(project, &terms)?);
            }
            if let Some(q) = &qvec {
                rankings.push(self.source_semantic(project, q)?);
            }
        }
        if query.include_meetings {
            if !terms.is_empty() {
                rankings.push(self.meeting_fts(query, &terms)?);
            }
            if let Some(q) = &qvec {
                rankings.push(self.meeting_semantic(query, q)?);
            }
        }

        let ids: Vec<Vec<String>> = rankings
            .iter()
            .map(|r| r.iter().map(|c| c.snippet.ref_id.clone()).collect())
            .collect();
        let mut by_ref: HashMap<String, Candidate> = HashMap::new();
        for c in rankings.into_iter().flatten() {
            by_ref.entry(c.snippet.ref_id.clone()).or_insert(c);
        }

        let mut out: Vec<Snippet> = Vec::new();
        let mut covered: Vec<(String, i64, i64)> = Vec::new();
        let mut total = 0usize;
        for (ref_id, score) in embed::rrf_fuse(&ids, RRF_K) {
            let Some(c) = by_ref.remove(&ref_id) else {
                continue;
            };
            if let (Some(meeting), Some((lo, hi))) = (c.snippet.meeting_id(), c.covers) {
                if covered
                    .iter()
                    .any(|(m, a, b)| m == meeting && lo <= *b && *a <= hi)
                {
                    continue;
                }
                covered.push((meeting.to_owned(), lo, hi));
            }
            let len = c.snippet.text.chars().count();
            if total + len > query.max_total_chars {
                continue;
            }
            total += len;
            out.push(Snippet { score, ..c.snippet });
            if out.len() == query.limit {
                break;
            }
        }
        Ok(out)
    }

    /// Resolves a ref from [`Library::retrieve`] to its snippet, or `None` when the text no longer
    /// exists (the source was removed or expired, or the transcript was pruned).
    pub fn snippet_for_ref(&self, ref_id: &str) -> Result<Option<Snippet>> {
        if let Some(rest) = ref_id.strip_prefix('S') {
            let Some((source, chunk)) = rest.split_once(":C") else {
                return Ok(None);
            };
            let (Ok(source), Ok(chunk)) = (source.parse::<i64>(), chunk.parse::<i64>()) else {
                return Ok(None);
            };
            return Ok(self
                .conn
                .query_row(
                    "SELECT c.text, c.line_start, s.label FROM source_chunk c
                     JOIN source s ON s.id = c.source_id
                     WHERE c.source_id = ?1 AND c.idx = ?2",
                    rusqlite::params![source, chunk],
                    |r| {
                        Ok(source_snippet(
                            source,
                            chunk,
                            r.get(2)?,
                            r.get(1)?,
                            &r.get::<_, String>(0)?,
                        ))
                    },
                )
                .optional()?);
        }
        if let Some(rest) = ref_id.strip_prefix('M') {
            let Some((meeting, seg)) = rest.rsplit_once(":T") else {
                return Ok(None);
            };
            let Ok(seg) = seg.parse::<i64>() else {
                return Ok(None);
            };
            return self.meeting_snippet(meeting, seg, 0.0);
        }
        Ok(None)
    }

    fn source_fts(&self, project: &str, terms: &[String]) -> Result<Vec<Candidate>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.source_id, c.idx, c.text, c.line_start, s.label, bm25(source_chunk_fts)
             FROM source_chunk_fts
             JOIN source_chunk c ON c.id = source_chunk_fts.rowid
             JOIN source s ON s.id = c.source_id
             WHERE source_chunk_fts MATCH ?1 AND s.project_id = ?2
             ORDER BY bm25(source_chunk_fts)
             LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(
                rusqlite::params![fts_any(terms), project, POOL as i64],
                |r| {
                    let mut s = source_snippet(
                        r.get(0)?,
                        r.get(1)?,
                        r.get(4)?,
                        r.get(3)?,
                        &r.get::<_, String>(2)?,
                    );
                    s.score = -r.get::<_, f64>(5)?;
                    Ok(Candidate {
                        snippet: s,
                        covers: None,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn source_semantic(&self, project: &str, qvec: &[f32]) -> Result<Vec<Candidate>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.source_id, c.idx, c.text, c.line_start, s.label, c.embedding
             FROM source_chunk c JOIN source s ON s.id = c.source_id
             WHERE s.project_id = ?1 AND c.embedding IS NOT NULL",
        )?;
        let mut rows = stmt
            .query_map([project], |r| {
                let blob: Vec<u8> = r.get(5)?;
                let mut s = source_snippet(
                    r.get(0)?,
                    r.get(1)?,
                    r.get(4)?,
                    r.get(3)?,
                    &r.get::<_, String>(2)?,
                );
                s.score = f64::from(embed::dot(qvec, &embed::from_bytes(&blob)));
                Ok(Candidate {
                    snippet: s,
                    covers: None,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(top_by_score(&mut rows))
    }

    fn meeting_fts(&self, query: &RetrievalQuery, terms: &[String]) -> Result<Vec<Candidate>> {
        let hits: Vec<(String, i64, f64)> = {
            let mut stmt = self.conn.prepare(
                "SELECT s.meeting_id, s.idx, bm25(segment_fts)
                 FROM segment_fts
                 JOIN segment s ON s.id = segment_fts.rowid
                 JOIN meeting m ON m.id = s.meeting_id
                 WHERE segment_fts MATCH ?1 AND m.project_id IS ?2 AND m.id IS NOT ?3
                 ORDER BY bm25(segment_fts)
                 LIMIT ?4",
            )?;
            let rows = stmt
                .query_map(
                    rusqlite::params![
                        fts_any(terms),
                        query.project_id,
                        query.exclude_meeting_id,
                        POOL as i64
                    ],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let mut out = Vec::new();
        for (meeting, idx, bm25) in hits {
            if let Some(snippet) = self.meeting_snippet(&meeting, idx, -bm25)? {
                out.push(Candidate {
                    snippet,
                    covers: Some((idx - 1, idx + 1)),
                });
            }
        }
        Ok(out)
    }

    fn meeting_semantic(&self, query: &RetrievalQuery, qvec: &[f32]) -> Result<Vec<Candidate>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.meeting_id, m.title, m.started_at_ms, c.text, c.embedding, c.seg_start,
                    c.seg_end, (SELECT start_ms FROM segment
                                WHERE meeting_id = c.meeting_id AND idx = c.seg_start)
             FROM chunk c JOIN meeting m ON m.id = c.meeting_id
             WHERE m.project_id IS ?1 AND m.id IS NOT ?2 AND c.seg_start IS NOT NULL",
        )?;
        let mut rows = stmt
            .query_map(
                rusqlite::params![query.project_id, query.exclude_meeting_id],
                |r| {
                    let meeting_id: String = r.get(0)?;
                    let blob: Vec<u8> = r.get(4)?;
                    let seg_start: i64 = r.get(5)?;
                    let seg_end: i64 = r.get(6)?;
                    Ok(Candidate {
                        snippet: Snippet {
                            ref_id: meeting_ref(&meeting_id, seg_start),
                            origin: SnippetOrigin::Meeting {
                                meeting_id,
                                title: r.get(1)?,
                                started_at_ms: r.get(2)?,
                                segment_idx: seg_start,
                                start_ms: r.get::<_, Option<i64>>(7)?.unwrap_or(0),
                            },
                            text: truncate_chars(&r.get::<_, String>(3)?, SNIPPET_CHARS),
                            score: f64::from(embed::dot(qvec, &embed::from_bytes(&blob))),
                        },
                        covers: Some((seg_start, seg_end)),
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(top_by_score(&mut rows))
    }

    /// Segment `idx` of a meeting with one segment of context either side, or `None` if the segment
    /// is gone.
    fn meeting_snippet(&self, meeting_id: &str, idx: i64, score: f64) -> Result<Option<Snippet>> {
        let head: Option<(String, i64, i64)> = self
            .conn
            .query_row(
                "SELECT m.title, m.started_at_ms, s.start_ms FROM segment s
                 JOIN meeting m ON m.id = s.meeting_id
                 WHERE s.meeting_id = ?1 AND s.idx = ?2",
                rusqlite::params![meeting_id, idx],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((title, started_at_ms, start_ms)) = head else {
            return Ok(None);
        };
        let mut stmt = self.conn.prepare(
            "SELECT text FROM segment WHERE meeting_id = ?1 AND idx BETWEEN ?2 AND ?3 ORDER BY idx",
        )?;
        let text = stmt
            .query_map(rusqlite::params![meeting_id, idx - 1, idx + 1], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .join(" ");
        Ok(Some(Snippet {
            ref_id: meeting_ref(meeting_id, idx),
            origin: SnippetOrigin::Meeting {
                meeting_id: meeting_id.to_owned(),
                title,
                started_at_ms,
                segment_idx: idx,
                start_ms,
            },
            text: truncate_chars(&text, SNIPPET_CHARS),
            score,
        }))
    }
}

fn source_snippet(
    source_id: i64,
    chunk_idx: i64,
    label: String,
    line_start: Option<i64>,
    text: &str,
) -> Snippet {
    Snippet {
        ref_id: source_ref(source_id, chunk_idx),
        origin: SnippetOrigin::Source {
            source_id,
            chunk_idx,
            label,
            line_start,
        },
        text: truncate_chars(text, SNIPPET_CHARS),
        score: 0.0,
    }
}

/// Keeps the [`POOL`] best candidates with a positive score, best first.
fn top_by_score(rows: &mut Vec<Candidate>) -> Vec<Candidate> {
    rows.retain(|c| c.snippet.score > 0.0);
    rows.sort_by(|a, b| {
        b.snippet
            .score
            .partial_cmp(&a.snippet.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows.truncate(POOL);
    std::mem::take(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Embedder, SourceInput, SourceKind, Upsert};
    use std::path::PathBuf;
    use std::time::Duration;
    use wisp_core::export::MeetingMeta;
    use wisp_core::transcript::{AudioSourceKind, SegmentStatus, TranscriptSegment};

    const T0: i64 = 1_700_000_000_000;

    fn seg(id: u64, text: &str) -> TranscriptSegment {
        TranscriptSegment {
            id,
            text: text.to_owned(),
            start: Duration::from_millis(id * 1000),
            end: Duration::from_millis(id * 1000 + 900),
            status: SegmentStatus::Final,
            source: AudioSourceKind::System,
            speaker: None,
            confidence: None,
            words: Vec::new(),
            aux_text: None,
        }
    }

    fn meta(title: &str) -> MeetingMeta {
        MeetingMeta {
            title: Some(title.to_owned()),
            date: None,
            engine: None,
            language: None,
            summary: None,
        }
    }

    /// One dimension per concept; "sso" and "saml" share one, so a semantic hit needs no shared
    /// word. Text with none of the concepts embeds to zero and matches nothing.
    struct ConceptEmbedder;

    const CONCEPTS: &[&[&str]] = &[&["azure"], &["sso", "saml", "oidc"], &["budget"]];

    impl Embedder for ConceptEmbedder {
        fn dim(&self) -> usize {
            CONCEPTS.len()
        }
        fn embed_passages(&self, texts: &[&str]) -> crate::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|t| concept_vec(t)).collect())
        }
        fn embed_query(&self, text: &str) -> crate::Result<Vec<f32>> {
            Ok(concept_vec(text))
        }
    }

    fn concept_vec(text: &str) -> Vec<f32> {
        let lower = text.to_lowercase();
        let mut v: Vec<f32> = CONCEPTS
            .iter()
            .map(|words| {
                if words.iter().any(|w| lower.contains(w)) {
                    1.0
                } else {
                    0.0
                }
            })
            .collect();
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            v.iter_mut().for_each(|x| *x /= norm);
        }
        v
    }

    fn pasted(label: &str, text: &str) -> SourceInput {
        SourceInput {
            kind: SourceKind::Pasted,
            label: label.to_owned(),
            text: text.to_owned(),
            origin_path: None,
            managed_path: None,
            added_at_ms: T0,
        }
    }

    fn file(path: &str, text: &str) -> SourceInput {
        SourceInput {
            kind: SourceKind::File,
            label: path.to_owned(),
            text: text.to_owned(),
            origin_path: Some(PathBuf::from(path)),
            managed_path: None,
            added_at_ms: T0,
        }
    }

    fn query<'a>(text: &'a str, project: Option<&'static str>) -> RetrievalQuery<'a> {
        RetrievalQuery {
            text,
            project_id: project,
            include_meetings: true,
            exclude_meeting_id: None,
            limit: 10,
            max_total_chars: 10_000,
        }
    }

    fn refs(snippets: &[Snippet]) -> Vec<&str> {
        snippets.iter().map(|s| s.ref_id.as_str()).collect()
    }

    /// Refs sorted, for results whose order is a tie.
    fn ref_set(snippets: &[Snippet]) -> Vec<&str> {
        let mut r = refs(snippets);
        r.sort_unstable();
        r
    }

    /// Two projects and an unassigned meeting, all mentioning Azure.
    fn two_projects() -> (Library, i64, i64) {
        let mut lib = Library::open_in_memory().unwrap();
        lib.create_project("a", "Acme", T0).unwrap();
        lib.create_project("b", "Beta", T0).unwrap();
        let sa = lib
            .add_source(
                "a",
                &pasted("acme notes", "Acme hosts everything in Azure."),
            )
            .unwrap();
        let sb = lib
            .add_source(
                "b",
                &pasted("beta notes", "Beta is moving off Azure next year."),
            )
            .unwrap();
        for (id, project) in [("ma", Some("a")), ("mb", Some("b")), ("mx", None)] {
            lib.save_note(id, &meta(id), T0, &[seg(1, "we deploy to azure")])
                .unwrap();
            lib.set_meeting_project(id, project).unwrap();
        }
        (lib, sa, sb)
    }

    #[test]
    fn fts_terms_drop_short_words_stopwords_and_duplicates() {
        assert_eq!(
            fts_terms("What is the Azure tenant? azure, AZURE; SSO it"),
            ["azure", "tenant", "sso"]
        );
        assert_eq!(fts_terms(&"word ".repeat(3)), ["word"]);
        let many: String = (0..40).map(|i| format!("term{i} ")).collect();
        assert_eq!(fts_terms(&many).len(), MAX_TERMS);
        assert!(fts_terms("a an to of").is_empty());
    }

    #[test]
    fn retrieval_stays_inside_the_project() {
        let (lib, sa, sb) = two_projects();
        let hits = lib.retrieve(&query("azure hosting", Some("a"))).unwrap();
        assert_eq!(ref_set(&hits), [meeting_ref("ma", 0), source_ref(sa, 0)]);

        let hits = lib.retrieve(&query("azure hosting", Some("b"))).unwrap();
        assert_eq!(ref_set(&hits), [meeting_ref("mb", 0), source_ref(sb, 0)]);

        // No project: only meetings outside every project, and no sources at all.
        let hits = lib.retrieve(&query("azure hosting", None)).unwrap();
        assert_eq!(refs(&hits), [meeting_ref("mx", 0)]);
    }

    #[test]
    fn retrieval_can_skip_meetings_or_leave_one_out() {
        let (lib, sa, _) = two_projects();
        let mut q = query("azure", Some("a"));
        q.exclude_meeting_id = Some("ma");
        assert_eq!(refs(&lib.retrieve(&q).unwrap()), [source_ref(sa, 0)]);

        let mut q = query("azure", Some("a"));
        q.include_meetings = false;
        assert_eq!(refs(&lib.retrieve(&q).unwrap()), [source_ref(sa, 0)]);
    }

    #[test]
    fn blank_or_stopword_queries_return_nothing_without_an_embedder() {
        let (lib, _, _) = two_projects();
        assert!(lib.retrieve(&query("  ", Some("a"))).unwrap().is_empty());
        assert!(lib
            .retrieve(&query("what is it", Some("a")))
            .unwrap()
            .is_empty());
        let mut q = query("azure", Some("a"));
        q.limit = 0;
        assert!(lib.retrieve(&q).unwrap().is_empty());
    }

    #[test]
    fn snippets_carry_their_origin_and_refs_resolve_until_the_text_is_gone() {
        let (mut lib, sa, _) = two_projects();
        let mut hits = lib.retrieve(&query("azure", Some("a"))).unwrap();
        hits.sort_by_key(|h| h.ref_id.starts_with('M'));
        assert_eq!(
            hits[0].origin,
            SnippetOrigin::Source {
                source_id: sa,
                chunk_idx: 0,
                label: "acme notes".into(),
                line_start: Some(1),
            }
        );
        assert_eq!(
            hits[1].origin,
            SnippetOrigin::Meeting {
                meeting_id: "ma".into(),
                title: "ma".into(),
                started_at_ms: T0,
                segment_idx: 0,
                start_ms: 1000,
            }
        );
        for hit in &hits {
            let resolved = lib.snippet_for_ref(&hit.ref_id).unwrap().unwrap();
            assert_eq!(
                (resolved.origin, resolved.text),
                (hit.origin.clone(), hit.text.clone())
            );
        }

        let root = tempfile::tempdir().unwrap();
        lib.remove_source(sa, root.path()).unwrap();
        lib.prune(i64::MAX, root.path()).unwrap();
        for hit in &hits {
            assert_eq!(lib.snippet_for_ref(&hit.ref_id).unwrap(), None);
        }
        assert!(lib.retrieve(&query("azure", Some("a"))).unwrap().is_empty());

        for bad in ["", "S", "S1", "Sx:C0", "S1:Cx", "Mma", "Mma:Tx", "Q1:C0"] {
            assert_eq!(lib.snippet_for_ref(bad).unwrap(), None, "{bad}");
        }
    }

    #[test]
    fn semantic_search_finds_related_text_with_no_shared_word() {
        let mut lib = Library::open_in_memory().unwrap();
        lib.set_embedder(Some(Box::new(ConceptEmbedder)));
        lib.create_project("a", "Acme", T0).unwrap();
        let s = lib
            .add_source("a", &pasted("security", "Login goes through SAML."))
            .unwrap();
        lib.add_source("a", &pasted("money", "The budget is fixed."))
            .unwrap();
        lib.save_note("m", &meta("m"), T0, &[seg(1, "they asked about OIDC")])
            .unwrap();
        lib.set_meeting_project("m", Some("a")).unwrap();

        let hits = lib.retrieve(&query("sso", Some("a"))).unwrap();
        assert_eq!(ref_set(&hits), [meeting_ref("m", 0), source_ref(s, 0)]);
    }

    #[test]
    fn overlapping_meeting_hits_collapse_into_one_snippet() {
        let mut lib = Library::open_in_memory().unwrap();
        lib.set_embedder(Some(Box::new(ConceptEmbedder)));
        lib.save_note(
            "m",
            &meta("m"),
            T0,
            &[
                seg(1, "azure tenant"),
                seg(2, "azure region"),
                seg(3, "lunch"),
            ],
        )
        .unwrap();
        let hits = lib.retrieve(&query("azure", None)).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].text.contains("tenant") && hits[0].text.contains("region"));
    }

    #[test]
    fn limit_and_character_budget_bound_the_result() {
        let mut lib = Library::open_in_memory().unwrap();
        lib.create_project("a", "Acme", T0).unwrap();
        for i in 0..8 {
            lib.add_source("a", &pasted(&format!("n{i}"), &format!("azure fact {i}")))
                .unwrap();
        }
        let mut q = query("azure", Some("a"));
        q.limit = 3;
        assert_eq!(lib.retrieve(&q).unwrap().len(), 3);

        let mut q = query("azure", Some("a"));
        q.max_total_chars = 25; // each snippet is 12 chars
        let hits = lib.retrieve(&q).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().map(|h| h.text.chars().count()).sum::<usize>() <= 25);
    }

    #[test]
    fn long_documents_come_back_as_bounded_chunks_with_line_numbers() {
        let mut lib = Library::open_in_memory().unwrap();
        lib.create_project("a", "Acme", T0).unwrap();
        let filler = "Background text about nothing in particular.\n".repeat(30);
        let doc = format!("{filler}\n\nSecurity\n\nThe tenant must stay in Azure.\n\n{filler}");
        let s = lib.add_source("a", &pasted("spec", &doc)).unwrap();
        let hits = lib.retrieve(&query("tenant azure", Some("a"))).unwrap();
        assert!(hits.len() == 1 && hits[0].text.chars().count() <= SNIPPET_CHARS);
        assert!(hits[0].text.contains("The tenant must stay in Azure."));
        let SnippetOrigin::Source { line_start, .. } = hits[0].origin else {
            panic!("not a source hit");
        };
        assert!(line_start.unwrap() >= 3, "{line_start:?}");
        assert!(hits[0].ref_id.starts_with(&format!("S{s}:C")));
    }

    #[test]
    fn upgrading_from_v3_backfills_chunk_spans() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.db");
        let before: Vec<(i64, i64)>;
        {
            let mut lib = Library::open(&path).unwrap();
            lib.set_embedder(Some(Box::new(ConceptEmbedder)));
            let long = "azure ".repeat(60);
            lib.save_note(
                "m",
                &meta("m"),
                T0,
                &[seg(1, &long), seg(2, &long), seg(3, "x")],
            )
            .unwrap();
            let mut stmt = lib
                .conn
                .prepare("SELECT seg_start, seg_end FROM chunk ORDER BY idx")
                .unwrap();
            before = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(before, [(0, 0), (1, 2)]);
            lib.conn
                .execute_batch(
                    "ALTER TABLE chunk DROP COLUMN seg_start;
                     ALTER TABLE chunk DROP COLUMN seg_end;
                     ALTER TABLE source_chunk DROP COLUMN line_start;
                     DROP TABLE state_op;
                     DROP TABLE candidate_log;
                     DROP TABLE project_memory;
                     DROP TABLE llm_call;
                     DROP TABLE speaker_name;
                     ALTER TABLE project DROP COLUMN instructions;
                     PRAGMA user_version = 3;",
                )
                .unwrap();
        }
        let lib = Library::open(&path).unwrap();
        let mut stmt = lib
            .conn
            .prepare("SELECT seg_start, seg_end FROM chunk ORDER BY idx")
            .unwrap();
        let after: Vec<(i64, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(after, before);
    }

    #[test]
    fn reindexing_covers_source_chunks() {
        let mut lib = Library::open_in_memory().unwrap();
        lib.create_project("a", "Acme", T0).unwrap();
        lib.add_source("a", &pasted("n", "Login goes through SAML."))
            .unwrap();
        let vectors = |lib: &Library| -> i64 {
            lib.conn
                .query_row(
                    "SELECT count(*) FROM source_chunk WHERE embedding IS NOT NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(vectors(&lib), 0);
        lib.set_embedder_and_reindex(Some(Box::new(ConceptEmbedder)))
            .unwrap();
        assert_eq!(vectors(&lib), 1);
        assert_eq!(lib.retrieve(&query("sso", Some("a"))).unwrap().len(), 1);
        lib.set_embedder_and_reindex(None).unwrap();
        assert_eq!(vectors(&lib), 0);
    }

    #[test]
    fn upserting_a_file_skips_unchanged_text_and_replaces_changed_text() {
        let mut lib = Library::open_in_memory().unwrap();
        lib.create_project("a", "Acme", T0).unwrap();
        let Upsert::Added(first) = lib
            .upsert_file_source("a", &file("/docs/spec.md", "azure v1"))
            .unwrap()
        else {
            panic!("expected Added");
        };
        assert_eq!(
            lib.upsert_file_source("a", &file("/docs/spec.md", "azure v1"))
                .unwrap(),
            Upsert::Unchanged(first)
        );
        let Upsert::Updated(second) = lib
            .upsert_file_source("a", &file("/docs/spec.md", "azure v2"))
            .unwrap()
        else {
            panic!("expected Updated");
        };
        let sources = lib.list_sources("a").unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].id, second);
        let hits = lib.retrieve(&query("azure", Some("a"))).unwrap();
        assert_eq!(refs(&hits), [source_ref(second, 0)]);
        assert_eq!(hits[0].text, "azure v2");

        // A leftover duplicate for the path (an interrupted refresh) goes with the next change.
        lib.add_source("a", &file("/docs/spec.md", "azure v2"))
            .unwrap();
        assert_eq!(lib.list_sources("a").unwrap().len(), 2);
        assert!(matches!(
            lib.upsert_file_source("a", &file("/docs/spec.md", "azure v3"))
                .unwrap(),
            Upsert::Updated(_)
        ));
        assert_eq!(lib.list_sources("a").unwrap().len(), 1);

        // The same path in another project is a separate source.
        lib.create_project("b", "Beta", T0).unwrap();
        assert!(matches!(
            lib.upsert_file_source("b", &file("/docs/spec.md", "azure v2"))
                .unwrap(),
            Upsert::Added(_)
        ));
        // Pasted text is always added.
        assert!(matches!(
            lib.upsert_file_source("a", &pasted("p", "azure v2"))
                .unwrap(),
            Upsert::Added(_)
        ));
    }
}
