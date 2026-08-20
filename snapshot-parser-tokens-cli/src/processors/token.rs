use crate::db_message::{DbMessage, OwnedSqlParams, OwnedSqlValue};
use crate::db_writer::DbWriter;
use crate::processors::Processor;
use crate::progress_bar::ProgressCounter;
use crate::sql_params;
use crate::stats::ProcessorCallback;
use async_trait::async_trait;
use log::debug;
use rusqlite::ToSql;
use solana_program::program_error::ProgramError;
use solana_program::program_pack::Pack;
use solana_program::pubkey::Pubkey;
use solana_sdk::account::{AccountSharedData, ReadableAccount};
use std::future::Future;
use std::string::ToString;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

pub const TOKEN_ACCOUNT_TABLE: &str = "token_account";
pub const INSERT_TOKEN_ACCOUNT_QUERY: &str = "INSERT OR REPLACE INTO token_account (pubkey, mint, owner, amount, delegate, state, is_native, delegated_amount, close_authority) SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?;";

pub fn spl_token_program_id() -> Pubkey {
    Pubkey::from(spl_token::ID.to_bytes())
}

pub fn is_token_account_of_mints(mints: &[Pubkey], account: &AccountSharedData) -> bool {
    match account.data().len() {
        spl_token::state::Account::LEN => match spl_token::state::Account::unpack(account.data()) {
            Ok(token) => mints.contains(&token.mint),
            Err(ProgramError::UninitializedAccount) => false,
            Err(e) => {
                debug!("Error: failed to unpack token account: {:?}", e);
                false
            }
        },
        _ => false,
    }
}

pub struct ProcessorToken {
    token_accounts: Arc<Vec<(Pubkey, AccountSharedData)>>,
    db_sender: Sender<DbMessage>,
    db_writer: DbWriter,
    token_counter: Arc<ProgressCounter>,
}

impl ProcessorToken {
    pub async fn new(
        token_accounts: Arc<Vec<(Pubkey, AccountSharedData)>>,
        db_sender: Sender<DbMessage>,
        token_progress_counter: Arc<ProgressCounter>,
    ) -> anyhow::Result<Self> {
        let processor = Self {
            token_accounts,
            db_writer: DbWriter::new(
                db_sender.clone(),
                INSERT_TOKEN_ACCOUNT_QUERY,
                token_progress_counter.clone(),
            ),
            db_sender,
            token_counter: token_progress_counter,
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
            "Token processor got {} accounts from the scan",
            self.token_accounts.len()
        );
        for (pubkey, account) in self.token_accounts.iter() {
            let token_account = spl_token::state::Account::unpack(account.data())?;
            self.db_writer
                .push(token_account_row(pubkey, &token_account))
                .await?;
        }
        self.db_writer.flush().await
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

pub fn token_account_row(
    pubkey: &Pubkey,
    token_account: &spl_token::state::Account,
) -> OwnedSqlParams {
    sql_params![
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
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_program::program_option::COption;
    use solana_sdk::account::Account;

    fn token_account_data(mint: &Pubkey, state: spl_token::state::AccountState) -> Vec<u8> {
        let mut data = vec![0u8; spl_token::state::Account::LEN];
        spl_token::state::Account::pack(
            spl_token::state::Account {
                mint: *mint,
                owner: Pubkey::new_unique(),
                amount: 42,
                delegate: COption::None,
                state,
                is_native: COption::None,
                delegated_amount: 0,
                close_authority: COption::None,
            },
            &mut data,
        )
        .unwrap();
        data
    }

    fn account_of(data: Vec<u8>) -> AccountSharedData {
        AccountSharedData::from(Account {
            lamports: 1,
            data,
            owner: spl_token_program_id(),
            executable: false,
            rent_epoch: 0,
        })
    }

    #[test]
    fn only_initialized_token_accounts_of_a_wanted_mint_are_kept() {
        let wanted = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        let mints = [wanted];
        let keep = |data: Vec<u8>| is_token_account_of_mints(&mints, &account_of(data));

        assert!(keep(token_account_data(
            &wanted,
            spl_token::state::AccountState::Initialized
        )));
        assert!(keep(token_account_data(
            &wanted,
            spl_token::state::AccountState::Frozen
        )));
        assert!(!keep(token_account_data(
            &other,
            spl_token::state::AccountState::Initialized
        )));
    }

    #[test]
    fn accounts_that_do_not_unpack_are_dropped_silently() {
        let mints = [Pubkey::new_unique()];
        let keep = |data: Vec<u8>| is_token_account_of_mints(&mints, &account_of(data));

        assert!(!keep(vec![0u8; spl_token::state::Account::LEN]));
        assert!(!keep(vec![0u8; spl_token::state::Mint::LEN]));
        assert!(!keep(vec![]));
        let mut invalid =
            token_account_data(&mints[0], spl_token::state::AccountState::Initialized);
        invalid[108] = 9;
        assert!(!keep(invalid));
    }

    #[tokio::test]
    async fn process_ships_every_account_of_a_run_shorter_than_one_batch() {
        use crate::db_message::BatchOutcome;
        use indicatif::MultiProgress;
        use tokio::sync::mpsc;

        let mint = Pubkey::new_unique();
        let accounts: Vec<(Pubkey, AccountSharedData)> = (0..3)
            .map(|_| {
                (
                    Pubkey::new_unique(),
                    account_of(token_account_data(
                        &mint,
                        spl_token::state::AccountState::Initialized,
                    )),
                )
            })
            .collect();
        let expected = accounts.len();

        let (sender, mut receiver) = mpsc::channel(4);
        let collector = tokio::spawn(async move {
            let mut rows_received = 0usize;
            while let Some(msg) = receiver.recv().await {
                match msg {
                    DbMessage::Execute { rows, response, .. } => {
                        rows_received += rows.len();
                        let _ = response.send(BatchOutcome {
                            rows_written: rows.len(),
                            rows_failed: 0,
                        });
                    }
                    _ => panic!("unexpected message"),
                }
            }
            rows_received
        });

        let counter = Arc::new(ProgressCounter::new(&MultiProgress::new(), "test"));
        let mut processor = ProcessorToken {
            token_accounts: Arc::new(accounts),
            db_writer: DbWriter::new(sender.clone(), INSERT_TOKEN_ACCOUNT_QUERY, counter.clone()),
            db_sender: sender,
            token_counter: counter.clone(),
        };

        processor.process().await.unwrap();
        drop(processor);

        assert_eq!(collector.await.unwrap(), expected);
        assert_eq!(counter.get(), expected as u64);
    }
}
