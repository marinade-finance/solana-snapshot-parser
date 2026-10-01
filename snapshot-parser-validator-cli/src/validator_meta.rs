use crate::jito_priority_fee::fetch_jito_priority_fee_metas;
use {
    crate::{
        epoch_inflation_account::{
            epoch_inflation_account, epoch_inflation_account_address, EpochInflationAccount,
        },
        inflation_rewards_points::{
            alpenglow_epoch, points_by_vote_account, AlpenglowEpochType, AG_MIGRATION_EPOCH_CREDIT,
        },
        jito_mev::fetch_jito_mev_metas,
    },
    agave_feature_set::FeatureSnapshot,
    log::{info, warn},
    serde::{Deserialize, Serialize},
    snapshot_parser::serde_serialize::option_epoch_credits_string_conversion,
    snapshot_parser::serde_serialize::option_pubkey_string_conversion,
    snapshot_parser::serde_serialize::option_u128_string_conversion,
    snapshot_parser::serde_serialize::pubkey_string_conversion,
    snapshot_parser::stake_activation::StakeActivation,
    snapshot_parser::utils::lamports_to_sol,
    solana_program::{clock::Slot, pubkey::Pubkey},
    solana_runtime::{
        bank::{Bank, DEFAULT_VAT_TO_BURN_PER_EPOCH, MAX_ALPENGLOW_VOTE_ACCOUNTS},
        epoch_stakes::BLSPubkeyToRankMap,
        slot_params::slot_time_feature_gates,
    },
    solana_sdk::{account::AccountSharedData, epoch_info::EpochInfo},
    solana_stake_interface::stake_history::Epoch,
    solana_vote::{
        vote_account::{VoteAccount, VoteAccountsHashMap},
        vote_state_view::VoteStateView,
    },
    solana_vote_interface::state::VoteStateV4,
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
    // null from the migration epoch on, whose Tower and lamport parts are never one unit
    pub credits: Option<u64>,
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
    // false is a refusal the E+1 distribution bank must repeat, null one only its stake vintage decides
    #[serde(default)]
    pub inflation_rewards_admitted: Option<bool>,
    // agave's Tower points over all delegations, missed epochs too; null after the migration epoch
    #[serde(default, with = "option_u128_string_conversion")]
    pub inflation_rewards_points: Option<u128>,
    #[serde(default)]
    pub tower_credits: Option<u64>,
    #[serde(default)]
    pub alpenglow_credits: Option<u64>,
    // the state at slot verbatim, the (MAX, MAX, MAX) migration marker included, as strings past 2^53
    #[serde(default, with = "option_epoch_credits_string_conversion")]
    pub epoch_credits: Option<Vec<(Epoch, u64, u64)>>,
    // stake in epoch_stakes(epoch), the Alpenglow reward committee; None is not a member
    #[serde(default)]
    pub epoch_stake: Option<u64>,
    // set only while alpenglow is active: agave's BLSPubkeyToRankMap panics on a keyless committee
    #[serde(default)]
    pub epoch_stake_rank: Option<u16>,
    #[serde(default)]
    pub epoch_stake_bls_pubkey: Option<String>,
    #[serde(default, with = "option_pubkey_string_conversion")]
    pub epoch_stake_node_pubkey: Option<Pubkey>,
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
    #[serde(default)]
    pub alpenglow_active: Option<bool>,
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
            alpenglow_active: Self::on_the_distribution_bank(features.alpenglow),
        }
    }

    fn admission_filter_active(&self) -> bool {
        self.inflation_rewards_validator_admission_ticket_active
            .unwrap_or(false)
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
    // staked rows absent from the commission_vintage snapshot, so agave and this collection both read epoch_stakes(epoch + 1)
    #[serde(default)]
    pub commission_vintage_next_snapshot_fallbacks: usize,
    // staked rows absent from that snapshot and the next, leaving the state at slot, which agave falls back to as well
    #[serde(default)]
    pub commission_vintage_live_state_fallbacks: usize,
    // staked schedule-vintage members with no row here, whose vote_pubkey leader-schedule.json cannot join
    #[serde(default)]
    pub leader_schedule_vote_accounts_absent_at_slot: usize,
    // staked rows SIMD-0357 refused at slot, whose commission is likely unearned rather than burned
    #[serde(default)]
    pub inflation_rewards_unadmitted_at_slot: usize,
    #[serde(default)]
    pub features: SnapshotFeatures,
    #[serde(default)]
    pub alpenglow_epoch_type: Option<AlpenglowEpochType>,
    #[serde(default)]
    pub alpenglow_migration_slot: Option<Slot>,
    #[serde(default)]
    pub epoch_total_stake: Option<u64>,
    #[serde(flatten)]
    pub vat_config: VatConfig,
    // the M and N of the Alpenglow vote reward M * s / (N * S), for this epoch and the one before
    #[serde(default)]
    pub epoch_inflation_account: Option<EpochInflationAccount>,
}

// the bank's values at slot; the E+1 boundary filter applies the next bank's, which a feature can move
#[derive(Clone, Deserialize, Serialize, Debug, Default, Eq, PartialEq)]
pub struct VatConfig {
    #[serde(default)]
    pub minimum_vote_account_balance_for_vat: Option<u64>,
    #[serde(default)]
    pub vat_lamports_per_epoch: Option<u64>,
    #[serde(default)]
    pub max_alpenglow_vote_accounts: Option<u64>,
}

fn vat_lamports_per_epoch_by_slot_time() -> Vec<u64> {
    std::iter::once(DEFAULT_VAT_TO_BURN_PER_EPOCH)
        .chain(slot_time_feature_gates().map(|(_, params)| params.vat_to_burn_per_epoch()))
        .collect()
}

// agave's own term of the VAT minimum, which get_minimum_balance_for_rent_exemption lifts to at least 1
fn vote_account_rent_exempt_minimum(bank: &Bank) -> u64 {
    bank.rent_collector()
        .rent
        .minimum_balance(VoteStateV4::size_of())
}

impl VatConfig {
    fn of(bank: &Bank) -> anyhow::Result<Self> {
        let features = bank.feature_set.snapshot();
        if !features.validator_admission_ticket {
            return Ok(Self::default());
        }
        let minimum_vote_account_balance_for_vat = bank.minimum_vote_account_balance_for_vat();
        let vat_lamports_per_epoch = if features.alpenglow {
            // vat_to_burn_per_epoch is crate-private, so it is taken back out of the minimum it is added to
            let vat_lamports_per_epoch = minimum_vote_account_balance_for_vat
                .saturating_sub(vote_account_rent_exempt_minimum(bank));
            let known = vat_lamports_per_epoch_by_slot_time();
            anyhow::ensure!(
                known.contains(&vat_lamports_per_epoch),
                "VAT burn of {vat_lamports_per_epoch} lamports per epoch is none of agave's {known:?}; minimum_vote_account_balance_for_vat is no longer rent plus the burn"
            );
            Some(vat_lamports_per_epoch)
        } else {
            None
        };
        Ok(Self {
            minimum_vote_account_balance_for_vat: Some(minimum_vote_account_balance_for_vat),
            vat_lamports_per_epoch,
            max_alpenglow_vote_accounts: Some(MAX_ALPENGLOW_VOTE_ACCOUNTS as u64),
        })
    }
}

struct VoteAccountMeta {
    vote_account: Pubkey,
    commission: u8,
    stake: u64,
    credits: EpochCredits,
    epoch_credits: Vec<(Epoch, u64, u64)>,
    committee: CommitteeFields,
    inflation_rewards_collector: Option<Pubkey>,
    inflation_rewards_commission_bps: u16,
    inflation_rewards_commission_bps_is_v4: bool,
    block_revenue_collector: Option<Pubkey>,
    block_revenue_commission_bps: Option<u16>,
    pending_delegator_rewards: Option<u64>,
    inflation_rewards_admitted: Option<bool>,
}

struct VoteAccountMetaCollection {
    metas: Vec<VoteAccountMeta>,
    commission_vintage: VoteStateVintage,
    commission_vintage_next_snapshot_fallbacks: usize,
    commission_vintage_live_state_fallbacks: usize,
    leader_schedule_vote_accounts_absent_at_slot: usize,
    inflation_rewards_unadmitted_at_slot: usize,
}

#[derive(Clone, Copy)]
struct EpochCredits {
    tower: Option<u64>,
    alpenglow: Option<u64>,
}

