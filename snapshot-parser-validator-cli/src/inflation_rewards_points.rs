use {
    snapshot_parser::stake_activation::StakeActivation,
    solana_program::pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_sdk::account::{AccountSharedData, ReadableAccount},
    solana_stake_interface::{
        stake_history::Epoch,
        state::{Stake, StakeStateV2},
    },
    solana_vote::{vote_account::VoteAccountsHashMap, vote_state_view::VoteStateView},
    std::collections::HashMap,
};

// agave_votor_messages::migration::AG_MIGRATION_EPOCH_CREDIT, past which epoch_credits are Alpenglow's
const AG_MIGRATION_EPOCH_CREDIT: (Epoch, u64, u64) = (Epoch::MAX, u64::MAX, u64::MAX);

// agave pays the migration epoch's Tower slots by Tower points too; only the epochs after are Alpenglow's
pub fn pays_tower_points(bank: &Bank) -> bool {
    bank.get_alpenglow_genesis_certificate()
        .is_none_or(|cert| bank.epoch_schedule().get_epoch(cert.cert_type.slot()) >= bank.epoch())
}

// agave's Tower calculate_stake_points_and_credits: a stake left unpaid earns every epoch it never observed
fn stake_points(
    stake: &Stake,
    vote_state: &VoteStateView,
    effective_at: impl Fn(Epoch) -> u64,
) -> u128 {
    let credits_in_stake = stake.credits_observed;
    if vote_state.credits() <= credits_in_stake {
        return 0;
    }

    let mut points = 0;
    let mut new_credits_observed = credits_in_stake;
    for item in vote_state.epoch_credits_iter() {
        let entry: (Epoch, u64, u64) = item.into();
        if entry == AG_MIGRATION_EPOCH_CREDIT {
            break;
        }
        let (epoch, final_epoch_credits, initial_epoch_credits) = entry;
        let earned_credits = if credits_in_stake < initial_epoch_credits {
            final_epoch_credits - initial_epoch_credits
        } else if credits_in_stake < final_epoch_credits {
            final_epoch_credits - new_credits_observed
        } else {
            0
        };
        new_credits_observed = new_credits_observed.max(final_epoch_credits);
        // the stake walks the history, and no stake turns a zero credit into a point
        if earned_credits > 0 {
            points += u128::from(effective_at(epoch)) * u128::from(earned_credits);
        }
    }
    points
}

