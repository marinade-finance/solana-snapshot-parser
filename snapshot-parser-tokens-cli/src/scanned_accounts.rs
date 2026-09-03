use crate::filters::Filters;
use crate::processors::{
    is_token_account_of_mints, is_voter_account, marinade_vsr_program_id, spl_token_program_id,
};
use log::info;
use snapshot_parser::account_scan::{
    scan_accounts_by_owner_filtered, verify_scan_matches, AccountPredicate, OwnerFilter,
};
use solana_program::program_pack::Pack;
use solana_program::pubkey::Pubkey;
use solana_runtime::bank::Bank;
use solana_sdk::account::{AccountSharedData, ReadableAccount};
use std::collections::HashMap;
use std::sync::Arc;

pub struct ScannedAccounts {
    pub token: Arc<Vec<(Pubkey, AccountSharedData)>>,
    pub voter: Arc<Vec<(Pubkey, AccountSharedData)>>,
    pub stake: Arc<Vec<(Pubkey, AccountSharedData)>>,
}

type ScannedByOwner = HashMap<Pubkey, Vec<(Pubkey, AccountSharedData)>>;

fn required_owner_predicates(
    filters: &Filters,
) -> anyhow::Result<[(Pubkey, Option<AccountPredicate<'_>>); 3]> {
    Ok([
        (
            spl_token_program_id(),
            Some(Box::new(|_pubkey: &Pubkey, data: &[u8]| {
                is_token_account_of_mints(&filters.account_mints, data)
            }) as AccountPredicate),
        ),
        (
            marinade_vsr_program_id()?,
            Some(
                Box::new(|_pubkey: &Pubkey, data: &[u8]| is_voter_account(data))
                    as AccountPredicate,
            ),
        ),
        (solana_stake_interface::program::ID, None),
    ])
}

fn take_required(
    scanned: &mut ScannedByOwner,
    owner: &Pubkey,
    what: &str,
) -> anyhow::Result<Arc<Vec<(Pubkey, AccountSharedData)>>> {
    let accounts = scanned.remove(owner).unwrap_or_default();
    anyhow::ensure!(
        !accounts.is_empty(),
        "Not expected. No {what} scanned (owner {owner}). \
         Evaluate the snapshot data and the filters file."
    );
    Ok(Arc::new(accounts))
}

pub fn scan_required_accounts(
    bank: &Arc<Bank>,
    filters: &Filters,
    verify: bool,
) -> anyhow::Result<ScannedAccounts> {
    let owner_predicates = required_owner_predicates(filters)?;
    let [token_program, vsr_program, stake_program] =
        owner_predicates.each_ref().map(|(owner, _)| *owner);
    let owner_filters: Vec<OwnerFilter> = owner_predicates
        .into_iter()
        .map(|(owner, predicate)| match predicate {
            Some(predicate) => OwnerFilter::matching(owner, predicate),
            None => OwnerFilter::all(owner),
        })
        .collect();

    let mut scanned = scan_accounts_by_owner_filtered(bank, &owner_filters)?;
    let scanned_accounts = ScannedAccounts {
        token: take_required(
            &mut scanned,
            &token_program,
            "token accounts of the configured account_mints",
        )?,
        voter: take_required(&mut scanned, &vsr_program, "VSR voter accounts")?,
        stake: take_required(&mut scanned, &stake_program, "stake accounts")?,
    };

    verify_token_supply(&mint_supplies(bank, filters)?, &scanned_accounts.token)?;
    if verify {
        verify_scan(bank, filters, &scanned_accounts)?;
    }
    Ok(scanned_accounts)
}

fn mint_supplies(bank: &Arc<Bank>, filters: &Filters) -> anyhow::Result<Vec<(Pubkey, u64)>> {
    filters
        .account_mints
        .iter()
        .map(|mint_pubkey| {
            let account = bank
                .get_account(mint_pubkey)
                .ok_or_else(|| anyhow::anyhow!("Mint account not found: {mint_pubkey}"))?;
            let mint = spl_token::state::Mint::unpack(account.data())
                .map_err(|e| anyhow::anyhow!("Failed to unpack mint {mint_pubkey}: {e:?}"))?;
            Ok((*mint_pubkey, mint.supply))
        })
        .collect()
}

fn verify_token_supply(
    mint_supplies: &[(Pubkey, u64)],
    token_accounts: &[(Pubkey, AccountSharedData)],
) -> anyhow::Result<()> {
    let mut scanned_totals: HashMap<Pubkey, u128> = HashMap::new();
    for (pubkey, account) in token_accounts {
        let token = spl_token::state::Account::unpack(account.data()).map_err(|e| {
            anyhow::anyhow!("Failed to unpack scanned token account {pubkey}: {e:?}")
        })?;
        *scanned_totals.entry(token.mint).or_default() += token.amount as u128;
    }

    for (mint_pubkey, supply) in mint_supplies {
        let scanned_total = scanned_totals.remove(mint_pubkey).unwrap_or_default();
        anyhow::ensure!(
            scanned_total == *supply as u128,
            "Token scan does not add up for mint {mint_pubkey}: the scanned accounts hold \
             {scanned_total} but the mint reports a supply of {supply}. The scan missed \
             accounts or read them wrong."
        );
        info!("Token scan reconciles with the supply of mint {mint_pubkey}: {scanned_total}");
    }

    anyhow::ensure!(
        scanned_totals.is_empty(),
        "Token scan returned accounts of {} mint(s) that were never asked for, so the scan \
         filter and the supply check disagree on what was collected",
        scanned_totals.len()
    );
    Ok(())
}

fn verify_scan(
    bank: &Arc<Bank>,
    filters: &Filters,
    scanned: &ScannedAccounts,
) -> anyhow::Result<()> {
    let token_program = spl_token_program_id();
    verify_scan_matches(
        &token_program,
        &scanned.token,
        &bank.get_filtered_program_accounts(&token_program, |account| {
            is_token_account_of_mints(&filters.account_mints, account.data())
        })?,
    )?;

    let vsr_program = marinade_vsr_program_id()?;
    verify_scan_matches(
        &vsr_program,
        &scanned.voter,
        &bank.get_filtered_program_accounts(&vsr_program, |account| {
            is_voter_account(account.data())
        })?,
    )?;

    let stake_program = solana_stake_interface::program::ID;
    verify_scan_matches(
        &stake_program,
        &scanned.stake,
        &bank.get_program_accounts(&stake_program)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::processors::VOTER_ACCOUNT_LEN;
    use solana_program::program_option::COption;
    use solana_program::program_pack::Pack;
    use solana_sdk::account::Account;

    fn filters_of(mint: Pubkey) -> Filters {
        Filters {
            account_mints: vec![mint],
            vsr_registrar_data: vec![],
        }
    }

    fn account_of(data: Vec<u8>) -> AccountSharedData {
        AccountSharedData::from(Account {
            lamports: 1,
            data,
            owner: Pubkey::new_unique(),
            executable: false,
            rent_epoch: 0,
        })
    }

    fn token_account_of(mint: &Pubkey) -> AccountSharedData {
        token_account_holding(mint, 42)
    }

    fn token_account_holding(mint: &Pubkey, amount: u64) -> AccountSharedData {
        let mut data = vec![0u8; spl_token::state::Account::LEN];
        spl_token::state::Account::pack(
            spl_token::state::Account {
                mint: *mint,
                owner: Pubkey::new_unique(),
                amount,
                delegate: COption::None,
                state: spl_token::state::AccountState::Initialized,
                is_native: COption::None,
                delegated_amount: 0,
                close_authority: COption::None,
            },
            &mut data,
        )
        .unwrap();
        account_of(data)
    }

    #[test]
    fn each_required_owner_is_wired_to_the_predicate_of_its_own_program() {
        let mint = Pubkey::new_unique();
        let filters = filters_of(mint);
        let [(token_program, token_predicate), (vsr_program, voter_predicate), (stake_program, stake_predicate)] =
            required_owner_predicates(&filters).unwrap();

        assert_eq!(token_program, spl_token_program_id());
        assert_eq!(vsr_program, marinade_vsr_program_id().unwrap());
        assert_eq!(stake_program, solana_stake_interface::program::ID);
        assert!(
            stake_predicate.is_none(),
            "every stake account of the program is wanted"
        );

        let token_account = token_account_of(&mint);
        let voter_account = account_of(vec![0u8; VOTER_ACCOUNT_LEN]);
        let pubkey = Pubkey::new_unique();
        let token_predicate = token_predicate.expect("the token program is scanned filtered");
        let voter_predicate = voter_predicate.expect("the VSR program is scanned filtered");

        assert!(token_predicate(&pubkey, token_account.data()));
        assert!(!token_predicate(&pubkey, voter_account.data()));
        assert!(voter_predicate(&pubkey, voter_account.data()));
        assert!(!voter_predicate(&pubkey, token_account.data()));
    }

    fn holdings(mint: &Pubkey, amounts: &[u64]) -> Vec<(Pubkey, AccountSharedData)> {
        amounts
            .iter()
            .map(|amount| (Pubkey::new_unique(), token_account_holding(mint, *amount)))
            .collect()
    }

    #[test]
    fn a_scan_whose_balances_add_up_to_the_supply_verifies() {
        let mint = Pubkey::new_unique();

        verify_token_supply(&[(mint, 42)], &holdings(&mint, &[30, 12])).unwrap();
    }

    #[test]
    fn a_scan_that_does_not_add_up_to_the_supply_fails() {
        let mint = Pubkey::new_unique();

        for scanned in [&[30u64, 11u64][..], &[30, 13][..], &[][..]] {
            let err = verify_token_supply(&[(mint, 42)], &holdings(&mint, scanned))
                .expect_err("balances that miss the supply must not verify");
            let err = err.to_string();
            assert!(err.contains(&mint.to_string()), "unexpected error: {err}");
            assert!(err.contains("supply of 42"), "unexpected error: {err}");
        }
    }

    #[test]
    fn a_scan_carrying_a_mint_nobody_asked_for_fails() {
        let wanted = Pubkey::new_unique();
        let stray = Pubkey::new_unique();
        let mut scanned = holdings(&wanted, &[42]);
        scanned.extend(holdings(&stray, &[7]));

        let err = verify_token_supply(&[(wanted, 42)], &scanned)
            .expect_err("an unrequested mint means the scan filter did not hold");
        assert!(
            err.to_string().contains("never asked for"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_scanned_account_that_does_not_unpack_fails_the_check() {
        let mint = Pubkey::new_unique();
        let scanned = vec![(
            Pubkey::new_unique(),
            account_of(vec![0u8; spl_token::state::Account::LEN]),
        )];

        let err = verify_token_supply(&[(mint, 0)], &scanned)
            .expect_err("an account the check cannot read must not pass silently");
        assert!(
            err.to_string().contains("Failed to unpack"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn an_empty_set_of_any_required_owner_fails_the_run() {
        let filters = filters_of(Pubkey::new_unique());
        let owners = required_owner_predicates(&filters)
            .unwrap()
            .each_ref()
            .map(|(owner, _)| *owner);

        for owner in owners {
            for mut scanned in [
                ScannedByOwner::new(),
                ScannedByOwner::from([(owner, Vec::new())]),
            ] {
                let err = take_required(&mut scanned, &owner, "voter accounts")
                    .expect_err("an owner without a single account must not be promoted")
                    .to_string();
                assert!(
                    err.contains(&owner.to_string()),
                    "the error must name the owner: {err}"
                );
                assert!(
                    err.contains("voter accounts"),
                    "the error must name what came back empty, not only the program: {err}"
                );
            }
        }
    }

    #[test]
    fn a_scanned_owner_is_taken_out_of_the_scan_whole() {
        let owner = Pubkey::new_unique();
        let pubkey = Pubkey::new_unique();
        let mut scanned =
            ScannedByOwner::from([(owner, vec![(pubkey, account_of(vec![1, 2, 3]))])]);

        let taken = take_required(&mut scanned, &owner, "stake accounts").unwrap();

        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].0, pubkey);
        assert!(scanned.is_empty(), "a taken owner must not be taken twice");
    }
}
