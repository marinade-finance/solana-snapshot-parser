use crate::utils::jito_parser::{get_epoch_created_at, read_jito_commission_and_epoch};
use crate::utils::SliceAt;
use solana_accounts_db::accounts_index::ScanConfig;
use solana_program::pubkey::Pubkey;
use solana_sdk::account::Account;
use {log::info, solana_program::stake_history::Epoch, solana_runtime::bank::Bank, std::sync::Arc};

pub struct JitoPriorityFeeMeta {
    pub validator_vote_account: Pubkey,
    pub validator_commission_bps: u16,
    pub total_lamports_transferred: u64,
}

// Jito PriorityFeeDistributionAccount used to distribute priority fees to stakers
// https://github.com/jito-foundation/jito-programs/blob/8f55af0a9b31ac2192415b59ce2c47329ee255a2/mev-programs/programs/priority-fee-distribution/src/state.rs#L35
// The account has got similar structure to TipDistributionAccount.
// A new account is created for each epoch for every validator.
// * https://www.jito.network/blog/tiprouter-upgrade-facilitating-priority-fees/
// * https://www.notion.so/marinade/Account-for-Jito-Tip-Distribution-Collect-1-4-22ae465715a480daa33ae55d5b92ba52
const JITO_PRIORITY_FEE_DISTRIBUTION_PROGRAM: &str = "Priority6weCZ5HwDn29NxLFpb7TDp2iLZ6XKc5e8d3";
const PRIORITY_FEE_DISTRIBUTION_ACCOUNT_DISCRIMINATOR: [u8; 8] =
    [163, 183, 254, 12, 121, 137, 235, 27];
const TOTAL_LAMPORTS_TRASFERRED_BYTE_OFFSET: usize = 8 + 2 + 8; // epoch + commission + expires_at

pub fn fetch_jito_priority_fee_metas(
    bank: &Arc<Bank>,
    epoch: Epoch,
) -> anyhow::Result<Vec<JitoPriorityFeeMeta>> {
    let jito_program: Pubkey = JITO_PRIORITY_FEE_DISTRIBUTION_PROGRAM.try_into()?;
    let jito_accounts_raw = bank.get_program_accounts(
        &jito_program,
        &ScanConfig {
            collect_all_unsorted: true,
            ..ScanConfig::default()
        },
    )?;
    info!(
        "jito priority fee distribution program {} `raw` processors loaded: {}",
        JITO_PRIORITY_FEE_DISTRIBUTION_PROGRAM,
        jito_accounts_raw.len()
    );

    let mut jito_priority_fee_metas: Vec<JitoPriorityFeeMeta> = Vec::new();

    for (pubkey, shared_account) in jito_accounts_raw {
        let account = Account::from(shared_account);
        if account.data[0..8] == PRIORITY_FEE_DISTRIBUTION_ACCOUNT_DISCRIMINATOR {
            update_jito_priority_fee_metas(&mut jito_priority_fee_metas, &account, pubkey, epoch)?;
        }
    }

    if jito_priority_fee_metas.is_empty() {
        return Err(anyhow::anyhow!(
            "Not expected. No Jito Priority Fee data found. Evaluate the snapshot data."
        ));
    }

    info!(
        "jito priority fee distribution processors for epoch {}: {}",
        epoch,
        jito_priority_fee_metas.len()
    );
    Ok(jito_priority_fee_metas)
}

fn update_jito_priority_fee_metas(
    jito_priority_fee_metas: &mut Vec<JitoPriorityFeeMeta>,
    account: &Account,
    pubkey: Pubkey,
    epoch: Epoch,
) -> anyhow::Result<()> {
    let (epoch_created_at, epoch_byte_index) = get_epoch_created_at(account)?;
    if epoch_created_at == epoch {
        let commission_data = read_jito_commission_and_epoch(pubkey, account, epoch_byte_index)?;
        assert_eq!(epoch, commission_data.epoch_created_at);
        let total_lamports_transferred =
            read_priority_fee_total_lamports_transferred(pubkey, account, epoch_byte_index)?;
        jito_priority_fee_metas.push(JitoPriorityFeeMeta {
            validator_vote_account: commission_data.validator_vote_account,
            validator_commission_bps: commission_data.validator_commission_bps,
            total_lamports_transferred,
        });
    }
    Ok(())
}

fn read_priority_fee_total_lamports_transferred(
    account_pubkey: Pubkey,
    account: &Account,
    end_merkle_root_byte_index: usize, // a byte index directly after MerkleRoot struct
) -> anyhow::Result<u64> {
    let total_lamports_transferred_byte_index =
        end_merkle_root_byte_index + TOTAL_LAMPORTS_TRASFERRED_BYTE_OFFSET;
    let total_lamports_transferred = u64::from_le_bytes(
        account
            .data
            .slice_at(total_lamports_transferred_byte_index, 8)?
            .try_into()
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to parse total_lamports_transferred for account {}: {:?}",
                    account_pubkey,
                    e
                )
            })?,
    );

    Ok(total_lamports_transferred)
}
