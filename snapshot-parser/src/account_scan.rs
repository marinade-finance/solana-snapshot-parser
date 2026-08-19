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

// Decides whether a scanned account is kept. Applied to the freshly loaded account in
// the liveness-confirm phase, so an owner with a huge population (every SPL-token
// account, say) is never materialised with its data: rejected accounts are dropped as
// soon as the predicate says no.
//
// Mirrors the filter of Bank::get_filtered_program_accounts, which likewise only ever
// sees loadable (non-zero-lamport) accounts owned by the program being scanned.
pub type AccountPredicate<'a> = Box<dyn Fn(&Pubkey, &AccountSharedData) -> bool + Send + Sync + 'a>;

// One owner to scan for, plus what to keep of it.
pub struct OwnerFilter<'a> {
    owner: Pubkey,
    predicate: Option<AccountPredicate<'a>>,
}

impl<'a> OwnerFilter<'a> {
    // Keep every account owned by `owner`, like Bank::get_program_accounts.
    pub fn all(owner: Pubkey) -> Self {
        Self {
            owner,
            predicate: None,
        }
    }

    // Keep the accounts owned by `owner` that `predicate` accepts, like
    // Bank::get_filtered_program_accounts.
    pub fn matching(
        owner: Pubkey,
        predicate: impl Fn(&Pubkey, &AccountSharedData) -> bool + Send + Sync + 'a,
    ) -> Self {
        Self {
            owner,
            predicate: Some(Box::new(predicate)),
        }
    }

    pub fn owner(&self) -> &Pubkey {
        &self.owner
    }
}

// Finds all accounts owned by the given programs in one pass over the storage files.
// Doing this with get_program_accounts instead would rescan the whole accounts index
// once per owner.
//
// Only use this on a bank loaded straight from a snapshot: there, every live account
// is guaranteed to be in a storage file. On a bank that has processed transactions
// since, recent writes may still sit in the write cache where this scan cannot see
// them (the flush below covers rooted slots only), so the scan errors out rather than
// under-report.
pub fn scan_accounts_by_owner(
    bank: &Arc<Bank>,
    owners: &[Pubkey],
) -> anyhow::Result<HashMap<Pubkey, Vec<(Pubkey, AccountSharedData)>>> {
    // owners used to be collected into a set, so a repeated owner was harmless here;
    // the filtered scan rejects repeats instead, as two filters for one owner are ambiguous
    let mut seen = HashSet::with_capacity(owners.len());
    let filters: Vec<OwnerFilter> = owners
        .iter()
        .filter(|owner| seen.insert(**owner))
        .map(|owner| OwnerFilter::all(*owner))
        .collect();
    scan_accounts_by_owner_filtered(bank, &filters)
}

// scan_accounts_by_owner with a per-owner predicate deciding what is kept. See
// AccountPredicate for where the predicate runs and why that matters for memory.
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

    // accounts still in the write cache have no storage for the sweep to find
    bank.force_flush_accounts_cache();
    // the flush covers rooted slots only, and empties the cache of every slot it flushed.
    // anything left is an unrooted write this scan would silently miss, so refuse to run.
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
            storage
                .accounts
                .scan_accounts_without_data(|_offset, account| {
                    // a live account is loadable, so a zero-lamport version is never the one kept
                    if account.lamports != 0 && wanted.contains_key(account.owner) {
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
            // a rejected account is dropped here, before anything clones or keeps its data
            predicate
                .is_none_or(|keep| keep(pubkey, &account))
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

    // the scan predicate gets a pubkey the bank filter never sees, so bridge the two
    fn assert_matches_get_filtered_program_accounts(
        bank: &Arc<Bank>,
        owner: &Pubkey,
        keep: impl Fn(&AccountSharedData) -> bool + Send + Sync + Copy,
    ) {
        let scanned = scan_accounts_by_owner_filtered(
            bank,
            &[OwnerFilter::matching(*owner, move |_pubkey, account| {
                keep(account)
            })],
        )
        .unwrap();
        assert_eq!(
            pubkeys(&scanned[owner]),
            pubkeys(&bank.get_filtered_program_accounts(owner, keep).unwrap()),
            "filtered scan diverged from get_filtered_program_accounts for owner {owner}"
        );
        assert!(
            scanned[owner].is_sorted_by_key(|(pubkey, _)| *pubkey),
            "filtered scan result for owner {owner} is not in a reproducible order"
        );
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
    fn unrooted_writes_fail_the_scan_instead_of_being_dropped() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(1_000_000);
        let (parent, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let wanted_owners = [Pubkey::new_unique()];
        let persisted = Pubkey::new_unique();
        parent.store_account(&persisted, &account(&wanted_owners[0], 10, vec![0; 4]));
        persist(&parent);

        // slot 1 is never squashed, so this write stays in the write cache: the rooted-only
        // flush inside the scan cannot move it to a storage
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
                OwnerFilter::matching(filtered_owner, |_pubkey, account| account.data() == [7; 4]),
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
    fn a_predicate_that_accepts_nothing_leaves_an_empty_entry_behind() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(1_000_000);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        let owner = Pubkey::new_unique();
        bank.store_account(&Pubkey::new_unique(), &account(&owner, 10, vec![1, 2, 3]));
        persist(&bank);

        let scanned =
            scan_accounts_by_owner_filtered(&bank, &[OwnerFilter::matching(owner, |_, _| false)])
                .unwrap();

        // the owner is still a key, so a caller taking it out gets an empty list, not a panic
        assert!(scanned[&owner].is_empty());
    }

    #[test]
    fn filtered_scan_matches_get_filtered_program_accounts() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(1_000_000);
        // a child bank needs the fork graph that only BankForks installs
        let (parent, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let owner = Pubkey::new_unique();
        let other_owner = Pubkey::new_unique();
        let store = |bank: &Arc<Bank>, owner: &Pubkey, data: Vec<u8>| {
            let pubkey = Pubkey::new_unique();
            bank.store_account(&pubkey, &account(owner, 10, data));
            pubkey
        };

        // slot 0: a mix of matching, non-matching and wrong-owner accounts
        let rewritten = store(&parent, &owner, vec![1; 8]);
        let drained = store(&parent, &owner, vec![1; 8]);
        store(&parent, &owner, vec![2; 8]);
        store(&parent, &other_owner, vec![1; 8]);
        persist(&parent);

        // slot 1: newer versions the filter has to be applied to, not the slot-0 ones
        let bank =
            Bank::new_from_parent_with_bank_forks(&bank_forks, parent, Default::default(), 1);
        // stops matching, even though the slot-0 version did
        bank.store_account(&rewritten, &account(&owner, 10, vec![2; 8]));
        bank.store_account(&drained, &account(&owner, 0, vec![]));
        store(&bank, &owner, vec![1; 8]);
        persist(&bank);

        // matches some accounts, all of them, and none of them
        assert_matches_get_filtered_program_accounts(&bank, &owner, |account| {
            account.data() == [1; 8]
        });
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
}
