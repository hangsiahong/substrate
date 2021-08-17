// Copyright (C) 2019-2021 Parity Technologies (UK) Ltd.
// Copyright (C) 2021 Subspace Labs, Inc.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! # Proof-of-Capacity (PoC) Consensus
//!
//! PoC is a slot-based block production mechanism which uses a Proof-of-Capacity to
//! randomly perform the slot allocation. On every slot, all the farmers evaluate
//! their disk-based plot. If they have a tag (reflecting a commitment to a valid
//! encoding) that it is lower than a given threshold (which is proportional to
//! the total space pledged by the network) they may produce a new block. The
//! proof of the PoC function execution will be used by other peers to validate
//! the legitimacy of the slot claim.
//!
//! The engine is also responsible for collecting entropy on-chain which will be
//! used to seed the given PoR challenge. An epoch is a contiguous number of slots
//! under which we will be using the same base PoR challenge. During an epoch all PoR
//! outputs produced as a result of block production will be collected into an
//! on-chain randomness pool. Epoch changes are announced one epoch in advance,
//! i.e. when ending epoch N, we announce the parameters (i.e, new randomness)
//! for epoch N+2.
//!
//! Since the slot assignment is randomized, it is possible that a slot is
//! claimed by multiple farmers, in which case we will have a temporary fork,
//! or that a slot is not claimed by any farmer, in which case no block is
//! produced. This means that block times are probabalistic.
//!
//! The protocol has a parameter `c` [0, 1] for which `1 - c` is the probability
//! of a slot being empty. The choice of this parameter affects the security of
//! the protocol relating to maximum tolerable network delays.
//!
//! The fork choice rule is weight-based, where weight equals the number of
//! primary blocks in the chain. We will pick the heaviest chain (more
//! blocks) and will go with the longest one in case of a tie.
#![forbid(unsafe_code)]
#![warn(missing_docs)]
use futures::channel::mpsc::{channel, Receiver, Sender};
use futures::channel::oneshot;
use parking_lot::Mutex;
use sc_client_api::{backend::AuxStore, BlockchainEvents, ProvideUncles, UsageProvider};
use sc_consensus::{
    block_import::{
        BlockCheckParams, BlockImport, BlockImportParams, ForkChoiceStrategy, ImportResult,
        StateAction,
    },
    import_queue::{BasicQueue, BoxJustificationImport, DefaultImportQueue, Verifier},
};
pub use sc_consensus_slots::SlotProportion;
use sc_telemetry::{telemetry, TelemetryHandle, CONSENSUS_DEBUG, CONSENSUS_TRACE};
use sp_api::{NumberFor, ProvideRuntimeApi};
use sp_block_builder::BlockBuilder as BlockBuilderApi;
pub use sp_consensus::SyncOracle;
use sp_consensus::{
    BlockOrigin, CacheKeyId, CanAuthorWith, Environment, Error as ConsensusError, Proposer,
    SelectChain, SlotData,
};
use sp_consensus_poc::inherents::PoCInherentData;
pub use sp_consensus_poc::{
    digests::{CompatibleDigestItem, NextConfigDescriptor, NextEpochDescriptor, PreDigest},
    ConsensusLog, FarmerId, PoCApi, PoCEpochConfiguration, PoCGenesisConfiguration, POC_ENGINE_ID,
};
use sp_inherents::{CreateInherentDataProviders, InherentData, InherentDataProvider};
use sp_runtime::{
    generic::{BlockId, OpaqueDigestItemId},
    traits::{Block as BlockT, DigestItemFor, Header, Zero},
    Justifications,
};
use std::{
    borrow::Cow, collections::HashMap, convert::TryInto, pin::Pin, sync::Arc, time::Duration, u64,
};

use codec::{Decode, Encode};
use futures::prelude::*;
use log::{debug, info, log, trace, warn};
use prometheus_endpoint::Registry;
use ring::digest;
use sc_consensus_epochs::{
    descendent_query, Epoch as EpochT, EpochChangesFor, SharedEpochChanges, ViableEpochDescriptor,
};
use sc_consensus_slots::{
    check_equivocation, BackoffAuthoringBlocksStrategy, CheckedHeader, InherentDataProviderExt,
    SimpleSlotWorker, SlotInfo, StorageChanges,
};
use schnorrkel::context::SigningContext;
use schnorrkel::SecretKey;
use sp_api::ApiExt;
use sp_blockchain::{
    Error as ClientError, HeaderBackend, HeaderMetadata, ProvideCache, Result as ClientResult,
};
use sp_consensus_poc::digests::{
    NextSaltDescriptor, NextSolutionRangeDescriptor, SaltDescriptor, Solution,
    SolutionRangeDescriptor,
};
use sp_consensus_poc::Randomness;
use sp_consensus_slots::Slot;
use sp_consensus_spartan::spartan::{Salt, Spartan, SIGNING_CONTEXT};
use sp_core::sr25519::Pair;
use sp_core::Pair as PairTrait;
use std::sync::mpsc;

mod verification;

pub mod aux_schema;
#[cfg(test)]
mod tests;

/// Information about new slot that just arrived
#[derive(Debug, Clone)]
pub struct NewSlotInfo {
    /// Slot
    pub slot: Slot,
    /// Slot challenge
    pub challenge: [u8; 8],
    /// Salt
    pub salt: Salt,
    /// Salt for the next eon
    pub next_salt: Option<Salt>,
    /// Acceptable solution range
    pub solution_range: u64,
}

/// A function that can be called whenever it is necessary to create a subscription for new slots
pub type NewSlotNotifier = Arc<
    Box<
        dyn (Fn() -> mpsc::Receiver<(NewSlotInfo, mpsc::Sender<(Solution, Vec<u8>)>)>)
            + Send
            + Sync,
    >,
>;

/// PoC epoch information
#[derive(Decode, Encode, PartialEq, Eq, Clone, Debug)]
pub struct Epoch {
    /// The epoch index.
    pub epoch_index: u64,
    /// The starting slot of the epoch.
    pub start_slot: Slot,
    /// The duration of this epoch.
    pub duration: u64,
    /// Randomness for this epoch.
    pub randomness: Randomness,
    /// Configuration of the epoch.
    pub config: PoCEpochConfiguration,
}

impl EpochT for Epoch {
    type NextEpochDescriptor = (NextEpochDescriptor, PoCEpochConfiguration);
    type Slot = Slot;

    fn increment(
        &self,
        (descriptor, config): (NextEpochDescriptor, PoCEpochConfiguration),
    ) -> Epoch {
        Epoch {
            epoch_index: self.epoch_index + 1,
            start_slot: self.start_slot + self.duration,
            duration: self.duration,
            randomness: descriptor.randomness,
            config,
        }
    }

    fn start_slot(&self) -> Slot {
        self.start_slot
    }

    fn end_slot(&self) -> Slot {
        self.start_slot + self.duration
    }
}

impl Epoch {
    /// Create the genesis epoch (epoch #0). This is defined to start at the slot of
    /// the first block, so that has to be provided.
    pub fn genesis(genesis_config: &PoCGenesisConfiguration, slot: Slot) -> Epoch {
        Epoch {
            epoch_index: 0,
            start_slot: slot,
            duration: genesis_config.epoch_length,
            randomness: genesis_config.randomness,
            config: PoCEpochConfiguration {
                c: genesis_config.c,
            },
        }
    }
}

