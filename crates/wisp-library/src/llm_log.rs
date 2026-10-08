//! The AI activity log: one row per model call, with everything sent and received (see
//! `SCHEMA_V8` and `SCHEMA_V10`). It holds transcript text, so it lives as long as transcripts do: a call made for
//! a meeting goes when that meeting's transcript expires or the meeting is deleted, and any other
//! call expires by its own time under the same retention.

use serde::{Deserialize, Serialize};

use crate::store::Library;
use crate::Result;

/// One logged model call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmCall {
    /// Assigned by the store; ignored on insert.
    pub id: i64,
    pub at_ms: i64,
    /// The meeting the call was made for, if any.
    pub meeting_id: Option<String>,
    pub task: String,
    pub backend: String,
    pub model: Option<String>,
    /// Whether the data stayed on this machine.
    pub local: bool,
    pub instructions: String,
    pub context: String,
    /// Paths of images attached to the call.
    pub images: Vec<String>,
    pub output: String,
    pub error: Option<String>,
    pub elapsed_ms: i64,
    /// Prompt tokens not read from or written to a cache (all prompt tokens before schema v10).
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub tokens_estimated: bool,
    /// Prompt tokens read from the provider's cache.
    #[serde(default)]
    pub cache_read_tokens: i64,
    /// Prompt tokens written to the provider's cache.
    #[serde(default)]
    pub cache_write_tokens: i64,
    /// The API-price cost the backend reported, in US dollars; `None` when it reported none.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// The model that answered, as the backend reported it.
    #[serde(default)]
    pub model_reported: Option<String>,
}

/// Sums over a set of logged calls.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmTotals {
    pub calls: i64,
    pub tokens_in: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub tokens_out: i64,
    /// Calls whose tokens are estimates.
    pub estimated_calls: i64,
    /// The sum of every reported cost, in US dollars.
    pub cost_usd: f64,
    /// Calls with no reported cost, so `cost_usd` leaves them out.
    pub uncosted_calls: i64,
}

const COLUMNS: &str = "id, at_ms, meeting_id, task, backend, model, local, instructions, context, \
                       images, output, error, elapsed_ms, tokens_in, tokens_out, tokens_estimated, \
                       cache_read_tokens, cache_write_tokens, cost_usd, model_reported";

