//! Durable account, session, password, machine and signing-key state.
//!
//! Recovered layout: `sessions.json` (DSID and GS token per account), `key.pem` (one reusable
//! RSA-2048 key per machine), `machine_id.txt` (UUID sent with CSRs). Sideport keeps the
//! non-secret parts in the state database and the rest in the secret store.

use super::{Inner, LiveAccount};
use crate::error::{EngineError, Result};
use crate::secrets::SecretStore;
use crate::store::Store;
use crate::types::{AccountSummary, SessionImport};
use rsa::RsaPrivateKey;
use serde::{Deserialize, Serialize};
use sl_apple::auth::AuthSession;
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

const SIGNING_KEY: &str = "signing-key";
const REJECTED_SIGNING_KEY: &str = "signing-key.bad";
const MACHINE_ID: &str = "machine_id";

#[derive(Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
struct SessionSecret {
    dsid: String,
    token: String,
    using_alternate: bool,
}

fn session_key(apple_id: &str) -> String {
    format!("session/{apple_id}")
}

fn password_key(apple_id: &str) -> String {
    format!("password/{apple_id}")
}

/// Accounts from the state database, each with its stored session when one is present.
pub(super) fn restore_accounts(store: &Store, secrets: &dyn SecretStore) -> Result<BTreeMap<String, LiveAccount>> {
    let mut accounts = BTreeMap::new();

    for summary in store.accounts()? {
        let session = match secrets.get(&session_key(&summary.apple_id))? {
            Some(bytes) => Some(Arc::new(decode_session(&summary.apple_id, &bytes)?)),
            None => None,
        };

        accounts.insert(summary.apple_id.clone(), LiveAccount { summary, session });
    }

    Ok(accounts)
}

fn decode_session(apple_id: &str, bytes: &[u8]) -> Result<AuthSession> {
    let secret: SessionSecret =
        serde_json::from_slice(bytes).map_err(|_| EngineError::Storage("stored session is corrupted".into()))?;

    let dsid = Zeroizing::new(secret.dsid.clone());
    let token = Zeroizing::new(secret.token.clone());

    AuthSession::restore(apple_id.into(), dsid, token, secret.using_alternate)
        .map_err(|_| EngineError::Storage("stored session is invalid".into()))
}

/// Persist a completed login: account record, session and the remember-password choice.
pub(super) fn save_login(
    inner: &Inner,
    summary: &AccountSummary,
    session: &AuthSession,
    password: Option<&str>,
) -> Result<()> {
    let secret = SessionSecret {
        dsid: session.dsid().into(),
        token: session.token().into(),
        using_alternate: session.using_alternate(),
    };
    let encoded = Zeroizing::new(serde_json::to_vec(&secret).map_err(|error| EngineError::Storage(error.to_string()))?);

    inner.secrets.set(&session_key(&summary.apple_id), &encoded)?;

    match password {
        Some(password) => inner.secrets.set(&password_key(&summary.apple_id), password.as_bytes())?,
        None => inner.secrets.delete(&password_key(&summary.apple_id))?,
    }

    inner.store.save_account(summary)
}

pub(super) fn save_summary(inner: &Inner, summary: &AccountSummary) -> Result<()> {
    inner.store.save_account(summary)
}

pub(super) fn remembered_password(inner: &Inner, apple_id: &str) -> Result<Option<Zeroizing<String>>> {
    let Some(bytes) = inner.secrets.get(&password_key(apple_id))? else {
        return Ok(None);
    };

    let password =
        String::from_utf8(bytes.to_vec()).map_err(|_| EngineError::Storage("stored password is invalid".into()))?;

    Ok(Some(Zeroizing::new(password)))
}

pub(super) fn forget_password(inner: &Inner, apple_id: &str) -> Result<()> {
    inner.secrets.delete(&password_key(apple_id))
}

/// Remove an account and every secret held for it.
pub(super) fn forget_account(inner: &Inner, apple_id: &str) -> Result<()> {
    inner.secrets.delete(&session_key(apple_id))?;
    inner.secrets.delete(&password_key(apple_id))?;

    inner.store.delete_account(apple_id)
}

/// Upper bound for the recovered client's `sessions.json`.
const MAX_RECOVERED_SESSIONS_BYTES: u64 = 1024 * 1024;

/// The recovered client's session file: `~/Library/Application Support/sideloadly/sessions.json`
/// on macOS, `%APPDATA%\sideloadly` on Windows and `$XDG_CONFIG_HOME/sideloadly` elsewhere.
pub(super) fn recovered_sessions_path() -> Option<std::path::PathBuf> {
    let base = if cfg!(any(target_os = "macos", windows)) { dirs::data_dir() } else { dirs::config_dir() };

    base.map(|directory| directory.join("sideloadly").join("sessions.json"))
}

