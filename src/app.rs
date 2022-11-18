use crate::{
    chain_subscriber::{ChainSubscriber, Error as SubscriberError},
    contracts::{self, Contracts},
    database::{self, Database},
    ethereum::{self, Ethereum},
    identity_committer::IdentityCommitter,
    server::{Error as ServerError, ToResponseCode},
    timed_read_progress_lock::TimedReadProgressLock,
};
use clap::Parser;

use ethers::types::U256;
use eyre::Result as EyreResult;
use futures::{pin_mut, StreamExt, TryFutureExt, TryStreamExt};
use hyper::StatusCode;
use semaphore::{
    merkle_tree::Hasher,
    poseidon_tree::{PoseidonHash, PoseidonTree, Proof},
    Field,
};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use tokio::try_join;
use tracing::{error, instrument, warn, info};

pub type Hash = <PoseidonHash as Hasher>::Hash;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonCommitment {
    pub last_block:  u64,
    pub commitments: Vec<Hash>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexResponse {
    identity_index: usize,
}

pub enum InclusionProofResponse {
    Proof { root: Field, proof: Proof },
    Pending,
}

impl ToResponseCode for InclusionProofResponse {
    fn to_response_code(&self) -> StatusCode {
        match self {
            Self::Proof { .. } => StatusCode::OK,
            Self::Pending => StatusCode::ACCEPTED,
        }
    }
}

impl Serialize for InclusionProofResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            InclusionProofResponse::Proof { root, proof } => {
                let mut state = serializer.serialize_struct("InclusionProof", 2)?;
                state.serialize_field("root", root)?;
                state.serialize_field("proof", proof)?;
                state.end()
            }
            InclusionProofResponse::Pending => serializer.serialize_str("pending"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Parser)]
#[group(skip)]
pub struct Options {
    #[clap(flatten)]
    pub ethereum: ethereum::Options,

    #[clap(flatten)]
    pub contracts: contracts::Options,

    #[clap(flatten)]
    pub database: database::Options,

    /// Block number to start syncing from
    #[clap(long, env, default_value = "0")]
    pub starting_block: u64,

    /// Timeout for the tree lock (seconds).
    #[clap(long, env, default_value = "120")]
    pub lock_timeout: u64,
}

pub struct TreeState {
    pub next_leaf:   usize,
    pub merkle_tree: PoseidonTree,
}

pub type SharedTreeState = Arc<TimedReadProgressLock<TreeState>>;

impl TreeState {
    #[must_use]
    pub fn new(tree_depth: usize, initial_leaf: Field) -> Self {
        Self {
            next_leaf:   0,
            merkle_tree: PoseidonTree::new(tree_depth, initial_leaf),
        }
    }
}

pub struct App {
    database:           Arc<Database>,
    #[allow(dead_code)]
    ethereum:           Ethereum,
    contracts:          Arc<Contracts>,
    identity_committer: IdentityCommitter,
    #[allow(dead_code)]
    chain_subscriber:   ChainSubscriber,
    tree_state:         SharedTreeState,
}

impl App {
    /// # Errors
    ///
    /// Will return `Err` if the internal Ethereum handler errors or if the
    /// `options.storage_file` is not accessible.
    #[allow(clippy::missing_panics_doc)] // TODO
    #[instrument(name = "App::new", level = "debug")]
    pub async fn new(options: Options) -> EyreResult<Self> {
        // Connect to Ethereum and Database
        let (database, (ethereum, contracts)) = {
            let db = Database::new(options.database);

            let eth = Ethereum::new(options.ethereum).and_then(|ethereum| async move {
                let contracts = Contracts::new(options.contracts, ethereum.clone()).await?;
                Ok((ethereum, Arc::new(contracts)))
            });

            // Connect to both in parallel
            try_join!(db, eth)?
        };

        let database = Arc::new(database);

        // Poseidon tree depth is one more than the contract's tree depth
        let mut tree_state = Arc::new(TimedReadProgressLock::new(
            Duration::from_secs(options.lock_timeout),
            TreeState::new(contracts.tree_depth() + 1, contracts.initial_leaf()),
        ));

        let identity_committer =
            IdentityCommitter::new(database.clone(), contracts.clone(), tree_state.clone());
        let mut chain_subscriber = ChainSubscriber::new(
            options.starting_block,
            database.clone(),
            contracts.clone(),
            tree_state.clone(),
        );

        match chain_subscriber.process_events().await {
            Err(SubscriberError::RootMismatch) => {
                error!("Error when rebuilding tree from cache. Retrying with db cache busted.");

                // Create a new empty MerkleTree and wipe out cache db
                tree_state = Arc::new(TimedReadProgressLock::new(
                    Duration::from_secs(options.lock_timeout),
                    TreeState::new(contracts.tree_depth() + 1, contracts.initial_leaf()),
                ));
                database.wipe_cache().await?;

                // Retry
                chain_subscriber = ChainSubscriber::new(
                    options.starting_block,
                    database.clone(),
                    contracts.clone(),
                    tree_state.clone(),
                );
                chain_subscriber.process_events().await?;
            }
            Err(e) => return Err(e.into()),
            Ok(_) => {}
        }

        // Sync with chain on start up
        // TODO: check leaves used to run before historical events. what's up with that?
        chain_subscriber.check_leaves().await;
        chain_subscriber.check_health().await;
        chain_subscriber.start().await;

        identity_committer.start().await;

        Ok(Self {
            database,
            ethereum,
            contracts,
            identity_committer,
            chain_subscriber,
            tree_state,
        })
    }

