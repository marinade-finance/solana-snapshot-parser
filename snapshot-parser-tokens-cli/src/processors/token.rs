use crate::db_message::{DbMessage, OwnedSqlValue};
use crate::filters::Filters;
use crate::processors::{insert_account_meta, Processor};
use crate::progress_bar::ProgressCounter;
use crate::sql_params;
use crate::stats::ProcessorCallback;
use async_trait::async_trait;
use log::{debug, error};
use rusqlite::ToSql;
use solana_accounts_db::accounts_index::{ScanConfig, ScanOrder};
use solana_program::program_error::ProgramError;
use solana_program::program_pack::Pack;
use solana_program::pubkey::Pubkey;
use solana_runtime::bank::Bank;
use solana_sdk::account::ReadableAccount;
use std::future::Future;
use std::string::ToString;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

pub const TOKEN_ACCOUNT_TABLE: &str = "token_account";
pub const INSERT_TOKEN_ACCOUNT_QUERY: &str = "INSERT OR REPLACE INTO token_account (pubkey, mint, owner, amount, delegate, state, is_native, delegated_amount, close_authority) SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?;";

pub struct ProcessorToken {
    bank: Arc<Bank>,
    db_sender: Sender<DbMessage>,
    mints: Vec<Pubkey>,
    account_owners_counter: Arc<ProgressCounter>,
    token_counter: Arc<ProgressCounter>,
}

impl ProcessorToken {
    pub async fn new(
        bank: Arc<Bank>,
        db_sender: Sender<DbMessage>,
        filters: &Filters,
        account_owners_progress_counter: Arc<ProgressCounter>,
        token_progress_counter: Arc<ProgressCounter>,
    ) -> anyhow::Result<Self> {
        let mints = filters.account_mints.clone();
        let processor = Self {
            bank,
            db_sender,
            account_owners_counter: account_owners_progress_counter,
            token_counter: token_progress_counter,
            mints,
        };
        processor.create_token_table().await?;
        Ok(processor)
    }

    async fn create_token_table(&self) -> anyhow::Result<usize> {
        let (response_tx, response_rx) = oneshot::channel();
        self.db_sender
            .send(DbMessage::ExecuteSpecial {
                query: "CREATE TABLE token_account (
                    pubkey TEXT NOT NULL PRIMARY KEY,
                    mint TEXT NOT NULL,
                    owner TEXT NOT NULL,
                    amount INTEGER(8) NOT NULL,
                    delegate TEXT,
                    state INTEGER(1) NOT NULL,
                    is_native INTEGER(8),
                    delegated_amount INTEGER(8) NOT NULL,
                    close_authority TEXT
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
            "Loading token accounts for {} mints from bank...",
            self.mints.len()
        );
        let token_accounts = self.bank.get_filtered_program_accounts(
            &spl_token::ID,
            |account_data| match account_data.data().len() {
                spl_token::state::Account::LEN => {
                    match spl_token::state::Account::unpack(account_data.data()) {
                        Ok(token) => self.mints.contains(&token.mint),
                        Err(ProgramError::UninitializedAccount) => false,
                        Err(e) => {
                            debug!("Error: failed to unpack token account: {:?}", e);
                            false
                        }
                    }
                }
                _ => false,
            },
            &ScanConfig {
                scan_order: ScanOrder::Unsorted,
                ..ScanConfig::default()
            },
        )?;

        debug!("Token processor loaded {} accounts", token_accounts.len());
        for (pubkey, account) in token_accounts {
            let token_account = spl_token::state::Account::unpack(account.data())?;
            insert_account_meta(
                &self.db_sender,
                &self.account_owners_counter,
                &pubkey,
                &account,
            )
            .await?;
            insert_token(
                &self.db_sender,
                &self.token_counter,
                &pubkey,
                &token_account,
            )
            .await
            .unwrap_or_else(|e| {
                error!("Failed to insert token account {}: {:?}", pubkey, e);
                0
            });
        }
        Ok(())
    }
}

impl Processor for ProcessorToken {
    fn name() -> &'static str {
        "Token"
    }
    fn process(&mut self) -> impl Future<Output = anyhow::Result<()>> + Send {
        self.process()
    }
}

#[async_trait]
impl ProcessorCallback for ProcessorToken {
    async fn get_count(&self) -> (String, u64) {
        (TOKEN_ACCOUNT_TABLE.to_string(), self.token_counter.get())
    }
}

pub async fn insert_token(
    db_sender: &Sender<DbMessage>,
    progress_counter: &Arc<ProgressCounter>,
    pubkey: &Pubkey,
    token_account: &spl_token::state::Account,
) -> anyhow::Result<usize> {
    let (response_tx, response_rx) = oneshot::channel();
    let owned_params = sql_params![
        pubkey.to_string(),
        token_account.mint.to_string(),
        token_account.owner.to_string(),
        token_account.amount as i64,
        token_account
            .delegate
            .map_or(None, |key| Some(key.to_string())),
        token_account.state as u8,
        Option::<u64>::from(token_account.is_native),
        token_account.delegated_amount as i64,
        token_account
            .close_authority
            .map_or(None, |key| Some(bs58::encode(key.as_ref()).into_string())),
    ];
    db_sender
        .send(DbMessage::Execute {
            query: INSERT_TOKEN_ACCOUNT_QUERY.to_string(),
            params: owned_params,
            response: response_tx,
        })
        .await?;
    progress_counter.inc();
    response_rx.await?
}