/// Errors encountered by the poc authorship task.
#[derive(derive_more::Display, Debug)]
pub enum Error<B: BlockT> {
    /// Multiple PoC pre-runtime digests
    #[display(fmt = "Multiple PoC pre-runtime digests, rejecting!")]
    MultiplePreRuntimeDigests,
    /// No PoC pre-runtime digest found
    #[display(fmt = "No PoC pre-runtime digest found")]
    NoPreRuntimeDigest,
    /// Multiple PoC epoch change digests
    #[display(fmt = "Multiple PoC epoch change digests, rejecting!")]
    MultipleEpochChangeDigests,
    /// Multiple PoC config change digests
    #[display(fmt = "Multiple PoC config change digests, rejecting!")]
    MultipleConfigChangeDigests,
    /// Multiple PoC solution range digests
    #[display(fmt = "Multiple PoC solution range digests, rejecting!")]
    MultipleSolutionRangeDigests,
    /// Multiple PoC next solution range digests
    #[display(fmt = "Multiple PoC next solution range digests, rejecting!")]
    MultipleNextSolutionRangeDigests,
    /// Multiple PoC salt digests
    #[display(fmt = "Multiple PoC salt digests, rejecting!")]
    MultipleSaltDigests,
    /// Multiple PoC next salt digests
    #[display(fmt = "Multiple PoC next salt digests, rejecting!")]
    MultipleNextSaltDigests,
    /// Could not extract timestamp and slot
    #[display(fmt = "Could not extract timestamp and slot: {:?}", _0)]
    Extraction(sp_consensus::Error),
    /// Could not fetch epoch
    #[display(fmt = "Could not fetch epoch at {:?}", _0)]
    FetchEpoch(B::Hash),
    /// Header rejected: too far in the future
    #[display(fmt = "Header {:?} rejected: too far in the future", _0)]
    TooFarInFuture(B::Hash),
    /// Parent unavailable. Cannot import
    #[display(fmt = "Parent ({}) of {} unavailable. Cannot import", _0, _1)]
    ParentUnavailable(B::Hash, B::Hash),
    /// Slot number must increase
    #[display(
        fmt = "Slot number must increase: parent slot: {}, this slot: {}",
        _0,
        _1
    )]
    SlotMustIncrease(Slot, Slot),
    /// Header has a bad seal
    #[display(fmt = "Header {:?} has a bad seal", _0)]
    HeaderBadSeal(B::Hash),
    /// Header is unsealed
    #[display(fmt = "Header {:?} is unsealed", _0)]
    HeaderUnsealed(B::Hash),
    /// Bad signature
    #[display(fmt = "Bad signature on {:?}", _0)]
    BadSignature(B::Hash),
    /// Bad solution signature
    #[display(fmt = "Bad solution signature")]
    BadSolutionSignature(Slot),
    /// Solution is outside of solution range
    #[display(fmt = "Solution is outside of solution range for slot {}", _0)]
    OutsideOfSolutionRange(Slot),
    /// Encoding is of wrong size
    #[display(fmt = "Encoding is of the wrong size")]
    EncodingOfWrongSize,
    /// Invalid encoding of genesis piece for public key and nonce
    #[display(fmt = "Invalid encoding for slot {}", _0)]
    InvalidEncoding(Slot),
    /// Invalid commitment for salt
    #[display(fmt = "Invalid commitment for salt for slot {}", _0)]
    InvalidCommitment(Slot),
    /// Could not fetch parent header
    #[display(fmt = "Could not fetch parent header: {:?}", _0)]
    FetchParentHeader(sp_blockchain::Error),
    /// Expected epoch change to happen.
    #[display(fmt = "Expected epoch change to happen at {:?}, s{}", _0, _1)]
    ExpectedEpochChange(B::Hash, Slot),
    /// Unexpected config change.
    #[display(fmt = "Unexpected config change")]
    UnexpectedConfigChange,
    /// Unexpected epoch change
    #[display(fmt = "Unexpected epoch change")]
    UnexpectedEpochChange,
    /// Parent block has no associated weight
    #[display(fmt = "Parent block of {} has no associated weight", _0)]
    ParentBlockNoAssociatedWeight(B::Hash),
    /// Block has no associated solution range
    #[display(fmt = "Missing solution range for block {}", _0)]
    MissingSolutionRange(B::Hash),
    /// Block has no associated salt
    #[display(fmt = "Missing salt for block {}", _0)]
    MissingSalt(B::Hash),
    /// Farmer in block list
    #[display(fmt = "Farmer {} is in block list", _0)]
    FarmerInBlockList(FarmerId),
    /// Check inherents error
    #[display(fmt = "Checking inherents failed: {}", _0)]
    CheckInherents(sp_inherents::Error),
    /// Unhandled check inherents error
    #[display(
        fmt = "Checking inherents unhandled error: {}",
        "String::from_utf8_lossy(_0)"
    )]
    CheckInherentsUnhandled(sp_inherents::InherentIdentifier),
    /// Create inherents error.
    #[display(fmt = "Creating inherents failed: {}", _0)]
    CreateInherents(sp_inherents::Error),
    /// Client error
    Client(sp_blockchain::Error),
    /// Runtime Api error.
    RuntimeApi(sp_api::ApiError),
    /// Fork tree error
    ForkTree(Box<fork_tree::Error<sp_blockchain::Error>>),
}

impl<B: BlockT> std::convert::From<Error<B>> for String {
    fn from(error: Error<B>) -> String {
        error.to_string()
    }
}

fn poc_err<B: BlockT>(error: Error<B>) -> Error<B> {
    debug!(target: "poc", "{}", error);
    error
}

/// Intermediate value passed to block importer.
pub struct PoCIntermediate<B: BlockT> {
    /// The epoch descriptor.
    pub epoch_descriptor: ViableEpochDescriptor<B::Hash, NumberFor<B>, Epoch>,
}

/// Intermediate key for PoC engine.
pub static INTERMEDIATE_KEY: &[u8] = b"poc0";

/// A slot duration. Create with `get_or_compute`.
// FIXME: Once Rust has higher-kinded types, the duplication between this
// and `super::poc::Config` can be eliminated.
// https://github.com/paritytech/substrate/issues/2434
#[derive(Clone)]
pub struct Config(sc_consensus_slots::SlotDuration<PoCGenesisConfiguration>);

impl Config {
    /// Either fetch the slot duration from disk or compute it from the genesis
    /// state.
    pub fn get_or_compute<B: BlockT, C>(client: &C) -> ClientResult<Self>
    where
        C: AuxStore + ProvideRuntimeApi<B> + UsageProvider<B>,
        C::Api: PoCApi<B>,
    {
        trace!(target: "poc", "Getting slot duration");
        match sc_consensus_slots::SlotDuration::get_or_compute(client, |a, b| {
            let has_api_v1 = a.has_api_with::<dyn PoCApi<B>, _>(&b, |v| v == 1)?;

            if has_api_v1 {
                a.configuration(b).map_err(Into::into)
            } else {
                Err(sp_blockchain::Error::VersionInvalid(
                    "Unsupported or invalid PoCApi version".to_string(),
                ))
            }
        })
        .map(Self)
        {
            Ok(s) => Ok(s),
            Err(s) => {
                warn!(target: "poc", "Failed to get slot duration");
                Err(s)
            }
        }
    }

    /// Get the inner slot duration
    pub fn slot_duration(&self) -> Duration {
        self.0.slot_duration()
    }
}

impl std::ops::Deref for Config {
    type Target = PoCGenesisConfiguration;

    fn deref(&self) -> &PoCGenesisConfiguration {
        &*self.0
    }
}

/// Parameters for PoC.
pub struct PoCParams<B: BlockT, C, SC, E, I, SO, L, CIDP, BS, CAW> {
    /// The client to use
    pub client: Arc<C>,

    /// The SelectChain Strategy
    pub select_chain: SC,

    /// The environment we are producing blocks for.
    pub env: E,

    /// The underlying block-import object to supply our produced blocks to.
    /// This must be a `PoCBlockImport` or a wrapper of it, otherwise
    /// critical consensus logic will be omitted.
    pub block_import: I,

    /// A sync oracle
    pub sync_oracle: SO,

    /// Hook into the sync module to control the justification sync process.
    pub justification_sync_link: L,

    /// Something that can create the inherent data providers.
    pub create_inherent_data_providers: CIDP,

    /// Force authoring of blocks even if we are offline
    pub force_authoring: bool,

    /// Strategy and parameters for backing off block production.
    pub backoff_authoring_blocks: Option<BS>,

    /// The source of timestamps for relative slots
    pub poc_link: PoCLink<B>,

    /// Checks if the current native implementation can author with a runtime at a given block.
    pub can_author_with: CAW,

    /// The proportion of the slot dedicated to proposing.
    ///
    /// The block proposing will be limited to this proportion of the slot from the starting of the
    /// slot. However, the proposing can still take longer when there is some lenience factor applied,
    /// because there were no blocks produced for some slots.
    pub block_proposal_slot_portion: SlotProportion,

    /// The maximum proportion of the slot dedicated to proposing with any lenience factor applied
    /// due to no blocks being produced.
    pub max_block_proposal_slot_portion: Option<SlotProportion>,

    /// Handle use to report telemetries.
    pub telemetry: Option<TelemetryHandle>,
}

