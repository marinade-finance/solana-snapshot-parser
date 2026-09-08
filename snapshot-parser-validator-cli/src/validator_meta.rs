use crate::jito_priority_fee::fetch_jito_priority_fee_metas;
use {
    crate::jito_mev::fetch_jito_mev_metas,
    agave_feature_set::FeatureSnapshot,
    log::{info, warn},
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
    #[serde(default, with = "option_pubkey_string_conversion")]
    pub inflation_rewards_collector: Option<Pubkey>,
    // agave synthesizes commission * 100 on a pre-v4 vote state, so this is what it applies either way
    #[serde(default)]
    pub inflation_rewards_commission_bps: Option<u16>,
    #[serde(default)]
    pub inflation_rewards_commission_bps_is_v4: Option<bool>,
    // absent means agave has no collector to pay and credits the leader identity instead
    #[serde(default, with = "option_pubkey_string_conversion")]
    pub block_revenue_collector: Option<Pubkey>,
    // applied only while SnapshotFeatures::block_revenue_sharing_active
    #[serde(default)]
    pub block_revenue_commission_bps: Option<u16>,
    // the pot that same feature's payout divides by stake share
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

// agave keys Bank::epoch_stakes by leader schedule epoch, so epoch_stakes(E) is captured at the first slot of E-1
#[derive(Clone, Deserialize, Serialize, Debug, Default, Eq, PartialEq)]
pub struct VoteStateVintage {
    // None is the bank's live stakes cache, i.e. the state at ValidatorMetaCollection::slot
    pub epoch_stakes_key: Option<Epoch>,
    pub captured_at_epoch: Epoch,
}

impl VoteStateVintage {
    pub(crate) fn from_epoch_stakes(epoch_stakes_key: Epoch) -> Self {
        Self {
            epoch_stakes_key: Some(epoch_stakes_key),
            captured_at_epoch: epoch_stakes_key.saturating_sub(1),
        }
    }

    // agave snapshots this into epoch_stakes(epoch + 2), minus what SIMD-0357 admission filtering drops
    fn from_live_stakes_cache(epoch: Epoch) -> Self {
        Self {
            epoch_stakes_key: None,
            captured_at_epoch: epoch.saturating_add(1),
        }
    }
}

// Block revenue flags are exact (deposit_or_burn_fee reads the producing bank); inflation ones are this bank's, which the epoch+1 distribution bank can only add to
#[derive(Clone, Deserialize, Serialize, Debug, Default, Eq, PartialEq)]
pub struct SnapshotFeatures {
    // agave custom_commission_collector, SIMD-0232; while false the runtime credits the leader identity
    pub block_revenue_custom_collector_active: bool,
    // agave block_revenue_sharing, SIMD-0123; while false the whole non-burned deposit goes to the collector
    pub block_revenue_sharing_active: bool,
    // while false the runtime ignores inflation_rewards_collector and pays the vote account
    pub inflation_rewards_custom_collector_active: Option<bool>,
    // while false the runtime ignores commission_vintage and takes the commission from the state at slot
    pub inflation_rewards_delay_commission_updates_active: Option<bool>,
    // while false the runtime applies commission * 100 and ignores a v4 basis-point commission
    pub inflation_rewards_commission_rate_in_basis_points_active: Option<bool>,
    // while true agave pays only the accounts surviving admission filtering; this collection still rows every one
    pub inflation_rewards_validator_admission_ticket_active: Option<bool>,
}

impl SnapshotFeatures {
    // features never deactivate and epoch+1 activates first, so active proves active and inactive proves nothing
    fn on_the_distribution_bank(active_at_slot: bool) -> Option<bool> {
        active_at_slot.then_some(true)
    }

    fn from_feature_snapshot(features: &FeatureSnapshot) -> Self {
        Self {
            block_revenue_custom_collector_active: features.custom_commission_collector,
            block_revenue_sharing_active: features.block_revenue_sharing,
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
    // None means the file recorded none; epoch_stakes_key: None is itself a vintage, so Default would assert one
    #[serde(default)]
    pub commission_vintage: Option<VoteStateVintage>,
    #[serde(default)]
    pub collector_vintage: Option<VoteStateVintage>,
    // absent from the commission_vintage snapshot, so agave and this collection both read epoch_stakes(epoch + 1)
    #[serde(default)]
    pub commission_vintage_next_snapshot_fallbacks: usize,
    // absent from that snapshot and the next, leaving the state at slot, which agave falls back to as well
    #[serde(default)]
    pub commission_vintage_live_state_fallbacks: usize,
    // staked schedule-vintage members with no row here, whose vote_pubkey leader-schedule.json cannot join
    #[serde(default)]
    pub leader_schedule_vote_accounts_absent_at_slot: usize,
    #[serde(default)]
    pub features: SnapshotFeatures,
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
    commission_vintage_next_snapshot_fallbacks: usize,
    commission_vintage_live_state_fallbacks: usize,
    leader_schedule_vote_accounts_absent_at_slot: usize,
}

struct InflationRewardsCommission {
    bps: u16,
    is_v4: bool,
}

struct V4CollectorFields {
    inflation_rewards_collector: Pubkey,
    pending_delegator_rewards: u64,
}

struct V4BlockRevenueFields {
    block_revenue_collector: Pubkey,
    block_revenue_commission_bps: u16,
}

// the commission getter answers on a pre-v4 state too, so only the collector's absence tells the versions apart
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

// primary/fallback are agave's snapshot_epoch_vote_accounts and rewarded_epoch_vote_accounts; only primary answers block revenue
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

    // paired with the epoch_stakes key that answered, so the caller can tell it from vintage()
    fn commission_view(&self, vote_account: &Pubkey) -> Option<(&'a VoteStateView, Epoch)> {
        Self::lookup(self.primary, vote_account)
            .map(|view| (view, self.epoch))
            .or_else(|| {
                Self::lookup(self.fallback, vote_account)
                    .map(|view| (view, self.epoch.saturating_add(1)))
            })
    }

    // agave resolves this through epoch_stakes(epoch) alone, so absent there is absent
    fn block_revenue_view(&self, vote_account: &Pubkey) -> Option<&'a VoteStateView> {
        Self::lookup(self.primary, vote_account)
    }

    // LeaderSchedule::new drops unstaked accounts, so only staked members need a row to join against
    fn staked_primary_absent_from(&self, live_vote_accounts: &VoteAccountsHashMap) -> usize {
        self.primary
            .map(|primary| {
                primary
                    .iter()
                    .filter(|(vote_account, (stake, _vote_account))| {
                        *stake > 0 && !live_vote_accounts.contains_key(*vote_account)
                    })
                    .count()
            })
            .unwrap_or(0)
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
    let commission_vintage = commission_source.vintage();
    let mut commission_vintage_next_snapshot_fallbacks = 0;
    let mut commission_vintage_live_state_fallbacks = 0;
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

        let commission = match commission_source.commission_view(pubkey) {
            Some((view, epoch_stakes_key)) => {
                if commission_vintage.epoch_stakes_key != Some(epoch_stakes_key) {
                    commission_vintage_next_snapshot_fallbacks += 1;
                }
                inflation_rewards_commission(view)
            }
            None => {
                // no snapshot vintage means every account is read at slot, which is not a fallback
                if commission_vintage.epoch_stakes_key.is_some() {
                    commission_vintage_live_state_fallbacks += 1;
                }
                inflation_rewards_commission(vote_state_view)
            }
        };
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
        commission_vintage,
        commission_vintage_next_snapshot_fallbacks,
        commission_vintage_live_state_fallbacks,
        leader_schedule_vote_accounts_absent_at_slot: commission_source
            .staked_primary_absent_from(live_vote_accounts),
    }
}

// A skipped last slot leaves the snapshot at an earlier one; only crossing into the next epoch breaks the vintages
pub(crate) fn check_end_of_epoch_bank(
    slot: u64,
    last_slot_in_epoch: u64,
    epoch: Epoch,
) -> anyhow::Result<()> {
    if slot > last_slot_in_epoch {
        anyhow::bail!(
            "Bank is at slot {slot}, past the last slot {last_slot_in_epoch} of epoch {epoch}; the vote state vintages this collection records would not hold. Parse the end-of-epoch snapshot instead."
        );
    }
    if slot != last_slot_in_epoch {
        warn!(
            "Bank is at slot {slot}, short of the last slot {last_slot_in_epoch} of epoch {epoch}; credits and the collector vintage are read that many slots early"
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
        commission_vintage_next_snapshot_fallbacks,
        commission_vintage_live_state_fallbacks,
        leader_schedule_vote_accounts_absent_at_slot,
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
        "Commission vintage: {:?}, of {} vote accounts {} fall back to the next snapshot and {} to the state at slot",
        commission_vintage,
        total_validators,
        commission_vintage_next_snapshot_fallbacks,
        commission_vintage_live_state_fallbacks,
    );
    info!("Collector vintage: {:?}", collector_vintage);
    if leader_schedule_vote_accounts_absent_at_slot > 0 {
        warn!(
            "{} staked vote accounts of the leader schedule vintage hold no row here; leader-schedule.json can name a vote_pubkey this collection cannot answer",
            leader_schedule_vote_accounts_absent_at_slot
        );
    }
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
        commission_vintage: Some(commission_vintage),
        collector_vintage: Some(collector_vintage),
        commission_vintage_next_snapshot_fallbacks,
        commission_vintage_live_state_fallbacks,
        leader_schedule_vote_accounts_absent_at_slot,
        features,
    })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::utils::vote_account_fixture::staked_vote_accounts,
        agave_feature_set::FeatureSet,
        solana_vote_interface::state::{VoteStateV3, VoteStateV4, VoteStateVersions},
        std::collections::HashMap,
    };

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
        staked_vote_accounts(
            entries.map(|(pubkey, vote_state_versions)| (pubkey, vote_state_versions, 42)),
        )
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

    // the anti-rug snapshot: a mid-epoch conversion to v4 at 100% is still paid the pre-v4 vintage's commission
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
        assert_eq!(collection.commission_vintage_next_snapshot_fallbacks, 0);
        assert_eq!(collection.commission_vintage_live_state_fallbacks, 0);
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

    // agave reads it from epoch_stakes(rewarded_epoch), taken a full epoch before the rewarded one
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
        assert_eq!(collection.commission_vintage_next_snapshot_fallbacks, 0);
        assert_eq!(collection.commission_vintage_live_state_fallbacks, 0);
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
        assert_eq!(collection.commission_vintage_next_snapshot_fallbacks, 1);
        assert_eq!(
            collection.commission_vintage_live_state_fallbacks, 0,
            "the next snapshot answered, so nothing fell through to the state at slot"
        );
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
        assert_eq!(collection.commission_vintage_live_state_fallbacks, 1);
        assert_eq!(
            collection.commission_vintage_next_snapshot_fallbacks, 0,
            "no next snapshot exists to fall back to, so the two counters cannot both claim it"
        );
    }

    // a staked schedule-vintage member gone by the slot leads slots no row here can be joined to
    #[test]
    fn a_staked_schedule_member_gone_by_the_slot_is_counted_as_absent() {
        let live = vote_accounts([(VOTE_ACCOUNT, v4(9_000))]);
        let epoch_stakes = HashMap::from([(
            EPOCH,
            vote_accounts([(VOTE_ACCOUNT, v4(5_000)), (OTHER_VOTE_ACCOUNT, v4(5_000))]),
        )]);

        let collection = metas_from(&live, &epoch_stakes);

        assert_eq!(collection.leader_schedule_vote_accounts_absent_at_slot, 1);
        assert!(
            collection
                .metas
                .iter()
                .all(|meta| meta.vote_account != OTHER_VOTE_ACCOUNT),
            "the absent account is the one with no meta row"
        );
    }

    #[test]
    fn an_unstaked_schedule_member_gone_by_the_slot_is_not_counted() {
        let live = vote_accounts([(VOTE_ACCOUNT, v4(9_000))]);
        let mut vintage = vote_accounts([(VOTE_ACCOUNT, v4(5_000))]);
        let (_, (_, unstaked)) = vote_accounts([(OTHER_VOTE_ACCOUNT, v4(5_000))])
            .into_iter()
            .next()
            .unwrap();
        vintage.insert(OTHER_VOTE_ACCOUNT, (0, unstaked));

        let collection = metas_from(&live, &HashMap::from([(EPOCH, vintage)]));

        assert_eq!(
            collection.leader_schedule_vote_accounts_absent_at_slot, 0,
            "LeaderSchedule::new drops an unstaked account, so it can never lead a slot"
        );
    }

    #[test]
    fn a_schedule_vintage_fully_present_at_the_slot_counts_none_absent() {
        let live = vote_accounts([(VOTE_ACCOUNT, v4(9_000)), (OTHER_VOTE_ACCOUNT, v4(9_000))]);
        let epoch_stakes = HashMap::from([(
            EPOCH,
            vote_accounts([(VOTE_ACCOUNT, v4(5_000)), (OTHER_VOTE_ACCOUNT, v4(5_000))]),
        )]);

        let collection = metas_from(&live, &epoch_stakes);

        assert_eq!(collection.leader_schedule_vote_accounts_absent_at_slot, 0);
    }

    // agave takes this from epoch_stakes(epoch) with no fallback, so no other vintage may leak in
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

    // a collection cannot both name the next snapshot as its vintage and count accounts missing from it
    #[test]
    fn a_missing_snapshot_vintage_makes_the_next_one_the_vintage_and_counts_no_fallback() {
        let live = vote_accounts([(VOTE_ACCOUNT, v4(9_000))]);
        let epoch_stakes = HashMap::from([(EPOCH + 1, vote_accounts([(VOTE_ACCOUNT, v4(6_000))]))]);

        let collection = metas_from(&live, &epoch_stakes);

        assert_eq!(
            collection.commission_vintage,
            VoteStateVintage::from_epoch_stakes(EPOCH + 1)
        );
        assert_eq!(collection.commission_vintage_next_snapshot_fallbacks, 0);
        assert_eq!(collection.commission_vintage_live_state_fallbacks, 0);
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
        assert_eq!(
            collection.commission_vintage_next_snapshot_fallbacks, 0,
            "with no snapshot to fall back from, neither counter can fire"
        );
        assert_eq!(collection.commission_vintage_live_state_fallbacks, 0);
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

    // each flag must name the feature governing it, not a neighbour
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
                block_revenue_sharing_active: false,
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
                block_revenue_sharing_active: false,
                inflation_rewards_custom_collector_active: Some(true),
                inflation_rewards_delay_commission_updates_active: Some(true),
                inflation_rewards_commission_rate_in_basis_points_active: Some(true),
                inflation_rewards_validator_admission_ticket_active: Some(true),
            }
        );

        features.block_revenue_sharing = true;

        assert_eq!(
            SnapshotFeatures::from_feature_snapshot(&features),
            SnapshotFeatures {
                block_revenue_custom_collector_active: true,
                block_revenue_sharing_active: true,
                inflation_rewards_custom_collector_active: Some(true),
                inflation_rewards_delay_commission_updates_active: Some(true),
                inflation_rewards_commission_rate_in_basis_points_active: Some(true),
                inflation_rewards_validator_admission_ticket_active: Some(true),
            },
            "block_revenue_sharing gates the commission split and nothing else"
        );
    }

    // SIMD-0232 redirects the deposit and SIMD-0123 splits it, so neither flag stands in for the other
    #[test]
    fn the_block_revenue_split_is_not_reported_by_the_collector_flag() {
        let mut features = FeatureSet::default().snapshot().clone();
        features.block_revenue_sharing = true;

        let published = SnapshotFeatures::from_feature_snapshot(&features);

        assert!(published.block_revenue_sharing_active);
        assert!(!published.block_revenue_custom_collector_active);
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

    // rejecting a skipped last slot would cost the epoch every collection, not just those slots' credits
    #[test]
    fn a_bank_short_of_the_last_slot_of_its_epoch_is_accepted() {
        check_end_of_epoch_bank(433_295_997, 433_295_999, 1002).unwrap();
        check_end_of_epoch_bank(433_000_000, 433_295_999, 1002).unwrap();
    }

    #[test]
    fn a_bank_past_the_last_slot_of_its_epoch_is_rejected() {
        let err = check_end_of_epoch_bank(433_296_000, 433_295_999, 1002)
            .expect_err("a bank in the next epoch would mislabel every recorded vintage");

        assert!(
            err.to_string().contains("433295999"),
            "the error must name the last slot of the epoch it was handed: {err}"
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

    // ds-sam and validator-bonds settle real SOL against this, so a rename must break here, not downstream
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
            commission_vintage: Some(VoteStateVintage::from_epoch_stakes(900)),
            collector_vintage: Some(VoteStateVintage::from_live_stakes_cache(900)),
            commission_vintage_next_snapshot_fallbacks: 3,
            commission_vintage_live_state_fallbacks: 1,
            leader_schedule_vote_accounts_absent_at_slot: 2,
            features: SnapshotFeatures {
                block_revenue_custom_collector_active: true,
                block_revenue_sharing_active: false,
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
                "commission_vintage_next_snapshot_fallbacks": 3,
                "commission_vintage_live_state_fallbacks": 1,
                "leader_schedule_vote_accounts_absent_at_slot": 2,
                "features": {
                    "block_revenue_custom_collector_active": true,
                    "block_revenue_sharing_active": false,
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

    // a backfill reads files written before SIMD-0185 was published, and every one must keep deserializing
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
        assert_eq!(
            (collection.commission_vintage, collection.collector_vintage),
            (None, None),
            "a file that recorded no vintage must not claim one"
        );
        assert_eq!(collection.commission_vintage_next_snapshot_fallbacks, 0);
        assert_eq!(collection.commission_vintage_live_state_fallbacks, 0);
        assert_eq!(collection.leader_schedule_vote_accounts_absent_at_slot, 0);
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
