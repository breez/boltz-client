use std::borrow::Cow::{self, Owned};
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fs, str::FromStr};

use anyhow::{Context, Result, bail};
use bip39::{Language, Mnemonic};
use clap::Parser;
use rustyline::Editor;
use rustyline::error::ReadlineError;
use rustyline::hint::HistoryHinter;
use rustyline::{Completer, Helper, Hinter, Validator, highlight::Highlighter};

use boltz_client::deposit::models::{Deposit, DepositSwap};
use boltz_client::{
    Asset, BoltzConfig, BoltzError, BoltzEventListener, BoltzService, BoltzStorage, BoltzSwapEvent,
    BoltzSwapStatus, DepositConfig, DepositInvoiceResolver, DepositParams, DepositStorage,
    DerivedKeyStore, InvoiceRequest,
};

const PHRASE_FILE_NAME: &str = "phrase";
const HISTORY_FILE_NAME: &str = "history.txt";
const INVOICE_FILE_NAME: &str = "invoice.txt";

// ─── Top-level CLI (startup args only) ─────────────────────────────────
#[derive(Parser)]
#[command(
    name = "boltz-cli",
    about = "Interactive CLI for the Boltz LN -> stablecoin (USDT/USDC) reverse swap flow"
)]
struct Cli {
    /// Use seed-derived (HD) preimages and a global gas signer instead of the
    /// default random per-swap secrets. Requires a mnemonic; recoverable from it.
    #[arg(long, env = "BOLTZ_SEEDED")]
    seeded: bool,

    /// BIP-39 mnemonic (12 or 24 words); if not provided, reads from
    /// data-dir or generates new. Always required: the inbound-deposit key is
    /// HD-derived even when swap secrets are seedless.
    #[arg(long, env = "BOLTZ_MNEMONIC")]
    mnemonic: Option<String>,

    /// Data directory for persisting mnemonic and state.
    #[arg(long, env = "BOLTZ_DATA_DIR", default_value = "./.data-boltz")]
    data_dir: PathBuf,

    /// Boltz referral ID.
    #[arg(long, env = "BOLTZ_REFERRAL_ID", default_value = "breez-sdk")]
    referral_id: String,

    /// Slippage tolerance in basis points (100 = 1%). Defaults to 100.
    #[arg(long)]
    slippage_bps: Option<u32>,
}

// ─── REPL commands (parsed per-line inside the interactive loop) ───────
#[derive(Clone, Parser)]
enum Command {
    /// Show key info (seeded: derived addresses; seedless: a note) and supported destinations.
    Info,

    /// Get current swap limits (min/max sats).
    Limits,

    /// Get a quote for a LN -> stablecoin swap (USDT or USDC; no commitment).
    ///
    /// The destination chain is picked interactively from the set of chains
    /// whose transport accepts the given address.
    Prepare {
        /// Stablecoin amount, 6 decimals (e.g. 1.5 for 1.50 USD). Mutually exclusive with --sats.
        #[arg(long, value_parser = parse_usd_amount, conflicts_with = "sats")]
        usd: Option<u64>,
        /// Input amount in sats. Mutually exclusive with --usd.
        #[arg(long, conflicts_with = "usd")]
        sats: Option<u64>,
        /// Destination address (any transport — the CLI filters supported
        /// chains by the address format).
        destination: String,
    },

    /// Full swap flow: prepare -> create -> wait for payment -> complete.
    Swap {
        /// Stablecoin amount, 6 decimals (e.g. 1.5 for 1.50 USD). Mutually exclusive with --sats.
        #[arg(long, value_parser = parse_usd_amount, conflicts_with = "sats")]
        usd: Option<u64>,
        /// Input amount in sats. Mutually exclusive with --usd.
        #[arg(long, conflicts_with = "usd")]
        sats: Option<u64>,
        /// Destination address (any transport — the CLI filters supported
        /// chains by the address format).
        destination: String,
    },

    /// Accept a degraded quote for a swap stuck waiting for approval.
    Accept {
        /// Swap ID.
        swap_id: String,
    },

    /// Force an immediate cross-chain delivery check for all settling swaps
    /// (CCTP via Circle Iris, OFT via `LayerZero` Scan). Normally runs
    /// automatically on the background poll cadence.
    RefreshDeliveries,

    /// Print the reusable inbound-deposit address.
    DepositAddress,

    /// List open deposits and in-flight deposit swaps.
    Deposits,

    /// List deposits parked awaiting an explicit `retry-parked`.
    Parked,

    /// Re-enter parked deposits into one new lock unit.
    RetryParked,

    /// Exit the interactive shell.
    #[command(hide = true)]
    Exit,
}

// ─── rustyline helper ──────────────────────────────────────────────────
#[derive(Helper, Completer, Hinter, Validator)]
struct CliHelper {
    #[rustyline(Hinter)]
    hinter: HistoryHinter,
}

