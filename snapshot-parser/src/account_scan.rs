use {
    log::info,
    rayon::prelude::*,
    solana_accounts_db::accounts_db::{LoadHint, PopulateReadCache},
    solana_program::pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_sdk::account::{AccountSharedData, ReadableAccount},
    std::{
        collections::{HashMap, HashSet},
        sync::Arc,
        time::Instant,
    },
};

pub type AccountPredicate<'a> = Box<dyn Fn(&Pubkey, &[u8]) -> bool + Send + Sync + 'a>;

pub struct OwnerFilter<'a> {
    owner: Pubkey,
    predicate: Option<AccountPredicate<'a>>,
}

impl<'a> OwnerFilter<'a> {
    pub fn all(owner: Pubkey) -> Self {
        Self {
            owner,
            predicate: None,
        }
    }

    pub fn matching(
        owner: Pubkey,
        predicate: impl Fn(&Pubkey, &[u8]) -> bool + Send + Sync + 'a,
    ) -> Self {
        Self {
            owner,
            predicate: Some(Box::new(predicate)),
        }
    }
}

pub fn scan_accounts_by_owner(
    bank: &Arc<Bank>,
    owners: &[Pubkey],
) -> anyhow::Result<HashMap<Pubkey, Vec<(Pubkey, AccountSharedData)>>> {
    let mut seen = HashSet::with_capacity(owners.len());
    let filters: Vec<OwnerFilter> = owners
        .iter()
        .filter(|owner| seen.insert(**owner))
        .map(|owner| OwnerFilter::all(*owner))
        .collect();
    scan_accounts_by_owner_filtered(bank, &filters)
}

