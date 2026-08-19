use crate::accounts::{Registrar, Voter};
use crate::db_message::{DbMessage, OwnedSqlValue};
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
const VOTER_ACCOUNT_LEN: usize = 2728;

pub fn marinade_vsr_program_id() -> anyhow::Result<Pubkey> {
    Pubkey::from_str(MARINADE_VSR_PROGRAM_ADDR).map_err(|e| {
        anyhow!(
            "Cannot pars VSR program address {}: {:?}",
            MARINADE_VSR_PROGRAM_ADDR,
            e
        )
    })
}

// Selects the VSR accounts the processor stores: a Voter account is recognised by its
// size alone, exactly as it was when this ran as the filter of
// get_filtered_program_accounts. Whether it deserialises is decided later, per account.
pub fn is_voter_account(account: &AccountSharedData) -> bool {
    matches!(account.data().len(), VOTER_ACCOUNT_LEN)
}

pub struct ProcessorVeMnde {
    voter_accounts: Arc<Vec<(Pubkey, AccountSharedData)>>,
    db_sender: Sender<DbMessage>,
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
                insert_vemnde(
                    &self.db_sender,
                    &self.vemnde_counter,
                    pubkey,
                    account.owner(),
                    &self.vsr_registrar,
                    &voter_account,
                    self.current_ts,
                )
                .await
                .unwrap_or_else(|e| {
                    error!("Error: failed to insert voter account {}: {:?}", pubkey, e);
                    0
                });
            } else {
                warn!("Error: failed to unpack voter account: {:?}", pubkey);
            }
        }

        Ok(())
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

pub async fn insert_vemnde(
    db_sender: &Sender<DbMessage>,
    progress_counter: &Arc<ProgressCounter>,
    pubkey: &Pubkey,
    owner: &Pubkey,
    registrar: &Registrar,
    voter: &Voter,
    current_ts: i64,
) -> anyhow::Result<usize> {
    let (response_tx, response_rx) = oneshot::channel();

    let voting_power = voter
        .deposits
        .iter()
        .filter(|d| d.is_used)
        .try_fold(0u64, |sum, d| {
            d.voting_power(
                &registrar.voting_mints[d.voting_mint_config_idx as usize],
                current_ts,
            )
            .map(|vp| sum.checked_add(vp).unwrap())
        })?;
    let owned_params = sql_params![
        pubkey.to_string(),
        voter.voter_authority.to_string(),
        voting_power.to_string(),
        owner.to_string(),
    ];
    db_sender
        .send(DbMessage::Execute {
            query: INSERT_VE_MNDE_ACCOUNT_QUERY.to_string(),
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

    // Size is the whole test: a Voter account that does not deserialize is still scanned,
    // and only warned about later, exactly as before.
    #[test]
    fn voter_accounts_are_recognised_by_size_alone() {
        assert!(is_voter_account(&account_of(VOTER_ACCOUNT_LEN)));
        assert!(!is_voter_account(&account_of(VOTER_ACCOUNT_LEN - 1)));
        assert!(!is_voter_account(&account_of(VOTER_ACCOUNT_LEN + 1)));
        assert!(!is_voter_account(&account_of(0)));
    }
}
