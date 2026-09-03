use base64::engine::general_purpose::STANDARD as base64_engine;
use base64::Engine;
use log::info;
use serde::{Deserialize, Serialize};
use snapshot_parser::utils::read_from_json_file;
use solana_program::pubkey::Pubkey;
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug, Deserialize, Serialize)]
struct FiltersData {
    account_owners: Option<String>,
    account_mints: String,
    vsr_registrar_data: String,
}

#[derive(Debug, Clone)]
pub struct Filters {
    pub account_mints: Vec<Pubkey>,
    pub vsr_registrar_data: Vec<u8>,
}

impl Filters {
    pub fn load(filters_path: &PathBuf) -> anyhow::Result<Self> {
        let data: FiltersData = read_from_json_file(filters_path)?;
        if let Some(account_owners) = &data.account_owners {
            info!(
                "Ignoring deprecated 'account_owners' filter ('{}'): the full account-owner scan is no longer performed",
                account_owners
            );
        }
        Ok(Self {
            account_mints: Self::split_pubkeys(&data.account_mints, "account_mints")?,
            vsr_registrar_data: base64_engine.decode(&data.vsr_registrar_data)?,
        })
    }

    fn split_pubkeys(pubkeys_string: &str, name: &str) -> anyhow::Result<Vec<Pubkey>> {
        let mut pubkeys: Vec<Pubkey> = Vec::new();
        for s in pubkeys_string.split(',') {
            let pubkey = Pubkey::from_str(s).map_err(|e| {
                anyhow::anyhow!(
                    "Could not parse pubkey from '{}' of name {}: {}",
                    s,
                    name,
                    e
                )
            })?;
            if !pubkeys.contains(&pubkey) {
                pubkeys.push(pubkey);
            }
        }
        Ok(pubkeys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
    const MSOL_MINT: &str = "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So";

    struct TempJson(PathBuf);

    impl TempJson {
        fn new(name: &str, content: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "snapshot-parser-tokens-filters-{name}-{}-{:?}.json",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::write(&path, content).unwrap();
            Self(path)
        }
    }

    impl Drop for TempJson {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn loads_filters_with_account_owners() {
        let file = TempJson::new(
            "with-owners",
            &format!(
                r#"{{"account_owners":"{SYSTEM_PROGRAM}","account_mints":"{MSOL_MINT}","vsr_registrar_data":"AQID"}}"#
            ),
        );

        let filters = Filters::load(&file.0).unwrap();

        assert_eq!(
            filters.account_mints,
            vec![Pubkey::from_str(MSOL_MINT).unwrap()]
        );
        assert_eq!(filters.vsr_registrar_data, vec![1, 2, 3]);
    }

    #[test]
    fn loads_filters_without_account_owners() {
        let file = TempJson::new(
            "without-owners",
            &format!(r#"{{"account_mints":"{MSOL_MINT}","vsr_registrar_data":"AQID"}}"#),
        );

        let filters = Filters::load(&file.0).unwrap();

        assert_eq!(
            filters.account_mints,
            vec![Pubkey::from_str(MSOL_MINT).unwrap()]
        );
        assert_eq!(filters.vsr_registrar_data, vec![1, 2, 3]);
    }

    #[test]
    fn loads_filters_with_empty_account_owners() {
        let file = TempJson::new(
            "empty-owners",
            &format!(
                r#"{{"account_owners":"","account_mints":"{MSOL_MINT}","vsr_registrar_data":""}}"#
            ),
        );

        let filters = Filters::load(&file.0).unwrap();

        assert_eq!(
            filters.account_mints,
            vec![Pubkey::from_str(MSOL_MINT).unwrap()]
        );
        assert!(filters.vsr_registrar_data.is_empty());
    }

    #[test]
    fn parses_multiple_account_mints() {
        let file = TempJson::new(
            "multi-mints",
            &format!(
                r#"{{"account_mints":"{MSOL_MINT},{SYSTEM_PROGRAM}","vsr_registrar_data":"AQID"}}"#
            ),
        );

        let filters = Filters::load(&file.0).unwrap();

        assert_eq!(
            filters.account_mints,
            vec![
                Pubkey::from_str(MSOL_MINT).unwrap(),
                Pubkey::from_str(SYSTEM_PROGRAM).unwrap(),
            ]
        );
    }

    #[test]
    fn a_repeated_account_mint_is_kept_once() {
        let file = TempJson::new(
            "duplicate-mints",
            &format!(
                r#"{{"account_mints":"{MSOL_MINT},{SYSTEM_PROGRAM},{MSOL_MINT}","vsr_registrar_data":"AQID"}}"#
            ),
        );

        let filters = Filters::load(&file.0).unwrap();

        assert_eq!(
            filters.account_mints,
            vec![
                Pubkey::from_str(MSOL_MINT).unwrap(),
                Pubkey::from_str(SYSTEM_PROGRAM).unwrap(),
            ]
        );
    }

    #[test]
    fn rejects_malformed_account_mints() {
        let file = TempJson::new(
            "bad-mints",
            r#"{"account_mints":"not-a-pubkey","vsr_registrar_data":"AQID"}"#,
        );

        let err = Filters::load(&file.0).unwrap_err().to_string();

        assert!(err.contains("account_mints"), "unexpected error: {err}");
    }
}
