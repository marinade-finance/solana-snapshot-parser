use {
    log::{info, warn},
    rayon::prelude::*,
    snapshot_parser::stake_activation::StakeActivation,
    solana_program::pubkey::Pubkey,
    solana_runtime::{bank::Bank, stake_account::StakeAccount},
    solana_sdk::account::{AccountSharedData, ReadableAccount},
    solana_stake_interface::state::Delegation,
    solana_vote::vote_account::{VoteAccount, VoteAccountsHashMap},
    solana_vote_interface::state::VoteStateVersions,
    std::collections::HashMap,
};

// How many differing vote accounts a failed cross-check names before it only counts the rest
const MISMATCH_EXAMPLES: usize = 10;

/// The vote accounts and per-vote-account stake agave keeps in its stakes cache at the bank's
/// slot, rebuilt from the accounts alone. Not every snapshot producer writes the stakes cache
/// into the manifest (Firedancer writes it empty), but every one writes the accounts, so this
/// is the one source the parser reads live vote state from for any snapshot.
///
/// Mirrors `StakesCache::check_and_store`: a vote account is kept when it holds lamports, is
/// initialized at the right size and parses; a stake account counts when it parses to a
/// delegation. Each vote account's stake is the sum of `delegation_effective_stake` at the
/// bank's epoch over the delegations to it, as `Stakes::calculate_delegated_stakes` sums it.
pub fn live_vote_accounts(
    vote_accounts: &[(Pubkey, AccountSharedData)],
    stake_accounts: &[(Pubkey, AccountSharedData)],
    stake_activation: &StakeActivation,
) -> VoteAccountsHashMap {
    let delegated_stakes = delegated_stakes(stake_accounts, stake_activation);
    vote_accounts
        .par_iter()
        .filter_map(|(pubkey, account)| {
            let vote_account = cacheable_vote_account(account)?;
            let stake = delegated_stakes.get(pubkey).copied().unwrap_or_default();
            Some((*pubkey, (stake, vote_account)))
        })
        .collect()
}

fn cacheable_vote_account(account: &AccountSharedData) -> Option<VoteAccount> {
    if account.lamports() == 0 || !VoteStateVersions::is_correct_size_and_initialized(account.data())
    {
        return None;
    }
    VoteAccount::try_from(account.clone()).ok()
}

fn delegated_stakes(
    stake_accounts: &[(Pubkey, AccountSharedData)],
    stake_activation: &StakeActivation,
) -> HashMap<Pubkey, u64> {
    stake_accounts
        .par_iter()
        .filter_map(|(_, account)| {
            if account.lamports() == 0 {
                return None;
            }
            let stake_account = StakeAccount::<Delegation>::try_from(account.clone()).ok()?;
            let delegation = stake_account.delegation();
            let stake = stake_activation.effective_at(delegation, stake_activation.epoch());
            Some((delegation.voter_pubkey, stake))
        })
        .fold(HashMap::new, |mut stakes, (voter, stake)| {
            *stakes.entry(voter).or_default() += stake;
            stakes
        })
        .reduce(HashMap::new, |mut merged, other| {
            for (voter, stake) in other {
                *merged.entry(voter).or_default() += stake;
            }
            merged
        })
}

#[derive(Debug, PartialEq, Eq)]
pub enum StakesCacheCheck {
    /// The manifest carried a stakes cache and it equals the one rebuilt from accounts
    Matched { vote_accounts: usize },
    /// The manifest carried no stakes cache, so there was nothing to compare against
    NoStakesCache,
}

/// Holds the rebuilt map to the stakes cache whenever the snapshot carries one, so every Agave
/// snapshot parsed re-proves that reading from accounts gives what agave caches. A snapshot
/// whose manifest left the cache empty (as Firedancer writes it) has nothing to compare.
pub fn check_against_stakes_cache(
    computed: &VoteAccountsHashMap,
    bank: &Bank,
) -> anyhow::Result<StakesCacheCheck> {
    let cached = bank.vote_accounts();
    if cached.is_empty() {
        let epoch = bank.epoch();
        let epoch_stakes_vote_accounts = bank
            .epoch_vote_accounts(epoch)
            .map(|vote_accounts| vote_accounts.len())
            .unwrap_or_default();
        warn!(
            "The snapshot manifest carries an empty stakes cache while epoch_stakes({epoch}) holds {epoch_stakes_vote_accounts} vote accounts, as a Firedancer-produced snapshot does; live vote state is read from the {} vote accounts rebuilt from accounts, with no cache to cross-check them against",
            computed.len()
        );
        return Ok(StakesCacheCheck::NoStakesCache);
    }
    compare(computed, &cached)?;
    info!(
        "Stakes cache cross-check passed: the {} vote accounts rebuilt from accounts equal the snapshot's stakes cache",
        computed.len()
    );
    Ok(StakesCacheCheck::Matched {
        vote_accounts: computed.len(),
    })
}

