use {
    agave_snapshots::snapshot_config::{SnapshotConfig, SnapshotUsage},
    log::info,
    solana_accounts_db::{accounts_db::AccountsDbConfig, accounts_index::AccountsIndexConfig},
    solana_genesis_utils::{open_genesis_config, MAX_GENESIS_ARCHIVE_UNPACKED_SIZE},
    solana_ledger::{bank_forks_utils, blockstore_processor::ProcessOptions},
    solana_runtime::bank::Bank,
    solana_sdk::hash::Hash,
    std::{
        fs,
        path::{Path, PathBuf},
        sync::{atomic::AtomicBool, Arc},
    },
};

// The genesis hash names the cluster; a cached genesis of another cluster (or of a reset one) must not load
pub fn check_genesis_hash(actual: Hash, expected: Option<Hash>) -> anyhow::Result<()> {
    match expected {
        Some(expected) if expected != actual => anyhow::bail!(
            "Genesis hash {actual} is not the expected {expected}: the genesis in the ledger path belongs to another cluster, or the cluster was reset; replace it with the cluster's current genesis"
        ),
        Some(_) => info!("Genesis hash {actual} is the expected one"),
        None => info!("Genesis hash {actual}, no expected hash given to check it against"),
    }
    Ok(())
}

pub fn create_bank_from_ledger(
    ledger_path: &Path,
    expected_genesis_hash: Option<Hash>,
) -> anyhow::Result<Arc<Bank>> {
    let genesis_config = open_genesis_config(ledger_path, MAX_GENESIS_ARCHIVE_UNPACKED_SIZE)?;
    check_genesis_hash(genesis_config.hash(), expected_genesis_hash)?;
    let snapshot_config = SnapshotConfig {
        usage: SnapshotUsage::LoadOnly,
        full_snapshot_archive_interval: agave_snapshots::SnapshotInterval::Disabled,
        incremental_snapshot_archive_interval: agave_snapshots::SnapshotInterval::Disabled,
        full_snapshot_archives_dir: PathBuf::from(ledger_path),
        incremental_snapshot_archives_dir: PathBuf::from(ledger_path),
        bank_snapshots_dir: PathBuf::from(ledger_path),
        ..SnapshotConfig::default()
    };

    let drive_dir = PathBuf::from(ledger_path).join("drive1");
    fs::create_dir_all(&drive_dir).unwrap();

    let account_paths = vec![PathBuf::from(ledger_path).join(Path::new("stake-meta.processors"))];

    // load_bank_forks was split in Agave 4.x: snapshot loading is now
    // try_load_bank_forks_from_snapshot, which takes no blockstore and returns
    // None when no snapshot archive is present.
    let (bank_forks, ..) = bank_forks_utils::try_load_bank_forks_from_snapshot(
        &genesis_config,
        &account_paths,
        &snapshot_config,
        &ProcessOptions {
            slot_callback: Some(Arc::new(|bank| info!("Slot callback: {}", bank.slot()))),
            accounts_db_config: AccountsDbConfig {
                index: Some(AccountsIndexConfig {
                    drives: Some(vec![drive_dir]),
                    ..AccountsIndexConfig::default()
                }),
                ..AccountsDbConfig::default()
            },
            ..ProcessOptions::default()
        },
        None,
        Arc::new(AtomicBool::new(false)),
    )?
    .ok_or_else(|| {
        anyhow::anyhow!(
            "No snapshot archive found to load in ledger path: {}",
            ledger_path.display()
        )
    })?;
    info!("Bank forks loaded.");

    let working_bank = bank_forks.read().unwrap().working_bank();
    info!("Bank slot: {}", working_bank.slot());

    Ok(working_bank)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_genesis_hash_is_checked_only_against_an_expected_one() {
        let actual = Hash::new_unique();
        check_genesis_hash(actual, None).unwrap();
        check_genesis_hash(actual, Some(actual)).unwrap();
        let err = check_genesis_hash(actual, Some(Hash::new_unique())).unwrap_err();
        assert!(err.to_string().contains(&actual.to_string()), "{err}");
    }
}
