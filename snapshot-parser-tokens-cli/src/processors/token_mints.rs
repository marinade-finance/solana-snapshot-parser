use crate::db_message::{DbMessage, OwnedSqlParams, OwnedSqlValue};
use crate::db_writer::DbWriter;
use crate::filters::Filters;
use crate::processors::Processor;
use crate::progress_bar::ProgressCounter;
use crate::sql_params;
use log::info;
use rusqlite::ToSql;
use solana_program::program_pack::Pack;
use solana_program::pubkey::Pubkey;
use solana_runtime::bank::Bank;
use solana_sdk::account::ReadableAccount;
use std::future::Future;
use std::string::ToString;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

pub const INSERT_MINT_QUERY: &str = "INSERT OR REPLACE INTO token_mint (pubkey, mint_authority, supply, decimals, is_initialized, freeze_authority) SELECT ?, ?, ?, ?, ?, ?;";

pub struct ProcessorMint {
    bank: Arc<Bank>,
    db_sender: Sender<DbMessage>,
    db_writer: DbWriter,
    mints: Vec<Pubkey>,
}

impl ProcessorMint {
    pub async fn new(
        bank: Arc<Bank>,
        db_sender: Sender<DbMessage>,
        filters: &Filters,
        token_progress_counter: Arc<ProgressCounter>,
    ) -> anyhow::Result<Self> {
        let mints = filters.account_mints.clone();
        let processor = Self {
            bank,
            db_writer: DbWriter::new(db_sender.clone(), INSERT_MINT_QUERY, token_progress_counter),
            db_sender,
            mints,
        };
        processor.create_mint_table().await?;
        Ok(processor)
    }

    async fn create_mint_table(&self) -> anyhow::Result<usize> {
        let (response_tx, response_rx) = oneshot::channel();
        self.db_sender
            .send(DbMessage::ExecuteSpecial {
                query: "CREATE TABLE token_mint (
                    pubkey TEXT NOT NULL PRIMARY KEY,
                    mint_authority TEXT NULL,
                    supply INTEGER(8) NOT NULL,
                    decimals INTEGER(2) NOT NULL,
                    is_initialized BOOL NOT NULL,
                    freeze_authority TEXT NULL
                );"
                .to_string(),
                params: vec![],
                response: response_tx,
            })
            .await?;
        response_rx.await?
    }

    pub async fn process(&mut self) -> anyhow::Result<()> {
        info!("Loading {} mint accounts...", self.mints.len());
        for mint_pubkey in self.mints.iter() {
            let account = self
                .bank
                .get_account(mint_pubkey)
                .ok_or_else(|| anyhow::anyhow!("Mint account not found: {}", mint_pubkey))?;
            let mint = spl_token::state::Mint::unpack(account.data())
                .map_err(|e| anyhow::anyhow!("Failed to unpack mint {}: {:?}", mint_pubkey, e))?;
            self.db_writer.push(mint_row(mint_pubkey, &mint)).await?;
        }
        self.db_writer.flush().await
    }
}

impl Processor for ProcessorMint {
    fn name() -> &'static str {
        "Mint"
    }
    fn process(&mut self) -> impl Future<Output = anyhow::Result<()>> + Send {
        self.process()
    }
}

pub fn mint_row(pubkey: &Pubkey, token_mint: &spl_token::state::Mint) -> OwnedSqlParams {
    sql_params![
        pubkey.to_string(),
        token_mint
            .mint_authority
            .map_or(None, |key| Some(key.to_string())),
        token_mint.supply as i64,
        token_mint.decimals,
        token_mint.is_initialized,
        token_mint
            .freeze_authority
            .map_or(None, |key| Some(key.to_string())),
    ]
}
