use {
    serde::{Deserialize, Serialize},
    solana_program::pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_sdk::account::ReadableAccount,
    solana_stake_interface::stake_history::Epoch,
    wincode::SchemaRead,
};

// agave's crate-private EpochInflationState; wincode is positional, so the field order must stay agave's
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, SchemaRead)]
pub struct EpochInflationState {
    pub max_possible_validator_reward: u64,
    pub slots_per_epoch: u64,
    pub epoch: Epoch,
}

// agave's EpochInflationAccountState, rewritten at every epoch start while alpenglow is active
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, SchemaRead)]
pub struct EpochInflationAccount {
    pub current: EpochInflationState,
    pub prev: Option<EpochInflationState>,
}

pub fn epoch_inflation_account_address() -> Pubkey {
    Pubkey::find_program_address(
        &[b"vote_reward_account"],
        &agave_feature_set::alpenglow::id(),
    )
    .0
}

pub fn epoch_inflation_account(bank: &Bank) -> anyhow::Result<Option<EpochInflationAccount>> {
    let address = epoch_inflation_account_address();
    let Some(account) = bank.get_account(&address) else {
        return Ok(None);
    };
    wincode::deserialize(account.data())
        .map(Some)
        .map_err(|err| {
            anyhow::anyhow!("Failed to decode the epoch inflation account {address}: {err}")
        })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::utils::vote_account_fixture::one_validator_genesis,
        solana_runtime::genesis_utils::{activate_alpenglow_at_genesis, GenesisConfigInfo},
        solana_sdk::account::AccountSharedData,
        std::sync::Arc,
    };

    fn state(
        max_possible_validator_reward: u64,
        slots_per_epoch: u64,
        epoch: Epoch,
    ) -> EpochInflationState {
        EpochInflationState {
            max_possible_validator_reward,
            slots_per_epoch,
            epoch,
        }
    }

    fn le_u64s(values: [u64; 3]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    #[test]
    fn a_hand_built_buffer_decodes_field_by_field_in_agaves_order() {
        let mut data = le_u64s([1, 2, 3]);
        data.push(1);
        data.extend(le_u64s([4, 5, 6]));

        assert_eq!(
            wincode::deserialize::<EpochInflationAccount>(&data).unwrap(),
            EpochInflationAccount {
                current: state(1, 2, 3),
                prev: Some(state(4, 5, 6)),
            }
        );
    }

    #[test]
    fn a_buffer_with_no_previous_epoch_decodes_to_none() {
        let mut data = le_u64s([7, 8, 9]);
        data.push(0);

        assert_eq!(
            wincode::deserialize::<EpochInflationAccount>(&data).unwrap(),
            EpochInflationAccount {
                current: state(7, 8, 9),
                prev: None,
            }
        );
    }

    fn genesis(alpenglow: bool) -> GenesisConfigInfo {
        let mut genesis = one_validator_genesis();
        if alpenglow {
            activate_alpenglow_at_genesis(&mut genesis.genesis_config);
        }
        genesis
    }

    fn bank_in_epoch(genesis: &GenesisConfigInfo, epoch: Epoch) -> Arc<Bank> {
        let (mut bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis.genesis_config);
        for next_epoch in 1..=epoch {
            bank.freeze();
            let slot = bank.epoch_schedule().get_first_slot_in_epoch(next_epoch);
            bank = Bank::new_from_parent_with_bank_forks(
                &bank_forks,
                bank.clone(),
                *bank.leader(),
                slot,
            );
        }
        bank
    }

    #[test]
    fn agaves_genesis_account_decodes_to_the_state_it_wrote() {
        let genesis = genesis(true);
        let bank = bank_in_epoch(&genesis, 0);

        assert_eq!(
            epoch_inflation_account(&bank).unwrap(),
            Some(EpochInflationAccount {
                current: state(0, genesis.genesis_config.epoch_schedule.slots_per_epoch, 0),
                prev: None,
            })
        );
    }

    #[test]
    fn agave_rewrites_the_account_at_each_epoch_start_and_keeps_the_previous_state() {
        let bank = bank_in_epoch(&genesis(true), 2);

        let account = epoch_inflation_account(&bank).unwrap().unwrap();

        assert_eq!(account.current.epoch, bank.epoch());
        assert_eq!(
            account.current.slots_per_epoch,
            bank.epoch_schedule().slots_per_epoch,
            "agave writes the schedule's slots_per_epoch, not the warmup epoch's slot count"
        );
        assert_ne!(account.current.max_possible_validator_reward, 0);
        assert_eq!(account.prev.map(|prev| prev.epoch), Some(bank.epoch() - 1));
    }

    #[test]
    fn a_bank_without_the_account_decodes_to_none() {
        let bank = bank_in_epoch(&genesis(false), 0);

        assert_eq!(epoch_inflation_account(&bank).unwrap(), None);
    }

    #[test]
    fn an_undecodable_account_is_an_error_naming_its_address() {
        let bank = bank_in_epoch(&genesis(false), 0);
        let mut account = AccountSharedData::new(1, 2, &Pubkey::default());
        account.set_data_from_slice(&[1, 2]);
        bank.store_account(&epoch_inflation_account_address(), &account);

        let err = epoch_inflation_account(&bank).expect_err("two bytes hold no state");

        assert!(
            err.to_string()
                .contains(&epoch_inflation_account_address().to_string()),
            "{err}"
        );
    }
}
