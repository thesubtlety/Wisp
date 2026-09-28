//! Finding the user's CLI. An app started from the macOS Finder gets a PATH of only
//! `/usr/bin:/bin:/usr/sbin:/sbin`, which misses where `codex` and `claude` are usually installed
//! (Homebrew, npm or nvm, `~/.local/bin`). The npm-installed CLIs are also `#!/usr/bin/env node`
//! scripts, so the child needs a PATH that finds `node` as well. This widens the search to those
//! places, for locating the program and for the child's own PATH.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Directories to search: the inherited PATH first, then the usual install locations under
/// `home`. Only existing directories, each once, in that order.
pub fn search_dirs(inherited: Option<OsString>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = inherited
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ]);
    if let Some(home) = home {
        for rel in [
            ".local/bin",
            ".claude/local",
            ".npm-global/bin",
            ".volta/bin",
            ".bun/bin",
        ] {
            dirs.push(home.join(rel));
        }
        // nvm: newest installed node first.
        if let Ok(entries) = std::fs::read_dir(home.join(".nvm/versions/node")) {
            let mut versions: Vec<PathBuf> =
                entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            versions.sort_by_key(|p| std::cmp::Reverse(node_version(p)));
            dirs.extend(versions.into_iter().map(|v| v.join("bin")));
        }
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for d in dirs {
        if d.is_dir() && !out.contains(&d) {
            out.push(d);
        }
    }
    out
}

/// `v20.11.1` → (20, 11, 1); anything else sorts first.
fn node_version(path: &Path) -> (u32, u32, u32) {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let mut parts = name
        .trim_start_matches('v')
        .split('.')
        .map(|p| p.parse().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

/// The first executable file called `name` in `dirs`. A name with a path separator is returned as
/// is: it was configured explicitly.
pub fn find_program(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    if name.contains('/') || name.contains('\\') {
        return Some(PathBuf::from(name));
    }
    dirs.iter().map(|d| d.join(name)).find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// A command for the CLI `program`: the program resolved against the widened search, and the child's
/// PATH set to that search, so an `env node` shebang works from a Finder-launched app.
pub(crate) fn cli_spec(program: &str) -> crate::runner::CommandSpec {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let dirs = search_dirs(std::env::var_os("PATH"), home.as_deref());
    let resolved = find_program(program, &dirs)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| program.to_owned());
    let mut spec = crate::runner::CommandSpec::new(resolved);
    if let Ok(path) = std::env::join_paths(&dirs) {
        spec.env_set
            .push(("PATH".to_owned(), path.to_string_lossy().into_owned()));
    }
    spec
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    #[test]
    fn a_finder_path_is_widened_to_user_install_locations() {
        let home = tempfile::tempdir().unwrap();
        let local = home.path().join(".local/bin");
        std::fs::create_dir_all(&local).unwrap();
        for v in ["v18.19.0", "v20.11.1", "v9.0.0"] {
            std::fs::create_dir_all(home.path().join(".nvm/versions/node").join(v).join("bin"))
                .unwrap();
        }
        let dirs = search_dirs(
            Some(OsString::from("/usr/bin:/bin:/usr/bin")),
            Some(home.path()),
        );
        assert_eq!(dirs[0], PathBuf::from("/usr/bin"));
        assert_eq!(
            dirs.iter().filter(|d| **d == Path::new("/usr/bin")).count(),
            1
        );
        assert!(dirs.contains(&local));
        let nvm: Vec<String> = dirs
            .iter()
            .filter_map(|d| d.to_str())
            .filter(|d| d.contains(".nvm"))
            .map(|d| d.rsplit('/').nth(1).unwrap().to_owned())
            .collect();
        assert_eq!(nvm, ["v20.11.1", "v18.19.0", "v9.0.0"]);
        assert!(
            !dirs.iter().any(|d| d.ends_with(".volta/bin")),
            "missing dirs are skipped"
        );
    }

    #[cfg(unix)]
    #[test]
    fn programs_resolve_to_the_first_executable_match() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("codex"), "not executable").unwrap();
        let real = exe(b.path(), "codex");
        let dirs = vec![a.path().to_path_buf(), b.path().to_path_buf()];
        assert_eq!(find_program("codex", &dirs), Some(real));
        assert_eq!(find_program("claude", &dirs), None);
        assert_eq!(
            find_program("/opt/tools/codex", &dirs),
            Some(PathBuf::from("/opt/tools/codex"))
        );
    }

    #[test]
    fn cli_spec_sets_the_child_path() {
        let spec = cli_spec("definitely-not-a-cli-xyz");
        assert_eq!(spec.program, "definitely-not-a-cli-xyz");
        assert!(spec
            .env_set
            .iter()
            .any(|(k, v)| k == "PATH" && !v.is_empty()));
    }
}
