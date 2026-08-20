use crate::db_message::{DbMessage, OwnedSqlParams, OwnedSqlValue};
use crate::db_writer::DbWriter;
use crate::processors::Processor;
use crate::progress_bar::ProgressCounter;
use crate::sql_params;
use crate::stats::ProcessorCallback;
use anyhow::anyhow;
use async_trait::async_trait;
use log::debug;
use rusqlite::ToSql;
use snapshot_parser::stake_meta::generate_stake_meta_collection_for_accounts;
use solana_program::pubkey::Pubkey;
use solana_runtime::bank::Bank;
use solana_sdk::account::AccountSharedData;
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
    stake_accounts: Arc<Vec<(Pubkey, AccountSharedData)>>,
    db_sender: Sender<DbMessage>,
    db_writer: DbWriter,
    native_stake_counter: Arc<ProgressCounter>,
    native_stake_authority: Pubkey,
}

impl ProcessorNativeStake {
    pub async fn new(
        bank: Arc<Bank>,
        stake_accounts: Arc<Vec<(Pubkey, AccountSharedData)>>,
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
            stake_accounts,
            db_writer: DbWriter::new(
                db_sender.clone(),
                INSERT_NATIVE_STAKE_ACCOUNT_QUERY,
                native_stake_counter.clone(),
            ),
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
            "Building stake metas for native staking authority {} from {} scanned accounts...",
            self.native_stake_authority,
            self.stake_accounts.len()
        );
        let stake_accounts =
            generate_stake_meta_collection_for_accounts(&self.bank, &self.stake_accounts)?;

        for stake_meta in stake_accounts.stake_metas.iter() {
            if stake_meta.stake_authority == self.native_stake_authority {
                self.db_writer
                    .push(native_stake_row(
                        &stake_meta.pubkey,
                        &stake_meta.withdraw_authority,
                        stake_meta.active_delegation_lamports,
                    ))
                    .await?;
            }
        }
        self.db_writer.flush().await
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

pub fn native_stake_row(
    pubkey: &Pubkey,
    authorized_withdrawer: &Pubkey,
    delegated_stake: u64,
) -> OwnedSqlParams {
    sql_params![
        pubkey.to_string(),
        authorized_withdrawer.to_string(),
        delegated_stake.to_string(),
    ]
}
