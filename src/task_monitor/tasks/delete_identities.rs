use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result as AnyhowResult;
use tokio::sync::Notify;
use tokio::time::sleep;
use tracing::instrument;

use crate::database::types::{DeletionEntry, UnprocessedCommitment};
use crate::database::Database;
use crate::identity_tree::{Hash, Latest, Status, TreeVersion, TreeVersionReadOps};

pub const MINIMUM_DELETION_BATCH_SIZE: usize = 10;
pub struct DeleteIdentities {
    database: Arc<Database>,
    latest_tree: TreeVersion<Latest>,
    wake_up_notify: Arc<Notify>,
}

impl DeleteIdentities {
    pub fn new(
        database: Arc<Database>,
        latest_tree: TreeVersion<Latest>,
        wake_up_notify: Arc<Notify>,
    ) -> Arc<Self> {
        Arc::new(Self {
            database,
            latest_tree,
            wake_up_notify,
        })
    }

    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        delete_identities_loop(&self.database, &self.latest_tree, &self.wake_up_notify).await
    }
}

async fn delete_identities_loop(
    database: &Database,
    latest_tree: &TreeVersion<Latest>,
    wake_up_notify: &Notify,
) -> AnyhowResult<()> {
    loop {
        // get commits from database
        let deletion_queue = database.get_deletions().await?;
        if deletion_queue.is_empty() || deletion_queue.len() < MINIMUM_DELETION_BATCH_SIZE {
            //TODO: update this sleep time
            sleep(Duration::from_secs(3600)).await;
            continue;
        }

        delete_identities(database, latest_tree, deletion_queue).await?;
        // Notify the identity processing task, that there are new identities
        wake_up_notify.notify_one();
    }
}

#[instrument(level = "info", skip_all)]
async fn delete_identities(
    database: &Database,
    latest_tree: &TreeVersion<Latest>,
    identities: Vec<DeletionEntry>,
) -> AnyhowResult<()> {
    todo!();
    Ok(())
}
