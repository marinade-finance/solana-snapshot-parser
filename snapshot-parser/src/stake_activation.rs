use {
    solana_runtime::bank::Bank,
    solana_sdk::account::ReadableAccount,
    solana_stake_interface::{
        stake_history::{Epoch, StakeHistory, StakeHistoryEntry},
        state::Delegation,
    },
};

// Owns the warmup/cooldown rate so no caller can pass one and re-pin the pre-activation 25% rate
pub struct StakeActivation {
    epoch: Epoch,
    history: StakeHistory,
    new_rate_activation_epoch: Option<Epoch>,
}

impl StakeActivation {
    pub fn new(bank: &Bank) -> anyhow::Result<Self> {
        let history_account = bank
            .get_account(&solana_stake_interface::sysvar::stake_history::ID)
            .ok_or_else(|| anyhow::anyhow!("Failed to fetch the stake history sysvar"))?;

        Ok(Self {
            epoch: bank.epoch(),
            history: bincode::deserialize(history_account.data())?,
            new_rate_activation_epoch: bank.new_warmup_cooldown_rate_epoch(),
        })
    }

    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    pub fn new_rate_activation_epoch(&self) -> Option<Epoch> {
        self.new_rate_activation_epoch
    }

    #[allow(clippy::disallowed_methods)]
    pub fn status(&self, delegation: &Delegation) -> StakeHistoryEntry {
        delegation.stake_activating_and_deactivating(
            self.epoch,
            &self.history,
            self.new_rate_activation_epoch,
        )
    }

    pub fn effective(&self, delegation: &Delegation) -> u64 {
        self.status(delegation).effective
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use {
        solana_runtime::genesis_utils::{create_genesis_config, GenesisConfigInfo},
        std::sync::Arc,
    };

    #[test]
    fn rate_comes_from_the_bank_and_is_never_none() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(1_000_000);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        let activation = StakeActivation::new(&bank).unwrap();

        // `None` here would select the 25% rate regardless of the cluster — the defect this pins
        assert_eq!(
            activation.new_rate_activation_epoch(),
            bank.new_warmup_cooldown_rate_epoch()
        );
        assert!(
            activation.new_rate_activation_epoch().is_some(),
            "reduce_stake_warmup_cooldown is active on a test genesis, so the rate epoch must be Some"
        );
        assert_eq!(activation.epoch(), bank.epoch());
    }
}
