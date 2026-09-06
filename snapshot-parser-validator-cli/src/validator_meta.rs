use crate::jito_priority_fee::fetch_jito_priority_fee_metas;
use {
    crate::jito_mev::fetch_jito_mev_metas,
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
    #[serde(with = "option_pubkey_string_conversion")]
    pub inflation_rewards_collector: Option<Pubkey>,
    /// SIMD-0185 // VoteStateV4 // inflation_rewards_commission_bps, read at
    /// `ValidatorMetaCollection::commission_vintage`; `None` on a pre-v4 vote state,
    /// which only carries the integer percent published as `commission`
    pub inflation_rewards_commission_bps: Option<u16>,
    /// SIMD-0185 // VoteStateV4 // block_revenue_collector, read at
    /// `ValidatorMetaCollection::commission_vintage`; `None` on a pre-v4 vote state
    #[serde(with = "option_pubkey_string_conversion")]
    pub block_revenue_collector: Option<Pubkey>,
    /// SIMD-0185 // VoteStateV4 // block_revenue_commission_bps, read at
    /// `ValidatorMetaCollection::commission_vintage`; `None` on a pre-v4 vote state
    pub block_revenue_commission_bps: Option<u16>,
    /// SIMD-0185 // VoteStateV4 // pending_delegator_rewards, read at
    /// `ValidatorMetaCollection::collector_vintage`; `None` on a pre-v4 vote state
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

    /// The stakes cache of a bank frozen at the last slot of `epoch`. agave
    /// snapshots exactly this state into `epoch_stakes(epoch + 2)` at the first
    /// slot of the distribution epoch and reads the inflation rewards collector
    /// out of it.
    fn from_live_stakes_cache(epoch: Epoch) -> Self {
        Self {
            epoch_stakes_key: None,
            captured_at_epoch: epoch.saturating_add(1),
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
    /// vintage of the vote state behind `inflation_rewards_commission_bps`,
    /// `block_revenue_collector` and `block_revenue_commission_bps`
    pub commission_vintage: VoteStateVintage,
    /// vintage of the vote state behind `inflation_rewards_collector` and
    /// `pending_delegator_rewards`
    pub collector_vintage: VoteStateVintage,
    /// vote accounts absent from `commission_vintage` — too young to be carried
    /// by it — whose commission fields agave resolves, and so does this
    /// collection, from the next snapshot instead
    pub commission_vintage_fallbacks: usize,
    /// agave `custom_commission_collector` (SIMD-0232) as of `slot`; while this
    /// is false the runtime ignores both collectors and pays the vote account
    pub custom_commission_collector_active: bool,
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
    inflation_rewards_commission_bps: Option<u16>,
    block_revenue_collector: Option<Pubkey>,
    block_revenue_commission_bps: Option<u16>,
    pending_delegator_rewards: Option<u64>,
}

struct VoteAccountMetaCollection {
    metas: Vec<VoteAccountMeta>,
    commission_vintage: VoteStateVintage,
    commission_vintage_fallbacks: usize,
}

/// SIMD-0185 fields whose value for a rewarded epoch is the one held by the
/// commission vintage.
struct V4CommissionFields {
    inflation_rewards_commission_bps: u16,
    block_revenue_collector: Pubkey,
    block_revenue_commission_bps: u16,
}

/// SIMD-0185 fields whose value for a rewarded epoch is the one held by the
/// collector vintage.
struct V4CollectorFields {
    inflation_rewards_collector: Pubkey,
    pending_delegator_rewards: u64,
}

/// `VoteStateView` answers every SIMD-0185 getter, synthesizing a value on a
/// pre-v4 vote state — `commission * 100` bps, a full 10000 bps block revenue
/// commission, zero pending rewards. Only the collector getters report absence,
/// so they are what gates each group.
fn v4_commission_fields(vote_state_view: &VoteStateView) -> Option<V4CommissionFields> {
    let block_revenue_collector = *vote_state_view.block_revenue_collector()?;

    Some(V4CommissionFields {
        inflation_rewards_commission_bps: vote_state_view.inflation_rewards_commission(),
        block_revenue_collector,
        block_revenue_commission_bps: vote_state_view.block_revenue_commission(),
    })
}

fn v4_collector_fields(vote_state_view: &VoteStateView) -> Option<V4CollectorFields> {
    let inflation_rewards_collector = *vote_state_view.inflation_rewards_collector()?;

    Some(V4CollectorFields {
        inflation_rewards_collector,
        pending_delegator_rewards: vote_state_view.pending_delegator_rewards(),
    })
}

/// The vote-account snapshots agave applies to a rewarded epoch's commission.
///
/// `primary` is agave `snapshot_epoch_vote_accounts`, the anti-rug snapshot the
/// runtime reads the inflation rewards commission from, and the same snapshot
/// SIMD-0232 reads the block revenue collector from while the epoch is being
/// produced. `fallback` is agave `rewarded_epoch_vote_accounts`, which the
/// runtime falls back to for a vote account the anti-rug snapshot does not carry.
struct CommissionVintageSource<'a> {
    primary: Option<&'a VoteAccountsHashMap>,
    fallback: Option<&'a VoteAccountsHashMap>,
    epoch: Epoch,
}

impl<'a> CommissionVintageSource<'a> {
    fn new(bank: &'a Bank, epoch: Epoch) -> Self {
        Self {
            primary: bank.epoch_vote_accounts(epoch),
            fallback: bank.epoch_vote_accounts(epoch.saturating_add(1)),
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

    fn view(&self, vote_account: &Pubkey) -> Option<&'a VoteStateView> {
        Self::lookup(self.primary, vote_account)
    }

    fn fallback_view(&self, vote_account: &Pubkey) -> Option<&'a VoteStateView> {
        Self::lookup(self.fallback, vote_account)
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

fn fetch_vote_account_metas(bank: &Arc<Bank>, epoch: Epoch) -> VoteAccountMetaCollection {
    let commission_source = CommissionVintageSource::new(bank, epoch);
    let live_vote_accounts = bank.vote_accounts();
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

        let commission_view = match commission_source.view(pubkey) {
            Some(commission_view) => commission_view,
            None => {
                commission_vintage_fallbacks += 1;
                commission_source
                    .fallback_view(pubkey)
                    .unwrap_or(vote_state_view)
            }
        };
        let commission_fields = v4_commission_fields(commission_view);
        let collector_fields = v4_collector_fields(vote_state_view);

        metas.push(VoteAccountMeta {
            vote_account: *pubkey,
            commission: vote_state_view.commission(),
            stake: *stake,
            credits,
            inflation_rewards_collector: collector_fields
                .as_ref()
                .map(|fields| fields.inflation_rewards_collector),
            inflation_rewards_commission_bps: commission_fields
                .as_ref()
                .map(|fields| fields.inflation_rewards_commission_bps),
            block_revenue_collector: commission_fields
                .as_ref()
                .map(|fields| fields.block_revenue_collector),
            block_revenue_commission_bps: commission_fields
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

    let validator_rate = bank
        .inflation()
        .validator(bank.slot_in_year_for_inflation());
    let capitalization = bank.capitalization();
    let epoch_duration_in_years = bank.epoch_duration_in_years(epoch);
    let validator_rewards =
        (validator_rate * capitalization as f64 * epoch_duration_in_years) as u64;

    let VoteAccountMetaCollection {
        metas: vote_account_metas,
        commission_vintage,
        commission_vintage_fallbacks,
    } = fetch_vote_account_metas(bank, epoch);
    let collector_vintage = VoteStateVintage::from_live_stakes_cache(epoch);
    let custom_commission_collector_active =
        bank.feature_set.snapshot().custom_commission_collector;
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
                inflation_rewards_commission_bps: vote_account_meta
                    .inflation_rewards_commission_bps,
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
        "Commission vintage: {:?}, resolved through the fallback for {} / {} vote accounts",
        commission_vintage, commission_vintage_fallbacks, total_validators
    );
    info!("Collector vintage: {:?}", collector_vintage);
    info!(
        "SIMD-0232 custom commission collector active: {}",
        custom_commission_collector_active
    );

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
        custom_commission_collector_active,
    })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_vote_interface::state::{VoteStateV3, VoteStateV4, VoteStateVersions},
    };

    fn vote_state_view(vote_state_versions: VoteStateVersions) -> VoteStateView {
        VoteStateView::try_new(Arc::new(bincode::serialize(&vote_state_versions).unwrap())).unwrap()
    }

    fn v4_vote_state_view() -> VoteStateView {
        vote_state_view(VoteStateVersions::new_v4(VoteStateV4 {
            inflation_rewards_collector: Pubkey::new_from_array([1u8; 32]),
            block_revenue_collector: Pubkey::new_from_array([2u8; 32]),
            inflation_rewards_commission_bps: 733,
            block_revenue_commission_bps: 1234,
            pending_delegator_rewards: 987_654_321,
            ..VoteStateV4::default()
        }))
    }

    #[test]
    fn a_v4_vote_state_yields_both_collectors_and_both_commissions() {
        let vote_state_view = v4_vote_state_view();

        let commission_fields = v4_commission_fields(&vote_state_view).unwrap();
        assert_eq!(commission_fields.inflation_rewards_commission_bps, 733);
        assert_eq!(
            commission_fields.block_revenue_collector,
            Pubkey::new_from_array([2u8; 32])
        );
        assert_eq!(commission_fields.block_revenue_commission_bps, 1234);

        let collector_fields = v4_collector_fields(&vote_state_view).unwrap();
        assert_eq!(
            collector_fields.inflation_rewards_collector,
            Pubkey::new_from_array([1u8; 32])
        );
        assert_eq!(collector_fields.pending_delegator_rewards, 987_654_321);
    }

    #[test]
    fn the_legacy_commission_stays_the_lossy_percent_of_the_v4_basis_points() {
        assert_eq!(v4_vote_state_view().commission(), 7);
    }

    #[test]
    fn a_pre_v4_vote_state_yields_no_v4_fields_and_an_unchanged_commission() {
        let vote_state_view = vote_state_view(VoteStateVersions::new_v3(VoteStateV3 {
            commission: 9,
            ..VoteStateV3::default()
        }));

        assert!(v4_commission_fields(&vote_state_view).is_none());
        assert!(v4_collector_fields(&vote_state_view).is_none());
        assert_eq!(vote_state_view.commission(), 9);
    }

    #[test]
    fn a_pre_v4_vote_state_still_answers_the_synthesized_v4_getters() {
        let vote_state_view = vote_state_view(VoteStateVersions::new_v3(VoteStateV3 {
            commission: 9,
            ..VoteStateV3::default()
        }));

        assert_eq!(vote_state_view.inflation_rewards_commission(), 900);
        assert_eq!(vote_state_view.block_revenue_commission(), 10_000);
        assert_eq!(vote_state_view.pending_delegator_rewards(), 0);
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

    fn validator_meta() -> ValidatorMeta {
        ValidatorMeta {
            vote_account: Pubkey::new_from_array([7u8; 32]),
            commission: 7,
            mev_commission: Some(1000),
            jito_priority_fee_commission: Some(2000),
            jito_priority_fee_lamports: 123,
            stake: 456,
            credits: 789,
            inflation_rewards_collector: Some(Pubkey::new_from_array([1u8; 32])),
            inflation_rewards_commission_bps: Some(733),
            block_revenue_collector: Some(Pubkey::new_from_array([2u8; 32])),
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
            inflation_rewards_commission_bps: None,
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
                "inflation_rewards_commission_bps": null,
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
            custom_commission_collector_active: true,
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
                "custom_commission_collector_active": true,
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
}