impl Highlighter for CliHelper {
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Owned("\x1b[1m".to_owned() + hint + "\x1b[m")
    }
}

// ─── main ──────────────────────────────────────────────────────────────
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Ensure data directory exists
    fs::create_dir_all(&cli.data_dir)
        .with_context(|| format!("Failed to create data dir: {}", cli.data_dir.display()))?;

    init_logging(&cli.data_dir)?;

    // A mnemonic is always required: deposits are always enabled and their
    // key is HD-derived even when swap secrets are seedless.
    let mnemonic = match &cli.mnemonic {
        Some(m) => Mnemonic::from_str(m).context("Invalid mnemonic")?,
        None => get_or_create_mnemonic(&cli.data_dir)?,
    };
    let full_seed: [u8; 64] = mnemonic.to_seed("");

    let seed: Option<&[u8]> = cli.seeded.then_some(full_seed.as_slice());
    let deposit_seed = full_seed.to_vec();

    let mut config = BoltzConfig::mainnet(cli.referral_id);
    if let Some(slippage_bps) = cli.slippage_bps {
        config.slippage_bps = slippage_bps;
    }

    // Initialize the service once — WebSocket + SwapManager stay alive for the
    // entire session, handling ongoing swaps in the background.
    let svc = init_service(config, seed, &cli.data_dir, deposit_seed).await?;

    println!(
        "Boltz CLI Interactive Mode ({} mode)",
        if cli.seeded { "seeded" } else { "seedless" }
    );
    println!("Type 'help' for available commands or 'exit' to quit\n");

    run_repl(&svc, seed, &cli.data_dir).await?;

    svc.shutdown().await;
    println!("Goodbye!");
    Ok(())
}

// ─── REPL loop ─────────────────────────────────────────────────────────
async fn run_repl(svc: &BoltzService, seed: Option<&[u8]>, data_dir: &Path) -> Result<()> {
    let history_file = data_dir.join(HISTORY_FILE_NAME);

    let rl = &mut Editor::new()?;
    rl.set_helper(Some(CliHelper {
        hinter: HistoryHinter {},
    }));
    if rl.load_history(&history_file).is_err() {
        // No history yet — that's fine.
    }

    loop {
        let readline = rl.readline("boltz> ");
        match readline {
            Ok(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                rl.add_history_entry(trimmed)?;

                match parse_command(trimmed) {
                    Ok(command) => match execute_command(command, svc, seed).await {
                        Ok(should_continue) => {
                            if !should_continue {
                                break;
                            }
                        }
                        Err(e) => println!("Error: {e}"),
                    },
                    Err(e) => println!("{e}"),
                }
            }
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
            Err(err) => {
                println!("Error: {err:?}");
                break;
            }
        }
    }

    let _ = rl.save_history(&history_file);
    Ok(())
}

fn parse_command(input: &str) -> Result<Command> {
    if input == "exit" || input == "quit" {
        return Ok(Command::Exit);
    }

    let mut args = vec!["boltz-cli".to_string()];
    match shlex::split(input) {
        Some(split_args) => args.extend(split_args),
        None => bail!("Failed to parse input: {input}"),
    }

    Command::try_parse_from(args).map_err(|e| anyhow::anyhow!("{e}"))
}

/// Returns `Ok(true)` to keep the REPL running, `Ok(false)` to exit.
async fn execute_command(
    command: Command,
    svc: &BoltzService,
    seed: Option<&[u8]>,
) -> Result<bool> {
    match command {
        Command::Exit => Ok(false),
        Command::Info => {
            cmd_info(svc, seed)?;
            Ok(true)
        }
        Command::Limits => {
            cmd_limits(svc).await?;
            Ok(true)
        }
        Command::Prepare {
            usd,
            sats,
            destination,
        } => {
            let (chain, asset) = pick_chain_for_address(svc, &destination)?;
            let prepared = prepare(svc, &destination, &chain, asset, usd, sats).await?;
            print_json(&prepared);
            Ok(true)
        }
        Command::Swap {
            usd,
            sats,
            destination,
        } => {
            let (chain, asset) = pick_chain_for_address(svc, &destination)?;
            cmd_swap(svc, &destination, &chain, asset, usd, sats).await?;
            Ok(true)
        }
        Command::Accept { swap_id } => {
            svc.accept_degraded_quote(&swap_id).await?;
            println!("Accepted degraded quote for {swap_id}");
            Ok(true)
        }
        Command::RefreshDeliveries => {
            svc.refresh_pending_deliveries().await?;
            println!("Delivery check complete. Use `info`/status events to see any completions.");
            Ok(true)
        }
        Command::DepositAddress => {
            cmd_deposit_address(svc);
            Ok(true)
        }
        Command::Deposits => {
            cmd_deposits(svc).await?;
            Ok(true)
        }
        Command::Parked => {
            cmd_parked(svc).await?;
            Ok(true)
        }
        Command::RetryParked => {
            cmd_retry_parked(svc).await?;
            Ok(true)
        }
    }
}

