use solana_program::pubkey::Pubkey;
use solana_sdk::account::Account;

// -- Fortunatelly the JITO distribution accounts have the same structure
const VALIDATOR_VOTE_ACCOUNT_BYTE_INDEX: usize = 8; // anchor header
const MERKLE_ROOT_OPTION_BYTE_INDEX: usize = 8 + // anchor header
    64; // vote account + upload authority
const EPOCH_CREATED_AT_NO_MERKLE_ROOT_BYTE_INDEX: usize = MERKLE_ROOT_OPTION_BYTE_INDEX // anchor + pubkeys
        + 1; // 1 byte for Option<MerkleRoot>
const EPOCH_CREATED_AT_WITH_MERKLE_ROOT_BYTE_INDEX: usize =
    EPOCH_CREATED_AT_NO_MERKLE_ROOT_BYTE_INDEX + 64; // MerkleRoot struct size
const VALIDATOR_COMMISSION_BPS_BYTE_OFFSET: usize = 8;

/// Returns the epoch and the byte index where the epoch was found at.
pub(crate) fn get_epoch_created_at(account: &Account) -> anyhow::Result<(u64, usize)> {
    // epoch_created_at_*_byte_index -1 contains info about Option is None (0) or Some (1)
    if u8::from_le_bytes([account.data[MERKLE_ROOT_OPTION_BYTE_INDEX]]) == 0 {
        Ok((
            u64::from_le_bytes(
                account.data[EPOCH_CREATED_AT_NO_MERKLE_ROOT_BYTE_INDEX
                    ..EPOCH_CREATED_AT_NO_MERKLE_ROOT_BYTE_INDEX + 8]
                    .try_into()?,
            ),
            EPOCH_CREATED_AT_NO_MERKLE_ROOT_BYTE_INDEX,
        ))
    } else {
        assert_eq!(
            u8::from_le_bytes([account.data[MERKLE_ROOT_OPTION_BYTE_INDEX]]),
            1
        );
        Ok((
            u64::from_le_bytes(
                account.data[EPOCH_CREATED_AT_WITH_MERKLE_ROOT_BYTE_INDEX
                    ..EPOCH_CREATED_AT_WITH_MERKLE_ROOT_BYTE_INDEX + 8]
                    .try_into()?,
            ),
            EPOCH_CREATED_AT_WITH_MERKLE_ROOT_BYTE_INDEX,
        ))
    }
}

pub(crate) struct JitoCommissionMeta {
    pub validator_vote_account: Pubkey,
    pub epoch_created_at: u64,
    pub validator_commission_bps: u16,
}

pub(crate) fn read_jito_commission_and_epoch(
    account_pubkey: Pubkey,
    account: &Account,
    end_merkle_root_byte_index: usize,
) -> anyhow::Result<JitoCommissionMeta> {
    let validator_vote_account: Pubkey = account.data
        [VALIDATOR_VOTE_ACCOUNT_BYTE_INDEX..VALIDATOR_VOTE_ACCOUNT_BYTE_INDEX + 32]
        .try_into()
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to parse on-chain account {}: {:?}",
                account_pubkey,
                e
            )
        })?;

    let epoch_created_at: u64 = u64::from_le_bytes(
        account.data[end_merkle_root_byte_index..end_merkle_root_byte_index + 8]
            .try_into()
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to parse epoch for account {}: {:?}",
                    account_pubkey,
                    e
                )
            })?,
    );

    let validator_commission_bps_byte_index =
        end_merkle_root_byte_index + VALIDATOR_COMMISSION_BPS_BYTE_OFFSET;
    let validator_commission_bps = u16::from_le_bytes(
        account.data[validator_commission_bps_byte_index..validator_commission_bps_byte_index + 2]
            .try_into()
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to parse validator_commission_bps for account {}: {:?}",
                    account_pubkey,
                    e
                )
            })?,
    );

    Ok(JitoCommissionMeta {
        validator_vote_account,
        epoch_created_at,
        validator_commission_bps,
    })
}
