use crate::jito_priority_fee::JITO_PRIORITY_FEE_DISTRIBUTION_PROGRAM;
use {
    snapshot_parser::account_scan::{scan_accounts_by_owner, verify_scan_matches},
    solana_program::pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_sdk::account::AccountSharedData,
    std::sync::Arc,
};

pub struct ScannedAccounts {
    pub stake: Arc<Vec<(Pubkey, AccountSharedData)>>,
    pub tip_distribution: Arc<Vec<(Pubkey, AccountSharedData)>>,
    pub priority_fee_distribution: Arc<Vec<(Pubkey, AccountSharedData)>>,
}

pub fn scan_required_accounts(
    bank: &Arc<Bank>,
    verify: bool,
    tip_distribution_program: Pubkey,
) -> anyhow::Result<ScannedAccounts> {
    let stake_program = solana_stake_interface::program::ID;
    let priority_fee_distribution_program: Pubkey =
        JITO_PRIORITY_FEE_DISTRIBUTION_PROGRAM.try_into()?;

    let mut scanned = scan_accounts_by_owner(
        bank,
        &[
            stake_program,
            tip_distribution_program,
            priority_fee_distribution_program,
        ],
    )?;
    let mut take = |owner: &Pubkey| Arc::new(scanned.remove(owner).unwrap_or_default());
    let scanned_accounts = ScannedAccounts {
        stake: take(&stake_program),
        tip_distribution: take(&tip_distribution_program),
        priority_fee_distribution: take(&priority_fee_distribution_program),
    };

    if verify {
        verify_matches_program_accounts(bank, &stake_program, &scanned_accounts.stake)?;
        verify_matches_program_accounts(
            bank,
            &tip_distribution_program,
            &scanned_accounts.tip_distribution,
        )?;
        verify_matches_program_accounts(
            bank,
            &priority_fee_distribution_program,
            &scanned_accounts.priority_fee_distribution,
        )?;
    }

    Ok(scanned_accounts)
}

fn verify_matches_program_accounts(
    bank: &Arc<Bank>,
    owner: &Pubkey,
    scanned: &[(Pubkey, AccountSharedData)],
) -> anyhow::Result<()> {
    verify_scan_matches(owner, scanned, &bank.get_program_accounts(owner)?)
}