/// Start the PoC worker.
pub fn start_poc<B, C, SC, E, I, SO, CIDP, BS, CAW, L, Error>(
    PoCParams {
        client,
        select_chain,
        env,
        block_import,
        sync_oracle,
        justification_sync_link,
        create_inherent_data_providers,
        force_authoring,
        backoff_authoring_blocks,
        poc_link,
        can_author_with,
        block_proposal_slot_portion,
        max_block_proposal_slot_portion,
        telemetry,
    }: PoCParams<B, C, SC, E, I, SO, L, CIDP, BS, CAW>,
) -> Result<PoCWorker<B>, sp_consensus::Error>
where
    B: BlockT,
    C: ProvideRuntimeApi<B>
        + ProvideCache<B>
        + ProvideUncles<B>
        + BlockchainEvents<B>
        + HeaderBackend<B>
        + HeaderMetadata<B, Error = ClientError>
        + Send
        + Sync
        + 'static,
    C::Api: PoCApi<B>,
    SC: SelectChain<B> + 'static,
    E: Environment<B, Error = Error> + Send + Sync + 'static,
    E::Proposer: Proposer<B, Error = Error, Transaction = sp_api::TransactionFor<C, B>>,
    I: BlockImport<B, Error = ConsensusError, Transaction = sp_api::TransactionFor<C, B>>
        + Send
        + Sync
        + 'static,
    SO: SyncOracle + Send + Sync + Clone + 'static,
    L: sc_consensus::JustificationSyncLink<B> + 'static,
    CIDP: CreateInherentDataProviders<B, ()> + Send + Sync + 'static,
    CIDP::InherentDataProviders: InherentDataProviderExt + Send,
    BS: BackoffAuthoringBlocksStrategy<NumberFor<B>> + Send + 'static,
    CAW: CanAuthorWith<B> + Send + Sync + 'static,
    Error: std::error::Error + Send + From<ConsensusError> + From<I::Error> + 'static,
{
    const HANDLE_BUFFER_SIZE: usize = 1024;

    let config = poc_link.config;

    let new_slot_senders: Arc<
        Mutex<Vec<mpsc::Sender<(NewSlotInfo, mpsc::Sender<(Solution, Vec<u8>)>)>>>,
    > = Arc::default();

    let worker = PoCSlotWorker {
        client: client.clone(),
        block_import,
        env,
        sync_oracle: sync_oracle.clone(),
        justification_sync_link,
        force_authoring,
        backoff_authoring_blocks,
        epoch_changes: poc_link.epoch_changes.clone(),
        config: config.clone(),
        on_claim_slot: Box::new({
            let new_slot_senders = Arc::clone(&new_slot_senders);

            move |slot,
                  epoch,
                  salt,
                  solution_range,
                  solution_sender: mpsc::Sender<(Solution, Vec<u8>)>| {
                let slot_info = NewSlotInfo {
                    slot,
                    challenge: create_global_challenge(epoch, slot),
                    salt,
                    // TODO: This will not be the correct way in the future once salt is no longer
                    //  just an incremented number
                    next_salt: Some((u64::from_le_bytes(salt) + 1).to_le_bytes()),
                    solution_range,
                };
                {
                    // drain_filter() would be more convenient here
                    let mut new_slot_senders = new_slot_senders.lock();
                    let mut i = 0;
                    while i != new_slot_senders.len() {
                        if new_slot_senders
                            .get_mut(i)
                            .unwrap()
                            .send((slot_info.clone(), solution_sender.clone()))
                            .is_err()
                        {
                            new_slot_senders.remove(i);
                        } else {
                            i += 1;
                        }
                    }
                }
            }
        }),
        spartan: Spartan::new(),
        // TODO: Figure out how to remove explicit schnorrkel dependency
        signing_context: schnorrkel::context::signing_context(SIGNING_CONTEXT),
        block_proposal_slot_portion,
        max_block_proposal_slot_portion,
        telemetry,
    };

    info!(target: "poc", "🧑‍🌾 Starting PoC Authorship worker");
    let inner = sc_consensus_slots::start_slot_worker(
        config.0.clone(),
        select_chain,
        worker,
        sync_oracle,
        create_inherent_data_providers,
        can_author_with,
    );

    let (worker_tx, worker_rx) = channel(HANDLE_BUFFER_SIZE);

    let answer_requests =
        answer_requests(worker_rx, config.0, client, poc_link.epoch_changes.clone());
    Ok(PoCWorker {
        inner: Box::pin(future::join(inner, answer_requests).map(|_| ())),
        handle: PoCWorkerHandle(worker_tx),
        new_slot_senders,
    })
}

async fn answer_requests<B: BlockT, C>(
    mut request_rx: Receiver<PoCRequest<B>>,
    genesis_config: sc_consensus_slots::SlotDuration<PoCGenesisConfiguration>,
    client: Arc<C>,
    epoch_changes: SharedEpochChanges<B, Epoch>,
) where
    C: ProvideRuntimeApi<B>
        + ProvideCache<B>
        + ProvideUncles<B>
        + BlockchainEvents<B>
        + HeaderBackend<B>
        + HeaderMetadata<B, Error = ClientError>
        + Send
        + Sync
        + 'static,
{
    while let Some(request) = request_rx.next().await {
        match request {
            PoCRequest::EpochForChild(parent_hash, parent_number, slot_number, response) => {
                let lookup = || {
                    let epoch_changes = epoch_changes.shared_data();
                    let epoch_descriptor = epoch_changes
                        .epoch_descriptor_for_child_of(
                            descendent_query(&*client),
                            &parent_hash,
                            parent_number,
                            slot_number,
                        )
                        .map_err(|e| Error::<B>::ForkTree(Box::new(e)))?
                        .ok_or_else(|| Error::<B>::FetchEpoch(parent_hash))?;

                    let viable_epoch = epoch_changes
                        .viable_epoch(&epoch_descriptor, |slot| {
                            Epoch::genesis(&genesis_config, slot)
                        })
                        .ok_or_else(|| Error::<B>::FetchEpoch(parent_hash))?;

                    Ok(sp_consensus_poc::Epoch {
                        epoch_index: viable_epoch.as_ref().epoch_index,
                        start_slot: viable_epoch.as_ref().start_slot,
                        duration: viable_epoch.as_ref().duration,
                        randomness: viable_epoch.as_ref().randomness,
                        config: viable_epoch.as_ref().config.clone(),
                    })
                };

                let _ = response.send(lookup());
            }
        }
    }
}

/// Requests to the PoC service.
#[non_exhaustive]
pub enum PoCRequest<B: BlockT> {
    /// Request the epoch that a child of the given block, with the given slot number would have.
    ///
    /// The parent block is identified by its hash and number.
    EpochForChild(
        B::Hash,
        NumberFor<B>,
        Slot,
        oneshot::Sender<Result<sp_consensus_poc::Epoch, Error<B>>>,
    ),
}

/// A handle to the PoC worker for issuing requests.
#[derive(Clone)]
pub struct PoCWorkerHandle<B: BlockT>(Sender<PoCRequest<B>>);

impl<B: BlockT> PoCWorkerHandle<B> {
    /// Send a request to the PoC service.
    pub async fn send(&mut self, request: PoCRequest<B>) {
        // Failure to send means that the service is down.
        // This will manifest as the receiver of the request being dropped.
        let _ = self.0.send(request).await;
    }
}

/// Worker for PoC which implements `Future<Output=()>`. This must be polled.
#[must_use]
pub struct PoCWorker<B: BlockT> {
    inner: Pin<Box<dyn futures::Future<Output = ()> + Send + 'static>>,
    handle: PoCWorkerHandle<B>,
    new_slot_senders:
        Arc<Mutex<Vec<mpsc::Sender<(NewSlotInfo, mpsc::Sender<(Solution, Vec<u8>)>)>>>>,
}

impl<B: BlockT> PoCWorker<B> {
    /// Returns a function that can be called whenever it is necessary to create a subscription for
    /// new slots
    pub fn get_new_slot_notifier(&self) -> NewSlotNotifier {
        let new_slot_senders = Arc::clone(&self.new_slot_senders);
        Arc::new(Box::new(move || {
            let (new_slot_sender, new_slot_receiver) = mpsc::channel();
            new_slot_senders.lock().push(new_slot_sender);
            new_slot_receiver
        }))
    }

    /// Get a handle to the worker.
    pub fn handle(&self) -> PoCWorkerHandle<B> {
        self.handle.clone()
    }
}

impl<B: BlockT> futures::Future for PoCWorker<B> {
    type Output = ();

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut futures::task::Context,
    ) -> futures::task::Poll<Self::Output> {
        self.inner.as_mut().poll(cx)
    }
}

struct PoCSlotWorker<B: BlockT, C, E, I, SO, L, BS> {
    client: Arc<C>,
    block_import: I,
    env: E,
    sync_oracle: SO,
    justification_sync_link: L,
    force_authoring: bool,
    backoff_authoring_blocks: Option<BS>,
    epoch_changes: SharedEpochChanges<B, Epoch>,
    config: Config,
    on_claim_slot: Box<
        dyn Fn(Slot, &Epoch, Salt, u64, mpsc::Sender<(Solution, Vec<u8>)>) + Send + Sync + 'static,
    >,
    spartan: Spartan,
    signing_context: SigningContext,
    block_proposal_slot_portion: SlotProportion,
    max_block_proposal_slot_portion: Option<SlotProportion>,
    telemetry: Option<TelemetryHandle>,
}

