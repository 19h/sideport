//! Developer-disk-image mounting over `com.apple.mobile.mobile_image_mounter`.
//!
//! [`ImageMounting`] is the small set of image-mounter operations the recovered mount flow
//! (`sideloadly/mobdev` `MountDeveloperImage`/`MountPersonalizedImage`) needs. [`IdeviceMounter`]
//! implements it with `idevice`; tests implement it with a fake so the DDI orchestration in
//! [`crate::ddi`] runs without hardware.

use crate::error::{DeviceError, Result};
use crate::mux::Mux;
use futures::FutureExt;
use futures::future::BoxFuture;
use idevice::IdeviceService;
use idevice::lockdown::LockdownClient;
use idevice::mobile_image_mounter::ImageMounter;
use idevice::provider::{IdeviceProvider, UsbmuxdProvider};
use plist::{Dictionary, Value};

/// Image-mounter image types (the `ImageType`/`PersonalizedImageType` strings).
pub const DEVELOPER: &str = "Developer";
pub const PERSONALIZED: &str = "Personalized";

/// The image type the personalized TSS/manifest queries use, as the recovered client and
/// `idevice` send it.
pub const PERSONALIZED_QUERY: &str = "DeveloperDiskImage";

/// Which developer image, if any, is mounted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mounted {
    Developer,
    Personalized,
}

/// The image-mounter operations used by the DDI mount flow.
///
/// Errors follow [`DeviceError`]; a closed socket after a failed personalized query is recovered
/// with [`ImageMounting::reconnect`], as the recovered client and `idevice` both do.
pub trait ImageMounting: Send {
    /// The mounted developer image, or `None` when none is mounted (recovered `IsMounted`:
    /// `LookupImage` for `Developer` then `Personalized`).
    fn mounted(&mut self) -> BoxFuture<'_, Result<Option<Mounted>>>;

    /// Upload an image and mount it. `trust_cache`/`info` are set for personalized images only.
    fn mount<'a>(
        &'a mut self,
        image_type: &'a str,
        image: &'a [u8],
        signature: Vec<u8>,
        trust_cache: Option<Vec<u8>>,
        info: Option<Value>,
    ) -> BoxFuture<'a, Result<()>>;

    /// A personalization manifest the device already holds for `image` (`QueryPersonalizationManifest`).
    /// `None` means the device has none and the caller must fetch one from TSS.
    fn device_manifest<'a>(&'a mut self, image: &'a [u8]) -> BoxFuture<'a, Result<Option<Vec<u8>>>>;

    /// `QueryPersonalizationIdentifiers` (board/chip/ECID and `Ap,*` tags).
    fn personalization_identifiers(&mut self) -> BoxFuture<'_, Result<Dictionary>>;

    /// `QueryNonce` for the developer disk image.
    fn nonce(&mut self) -> BoxFuture<'_, Result<Vec<u8>>>;

    /// Reopen the mounter socket (needed after a failed personalized query closes it).
    fn reconnect(&mut self) -> BoxFuture<'_, Result<()>>;
}

/// The system image mounter through `idevice`.
pub struct IdeviceMounter {
    mux: Mux,
    udid: String,
    mounter: ImageMounter,
}

impl std::fmt::Debug for IdeviceMounter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("IdeviceMounter").finish_non_exhaustive()
    }
}

impl IdeviceMounter {
    /// Connect the mounter and, as `idevice` documents is required, keep a lockdown session
    /// alive with one query so the device keeps answering.
    pub async fn connect(mux: &Mux, udid: &str) -> Result<Self> {
        let provider = provider(mux, udid).await?;

        let mut lockdown = LockdownClient::connect(&provider).await?;
        let pairing = provider.get_pairing_file().await.map_err(|_| DeviceError::NotPaired)?;
        lockdown.start_session(&pairing).await?;
        let _ = lockdown.get_value(Some("ProductVersion"), None).await?;

        let mounter = ImageMounter::connect(&provider).await?;

        Ok(Self { mux: mux.clone(), udid: udid.to_owned(), mounter })
    }
}

async fn provider(mux: &Mux, udid: &str) -> Result<UsbmuxdProvider> {
    mux.provider(udid, false).await
}

impl ImageMounting for IdeviceMounter {
    fn mounted(&mut self) -> BoxFuture<'_, Result<Option<Mounted>>> {
        async move {
            for (image_type, mounted) in [(DEVELOPER, Mounted::Developer), (PERSONALIZED, Mounted::Personalized)] {
                match self.mounter.lookup_image(image_type).await {
                    Ok(_) => return Ok(Some(mounted)),
                    Err(idevice::IdeviceError::NotFound) => {}
                    Err(error) => return Err(error.into()),
                }
            }

            Ok(None)
        }
        .boxed()
    }

    fn mount<'a>(
        &'a mut self,
        image_type: &'a str,
        image: &'a [u8],
        signature: Vec<u8>,
        trust_cache: Option<Vec<u8>>,
        info: Option<Value>,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            self.mounter.upload_image(image_type, image, signature.clone()).await?;
            self.mounter.mount_image(image_type, signature, trust_cache, info).await?;

            Ok(())
        }
        .boxed()
    }

    fn device_manifest<'a>(&'a mut self, image: &'a [u8]) -> BoxFuture<'a, Result<Option<Vec<u8>>>> {
        async move {
            use sha2::{Digest, Sha384};

            let hash = Sha384::digest(image).to_vec();

            match self.mounter.query_personalization_manifest(PERSONALIZED_QUERY, hash).await {
                Ok(manifest) => Ok(Some(manifest)),
                Err(idevice::IdeviceError::NotFound) => Ok(None),
                // A failed query closes the socket; the caller reconnects before TSS.
                Err(_) => Ok(None),
            }
        }
        .boxed()
    }

    fn personalization_identifiers(&mut self) -> BoxFuture<'_, Result<Dictionary>> {
        async move { self.mounter.query_personalization_identifiers(None).await.map_err(DeviceError::from) }.boxed()
    }

    fn nonce(&mut self) -> BoxFuture<'_, Result<Vec<u8>>> {
        async move { self.mounter.query_nonce(Some(PERSONALIZED_QUERY)).await.map_err(DeviceError::from) }.boxed()
    }

    fn reconnect(&mut self) -> BoxFuture<'_, Result<()>> {
        async move {
            let provider = provider(&self.mux, &self.udid).await?;
            self.mounter = ImageMounter::connect(&provider).await?;

            Ok(())
        }
        .boxed()
    }
}
