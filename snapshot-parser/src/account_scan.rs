use {
    log::info,
    solana_accounts_db::is_loadable::IsLoadable,
    solana_program::pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_sdk::account::{AccountSharedData, ReadableAccount},
    std::{
        collections::{HashMap, HashSet},
        sync::Arc,
        time::{Duration, Instant},
    },
};

const PROGRESS_REPORT_EVERY: Duration = Duration::from_secs(60);
// reading the clock per account would cost minutes over a half-billion-entry index
const PROGRESS_CLOCK_EVERY_ENTRIES: u64 = 1_000_000;

// get_program_accounts is a full accounts-index scan per call, so one call per owner costs N passes
pub fn scan_accounts_by_owner(
    bank: &Arc<Bank>,
    owners: &[Pubkey],
) -> anyhow::Result<HashMap<Pubkey, Vec<(Pubkey, AccountSharedData)>>> {
    let wanted: HashSet<Pubkey> = owners.iter().copied().collect();
    let mut collected: HashMap<Pubkey, Vec<(Pubkey, AccountSharedData)>> =
        wanted.iter().map(|owner| (*owner, Vec::new())).collect();

    let started = Instant::now();
    let mut reported_at = started;
    let mut visited: u64 = 0;

    bank.scan_all_accounts(|maybe_account| {
        visited += 1;
        if visited.is_multiple_of(PROGRESS_CLOCK_EVERY_ENTRIES) {
            let now = Instant::now();
            if now.duration_since(reported_at) >= PROGRESS_REPORT_EVERY {
                reported_at = now;
                let elapsed = now.duration_since(started).as_secs_f64();
                info!(
                    "Account scan progress: {} index entries in {:.0}s ({:.0} entries/s), {} accounts collected",
                    visited,
                    elapsed,
                    visited as f64 / elapsed,
                    collected.values().map(Vec::len).sum::<usize>()
                );
            }
        }
        let Some((pubkey, account, _slot)) = maybe_account else {
            return;
        };
        // the same predicate get_program_accounts filters on, not a copy of it
        if !account.is_loadable() {
            return;
        }
        if let Some(accounts) = collected.get_mut(account.owner()) {
            accounts.push((*pubkey, account));
        }
    })?;

    info!(
        "Account scan finished: {} index entries in {:.0}s",
        visited,
        started.elapsed().as_secs_f64()
    );
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
        accounts.iter().map(|(pubkey, _)| *pubkey).collect()
    }

    #[test]
    fn one_pass_matches_get_program_accounts_per_owner() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(1_000_000);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        let wanted_owners = [Pubkey::new_unique(), Pubkey::new_unique()];
        let ignored_owner = Pubkey::new_unique();
        let store = |owner: &Pubkey, lamports: u64| {
            let pubkey = Pubkey::new_unique();
            bank.store_account(
                &pubkey,
                &AccountSharedData::from(Account {
                    lamports,
                    data: vec![1, 2, 3],
                    owner: *owner,
                    executable: false,
                    rent_epoch: 0,
                }),
            );
            pubkey
        };

        let first = [store(&wanted_owners[0], 10), store(&wanted_owners[0], 20)];
        let second = [store(&wanted_owners[1], 30)];
        store(&ignored_owner, 40);

        let scanned = scan_accounts_by_owner(&bank, &wanted_owners).unwrap();

        assert_eq!(pubkeys(&scanned[&wanted_owners[0]]), pubkeys_of(&first));
        assert_eq!(pubkeys(&scanned[&wanted_owners[1]]), pubkeys_of(&second));
        assert!(!scanned.contains_key(&ignored_owner));

        // the is_loadable() skip is not covered: a test bank drops zero-lamport accounts before the scan
        for owner in &wanted_owners {
            assert_eq!(
                pubkeys(&scanned[owner]),
                pubkeys(&bank.get_program_accounts(owner).unwrap()),
                "single pass diverged from get_program_accounts for owner {owner}"
            );
        }
    }

    fn pubkeys_of(keys: &[Pubkey]) -> BTreeSet<Pubkey> {
        keys.iter().copied().collect()
    }
}
