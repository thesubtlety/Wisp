//! Project ingestion for Wisp: turns local files into project sources in the [`wisp_library`].
//!
//! - [`ingest_folder`] indexes a folder the user picked, in place. Files are read, never written;
//!   a file that changed is re-indexed and one that disappeared is dropped from the index.
//! - [`import_file`] copies one file into the app's managed directory and indexes the copy, which
//!   then expires with the retention policy.
//! - Pasted text needs no help: pass a [`SourceInput`] of kind [`SourceKind::Pasted`] straight to
//!   [`Library::add_source`].
//!
//! Scans are conservative: symlinks are never followed, hidden entries and build/dependency folders
//! are skipped, and file counts, depth and sizes are capped by [`Limits`]. Only extracted text
//! reaches the library, and only small retrieved snippets ever reach a reasoning backend.

mod extract;

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

pub use extract::{extract, format_of, Format};
use wisp_library::{Library, LibraryError, SourceInput, SourceKind, Upsert};

/// Directory names never descended into.
const IGNORED_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "build",
    "dist",
    "out",
    "vendor",
    "venv",
    "__pycache__",
    "Pods",
    "DerivedData",
];

/// Bounds on what a scan or an extraction will read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Largest plain-text file read, in bytes.
    pub max_file_bytes: u64,
    /// Largest `.docx` archive opened, in bytes.
    pub max_docx_bytes: u64,
    /// Most bytes of `.docx` body XML inflated.
    pub max_docx_xml_bytes: u64,
    /// Most files a folder scan indexes.
    pub max_files: usize,
    /// Deepest folder level a scan descends to (the picked folder is level 0).
    pub max_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: 5 * 1024 * 1024,
            max_docx_bytes: 25 * 1024 * 1024,
            max_docx_xml_bytes: 20 * 1024 * 1024,
            max_files: 5_000,
            max_depth: 12,
        }
    }
}

/// Why a file was not indexed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// Not a format the app reads.
    Unsupported,
    /// A PDF; not read yet.
    PdfNotSupported,
    /// Over the size limit (bytes).
    TooLarge(u64),
    /// Binary, not UTF-8, or a broken document.
    NotText,
    /// No text in it.
    Empty,
    /// A symbolic link; never followed.
    Symlink,
    /// Could not be read.
    Unreadable(String),
    /// The scan's file limit was reached before this point; the rest of the folder was not read.
    FileLimitReached,
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Skip::Unsupported => write!(f, "unsupported file type"),
            Skip::PdfNotSupported => write!(f, "PDF is not supported yet"),
            Skip::TooLarge(bytes) => write!(f, "too large ({bytes} bytes)"),
            Skip::NotText => write!(f, "not readable text"),
            Skip::Empty => write!(f, "no text"),
            Skip::Symlink => write!(f, "symbolic link (not followed)"),
            Skip::Unreadable(e) => write!(f, "unreadable: {e}"),
            Skip::FileLimitReached => write!(f, "file limit reached"),
        }
    }
}

/// A file that was not indexed, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: Skip,
}

/// What a folder scan found.
#[derive(Debug, Default)]
pub struct Scan {
    /// Files in a readable format, in a stable order.
    pub files: Vec<PathBuf>,
    pub skipped: Vec<Skipped>,
    /// Whether [`Limits::max_files`] cut the scan short.
    pub truncated: bool,
}

/// What [`ingest_folder`] did.
#[derive(Debug, Default)]
pub struct IngestReport {
    pub added: usize,
    pub updated: usize,
    pub unchanged: usize,
    /// Sources dropped because their file is gone or no longer readable.
    pub removed: usize,
    pub skipped: Vec<Skipped>,
}

/// An ingestion failure.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error(transparent)]
    Library(#[from] LibraryError),
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("not imported: {0}")]
    Skipped(Skip),
}

/// Lists the readable files under `root`. Symlinks are recorded as skipped and never followed;
/// hidden entries and [`IGNORED_DIRS`] are passed over silently; folders deeper than
/// [`Limits::max_depth`] are not entered; a subfolder that can't be listed is recorded as skipped.
/// Fails only if `root` itself can't be listed.
pub fn scan_folder(root: &Path, limits: &Limits) -> io::Result<Scan> {
    let mut scan = Scan::default();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let read = match fs::read_dir(&dir) {
            Ok(read) => read,
            Err(e) if depth == 0 => return Err(e),
            Err(e) => {
                scan.skipped.push(Skipped {
                    path: dir,
                    reason: Skip::Unreadable(e.to_string()),
                });
                continue;
            }
        };
        let mut entries: Vec<_> = read.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());
        let mut subdirs = Vec::new();
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                scan.skipped.push(Skipped {
                    path,
                    reason: Skip::Symlink,
                });
            } else if meta.is_dir() {
                if depth < limits.max_depth && !IGNORED_DIRS.contains(&name.as_ref()) {
                    subdirs.push((path, depth + 1));
                }
            } else if meta.is_file() {
                match format_of(&path) {
                    Ok(_) if scan.files.len() >= limits.max_files => {
                        scan.truncated = true;
                        scan.skipped.push(Skipped {
                            path: root.to_path_buf(),
                            reason: Skip::FileLimitReached,
                        });
                        return Ok(scan);
                    }
                    Ok(_) => scan.files.push(path),
                    Err(reason) => scan.skipped.push(Skipped { path, reason }),
                }
            }
        }
        // Reverse so the stack pops subfolders in name order.
        stack.extend(subdirs.into_iter().rev());
    }
    Ok(scan)
}

