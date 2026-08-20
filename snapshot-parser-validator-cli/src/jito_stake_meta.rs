use crate::jito_mev::TIP_DISTRIBUTION_ACCOUNT_DISCRIMINATOR;
use crate::jito_priority_fee::{
    JITO_PRIORITY_FEE_DISTRIBUTION_PROGRAM, PRIORITY_FEE_DISTRIBUTION_ACCOUNT_DISCRIMINATOR,
};
use crate::jito_program_hash::compute_jito_program_hash;
use crate::utils::jito_parser::{
    get_epoch_created_at, read_jito_commission_and_epoch, read_merkle_root_upload_authority,
};
use crate::utils::SliceAt;
use {
    log::{error, info, warn},
    serde::{Deserialize, Serialize},
    snapshot_parser::serde_serialize::pubkey_string_conversion,
    snapshot_parser::stake_activation::StakeActivation,
    solana_program::pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_sdk::account::{AccountSharedData, ReadableAccount},
    solana_stake_interface::{stake_history::Epoch, state::StakeStateV2},
    std::{collections::HashMap, sync::Arc},
};

// Replicates jito-tip-router/tip-router-operator-cli/src/stake_meta_generator.rs

pub const JITO_TIP_PAYMENT_PROGRAM: &str = "T1pyyaTNZsKv2WcRAB8oVnk93mLJw2XzjtVYqCsaHqt";
const CONFIG_ACCOUNT_SEED: &[u8] = b"CONFIG_ACCOUNT";
const TIP_ACCOUNT_SEEDS: [&[u8]; 8] = [
    b"TIP_ACCOUNT_0",
    b"TIP_ACCOUNT_1",
    b"TIP_ACCOUNT_2",
    b"TIP_ACCOUNT_3",
    b"TIP_ACCOUNT_4",
    b"TIP_ACCOUNT_5",
    b"TIP_ACCOUNT_6",
    b"TIP_ACCOUNT_7",
];
const CONFIG_TIP_RECEIVER_BYTE_INDEX: usize = 8; // anchor header
const CONFIG_BLOCK_BUILDER_COMMISSION_PCT_BYTE_INDEX: usize = 8 + // anchor header
    64; // tip_receiver + block_builder pubkeys

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JitoStakeMetaCollection {
    pub stake_metas: Vec<JitoStakeMeta>,
    #[serde(with = "pubkey_string_conversion")]
    pub tip_distribution_program_id: Pubkey,
    #[serde(with = "pubkey_string_conversion")]
    pub priority_fee_distribution_program_id: Pubkey,
    pub bank_hash: String,
    pub epoch: Epoch,
    pub slot: u64,
    // Extra field on top of Jito's format; a collection produced by Jito has none
    #[serde(default)]
    pub jito_program_hash: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct JitoStakeMeta {
    #[serde(with = "pubkey_string_conversion")]
    pub validator_vote_account: Pubkey,
    #[serde(with = "pubkey_string_conversion")]
    pub validator_node_pubkey: Pubkey,
    pub maybe_tip_distribution_meta: Option<JitoTipDistributionMeta>,
    pub maybe_priority_fee_distribution_meta: Option<JitoPriorityFeeDistributionMeta>,
    pub delegations: Vec<JitoDelegation>,
    pub total_delegated: u64,
    pub commission: u8,
}

impl Ord for JitoStakeMeta {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.validator_vote_account
            .cmp(&other.validator_vote_account)
    }
}

