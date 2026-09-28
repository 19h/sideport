//! Anisette provider selection.
//!
//! Recovered chain: Private (local AOSKit) → Mail plug-in → Remote. The Mail plug-in is
//! unsupported since macOS Sonoma in the recovered client itself, so Sideport tries the local
//! provider and falls back to the configured alternate provider for the job.

use super::Inner;
use crate::error::{EngineError, Result};
use crate::job::JobContext;
use crate::types::AnisetteSetting;
use sl_apple::anisette::{LocalAnisette, MachineSource, RemoteAnisette};
use sl_apple::auth::{AnisetteProvider, AuthSession};
use std::fmt;
use std::sync::Arc;

/// Local machine anisette for [`crate::EngineConfig`]; the default on macOS is AOSKit.
#[derive(Clone)]
pub struct MachineAnisette(pub Arc<dyn MachineSource>);

impl fmt::Debug for MachineAnisette {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("MachineAnisette").finish_non_exhaustive()
    }
}

pub(super) fn default_machine() -> Option<Arc<dyn MachineSource>> {
    #[cfg(target_os = "macos")]
    {
        Some(Arc::new(sl_macos::AosKitSource))
    }

    #[cfg(not(target_os = "macos"))]
    None
}

pub(super) fn provider(inner: &Inner, setting: &AnisetteSetting) -> Result<Arc<dyn AnisetteProvider>> {
    match setting {
        AnisetteSetting::Remote { url } => {
            let remote = RemoteAnisette::new(url).map_err(|error| EngineError::Anisette(error.to_string()))?;

            Ok(Arc::new(remote))
        }

        AnisetteSetting::Local => {
            let source = inner
                .machine
                .clone()
                .ok_or_else(|| EngineError::Unsupported("local anisette requires macOS".into()))?;

            Ok(Arc::new(LocalAnisette::new(source)))
        }
    }
}

/// The primary provider for a job. A local provider is probed; if it cannot produce headers,
/// the alternate provider is used instead, else the job fails with guidance.
pub(super) async fn primary(inner: &Inner, context: Option<&JobContext>) -> Result<Arc<dyn AnisetteProvider>> {
    let settings = inner.settings();
    let configured = provider(inner, &settings.anisette)?;

    if settings.anisette != AnisetteSetting::Local {
        return Ok(configured);
    }

    let probe = configured.headers("").await;

    match (probe, &settings.alternate_anisette) {
        (Ok(_), _) => Ok(configured),

        (Err(error), Some(alternate)) => {
            if let Some(context) = context {
                context.warn(format!("Local anisette is unavailable ({error}); using the alternate provider."));
            }

            provider(inner, alternate)
        }

        (Err(error), None) => {
            Err(EngineError::Anisette(format!("local anisette is unavailable on this computer ({error})")))
        }
    }
}

/// The optional alternate provider tried after a GSA anisette mismatch (-36607).
pub(super) fn alternate(inner: &Inner) -> Result<Option<Arc<dyn AnisetteProvider>>> {
    let settings = inner.settings();

    settings.alternate_anisette.as_ref().map(|setting| provider(inner, setting)).transpose()
}

/// The provider a session was established with; portal requests must use the same machine.
pub(super) async fn for_session(
    inner: &Inner,
    session: &AuthSession,
    context: Option<&JobContext>,
) -> Result<Arc<dyn AnisetteProvider>> {
    if session.using_alternate() {
        return alternate(inner)?
            .ok_or_else(|| EngineError::Anisette("alternate anisette provider is no longer configured".into()));
    }

    primary(inner, context).await
}