/// Indexes the readable files under `root` into `project_id`, where they live. Unchanged files are
/// left alone, changed ones re-indexed. Sources previously indexed from under `root` whose file is
/// gone or no longer readable are removed from the index (the files themselves are never touched),
/// unless the scan was cut short by the file limit.
pub fn ingest_folder(
    library: &mut Library,
    project_id: &str,
    root: &Path,
    now_ms: i64,
    limits: &Limits,
) -> Result<IngestReport, IngestError> {
    let root = root.canonicalize()?;
    let scan = scan_folder(&root, limits)?;
    let mut report = IngestReport {
        skipped: scan.skipped,
        ..IngestReport::default()
    };
    let mut indexed: HashSet<String> = HashSet::new();
    for path in scan.files {
        let text = match extract(&path, limits) {
            Ok(text) => text,
            Err(reason) => {
                report.skipped.push(Skipped { path, reason });
                continue;
            }
        };
        let label = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .display()
            .to_string();
        let input = SourceInput {
            kind: SourceKind::File,
            label,
            text,
            origin_path: Some(path.clone()),
            managed_path: None,
            added_at_ms: now_ms,
        };
        match library.upsert_file_source(project_id, &input)? {
            Upsert::Added(_) => report.added += 1,
            Upsert::Updated(_) => report.updated += 1,
            Upsert::Unchanged(_) => report.unchanged += 1,
        }
        indexed.insert(path.display().to_string());
    }

    if !scan.truncated {
        for source in library.list_sources(project_id)? {
            let Some(origin) = &source.origin_path else {
                continue;
            };
            let stale = source.kind == "file"
                && source.managed_path.is_none()
                && Path::new(origin).starts_with(&root)
                && !indexed.contains(origin);
            if stale {
                // A file source has no managed copy, so nothing on disk is deleted.
                library.remove_source(source.id, &root)?;
                report.removed += 1;
            }
        }
    }
    Ok(report)
}

