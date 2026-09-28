//! iOS device access: usbmuxd discovery, AFC staging, installation, profiles and apps.
//!
//! [`install::install`] implements the recovered upload/installation retry policy against the
//! [`install::Connector`] trait. [`backend::IdeviceConnector`] implements it with `idevice`
//! over usbmuxd; tests implement it with fakes.

#![forbid(unsafe_code)]

pub mod backend;
pub mod ddi;
pub mod error;
pub mod framing;
pub mod install;
pub mod jit;
pub mod models;
pub mod mounter;
pub mod mux;
pub mod service;
pub mod tss;

pub use backend::{Backend, DeviceValues, IdeviceBackend, IdeviceConnector, InstalledApp, LineStream};
pub use ddi::{Catalog, Outcome, Plan, Store};
pub use error::{DeviceError, Recovery, Result};
pub use install::{
    Connector, Delegate, DeviceWait, FilePackage, InstallRequest, Package, PackageReader, Phase, Question,
};
pub use jit::{AppLaunch, Debugger};
pub use mounter::{ImageMounting, Mounted};
pub use mux::{Attached, EventStream, Link, Mux, MuxEvent};
pub use tss::TssClient;