// ─── command handlers ──────────────────────────────────────────────────

fn get_or_create_mnemonic(data_dir: &Path) -> Result<Mnemonic> {
    let filename = data_dir.join(PHRASE_FILE_NAME);

    match fs::read_to_string(&filename) {
        Ok(phrase) => {
            let mnemonic = Mnemonic::from_str(phrase.trim())?;
            println!("Loaded mnemonic from {}\n", filename.display());
            Ok(mnemonic)
        }
        Err(e) => {
            if e.kind() != io::ErrorKind::NotFound {
                bail!("Can't read from file: {}, err {e}", filename.display());
            }
            let mnemonic = Mnemonic::from_entropy_in(Language::English, &rand_entropy())?;
            fs::write(&filename, mnemonic.to_string())?;
            println!(
                "Generated new mnemonic (saved to {}):\n  {mnemonic}\n",
                filename.display()
            );
            Ok(mnemonic)
        }
    }
}

async fn init_service(
    config: BoltzConfig,
    seed: Option<&[u8]>,
    data_dir: &Path,
    deposit_seed: Vec<u8>,
) -> Result<BoltzService> {
    let store = Arc::new(FileBoltzStorage::new(data_dir));

    let deposits = Some(DepositParams {
        config: DepositConfig::default(),
        store: store.clone(),
        resolver: Arc::new(PromptingInvoiceResolver::new(data_dir)),
        seed: Some(deposit_seed),
    });

    let svc = match seed {
        Some(seed) => BoltzService::new(config, seed, store, deposits).await,
        None => BoltzService::new_seedless(config, store, deposits).await,
    }
    .context("Failed to initialize BoltzService")?;

    // Register a global listener that prints status updates for all swaps.
    svc.add_event_listener(Box::new(PrintingEventListener))
        .await;

    // Resume any active swaps from a previous run.
    let resumed = svc.resume_swaps().await.context("Failed to resume swaps")?;
    if !resumed.is_empty() {
        println!("Resumed {} active swap(s):", resumed.len());
        for id in &resumed {
            match svc.get_swap(id).await {
                Ok(Some(swap)) => {
                    println!("  [{}] Status: {:?}", swap.id, swap.status);
                    if swap.status == BoltzSwapStatus::Settling {
                        let bridge_ref = swap.bridge_ref.as_deref().unwrap_or("<pending>");
                        println!("    Bridge ref ({:?}): {bridge_ref}", swap.bridge_kind);
                    }
                }
                Ok(None) => println!("  [{id}] (not found)"),
                Err(e) => println!("  [{id}] (failed to load: {e})"),
            }
        }
    }
    Ok(svc)
}

fn cmd_info(svc: &BoltzService, seed: Option<&[u8]>) -> Result<()> {
    match seed {
        Some(seed) => {
            let km = boltz_client::EvmKeyManager::from_seed(seed)?;
            let chain_id =
                u32::try_from(boltz_client::ARBITRUM_CHAIN_ID).context("Chain ID overflow")?;
            let gas = km.derive_gas_signer(chain_id)?;
            let preimage_key = km.derive_preimage_key(chain_id, 0)?;

            println!(
                "EVM Key Info (seeded, Arbitrum, chain_id={}):",
                boltz_client::ARBITRUM_CHAIN_ID
            );
            println!("  Gas signer address:     {}", gas.address_hex());
            println!(
                "  Preimage key[0] pubkey: {}",
                hex::encode(&preimage_key.public_key)
            );
            println!("  Preimage key[0] addr:   {}", preimage_key.address_hex());
        }
        None => {
            println!(
                "Seedless mode: each swap uses a random preimage and a per-swap gas key \
                 (no global derived addresses; secrets live in the local store)."
            );
        }
    }

    let mut dests: Vec<String> = svc
        .supported_destinations()
        .into_iter()
        .map(|d| format!("{} ({})", d.chain_label, d.asset))
        .collect();
    dests.sort();
    println!("\nSupported destinations:\n  {}", dests.join(", "));

    Ok(())
}

async fn cmd_limits(svc: &BoltzService) -> Result<()> {
    let limits = svc.get_limits().await?;
    print_json(&limits);
    Ok(())
}

fn cmd_deposit_address(svc: &BoltzService) {
    match svc.deposit_address() {
        Some(address) => println!("Deposit address: {address}"),
        None => println!("Deposits are not enabled."),
    }
}