/// Copies `path` into `managed_root` and indexes the copy in `project_id` as a managed source,
/// which expires under the retention policy (taking the copy with it). The original is only read.
/// The copy is owner-only. Returns the source id.
pub fn import_file(
    library: &mut Library,
    project_id: &str,
    path: &Path,
    managed_root: &Path,
    now_ms: i64,
    limits: &Limits,
) -> Result<i64, IngestError> {
    let text = extract(path, limits).map_err(IngestError::Skipped)?;
    create_private_dir(managed_root)?;
    let name = path
        .file_name()
        .map(|n| safe_file_name(&n.to_string_lossy()))
        .unwrap_or_else(|| "import".to_owned());

    let mut n = 0;
    let (copy, mut dest) = loop {
        let candidate = managed_root.join(match n {
            0 => format!("{now_ms}-{name}"),
            _ => format!("{now_ms}-{n}-{name}"),
        });
        match create_private_file(&candidate) {
            Ok(file) => break (candidate, file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && n < 1000 => n += 1,
            Err(e) => return Err(e.into()),
        }
    };
    let copied = fs::File::open(path).and_then(|mut src| io::copy(&mut src, &mut dest));
    drop(dest);
    if let Err(e) = copied {
        let _ = fs::remove_file(&copy);
        return Err(e.into());
    }

    let input = SourceInput {
        kind: SourceKind::Managed,
        label: name,
        text,
        origin_path: None,
        managed_path: Some(copy.clone()),
        added_at_ms: now_ms,
    };
    library.add_source(project_id, &input).map_err(|e| {
        let _ = fs::remove_file(&copy);
        e.into()
    })
}

/// A file name with path separators and control characters replaced, never empty or a dot name.
fn safe_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == ':' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim_start_matches('.').trim();
    if cleaned.is_empty() {
        "import".to_owned()
    } else {
        cleaned.chars().take(120).collect()
    }
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn create_private_file(path: &Path) -> io::Result<fs::File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wisp_library::RetrievalQuery;

    const T0: i64 = 1_700_000_000_000;

    fn write(root: &Path, rel: &str, text: &str) -> PathBuf {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        path
    }

    fn library() -> Library {
        let lib = Library::open_in_memory().unwrap();
        lib.create_project("p", "Acme", T0).unwrap();
        lib
    }

    fn labels(lib: &Library) -> Vec<String> {
        let mut l: Vec<String> = lib
            .list_sources("p")
            .unwrap()
            .into_iter()
            .map(|s| s.label)
            .collect();
        l.sort();
        l
    }

    fn rel(skipped: &[Skipped], root: &Path) -> Vec<(String, Skip)> {
        skipped
            .iter()
            .map(|s| {
                (
                    s.path
                        .strip_prefix(root)
                        .unwrap_or(&s.path)
                        .display()
                        .to_string(),
                    s.reason.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn scans_skip_hidden_ignored_deep_and_unsupported_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "notes.md", "a");
        write(root, "sub/spec.txt", "b");
        write(root, "sub/image.png", "c");
        write(root, "sub/deck.pdf", "d");
        write(root, ".git/config", "e");
        write(root, ".env", "f");
        write(root, "node_modules/x/readme.md", "g");
        write(root, "a/b/c/deep.md", "h");
        let limits = Limits {
            max_depth: 2,
            ..Limits::default()
        };
        let scan = scan_folder(root, &limits).unwrap();
        let files: Vec<String> = scan
            .files
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().display().to_string())
            .collect();
        assert_eq!(files, ["notes.md", "sub/spec.txt"]);
        assert_eq!(
            rel(&scan.skipped, root),
            [
                ("sub/deck.pdf".to_owned(), Skip::PdfNotSupported),
                ("sub/image.png".to_owned(), Skip::Unsupported),
            ]
        );
        assert!(!scan.truncated);
    }

    #[cfg(unix)]
    #[test]
    fn scans_never_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "secret.txt", "private key material");
        write(dir.path(), "ok.txt", "fine");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("linked-dir")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("linked.txt"),
        )
        .unwrap();
        let scan = scan_folder(dir.path(), &Limits::default()).unwrap();
        assert_eq!(scan.files, [dir.path().join("ok.txt")]);
        assert_eq!(
            rel(&scan.skipped, dir.path()),
            [
                ("linked-dir".to_owned(), Skip::Symlink),
                ("linked.txt".to_owned(), Skip::Symlink),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unlistable_subfolder_is_skipped_not_fatal() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "ok.txt", "fine");
        write(dir.path(), "locked/inside.txt", "hidden");
        let locked = dir.path().join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let listable = fs::read_dir(&locked).is_ok(); // root ignores permissions
        let scan = scan_folder(dir.path(), &Limits::default());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        let scan = scan.unwrap();
        assert_eq!(scan.files[0], dir.path().join("ok.txt"));
        if !listable {
            assert_eq!(scan.files.len(), 1);
            assert!(matches!(
                scan.skipped.as_slice(),
                [Skipped {
                    reason: Skip::Unreadable(_),
                    ..
                }]
            ));
        }
        assert!(scan_folder(&dir.path().join("missing"), &Limits::default()).is_err());
    }

    #[test]
    fn the_file_limit_stops_the_scan_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            write(dir.path(), &format!("f{i}.txt"), "x");
        }
        let limits = Limits {
            max_files: 3,
            ..Limits::default()
        };
        let scan = scan_folder(dir.path(), &limits).unwrap();
        assert_eq!(scan.files.len(), 3);
        assert!(scan.truncated);
        assert_eq!(scan.skipped.last().unwrap().reason, Skip::FileLimitReached);
    }

    #[test]
    fn ingesting_a_folder_indexes_refreshes_and_drops_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "hosting.md",
            "Production runs in the customer's Azure tenant.",
        );
        write(root, "auth/sso.txt", "Login goes through SAML.");
        write(root, "empty.txt", "   ");
        let mut lib = library();

        let r = ingest_folder(&mut lib, "p", root, T0, &Limits::default()).unwrap();
        assert_eq!((r.added, r.updated, r.unchanged, r.removed), (2, 0, 0, 0));
        assert_eq!(
            rel(&r.skipped, &root.canonicalize().unwrap()),
            [("empty.txt".to_owned(), Skip::Empty)]
        );
        assert_eq!(labels(&lib), ["auth/sso.txt", "hosting.md"]);
        let src = lib.list_sources("p").unwrap();
        assert!(src
            .iter()
            .all(|s| s.kind == "file" && s.expires_at_ms.is_none()));

        let r = ingest_folder(&mut lib, "p", root, T0 + 1, &Limits::default()).unwrap();
        assert_eq!((r.added, r.updated, r.unchanged, r.removed), (0, 0, 2, 0));

        write(root, "auth/sso.txt", "Login goes through OIDC now.");
        fs::remove_file(root.join("hosting.md")).unwrap();
        let r = ingest_folder(&mut lib, "p", root, T0 + 2, &Limits::default()).unwrap();
        assert_eq!((r.added, r.updated, r.unchanged, r.removed), (0, 1, 0, 1));
        assert_eq!(labels(&lib), ["auth/sso.txt"]);

        let hits = lib
            .retrieve(&RetrievalQuery {
                text: "oidc login",
                project_id: Some("p"),
                include_meetings: false,
                exclude_meeting_id: None,
                limit: 5,
                max_total_chars: 2000,
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "Login goes through OIDC now.");
        // The user's files are never modified or deleted.
        assert!(root.join("auth/sso.txt").exists());
    }

    #[test]
    fn ingesting_one_folder_leaves_sources_from_elsewhere_alone() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        write(a.path(), "a.md", "alpha");
        write(b.path(), "b.md", "beta");
        let mut lib = library();
        ingest_folder(&mut lib, "p", a.path(), T0, &Limits::default()).unwrap();
        lib.add_source(
            "p",
            &SourceInput {
                kind: SourceKind::Pasted,
                label: "pasted".into(),
                text: "gamma".into(),
                origin_path: None,
                managed_path: None,
                added_at_ms: T0,
            },
        )
        .unwrap();
        let r = ingest_folder(&mut lib, "p", b.path(), T0, &Limits::default()).unwrap();
        assert_eq!((r.added, r.removed), (1, 0));
        assert_eq!(labels(&lib), ["a.md", "b.md", "pasted"]);
    }

    #[test]
    fn a_truncated_scan_removes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..4 {
            write(dir.path(), &format!("f{i}.txt"), &format!("file {i}"));
        }
        let mut lib = library();
        ingest_folder(&mut lib, "p", dir.path(), T0, &Limits::default()).unwrap();
        let tight = Limits {
            max_files: 2,
            ..Limits::default()
        };
        let r = ingest_folder(&mut lib, "p", dir.path(), T0, &tight).unwrap();
        assert_eq!((r.unchanged, r.removed), (2, 0));
        assert_eq!(lib.list_sources("p").unwrap().len(), 4);
    }

    #[test]
    fn importing_copies_into_the_managed_dir_and_prune_takes_the_copy() {
        let src_dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let managed = data.path().join("managed-sources");
        let original = write(
            src_dir.path(),
            "Security Reqs.md",
            "The tenant must stay in Azure.",
        );
        let mut lib = library();

        let id = import_file(&mut lib, "p", &original, &managed, T0, &Limits::default()).unwrap();
        let again =
            import_file(&mut lib, "p", &original, &managed, T0, &Limits::default()).unwrap();
        let sources = lib.list_sources("p").unwrap();
        let source = sources.iter().find(|s| s.id == id).unwrap();
        let copy = PathBuf::from(source.managed_path.clone().unwrap());
        assert_eq!(source.kind, "managed");
        assert!(source.expires_at_ms.is_some());
        assert!(copy.starts_with(&managed));
        assert_eq!(
            fs::read_to_string(&copy).unwrap(),
            "The tenant must stay in Azure."
        );
        let other = sources.iter().find(|s| s.id == again).unwrap();
        assert_ne!(
            other.managed_path, source.managed_path,
            "a second import never overwrites"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&managed).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }

        let report = lib.prune(i64::MAX, &managed).unwrap();
        assert_eq!((report.sources_expired, report.files_deleted), (2, 2));
        assert!(!copy.exists());
        assert!(original.exists(), "the original is never touched");
    }

    #[test]
    fn importing_an_unreadable_file_leaves_no_copy() {
        let src_dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let managed = data.path().join("managed");
        let png = write(src_dir.path(), "shot.png", "not text");
        let mut lib = library();
        let err = import_file(&mut lib, "p", &png, &managed, T0, &Limits::default()).unwrap_err();
        assert!(matches!(err, IngestError::Skipped(Skip::Unsupported)));
        assert!(!managed.exists() || fs::read_dir(&managed).unwrap().next().is_none());

        // A library failure (unknown project) removes the copy it made.
        let txt = write(src_dir.path(), "notes.txt", "text");
        assert!(import_file(&mut lib, "nope", &txt, &managed, T0, &Limits::default()).is_err());
        assert_eq!(fs::read_dir(&managed).unwrap().count(), 0);
    }

    #[test]
    fn managed_file_names_are_sanitized() {
        assert_eq!(safe_file_name("a/b\\c:d.md"), "a_b_c_d.md");
        assert_eq!(safe_file_name("..hidden"), "hidden");
        assert_eq!(safe_file_name("..."), "import");
        assert_eq!(safe_file_name("x\u{7}y"), "x_y");
        assert_eq!(safe_file_name(&"n".repeat(300)).len(), 120);
    }
}
