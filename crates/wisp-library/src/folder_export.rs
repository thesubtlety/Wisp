//! Folder export: a project's meetings written as files into a folder the user picked, so other
//! apps can read them. The app renders each artifact; this module names the files, records what it
//! wrote (the path and content hash per meeting and kind), and applies one rule when writing again:
//! a file is overwritten or removed only while it still matches that record. A file changed by the
//! user or another app, or one Wisp never wrote, is left alone and reported.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension;
use serde::Serialize;
use sha2::{Digest, Sha256};
use wisp_core::localtime::LocalTime;

use crate::retention::hex;
use crate::store::Library;
use crate::Result;

/// The artifacts a meeting can be exported as, in the order they are offered: the Markdown summary,
/// the transcript, the meeting record, the AI context packet, and the state as JSON.
pub const EXPORT_KINDS: [&str; 5] = ["summary", "transcript", "record", "packet", "json"];

/// Most characters of the title in a file name.
const TITLE_CHARS: usize = 80;

/// The file name for `kind`, from the meeting's stem (see [`file_stem`]): the summary is the bare
/// `<stem>.md`, the others say what they are. `None` for an unknown kind.
pub fn export_file_name(stem: &str, kind: &str) -> Option<String> {
    let suffix = match kind {
        "summary" => ".md",
        "transcript" => ".transcript.md",
        "record" => ".record.md",
        "packet" => ".context.md",
        "json" => ".state.json",
        _ => return None,
    };
    Some(format!("{stem}{suffix}"))
}

/// `YYYY-MM-DD HHMM <title>`: the meeting's local start, then its title made safe for a file name
/// (path separators and the characters Windows forbids dropped, whitespace collapsed, capped at
/// [`TITLE_CHARS`]). A title with nothing left becomes "Meeting". The time keeps two meetings of
/// one day apart.
pub fn file_stem(started: &LocalTime, title: &str) -> String {
    let kept: String = title
        .chars()
        .filter(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .filter(|c| c.is_whitespace() || !c.is_control())
        .collect();
    let mut safe = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    if safe.chars().count() > TITLE_CHARS {
        safe = safe.chars().take(TITLE_CHARS).collect();
    }
    let safe = safe.trim().trim_matches('.').trim();
    let safe = if safe.is_empty() { "Meeting" } else { safe };
    format!("{} {safe}", started.file_stamp())
}

/// A YAML front matter block for a Markdown export: `title`, `date`, `project` (when the meeting
/// has one) and `meeting_id`, every value double-quoted so another app can parse it.
pub fn front_matter(title: &str, date: &str, project: Option<&str>, meeting_id: &str) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("title: {}\n", yaml_quote(title)));
    out.push_str(&format!("date: {}\n", yaml_quote(date)));
    if let Some(project) = project {
        out.push_str(&format!("project: {}\n", yaml_quote(project)));
    }
    out.push_str(&format!("meeting_id: {}\n", yaml_quote(meeting_id)));
    out.push_str("---\n\n");
    out
}

fn yaml_quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One file Wisp wrote for a meeting, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportFile {
    pub meeting_id: String,
    pub kind: String,
    pub path: PathBuf,
    /// Hex SHA-256 of the content written.
    pub sha256: String,
    pub written_at_ms: i64,
}

/// What one meeting's file of one kind should become.
#[derive(Debug, Clone, Copy)]
pub enum ExportTarget<'a> {
    /// Write `content` at the path, unless the file there isn't Wisp's own unchanged one.
    Write(&'a Path, &'a str),
    /// Nothing to write right now (no summary yet, transcript expired): leave whatever is there.
    Keep,
    /// No file is wanted any more (kind off, folder cleared, meeting deleted): remove Wisp's own
    /// unchanged file and forget it.
    Retire,
}

/// What a sync did, for the UI: files written (new or changed), left as they were, removed, and
/// those left alone because they were changed outside Wisp or were never Wisp's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncReport {
    pub written: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub skipped: Vec<PathBuf>,
}

impl SyncReport {
    /// Folds another report into this one.
    pub fn extend(&mut self, other: SyncReport) {
        self.written += other.written;
        self.unchanged += other.unchanged;
        self.removed += other.removed;
        self.skipped.extend(other.skipped);
    }
}

