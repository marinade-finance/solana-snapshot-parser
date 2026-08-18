use {
    log::info,
    rayon::prelude::*,
    solana_accounts_db::{
        accounts_db::{LoadHint, PopulateReadCache},
        is_loadable::IsLoadable,
    },
    solana_program::pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_sdk::account::{AccountSharedData, ReadableAccount},
    std::{
        collections::{HashMap, HashSet},
        sync::Arc,
        time::Instant,
    },
};

// get_program_accounts is a full accounts-index scan per call, so one call per owner costs N passes
// Correct only for a bank whose live accounts are all in storages (e.g. freshly snapshot-restored):
// force_flush_accounts_cache flushes rooted slots only, so unrooted write-cache entries stay invisible
pub fn scan_accounts_by_owner(
    bank: &Arc<Bank>,
    owners: &[Pubkey],
) -> anyhow::Result<HashMap<Pubkey, Vec<(Pubkey, AccountSharedData)>>> {
    let wanted: HashSet<Pubkey> = owners.iter().copied().collect();

    // accounts still in the write cache have no storage for the sweep to find
    bank.force_flush_accounts_cache();

    let sweep_started = Instant::now();
    let storages = bank.get_snapshot_storages(None);
    let candidates: HashSet<Pubkey> = storages
        .par_iter()
        .try_fold(HashSet::<Pubkey>::new, |mut candidates, storage| {
            storage
                .accounts
                .scan_accounts_without_data(|_offset, account| {
                    // a live account is loadable, so a zero-lamport version is never the one kept
                    if account.lamports != 0 && wanted.contains(account.owner) {
                        candidates.insert(*account.pubkey);
                    }
                })?;
            anyhow::Ok(candidates)
        })
        .try_reduce(HashSet::new, |mut merged, other| {
            merged.extend(other);
            anyhow::Ok(merged)
        })?;
    // HashSet order is randomised per process; sorting keeps the output reproducible run to run
    let mut candidates: Vec<Pubkey> = candidates.into_iter().collect();
    candidates.sort_unstable();
    info!(
        "Storage sweep: {} candidate pubkeys from {} storages in {:?}",
        candidates.len(),
        storages.len(),
        sweep_started.elapsed()
    );

    let confirm_started = Instant::now();
    let accounts_db = &bank.rc.accounts.accounts_db;
    let confirmed: Vec<(Pubkey, AccountSharedData)> = candidates
        .par_iter()
        .filter_map(|pubkey| {
            // every candidate is loaded once, so caching it would only evict useful entries
            let (account, _slot) = accounts_db.load(
                &bank.ancestors,
                pubkey,
                LoadHint::Unspecified,
                PopulateReadCache::False,
            )?;
            // the same predicate get_program_accounts filters on, not a copy of it
            (account.is_loadable() && wanted.contains(account.owner()))
                .then_some((*pubkey, account))
        })
        .collect();
    info!(
        "Liveness confirm: {} of {} candidates live in {:?}",
        confirmed.len(),
        candidates.len(),
        confirm_started.elapsed()
    );

    let mut collected: HashMap<Pubkey, Vec<(Pubkey, AccountSharedData)>> =
        wanted.iter().map(|owner| (*owner, Vec::new())).collect();
    for (pubkey, account) in confirmed {
        collected
            .entry(*account.owner())
            .or_default()
            .push((pubkey, account));
    }

    for (owner, accounts) in &collected {
        info!("Accounts scanned for owner {}: {}", owner, accounts.len());
    }

    Ok(collected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use {
        solana_runtime::genesis_utils::{create_genesis_config, GenesisConfigInfo},
        solana_sdk::account::Account,
        std::collections::BTreeSet,
    };

    fn pubkeys(accounts: &[(Pubkey, AccountSharedData)]) -> BTreeSet<Pubkey> {
        let pubkeys: BTreeSet<Pubkey> = accounts.iter().map(|(pubkey, _)| *pubkey).collect();
        assert_eq!(
            pubkeys.len(),
            accounts.len(),
            "the same pubkey was returned more than once"
        );
        pubkeys
    }

    fn pubkeys_of(keys: &[Pubkey]) -> BTreeSet<Pubkey> {
        keys.iter().copied().collect()
    }

    fn account(owner: &Pubkey, lamports: u64, data: Vec<u8>) -> AccountSharedData {
        AccountSharedData::from(Account {
            lamports,
            data,
            owner: *owner,
            executable: false,
            rent_epoch: 0,
        })
    }

    // the sweep only sees storages, so a test bank has to root and flush its write cache
    fn persist(bank: &Arc<Bank>) {
        bank.freeze();
        bank.squash();
        bank.force_flush_accounts_cache();
    }

    fn assert_matches_get_program_accounts(bank: &Arc<Bank>, owners: &[Pubkey]) {
        let scanned = scan_accounts_by_owner(bank, owners).unwrap();
        for owner in owners {
            assert_eq!(
                pubkeys(&scanned[owner]),
                pubkeys(&bank.get_program_accounts(owner).unwrap()),
                "scan diverged from get_program_accounts for owner {owner}"
            );
            assert!(
                scanned[owner].is_sorted_by_key(|(pubkey, _)| *pubkey),
                "scan result for owner {owner} is not in a reproducible order"
            );
        }
    }

    #[test]
    fn one_pass_matches_get_program_accounts_per_owner() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(1_000_000);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        let wanted_owners = [Pubkey::new_unique(), Pubkey::new_unique()];
        let ignored_owner = Pubkey::new_unique();
        let store = |owner: &Pubkey, lamports: u64| {
            let pubkey = Pubkey::new_unique();
            bank.store_account(&pubkey, &account(owner, lamports, vec![1, 2, 3]));
            pubkey
        };

        let first = [store(&wanted_owners[0], 10), store(&wanted_owners[0], 20)];
        let second = [store(&wanted_owners[1], 30)];
        store(&ignored_owner, 40);
        persist(&bank);

        let scanned = scan_accounts_by_owner(&bank, &wanted_owners).unwrap();

        assert_eq!(pubkeys(&scanned[&wanted_owners[0]]), pubkeys_of(&first));
        assert_eq!(pubkeys(&scanned[&wanted_owners[1]]), pubkeys_of(&second));
        assert!(!scanned.contains_key(&ignored_owner));

        assert_matches_get_program_accounts(&bank, &wanted_owners);
    }

    #[test]
    fn stale_storage_versions_do_not_leak_into_the_result() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(1_000_000);
        // a child bank needs the fork graph that only BankForks installs
        let (parent, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let wanted_owners = [Pubkey::new_unique(), Pubkey::new_unique()];
        let ignored_owner = Pubkey::new_unique();

        // slot 0
        let rewritten = Pubkey::new_unique();
        let reowned = Pubkey::new_unique();
        let drained = Pubkey::new_unique();
        let untouched = Pubkey::new_unique();
        parent.store_account(&rewritten, &account(&wanted_owners[0], 10, vec![0; 4]));
        parent.store_account(&reowned, &account(&wanted_owners[0], 20, vec![1; 4]));
        parent.store_account(&drained, &account(&wanted_owners[1], 30, vec![2; 4]));
        parent.store_account(&untouched, &account(&wanted_owners[1], 40, vec![3; 4]));
        persist(&parent);

        // slot 1: newer versions of the same pubkeys land in a second storage
        let bank = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            parent.clone(),
            Default::default(),
            1,
        );
        let added = Pubkey::new_unique();
        bank.store_account(&rewritten, &account(&wanted_owners[0], 11, vec![9; 4]));
        bank.store_account(&reowned, &account(&ignored_owner, 20, vec![1; 4]));
        bank.store_account(&drained, &account(&wanted_owners[1], 0, vec![]));
        bank.store_account(&added, &account(&wanted_owners[1], 50, vec![4; 4]));
        persist(&bank);

        let scanned = scan_accounts_by_owner(&bank, &wanted_owners).unwrap();

        assert_eq!(
            pubkeys(&scanned[&wanted_owners[0]]),
            pubkeys_of(&[rewritten]),
            "an account whose owner moved out of the wanted set must be dropped"
        );
        assert_eq!(
            pubkeys(&scanned[&wanted_owners[1]]),
            pubkeys_of(&[untouched, added]),
            "a drained account must be dropped, an untouched one kept"
        );

        let (_, account) = scanned[&wanted_owners[0]]
            .iter()
            .find(|(pubkey, _)| *pubkey == rewritten)
            .unwrap();
        assert_eq!(account.lamports(), 11);
        assert_eq!(account.data(), &[9; 4]);

        assert_matches_get_program_accounts(&bank, &wanted_owners);
    }
}