    /// Queues an insert into the merkle tree.
    ///
    /// # Errors
    ///
    /// Will return `Err` if identity is already queued, or in the tree, or the
    /// queue malfunctions.
    #[instrument(level = "debug", skip_all)]
    pub async fn insert_identity(
        &self,
        group_id: usize,
        commitment: Hash,
    ) -> Result<(), ServerError> {
        if U256::from(group_id) != self.contracts.group_id() {
            return Err(ServerError::InvalidGroupId);
        }

        let tree = self.tree_state.read().await?;

        if commitment == self.contracts.initial_leaf() {
            warn!(?commitment, next = %tree.next_leaf, "Attempt to insert initial leaf.");
            return Err(ServerError::InvalidCommitment);
        }

        // Note the ordering of duplicate checks: since we never want to lose data,
        // pending identities are removed from the DB _after_ they are inserted into the
        // tree. Therefore this order of checks guarantees we will not insert a
        // duplicate.
        if self
            .database
            .pending_identity_exists(group_id, &commitment)
            .await?
        {
            warn!(?commitment, next = %tree.next_leaf, "Pending identity already exists.");
            return Err(ServerError::DuplicateCommitment);
        }

        if let Some(existing) = tree
            .merkle_tree
            .leaves()
            .iter()
            .position(|&x| x == commitment)
        {
            warn!(?existing, ?commitment, next = %tree.next_leaf, "Commitment already exists in tree.");
            return Err(ServerError::DuplicateCommitment);
        };

        self.database
            .insert_pending_identity(group_id, &commitment)
            .await?;

        self.identity_committer.notify_queued().await;

        Ok(())
    }

    /// # Errors
    ///
    /// Will return `Err` if the provided index is out of bounds.
    #[instrument(level = "debug", skip_all)]
    pub async fn inclusion_proof(
        &self,
        group_id: usize,
        commitment: &Hash,
    ) -> Result<InclusionProofResponse, ServerError> {
        if U256::from(group_id) != self.contracts.group_id() {
            return Err(ServerError::InvalidGroupId);
        }

        if commitment == &self.contracts.initial_leaf() {
            return Err(ServerError::InvalidCommitment);
        }

        let tree = self.tree_state.read().await.map_err(|e| {
            error!(?e, "Failed to obtain tree lock in inclusion_proof.");
            panic!("Sequencer potentially deadlocked, terminating.");
            #[allow(unreachable_code)]
            e
        })?;

        if let Some(identity_index) = tree
            .merkle_tree
            .leaves()
            .iter()
            .position(|&x| x == *commitment)
        {
            let proof = tree
                .merkle_tree
                .proof(identity_index)
                .ok_or(ServerError::IndexOutOfBounds)?;
            let root = tree.merkle_tree.root();

            // Locally check the proof
            // TODO: Check the leaf index / path
            if !tree.merkle_tree.verify(*commitment, &proof) {
                error!(
                    ?commitment,
                    ?identity_index,
                    ?root,
                    "Proof does not verify locally."
                );
                panic!("Proof does not verify locally.");
            }

            // Verify the root on chain
            if let Err(error) = self.contracts.assert_valid_root(root).await {
                error!(
                    computed_root = ?root,
                    ?error,
                    "Root mismatch between tree and contract."
                );
                return Err(ServerError::RootMismatch);
            }
            Ok(InclusionProofResponse::Proof { root, proof })
        } else if self
            .database
            .pending_identity_exists(group_id, commitment)
            .await?
        {
            Ok(InclusionProofResponse::Pending)
        } else {
            Err(ServerError::IdentityCommitmentNotFound)
        }
    }

    /// # Errors
    ///
    /// Will return an Error if any of the components cannot be shut down
    /// gracefully.
    pub async fn shutdown(&self) -> eyre::Result<()> {
        info!("Shutting down identity committer.");
        self.identity_committer.shutdown().await
    }
}
