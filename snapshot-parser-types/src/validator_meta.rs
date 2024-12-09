use {
    crate::serde_serialize_solana_17::pubkey_string_conversion,
    serde::{Deserialize, Serialize},
    solana_program::{clock::Epoch, pubkey::Pubkey},
};

#[derive(Clone, Deserialize, Serialize, Debug, Eq, PartialEq)]
pub struct ValidatorMeta {
    #[serde(with = "pubkey_string_conversion")]
    pub vote_account: Pubkey,
    pub commission: u8,
    /// jito-tip-distribution // TipDistributionAccount // validator_commission_bps
    pub mev_commission: Option<u16>,
    pub stake: u64,
    pub credits: u64,
}

impl Ord for ValidatorMeta {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.vote_account.cmp(&other.vote_account)
    }
}

impl PartialOrd<Self> for ValidatorMeta {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Deserialize, Serialize, Debug, Default)]
pub struct ValidatorMetaCollection {
    pub epoch: Epoch,
    pub slot: u64,
    pub capitalization: u64,
    pub epoch_duration_in_years: f64,
    pub validator_rate: f64,
    pub validator_rewards: u64,
    pub validator_metas: Vec<ValidatorMeta>,
}

impl ValidatorMetaCollection {
    pub fn total_stake_weighted_credits(&self) -> u128 {
        self.validator_metas
            .iter()
            .map(|v| v.credits as u128 * v.stake as u128)
            .sum()
    }

    /// sum of lamports staked to all validators
    pub fn total_stake(&self) -> u64 {
        self.validator_metas.iter().map(|v| v.stake).sum()
    }

    // TODO: DELETE ME? (not used anymore)
    /// expected staker commission (MEV not calculated) reward for a staked lamport to be delivered by a validator
    pub fn expected_epr(&self) -> f64 {
        self.validator_rewards as f64 / self.total_stake() as f64
    }

    /// calculates expected staker reward per one staked lamport when particular commission is set
    pub fn expected_epr_calculator(&self) -> impl Fn(u8) -> f64 {
        let expected_epr = self.expected_epr();

        move |commission: u8| expected_epr * (100.0 - commission as f64) / 100.0
    }
}