pub fn scan_accounts_by_owner_filtered(
    bank: &Arc<Bank>,
    filters: &[OwnerFilter<'_>],
) -> anyhow::Result<HashMap<Pubkey, Vec<(Pubkey, AccountSharedData)>>> {
    let wanted: HashMap<Pubkey, Option<&AccountPredicate<'_>>> = filters
        .iter()
        .map(|filter| (filter.owner, filter.predicate.as_ref()))
        .collect();
    anyhow::ensure!(
        wanted.len() == filters.len(),
        "account scan got more than one filter for the same owner; which one applies would be \
         arbitrary, so pass exactly one filter per owner"
    );

    bank.force_flush_accounts_cache();
    let unflushed_slots = bank.rc.accounts.accounts_db.accounts_cache.num_slots();
    anyhow::ensure!(
        unflushed_slots == 0,
        "account scan needs an empty write cache, but {unflushed_slots} slot(s) remain after \
         flushing the rooted ones; scan a bank loaded straight from a snapshot, or root and \
         flush the pending writes first"
    );

    let sweep_started = Instant::now();
    let storages = bank.get_snapshot_storages(None);
    let candidates: HashSet<Pubkey> = storages
        .par_iter()
        .try_fold(HashSet::<Pubkey>::new, |mut candidates, storage| {
            let accounts = &storage.accounts;
            accounts.scan_accounts_without_data(|offset, account| {
                // a live account is loadable, so a zero-lamport version is never the one kept
                if account.lamports == 0 {
                    return;
                }
                let Some(predicate) = wanted.get(account.owner) else {
                    return;
                };
                // judging every stored version keeps the live one among them, so none is missed
                let keep = match predicate {
                    None => true,
                    Some(predicate) => accounts
                        .get_stored_account_callback(offset, |stored| {
                            predicate(stored.pubkey, stored.data)
                        })
                        .unwrap_or(true),
                };
                if keep {
                    candidates.insert(*account.pubkey);
                }
            })?;
            anyhow::Ok(candidates)
        })
        .try_reduce(HashSet::new, |mut merged, mut other| {
            // extend the larger set with the smaller one, so the merge moves fewer entries
            if other.len() > merged.len() {
                std::mem::swap(&mut merged, &mut other);
            }
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
            // load() already drops zero-lamport accounts, so only the owner is left to check
            let predicate = wanted.get(account.owner()).copied()?;
            predicate
                .is_none_or(|keep| keep(pubkey, account.data()))
                .then_some((*pubkey, account))
        })
        .collect();
    info!(
        "Liveness confirm: {} of {} candidates kept in {:?}",
        confirmed.len(),
        candidates.len(),
        confirm_started.elapsed()
    );

    let mut collected: HashMap<Pubkey, Vec<(Pubkey, AccountSharedData)>> =
        wanted.keys().map(|owner| (*owner, Vec::new())).collect();
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

pub fn verify_scan_matches(
    owner: &Pubkey,
    scanned: &[(Pubkey, AccountSharedData)],
    expected: &[(Pubkey, AccountSharedData)],
) -> anyhow::Result<()> {
    anyhow::ensure!(
        scanned.len() == expected.len(),
        "Account scan mismatch for owner {owner}: single pass produced {} accounts, the index scan produced {}",
        scanned.len(),
        expected.len()
    );

    let scanned_pubkeys: HashSet<Pubkey> = scanned.iter().map(|(pubkey, _)| *pubkey).collect();
    let expected_pubkeys: HashSet<Pubkey> = expected.iter().map(|(pubkey, _)| *pubkey).collect();
    anyhow::ensure!(
        scanned_pubkeys == expected_pubkeys,
        "Account scan pubkey set mismatch for owner {owner}: {} only in single pass, {} only in the index scan",
        scanned_pubkeys.difference(&expected_pubkeys).count(),
        expected_pubkeys.difference(&scanned_pubkeys).count()
    );

    let mut scanned_sorted: Vec<&(Pubkey, AccountSharedData)> = scanned.iter().collect();
    let mut expected_sorted: Vec<&(Pubkey, AccountSharedData)> = expected.iter().collect();
    scanned_sorted.sort_unstable_by_key(|(pubkey, _)| *pubkey);
    expected_sorted.sort_unstable_by_key(|(pubkey, _)| *pubkey);
    for ((pubkey, account), (_, expected_account)) in
        scanned_sorted.into_iter().zip(expected_sorted)
    {
        anyhow::ensure!(
            account == expected_account,
            "Account scan content mismatch for {pubkey} of owner {owner}, single pass versus the \
             index scan: lamports {} vs {}, data bytes {} vs {}, owner {} vs {}",
            account.lamports(),
            expected_account.lamports(),
            account.data().len(),
            expected_account.data().len(),
            account.owner(),
            expected_account.owner()
        );
    }

    info!(
        "Account scan verified against the index scan for owner {}: {} accounts",
        owner,
        scanned.len()
    );
    Ok(())
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

    fn assert_matches_get_filtered_program_accounts(
        bank: &Arc<Bank>,
        owner: &Pubkey,
        keep: impl Fn(&[u8]) -> bool + Send + Sync + Copy,
    ) {
        let scanned = scan_accounts_by_owner_filtered(
            bank,
            &[OwnerFilter::matching(*owner, move |_pubkey, data| {
                keep(data)
            })],
        )
        .unwrap();
        let expected = bank
            .get_filtered_program_accounts(owner, |account| keep(account.data()))
            .unwrap();
        assert_eq!(
            pubkeys(&scanned[owner]),
            pubkeys(&expected),
            "filtered scan diverged from get_filtered_program_accounts for owner {owner}"
        );
        verify_scan_matches(owner, &scanned[owner], &expected).unwrap();
        assert!(
            scanned[owner].is_sorted_by_key(|(pubkey, _)| *pubkey),
            "filtered scan result for owner {owner} is not in a reproducible order"
        );
    }

    fn assert_matches_get_program_accounts(bank: &Arc<Bank>, owners: &[Pubkey]) {
        let scanned = scan_accounts_by_owner(bank, owners).unwrap();
        for owner in owners {
            let expected = bank.get_program_accounts(owner).unwrap();
            assert_eq!(
                pubkeys(&scanned[owner]),
                pubkeys(&expected),
                "scan diverged from get_program_accounts for owner {owner}"
            );
            verify_scan_matches(owner, &scanned[owner], &expected).unwrap();
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
    fn unrooted_writes_fail_the_scan_instead_of_being_dropped() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(1_000_000);
        let (parent, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let wanted_owners = [Pubkey::new_unique()];
        let persisted = Pubkey::new_unique();
        parent.store_account(&persisted, &account(&wanted_owners[0], 10, vec![0; 4]));
        persist(&parent);

        // slot 1 is never squashed, so this write stays in the write cache
        let bank =
            Bank::new_from_parent_with_bank_forks(&bank_forks, parent, Default::default(), 1);
        let unrooted = Pubkey::new_unique();
        bank.store_account(&unrooted, &account(&wanted_owners[0], 20, vec![1; 4]));

        // the account is live as far as the bank is concerned
        assert_eq!(
            pubkeys(&bank.get_program_accounts(&wanted_owners[0]).unwrap()),
            pubkeys_of(&[persisted, unrooted])
        );

        // ...but the sweep would only find the persisted one, so the scan must refuse to answer
        let err = scan_accounts_by_owner(&bank, &wanted_owners)
            .expect_err("a scan that cannot see the write cache must not report a partial result");
        assert!(
            err.to_string().contains("empty write cache"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn stale_storage_versions_do_not_leak_into_the_result() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(1_000_000);
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
            scanned.keys().copied().collect::<BTreeSet<Pubkey>>(),
            pubkeys_of(&wanted_owners),
            "the result must hold an entry per wanted owner and nothing else, so an account \
             that moved to another owner cannot come back under a key of its own"
        );
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

    #[test]
    fn a_predicate_keeps_only_what_it_accepts_and_leaves_other_owners_whole() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(1_000_000);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        let filtered_owner = Pubkey::new_unique();
        let unfiltered_owner = Pubkey::new_unique();
        let store = |owner: &Pubkey, data: Vec<u8>| {
            let pubkey = Pubkey::new_unique();
            bank.store_account(&pubkey, &account(owner, 10, data));
            pubkey
        };

        let kept = [
            store(&filtered_owner, vec![7; 4]),
            store(&filtered_owner, vec![7; 4]),
        ];
        let dropped = store(&filtered_owner, vec![0; 4]);
        let untouched = [store(&unfiltered_owner, vec![0; 4])];
        persist(&bank);

        let scanned = scan_accounts_by_owner_filtered(
            &bank,
            &[
                OwnerFilter::matching(filtered_owner, |_pubkey, data| data == [7; 4]),
                OwnerFilter::all(unfiltered_owner),
            ],
        )
        .unwrap();

        assert_eq!(pubkeys(&scanned[&filtered_owner]), pubkeys_of(&kept));
        assert!(!scanned[&filtered_owner]
            .iter()
            .any(|(pubkey, _)| *pubkey == dropped));
        assert_eq!(pubkeys(&scanned[&unfiltered_owner]), pubkeys_of(&untouched));
    }

    #[test]
    fn a_predicate_judges_every_stored_version_so_the_live_one_decides() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(1_000_000);
        let (parent, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let owner = Pubkey::new_unique();
        let promoted = Pubkey::new_unique();
        let demoted = Pubkey::new_unique();
        parent.store_account(&promoted, &account(&owner, 10, vec![0; 4]));
        parent.store_account(&demoted, &account(&owner, 20, vec![7; 4]));
        persist(&parent);

        let bank =
            Bank::new_from_parent_with_bank_forks(&bank_forks, parent, Default::default(), 1);
        bank.store_account(&promoted, &account(&owner, 10, vec![7; 4]));
        bank.store_account(&demoted, &account(&owner, 20, vec![0; 4]));
        persist(&bank);

        let scanned = scan_accounts_by_owner_filtered(
            &bank,
            &[OwnerFilter::matching(owner, |_pubkey, data| data == [7; 4])],
        )
        .unwrap();

        assert_eq!(
            pubkeys(&scanned[&owner]),
            pubkeys_of(&[promoted]),
            "an account is kept on the verdict of its live version, whatever an older stored \
             version of it says"
        );

        assert_matches_get_filtered_program_accounts(&bank, &owner, |data| data == [7; 4]);
    }

    #[test]
    fn a_predicate_that_accepts_nothing_leaves_an_empty_entry_behind() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(1_000_000);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        let owner = Pubkey::new_unique();
        bank.store_account(&Pubkey::new_unique(), &account(&owner, 10, vec![1, 2, 3]));
        persist(&bank);

        let scanned =
            scan_accounts_by_owner_filtered(&bank, &[OwnerFilter::matching(owner, |_, _| false)])
                .unwrap();

        assert!(scanned[&owner].is_empty());
    }

    #[test]
    fn filtered_scan_matches_get_filtered_program_accounts() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(1_000_000);
        let (parent, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let owner = Pubkey::new_unique();
        let other_owner = Pubkey::new_unique();
        let store = |bank: &Arc<Bank>, owner: &Pubkey, data: Vec<u8>| {
            let pubkey = Pubkey::new_unique();
            bank.store_account(&pubkey, &account(owner, 10, data));
            pubkey
        };

        let rewritten = store(&parent, &owner, vec![1; 8]);
        let drained = store(&parent, &owner, vec![1; 8]);
        store(&parent, &owner, vec![2; 8]);
        store(&parent, &other_owner, vec![1; 8]);
        persist(&parent);

        let bank =
            Bank::new_from_parent_with_bank_forks(&bank_forks, parent, Default::default(), 1);
        bank.store_account(&rewritten, &account(&owner, 10, vec![2; 8]));
        bank.store_account(&drained, &account(&owner, 0, vec![]));
        store(&bank, &owner, vec![1; 8]);
        persist(&bank);

        assert_matches_get_filtered_program_accounts(&bank, &owner, |data| data == [1; 8]);
        assert_matches_get_filtered_program_accounts(&bank, &owner, |_| true);
        assert_matches_get_filtered_program_accounts(&bank, &owner, |_| false);
    }

    #[test]
    fn two_filters_for_one_owner_are_rejected() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(1_000_000);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        persist(&bank);

        let owner = Pubkey::new_unique();
        let err = scan_accounts_by_owner_filtered(
            &bank,
            &[
                OwnerFilter::matching(owner, |_, _| true),
                OwnerFilter::matching(owner, |_, _| false),
            ],
        )
        .expect_err("an ambiguous filter set must not be answered");
        assert!(
            err.to_string().contains("same owner"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_repeated_owner_is_still_harmless_for_the_unfiltered_scan() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(1_000_000);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        let owner = Pubkey::new_unique();
        let stored = Pubkey::new_unique();
        bank.store_account(&stored, &account(&owner, 10, vec![1, 2, 3]));
        persist(&bank);

        let scanned = scan_accounts_by_owner(&bank, &[owner, owner]).unwrap();

        assert_eq!(pubkeys(&scanned[&owner]), pubkeys_of(&[stored]));
    }

    #[test]
    fn a_scan_that_diverges_from_the_index_does_not_verify() {
        let owner = Pubkey::new_unique();
        let shared = (Pubkey::new_unique(), account(&owner, 10, vec![1]));
        let only_scanned = (Pubkey::new_unique(), account(&owner, 10, vec![2]));
        let only_expected = (Pubkey::new_unique(), account(&owner, 10, vec![3]));

        let one = std::slice::from_ref(&shared);
        verify_scan_matches(&owner, one, one).unwrap();

        let err = verify_scan_matches(&owner, one, &[shared.clone(), only_expected.clone()])
            .expect_err("a scan short of the index must not verify");
        assert!(
            err.to_string().contains("produced 1 accounts"),
            "unexpected error: {err}"
        );

        let err = verify_scan_matches(
            &owner,
            &[shared.clone(), only_scanned],
            &[shared, only_expected],
        )
        .expect_err("a scan of the same size but a different set must not verify");
        assert!(
            err.to_string().contains("pubkey set mismatch"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_scan_of_the_right_pubkeys_carrying_stale_contents_does_not_verify() {
        let owner = Pubkey::new_unique();
        let pubkey = Pubkey::new_unique();
        let live = (pubkey, account(&owner, 11, vec![9; 4]));
        let stale_lamports = (pubkey, account(&owner, 10, vec![9; 4]));
        let stale_data = (pubkey, account(&owner, 11, vec![0; 4]));
        let reowned = (pubkey, account(&Pubkey::new_unique(), 11, vec![9; 4]));

        let live_one = std::slice::from_ref(&live);
        verify_scan_matches(&owner, live_one, live_one).unwrap();

        for (stale, what) in [
            (&stale_lamports, "lamport balance"),
            (&stale_data, "account data"),
            (&reowned, "owner"),
        ] {
            let err = verify_scan_matches(&owner, std::slice::from_ref(stale), live_one)
                .expect_err(&format!("a scan carrying a stale {what} must not verify"));
            assert!(
                err.to_string().contains("content mismatch"),
                "unexpected error for a stale {what}: {err}"
            );
        }
    }
}