impl<B, C, E, I, Error, SO, L, BS> SimpleSlotWorker<B> for PoCSlotWorker<B, C, E, I, SO, L, BS>
where
    B: BlockT,
    C: ProvideRuntimeApi<B>
        + ProvideCache<B>
        + HeaderBackend<B>
        + HeaderMetadata<B, Error = ClientError>,
    C::Api: PoCApi<B>,
    E: Environment<B, Error = Error>,
    E::Proposer: Proposer<B, Error = Error, Transaction = sp_api::TransactionFor<C, B>>,
    I: BlockImport<B, Transaction = sp_api::TransactionFor<C, B>> + Send + Sync + 'static,
    SO: SyncOracle + Send + Clone,
    L: sc_consensus::JustificationSyncLink<B>,
    BS: BackoffAuthoringBlocksStrategy<NumberFor<B>>,
    Error: std::error::Error + Send + From<ConsensusError> + From<I::Error> + 'static,
{
    type EpochData = ViableEpochDescriptor<B::Hash, NumberFor<B>, Epoch>;
    type Claim = (PreDigest, Pair);
    type SyncOracle = SO;
    type JustificationSyncLink = L;
    type CreateProposer =
        Pin<Box<dyn Future<Output = Result<E::Proposer, sp_consensus::Error>> + Send + 'static>>;
    type Proposer = E::Proposer;
    type BlockImport = I;

    fn logging_target(&self) -> &'static str {
        "poc"
    }

    fn block_import(&mut self) -> &mut Self::BlockImport {
        &mut self.block_import
    }

    fn epoch_data(
        &self,
        parent: &B::Header,
        slot: Slot,
    ) -> Result<Self::EpochData, ConsensusError> {
        self.epoch_changes
            .shared_data()
            .epoch_descriptor_for_child_of(
                descendent_query(&*self.client),
                &parent.hash(),
                parent.number().clone(),
                slot,
            )
            .map_err(|e| ConsensusError::ChainLookup(format!("{:?}", e)))?
            .ok_or(sp_consensus::Error::InvalidAuthoritiesSet)
    }

    fn claim_slot(
        &self,
        parent_header: &B::Header,
        slot: Slot,
        epoch_descriptor: &ViableEpochDescriptor<B::Hash, NumberFor<B>, Epoch>,
    ) -> Option<Self::Claim> {
        debug!(target: "poc", "Attempting to claim slot {}", slot);

        let epoch_changes = self.epoch_changes.shared_data();
        let epoch = epoch_changes
            .viable_epoch(&epoch_descriptor, |slot| Epoch::genesis(&self.config, slot))?;
        let block_id = BlockId::Hash(parent_header.hash());
        // Here we always use parent block as the source of information, thus on the edge of the era
        // the very first block of the era still uses solution range from the previous one, but the
        // block after it uses "next" solution range deposited in the first block.
        let solution_range = find_next_solution_range_digest::<B>(&parent_header)
            .ok()?
            .map(|d| d.solution_range)
            .or_else(|| {
                // We use runtime API as it will fallback to default value for genesis when there is
                // no solution range stored yet
                self.client.runtime_api().solution_range(&block_id).ok()
            })?;
        // Here we always use parent block as the source of information, thus on the edge of the eon
        // the very first block of the eon still uses salt from the previous one, but the
        // block after it uses "next" salt deposited in the first block.
        let salt = find_next_salt_digest::<B>(&parent_header)
            .ok()?
            .map(|d| d.salt)
            .or_else(|| {
                // We use runtime API as it will fallback to default value for genesis when there is
                // no salt stored yet
                self.client.runtime_api().salt(&block_id).ok()
            })?;

        let (solution_sender, solution_receiver) = mpsc::channel();

        (self.on_claim_slot)(
            slot,
            epoch.as_ref(),
            salt.to_le_bytes(),
            solution_range,
            solution_sender,
        );

        while let Ok((solution, secret_key)) = solution_receiver.recv() {
            // TODO: We need also need to check for equivocation of farmers connected to *this node*
            //  during block import, currently farmers connected to this node are considered trusted
            if self
                .client
                .runtime_api()
                .is_in_block_list(&block_id, &solution.public_key)
                .ok()?
            {
                warn!(
                    target: "poc",
                    "Ignoring solution for slot {} provided by farmer in block list: {}",
                    slot,
                    solution.public_key,
                );

                continue;
            }

            let secret_key = SecretKey::from_bytes(&secret_key).ok()?;

            match verification::verify_solution::<B>(
                &solution,
                epoch.as_ref(),
                solution_range,
                slot,
                salt.to_le_bytes(),
                &self.spartan,
                &self.signing_context,
            ) {
                Ok(_) => {
                    debug!(target: "poc", "Claimed slot {}", slot);

                    return Some((PreDigest { solution, slot }, secret_key.into()));
                }
                Err(error) => {
                    warn!(target: "poc", "Invalid solution received for slot {}: {:?}", slot, error);
                }
            }
        }

        None
    }

    fn pre_digest_data(
        &self,
        _slot: Slot,
        claim: &Self::Claim,
    ) -> Vec<sp_runtime::DigestItem<B::Hash>> {
        vec![<DigestItemFor<B> as CompatibleDigestItem>::poc_pre_digest(
            claim.0.clone(),
        )]
    }

    fn block_import_params(
        &self,
    ) -> Box<
        dyn Fn(
                B::Header,
                &B::Hash,
                Vec<B::Extrinsic>,
                StorageChanges<I::Transaction, B>,
                Self::Claim,
                Self::EpochData,
            )
                -> Result<sc_consensus::BlockImportParams<B, I::Transaction>, sp_consensus::Error>
            + Send
            + 'static,
    > {
        Box::new(
            move |header,
                  header_hash,
                  body,
                  storage_changes,
                  (_pre_digest, keypair),
                  epoch_descriptor| {
                // sign the pre-sealed hash of the block and then
                // add it to a digest item.
                let signature = keypair.sign(header_hash.as_ref());
                let digest_item =
                    <DigestItemFor<B> as CompatibleDigestItem>::poc_seal(signature.into());

                let mut import_block = BlockImportParams::new(BlockOrigin::Own, header);
                import_block.post_digests.push(digest_item);
                import_block.body = Some(body);
                import_block.state_action = StateAction::ApplyChanges(
                    sc_consensus::StorageChanges::Changes(storage_changes),
                );
                import_block.intermediates.insert(
                    Cow::from(INTERMEDIATE_KEY),
                    Box::new(PoCIntermediate::<B> { epoch_descriptor }) as Box<_>,
                );

                Ok(import_block)
            },
        )
    }

    fn force_authoring(&self) -> bool {
        self.force_authoring
    }

    fn should_backoff(&self, slot: Slot, chain_head: &B::Header) -> bool {
        if let Some(ref strategy) = self.backoff_authoring_blocks {
            if let Ok(chain_head_slot) = find_pre_digest::<B>(chain_head).map(|digest| digest.slot)
            {
                return strategy.should_backoff(
                    *chain_head.number(),
                    chain_head_slot,
                    self.client.info().finalized_number,
                    slot,
                    self.logging_target(),
                );
            }
        }
        false
    }

    fn sync_oracle(&mut self) -> &mut Self::SyncOracle {
        &mut self.sync_oracle
    }

    fn justification_sync_link(&mut self) -> &mut Self::JustificationSyncLink {
        &mut self.justification_sync_link
    }

    fn proposer(&mut self, block: &B::Header) -> Self::CreateProposer {
        Box::pin(
            self.env
                .init(block)
                .map_err(|e| sp_consensus::Error::ClientImport(format!("{:?}", e))),
        )
    }

    fn telemetry(&self) -> Option<TelemetryHandle> {
        self.telemetry.clone()
    }

    fn proposing_remaining_duration(&self, slot_info: &SlotInfo<B>) -> std::time::Duration {
        let parent_slot = find_pre_digest::<B>(&slot_info.chain_head)
            .ok()
            .map(|d| d.slot);

        sc_consensus_slots::proposing_remaining_duration(
            parent_slot,
            slot_info,
            &self.block_proposal_slot_portion,
            self.max_block_proposal_slot_portion.as_ref(),
            sc_consensus_slots::SlotLenienceType::Exponential,
            self.logging_target(),
        )
    }

    fn authorities_len(&self, _epoch_data: &Self::EpochData) -> Option<usize> {
        None
    }
}

