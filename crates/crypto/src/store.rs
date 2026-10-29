//! Where the device key lives between runs.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use zeroize::Zeroizing;

use crate::{CryptoError, DeviceKey, Result, SEED_LEN, protect};

/// Keeps a device's key between runs.
///
/// The key is a file only the user can read ([`FileKeyStore`]), sealed by
/// the OS where it can (DPAPI on Windows).
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

/// File header: format and version. Version 1 holds the key as is,
/// version 2 sealed by the OS ([`protect`]).
const MAGIC: &[u8; 8] = b"HEXTKEY1";
const MAGIC_SEALED: &[u8; 8] = b"HEXTKEY2";

/// The key: the seed, then the public key (which checks the seed).
const KEY_LEN: usize = SEED_LEN + 32;
/// Header and key, as is.
const FILE_LEN: usize = MAGIC.len() + KEY_LEN;
/// More than any sealed key takes (DPAPI adds a few hundred bytes).
const MAX_FILE_LEN: usize = 4096;

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

    /// The key in a file's contents; whether it is stored as is.
    fn parse(&self, bytes: &[u8]) -> Result<(DeviceKey, bool)> {
        if let Some(sealed) = bytes.strip_prefix(MAGIC_SEALED) {
            if bytes.len() > MAX_FILE_LEN {
                return Err(CryptoError::Corrupt(self.path.clone()));
            }
            let key = protect::unseal(sealed).map_err(|source| CryptoError::Sealed {
                path: self.path.clone(),
                source,
            })?;
            return Ok((self.key_from(&key)?, false));
        }
        match bytes.strip_prefix(MAGIC) {
            Some(key) if bytes.len() == FILE_LEN => Ok((self.key_from(key)?, true)),
            _ => Err(CryptoError::Corrupt(self.path.clone())),
        }
    }

    /// Seed and public key, as stored.
    fn key_from(&self, bytes: &[u8]) -> Result<DeviceKey> {
        let corrupt = || CryptoError::Corrupt(self.path.clone());
        if bytes.len() != KEY_LEN {
            return Err(corrupt());
        }
        let (seed, public) = bytes.split_at(SEED_LEN);
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
        let mut file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(self.io(e)),
        };
        // A file restored or copied with loose permissions is made private.
        #[cfg(unix)]
        restrict(&file).map_err(|e| self.io(e))?;
        // Read no more than a key file can hold (not a huge file by mistake).
        let mut bytes = Zeroizing::new(Vec::with_capacity(FILE_LEN));
        (&mut file)
            .take(MAX_FILE_LEN as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| self.io(e))?;
        drop(file);
        let (key, plain) = self.parse(&bytes)?;
        // A key from before sealing (or from where it is not available) is
        // sealed now, if this OS can: best effort, it stays usable as is.
        if plain && protect::AVAILABLE {
            let _ = self.seal_in_place(&key, &bytes);
        }
        Ok(Some(key))
    }

    fn create(&self, key: DeviceKey) -> Result<DeviceKey> {
        let dir = self.path.parent().filter(|d| !d.as_os_str().is_empty());
        if let Some(dir) = dir {
            create_private_dir(dir).map_err(|e| self.io(e))?;
        }
        let contents = contents(&key);

        // A key that vanishes right after another process created it (the
        // user deleted it) is created again, a few times at most.
        for _ in 0..3 {
            match self.place(&contents) {
                Ok(()) => {
                    // The new name must survive a power cut, or the next run
                    // would create a different identity.
                    #[cfg(unix)]
                    if let Some(dir) = dir {
                        let _ = fs::File::open(dir).and_then(|d| d.sync_all());
                    }
                    return Ok(key);
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    if let Some(stored) = self.load()? {
                        return Ok(stored);
                    }
                }
                Err(e) => return Err(self.io(e)),
            }
        }
        Err(CryptoError::Corrupt(self.path.clone()))
    }
}

/// What the key file holds: the key sealed by the OS where it can, as is
/// elsewhere (or if sealing fails: a key that is not sealed beats an app
/// that cannot start; the next run tries again).
fn contents(key: &DeviceKey) -> Zeroizing<Vec<u8>> {
    let mut plain = Zeroizing::new(Vec::with_capacity(KEY_LEN));
    plain.extend_from_slice(key.seed());
    plain.extend_from_slice(key.public_key().as_bytes());
    let mut contents = Zeroizing::new(Vec::with_capacity(MAX_FILE_LEN));
    match protect::seal(&plain) {
        Ok(sealed) => {
            contents.extend_from_slice(MAGIC_SEALED);
            contents.extend_from_slice(&sealed);
        }
        Err(_) => {
            contents.extend_from_slice(MAGIC);
            contents.extend_from_slice(&plain);
        }
    }
    contents
}

impl FileKeyStore {
    /// Replaces a key stored as is (`was`, the file as read) with the same
    /// key sealed: written whole to a temporary file, checked, then renamed
    /// over the old one, unless the file changed meanwhile.
    fn seal_in_place(&self, key: &DeviceKey, was: &[u8]) -> Result<()> {
        let contents = contents(key);
        if !contents.starts_with(MAGIC_SEALED) {
            return Ok(()); // sealing failed: stays as is
        }
        let (stored, _) = self.parse(&contents)?;
        if stored.public_key() != key.public_key() {
            return Err(CryptoError::Corrupt(self.path.clone()));
        }
        let tmp = self.tmp_path();
        let result = write_private(&tmp, &contents, false)
            .and_then(|()| {
                let now = Zeroizing::new(fs::read(&self.path)?);
                if now.as_slice() != was {
                    return Err(ErrorKind::AlreadyExists.into());
                }
                fs::rename(&tmp, &self.path)
            })
            .map_err(|e| self.io(e));
        let _ = fs::remove_file(&tmp);
        result
    }

