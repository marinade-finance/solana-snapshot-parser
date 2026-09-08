use {
    solana_program::pubkey::Pubkey,
    solana_sdk::account::AccountSharedData,
    solana_vote::vote_account::{VoteAccount, VoteAccountsHashMap},
    solana_vote_interface::state::VoteStateVersions,
    std::sync::Arc,
};

pub fn staked_vote_accounts<const N: usize>(
    entries: [(Pubkey, VoteStateVersions, u64); N],
) -> VoteAccountsHashMap {
    entries
        .into_iter()
        .map(|(vote_pubkey, vote_state_versions, stake)| {
            let account = AccountSharedData::create_from_existing_shared_data(
                1,
                Arc::new(bincode::serialize(&vote_state_versions).unwrap()),
                solana_vote_interface::program::id(),
                false,
                0,
            );
            (
                vote_pubkey,
                (stake, VoteAccount::try_from(account).unwrap()),
            )
        })
        .collect()
}
