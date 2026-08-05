use env_logger::{Builder, Env};
use log::LevelFilter;
use snapshot_parser::stake_meta;
use snapshot_parser::utils::write_to_json_file;
use snapshot_parser_validator_cli::scanned_accounts::scan_required_accounts;
use snapshot_parser_validator_cli::{jito_stake_meta, validator_meta};
use std::thread::spawn;
use {
    clap::Parser,
    log::{error, info},
    snapshot_parser::bank_loader::create_bank_from_ledger,
    snapshot_parser::cli::path_parser,
    std::path::PathBuf,
};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the directory where the snapshot is unpacked (e.g., from .tar.zst)
    #[arg(long, env, value_parser = path_parser)]
    ledger_path: PathBuf,

    /// Path to write JSON file to for the validator metas (e.g., validators.json)
    #[arg(long, env)]
    output_validator_meta_collection: String,

    /// Path to write JSON file to for the stake metas (e.g., stakes.json)
    #[arg(long, env)]
    output_stake_meta_collection: String,

    /// Path to write JSON file to for the Jito-format stake metas (e.g., jito-stake-meta.json)
    #[arg(long, env)]
    output_jito_stake_meta: Option<String>,

    /// Cross-check the single-pass account scan against get_program_accounts; costs a scan per owner
    #[arg(long, env, default_value_t = false)]
    verify_account_scan: bool,
}

fn main() -> anyhow::Result<()> {
    let mut builder = Builder::from_env(Env::default().default_filter_or("info"));
    builder.filter_module("solana_metrics::metrics", LevelFilter::Error);
    builder.init();

    info!("Starting snapshot parser...");
    let args: Args = Args::parse();

    info!("Creating bank from ledger path: {:?}", args.ledger_path);
    let bank = create_bank_from_ledger(&args.ledger_path)?;

    info!("Scanning accounts from the bank...");
    let scanned_accounts = scan_required_accounts(&bank, args.verify_account_scan)?;

    let validator_meta_collection_handle = {
        let bank = bank.clone();
        let tip_distribution = scanned_accounts.tip_distribution.clone();
        let priority_fee_distribution = scanned_accounts.priority_fee_distribution.clone();
        spawn(move || {
            info!("Creating validator meta collection...");

            let call = || -> anyhow::Result<()> {
                let validator_meta_collection = validator_meta::generate_validator_collection(
                    &bank,
                    &tip_distribution,
                    &priority_fee_distribution,
                )?;
                write_to_json_file(
                    &validator_meta_collection,
                    &args.output_validator_meta_collection,
                )?;
                info!("Validator meta collection finished.");
                Ok(())
            };

            call()
        })
    };

    let stake_meta_collection_handle = {
        let bank = bank.clone();
        let stake_accounts = scanned_accounts.stake.clone();
        spawn(move || {
            info!("Creating stake meta collection...");

            let call = || -> anyhow::Result<()> {
                let stake_meta_collection =
                    stake_meta::generate_stake_meta_collection_for_accounts(
                        &bank,
                        &stake_accounts,
                    )?;
                write_to_json_file(&stake_meta_collection, &args.output_stake_meta_collection)?;
                info!("Stake meta collection finished.");
                Ok(())
            };

            call()
        })
    };

    let jito_stake_meta_collection_handle = args.output_jito_stake_meta.map(|output_path| {
        let bank = bank.clone();
        let stake_accounts = scanned_accounts.stake.clone();
        let tip_distribution = scanned_accounts.tip_distribution.clone();
        let priority_fee_distribution = scanned_accounts.priority_fee_distribution.clone();
        spawn(move || {
            info!("Creating Jito stake meta collection...");

            let call = || -> anyhow::Result<()> {
                let jito_stake_meta_collection =
                    jito_stake_meta::generate_jito_stake_meta_collection(
                        &bank,
                        &stake_accounts,
                        &tip_distribution,
                        &priority_fee_distribution,
                    )?;
                // Jito publishes this collection pretty-printed; parity is checked byte for byte
                write_to_json_file(&jito_stake_meta_collection, &output_path)?;
                info!("Jito stake meta collection finished.");
                Ok(())
            };

            call()
        })
    });

    // Every handle must be joined before returning, otherwise process exit kills a thread mid-write
    let mut failure = None;
    for handle in [
        validator_meta_collection_handle,
        stake_meta_collection_handle,
    ] {
        let outcome = match handle.join() {
            Ok(Ok(())) => {
                info!("Thread completed successfully.");
                continue;
            }
            Ok(Err(err)) => format!("Error in thread: {err:?}"),
            Err(err) => format!("Thread panicked: {err:?}"),
        };
        error!("{outcome}");
        failure = failure.or(Some(outcome));
    }

    // The Jito collection is a backup of what Jito publishes itself, it must not fail the parsing
    if let Some(handle) = jito_stake_meta_collection_handle {
        match handle.join() {
            Ok(Ok(())) => info!("Jito stake meta collection completed successfully."),
            Ok(Err(err)) => error!("Error in Jito stake meta thread: {err:?}"),
            Err(err) => error!("Jito stake meta thread panicked: {err:?}"),
        }
    }

    if let Some(failure) = failure {
        anyhow::bail!(failure);
    }

    info!("Finished.");
    Ok(())
}
