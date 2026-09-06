use crate::jito_priority_fee::fetch_jito_priority_fee_metas;
use {
    crate::jito_mev::fetch_jito_mev_metas,
    agave_feature_set::FeatureSnapshot,
    log::info,
    serde::{Deserialize, Serialize},
    snapshot_parser::serde_serialize::option_pubkey_string_conversion,
    snapshot_parser::serde_serialize::pubkey_string_conversion,
    snapshot_parser::utils::lamports_to_sol,
    solana_program::pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_sdk::{account::AccountSharedData, epoch_info::EpochInfo},
    solana_stake_interface::stake_history::Epoch,
    solana_vote::{vote_account::VoteAccountsHashMap, vote_state_view::VoteStateView},
    std::{fmt::Debug, sync::Arc},
};

#[derive(Clone, Deserialize, Serialize, Debug, Eq, PartialEq)]
pub struct ValidatorMeta {
    #[serde(with = "pubkey_string_conversion")]
    pub vote_account: Pubkey,
    pub commission: u8,
    /// jito-tip-distribution // TipDistributionAccount // validator_commission_bps
    pub mev_commission: Option<u16>,
    /// priority-fee-distribution // PriorityFeeDistributionAccount // validator_commission_bps
    pub jito_priority_fee_commission: Option<u16>,
    /// priority-fee-distribution // PriorityFeeDistributionAccount // total_lamports_transferred
    pub jito_priority_fee_lamports: u64,
    pub stake: u64,
    pub credits: u64,
    /// SIMD-0185 // VoteStateV4 // inflation_rewards_collector, read at
    /// `ValidatorMetaCollection::collector_vintage`; `None` on a pre-v4 vote state
    #[serde(default, with = "option_pubkey_string_conversion")]
    pub inflation_rewards_collector: Option<Pubkey>,
    /// the inflation rewards commission in basis points agave applies to `epoch`,
    /// taken from `ValidatorMetaCollection::commission_vintage`. A pre-v4 vote
    /// state has no basis-point field and agave synthesizes `commission * 100`
    /// from its integer percent, so this is always the number agave applies,
    /// whichever vote state version the vintage holds. `None` only in a file
    /// written before this field existed.
    #[serde(default)]
    pub inflation_rewards_commission_bps: Option<u16>,
    /// `true` when `inflation_rewards_commission_bps` is the VoteStateV4 field
    /// itself, `false` when it is `commission * 100` synthesized from a pre-v4
    /// vote state. `None` only in a file written before this field existed.
    #[serde(default)]
    pub inflation_rewards_commission_bps_is_v4: Option<bool>,
    /// SIMD-0185 // VoteStateV4 // block_revenue_collector, read from
    /// `epoch_stakes(epoch)` alone: agave resolves it there with no fallback, so
    /// `None` means agave has no collector to pay and credits the leader
    /// identity instead. Always `None` for every validator when
    /// `commission_vintage.epoch_stakes_key` is not `Some(epoch)`, because then
    /// the bank does not carry that snapshot at all.
    #[serde(default, with = "option_pubkey_string_conversion")]
    pub block_revenue_collector: Option<Pubkey>,
    /// SIMD-0185 // VoteStateV4 // block_revenue_commission_bps as held by the
    /// same snapshot as `block_revenue_collector`. Nothing in agave 4.2.1 reads
    /// it: `deposit_or_burn_fee` credits the whole non-burned deposit to the
    /// collector and applies no split.
    #[serde(default)]
    pub block_revenue_commission_bps: Option<u16>,
    /// SIMD-0185 // VoteStateV4 // pending_delegator_rewards as of `slot`.
    /// Written by the vote program and read by nothing in agave 4.2.1's reward
    /// path, so it is published as observed rather than at a vintage.
    #[serde(default)]
    pub pending_delegator_rewards: Option<u64>,
}

impl Ord for ValidatorMeta {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.vote_account.cmp(&other.vote_account)
    }
}

impl PartialOrd<Self> for ValidatorMeta {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Names the cached vote-account state that a field was read from.
///
/// agave keys `Bank::epoch_stakes` by leader schedule epoch, so the snapshot
/// stored at `epoch_stakes(E)` holds vote state as captured at the first slot of
/// epoch `E - 1`.
#[derive(Clone, Deserialize, Serialize, Debug, Default, Eq, PartialEq)]
pub struct VoteStateVintage {
    /// `Bank::epoch_stakes` key that was read, or `None` for the bank's live
    /// stakes cache, i.e. the state at `ValidatorMetaCollection::slot`
    pub epoch_stakes_key: Option<Epoch>,
    /// epoch at whose first slot the vote state behind this source was captured
    pub captured_at_epoch: Epoch,
}

impl VoteStateVintage {
    fn from_epoch_stakes(epoch_stakes_key: Epoch) -> Self {
        Self {
            epoch_stakes_key: Some(epoch_stakes_key),
            captured_at_epoch: epoch_stakes_key.saturating_sub(1),
        }
    }

