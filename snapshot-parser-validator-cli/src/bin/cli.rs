use env_logger::{Builder, Env};
use log::LevelFilter;
use snapshot_parser::stake_meta;
use snapshot_parser::utils::write_to_json_file;
use snapshot_parser_validator_cli::validator_meta;
use std::thread::spawn;
use {
    clap::Parser, log::info, snapshot_parser::bank_loader::create_bank_from_ledger,
    snapshot_parser::cli::path_parser, std::path::PathBuf,
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
}

fn main() -> anyhow::Result<()> {
    let mut builder = Builder::from_env(Env::default().default_filter_or("info"));
    builder.filter_module("solana_metrics::metrics", LevelFilter::Error);
    builder.init();

    info!("Starting snapshot parser...");
    let args: Args = Args::parse();

    info!("Creating bank from ledger path: {:?}", &args.ledger_path);
    let bank = create_bank_from_ledger(&args.ledger_path)?;

    let validator_meta_collection_handle = {
        let bank = bank.clone();
        spawn(move || {
            info!("Creating validator meta collection...");

            let call = || -> anyhow::Result<()> {
                let validator_meta_collection =
                    validator_meta::generate_validator_collection(&bank)?;
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
        spawn(move || {
            info!("Creating stake meta collection...");

            let call = || -> anyhow::Result<()> {
                let stake_meta_collection = stake_meta::generate_stake_meta_collection(&bank)?;
                write_to_json_file(&stake_meta_collection, &args.output_stake_meta_collection)?;
                info!("Stake meta collection finished.");
                Ok(())
            };

            call()
        })
    };

    for handle in vec![
        validator_meta_collection_handle,
        stake_meta_collection_handle,
    ] {
        match handle.join() {
            Ok(Ok(())) => info!("Thread completed successfully."),
            Ok(Err(err)) => anyhow::bail!("Error in thread: {err:?}"),
            Err(err) => anyhow::bail!("Thread panicked: {err:?}"),
        }
    }

    info!("Finished.");
    Ok(())
}
