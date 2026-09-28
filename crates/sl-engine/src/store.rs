//! Durable engine state in `<data_dir>/state.sqlite3`.
//!
//! The recovered client keeps installations, devices and cached files in SQLite
//! (`installations.db`). Sideport keeps accounts, certificates, installations, the refresh queue
//! and content-addressed IPA copies here; secrets live in a [`crate::secrets::SecretStore`].
//! Every process opens its own connection. WAL mode and a busy timeout serialize writers.

use crate::error::{EngineError, Result};
use crate::types::{AccountSummary, Installation};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::fmt;
use std::path::Path;
use std::time::Duration;

const SCHEMA_VERSION: i64 = 2;

const SCHEMA: &str = "
    CREATE TABLE accounts (
        apple_id TEXT PRIMARY KEY,
        record TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    );

    CREATE TABLE certificates (
        team_id TEXT PRIMARY KEY,
        serial TEXT NOT NULL,
        der BLOB NOT NULL,
        updated_at INTEGER NOT NULL
    );

    CREATE TABLE installations (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        device_udid TEXT NOT NULL,
        bundle_id TEXT NOT NULL,
        record TEXT NOT NULL,
        icon BLOB,
        expires_at INTEGER,
        auto_refresh INTEGER NOT NULL,
        enqueued_at INTEGER,
        enqueue_token TEXT,
        stored_file TEXT,
        UNIQUE (device_udid, bundle_id)
    );

    CREATE TABLE stored_files (
        sha256 TEXT PRIMARY KEY,
        file_name TEXT NOT NULL,
        size INTEGER NOT NULL,
        created_at INTEGER NOT NULL
    );

    CREATE TABLE meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
";

/// Version 2: refresh claims, so processes sharing a data directory run each refresh once.
const MIGRATION_2: &str = "ALTER TABLE installations ADD COLUMN claimed_at INTEGER;";

/// A development certificate issued for this machine's signing key within one team.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredCertificate {
    pub serial: String,
    pub der: Vec<u8>,
}

/// A queued refresh request, ordered by `enqueued_at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueuedRefresh {
    pub installation_id: i64,
    pub token: String,
}

pub(crate) struct Store {
    connection: Mutex<Connection>,
}

impl fmt::Debug for Store {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    pub(crate) fn open(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir).map_err(|error| EngineError::Storage(error.to_string()))?;

        let connection = Connection::open(data_dir.join("state.sqlite3")).map_err(sql_error)?;
        connection.busy_timeout(Duration::from_secs(10)).map_err(sql_error)?;
        connection.pragma_update(None, "journal_mode", "WAL").map_err(sql_error)?;
        connection.pragma_update(None, "foreign_keys", true).map_err(sql_error)?;