impl PartialOrd<Self> for JitoStakeMeta {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct JitoTipDistributionMeta {
    #[serde(with = "pubkey_string_conversion")]
    pub merkle_root_upload_authority: Pubkey,
    #[serde(with = "pubkey_string_conversion")]
    pub tip_distribution_pubkey: Pubkey,
    pub total_tips: u64,
    pub validator_fee_bps: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct JitoPriorityFeeDistributionMeta {
    #[serde(with = "pubkey_string_conversion")]
    pub merkle_root_upload_authority: Pubkey,
    #[serde(with = "pubkey_string_conversion")]
    pub priority_fee_distribution_pubkey: Pubkey,
    pub total_tips: u64,
    pub validator_fee_bps: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct JitoDelegation {
    #[serde(with = "pubkey_string_conversion")]
    pub stake_account_pubkey: Pubkey,
    #[serde(with = "pubkey_string_conversion")]
    pub staker_pubkey: Pubkey,
    #[serde(with = "pubkey_string_conversion")]
    pub withdrawer_pubkey: Pubkey,
    pub lamports_delegated: u64,
}

impl Ord for JitoDelegation {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (
            self.stake_account_pubkey,
            self.withdrawer_pubkey,
            self.staker_pubkey,
            self.lamports_delegated,
        )
            .cmp(&(
                other.stake_account_pubkey,
                other.withdrawer_pubkey,
                other.staker_pubkey,
                other.lamports_delegated,
            ))
    }
}

impl PartialOrd<Self> for JitoDelegation {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

struct DistributionAccountMeta {
    pubkey: Pubkey,
    merkle_root_upload_authority: Pubkey,
    validator_fee_bps: u16,
    lamports: u64,
    data_len: usize,
}

pub fn generate_jito_stake_meta_collection(
    bank: &Arc<Bank>,
    stake_accounts: &[(Pubkey, AccountSharedData)],
    tip_distribution_accounts: &[(Pubkey, AccountSharedData)],
    priority_fee_distribution_accounts: &[(Pubkey, AccountSharedData)],
    tip_distribution_program: Pubkey,
    tip_payment_program: Pubkey,
    require_priority_fee_data: bool,
) -> anyhow::Result<JitoStakeMetaCollection> {
    assert!(bank.is_frozen());
    let epoch = bank.epoch();

    let priority_fee_distribution_program: Pubkey =
        JITO_PRIORITY_FEE_DISTRIBUTION_PROGRAM.try_into()?;
    let program_hash = compute_jito_program_hash(
        bank,
        &[tip_distribution_program, priority_fee_distribution_program],
    )?;

    let last_slot_in_epoch = bank.epoch_schedule().get_last_slot_in_epoch(epoch);
    if bank.slot() != last_slot_in_epoch {
        warn!(
            "Snapshot slot is not the last slot of the epoch, tips of the remaining slots are missing and the collection may differ from what Jito publishes [epoch={}, slot={}, last_slot_in_epoch={}]",
            epoch,
            bank.slot(),
            last_slot_in_epoch
        );
    }

    let mut delegations_by_voter = group_delegations_by_voter(bank, stake_accounts)?;
    let tip_distribution_metas = distribution_account_metas(
        tip_distribution_accounts,
        &TIP_DISTRIBUTION_ACCOUNT_DISCRIMINATOR,
        epoch,
    )?;
    if tip_distribution_metas.is_empty() {
        anyhow::bail!("Not expected. No Jito tip distribution accounts found for epoch {epoch}. Evaluate the snapshot data.");
    }
    info!(
        "Jito tip distribution accounts for epoch {}: {}",
        epoch,
        tip_distribution_metas.len()
    );
    let priority_fee_distribution_metas = distribution_account_metas(
        priority_fee_distribution_accounts,
        &PRIORITY_FEE_DISTRIBUTION_ACCOUNT_DISCRIMINATOR,
        epoch,
    )?;
    if priority_fee_distribution_metas.is_empty() {
        if require_priority_fee_data {
            anyhow::bail!("Not expected. No Jito priority fee distribution accounts found for epoch {epoch}. Evaluate the snapshot data.");
        }
        warn!("No Jito priority fee distribution accounts found for epoch {epoch}; continuing (priority-fee data not required).");
    }
    info!(
        "Jito priority fee distribution accounts for epoch {}: {}",
        epoch,
        priority_fee_distribution_metas.len()
    );

    let (tip_receiver, tip_receiver_fee) =
        read_undistributed_tip_receiver_fee(bank, tip_payment_program)?;

    let epoch_vote_accounts = bank
        .epoch_vote_accounts(epoch)
        .ok_or_else(|| anyhow::anyhow!("No epoch vote accounts found for epoch {epoch}"))?;

    let mut voters_without_delegations = 0;
    let mut stake_metas: Vec<JitoStakeMeta> = Vec::new();
    for (vote_pubkey, (_stake, vote_account)) in epoch_vote_accounts.iter() {
        let Some(mut delegations) = delegations_by_voter.remove(vote_pubkey) else {
            voters_without_delegations += 1;
            continue;
        };
        delegations.sort();
        let total_delegated = delegations.iter().try_fold(0_u64, |sum, delegation| {
            sum.checked_add(delegation.lamports_delegated)
                .ok_or_else(|| anyhow::anyhow!("total_delegated overflow for vote {vote_pubkey}"))
        })?;

        let maybe_tip_distribution_meta = tip_distribution_metas
            .get(vote_pubkey)
            .map(|meta| {
                anyhow::Ok(JitoTipDistributionMeta {
                    merkle_root_upload_authority: meta.merkle_root_upload_authority,
                    tip_distribution_pubkey: meta.pubkey,
                    total_tips: total_tips(
                        meta,
                        bank.get_minimum_balance_for_rent_exemption(meta.data_len),
                        Some((&tip_receiver, tip_receiver_fee)),
                    )?,
                    validator_fee_bps: meta.validator_fee_bps,
                })
            })
            .transpose()?;
        let maybe_priority_fee_distribution_meta = priority_fee_distribution_metas
            .get(vote_pubkey)
            .map(|meta| {
                anyhow::Ok(JitoPriorityFeeDistributionMeta {
                    merkle_root_upload_authority: meta.merkle_root_upload_authority,
                    priority_fee_distribution_pubkey: meta.pubkey,
                    total_tips: total_tips(
                        meta,
                        bank.get_minimum_balance_for_rent_exemption(meta.data_len),
                        None,
                    )?,
                    validator_fee_bps: meta.validator_fee_bps,
                })
            })
            .transpose()?;

        let vote_state_view = vote_account.vote_state_view();
        stake_metas.push(JitoStakeMeta {
            validator_vote_account: *vote_pubkey,
            validator_node_pubkey: *vote_state_view.node_pubkey(),
            maybe_tip_distribution_meta,
            maybe_priority_fee_distribution_meta,
            delegations,
            total_delegated,
            commission: vote_state_view.commission(),
        });
    }
    if voters_without_delegations > 0 {
        warn!("voter_pubkey not found in delegations map for {voters_without_delegations} vote accounts");
    }
    if !delegations_by_voter.is_empty() {
        warn!(
            "Delegations of {} voters not present in epoch vote accounts were dropped",
            delegations_by_voter.len()
        );
    }
    if stake_metas.is_empty() {
        anyhow::bail!("Not expected. No Jito stake metas collected for epoch {epoch}. Evaluate the snapshot data.");
    }
    // Map membership is not enough: the receiver's meta must survive the delegations filter too
    if tip_receiver_fee > 0
        && !stake_metas.iter().any(|stake_meta| {
            stake_meta
                .maybe_tip_distribution_meta
                .as_ref()
                .is_some_and(|meta| meta.tip_distribution_pubkey == tip_receiver)
        })
    {
        anyhow::bail!("Jito tip_receiver {tip_receiver} produced no stake meta for epoch {epoch}, {tip_receiver_fee} lamports of undistributed tips would be dropped");
    }

    stake_metas.sort();
    info!("Collected Jito stake metas: {}", stake_metas.len());

    Ok(JitoStakeMetaCollection {
        stake_metas,
        tip_distribution_program_id: tip_distribution_program,
        priority_fee_distribution_program_id: priority_fee_distribution_program,
        bank_hash: bank.hash().to_string(),
        epoch,
        slot: bank.slot(),
        jito_program_hash: program_hash.combined,
    })
}

fn total_tips(
    meta: &DistributionAccountMeta,
    rent_exempt_minimum: u64,
    tip_receiver: Option<(&Pubkey, u64)>,
) -> anyhow::Result<u64> {
    let lamports = match tip_receiver {
        Some((tip_receiver, tip_receiver_fee)) if meta.pubkey == *tip_receiver => meta
            .lamports
            .checked_add(tip_receiver_fee)
            .ok_or_else(|| anyhow::anyhow!("tip overflow for account {}", meta.pubkey))?,
        _ => meta.lamports,
    };
    lamports
        .checked_sub(rent_exempt_minimum)
        .ok_or_else(|| anyhow::anyhow!("total_tips underflow for account {}", meta.pubkey))
}

fn group_delegations_by_voter(
    bank: &Arc<Bank>,
    stake_accounts: &[(Pubkey, AccountSharedData)],
) -> anyhow::Result<HashMap<Pubkey, Vec<JitoDelegation>>> {
    let stake_activation = StakeActivation::new(bank)?;

    let mut delegations_by_voter: HashMap<Pubkey, Vec<JitoDelegation>> = HashMap::new();
    for (pubkey, shared_account) in stake_accounts {
        let pubkey = *pubkey;
        let stake_state: StakeStateV2 = match bincode::deserialize(shared_account.data()) {
            Ok(account) => account,
            Err(err) => {
                error!("Error parsing stake account {}: {}", pubkey, err);
                continue;
            }
        };
        let Some(stake) = stake_state.stake() else {
            continue;
        };
        if stake_activation.effective(&stake.delegation) == 0 {
            continue;
        }
        let authorized = stake_state.authorized().unwrap_or_default();
        delegations_by_voter
            .entry(stake.delegation.voter_pubkey)
            .or_default()
            .push(JitoDelegation {
                stake_account_pubkey: pubkey,
                staker_pubkey: authorized.staker,
                withdrawer_pubkey: authorized.withdrawer,
                lamports_delegated: stake.delegation.stake,
            });
    }
    Ok(delegations_by_voter)
}

fn distribution_account_metas(
    accounts: &[(Pubkey, AccountSharedData)],
    discriminator: &[u8; 8],
    epoch: Epoch,
) -> anyhow::Result<HashMap<Pubkey, DistributionAccountMeta>> {
    let mut metas: HashMap<Pubkey, DistributionAccountMeta> = HashMap::new();
    for (pubkey, account) in accounts {
        let pubkey = *pubkey;
        if !account.data().starts_with(discriminator) {
            continue;
        }
        let (epoch_created_at, epoch_byte_index) = get_epoch_created_at(account)?;
        if epoch_created_at != epoch {
            continue;
        }
        let commission_meta = read_jito_commission_and_epoch(pubkey, account, epoch_byte_index)?;
        metas.insert(
            commission_meta.validator_vote_account,
            DistributionAccountMeta {
                pubkey,
                merkle_root_upload_authority: read_merkle_root_upload_authority(pubkey, account)?,
                validator_fee_bps: commission_meta.validator_commission_bps,
                lamports: account.lamports(),
                data_len: account.data().len(),
            },
        );
    }
    Ok(metas)
}

// Tips left in the tip payment PDAs are cranked to the configured tip receiver in the next epoch
fn read_undistributed_tip_receiver_fee(
    bank: &Arc<Bank>,
    tip_payment_program: Pubkey,
) -> anyhow::Result<(Pubkey, u64)> {
    let (config_pubkey, _) =
        Pubkey::find_program_address(&[CONFIG_ACCOUNT_SEED], &tip_payment_program);
    let config_account = bank.get_account(&config_pubkey).ok_or_else(|| {
        anyhow::anyhow!("Jito tip payment config account {config_pubkey} not found")
    })?;
    let (tip_receiver, block_builder_commission_pct) = parse_tip_payment_config(&config_account)?;

    let mut excess_tip_balances: u64 = 0;
    for seed in TIP_ACCOUNT_SEEDS {
        let (tip_pubkey, _) = Pubkey::find_program_address(&[seed], &tip_payment_program);
        let tip_account = bank
            .get_account(&tip_pubkey)
            .ok_or_else(|| anyhow::anyhow!("Jito tip payment account {tip_pubkey} not found"))?;
        let excess = tip_account
            .lamports()
            .checked_sub(bank.get_minimum_balance_for_rent_exemption(tip_account.data().len()))
            .ok_or_else(|| anyhow::anyhow!("tip balance underflow for account {tip_pubkey}"))?;
        excess_tip_balances = excess_tip_balances
            .checked_add(excess)
            .ok_or_else(|| anyhow::anyhow!("excess tip balances overflow"))?;
    }

    let tip_receiver_fee =
        split_block_builder_fee(excess_tip_balances, block_builder_commission_pct)?;

    info!(
        "Jito undistributed tips: {} lamports, tip receiver: {}, fee after block builder cut: {}",
        excess_tip_balances, tip_receiver, tip_receiver_fee
    );
    Ok((tip_receiver, tip_receiver_fee))
}

// matches math in the tip payment program
fn split_block_builder_fee(
    excess_tip_balances: u64,
    block_builder_commission_pct: u64,
) -> anyhow::Result<u64> {
    let block_builder_tips = excess_tip_balances
        .checked_mul(block_builder_commission_pct)
        .ok_or_else(|| anyhow::anyhow!("block_builder_tips overflow"))?
        / 100;
    excess_tip_balances
        .checked_sub(block_builder_tips)
        .ok_or_else(|| anyhow::anyhow!("tip_receiver_fee underflow"))
}

fn parse_tip_payment_config(
    config_account: &impl ReadableAccount,
) -> anyhow::Result<(Pubkey, u64)> {
    let tip_receiver: Pubkey = config_account
        .data()
        .slice_at(CONFIG_TIP_RECEIVER_BYTE_INDEX, 32)?
        .try_into()
        .map_err(|e| anyhow::anyhow!("Failed to parse tip payment config tip_receiver: {:?}", e))?;
    let block_builder_commission_pct = u64::from_le_bytes(
        config_account
            .data()
            .slice_at(CONFIG_BLOCK_BUILDER_COMMISSION_PCT_BYTE_INDEX, 8)?
            .try_into()
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to parse tip payment config block_builder_commission_pct: {:?}",
                    e
                )
            })?,
    );
    Ok((tip_receiver, block_builder_commission_pct))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jito_mev::JITO_PROGRAM;
    use solana_sdk::account::Account;