async fn cmd_deposits(svc: &BoltzService) -> Result<()> {
    println!("Open deposits:");
    print_json(&svc.list_open_deposits().await?);
    println!("\nActive deposit swaps:");
    print_json(&svc.list_active_deposit_swaps().await?);
    Ok(())
}

async fn cmd_parked(svc: &BoltzService) -> Result<()> {
    print_json(&svc.parked_deposits().await?);
    Ok(())
}

async fn cmd_retry_parked(svc: &BoltzService) -> Result<()> {
    match svc.retry_parked().await? {
        Some(id) => println!("Created new deposit swap: {id}"),
        None => println!("Nothing to retry."),
    }
    Ok(())
}

async fn prepare(
    svc: &BoltzService,
    destination: &str,
    chain: &str,
    asset: Asset,
    usd: Option<u64>,
    sats: Option<u64>,
) -> Result<boltz_client::PreparedSwap> {
    match (usd, sats) {
        (Some(usd_amount), _) => Ok(svc
            .prepare_reverse_swap(destination, chain, asset, usd_amount, None)
            .await?),
        (_, Some(sats_amount)) => Ok(svc
            .prepare_reverse_swap_from_sats(destination, chain, asset, sats_amount, None)
            .await?),
        _ => bail!("Either --usd or --sats must be provided"),
    }
}

/// Pick a destination (chain + asset) by asking which supported destinations
/// can accept the given address, then prompting the user to choose by number.
///
/// Filters via [`BoltzService::destinations_accepting`] so the list contains
/// only transports compatible with the address format, spanning both USDT0
/// (OFT) and USDC (CCTP). Auto-selects if exactly one matches; errors if none.
fn pick_chain_for_address(svc: &BoltzService, destination: &str) -> Result<(String, Asset)> {
    let mut candidates: Vec<(String, (String, Asset))> = svc
        .destinations_accepting(destination)
        .into_iter()
        .map(|d| {
            (
                format!("{} ({})", d.chain_label, d.asset),
                (d.chain_label, d.asset),
            )
        })
        .collect();
    candidates.sort_by(|a, b| a.0.cmp(&b.0));

    match candidates.len() {
        0 => bail!(
            "No supported destination accepts the address '{destination}'. Run `info` for the list."
        ),
        1 => {
            let (name, dest) = candidates.into_iter().next().unwrap();
            println!("Only one destination supports this address: {name} — proceeding.");
            Ok(dest)
        }
        _ => {
            println!("\nWhich destination?");
            for (i, (name, _)) in candidates.iter().enumerate() {
                println!("  {:>2}. {}", i.saturating_add(1), name);
            }
            loop {
                print!("> ");
                io::stdout().flush()?;
                let mut input = String::new();
                io::stdin().read_line(&mut input)?;
                let trimmed = input.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match trimmed.parse::<usize>() {
                    Ok(n) if n >= 1 && n <= candidates.len() => {
                        let idx = n.saturating_sub(1);
                        let (_, dest) = candidates.into_iter().nth(idx).unwrap();
                        return Ok(dest);
                    }
                    _ => println!("Enter a number between 1 and {}.", candidates.len()),
                }
            }
        }
    }
}

async fn cmd_swap(
    svc: &BoltzService,
    destination: &str,
    chain: &str,
    asset: Asset,
    usd: Option<u64>,
    sats: Option<u64>,
) -> Result<()> {
    // Step 1: Prepare
    println!("Fetching quote...\n");
    let prepared = prepare(svc, destination, chain, asset, usd, sats).await?;
    print_json(&prepared);

    // Confirm
    if !confirm("\nProceed with swap?")? {
        println!("Cancelled.");
        return Ok(());
    }

    // Create — swap monitoring starts automatically
    println!("\nCreating swap on Boltz...");
    let created = svc.create_reverse_swap(&prepared).await?;
    println!("\nSwap created:");
    print_json(&created);
    println!("\n>>> PAY THIS INVOICE to continue <<<\n");

    Ok(())
}

/// Global event listener that prints swap status updates to stdout.
struct PrintingEventListener;

#[macros::async_trait]
impl BoltzEventListener for PrintingEventListener {
    async fn on_event(&self, event: BoltzSwapEvent) {
        match &event {
            BoltzSwapEvent::SwapUpdated { swap } => {
                println!("[{}] Status: {:?}", swap.id, swap.status);
                if swap.status == BoltzSwapStatus::Settling {
                    match &swap.bridge_ref {
                        Some(bridge_ref) => {
                            println!("  Bridge ref ({:?}): {bridge_ref}", swap.bridge_kind);
                        }
                        None => println!("  Bridge ref ({:?}): <pending>", swap.bridge_kind),
                    }
                }
                if swap.status.is_terminal() {
                    println!("  Final state:");
                    print_json(swap);
                }
            }
            BoltzSwapEvent::QuoteDegraded {
                swap,
                expected_usd,
                quoted_usd,
            } => {
                println!(
                    "[{}] Quote degraded: expected {} USD, got {} USD. \
                     Call accept_degraded_quote to proceed.",
                    swap.id, expected_usd, quoted_usd
                );
            }
            BoltzSwapEvent::DepositUpdated { deposit } => {
                println!(
                    "[deposit {}] {:?} — {} USDC(6dp) on chain {}",
                    deposit.id, deposit.status, deposit.amount, deposit.chain_id
                );
            }
            BoltzSwapEvent::DepositSwapUpdated { swap } => {
                println!("[deposit-swap {}] {:?}", swap.id, swap.status);
            }
        }
    }
}

