use crate::filters::Filters;
use crate::processors::{
    is_token_account_of_mints, is_voter_account, marinade_vsr_program_id, spl_token_program_id,
};
use snapshot_parser::account_scan::{
    scan_accounts_by_owner_filtered, verify_scan_matches, AccountPredicate, OwnerFilter,
};
use solana_program::pubkey::Pubkey;
use solana_runtime::bank::Bank;
use solana_sdk::account::AccountSharedData;
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
            Some(Box::new(|_pubkey: &Pubkey, account: &AccountSharedData| {
                is_token_account_of_mints(&filters.account_mints, account)
            }) as AccountPredicate),
        ),
        (
            marinade_vsr_program_id()?,
            Some(
                Box::new(|_pubkey: &Pubkey, account: &AccountSharedData| is_voter_account(account))
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

    if verify {
        verify_scan(bank, filters, &scanned_accounts)?;
    }
    Ok(scanned_accounts)
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
            is_token_account_of_mints(&filters.account_mints, account)
        })?,
    )?;

    let vsr_program = marinade_vsr_program_id()?;
    verify_scan_matches(
        &vsr_program,
        &scanned.voter,
        &bank.get_filtered_program_accounts(&vsr_program, is_voter_account)?,
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
        let mut data = vec![0u8; spl_token::state::Account::LEN];
        spl_token::state::Account::pack(
            spl_token::state::Account {
                mint: *mint,
                owner: Pubkey::new_unique(),
                amount: 42,
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

        assert!(token_predicate(&pubkey, &token_account));
        assert!(!token_predicate(&pubkey, &voter_account));
        assert!(voter_predicate(&pubkey, &voter_account));
        assert!(!voter_predicate(&pubkey, &token_account));
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