    /// The stakes cache of a bank frozen at the last slot of `epoch`, which is
    /// the state agave reads the inflation rewards collector out of. agave
    /// snapshots it into `epoch_stakes(epoch + 2)` at the first slot of the
    /// distribution epoch, but only the accounts that survive SIMD-0357
    /// admission filtering while `validator_admission_ticket` is active: for an
    /// account carried by both the vote state is identical, what the filter
    /// changes is set membership.
    fn from_live_stakes_cache(epoch: Epoch) -> Self {
        Self {
            epoch_stakes_key: None,
            captured_at_epoch: epoch.saturating_add(1),
        }
    }
}

/// The agave features that decide how the vintages in this collection are used.
#[derive(Clone, Deserialize, Serialize, Debug, Default, Eq, PartialEq)]
pub struct SnapshotFeatures {
    /// agave `custom_commission_collector` (SIMD-0232) as of `slot`, which is
    /// exactly the value agave applies to block revenue produced during `epoch`:
    /// `deposit_or_burn_fee` reads the feature set of the bank that produces the
    /// block. While it is false the runtime credits the leader identity.
    pub block_revenue_custom_collector_active: bool,
    /// agave `custom_commission_collector` (SIMD-0232) for `epoch`'s inflation
    /// rewards; while it is false the runtime ignores
    /// `ValidatorMeta::inflation_rewards_collector` and pays the vote account
    pub inflation_rewards_custom_collector_active: Option<bool>,
    /// agave `delay_commission_updates` for `epoch`'s inflation rewards; while
    /// it is false the runtime ignores `commission_vintage` and takes the
    /// commission from the vote state at `slot`
    pub inflation_rewards_delay_commission_updates_active: Option<bool>,
    /// agave `commission_rate_in_basis_points` for `epoch`'s inflation rewards;
    /// while it is false the runtime applies `commission * 100` and ignores a
    /// VoteStateV4 basis-point commission
    pub inflation_rewards_commission_rate_in_basis_points_active: Option<bool>,
    /// agave `validator_admission_ticket` (SIMD-0357) for `epoch`'s inflation
    /// rewards; while it is true agave pays them only to the vote accounts that
    /// survive admission filtering, and this collection still carries a row for
    /// every vote account the bank holds
    pub inflation_rewards_validator_admission_ticket_active: Option<bool>,
}

impl SnapshotFeatures {
    /// agave calculates `epoch`'s inflation rewards on the first bank of
    /// `epoch + 1`, after that bank has applied its own feature activations, so
    /// a flag inactive at `slot` may still be active there. Features never
    /// deactivate, so an already active flag is all this bank can prove:
    /// `Some(true)` once active, `None` while not.
    fn on_the_distribution_bank(active_at_slot: bool) -> Option<bool> {
        active_at_slot.then_some(true)
    }

    fn from_feature_snapshot(features: &FeatureSnapshot) -> Self {
        Self {
            block_revenue_custom_collector_active: features.custom_commission_collector,
            inflation_rewards_custom_collector_active: Self::on_the_distribution_bank(
                features.custom_commission_collector,
            ),
            inflation_rewards_delay_commission_updates_active: Self::on_the_distribution_bank(
                features.delay_commission_updates,
            ),
            inflation_rewards_commission_rate_in_basis_points_active:
                Self::on_the_distribution_bank(features.commission_rate_in_basis_points),
            inflation_rewards_validator_admission_ticket_active: Self::on_the_distribution_bank(
                features.validator_admission_ticket,
            ),
        }
    }
}

#[derive(Clone, Deserialize, Serialize, Debug, Default)]
pub struct ValidatorMetaCollection {
    pub epoch: Epoch,
    pub slot: u64,
    pub capitalization: u64,
    pub epoch_duration_in_years: f64,
    pub validator_rate: f64,
    pub validator_rewards: u64,
    pub validator_metas: Vec<ValidatorMeta>,
    /// vintage of the vote state behind
    /// `ValidatorMeta::inflation_rewards_commission_bps`
    #[serde(default)]
    pub commission_vintage: VoteStateVintage,
    /// vintage of the vote state behind
    /// `ValidatorMeta::inflation_rewards_collector`
    #[serde(default)]
    pub collector_vintage: VoteStateVintage,
    /// vote accounts the snapshot `commission_vintage` names does not carry —
    /// too young to be in it, dropped from it by admission filtering, or closed
    /// and recreated since it was taken — whose commission agave resolves, and
    /// so does this collection, from the next snapshot instead. Counted against
    /// the snapshot `commission_vintage` names, so it is 0 when that is the
    /// live stakes cache, which carries every vote account by construction.
    #[serde(default)]
    pub commission_vintage_fallbacks: usize,
    /// agave features this collection's vintages depend on, as of `slot`
    #[serde(default)]
    pub features: SnapshotFeatures,
}

impl ValidatorMetaCollection {
    pub fn total_stake_weighted_credits(&self) -> u128 {
        self.validator_metas
            .iter()
            .map(|v| v.credits as u128 * v.stake as u128)
            .sum()
    }

    /// sum of lamports staked to all validators
    pub fn total_stake(&self) -> u64 {
        self.validator_metas.iter().map(|v| v.stake).sum()
    }

    // TODO: DELETE ME? (not used anymore)
    /// expected staker commission (MEV not calculated) reward for a staked lamport to be delivered by a validator
    pub fn expected_epr(&self) -> f64 {
        self.validator_rewards as f64 / self.total_stake() as f64
    }