impl Library {
    /// Logs one call. Returns its row id.
    pub fn insert_llm_call(&self, call: &LlmCall) -> Result<i64> {
        let images = serde_json::to_string(&call.images).unwrap_or_else(|_| "[]".into());
        self.conn.execute(
            "INSERT INTO llm_call (at_ms, meeting_id, task, backend, model, local, instructions,
                 context, images, output, error, elapsed_ms, tokens_in, tokens_out, tokens_estimated,
                 cache_read_tokens, cache_write_tokens, cost_usd, model_reported)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                 ?18, ?19)",
            rusqlite::params![
                call.at_ms,
                call.meeting_id,
                call.task,
                call.backend,
                call.model,
                call.local,
                call.instructions,
                call.context,
                images,
                call.output,
                call.error,
                call.elapsed_ms,
                call.tokens_in,
                call.tokens_out,
                call.tokens_estimated,
                call.cache_read_tokens,
                call.cache_write_tokens,
                call.cost_usd,
                call.model_reported,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The newest `limit` calls, newest first; only `meeting_id`'s when given.
    pub fn llm_calls(&self, meeting_id: Option<&str>, limit: usize) -> Result<Vec<LlmCall>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLUMNS} FROM llm_call WHERE ?1 IS NULL OR meeting_id = ?1
             ORDER BY at_ms DESC, id DESC LIMIT ?2"
        ))?;
        let rows = stmt
            .query_map(rusqlite::params![meeting_id, limit as i64], |r| {
                let images: String = r.get(9)?;
                Ok(LlmCall {
                    id: r.get(0)?,
                    at_ms: r.get(1)?,
                    meeting_id: r.get(2)?,
                    task: r.get(3)?,
                    backend: r.get(4)?,
                    model: r.get(5)?,
                    local: r.get(6)?,
                    instructions: r.get(7)?,
                    context: r.get(8)?,
                    images: serde_json::from_str(&images).unwrap_or_default(),
                    output: r.get(10)?,
                    error: r.get(11)?,
                    elapsed_ms: r.get(12)?,
                    tokens_in: r.get(13)?,
                    tokens_out: r.get(14)?,
                    tokens_estimated: r.get(15)?,
                    cache_read_tokens: r.get(16)?,
                    cache_write_tokens: r.get(17)?,
                    cost_usd: r.get(18)?,
                    model_reported: r.get(19)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Sums over every logged call, or only `meeting_id`'s when given. Not capped like
    /// [`Library::llm_calls`].
    pub fn llm_call_totals(&self, meeting_id: Option<&str>) -> Result<LlmTotals> {
        let totals = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(tokens_in), 0), COALESCE(SUM(cache_read_tokens), 0),
                    COALESCE(SUM(cache_write_tokens), 0), COALESCE(SUM(tokens_out), 0),
                    COALESCE(SUM(tokens_estimated), 0), COALESCE(SUM(cost_usd), 0.0),
                    COUNT(*) - COUNT(cost_usd)
             FROM llm_call WHERE ?1 IS NULL OR meeting_id = ?1",
            [meeting_id],
            |r| {
                Ok(LlmTotals {
                    calls: r.get(0)?,
                    tokens_in: r.get(1)?,
                    cache_read_tokens: r.get(2)?,
                    cache_write_tokens: r.get(3)?,
                    tokens_out: r.get(4)?,
                    estimated_calls: r.get(5)?,
                    cost_usd: r.get(6)?,
                    uncosted_calls: r.get(7)?,
                })
            },
        )?;
        Ok(totals)
    }

    /// Deletes the whole log. Returns how many calls it held.
    pub fn clear_llm_calls(&self) -> Result<usize> {
        let n = self.conn.execute("DELETE FROM llm_call", [])?;
        Ok(n)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn call(at_ms: i64, meeting: Option<&str>) -> LlmCall {
        LlmCall {
            id: 0,
            at_ms,
            meeting_id: meeting.map(str::to_owned),
            task: "ask".into(),
            backend: "local".into(),
            model: Some("qwen3:8b".into()),
            local: true,
            instructions: "Answer.".into(),
            context: "L1 You: hi".into(),
            images: vec!["/data/shot.png".into()],
            output: "{\"answer\":\"hi\"}".into(),
            error: None,
            elapsed_ms: 900,
            tokens_in: 12,
            tokens_out: 4,
            tokens_estimated: false,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: Some(0.0),
            model_reported: None,
        }
    }

    /// A Claude call that reported cache use, cost and model.
    fn claude_call(at_ms: i64, meeting: Option<&str>, cost: f64) -> LlmCall {
        LlmCall {
            backend: "claude".into(),
            model: None,
            local: false,
            tokens_in: 2,
            tokens_out: 4,
            cache_read_tokens: 3144,
            cache_write_tokens: 3116,
            cost_usd: Some(cost),
            model_reported: Some("claude-opus-5-5".into()),
            ..call(at_ms, meeting)
        }
    }

    #[test]
    fn cost_cache_and_model_round_trip() {
        let lib = Library::open_in_memory().unwrap();
        let id = lib
            .insert_llm_call(&claude_call(1, Some("m1"), 0.0453))
            .unwrap();
        assert_eq!(
            lib.llm_calls(None, 10).unwrap(),
            [LlmCall {
                id,
                ..claude_call(1, Some("m1"), 0.0453)
            }]
        );
    }

    #[test]
    fn totals_sum_tokens_and_known_costs_and_count_the_rest() {
        let lib = Library::open_in_memory().unwrap();
        assert_eq!(lib.llm_call_totals(None).unwrap(), LlmTotals::default());
        lib.insert_llm_call(&claude_call(1, Some("m1"), 0.04))
            .unwrap();
        lib.insert_llm_call(&claude_call(2, Some("m1"), 0.01))
            .unwrap();
        // A Codex call: real tokens, no cost.
        lib.insert_llm_call(&LlmCall {
            backend: "codex".into(),
            cost_usd: None,
            ..call(3, Some("m1"))
        })
        .unwrap();
        lib.insert_llm_call(&LlmCall {
            tokens_estimated: true,
            ..call(4, Some("m2"))
        })
        .unwrap();

        let m1 = lib.llm_call_totals(Some("m1")).unwrap();
        assert_eq!(m1.calls, 3);
        assert_eq!(m1.tokens_in, 2 + 2 + 12);
        assert_eq!(m1.cache_read_tokens, 2 * 3144);
        assert_eq!(m1.cache_write_tokens, 2 * 3116);
        assert_eq!(m1.tokens_out, 4 + 4 + 4);
        assert!((m1.cost_usd - 0.05).abs() < 1e-9, "{}", m1.cost_usd);
        assert_eq!(m1.uncosted_calls, 1, "the codex call is partial");
        assert_eq!(m1.estimated_calls, 0);

        let all = lib.llm_call_totals(None).unwrap();
        assert_eq!(
            (all.calls, all.estimated_calls, all.uncosted_calls),
            (4, 1, 1)
        );
    }

    #[test]
    fn v10_upgrades_a_v9_log_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.db");
        {
            let lib = Library::open(&path).unwrap();
            lib.insert_llm_call(&call(1, Some("m1"))).unwrap();
            lib.conn
                .execute_batch(&format!(
                    "{}{}ALTER TABLE llm_call DROP COLUMN cache_read_tokens;
                     ALTER TABLE llm_call DROP COLUMN cache_write_tokens;
                     ALTER TABLE llm_call DROP COLUMN cost_usd;
                     ALTER TABLE llm_call DROP COLUMN model_reported;
                     DROP TABLE prompt_run;
                     DROP TABLE prompt;
                     DROP TABLE project_item;
                     PRAGMA user_version = 9;",
                    crate::store::DROP_V14,
                    crate::store::DROP_V13
                ))
                .unwrap();
        }
        let lib = Library::open(&path).unwrap();
        let old = &lib.llm_calls(None, 10).unwrap()[0];
        assert_eq!(old.tokens_in, 12, "an old row keeps its tokens");
        assert_eq!((old.cache_read_tokens, old.cache_write_tokens), (0, 0));
        assert_eq!((old.cost_usd, old.model_reported.as_deref()), (None, None));
        lib.insert_llm_call(&claude_call(2, None, 0.02)).unwrap();
        assert_eq!(lib.llm_call_totals(None).unwrap().uncosted_calls, 1);
        let version: i64 = lib
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 14);
    }

    #[test]
    fn calls_round_trip_newest_first_filter_and_clear() {
        let lib = Library::open_in_memory().unwrap();
        lib.insert_llm_call(&call(1, Some("m1"))).unwrap();
        lib.insert_llm_call(&call(3, None)).unwrap();
        let id = lib.insert_llm_call(&call(2, Some("m2"))).unwrap();

        let all = lib.llm_calls(None, 10).unwrap();
        assert_eq!(
            all.iter().map(|c| c.at_ms).collect::<Vec<_>>(),
            [3, 2, 1],
            "newest first"
        );
        assert_eq!(
            all[1],
            LlmCall {
                id,
                ..call(2, Some("m2"))
            }
        );
        assert_eq!(lib.llm_calls(None, 1).unwrap().len(), 1);
        let m1 = lib.llm_calls(Some("m1"), 10).unwrap();
        assert_eq!(m1.len(), 1);
        assert_eq!(m1[0].meeting_id.as_deref(), Some("m1"));

        assert_eq!(lib.clear_llm_calls().unwrap(), 3);
        assert!(lib.llm_calls(None, 10).unwrap().is_empty());
    }
}
