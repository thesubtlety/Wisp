//! Indexes a folder into a throwaway project and prints what retrieval returns for a query.
//! Full-text only (no embedding model), in memory unless `--db` is given; the folder is only read.
//!
//!   cargo run -p wisp-ingest --example ingest_probe -- <folder> "<query>" [--db <path>]

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use wisp_ingest::{ingest_folder, Limits};
use wisp_library::{Library, RetrievalQuery, SnippetOrigin};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (folder, query) = match args.as_slice() {
        [f, q, ..] => (PathBuf::from(f), q.clone()),
        _ => {
            eprintln!("usage: ingest_probe <folder> \"<query>\" [--db <path>]");
            std::process::exit(2);
        }
    };
    let db = args
        .windows(2)
        .find(|w| w[0] == "--db")
        .map(|w| PathBuf::from(&w[1]));
    let mut lib = match &db {
        Some(path) => Library::open(path)?,
        None => Library::open_in_memory()?,
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;
    if !lib.list_projects()?.iter().any(|p| p.id == "probe") {
        lib.create_project("probe", "Probe", now)?;
    }

    let report = ingest_folder(&mut lib, "probe", &folder, now, &Limits::default())?;
    println!(
        "added {} · updated {} · unchanged {} · removed {} · skipped {}",
        report.added,
        report.updated,
        report.unchanged,
        report.removed,
        report.skipped.len()
    );
    for s in report.skipped.iter().take(20) {
        println!("  skipped {}: {}", s.path.display(), s.reason);
    }

    let hits = lib.retrieve(&RetrievalQuery {
        text: &query,
        project_id: Some("probe"),
        include_meetings: false,
        exclude_meeting_id: None,
        limit: 8,
        max_total_chars: 4000,
    })?;
    println!("\n{} snippet(s) for {query:?}:", hits.len());
    for hit in hits {
        let place = match &hit.origin {
            SnippetOrigin::Source {
                label, line_start, ..
            } => format!("{label}:{}", line_start.unwrap_or(0)),
            SnippetOrigin::Meeting { title, .. } => title.clone(),
        };
        println!("\n[{}] {place}\n{}", hit.ref_id, hit.text);
    }
    Ok(())
}
