use clap::Parser;
use env_logger::{Builder, Env};
use indicatif::MultiProgress;
use log::LevelFilter;
use log::{debug, info};
use snapshot_parser::bank_loader::create_bank_from_ledger;
use snapshot_parser::cli::path_parser;
use snapshot_parser_tokens_cli::db_message::DbMessage;
use snapshot_parser_tokens_cli::filters::Filters;
use snapshot_parser_tokens_cli::processors::account_owners::ProcessorAccountOwners;
use snapshot_parser_tokens_cli::processors::{
    spawn_processor_task, ProcessorMint, ProcessorNativeStake, ProcessorToken,
    ProcessorTokenMetadata, ProcessorVeMnde, META_ACCOUNT_TABLE, NATIVE_STAKE_ACCOUNT_TABLE,
    TOKEN_ACCOUNT_TABLE, TOKEN_METADATA_ACCOUNT_TABLE, VE_MNDE_ACCOUNT_TABLE,
};
use snapshot_parser_tokens_cli::progress_bar::ProgressCounter;
use snapshot_parser_tokens_cli::stats::Stats;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::{self};
use tokio::sync::oneshot;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the directory where the snapshot is unpacked (e.g., from .tar.zst)
    #[arg(long, env, value_parser = path_parser)]
    ledger_path: PathBuf,

    /// Path to SQLite DB data to write to (e.g., snapshot.db)
    #[arg(long, env)]
    output_sqlite: String,

    /// Path to filters file generated by solana-snapshot-manager CLI
    #[arg(long, env, value_parser = path_parser)]
    filters: PathBuf,

    /// Tokio Sender/receiver channel size for communication
    #[arg(long)]
    channel_size: Option<usize>,

    /// SQLite3 cache size in MB
    #[arg(long)]
    sqlite_cache_size: Option<i64>,

    // SQLite3 memory mapped IO file size in MB, 0 means to disable
    #[arg(long)]
    sqlite_mmap_size: Option<u16>,

    /// Processing in transaction bulks. This is number of inserts in one transaction.
    #[arg(long)]
    sqlite_tx_bulk: Option<u16>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut builder = Builder::from_env(Env::default().default_filter_or("info"));
    builder.filter_module("solana_metrics::metrics", LevelFilter::Error);
    builder.init();
    let args: Args = Args::parse();

    let now = SystemTime::now();
    let since_the_epoch = now.duration_since(UNIX_EPOCH)?;
    let current_timestamp = since_the_epoch.as_secs() as i64;

    info!(
        "Starting snapshot parser for tokens at timestamp {}",
        current_timestamp
    );

    info!("Loading filters from: {:?}", &args.filters);
    let filters = Filters::load(&args.filters)?;

    // let solana_ledger::genesis_utils::GenesisConfigInfo { genesis_config, .. } =
    //     solana_ledger::genesis_utils::create_genesis_config(100);
    // let bank: Arc<solana_runtime::bank::Bank> = Arc::new(solana_runtime::bank::Bank::new_for_tests(&genesis_config));
    info!("Creating bank from ledger path: {:?}", &args.ledger_path);
    let bank = create_bank_from_ledger(&args.ledger_path)?;
    assert!(bank.is_frozen());
    info!(
        "Bank created. Epoch: {}, slot: {}, hash: {}, timestamp from genesis: {}",
        bank.epoch(),
        bank.slot(),
        bank.hash(),
        bank.unix_timestamp_from_genesis()
    );

    info!("Creating progress bar instance...");
    let stats = Stats::new();
    let multi_progress = MultiProgress::new();
    let db_progress_counter = define_counter("db_execute", &multi_progress, &stats).await;
    let account_owners_counter = define_counter(META_ACCOUNT_TABLE, &multi_progress, &stats).await;
    let token_counter = define_counter(TOKEN_ACCOUNT_TABLE, &multi_progress, &stats).await;
    let token_metadata_counter =
        define_counter(TOKEN_METADATA_ACCOUNT_TABLE, &multi_progress, &stats).await;
    let vemnde_counter = define_counter(VE_MNDE_ACCOUNT_TABLE, &multi_progress, &stats).await;
    let native_stake_counter =
        define_counter(NATIVE_STAKE_ACCOUNT_TABLE, &multi_progress, &stats).await;

    let channel_size = args.channel_size.unwrap_or(1000);
    info!("Creating communication channels size {}...", channel_size);
    let (sender, receiver) = mpsc::channel(channel_size);

    let (consumer_ready_tx, consumer_ready_rx) = oneshot::channel();
    let db_handle: tokio::task::JoinHandle<anyhow::Result<()>> = {
        tokio::spawn(async move {
            info!("Starting SQLite executor task...");
            consumer_ready_tx
                .send(())
                .expect("Failed to send ready signal");
            let db = snapshot_parser_tokens_cli::db_connection::SQLiteExecutor::new(
                PathBuf::from(&args.output_sqlite),
                args.sqlite_cache_size,
                args.sqlite_mmap_size,
                args.sqlite_tx_bulk,
                db_progress_counter,
                receiver,
            )?;
            db.start().await;
            debug!("SQLite executor task finished");
            Ok(())
        })
    };
    consumer_ready_rx
        .await
        .expect("Failed to receive SQLite ready signal");

    let account_owners_handle = spawn_processor_task(
        ProcessorAccountOwners::new(
            bank.clone(),
            sender.clone(),
            &filters,
            account_owners_counter.clone(),
        )
        .await?,
    )
    .await?;

    let token_handle = spawn_processor_task(
        ProcessorToken::new(
            bank.clone(),
            sender.clone(),
            &filters,
            account_owners_counter,
            token_counter.clone(),
        )
        .await?,
    )
    .await?;

    let mint_handle = spawn_processor_task(
        ProcessorMint::new(bank.clone(), sender.clone(), &filters, token_counter).await?,
    )
    .await?;

    let vemnde_handle = spawn_processor_task(
        ProcessorVeMnde::new(
            bank.clone(),
            sender.clone(),
            &filters,
            vemnde_counter,
            current_timestamp,
        )
        .await?,
    )
    .await?;

    let native_stake_handle = spawn_processor_task(
        ProcessorNativeStake::new(bank.clone(), sender.clone(), native_stake_counter).await?,
    )
    .await?;

    let token_metadata_handle = spawn_processor_task(
        ProcessorTokenMetadata::new(bank.clone(), sender.clone(), token_metadata_counter.clone())
            .await?,
    )
    .await?;

    let _ = tokio::join!(
        account_owners_handle,
        token_handle,
        mint_handle,
        vemnde_handle,
        native_stake_handle,
        token_metadata_handle,
    );

    let (response_tx, response_rx) = oneshot::channel();
    sender
        .send(DbMessage::Shutdown {
            response: response_tx,
        })
        .await?;
    let _ = response_rx.await?;
    drop(sender);
    db_handle.await??;
    let _ = multi_progress;

    stats.print_info().await;

    Ok(())
}

async fn define_counter(
    name: &str,
    multi_progress: &MultiProgress,
    stats: &Stats,
) -> Arc<ProgressCounter> {
    let progress_counter = Arc::new(ProgressCounter::new(multi_progress, name));
    stats.add_callback(progress_counter.clone()).await;
    progress_counter
}