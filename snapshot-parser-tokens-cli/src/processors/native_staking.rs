use crate::db_message::{DbMessage, OwnedSqlValue};
use crate::processors::Processor;
use crate::progress_bar::ProgressCounter;
use crate::sql_params;
use crate::stats::ProcessorCallback;
use anyhow::anyhow;
use async_trait::async_trait;
use log::{debug, error};
use rusqlite::ToSql;
use snapshot_parser::stake_meta::generate_stake_meta_collection;
use solana_program::pubkey::Pubkey;
use solana_runtime::bank::Bank;
use std::future::Future;
use std::str::FromStr;
use std::string::ToString;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

pub const NATIVE_STAKE_ACCOUNT_TABLE: &str = "native_stake_accounts";
pub const INSERT_NATIVE_STAKE_ACCOUNT_QUERY: &str = "INSERT OR REPLACE INTO native_stake_accounts (pubkey, withdraw_authority, amount) SELECT ?, ?, ?;";
const MARINADE_NATIVE_STAKE_AUTHORITY_ADDR: &str = "stWirqFCf2Uts1JBL1Jsd3r6VBWhgnpdPxCTe1MFjrq";

pub struct ProcessorNativeStake {
    bank: Arc<Bank>,
    db_sender: Sender<DbMessage>,
    native_stake_counter: Arc<ProgressCounter>,
    native_stake_authority: Pubkey,
}

impl ProcessorNativeStake {
    pub async fn new(
        bank: Arc<Bank>,
        db_sender: Sender<DbMessage>,
        native_stake_counter: Arc<ProgressCounter>,
    ) -> anyhow::Result<Self> {
        let native_stake_authority: Pubkey = Pubkey::from_str(MARINADE_NATIVE_STAKE_AUTHORITY_ADDR)
            .map_err(|e| {
                anyhow!(
                    "Cannot parse native staking authority address {}: {:?}",
                    MARINADE_NATIVE_STAKE_AUTHORITY_ADDR,
                    e
                )
            })?;
        let processor = Self {
            bank,
            db_sender,
            native_stake_counter,
            native_stake_authority,
        };
        processor.create_native_staking_table().await?;
        Ok(processor)
    }

    async fn create_native_staking_table(&self) -> anyhow::Result<usize> {
        let (response_tx, response_rx) = oneshot::channel();
        self.db_sender
            .send(DbMessage::ExecuteSpecial {
                query: "CREATE TABLE native_stake_accounts (
                    pubkey TEXT NOT NULL PRIMARY KEY,
                    withdraw_authority TEXT NOT NULL,
                    amount TEXT NOT NULL
                );"
                .to_string(),
                params: vec![],
                response: response_tx,
            })
            .await?;
        response_rx.await?
    }

    pub async fn process(&mut self) -> anyhow::Result<()> {
        debug!(
            "Loading staking accounts for native staking authority {} from bank...",
            self.native_stake_authority
        );
        let stake_accounts = generate_stake_meta_collection(&self.bank)?;

        for stake_meta in stake_accounts.stake_metas.iter() {
            if stake_meta.stake_authority == self.native_stake_authority {
                insert_native_staking(
                    &self.db_sender,
                    &self.native_stake_counter,
                    &stake_meta.pubkey,
                    &stake_meta.withdraw_authority,
                    stake_meta.active_delegation_lamports,
                )
                .await
                .unwrap_or_else(|e| {
                    error!(
                        "Failed to insert native stake {}: {:?}",
                        stake_meta.pubkey, e
                    );
                    0
                });
            }
        }
        Ok(())
    }
}

impl Processor for ProcessorNativeStake {
    fn name() -> &'static str {
        "Native Stake"
    }
    fn process(&mut self) -> impl Future<Output = anyhow::Result<()>> + Send {
        self.process()
    }
}

#[async_trait]
impl ProcessorCallback for ProcessorNativeStake {
    async fn get_count(&self) -> (String, u64) {
        (
            NATIVE_STAKE_ACCOUNT_TABLE.to_string(),
            self.native_stake_counter.get(),
        )
    }
}

pub async fn insert_native_staking(
    db_sender: &Sender<DbMessage>,
    progress_counter: &Arc<ProgressCounter>,
    pubkey: &Pubkey,
    authorized_withdrawer: &Pubkey,
    delegated_stake: u64,
) -> anyhow::Result<usize> {
    let (response_tx, response_rx) = oneshot::channel();
    let owned_params = sql_params![
        pubkey.to_string(),
        authorized_withdrawer.to_string(),
        delegated_stake.to_string(),
    ];
    db_sender
        .send(DbMessage::Execute {
            query: INSERT_NATIVE_STAKE_ACCOUNT_QUERY.to_string(),
            params: owned_params,
            response: response_tx,
        })
        .await?;
    progress_counter.inc();
    response_rx.await?
}
