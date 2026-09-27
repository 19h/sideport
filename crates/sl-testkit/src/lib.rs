//! Test-only fixtures shared by engine and CLI tests.
//!
//! Everything here is generated at run time: keys, certificates, signed profiles and portal
//! state. Fixture names that resemble Apple's do not make any certificate Apple-trusted; tests
//! pass [`pki::ProfileChain::trust`] explicitly.

#![forbid(unsafe_code)]

pub mod pki;
pub mod portal;
pub mod profile;

pub use pki::{Issued, ProfileChain};
pub use portal::{FakePortal, PortalState};
