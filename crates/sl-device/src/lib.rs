//! iOS device access: usbmuxd discovery, AFC staging, installation, profiles and apps.
//!
//! [`install::install`] implements the recovered upload/installation retry policy against the
//! [`install::Connector`] trait. [`backend::IdeviceConnector`] implements it with `idevice`
//! over usbmuxd; tests implement it with fakes.

#![forbid(unsafe_code)]

pub mod backend;
pub mod error;
pub mod install;
pub mod models;
pub mod mux;

pub use backend::{Backend, DeviceValues, IdeviceBackend, IdeviceConnector, InstalledApp, LineStream};
pub use error::{DeviceError, Recovery, Result};
pub use install::{
    Connector, Delegate, DeviceWait, FilePackage, InstallRequest, Package, PackageReader, Phase, Question,
};
pub use mux::{Attached, EventStream, Link, Mux, MuxEvent};
