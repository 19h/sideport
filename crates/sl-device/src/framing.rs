//! Lockdown service framing: length-prefixed plists and raw bytes over one service socket.

use crate::error::{DeviceError, Result};
use futures::FutureExt;
use futures::future::BoxFuture;
use idevice::ReadWrite;
use plist::{Dictionary, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Largest plist message accepted from a device.
const MAX_MESSAGE_BYTES: u32 = 16 * 1024 * 1024;

/// A service that exchanges length-prefixed plists and, for uploads, raw bytes. The image
/// mounter uses it; tests implement it with fakes that interpret the messages.
pub trait PlistChannel: Send {
    fn send<'a>(&'a mut self, message: &'a Dictionary) -> BoxFuture<'a, Result<()>>;

    fn receive(&mut self) -> BoxFuture<'_, Result<Dictionary>>;

    fn send_raw<'a>(&'a mut self, bytes: &'a [u8]) -> BoxFuture<'a, Result<()>>;
}

/// Length-prefixed plist messages (32-bit big-endian length, then the plist) over a service
/// socket, as lockdown services frame them.
pub(crate) struct PlistStream {
    socket: Box<dyn ReadWrite>,
}

impl PlistStream {
    pub(crate) fn new(socket: Box<dyn ReadWrite>) -> Self {
        Self { socket }
    }

    pub(crate) async fn send(&mut self, message: &Dictionary) -> Result<()> {
        let mut body = Vec::new();
        Value::Dictionary(message.clone())
            .to_writer_xml(&mut body)
            .map_err(|error| DeviceError::Protocol(error.to_string()))?;

        let length = u32::try_from(body.len()).map_err(|_| DeviceError::Protocol("message too large".into()))?;

        self.socket.write_all(&length.to_be_bytes()).await.map_err(io_error)?;
        self.socket.write_all(&body).await.map_err(io_error)?;
        self.socket.flush().await.map_err(io_error)
    }

    pub(crate) async fn receive(&mut self) -> Result<Dictionary> {
        let mut header = [0; 4];
        self.socket.read_exact(&mut header).await.map_err(io_error)?;

        let length = u32::from_be_bytes(header);

        if length > MAX_MESSAGE_BYTES {
            return Err(DeviceError::Protocol(format!("device message of {length} bytes exceeds 16 MiB")));
        }

        let mut body = vec![0; length as usize];
        self.socket.read_exact(&mut body).await.map_err(io_error)?;

        let value =
            Value::from_reader(std::io::Cursor::new(body)).map_err(|error| DeviceError::Protocol(error.to_string()))?;

        value.into_dictionary().ok_or_else(|| DeviceError::Protocol("device message is not a dictionary".into()))
    }

    pub(crate) async fn send_raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.socket.write_all(bytes).await.map_err(io_error)?;
        self.socket.flush().await.map_err(io_error)
    }
}

impl PlistChannel for PlistStream {
    fn send<'a>(&'a mut self, message: &'a Dictionary) -> BoxFuture<'a, Result<()>> {
        PlistStream::send(self, message).boxed()
    }

    fn receive(&mut self) -> BoxFuture<'_, Result<Dictionary>> {
        PlistStream::receive(self).boxed()
    }

    fn send_raw<'a>(&'a mut self, bytes: &'a [u8]) -> BoxFuture<'a, Result<()>> {
        PlistStream::send_raw(self, bytes).boxed()
    }
}

pub(crate) fn io_error(error: std::io::Error) -> DeviceError {
    DeviceError::Interrupted(error.to_string())
}