/// Extract the PoC pre digest from the given header. Pre-runtime digests are
/// mandatory, the function will return `Err` if none is found.
pub fn find_pre_digest<B: BlockT>(header: &B::Header) -> Result<PreDigest, Error<B>> {
    // genesis block doesn't contain a pre digest so let's generate a
    // dummy one to not break any invariants in the rest of the code
    if header.number().is_zero() {
        return Ok(PreDigest {
            slot: Slot::from(0),
            solution: Solution::get_for_genesis(),
        });
    }

    let mut pre_digest: Option<_> = None;
    for log in header.digest().logs() {
        trace!(target: "poc", "Checking log {:?}, looking for pre runtime digest", log);
        match (log.as_poc_pre_digest(), pre_digest.is_some()) {
            (Some(_), true) => return Err(poc_err(Error::MultiplePreRuntimeDigests)),
            (None, _) => trace!(target: "poc", "Ignoring digest not meant for us"),
            (s, false) => pre_digest = s,
        }
    }
    pre_digest.ok_or_else(|| poc_err(Error::NoPreRuntimeDigest))
}

/// Extract the PoC epoch change digest from the given header, if it exists.
fn find_next_epoch_digest<B: BlockT>(
    header: &B::Header,
) -> Result<Option<NextEpochDescriptor>, Error<B>>
where
    DigestItemFor<B>: CompatibleDigestItem,
{
    let mut epoch_digest: Option<_> = None;
    for log in header.digest().logs() {
        trace!(target: "poc", "Checking log {:?}, looking for epoch change digest.", log);
        let log = log.try_to::<ConsensusLog>(OpaqueDigestItemId::Consensus(&POC_ENGINE_ID));
        match (log, epoch_digest.is_some()) {
            (Some(ConsensusLog::NextEpochData(_)), true) => {
                return Err(poc_err(Error::MultipleEpochChangeDigests))
            }
            (Some(ConsensusLog::NextEpochData(epoch)), false) => epoch_digest = Some(epoch),
            _ => trace!(target: "poc", "Ignoring digest not meant for us"),
        }
    }

    Ok(epoch_digest)
}

/// Extract the PoC config change digest from the given header, if it exists.
fn find_next_config_digest<B: BlockT>(
    header: &B::Header,
) -> Result<Option<NextConfigDescriptor>, Error<B>>
where
    DigestItemFor<B>: CompatibleDigestItem,
{
    let mut config_digest: Option<_> = None;
    for log in header.digest().logs() {
        trace!(target: "poc", "Checking log {:?}, looking for epoch change digest.", log);
        let log = log.try_to::<ConsensusLog>(OpaqueDigestItemId::Consensus(&POC_ENGINE_ID));
        match (log, config_digest.is_some()) {
            (Some(ConsensusLog::NextConfigData(_)), true) => {
                return Err(poc_err(Error::MultipleConfigChangeDigests))
            }
            (Some(ConsensusLog::NextConfigData(config)), false) => config_digest = Some(config),
            _ => trace!(target: "poc", "Ignoring digest not meant for us"),
        }
    }

    Ok(config_digest)
}

/// Extract the PoC solution range digest from the given header.
fn find_solution_range_digest<B: BlockT>(
    header: &B::Header,
) -> Result<Option<SolutionRangeDescriptor>, Error<B>>
where
    DigestItemFor<B>: CompatibleDigestItem,
{
    let mut solution_range_digest: Option<_> = None;
    for log in header.digest().logs() {
        trace!(target: "poc", "Checking log {:?}, looking for solution range digest.", log);
        let log = log.try_to::<ConsensusLog>(OpaqueDigestItemId::Consensus(&POC_ENGINE_ID));
        match (log, solution_range_digest.is_some()) {
            (Some(ConsensusLog::SolutionRangeData(_)), true) => {
                return Err(poc_err(Error::MultipleSolutionRangeDigests))
            }
            (Some(ConsensusLog::SolutionRangeData(solution_range)), false) => {
                solution_range_digest = Some(solution_range)
            }
            _ => trace!(target: "poc", "Ignoring digest not meant for us"),
        }
    }

    Ok(solution_range_digest)
}

/// Extract the next PoC solution range digest from the given header if it exists.
fn find_next_solution_range_digest<B: BlockT>(
    header: &B::Header,
) -> Result<Option<NextSolutionRangeDescriptor>, Error<B>>
where
    DigestItemFor<B>: CompatibleDigestItem,
{
    let mut next_solution_range_digest: Option<_> = None;
    for log in header.digest().logs() {
        trace!(target: "poc", "Checking log {:?}, looking for next solution range digest.", log);
        let log = log.try_to::<ConsensusLog>(OpaqueDigestItemId::Consensus(&POC_ENGINE_ID));
        match (log, next_solution_range_digest.is_some()) {
            (Some(ConsensusLog::NextSolutionRangeData(_)), true) => {
                return Err(poc_err(Error::MultipleNextSolutionRangeDigests))
            }
            (Some(ConsensusLog::NextSolutionRangeData(solution_range)), false) => {
                next_solution_range_digest = Some(solution_range)
            }
            _ => trace!(target: "poc", "Ignoring digest not meant for us"),
        }
    }

    Ok(next_solution_range_digest)
}

/// Extract the PoC salt digest from the given header.
fn find_salt_digest<B: BlockT>(header: &B::Header) -> Result<Option<SaltDescriptor>, Error<B>>
where
    DigestItemFor<B>: CompatibleDigestItem,
{
    let mut salt_digest: Option<_> = None;
    for log in header.digest().logs() {
        trace!(target: "poc", "Checking log {:?}, looking for salt digest.", log);
        let log = log.try_to::<ConsensusLog>(OpaqueDigestItemId::Consensus(&POC_ENGINE_ID));
        match (log, salt_digest.is_some()) {
            (Some(ConsensusLog::SaltData(_)), true) => {
                return Err(poc_err(Error::MultipleSaltDigests))
            }
            (Some(ConsensusLog::SaltData(salt)), false) => salt_digest = Some(salt),
            _ => trace!(target: "poc", "Ignoring digest not meant for us"),
        }
    }

    Ok(salt_digest)
}

/// Extract the next PoC salt digest from the given header if it exists.
fn find_next_salt_digest<B: BlockT>(
    header: &B::Header,
) -> Result<Option<NextSaltDescriptor>, Error<B>>
where
    DigestItemFor<B>: CompatibleDigestItem,
{
    let mut next_salt_digest: Option<_> = None;
    for log in header.digest().logs() {
        trace!(target: "poc", "Checking log {:?}, looking for salt digest.", log);
        let log = log.try_to::<ConsensusLog>(OpaqueDigestItemId::Consensus(&POC_ENGINE_ID));
        match (log, next_salt_digest.is_some()) {
            (Some(ConsensusLog::NextSaltData(_)), true) => {
                return Err(poc_err(Error::MultipleSaltDigests))
            }
            (Some(ConsensusLog::NextSaltData(salt)), false) => next_salt_digest = Some(salt),
            _ => trace!(target: "poc", "Ignoring digest not meant for us"),
        }
    }

    Ok(next_salt_digest)
}

/// State that must be shared between the import queue and the authoring logic.
#[derive(Clone)]
pub struct PoCLink<Block: BlockT> {
    epoch_changes: SharedEpochChanges<Block, Epoch>,
    config: Config,
}

impl<Block: BlockT> PoCLink<Block> {
    /// Get the epoch changes of this link.
    pub fn epoch_changes(&self) -> &SharedEpochChanges<Block, Epoch> {
        &self.epoch_changes
    }

    /// Get the config of this link.
    pub fn config(&self) -> &Config {
        &self.config
    }
}

/// A verifier for PoC blocks.
pub struct PoCVerifier<Block: BlockT, Client, SelectChain, CAW, CIDP> {
    client: Arc<Client>,
    select_chain: SelectChain,
    create_inherent_data_providers: CIDP,
    config: Config,
    epoch_changes: SharedEpochChanges<Block, Epoch>,
    can_author_with: CAW,
    telemetry: Option<TelemetryHandle>,
    spartan: Spartan,
    signing_context: SigningContext,
}

