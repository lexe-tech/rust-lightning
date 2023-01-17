//! Provides utilities for syncing LDK via the transaction-based [`Confirm`] interface.
//!
//! [`Confirm`]: lightning::chain::Confirm

// Prefix these with `rustdoc::` when we update our MSRV to be >= 1.52 to remove warnings.
#![deny(broken_intra_doc_links)]
#![deny(private_intra_doc_links)]

#![deny(missing_docs)]
#![deny(unsafe_code)]

#![cfg_attr(docsrs, feature(doc_auto_cfg))]

#[cfg(any(feature = "esplora-blocking", feature = "esplora-async"))]
#[macro_use]
extern crate bdk_macros;

#[cfg(any(feature = "esplora-blocking", feature = "esplora-async"))]
mod esplora;
mod error;

pub use error::TxSyncError;

#[cfg(any(feature = "esplora-blocking", feature = "esplora-async"))]
pub use esplora::EsploraSyncClient;
