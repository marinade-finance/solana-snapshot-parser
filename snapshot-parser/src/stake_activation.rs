use {
    solana_runtime::bank::Bank,
    solana_sdk::account::ReadableAccount,
    solana_stake_interface::{
        stake_history::{Epoch, StakeHistory, StakeHistoryEntry},
        state::Delegation,
    },
};

// Owns the bank-derived warmup/cooldown inputs so no caller can re-pin the 25% rate or the float math
pub struct StakeActivation {
    epoch: Epoch,
    history: StakeHistory,
    new_rate_activation_epoch: Option<Epoch>,
    fixed_point_stake_math: bool,
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
            fixed_point_stake_math: bank
                .feature_set
                .snapshot()
                .upgrade_bpf_stake_program_to_v5_1,
        })
    }

    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    pub fn new_rate_activation_epoch(&self) -> Option<Epoch> {
        self.new_rate_activation_epoch
    }

    #[allow(clippy::disallowed_methods, deprecated)]
    pub fn status(&self, delegation: &Delegation) -> StakeHistoryEntry {
        if self.fixed_point_stake_math {
            delegation.stake_activating_and_deactivating_v2(
                self.epoch,
                &self.history,
                self.new_rate_activation_epoch,
            )
        } else {
            delegation.stake_activating_and_deactivating(
                self.epoch,
                &self.history,
                self.new_rate_activation_epoch,
            )
        }
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

    #[test]
    fn fixed_point_math_comes_from_the_bank() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(1_000_000);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        let activation = StakeActivation::new(&bank).unwrap();

        assert_eq!(
            activation.fixed_point_stake_math,
            bank.feature_set
                .snapshot()
                .upgrade_bpf_stake_program_to_v5_1
        );
    }

    #[allow(clippy::disallowed_methods, deprecated)]
    #[test]
    fn each_flag_value_selects_the_matching_upstream_math() {
        let epoch = 1;
        let mut history = StakeHistory::default();
        history.add(
            0,
            StakeHistoryEntry {
                effective: 400_000_000_000_000_000,
                activating: 70_000_000_000_000_003,
                deactivating: 0,
            },
        );
        let delegation = Delegation {
            stake: 9_000_000_030,
            ..Default::default()
        };

        let float = delegation.stake_activating_and_deactivating(epoch, &history, Some(0));
        let fixed = delegation.stake_activating_and_deactivating_v2(epoch, &history, Some(0));
        assert_ne!(
            float, fixed,
            "fixture must keep the two math paths apart, otherwise the dispatch below is untested"
        );

        let activation = |fixed_point_stake_math| StakeActivation {
            epoch,
            history: history.clone(),
            new_rate_activation_epoch: Some(0),
            fixed_point_stake_math,
        };

        assert_eq!(activation(false).status(&delegation), float);
        assert_eq!(activation(true).status(&delegation), fixed);
    }
}
