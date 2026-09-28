//! Where Wisp keeps cloud API keys.
//!
//! The keys are one JSON map (provider id → key) held by a [`SecretStore`]. On macOS that's a
//! single generic-password item in the login keychain; elsewhere it's a file readable only by its
//! owner. [`load_keys`] also moves keys out of the old plain-text `cloud-keys.json` the first time
//! it runs, then deletes that file.
//!
//! One item for all keys, rather than one per provider, means an app whose code signature changes
//! (an ad-hoc signed build, rebuilt) asks for keychain access once, not once per provider.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Somewhere to keep one secret string.
pub trait SecretStore: Send + Sync {
    /// The stored value, or `None` when nothing has been stored yet.
    fn read(&self) -> Result<Option<String>, String>;
    /// Replaces the stored value.
    fn write(&self, value: &str) -> Result<(), String>;
    /// Where the value lives, for logs and errors.
    fn describe(&self) -> String;
}

/// A file only its owner can read or write (mode 0600 on Unix).
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl SecretStore for FileStore {
    fn read(&self) -> Result<Option<String>, String> {
        match fs::read_to_string(&self.path) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", self.path.display())),
        }
    }

    fn write(&self, value: &str) -> Result<(), String> {
        let err = |e: std::io::Error| format!("{}: {e}", self.path.display());
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&self.path).map_err(err)?;
        // `mode` only applies when the file is created; tighten one left by an older version.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(err)?;
        }
        file.write_all(value.as_bytes()).map_err(err)
    }

    fn describe(&self) -> String {
        self.path.display().to_string()
    }
}

/// One generic-password item in the macOS login keychain.
#[cfg(target_os = "macos")]
pub struct KeychainStore {
    service: String,
    account: String,
}

#[cfg(target_os = "macos")]
impl KeychainStore {
    pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            account: account.into(),
        }
    }
}

#[cfg(target_os = "macos")]
impl SecretStore for KeychainStore {
    fn read(&self) -> Result<Option<String>, String> {
        use security_framework::passwords::get_generic_password;
        /// `errSecItemNotFound`.
        const NOT_FOUND: i32 = -25300;
        match get_generic_password(&self.service, &self.account) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| "keychain item is not UTF-8".to_owned()),
            Err(e) if e.code() == NOT_FOUND => Ok(None),
            Err(e) => Err(format!("keychain read failed: {e}")),
        }
    }

    fn write(&self, value: &str) -> Result<(), String> {
        security_framework::passwords::set_generic_password(
            &self.service,
            &self.account,
            value.as_bytes(),
        )
        .map_err(|e| format!("keychain write failed: {e}"))
    }

    fn describe(&self) -> String {
        format!("keychain item \"{}\"", self.service)
    }
}

