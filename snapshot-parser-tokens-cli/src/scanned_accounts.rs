use crate::filters::Filters;
use crate::processors::{
    is_token_account_of_mints, is_voter_account, marinade_vsr_program_id, spl_token_program_id,
};
use snapshot_parser::account_scan::{scan_accounts_by_owner_filtered, OwnerFilter};
use solana_program::pubkey::Pubkey;
use solana_runtime::bank::Bank;
use solana_sdk::account::AccountSharedData;
use std::sync::Arc;

pub struct ScannedAccounts {
    pub token: Arc<Vec<(Pubkey, AccountSharedData)>>,
    pub voter: Arc<Vec<(Pubkey, AccountSharedData)>>,
    pub stake: Arc<Vec<(Pubkey, AccountSharedData)>>,
}

pub fn scan_required_accounts(
    bank: &Arc<Bank>,
    filters: &Filters,
) -> anyhow::Result<ScannedAccounts> {
    let token_program = spl_token_program_id();
    let vsr_program = marinade_vsr_program_id()?;
    let stake_program = solana_stake_interface::program::ID;

    let mut scanned = scan_accounts_by_owner_filtered(
        bank,
        &[
            OwnerFilter::matching(token_program, |_pubkey, account| {
                is_token_account_of_mints(&filters.account_mints, account)
            }),
            OwnerFilter::matching(vsr_program, |_pubkey, account| is_voter_account(account)),
            OwnerFilter::all(stake_program),
        ],
    )?;

    let mut take = |owner: &Pubkey| Arc::new(scanned.remove(owner).unwrap_or_default());
    Ok(ScannedAccounts {
        token: take(&token_program),
        voter: take(&vsr_program),
        stake: take(&stake_program),
    })
}