impl Library {
    /// The folder a project's meetings are exported to, if one was picked (`None` too when the
    /// project doesn't exist).
    pub fn project_export_dir(&self, project_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT export_dir FROM project WHERE id = ?1",
                [project_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Sets (or with `None` or blank, clears) a project's export folder. Returns whether the
    /// project exists.
    pub fn set_project_export_dir(&self, project_id: &str, dir: Option<&str>) -> Result<bool> {
        let dir = dir.map(str::trim).filter(|d| !d.is_empty());
        let n = self.conn.execute(
            "UPDATE project SET export_dir = ?2 WHERE id = ?1",
            rusqlite::params![project_id, dir],
        )?;
        Ok(n > 0)
    }

    /// The files recorded as written for a meeting, by kind.
    pub fn export_files(&self, meeting_id: &str) -> Result<Vec<ExportFile>> {
        let mut stmt = self.conn.prepare(
            "SELECT kind, path, sha256, written_at_ms FROM export_file
             WHERE meeting_id = ?1 ORDER BY kind",
        )?;
        let rows = stmt
            .query_map([meeting_id], |r| {
                Ok(ExportFile {
                    meeting_id: meeting_id.to_owned(),
                    kind: r.get(0)?,
                    path: PathBuf::from(r.get::<_, String>(1)?),
                    sha256: r.get(2)?,
                    written_at_ms: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn export_file(&self, meeting_id: &str, kind: &str) -> Result<Option<ExportFile>> {
        Ok(self
            .export_files(meeting_id)?
            .into_iter()
            .find(|f| f.kind == kind))
    }

    fn record_export_file(
        &self,
        meeting_id: &str,
        kind: &str,
        path: &Path,
        sha256: &str,
        now_ms: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO export_file (meeting_id, kind, path, sha256, written_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![meeting_id, kind, path.display().to_string(), sha256, now_ms],
        )?;
        Ok(())
    }

    fn forget_export_file(&self, meeting_id: &str, kind: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM export_file WHERE meeting_id = ?1 AND kind = ?2",
            rusqlite::params![meeting_id, kind],
        )?;
        Ok(())
    }

    /// Brings one meeting's file of `kind` in line with `target`, under the one rule: a file is
    /// overwritten or removed only while it still matches what Wisp recorded writing there. When
    /// the path changed (a rename, a move to another project), the old file is retired first. A
    /// file with the same content as what would be written is simply recorded as Wisp's.
    pub fn sync_export_file(
        &self,
        meeting_id: &str,
        kind: &str,
        target: ExportTarget<'_>,
        now_ms: i64,
    ) -> Result<SyncReport> {
        let recorded = self.export_file(meeting_id, kind)?;
        let (path, content) = match target {
            ExportTarget::Keep => return Ok(SyncReport::default()),
            ExportTarget::Retire => {
                return match recorded {
                    None => Ok(SyncReport::default()),
                    Some(old) => {
                        self.forget_export_file(meeting_id, kind)?;
                        retire(&old)
                    }
                };
            }
            ExportTarget::Write(path, content) => (path, content),
        };
        let mut report = SyncReport::default();
        if let Some(old) = recorded.as_ref().filter(|r| r.path != path) {
            report.extend(retire(old)?);
        }
        let ours = recorded
            .as_ref()
            .filter(|r| r.path == path)
            .map(|r| r.sha256.as_str());
        let hash = hex(&Sha256::digest(content.as_bytes()));
        match fs::read(path) {
            Ok(bytes) => {
                let on_disk = hex(&Sha256::digest(&bytes));
                if on_disk == hash {
                    self.record_export_file(meeting_id, kind, path, &hash, now_ms)?;
                    report.unchanged += 1;
                    return Ok(report);
                }
                if ours != Some(on_disk.as_str()) {
                    report.skipped.push(path.to_path_buf());
                    return Ok(report);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        write_atomic(path, content)?;
        self.record_export_file(meeting_id, kind, path, &hash, now_ms)?;
        report.written += 1;
        Ok(report)
    }
}

/// Removes Wisp's file at `old.path` if it still matches the record. An edited (or replaced) file
/// is left and reported as skipped; one already gone needs nothing.
fn retire(old: &ExportFile) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    match fs::read(&old.path) {
        Ok(bytes) if hex(&Sha256::digest(&bytes)) == old.sha256 => {
            fs::remove_file(&old.path)?;
            report.removed += 1;
        }
        Ok(_) => report.skipped.push(old.path.clone()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(report)
}

/// Writes through a temporary file in the same folder and renames it into place, so a reader never
/// sees a half-written file and a crash leaves the old one intact.
fn write_atomic(path: &Path, content: &str) -> io::Result<()> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidInput, what.to_owned());
    let dir = path
        .parent()
        .ok_or_else(|| bad("export path has no folder"))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| bad("export path has no file name"))?;
    let tmp = dir.join(format!(".{name}.wisp-tmp"));
    fs::write(&tmp, content)?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use wisp_core::export::MeetingMeta;
    use wisp_core::transcript::{AudioSourceKind, SegmentStatus, TranscriptSegment};

    use super::*;

    fn seg(id: u64, text: &str) -> TranscriptSegment {
        TranscriptSegment {
            id,
            text: text.into(),
            start: Duration::from_millis(id * 1000),
            end: Duration::from_millis(id * 1000 + 500),
            status: SegmentStatus::Final,
            source: AudioSourceKind::Microphone,
            speaker: None,
            confidence: None,
            words: Vec::new(),
            aux_text: None,
        }
    }

    fn library_with_meeting() -> Library {
        let mut lib = Library::open_in_memory().unwrap();
        lib.save_note("m1", &MeetingMeta::default(), 0, &[seg(0, "Hello.")])
            .unwrap();
        lib
    }

    /// 2026-10-28 12:04 UTC.
    fn noon() -> LocalTime {
        LocalTime::from_epoch_ms(1_793_189_040_000, 0)
    }

    #[test]
    fn the_file_names_follow_the_kind() {
        let names: Vec<String> = EXPORT_KINDS
            .iter()
            .map(|k| export_file_name("2026-10-28 1204 Kickoff", k).unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "2026-10-28 1204 Kickoff.md",
                "2026-10-28 1204 Kickoff.transcript.md",
                "2026-10-28 1204 Kickoff.record.md",
                "2026-10-28 1204 Kickoff.context.md",
                "2026-10-28 1204 Kickoff.state.json",
            ]
        );
        assert_eq!(export_file_name("x", "pdf"), None);
    }

    #[test]
    fn the_stem_is_the_local_start_and_a_safe_title() {
        assert_eq!(noon().display(), "2026-10-28 12:04");
        assert_eq!(
            file_stem(&noon(), "  Acme: kickoff / Q4?\tplan  "),
            "2026-10-28 1204 Acme kickoff Q4 plan"
        );
        assert_eq!(file_stem(&noon(), "..."), "2026-10-28 1204 Meeting");
        assert_eq!(file_stem(&noon(), ""), "2026-10-28 1204 Meeting");
        let long = "x".repeat(200);
        assert_eq!(
            file_stem(&noon(), &long).chars().count(),
            "2026-10-28 1204 ".len() + TITLE_CHARS
        );
    }

    #[test]
    fn front_matter_quotes_its_values() {
        assert_eq!(
            front_matter("Say \"hi\"", "2026-10-28 12:04", Some("Acme\\Beta"), "m1"),
            "---\ntitle: \"Say \\\"hi\\\"\"\ndate: \"2026-10-28 12:04\"\nproject: \"Acme\\\\Beta\"\n\
             meeting_id: \"m1\"\n---\n\n"
        );
        assert!(!front_matter("t", "d", None, "m").contains("project:"));
    }

    #[test]
    fn a_new_file_is_written_recorded_and_then_unchanged() {
        let lib = library_with_meeting();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        let r = lib
            .sync_export_file("m1", "summary", ExportTarget::Write(&path, "one"), 5)
            .unwrap();
        assert_eq!(r.written, 1);
        assert_eq!(fs::read_to_string(&path).unwrap(), "one");
        let files = lib.export_files("m1").unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!((files[0].kind.as_str(), &files[0].path), ("summary", &path));
        assert_eq!(files[0].written_at_ms, 5);
        assert!(!dir.path().join(".a.md.wisp-tmp").exists());

        let r = lib
            .sync_export_file("m1", "summary", ExportTarget::Write(&path, "one"), 6)
            .unwrap();
        assert_eq!((r.written, r.unchanged), (0, 1));
        let r = lib
            .sync_export_file("m1", "summary", ExportTarget::Write(&path, "two"), 7)
            .unwrap();
        assert_eq!(r.written, 1);
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
    }

    #[test]
    fn a_file_changed_outside_wisp_is_left_alone() {
        let lib = library_with_meeting();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        lib.sync_export_file("m1", "summary", ExportTarget::Write(&path, "one"), 1)
            .unwrap();
        fs::write(&path, "one, annotated by hand").unwrap();
        let r = lib
            .sync_export_file("m1", "summary", ExportTarget::Write(&path, "two"), 2)
            .unwrap();
        assert_eq!(r.written, 0);
        assert_eq!(r.skipped, vec![path.clone()]);
        assert_eq!(fs::read_to_string(&path).unwrap(), "one, annotated by hand");
        // Retiring leaves it too.
        let r = lib
            .sync_export_file("m1", "summary", ExportTarget::Retire, 3)
            .unwrap();
        assert_eq!((r.removed, r.skipped.len()), (0, 1));
        assert!(path.exists());
        assert!(lib.export_files("m1").unwrap().is_empty());
    }

    #[test]
    fn a_file_wisp_never_wrote_is_not_overwritten_unless_identical() {
        let lib = library_with_meeting();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        fs::write(&path, "theirs").unwrap();
        let r = lib
            .sync_export_file("m1", "summary", ExportTarget::Write(&path, "ours"), 1)
            .unwrap();
        assert_eq!(r.skipped, vec![path.clone()]);
        assert!(lib.export_files("m1").unwrap().is_empty());
        let r = lib
            .sync_export_file("m1", "summary", ExportTarget::Write(&path, "theirs"), 2)
            .unwrap();
        assert_eq!(r.unchanged, 1);
        assert_eq!(lib.export_files("m1").unwrap().len(), 1);
    }

    #[test]
    fn a_moved_target_retires_the_old_file_and_keep_touches_nothing() {
        let lib = library_with_meeting();
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.md");
        let new = dir.path().join("new.md");
        lib.sync_export_file("m1", "summary", ExportTarget::Write(&old, "one"), 1)
            .unwrap();
        let r = lib
            .sync_export_file("m1", "summary", ExportTarget::Keep, 2)
            .unwrap();
        assert_eq!(r, SyncReport::default());
        assert!(old.exists());
        let r = lib
            .sync_export_file("m1", "summary", ExportTarget::Write(&new, "one"), 3)
            .unwrap();
        assert_eq!((r.written, r.removed), (1, 1));
        assert!(!old.exists());
        assert_eq!(fs::read_to_string(&new).unwrap(), "one");
        assert_eq!(lib.export_files("m1").unwrap()[0].path, new);
    }

    #[test]
    fn retiring_removes_a_matching_file_and_a_missing_one_is_fine() {
        let lib = library_with_meeting();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        lib.sync_export_file("m1", "summary", ExportTarget::Write(&path, "one"), 1)
            .unwrap();
        let r = lib
            .sync_export_file("m1", "summary", ExportTarget::Retire, 2)
            .unwrap();
        assert_eq!(r.removed, 1);
        assert!(!path.exists());
        assert!(lib.export_files("m1").unwrap().is_empty());
        // Nothing recorded: nothing to do.
        let r = lib
            .sync_export_file("m1", "summary", ExportTarget::Retire, 3)
            .unwrap();
        assert_eq!(r, SyncReport::default());
        // Recorded but already deleted by the user: forgotten quietly.
        lib.sync_export_file("m1", "summary", ExportTarget::Write(&path, "one"), 4)
            .unwrap();
        fs::remove_file(&path).unwrap();
        let r = lib
            .sync_export_file("m1", "summary", ExportTarget::Retire, 5)
            .unwrap();
        assert_eq!(r, SyncReport::default());
    }

    #[test]
    fn the_export_dir_round_trips_and_records_go_with_the_meeting() {
        let lib = library_with_meeting();
        lib.create_project("p", "Acme", 0).unwrap();
        assert_eq!(lib.project_export_dir("p").unwrap(), None);
        assert!(lib
            .set_project_export_dir("p", Some(" /tmp/acme "))
            .unwrap());
        assert_eq!(
            lib.project_export_dir("p").unwrap().as_deref(),
            Some("/tmp/acme")
        );
        assert_eq!(
            lib.list_projects().unwrap()[0].export_dir.as_deref(),
            Some("/tmp/acme")
        );
        assert!(lib.set_project_export_dir("p", Some("  ")).unwrap());
        assert_eq!(lib.project_export_dir("p").unwrap(), None);
        assert!(!lib.set_project_export_dir("nope", Some("/x")).unwrap());
        assert_eq!(lib.project_export_dir("nope").unwrap(), None);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        lib.sync_export_file("m1", "summary", ExportTarget::Write(&path, "one"), 1)
            .unwrap();
        assert!(lib.delete_note("m1").unwrap());
        assert!(lib.export_files("m1").unwrap().is_empty());
    }

    #[test]
    fn a_v14_database_gains_the_export_folder_and_file_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.db");
        {
            let mut lib = Library::open(&path).unwrap();
            lib.save_note("m1", &MeetingMeta::default(), 0, &[seg(0, "Hello.")])
                .unwrap();
            lib.create_project("p", "Acme", 0).unwrap();
            lib.conn
                .execute_batch(&format!(
                    "{}PRAGMA user_version = 14;",
                    crate::store::DROP_V15
                ))
                .unwrap();
        }
        let lib = Library::open(&path).unwrap();
        let version: i64 = lib
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 15);
        assert_eq!(lib.project_export_dir("p").unwrap(), None);
        assert!(lib.set_project_export_dir("p", Some("/tmp/acme")).unwrap());
        assert!(lib.export_files("m1").unwrap().is_empty());
        assert_eq!(lib.count().unwrap(), 1);
    }
}
