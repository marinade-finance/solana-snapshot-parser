use {
    agave_snapshots::snapshot_config::{SnapshotConfig, SnapshotUsage},
    log::info,
    solana_accounts_db::{accounts_db::AccountsDbConfig, accounts_index::AccountsIndexConfig},
    solana_genesis_utils::{open_genesis_config, MAX_GENESIS_ARCHIVE_UNPACKED_SIZE},
    solana_ledger::{bank_forks_utils, blockstore_processor::ProcessOptions},
    solana_runtime::bank::Bank,
    std::{
        fs,
        path::{Path, PathBuf},
        sync::{atomic::AtomicBool, Arc},
    },
};

pub fn create_bank_from_ledger(ledger_path: &Path) -> anyhow::Result<Arc<Bank>> {
    let genesis_config = open_genesis_config(ledger_path, MAX_GENESIS_ARCHIVE_UNPACKED_SIZE)?;
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
