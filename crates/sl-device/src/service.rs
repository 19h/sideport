//! Lockdown services opened through `idevice` and handed over as their (TLS-wrapped when the
//! service asks for it) socket, for services whose messages Sideport frames itself.

use crate::error::{DeviceError, Result};
use idevice::provider::IdeviceProvider;
use idevice::{Idevice, IdeviceError, IdeviceService, ReadWrite};
use std::borrow::Cow;
use std::marker::PhantomData;

/// A lockdown service identifier.
pub(crate) trait ServiceName: Send {
    const NAME: &'static str;
}

/// libimobiledevice's `debugserver_client_start_service` tries the TLS proxy first.
pub(crate) struct SecureDebugServer;

impl ServiceName for SecureDebugServer {
    const NAME: &'static str = "com.apple.debugserver.DVTSecureSocketProxy";
}

pub(crate) struct DebugServer;

impl ServiceName for DebugServer {
    const NAME: &'static str = "com.apple.debugserver";
}

/// The socket of a started service.
pub(crate) struct ServiceSocket<N> {
    pub(crate) socket: Box<dyn ReadWrite>,
    name: PhantomData<fn() -> N>,
}

impl<N: ServiceName> IdeviceService for ServiceSocket<N> {
    fn service_name() -> Cow<'static, str> {
        Cow::Borrowed(N::NAME)
    }

    async fn from_stream(idevice: Idevice) -> std::result::Result<Self, IdeviceError> {
        let socket = idevice.get_socket().ok_or(IdeviceError::NoEstablishedConnection)?;

        Ok(Self { socket, name: PhantomData })
    }
}

/// Start `N` over a paired lockdown session and return its socket.
pub(crate) async fn open<N: ServiceName>(provider: &dyn IdeviceProvider) -> Result<Box<dyn ReadWrite>> {
    let service = ServiceSocket::<N>::connect(provider).await.map_err(|error| started(N::NAME, error))?;

    Ok(service.socket)
}

/// Keep pairing states as they are (the caller asks the user); describe everything else with
/// the service name, as the recovered `enrichSvcError` does.
fn started(name: &str, error: IdeviceError) -> DeviceError {
    match DeviceError::from(error) {
        error @ (DeviceError::PasswordProtected
        | DeviceError::UserDeniedPairing
        | DeviceError::PairingDialogPending
        | DeviceError::NotPaired
        | DeviceError::NotConnected(_)
        | DeviceError::Interrupted(_)) => error,
        other => DeviceError::Protocol(format!("could not start {name}: {other}")),
    }
}
