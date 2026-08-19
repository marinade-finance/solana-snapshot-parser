use crate::db_message::{DbMessage, OwnedSqlValue};
use crate::processors::Processor;
use crate::progress_bar::ProgressCounter;
use crate::sql_params;
use crate::stats::ProcessorCallback;
use async_trait::async_trait;
use log::{debug, error};
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

// spl-token re-exports an older solana-pubkey major than the bank API expects,
// so bridge spl_token::ID through its raw bytes.
pub fn spl_token_program_id() -> Pubkey {
    Pubkey::from(spl_token::ID.to_bytes())
}

// Selects the token accounts the processor stores: an initialised SPL-token account of
// one of the wanted mints. An account that fails to unpack is silently skipped, as it
// was when this ran as the filter of get_filtered_program_accounts.
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
            insert_token(&self.db_sender, &self.token_counter, pubkey, &token_account)
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

    // These are the accounts the filter used to drop while scanning the whole SPL-token
    // population: unpack failures are swallowed, not reported.
    #[test]
    fn accounts_that_do_not_unpack_are_dropped_silently() {
        let mints = [Pubkey::new_unique()];
        let keep = |data: Vec<u8>| is_token_account_of_mints(&mints, &account_of(data));

        // an uninitialized account of the right size: unpack answers UninitializedAccount
        assert!(!keep(vec![0u8; spl_token::state::Account::LEN]));
        // a mint account, and anything else that is not token-account sized
        assert!(!keep(vec![0u8; spl_token::state::Mint::LEN]));
        assert!(!keep(vec![]));
        // the right size, but the state byte is not a state: unpack answers InvalidAccountData
        let mut invalid =
            token_account_data(&mints[0], spl_token::state::AccountState::Initialized);
        invalid[108] = 9;
        assert!(!keep(invalid));
    }
}
