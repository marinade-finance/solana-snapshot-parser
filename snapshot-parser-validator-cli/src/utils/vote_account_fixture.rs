use {
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        consensus_message::Block,
    },
    solana_program::{clock::Slot, pubkey::Pubkey},
    solana_runtime::bank::Bank,
    solana_sdk::account::{AccountSharedData, ReadableAccount},
    solana_stake_interface::stake_history::Epoch,
    solana_vote::vote_account::{VoteAccount, VoteAccountsHashMap},
    solana_vote_interface::state::{VoteStateV4, VoteStateVersions},
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

// the stakes cache drops an account whose data changed size, so the state is written in place
pub fn store_state(
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

pub fn set_epoch_credits(bank: &Bank, vote_pubkey: &Pubkey, epoch_credits: Vec<(Epoch, u64, u64)>) {
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

pub fn set_genesis_certificate(bank: &Bank, slot: Slot) {
    bank.set_alpenglow_genesis_certificate(&Certificate {
        cert_type: CertificateType::Genesis(Block {
            slot,
            block_id: Default::default(),
        }),
        signature: "A".repeat(256).parse().unwrap(),
        bitmap: vec![],
    });
}