        let store = Self { connection: Mutex::new(connection) };
        store.migrate()?;

        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_error)?;

        let version: i64 = transaction.pragma_query_value(None, "user_version", |row| row.get(0)).map_err(sql_error)?;

        if version > SCHEMA_VERSION {
            return Err(EngineError::Storage(format!("state database schema {version} is newer than this build")));
        }

        if version < 1 {
            transaction.execute_batch(SCHEMA).map_err(sql_error)?;
        }

        if version < 2 {
            transaction.execute_batch(MIGRATION_2).map_err(sql_error)?;
        }

        transaction.pragma_update(None, "user_version", SCHEMA_VERSION).map_err(sql_error)?;

        transaction.commit().map_err(sql_error)
    }

    /// Run `operation` while holding the database write lock, excluding other processes'
    /// writers. `operation` must not use this store.
    pub(crate) fn exclusive<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_error)?;

        let value = operation()?;

        transaction.commit().map_err(sql_error)?;

        Ok(value)
    }

    // --------------------------------------------------------------------------------------------
    // Accounts

    pub(crate) fn accounts(&self) -> Result<Vec<AccountSummary>> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare("SELECT record FROM accounts ORDER BY apple_id").map_err(sql_error)?;

        let records = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(sql_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_error)?;

        records.iter().map(|record| decode(record, "account")).collect()
    }

    pub(crate) fn save_account(&self, account: &AccountSummary) -> Result<()> {
        let mut record = account.clone();
        record.has_session = false;

        let encoded = encode(&record)?;

        self.connection
            .lock()
            .execute(
                "INSERT INTO accounts (apple_id, record, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (apple_id) DO UPDATE SET record = excluded.record, updated_at = excluded.updated_at",
                params![account.apple_id, encoded, Utc::now().timestamp()],
            )
            .map_err(sql_error)?;

        Ok(())
    }

    pub(crate) fn delete_account(&self, apple_id: &str) -> Result<()> {
        self.connection.lock().execute("DELETE FROM accounts WHERE apple_id = ?1", [apple_id]).map_err(sql_error)?;

        Ok(())
    }

    // --------------------------------------------------------------------------------------------
    // Certificates

    pub(crate) fn certificate(&self, team_id: &str) -> Result<Option<StoredCertificate>> {
        let connection = self.connection.lock();

        connection
            .query_row("SELECT serial, der FROM certificates WHERE team_id = ?1", [team_id], |row| {
                Ok(StoredCertificate { serial: row.get(0)?, der: row.get(1)? })
            })
            .optional()
            .map_err(sql_error)
    }

    pub(crate) fn save_certificate(&self, team_id: &str, certificate: &StoredCertificate) -> Result<()> {
        self.connection
            .lock()
            .execute(
                "INSERT INTO certificates (team_id, serial, der, updated_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (team_id) DO UPDATE SET
                     serial = excluded.serial, der = excluded.der, updated_at = excluded.updated_at",
                params![team_id, certificate.serial, certificate.der, Utc::now().timestamp()],
            )
            .map_err(sql_error)?;

        Ok(())
    }

    pub(crate) fn delete_certificate(&self, team_id: &str) -> Result<()> {
        self.connection.lock().execute("DELETE FROM certificates WHERE team_id = ?1", [team_id]).map_err(sql_error)?;

        Ok(())
    }

    // --------------------------------------------------------------------------------------------
    // Metadata

    #[cfg_attr(not(test), expect(dead_code, reason = "used by the refresh scheduler and file cache"))]
    pub(crate) fn meta(&self, key: &str) -> Result<Option<String>> {
        let connection = self.connection.lock();

        connection
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| row.get(0))
            .optional()
            .map_err(sql_error)
    }

    /// Insert `value` unless the key exists, then return the stored value. Concurrent processes
    /// therefore agree on one value, such as the machine identifier.
    pub(crate) fn meta_or_insert(&self, key: &str, value: &str) -> Result<String> {
        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_error)?;

        transaction
            .execute("INSERT OR IGNORE INTO meta (key, value) VALUES (?1, ?2)", [key, value])
            .map_err(sql_error)?;

        let stored = transaction.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| row.get(0));
        let stored = stored.map_err(sql_error)?;

        transaction.commit().map_err(sql_error)?;

        Ok(stored)
    }

    pub(crate) fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.connection
            .lock()
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                [key, value],
            )
            .map_err(sql_error)?;

        Ok(())
    }

    // --------------------------------------------------------------------------------------------
    // Installations

    pub(crate) fn installations(&self) -> Result<Vec<Installation>> {
        let connection = self.connection.lock();
        let mut statement =
            connection.prepare("SELECT id, record, icon FROM installations ORDER BY id").map_err(sql_error)?;

        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<Vec<u8>>>(2)?))
            })
            .map_err(sql_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_error)?;

        rows.into_iter().map(|(id, record, icon)| installation(id, &record, icon)).collect()
    }

    pub(crate) fn installation(&self, id: i64) -> Result<Option<Installation>> {
        let connection = self.connection.lock();

        let row = connection
            .query_row("SELECT record, icon FROM installations WHERE id = ?1", [id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
            })
            .optional()
            .map_err(sql_error)?;

        row.map(|(record, icon)| installation(id, &record, icon)).transpose()
    }

    /// Insert or replace the installation of a bundle on a device, returning its stable id.
    /// Reinstalling the same bundle on the same device keeps the row and its auto-refresh choice.
    pub(crate) fn record_installation(&self, installation: &Installation) -> Result<i64> {
        let (record, icon) = split_installation(installation)?;
        let expires_at = installation.expires_at.map(|time| time.timestamp());

        let mut connection = self.connection.lock();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(sql_error)?;

        let existing: Option<(i64, bool)> = transaction
            .query_row(
                "SELECT id, auto_refresh FROM installations WHERE device_udid = ?1 AND bundle_id = ?2",
                [&installation.device_udid, &installation.bundle_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql_error)?;

        let id = match existing {
            Some((id, auto_refresh)) => {
                let mut kept = installation.clone();
                kept.id = id;
                kept.auto_refresh = auto_refresh;

                let (record, icon) = split_installation(&kept)?;

                transaction
                    .execute(
                        "UPDATE installations SET record = ?2, icon = ?3, expires_at = ?4, stored_file = ?5,
                             enqueued_at = NULL, enqueue_token = NULL WHERE id = ?1",
                        params![id, record, icon, expires_at, stored_file(&kept)],
                    )
                    .map_err(sql_error)?;

                id
            }

            None => {
                transaction
                    .execute(
                        "INSERT INTO installations
                             (device_udid, bundle_id, record, icon, expires_at, auto_refresh, stored_file)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            installation.device_udid,
                            installation.bundle_id,
                            record,
                            icon,
                            expires_at,
                            installation.auto_refresh,
                            stored_file(installation)
                        ],
                    )
                    .map_err(sql_error)?;

                transaction.last_insert_rowid()
            }
        };

        transaction.commit().map_err(sql_error)?;

        Ok(id)
    }

    /// Persist changed fields of an existing installation (refresh outcome, errors, toggles).
    pub(crate) fn update_installation(&self, installation: &Installation) -> Result<()> {
        let (record, icon) = split_installation(installation)?;
        let expires_at = installation.expires_at.map(|time| time.timestamp());

        let changed = self
            .connection
            .lock()
            .execute(
                "UPDATE installations
                 SET record = ?2, icon = ?3, expires_at = ?4, auto_refresh = ?5, stored_file = ?6 WHERE id = ?1",
                params![
                    installation.id,
                    record,
                    icon,
                    expires_at,
                    installation.auto_refresh,
                    stored_file(installation)
                ],
            )
            .map_err(sql_error)?;

        if changed == 0 {
            return Err(EngineError::Storage(format!("installation {} does not exist", installation.id)));
        }

        Ok(())
    }

    pub(crate) fn delete_installation(&self, id: i64) -> Result<bool> {
        let deleted =
            self.connection.lock().execute("DELETE FROM installations WHERE id = ?1", [id]).map_err(sql_error)?;

        Ok(deleted != 0)
    }

    /// Queue a refresh once; an installation already queued keeps its original position.
    pub(crate) fn enqueue_refresh(&self, id: i64, token: &str, now: DateTime<Utc>) -> Result<bool> {
        let queued = self
            .connection
            .lock()
            .execute(
                "UPDATE installations SET enqueued_at = ?2, enqueue_token = ?3, claimed_at = NULL
                 WHERE id = ?1 AND enqueued_at IS NULL",
                params![id, now.timestamp(), token],
            )
            .map_err(sql_error)?;

        Ok(queued != 0)
    }

    /// The oldest queued refresh, matching the recovered `GetNextPendingInstallation` order.
    #[cfg(test)]
    pub(crate) fn next_refresh(&self) -> Result<Option<QueuedRefresh>> {
        let connection = self.connection.lock();

        connection
            .query_row(
                "SELECT id, enqueue_token FROM installations
                 WHERE enqueued_at IS NOT NULL AND enqueue_token IS NOT NULL
                 ORDER BY enqueued_at, id LIMIT 1",
                [],
                |row| Ok(QueuedRefresh { installation_id: row.get(0)?, token: row.get(1)? }),
            )
            .optional()
            .map_err(sql_error)
    }

    /// Claim the oldest queued refresh for `claim`. An entry claimed before `stale_before`
    /// (a crashed process) can be claimed again. Returns the claimed entry.
    pub(crate) fn claim_refresh(
        &self,
        claim: &str,
        now: DateTime<Utc>,
        stale_before: DateTime<Utc>,
    ) -> Result<Option<QueuedRefresh>> {
        let connection = self.connection.lock();

        connection
            .query_row(
                "UPDATE installations SET enqueue_token = ?1, claimed_at = ?2
                 WHERE id = (
                     SELECT id FROM installations
                     WHERE enqueued_at IS NOT NULL AND enqueue_token IS NOT NULL
                       AND (claimed_at IS NULL OR claimed_at < ?3)
                     ORDER BY enqueued_at, id LIMIT 1
                 )
                 RETURNING id, enqueue_token",
                params![claim, now.timestamp(), stale_before.timestamp()],
                |row| Ok(QueuedRefresh { installation_id: row.get(0)?, token: row.get(1)? }),
            )
            .optional()
            .map_err(sql_error)
    }

    /// Clear a queue entry only if it still carries `token`, so a newer request is kept.
    pub(crate) fn dequeue_refresh(&self, entry: &QueuedRefresh) -> Result<bool> {
        let cleared = self
            .connection
            .lock()
            .execute(
                "UPDATE installations SET enqueued_at = NULL, enqueue_token = NULL, claimed_at = NULL
                 WHERE id = ?1 AND enqueue_token = ?2",
                params![entry.installation_id, entry.token],
            )
            .map_err(sql_error)?;

        Ok(cleared != 0)
    }

    // --------------------------------------------------------------------------------------------
    // Stored files

    pub(crate) fn record_stored_file(&self, sha256: &str, file_name: &str, size: u64) -> Result<()> {
        let size = i64::try_from(size).map_err(|_| EngineError::Storage("stored file is too large".into()))?;

        self.connection
            .lock()
            .execute(
                "INSERT OR IGNORE INTO stored_files (sha256, file_name, size, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![sha256, file_name, size, Utc::now().timestamp()],
            )
            .map_err(sql_error)?;

        Ok(())
    }

    pub(crate) fn stored_file_referenced(&self, sha256: &str) -> Result<bool> {
        let connection = self.connection.lock();

        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM installations WHERE stored_file = ?1", [sha256], |row| row.get(0))
            .map_err(sql_error)?;

        Ok(count != 0)
    }

    pub(crate) fn delete_stored_file(&self, sha256: &str) -> Result<()> {
        self.connection.lock().execute("DELETE FROM stored_files WHERE sha256 = ?1", [sha256]).map_err(sql_error)?;

        Ok(())
    }
}