    /// A temporary file next to the key, unique to this call.
    fn tmp_path(&self) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let mut name = self.path.as_os_str().to_owned();
        name.push(format!(".{}-{n}.tmp", std::process::id()));
        PathBuf::from(name)
    }

    /// Puts `contents` at the key's path unless a file is there
    /// (`AlreadyExists`).
    ///
    /// Written whole to a temporary file, then linked into place: a crash
    /// leaves no half-written key, and of two processes starting at once
    /// both end up with the same key. Where hard links are not supported
    /// (FAT, some network shares) the file is created in place instead.
    fn place(&self, contents: &[u8]) -> std::io::Result<()> {
        let tmp = self.tmp_path();
        let linked =
            write_private(&tmp, contents, false).and_then(|()| fs::hard_link(&tmp, &self.path));
        let _ = fs::remove_file(&tmp);
        match linked {
            Err(e) if e.kind() != ErrorKind::AlreadyExists => {
                write_private(&self.path, contents, true).inspect_err(|e| {
                    if e.kind() != ErrorKind::AlreadyExists {
                        let _ = fs::remove_file(&self.path);
                    }
                })
            }
            other => other,
        }
    }
}

/// A file readable only by this user (`exclusive`: fails if it exists).
fn write_private(path: &Path, contents: &[u8], exclusive: bool) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true);
    if exclusive {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(unix)]
fn restrict(file: &fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = file.metadata()?.permissions().mode();
    if mode & 0o077 != 0 {
        file.set_permissions(fs::Permissions::from_mode(mode & 0o700))?;
    }
    Ok(())
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// This user's folder for the app's settings (the key and other files).
pub fn config_dir() -> Result<PathBuf> {
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
        let stored = fs::read(store.path()).unwrap();
        if protect::AVAILABLE {
            assert!(stored.starts_with(MAGIC_SEALED));
            assert!(
                !stored.windows(SEED_LEN).any(|w| w == first.seed()),
                "the seed is not in the file"
            );
        } else {
            assert_eq!(stored.len(), FILE_LEN);
        }
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
        // (A damaged sealed key does not open at all.)
        assert!(matches!(
            load_or_create(&store),
            Err(CryptoError::Corrupt(_) | CryptoError::Sealed { .. })
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

    #[cfg(unix)]
    #[test]
    fn a_readable_key_is_made_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("loose");
        let store = FileKeyStore::new(dir.join("device.key"));
        let key = store.create(DeviceKey::generate().unwrap()).unwrap();
        fs::set_permissions(store.path(), fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            store.load().unwrap().unwrap().public_key(),
            key.public_key()
        );
        let mode = fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_key_from_before_sealing_still_loads_and_gets_sealed() {
        let dir = temp_dir("plain");
        fs::create_dir_all(&dir).unwrap();
        let store = FileKeyStore::new(dir.join("device.key"));
        let key = DeviceKey::generate().unwrap();
        let mut plain = MAGIC.to_vec();
        plain.extend_from_slice(key.seed());
        plain.extend_from_slice(key.public_key().as_bytes());
        fs::write(store.path(), &plain).unwrap();
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.public_key(), key.public_key());
        let stored = fs::read(store.path()).unwrap();
        let magic = if protect::AVAILABLE {
            MAGIC_SEALED
        } else {
            MAGIC
        };
        assert!(stored.starts_with(magic));
        assert_eq!(
            store.load().unwrap().unwrap().public_key(),
            key.public_key()
        );
        let leftovers = fs::read_dir(&dir).unwrap().count();
        assert_eq!(leftovers, 1, "no temporary files left behind");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_sealed_key_that_does_not_open_is_reported() {
        let dir = temp_dir("sealed");
        fs::create_dir_all(&dir).unwrap();
        let store = FileKeyStore::new(dir.join("device.key"));
        let mut bytes = MAGIC_SEALED.to_vec();
        bytes.extend_from_slice(&[7; 200]);
        fs::write(store.path(), &bytes).unwrap();
        assert!(matches!(store.load(), Err(CryptoError::Sealed { .. })));
        // Left for the user (the right Windows account opens it).
        assert_eq!(fs::read(store.path()).unwrap(), bytes);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_huge_file_is_not_read_whole() {
        let dir = temp_dir("huge");
        fs::create_dir_all(&dir).unwrap();
        let store = FileKeyStore::new(dir.join("device.key"));
        fs::write(store.path(), vec![0u8; 1 << 20]).unwrap();
        assert!(matches!(store.load(), Err(CryptoError::Corrupt(_))));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn default_location_is_per_user() {
        match FileKeyStore::default_location() {
            Ok(store) => assert!(store.path().ends_with("device.key")),
            // No HOME/APPDATA (a bare environment): that is reported.
            Err(e) => assert!(matches!(e, CryptoError::NoConfigDir(_))),
        }
    }
}
