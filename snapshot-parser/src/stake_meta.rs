use {
    crate::serde_serialize::{option_pubkey_string_conversion, pubkey_string_conversion},
    crate::stake_activation::StakeActivation,
    crate::utils::lamports_to_sol,
    log::{error, info},
    serde::{Deserialize, Serialize},
    solana_program::pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_sdk::{
        account::{AccountSharedData, ReadableAccount},
        epoch_info::EpochInfo,
    },
    solana_stake_interface::{
        stake_history::{Epoch, StakeHistoryEntry},
        state::StakeStateV2,
    },
    std::{fmt::Debug, sync::Arc},
};

#[derive(Clone, Deserialize, Serialize, Debug, Eq, PartialEq)]
pub struct StakeMeta {
    #[serde(with = "pubkey_string_conversion")]
    pub pubkey: Pubkey,
    pub balance_lamports: u64,
    pub active_delegation_lamports: u64,
    pub activating_delegation_lamports: u64,
    pub deactivating_delegation_lamports: u64,
    #[serde(with = "option_pubkey_string_conversion")]
    pub validator: Option<Pubkey>,
    #[serde(with = "pubkey_string_conversion")]
    pub stake_authority: Pubkey,
    #[serde(with = "pubkey_string_conversion")]
    pub withdraw_authority: Pubkey,
}

impl Ord for StakeMeta {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.pubkey.cmp(&other.pubkey)
    }
}

impl PartialOrd<Self> for StakeMeta {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Deserialize, Serialize, Debug)]
pub struct StakeMetaCollection {
    pub epoch: Epoch,
    pub slot: u64,
    pub stake_metas: Vec<StakeMeta>,
}

// The stake accounts come from account_scan::scan_accounts_by_owner, which sweeps the
// storages once for every owner the caller needs. Loading them here instead would mean
// another full walk of the accounts index, which is what that scan exists to avoid.
pub fn generate_stake_meta_collection_for_accounts(
    bank: &Arc<Bank>,
    stake_accounts: &[(Pubkey, AccountSharedData)],
) -> anyhow::Result<StakeMetaCollection> {
    assert!(bank.is_frozen());

    let EpochInfo { absolute_slot, .. } = bank.get_epoch_info();

    let stake_activation = StakeActivation::new(bank)?;
    let epoch = stake_activation.epoch();
    info!("Stake history loaded.");

    let mut stake_metas: Vec<StakeMeta> = Default::default();

    for (pubkey, shared_account) in stake_accounts {
        let pubkey = *pubkey;
        let stake_account: StakeStateV2 = match bincode::deserialize(shared_account.data()) {
            Ok(account) => account,
            Err(err) => {
                error!("Error parsing stake account {}: {}", pubkey, err);
                continue;
            }
        };

        let (
            validator,
            active_delegation_lamports,
            activating_delegation_lamports,
            deactivating_delegation_lamports,
        ) = match stake_account.stake() {
            Some(stake) => {
                let StakeHistoryEntry {
                    effective,
                    activating,
                    deactivating,
                } = stake_activation.status(&stake.delegation);
                (
                    Some(stake.delegation.voter_pubkey),
                    effective,
                    activating,
                    deactivating,
                )
            }
            None => (None, 0, 0, 0),
        };

        stake_metas.push(StakeMeta {
            pubkey,
            balance_lamports: shared_account.lamports(),
            active_delegation_lamports,
            activating_delegation_lamports,
            deactivating_delegation_lamports,
            validator,
            stake_authority: stake_account.meta().unwrap_or_default().authorized.staker,
            withdraw_authority: stake_account
                .meta()
                .unwrap_or_default()
                .authorized
                .withdrawer,
        })
    }
    info!("Collected all stake account metas: {}", stake_metas.len());

    if stake_metas.is_empty() {
        anyhow::bail!(
            "Not expected. No stake metas collected for epoch {epoch}. Evaluate the snapshot data."
        );
    }

    let total_active: u64 = stake_metas
        .iter()
        .map(|s| s.active_delegation_lamports)
        .sum();
    let total_activating: u64 = stake_metas
        .iter()
        .map(|s| s.activating_delegation_lamports)
        .sum();
    let total_deactivating: u64 = stake_metas
        .iter()
        .map(|s| s.deactivating_delegation_lamports)
        .sum();

    info!("Total activated stake: {}", lamports_to_sol(total_active));
    info!(
        "Total activating stake: {}",
        lamports_to_sol(total_activating)
    );
    info!(
        "Total deactivating stake: {}",
        lamports_to_sol(total_deactivating)
    );

    stake_metas.sort();
    info!("Sorted stake account metas");

    Ok(StakeMetaCollection {
        epoch,
        slot: absolute_slot,
        stake_metas,
    })
}
