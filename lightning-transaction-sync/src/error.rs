use std::fmt;

#[derive(Debug)]
/// An error that possibly needs to be handled by the user.
pub enum TxSyncError {
	/// A transaction sync failed and needs to be retried eventually.
	Failed,
	/// An inconsisteny was encounterd during transaction sync.
	Inconsistency,
}

impl fmt::Display for TxSyncError {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match *self {
			Self::Failed => write!(f, "Failed to conduct transaction sync."),
			Self::Inconsistency => {
				write!(f, "Encountered an inconsisteny during transaction sync.")
			}
		}
	}
}

impl std::error::Error for TxSyncError {}

#[cfg(any(feature = "esplora-blocking", feature = "esplora-async"))]
impl From<esplora_client::Error> for TxSyncError {
	fn from(_e: esplora_client::Error) -> Self {
		Self::Failed
	}
}
