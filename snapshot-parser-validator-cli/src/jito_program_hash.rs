//! POSIX cksum CRC-32 of the deployed tip-distribution and priority-fee programs, joined as
//! `<crc_tip>.<crc_priority>`. Reproduce either half by hand with
//! `solana program dump <program_id> f.so && cksum f.so`.

use {
    crc::{Crc, CRC_32_CKSUM},
    log::info,
    solana_loader_v3_interface::state::UpgradeableLoaderState,
    solana_program::pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_sdk::account::ReadableAccount,
    std::sync::Arc,
};

/// Poly 0x04C11DB7, non-reflected, complemented. The catalogued algorithm stops there; the
/// byte count `cksum` folds in on top of it is [`cksum`]'s job.
const CRC: Crc<u32> = Crc::<u32>::new(&CRC_32_CKSUM);

/// What plain `cksum` prints: the CRC-32 of the bytes followed by their count, encoded base-256
/// little-endian with leading zero bytes dropped (so an empty input folds in nothing).
fn cksum(bytes: &[u8]) -> u32 {
    let mut digest = CRC.digest();
    digest.update(bytes);

    let mut remaining = bytes.len();
    while remaining > 0 {
        digest.update(&[remaining as u8]);
        remaining >>= 8;
    }

    digest.finalize()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JitoProgramHash {
    pub combined: String,
    pub per_program: Vec<(Pubkey, String)>,
}

impl JitoProgramHash {
    fn new(per_program: Vec<(Pubkey, String)>) -> Self {
        let combined = per_program
            .iter()
            .map(|(_, crc)| crc.as_str())
            .collect::<Vec<_>>()
            .join(".");

        Self {
            combined,
            per_program,
        }
    }
}

pub fn compute_jito_program_hash(
    bank: &Arc<Bank>,
    program_ids: &[Pubkey],
) -> anyhow::Result<JitoProgramHash> {
    let per_program = program_ids
        .iter()
        .map(|program_id| {
            let dump = program_dump(bank, *program_id)?;
            Ok((*program_id, cksum(&dump).to_string()))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let program_hash = JitoProgramHash::new(per_program);
    for (program_id, crc) in &program_hash.per_program {
        info!("Jito program {program_id} cksum CRC-32: {crc}");
    }
    info!("Jito program hash: {}", program_hash.combined);

    Ok(program_hash)
}

/// The bytes `solana program dump <program_id>` writes to a file
fn program_dump(bank: &Arc<Bank>, program_id: Pubkey) -> anyhow::Result<Vec<u8>> {
    let program_account = bank.get_account(&program_id).ok_or_else(|| {
        anyhow::anyhow!("Jito program account {program_id} not found in the bank")
    })?;
    let programdata_address = programdata_address(program_account.data())
        .map_err(|err| anyhow::anyhow!("Jito program account {program_id}: {err}"))?;

    let programdata_account = bank.get_account(&programdata_address).ok_or_else(|| {
        anyhow::anyhow!(
            "Jito programdata account {programdata_address} of program {program_id} not found in the bank"
        )
    })?;
    let dump = dump_from_programdata(programdata_account.data()).map_err(|err| {
        anyhow::anyhow!(
            "Jito programdata account {programdata_address} of program {program_id}: {err}"
        )
    })?;

    info!(
        "Jito program {program_id} deployed from programdata {programdata_address}: {} dumped bytes",
        dump.len()
    );
    Ok(dump.to_vec())
}

fn programdata_address(data: &[u8]) -> anyhow::Result<Pubkey> {
    match bincode::deserialize(data) {
        Ok(UpgradeableLoaderState::Program {
            programdata_address,
        }) => Ok(programdata_address),
        Ok(state) => anyhow::bail!("expected an upgradeable Program account, got {state:?}"),
        Err(err) => anyhow::bail!("failed to parse the upgradeable Program account: {err}"),
    }
}

// Everything after the metadata, zero padding of the over-allocated account included: the CLI
// dumps the tail verbatim, so stripping anything here would break hand-reproduction
fn dump_from_programdata(data: &[u8]) -> anyhow::Result<&[u8]> {
    match bincode::deserialize(data) {
        Ok(UpgradeableLoaderState::ProgramData { .. }) => {}
        Ok(state) => anyhow::bail!("expected a ProgramData account, got {state:?}"),
        Err(err) => anyhow::bail!("failed to parse the ProgramData header: {err}"),
    }

    let header_len = UpgradeableLoaderState::size_of_programdata_metadata();
    let body = data.get(header_len..).ok_or_else(|| {
        anyhow::anyhow!(
            "account of {} bytes is shorter than the {header_len} byte ProgramData header",
            data.len()
        )
    })?;
    if body.is_empty() {
        anyhow::bail!("no program bytes found after the {header_len} byte ProgramData header");
    }

    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PADDING: usize = 128;

    fn programdata_account(elf: &[u8]) -> Vec<u8> {
        let mut data = bincode::serialize(&UpgradeableLoaderState::ProgramData {
            slot: 123_456,
            upgrade_authority_address: Some(Pubkey::new_unique()),
        })
        .unwrap();
        assert_eq!(
            data.len(),
            UpgradeableLoaderState::size_of_programdata_metadata(),
            "the ProgramData header the dump offset is derived from"
        );
        data.extend_from_slice(elf);
        data.extend_from_slice(&[0_u8; PADDING]);
        data
    }

    // Vectors produced by the coreutils `cksum` binary, so the crate constant stays pinned to it
    #[test]
    fn crc_matches_the_posix_cksum_binary() {
        assert_eq!(
            cksum(b"The quick brown fox jumps over the lazy dog"),
            2_074_844_392
        );
        assert_eq!(cksum(b""), 4_294_967_295);
        let all_bytes: Vec<u8> = (0..=255_u8).collect();
        assert_eq!(cksum(&all_bytes), 1_313_719_201);

        // Without the byte count folded in the CRC is a different, non-reproducible number
        assert_eq!(
            CRC.checksum(b"The quick brown fox jumps over the lazy dog"),
            917_995_649
        );
    }

    #[test]
    fn programdata_metadata_header_is_45_bytes() {
        assert_eq!(UpgradeableLoaderState::size_of_programdata_metadata(), 45);
    }

    #[test]
    fn strips_the_header_and_keeps_the_zero_padding() {
        let elf = bytes_1_to_32();
        let account = programdata_account(&elf);
        assert_eq!(account.len(), 45 + elf.len() + PADDING);

        let dump = dump_from_programdata(&account).unwrap();
        assert_eq!(dump.len(), elf.len() + PADDING);
        assert_eq!(&dump[..elf.len()], elf.as_slice());
        assert!(dump[elf.len()..].iter().all(|byte| *byte == 0));
    }

    // The same bytes fed to `cksum` after `solana program dump`
    #[test]
    fn checksums_the_dumped_bytes() {
        assert_eq!(
            cksum(dump_from_programdata(&programdata_account(&bytes_1_to_32())).unwrap()),
            2_964_876_213
        );
        assert_eq!(
            cksum(dump_from_programdata(&programdata_account(&priority_payload())).unwrap()),
            3_844_531_109
        );
    }

    #[test]
    fn combined_hash_dot_joins_the_per_program_crcs_in_order() {
        let tip = (Pubkey::new_unique(), "2964876213".to_string());
        let priority = (Pubkey::new_unique(), "3844531109".to_string());

        assert_eq!(
            JitoProgramHash::new(vec![tip.clone(), priority.clone()]).combined,
            "2964876213.3844531109"
        );
        assert_eq!(
            JitoProgramHash::new(vec![priority, tip]).combined,
            "3844531109.2964876213"
        );
    }

    #[test]
    fn reads_the_programdata_address_of_a_program_account() {
        let programdata = Pubkey::new_unique();
        let data = bincode::serialize(&UpgradeableLoaderState::Program {
            programdata_address: programdata,
        })
        .unwrap();
        assert_eq!(data.len(), UpgradeableLoaderState::size_of_program());
        assert_eq!(programdata_address(&data).unwrap(), programdata);
    }

    #[test]
    fn rejects_accounts_of_the_wrong_upgradeable_loader_state() {
        let buffer = bincode::serialize(&UpgradeableLoaderState::Buffer {
            authority_address: Some(Pubkey::new_unique()),
        })
        .unwrap();
        assert!(programdata_address(&buffer).is_err());
        assert!(dump_from_programdata(&buffer).is_err());

        let program = bincode::serialize(&UpgradeableLoaderState::Program {
            programdata_address: Pubkey::new_unique(),
        })
        .unwrap();
        assert!(dump_from_programdata(&program).is_err());
    }

    #[test]
    fn rejects_a_programdata_account_without_a_body() {
        let header = bincode::serialize(&UpgradeableLoaderState::ProgramData {
            slot: 1,
            upgrade_authority_address: Some(Pubkey::new_unique()),
        })
        .unwrap();
        assert_eq!(
            header.len(),
            UpgradeableLoaderState::size_of_programdata_metadata()
        );
        assert!(dump_from_programdata(&header).is_err());
        assert!(dump_from_programdata(&[]).is_err());
    }

    fn bytes_1_to_32() -> Vec<u8> {
        (1..=32_u8).collect()
    }

    fn priority_payload() -> Vec<u8> {
        let mut payload = b"priority-fee elf payload".to_vec();
        payload.push(7);
        payload
    }
}