impl EpochCredits {
    // the migration epoch holds a Tower entry and an Alpenglow entry for the epoch, so credits() is not its delta
    fn of(epoch_credits: &[(Epoch, u64, u64)], epoch: Epoch, regime: AlpenglowEpochType) -> Self {
        // the marker scrolls off the 64-entry history, and an account idle through the migration never gets one
        let mut after_marker = regime == AlpenglowEpochType::Alpenglow;
        let mut credits = Self {
            tower: None,
            alpenglow: None,
        };
        for &entry in epoch_credits {
            if entry == AG_MIGRATION_EPOCH_CREDIT {
                after_marker = true;
                continue;
            }
            let (entry_epoch, final_credits, initial_credits) = entry;
            if entry_epoch == epoch {
                let part = if after_marker {
                    &mut credits.alpenglow
                } else {
                    &mut credits.tower
                };
                part.get_or_insert(final_credits - initial_credits);
            }
        }
        credits
    }

    fn published(&self, regime: AlpenglowEpochType) -> Option<u64> {
        (regime == AlpenglowEpochType::Tower).then(|| self.tower.unwrap_or(0))
    }
}

#[derive(Default)]
struct CommitteeFields {
    epoch_stake: Option<u64>,
    epoch_stake_rank: Option<u16>,
    epoch_stake_bls_pubkey: Option<String>,
    epoch_stake_node_pubkey: Option<Pubkey>,
}

impl CommitteeFields {
    // read off the committee's own vote state, never the live one
    fn of(committee_entry: Option<&(u64, VoteAccount)>, rank: Option<u16>) -> Self {
        let Some((stake, account)) = committee_entry.filter(|(stake, _)| *stake > 0) else {
            return Self::default();
        };
        let vote_state_view = account.vote_state_view();
        Self {
            epoch_stake: Some(*stake),
            epoch_stake_rank: rank,
            epoch_stake_bls_pubkey: vote_state_view
                .bls_pubkey_compressed()
                .map(|key| bs58::encode(key).into_string()),
            epoch_stake_node_pubkey: Some(*vote_state_view.node_pubkey()),
        }
    }
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

    fn committee_entry(&self, vote_account: &Pubkey) -> Option<&'a (u64, VoteAccount)> {
        self.primary.and_then(|primary| primary.get(vote_account))
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

// agave re-runs clone_and_filter_for_vat on the E+1 activated stakes; refresh_vote_accounts hands
// it these same vote accounts with only the stake replaced, so the BLS key is the one criterion of
// the three that reads what this bank already holds. Stake, the 2000-account cutoff and the balance
// threshold are all the distribution bank's own, the last because a boundary feature activation moves it
struct AdmissionFilter<'a> {
    admitted: &'a VoteAccountsHashMap,
}

impl AdmissionFilter<'_> {
    fn verdict(&self, vote_account: &Pubkey, account: &VoteAccount) -> Option<bool> {
        if self.admitted.contains_key(vote_account) {
            return Some(true);
        }
        account
            .vote_state_view()
            .bls_pubkey_compressed()
            .is_none()
            .then_some(false)
    }
}