/// Manual-testing [`DepositInvoiceResolver`]: prints the request and reads a
/// BOLT11 string from `<data_dir>/invoice.txt`. Rustyline owns stdin on the
/// REPL thread, so this async callback can't prompt interactively — the file
/// is the hand-off point instead; write the invoice there after seeing the
/// printed instructions, and the engine's next retry tick picks it up.
struct PromptingInvoiceResolver {
    invoice_file: PathBuf,
}

impl PromptingInvoiceResolver {
    fn new(data_dir: &Path) -> Self {
        Self {
            invoice_file: data_dir.join(INVOICE_FILE_NAME),
        }
    }
}

#[macros::async_trait]
impl DepositInvoiceResolver for PromptingInvoiceResolver {
    async fn resolve_invoice(&self, request: &InvoiceRequest) -> Result<String, BoltzError> {
        match fs::read_to_string(&self.invoice_file) {
            Ok(contents) if !contents.trim().is_empty() => Ok(contents.trim().to_string()),
            _ => {
                println!(
                    "\n>>> Deposit swap {} needs a BOLT11 invoice for EXACTLY {} sats \
                     (locking {} USDC, 6dp) <<<\n    Write it to {} — this will be retried \
                     automatically.\n",
                    request.deposit_swap_id,
                    request.amount_sats,
                    request.lock_amount,
                    self.invoice_file.display()
                );
                Err(BoltzError::Generic(
                    "no invoice available yet — write one to invoice.txt".to_string(),
                ))
            }
        }
    }
}

// ─── Formatting ────────────────────────────────────────────────────────

const OUTPUT_FIELDS: &[&str] = &[
    "output_amount",
    "output_delivered",
    "expected_output_amount",
];

fn print_json(value: &impl serde::Serialize) {
    let mut json = serde_json::to_value(value).unwrap();
    redact_key_source(&mut json);
    format_output_fields(&mut json);
    println!("{}", serde_json::to_string_pretty(&json).unwrap());
}

/// Replace a swap's `key_source` with a non-secret summary before printing.
/// `Secret`'s `Serialize` emits raw bytes (the store needs them), so the
/// seedless preimage and gas key would otherwise leak into stdout/logs.
fn redact_key_source(value: &mut serde_json::Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    let Some(key_source) = obj.get_mut("key_source") else {
        return;
    };
    let summary = match key_source
        .get("Derived")
        .and_then(|d| d.get("claim_key_index"))
    {
        Some(index) => format!("Derived (index {index})"),
        None => "Stored (secrets redacted)".to_string(),
    };
    *key_source = serde_json::Value::String(summary);
}

fn format_output_fields(value: &mut serde_json::Value) {
    if let Some(obj) = value.as_object_mut() {
        for (key, val) in obj.iter_mut() {
            if OUTPUT_FIELDS.contains(&key.as_str())
                && let Some(raw) = val.as_u64()
            {
                *val = serde_json::Value::String(format!(
                    "{}.{:06} USD",
                    raw / 1_000_000,
                    raw % 1_000_000
                ));
            }
        }
    }
}

// ─── Logging ────────────────────────────────────────────────────────────

fn init_logging(data_dir: &Path) -> Result<()> {
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        "debug,h2=warn,rustls=warn,hyper=warn,tonic=warn"
            .parse()
            .unwrap()
    });

    let log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join("boltz.log"))
        .with_context(|| "Failed to open log file")?;

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(log_file)
                .with_ansi(false),
        )
        .try_init()
        .ok(); // Ignore if already initialized

    Ok(())
}

// ─── File-backed BoltzStorage ─────────────────────────────────────────────
// Persists the key index to `{data_dir}/key_index` and swap state to
// `{data_dir}/swaps/{swap_id}.json` so that active swaps survive CLI restarts.
// Deposit records/deposit-swaps follow the same one-file-per-record layout
// under `{data_dir}/deposits/` and `{data_dir}/deposit_swaps/`; scan
// watermarks are a single `{data_dir}/watermarks.json` map keyed by chain id
// (JSON object keys must be strings, so chain ids round-trip through
// `to_string`/`parse`).
//
// Known limitations (acceptable for a CLI tool):
// - Writes are not atomic (fs::write, not write-to-temp-then-rename). A crash
//   mid-write could produce corrupted JSON. The SDK should provide its own
//   BoltzStorage with atomic writes.
// - Uses blocking I/O (std::fs) inside async trait methods. Tolerable with
//   tokio's multi-threaded runtime.