/// Import GSA sessions from the recovered `sessions.json` (keys `<apple id>:a`, `_type`
/// `GsaAuthenticator`, fields `dsid`, `gs_token`, `using_alt_anisette`). Legacy IDMS (`:i`)
/// sessions and pointer keys are skipped with a reason. Imported tokens are validated only by
/// shape; the next portal request establishes whether Apple still accepts them.
pub(super) fn import_recovered_sessions(inner: &Inner, path: &std::path::Path) -> Result<SessionImport> {
    use std::io::Read;

    let file = std::fs::File::open(path).map_err(|error| EngineError::Storage(error.to_string()))?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_RECOVERED_SESSIONS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| EngineError::Storage(error.to_string()))?;

    if bytes.len() as u64 > MAX_RECOVERED_SESSIONS_BYTES {
        return Err(EngineError::Storage("sessions file exceeds 1 MiB".into()));
    }

    let sessions: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(&bytes)
        .map_err(|_| EngineError::Storage("sessions file is not a JSON object".into()))?;

    let mut report = SessionImport::default();

    for (key, value) in &sessions {
        if key.starts_with("latest") {
            continue;
        }

        let Some(apple_id) = key.strip_suffix(":a") else {
            report.skipped.push((key.clone(), "legacy IDMS sessions are not supported".into()));

            continue;
        };

        match import_session(inner, apple_id, value) {
            Ok(()) => report.imported.push(apple_id.into()),
            Err(reason) => report.skipped.push((apple_id.into(), reason)),
        }
    }

    Ok(report)
}

fn import_session(inner: &Inner, apple_id: &str, value: &serde_json::Value) -> std::result::Result<(), String> {
    let field = |name: &str| value.get(name).and_then(serde_json::Value::as_str);

    if field("_type") != Some("GsaAuthenticator") {
        return Err("not a GrandSlam session".into());
    }

    let dsid = match value.get("dsid") {
        Some(serde_json::Value::String(dsid)) => dsid.clone(),
        Some(serde_json::Value::Number(dsid)) => dsid.to_string(),
        _ => return Err("missing DSID".into()),
    };

    let token = field("gs_token").ok_or("missing token")?;
    let using_alternate = value.get("using_alt_anisette").and_then(serde_json::Value::as_bool).unwrap_or(false);

    let session =
        AuthSession::restore(apple_id.into(), Zeroizing::new(dsid), Zeroizing::new(token.into()), using_alternate)
            .map_err(|_| "invalid session fields".to_owned())?;

    if inner.accounts.lock().get(apple_id).is_some_and(|account| account.session.is_some()) {
        return Err("a Sideport session already exists".into());
    }

    let summary = AccountSummary {
        apple_id: apple_id.into(),
        teams: Vec::new(),
        default_team: None,
        has_session: true,
        remembers_password: false,
        last_login: None,
    };

    save_login(inner, &summary, &session, None).map_err(|error| error.to_string())?;
    inner.accounts.lock().insert(apple_id.into(), LiveAccount { summary, session: Some(Arc::new(session)) });

    Ok(())
}

/// The UUID sent as `machineId` with development CSRs, created once per data directory.
pub(crate) fn machine_id(store: &Store) -> Result<Uuid> {
    let candidate = Uuid::new_v4().to_string();
    let stored = store.meta_or_insert(MACHINE_ID, &candidate)?;

    if let Ok(machine_id) = Uuid::parse_str(&stored) {
        return Ok(machine_id);
    }

    // Recovered behavior: a corrupt machine identifier is replaced.
    store.set_meta(MACHINE_ID, &candidate)?;

    Uuid::parse_str(&candidate).map_err(|error| EngineError::Storage(error.to_string()))
}

/// The machine's reusable signing key. `create` generates and stores one when absent, holding the
/// database write lock so concurrent processes agree on one key. An undecodable stored key is
/// preserved under another name and replaced, as recovered.
pub(crate) fn signing_key(inner: &Inner, create: bool) -> Result<Option<RsaPrivateKey>> {
    let _guard = inner.key_lock.lock();

    if !create {
        return load_signing_key(inner, false);
    }

    inner.store.exclusive(|| load_signing_key(inner, true))
}

fn load_signing_key(inner: &Inner, create: bool) -> Result<Option<RsaPrivateKey>> {
    if let Some(bytes) = inner.secrets.get(SIGNING_KEY)? {
        match sl_codesign::decode_signing_key(&bytes) {
            Ok(key) => return Ok(Some(key)),

            Err(_) => {
                inner.secrets.set(REJECTED_SIGNING_KEY, &bytes)?;
                inner.secrets.delete(SIGNING_KEY)?;
            }
        }
    }

    if !create {
        return Ok(None);
    }

    let key = sl_codesign::generate_signing_key().map_err(|error| EngineError::Signing(error.to_string()))?;
    let encoded = sl_codesign::encode_signing_key(&key).map_err(|error| EngineError::Signing(error.to_string()))?;

    inner.secrets.set(SIGNING_KEY, &encoded)?;

    Ok(Some(key))
}

