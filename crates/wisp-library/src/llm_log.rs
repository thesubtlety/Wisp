//! The AI activity log: one row per model call, with everything sent and received (see
//! `SCHEMA_V8`). It holds transcript text, so it lives as long as transcripts do: a call made for
//! a meeting goes when that meeting's transcript expires or the meeting is deleted, and any other
//! call expires by its own time under the same retention.

use serde::{Deserialize, Serialize};

use crate::store::Library;
use crate::Result;

/// One logged model call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub tokens_estimated: bool,
}

const COLUMNS: &str = "id, at_ms, meeting_id, task, backend, model, local, instructions, context, \
                       images, output, error, elapsed_ms, tokens_in, tokens_out, tokens_estimated";

impl Library {
    /// Logs one call. Returns its row id.
    pub fn insert_llm_call(&self, call: &LlmCall) -> Result<i64> {
        let images = serde_json::to_string(&call.images).unwrap_or_else(|_| "[]".into());
        self.conn.execute(
            "INSERT INTO llm_call (at_ms, meeting_id, task, backend, model, local, instructions,
                 context, images, output, error, elapsed_ms, tokens_in, tokens_out, tokens_estimated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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
        }
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
