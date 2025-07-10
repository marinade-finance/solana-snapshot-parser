use crate::utils::jito_parser::{
    get_epoch_created_at, read_jito_commission_and_epoch, JitoCommissionMeta,
};
use solana_accounts_db::accounts_index::ScanConfig;
use solana_program::pubkey::Pubkey;
use solana_sdk::account::Account;
use {log::info, solana_program::stake_history::Epoch, solana_runtime::bank::Bank, std::sync::Arc};

pub struct JitoMevMeta {
    pub vote_account: Pubkey,
    pub mev_commission: u16,
}

// https://github.com/jito-foundation/jito-programs/blob/v0.1.5/mev-programs/programs/tip-distribution/src/state.rs#L32
// only one TipDistribution account per epoch
// https://github.com/jito-foundation/jito-programs/blob/v0.1.5/mev-programs/programs/tip-distribution/src/lib.rs#L385
const JITO_PROGRAM: &str = "4R3gSG8BpU4t19KYj8CfnbtRpnT8gtk4dvTHxVRwc2r7";
const TIP_DISTRIBUTION_ACCOUNT_DISCRIMINATOR: [u8; 8] = [85, 64, 113, 198, 234, 94, 120, 123];

pub fn fetch_jito_mev_metas(bank: &Arc<Bank>, epoch: Epoch) -> anyhow::Result<Vec<JitoMevMeta>> {
    let jito_program: Pubkey = JITO_PROGRAM.try_into()?;
    let jito_accounts_raw = bank.get_program_accounts(
        &jito_program,
        &ScanConfig {
            collect_all_unsorted: true,
            ..ScanConfig::default()
        },
    )?;
    info!(
        "jito mev distribution program {} `raw` processors loaded: {}",
        JITO_PROGRAM,
        jito_accounts_raw.len()
    );

    let mut jito_mev_metas: Vec<JitoMevMeta> = Vec::new();

    for (pubkey, shared_account) in jito_accounts_raw {
        let account = Account::from(shared_account);
        if account.data[0..8] == TIP_DISTRIBUTION_ACCOUNT_DISCRIMINATOR {
            update_jito_mev_metas(&mut jito_mev_metas, &account, pubkey, epoch)?;
        }
    }

    if jito_mev_metas.is_empty() {
        return Err(anyhow::anyhow!(
            "Not expected. No Jito MEV commissions found. Evaluate the snapshot data."
        ));
    }

    info!(
        "jito tip distribution processors for epoch {}: {}",
        epoch,
        jito_mev_metas.len()
    );
    Ok(jito_mev_metas)
}

fn update_jito_mev_metas(
    jito_mev_metas: &mut Vec<JitoMevMeta>,
    account: &Account,
    pubkey: Pubkey,
    epoch: Epoch,
) -> anyhow::Result<()> {
    let (epoch_created_at, epoch_byte_index) = get_epoch_created_at(account)?;
    if epoch_created_at == epoch {
        update_mev_commission(jito_mev_metas, account, pubkey, epoch_byte_index, epoch)?;
    }
    Ok(())
}

fn update_mev_commission(
    jito_mev_metas: &mut Vec<JitoMevMeta>,
    account: &Account,
    account_pubkey: Pubkey,
    epoch_byte_index: usize,
    epoch: Epoch,
) -> anyhow::Result<()> {
    let JitoCommissionMeta {
        epoch_created_at,
        validator_commission_bps: jito_commission,
        validator_vote_account: vote_account,
    } = read_jito_commission_and_epoch(account_pubkey, account, epoch_byte_index)?;
    assert_eq!(epoch, epoch_created_at);
    jito_mev_metas.push(JitoMevMeta {
        vote_account,
        mev_commission: jito_commission,
    });
    Ok(())
}
