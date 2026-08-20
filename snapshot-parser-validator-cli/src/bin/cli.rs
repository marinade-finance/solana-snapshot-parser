use env_logger::{Builder, Env};
use log::LevelFilter;
use snapshot_parser::stake_meta;
use snapshot_parser::utils::write_to_json_file;
use snapshot_parser_validator_cli::jito_mev::JITO_PROGRAM;
use snapshot_parser_validator_cli::jito_stake_meta::JITO_TIP_PAYMENT_PROGRAM;
use snapshot_parser_validator_cli::scanned_accounts::scan_required_accounts;
use snapshot_parser_validator_cli::{jito_stake_meta, validator_meta};
use solana_program::pubkey::Pubkey;
use std::thread::spawn;
use {
    clap::Parser,
    log::{error, info},
    snapshot_parser::bank_loader::create_bank_from_ledger,
    snapshot_parser::cli::path_parser,
    std::path::{Path, PathBuf},
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

    /// Jito tip-distribution program id (defaults to mainnet); override for other clusters
    #[arg(long, env, default_value = JITO_PROGRAM)]
    tip_distribution_program: Pubkey,

    /// Jito tip-payment program id (defaults to mainnet); override for other clusters
    #[arg(long, env, default_value = JITO_TIP_PAYMENT_PROGRAM)]
    tip_payment_program: Pubkey,

    /// Treat missing Jito priority-fee distribution data as fatal; disable for
    /// clusters (e.g. testnet) that have no priority-fee accounts
    #[arg(long, env, action = clap::ArgAction::Set, default_value_t = true)]
    require_priority_fee_data: bool,

    /// Treat a failed Jito stake meta collection as fatal; requires --output-jito-stake-meta
    #[arg(long, env, action = clap::ArgAction::Set, default_value_t = false)]
    require_jito_stake_meta: bool,
}

impl Args {
    fn validate(&self) -> anyhow::Result<()> {
        if self.require_jito_stake_meta && self.output_jito_stake_meta.is_none() {
            anyhow::bail!("--require-jito-stake-meta true needs --output-jito-stake-meta, otherwise the required collection is never produced");
        }

        Ok(())
    }
}

fn hash_named_path(output_jito_stake_meta: &str, jito_program_hash: &str) -> String {
    let path = Path::new(output_jito_stake_meta);
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let file_name = match path.extension() {
        Some(extension) => format!("{stem}-{jito_program_hash}.{}", extension.to_string_lossy()),
        None => format!("{stem}-{jito_program_hash}"),
    };

    path.with_file_name(file_name)
        .to_string_lossy()
        .into_owned()
}