fn fetch_vote_account_metas<'a>(
    live_vote_accounts: &VoteAccountsHashMap,
    epoch_vote_accounts: impl Fn(Epoch) -> Option<&'a VoteAccountsHashMap>,
    admission: Option<AdmissionFilter<'_>>,
    epoch: Epoch,
    regime: AlpenglowEpochType,
    rank_map: Option<&BLSPubkeyToRankMap>,
) -> VoteAccountMetaCollection {
    let commission_source = CommissionVintageSource::new(epoch_vote_accounts, epoch);
    let commission_vintage = commission_source.vintage();
    let mut commission_vintage_next_snapshot_fallbacks = 0;
    let mut commission_vintage_live_state_fallbacks = 0;
    let mut inflation_rewards_unadmitted_at_slot = 0;
    let mut metas = Vec::with_capacity(live_vote_accounts.len());

    for (pubkey, (stake, vote_account)) in live_vote_accounts.iter() {
        let vote_state_view = vote_account.vote_state_view();
        let epoch_credits: Vec<(Epoch, u64, u64)> = vote_state_view
            .epoch_credits_iter()
            .map(Into::into)
            .collect();
        let credits = EpochCredits::of(&epoch_credits, epoch, regime);

        // both counters are of the staked population: SIMD-0357 filtering keeps every unstaked
        // account out of either snapshot, and the payout applies no commission of theirs anyway
        let commission = match commission_source.commission_view(pubkey) {
            Some((view, epoch_stakes_key)) => {
                if *stake > 0 && commission_vintage.epoch_stakes_key != Some(epoch_stakes_key) {
                    commission_vintage_next_snapshot_fallbacks += 1;
                }
                inflation_rewards_commission(view)
            }
            None => {
                // no snapshot vintage means every account is read at slot, which is not a fallback
                if *stake > 0 && commission_vintage.epoch_stakes_key.is_some() {
                    commission_vintage_live_state_fallbacks += 1;
                }
                inflation_rewards_commission(vote_state_view)
            }
        };
        let collector_fields = v4_collector_fields(vote_state_view);
        let block_revenue_fields = commission_source
            .block_revenue_view(pubkey)
            .and_then(v4_block_revenue_fields);
        let inflation_rewards_admitted = admission
            .as_ref()
            .and_then(|admission| admission.verdict(pubkey, vote_account));
        // an unstaked refusal moves no number downstream: no points, no rows, no commission to burn
        if inflation_rewards_admitted == Some(false) && *stake > 0 {
            inflation_rewards_unadmitted_at_slot += 1;
        }

        metas.push(VoteAccountMeta {
            vote_account: *pubkey,
            commission: vote_state_view.commission(),
            stake: *stake,
            credits,
            epoch_credits,
            committee: CommitteeFields::of(
                commission_source.committee_entry(pubkey),
                rank_map.and_then(|rank_map| rank_map.get_rank_for_vote_pubkey(pubkey).copied()),
            ),
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
            inflation_rewards_admitted,
        });
    }

    VoteAccountMetaCollection {
        metas,
        commission_vintage,
        commission_vintage_next_snapshot_fallbacks,
        commission_vintage_live_state_fallbacks,
        leader_schedule_vote_accounts_absent_at_slot: commission_source
            .staked_primary_absent_from(live_vote_accounts),
        inflation_rewards_unadmitted_at_slot,
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
    stake_accounts: &[(Pubkey, AccountSharedData)],
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

    let (alpenglow_epoch_type, alpenglow_migration_slot) = alpenglow_epoch(bank);
    let live_vote_accounts = bank.vote_accounts();
    let features = SnapshotFeatures::from_feature_snapshot(bank.feature_set.snapshot());
    let epoch_stakes = bank
        .epoch_stakes(epoch)
        .ok_or_else(|| anyhow::anyhow!("Bank holds no epoch_stakes for its own epoch {epoch}"))?;
    let rank_map = features
        .alpenglow_active
        .unwrap_or(false)
        .then(|| epoch_stakes.bls_pubkey_to_rank_map());
    let vat_config = VatConfig::of(bank)?;
    // anyone can prefund the address before activation, which agave reads as no account either
    let epoch_inflation_account = if features.alpenglow_active == Some(true) {
        Some(epoch_inflation_account(bank)?.ok_or_else(|| {
            anyhow::anyhow!(
                "Alpenglow is active at slot {absolute_slot}, yet the bank holds no epoch inflation account at {}, which agave writes at every epoch start",
                epoch_inflation_account_address()
            )
        })?)
    } else {
        None
    };
    let admitted_stakes = features
        .admission_filter_active()
        .then(|| bank.get_top_epoch_stakes());
    let VoteAccountMetaCollection {
        metas: vote_account_metas,
        commission_vintage,
        commission_vintage_next_snapshot_fallbacks,
        commission_vintage_live_state_fallbacks,
        leader_schedule_vote_accounts_absent_at_slot,
        inflation_rewards_unadmitted_at_slot,
    } = fetch_vote_account_metas(
        &live_vote_accounts,
        |epoch| bank.epoch_vote_accounts(epoch),
        admitted_stakes.as_ref().map(|stakes| AdmissionFilter {
            admitted: stakes.vote_accounts().as_ref(),
        }),
        epoch,
        alpenglow_epoch_type,
        rank_map.map(|rank_map| rank_map.as_ref()),
    );
    let collector_vintage = VoteStateVintage::from_live_stakes_cache(epoch);
    let inflation_rewards_points = if alpenglow_epoch_type.pays_tower_points() {
        Some(points_by_vote_account(
            stake_accounts,
            &live_vote_accounts,
            &StakeActivation::on_the_distribution_bank(bank)?,
        ))
    } else {
        None
    };
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
                credits: vote_account_meta.credits.published(alpenglow_epoch_type),
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
                inflation_rewards_admitted: vote_account_meta.inflation_rewards_admitted,
                inflation_rewards_points: inflation_rewards_points.as_ref().map(|points| {
                    points
                        .get(&vote_account_meta.vote_account)
                        .copied()
                        .unwrap_or(0)
                }),
                tower_credits: vote_account_meta.credits.tower,
                alpenglow_credits: vote_account_meta.credits.alpenglow,
                epoch_credits: Some(vote_account_meta.epoch_credits),
                epoch_stake: vote_account_meta.committee.epoch_stake,
                epoch_stake_rank: vote_account_meta.committee.epoch_stake_rank,
                epoch_stake_bls_pubkey: vote_account_meta.committee.epoch_stake_bls_pubkey,
                epoch_stake_node_pubkey: vote_account_meta.committee.epoch_stake_node_pubkey,
            }
        })
        .collect::<Vec<_>>();

    let total_validators = validator_metas.len();
    // a migration late in the epoch can land its Tower credits before any Alpenglow reward
    let validators_with_credits = validator_metas
        .iter()
        .filter(|v| {
            v.tower_credits.is_some_and(|credits| credits > 0)
                || v.alpenglow_credits.is_some_and(|credits| credits > 0)
        })
        .count();
    let total_tower_credits: u64 = validator_metas.iter().filter_map(|v| v.tower_credits).sum();
    let total_alpenglow_credits: u64 = validator_metas
        .iter()
        .filter_map(|v| v.alpenglow_credits)
        .sum();
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
    info!(
        "Total credits: {} Tower, {} Alpenglow lamports",
        total_tower_credits, total_alpenglow_credits
    );
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
    match &inflation_rewards_points {
        Some(points) => info!(
            "Inflation rewards points: {} over {} vote accounts",
            points.values().sum::<u128>(),
            points.len()
        ),
        None => warn!(
            "Alpenglow migrated before epoch {epoch}, so it is not paid by Tower points and none are published"
        ),
    }
    if leader_schedule_vote_accounts_absent_at_slot > 0 {
        warn!(
            "{} staked vote accounts of the leader schedule vintage hold no row here; leader-schedule.json can name a vote_pubkey this collection cannot answer",
            leader_schedule_vote_accounts_absent_at_slot
        );
    }
    if inflation_rewards_unadmitted_at_slot > 0 {
        warn!(
            "{} staked vote accounts carry no BLS key, which SIMD-0357 admission needs and the E+1 distribution bank reads off this same state; agave pays them and their delegators nothing, so a consumer reading their commission as burned invents it",
            inflation_rewards_unadmitted_at_slot
        );
    }
    info!("Snapshot features: {:?}", features);
    info!(
        "Alpenglow epoch type: {:?}, migration slot: {:?}",
        alpenglow_epoch_type, alpenglow_migration_slot
    );
    info!("VAT config: {:?}", vat_config);
    info!("Epoch inflation account: {:?}", epoch_inflation_account);

    if validators_with_credits == 0 {
        anyhow::bail!(
            "No validator earned credits in epoch {}. This likely indicates a problem with the snapshot data.",
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
        inflation_rewards_unadmitted_at_slot,
        features,
        alpenglow_epoch_type: Some(alpenglow_epoch_type),
        alpenglow_migration_slot,
        epoch_total_stake: Some(epoch_stakes.total_stake()),
        vat_config,
        epoch_inflation_account,
    })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::epoch_inflation_account::EpochInflationState,
        crate::utils::vote_account_fixture::{
            one_validator_genesis, set_epoch_credits, set_genesis_certificate, staked_vote_accounts,
        },
        agave_feature_set::FeatureSet,
        solana_runtime::{
            bank_forks::BankForks,
            genesis_utils::{
                activate_alpenglow_at_genesis, activate_feature,
                create_genesis_config_with_vote_accounts, deactivate_features, GenesisConfigInfo,
                ValidatorVoteKeypairs,
            },
            slot_params::slot_time_feature_ids,
        },
        solana_vote_interface::state::{
            VoteStateV3, VoteStateV4, VoteStateVersions, BLS_PUBLIC_KEY_COMPRESSED_SIZE,
        },
        std::{collections::HashMap, sync::RwLock},
    };

    const EPOCH: Epoch = 900;
    const INFLATION_REWARDS_COLLECTOR: Pubkey = Pubkey::new_from_array([1u8; 32]);
    const BLOCK_REVENUE_COLLECTOR: Pubkey = Pubkey::new_from_array([2u8; 32]);
    const VOTE_ACCOUNT: Pubkey = Pubkey::new_from_array([7u8; 32]);
    const OTHER_VOTE_ACCOUNT: Pubkey = Pubkey::new_from_array([8u8; 32]);
    const NODE: Pubkey = Pubkey::new_from_array([3u8; 32]);

    fn v3(commission: u8) -> VoteStateVersions {
        VoteStateVersions::new_v3(VoteStateV3 {
            commission,
            ..VoteStateV3::default()
        })
    }

    fn v4_state(inflation_rewards_commission_bps: u16) -> VoteStateV4 {
        VoteStateV4 {
            inflation_rewards_collector: INFLATION_REWARDS_COLLECTOR,
            block_revenue_collector: BLOCK_REVENUE_COLLECTOR,
            inflation_rewards_commission_bps,
            block_revenue_commission_bps: 1234,
            pending_delegator_rewards: 987_654_321,
            ..VoteStateV4::default()
        }
    }

    fn v4(inflation_rewards_commission_bps: u16) -> VoteStateVersions {
        VoteStateVersions::new_v4(v4_state(inflation_rewards_commission_bps))
    }

    fn v4_with_bls_key(inflation_rewards_commission_bps: u16) -> VoteStateVersions {
        VoteStateVersions::new_v4(VoteStateV4 {
            bls_pubkey_compressed: Some([9u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE]),
            ..v4_state(inflation_rewards_commission_bps)
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
        fetch_vote_account_metas(
            live_vote_accounts,
            |epoch| epoch_stakes.get(&epoch),
            None,
            EPOCH,
            AlpenglowEpochType::Tower,
            None,
        )
    }

    fn metas_admitting(
        live_vote_accounts: &VoteAccountsHashMap,
        admitted: &VoteAccountsHashMap,
    ) -> VoteAccountMetaCollection {
        fetch_vote_account_metas(
            live_vote_accounts,
            |_| None,
            Some(AdmissionFilter { admitted }),
            EPOCH,
            AlpenglowEpochType::Tower,
            None,
        )
    }

    fn features_admitting(active: Option<bool>) -> SnapshotFeatures {
        SnapshotFeatures {
            inflation_rewards_validator_admission_ticket_active: active,
            ..SnapshotFeatures::default()
        }
    }

    #[test]
    fn an_admitted_vote_account_is_marked_admitted_and_counted_nowhere() {
        let live = vote_accounts([(VOTE_ACCOUNT, v4(700))]);
        let admitted = vote_accounts([(VOTE_ACCOUNT, v4(700))]);

        let collection = metas_admitting(&live, &admitted);

        assert_eq!(
            meta_of(&collection, &VOTE_ACCOUNT).inflation_rewards_admitted,
            Some(true)
        );
        assert_eq!(collection.inflation_rewards_unadmitted_at_slot, 0);
    }

    #[test]
    fn a_staked_vote_account_holding_no_bls_key_is_marked_unadmitted_and_counted() {
        let live = vote_accounts([(VOTE_ACCOUNT, v4(700)), (OTHER_VOTE_ACCOUNT, v4(500))]);
        let admitted = vote_accounts([(VOTE_ACCOUNT, v4(700))]);

        let collection = metas_admitting(&live, &admitted);

        assert_eq!(
            meta_of(&collection, &OTHER_VOTE_ACCOUNT).inflation_rewards_admitted,
            Some(false)
        );
        assert_eq!(collection.inflation_rewards_unadmitted_at_slot, 1);
    }

    #[test]
    fn an_unstaked_vote_account_holding_no_bls_key_is_marked_unadmitted_and_not_counted() {
        let live = staked_vote_accounts([(VOTE_ACCOUNT, v4(700), 0)]);
        let admitted = vote_accounts([(OTHER_VOTE_ACCOUNT, v4(500))]);

        let collection = metas_admitting(&live, &admitted);

        assert_eq!(
            meta_of(&collection, &VOTE_ACCOUNT).inflation_rewards_admitted,
            Some(false)
        );
        assert_eq!(collection.inflation_rewards_unadmitted_at_slot, 0);
    }

    // the epoch 1030 defect: a validator that starts voting mid-epoch holds no stake here and its
    // delegations activate on the very bank that filters, which admits it and credits it a Voting row
    #[test]
    fn an_unstaked_vote_account_the_stake_criterion_alone_drops_records_no_verdict() {
        let live = staked_vote_accounts([(VOTE_ACCOUNT, v4_with_bls_key(700), 0)]);
        let admitted = vote_accounts([(OTHER_VOTE_ACCOUNT, v4(500))]);

        let collection = metas_admitting(&live, &admitted);

        assert_eq!(
            meta_of(&collection, &VOTE_ACCOUNT).inflation_rewards_admitted,
            None
        );
        assert_eq!(collection.inflation_rewards_unadmitted_at_slot, 0);
    }

    #[test]
    fn a_staked_vote_account_the_cutoff_alone_drops_records_no_verdict() {
        let live = vote_accounts([
            (VOTE_ACCOUNT, v4_with_bls_key(700)),
            (OTHER_VOTE_ACCOUNT, v4_with_bls_key(500)),
        ]);
        let admitted = vote_accounts([(VOTE_ACCOUNT, v4_with_bls_key(700))]);

        let collection = metas_admitting(&live, &admitted);

        assert_eq!(
            meta_of(&collection, &OTHER_VOTE_ACCOUNT).inflation_rewards_admitted,
            None,
            "the E+1 stakes rank the cutoff, so this bank cannot say the payout refused it"
        );
        assert_eq!(collection.inflation_rewards_unadmitted_at_slot, 0);
    }

    #[test]
    fn an_epoch_whose_filter_was_inactive_records_no_admission_either_way() {
        let live = vote_accounts([(VOTE_ACCOUNT, v4(700))]);

        let collection = metas_from(&live, &HashMap::new());

        assert_eq!(
            meta_of(&collection, &VOTE_ACCOUNT).inflation_rewards_admitted,
            None
        );
        assert_eq!(collection.inflation_rewards_unadmitted_at_slot, 0);
    }

    #[test]
    fn the_bank_filter_runs_only_where_the_snapshot_proves_simd_0357_active() {
        assert!(features_admitting(Some(true)).admission_filter_active());
    }

    #[test]
    fn an_unproven_admission_flag_leaves_the_bank_filter_off() {
        // get_top_epoch_stakes hands back every stake unfiltered while the feature is off, which would publish a blanket true
        assert!(!features_admitting(None).admission_filter_active());
        assert!(!features_admitting(Some(false)).admission_filter_active());
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

    // SIMD-0357 filtering leaves thousands of these outside both snapshots every epoch
    #[test]
    fn an_unstaked_vote_account_no_snapshot_carries_is_resolved_at_the_slot_and_counted_nowhere() {
        let live = staked_vote_accounts([(VOTE_ACCOUNT, v4(9_000), 0)]);
        let epoch_stakes =
            HashMap::from([(EPOCH, vote_accounts([(OTHER_VOTE_ACCOUNT, v4(5_000))]))]);

        let collection = metas_from(&live, &epoch_stakes);

        assert_eq!(
            meta_of(&collection, &VOTE_ACCOUNT).inflation_rewards_commission_bps,
            9_000,
            "the commission still comes from where agave would read it"
        );
        assert_eq!(collection.commission_vintage_live_state_fallbacks, 0);
        assert_eq!(collection.commission_vintage_next_snapshot_fallbacks, 0);
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

    const MARKER: (Epoch, u64, u64) = AG_MIGRATION_EPOCH_CREDIT;

    fn credits_in(
        regime: AlpenglowEpochType,
        epoch_credits: Vec<(Epoch, u64, u64)>,
    ) -> (Option<u64>, EpochCredits) {
        let versions = VoteStateVersions::new_v4(VoteStateV4 {
            epoch_credits,
            ..v4_state(700)
        });
        let live = vote_accounts([(VOTE_ACCOUNT, versions)]);
        let collection = fetch_vote_account_metas(&live, |_| None, None, EPOCH, regime, None);
        let credits = meta_of(&collection, &VOTE_ACCOUNT).credits;
        (credits.published(regime), credits)
    }

    fn bls_key() -> [u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE] {
        ValidatorVoteKeypairs::new_rand()
            .bls_keypair
            .public
            .to_bytes_compressed()
    }

    // agave's rank map drops a node pubkey held twice, so every member gets its own
    fn committee_member(
        bls_pubkey_compressed: Option<[u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE]>,
    ) -> VoteStateVersions {
        VoteStateVersions::new_v4(VoteStateV4 {
            node_pubkey: Pubkey::new_unique(),
            bls_pubkey_compressed,
            ..v4_state(700)
        })
    }

    fn committee_metas(
        live: &VoteAccountsHashMap,
        committee: VoteAccountsHashMap,
    ) -> (VoteAccountMetaCollection, BLSPubkeyToRankMap) {
        let rank_map = BLSPubkeyToRankMap::new(&committee);
        let epoch_stakes = HashMap::from([(EPOCH, committee)]);
        let collection = fetch_vote_account_metas(
            live,
            |epoch| epoch_stakes.get(&epoch),
            None,
            EPOCH,
            AlpenglowEpochType::Alpenglow,
            Some(&rank_map),
        );
        (collection, rank_map)
    }

    #[test]
    fn committee_ranks_order_by_stake_then_by_the_compressed_bls_key() {
        let (top, first_tied, second_tied) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let (top_key, first_key, second_key) = (bls_key(), bls_key(), bls_key());
        let top_member = committee_member(Some(top_key));
        let VoteStateVersions::V4(top_state) = &top_member else {
            unreachable!()
        };
        let top_node = top_state.node_pubkey;
        let committee = staked_vote_accounts([
            (top, top_member, 30),
            (first_tied, committee_member(Some(first_key)), 20),
            (second_tied, committee_member(Some(second_key)), 20),
        ]);

        let (collection, rank_map) = committee_metas(&committee.clone(), committee);
        let committee_of = |vote_account: &Pubkey| &meta_of(&collection, vote_account).committee;

        assert_eq!(committee_of(&top).epoch_stake_rank, Some(0));
        let (lower_key, higher_key) = if first_key < second_key {
            (first_tied, second_tied)
        } else {
            (second_tied, first_tied)
        };
        assert_eq!(
            (
                committee_of(&lower_key).epoch_stake_rank,
                committee_of(&higher_key).epoch_stake_rank
            ),
            (Some(1), Some(2)),
            "equal stakes are ordered by the compressed BLS key ascending"
        );
        for vote_account in [top, first_tied, second_tied] {
            let rank = committee_of(&vote_account).epoch_stake_rank.unwrap();
            assert_eq!(
                rank_map
                    .get_pubkey_stake_entry(rank.into())
                    .unwrap()
                    .vote_account_pubkey,
                vote_account
            );
        }
        let top_fields = committee_of(&top);
        assert_eq!(top_fields.epoch_stake, Some(30));
        assert_eq!(
            top_fields.epoch_stake_bls_pubkey,
            Some(bs58::encode(top_key).into_string())
        );
        assert_eq!(top_fields.epoch_stake_node_pubkey, Some(top_node));
    }

    #[test]
    fn a_duplicated_bls_key_keeps_its_stake_and_loses_its_rank() {
        let shared_key = bls_key();
        let committee = staked_vote_accounts([
            (VOTE_ACCOUNT, committee_member(Some(shared_key)), 30),
            (OTHER_VOTE_ACCOUNT, committee_member(Some(shared_key)), 20),
            (Pubkey::new_unique(), committee_member(Some(bls_key())), 10),
        ]);

        let (collection, _) = committee_metas(&committee.clone(), committee);

        for (vote_account, stake) in [(VOTE_ACCOUNT, 30), (OTHER_VOTE_ACCOUNT, 20)] {
            let committee = &meta_of(&collection, &vote_account).committee;
            assert_eq!(committee.epoch_stake, Some(stake));
            assert_eq!(committee.epoch_stake_rank, None);
        }
    }

    #[test]
    fn a_pre_v4_committee_state_keeps_its_stake_and_node_and_has_no_key_or_rank() {
        let committee = staked_vote_accounts([
            (VOTE_ACCOUNT, v3(5), 30),
            (OTHER_VOTE_ACCOUNT, committee_member(Some(bls_key())), 20),
        ]);
        let live = vote_accounts([(VOTE_ACCOUNT, v4_with_bls_key(700))]);

        let (collection, _) = committee_metas(&live, committee);
        let committee = &meta_of(&collection, &VOTE_ACCOUNT).committee;

        assert_eq!(committee.epoch_stake, Some(30));
        assert_eq!(
            committee.epoch_stake_bls_pubkey, None,
            "the key is the committee state's, not the live one's"
        );
        assert_eq!(committee.epoch_stake_rank, None);
        assert_eq!(committee.epoch_stake_node_pubkey, Some(Pubkey::default()));
    }

    #[test]
    fn a_live_account_outside_the_committee_has_no_committee_fields() {
        let committee =
            staked_vote_accounts([(OTHER_VOTE_ACCOUNT, committee_member(Some(bls_key())), 20)]);
        let live = vote_accounts([(VOTE_ACCOUNT, v4_with_bls_key(700))]);

        let (collection, _) = committee_metas(&live, committee);
        let committee = &meta_of(&collection, &VOTE_ACCOUNT).committee;

        assert_eq!(
            (
                committee.epoch_stake,
                committee.epoch_stake_rank,
                committee.epoch_stake_bls_pubkey.as_deref(),
                committee.epoch_stake_node_pubkey
            ),
            (None, None, None, None)
        );
    }

    #[test]
    fn an_unstaked_epoch_stakes_entry_is_no_committee_member() {
        let committee = staked_vote_accounts([
            (VOTE_ACCOUNT, committee_member(Some(bls_key())), 0),
            (OTHER_VOTE_ACCOUNT, committee_member(Some(bls_key())), 20),
        ]);

        let (collection, _) = committee_metas(&committee.clone(), committee);
        let committee = &meta_of(&collection, &VOTE_ACCOUNT).committee;

        assert_eq!(
            (
                committee.epoch_stake,
                committee.epoch_stake_rank,
                committee.epoch_stake_bls_pubkey.as_deref(),
                committee.epoch_stake_node_pubkey
            ),
            (None, None, None, None)
        );
    }

    #[test]
    fn the_raw_history_is_published_verbatim_with_the_marker() {
        let history = vec![(EPOCH - 1, 1_000, 0), (EPOCH, 1_800, 1_000), MARKER];
        let versions = VoteStateVersions::new_v4(VoteStateV4 {
            epoch_credits: history.clone(),
            ..v4_state(700)
        });
        let live = vote_accounts([(VOTE_ACCOUNT, versions)]);

        let collection = fetch_vote_account_metas(
            &live,
            |_| None,
            None,
            EPOCH,
            AlpenglowEpochType::Migration,
            None,
        );

        assert_eq!(meta_of(&collection, &VOTE_ACCOUNT).epoch_credits, history);
    }

    #[test]
    fn the_marker_serializes_as_u64_max_strings_and_round_trips() {
        let meta = ValidatorMeta {
            epoch_credits: Some(vec![MARKER]),
            ..validator_meta()
        };

        let json = serde_json::to_value(&meta).unwrap();
        assert_eq!(
            json["epoch_credits"],
            serde_json::json!([[
                "18446744073709551615",
                "18446744073709551615",
                "18446744073709551615"
            ]])
        );
        assert_eq!(serde_json::from_value::<ValidatorMeta>(json).unwrap(), meta);
    }

    #[test]
    fn epoch_credits_that_are_no_u64_strings_are_rejected() {
        for epoch_credits in [
            serde_json::json!([[899, 1000, 0]]),
            serde_json::json!([["899", "1e3", "0"]]),
            serde_json::json!([["899", "18446744073709551616", "0"]]),
        ] {
            let mut json = serde_json::to_value(validator_meta()).unwrap();
            json["epoch_credits"] = epoch_credits.clone();

            assert!(
                serde_json::from_value::<ValidatorMeta>(json).is_err(),
                "{epoch_credits} must not parse"
            );
        }
    }

    #[test]
    fn a_tower_epoch_publishes_the_tower_delta_as_before() {
        let history = vec![(EPOCH - 1, 1_000, 0), (EPOCH, 2_500, 1_000)];
        let old_formula = 2_500 - 1_000;

        let (credits, parts) = credits_in(AlpenglowEpochType::Tower, history);

        assert_eq!(credits, Some(old_formula));
        assert_eq!((parts.tower, parts.alpenglow), (Some(old_formula), None));
        assert_eq!(
            credits_in(AlpenglowEpochType::Tower, vec![(EPOCH - 1, 1_000, 0)]).0,
            Some(0),
            "a Tower epoch with no entry for the epoch still earned zero"
        );
    }

    #[test]
    fn an_alpenglow_epoch_publishes_the_lamport_delta_past_the_marker_and_null_credits() {
        let history = vec![
            (EPOCH - 2, 1_000, 0),
            MARKER,
            (EPOCH - 1, 5_000, 1_000),
            (EPOCH, 12_000, 5_000),
        ];

        let (credits, parts) = credits_in(AlpenglowEpochType::Alpenglow, history);

        assert_eq!(credits, None);
        assert_eq!((parts.tower, parts.alpenglow), (None, Some(7_000)));
    }

    #[test]
    fn the_migration_epoch_publishes_both_parts_and_null_credits() {
        let (t0, t1, a) = (1_000, 1_800, 50_000);
        let history = vec![(EPOCH, t1, t0), MARKER, (EPOCH, a + t1, t1)];

        let (credits, parts) = credits_in(AlpenglowEpochType::Migration, history);

        assert_eq!(parts.tower, Some(t1 - t0));
        assert_eq!(parts.alpenglow, Some(a));
        assert_eq!(
            credits, None,
            "the Tower and the lamport parts are never summed nor swapped into credits"
        );
    }

    #[test]
    fn a_migration_epoch_ending_on_the_marker_publishes_no_alpenglow_part_and_null_credits() {
        let history = vec![(EPOCH - 1, 1_000, 0), (EPOCH, 1_800, 1_000), MARKER];

        let (credits, parts) = credits_in(AlpenglowEpochType::Migration, history);

        assert_eq!(parts.tower, Some(800));
        assert_eq!(parts.alpenglow, None);
        assert_eq!(
            credits, None,
            "credits() of a history ending on the marker is u64::MAX and must never leak"
        );
    }

    #[test]
    fn an_alpenglow_epoch_whose_marker_scrolled_off_still_publishes_the_lamport_delta() {
        let history = vec![(EPOCH - 1, 5_000, 1_000), (EPOCH, 12_000, 5_000)];

        let (credits, parts) = credits_in(AlpenglowEpochType::Alpenglow, history);

        assert_eq!(credits, None);
        assert_eq!((parts.tower, parts.alpenglow), (None, Some(7_000)));
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
                alpenglow_active: None,
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
                alpenglow_active: None,
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
                alpenglow_active: None,
            },
            "block_revenue_sharing gates the commission split and nothing else"
        );
    }

    #[test]
    fn the_alpenglow_flag_reports_the_alpenglow_feature_alone() {
        let mut features = FeatureSet::default().snapshot().clone();
        features.alpenglow = true;

        assert_eq!(
            SnapshotFeatures::from_feature_snapshot(&features),
            SnapshotFeatures {
                alpenglow_active: Some(true),
                ..SnapshotFeatures::default()
            }
        );
        assert_eq!(
            SnapshotFeatures::from_feature_snapshot(FeatureSet::default().snapshot())
                .alpenglow_active,
            None
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
            credits: Some(789),
            inflation_rewards_collector: Some(INFLATION_REWARDS_COLLECTOR),
            inflation_rewards_commission_bps: Some(733),
            inflation_rewards_commission_bps_is_v4: Some(true),
            block_revenue_collector: Some(BLOCK_REVENUE_COLLECTOR),
            block_revenue_commission_bps: Some(1234),
            pending_delegator_rewards: Some(987_654_321),
            inflation_rewards_admitted: Some(true),
            inflation_rewards_points: Some(29_503_827_922_340_690_000_000),
            tower_credits: Some(789),
            alpenglow_credits: None,
            epoch_credits: Some(vec![(EPOCH - 1, 1_000, 0), (EPOCH, 1_789, 1_000)]),
            epoch_stake: Some(450),
            epoch_stake_rank: Some(3),
            epoch_stake_bls_pubkey: Some(
                bs58::encode([9u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE]).into_string(),
            ),
            epoch_stake_node_pubkey: Some(NODE),
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
                "inflation_rewards_admitted": true,
                "inflation_rewards_points": "29503827922340690000000",
                "tower_credits": 789,
                "alpenglow_credits": null,
                "epoch_credits": [["899", "1000", "0"], ["900", "1789", "1000"]],
                "epoch_stake": 450,
                "epoch_stake_rank": 3,
                "epoch_stake_bls_pubkey": "LDxMHkBdiMPeeGd2r1n1zvbsyFVCwW4fqkuVXchP3HsfKKen1mAndxYXCxCQZFnXS",
                "epoch_stake_node_pubkey": "CktRuQ2mttgRGkXJtyksdKHjUdc2C4TgDzyB98oEzy8",
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
            inflation_rewards_admitted: None,
            inflation_rewards_points: None,
            epoch_stake_rank: None,
            epoch_stake_bls_pubkey: None,
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
                "inflation_rewards_admitted": null,
                "inflation_rewards_points": null,
                "tower_credits": 789,
                "alpenglow_credits": null,
                "epoch_credits": [["899", "1000", "0"], ["900", "1789", "1000"]],
                "epoch_stake": 450,
                "epoch_stake_rank": null,
                "epoch_stake_bls_pubkey": null,
                "epoch_stake_node_pubkey": "CktRuQ2mttgRGkXJtyksdKHjUdc2C4TgDzyB98oEzy8",
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
            inflation_rewards_unadmitted_at_slot: 4,
            features: SnapshotFeatures {
                block_revenue_custom_collector_active: true,
                block_revenue_sharing_active: false,
                inflation_rewards_custom_collector_active: Some(true),
                inflation_rewards_delay_commission_updates_active: Some(true),
                inflation_rewards_commission_rate_in_basis_points_active: None,
                inflation_rewards_validator_admission_ticket_active: Some(true),
                alpenglow_active: None,
            },
            alpenglow_epoch_type: Some(AlpenglowEpochType::Migration),
            alpenglow_migration_slot: Some(950),
            epoch_total_stake: Some(10_000),
            vat_config: VatConfig {
                minimum_vote_account_balance_for_vat: Some(1_627_074_240),
                vat_lamports_per_epoch: Some(1_600_000_000),
                max_alpenglow_vote_accounts: Some(2_000),
            },
            epoch_inflation_account: Some(EpochInflationAccount {
                current: EpochInflationState {
                    max_possible_validator_reward: 7_000,
                    slots_per_epoch: 432_000,
                    epoch: 900,
                },
                prev: None,
            }),
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
                "inflation_rewards_unadmitted_at_slot": 4,
                "features": {
                    "block_revenue_custom_collector_active": true,
                    "block_revenue_sharing_active": false,
                    "inflation_rewards_custom_collector_active": true,
                    "inflation_rewards_delay_commission_updates_active": true,
                    "inflation_rewards_commission_rate_in_basis_points_active": null,
                    "inflation_rewards_validator_admission_ticket_active": true,
                    "alpenglow_active": null,
                },
                "alpenglow_epoch_type": "migration",
                "alpenglow_migration_slot": 950,
                "epoch_total_stake": 10_000,
                "minimum_vote_account_balance_for_vat": 1_627_074_240,
                "vat_lamports_per_epoch": 1_600_000_000,
                "max_alpenglow_vote_accounts": 2_000,
                "epoch_inflation_account": {
                    "current": {
                        "max_possible_validator_reward": 7_000,
                        "slots_per_epoch": 432_000,
                        "epoch": 900,
                    },
                    "prev": null,
                },
            })
        );
    }

    // institutional-staking and validator-bonds read credits as a number, so a null must be pinned here
    #[test]
    fn an_alpenglow_row_publishes_null_credits_beside_its_parts() {
        let alpenglow_row = ValidatorMeta {
            credits: None,
            tower_credits: None,
            alpenglow_credits: Some(7_000),
            ..validator_meta()
        };

        let json = serde_json::to_value(&alpenglow_row).unwrap();

        assert_eq!(json["credits"], serde_json::Value::Null);
        assert_eq!(json["tower_credits"], serde_json::Value::Null);
        assert_eq!(json["alpenglow_credits"], serde_json::json!(7_000));
        assert_eq!(
            serde_json::from_value::<ValidatorMeta>(json).unwrap(),
            alpenglow_row
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
        assert_eq!(
            (
                collection.alpenglow_epoch_type,
                collection.alpenglow_migration_slot
            ),
            (None, None),
            "a file that recorded no regime must not claim Tower"
        );
        assert_eq!(collection.epoch_total_stake, None);
        assert_eq!(collection.vat_config, VatConfig::default());
        assert_eq!(collection.epoch_inflation_account, None);
        let meta = &collection.validator_metas[0];
        assert_eq!(meta.commission, 7);
        assert_eq!(meta.stake, 456);
        assert_eq!(meta.credits, Some(789));
        assert_eq!(meta.inflation_rewards_collector, None);
        assert_eq!(meta.inflation_rewards_commission_bps, None);
        assert_eq!(meta.inflation_rewards_commission_bps_is_v4, None);
        assert_eq!(meta.block_revenue_collector, None);
        assert_eq!(meta.block_revenue_commission_bps, None);
        assert_eq!(meta.pending_delegator_rewards, None);
        assert_eq!(
            meta.inflation_rewards_points, None,
            "a file that published no points must not be read as zero points"
        );
        assert_eq!((meta.tower_credits, meta.alpenglow_credits), (None, None));
        assert_eq!(
            meta.epoch_credits, None,
            "a file that published no history must not be read as an empty one"
        );
        assert_eq!(
            (
                meta.epoch_stake,
                meta.epoch_stake_rank,
                meta.epoch_stake_bls_pubkey.as_deref(),
                meta.epoch_stake_node_pubkey
            ),
            (None, None, None, None)
        );
    }

    const BANK_EPOCH: Epoch = 2;

    fn three_validator_genesis() -> GenesisConfigInfo {
        let keypairs: Vec<_> = (0..3).map(|_| ValidatorVoteKeypairs::new_rand()).collect();
        create_genesis_config_with_vote_accounts(
            1_000_000_000_000,
            &keypairs,
            vec![1_000_000_000; 3],
        )
    }

    fn end_of_epoch_bank(
        epoch_credits: Vec<(Epoch, u64, u64)>,
    ) -> (Arc<Bank>, Arc<RwLock<BankForks>>) {
        end_of_epoch_bank_from(&three_validator_genesis(), epoch_credits)
    }

    fn end_of_epoch_bank_from(
        genesis: &GenesisConfigInfo,
        epoch_credits: Vec<(Epoch, u64, u64)>,
    ) -> (Arc<Bank>, Arc<RwLock<BankForks>>) {
        end_of_epoch_bank_crossing(genesis, epoch_credits, |_| {})
    }

    fn end_of_epoch_bank_crossing(
        genesis: &GenesisConfigInfo,
        epoch_credits: Vec<(Epoch, u64, u64)>,
        before_boundary: impl FnOnce(&Bank),
    ) -> (Arc<Bank>, Arc<RwLock<BankForks>>) {
        let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis.genesis_config);
        // a bank creates epoch_stakes only for the boundary it crosses, so epoch 1 must be entered on the way
        let bank1 = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            bank0.clone(),
            *bank0.leader(),
            bank0
                .epoch_schedule()
                .get_first_slot_in_epoch(BANK_EPOCH - 1),
        );
        before_boundary(&bank1);
        bank1.freeze();
        let bank = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            bank1.clone(),
            *bank1.leader(),
            bank1.epoch_schedule().get_last_slot_in_epoch(BANK_EPOCH),
        );
        for vote_pubkey in bank.vote_accounts().keys() {
            set_epoch_credits(&bank, vote_pubkey, epoch_credits.clone());
        }
        (bank, bank_forks)
    }

    fn collection_of(bank: &Arc<Bank>) -> ValidatorMetaCollection {
        try_collection_of(bank).unwrap()
    }

    fn try_collection_of(bank: &Arc<Bank>) -> anyhow::Result<ValidatorMetaCollection> {
        bank.freeze();
        let stake_accounts = bank
            .get_program_accounts(&solana_stake_interface::program::ID)
            .unwrap();
        let tip_distribution_accounts: Vec<_> = bank
            .vote_accounts()
            .keys()
            .map(|vote_account| tip_distribution_account(vote_account, BANK_EPOCH))
            .collect();
        generate_validator_collection(
            bank,
            &stake_accounts,
            &tip_distribution_accounts,
            &[],
            false,
        )
    }

    // the utils::jito_parser offsets of a TipDistributionAccount with no merkle root
    fn tip_distribution_account(
        vote_account: &Pubkey,
        epoch: Epoch,
    ) -> (Pubkey, AccountSharedData) {
        let mut data = crate::jito_mev::TIP_DISTRIBUTION_ACCOUNT_DISCRIMINATOR.to_vec();
        data.extend_from_slice(vote_account.as_ref());
        data.extend_from_slice(&[0u8; 32]);
        data.push(0);
        data.extend_from_slice(&epoch.to_le_bytes());
        data.extend_from_slice(&800u16.to_le_bytes());
        let account = solana_sdk::account::Account {
            lamports: 1,
            data,
            ..solana_sdk::account::Account::default()
        };
        (Pubkey::new_unique(), account.into())
    }

    fn collection_certified_at(
        genesis_certificate_slot: impl Fn(&Bank) -> Option<Slot>,
        epoch_credits: Vec<(Epoch, u64, u64)>,
    ) -> (ValidatorMetaCollection, Option<Slot>) {
        let (bank, _bank_forks) = end_of_epoch_bank(epoch_credits);
        let certified_slot = genesis_certificate_slot(&bank);
        if let Some(slot) = certified_slot {
            set_genesis_certificate(&bank, slot);
        }
        (collection_of(&bank), certified_slot)
    }

    #[test]
    fn the_collection_publishes_the_regime_its_genesis_certificate_sets() {
        let (tower, _) = collection_certified_at(|_| None, vec![(BANK_EPOCH, 1_000, 0)]);
        assert_eq!(
            (tower.alpenglow_epoch_type, tower.alpenglow_migration_slot),
            (Some(AlpenglowEpochType::Tower), None)
        );
        assert!(tower
            .validator_metas
            .iter()
            .all(|meta| meta.inflation_rewards_points.is_some()));
        assert!(
            tower
                .validator_metas
                .iter()
                .all(|meta| meta.epoch_stake_rank.is_none()),
            "no rank map is built while alpenglow is inactive"
        );
        assert_eq!(
            (
                tower.vat_config.vat_lamports_per_epoch,
                tower.vat_config.max_alpenglow_vote_accounts
            ),
            (None, Some(2_000)),
            "the collection carries the VAT config of its bank"
        );

        let migration_slot =
            |bank: &Bank| Some(bank.epoch_schedule().get_first_slot_in_epoch(BANK_EPOCH) + 1);
        let (migration, certified_slot) = collection_certified_at(
            migration_slot,
            vec![(BANK_EPOCH, 1_000, 0), MARKER, (BANK_EPOCH, 5_000, 1_000)],
        );
        assert_eq!(
            migration.alpenglow_epoch_type,
            Some(AlpenglowEpochType::Migration)
        );
        assert!(certified_slot.is_some());
        assert_eq!(migration.alpenglow_migration_slot, certified_slot);
        assert!(
            migration
                .validator_metas
                .iter()
                .all(|meta| meta.inflation_rewards_points.is_some()),
            "the migration epoch still pays its Tower slots by Tower points"
        );
        assert!(migration.validator_metas.iter().all(|meta| (
            meta.credits,
            meta.tower_credits,
            meta.alpenglow_credits
        ) == (None, Some(1_000), Some(4_000))));

        let alpenglow_slot = |bank: &Bank| {
            Some(
                bank.epoch_schedule()
                    .get_first_slot_in_epoch(BANK_EPOCH - 1),
            )
        };
        let (alpenglow, certified_slot) =
            collection_certified_at(alpenglow_slot, vec![(BANK_EPOCH, 1_000, 0)]);
        assert_eq!(
            alpenglow.alpenglow_epoch_type,
            Some(AlpenglowEpochType::Alpenglow)
        );
        assert_eq!(alpenglow.alpenglow_migration_slot, certified_slot);
        assert!(alpenglow
            .validator_metas
            .iter()
            .all(|meta| meta.inflation_rewards_points.is_none()));
    }

    #[test]
    fn an_alpenglow_collection_publishes_every_raw_input_of_the_vote_reward() {
        let mut genesis = three_validator_genesis();
        activate_alpenglow_at_genesis(&mut genesis.genesis_config);
        let (bank, _bank_forks) = end_of_epoch_bank_from(
            &genesis,
            vec![(BANK_EPOCH - 1, 1_000, 0), (BANK_EPOCH, 5_000, 1_000)],
        );

        let collection = collection_of(&bank);

        assert_eq!(
            collection.alpenglow_epoch_type,
            Some(AlpenglowEpochType::Alpenglow),
            "agave's alpenglow genesis certifies slot 0"
        );
        assert_eq!(collection.features.alpenglow_active, Some(true));
        let epoch_inflation_account = collection.epoch_inflation_account.as_ref().unwrap();
        assert_eq!(epoch_inflation_account.current.epoch, BANK_EPOCH);
        assert!(collection.vat_config.vat_lamports_per_epoch.is_some());
        let mut ranks: Vec<_> = collection
            .validator_metas
            .iter()
            .map(|meta| meta.epoch_stake_rank.unwrap())
            .collect();
        ranks.sort();
        assert_eq!(
            ranks,
            (0..3).collect::<Vec<u16>>(),
            "ranks cover 0..n without gaps"
        );
        assert!(collection.validator_metas.iter().all(|meta| {
            (
                meta.credits,
                meta.tower_credits,
                meta.alpenglow_credits,
                meta.inflation_rewards_points,
            ) == (None, None, Some(4_000), None)
        }));
    }

    #[test]
    fn the_epoch_total_stake_sums_the_committee_stakes() {
        let (bank, _bank_forks) = end_of_epoch_bank(vec![(BANK_EPOCH, 1_000, 0)]);
        let committee_size = bank.epoch_vote_accounts(BANK_EPOCH).unwrap().len();

        let collection = collection_of(&bank);

        let members: Vec<_> = collection
            .validator_metas
            .iter()
            .filter_map(|meta| meta.epoch_stake)
            .collect();
        assert_eq!(
            members.len(),
            committee_size,
            "every committee member is live"
        );
        assert_ne!(collection.epoch_total_stake, Some(0));
        assert_eq!(collection.epoch_total_stake, Some(members.iter().sum()));
        assert!(collection
            .validator_metas
            .iter()
            .all(|meta| meta.epoch_stake_bls_pubkey.is_some()
                && meta.epoch_stake_node_pubkey.is_some()));
    }

    #[test]
    fn a_migration_epoch_with_tower_credits_and_no_alpenglow_reward_yet_is_accepted() {
        let migration_slot =
            |bank: &Bank| Some(bank.epoch_schedule().get_last_slot_in_epoch(BANK_EPOCH) - 1);

        let (migration, _) =
            collection_certified_at(migration_slot, vec![(BANK_EPOCH, 1_000, 0), MARKER]);

        assert_eq!(
            migration.alpenglow_epoch_type,
            Some(AlpenglowEpochType::Migration)
        );
        assert!(migration.validator_metas.iter().all(|meta| (
            meta.credits,
            meta.tower_credits,
            meta.alpenglow_credits
        ) == (None, Some(1_000), None)));
    }

    #[test]
    fn a_collection_where_no_validator_earned_credits_is_rejected() {
        for (regime_slot, epoch_credits) in [
            (None, vec![(BANK_EPOCH - 1, 1_000, 0)]),
            (Some(0), vec![(BANK_EPOCH, 1_000, 1_000), MARKER]),
        ] {
            let (bank, _bank_forks) = end_of_epoch_bank(epoch_credits);
            if let Some(slot) = regime_slot {
                let migration_slot =
                    bank.epoch_schedule().get_first_slot_in_epoch(BANK_EPOCH) + slot;
                set_genesis_certificate(&bank, migration_slot);
            }

            let err =
                try_collection_of(&bank).expect_err("a snapshot where nobody earned is broken");

            assert!(
                err.to_string().contains("No validator earned credits"),
                "{err}"
            );
        }
    }

    #[test]
    fn a_tower_bank_with_a_prefunded_inflation_account_address_publishes_no_account() {
        let (bank, _bank_forks) = end_of_epoch_bank(vec![(BANK_EPOCH, 1_000, 0)]);
        let prefunded = AccountSharedData::new(
            bank.get_minimum_balance_for_rent_exemption(0),
            0,
            &Pubkey::default(),
        );
        bank.store_account(&epoch_inflation_account_address(), &prefunded);
        assert!(epoch_inflation_account(&bank).is_err());

        let collection = collection_of(&bank);

        assert_eq!(collection.features.alpenglow_active, None);
        assert_eq!(collection.epoch_inflation_account, None);
    }

    #[test]
    fn alpenglow_activated_at_an_epoch_boundary_holds_the_inflation_account_of_that_epoch() {
        let genesis = three_validator_genesis();
        let mut activated = genesis.genesis_config.clone();
        activate_feature(&mut activated, agave_feature_set::alpenglow::id());
        let mut pending = AccountSharedData::from(
            activated.accounts[&agave_feature_set::alpenglow::id()].clone(),
        );
        // bincode of Feature { activated_at: None }, padded to Feature::size_of
        pending.set_data_from_slice(&[0; 9]);
        let (bank, _bank_forks) = end_of_epoch_bank_crossing(
            &genesis,
            vec![(BANK_EPOCH, 1_000, 0)],
            |previous_epoch_bank| {
                assert!(!previous_epoch_bank.feature_set.snapshot().alpenglow);
                assert_eq!(epoch_inflation_account(previous_epoch_bank).unwrap(), None);
                previous_epoch_bank.store_account(&agave_feature_set::alpenglow::id(), &pending);
            },
        );

        let collection = collection_of(&bank);

        assert_eq!(collection.features.alpenglow_active, Some(true));
        assert_eq!(
            collection.alpenglow_epoch_type,
            Some(AlpenglowEpochType::Tower),
            "the feature activates before any genesis certificate migrates the cluster"
        );
        let epoch_inflation_account = collection.epoch_inflation_account.unwrap();
        assert_eq!(epoch_inflation_account.current.epoch, BANK_EPOCH);
        assert_eq!(epoch_inflation_account.prev, None);
    }

    // a slot-time feature takes effect at a later epoch boundary than the one activating it
    fn vat_config_of(genesis: &GenesisConfigInfo) -> (VatConfig, u64) {
        let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis.genesis_config);
        let bank = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            bank0.clone(),
            *bank0.leader(),
            bank0.epoch_schedule().get_first_slot_in_epoch(1),
        );
        let rent_exempt_minimum = vote_account_rent_exempt_minimum(&bank);
        (VatConfig::of(&bank).unwrap(), rent_exempt_minimum)
    }

    #[test]
    fn an_alpenglow_bank_at_the_legacy_slot_time_burns_the_legacy_vat() {
        let mut genesis = one_validator_genesis();
        activate_alpenglow_at_genesis(&mut genesis.genesis_config);
        deactivate_features(
            &mut genesis.genesis_config,
            &slot_time_feature_ids().to_vec(),
        );

        let (vat_config, rent_exempt_minimum) = vat_config_of(&genesis);

        assert_eq!(
            vat_config,
            VatConfig {
                minimum_vote_account_balance_for_vat: Some(rent_exempt_minimum + 1_600_000_000),
                vat_lamports_per_epoch: Some(1_600_000_000),
                max_alpenglow_vote_accounts: Some(2_000),
            }
        );
    }

    #[test]
    fn an_alpenglow_bank_at_200ms_slots_burns_half_the_legacy_vat() {
        let mut genesis = one_validator_genesis();
        activate_alpenglow_at_genesis(&mut genesis.genesis_config);

        let (vat_config, rent_exempt_minimum) = vat_config_of(&genesis);

        assert_eq!(vat_config.vat_lamports_per_epoch, Some(800_000_000));
        assert_eq!(
            vat_config.minimum_vote_account_balance_for_vat,
            Some(rent_exempt_minimum + 800_000_000)
        );
    }

    #[test]
    fn a_tower_bank_under_vat_burns_nothing_and_needs_only_rent() {
        let (vat_config, rent_exempt_minimum) = vat_config_of(&one_validator_genesis());

        assert_eq!(
            vat_config,
            VatConfig {
                minimum_vote_account_balance_for_vat: Some(rent_exempt_minimum),
                vat_lamports_per_epoch: None,
                max_alpenglow_vote_accounts: Some(2_000),
            }
        );
    }

    #[test]
    fn a_bank_without_vat_publishes_no_vat_config() {
        let mut genesis = one_validator_genesis();
        deactivate_features(
            &mut genesis.genesis_config,
            &vec![agave_feature_set::validator_admission_ticket::id()],
        );

        let (vat_config, _) = vat_config_of(&genesis);

        assert_eq!(vat_config, VatConfig::default());
    }

    // VALIDATORS_JSON=<file> cargo test -- --ignored, on a parsed testnet epoch after the migration one
    #[test]
    #[ignore = "reads a parsed validators.json named by VALIDATORS_JSON"]
    fn a_parsed_testnet_alpenglow_epoch_publishes_every_raw_input() {
        let path =
            std::env::var("VALIDATORS_JSON").expect("VALIDATORS_JSON names the file to check");
        let collection: ValidatorMetaCollection =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let metas = &collection.validator_metas;

        assert_eq!(
            collection.alpenglow_epoch_type,
            Some(AlpenglowEpochType::Alpenglow)
        );
        assert!(collection.alpenglow_migration_slot.is_some());
        assert_eq!(collection.features.alpenglow_active, Some(true));
        assert!(metas.iter().all(|meta| meta.credits.is_none()
            && meta.tower_credits.is_none()
            && meta.inflation_rewards_points.is_none()
            && meta.epoch_credits.is_some()));
        assert!(metas
            .iter()
            .any(|meta| meta.alpenglow_credits.is_some_and(|credits| credits > 0)));
        assert_eq!(
            collection.vat_config.vat_lamports_per_epoch,
            Some(800_000_000),
            "testnet burns this since slot 446348256"
        );
        assert_eq!(
            collection
                .epoch_inflation_account
                .as_ref()
                .map(|account| account.current.epoch),
            Some(collection.epoch)
        );
        let staked_in_committee: u64 = metas.iter().filter_map(|meta| meta.epoch_stake).sum();
        assert!(collection.epoch_total_stake.unwrap() >= staked_in_committee);

        let mut ranks: Vec<u16> = metas
            .iter()
            .filter_map(|meta| meta.epoch_stake_rank)
            .collect();
        ranks.sort();
        assert!(!ranks.is_empty());
        assert!(
            ranks.windows(2).all(|pair| pair[0] < pair[1]),
            "a rank is held twice"
        );
        if collection.leader_schedule_vote_accounts_absent_at_slot == 0 {
            assert_eq!(
                ranks,
                (0..ranks.len() as u16).collect::<Vec<_>>(),
                "with every committee member live, ranks cover 0..n without gaps"
            );
        }
    }

    // past u64, so a float or a u64 on either side of the file would corrupt it
    #[test]
    fn points_past_u64_round_trip_through_json_as_a_string() {
        let meta = ValidatorMeta {
            inflation_rewards_points: Some(u128::MAX),
            ..validator_meta()
        };

        let json = serde_json::to_value(&meta).unwrap();
        assert_eq!(
            json["inflation_rewards_points"],
            serde_json::json!(u128::MAX.to_string())
        );
        assert_eq!(serde_json::from_value::<ValidatorMeta>(json).unwrap(), meta);
    }
}