impl<Block, Client, SelectChain, CAW, CIDP> PoCVerifier<Block, Client, SelectChain, CAW, CIDP>
where
    Block: BlockT,
    Client: AuxStore + HeaderBackend<Block> + HeaderMetadata<Block> + ProvideRuntimeApi<Block>,
    Client::Api: BlockBuilderApi<Block> + PoCApi<Block>,
    SelectChain: sp_consensus::SelectChain<Block>,
    CAW: CanAuthorWith<Block>,
    CIDP: CreateInherentDataProviders<Block, ()>,
{
    async fn check_inherents(
        &self,
        block: Block,
        block_id: BlockId<Block>,
        inherent_data: InherentData,
        create_inherent_data_providers: CIDP::InherentDataProviders,
    ) -> Result<(), Error<Block>> {
        if let Err(e) = self.can_author_with.can_author_with(&block_id) {
            debug!(
                target: "poc",
                "Skipping `check_inherents` as authoring version is not compatible: {}",
                e,
            );

            return Ok(());
        }

        let inherent_res = self
            .client
            .runtime_api()
            .check_inherents(&block_id, block, inherent_data)
            .map_err(Error::RuntimeApi)?;

        if !inherent_res.ok() {
            for (i, e) in inherent_res.into_errors() {
                match create_inherent_data_providers
                    .try_handle_error(&i, &e)
                    .await
                {
                    Some(res) => res.map_err(|e| Error::CheckInherents(e))?,
                    None => return Err(Error::CheckInherentsUnhandled(i)),
                }
            }
        }

        Ok(())
    }

    async fn check_and_report_equivocation(
        &self,
        slot_now: Slot,
        slot: Slot,
        header: &Block::Header,
        author: &FarmerId,
        origin: &BlockOrigin,
    ) -> Result<(), Error<Block>> {
        // don't report any equivocations during initial sync
        // as they are most likely stale.
        if *origin == BlockOrigin::NetworkInitialSync {
            return Ok(());
        }

        // check if authorship of this header is an equivocation and return a proof if so.
        let equivocation_proof =
            match check_equivocation(&*self.client, slot_now, slot, header, author)
                .map_err(Error::Client)?
            {
                Some(proof) => proof,
                None => return Ok(()),
            };

        info!(
            "Slot author {:?} is equivocating at slot {} with headers {:?} and {:?}",
            author,
            slot,
            equivocation_proof.first_header.hash(),
            equivocation_proof.second_header.hash(),
        );

        // get the best block on which we will build and send the equivocation report.
        let best_id = self
            .select_chain
            .best_chain()
            .await
            .map(|h| BlockId::Hash(h.hash()))
            .map_err(|e| Error::Client(e.into()))?;

        // submit equivocation report at best block.
        self.client
            .runtime_api()
            .submit_report_equivocation_unsigned_extrinsic(&best_id, equivocation_proof)
            .map_err(Error::RuntimeApi)?;

        info!(target: "poc", "Submitted equivocation report for author {:?}", author);

        Ok(())
    }
}

type BlockVerificationResult<Block> = Result<
    (
        BlockImportParams<Block, ()>,
        Option<Vec<(CacheKeyId, Vec<u8>)>>,
    ),
    String,
>;

#[async_trait::async_trait]
impl<Block, Client, SelectChain, CAW, CIDP> Verifier<Block>
    for PoCVerifier<Block, Client, SelectChain, CAW, CIDP>
where
    Block: BlockT,
    Client: HeaderMetadata<Block, Error = sp_blockchain::Error>
        + HeaderBackend<Block>
        + ProvideRuntimeApi<Block>
        + Send
        + Sync
        + AuxStore
        + ProvideCache<Block>,
    Client::Api: BlockBuilderApi<Block> + PoCApi<Block>,
    SelectChain: sp_consensus::SelectChain<Block>,
    CAW: CanAuthorWith<Block> + Send + Sync,
    CIDP: CreateInherentDataProviders<Block, ()> + Send + Sync,
    CIDP::InherentDataProviders: InherentDataProviderExt + Send + Sync,
{
    async fn verify(
        &mut self,
        origin: BlockOrigin,
        header: Block::Header,
        justifications: Option<Justifications>,
        mut body: Option<Vec<Block::Extrinsic>>,
    ) -> BlockVerificationResult<Block> {
        trace!(
            target: "poc",
            "Verifying origin: {:?} header: {:?} justification(s): {:?} body: {:?}",
            origin,
            header,
            justifications,
            body,
        );

        let hash = header.hash();
        let parent_hash = *header.parent_hash();

        debug!(target: "poc", "We have {:?} logs in this header", header.digest().logs().len());

        let create_inherent_data_providers = self
            .create_inherent_data_providers
            .create_inherent_data_providers(parent_hash, ())
            .await
            .map_err(|e| Error::<Block>::Client(sp_consensus::Error::from(e).into()))?;

        let slot_now = create_inherent_data_providers.slot();

        let parent_header_metadata = self
            .client
            .header_metadata(parent_hash)
            .map_err(Error::<Block>::FetchParentHeader)?;

        let pre_digest = find_pre_digest::<Block>(&header)?;
        let (check_header, epoch_descriptor) = {
            let epoch_changes = self.epoch_changes.shared_data();
            let epoch_descriptor = epoch_changes
                .epoch_descriptor_for_child_of(
                    descendent_query(&*self.client),
                    &parent_hash,
                    parent_header_metadata.number,
                    pre_digest.slot,
                )
                .map_err(|e| Error::<Block>::ForkTree(Box::new(e)))?
                .ok_or_else(|| Error::<Block>::FetchEpoch(parent_hash))?;
            let viable_epoch = epoch_changes
                .viable_epoch(&epoch_descriptor, |slot| Epoch::genesis(&self.config, slot))
                .ok_or_else(|| Error::<Block>::FetchEpoch(parent_hash))?;
            // TODO: Is it actually secure to validate it using solution range digest?
            let solution_range = find_solution_range_digest::<Block>(&header)?
                .ok_or_else(|| Error::<Block>::MissingSolutionRange(hash))?
                .solution_range;
            let salt = find_salt_digest::<Block>(&header)?
                .ok_or_else(|| Error::<Block>::MissingSalt(hash))?
                .salt;

            if self
                .client
                .runtime_api()
                .is_in_block_list(&BlockId::Hash(parent_hash), &pre_digest.solution.public_key)
                .map_err(Error::<Block>::RuntimeApi)?
            {
                warn!(
                    target: "poc",
                    "Ignoring block with solution provided by farmer in block list: {}",
                    pre_digest.solution.public_key
                );

                return Err(
                    Error::<Block>::FarmerInBlockList(pre_digest.solution.public_key).into(),
                );
            }

            // We add one to the current slot to allow for some small drift.
            // FIXME #1019 in the future, alter this queue to allow deferring of headers
            let v_params = verification::VerificationParams {
                header: header.clone(),
                pre_digest: Some(pre_digest),
                slot_now: slot_now + 1,
                epoch: viable_epoch.as_ref(),
                solution_range,
                salt: salt.to_le_bytes(),
                spartan: &self.spartan,
                signing_context: &self.signing_context,
            };

            (
                verification::check_header::<Block>(v_params)?,
                epoch_descriptor,
            )
        };

        match check_header {
            CheckedHeader::Checked(pre_header, verified_info) => {
                let poc_pre_digest = verified_info
                    .pre_digest
                    .as_poc_pre_digest()
                    .expect("check_header always returns a pre-digest digest item; qed");
                let slot = poc_pre_digest.slot;

                // the header is valid but let's check if there was something else already
                // proposed at the same slot by the given author. if there was, we will
                // report the equivocation to the runtime.
                if let Err(err) = self
                    .check_and_report_equivocation(
                        slot_now,
                        slot,
                        &header,
                        &poc_pre_digest.solution.public_key,
                        &origin,
                    )
                    .await
                {
                    warn!(target: "poc", "Error checking/reporting PoC equivocation: {:?}", err);
                }

                // if the body is passed through, we need to use the runtime
                // to check that the internally-set timestamp in the inherents
                // actually matches the slot set in the seal.
                if let Some(inner_body) = body.take() {
                    let mut inherent_data = create_inherent_data_providers
                        .create_inherent_data()
                        .map_err(Error::<Block>::CreateInherents)?;
                    inherent_data.poc_replace_inherent_data(slot);
                    let block = Block::new(pre_header.clone(), inner_body);

                    self.check_inherents(
                        block.clone(),
                        BlockId::Hash(parent_hash),
                        inherent_data,
                        create_inherent_data_providers,
                    )
                    .await?;

                    let (_, inner_body) = block.deconstruct();
                    body = Some(inner_body);
                }

                trace!(target: "poc", "Checked {:?}; importing.", pre_header);
                telemetry!(
                    self.telemetry;
                    CONSENSUS_TRACE;
                    "poc.checked_and_importing";
                    "pre_header" => ?pre_header,
                );

                let mut import_block = BlockImportParams::new(origin, pre_header);
                import_block.post_digests.push(verified_info.seal);
                import_block.body = body;
                import_block.justifications = justifications;
                import_block.intermediates.insert(
                    Cow::from(INTERMEDIATE_KEY),
                    Box::new(PoCIntermediate::<Block> { epoch_descriptor }) as Box<_>,
                );
                import_block.post_hash = Some(hash);

                Ok((import_block, Default::default()))
            }
            CheckedHeader::Deferred(a, b) => {
                debug!(target: "poc", "Checking {:?} failed; {:?}, {:?}.", hash, a, b);
                telemetry!(
                    self.telemetry;
                    CONSENSUS_DEBUG;
                    "poc.header_too_far_in_future";
                    "hash" => ?hash, "a" => ?a, "b" => ?b
                );
                Err(Error::<Block>::TooFarInFuture(hash).into())
            }
        }
    }
}