fn main() -> anyhow::Result<()> {
    let mut builder = Builder::from_env(Env::default().default_filter_or("info"));
    builder.filter_module("solana_metrics::metrics", LevelFilter::Error);
    builder.init();

    info!("Starting snapshot parser...");
    let args: Args = Args::parse();
    args.validate()?;

    info!("Creating bank from ledger path: {:?}", args.ledger_path);
    let bank = create_bank_from_ledger(&args.ledger_path)?;

    info!("Scanning accounts from the bank...");
    let scanned_accounts = scan_required_accounts(
        &bank,
        args.verify_account_scan,
        args.tip_distribution_program,
    )?;

    let require_priority_fee_data = args.require_priority_fee_data;
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
                    require_priority_fee_data,
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

    let tip_distribution_program = args.tip_distribution_program;
    let tip_payment_program = args.tip_payment_program;
    let require_jito_stake_meta = args.require_jito_stake_meta;
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
                        tip_distribution_program,
                        tip_payment_program,
                        require_priority_fee_data,
                    )?;
                let hash_named_output =
                    hash_named_path(&output_path, &jito_stake_meta_collection.jito_program_hash);
                // Jito publishes this collection pretty-printed
                write_to_json_file(&jito_stake_meta_collection, &hash_named_output)?;
                info!("Jito stake meta collection finished, written to {hash_named_output}.");
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

    if let Some(handle) = jito_stake_meta_collection_handle {
        let outcome = match handle.join() {
            Ok(Ok(())) => {
                info!("Jito stake meta collection completed successfully.");
                None
            }
            Ok(Err(err)) => Some(format!("Error in Jito stake meta thread: {err:?}")),
            Err(err) => Some(format!("Jito stake meta thread panicked: {err:?}")),
        };
        if let Some(outcome) = outcome {
            error!("{outcome}");
            if require_jito_stake_meta {
                failure = failure.or(Some(outcome));
            }
        }
    }

    if let Some(failure) = failure {
        anyhow::bail!(failure);
    }

    info!("Finished.");
    log::logger().flush();

    // Outputs are already flushed and synced, so skip the accounts-db teardown of a dying process
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use snapshot_parser::utils::read_from_json_file;
    use snapshot_parser_validator_cli::jito_stake_meta::JitoStakeMetaCollection;
    use std::fs;

    // Replays the testnet Parse step's exact argv so the CLI contract is checked
    // without a snapshot download; would have caught --require-priority-fee-data
    // being parsed as a bare bool flag.
    #[test]
    fn parses_testnet_parse_step_argv() {
        let args = Args::try_parse_from([
            "snapshot-parser-validator-cli",
            "--ledger-path",
            ".",
            "--output-validator-meta-collection",
            "./validators.json",
            "--output-stake-meta-collection",
            "./stakes.json",
            "--output-jito-stake-meta",
            "./jito-stake-meta.json",
            "--tip-distribution-program",
            "DzvGET57TAgEDxvm3ERUM4GNcsAJdqjDLCne9sdfY4wf",
            "--tip-payment-program",
            "GJHtFqM9agxPmkeKjHny6qiRKrXZALvvFGiKf11QE7hy",
            "--require-priority-fee-data",
            "false",
            "--require-jito-stake-meta",
            "false",
        ])
        .expect("testnet Parse step argv must parse");
        args.validate()
            .expect("testnet Parse step argv must be valid");

        assert!(!args.require_priority_fee_data);
        assert!(!args.require_jito_stake_meta);
        assert_eq!(
            args.tip_distribution_program,
            "DzvGET57TAgEDxvm3ERUM4GNcsAJdqjDLCne9sdfY4wf"
                .parse::<Pubkey>()
                .unwrap()
        );
        assert_eq!(
            args.tip_payment_program,
            "GJHtFqM9agxPmkeKjHny6qiRKrXZALvvFGiKf11QE7hy"
                .parse::<Pubkey>()
                .unwrap()
        );
        assert_eq!(
            hash_named_path(&args.output_jito_stake_meta.unwrap(), HASH),
            "./jito-stake-meta-2426260379.1775319386.json",
            "the Parse step globs ./jito-stake-meta-*.json for exactly this name"
        );
    }

    // Mainnet keeps the strict default when the flag is omitted.
    #[test]
    fn require_priority_fee_data_defaults_true() {
        let args = Args::try_parse_from([
            "snapshot-parser-validator-cli",
            "--ledger-path",
            ".",
            "--output-validator-meta-collection",
            "./validators.json",
            "--output-stake-meta-collection",
            "./stakes.json",
        ])
        .expect("minimal argv must parse");
        args.validate().expect("minimal argv must be valid");
        assert!(args.require_priority_fee_data);
        assert!(!args.require_jito_stake_meta);
    }

    #[test]
    fn require_jito_stake_meta_without_an_output_is_rejected() {
        let args = Args::try_parse_from([
            "snapshot-parser-validator-cli",
            "--ledger-path",
            ".",
            "--output-validator-meta-collection",
            "./validators.json",
            "--output-stake-meta-collection",
            "./stakes.json",
            "--require-jito-stake-meta",
            "true",
        ])
        .expect("argv parses, the combination is rejected by validation");

        let err = args
            .validate()
            .expect_err("--require-jito-stake-meta true must not be a silent no-op");
        assert!(
            err.to_string().contains("--output-jito-stake-meta"),
            "the error must name the missing flag: {err}"
        );
    }

    #[test]
    fn not_requiring_jito_stake_meta_without_an_output_is_allowed() {
        let args = Args::try_parse_from([
            "snapshot-parser-validator-cli",
            "--ledger-path",
            ".",
            "--output-validator-meta-collection",
            "./validators.json",
            "--output-stake-meta-collection",
            "./stakes.json",
            "--require-jito-stake-meta",
            "false",
        ])
        .expect("argv must parse");
        args.validate().expect("argv must be valid");
        assert!(!args.require_jito_stake_meta);
    }

    const HASH: &str = "2426260379.1775319386";

    #[test]
    fn the_hash_goes_before_the_extension() {
        assert_eq!(
            hash_named_path("./jito-stake-meta.json", HASH),
            "./jito-stake-meta-2426260379.1775319386.json"
        );
        assert_eq!(
            hash_named_path("jito-stake-meta.json", HASH),
            "jito-stake-meta-2426260379.1775319386.json"
        );
        assert_eq!(
            hash_named_path("/mnt/out/jito-stake-meta.json", HASH),
            "/mnt/out/jito-stake-meta-2426260379.1775319386.json"
        );
    }

    #[test]
    fn a_dotted_name_splits_at_the_last_extension() {
        assert_eq!(
            hash_named_path("./jito.stake.meta.json", HASH),
            "./jito.stake.meta-2426260379.1775319386.json"
        );
    }

    #[test]
    fn a_path_without_an_extension_gets_the_hash_appended() {
        assert_eq!(
            hash_named_path("./jito-stake-meta", HASH),
            "./jito-stake-meta-2426260379.1775319386"
        );
        assert_eq!(
            hash_named_path("/mnt/out/jito-stake-meta", HASH),
            "/mnt/out/jito-stake-meta-2426260379.1775319386"
        );
    }

    #[test]
    fn the_written_file_is_named_by_the_hash_it_carries() {
        let dir = std::env::temp_dir().join(format!(
            "jito-stake-meta-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let collection = JitoStakeMetaCollection {
            stake_metas: vec![],
            tip_distribution_program_id: Pubkey::new_unique(),
            priority_fee_distribution_program_id: Pubkey::new_unique(),
            bank_hash: "hash".to_string(),
            epoch: 1002,
            slot: 433295999,
            jito_program_hash: HASH.to_string(),
        };

        let out = hash_named_path(
            &dir.join("jito-stake-meta.json").to_string_lossy(),
            &collection.jito_program_hash,
        );
        write_to_json_file(&collection, &out).unwrap();

        let written: JitoStakeMetaCollection = read_from_json_file(&out).unwrap();
        assert_eq!(
            Path::new(&out).file_name().unwrap().to_string_lossy(),
            format!("jito-stake-meta-{}.json", written.jito_program_hash),
            "the GCS object name the stakes ETL derives from the hash must be the written file"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parses_mainnet_parse_step_argv() {
        let args = Args::try_parse_from([
            "snapshot-parser-validator-cli",
            "--ledger-path",
            ".",
            "--output-validator-meta-collection",
            "./validators.json",
            "--output-stake-meta-collection",
            "./stakes.json",
            "--output-jito-stake-meta",
            "./jito-stake-meta.json",
            "--require-jito-stake-meta",
            "true",
        ])
        .expect("mainnet Parse step argv must parse");
        args.validate()
            .expect("mainnet Parse step argv must be valid");

        assert!(args.require_jito_stake_meta);
        assert!(args.require_priority_fee_data);
        assert_eq!(
            hash_named_path(&args.output_jito_stake_meta.unwrap(), HASH),
            "./jito-stake-meta-2426260379.1775319386.json",
            "the Parse step globs ./jito-stake-meta-*.json for exactly this name"
        );
    }
}