pub fn points_by_vote_account(
    stake_accounts: &[(Pubkey, AccountSharedData)],
    vote_accounts: &VoteAccountsHashMap,
    stake_activation: &StakeActivation,
) -> HashMap<Pubkey, u128> {
    let mut points: HashMap<Pubkey, u128> = HashMap::new();
    for (_, account) in stake_accounts {
        // the stake meta collection already logs an account that does not parse
        let Ok(StakeStateV2::Stake(_, stake, _)) = bincode::deserialize(account.data()) else {
            continue;
        };
        let voter = stake.delegation.voter_pubkey;
        // agave scores a delegation to a vote account missing from the stakes cache no points
        let Some((_, vote_account)) = vote_accounts.get(&voter) else {
            continue;
        };
        *points.entry(voter).or_default() +=
            stake_points(&stake, vote_account.vote_state_view(), |epoch| {
                stake_activation.effective_at(&stake.delegation, epoch)
            });
    }
    points
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_feature_set::validator_admission_ticket,
        agave_votor_messages::{
            certificate::{Certificate, CertificateType},
            consensus_message::Block,
        },
        solana_runtime::genesis_utils::{
            create_genesis_config_with_vote_accounts, deactivate_features, ValidatorVoteKeypairs,
        },
        solana_sdk::{epoch_rewards::EpochRewards, sysvar},
        solana_stake_interface::state::Delegation,
        solana_vote_interface::state::{VoteStateV4, VoteStateVersions},
        std::sync::Arc,
    };

    const STAKE: u64 = 1_000_000;

    fn view(epoch_credits: Vec<(Epoch, u64, u64)>) -> VoteStateView {
        let versions = VoteStateVersions::new_v4(VoteStateV4 {
            epoch_credits,
            ..VoteStateV4::default()
        });
        VoteStateView::try_new(Arc::new(bincode::serialize(&versions).unwrap())).unwrap()
    }

    fn stake_observing(credits_observed: u64) -> Stake {
        Stake {
            delegation: Delegation {
                stake: STAKE,
                ..Delegation::default()
            },
            credits_observed,
        }
    }

    fn full_stake(_: Epoch) -> u64 {
        STAKE
    }

    fn three_epochs() -> VoteStateView {
        view(vec![(1, 1_000, 0), (2, 2_500, 1_000), (3, 3_000, 2_500)])
    }

    #[test]
    fn a_stake_paid_every_epoch_earns_the_last_epoch_credits_only() {
        assert_eq!(
            stake_points(&stake_observing(2_500), &three_epochs(), full_stake),
            u128::from(STAKE) * 500,
            "this is the stake * credits every consumer reconstructed so far"
        );
    }

    // the epoch 1034 defect: SIMD-0357 refused a validator for epochs and its delegations caught up at once
    #[test]
    fn a_stake_left_unpaid_earns_every_epoch_it_never_observed() {
        assert_eq!(
            stake_points(&stake_observing(0), &three_epochs(), full_stake),
            u128::from(STAKE) * 3_000
        );
    }

    #[test]
    fn a_stake_observed_mid_epoch_earns_that_epoch_from_what_it_observed() {
        assert_eq!(
            stake_points(&stake_observing(1_500), &three_epochs(), full_stake),
            u128::from(STAKE) * (1_000 + 500)
        );
    }

    #[test]
    fn each_unobserved_epoch_is_weighed_by_the_stake_effective_in_it() {
        let warming_up = |epoch: Epoch| if epoch == 2 { STAKE / 4 } else { STAKE };

        assert_eq!(
            stake_points(&stake_observing(0), &three_epochs(), warming_up),
            u128::from(STAKE) * 1_000 + u128::from(STAKE / 4) * 1_500 + u128::from(STAKE) * 500
        );
    }

    #[test]
    fn a_stake_that_observed_every_credit_earns_nothing() {
        assert_eq!(
            stake_points(&stake_observing(3_000), &three_epochs(), full_stake),
            0
        );
    }

    #[test]
    fn credits_past_the_alpenglow_migration_marker_earn_no_tower_points() {
        let migrated = view(vec![
            (1, 1_000, 0),
            AG_MIGRATION_EPOCH_CREDIT,
            (1, 1_500, 1_000),
        ]);

        assert_eq!(
            stake_points(&stake_observing(0), &migrated, full_stake),
            u128::from(STAKE) * 1_000
        );
        assert_eq!(
            stake_points(&stake_observing(1_000), &migrated, full_stake),
            0,
            "a stake paid up to the marker must not underflow on the Alpenglow entry after it"
        );
    }

    #[test]
    fn the_copied_migration_marker_is_agaves() {
        assert_eq!(
            AG_MIGRATION_EPOCH_CREDIT,
            agave_votor_messages::migration::AG_MIGRATION_EPOCH_CREDIT
        );
    }

    fn set_genesis_certificate(bank: &Bank, slot: u64) {
        bank.set_alpenglow_genesis_certificate(&Certificate {
            cert_type: CertificateType::Genesis(Block {
                slot,
                block_id: Default::default(),
            }),
            signature: "A".repeat(256).parse().unwrap(),
            bitmap: vec![],
        });
    }

    #[test]
    fn tower_points_pay_through_the_migration_epoch_and_stop_after_it() {
        let keypairs = [ValidatorVoteKeypairs::new_rand()];
        let genesis =
            create_genesis_config_with_vote_accounts(1_000_000_000_000, &keypairs, vec![STAKE])
                .genesis_config;
        let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis);
        let epoch = 2;
        let first_slot = bank0.epoch_schedule().get_first_slot_in_epoch(epoch);
        let bank = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            bank0.clone(),
            *bank0.leader(),
            first_slot,
        );

        assert!(
            pays_tower_points(&bank),
            "a Tower bank holds no certificate"
        );
        set_genesis_certificate(&bank, first_slot + 5);
        assert!(
            pays_tower_points(&bank),
            "the migration epoch still pays its Tower slots by Tower points"
        );
        set_genesis_certificate(&bank, first_slot - 5);
        assert!(
            !pays_tower_points(&bank),
            "the epoch after the migration is Alpenglow's"
        );
    }

    // agave rewinds credits_observed for a recreated vote account and pays that epoch nothing
    #[test]
    fn a_stake_that_observed_more_than_the_vote_account_holds_earns_nothing() {
        assert_eq!(
            stake_points(&stake_observing(9_000), &three_epochs(), full_stake),
            0
        );
    }

    // the stakes cache drops an account whose data changed size, so the state is written in place
    fn store_state(
        bank: &Bank,
        pubkey: &Pubkey,
        account: &AccountSharedData,
        state: &impl serde::Serialize,
    ) {
        let mut account = account.clone();
        let mut data = account.data().to_vec();
        bincode::serialize_into(&mut data[..], state).unwrap();
        account.set_data_from_slice(&data);
        bank.store_account(pubkey, &account);
    }

    fn set_epoch_credits(bank: &Bank, vote_pubkey: &Pubkey, epoch_credits: Vec<(Epoch, u64, u64)>) {
        let account = bank.get_account(vote_pubkey).unwrap();
        let VoteStateVersions::V4(state) = bincode::deserialize(account.data()).unwrap() else {
            panic!("genesis creates v4 vote accounts");
        };
        let versions = VoteStateVersions::new_v4(VoteStateV4 {
            epoch_credits,
            ..*state
        });
        store_state(bank, vote_pubkey, &account, &versions);
    }

    fn set_credits_observed(
        bank: &Bank,
        stake_pubkey: &Pubkey,
        account: &AccountSharedData,
        credits_observed: u64,
    ) {
        let StakeStateV2::Stake(meta, stake, flags) = bincode::deserialize(account.data()).unwrap()
        else {
            panic!("genesis delegates every stake account");
        };
        let stake = Stake {
            credits_observed,
            ..stake
        };
        store_state(
            bank,
            stake_pubkey,
            account,
            &StakeStateV2::Stake(meta, stake, flags),
        );
    }

    // the oracle: the points agave itself sums into EpochRewards when the next epoch starts
    #[test]
    fn the_points_sum_to_the_total_agave_records_for_the_rewarded_epoch() {
        let keypairs: Vec<_> = (0..3).map(|_| ValidatorVoteKeypairs::new_rand()).collect();
        let mut genesis = create_genesis_config_with_vote_accounts(
            1_000_000_000_000,
            &keypairs,
            vec![STAKE * 1_000; 3],
        )
        .genesis_config;
        // filtering is agave's to apply and the collection's admission verdict to report, not the points'
        deactivate_features(&mut genesis, &vec![validator_admission_ticket::id()]);

        let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis);
        let rewarded_epoch = 2;
        let bank = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            bank0.clone(),
            *bank0.leader(),
            bank0
                .epoch_schedule()
                .get_last_slot_in_epoch(rewarded_epoch),
        );

        let epoch_credits = vec![(0, 1_000, 0), (1, 2_500, 1_000), (2, 3_000, 2_500)];
        for vote_pubkey in bank.vote_accounts().keys() {
            set_epoch_credits(&bank, vote_pubkey, epoch_credits.clone());
        }

        let mut stake_accounts = bank
            .get_program_accounts(&solana_stake_interface::program::ID)
            .unwrap();
        stake_accounts.sort_by_key(|(pubkey, _)| *pubkey);
        assert_eq!(stake_accounts.len(), 3);
        let (paid, unpaid, mid_epoch) =
            (&stake_accounts[0], &stake_accounts[1], &stake_accounts[2]);
        set_credits_observed(&bank, &paid.0, &paid.1, 2_500);
        set_credits_observed(&bank, &unpaid.0, &unpaid.1, 0);
        set_credits_observed(&bank, &mid_epoch.0, &mid_epoch.1, 1_500);
        // a second delegation to the unpaid voter, so the grouping sums rather than overwrites
        let unpaid_twin = Pubkey::new_unique();
        set_credits_observed(&bank, &unpaid_twin, &unpaid.1, 2_500);
        bank.freeze();

        let stake_accounts = bank
            .get_program_accounts(&solana_stake_interface::program::ID)
            .unwrap();
        let points = points_by_vote_account(
            &stake_accounts,
            &bank.vote_accounts(),
            &StakeActivation::on_the_distribution_bank(&bank).unwrap(),
        );

        let next = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            bank.clone(),
            *bank.leader(),
            bank.epoch_schedule()
                .get_first_slot_in_epoch(rewarded_epoch + 1),
        );
        let epoch_rewards: EpochRewards = bincode::deserialize(
            next.get_account(&sysvar::epoch_rewards::id())
                .unwrap()
                .data(),
        )
        .unwrap();

        assert_eq!(points.len(), 3, "every voter is scored: {points:?}");
        assert_ne!(
            epoch_rewards.total_points, 0,
            "a zero total would make the comparison below vacuous"
        );
        assert_eq!(
            points.values().sum::<u128>(),
            epoch_rewards.total_points,
            "the collection's points must be the ones agave divides the epoch's inflation by: {points:?}"
        );
        let voter_of = |account: &AccountSharedData| {
            let StakeStateV2::Stake(_, stake, _) = bincode::deserialize(account.data()).unwrap()
            else {
                unreachable!()
            };
            (
                stake.delegation.voter_pubkey,
                u128::from(stake.delegation.stake),
            )
        };
        let (unpaid_voter, stake) = voter_of(&unpaid.1);
        assert_eq!(
            points[&unpaid_voter],
            stake * 3_000 + stake * 500,
            "the unpaid stake catches up on every epoch and its twin earns only the last one"
        );
    }
}