    /// calculates expected staker reward per one staked lamport when particular commission is set
    pub fn expected_epr_calculator(&self) -> impl Fn(u8) -> f64 {
        let expected_epr = self.expected_epr();

        move |commission: u8| expected_epr * (100.0 - commission as f64) / 100.0
    }
}

struct VoteAccountMeta {
    vote_account: Pubkey,
    commission: u8,
    stake: u64,
    credits: u64,
    inflation_rewards_collector: Option<Pubkey>,
    inflation_rewards_commission_bps: u16,
    inflation_rewards_commission_bps_is_v4: bool,
    block_revenue_collector: Option<Pubkey>,
    block_revenue_commission_bps: Option<u16>,
    pending_delegator_rewards: Option<u64>,
}

struct VoteAccountMetaCollection {
    metas: Vec<VoteAccountMeta>,
    commission_vintage: VoteStateVintage,
    commission_vintage_fallbacks: usize,
}

/// The inflation rewards commission agave applies, and whether the vote state it
/// came from carries basis points of its own.
struct InflationRewardsCommission {
    bps: u16,
    is_v4: bool,
}

/// SIMD-0185 fields agave reads from the vote state at the end of the rewarded
/// epoch.
struct V4CollectorFields {
    inflation_rewards_collector: Pubkey,
    pending_delegator_rewards: u64,
}

/// SIMD-0232 block revenue fields, which are payable only while the vote state
/// carrying them is v4.
struct V4BlockRevenueFields {
    block_revenue_collector: Pubkey,
    block_revenue_commission_bps: u16,
}

/// `VoteStateView::inflation_rewards_commission` is defined on a pre-v4 vote
/// state too, where it synthesizes `commission * 100`, and agave applies that
/// synthesized value verbatim. Only the collector getters report absence, so
/// `inflation_rewards_collector` is what tells the two vote state versions apart.
fn inflation_rewards_commission(vote_state_view: &VoteStateView) -> InflationRewardsCommission {
    InflationRewardsCommission {
        bps: vote_state_view.inflation_rewards_commission(),
        is_v4: vote_state_view.inflation_rewards_collector().is_some(),
    }
}

fn v4_collector_fields(vote_state_view: &VoteStateView) -> Option<V4CollectorFields> {
    Some(V4CollectorFields {
        inflation_rewards_collector: *vote_state_view.inflation_rewards_collector()?,
        pending_delegator_rewards: vote_state_view.pending_delegator_rewards(),
    })
}

fn v4_block_revenue_fields(vote_state_view: &VoteStateView) -> Option<V4BlockRevenueFields> {
    Some(V4BlockRevenueFields {
        block_revenue_collector: *vote_state_view.block_revenue_collector()?,
        block_revenue_commission_bps: vote_state_view.block_revenue_commission(),
    })
}

/// The vote-account snapshots agave applies to a rewarded epoch's commission.
///
/// `primary` is agave `snapshot_epoch_vote_accounts`, the anti-rug snapshot the
/// runtime reads the inflation rewards commission from, and the only snapshot
/// SIMD-0232 reads the block revenue collector from while the epoch is being
/// produced. `fallback` is agave `rewarded_epoch_vote_accounts`, which the
/// runtime falls back to for the commission of a vote account the anti-rug
/// snapshot does not carry.
struct CommissionVintageSource<'a> {
    primary: Option<&'a VoteAccountsHashMap>,
    fallback: Option<&'a VoteAccountsHashMap>,
    epoch: Epoch,
}

impl<'a> CommissionVintageSource<'a> {
    fn new(
        epoch_vote_accounts: impl Fn(Epoch) -> Option<&'a VoteAccountsHashMap>,
        epoch: Epoch,
    ) -> Self {
        Self {
            primary: epoch_vote_accounts(epoch),
            fallback: epoch_vote_accounts(epoch.saturating_add(1)),
            epoch,
        }
    }

    fn vintage(&self) -> VoteStateVintage {
        if self.primary.is_some() {
            VoteStateVintage::from_epoch_stakes(self.epoch)
        } else if self.fallback.is_some() {
            VoteStateVintage::from_epoch_stakes(self.epoch.saturating_add(1))
        } else {
            VoteStateVintage::from_live_stakes_cache(self.epoch)
        }
    }

    /// The snapshot `vintage` names, or `None` when that is the live stakes cache.
    fn vintage_accounts(&self) -> Option<&'a VoteAccountsHashMap> {
        self.primary.or(self.fallback)
    }

    fn misses_its_vintage(&self, vote_account: &Pubkey) -> bool {
        self.vintage_accounts()
            .is_some_and(|vote_accounts| !vote_accounts.contains_key(vote_account))
    }

    /// agave `snapshot_epoch_vote_accounts.or_else(rewarded_epoch_vote_accounts)`.
    fn commission_view(&self, vote_account: &Pubkey) -> Option<&'a VoteStateView> {
        Self::lookup(self.primary, vote_account)
            .or_else(|| Self::lookup(self.fallback, vote_account))
    }

    /// agave resolves the block revenue collector through `epoch_stakes(epoch)`
    /// alone and expects the leader to be in it, so absent there is absent.
    fn block_revenue_view(&self, vote_account: &Pubkey) -> Option<&'a VoteStateView> {
        Self::lookup(self.primary, vote_account)
    }

    fn lookup(
        vote_accounts: Option<&'a VoteAccountsHashMap>,
        vote_account: &Pubkey,
    ) -> Option<&'a VoteStateView> {
        vote_accounts
            .and_then(|vote_accounts| vote_accounts.get(vote_account))
            .map(|(_, vote_account)| vote_account.vote_state_view())
    }
}

fn fetch_vote_account_metas<'a>(
    live_vote_accounts: &VoteAccountsHashMap,
    epoch_vote_accounts: impl Fn(Epoch) -> Option<&'a VoteAccountsHashMap>,
    epoch: Epoch,
) -> VoteAccountMetaCollection {
    let commission_source = CommissionVintageSource::new(epoch_vote_accounts, epoch);
    let mut commission_vintage_fallbacks = 0;
    let mut metas = Vec::with_capacity(live_vote_accounts.len());

    for (pubkey, (stake, vote_account)) in live_vote_accounts.iter() {
        let vote_state_view = vote_account.vote_state_view();
        let credits = vote_state_view
            .epoch_credits_iter()
            .find_map(|item| {
                if item.epoch() == epoch {
                    Some(vote_state_view.credits() - item.prev_credits())
                } else {
                    None
                }
            })
            .unwrap_or(0);

        if commission_source.misses_its_vintage(pubkey) {
            commission_vintage_fallbacks += 1;
        }
        let commission = inflation_rewards_commission(
            commission_source
                .commission_view(pubkey)
                .unwrap_or(vote_state_view),
        );
        let collector_fields = v4_collector_fields(vote_state_view);
        let block_revenue_fields = commission_source
            .block_revenue_view(pubkey)
            .and_then(v4_block_revenue_fields);

        metas.push(VoteAccountMeta {
            vote_account: *pubkey,
            commission: vote_state_view.commission(),
            stake: *stake,
            credits,
            inflation_rewards_collector: collector_fields
                .as_ref()
                .map(|fields| fields.inflation_rewards_collector),
            inflation_rewards_commission_bps: commission.bps,
            inflation_rewards_commission_bps_is_v4: commission.is_v4,
            block_revenue_collector: block_revenue_fields
                .as_ref()
                .map(|fields| fields.block_revenue_collector),
            block_revenue_commission_bps: block_revenue_fields
                .as_ref()
                .map(|fields| fields.block_revenue_commission_bps),
            pending_delegator_rewards: collector_fields
                .as_ref()
                .map(|fields| fields.pending_delegator_rewards),
        });
    }

    VoteAccountMetaCollection {
        metas,
        commission_vintage: commission_source.vintage(),
        commission_vintage_fallbacks,
    }
}