/// The store Wisp uses on this platform: the login keychain on macOS, otherwise an owner-only file
/// at `fallback_file`.
pub fn platform_store(service: &str, fallback_file: &Path) -> Box<dyn SecretStore> {
    #[cfg(target_os = "macos")]
    {
        let _ = fallback_file;
        Box::new(KeychainStore::new(service, "cloud-api-keys"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = service;
        Box::new(FileStore::new(fallback_file))
    }
}

/// Loads the keys from `store`. When `store` is empty and the old plain-text `legacy` file exists,
/// its keys are copied into `store` and the file is deleted, but only after the copy is written:
/// a failed write leaves the file in place (tightened to owner-only) so nothing is lost.
///
/// A missing or unreadable store yields no keys rather than an error, so the app still starts; the
/// problem is logged.
pub fn load_keys(store: &dyn SecretStore, legacy: &FileStore) -> HashMap<String, String> {
    match store.read() {
        Ok(Some(json)) => return parse(&json),
        Ok(None) => {}
        Err(e) => {
            eprintln!(
                "wisp: could not read API keys from {}: {e}",
                store.describe()
            );
            return HashMap::new();
        }
    }
    if store.describe() == legacy.describe() {
        return HashMap::new();
    }
    let keys = match legacy.read() {
        Ok(Some(json)) => parse(&json),
        _ => return HashMap::new(),
    };
    match save_keys(store, &keys) {
        Ok(()) => {
            if let Err(e) = fs::remove_file(legacy.path()) {
                eprintln!(
                    "wisp: moved API keys to {} but could not delete {}: {e}",
                    store.describe(),
                    legacy.describe()
                );
            }
        }
        Err(e) => {
            eprintln!("wisp: could not move API keys to {}: {e}", store.describe());
            let _ = legacy.write(&serialize(&keys));
        }
    }
    keys
}

/// Replaces the stored keys with `keys`.
pub fn save_keys(store: &dyn SecretStore, keys: &HashMap<String, String>) -> Result<(), String> {
    store.write(&serialize(keys))
}

fn parse(json: &str) -> HashMap<String, String> {
    serde_json::from_str(json).unwrap_or_default()
}

fn serialize(keys: &HashMap<String, String>) -> String {
    serde_json::to_string(keys).unwrap_or_else(|_| "{}".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// An in-memory stand-in for the keychain.
    #[derive(Default)]
    struct MemoryStore {
        value: Mutex<Option<String>>,
        fail_writes: bool,
    }

    impl SecretStore for MemoryStore {
        fn read(&self) -> Result<Option<String>, String> {
            Ok(self.value.lock().unwrap().clone())
        }
        fn write(&self, value: &str) -> Result<(), String> {
            if self.fail_writes {
                return Err("denied".into());
            }
            *self.value.lock().unwrap() = Some(value.to_owned());
            Ok(())
        }
        fn describe(&self) -> String {
            "memory".into()
        }
    }

    fn keys(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn legacy_file_moves_into_the_store_and_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = FileStore::new(dir.path().join("cloud-keys.json"));
        fs::write(legacy.path(), r#"{"openai":"sk-1"}"#).unwrap();
        let store = MemoryStore::default();

        assert_eq!(load_keys(&store, &legacy), keys(&[("openai", "sk-1")]));
        assert!(
            !legacy.path().exists(),
            "plain-text file deleted after the move"
        );
        assert_eq!(
            parse(&store.read().unwrap().unwrap()),
            keys(&[("openai", "sk-1")])
        );
        // Next start reads the store; the file stays gone.
        assert_eq!(load_keys(&store, &legacy), keys(&[("openai", "sk-1")]));
    }

    #[test]
    fn a_failed_move_keeps_the_file_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = FileStore::new(dir.path().join("cloud-keys.json"));
        fs::write(legacy.path(), r#"{"groq":"gsk-2"}"#).unwrap();
        let store = MemoryStore {
            fail_writes: true,
            ..Default::default()
        };

        assert_eq!(load_keys(&store, &legacy), keys(&[("groq", "gsk-2")]));
        assert!(
            legacy.path().exists(),
            "keys are not lost when the store refuses"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(legacy.path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn the_store_wins_over_a_stale_legacy_file() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = FileStore::new(dir.path().join("cloud-keys.json"));
        fs::write(legacy.path(), r#"{"openai":"old"}"#).unwrap();
        let store = MemoryStore::default();
        save_keys(&store, &keys(&[("openai", "new")])).unwrap();

        assert_eq!(load_keys(&store, &legacy), keys(&[("openai", "new")]));
    }

    #[test]
    fn empty_or_garbage_yields_no_keys() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = FileStore::new(dir.path().join("missing.json"));
        assert!(load_keys(&MemoryStore::default(), &legacy).is_empty());

        let garbage = MemoryStore::default();
        garbage.write("not json").unwrap();
        assert!(load_keys(&garbage, &legacy).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn file_store_writes_owner_only_and_round_trips() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.json");
        // An older version left a world-readable file behind.
        fs::write(&path, "{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let store = FileStore::new(&path);
        save_keys(&store, &keys(&[("openai", "sk-3")])).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // As the platform store off macOS, the file is both store and "legacy": no self-migration.
        assert_eq!(
            load_keys(&store, &FileStore::new(&path)),
            keys(&[("openai", "sk-3")])
        );
        assert!(path.exists());
    }
}
