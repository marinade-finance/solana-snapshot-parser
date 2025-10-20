use {
    crate::serde_serialize_solana::{option_pubkey_string_conversion, pubkey_string_conversion},
    serde::{Deserialize, Serialize},
    solana_program::{clock::Epoch, pubkey::Pubkey},
};

#[derive(Clone, Deserialize, Serialize, Debug, Eq, PartialEq)]
pub struct StakeMeta {
    #[serde(with = "pubkey_string_conversion")]
    pub pubkey: Pubkey,
    pub balance_lamports: u64,
    pub active_delegation_lamports: u64,
    pub activating_delegation_lamports: u64,
    pub deactivating_delegation_lamports: u64,
    #[serde(with = "option_pubkey_string_conversion")]
    pub validator: Option<Pubkey>,
    #[serde(with = "pubkey_string_conversion")]
    pub stake_authority: Pubkey,
    #[serde(with = "pubkey_string_conversion")]
    pub withdraw_authority: Pubkey,
}

impl Ord for StakeMeta {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.pubkey.cmp(&other.pubkey)
    }
}

impl PartialOrd<Self> for StakeMeta {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Deserialize, Serialize, Debug)]
pub struct StakeMetaCollection {
    pub epoch: Epoch,
    pub slot: u64,
    pub stake_metas: Vec<StakeMeta>,
}
