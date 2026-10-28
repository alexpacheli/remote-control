//! The keys of hosts this device has connected to, per ID (trust on first
//! use): a server that hands out a different key for a known ID — someone
//! else's computer, or a compromised server — is caught.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use hext_crypto::PublicKey;

use crate::DeviceId;

/// What the stored key says about a host's key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// The key this ID had before.
    Known,
    /// First connection to this ID.
    New,
    /// This ID had another key: do not connect.
    Changed { before: PublicKey },
}

/// A text file, one `ID key` per line.
#[derive(Debug, Clone)]
pub struct KnownHosts {
    path: PathBuf,
}

impl KnownHosts {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `known_hosts` in this user's settings folder.
    pub fn default_location() -> Result<Self, hext_crypto::CryptoError> {
        Ok(Self::new(hext_crypto::config_dir()?.join("known_hosts")))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn check(&self, id: DeviceId, key: &PublicKey) -> std::io::Result<Check> {
        Ok(match self.read()?.get(&id) {
            None => Check::New,
            Some(before) if before == key => Check::Known,
            Some(&before) => Check::Changed { before },
        })
    }

    /// Remembers `key` for `id` (replacing what was there).
    pub fn remember(&self, id: DeviceId, key: &PublicKey) -> std::io::Result<()> {
        let mut hosts = self.read()?;
        hosts.insert(id, *key);
        let text: String = hosts
            .iter()
            .map(|(id, key)| format!("{} {key}\n", id.get()))
            .collect();
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir)?;
        }
        // Whole file at once: a crash keeps the old one. (Two viewers adding
        // hosts at the same instant may lose one entry; it is re-learned on
        // the next connection.)
        let mut tmp = self.path.as_os_str().to_owned();
        tmp.push(format!(".{}.tmp", std::process::id()));
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &self.path)
    }

    fn read(&self) -> std::io::Result<BTreeMap<DeviceId, PublicKey>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(e),
        };
        // Lines that do not parse are skipped (and dropped on the next write).
        Ok(text
            .lines()
            .filter_map(|line| {
                let (id, key) = line.trim().split_once(' ')?;
                Some((id.parse().ok()?, key.trim().parse().ok()?))
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hext_crypto::DeviceKey;

    #[test]
    fn first_use_then_known_then_changed() {
        let dir = std::env::temp_dir().join(format!("hext-known-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let hosts = KnownHosts::new(dir.join("sub").join("known_hosts"));
        let id = DeviceId::new(482_913_557).unwrap();
        let other = DeviceId::new(111_222_333).unwrap();
        let key = DeviceKey::generate().unwrap().public_key();
        let impostor = DeviceKey::generate().unwrap().public_key();

        assert_eq!(hosts.check(id, &key).unwrap(), Check::New);
        hosts.remember(id, &key).unwrap();
        hosts.remember(other, &impostor).unwrap();
        assert_eq!(hosts.check(id, &key).unwrap(), Check::Known);
        assert_eq!(
            hosts.check(id, &impostor).unwrap(),
            Check::Changed { before: key }
        );
        assert_eq!(hosts.check(other, &impostor).unwrap(), Check::Known);

        // Junk lines are ignored.
        let mut text = fs::read_to_string(hosts.path()).unwrap();
        text.push_str("garbage\n123 notakey\n");
        fs::write(hosts.path(), text).unwrap();
        assert_eq!(hosts.check(id, &key).unwrap(), Check::Known);
        let _ = fs::remove_dir_all(dir);
    }
}