/// Whether the certificate was issued for this machine's stored signing key.
pub(crate) fn owns_certificate(key: Option<&RsaPrivateKey>, certificate_der: Option<&[u8]>) -> bool {
    match (key, certificate_der) {
        (Some(key), Some(der)) => sl_codesign::certificate_matches_key(der, key).unwrap_or(false),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, EngineConfig};

    fn engine(directory: &std::path::Path) -> Engine {
        let config = EngineConfig {
            data_dir: Some(directory.into()),
            file_secrets: true,
            disable_scheduler: true,
            ..EngineConfig::default()
        };

        Engine::new(config).expect("engine")
    }

    fn write_sessions(directory: &std::path::Path, value: serde_json::Value) -> std::path::PathBuf {
        let path = directory.join("sessions.json");
        std::fs::write(&path, serde_json::to_vec(&value).expect("JSON")).expect("sessions");

        path
    }

    #[test]
    fn recovered_sessions_import_gsa_entries_and_explain_skipped_ones() {
        let directory = tempfile::tempdir().expect("data directory");
        let engine = engine(&directory.path().join("data"));

        let sessions = serde_json::json!({
            "numeric@example.test:a": { "_type": "GsaAuthenticator", "dsid": 123456789, "gs_token": "private-value" },
            "alternate@example.test:a": {
                "_type": "GsaAuthenticator", "dsid": "42", "gs_token": "private-value", "using_alt_anisette": true
            },
            "control@example.test:a": { "_type": "GsaAuthenticator", "dsid": "1", "gs_token": "private\nvalue" },
            "missing@example.test:a": { "_type": "GsaAuthenticator", "dsid": "1" },
            "other@example.test:a": { "_type": "Unknown" },
            "legacy@example.test:i": { "_type": "IdmsAuthenticator" },
            "latest:a": "numeric@example.test",
            "latest": "numeric@example.test",
        });

        let report = engine.import_sessions(write_sessions(directory.path(), sessions)).expect("import");

        assert_eq!(report.imported, ["alternate@example.test", "numeric@example.test"]);
        assert_eq!(report.skipped.len(), 4, "{:?}", report.skipped);
        assert!(!format!("{report:?}").contains("private"));

        let restored = engine.inner.accounts.lock();
        let alternate = restored["alternate@example.test"].session.as_ref().expect("session");
        assert!(alternate.using_alternate());
        assert_eq!(restored["numeric@example.test"].session.as_ref().expect("session").dsid(), "123456789");
        drop(restored);

        let again = serde_json::json!({ "numeric@example.test:a": { "_type": "GsaAuthenticator", "dsid": "9", "gs_token": "t" } });
        let report = engine.import_sessions(write_sessions(directory.path(), again)).expect("second import");
        assert!(report.imported.is_empty(), "existing Sideport sessions are kept");

        std::fs::write(directory.path().join("sessions.json"), b"[]").expect("array");
        assert!(engine.import_sessions(directory.path().join("sessions.json")).is_err());
    }

    #[test]
    fn keys_machine_ids_and_sessions_survive_restart_and_corruption() {
        let directory = tempfile::tempdir().expect("data directory");
        let first = engine(directory.path());

        assert!(signing_key(&first.inner, false).expect("absent").is_none());
        let key = signing_key(&first.inner, true).expect("create").expect("key");
        let machine = machine_id(&first.inner.store).expect("machine id");

        let session = AuthSession::restore(
            "fixture@example.test".into(),
            Zeroizing::new("42".into()),
            Zeroizing::new("token".into()),
            false,
        )
        .expect("session");
        let summary = AccountSummary {
            apple_id: "fixture@example.test".into(),
            teams: Vec::new(),
            default_team: None,
            has_session: true,
            remembers_password: true,
            last_login: None,
        };
        save_login(&first.inner, &summary, &session, Some("secret")).expect("save login");
        drop(first);

        let second = engine(directory.path());
        assert_eq!(signing_key(&second.inner, false).expect("stored").expect("key"), key);
        assert_eq!(machine_id(&second.inner.store).expect("machine id"), machine);
        assert_eq!(
            remembered_password(&second.inner, "fixture@example.test")
                .expect("password")
                .as_deref()
                .map(String::as_str),
            Some("secret")
        );
        assert!(second.accounts().expect("accounts")[0].has_session);

        second.inner.secrets.set(SIGNING_KEY, b"not a key").expect("corrupt key");
        second.inner.secrets.delete(&session_key("fixture@example.test")).expect("remove session");
        drop(second);

        let third = engine(directory.path());
        assert!(!third.accounts().expect("accounts")[0].has_session, "a missing session requires signing in again");
        assert!(signing_key(&third.inner, false).expect("rejected").is_none());
        assert_eq!(
            third.inner.secrets.get(REJECTED_SIGNING_KEY).expect("kept").as_deref().map(Vec::as_slice),
            Some(&b"not a key"[..])
        );

        let replacement = signing_key(&third.inner, true).expect("replace").expect("key");
        assert_ne!(replacement, key);

        futures::executor::block_on(third.logout("fixture@example.test".into())).expect("logout");
        assert!(third.accounts().expect("accounts").is_empty());
        assert!(remembered_password(&third.inner, "fixture@example.test").expect("password").is_none());
    }
}
