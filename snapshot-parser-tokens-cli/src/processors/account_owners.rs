use crate::db_message::{DbMessage, OwnedSqlValue};
use crate::filters::Filters;
use crate::processors::processor::Processor;
use crate::progress_bar::ProgressCounter;
use crate::sql_params;
use crate::stats::ProcessorCallback;
use async_trait::async_trait;
use log::{debug, error};
use rusqlite::ToSql;
use solana_accounts_db::accounts_index::{ScanConfig, ScanOrder};
use solana_program::pubkey::Pubkey;
use solana_runtime::bank::Bank;
use solana_sdk::account::{AccountSharedData, ReadableAccount};
use std::future::Future;
use std::string::ToString;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

pub const META_ACCOUNT_TABLE: &str = "account";
pub const INSERT_META_ACCOUNT_QUERY: &str = "INSERT OR REPLACE INTO account (pubkey, data_len, owner, lamports, executable, rent_epoch) SELECT ?, ?, ?, ?, ?, ?;";

pub struct ProcessorAccountOwners {
    bank: Arc<Bank>,
    db_sender: Sender<DbMessage>,
    account_owners: Vec<Pubkey>,
    account_owners_counter: Arc<ProgressCounter>,
}

impl ProcessorAccountOwners {
    pub async fn new(
        bank: Arc<Bank>,
        db_sender: Sender<DbMessage>,
        filters: &Filters,
        account_owners_progress_counter: Arc<ProgressCounter>,
    ) -> anyhow::Result<Self> {
        let account_owners = filters.account_owners.clone();
        let processor = Self {
            bank,
            db_sender,
            account_owners_counter: account_owners_progress_counter,
            account_owners,
        };
        processor.create_table().await?;
        Ok(processor)
    }

    async fn create_table(&self) -> anyhow::Result<usize> {
        let (response_tx, response_rx) = oneshot::channel();
        self.db_sender
            .send(DbMessage::ExecuteSpecial {
                query: "CREATE TABLE account  (
                    pubkey TEXT NOT NULL PRIMARY KEY,
                    data_len INTEGER(8) NOT NULL,
                    owner TEXT NOT NULL,
                    lamports INTEGER(8) NOT NULL,
                    executable INTEGER(1) NOT NULL,
                    rent_epoch INTEGER(8) NOT NULL
                );"
                .to_string(),
                params: vec![],
                response: response_tx,
            })
            .await?;
        response_rx.await?
    }

    pub async fn process(&mut self) -> anyhow::Result<()> {
        for pubkey in self.account_owners.clone() {
            debug!("Loading program {} account_owners from bank...", pubkey);
            let transaction_accounts = self.bank.get_program_accounts(
                &pubkey,
                &ScanConfig {
                    scan_order: ScanOrder::Unsorted,
                    ..ScanConfig::default()
                },
            )?;
            debug!(
                "Loaded program {} {} account_owners",
                pubkey,
                transaction_accounts.len()
            );
            for (pubkey, account) in transaction_accounts {
                insert_account_meta(
                    &self.db_sender,
                    &self.account_owners_counter,
                    &pubkey,
                    &account,
                )
                .await
                .unwrap_or_else(|e| {
                    error!("Failed to insert account {}: {:?}", pubkey, e);
                    0
                });
            }
        }
        Ok(())
    }
}

impl Processor for ProcessorAccountOwners {
    fn name() -> &'static str {
        "Account owners"
    }
    fn process(&mut self) -> impl Future<Output = anyhow::Result<()>> + Send {
        self.process()
    }
}

#[async_trait]
impl ProcessorCallback for ProcessorAccountOwners {
    async fn get_count(&self) -> (String, u64) {
        (
            META_ACCOUNT_TABLE.to_string(),
            self.account_owners_counter.get(),
        )
    }
}

pub async fn insert_account_meta(
    db_sender: &Sender<DbMessage>,
    progress_counter: &Arc<ProgressCounter>,
    pubkey: &Pubkey,
    account: &AccountSharedData,
) -> anyhow::Result<usize> {
    let (response_tx, response_rx) = oneshot::channel();
    let owned_params = sql_params![
        pubkey.to_string(),
        account.data().len() as i64,
        account.owner().to_string(),
        account.lamports() as i64,
        account.executable(),
        account.rent_epoch() as i64
    ];
    db_sender
        .send(DbMessage::Execute {
            query: INSERT_META_ACCOUNT_QUERY.to_string(),
            params: owned_params,
            response: response_tx,
        })
        .await?;
    progress_counter.inc();
    response_rx.await?
}
