use crate::accounts::{Registrar, Voter};
use crate::db_message::{DbMessage, OwnedSqlParams, OwnedSqlValue};
use crate::db_writer::DbWriter;
use crate::filters::Filters;
use crate::processors::Processor;
use crate::progress_bar::ProgressCounter;
use crate::sql_params;
use crate::stats::ProcessorCallback;
use anyhow::anyhow;
use async_trait::async_trait;
use borsh::BorshDeserialize;
use log::{debug, error, warn};
use rusqlite::ToSql;
use solana_program::pubkey::Pubkey;
use solana_sdk::account::{AccountSharedData, ReadableAccount};
use std::future::Future;
use std::str::FromStr;
use std::string::ToString;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

pub const VE_MNDE_ACCOUNT_TABLE: &str = "vemnde_accounts";
pub const INSERT_VE_MNDE_ACCOUNT_QUERY: &str = "INSERT OR REPLACE INTO vemnde_accounts (pubkey, voter_authority, voting_power, owner) SELECT ?, ?, ?, ?;";
const MARINADE_VSR_PROGRAM_ADDR: &str = "VoteMBhDCqGLRgYpp9o7DGyq81KNmwjXQRAHStjtJsS";
pub const VOTER_ACCOUNT_LEN: usize = 2728;

pub fn marinade_vsr_program_id() -> anyhow::Result<Pubkey> {
    Pubkey::from_str(MARINADE_VSR_PROGRAM_ADDR).map_err(|e| {
        anyhow!(
            "Cannot pars VSR program address {}: {:?}",
            MARINADE_VSR_PROGRAM_ADDR,
            e
        )
    })
}

pub fn is_voter_account(account: &AccountSharedData) -> bool {
    matches!(account.data().len(), VOTER_ACCOUNT_LEN)
}

pub struct ProcessorVeMnde {
    voter_accounts: Arc<Vec<(Pubkey, AccountSharedData)>>,
    db_sender: Sender<DbMessage>,
    db_writer: DbWriter,
    vsr_registrar: Registrar,
    vemnde_counter: Arc<ProgressCounter>,
    current_ts: i64,
}

impl ProcessorVeMnde {
    pub async fn new(
        voter_accounts: Arc<Vec<(Pubkey, AccountSharedData)>>,
        db_sender: Sender<DbMessage>,
        filters: &Filters,
        vemnde_progress_counter: Arc<ProgressCounter>,
        current_ts: i64,
    ) -> anyhow::Result<Self> {
        let vsr_registrar_vec = filters.vsr_registrar_data.clone();
        let vsr_registrar_data: &mut &[u8] = &mut vsr_registrar_vec.as_slice();
        let vsr_registrar: Registrar = Registrar::deserialize(vsr_registrar_data)?;
        let processor = Self {
            voter_accounts,
            db_writer: DbWriter::new(
                db_sender.clone(),
                INSERT_VE_MNDE_ACCOUNT_QUERY,
                vemnde_progress_counter.clone(),
            ),
            db_sender,
            vemnde_counter: vemnde_progress_counter,
            vsr_registrar,
            current_ts,
        };
        processor.create_native_staking_table().await?;
        Ok(processor)
    }

    async fn create_native_staking_table(&self) -> anyhow::Result<usize> {
        let (response_tx, response_rx) = oneshot::channel();
        self.db_sender
            .send(DbMessage::ExecuteSpecial {
                query: "CREATE TABLE vemnde_accounts (
                    pubkey TEXT NOT NULL PRIMARY KEY,
                    voter_authority TEXT NOT NULL,
                    voting_power TEXT NOT NULL,
                    owner TEXT NOT NULL
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
            "VeMMNDE processor got {} Voter accounts from the scan",
            self.voter_accounts.len()
        );
        for (pubkey, account) in self.voter_accounts.iter() {
            if let Ok(voter_account) = Voter::deserialize(&mut account.data()) {
                match vemnde_row(
                    pubkey,
                    account.owner(),
                    &self.vsr_registrar,
                    &voter_account,
                    self.current_ts,
                ) {
                    Ok(row) => self.db_writer.push(row).await?,
                    Err(e) => error!(
                        "Error: skipping voter account {} whose voting power does not add up: {:?}",
                        pubkey, e
                    ),
                }
            } else {
                warn!("Error: failed to unpack voter account: {:?}", pubkey);
            }
        }

        self.db_writer.flush().await
    }
}

impl Processor for ProcessorVeMnde {
    fn name() -> &'static str {
        "VeMnde"
    }
    fn process(&mut self) -> impl Future<Output = anyhow::Result<()>> + Send {
        self.process()
    }
}

