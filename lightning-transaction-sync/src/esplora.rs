use crate::TxSyncError;

use lightning::util::logger::Logger;
use lightning::{log_error, log_given_level, log_info, log_internal, log_debug, log_trace};
use lightning::chain::WatchedOutput;
use lightning::chain::{Confirm, Filter};

use bitcoin::{BlockHash, BlockHeader, Script, Transaction, Txid};

use esplora_client::Builder;
#[cfg(feature = "async-interface")]
use esplora_client::r#async::AsyncClient;
#[cfg(not(feature = "async-interface"))]
use esplora_client::blocking::BlockingClient;

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::HashSet;
use core::ops::Deref;

/// Synchronizes LDK with a given [`Esplora`] server.
///
/// Needs to be registerd with a [`ChainMonitor`] via the [`Filter`] interface to be informed of
/// transactions and outputs to montor.
///
/// [`Esplora`]: https://github.com/Blockstream/electrs
/// [`ChainMonitor`]: lightning::chain::chainmonitor::ChainMonitor
/// [`Filter`]: lightning::chain::Filter
pub struct EsploraSyncClient<L: Deref>
where
	L::Target: Logger,
{
	// Transactions that were registered via the `Filter` interface and have to be processed.
	queued_transactions: Mutex<HashSet<Txid>>,
	// Transactions that were previously processed, but must not be forgotten yet.
	watched_transactions: Mutex<HashSet<Txid>>,
	// Outputs that were registered via the `Filter` interface and have to be processed.
	queued_outputs: Mutex<HashSet<WatchedOutput>>,
	// Outputs that were previously processed, but must not be forgotten yet.
	watched_outputs: Mutex<HashSet<WatchedOutput>>,
	// Indicates whether we need to resync, e.g., after encountering an error.
	pending_sync: AtomicBool,
	// The tip hash observed during our last sync.
	#[cfg(not(feature = "async-interface"))]
	last_sync_hash: std::sync::Mutex<Option<BlockHash>>,
	#[cfg(feature = "async-interface")]
	last_sync_hash: futures::lock::Mutex<Option<BlockHash>>,
	#[cfg(not(feature = "async-interface"))]
	client: BlockingClient,
	#[cfg(feature = "async-interface")]
	client: AsyncClient,
	logger: L,
}