struct FileBoltzStorage {
    data_dir: PathBuf,
}

impl FileBoltzStorage {
    fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
        }
    }

    fn index_path(&self) -> PathBuf {
        self.data_dir.join("key_index")
    }

    fn swaps_dir(&self) -> PathBuf {
        self.data_dir.join("swaps")
    }

    fn swap_path(&self, id: &str) -> PathBuf {
        self.swaps_dir().join(format!("{id}.json"))
    }

    fn read_index(&self) -> Result<u32, BoltzError> {
        match fs::read_to_string(self.index_path()) {
            Ok(s) => s
                .trim()
                .parse()
                .map_err(|e| BoltzError::Store(format!("Invalid key index: {e}"))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(BoltzError::Store(format!("Failed to read key index: {e}"))),
        }
    }

    fn write_index(&self, index: u32) -> Result<(), BoltzError> {
        fs::write(self.index_path(), index.to_string())
            .map_err(|e| BoltzError::Store(format!("Failed to write key index: {e}")))
    }

    fn write_swap(&self, swap: &boltz_client::BoltzSwap) -> Result<(), BoltzError> {
        let dir = self.swaps_dir();
        fs::create_dir_all(&dir)
            .map_err(|e| BoltzError::Store(format!("Failed to create swaps dir: {e}")))?;
        let json = serde_json::to_string_pretty(swap)
            .map_err(|e| BoltzError::Store(format!("Failed to serialize swap: {e}")))?;
        fs::write(self.swap_path(&swap.id), json)
            .map_err(|e| BoltzError::Store(format!("Failed to write swap: {e}")))
    }

    fn read_swap(&self, id: &str) -> Result<Option<boltz_client::BoltzSwap>, BoltzError> {
        let path = self.swap_path(id);
        match fs::read_to_string(&path) {
            Ok(json) => {
                let swap: boltz_client::BoltzSwap = serde_json::from_str(&json)
                    .map_err(|e| BoltzError::Store(format!("Failed to parse swap: {e}")))?;
                Ok(Some(swap))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(BoltzError::Store(format!("Failed to read swap: {e}"))),
        }
    }

    fn deposits_dir(&self) -> PathBuf {
        self.data_dir.join("deposits")
    }

    /// Deposit ids are `"{chain_id}:{tx_hash}:{log_index}"` — `:` is escaped
    /// for filesystem safety; the record's own `id` field (read back from the
    /// file contents) stays canonical.
    fn deposit_path(&self, id: &str) -> PathBuf {
        self.deposits_dir()
            .join(format!("{}.json", id.replace(':', "_")))
    }

    fn deposit_swaps_dir(&self) -> PathBuf {
        self.data_dir.join("deposit_swaps")
    }

    fn deposit_swap_path(&self, id: &str) -> PathBuf {
        self.deposit_swaps_dir().join(format!("{id}.json"))
    }

    fn watermarks_path(&self) -> PathBuf {
        self.data_dir.join("watermarks.json")
    }

    fn write_deposit(&self, deposit: &Deposit) -> Result<(), BoltzError> {
        let dir = self.deposits_dir();
        fs::create_dir_all(&dir)
            .map_err(|e| BoltzError::Store(format!("Failed to create deposits dir: {e}")))?;
        let json = serde_json::to_string_pretty(deposit)
            .map_err(|e| BoltzError::Store(format!("Failed to serialize deposit: {e}")))?;
        fs::write(self.deposit_path(&deposit.id), json)
            .map_err(|e| BoltzError::Store(format!("Failed to write deposit: {e}")))
    }

    fn read_deposit(&self, id: &str) -> Result<Option<Deposit>, BoltzError> {
        match fs::read_to_string(self.deposit_path(id)) {
            Ok(json) => {
                let deposit: Deposit = serde_json::from_str(&json)
                    .map_err(|e| BoltzError::Store(format!("Failed to parse deposit: {e}")))?;
                Ok(Some(deposit))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(BoltzError::Store(format!("Failed to read deposit: {e}"))),
        }
    }

    fn list_deposits(&self) -> Result<Vec<Deposit>, BoltzError> {
        let dir = self.deposits_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut deposits = Vec::new();
        let entries = fs::read_dir(&dir)
            .map_err(|e| BoltzError::Store(format!("Failed to read deposits dir: {e}")))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| BoltzError::Store(format!("Failed to read dir entry: {e}")))?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                let json = fs::read_to_string(&path)
                    .map_err(|e| BoltzError::Store(format!("Failed to read deposit file: {e}")))?;
                let deposit: Deposit = serde_json::from_str(&json)
                    .map_err(|e| BoltzError::Store(format!("Failed to parse deposit: {e}")))?;
                deposits.push(deposit);
            }
        }
        Ok(deposits)
    }

    fn write_deposit_swap(&self, swap: &DepositSwap) -> Result<(), BoltzError> {
        let dir = self.deposit_swaps_dir();
        fs::create_dir_all(&dir)
            .map_err(|e| BoltzError::Store(format!("Failed to create deposit-swaps dir: {e}")))?;
        let json = serde_json::to_string_pretty(swap)
            .map_err(|e| BoltzError::Store(format!("Failed to serialize deposit swap: {e}")))?;
        fs::write(self.deposit_swap_path(&swap.id), json)
            .map_err(|e| BoltzError::Store(format!("Failed to write deposit swap: {e}")))
    }

    fn read_deposit_swap(&self, id: &str) -> Result<Option<DepositSwap>, BoltzError> {
        match fs::read_to_string(self.deposit_swap_path(id)) {
            Ok(json) => {
                let swap: DepositSwap = serde_json::from_str(&json)
                    .map_err(|e| BoltzError::Store(format!("Failed to parse deposit swap: {e}")))?;
                Ok(Some(swap))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(BoltzError::Store(format!(
                "Failed to read deposit swap: {e}"
            ))),
        }
    }

    fn list_deposit_swaps(&self) -> Result<Vec<DepositSwap>, BoltzError> {
        let dir = self.deposit_swaps_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut swaps = Vec::new();
        let entries = fs::read_dir(&dir)
            .map_err(|e| BoltzError::Store(format!("Failed to read deposit-swaps dir: {e}")))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| BoltzError::Store(format!("Failed to read dir entry: {e}")))?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                let json = fs::read_to_string(&path).map_err(|e| {
                    BoltzError::Store(format!("Failed to read deposit-swap file: {e}"))
                })?;
                let swap: DepositSwap = serde_json::from_str(&json)
                    .map_err(|e| BoltzError::Store(format!("Failed to parse deposit swap: {e}")))?;
                swaps.push(swap);
            }
        }
        Ok(swaps)
    }

    fn read_watermarks(&self) -> Result<HashMap<u64, u64>, BoltzError> {
        match fs::read_to_string(self.watermarks_path()) {
            Ok(json) => {
                let raw: HashMap<String, u64> = serde_json::from_str(&json)
                    .map_err(|e| BoltzError::Store(format!("Failed to parse watermarks: {e}")))?;
                raw.into_iter()
                    .map(|(k, v)| {
                        k.parse::<u64>()
                            .map(|k| (k, v))
                            .map_err(|e| BoltzError::Store(format!("Invalid watermark key: {e}")))
                    })
                    .collect()
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(e) => Err(BoltzError::Store(format!("Failed to read watermarks: {e}"))),
        }
    }

    fn write_watermarks(&self, watermarks: &HashMap<u64, u64>) -> Result<(), BoltzError> {
        let raw: HashMap<String, u64> = watermarks
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect();
        let json = serde_json::to_string_pretty(&raw)
            .map_err(|e| BoltzError::Store(format!("Failed to serialize watermarks: {e}")))?;
        fs::write(self.watermarks_path(), json)
            .map_err(|e| BoltzError::Store(format!("Failed to write watermarks: {e}")))
    }
}