#[async_trait]
impl ProcessorCallback for ProcessorVeMnde {
    async fn get_count(&self) -> (String, u64) {
        (VE_MNDE_ACCOUNT_TABLE.to_string(), self.vemnde_counter.get())
    }
}

pub fn vemnde_row(
    pubkey: &Pubkey,
    owner: &Pubkey,
    registrar: &Registrar,
    voter: &Voter,
    current_ts: i64,
) -> anyhow::Result<OwnedSqlParams> {
    let voting_power = voter
        .deposits
        .iter()
        .filter(|d| d.is_used)
        .try_fold(0u64, |sum, d| {
            // account data is untrusted: anyone can assign a 2728 byte account to the VSR program
            let voting_mint = registrar
                .voting_mints
                .get(d.voting_mint_config_idx as usize)
                .ok_or_else(|| {
                    anyhow!(
                        "deposit points at voting mint {} of {}",
                        d.voting_mint_config_idx,
                        registrar.voting_mints.len()
                    )
                })?;
            let vp = d.voting_power(voting_mint, current_ts)?;
            sum.checked_add(vp)
                .ok_or_else(|| anyhow!("voting power sum overflows u64"))
        })?;
    Ok(sql_params![
        pubkey.to_string(),
        voter.voter_authority.to_string(),
        voting_power.to_string(),
        owner.to_string(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::account::Account;

    fn account_of(data_len: usize) -> AccountSharedData {
        AccountSharedData::from(Account {
            lamports: 1,
            data: vec![0u8; data_len],
            owner: marinade_vsr_program_id().unwrap(),
            executable: false,
            rent_epoch: 0,
        })
    }

    #[test]
    fn voter_accounts_are_recognised_by_size_alone() {
        assert!(is_voter_account(&account_of(VOTER_ACCOUNT_LEN)));
        assert!(!is_voter_account(&account_of(VOTER_ACCOUNT_LEN - 1)));
        assert!(!is_voter_account(&account_of(VOTER_ACCOUNT_LEN + 1)));
        assert!(!is_voter_account(&account_of(0)));
    }

    fn zeroed_registrar() -> Registrar {
        Registrar::deserialize(&mut vec![0u8; 4096].as_slice()).unwrap()
    }

    fn voter_with_used_deposit(voting_mint_config_idx: u8) -> Voter {
        const DEPOSITS_OFFSET: usize = 8 + 32 + 32;
        const IS_USED_OFFSET: usize = 8 + 8 + 1 + 15 + 8 + 8;
        let mut data = vec![0u8; VOTER_ACCOUNT_LEN];
        data[DEPOSITS_OFFSET + IS_USED_OFFSET] = 1;
        data[DEPOSITS_OFFSET + IS_USED_OFFSET + 2] = voting_mint_config_idx;
        Voter::deserialize(&mut data.as_slice()).unwrap()
    }

    fn row_of(voter: &Voter) -> anyhow::Result<OwnedSqlParams> {
        vemnde_row(
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            &zeroed_registrar(),
            voter,
            0,
        )
    }

    #[test]
    fn a_deposit_pointing_past_the_registrar_mints_is_an_error_not_a_panic() {
        let Err(err) = row_of(&voter_with_used_deposit(4)) else {
            panic!("a deposit that indexes past the registrar must not be answered");
        };

        assert!(
            err.to_string().contains("voting mint 4 of 4"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_deposit_inside_the_registrar_mints_is_priced() {
        row_of(&voter_with_used_deposit(3)).unwrap();
    }
}
