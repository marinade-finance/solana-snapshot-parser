use crate::jito_mev::JITO_PROGRAM;
use crate::jito_priority_fee::JITO_PRIORITY_FEE_DISTRIBUTION_PROGRAM;
use {
    log::info, snapshot_parser::account_scan::scan_accounts_by_owner,
    solana_program::pubkey::Pubkey, solana_runtime::bank::Bank,
    solana_sdk::account::AccountSharedData, std::collections::HashSet, std::sync::Arc,
};

pub struct ScannedAccounts {
    pub stake: Arc<Vec<(Pubkey, AccountSharedData)>>,
    pub tip_distribution: Arc<Vec<(Pubkey, AccountSharedData)>>,
    pub priority_fee_distribution: Arc<Vec<(Pubkey, AccountSharedData)>>,
}

pub fn scan_required_accounts(bank: &Arc<Bank>, verify: bool) -> anyhow::Result<ScannedAccounts> {
    let stake_program = solana_stake_interface::program::ID;
    let tip_distribution_program: Pubkey = JITO_PROGRAM.try_into()?;
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

// Costs one extra full accounts-index scan per owner, which is what the single pass exists to avoid
fn verify_matches_program_accounts(
    bank: &Arc<Bank>,
    owner: &Pubkey,
    scanned: &[(Pubkey, AccountSharedData)],
) -> anyhow::Result<()> {
    let expected = bank.get_program_accounts(owner)?;
    anyhow::ensure!(
        scanned.len() == expected.len(),
        "Account scan mismatch for owner {owner}: single pass produced {} accounts, get_program_accounts produced {}",
        scanned.len(),
        expected.len()
    );

    let scanned_pubkeys: HashSet<Pubkey> = scanned.iter().map(|(pubkey, _)| *pubkey).collect();
    let expected_pubkeys: HashSet<Pubkey> = expected.iter().map(|(pubkey, _)| *pubkey).collect();
    anyhow::ensure!(
        scanned_pubkeys == expected_pubkeys,
        "Account scan pubkey set mismatch for owner {owner}: {} only in single pass, {} only in get_program_accounts",
        scanned_pubkeys.difference(&expected_pubkeys).count(),
        expected_pubkeys.difference(&scanned_pubkeys).count()
    );

    info!(
        "Account scan verified against get_program_accounts for owner {}: {} accounts",
        owner,
        scanned.len()
    );
    Ok(())
}
