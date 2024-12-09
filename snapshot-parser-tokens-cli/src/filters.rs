use base64::engine::general_purpose::STANDARD as base64_engine;
use base64::Engine;
use serde::{Deserialize, Serialize};
use snapshot_parser::utils::read_from_json_file;
use solana_program::pubkey::Pubkey;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Deserialize, Serialize)]
struct FiltersData {
    account_owners: String,
    account_mints: String,
    vsr_registrar_data: String,
}

#[derive(Debug, Clone)]
pub struct Filters {
    pub account_owners: Vec<Pubkey>,
    pub account_mints: Vec<Pubkey>,
    pub vsr_registrar_data: Vec<u8>,
}

impl Filters {
    pub fn load(filters_path: &PathBuf) -> anyhow::Result<Self> {
        let data: FiltersData = read_from_json_file(filters_path)?;
        Ok(Self {
            account_owners: Self::split_pubkeys(&data.account_owners, "account_owners")?,
            account_mints: Self::split_pubkeys(&data.account_mints, "account_mints")?,
            vsr_registrar_data: base64_engine.decode(&data.vsr_registrar_data)?,
        })
    }

    fn split_pubkeys(pubkeys_string: &str, name: &str) -> anyhow::Result<Vec<Pubkey>> {
        pubkeys_string
            .split(',')
            .map(|s| {
                Pubkey::from_str(s).map_err(|e| {
                    anyhow::anyhow!(
                        "Could not parse pubkey from '{}' of name {}: {}",
                        s,
                        name,
                        e
                    )
                })
            })
            .collect()
    }
}