/// Describe the machine Apple will list for this account (recovered `describe_anisette`).
pub(super) async fn describe(provider: &dyn AnisetteProvider, apple_id: &str) -> Result<String> {
    let headers = provider.headers(apple_id).await.map_err(|error| EngineError::Anisette(error.to_string()))?;

    Ok(headers.description())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, EngineConfig};
    use futures::FutureExt;
    use futures::future::BoxFuture;
    use sl_apple::anisette::MachineValues;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zeroize::Zeroizing;

    #[derive(Debug)]
    struct Machine {
        working: bool,
    }

    impl MachineSource for Machine {
        fn values(&self) -> BoxFuture<'_, sl_apple::Result<MachineValues>> {
            let working = self.working;

            async move {
                if !working {
                    return Err(sl_apple::Error::Invalid("AOSKit returned no one-time password"));
                }

                Ok(MachineValues {
                    otp: Zeroizing::new("otp".into()),
                    machine_token: Zeroizing::new("token".into()),
                    serial: "C02LOCAL".into(),
                    device_id: "LOCAL-DEVICE".into(),
                    locale: "en".into(),
                    time_zone: "UTC".into(),
                    hardware_model: "Mac16,5".into(),
                    os_version: "27.2".into(),
                    os_build: "26B5091g".into(),
                })
            }
            .boxed()
        }
    }

    fn engine(directory: &std::path::Path, working: bool, alternate: Option<String>) -> Engine {
        let config = EngineConfig {
            data_dir: Some(directory.into()),
            machine_anisette: Some(MachineAnisette(Arc::new(Machine { working }))),
            file_secrets: true,
            disable_scheduler: true,
            ..EngineConfig::default()
        };
        let engine = Engine::new(config).expect("engine");

        let mut settings = engine.settings();
        settings.anisette = AnisetteSetting::Local;
        settings.alternate_anisette = alternate.map(|url| AnisetteSetting::Remote { url });
        engine.update_settings(settings).expect("settings");

        engine
    }

    async fn remote() -> MockServer {
        let server = MockServer::start().await;
        let headers = serde_json::json!({
            "X-Apple-I-MD": "remote-otp",
            "X-Apple-I-MD-M": "remote-token",
            "X-Mme-Device-Id": "REMOTE-DEVICE",
            "X-MMe-Client-Info": "<iMac20,1> <macOS;14.0;23A344> <com.apple.AuthKit/1>",
            "X-Apple-Locale": "en_US",
            "X-Apple-I-SRL-NO": "C02REMOTE",
        });

        Mock::given(method("GET"))
            .and(path("/anisette"))
            .respond_with(ResponseTemplate::new(200).set_body_json(headers))
            .mount(&server)
            .await;

        server
    }

    #[tokio::test]
    async fn local_anisette_is_used_when_the_machine_answers() {
        let directory = tempfile::tempdir().expect("data directory");
        let engine = engine(directory.path(), true, None);

        let chosen = primary(&engine.inner, None).await.expect("local provider");
        let description = describe(chosen.as_ref(), "fixture@example.test").await.expect("description");

        assert_eq!(description, "Mac16,5 with serial number C02LOCAL running macOS 27.2 26B5091g");
        assert_eq!(engine.test_anisette(AnisetteSetting::Local).await.expect("test"), description);
    }

    #[tokio::test]
    async fn an_unavailable_local_provider_falls_back_to_the_alternate_or_explains() {
        let server = remote().await;
        let directory = tempfile::tempdir().expect("data directory");

        let with_alternate = engine(directory.path(), false, Some(format!("{}/anisette", server.uri())));
        let (context, events, _) = JobContext::channel();
        let chosen = primary(&with_alternate.inner, Some(&context)).await.expect("fallback provider");
        let description = describe(chosen.as_ref(), "fixture@example.test").await.expect("description");

        assert!(description.contains("C02REMOTE"), "{description}");
        drop(context);

        let warned = std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| matches!(event, crate::job::JobEvent::Log { message, .. } if message.contains("alternate provider")));
        assert!(warned);

        let second = tempfile::tempdir().expect("data directory");
        let without = engine(second.path(), false, None);
        let error = primary(&without.inner, None).await.expect_err("no fallback");

        assert!(
            matches!(&error, EngineError::Anisette(message) if message.contains("local anisette is unavailable")),
            "{error}"
        );
    }
}
