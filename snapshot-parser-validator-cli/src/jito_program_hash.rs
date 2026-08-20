//! sha256 over the deployed tip-distribution and priority-fee program ELFs, in that
//! order: programdata bytes after the 45 byte loader metadata, trailing zeros stripped.
//! The stakes ETL pins this scheme and finds the upload by the first 16 hex chars.

use {
    log::info,
    solana_loader_v3_interface::state::UpgradeableLoaderState,
    solana_program::{
        hash::{hash, hashv},
        pubkey::Pubkey,
    },
    solana_runtime::bank::Bank,
    solana_sdk::account::ReadableAccount,
    std::sync::Arc,
};

pub const PROGRAM_HASH_SHORT_LEN: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JitoProgramHash {
    pub combined: String,
    pub per_program: Vec<(Pubkey, String)>,
}

impl JitoProgramHash {
    pub fn short(&self) -> &str {
        &self.combined[..PROGRAM_HASH_SHORT_LEN]
    }
}

pub fn compute_jito_program_hash(
    bank: &Arc<Bank>,
    program_ids: &[Pubkey],
) -> anyhow::Result<JitoProgramHash> {
    let elfs = program_ids
        .iter()
        .map(|program_id| program_elf(bank, *program_id))
        .collect::<anyhow::Result<Vec<_>>>()?;

    let per_program = program_ids
        .iter()
        .zip(elfs.iter())
        .map(|(program_id, elf)| (*program_id, to_hex(&hash(elf).to_bytes())))
        .collect();
    let elf_slices: Vec<&[u8]> = elfs.iter().map(Vec::as_slice).collect();
    let combined = to_hex(&hashv(&elf_slices).to_bytes());

    let program_hash = JitoProgramHash {
        combined,
        per_program,
    };
    for (program_id, program_hash) in &program_hash.per_program {
        info!("Jito program {program_id} ELF sha256: {program_hash}");
    }
    info!(
        "Jito program hash: {} (short: {})",
        program_hash.combined,
        program_hash.short()
    );

    Ok(program_hash)
}

fn program_elf(bank: &Arc<Bank>, program_id: Pubkey) -> anyhow::Result<Vec<u8>> {
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
    let elf = elf_from_programdata(programdata_account.data()).map_err(|err| {
        anyhow::anyhow!(
            "Jito programdata account {programdata_address} of program {program_id}: {err}"
        )
    })?;

    info!(
        "Jito program {program_id} deployed from programdata {programdata_address}: {} ELF bytes",
        elf.len()
    );
    Ok(elf.to_vec())
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

// The account is over-allocated so a bigger upgrade fits; the spare room is zero padding
fn elf_from_programdata(data: &[u8]) -> anyhow::Result<&[u8]> {
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
    let elf_len = body
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |last| last + 1);
    if elf_len == 0 {
        anyhow::bail!("no ELF bytes found after the {header_len} byte ProgramData header");
    }

    Ok(&body[..elf_len])
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;

    bytes.iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
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
            "the ProgramData header the ELF offset is derived from"
        );
        data.extend_from_slice(elf);
        data.extend_from_slice(&[0_u8; PADDING]);
        data
    }

    #[test]
    fn programdata_metadata_header_is_45_bytes() {
        assert_eq!(UpgradeableLoaderState::size_of_programdata_metadata(), 45);
    }

    #[test]
    fn strips_the_header_and_the_zero_padding() {
        let elf = bytes_1_to_32();
        let account = programdata_account(&elf);
        assert_eq!(account.len(), 45 + elf.len() + PADDING);
        assert_eq!(elf_from_programdata(&account).unwrap(), elf.as_slice());
    }

    #[test]
    fn hashes_the_sliced_elf_bytes() {
        let tip_elf = bytes_1_to_32();
        let priority_elf = priority_payload();

        assert_eq!(
            to_hex(&hash(elf_from_programdata(&programdata_account(&tip_elf)).unwrap()).to_bytes()),
            "ae216c2ef5247a3782c135efa279a3e4cdc61094270f5d2be58c6204b7a612c9"
        );
        assert_eq!(
            to_hex(
                &hash(elf_from_programdata(&programdata_account(&priority_elf)).unwrap())
                    .to_bytes()
            ),
            "baefa1f403ba2fd6aac32e6be36821e8f3253de02aad07f804a349aaefc60b24"
        );
        assert_eq!(
            to_hex(&hashv(&[tip_elf.as_slice(), priority_elf.as_slice()]).to_bytes()),
            "e4e1ba6f03b8a9c60c2875d51703481abd602e6308a8f51a8cc1d6965c81a3ae"
        );
    }

    #[test]
    fn combined_hash_depends_on_the_program_order() {
        let tip_elf = bytes_1_to_32();
        let priority_elf = priority_payload();
        assert_ne!(
            to_hex(&hashv(&[tip_elf.as_slice(), priority_elf.as_slice()]).to_bytes()),
            to_hex(&hashv(&[priority_elf.as_slice(), tip_elf.as_slice()]).to_bytes())
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
        assert!(elf_from_programdata(&buffer).is_err());

        let program = bincode::serialize(&UpgradeableLoaderState::Program {
            programdata_address: Pubkey::new_unique(),
        })
        .unwrap();
        assert!(elf_from_programdata(&program).is_err());
    }

    #[test]
    fn rejects_a_programdata_account_without_elf_bytes() {
        assert!(elf_from_programdata(&programdata_account(&[])).is_err());
        assert!(elf_from_programdata(&[]).is_err());
    }

    #[test]
    fn short_hash_is_the_first_16_hex_chars() {
        let program_hash = JitoProgramHash {
            combined: "5dcd612ee1fd3aa36f2d67039a1e64093e2687f7331494fc8bcaed8070b28056"
                .to_string(),
            per_program: vec![],
        };
        assert_eq!(program_hash.combined.len(), 64);
        assert_eq!(program_hash.short(), "5dcd612ee1fd3aa3");
        assert_eq!(program_hash.short().len(), PROGRAM_HASH_SHORT_LEN);
    }

    #[test]
    fn formats_bytes_as_lowercase_hex() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
        assert_eq!(to_hex(&[]), "");
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
