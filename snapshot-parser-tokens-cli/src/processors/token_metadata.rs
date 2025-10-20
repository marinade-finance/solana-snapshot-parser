use crate::db_message::{DbMessage, OwnedSqlValue};
use crate::processors::Processor;
use crate::progress_bar::ProgressCounter;
use crate::sql_params;
use crate::stats::ProcessorCallback;
use async_trait::async_trait;
use log::{debug, error};
use mpl_token_metadata::accounts::Metadata;
use rusqlite::ToSql;
use solana_accounts_db::accounts_index::{ScanConfig, ScanOrder};
use solana_program::pubkey::Pubkey;
use solana_runtime::bank::Bank;
use solana_sdk::account::ReadableAccount;
use std::future::Future;
use std::io::ErrorKind;
use std::string::ToString;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

pub const TOKEN_METADATA_ACCOUNT_TABLE: &str = "token_metadata";
pub const INSERT_TOKEN_METADATA_ACCOUNT_QUERY: &str = "INSERT OR REPLACE INTO token_metadata (pubkey, mint, update_authority, name, symbol, uri, data_length, seller_fee_basis_points, primary_sale_happened, is_mutable, edition_nonce, collection_verified, collection_key)\
SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?;";

pub struct ProcessorTokenMetadata {
    bank: Arc<Bank>,
    db_sender: Sender<DbMessage>,
    token_metadata_counter: Arc<ProgressCounter>,
}

impl ProcessorTokenMetadata {
    pub async fn new(
        bank: Arc<Bank>,
        db_sender: Sender<DbMessage>,
        token_metadata_counter: Arc<ProgressCounter>,
    ) -> anyhow::Result<Self> {
        let processor = Self {
            bank,
            db_sender,
            token_metadata_counter,
        };
        processor.create_token_table().await?;
        Ok(processor)
    }

    async fn create_token_table(&self) -> anyhow::Result<usize> {
        let (response_tx, response_rx) = oneshot::channel();
        self.db_sender
            .send(DbMessage::ExecuteSpecial {
                query: "CREATE TABLE token_metadata (
                    pubkey TEXT NOT NULL PRIMARY KEY,
                    mint TEXT NOT NULL,
                    update_authority TEXT NOT NULL,
                    name TEXT NOT NULL,
                    symbol TEXT(10) NOT NULL,
                    uri TEXT(200) NOT NULL,
                    data_length INTEGER(8) NOT NULL,
                    seller_fee_basis_points INTEGER(4) NOT NULL,
                    primary_sale_happened INTEGER(1) NOT NULL,
                    is_mutable INTEGER(1) NOT NULL,
                    edition_nonce INTEGER(2) NULL,
                    collection_verified INTEGER(1) NULL,
                    collection_key TEXT NULL
                );"
                .to_string(),
                params: vec![],
                response: response_tx,
            })
            .await?;
        response_rx.await?
    }

    pub async fn process(&mut self) -> anyhow::Result<()> {
        let metadata_id = Pubkey::from(mpl_token_metadata::ID.to_bytes());
        debug!(
            "Loading token metadata accounts for owner {} from bank...",
            metadata_id,
        );
        let token_metadata_accounts = self.bank.get_program_accounts(
            &metadata_id,
            &ScanConfig {
                scan_order: ScanOrder::Unsorted,
                ..ScanConfig::default()
            },
        )?;

        debug!(
            "Token metadata processor loaded {} accounts",
            token_metadata_accounts.len()
        );
        for (pubkey, account) in token_metadata_accounts {
            match Metadata::safe_deserialize(account.data()) {
                Ok(metadata) => {
                    insert_token_metadata(
                        &self.db_sender,
                        &self.token_metadata_counter,
                        &pubkey,
                        account.data().len(),
                        &metadata,
                    )
                    .await
                    .unwrap_or_else(|e| {
                        error!(
                            "Failed to insert token metadata account {}: {:?}",
                            pubkey, e
                        );
                        0
                    });
                }
                Err(e) => match e.kind() {
                    ErrorKind::Other => {
                        // Ignore; this is expected for non-MetadataV1 accounts
                    }
                    _ => {
                        debug!(
                            "Failed to deserialize token metadata account {}: {:?}",
                            pubkey, e
                        );
                    }
                },
            }
        }

        Ok(())
    }
}

impl Processor for ProcessorTokenMetadata {
    fn name() -> &'static str {
        "Token Metadata"
    }
    fn process(&mut self) -> impl Future<Output = anyhow::Result<()>> + Send {
        self.process()
    }
}

#[async_trait]
impl ProcessorCallback for ProcessorTokenMetadata {
    async fn get_count(&self) -> (String, u64) {
        (
            TOKEN_METADATA_ACCOUNT_TABLE.to_string(),
            self.token_metadata_counter.get(),
        )
    }
}

pub async fn insert_token_metadata(
    db_sender: &Sender<DbMessage>,
    progress_counter: &Arc<ProgressCounter>,
    pubkey: &Pubkey,
    account_data_len: usize,
    metadata: &Metadata,
) -> anyhow::Result<usize> {
    let (response_tx, response_rx) = oneshot::channel();
    let owned_params = sql_params![
        pubkey.to_string(),
        metadata.mint.to_string(),
        metadata.update_authority.to_string(),
        metadata.name.clone(),
        metadata.symbol.clone(),
        metadata.uri.clone(),
        account_data_len as u64,
        metadata.seller_fee_basis_points,
        metadata.primary_sale_happened,
        metadata.is_mutable,
        metadata.edition_nonce,
        metadata.collection.clone().map(|c| c.verified),
        format!("{:?}", metadata.key),
    ];
    db_sender
        .send(DbMessage::Execute {
            query: INSERT_TOKEN_METADATA_ACCOUNT_QUERY.to_string(),
            params: owned_params,
            response: response_tx,
        })
        .await?;
    progress_counter.inc();
    response_rx.await?
}