#[macros::async_trait]
impl BoltzStorage for FileBoltzStorage {
    async fn upsert_swap(&self, swap: &boltz_client::BoltzSwap) -> Result<(), BoltzError> {
        self.write_swap(swap)
    }

    async fn get_swap(&self, id: &str) -> Result<Option<boltz_client::BoltzSwap>, BoltzError> {
        self.read_swap(id)
    }

    async fn list_active_swaps(&self) -> Result<Vec<boltz_client::BoltzSwap>, BoltzError> {
        let dir = self.swaps_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut active = Vec::new();
        let entries = fs::read_dir(&dir)
            .map_err(|e| BoltzError::Store(format!("Failed to read swaps dir: {e}")))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| BoltzError::Store(format!("Failed to read dir entry: {e}")))?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                let json = fs::read_to_string(&path)
                    .map_err(|e| BoltzError::Store(format!("Failed to read swap file: {e}")))?;
                let swap: boltz_client::BoltzSwap = serde_json::from_str(&json)
                    .map_err(|e| BoltzError::Store(format!("Failed to parse swap: {e}")))?;
                if !swap.status.is_terminal() {
                    active.push(swap);
                }
            }
        }
        Ok(active)
    }
}

#[macros::async_trait]
impl DerivedKeyStore for FileBoltzStorage {
    async fn increment_key_index(&self) -> Result<u32, BoltzError> {
        let current = self.read_index()?;
        let next = current
            .checked_add(1)
            .ok_or_else(|| BoltzError::Store("Key index overflow".to_string()))?;
        self.write_index(next)?;
        Ok(current)
    }
}