/// A block-import handler for PoC.
///
/// This scans each imported block for epoch change signals. The signals are
/// tracked in a tree (of all forks), and the import logic validates all epoch
/// change transitions, i.e. whether a given epoch change is expected or whether
/// it is missing.
///
/// The epoch change tree should be pruned as blocks are finalized.
pub struct PoCBlockImport<Block: BlockT, Client, I> {
    inner: I,
    client: Arc<Client>,
    epoch_changes: SharedEpochChanges<Block, Epoch>,
    config: Config,
}

impl<Block: BlockT, I: Clone, Client> Clone for PoCBlockImport<Block, Client, I> {
    fn clone(&self) -> Self {
        PoCBlockImport {
            inner: self.inner.clone(),
            client: self.client.clone(),
            epoch_changes: self.epoch_changes.clone(),
            config: self.config.clone(),
        }
    }
}

impl<Block: BlockT, Client, I> PoCBlockImport<Block, Client, I> {
    fn new(
        client: Arc<Client>,
        epoch_changes: SharedEpochChanges<Block, Epoch>,
        block_import: I,
        config: Config,
    ) -> Self {
        PoCBlockImport {
            client,
            inner: block_import,
            epoch_changes,
            config,
        }
    }
}

#[async_trait::async_trait]
impl<Block, Client, Inner> BlockImport<Block> for PoCBlockImport<Block, Client, Inner>
where
    Block: BlockT,
    Inner: BlockImport<Block, Transaction = sp_api::TransactionFor<Client, Block>> + Send + Sync,
    Inner::Error: Into<ConsensusError>,
    Client: HeaderBackend<Block>
        + HeaderMetadata<Block, Error = sp_blockchain::Error>
        + AuxStore
        + ProvideRuntimeApi<Block>
        + ProvideCache<Block>
        + Send
        + Sync,
    Client::Api: PoCApi<Block> + ApiExt<Block>,
{
    type Error = ConsensusError;
    type Transaction = sp_api::TransactionFor<Client, Block>;

    async fn import_block(
        &mut self,
        mut block: BlockImportParams<Block, Self::Transaction>,
        new_cache: HashMap<CacheKeyId, Vec<u8>>,
    ) -> Result<ImportResult, Self::Error> {
        let hash = block.post_hash();
        let number = *block.header.number();

        // early exit if block already in chain, otherwise the check for
        // epoch changes will error when trying to re-import an epoch change
        match self.client.status(BlockId::Hash(hash)) {
            Ok(sp_blockchain::BlockStatus::InChain) => {
                // When re-importing existing block strip away intermediates.
                let _ = block.take_intermediate::<PoCIntermediate<Block>>(INTERMEDIATE_KEY)?;
                block.fork_choice = Some(ForkChoiceStrategy::Custom(false));
                return self
                    .inner
                    .import_block(block, new_cache)
                    .await
                    .map_err(Into::into);
            }
            Ok(sp_blockchain::BlockStatus::Unknown) => {}
            Err(e) => return Err(ConsensusError::ClientImport(e.to_string())),
        }

        let pre_digest = find_pre_digest::<Block>(&block.header).expect(
            "valid PoC headers must contain a predigest; \
					 header has been already verified; qed",
        );
        let slot = pre_digest.slot;

        let parent_hash = *block.header.parent_hash();
        let parent_header = self
            .client
            .header(BlockId::Hash(parent_hash))
            .map_err(|e| ConsensusError::ChainLookup(e.to_string()))?
            .ok_or_else(|| {
                ConsensusError::ChainLookup(
                    poc_err(Error::<Block>::ParentUnavailable(parent_hash, hash)).into(),
                )
            })?;

        let parent_slot = find_pre_digest::<Block>(&parent_header)
            .map(|d| d.slot)
            .expect(
                "parent is non-genesis; valid PoC headers contain a pre-digest; \
					header has already been verified; qed",
            );

        // make sure that slot number is strictly increasing
        if slot <= parent_slot {
            return Err(ConsensusError::ClientImport(
                poc_err(Error::<Block>::SlotMustIncrease(parent_slot, slot)).into(),
            ));
        }

        // if there's a pending epoch we'll save the previous epoch changes here
        // this way we can revert it if there's any error
        let mut old_epoch_changes = None;

        // Use an extra scope to make the compiler happy, because otherwise he complains about the
        // mutex, even if we dropped it...
        let mut epoch_changes = {
            let mut epoch_changes = self.epoch_changes.shared_data_locked();

            // check if there's any epoch change expected to happen at this slot.
            // `epoch` is the epoch to verify the block under, and `first_in_epoch` is true
            // if this is the first block in its chain for that epoch.
            //
            // also provides the total weight of the chain, including the imported block.
            let (epoch_descriptor, first_in_epoch, parent_weight) = {
                let parent_weight = if *parent_header.number() == Zero::zero() {
                    0
                } else {
                    aux_schema::load_block_weight(&*self.client, parent_hash)
                        .map_err(|e| ConsensusError::ClientImport(e.to_string()))?
                        .ok_or_else(|| {
                            ConsensusError::ClientImport(
                                poc_err(Error::<Block>::ParentBlockNoAssociatedWeight(hash)).into(),
                            )
                        })?
                };

                let intermediate =
                    block.take_intermediate::<PoCIntermediate<Block>>(INTERMEDIATE_KEY)?;

                let epoch_descriptor = intermediate.epoch_descriptor;
                let first_in_epoch = parent_slot < epoch_descriptor.start_slot();
                (epoch_descriptor, first_in_epoch, parent_weight)
            };

            let total_weight = parent_weight + pre_digest.added_weight();

            // search for this all the time so we can reject unexpected announcements.
            let next_epoch_digest = find_next_epoch_digest::<Block>(&block.header)
                .map_err(|e| ConsensusError::ClientImport(e.to_string()))?;
            let next_config_digest = find_next_config_digest::<Block>(&block.header)
                .map_err(|e| ConsensusError::ClientImport(e.to_string()))?;

            match (
                first_in_epoch,
                next_epoch_digest.is_some(),
                next_config_digest.is_some(),
            ) {
                (true, true, _) => {}
                (false, false, false) => {}
                (false, false, true) => {
                    return Err(ConsensusError::ClientImport(
                        poc_err(Error::<Block>::UnexpectedConfigChange).into(),
                    ))
                }
                (true, false, _) => {
                    return Err(ConsensusError::ClientImport(
                        poc_err(Error::<Block>::ExpectedEpochChange(hash, slot)).into(),
                    ))
                }
                (false, true, _) => {
                    return Err(ConsensusError::ClientImport(
                        poc_err(Error::<Block>::UnexpectedEpochChange).into(),
                    ))
                }
            }

            let info = self.client.info();

            if let Some(next_epoch_descriptor) = next_epoch_digest {
                old_epoch_changes = Some((*epoch_changes).clone());

                let viable_epoch = epoch_changes
                    .viable_epoch(&epoch_descriptor, |slot| Epoch::genesis(&self.config, slot))
                    .ok_or_else(|| {
                        ConsensusError::ClientImport(Error::<Block>::FetchEpoch(parent_hash).into())
                    })?;

                let epoch_config = next_config_digest
                    .map(Into::into)
                    .unwrap_or_else(|| viable_epoch.as_ref().config.clone());

                // restrict info logging during initial sync to avoid spam
                let log_level = if block.origin == BlockOrigin::NetworkInitialSync {
                    log::Level::Debug
                } else {
                    log::Level::Info
                };

                log!(target: "poc",
                     log_level,
                     "🧑‍🌾 New epoch {} launching at block {} (block slot {} >= start slot {}).",
                     viable_epoch.as_ref().epoch_index,
                     hash,
                     slot,
                     viable_epoch.as_ref().start_slot,
                );

                let next_epoch = viable_epoch.increment((next_epoch_descriptor, epoch_config));

                log!(target: "poc",
                     log_level,
                     "🧑‍🌾 Next epoch starts at slot {}",
                     next_epoch.as_ref().start_slot,
                );

                // prune the tree of epochs not part of the finalized chain or
                // that are not live anymore, and then track the given epoch change
                // in the tree.
                // NOTE: it is important that these operations are done in this
                // order, otherwise if pruning after import the `is_descendent_of`
                // used by pruning may not know about the block that is being
                // imported.
                let prune_and_import = || {
                    prune_finalized(self.client.clone(), &mut epoch_changes)?;

                    epoch_changes
                        .import(
                            descendent_query(&*self.client),
                            hash,
                            number,
                            *block.header.parent_hash(),
                            next_epoch,
                        )
                        .map_err(|e| ConsensusError::ClientImport(format!("{:?}", e)))?;

                    Ok(())
                };

                if let Err(e) = prune_and_import() {
                    debug!(target: "poc", "Failed to launch next epoch: {:?}", e);
                    *epoch_changes =
                        old_epoch_changes.expect("set `Some` above and not taken; qed");
                    return Err(e);
                }

                crate::aux_schema::write_epoch_changes::<Block, _, _>(&*epoch_changes, |insert| {
                    block
                        .auxiliary
                        .extend(insert.iter().map(|(k, v)| (k.to_vec(), Some(v.to_vec()))))
                });
            }

            aux_schema::write_block_weight(hash, total_weight, |values| {
                block
                    .auxiliary
                    .extend(values.iter().map(|(k, v)| (k.to_vec(), Some(v.to_vec()))))
            });

            // The fork choice rule is that we pick the heaviest chain (i.e.
            // more primary blocks), if there's a tie we go with the longest
            // chain.
            block.fork_choice = {
                let (last_best, last_best_number) = (info.best_hash, info.best_number);

                let last_best_weight = if &last_best == block.header.parent_hash() {
                    // the parent=genesis case is already covered for loading parent weight,
                    // so we don't need to cover again here.
                    parent_weight
                } else {
                    aux_schema::load_block_weight(&*self.client, last_best)
                        .map_err(|e| ConsensusError::ChainLookup(format!("{:?}", e)))?
                        .ok_or_else(|| {
                            ConsensusError::ChainLookup(
                                "No block weight for parent header.".to_string(),
                            )
                        })?
                };

                Some(ForkChoiceStrategy::Custom(
                    if total_weight > last_best_weight {
                        true
                    } else if total_weight == last_best_weight {
                        number > last_best_number
                    } else {
                        false
                    },
                ))
            };

            // Release the mutex, but it stays locked
            epoch_changes.release_mutex()
        };

        let import_result = self.inner.import_block(block, new_cache).await;

        // revert to the original epoch changes in case there's an error
        // importing the block
        if import_result.is_err() {
            if let Some(old_epoch_changes) = old_epoch_changes {
                *epoch_changes.upgrade() = old_epoch_changes;
            }
        }

        import_result.map_err(Into::into)
    }

    async fn check_block(
        &mut self,
        block: BlockCheckParams<Block>,
    ) -> Result<ImportResult, Self::Error> {
        self.inner.check_block(block).await.map_err(Into::into)
    }
}

