use crate::{
    database::{Database, Error as DatabaseError},
    ethereum::ProviderStack,
};
use async_stream::try_stream;
use ethers::{
    providers::{LogQueryError, Middleware, ProviderError},
    types::{Filter, Log, U64},
};
use futures::{Stream, StreamExt};
use std::{cmp::max, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::time::sleep;
use tracing::{error, info};

pub struct CachingLogQuery {
    provider:                  Arc<ProviderStack>,
    filter:                    Filter,
    page_size:                 u64,
    confirmation_blocks_delay: u64,
    database:                  Option<Arc<Database>>,
}

#[derive(Error, Debug)]
pub enum Error<ProviderError> {
    #[error("couldn't load last block number: {0}")]
    LoadLastBlock(#[source] ProviderError),
    #[error("error loading logs")]
    LoadLogs(#[from] LogQueryError<ProviderError>),
    #[error(transparent)]
    Database(#[from] DatabaseError),
    #[error("couldn't parse log json: {0}")]
    Parse(#[source] serde_json::Error),
    #[error("couldn't serialize log to json: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("empty block index")]
    EmptyBlockIndex,
    #[error("empty transaction index")]
    EmptyTransactionIndex,
    #[error("empty log index")]
    EmptyLogIndex,
    #[error("block index out of range: {0}")]
    BlockIndexOutOfRange(String),
    #[error("transaction index out of range: {0}")]
    TransactionIndexOutOfRange(String),
    #[error("log index out of range: {0}")]
    LogIndexOutOfRange(String),
}

impl CachingLogQuery {
    pub fn new(provider: Arc<ProviderStack>, filter: &Filter) -> Self {
        Self {
            provider,
            filter: filter.clone(),
            page_size: 10000,
            confirmation_blocks_delay: 0,
            database: None,
        }
    }

    /// set page size for pagination
    pub const fn with_page_size(mut self, page_size: u64) -> Self {
        self.page_size = page_size;
        self
    }

    pub const fn with_blocks_delay(mut self, confirmation_blocks_delay: u64) -> Self {
        self.confirmation_blocks_delay = confirmation_blocks_delay;
        self
    }

    pub fn with_database(mut self, database: Arc<Database>) -> Self {
        self.database = Some(database);
        self
    }

    pub fn into_stream(self) -> impl Stream<Item = Result<Log, Error<ProviderError>>> {
        try_stream! {
            let last_block = self.get_block_number().await?;

            info!("Reading MemberAdded events from cache");

            let cached_events = self.load_db_logs().await?;
            for log in cached_events {
                yield serde_json::from_str(&log).map_err(Error::Parse)?;
            }

            let mut last_successful_block = U64::default();
            let mut current_page_size = self.page_size;
            let mut backoff_time = Duration::from_secs(1);

            'restart: loop {
                let filter = self.filter.clone().from_block(max(
                    max(last_successful_block + 1, last_block.db + 1),
                    self.filter.get_from_block().unwrap_or_default(),
                ));

                info!(
                    page_size = current_page_size,
                    from_block = filter.get_from_block().unwrap_or_default().as_u64(),
                    "Reading MemberAdded events from chains"
                );

                let mut stream = self.provider
                    .get_logs_paginated(&filter, current_page_size);

                while let Some(log) = stream.next().await {
                    let log = match log {
                        Err(e) if e.to_string().contains("Query timeout exceeded") => {
                            if current_page_size >= 2000 {
                                error!(error = ?e, "Retriable error, decreasing page size");
                                current_page_size /= 2;
                                continue 'restart;
                            } else if backoff_time <= Duration::from_secs(32) {
                                error!(error = ?e, "Retriable error, backoff");
                                sleep(backoff_time).await;
                                backoff_time *= 2;
                                continue 'restart;
                            } else {
                                error!(error = ?e, "Retriable error, max number of attempts reached");
                                // This seems to be the only way to return errors in the try_stream! macro
                                Err(Error::LoadLogs(e))?;
                                break 'restart;
                            }
                        }
                        Err(e) => {
                            // This seems to be the only way to return errors in the try_stream! macro
                            Err(Error::LoadLogs(e))?;
                            break 'restart;
                        }
                        Ok(log) => log
                    };

                    // If we need to decrease page size later, don't process same blocks twice
                    last_successful_block = log.block_number.unwrap_or(last_successful_block);

                    if self.is_confirmed(&log, last_block) {
                        let raw_log = serde_json::to_string(&log).map_err(Error::Serialize)?;
                        self.cache_log(raw_log, &log).await?;
                    }

                    yield log;
                }

                // We've iterated over all events, we can kill the restart loop
                break;
            }
        }
    }

    async fn get_block_number(&self) -> Result<LastBlock, Error<ProviderError>> {
        let provider = self.provider.provider();
        let last_eth_block = provider
            .get_block_number()
            .await
            .map_err(Error::LoadLastBlock)?;
        let last_db_block: u64 = match &self.database {
            Some(database) => database.get_block_number().await?,
            None => 0,
        };

        Ok(LastBlock {
            eth: last_eth_block,
            db:  U64([last_db_block]),
        })
    }

    async fn load_db_logs(&self) -> Result<Vec<String>, Error<ProviderError>> {
        match &self.database {
            Some(database) => database.load_logs().await.map_err(Error::Database),
            None => Ok(vec![]),
        }
    }

    fn is_confirmed(&self, log: &Log, last_block: LastBlock) -> bool {
        log.block_number.map_or(false, |block| {
            block + self.confirmation_blocks_delay <= last_block.eth
        })
    }

    async fn cache_log(&self, raw_log: String, log: &Log) -> Result<(), Error<ProviderError>> {
        if let Some(database) = &self.database {
            database
                .save_log(
                    log.block_number
                        .ok_or(Error::<ProviderError>::EmptyBlockIndex)?
                        .try_into()
                        .map_err(|e: &str| {
                            Error::<ProviderError>::BlockIndexOutOfRange(e.into())
                        })?,
                    log.transaction_index
                        .ok_or(Error::<ProviderError>::EmptyTransactionIndex)?
                        .try_into()
                        .map_err(|e: &str| {
                            Error::<ProviderError>::TransactionIndexOutOfRange(e.into())
                        })?,
                    log.log_index
                        .ok_or(Error::<ProviderError>::EmptyLogIndex)?
                        .try_into()
                        .map_err(|e: &str| Error::<ProviderError>::LogIndexOutOfRange(e.into()))?,
                    raw_log,
                )
                .await
                .map_err(Error::Database)?;
        }

        Ok(())
    }
}

#[derive(Copy, Clone)]
struct LastBlock {
    eth: U64,
    db:  U64,
}
