//! Devices and the audit log, in SQLite.

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use hext_crypto::PublicKey;
use hext_signal::DeviceId;
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::{Connection, OptionalExtension, params};

/// The database. Every call is a short query, so one connection behind a
/// lock is enough for now.
pub struct Db {
    conn: Mutex<Connection>,
    rng: SystemRandom,
}

impl Db {
    /// Opens (or creates) the database file.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        Self::init(Connection::open(path)?)
    }

    /// A database that lives only in memory, for tests.
    pub fn in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS devices (
                 id INTEGER PRIMARY KEY,
                 public_key BLOB NOT NULL UNIQUE,
                 name TEXT NOT NULL,
                 app_version TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 last_seen INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS audit (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 at INTEGER NOT NULL,
                 from_id INTEGER NOT NULL,
                 to_id INTEGER NOT NULL,
                 event TEXT NOT NULL
             );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
            rng: SystemRandom::new(),
        })
    }

    /// The ID of the device with this key, if it has logged in before;
    /// updates its name and last-seen time.
    pub fn find(
        &self,
        key: &PublicKey,
        name: &str,
        app_version: &str,
    ) -> rusqlite::Result<Option<DeviceId>> {
        let conn = self.conn.lock().expect("db lock");
        let Some(id) = id_of(&conn, key)? else {
            return Ok(None);
        };
        conn.execute(
            "UPDATE devices SET name = ?2, app_version = ?3, last_seen = ?4 WHERE id = ?1",
            params![id.get(), name, app_version, now()],
        )?;
        Ok(Some(id))
    }

    /// Whether a device with this key has an ID (logged in at least once).
    pub fn is_registered(&self, key: &PublicKey) -> rusqlite::Result<bool> {
        let conn = self.conn.lock().expect("db lock");
        Ok(id_of(&conn, key)?.is_some())
    }

    /// Gives a new device its ID: at random, so IDs say nothing about how
    /// many devices exist or when they joined. (If the key is already
    /// known, its ID.)
    pub fn register(
        &self,
        key: &PublicKey,
        name: &str,
        app_version: &str,
    ) -> rusqlite::Result<DeviceId> {
        let conn = self.conn.lock().expect("db lock");
        loop {
            if let Some(id) = id_of(&conn, key)? {
                return Ok(id);
            }
            let id = self.random_id();
            let inserted = conn.execute(
                "INSERT OR IGNORE INTO devices (id, public_key, name, app_version, created_at, last_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                params![id.get(), key.as_bytes(), name, app_version, now()],
            )?;
            if inserted == 1 {
                return Ok(id);
            }
            // The ID was taken: draw another.
        }
    }

    /// Records who asked to reach whom, and what came of it.
    pub fn audit(&self, from: DeviceId, to: DeviceId, event: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().expect("db lock");
        conn.execute(
            "INSERT INTO audit (at, from_id, to_id, event) VALUES (?1, ?2, ?3, ?4)",
            params![now(), from.get(), to.get(), event],
        )?;
        Ok(())
    }

    /// Audit entries about `to`, oldest first (for tests and support).
    pub fn audit_of(&self, to: DeviceId) -> rusqlite::Result<Vec<(DeviceId, String)>> {
        let conn = self.conn.lock().expect("db lock");
        let mut stmt =
            conn.prepare("SELECT from_id, event FROM audit WHERE to_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map(params![to.get()], |row| {
            Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.filter_map(|row| match row {
            Ok((from, event)) => DeviceId::new(from).map(|from| Ok((from, event))),
            Err(e) => Some(Err(e)),
        })
        .collect()
    }

    fn random_id(&self) -> DeviceId {
        // Uniform over the 900 million 9-digit numbers (rejection sampling).
        let span = DeviceId::MAX - DeviceId::MIN + 1;
        let limit = u32::MAX - u32::MAX % span;
        loop {
            let mut bytes = [0; 4];
            self.rng.fill(&mut bytes).expect("system random numbers");
            let n = u32::from_le_bytes(bytes);
            if n < limit {
                return DeviceId::new(DeviceId::MIN + n % span).expect("in range");
            }
        }
    }
}

/// The ID stored for `key`. An ID outside the 9-digit range (a damaged or
/// hand-edited database) is an error, not a reason to register again.
fn id_of(conn: &Connection, key: &PublicKey) -> rusqlite::Result<Option<DeviceId>> {
    let stored: Option<u32> = conn
        .query_row(
            "SELECT id FROM devices WHERE public_key = ?1",
            params![key.as_bytes()],
            |row| row.get(0),
        )
        .optional()?;
    stored
        .map(|n| DeviceId::new(n).ok_or(rusqlite::Error::IntegralValueOutOfRange(0, n.into())))
        .transpose()
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hext_crypto::DeviceKey;

    #[test]
    fn same_key_same_id_other_key_other_id() {
        let db = Db::in_memory().unwrap();
        let a = DeviceKey::generate().unwrap().public_key();
        let b = DeviceKey::generate().unwrap().public_key();
        assert_eq!(db.find(&a, "A", "0.1.0").unwrap(), None);
        let id_a = db.register(&a, "A", "0.1.0").unwrap();
        assert_eq!(db.find(&a, "A renamed", "0.2.0").unwrap(), Some(id_a));
        assert_eq!(db.register(&a, "A", "0.1.0").unwrap(), id_a);
        assert_ne!(db.register(&b, "B", "0.1.0").unwrap(), id_a);
    }

    #[test]
    fn ids_persist_in_the_file() {
        let dir = std::env::temp_dir().join(format!("hext-server-db-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hext.db");
        let key = DeviceKey::generate().unwrap().public_key();
        let id = Db::open(&path)
            .unwrap()
            .register(&key, "A", "0.1.0")
            .unwrap();
        assert_eq!(
            Db::open(&path).unwrap().find(&key, "A", "0.1.0").unwrap(),
            Some(id)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ids_are_spread_out() {
        let db = Db::in_memory().unwrap();
        let ids: Vec<u32> = (0..50).map(|_| db.random_id().get()).collect();
        assert!(
            ids.iter()
                .all(|&n| (DeviceId::MIN..=DeviceId::MAX).contains(&n))
        );
        // Not sequential: consecutive IDs are far apart.
        let close = ids
            .windows(2)
            .filter(|w| w[0].abs_diff(w[1]) < 1_000)
            .count();
        assert_eq!(close, 0);
    }

    #[test]
    fn a_damaged_id_is_an_error_not_a_hang() {
        let db = Db::in_memory().unwrap();
        let key = DeviceKey::generate().unwrap().public_key();
        db.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO devices VALUES (42, ?1, 'x', '0', 0, 0)",
                params![key.as_bytes()],
            )
            .unwrap();
        assert!(db.find(&key, "x", "0").is_err());
        assert!(db.register(&key, "x", "0").is_err());
    }

    #[test]
    fn audit_log() {
        let db = Db::in_memory().unwrap();
        let (a, b) = (
            DeviceId::new(111_111_111).unwrap(),
            DeviceId::new(222_222_222).unwrap(),
        );
        db.audit(a, b, "requested").unwrap();
        db.audit(a, b, "accepted").unwrap();
        assert_eq!(
            db.audit_of(b).unwrap(),
            vec![(a, "requested".to_owned()), (a, "accepted".to_owned())]
        );
    }
}