    fn config_account(tip_receiver: Pubkey, block_builder: Pubkey, commission_pct: u64) -> Account {
        let mut data = vec![0_u8; 8];
        data.extend_from_slice(tip_receiver.as_ref());
        data.extend_from_slice(block_builder.as_ref());
        data.extend_from_slice(&commission_pct.to_le_bytes());
        data.extend_from_slice(&[0_u8; 9]);
        Account {
            lamports: 1,
            data,
            owner: JITO_TIP_PAYMENT_PROGRAM.try_into().unwrap(),
            executable: false,
            rent_epoch: 0,
        }
    }

    #[test]
    fn parses_tip_payment_config() {
        let tip_receiver = Pubkey::new_unique();
        let account = config_account(tip_receiver, Pubkey::new_unique(), 5);
        let (parsed_receiver, parsed_pct) = parse_tip_payment_config(&account).unwrap();
        assert_eq!(parsed_receiver, tip_receiver);
        assert_eq!(parsed_pct, 5);
    }

    #[test]
    fn serializes_with_jito_field_names() {
        let vote_account = Pubkey::new_unique();
        let collection = JitoStakeMetaCollection {
            stake_metas: vec![JitoStakeMeta {
                validator_vote_account: vote_account,
                validator_node_pubkey: Pubkey::new_unique(),
                maybe_tip_distribution_meta: Some(JitoTipDistributionMeta {
                    merkle_root_upload_authority: Pubkey::new_unique(),
                    tip_distribution_pubkey: Pubkey::new_unique(),
                    total_tips: 42,
                    validator_fee_bps: 800,
                }),
                maybe_priority_fee_distribution_meta: None,
                delegations: vec![JitoDelegation {
                    stake_account_pubkey: Pubkey::new_unique(),
                    staker_pubkey: Pubkey::new_unique(),
                    withdrawer_pubkey: Pubkey::new_unique(),
                    lamports_delegated: 1000,
                }],
                total_delegated: 1000,
                commission: 7,
            }],
            tip_distribution_program_id: JITO_PROGRAM.try_into().unwrap(),
            priority_fee_distribution_program_id: JITO_PRIORITY_FEE_DISTRIBUTION_PROGRAM
                .try_into()
                .unwrap(),
            bank_hash: "hash".to_string(),
            epoch: 1002,
            slot: 433295999,
            jito_program_hash: "2426260379.1775319386".to_string(),
        };

        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&collection).unwrap()).unwrap();
        assert_eq!(json["epoch"], 1002);
        assert_eq!(json["jito_program_hash"], "2426260379.1775319386");
        assert_eq!(
            json["tip_distribution_program_id"],
            JITO_PROGRAM.to_string()
        );
        let stake_meta = &json["stake_metas"][0];
        assert_eq!(
            stake_meta["validator_vote_account"],
            vote_account.to_string()
        );
        assert_eq!(stake_meta["commission"], 7);
        assert_eq!(stake_meta["total_delegated"], 1000);
        assert_eq!(stake_meta["maybe_tip_distribution_meta"]["total_tips"], 42);
        assert_eq!(
            stake_meta["maybe_tip_distribution_meta"]["validator_fee_bps"],
            800
        );
        assert!(stake_meta["maybe_priority_fee_distribution_meta"].is_null());
        assert_eq!(stake_meta["delegations"][0]["lamports_delegated"], 1000);
    }

    #[test]
    fn reads_a_collection_without_the_program_hash() {
        let jito_json = serde_json::json!({
            "stake_metas": [],
            "tip_distribution_program_id": JITO_PROGRAM,
            "priority_fee_distribution_program_id": JITO_PRIORITY_FEE_DISTRIBUTION_PROGRAM,
            "bank_hash": "hash",
            "epoch": 1002,
            "slot": 433295999,
        });
        let collection: JitoStakeMetaCollection = serde_json::from_value(jito_json).unwrap();
        assert_eq!(collection.jito_program_hash, "");
    }

    #[test]
    fn sorts_delegations_like_jito() {
        let mut keys = [Pubkey::new_unique(), Pubkey::new_unique()];
        keys.sort();
        let delegation = |stake_account: Pubkey| JitoDelegation {
            stake_account_pubkey: stake_account,
            staker_pubkey: Pubkey::new_unique(),
            withdrawer_pubkey: Pubkey::new_unique(),
            lamports_delegated: 1,
        };
        let mut delegations = [delegation(keys[1]), delegation(keys[0])];
        delegations.sort();
        assert_eq!(delegations[0].stake_account_pubkey, keys[0]);
        assert_eq!(delegations[1].stake_account_pubkey, keys[1]);
    }

    #[test]
    fn delegation_tie_break_orders_withdrawer_before_staker() {
        let stake_account = Pubkey::new_unique();
        let mut keys = [Pubkey::new_unique(), Pubkey::new_unique()];
        keys.sort();
        let delegation = |staker: Pubkey, withdrawer: Pubkey| JitoDelegation {
            stake_account_pubkey: stake_account,
            staker_pubkey: staker,
            withdrawer_pubkey: withdrawer,
            lamports_delegated: 1,
        };
        // the lower staker must lose to the lower withdrawer
        let mut delegations = [delegation(keys[0], keys[1]), delegation(keys[1], keys[0])];
        delegations.sort();
        assert_eq!(delegations[0].withdrawer_pubkey, keys[0]);
        assert_eq!(delegations[0].staker_pubkey, keys[1]);
    }

    #[test]
    fn splits_block_builder_fee() {
        assert_eq!(split_block_builder_fee(1_000, 0).unwrap(), 1_000);
        assert_eq!(split_block_builder_fee(1_000, 100).unwrap(), 0);
        assert_eq!(split_block_builder_fee(1_000, 5).unwrap(), 950);
        // truncation of the block builder cut must leave the remainder with the tip receiver
        assert_eq!(split_block_builder_fee(1_001, 5).unwrap(), 951);
        assert_eq!(split_block_builder_fee(0, 5).unwrap(), 0);
        assert!(split_block_builder_fee(u64::MAX, 101).is_err());
    }

    #[test]
    fn total_tips_credits_the_tip_receiver_only() {
        let tip_distribution_pubkey = Pubkey::new_unique();
        let meta = DistributionAccountMeta {
            pubkey: tip_distribution_pubkey,
            merkle_root_upload_authority: Pubkey::new_unique(),
            validator_fee_bps: 800,
            lamports: 1_000,
            data_len: 0,
        };

        assert_eq!(
            total_tips(&meta, 100, Some((&tip_distribution_pubkey, 50))).unwrap(),
            950
        );
        let other_receiver = Pubkey::new_unique();
        assert_eq!(
            total_tips(&meta, 100, Some((&other_receiver, 50))).unwrap(),
            900
        );
        assert_eq!(total_tips(&meta, 100, None).unwrap(), 900);
        assert!(total_tips(&meta, 2_000, None).is_err());
    }
}
