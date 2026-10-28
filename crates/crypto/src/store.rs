//! Where the device key lives between runs.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use zeroize::Zeroizing;

use crate::{CryptoError, DeviceKey, Result, SEED_LEN};

/// Keeps a device's key between runs.
///
/// For now the key is a file only the user can read ([`FileKeyStore`]);
/// keys protected by the OS (DPAPI on Windows, Keychain on macOS) will be
/// other implementations.
pub trait KeyStore {
    /// The stored key, or `None` if there is none yet.
    fn load(&self) -> Result<Option<DeviceKey>>;

    /// Stores `key` unless a key is already stored (another process got
    /// there first); returns the stored key.
    fn create(&self, key: DeviceKey) -> Result<DeviceKey>;
}

/// The stored key, or a new one (stored) on first run. The flag is true
/// when the key was just created.
pub fn load_or_create(store: &impl KeyStore) -> Result<(DeviceKey, bool)> {
    if let Some(key) = store.load()? {
        return Ok((key, false));
    }
    let new = DeviceKey::generate()?;
    let public = new.public_key();
    let stored = store.create(new)?;
    let created = stored.public_key() == public;
    Ok((stored, created))
}

/// File header: format and version.
const MAGIC: &[u8; 8] = b"HEXTKEY1";

/// Header, seed, then the public key (which checks the seed).
const FILE_LEN: usize = MAGIC.len() + SEED_LEN + 32;

/// The key in a file only the user can read.
#[derive(Debug, Clone)]
pub struct FileKeyStore {
    path: PathBuf,
}

impl FileKeyStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The usual place for this user: `%APPDATA%\HextRemote` on Windows,
    /// `~/Library/Application Support/HextRemote` on macOS,
    /// `$XDG_CONFIG_HOME/hext-remote` (or `~/.config/hext-remote`) elsewhere.
    pub fn default_location() -> Result<Self> {
        Ok(Self::new(config_dir()?.join("device.key")))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn io(&self, source: std::io::Error) -> CryptoError {
        CryptoError::Io {
            path: self.path.clone(),
            source,
        }
    }

    fn parse(&self, bytes: &[u8]) -> Result<DeviceKey> {
        let corrupt = || CryptoError::Corrupt(self.path.clone());
        if bytes.len() != FILE_LEN || !bytes.starts_with(MAGIC) {
            return Err(corrupt());
        }
        let (seed, public) = bytes[MAGIC.len()..].split_at(SEED_LEN);
        let seed: Zeroizing<[u8; SEED_LEN]> =
            Zeroizing::new(seed.try_into().map_err(|_| corrupt())?);
        let key = DeviceKey::from_seed(&seed).map_err(|_| corrupt())?;
        if key.public_key().as_bytes()[..] != *public {
            return Err(corrupt());
        }
        Ok(key)
    }
}

impl KeyStore for FileKeyStore {
    fn load(&self) -> Result<Option<DeviceKey>> {
        match fs::read(&self.path) {
            Ok(bytes) => self.parse(&Zeroizing::new(bytes)).map(Some),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(self.io(e)),
        }
    }

    fn create(&self, key: DeviceKey) -> Result<DeviceKey> {
        if let Some(dir) = self.path.parent() {
            create_private_dir(dir).map_err(|e| self.io(e))?;
        }
        let mut contents = Zeroizing::new(Vec::with_capacity(FILE_LEN));
        contents.extend_from_slice(MAGIC);
        contents.extend_from_slice(key.seed());
        contents.extend_from_slice(key.public_key().as_bytes());

        // Written whole to a temporary file, then linked into place only if
        // no key exists: a crash leaves no half-written key, and of two
        // processes starting at once both end up with the same key.
        let mut tmp_name = self.path.as_os_str().to_owned();
        tmp_name.push(format!(".{}.tmp", std::process::id()));
        let tmp = PathBuf::from(tmp_name);
        let written = write_private(&tmp, &contents);
        let placed = written.and_then(|()| fs::hard_link(&tmp, &self.path));
        let _ = fs::remove_file(&tmp);
        match placed {
            Ok(()) => Ok(key),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => self
                .load()?
                .ok_or_else(|| CryptoError::Corrupt(self.path.clone())),
            Err(e) => Err(self.io(e)),
        }
    }
}

/// A new file readable only by this user.
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// This user's folder for the app's settings.
fn config_dir() -> Result<PathBuf> {
    let var = |name: &'static str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .ok_or(CryptoError::NoConfigDir(name))
    };
    if cfg!(windows) {
        Ok(var("APPDATA")?.join("HextRemote"))
    } else if cfg!(target_os = "macos") {
        Ok(var("HOME")?.join("Library/Application Support/HextRemote"))
    } else {
        let base = var("XDG_CONFIG_HOME").or_else(|_| var("HOME").map(|h| h.join(".config")))?;
        Ok(base.join("hext-remote"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh folder for one test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hext-crypto-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn creates_once_then_loads_the_same_key() {
        let dir = temp_dir("roundtrip");
        let store = FileKeyStore::new(dir.join("sub").join("device.key"));
        let (first, created) = load_or_create(&store).unwrap();
        assert!(created);
        let (again, created) = load_or_create(&store).unwrap();
        assert!(!created);
        assert_eq!(first.public_key(), again.public_key());
        assert_eq!(fs::read(store.path()).unwrap().len(), FILE_LEN);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_existing_key_wins_over_a_new_one() {
        let dir = temp_dir("race");
        let store = FileKeyStore::new(dir.join("device.key"));
        let first = store.create(DeviceKey::generate().unwrap()).unwrap();
        // Another process created its key at the same time: it gets ours.
        let second = store.create(DeviceKey::generate().unwrap()).unwrap();
        assert_eq!(first.public_key(), second.public_key());
        let leftovers = fs::read_dir(&dir).unwrap().count();
        assert_eq!(leftovers, 1, "no temporary files left behind");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_damaged_file_is_an_error_not_a_new_identity() {
        let dir = temp_dir("corrupt");
        let store = FileKeyStore::new(dir.join("device.key"));
        store.create(DeviceKey::generate().unwrap()).unwrap();
        let mut bytes = fs::read(store.path()).unwrap();
        bytes[MAGIC.len()] ^= 1; // the seed no longer matches the public key
        fs::write(store.path(), &bytes).unwrap();
        assert!(matches!(
            load_or_create(&store),
            Err(CryptoError::Corrupt(_))
        ));
        fs::write(store.path(), b"short").unwrap();
        assert!(matches!(store.load(), Err(CryptoError::Corrupt(_))));
        // The damaged file is left for the user to restore.
        assert_eq!(fs::read(store.path()).unwrap(), b"short");
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn only_the_user_can_read_the_key() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("perms");
        let store = FileKeyStore::new(dir.join("keys").join("device.key"));
        load_or_create(&store).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(store.path()), 0o600);
        assert_eq!(mode(&dir.join("keys")), 0o700);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn default_location_is_per_user() {
        let store = FileKeyStore::default_location().unwrap();
        assert!(store.path().ends_with("device.key"));
    }
}
