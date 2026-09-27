//! GrandSlam request, proof, alternate-provider and second-factor orchestration.

mod factor;
mod request;

pub use factor::{FactorDelegate, FactorPrompt, FactorReply};

use crate::anisette::{AnisetteHeaders, RemoteAnisette};
use crate::srp::{Challenge, Continuation, PasswordProtocol, SessionData, SrpClient, XCODE_APP};
use crate::wire::{self, SecretValue, dictionary};
use crate::{Error, Result, transport};
use futures::FutureExt;
use futures::future::BoxFuture;
use plist::Value;
use reqwest::{Client, Url};
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

pub(super) use crate::transport::{cancellable, checkpoint};

pub trait AnisetteProvider: fmt::Debug + Send + Sync {
    fn headers<'a>(&'a self, username: &'a str) -> BoxFuture<'a, Result<AnisetteHeaders>>;
}

impl AnisetteProvider for RemoteAnisette {
    fn headers<'a>(&'a self, username: &'a str) -> BoxFuture<'a, Result<AnisetteHeaders>> {
        RemoteAnisette::headers(self, Some(username)).boxed()
    }
}

#[derive(Clone)]
pub struct AnisetteSources {
    primary: Arc<dyn AnisetteProvider>,
    alternate: Option<Arc<dyn AnisetteProvider>>,
}

impl fmt::Debug for AnisetteSources {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnisetteSources")
            .field("has_alternate", &self.alternate.is_some())
            .finish_non_exhaustive()
    }
}

impl AnisetteSources {
    pub fn new(primary: Arc<dyn AnisetteProvider>) -> Self {
        Self { primary, alternate: None }
    }

    pub fn with_alternate(mut self, alternate: Arc<dyn AnisetteProvider>) -> Self {
        self.alternate = Some(alternate);

        self
    }
}

pub struct AuthSession {
    username: String,
    dsid: Zeroizing<String>,
    token: Zeroizing<String>,
    using_alternate: bool,
}

impl fmt::Debug for AuthSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("AuthSession").field("using_alternate", &self.using_alternate).finish_non_exhaustive()
    }
}

impl AuthSession {
    #[cfg(test)]
    pub(crate) fn fixture(username: &str, dsid: &str, token: &str) -> Self {
        Self {
            username: username.into(),
            dsid: Zeroizing::new(dsid.into()),
            token: Zeroizing::new(token.into()),
            using_alternate: false,
        }
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn dsid(&self) -> &str {
        &self.dsid
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn using_alternate(&self) -> bool {
        self.using_alternate
    }
}

#[derive(Clone)]
pub struct AuthClient {
    client: Client,
    origin: Url,
    #[cfg(test)]
    ephemeral: Option<[u8; 32]>,
}

impl fmt::Debug for AuthClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("AuthClient").finish_non_exhaustive()
    }
}

enum Exchange {
    Complete(AuthSession),
    SecondFactor { data: SessionData, unlock: String },
}

impl AuthClient {
    pub fn new() -> Result<Self> {
        Self::with_origin("https://gsa.apple.com")
    }

    /// HTTPS origins and loopback HTTP fixtures share the recovered relative paths.
    pub fn with_origin(origin: &str) -> Result<Self> {
        let origin = Url::parse(origin).map_err(|_| Error::Invalid("authentication origin"))?;
        let host = origin.host_str().unwrap_or_default().trim_matches(['[', ']']);
        let loopback = host.parse::<IpAddr>().is_ok_and(|address| address.is_loopback());
        let permitted_scheme = origin.scheme() == "https" || origin.scheme() == "http" && loopback;
        let clean_origin = origin.path() == "/"
            && origin.query().is_none()
            && origin.fragment().is_none()
            && origin.username().is_empty()
            && origin.password().is_none();

        if !permitted_scheme || host.is_empty() || !clean_origin {
            return Err(Error::Invalid("authentication origin"));
        }

        Ok(Self {
            client: transport::client_with_cookies(true)?,
            origin,
            #[cfg(test)]
            ephemeral: None,
        })
    }