/// `collector_vintage` and `commission_vintage` only describe a bank frozen at
/// the last slot of its epoch; a mid-epoch archive would silently mislabel every
/// field read out of the live stakes cache.
fn check_end_of_epoch_bank(slot: u64, last_slot_in_epoch: u64, epoch: Epoch) -> anyhow::Result<()> {
    if slot != last_slot_in_epoch {
        anyhow::bail!(
            "Bank is at slot {slot}, not the last slot {last_slot_in_epoch} of epoch {epoch}; the vote state vintages this collection records would not hold. Parse the end-of-epoch snapshot instead."
        );
    }

    Ok(())
}

pub fn generate_validator_collection(
    bank: &Arc<Bank>,
    tip_distribution_accounts: &[(Pubkey, AccountSharedData)],
    priority_fee_distribution_accounts: &[(Pubkey, AccountSharedData)],
    require_priority_fee_data: bool,
) -> anyhow::Result<ValidatorMetaCollection> {
    assert!(bank.is_frozen());

    let EpochInfo {
        epoch,
        absolute_slot,
        ..
    } = bank.get_epoch_info();
    check_end_of_epoch_bank(
        absolute_slot,
        bank.epoch_schedule().get_last_slot_in_epoch(epoch),
        epoch,
    )?;

    let validator_rate = bank
        .inflation()
        .validator(bank.slot_in_year_for_inflation());
    let capitalization = bank.capitalization();
    let epoch_duration_in_years = bank.epoch_duration_in_years(epoch);
    let validator_rewards =
        (validator_rate * capitalization as f64 * epoch_duration_in_years) as u64;

    let live_vote_accounts = bank.vote_accounts();
    let VoteAccountMetaCollection {
        metas: vote_account_metas,
        commission_vintage,
        commission_vintage_fallbacks,
    } = fetch_vote_account_metas(
        &live_vote_accounts,
        |epoch| bank.epoch_vote_accounts(epoch),
        epoch,
    );
    let collector_vintage = VoteStateVintage::from_live_stakes_cache(epoch);
    let features = SnapshotFeatures::from_feature_snapshot(bank.feature_set.snapshot());
    let jito_mev_metas = fetch_jito_mev_metas(tip_distribution_accounts, epoch)?;
    let jito_priority_fee_metas = fetch_jito_priority_fee_metas(
        priority_fee_distribution_accounts,
        epoch,
        require_priority_fee_data,
    )?;

    let mut validator_metas = vote_account_metas
        .into_iter()
        .map(|vote_account_meta| {
            let mev_commission = jito_mev_metas
                .iter()
                .find(|jito_mev_meta| jito_mev_meta.vote_account == vote_account_meta.vote_account)
                .map(|jito_mev_meta| Some(jito_mev_meta.mev_commission))
                .unwrap_or_else(|| {
                    info!(
                        "No Jito MEV commission found for vote account: {}",
                        vote_account_meta.vote_account
                    );
                    None
                });
            let priority_fee = jito_priority_fee_metas
                .iter()
                .find(|jito_priority_fee_meta| {
                    jito_priority_fee_meta.validator_vote_account == vote_account_meta.vote_account
                })
                .map(|jito_priority_fee_meta| {
                    (
                        Some(jito_priority_fee_meta.validator_commission_bps),
                        jito_priority_fee_meta.total_lamports_transferred,
                    )
                })
                .unwrap_or_else(|| {
                    info!(
                        "No Jito Priority Fee commission found for vote account: {}",
                        vote_account_meta.vote_account
                    );
                    (None, 0)
                });
            ValidatorMeta {
                vote_account: vote_account_meta.vote_account,
                commission: vote_account_meta.commission,
                mev_commission,
                jito_priority_fee_commission: priority_fee.0,
                jito_priority_fee_lamports: priority_fee.1,
                stake: vote_account_meta.stake,
                credits: vote_account_meta.credits,
                inflation_rewards_collector: vote_account_meta.inflation_rewards_collector,
                inflation_rewards_commission_bps: Some(
                    vote_account_meta.inflation_rewards_commission_bps,
                ),
                inflation_rewards_commission_bps_is_v4: Some(
                    vote_account_meta.inflation_rewards_commission_bps_is_v4,
                ),
                block_revenue_collector: vote_account_meta.block_revenue_collector,
                block_revenue_commission_bps: vote_account_meta.block_revenue_commission_bps,
                pending_delegator_rewards: vote_account_meta.pending_delegator_rewards,
            }
        })
        .collect::<Vec<_>>();

    let total_validators = validator_metas.len();
    let validators_with_credits = validator_metas.iter().filter(|v| v.credits > 0).count();
    let total_credits: u64 = validator_metas.iter().map(|v| v.credits).sum();
    let total_stake: u64 = validator_metas.iter().map(|v| v.stake).sum();
    let v4_validators = validator_metas
        .iter()
        .filter(|v| v.inflation_rewards_collector.is_some())
        .count();

    info!("Collected all vote account metas: {}", total_validators);
    info!(
        "Validators with credits: {} / {}",
        validators_with_credits, total_validators
    );
    info!("Total credits: {}", total_credits);
    info!(
        "Total stake: {} lamports ({:.2} SOL)",
        total_stake,
        lamports_to_sol(total_stake)
    );
    info!(
        "Validator rewards: {} lamports ({:.2} SOL)",
        validator_rewards,
        lamports_to_sol(validator_rewards)
    );
    info!(
        "Vote accounts on SIMD-0185 v4 state: {} / {}",
        v4_validators, total_validators
    );
    info!(
        "Commission vintage: {:?}, missing {} / {} vote accounts that are resolved from the next snapshot instead",
        commission_vintage, commission_vintage_fallbacks, total_validators
    );
    info!("Collector vintage: {:?}", collector_vintage);
    info!("Snapshot features: {:?}", features);

    if total_credits == 0 {
        anyhow::bail!(
            "Total credits sum is 0 for epoch {}. This likely indicates a problem with the snapshot data.",
            epoch
        );
    }

    validator_metas.sort();
    info!("Sorted vote account metas");

    Ok(ValidatorMetaCollection {
        epoch,
        slot: absolute_slot,
        capitalization,
        epoch_duration_in_years,
        validator_rate,
        validator_rewards,
        validator_metas,
        commission_vintage,
        collector_vintage,
        commission_vintage_fallbacks,
        features,
    })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_feature_set::FeatureSet,
        solana_vote::vote_account::VoteAccount,
        solana_vote_interface::state::{VoteStateV3, VoteStateV4, VoteStateVersions},
        std::collections::HashMap,
    };

    const VOTE_PROGRAM_ID: &str = "Vote111111111111111111111111111111111111111";
    const EPOCH: Epoch = 900;
    const INFLATION_REWARDS_COLLECTOR: Pubkey = Pubkey::new_from_array([1u8; 32]);
    const BLOCK_REVENUE_COLLECTOR: Pubkey = Pubkey::new_from_array([2u8; 32]);
    const VOTE_ACCOUNT: Pubkey = Pubkey::new_from_array([7u8; 32]);
    const OTHER_VOTE_ACCOUNT: Pubkey = Pubkey::new_from_array([8u8; 32]);

    fn v3(commission: u8) -> VoteStateVersions {
        VoteStateVersions::new_v3(VoteStateV3 {
            commission,
            ..VoteStateV3::default()
        })
    }

    fn v4(inflation_rewards_commission_bps: u16) -> VoteStateVersions {
        VoteStateVersions::new_v4(VoteStateV4 {
            inflation_rewards_collector: INFLATION_REWARDS_COLLECTOR,
            block_revenue_collector: BLOCK_REVENUE_COLLECTOR,
            inflation_rewards_commission_bps,
            block_revenue_commission_bps: 1234,
            pending_delegator_rewards: 987_654_321,
            ..VoteStateV4::default()
        })
    }

    fn vote_state_view(vote_state_versions: VoteStateVersions) -> VoteStateView {
        VoteStateView::try_new(Arc::new(bincode::serialize(&vote_state_versions).unwrap())).unwrap()
    }

    fn vote_accounts<const N: usize>(
        entries: [(Pubkey, VoteStateVersions); N],
    ) -> VoteAccountsHashMap {
        entries
            .into_iter()
            .map(|(pubkey, vote_state_versions)| {
                let account = AccountSharedData::create_from_existing_shared_data(
                    1,
                    Arc::new(bincode::serialize(&vote_state_versions).unwrap()),
                    VOTE_PROGRAM_ID.parse().unwrap(),
                    false,
                    0,
                );
                (pubkey, (42, VoteAccount::try_from(account).unwrap()))
            })
            .collect()
    }

    fn meta_of<'a>(
        collection: &'a VoteAccountMetaCollection,
        vote_account: &Pubkey,
    ) -> &'a VoteAccountMeta {
        collection
            .metas
            .iter()
            .find(|meta| &meta.vote_account == vote_account)
            .expect("every live vote account gets a meta")
    }

    fn metas_from(
        live_vote_accounts: &VoteAccountsHashMap,
        epoch_stakes: &HashMap<Epoch, VoteAccountsHashMap>,
    ) -> VoteAccountMetaCollection {
        fetch_vote_account_metas(live_vote_accounts, |epoch| epoch_stakes.get(&epoch), EPOCH)
    }

    // The whole point of the anti-rug snapshot: a validator that converts to v4
    // and sets 100% mid-epoch is still paid at the commission it published a
    // full epoch earlier, even though that vintage carries no basis-point field.
    #[test]
    fn a_mid_epoch_v4_conversion_is_still_paid_at_the_snapshot_vintage_commission() {
        let live = vote_accounts([(VOTE_ACCOUNT, v4(10_000))]);
        let epoch_stakes = HashMap::from([(EPOCH, vote_accounts([(VOTE_ACCOUNT, v3(5))]))]);

        let collection = metas_from(&live, &epoch_stakes);
        let meta = meta_of(&collection, &VOTE_ACCOUNT);

        assert_eq!(
            meta.inflation_rewards_commission_bps, 500,
            "agave synthesizes commission * 100 out of the pre-v4 vintage and pays that"
        );
        assert!(!meta.inflation_rewards_commission_bps_is_v4);
        assert_eq!(
            meta.commission, 100,
            "the legacy percent stays the live, rugged one"
        );
        assert_eq!(
            meta.inflation_rewards_collector,
            Some(INFLATION_REWARDS_COLLECTOR),
            "the collector is read at the distribution vintage, where the state is v4"
        );
        assert_eq!(
            meta.block_revenue_collector, None,
            "the block revenue collector vintage still holds a pre-v4 state"
        );
        assert_eq!(collection.commission_vintage_fallbacks, 0);
    }

    #[test]
    fn a_v4_commission_vintage_is_published_as_its_own_basis_points() {
        let live = vote_accounts([(VOTE_ACCOUNT, v3(9))]);
        let epoch_stakes = HashMap::from([(EPOCH, vote_accounts([(VOTE_ACCOUNT, v4(733))]))]);

        let collection = metas_from(&live, &epoch_stakes);
        let meta = meta_of(&collection, &VOTE_ACCOUNT);

        assert_eq!(meta.inflation_rewards_commission_bps, 733);
        assert!(meta.inflation_rewards_commission_bps_is_v4);
        assert_eq!(meta.block_revenue_collector, Some(BLOCK_REVENUE_COLLECTOR));
        assert_eq!(meta.block_revenue_commission_bps, Some(1234));
        assert_eq!(
            meta.pending_delegator_rewards, None,
            "pending delegator rewards are observed at the slot, where the state is pre-v4"
        );
    }

    // agave reads the commission from epoch_stakes(rewarded_epoch), the snapshot
    // taken a full epoch before the rewarded one.
    #[test]
    fn the_commission_comes_from_the_epoch_stakes_snapshot_keyed_by_the_rewarded_epoch() {
        let live = vote_accounts([(VOTE_ACCOUNT, v4(9_000))]);
        let epoch_stakes = HashMap::from([
            (EPOCH - 1, vote_accounts([(VOTE_ACCOUNT, v4(4_000))])),
            (EPOCH, vote_accounts([(VOTE_ACCOUNT, v4(5_000))])),
            (EPOCH + 1, vote_accounts([(VOTE_ACCOUNT, v4(6_000))])),
            (EPOCH + 2, vote_accounts([(VOTE_ACCOUNT, v4(7_000))])),
        ]);

        let collection = metas_from(&live, &epoch_stakes);

        assert_eq!(
            meta_of(&collection, &VOTE_ACCOUNT).inflation_rewards_commission_bps,
            5_000
        );
        assert_eq!(
            collection.commission_vintage,
            VoteStateVintage::from_epoch_stakes(EPOCH)
        );
        assert_eq!(collection.commission_vintage_fallbacks, 0);
    }

    // agave: snapshot_epoch_vote_accounts.or_else(rewarded_epoch_vote_accounts).
    #[test]
    fn a_vote_account_the_snapshot_vintage_misses_falls_back_to_the_next_snapshot() {
        let live = vote_accounts([(VOTE_ACCOUNT, v4(9_000)), (OTHER_VOTE_ACCOUNT, v4(9_000))]);
        let epoch_stakes = HashMap::from([
            (EPOCH, vote_accounts([(OTHER_VOTE_ACCOUNT, v4(5_000))])),
            (
                EPOCH + 1,
                vote_accounts([(VOTE_ACCOUNT, v4(6_000)), (OTHER_VOTE_ACCOUNT, v4(6_000))]),
            ),
        ]);

        let collection = metas_from(&live, &epoch_stakes);

        assert_eq!(
            meta_of(&collection, &VOTE_ACCOUNT).inflation_rewards_commission_bps,
            6_000,
            "the account too young for the anti-rug snapshot is resolved from the next one"
        );
        assert_eq!(
            meta_of(&collection, &OTHER_VOTE_ACCOUNT).inflation_rewards_commission_bps,
            5_000
        );
        assert_eq!(collection.commission_vintage_fallbacks, 1);
        assert_eq!(
            collection.commission_vintage,
            VoteStateVintage::from_epoch_stakes(EPOCH)
        );
    }

    #[test]
    fn a_vote_account_no_snapshot_carries_is_resolved_from_the_state_at_the_slot() {
        let live = vote_accounts([(VOTE_ACCOUNT, v4(9_000))]);
        let epoch_stakes =
            HashMap::from([(EPOCH, vote_accounts([(OTHER_VOTE_ACCOUNT, v4(5_000))]))]);

        let collection = metas_from(&live, &epoch_stakes);

        assert_eq!(
            meta_of(&collection, &VOTE_ACCOUNT).inflation_rewards_commission_bps,
            9_000
        );
        assert_eq!(collection.commission_vintage_fallbacks, 1);
    }

    // agave resolves the block revenue collector through epoch_stakes(epoch) with
    // no fallback, so a vintage agave would never read must not leak into it.
    #[test]
    fn the_block_revenue_collector_is_absent_when_the_snapshot_vintage_is() {
        let live = vote_accounts([(VOTE_ACCOUNT, v4(9_000))]);
        let epoch_stakes = HashMap::from([
            (EPOCH, vote_accounts([(OTHER_VOTE_ACCOUNT, v4(5_000))])),
            (EPOCH + 1, vote_accounts([(VOTE_ACCOUNT, v4(6_000))])),
        ]);

        let collection = metas_from(&live, &epoch_stakes);
        let meta = meta_of(&collection, &VOTE_ACCOUNT);

        assert_eq!(meta.block_revenue_collector, None);
        assert_eq!(meta.block_revenue_commission_bps, None);
        assert_eq!(
            meta.inflation_rewards_commission_bps, 6_000,
            "only the commission follows agave's fallback"
        );
    }

    // A collection cannot say "my vintage is the next snapshot" and "N accounts
    // were missing from my vintage" at the same time.
    #[test]
    fn a_missing_snapshot_vintage_makes_the_next_one_the_vintage_and_counts_no_fallback() {
        let live = vote_accounts([(VOTE_ACCOUNT, v4(9_000))]);
        let epoch_stakes = HashMap::from([(EPOCH + 1, vote_accounts([(VOTE_ACCOUNT, v4(6_000))]))]);

        let collection = metas_from(&live, &epoch_stakes);

        assert_eq!(
            collection.commission_vintage,
            VoteStateVintage::from_epoch_stakes(EPOCH + 1)
        );
        assert_eq!(collection.commission_vintage_fallbacks, 0);
        assert_eq!(
            meta_of(&collection, &VOTE_ACCOUNT).inflation_rewards_commission_bps,
            6_000
        );
    }

    #[test]
    fn no_snapshot_at_all_makes_the_live_stakes_cache_the_vintage_and_counts_no_fallback() {
        let live = vote_accounts([(VOTE_ACCOUNT, v4(9_000))]);

        let collection = metas_from(&live, &HashMap::new());

        assert_eq!(
            collection.commission_vintage,
            VoteStateVintage::from_live_stakes_cache(EPOCH)
        );
        assert_eq!(collection.commission_vintage_fallbacks, 0);
        assert_eq!(
            meta_of(&collection, &VOTE_ACCOUNT).inflation_rewards_commission_bps,
            9_000
        );
    }

    #[test]
    fn the_legacy_commission_stays_the_lossy_percent_of_the_v4_basis_points() {
        assert_eq!(vote_state_view(v4(733)).commission(), 7);
    }

    #[test]
    fn a_pre_v4_vote_state_yields_no_v4_only_fields_and_an_unchanged_commission() {
        let vote_state_view = vote_state_view(v3(9));

        assert!(v4_collector_fields(&vote_state_view).is_none());
        assert!(v4_block_revenue_fields(&vote_state_view).is_none());
        assert_eq!(vote_state_view.commission(), 9);
    }

    #[test]
    fn a_pre_v4_vote_state_still_answers_the_synthesized_commission_getter() {
        let commission = inflation_rewards_commission(&vote_state_view(v3(9)));

        assert_eq!(commission.bps, 900);
        assert!(!commission.is_v4);
    }

    #[test]
    fn the_commission_vintage_is_the_state_from_the_start_of_the_epoch_before_the_rewarded_one() {
        assert_eq!(
            VoteStateVintage::from_epoch_stakes(900),
            VoteStateVintage {
                epoch_stakes_key: Some(900),
                captured_at_epoch: 899,
            }
        );
    }

    #[test]
    fn the_collector_vintage_is_the_state_from_the_start_of_the_distribution_epoch() {
        assert_eq!(
            VoteStateVintage::from_live_stakes_cache(900),
            VoteStateVintage {
                epoch_stakes_key: None,
                captured_at_epoch: 901,
            }
        );
    }

    // Each flag has to name the feature that governs it: the block revenue
    // collector is gated on custom_commission_collector, not on a neighbour.
    #[test]
    fn every_published_flag_reports_its_own_feature() {
        let mut features = FeatureSet::default().snapshot().clone();
        features.commission_rate_in_basis_points = true;
        features.delay_commission_updates = true;
        features.validator_admission_ticket = true;

        assert_eq!(
            SnapshotFeatures::from_feature_snapshot(&features),
            SnapshotFeatures {
                block_revenue_custom_collector_active: false,
                inflation_rewards_custom_collector_active: None,
                inflation_rewards_delay_commission_updates_active: Some(true),
                inflation_rewards_commission_rate_in_basis_points_active: Some(true),
                inflation_rewards_validator_admission_ticket_active: Some(true),
            }
        );

        features.custom_commission_collector = true;

        assert_eq!(
            SnapshotFeatures::from_feature_snapshot(&features),
            SnapshotFeatures {
                block_revenue_custom_collector_active: true,
                inflation_rewards_custom_collector_active: Some(true),
                inflation_rewards_delay_commission_updates_active: Some(true),
                inflation_rewards_commission_rate_in_basis_points_active: Some(true),
                inflation_rewards_validator_admission_ticket_active: Some(true),
            }
        );
    }

    #[test]
    fn a_flag_inactive_at_the_slot_is_not_reported_as_inactive_for_the_inflation_rewards() {
        let features = SnapshotFeatures::from_feature_snapshot(FeatureSet::default().snapshot());

        assert!(!features.block_revenue_custom_collector_active);
        assert_eq!(features.inflation_rewards_custom_collector_active, None);
        assert_eq!(
            features.inflation_rewards_delay_commission_updates_active,
            None
        );
    }

    #[test]
    fn a_bank_at_the_last_slot_of_its_epoch_is_accepted() {
        check_end_of_epoch_bank(433_295_999, 433_295_999, 1002).unwrap();
    }

    #[test]
    fn a_bank_short_of_the_last_slot_of_its_epoch_is_rejected() {
        let err = check_end_of_epoch_bank(433_000_000, 433_295_999, 1002)
            .expect_err("a mid-epoch archive would mislabel every recorded vintage");

        assert!(
            err.to_string().contains("433295999"),
            "the error must name the slot the snapshot should have been taken at: {err}"
        );
    }

    fn validator_meta() -> ValidatorMeta {
        ValidatorMeta {
            vote_account: VOTE_ACCOUNT,
            commission: 7,
            mev_commission: Some(1000),
            jito_priority_fee_commission: Some(2000),
            jito_priority_fee_lamports: 123,
            stake: 456,
            credits: 789,
            inflation_rewards_collector: Some(INFLATION_REWARDS_COLLECTOR),
            inflation_rewards_commission_bps: Some(733),
            inflation_rewards_commission_bps_is_v4: Some(true),
            block_revenue_collector: Some(BLOCK_REVENUE_COLLECTOR),
            block_revenue_commission_bps: Some(1234),
            pending_delegator_rewards: Some(987_654_321),
        }
    }

    // ds-sam and validator-bonds read this file and settle real SOL against it,
    // so a rename has to break the build here rather than a downstream consumer
    #[test]
    fn the_validator_meta_json_shape_is_pinned() {
        assert_eq!(
            serde_json::to_value(validator_meta()).unwrap(),
            serde_json::json!({
                "vote_account": "US517G5965aydkZ46HS38QLi7UQiSojurfbQfKCELFx",
                "commission": 7,
                "mev_commission": 1000,
                "jito_priority_fee_commission": 2000,
                "jito_priority_fee_lamports": 123,
                "stake": 456,
                "credits": 789,
                "inflation_rewards_collector": "4vJ9JU1bJJE96FWSJKvHsmmFADCg4gpZQff4P3bkLKi",
                "inflation_rewards_commission_bps": 733,
                "inflation_rewards_commission_bps_is_v4": true,
                "block_revenue_collector": "8qbHbw2BbbTHBW1sbeqakYXVKRQM8Ne7pLK7m6CVfeR",
                "block_revenue_commission_bps": 1234,
                "pending_delegator_rewards": 987_654_321,
            })
        );
    }

    #[test]
    fn a_pre_v4_validator_meta_serializes_to_the_pre_simd_0185_shape_plus_nulls() {
        let pre_v4 = ValidatorMeta {
            inflation_rewards_collector: None,
            inflation_rewards_commission_bps: Some(900),
            inflation_rewards_commission_bps_is_v4: Some(false),
            block_revenue_collector: None,
            block_revenue_commission_bps: None,
            pending_delegator_rewards: None,
            ..validator_meta()
        };

        assert_eq!(
            serde_json::to_value(pre_v4).unwrap(),
            serde_json::json!({
                "vote_account": "US517G5965aydkZ46HS38QLi7UQiSojurfbQfKCELFx",
                "commission": 7,
                "mev_commission": 1000,
                "jito_priority_fee_commission": 2000,
                "jito_priority_fee_lamports": 123,
                "stake": 456,
                "credits": 789,
                "inflation_rewards_collector": null,
                "inflation_rewards_commission_bps": 900,
                "inflation_rewards_commission_bps_is_v4": false,
                "block_revenue_collector": null,
                "block_revenue_commission_bps": null,
                "pending_delegator_rewards": null,
            })
        );
    }

    #[test]
    fn the_validator_meta_collection_json_shape_is_pinned() {
        let collection = ValidatorMetaCollection {
            epoch: 900,
            slot: 1000,
            capitalization: 5,
            epoch_duration_in_years: 0.5,
            validator_rate: 0.04,
            validator_rewards: 42,
            validator_metas: vec![validator_meta()],
            commission_vintage: VoteStateVintage::from_epoch_stakes(900),
            collector_vintage: VoteStateVintage::from_live_stakes_cache(900),
            commission_vintage_fallbacks: 3,
            features: SnapshotFeatures {
                block_revenue_custom_collector_active: true,
                inflation_rewards_custom_collector_active: Some(true),
                inflation_rewards_delay_commission_updates_active: Some(true),
                inflation_rewards_commission_rate_in_basis_points_active: None,
                inflation_rewards_validator_admission_ticket_active: Some(true),
            },
        };

        let mut value = serde_json::to_value(collection).unwrap();
        value.as_object_mut().unwrap().remove("validator_metas");
        assert_eq!(
            value,
            serde_json::json!({
                "epoch": 900,
                "slot": 1000,
                "capitalization": 5,
                "epoch_duration_in_years": 0.5,
                "validator_rate": 0.04,
                "validator_rewards": 42,
                "commission_vintage": {
                    "epoch_stakes_key": 900,
                    "captured_at_epoch": 899,
                },
                "collector_vintage": {
                    "epoch_stakes_key": null,
                    "captured_at_epoch": 901,
                },
                "commission_vintage_fallbacks": 3,
                "features": {
                    "block_revenue_custom_collector_active": true,
                    "inflation_rewards_custom_collector_active": true,
                    "inflation_rewards_delay_commission_updates_active": true,
                    "inflation_rewards_commission_rate_in_basis_points_active": null,
                    "inflation_rewards_validator_admission_ticket_active": true,
                },
            })
        );
    }

    #[test]
    fn a_validator_meta_round_trips_through_json() {
        let meta = validator_meta();
        let deserialized: ValidatorMeta =
            serde_json::from_str(&serde_json::to_string(&meta).unwrap()).unwrap();

        assert_eq!(meta, deserialized);
    }

    // A backfill or replay reads validators.json files written before SIMD-0185
    // was published at all; every one of them has to keep deserializing.
    #[test]
    fn a_pre_simd_0185_validators_json_still_deserializes() {
        let collection: ValidatorMetaCollection = serde_json::from_str(
            r#"{
                "epoch": 900,
                "slot": 1000,
                "capitalization": 5,
                "epoch_duration_in_years": 0.5,
                "validator_rate": 0.04,
                "validator_rewards": 42,
                "validator_metas": [
                    {
                        "vote_account": "US517G5965aydkZ46HS38QLi7UQiSojurfbQfKCELFx",
                        "commission": 7,
                        "mev_commission": 1000,
                        "jito_priority_fee_commission": 2000,
                        "jito_priority_fee_lamports": 123,
                        "stake": 456,
                        "credits": 789
                    }
                ]
            }"#,
        )
        .expect("an archived pre-SIMD-0185 validators.json must still be readable");

        assert_eq!(collection.epoch, 900);
        assert_eq!(collection.commission_vintage_fallbacks, 0);
        assert_eq!(collection.features, SnapshotFeatures::default());
        let meta = &collection.validator_metas[0];
        assert_eq!(meta.commission, 7);
        assert_eq!(meta.stake, 456);
        assert_eq!(meta.inflation_rewards_collector, None);
        assert_eq!(meta.inflation_rewards_commission_bps, None);
        assert_eq!(meta.inflation_rewards_commission_bps_is_v4, None);
        assert_eq!(meta.block_revenue_collector, None);
        assert_eq!(meta.block_revenue_commission_bps, None);
        assert_eq!(meta.pending_delegator_rewards, None);
    }
}