/// Stored IPA copies are named `<sha256>.ipa`; other sources are not tracked as stored files.
fn stored_file(installation: &Installation) -> Option<String> {
    let name = installation.spec.source.file_name()?.to_str()?;
    let digest = name.strip_suffix(".ipa")?;

    let hexadecimal = digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit());

    hexadecimal.then(|| digest.to_ascii_lowercase())
}

fn split_installation(installation: &Installation) -> Result<(String, Option<Vec<u8>>)> {
    let mut record = installation.clone();
    let icon = record.icon_png.take();

    Ok((encode(&record)?, icon))
}

fn installation(id: i64, record: &str, icon: Option<Vec<u8>>) -> Result<Installation> {
    let mut installation: Installation = decode(record, "installation")?;
    installation.id = id;
    installation.icon_png = icon;

    Ok(installation)
}

fn encode<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|error| EngineError::Storage(error.to_string()))
}

fn decode<T: serde::de::DeserializeOwned>(record: &str, kind: &str) -> Result<T> {
    serde_json::from_str(record).map_err(|error| EngineError::Storage(format!("invalid stored {kind}: {error}")))
}

fn sql_error(error: rusqlite::Error) -> EngineError {
    EngineError::Storage(format!("state database: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AppOptions, JobSpec, SigningMode, Target};

    fn installation(udid: &str, bundle_id: &str, source: &str) -> Installation {
        Installation {
            id: 0,
            app_name: "App".into(),
            bundle_id: bundle_id.into(),
            original_bundle_id: "com.example.app".into(),
            version: Some("1".into()),
            device_udid: udid.into(),
            device_name: "Phone".into(),
            apple_id: "fixture@example.test".into(),
            team_id: "TEAM123456".into(),
            installed_at: "2026-09-01T00:00:00Z".parse().expect("date"),
            expires_at: Some("2026-09-08T00:00:00Z".parse().expect("date")),
            auto_refresh: true,
            last_error: None,
            consecutive_failures: 0,
            icon_png: Some(vec![1, 2, 3]),
            spec: JobSpec {
                source: source.into(),
                target: Target::Device { udid: udid.into(), prefer_network: false },
                signing: SigningMode::AppleId { apple_id: "fixture@example.test".into() },
                options: AppOptions::default(),
            },
        }
    }

    #[test]
    fn version_one_databases_migrate_to_the_current_schema() {
        let directory = tempfile::tempdir().expect("data directory");
        let connection = Connection::open(directory.path().join("state.sqlite3")).expect("connection");
        connection.execute_batch(SCHEMA).expect("version 1 schema");
        connection.pragma_update(None, "user_version", 1).expect("version 1");
        drop(connection);

        let store = Store::open(directory.path()).expect("migrate");
        store.record_installation(&installation("UDID1", "com.example.app", "/a.ipa")).expect("insert");

        let connection = Connection::open(directory.path().join("state.sqlite3")).expect("connection");
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0)).expect("version");
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn schema_is_created_once_and_newer_schemas_are_refused() {
        let directory = tempfile::tempdir().expect("data directory");

        Store::open(directory.path()).expect("create");
        Store::open(directory.path()).expect("reopen");

        let connection = Connection::open(directory.path().join("state.sqlite3")).expect("connection");
        connection.pragma_update(None, "user_version", SCHEMA_VERSION + 1).expect("future schema");
        drop(connection);

        let error = Store::open(directory.path()).expect_err("newer schema");
        assert!(error.to_string().contains("newer"));
    }

    #[test]
    fn accounts_round_trip_without_persisting_session_presence() {
        let directory = tempfile::tempdir().expect("data directory");
        let store = Store::open(directory.path()).expect("store");

        let mut account = AccountSummary {
            apple_id: "b@example.test".into(),
            teams: Vec::new(),
            default_team: Some("TEAM".into()),
            has_session: true,
            remembers_password: true,
            last_login: None,
        };

        store.save_account(&account).expect("save");
        account.apple_id = "a@example.test".into();
        store.save_account(&account).expect("save second");

        let stored = store.accounts().expect("accounts");
        assert_eq!(
            stored.iter().map(|account| account.apple_id.as_str()).collect::<Vec<_>>(),
            ["a@example.test", "b@example.test"]
        );
        assert!(stored.iter().all(|account| !account.has_session && account.remembers_password));

        store.delete_account("a@example.test").expect("delete");
        assert_eq!(store.accounts().expect("accounts").len(), 1);
    }

    #[test]
    fn reinstalling_a_bundle_keeps_its_row_icon_and_refresh_choice() {
        let directory = tempfile::tempdir().expect("data directory");
        let store = Store::open(directory.path()).expect("store");
        let digest = "a".repeat(64);

        let first =
            store.record_installation(&installation("UDID1", "com.example.app", &format!("/files/{digest}.ipa")));
        let first = first.expect("insert");
        let other = store.record_installation(&installation("UDID2", "com.example.app", "/elsewhere/App.ipa"));
        let other = other.expect("other device");
        assert_ne!(first, other);

        let mut stored = store.installation(first).expect("read").expect("present");
        assert_eq!(stored.icon_png.as_deref(), Some(&[1, 2, 3][..]));
        assert!(store.stored_file_referenced(&digest).expect("referenced"));

        stored.auto_refresh = false;
        stored.last_error = Some("failed".into());
        store.update_installation(&stored).expect("update");

        let mut reinstalled = installation("UDID1", "com.example.app", "/elsewhere/App.ipa");
        reinstalled.version = Some("2".into());
        assert_eq!(store.record_installation(&reinstalled).expect("reinstall"), first);

        let stored = store.installation(first).expect("read").expect("present");
        assert_eq!(stored.version.as_deref(), Some("2"));
        assert!(!stored.auto_refresh, "a reinstall keeps the user's refresh choice");
        assert!(!store.stored_file_referenced(&digest).expect("unreferenced"));

        assert!(store.delete_installation(first).expect("delete"));
        assert!(!store.delete_installation(first).expect("absent"));
        assert_eq!(store.installations().expect("list").len(), 1);

        let mut missing = stored;
        missing.id = 999;
        assert!(store.update_installation(&missing).is_err());
    }

    #[test]
    fn the_refresh_queue_keeps_first_position_and_ignores_stale_tokens() {
        let directory = tempfile::tempdir().expect("data directory");
        let store = Store::open(directory.path()).expect("store");
        let first = store.record_installation(&installation("UDID1", "com.example.one", "/a.ipa")).expect("one");
        let second = store.record_installation(&installation("UDID1", "com.example.two", "/b.ipa")).expect("two");

        let early: DateTime<Utc> = "2026-09-01T00:00:00Z".parse().expect("date");
        let late: DateTime<Utc> = "2026-09-02T00:00:00Z".parse().expect("date");

        assert!(store.enqueue_refresh(second, "token-2", early).expect("queue second"));
        assert!(store.enqueue_refresh(first, "token-1", late).expect("queue first"));
        assert!(!store.enqueue_refresh(second, "token-3", late).expect("already queued"));

        let next = store.next_refresh().expect("next").expect("queued");
        assert_eq!(next.installation_id, second);

        let claimed = store.claim_refresh("claim-a", late, early).expect("claim").expect("entry");
        assert_eq!(claimed.installation_id, second);
        assert_eq!(
            store.claim_refresh("claim-b", late, early).expect("claim").map(|entry| entry.installation_id),
            Some(first)
        );
        assert!(
            store.claim_refresh("claim-c", late, early).expect("claim").is_none(),
            "claimed entries are not reissued"
        );

        let much_later: DateTime<Utc> = "2026-09-03T00:00:00Z".parse().expect("date");
        let reclaimed = store.claim_refresh("claim-d", much_later, much_later).expect("claim").expect("stale claim");
        assert_eq!(reclaimed.installation_id, second, "a stale claim can be taken over");
        assert!(store.dequeue_refresh(&reclaimed).expect("dequeue reclaimed"));
        assert!(store.enqueue_refresh(second, "token-2", early).expect("queue again"));

        let stale = QueuedRefresh { installation_id: second, token: "token-old".into() };
        assert!(!store.dequeue_refresh(&stale).expect("stale token"));
        assert!(store.dequeue_refresh(&next).expect("dequeue"));
        assert_eq!(store.next_refresh().expect("next").map(|entry| entry.installation_id), Some(first));
    }

    #[test]
    fn concurrent_connections_agree_on_first_inserted_metadata() {
        let directory = tempfile::tempdir().expect("data directory");
        let one = Store::open(directory.path()).expect("first connection");
        let two = Store::open(directory.path()).expect("second connection");

        assert_eq!(one.meta_or_insert("machine_id", "first").expect("first"), "first");
        assert_eq!(two.meta_or_insert("machine_id", "second").expect("second"), "first");

        two.set_meta("machine_id", "replaced").expect("replace");
        assert_eq!(one.meta("machine_id").expect("read").as_deref(), Some("replaced"));

        one.record_stored_file(&"b".repeat(64), "App.ipa", 42).expect("stored file");
        two.record_stored_file(&"b".repeat(64), "Again.ipa", 42).expect("idempotent stored file");
        one.delete_stored_file(&"b".repeat(64)).expect("delete stored file");

        let certificate = StoredCertificate { serial: "ABC".into(), der: vec![1, 2] };
        one.save_certificate("TEAM", &certificate).expect("certificate");
        assert_eq!(two.certificate("TEAM").expect("read"), Some(certificate));
        two.delete_certificate("TEAM").expect("delete");
        assert_eq!(one.certificate("TEAM").expect("read"), None);
    }
}