impl<L: Deref> EsploraSyncClient<L>
where
	L::Target: Logger,
{
	/// Synchronizes the given confirmables via the [`Confirm`] interface. This method should be
	/// called regularly to keep LDK up-to-date with current chain data.
	///
	/// [`Confirm`]: lightning::chain::Confirm
	#[maybe_async]
	pub fn sync(&self, confirmables: Vec<&(dyn Confirm + Sync + Send)>) -> Result<(), TxSyncError> {
		log_info!(self.logger, "Starting transaction sync.");
		// This lock makes sure we're syncing once at a time.
		#[cfg(not(feature = "async-interface"))]
		let mut locked_last_sync_hash = self.last_sync_hash.lock().unwrap();
		#[cfg(feature = "async-interface")]
		let mut locked_last_sync_hash = self.last_sync_hash.lock().await;

		let mut tip_hash = maybe_await!(self.client.get_tip_hash())?;

		loop {
			let pending_sync = self.pending_sync.load(Ordering::SeqCst);
			let pending_registrations = self.process_queues();
			let tip_is_new = Some(tip_hash) != *locked_last_sync_hash;

			// We loop until any registered transactions have been processed at least once, or the
			// tip hasn't been updated during the last iteration.
			if !pending_sync && !pending_registrations && !tip_is_new {
				// Nothing to do.
				break;
			} else {
				// Update the known tip to the newest one.
				if tip_is_new {
					// First check for any unconfirmed transactions and act on it immediately.
					match maybe_await!(self.sync_unconfirmed_transactions(&confirmables)) {
						Ok(()) => {},
						Err(err) => {
							// (Semi-)permanent failure, retry later.
							log_error!(self.logger, "Failed during transaction sync, aborting.");
							self.pending_sync.store(true, Ordering::SeqCst);
							return Err(err);
						}
					}

					match maybe_await!(self.sync_best_block_updated(&confirmables, &tip_hash)) {
						Ok(()) => {}
						Err(TxSyncError::Inconsistency) => {
							// Immediately restart syncing when we encounter any inconsistencies.
							log_debug!(self.logger, "Encountered inconsistency during transaction sync, restarting.");
							self.pending_sync.store(true, Ordering::SeqCst);
							continue;
						}
						Err(err) => {
							// (Semi-)permanent failure, retry later.
							self.pending_sync.store(true, Ordering::SeqCst);
							return Err(err);
						}
					}
				}

				match maybe_await!(self.get_confirmed_transactions()) {
					Ok((confirmed_txs, spent_outputs)) => {
						// Double-check the tip hash. If if it changed, a reorg happened since
						// we started syncing and we need to restart last-minute.
						let check_tip_hash = maybe_await!(self.client.get_tip_hash())?;
						if check_tip_hash != tip_hash {
							tip_hash = check_tip_hash;
							continue;
						}

						self.sync_confirmed_transactions(
							&confirmables,
							confirmed_txs,
							spent_outputs,
						);
					}
					Err(TxSyncError::Inconsistency) => {
						// Immediately restart syncing when we encounter any inconsistencies.
						log_debug!(self.logger, "Encountered inconsistency during transaction sync, restarting.");
						self.pending_sync.store(true, Ordering::SeqCst);
						continue;
					}
					Err(err) => {
						// (Semi-)permanent failure, retry later.
						log_error!(self.logger, "Failed during transaction sync, aborting.");
						self.pending_sync.store(true, Ordering::SeqCst);
						return Err(err);
					}
				}
				*locked_last_sync_hash = Some(tip_hash);
				self.pending_sync.store(false, Ordering::SeqCst);
			}
		}
		log_info!(self.logger, "Finished transaction sync.");
		Ok(())
	}

	/// Returns a new [`EsploraSyncClient`] object.
	pub fn new(server_url: String, logger: L) -> Self {
		let watched_transactions = Mutex::new(HashSet::new());
		let queued_transactions = Mutex::new(HashSet::new());
		let watched_outputs = Mutex::new(HashSet::new());
		let queued_outputs = Mutex::new(HashSet::new());
		let pending_sync = AtomicBool::new(false);
		#[cfg(not(feature = "async-interface"))]
		let last_sync_hash = Mutex::new(None);
		#[cfg(feature = "async-interface")]
		let last_sync_hash = futures::lock::Mutex::new(None);
		let builder = Builder::new(&server_url);
		#[cfg(not(feature = "async-interface"))]
		let client = builder.build_blocking().unwrap();
		#[cfg(feature = "async-interface")]
		let client = builder.build_async().unwrap();
		Self {
			queued_transactions,
			watched_transactions,
			queued_outputs,
			watched_outputs,
			pending_sync,
			last_sync_hash,
			client,
			logger,
		}
	}

	// Processes the transaction and output queues, returns `true` if new items had been
	// registered.
	fn process_queues(&self) -> bool {
		let mut pending_registrations = false;
		{
			let mut locked_queued_transactions = self.queued_transactions.lock().unwrap();
			if !locked_queued_transactions.is_empty() {
				let mut locked_watched_transactions = self.watched_transactions.lock().unwrap();
				pending_registrations = true;

				locked_watched_transactions.extend(locked_queued_transactions.iter());
				*locked_queued_transactions = HashSet::new();
			}
		}
		{
			let mut locked_queued_outputs = self.queued_outputs.lock().unwrap();
			if !locked_queued_outputs.is_empty() {
				let mut locked_watched_outputs = self.watched_outputs.lock().unwrap();
				pending_registrations = true;

				locked_watched_outputs.extend(locked_queued_outputs.iter().cloned());
				*locked_queued_outputs = HashSet::new();
			}
		}
		pending_registrations
	}

	#[maybe_async]
	fn sync_best_block_updated(
		&self, confirmables: &Vec<&(dyn Confirm + Sync + Send)>, tip_hash: &BlockHash,
	) -> Result<(), TxSyncError> {

		// Inform the interface of the new block.
		let tip_header = maybe_await!(self.client.get_header_by_hash(tip_hash))?;
		let tip_status = maybe_await!(self.client.get_block_status(&tip_hash))?;
		if tip_status.in_best_chain {
			if let Some(tip_height) = tip_status.height {
				for c in confirmables {
					c.best_block_updated(&tip_header, tip_height);
				}
			}
		} else {
			return Err(TxSyncError::Inconsistency);
		}
		Ok(())
	}

	fn sync_confirmed_transactions(
		&self, confirmables: &Vec<&(dyn Confirm + Sync + Send)>, confirmed_txs: Vec<ConfirmedTx>,
		spent_outputs: HashSet<WatchedOutput>,
	) {
		let mut locked_watched_transactions = self.watched_transactions.lock().unwrap();
		for ctx in confirmed_txs {
			for c in confirmables {
				c.transactions_confirmed(
					&ctx.block_header,
					&[(ctx.pos, &ctx.tx)],
					ctx.block_height,
				);
			}

			locked_watched_transactions.remove(&ctx.tx.txid());
		}

		let mut locked_watched_outputs = self.watched_outputs.lock().unwrap();
		*locked_watched_outputs = &*locked_watched_outputs - &spent_outputs;
	}

	#[maybe_async]
	fn get_confirmed_transactions(
		&self,
	) -> Result<(Vec<ConfirmedTx>, HashSet<WatchedOutput>), TxSyncError> {

		// First, check the confirmation status of registered transactions as well as the
		// status of dependent transactions of registered outputs.

		let mut confirmed_txs = Vec::new();

		// Check in the current queue, as well as in registered transactions leftover from
		// previous iterations.
		let registered_txs = self.watched_transactions.lock().unwrap().clone();

		for txid in registered_txs {
			if let Some(confirmed_tx) = maybe_await!(self.get_confirmed_tx(&txid, None, None))? {
				confirmed_txs.push(confirmed_tx);
			}
		}

		// Check all registered outputs for dependent spending transactions.
		let registered_outputs = self.watched_outputs.lock().unwrap().clone();

		// Remember all registered outputs that have been spent.
		let mut spent_outputs = HashSet::new();

		for output in registered_outputs {
			if let Some(output_status) = maybe_await!(self.client
				.get_output_status(&output.outpoint.txid, output.outpoint.index as u64))?
			{
				if let Some(spending_txid) = output_status.txid {
					if let Some(spending_tx_status) = output_status.status {
						if let Some(confirmed_tx) = maybe_await!(self
							.get_confirmed_tx(
								&spending_txid,
								spending_tx_status.block_hash,
								spending_tx_status.block_height,
							))?
						{
							confirmed_txs.push(confirmed_tx);
							spent_outputs.insert(output);
							continue;
						}
					}
				}
			}
		}

		// Sort all confirmed transactions first by block height, then by in-block
		// position, and finally feed them to the interface in order.
		confirmed_txs.sort_unstable_by(|tx1, tx2| {
			tx1.block_height.cmp(&tx2.block_height).then_with(|| tx1.pos.cmp(&tx2.pos))
		});

		Ok((confirmed_txs, spent_outputs))
	}

	#[maybe_async]
	fn get_confirmed_tx(
		&self, txid: &Txid, expected_block_hash: Option<BlockHash>, known_block_height: Option<u32>,
	) -> Result<Option<ConfirmedTx>, TxSyncError> {
		if let Some(merkle_block) = maybe_await!(self.client.get_merkle_block(&txid))? {
			let block_header = merkle_block.header;
			let block_hash = block_header.block_hash();
			if let Some(expected_block_hash) = expected_block_hash {
				if expected_block_hash != block_hash {
					log_trace!(self.logger, "Inconsistency: Tx {} expected in block {}, but is confirmed in {}", txid, expected_block_hash, block_hash);
					return Err(TxSyncError::Inconsistency);
				}
			}

			let mut matches = vec![*txid];
			let mut indexes = Vec::new();
			let _ = merkle_block.txn.extract_matches(&mut matches, &mut indexes);
			debug_assert_eq!(indexes.len(), 1);
			let pos = *indexes.get(0).ok_or(TxSyncError::Failed)? as usize;
			if let Some(tx) = maybe_await!(self.client.get_tx(&txid))? {
				if let Some(block_height) = known_block_height {
					// We can take a shortcut here if a previous call already gave us the height.
					return Ok(Some(ConfirmedTx { tx, block_header, pos, block_height }));
				}

				let block_status = maybe_await!(self.client.get_block_status(&block_hash))?;
				if let Some(block_height) = block_status.height {
					return Ok(Some(ConfirmedTx { tx, block_header, pos, block_height }));
				} else {
					// If any previously-confirmed block suddenly is no longer confirmed, we found
					// an inconsistency and should start over.
					log_trace!(self.logger, "Inconsistency: Tx {} was unconfirmed during syncing.", txid);
					return Err(TxSyncError::Inconsistency);
				}
			}
		}
		Ok(None)
	}

	#[maybe_async]
	fn sync_unconfirmed_transactions(
		&self, confirmables: &Vec<&(dyn Confirm + Sync + Send)>,
	) -> Result<(), TxSyncError> {
		// Query the interface for relevant txids and check whether the relevant blocks are still
		// in the best chain, mark them unconfirmed otherwise. If the transactions have been
		// reconfirmed in another block, we'll confirm them in the next sync iteration.
		let relevant_txids = confirmables
			.iter()
			.flat_map(|c| c.get_relevant_txids())
			.collect::<HashSet<(Txid, Option<BlockHash>)>>();

		for (txid, block_hash_opt) in relevant_txids {
			if let Some(block_hash) = block_hash_opt {
				let block_status = maybe_await!(self.client.get_block_status(&block_hash))?;
				if block_status.in_best_chain {
					// Skip if the block in question is still confirmed.
					continue;
				}
			}

			for c in confirmables {
				c.transaction_unconfirmed(&txid);
			}

			self.watched_transactions.lock().unwrap().insert(txid);
		}

		Ok(())
	}
}

struct ConfirmedTx {
	tx: Transaction,
	block_header: BlockHeader,
	block_height: u32,
	pos: usize,
}

impl<L: Deref> Filter for EsploraSyncClient<L>
where
	L::Target: Logger,
{
	fn register_tx(&self, txid: &Txid, _script_pubkey: &Script) {
		self.queued_transactions.lock().unwrap().insert(*txid);
	}

	fn register_output(&self, output: WatchedOutput) {
		self.queued_outputs.lock().unwrap().insert(output);
	}
}