    pub async fn login(
        &self,
        username: String,
        password: Zeroizing<String>,
        sources: &AnisetteSources,
        delegate: &dyn FactorDelegate,
        cancellation: &CancellationToken,
    ) -> Result<AuthSession> {
        let mut using_alternate = false;
        let mut verified_factor = false;

        loop {
            let provider = if using_alternate {
                sources.alternate.as_deref().ok_or(Error::Invalid("alternate anisette provider"))?
            } else {
                sources.primary.as_ref()
            };
            let exchange = self.exchange(&username, &password, provider, cancellation).await;

            match exchange {
                Ok(Exchange::Complete(mut session)) => {
                    checkpoint(cancellation)?;
                    session.using_alternate = using_alternate;

                    return Ok(session);
                }
                Ok(Exchange::SecondFactor { data, unlock }) => {
                    if verified_factor {
                        return Err(Error::RetryLimit("second-factor login restart"));
                    }

                    self.verify_factor(&username, data, &unlock, provider, delegate, cancellation).await?;
                    verified_factor = true;
                }
                Err(Error::Service { operation: "complete", code: -36607 })
                    if !using_alternate && sources.alternate.is_some() =>
                {
                    using_alternate = true;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn exchange(
        &self,
        username: &str,
        password: &str,
        provider: &dyn AnisetteProvider,
        cancellation: &CancellationToken,
    ) -> Result<Exchange> {
        let identity = username.to_owned();
        #[cfg(test)]
        let ephemeral = self.ephemeral;

        let srp = crypto(cancellation, move || {
            #[cfg(test)]
            if let Some(ephemeral) = ephemeral {
                return SrpClient::with_ephemeral(identity, ephemeral);
            }

            SrpClient::new(identity)
        })
        .await?;
        let init = dictionary([
            ("A2k", Value::Data(srp.public_key())),
            ("ps", Value::Array(vec![Value::String("s2k".into()), Value::String("s2k_fo".into())])),
        ]);
        let response = self.gsa("init", username, username, init, provider, cancellation).await?;
        let init = wire::dict(&response.0)?;

        let protocol = PasswordProtocol::parse(wire::string(init, "sp")?)?;
        let iterations =
            u32::try_from(wire::integer(init, "i")?).map_err(|_| Error::Invalid("PBKDF2 iteration count"))?;
        let salt = Zeroizing::new(wire::data(init, "s")?.to_vec());
        let server_public = wire::data(init, "B")?.to_vec();
        let continuation = SecretValue(init.get("c").cloned().ok_or(Error::Invalid("init continuation"))?);
        let password = Zeroizing::new(password.to_owned());

        let proof = crypto(cancellation, move || {
            let challenge = Challenge { protocol, iterations, salt: &salt, server_public: &server_public };

            srp.process(&password, challenge)
        })
        .await?;
        let complete = dictionary([("M1", Value::Data(proof.client_proof().to_vec())), ("c", continuation.0.clone())]);
        let response = self.gsa("complete", username, username, complete, provider, cancellation).await?;
        let complete = wire::dict(&response.0)?;
        let verified = proof.verify(wire::data(complete, "M2")?)?;
        let context = match complete.get("sc") {
            Some(Value::Data(context)) => context.as_slice(),
            None => &[],
            _ => return Err(Error::Invalid("negotiation context")),
        };
        let data = verified.decrypt_session_data(wire::data(complete, "spd")?, context, wire::data(complete, "np")?)?;
        let status = wire::dict(complete.get("Status").ok_or(Error::Invalid("complete status"))?)?;
        let unlock = status.get("au").and_then(Value::as_string).unwrap_or_default();

        if !unlock.is_empty() {
            match unlock {
                "repair" => return Err(Error::AccountAction("repair")),
                "securityUpgrade" => return Err(Error::AccountAction("security upgrade")),
                _ => return Ok(Exchange::SecondFactor { data, unlock: unlock.into() }),
            }
        }

        let continuation = match data.continuation() {
            Continuation::Text(value) => Value::String(value.to_string()),
            Continuation::Data(value) => Value::Data(value.to_vec()),
        };
        let tokens = dictionary([
            ("c", continuation),
            ("app", Value::Array(vec![Value::String(XCODE_APP.into())])),
            ("t", Value::String(data.idms_token().into())),
            ("checksum", Value::Data(data.app_token_checksum().to_vec())),
        ]);
        let response = self.gsa("apptokens", data.dsid(), username, tokens, provider, cancellation).await?;
        let tokens = wire::dict(&response.0)?;
        let token = data.decrypt_app_token(wire::data(tokens, "et")?)?;

        Ok(Exchange::Complete(AuthSession {
            username: username.into(),
            dsid: Zeroizing::new(data.dsid().into()),
            token,
            using_alternate: false,
        }))
    }
}

async fn crypto<T: Send + 'static>(
    cancellation: &CancellationToken,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    checkpoint(cancellation)?;
    let mut worker = tokio::task::spawn_blocking(work);

    tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            let _ = worker.await;

            Err(Error::Cancelled)
        }
        result = &mut worker => {
            checkpoint(cancellation)?;

            result.map_err(|_| Error::Worker)?
        }
    }
}

#[cfg(test)]
mod tests;