fn compare(computed: &VoteAccountsHashMap, cached: &VoteAccountsHashMap) -> anyhow::Result<()> {
    let mut mismatches: Vec<String> = Vec::new();
    for (pubkey, (stake, vote_account)) in cached {
        match computed.get(pubkey) {
            None => mismatches.push(format!(
                "{pubkey}: in the stakes cache with stake {stake}, not rebuilt from accounts"
            )),
            Some((computed_stake, _)) if computed_stake != stake => mismatches.push(format!(
                "{pubkey}: stake {computed_stake} rebuilt from accounts, {stake} in the stakes cache"
            )),
            Some((_, computed_account)) if computed_account != vote_account => mismatches.push(
                format!("{pubkey}: account rebuilt from accounts differs from the cached one"),
            ),
            Some(_) => {}
        }
    }
    for (pubkey, (stake, _)) in computed {
        if !cached.contains_key(pubkey) {
            mismatches.push(format!(
                "{pubkey}: rebuilt from accounts with stake {stake}, absent from the stakes cache"
            ));
        }
    }
    if mismatches.is_empty() {
        return Ok(());
    }
    mismatches.sort();
    let total = mismatches.len();
    mismatches.truncate(MISMATCH_EXAMPLES);
    anyhow::bail!(
        "The vote accounts rebuilt from accounts differ from the snapshot's stakes cache in {total} vote account(s), so reading live vote state from accounts would not give what agave caches: {}{}",
        mismatches.join("; "),
        if total > MISMATCH_EXAMPLES {
            format!("; and {} more", total - MISMATCH_EXAMPLES)
        } else {
            String::new()
        }
    )
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_runtime::genesis_utils::{create_genesis_config_with_vote_accounts, ValidatorVoteKeypairs},
        solana_sdk::account::WritableAccount,
        std::sync::Arc,
    };

    const STAKE: u64 = 1_000_000_000;

    fn bank_with_validators(count: usize) -> Arc<Bank> {
        let keypairs: Vec<_> = (0..count).map(|_| ValidatorVoteKeypairs::new_rand()).collect();
        let stakes = (1..=count as u64).map(|i| STAKE * i).collect();
        let genesis =
            create_genesis_config_with_vote_accounts(1_000_000_000_000_000, &keypairs, stakes)
                .genesis_config;
        let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis);
        let bank = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            bank0.clone(),
            *bank0.leader(),
            bank0.epoch_schedule().get_last_slot_in_epoch(2),
        );
        bank.freeze();
        bank
    }

    fn rebuilt(bank: &Bank) -> VoteAccountsHashMap {
        let vote_accounts = bank
            .get_program_accounts(&solana_vote_interface::program::ID)
            .unwrap();
        let stake_accounts = bank
            .get_program_accounts(&solana_stake_interface::program::ID)
            .unwrap();
        live_vote_accounts(
            &vote_accounts,
            &stake_accounts,
            &StakeActivation::new(bank).unwrap(),
        )
    }

    #[test]
    fn the_map_rebuilt_from_accounts_is_the_stakes_cache() {
        let bank = bank_with_validators(3);
        let computed = rebuilt(&bank);

        assert_eq!(computed.len(), 3);
        assert_eq!(computed, *bank.vote_accounts());
        assert_eq!(
            check_against_stakes_cache(&computed, &bank).unwrap(),
            StakesCacheCheck::Matched { vote_accounts: 3 }
        );
    }

    #[test]
    fn a_stake_that_differs_from_the_cache_fails_the_cross_check() {
        let bank = bank_with_validators(2);
        let mut computed = rebuilt(&bank);
        let (_, (stake, _)) = computed.iter_mut().next().unwrap();
        *stake += 1;

        let err = check_against_stakes_cache(&computed, &bank).unwrap_err();
        assert!(err.to_string().contains("in 1 vote account(s)"), "{err}");
    }

    #[test]
    fn a_vote_account_missing_from_either_side_fails_the_cross_check() {
        let bank = bank_with_validators(2);
        let mut computed = rebuilt(&bank);
        let removed = *computed.keys().next().unwrap();
        computed.remove(&removed);

        let err = check_against_stakes_cache(&computed, &bank).unwrap_err();
        assert!(err.to_string().contains("not rebuilt from accounts"), "{err}");
    }

    #[test]
    fn an_uninitialized_or_drained_vote_account_is_not_cached() {
        let bank = bank_with_validators(1);
        let (pubkey, account) = bank
            .get_program_accounts(&solana_vote_interface::program::ID)
            .unwrap()
            .remove(0);

        let mut drained = account.clone();
        drained.set_lamports(0);
        let mut uninitialized = account.clone();
        uninitialized.data_as_mut_slice().fill(0);

        let activation = StakeActivation::new(&bank).unwrap();
        for vote_account in [drained, uninitialized] {
            assert!(live_vote_accounts(&[(pubkey, vote_account)], &[], &activation).is_empty());
        }
        assert_eq!(
            live_vote_accounts(&[(pubkey, account)], &[], &activation)[&pubkey].0,
            0,
            "a vote account with no delegation to it is still cached, at zero stake"
        );
    }
}