/// Gets the best finalized block and its slot, and prunes the given epoch tree.
fn prune_finalized<Block, Client>(
    client: Arc<Client>,
    epoch_changes: &mut EpochChangesFor<Block, Epoch>,
) -> Result<(), ConsensusError>
where
    Block: BlockT,
    Client: HeaderBackend<Block> + HeaderMetadata<Block, Error = sp_blockchain::Error>,
{
    let info = client.info();

    let finalized_slot = {
        let finalized_header = client
            .header(BlockId::Hash(info.finalized_hash))
            .map_err(|e| ConsensusError::ClientImport(format!("{:?}", e)))?
            .expect(
                "best finalized hash was given by client; \
				 finalized headers must exist in db; qed",
            );

        find_pre_digest::<Block>(&finalized_header)
            .expect(
                "finalized header must be valid; \
					 valid blocks have a pre-digest; qed",
            )
            .slot
    };

    epoch_changes
        .prune_finalized(
            descendent_query(&*client),
            &info.finalized_hash,
            info.finalized_number,
            finalized_slot,
        )
        .map_err(|e| ConsensusError::ClientImport(format!("{:?}", e)))?;

    Ok(())
}

/// Produce a PoC block-import object to be used later on in the construction of
/// an import-queue.
///
/// Also returns a link object used to correctly instantiate the import queue
/// and background worker.
pub fn block_import<Client, Block: BlockT, I>(
    config: Config,
    wrapped_block_import: I,
    client: Arc<Client>,
) -> ClientResult<(PoCBlockImport<Block, Client, I>, PoCLink<Block>)>
where
    Client: AuxStore + HeaderBackend<Block> + HeaderMetadata<Block, Error = sp_blockchain::Error>,
{
    let epoch_changes = aux_schema::load_epoch_changes::<Block, _>(&*client, &config)?;
    let link = PoCLink {
        epoch_changes: epoch_changes.clone(),
        config: config.clone(),
    };

    // NOTE: this isn't entirely necessary, but since we didn't use to prune the
    // epoch tree it is useful as a migration, so that nodes prune long trees on
    // startup rather than waiting until importing the next epoch change block.
    prune_finalized(client.clone(), &mut epoch_changes.shared_data())?;

    let import = PoCBlockImport::new(client, epoch_changes, wrapped_block_import, config);

    Ok((import, link))
}

/// Start an import queue for the PoC consensus algorithm.
///
/// This method returns the import queue, some data that needs to be passed to the block authoring
/// logic (`PocLink`), and a future that must be run to
/// completion and is responsible for listening to finality notifications and
/// pruning the epoch changes tree.
///
/// The block import object provided must be the `PocBlockImport` or a wrapper
/// of it, otherwise crucial import logic will be omitted.
pub fn import_queue<Block: BlockT, Client, SelectChain, Inner, CAW, CIDP>(
    poc_link: PoCLink<Block>,
    block_import: Inner,
    justification_import: Option<BoxJustificationImport<Block>>,
    client: Arc<Client>,
    select_chain: SelectChain,
    create_inherent_data_providers: CIDP,
    spawner: &impl sp_core::traits::SpawnEssentialNamed,
    registry: Option<&Registry>,
    can_author_with: CAW,
    telemetry: Option<TelemetryHandle>,
) -> ClientResult<DefaultImportQueue<Block, Client>>
where
    Inner: BlockImport<
            Block,
            Error = ConsensusError,
            Transaction = sp_api::TransactionFor<Client, Block>,
        > + Send
        + Sync
        + 'static,
    Client: ProvideRuntimeApi<Block>
        + ProvideCache<Block>
        + HeaderBackend<Block>
        + HeaderMetadata<Block, Error = sp_blockchain::Error>
        + AuxStore
        + Send
        + Sync
        + 'static,
    Client::Api: BlockBuilderApi<Block> + PoCApi<Block> + ApiExt<Block>,
    SelectChain: sp_consensus::SelectChain<Block> + 'static,
    CAW: CanAuthorWith<Block> + Send + Sync + 'static,
    CIDP: CreateInherentDataProviders<Block, ()> + Send + Sync + 'static,
    CIDP::InherentDataProviders: InherentDataProviderExt + Send + Sync,
{
    let verifier = PoCVerifier {
        select_chain,
        create_inherent_data_providers,
        config: poc_link.config,
        epoch_changes: poc_link.epoch_changes,
        can_author_with,
        telemetry,
        client,
        spartan: Spartan::new(),
        // TODO: Figure out how to remove explicit schnorrkel dependency
        signing_context: schnorrkel::context::signing_context(SIGNING_CONTEXT),
    };

    Ok(BasicQueue::new(
        verifier,
        Box::new(block_import),
        justification_import,
        spawner,
        registry,
    ))
}

pub(crate) fn create_global_challenge(epoch: &Epoch, slot: Slot) -> [u8; 8] {
    digest::digest(&digest::SHA256, &{
        let mut data = Vec::with_capacity(epoch.randomness.len() + std::mem::size_of::<Slot>());
        data.extend_from_slice(&epoch.randomness);
        data.extend_from_slice(&slot.to_le_bytes());
        data
    })
    .as_ref()[..8]
        .try_into()
        .unwrap()
}