#[macros::async_trait]
impl DepositStorage for FileBoltzStorage {
    async fn upsert_deposit(&self, deposit: &Deposit) -> Result<(), BoltzError> {
        self.write_deposit(deposit)
    }

    async fn get_deposit(&self, id: &str) -> Result<Option<Deposit>, BoltzError> {
        self.read_deposit(id)
    }

    async fn list_open_deposits(&self) -> Result<Vec<Deposit>, BoltzError> {
        Ok(self
            .list_deposits()?
            .into_iter()
            .filter(|d| !d.is_terminal())
            .collect())
    }

    async fn list_chain_deposits(&self, chain_id: u64) -> Result<Vec<Deposit>, BoltzError> {
        Ok(self
            .list_deposits()?
            .into_iter()
            .filter(|d| d.chain_id == chain_id)
            .collect())
    }

    async fn upsert_deposit_swap(&self, swap: &DepositSwap) -> Result<(), BoltzError> {
        self.write_deposit_swap(swap)
    }

    async fn get_deposit_swap(&self, id: &str) -> Result<Option<DepositSwap>, BoltzError> {
        self.read_deposit_swap(id)
    }

    async fn list_active_deposit_swaps(&self) -> Result<Vec<DepositSwap>, BoltzError> {
        Ok(self
            .list_deposit_swaps()?
            .into_iter()
            .filter(|s| !s.status.is_terminal())
            .collect())
    }

    async fn get_deposit_watermark(&self, chain_id: u64) -> Result<Option<u64>, BoltzError> {
        Ok(self.read_watermarks()?.get(&chain_id).copied())
    }

    async fn set_deposit_watermark(&self, chain_id: u64, block: u64) -> Result<(), BoltzError> {
        let mut watermarks = self.read_watermarks()?;
        watermarks.insert(chain_id, block);
        self.write_watermarks(&watermarks)
    }
}

fn rand_entropy() -> [u8; 16] {
    let mut out = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut out);
    out
}

/// Parse a human-readable USD amount (e.g. "1.5") into 6-decimal raw units (1500000).
fn parse_usd_amount(s: &str) -> std::result::Result<u64, String> {
    const DECIMALS: u32 = 6;
    let parts: Vec<&str> = s.split('.').collect();
    match parts.len() {
        1 => {
            let whole: u64 = parts[0].parse().map_err(|e| format!("{e}"))?;
            whole
                .checked_mul(10u64.pow(DECIMALS))
                .ok_or_else(|| "amount too large".to_string())
        }
        2 => {
            let whole: u64 = parts[0].parse().map_err(|e| format!("{e}"))?;
            let frac_str = parts[1];
            if frac_str.len() > DECIMALS as usize {
                return Err(format!("too many decimal places (max {DECIMALS})"));
            }
            let padded = format!("{frac_str:0<width$}", width = DECIMALS as usize);
            let frac: u64 = padded.parse().map_err(|e| format!("{e}"))?;
            whole
                .checked_mul(10u64.pow(DECIMALS))
                .and_then(|w| w.checked_add(frac))
                .ok_or_else(|| "amount too large".to_string())
        }
        _ => Err("invalid amount format".to_string()),
    }
}

fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt} [y/N] ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim().to_lowercase();
    Ok(trimmed == "y" || trimmed == "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_key_source_hides_seedless_secrets() {
        // Mimics a serialized seedless swap: Secret serializes as a raw byte array.
        let mut json = serde_json::json!({
            "id": "s1",
            "key_source": { "Stored": { "preimage": [1, 2, 3], "gas_key": [4, 5, 6] } },
        });
        redact_key_source(&mut json);
        let printed = serde_json::to_string(&json).unwrap();
        assert!(!printed.contains("preimage"), "preimage leaked: {printed}");
        assert!(!printed.contains("gas_key"), "gas_key leaked: {printed}");
        assert_eq!(
            json["key_source"],
            serde_json::json!("Stored (secrets redacted)")
        );
    }

    #[test]
    fn redact_key_source_keeps_derived_index() {
        // The HD index is not a secret, so it stays visible.
        let mut json = serde_json::json!({
            "id": "s1",
            "key_source": { "Derived": { "claim_key_index": 7 } },
        });
        redact_key_source(&mut json);
        assert_eq!(json["key_source"], serde_json::json!("Derived (index 7)"));
    }
}
