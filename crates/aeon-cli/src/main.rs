use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(feature = "rest-api")]
mod connectors;

/// Default REST API address for CLI commands.
/// Overridable via `AEON_API_URL` environment variable (12-Factor Config).
fn default_api() -> String {
    std::env::var("AEON_API_URL").unwrap_or_else(|_| "http://localhost:4471".into())
}

#[derive(Parser)]
#[command(
    name = "aeon",
    version,
    about = "Aeon — real-time data processing engine"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Scaffold a new processor project
    New {
        /// Project name (creates a directory with this name)
        name: String,
        /// Processor runtime
        #[arg(long, default_value = "wasm")]
        runtime: Runtime,
        /// Programming language
        #[arg(long, default_value = "rust")]
        lang: Lang,
    },
    /// Build a processor project
    Build {
        /// Build in release mode
        #[arg(long)]
        release: bool,
        /// Project directory (defaults to current dir)
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Validate a compiled processor artifact
    Validate {
        /// Path to .wasm, .so, or .dll file
        path: PathBuf,
    },
    /// Manage processors in the registry
    Processor {
        /// Aeon REST API address
        #[arg(long, default_value_t = default_api(), global = true)]
        api: String,
        #[command(subcommand)]
        action: ProcessorAction,
    },
    /// Manage pipelines
    Pipeline {
        /// Aeon REST API address
        #[arg(long, default_value_t = default_api(), global = true)]
        api: String,
        #[command(subcommand)]
        action: PipelineAction,
    },
    /// Deploy an artifact to a running pipeline (register + upgrade)
    Deploy {
        /// Path to .wasm or .so artifact
        artifact: PathBuf,
        /// Pipeline to upgrade
        #[arg(long)]
        pipeline: String,
        /// Processor name
        #[arg(long)]
        processor: String,
        /// Version string
        #[arg(long)]
        version: String,
        /// Aeon REST API address
        #[arg(long, default_value_t = default_api())]
        api: String,
    },
    /// Show real-time pipeline status (simple text dashboard)
    Top {
        /// Aeon REST API address
        #[arg(long, default_value_t = default_api())]
        api: String,
    },
    /// Verify PoH/Merkle chain integrity (placeholder)
    Verify {
        /// Pipeline to verify (or "all")
        #[arg(default_value = "all")]
        target: String,
        /// Aeon REST API address
        #[arg(long, default_value_t = default_api())]
        api: String,
    },
    /// Apply a YAML manifest (declarative processors + pipelines)
    Apply {
        /// Path to manifest YAML file
        #[arg(short, long)]
        file: PathBuf,
        /// Aeon REST API address
        #[arg(long, default_value_t = default_api())]
        api: String,
        /// Dry run (show what would change without applying)
        #[arg(long)]
        dry_run: bool,
    },
    /// Export current state as a YAML manifest
    Export {
        /// Output file path (stdout if omitted)
        #[arg(short, long)]
        file: Option<PathBuf>,
        /// Aeon REST API address
        #[arg(long, default_value_t = default_api())]
        api: String,
    },
    /// Diff a manifest against the current state
    Diff {
        /// Path to manifest YAML file
        #[arg(short, long)]
        file: PathBuf,
        /// Aeon REST API address
        #[arg(long, default_value_t = default_api())]
        api: String,
    },
    /// Start/stop the local development environment (Redpanda)
    Dev {
        #[command(subcommand)]
        action: DevAction,
    },
    /// TLS certificate management
    Tls {
        #[command(subcommand)]
        action: TlsAction,
    },
    /// Start the Aeon server (REST API + pipeline engine)
    #[cfg(feature = "rest-api")]
    Serve {
        /// REST API listen address
        #[arg(long, default_value = "0.0.0.0:4471")]
        addr: String,
        /// Artifact storage directory
        #[arg(long, default_value = "/app/artifacts")]
        artifact_dir: String,
    },
    /// Inspect and control the Aeon cluster (Raft status, partition handover)
    Cluster {
        /// Aeon REST API address
        #[arg(long, default_value_t = default_api(), global = true)]
        api: String,
        #[command(subcommand)]
        action: ClusterAction,
    },
    /// Check environment readiness (DX-2): ports, connectivity, state dir, artifacts
    Doctor {
        /// Aeon REST API address (probes /health)
        #[arg(long, default_value_t = default_api())]
        api: String,
        /// Kafka/Redpanda bootstrap address to probe (TCP connect)
        #[arg(long, default_value = "localhost:19092")]
        kafka: String,
        /// Local state directory to verify writability
        #[arg(long, default_value = "./data/state")]
        state_dir: PathBuf,
        /// Optional manifest YAML to validate (schema + referenced artifacts)
        #[arg(long)]
        manifest: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ProcessorAction {
    /// List all registered processors
    List,
    /// Inspect a processor
    Inspect { name: String },
    /// List versions of a processor
    Versions { name: String },
    /// Register a processor artifact
    Register {
        /// Processor name
        name: String,
        /// Version string (e.g., "1.0.0")
        #[arg(long)]
        version: String,
        /// Description
        #[arg(long, default_value = "")]
        description: String,
        /// Path to .wasm or .so artifact
        #[arg(long)]
        artifact: PathBuf,
        /// Platform (e.g., "wasm32", "x86_64-linux")
        #[arg(long, default_value = "wasm32")]
        platform: String,
    },
    /// Delete a processor version
    Delete {
        /// Processor name
        name: String,
        /// Version to delete
        #[arg(long)]
        version: String,
    },
    /// Manage processor identities (ED25519 keys for T3/T4 auth)
    Identity {
        /// Processor name
        name: String,
        #[command(subcommand)]
        action: IdentityAction,
    },
}

#[derive(Subcommand)]
enum IdentityAction {
    /// Register an ED25519 public key for a processor
    Add {
        /// ED25519 public key ("ed25519:<base64>")
        #[arg(long)]
        public_key: String,
        /// Comma-separated pipeline names (or "all" for all matching)
        #[arg(long, default_value = "all")]
        pipelines: String,
        /// Maximum concurrent connections
        #[arg(long, default_value = "1")]
        max_instances: u32,
    },
    /// List registered identities for a processor
    List,
    /// Revoke a processor identity by fingerprint
    Revoke {
        /// Key fingerprint (SHA256:...)
        #[arg(long)]
        fingerprint: String,
    },
}

#[derive(Subcommand)]
enum PipelineAction {
    /// List all pipelines
    List,
    /// Inspect a pipeline
    Inspect { name: String },
    /// Create a pipeline from JSON definition
    Create {
        /// Path to pipeline JSON file
        #[arg(long)]
        file: PathBuf,
    },
    /// Start a pipeline
    Start { name: String },
    /// Stop a pipeline
    Stop { name: String },
    /// Upgrade a pipeline's processor (drain-swap, default)
    Upgrade {
        /// Pipeline name
        name: String,
        /// Processor name
        #[arg(long)]
        processor: String,
        /// Processor version
        #[arg(long)]
        version: String,
        /// Upgrade strategy
        #[arg(long, default_value = "drain-swap")]
        strategy: UpgradeStrategyArg,
        /// Canary traffic steps (comma-separated, e.g., "10,50,100")
        #[arg(long)]
        steps: Option<String>,
    },
    /// Cut over blue-green deployment to the new processor
    Cutover { name: String },
    /// Roll back an in-progress upgrade (blue-green or canary)
    Rollback { name: String },
    /// Promote canary to the next traffic step
    Promote { name: String },
    /// Show canary upgrade status
    CanaryStatus { name: String },
    /// Show pipeline lifecycle history
    History { name: String },
    /// Delete a pipeline
    Delete { name: String },
}

#[derive(Clone, ValueEnum)]
enum UpgradeStrategyArg {
    DrainSwap,
    BlueGreen,
    Canary,
}

#[derive(Subcommand)]
enum DevAction {
    /// Start the development environment (Redpanda + Console)
    Up,
    /// Stop the development environment
    Down,
    /// Show development environment status
    Status,
    /// Watch a processor artifact and hot-reload on change
    #[cfg(feature = "rest-api")]
    Watch {
        /// Path to the processor artifact (.wasm, .so, .dll)
        #[arg(long)]
        artifact: PathBuf,
    },
}

#[derive(Subcommand)]
enum ClusterAction {
    /// Show cluster status: node ID, leader, partition assignments, Raft metrics.
    ///
    /// With `--watch`, polls the endpoint on an interval and re-renders a
    /// compact human-readable view (partition ownership, transfers in flight,
    /// Raft term/index). Ctrl-C to exit. Without `--watch`, prints a raw JSON
    /// snapshot — script-friendly.
    Status {
        /// Watch mode: clear + re-render on each tick until Ctrl-C.
        #[arg(long)]
        watch: bool,
        /// Poll interval in seconds when `--watch` is set.
        #[arg(long, default_value_t = 2.0)]
        interval: f64,
    },
    /// Initiate a partition handover from the current owner to a target node.
    ///
    /// Must be issued against the current Raft leader. If you hit a follower,
    /// the server replies `409 Conflict` with an `X-Leader-Id` header pointing
    /// to the real leader — rerun against that node.
    TransferPartition {
        /// Partition ID to transfer
        #[arg(long)]
        partition: u16,
        /// Target node ID (the partition's new owner)
        #[arg(long)]
        target: u64,
    },
    /// Send every partition currently owned by `--node` to the other live members.
    ///
    /// Must be issued against the current Raft leader. Follower replies carry
    /// `X-Leader-Id` — rerun against that node.
    Drain {
        /// Node ID to drain (all its partitions handed off to remaining live members)
        #[arg(long)]
        node: u64,
    },
    /// Redistribute partitions across live members toward an even split.
    ///
    /// Must be issued against the current Raft leader. Follower replies carry
    /// `X-Leader-Id` — rerun against that node.
    Rebalance,
    /// Ask the Raft leader to remove THIS node from the cluster.
    ///
    /// Issued against the local node's API (typically `http://localhost:4471`
    /// from inside the pod). The server reads its own `node_id`, forwards a
    /// `RemoveNodeRequest` to the current leader, and returns the result. If
    /// this node IS the leader the request is rejected with 409 — transfer
    /// leadership first. Used by the K8s preStop hook so scale-down doesn't
    /// leave a stale voter behind.
    Leave,
}

#[derive(Subcommand)]
enum TlsAction {
    /// Export the CA certificate (for distributing to clients)
    ExportCa {
        /// Path to the CA PEM file to export
        #[arg(long)]
        ca: PathBuf,
        /// Output file (stdout if omitted)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Show certificate information (expiry, subject, issuer)
    Info {
        /// Path to a PEM certificate file
        cert: PathBuf,
    },
}

#[derive(Clone, ValueEnum)]
enum Runtime {
    Wasm,
    Native,
}

#[derive(Clone, ValueEnum)]
enum Lang {
    Rust,
    Typescript,
}

fn main() {
    let cli = Cli::parse();
    if let Err(err) = run(cli) {
        print_pretty_error(&err);
        std::process::exit(1);
    }
}

/// Pretty-print an `anyhow::Error` with a human-readable summary, full context
/// chain, and (when we can recognise the pattern) an actionable hint.
///
/// anyhow's default Debug output is information-dense but reads like a stack
/// trace; end users typically want "what broke" + "what to try next" on two
/// lines. DX-4.
fn print_pretty_error(err: &anyhow::Error) {
    // Top-level message
    eprintln!("error: {err}");

    // Context chain (skip the top which we already printed)
    let mut causes = err.chain().skip(1).peekable();
    if causes.peek().is_some() {
        for cause in causes {
            eprintln!("  caused by: {cause}");
        }
    }

    if let Some(hint) = error_hint(err) {
        eprintln!();
        eprintln!("hint: {hint}");
    }
}

/// Map common error shapes to actionable remediation hints. Matching happens
/// on the flattened error chain text — cheap, string-based, and easy to extend.
fn error_hint(err: &anyhow::Error) -> Option<&'static str> {
    // Build a lowercased haystack of the full chain once.
    let haystack: String = err
        .chain()
        .map(|c| c.to_string().to_lowercase())
        .collect::<Vec<_>>()
        .join(" | ");

    // Order matters: more specific patterns first.
    if haystack.contains("connection refused")
        || haystack.contains("failed to reach aeon api")
        || haystack.contains("tcp connect error")
    {
        return Some(
            "Aeon server unreachable. Is `aeon serve` running? Run `aeon doctor` to diagnose, or set AEON_API_URL to a reachable address.",
        );
    }
    if haystack.contains("status code 401") || haystack.contains("unauthorized") {
        return Some("authentication failed. Check AEON_API_TOKEN or your pipeline identity key (`aeon processor identity list <name>`).");
    }
    if haystack.contains("status code 403") || haystack.contains("forbidden") {
        return Some("access denied. The caller has no permission for this resource — check identity scope (allowed_pipelines).");
    }
    if haystack.contains("status code 404") {
        return Some("resource not found. Run `aeon processor list` / `aeon pipeline list` to see what exists.");
    }
    if haystack.contains("failed to run npm") {
        return Some("Node.js is required. Install from https://nodejs.org or your OS package manager.");
    }
    if haystack.contains("failed to run cargo build") || haystack.contains("cargo build failed") {
        return Some("Rust toolchain build failed. Ensure `rustup target add wasm32-unknown-unknown` for Wasm processors.");
    }
    if haystack.contains("unknown file extension") {
        return Some("supported artifacts: .wasm (guest) or .so/.dll/.dylib (native). Run `aeon build` to produce one.");
    }
    if haystack.contains("wasm validation failed") || haystack.contains("failed to compile wasm") {
        return Some("artifact is not a valid Aeon Wasm component. Rebuild with `aeon build --release` and re-run `aeon validate`.");
    }
    if haystack.contains("native validation requires the 'native-validate' feature") {
        return Some("rebuild the CLI with `--features native-validate` (enabled by default in release builds).");
    }
    if haystack.contains("directory '") && haystack.contains("already exists") {
        return Some("remove the existing directory or pick a different project name.");
    }
    if haystack.contains("file not found") || haystack.contains("no such file") {
        return Some("check the path spelling and current working directory. Use an absolute path if unsure.");
    }
    if haystack.contains("invalid json response") || haystack.contains("expected array") {
        return Some(
            "unexpected response shape from the API. Check that the Aeon server version matches this CLI — run `aeon --version` and compare with the server banner.",
        );
    }
    None
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Some(Commands::New {
            name,
            runtime,
            lang,
        }) => cmd_new(&name, &runtime, &lang),
        Some(Commands::Build { release, dir }) => cmd_build(release, dir.as_deref()),
        Some(Commands::Validate { path }) => cmd_validate(&path),
        Some(Commands::Processor { api, action }) => cmd_processor(&api, &action),
        Some(Commands::Pipeline { api, action }) => cmd_pipeline(&api, &action),
        Some(Commands::Deploy {
            artifact,
            pipeline,
            processor,
            version,
            api,
        }) => cmd_deploy(&artifact, &pipeline, &processor, &version, &api),
        Some(Commands::Top { api }) => cmd_top(&api),
        Some(Commands::Verify { target, api }) => cmd_verify(&target, &api),
        Some(Commands::Apply { file, api, dry_run }) => cmd_apply(&file, &api, dry_run),
        Some(Commands::Export { file, api }) => cmd_export(file.as_deref(), &api),
        Some(Commands::Diff { file, api }) => cmd_diff(&file, &api),
        Some(Commands::Dev { action }) => {
            #[cfg(feature = "rest-api")]
            if let DevAction::Watch { artifact } = &action {
                let rt = tokio::runtime::Runtime::new()?;
                return rt.block_on(cmd_dev_watch(artifact));
            }
            cmd_dev(&action)
        }
        Some(Commands::Tls { action }) => cmd_tls(&action),
        Some(Commands::Cluster { api, action }) => cmd_cluster(&api, &action),
        #[cfg(feature = "rest-api")]
        Some(Commands::Serve { addr, artifact_dir }) => cmd_serve(&addr, &artifact_dir),
        Some(Commands::Doctor {
            api,
            kafka,
            state_dir,
            manifest,
        }) => cmd_doctor(&api, &kafka, &state_dir, manifest.as_deref()),
        None => {
            println!("Aeon v{}", env!("CARGO_PKG_VERSION"));
            println!("Run `aeon --help` for available commands.");
            Ok(())
        }
    }
}

// ── Server ────────────────────────────────────────────────────────────

/// G11.b — build and install the leader-side `PartitionTransferDriver`
/// for the given `pipeline_name`. Called either from the env-var branch
/// at boot (`AEON_PIPELINE_NAME` set) or from the auto-discovery watcher
/// once the single-pipeline-per-node assumption is satisfied. Spawning
/// the watch loop here ties it to the node's shutdown flag.
#[cfg(feature = "rest-api")]
async fn install_partition_transfer_driver(
    endpoint: std::sync::Arc<aeon_cluster::transport::endpoint::QuicEndpoint>,
    node: std::sync::Arc<aeon_cluster::ClusterNode>,
    supervisor: std::sync::Arc<aeon_engine::PipelineSupervisor>,
    l2_root: String,
    poh_mode: aeon_crypto::poh::PohVerifyMode,
    pipeline_name: String,
) -> anyhow::Result<()> {
    use std::sync::Arc;

    let seg_installer = Arc::new(aeon_engine::L2SegmentInstaller::new(
        &l2_root,
        &pipeline_name,
    ));

    let poh_registry = aeon_engine::InstalledPohChainRegistry::new();
    let poh_installer = Arc::new(aeon_engine::PohChainInstallerImpl::new(
        poh_registry.clone(),
        poh_mode,
    ));

    // G11.c: the supervisor reads from the same registry the installer
    // writes to, so a chain installed during CL-6 handover is picked up
    // by `create_poh_state` when the post-transfer pipeline task starts
    // on this node. Without this, every ownership flip would restart
    // the PoH chain at sequence 0 and Gate 2's "PoH chain has no gaps
    // across transfers" row can't pass.
    supervisor
        .set_poh_installed_chains(Arc::new(poh_registry.clone()))
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let resolver: Arc<dyn aeon_cluster::NodeResolver> =
        Arc::new(aeon_cluster::RaftNodeResolver::new(node.raft().clone()));

    let driver = Arc::new(aeon_cluster::PartitionTransferDriver::new(
        Arc::clone(&endpoint),
        node.raft().clone(),
        node.shared_state(),
        resolver,
        node.config().node_id,
        pipeline_name.clone(),
        seg_installer,
        poh_installer,
    ));

    node.install_transfer_driver(Arc::clone(&driver)).await;

    let driver_shutdown = node.shutdown_flag();
    let driver_for_loop = Arc::clone(&driver);
    tokio::spawn(async move {
        driver_for_loop.watch_loop(driver_shutdown).await;
    });

    tracing::info!(
        pipeline = %pipeline_name,
        l2_root = %l2_root,
        "PartitionTransferDriver spawned — transfers will drive automatically"
    );
    Ok(())
}

#[cfg(feature = "rest-api")]
fn cmd_serve(addr: &str, artifact_dir: &str) -> Result<()> {
    use std::sync::Arc;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    rt.block_on(async {
        // Resolve artifact directory from env or CLI flag
        let dir = std::env::var("AEON_ARTIFACT_DIR").unwrap_or_else(|_| artifact_dir.to_string());
        let registry = Arc::new(
            aeon_engine::registry::ProcessorRegistry::new(&dir)
                .map_err(|e| anyhow::anyhow!("{e}"))?,
        );
        let pipelines = Arc::new(aeon_engine::pipeline_manager::PipelineManager::new());
        let identities = Arc::new(aeon_engine::identity_store::ProcessorIdentityStore::new());

        // Build the ConnectorRegistry and register every default connector
        // factory this binary ships with (memory / kafka sources and
        // blackhole / stdout / kafka sinks). The supervisor then drives
        // running tokio tasks off that registry — both through the REST
        // start/stop path and through Raft-committed SetPipelineState.
        let mut connectors = aeon_engine::ConnectorRegistry::new();
        crate::connectors::register_defaults(&mut connectors);
        let connectors = Arc::new(connectors);
        let supervisor = Arc::new(aeon_engine::PipelineSupervisor::new(Arc::clone(&connectors)));

        // Cluster initialization — detect K8s environment
        let cluster_enabled = std::env::var("AEON_CLUSTER_ENABLED")
            .map(|v| v.to_lowercase() == "true")
            .unwrap_or(false);

        let cluster_node: Option<Arc<aeon_cluster::ClusterNode>> = if cluster_enabled {
            match aeon_cluster::discovery::from_k8s_env() {
                Some(k8s) => {
                    tracing::info!(
                        node_id = k8s.node_id,
                        pod = %k8s.pod_name,
                        replicas = k8s.replicas,
                        partitions = k8s.partitions,
                        "K8s cluster discovery: initializing Raft node"
                    );

                    // G10 — scale-up pods (StatefulSet ordinal >= initial
                    // replicas count) must not re-run `raft.initialize`; they
                    // seed-join the existing cluster instead. `is_scale_up_pod`
                    // reads the stale `AEON_CLUSTER_REPLICAS` env that helm
                    // baked at install time against this pod's ordinal.
                    let scale_up = k8s.is_scale_up_pod();
                    let mut config = if scale_up {
                        tracing::info!(
                            ordinal = k8s.ordinal,
                            replicas = k8s.replicas,
                            "scale-up pod detected — will seed-join existing cluster"
                        );
                        k8s.to_join_config()
                    } else {
                        k8s.to_cluster_config()
                    };

                    // G11.a: allow operators to override the PoH chain verification
                    // mode at install time via env (Helm values set this on the
                    // StatefulSet). YAML is the source of truth; env is the
                    // operator override because the k8s path doesn't read YAML.
                    if let Ok(mode) = std::env::var("AEON_CLUSTER_POH_VERIFY_MODE") {
                        config.poh_verify_mode = mode;
                    }

                    // G14: raft_timing env overrides. Helm surfaces
                    // `cluster.raftTiming.*` as these env vars so operators can
                    // pick `fast_failover` (<5s target) vs `prod_recommended`
                    // vs `flaky_network` without rebuilding. Validation runs
                    // below so an invalid combo fails fast at startup.
                    if let Some(v) = std::env::var("AEON_RAFT_HEARTBEAT_MS")
                        .ok()
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        config.raft_timing.heartbeat_ms = v;
                    }
                    if let Some(v) = std::env::var("AEON_RAFT_ELECTION_MIN_MS")
                        .ok()
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        config.raft_timing.election_min_ms = v;
                    }
                    if let Some(v) = std::env::var("AEON_RAFT_ELECTION_MAX_MS")
                        .ok()
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        config.raft_timing.election_max_ms = v;
                    }

                    // G9: auto-forward REST writes to leader needs to know
                    // (a) what port peers listen on — all pods share the same
                    // REST port because the StatefulSet's headless service
                    // publishes it on every pod FQDN — and (b) the scheme.
                    // Parse the bind address string (CLI flag or AEON_API_ADDR
                    // env) once so follower pods can synthesise a 307 Location.
                    let rest_listen = std::env::var("AEON_API_ADDR")
                        .unwrap_or_else(|_| addr.to_string());
                    if let Ok(parsed) = rest_listen.parse::<std::net::SocketAddr>() {
                        config.rest_api_port = Some(parsed.port());
                    } else if let Some(port) = rest_listen
                        .rsplit(':')
                        .next()
                        .and_then(|p| p.parse::<u16>().ok())
                    {
                        config.rest_api_port = Some(port);
                    }
                    if config.rest_scheme.is_none() {
                        config.rest_scheme = Some("http".to_string());
                    }

                    config
                        .validate()
                        .map_err(|e| anyhow::anyhow!("cluster config: {e}"))?;

                    // Select TLS mode based on cluster config (FT-8):
                    //   - config.tls = Some(...)  → production mTLS (file-based)
                    //   - config.auto_tls = true  → insecure dev self-signed (loud warning)
                    //   - neither                 → error
                    let (server_cfg, client_cfg) =
                        aeon_cluster::transport::tls::quic_configs_for_cluster(&config)
                            .map_err(|e| anyhow::anyhow!("TLS config: {e}"))?;

                    let endpoint = Arc::new(
                        aeon_cluster::transport::endpoint::QuicEndpoint::bind(
                            config.bind,
                            server_cfg,
                            client_cfg,
                        )
                        .map_err(|e| anyhow::anyhow!("QUIC bind failed: {e}"))?,
                    );

                    // G10 — branch on bootstrap vs join. bootstrap_multi()
                    // runs `raft.initialize(members)`; join() seed-contacts an
                    // existing leader and lets it `add_learner → change_membership`.
                    let poh_mode_str = config.poh_verify_mode.clone();
                    let node = if scale_up {
                        aeon_cluster::ClusterNode::join(config, Arc::clone(&endpoint))
                            .await
                            .map_err(|e| anyhow::anyhow!("Cluster seed-join failed: {e}"))?
                    } else {
                        aeon_cluster::ClusterNode::bootstrap_multi(
                            config,
                            Arc::clone(&endpoint),
                        )
                        .await
                        .map_err(|e| anyhow::anyhow!("Cluster bootstrap failed: {e}"))?
                    };

                    let node = Arc::new(node);

                    // Install the engine-side RegistryApplier so RegistryCommand
                    // entries committed through Raft (from any node) dispatch to
                    // this node's ProcessorRegistry / PipelineManager. Without this,
                    // pipelines created via REST on one node would replicate but
                    // never take effect on follower replicas.
                    node.install_registry_applier(Arc::new(
                        aeon_engine::ClusterRegistryApplier::with_supervisor(
                            Arc::clone(&registry),
                            Arc::clone(&pipelines),
                            Arc::clone(&supervisor),
                        ),
                    ))
                    .await;

                    // G1: install the cluster-backed partition-ownership
                    // resolver onto the supervisor. When a pipeline manifest
                    // leaves `partitions` empty, start() now fills it with
                    // this node's owned slice from the Raft-replicated
                    // PartitionTable rather than silently defaulting to [0].
                    supervisor
                        .set_ownership_resolver(Arc::new(
                            aeon_engine::ClusterPartitionOwnership::new(
                                node.shared_state(),
                                node.config().node_id,
                            )
                            // P5: attach the cluster-side owned-partitions
                            // watch so the pipeline source loop can
                            // reassign_partitions on a CL-6 transfer commit
                            // without a pipeline restart.
                            .with_watch(node.watch_owned_partitions()),
                        ))
                        .map_err(|e| {
                            anyhow::anyhow!("install ownership resolver: {e}")
                        })?;

                    // G11.b — spawn the leader-side partition-transfer driver.
                    // Without this, Raft-committed `Transferring` entries never
                    // drive the three-step CL-6 handover and T4 / T2 cutovers
                    // stall with `owner=null` forever.
                    //
                    // Activation: the driver is bound to a single pipeline name
                    // (single-pipeline-per-node assumption — see
                    // partition_driver.rs). Two paths to discover that name:
                    //   1. Explicit — `AEON_PIPELINE_NAME` env var (operator
                    //      set via helm `cluster.pipelineName`). Installs now.
                    //   2. Auto — env var unset. Spawn a watcher that polls
                    //      the pipeline catalog; once exactly one pipeline is
                    //      registered, install the driver bound to its name.
                    //      Matches the common case where the operator deploys
                    //      Aeon first, then creates their pipeline via REST.
                    //
                    // If multiple pipelines coexist on a node the watcher
                    // stays dormant — driver install requires an operator
                    // explicitly setting the env var.
                    let poh_mode: aeon_crypto::poh::PohVerifyMode = poh_mode_str
                        .parse()
                        .map_err(|e| anyhow::anyhow!(
                            "invalid cluster.poh_verify_mode '{poh_mode_str}': {e}"
                        ))?;
                    let l2_root = std::env::var("AEON_L2_ROOT").unwrap_or_else(|_| {
                        format!("{dir}/l2body")
                    });

                    if let Ok(pipeline_name) = std::env::var("AEON_PIPELINE_NAME") {
                        install_partition_transfer_driver(
                            Arc::clone(&endpoint),
                            Arc::clone(&node),
                            Arc::clone(&supervisor),
                            l2_root,
                            poh_mode,
                            pipeline_name,
                        )
                        .await?;
                    } else {
                        tracing::info!(
                            "AEON_PIPELINE_NAME unset — auto-discovering pipeline name \
                             from the Raft-replicated catalog. \
                             PartitionTransferDriver installs once exactly one pipeline \
                             is registered."
                        );
                        let endpoint_disc = Arc::clone(&endpoint);
                        let node_disc = Arc::clone(&node);
                        let supervisor_disc = Arc::clone(&supervisor);
                        let pipelines_disc = Arc::clone(&pipelines);
                        let shutdown_flag = node.shutdown_flag();
                        tokio::spawn(async move {
                            let mut interval = tokio::time::interval(
                                std::time::Duration::from_secs(2),
                            );
                            interval.set_missed_tick_behavior(
                                tokio::time::MissedTickBehavior::Delay,
                            );
                            let mut logged_ambiguous = false;
                            loop {
                                if shutdown_flag
                                    .load(std::sync::atomic::Ordering::Relaxed)
                                {
                                    return;
                                }
                                interval.tick().await;
                                let names = pipelines_disc.list().await;
                                match names.len() {
                                    0 => {}
                                    1 => {
                                        let name = names.into_iter().next().unwrap();
                                        tracing::info!(
                                            pipeline = %name,
                                            "auto-discovered pipeline; installing \
                                             PartitionTransferDriver"
                                        );
                                        if let Err(e) = install_partition_transfer_driver(
                                            endpoint_disc,
                                            node_disc,
                                            supervisor_disc,
                                            l2_root,
                                            poh_mode,
                                            name,
                                        )
                                        .await
                                        {
                                            tracing::error!(
                                                error = %e,
                                                "failed to install PartitionTransferDriver \
                                                 after auto-discovery"
                                            );
                                        }
                                        return;
                                    }
                                    n => {
                                        if !logged_ambiguous {
                                            tracing::warn!(
                                                count = n,
                                                "AEON_PIPELINE_NAME unset and multiple \
                                                 pipelines registered; \
                                                 PartitionTransferDriver will not \
                                                 auto-install. Set the env var to pick one."
                                            );
                                            logged_ambiguous = true;
                                        }
                                    }
                                }
                            }
                        });
                    }

                    // Spawn background task for leader election + partition assignment.
                    // Retries for up to 120s. Each iteration checks:
                    //   1. Is a leader elected?
                    //   2. Am I the leader?
                    //   3. Are partitions already assigned?
                    // Keeps retrying because during rolling restarts, leadership
                    // can change — the first detected leader may be an old pod.
                    let bg_node = Arc::clone(&node);
                    tokio::spawn(async move {
                        let mut leader_logged = false;
                        for attempt in 1..=60 {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

                            // Check if partitions are already assigned (by any leader)
                            let pt = bg_node.partition_table_snapshot().await;
                            if pt.iter().next().is_some() {
                                tracing::info!(
                                    attempt,
                                    "Partition table already populated — assignment complete"
                                );
                                return;
                            }

                            if let Some(leader) = bg_node.current_leader().await {
                                if !leader_logged {
                                    tracing::info!(leader_id = leader, attempt, "Raft leader elected");
                                    leader_logged = true;
                                }
                                if leader == bg_node.config().node_id {
                                    match bg_node.assign_initial_partitions().await {
                                        Ok(()) => {
                                            tracing::info!(
                                                partitions = bg_node.config().num_partitions,
                                                "Initial partition assignment committed via Raft"
                                            );
                                            return;
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                error = %e,
                                                attempt,
                                                "Initial partition assignment failed — will retry"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        tracing::error!("Partition assignment not completed after 120s — cluster may be unhealthy");
                    });

                    Some(node)
                }
                None => {
                    tracing::warn!(
                        "AEON_CLUSTER_ENABLED=true but K8s env vars missing — running standalone"
                    );
                    None
                }
            }
        } else {
            None
        };

        let state = Arc::new(aeon_engine::AppState {
            registry,
            pipelines,
            supervisor,
            delivery_ledgers: dashmap::DashMap::new(),
            pipeline_controls: dashmap::DashMap::new(),
            pipeline_metrics: dashmap::DashMap::new(),
            poh_chains: dashmap::DashMap::new(),
            identities,
            authenticator: None,
            ws_host: None,
            cluster_node: cluster_node.clone(),
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });

        let listen_addr =
            std::env::var("AEON_API_ADDR").unwrap_or_else(|_| addr.to_string());

        // Log cluster status
        if let Some(ref node) = cluster_node {
            tracing::info!(
                node_id = node.config().node_id,
                partitions = node.config().num_partitions,
                peers = node.config().peers.len(),
                "Aeon serving with Raft cluster"
            );
        }

        aeon_engine::serve(state, &listen_addr)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    })
}

// ── Input validation ──────────────────────────────────────────────────

/// Validate a resource name (processor, pipeline, project).
/// Prevents path traversal (OWASP A03) and injection via names containing
/// special characters, `..`, `/`, `\`, or control chars.
fn validate_name(name: &str, kind: &str) -> Result<()> {
    if name.is_empty() {
        bail!("{kind} name must not be empty");
    }
    if name.len() > 128 {
        bail!("{kind} name must be 128 characters or fewer");
    }
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        bail!("{kind} name must not contain '..', '/', or '\\'");
    }
    if name.starts_with('.') || name.starts_with('-') {
        bail!("{kind} name must not start with '.' or '-'");
    }
    if name.chars().any(|c| c.is_control()) {
        bail!("{kind} name must not contain control characters");
    }
    // Allow alphanumeric, hyphens, underscores, dots (not leading)
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        bail!(
            "{kind} name may only contain alphanumeric characters, hyphens, underscores, and dots"
        );
    }
    Ok(())
}

// ── aeon new ───────────────────────────────────────────────────────────

fn cmd_new(name: &str, runtime: &Runtime, lang: &Lang) -> Result<()> {
    validate_name(name, "project")?;
    match (runtime, lang) {
        (Runtime::Wasm, Lang::Rust) => scaffold_wasm_rust(name),
        (Runtime::Native, Lang::Rust) => scaffold_native_rust(name),
        (Runtime::Wasm, Lang::Typescript) => scaffold_wasm_typescript(name),
        (Runtime::Native, Lang::Typescript) => {
            bail!("Native processors must be written in Rust, C, or C++")
        }
    }
}

fn scaffold_wasm_rust(name: &str) -> Result<()> {
    let dir = Path::new(name);
    if dir.exists() {
        bail!("directory '{}' already exists", name);
    }

    std::fs::create_dir_all(dir.join("src"))
        .with_context(|| format!("failed to create {}/src", name))?;

    // Cargo.toml
    let cargo_toml = format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
aeon-wasm-sdk = {{ git = "https://github.com/your-org/aeon.git", branch = "main" }}

[profile.release]
opt-level = "s"
lto = true
"#
    );
    std::fs::write(dir.join("Cargo.toml"), cargo_toml)?;

    // src/lib.rs
    let lib_rs = format!(
        r#"//! {name} — Aeon Wasm processor
//!
//! Build: cargo build --target wasm32-unknown-unknown --release

#![no_std]
extern crate alloc;

use aeon_wasm_sdk::prelude::*;

fn process(event: Event) -> Vec<Output> {{
    // Passthrough: forward payload to "output" topic
    vec![Output::new("output", event.payload.clone())]
}}

aeon_processor!(process);
"#
    );
    std::fs::write(dir.join("src").join("lib.rs"), lib_rs)?;

    // .cargo/config.toml for wasm target
    std::fs::create_dir_all(dir.join(".cargo"))?;
    std::fs::write(
        dir.join(".cargo").join("config.toml"),
        "[build]\ntarget = \"wasm32-unknown-unknown\"\n",
    )?;

    println!("Created Wasm Rust processor project: {name}/");
    println!("  cd {name}");
    println!("  cargo build --release");
    println!("  aeon validate target/wasm32-unknown-unknown/release/{name}.wasm");
    Ok(())
}

fn scaffold_native_rust(name: &str) -> Result<()> {
    let dir = Path::new(name);
    if dir.exists() {
        bail!("directory '{}' already exists", name);
    }

    std::fs::create_dir_all(dir.join("src"))
        .with_context(|| format!("failed to create {}/src", name))?;

    // Cargo.toml
    let cargo_toml = format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
aeon-native-sdk = {{ git = "https://github.com/your-org/aeon.git", branch = "main" }}
aeon-types = {{ git = "https://github.com/your-org/aeon.git", branch = "main" }}
bytes = "1"
smallvec = "1"
"#
    );
    std::fs::write(dir.join("Cargo.toml"), cargo_toml)?;

    // src/lib.rs
    let lib_rs = format!(
        r#"//! {name} — Aeon native (.so/.dll) processor

use aeon_native_sdk::prelude::*;

struct MyProcessor;

impl Processor for MyProcessor {{
    fn process(&self, event: Event) -> Result<Vec<Output>, AeonError> {{
        // Passthrough: forward payload to "output" topic
        Ok(vec![Output::new(Arc::from("output"), event.payload.clone())
            .with_event_identity(&event)])
    }}

    fn process_batch(&self, events: Vec<Event>) -> Result<Vec<Output>, AeonError> {{
        let mut outputs = Vec::with_capacity(events.len());
        for event in events {{
            outputs.extend(self.process(event)?);
        }}
        Ok(outputs)
    }}
}}

fn create(_config: &[u8]) -> Box<dyn Processor> {{
    Box::new(MyProcessor)
}}

export_processor!(create);
"#
    );
    std::fs::write(dir.join("src").join("lib.rs"), lib_rs)?;

    println!("Created native Rust processor project: {name}/");
    println!("  cd {name}");
    println!("  cargo build --release");
    let ext = if cfg!(windows) { "dll" } else { "so" };
    println!(
        "  aeon validate target/release/{name}.{ext}",
        name = name.replace('-', "_"),
        ext = ext
    );
    Ok(())
}

fn scaffold_wasm_typescript(name: &str) -> Result<()> {
    let dir = Path::new(name);
    if dir.exists() {
        bail!("directory '{}' already exists", name);
    }

    std::fs::create_dir_all(dir.join("assembly"))
        .with_context(|| format!("failed to create {}/assembly", name))?;

    // package.json
    let package_json = format!(
        r#"{{
  "name": "{name}",
  "version": "0.1.0",
  "description": "Aeon Wasm processor (TypeScript/AssemblyScript)",
  "scripts": {{
    "asbuild:debug": "asc assembly/index.ts --target debug",
    "asbuild:release": "asc assembly/index.ts --target release",
    "asbuild": "npm run asbuild:release"
  }},
  "devDependencies": {{
    "assemblyscript": "^0.27.0"
  }},
  "license": "Apache-2.0"
}}
"#
    );
    std::fs::write(dir.join("package.json"), package_json)?;

    // asconfig.json
    std::fs::write(
        dir.join("asconfig.json"),
        r#"{
  "targets": {
    "debug": {
      "outFile": "build/debug.wasm",
      "textFile": "build/debug.wat",
      "sourceMap": true,
      "debug": true
    },
    "release": {
      "outFile": "build/release.wasm",
      "textFile": "build/release.wat",
      "sourceMap": true,
      "optimizeLevel": 3,
      "shrinkLevel": 1,
      "noAssert": false
    }
  },
  "options": {
    "exportRuntime": true,
    "runtime": "stub"
  }
}
"#,
    )?;

    // assembly/tsconfig.json
    std::fs::write(
        dir.join("assembly").join("tsconfig.json"),
        r#"{
  "extends": "assemblyscript/std/assembly.json",
  "include": ["./**/*.ts"]
}
"#,
    )?;

    // assembly/index.ts
    let index_ts = format!(
        r#"/**
 * {name} — Aeon Wasm processor (TypeScript/AssemblyScript)
 *
 * Build: npm install && npm run asbuild
 * Validate: aeon validate build/release.wasm
 */

// NOTE: Import paths will resolve once @aeon/wasm-sdk is published or linked.
// For now, copy the SDK assembly/ files into your project or use a local path.
// import {{ Event, Output, registerProcessor }} from "@aeon/wasm-sdk/assembly/index";

import {{ Event, Output, Header }} from "./types";
import {{ deserializeEvent, serializeOutputs }} from "./wire";

type ProcessorFn = (event: Event) => Output[];

let _processorFn: ProcessorFn | null = null;

function registerProcessor(fn: ProcessorFn): void {{
  _processorFn = fn;
}}

// ── Your processor logic ───────────────────────────────────────────

registerProcessor((event: Event): Output[] => {{
  // Passthrough: forward payload to "output" topic
  return [new Output("output", event.payload)];
}});

// ── Wasm ABI exports ───────────────────────────────────────────────

export function alloc(size: i32): i32 {{
  return heap.alloc(size) as i32;
}}

export function dealloc(ptr: i32, size: i32): void {{
  heap.free(ptr as usize);
}}

export function process(ptr: i32, len: i32): i32 {{
  if (_processorFn === null) {{
    const emptyPtr = heap.alloc(8);
    store<u32>(emptyPtr, 4);
    store<u32>(emptyPtr + 4, 0);
    return emptyPtr as i32;
  }}
  const event = deserializeEvent(ptr as usize, len as usize);
  const outputs = _processorFn!(event);
  return serializeOutputs(outputs) as i32;
}}
"#
    );
    std::fs::write(dir.join("assembly").join("index.ts"), index_ts)?;

    println!("Created TypeScript Wasm processor project: {name}/");
    println!("  cd {name}");
    println!("  npm install");
    println!("  npm run asbuild");
    println!("  aeon validate build/release.wasm");
    Ok(())
}

// ── aeon build ─────────────────────────────────────────────────────────

fn cmd_build(release: bool, dir: Option<&Path>) -> Result<()> {
    let project_dir = dir.unwrap_or(Path::new("."));

    // Detect project type: package.json = TypeScript, Cargo.toml = Rust
    let package_json_path = project_dir.join("package.json");
    let cargo_toml_path = project_dir.join("Cargo.toml");

    if package_json_path.exists() {
        // TypeScript / AssemblyScript project
        let target = if release {
            "asbuild:release"
        } else {
            "asbuild:debug"
        };
        println!("Building TypeScript Wasm processor ({target})...");

        let status = Command::new("npm")
            .args(["run", target])
            .current_dir(project_dir)
            .status()
            .context("failed to run npm. Is Node.js installed?")?;

        if !status.success() {
            bail!("npm run {target} failed with exit code: {status}");
        }
    } else if cargo_toml_path.exists() {
        // Rust project
        let cargo_content =
            std::fs::read_to_string(&cargo_toml_path).context("failed to read Cargo.toml")?;

        let is_wasm = cargo_content.contains("aeon-wasm-sdk")
            || cargo_content.contains("wasm32-unknown-unknown");

        let mut cmd = Command::new("cargo");
        cmd.arg("build");
        cmd.current_dir(project_dir);

        if is_wasm {
            cmd.arg("--target").arg("wasm32-unknown-unknown");
            println!("Building Wasm processor...");
        } else {
            println!("Building native processor...");
        }

        if release {
            cmd.arg("--release");
        }

        let status = cmd.status().context("failed to run cargo build")?;
        if !status.success() {
            bail!("cargo build failed with exit code: {status}");
        }
    } else {
        bail!(
            "no Cargo.toml or package.json found in {}",
            project_dir.display()
        );
    }

    println!("Build successful.");
    Ok(())
}

// ── aeon validate ──────────────────────────────────────────────────────

fn cmd_validate(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("file not found: {}", path.display());
    }

    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

    match ext {
        "wasm" => validate_wasm(path),
        "so" | "dll" | "dylib" => validate_native(path),
        _ => bail!("unknown file extension '.{ext}'. Expected .wasm, .so, .dll, or .dylib"),
    }
}

fn validate_wasm(path: &Path) -> Result<()> {
    println!("Validating Wasm processor: {}", path.display());

    let wasm_bytes = std::fs::read(path).context("failed to read Wasm file")?;

    let engine = wasmtime::Engine::default();
    let module =
        wasmtime::Module::new(&engine, &wasm_bytes).context("failed to compile Wasm module")?;

    let exports: Vec<String> = module.exports().map(|e| e.name().to_string()).collect();

    let required = ["alloc", "dealloc", "process", "memory"];
    let mut missing = Vec::new();
    let mut found = Vec::new();

    for name in &required {
        if exports.iter().any(|e| e == name) {
            found.push(*name);
        } else {
            missing.push(*name);
        }
    }

    if !missing.is_empty() {
        println!("FAIL: missing required exports: {}", missing.join(", "));
        println!("  Found exports: {}", exports.join(", "));
        bail!("Wasm validation failed");
    }

    println!("  Required exports: {}", found.join(", "));

    // Check for optional host imports
    let imports: Vec<String> = module
        .imports()
        .map(|i| format!("{}::{}", i.module(), i.name()))
        .collect();

    if !imports.is_empty() {
        println!("  Host imports: {}", imports.join(", "));
    }

    println!("PASS: valid Aeon Wasm processor");
    Ok(())
}

fn validate_native(path: &Path) -> Result<()> {
    println!("Validating native processor: {}", path.display());

    #[cfg(feature = "native-validate")]
    {
        let found = aeon_engine::native_loader::NativeProcessor::validate(path)
            .context("native processor validation failed")?;
        println!("  Exported symbols: {}", found.join(", "));
        println!("PASS: valid Aeon native processor");
        Ok(())
    }

    #[cfg(not(feature = "native-validate"))]
    {
        let _ = path;
        bail!("native validation requires the 'native-validate' feature (enabled by default)")
    }
}

// ── aeon processor ─────────────────────────────────────────────────────

fn cmd_processor(api: &str, action: &ProcessorAction) -> Result<()> {
    match action {
        ProcessorAction::List => {
            let body: serde_json::Value = ureq::get(&format!("{api}/api/v1/processors"))
                .call()
                .context("failed to reach Aeon API")?
                .into_json()
                .context("invalid JSON response")?;
            let arr = body.as_array().context("expected array")?;
            if arr.is_empty() {
                println!("No processors registered.");
            } else {
                println!("{:<30} {:>8}  LATEST", "NAME", "VERSIONS");
                for item in arr {
                    println!(
                        "{:<30} {:>8}  {}",
                        item["name"].as_str().unwrap_or("-"),
                        item["version_count"].as_u64().unwrap_or(0),
                        item["latest_version"].as_str().unwrap_or("-"),
                    );
                }
            }
            Ok(())
        }
        ProcessorAction::Inspect { name } => {
            let body: serde_json::Value = ureq::get(&format!("{api}/api/v1/processors/{name}"))
                .call()
                .context("failed to reach Aeon API")?
                .into_json()?;
            println!("{}", serde_json::to_string_pretty(&body)?);
            Ok(())
        }
        ProcessorAction::Versions { name } => {
            let body: serde_json::Value =
                ureq::get(&format!("{api}/api/v1/processors/{name}/versions"))
                    .call()
                    .context("failed to reach Aeon API")?
                    .into_json()?;
            println!("{}", serde_json::to_string_pretty(&body)?);
            Ok(())
        }
        ProcessorAction::Register {
            name,
            version,
            description,
            artifact,
            platform,
        } => {
            let artifact_bytes = std::fs::read(artifact)
                .with_context(|| format!("failed to read {}", artifact.display()))?;

            // Compute SHA-512 placeholder hash (matches registry.rs sha512_hex)
            let sha512 = aeon_engine::registry::sha512_hex(&artifact_bytes);

            let processor_type = if artifact.extension().is_some_and(|e| e == "wasm") {
                "wasm"
            } else {
                "native-so"
            };

            let payload = serde_json::json!({
                "name": name,
                "description": description,
                "version": {
                    "version": version,
                    "sha512": sha512,
                    "size_bytes": artifact_bytes.len(),
                    "processor_type": processor_type,
                    "platform": platform,
                    "status": "available",
                    "registered_at": now_millis(),
                    "registered_by": "cli",
                },
            });

            let body: serde_json::Value = ureq::post(&format!("{api}/api/v1/processors"))
                .send_json(&payload)
                .context("failed to register processor")?
                .into_json()?;
            println!("{}", serde_json::to_string_pretty(&body)?);
            Ok(())
        }
        ProcessorAction::Delete { name, version } => {
            let body: serde_json::Value = ureq::delete(&format!(
                "{api}/api/v1/processors/{name}/versions/{version}"
            ))
            .call()
            .context("failed to delete processor version")?
            .into_json()?;
            println!("{}", serde_json::to_string_pretty(&body)?);
            Ok(())
        }
        ProcessorAction::Identity {
            name,
            action: id_action,
        } => cmd_identity(api, name, id_action),
    }
}

// ── aeon processor identity ──────────────────────────────────────────

fn cmd_identity(api: &str, name: &str, action: &IdentityAction) -> Result<()> {
    match action {
        IdentityAction::Add {
            public_key,
            pipelines,
            max_instances,
        } => {
            let allowed_pipelines = if pipelines == "all" {
                serde_json::json!("all-matching-pipelines")
            } else {
                let names: Vec<&str> = pipelines.split(',').map(str::trim).collect();
                serde_json::json!({"named": names})
            };

            let body = serde_json::json!({
                "public_key": public_key,
                "allowed_pipelines": allowed_pipelines,
                "max_instances": max_instances,
            });

            let resp: serde_json::Value =
                ureq::post(&format!("{api}/api/v1/processors/{name}/identities"))
                    .send_json(&body)
                    .context("failed to register identity")?
                    .into_json()?;
            println!("{}", serde_json::to_string_pretty(&resp)?);
            Ok(())
        }
        IdentityAction::List => {
            let body: serde_json::Value =
                ureq::get(&format!("{api}/api/v1/processors/{name}/identities"))
                    .call()
                    .context("failed to list identities")?
                    .into_json()?;

            let empty_vec = vec![];
            let identities = body["identities"].as_array().unwrap_or(&empty_vec);
            if identities.is_empty() {
                println!("No identities registered for '{name}'.");
            } else {
                println!("{:<50} {:<8} {:<20}", "FINGERPRINT", "STATUS", "PIPELINES");
                for id in identities {
                    let fp = id["fingerprint"].as_str().unwrap_or("?");
                    let status = if id["revoked_at"].is_null() {
                        "active"
                    } else {
                        "revoked"
                    };
                    let pipelines = serde_json::to_string(&id["allowed_pipelines"])
                        .unwrap_or_else(|_| "?".into());
                    println!("{:<50} {:<8} {:<20}", fp, status, pipelines);
                }
            }
            Ok(())
        }
        IdentityAction::Revoke { fingerprint } => {
            let encoded_fp = fingerprint.replace(':', "%3A");
            let resp: serde_json::Value = ureq::delete(&format!(
                "{api}/api/v1/processors/{name}/identities/{encoded_fp}"
            ))
            .call()
            .context("failed to revoke identity")?
            .into_json()?;
            println!("{}", serde_json::to_string_pretty(&resp)?);
            Ok(())
        }
    }
}

// ── aeon pipeline ──────────────────────────────────────────────────────

fn cmd_pipeline(api: &str, action: &PipelineAction) -> Result<()> {
    match action {
        PipelineAction::List => {
            let body: serde_json::Value = ureq::get(&format!("{api}/api/v1/pipelines"))
                .call()
                .context("failed to reach Aeon API")?
                .into_json()
                .context("invalid JSON response")?;
            let arr = body.as_array().context("expected array")?;
            if arr.is_empty() {
                println!("No pipelines.");
            } else {
                println!("{:<30} STATE", "NAME");
                for item in arr {
                    println!(
                        "{:<30} {}",
                        item["name"].as_str().unwrap_or("-"),
                        item["state"].as_str().unwrap_or("-"),
                    );
                }
            }
            Ok(())
        }
        PipelineAction::Inspect { name } => {
            let body: serde_json::Value = ureq::get(&format!("{api}/api/v1/pipelines/{name}"))
                .call()
                .context("failed to reach Aeon API")?
                .into_json()?;
            println!("{}", serde_json::to_string_pretty(&body)?);
            Ok(())
        }
        PipelineAction::Create { file } => {
            let content = std::fs::read_to_string(file)
                .with_context(|| format!("failed to read {}", file.display()))?;
            let payload: serde_json::Value =
                serde_json::from_str(&content).context("invalid JSON in pipeline definition")?;

            let body: serde_json::Value = ureq::post(&format!("{api}/api/v1/pipelines"))
                .send_json(&payload)
                .context("failed to create pipeline")?
                .into_json()?;
            println!("{}", serde_json::to_string_pretty(&body)?);
            Ok(())
        }
        PipelineAction::Start { name } => {
            let body: serde_json::Value =
                ureq::post(&format!("{api}/api/v1/pipelines/{name}/start"))
                    .send_json(serde_json::json!({}))
                    .context("failed to start pipeline")?
                    .into_json()?;
            println!("{}", serde_json::to_string_pretty(&body)?);
            Ok(())
        }
        PipelineAction::Stop { name } => {
            let body: serde_json::Value =
                ureq::post(&format!("{api}/api/v1/pipelines/{name}/stop"))
                    .send_json(serde_json::json!({}))
                    .context("failed to stop pipeline")?
                    .into_json()?;
            println!("{}", serde_json::to_string_pretty(&body)?);
            Ok(())
        }
        PipelineAction::Upgrade {
            name,
            processor,
            version,
            strategy,
            steps,
        } => {
            let endpoint = match strategy {
                UpgradeStrategyArg::DrainSwap => format!("{api}/api/v1/pipelines/{name}/upgrade"),
                UpgradeStrategyArg::BlueGreen => {
                    format!("{api}/api/v1/pipelines/{name}/upgrade/blue-green")
                }
                UpgradeStrategyArg::Canary => {
                    format!("{api}/api/v1/pipelines/{name}/upgrade/canary")
                }
            };
            let mut payload = serde_json::json!({
                "processor_name": processor,
                "processor_version": version,
            });
            if let (UpgradeStrategyArg::Canary, Some(steps_str)) = (strategy, steps) {
                let parsed: Vec<u8> = steps_str
                    .split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect();
                if !parsed.is_empty() {
                    payload["steps"] = serde_json::json!(parsed);
                }
            }
            let body: serde_json::Value = ureq::post(&endpoint)
                .send_json(&payload)
                .context("failed to upgrade pipeline")?
                .into_json()?;
            println!("{}", serde_json::to_string_pretty(&body)?);
            Ok(())
        }
        PipelineAction::Cutover { name } => {
            let body: serde_json::Value =
                ureq::post(&format!("{api}/api/v1/pipelines/{name}/cutover"))
                    .send_json(serde_json::json!({}))
                    .context("failed to cutover pipeline")?
                    .into_json()?;
            println!("{}", serde_json::to_string_pretty(&body)?);
            Ok(())
        }
        PipelineAction::Rollback { name } => {
            let body: serde_json::Value =
                ureq::post(&format!("{api}/api/v1/pipelines/{name}/rollback"))
                    .send_json(serde_json::json!({}))
                    .context("failed to rollback pipeline")?
                    .into_json()?;
            println!("{}", serde_json::to_string_pretty(&body)?);
            Ok(())
        }
        PipelineAction::Promote { name } => {
            let body: serde_json::Value =
                ureq::post(&format!("{api}/api/v1/pipelines/{name}/promote"))
                    .send_json(serde_json::json!({}))
                    .context("failed to promote canary")?
                    .into_json()?;
            println!("{}", serde_json::to_string_pretty(&body)?);
            Ok(())
        }
        PipelineAction::CanaryStatus { name } => {
            let body: serde_json::Value =
                ureq::get(&format!("{api}/api/v1/pipelines/{name}/canary-status"))
                    .call()
                    .context("failed to reach Aeon API")?
                    .into_json()?;
            println!("{}", serde_json::to_string_pretty(&body)?);
            Ok(())
        }
        PipelineAction::History { name } => {
            let body: serde_json::Value =
                ureq::get(&format!("{api}/api/v1/pipelines/{name}/history"))
                    .call()
                    .context("failed to reach Aeon API")?
                    .into_json()?;
            println!("{}", serde_json::to_string_pretty(&body)?);
            Ok(())
        }
        PipelineAction::Delete { name } => {
            let body: serde_json::Value = ureq::delete(&format!("{api}/api/v1/pipelines/{name}"))
                .call()
                .context("failed to delete pipeline")?
                .into_json()?;
            println!("{}", serde_json::to_string_pretty(&body)?);
            Ok(())
        }
    }
}

// ── aeon cluster ──────────────────────────────────────────────────────

fn cmd_cluster(api: &str, action: &ClusterAction) -> Result<()> {
    match action {
        ClusterAction::Status { watch, interval } => {
            cmd_cluster_status(api, *watch, *interval)
        }
        ClusterAction::TransferPartition { partition, target } => {
            let url = format!("{api}/api/v1/cluster/partitions/{partition}/transfer");
            let payload = serde_json::json!({ "target_node_id": target });
            post_cluster_mutation(&url, Some(payload), "transfer")
        }
        ClusterAction::Drain { node } => {
            let url = format!("{api}/api/v1/cluster/drain");
            let payload = serde_json::json!({ "node_id": node });
            post_cluster_mutation(&url, Some(payload), "drain")
        }
        ClusterAction::Rebalance => {
            let url = format!("{api}/api/v1/cluster/rebalance");
            post_cluster_mutation(&url, None, "rebalance")
        }
        ClusterAction::Leave => {
            let url = format!("{api}/api/v1/cluster/leave");
            post_cluster_mutation(&url, None, "leave")
        }
    }
}

/// POST a cluster mutation (transfer / drain / rebalance) and pretty-print the
/// response. Translates 409 Conflict into an operator-friendly "hit a follower,
/// leader is node N" message using the `X-Leader-Id` header set by the server.
fn post_cluster_mutation(
    url: &str,
    body: Option<serde_json::Value>,
    op: &str,
) -> Result<()> {
    let req = ureq::post(url);
    let result = match body {
        Some(ref json) => req.send_json(json),
        None => req.call(),
    };
    match result {
        Ok(resp) => {
            let body: serde_json::Value = resp
                .into_json()
                .with_context(|| format!("invalid JSON response from {op}"))?;
            println!("{}", serde_json::to_string_pretty(&body)?);
            Ok(())
        }
        Err(ureq::Error::Status(code, resp)) => {
            let leader_hint = resp.header("X-Leader-Id").map(|s| s.to_string());
            let body_text = resp.into_string().unwrap_or_default();
            if let Some(leader_id) = leader_hint {
                bail!(
                    "{op} rejected (HTTP {code}); current Raft leader is node {leader_id}\n\
                     body: {body_text}"
                );
            }
            bail!("{op} rejected (HTTP {code}): {body_text}");
        }
        Err(e) => Err(anyhow::Error::new(e).context("failed to reach Aeon API")),
    }
}

/// Fetch `/api/v1/cluster/status`. Once-shot prints the raw JSON (script
/// friendly); `--watch` re-renders a compact human view on every tick.
/// Ctrl-C exits (the loop runs on the main thread; SIGINT/Ctrl-C kills
/// the process, which is the right UX for a terminal watch command).
fn cmd_cluster_status(api: &str, watch: bool, interval_secs: f64) -> Result<()> {
    let url = format!("{api}/api/v1/cluster/status");
    if !watch {
        let body: serde_json::Value = ureq::get(&url)
            .call()
            .context("failed to reach Aeon API")?
            .into_json()
            .context("invalid JSON response")?;
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }

    if !interval_secs.is_finite() || interval_secs < 0.1 {
        bail!("--interval must be >= 0.1 seconds");
    }
    let sleep_ms = (interval_secs * 1000.0) as u64;

    loop {
        let rendered = match fetch_cluster_status(&url) {
            Ok(body) => render_cluster_status(&body, interval_secs),
            Err(e) => format!("error: {e}\n(retrying in {interval_secs}s)"),
        };
        // ANSI clear screen + home cursor. Every terminal Aeon targets
        // (Linux, macOS Terminal, Windows Terminal, PowerShell) handles
        // these; the old conhost does not, but that's not a supported
        // Aeon target any more.
        print!("\x1b[2J\x1b[H{rendered}");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
    }
}

fn fetch_cluster_status(url: &str) -> Result<serde_json::Value> {
    let resp = ureq::get(url).call().context("HTTP GET failed")?;
    resp.into_json::<serde_json::Value>()
        .context("invalid JSON response")
}

/// Render the `/api/v1/cluster/status` JSON body as an operator-friendly
/// plain-text block. Falls back to the raw JSON if the body shape does
/// not match the expected cluster schema (e.g. standalone mode).
fn render_cluster_status(body: &serde_json::Value, interval_secs: f64) -> String {
    let now = chrono_like_hms();
    let mode = body.get("mode").and_then(|v| v.as_str()).unwrap_or("?");

    if mode != "cluster" {
        let msg = body
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("no cluster");
        return format!(
            "Aeon cluster status  ({now})\n\nmode: {mode}\n{msg}\n\n(every {interval_secs}s, Ctrl-C to exit)\n"
        );
    }

    let node_id = body.get("node_id").and_then(|v| v.as_u64()).unwrap_or(0);
    let leader = body
        .get("leader_id")
        .and_then(|v| v.as_u64())
        .map(|n| n.to_string())
        .unwrap_or_else(|| "<none>".to_string());
    let num_partitions = body.get("num_partitions").and_then(|v| v.as_u64()).unwrap_or(0);

    let raft = body.get("raft").cloned().unwrap_or(serde_json::Value::Null);
    let raft_state = raft.get("state").and_then(|v| v.as_str()).unwrap_or("?");
    let term = raft.get("current_term").and_then(|v| v.as_u64()).unwrap_or(0);
    let last_applied = raft
        .get("last_applied")
        .and_then(|v| v.as_u64())
        .map(|n| n.to_string())
        .unwrap_or_else(|| "-".to_string());
    let last_log = raft
        .get("last_log_index")
        .and_then(|v| v.as_u64())
        .map(|n| n.to_string())
        .unwrap_or_else(|| "-".to_string());

    let mut out = String::new();
    out.push_str(&format!("Aeon cluster status  ({now})\n\n"));
    out.push_str(&format!(
        "node: {node_id}    leader: {leader}    raft: {raft_state}\n"
    ));
    out.push_str(&format!(
        "term: {term}    last_applied: {last_applied}    last_log: {last_log}\n\n"
    ));
    out.push_str(&format!("Partitions ({num_partitions} total)\n"));

    // Partitions come back as a map keyed by stringified partition id.
    // Sort numerically so the output order is stable.
    let mut rows: Vec<(u16, String)> = Vec::new();
    if let Some(map) = body.get("partitions").and_then(|v| v.as_object()) {
        for (k, v) in map.iter() {
            let pid: u16 = match k.parse() {
                Ok(n) => n,
                Err(_) => continue,
            };
            let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("?");
            let cell = match status {
                "owned" => {
                    let owner = v.get("owner").and_then(|n| n.as_u64()).unwrap_or(0);
                    format!("owned   node {owner}")
                }
                "transferring" => {
                    let src = v.get("source").and_then(|n| n.as_u64()).unwrap_or(0);
                    let tgt = v.get("target").and_then(|n| n.as_u64()).unwrap_or(0);
                    format!("transfer  {src} → {tgt}")
                }
                other => other.to_string(),
            };
            rows.push((pid, cell));
        }
    }
    rows.sort_by_key(|(pid, _)| *pid);
    for (pid, cell) in rows {
        out.push_str(&format!("  P{pid:<3}  {cell}\n"));
    }

    out.push_str(&format!(
        "\n(every {interval_secs}s, Ctrl-C to exit)\n"
    ));
    out
}

/// `HH:MM:SS` wall clock without pulling `chrono` — used only for the
/// watch header. Never parses, never does timezone arithmetic, just
/// turns local elapsed-since-epoch seconds into a stable UTC string so
/// the rendered block has *some* time marker the operator can correlate
/// with log timestamps. UTC is fine — operators cross-reference by
/// matching HH:MM:SS, not by reading off a specific zone.
fn chrono_like_hms() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let hh = (now / 3600) % 24;
    let mm = (now / 60) % 60;
    let ss = now % 60;
    format!("{hh:02}:{mm:02}:{ss:02}Z")
}

// ── aeon deploy ───────────────────────────────────────────────────────

fn cmd_deploy(
    artifact: &Path,
    pipeline: &str,
    processor: &str,
    version: &str,
    api: &str,
) -> Result<()> {
    if !artifact.exists() {
        bail!("artifact not found: {}", artifact.display());
    }

    let artifact_bytes = std::fs::read(artifact)
        .with_context(|| format!("failed to read {}", artifact.display()))?;

    let sha512 = aeon_engine::registry::sha512_hex(&artifact_bytes);
    let processor_type = if artifact.extension().is_some_and(|e| e == "wasm") {
        "wasm"
    } else {
        "native-so"
    };

    // Step 1: Register processor
    println!("Registering {processor}:{version}...");
    let payload = serde_json::json!({
        "name": processor,
        "description": "",
        "version": {
            "version": version,
            "sha512": sha512,
            "size_bytes": artifact_bytes.len(),
            "processor_type": processor_type,
            "platform": "wasm32",
            "status": "available",
            "registered_at": now_millis(),
            "registered_by": "cli",
        },
    });
    let _: serde_json::Value = ureq::post(&format!("{api}/api/v1/processors"))
        .send_json(&payload)
        .context("failed to register processor")?
        .into_json()?;

    // Step 2: Upgrade pipeline
    println!("Upgrading pipeline '{pipeline}' to {processor}:{version}...");
    let upgrade_payload = serde_json::json!({
        "processor_name": processor,
        "processor_version": version,
    });
    let _: serde_json::Value = ureq::post(&format!("{api}/api/v1/pipelines/{pipeline}/upgrade"))
        .send_json(&upgrade_payload)
        .context("failed to upgrade pipeline")?
        .into_json()?;

    println!("Deployed {processor}:{version} to pipeline '{pipeline}'.");
    Ok(())
}

// ── aeon top ──────────────────────────────────────────────────────────

fn cmd_top(api: &str) -> Result<()> {
    println!("Aeon Pipeline Dashboard ({})", api);
    println!("{}", "=".repeat(70));

    let pipelines: serde_json::Value = ureq::get(&format!("{api}/api/v1/pipelines"))
        .call()
        .context("failed to reach Aeon API")?
        .into_json()?;

    let arr = pipelines.as_array().context("expected array")?;
    if arr.is_empty() {
        println!("No pipelines.");
        return Ok(());
    }

    println!(
        "{:<25} {:<12} {:<20} {:<12}",
        "PIPELINE", "STATE", "PROCESSOR", "UPGRADE"
    );
    println!("{}", "-".repeat(70));

    for item in arr {
        let name = item["name"].as_str().unwrap_or("-");

        // Fetch full details
        let detail: serde_json::Value =
            match ureq::get(&format!("{api}/api/v1/pipelines/{name}")).call() {
                Ok(resp) => resp.into_json().unwrap_or_default(),
                Err(_) => serde_json::json!({}),
            };

        let state = detail["state"]
            .as_str()
            .unwrap_or(item["state"].as_str().unwrap_or("-"));
        let proc_name = detail["processor"]["name"].as_str().unwrap_or("-");
        let proc_ver = detail["processor"]["version"].as_str().unwrap_or("-");
        let proc_str = format!("{proc_name}:{proc_ver}");

        let upgrade = if detail["upgrade_state"].is_object() {
            let strategy = detail["upgrade_state"]["strategy"]
                .as_str()
                .unwrap_or("unknown");
            match strategy {
                "canary" => {
                    let pct = detail["upgrade_state"]["traffic_pct"].as_u64().unwrap_or(0);
                    format!("canary {pct}%")
                }
                "blue-green" => {
                    let active = detail["upgrade_state"]["active"].as_str().unwrap_or("blue");
                    format!("bg:{active}")
                }
                _ => strategy.to_string(),
            }
        } else {
            "-".to_string()
        };

        println!(
            "{:<25} {:<12} {:<20} {:<12}",
            name, state, proc_str, upgrade
        );
    }
    Ok(())
}

// ── aeon verify ───────────────────────────────────────────────────────

fn cmd_verify(target: &str, api: &str) -> Result<()> {
    use aeon_crypto::hash::sha512;
    use aeon_crypto::merkle::MerkleTree;
    use aeon_crypto::mmr::MerkleMountainRange;
    use aeon_crypto::poh::PohChain;
    use aeon_types::PartitionId;

    println!("Aeon Integrity Verification");
    println!("{}", "=".repeat(60));
    println!("Target: {target}");
    println!("API:    {api}");
    println!();

    // Step 1: Validate local crypto modules are functional
    println!("Local module validation:");

    // PoH chain: create, append, verify
    let mut chain = PohChain::new(PartitionId::new(0), 64);
    chain.append_batch(&[b"test-event-1", b"test-event-2"], 1_000_000, None);
    chain.append_batch(&[b"test-event-3"], 2_000_000, None);
    let poh_ok = chain.verify_recent().is_none(); // None = all valid
    println!(
        "  PoH chain:    {} (seq={}, hash={}..)",
        if poh_ok { "OK" } else { "FAIL" },
        chain.sequence(),
        &chain.current_hash().to_hex()[..16],
    );

    // Merkle tree: build, prove, verify
    let tree = MerkleTree::from_data(&[b"a", b"b", b"c", b"d"]);
    let merkle_ok = match &tree {
        Some(t) => {
            let proof = t.proof(0);
            proof.is_some_and(|p: aeon_crypto::merkle::MerkleProof| p.verify())
        }
        None => false,
    };
    println!(
        "  Merkle tree:  {} (root={}..)",
        if merkle_ok { "OK" } else { "FAIL" },
        tree.as_ref()
            .map(|t| t.root().to_hex()[..16].to_string())
            .unwrap_or_else(|| "none".to_string()),
    );

    // MMR: append, verify root
    let mut mmr = MerkleMountainRange::new();
    mmr.append(sha512(b"leaf-0"));
    mmr.append(sha512(b"leaf-1"));
    mmr.append(sha512(b"leaf-2"));
    let mmr_ok = mmr.root() != aeon_crypto::hash::Hash512::ZERO;
    println!(
        "  MMR:          {} (leaves={}, root={}..)",
        if mmr_ok { "OK" } else { "FAIL" },
        mmr.leaf_count(),
        &mmr.root().to_hex()[..16],
    );

    // Signing: generate key, sign, verify
    let signing_key = aeon_crypto::signing::SigningKey::generate();
    let msg = b"integrity-check";
    let sig = signing_key.sign(msg);
    let vk = signing_key.verifying_key();
    let sig_ok = vk.verify(msg, &sig);
    let pubkey_hex = hex::encode(vk.to_bytes());
    println!(
        "  Ed25519 sign: {} (pubkey={}..)",
        if sig_ok { "OK" } else { "FAIL" },
        &pubkey_hex[..16],
    );

    let all_local_ok = poh_ok && merkle_ok && mmr_ok && sig_ok;
    println!();

    // Step 2: Query the API for pipeline verification state
    println!("Remote pipeline verification:");

    let url = format!("{api}/api/v1/pipelines/{target}/verify");
    match ureq::get(&url).call() {
        Ok(resp) => {
            let body: serde_json::Value = resp.into_json().context("invalid JSON response")?;

            let poh_active = body["poh_active"].as_bool().unwrap_or(false);
            let status = body["status"].as_str().unwrap_or("unknown");

            if target == "all" {
                // System-wide report
                let pipelines = body["pipelines"].as_array();
                match pipelines {
                    Some(arr) if !arr.is_empty() => {
                        println!("  {:<25} {:<12} {:<10}", "PIPELINE", "STATE", "POH");
                        println!("  {}", "-".repeat(47));
                        for p in arr {
                            let name = p["name"].as_str().unwrap_or("-");
                            let state = p["state"].as_str().unwrap_or("-");
                            let active = if p["poh_active"].as_bool().unwrap_or(false) {
                                "active"
                            } else {
                                "pending"
                            };
                            println!("  {:<25} {:<12} {:<10}", name, state, active);
                        }
                    }
                    _ => println!("  No pipelines found."),
                }
            } else {
                // Single pipeline report
                println!("  Pipeline:   {target}");
                println!("  Status:     {status}");
                println!(
                    "  PoH active: {}",
                    if poh_active {
                        "yes"
                    } else {
                        "no (pending runtime wiring)"
                    }
                );

                // Show chain heads if present
                if let Some(heads) = body["chain_heads"].as_array() {
                    if !heads.is_empty() {
                        println!();
                        println!(
                            "  {:<12} {:<10} {:<34}",
                            "PARTITION", "SEQUENCE", "HEAD HASH"
                        );
                        println!("  {}", "-".repeat(56));
                        for head in heads {
                            let part = head["partition"].as_u64().unwrap_or(0);
                            let seq = head["sequence"].as_u64().unwrap_or(0);
                            let hash = head["hash"].as_str().unwrap_or("-");
                            let hash_short = if hash.len() > 32 { &hash[..32] } else { hash };
                            println!("  {:<12} {:<10} {}...", part, seq, hash_short);
                        }
                    }
                }

                if let Some(agg) = body["aggregate_hash"].as_str() {
                    println!("  Aggregate:  {}...", &agg[..32.min(agg.len())]);
                }
            }

            // Module availability
            if let Some(modules) = body["modules"].as_object() {
                println!();
                println!("  Crypto modules:");
                for (name, status) in modules {
                    println!("    {:<10} {}", name, status.as_str().unwrap_or("unknown"));
                }
            }
        }
        Err(ureq::Error::Status(404, _)) => {
            println!("  Pipeline '{target}' not found on {api}");
        }
        Err(e) => {
            println!("  Could not reach API at {api}: {e}");
            println!("  (Run `aeon verify` while the Aeon server is running)");
        }
    }

    println!();
    println!("Result:");
    if all_local_ok {
        println!("  All local crypto modules operational.");
    } else {
        println!("  WARNING: One or more crypto modules failed self-test!");
    }

    Ok(())
}

// ── aeon apply / export / diff ─────────────────────────────────────────

/// A declarative manifest for processors and pipelines.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Manifest {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pipelines: Vec<aeon_types::manifest::PipelineManifest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    identities: Vec<ManifestIdentity>,
}

/// Identity declaration within a YAML manifest.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct ManifestIdentity {
    /// Processor name this identity belongs to.
    processor: String,
    /// ED25519 public key (e.g. "ed25519:<base64>").
    public_key: String,
    /// Pipeline scope: "all" or comma-separated names.
    #[serde(default = "default_pipelines_all")]
    pipelines: String,
    /// Maximum concurrent connections.
    #[serde(default = "default_max_instances")]
    max_instances: u32,
}

fn default_pipelines_all() -> String {
    "all".into()
}

fn default_max_instances() -> u32 {
    1
}

/// Export-only manifest shape. Uses opaque JSON for pipelines since the server
/// returns `PipelineDefinition`, not `PipelineManifest`.
#[derive(serde::Serialize)]
struct ExportManifest {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pipelines: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    identities: Vec<ManifestIdentity>,
}

/// Maximum YAML manifest size (1 MB) to prevent resource exhaustion (OWASP A04).
const MAX_MANIFEST_SIZE: u64 = 1024 * 1024;

fn cmd_apply(file: &Path, api: &str, dry_run: bool) -> Result<()> {
    let metadata =
        std::fs::metadata(file).with_context(|| format!("failed to stat {}", file.display()))?;
    if metadata.len() > MAX_MANIFEST_SIZE {
        bail!(
            "manifest file exceeds maximum size ({} bytes > {} bytes)",
            metadata.len(),
            MAX_MANIFEST_SIZE
        );
    }
    let content = std::fs::read_to_string(file)
        .with_context(|| format!("failed to read {}", file.display()))?;
    let manifest: Manifest = serde_yaml::from_str(&content).context("invalid YAML manifest")?;

    if manifest.pipelines.is_empty() && manifest.identities.is_empty() {
        println!("Manifest is empty — nothing to apply.");
        return Ok(());
    }

    // Fetch existing pipelines
    let existing: serde_json::Value = ureq::get(&format!("{api}/api/v1/pipelines"))
        .call()
        .context("failed to reach Aeon API")?
        .into_json()?;
    let existing_names: Vec<String> = existing
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|p| p["name"].as_str().map(String::from))
        .collect();

    for pipeline_manifest in &manifest.pipelines {
        let name = &pipeline_manifest.name;

        // EO-2 P6: validate manifest shape before sending to the API.
        aeon_types::manifest::validate_pipeline_shape(pipeline_manifest)
            .with_context(|| format!("validation failed for pipeline '{name}'"))?;

        for source in &pipeline_manifest.sources {
            aeon_types::manifest::validate_source_shape(
                &source.name,
                source.kind,
                &source.identity,
                &source.event_time,
                // Conservative: assume no broker-ts support at the CLI layer.
                // The engine will re-validate with the actual connector at start.
                source.kind == aeon_types::SourceKind::Pull,
            )
            .with_context(|| {
                format!("source '{}' validation failed in pipeline '{name}'", source.name)
            })?;
        }

        let pipeline_def = pipeline_manifest.to_pipeline_definition();
        let exists = existing_names.contains(&name.to_string());

        if dry_run {
            if exists {
                println!("[dry-run] pipeline '{name}' — already exists (skip)");
            } else {
                println!(
                    "[dry-run] pipeline '{name}' — would create ({} source(s), {} sink(s))",
                    pipeline_manifest.sources.len(),
                    pipeline_manifest.sinks.len(),
                );
            }
        } else if exists {
            println!("pipeline '{name}' — already exists (skip)");
        } else {
            ureq::post(&format!("{api}/api/v1/pipelines"))
                .send_json(&pipeline_def)
                .with_context(|| format!("failed to create pipeline '{name}'"))?;
            println!("pipeline '{name}' — created");
        }
    }

    // Apply identities
    for identity in &manifest.identities {
        let allowed_pipelines = if identity.pipelines == "all" {
            serde_json::json!("all-matching-pipelines")
        } else {
            let names: Vec<&str> = identity.pipelines.split(',').map(str::trim).collect();
            serde_json::json!({"named": names})
        };

        let body = serde_json::json!({
            "public_key": identity.public_key,
            "allowed_pipelines": allowed_pipelines,
            "max_instances": identity.max_instances,
        });

        if dry_run {
            println!(
                "[dry-run] identity for '{}' (key={}...) — would register",
                identity.processor,
                &identity.public_key[..identity.public_key.len().min(20)]
            );
        } else {
            match ureq::post(&format!(
                "{api}/api/v1/processors/{}/identities",
                identity.processor
            ))
            .send_json(&body)
            {
                Ok(_) => {
                    println!(
                        "identity for '{}' (key={}...) — registered",
                        identity.processor,
                        &identity.public_key[..identity.public_key.len().min(20)]
                    );
                }
                Err(e) => {
                    eprintln!("identity for '{}' — failed: {e}", identity.processor);
                }
            }
        }
    }

    if dry_run {
        println!("\nDry run complete. No changes applied.");
    } else {
        println!("\nManifest applied.");
    }
    Ok(())
}

fn cmd_export(file: Option<&Path>, api: &str) -> Result<()> {
    let pipelines: serde_json::Value = ureq::get(&format!("{api}/api/v1/pipelines"))
        .call()
        .context("failed to reach Aeon API")?
        .into_json()?;

    // Fetch full details for each pipeline
    let mut full_pipelines = Vec::new();
    if let Some(arr) = pipelines.as_array() {
        for p in arr {
            if let Some(name) = p["name"].as_str() {
                let detail: serde_json::Value =
                    ureq::get(&format!("{api}/api/v1/pipelines/{name}"))
                        .call()
                        .with_context(|| format!("failed to fetch pipeline '{name}'"))?
                        .into_json()?;
                full_pipelines.push(detail);
            }
        }
    }

    // Fetch identities for each known processor
    let mut identities = Vec::new();
    let processors: serde_json::Value = ureq::get(&format!("{api}/api/v1/processors"))
        .call()
        .ok()
        .and_then(|r| r.into_json().ok())
        .unwrap_or(serde_json::json!([]));
    if let Some(procs) = processors.as_array() {
        for p in procs {
            let Some(proc_name) = p["name"].as_str() else {
                continue;
            };
            if let Ok(resp) =
                ureq::get(&format!("{api}/api/v1/processors/{proc_name}/identities")).call()
            {
                if let Ok(body) = resp.into_json::<serde_json::Value>() {
                    if let Some(ids) = body["identities"].as_array() {
                        for id in ids {
                            if id["revoked_at"].is_null() {
                                let pipelines = match &id["allowed_pipelines"] {
                                    serde_json::Value::String(s)
                                        if s == "all-matching-pipelines" =>
                                    {
                                        "all".to_string()
                                    }
                                    serde_json::Value::Object(o) => {
                                        if let Some(named) =
                                            o.get("named").and_then(|n| n.as_array())
                                        {
                                            named
                                                .iter()
                                                .filter_map(|v| v.as_str())
                                                .collect::<Vec<_>>()
                                                .join(",")
                                        } else {
                                            "all".to_string()
                                        }
                                    }
                                    _ => "all".to_string(),
                                };
                                identities.push(ManifestIdentity {
                                    processor: proc_name.to_string(),
                                    public_key: id["public_key"].as_str().unwrap_or("").to_string(),
                                    pipelines,
                                    max_instances: id["max_instances"].as_u64().unwrap_or(1) as u32,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    let export = ExportManifest {
        pipelines: full_pipelines,
        identities,
    };
    let yaml = serde_yaml::to_string(&export).context("failed to serialize manifest")?;

    match file {
        Some(path) => {
            std::fs::write(path, &yaml)
                .with_context(|| format!("failed to write {}", path.display()))?;
            println!("Exported to {}", path.display());
        }
        None => print!("{yaml}"),
    }
    Ok(())
}

fn cmd_diff(file: &Path, api: &str) -> Result<()> {
    let content = std::fs::read_to_string(file)
        .with_context(|| format!("failed to read {}", file.display()))?;
    let manifest: Manifest = serde_yaml::from_str(&content).context("invalid YAML manifest")?;

    let existing: serde_json::Value = ureq::get(&format!("{api}/api/v1/pipelines"))
        .call()
        .context("failed to reach Aeon API")?
        .into_json()?;
    let existing_names: Vec<String> = existing
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|p| p["name"].as_str().map(String::from))
        .collect();

    let manifest_names: Vec<String> = manifest
        .pipelines
        .iter()
        .map(|p| p.name.clone())
        .collect();

    let mut changes = false;
    for name in &manifest_names {
        if !existing_names.contains(name) {
            println!("+ pipeline '{name}' (would be created)");
            changes = true;
        }
    }
    for name in &existing_names {
        if !manifest_names.contains(name) {
            println!("- pipeline '{name}' (exists but not in manifest)");
            changes = true;
        }
    }
    for name in &manifest_names {
        if existing_names.contains(name) {
            println!("  pipeline '{name}' (unchanged)");
        }
    }

    // Compare identities
    for identity in &manifest.identities {
        let key_preview = &identity.public_key[..identity.public_key.len().min(20)];
        println!(
            "? identity for '{}' (key={key_preview}...) — cannot diff (requires live check)",
            identity.processor
        );
        changes = true;
    }

    if !changes {
        println!("\nNo differences.");
    }
    Ok(())
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// ── aeon dev ───────────────────────────────────────────────────────────

fn cmd_dev(action: &DevAction) -> Result<()> {
    // Find the compose file relative to the Aeon workspace root.
    // Look for docker/docker-compose.dev.yml in current dir or parents.
    let compose_file = find_compose_file()?;

    match action {
        DevAction::Up => {
            println!("Starting Aeon dev environment...");
            let status = Command::new("docker")
                .args([
                    "compose",
                    "-f",
                    &compose_file.to_string_lossy(),
                    "-p",
                    "aeon-dev",
                    "up",
                    "-d",
                ])
                .status()
                .context("failed to run docker compose. Is Docker installed?")?;

            if !status.success() {
                bail!("docker compose up failed");
            }
            println!();
            println!("Aeon dev environment is running:");
            println!("  Redpanda Kafka:   localhost:19092");
            println!("  Redpanda Console: http://localhost:8080");
            println!("  Schema Registry:  http://localhost:18081");
            println!();
            println!("Topics: aeon-source, aeon-sink, aeon-dlq");
            println!("Stop with: aeon dev down");
            Ok(())
        }
        DevAction::Down => {
            println!("Stopping Aeon dev environment...");
            let status = Command::new("docker")
                .args([
                    "compose",
                    "-f",
                    &compose_file.to_string_lossy(),
                    "-p",
                    "aeon-dev",
                    "down",
                ])
                .status()
                .context("failed to run docker compose")?;

            if !status.success() {
                bail!("docker compose down failed");
            }
            println!("Aeon dev environment stopped.");
            Ok(())
        }
        DevAction::Status => {
            let status = Command::new("docker")
                .args([
                    "compose",
                    "-f",
                    &compose_file.to_string_lossy(),
                    "-p",
                    "aeon-dev",
                    "ps",
                ])
                .status()
                .context("failed to run docker compose ps")?;

            if !status.success() {
                bail!("docker compose ps failed");
            }
            Ok(())
        }
        // Watch is handled before cmd_dev is called (needs async runtime)
        #[cfg(feature = "rest-api")]
        DevAction::Watch { .. } => unreachable!("watch handled in main dispatch"),
    }
}

/// Find docker/docker-compose.dev.yml by walking up from cwd.
fn find_compose_file() -> Result<PathBuf> {
    let target = "docker/docker-compose.dev.yml";
    let mut dir = std::env::current_dir().context("failed to get current directory")?;

    loop {
        let candidate = dir.join(target);
        if candidate.exists() {
            return Ok(candidate);
        }
        if !dir.pop() {
            break;
        }
    }

    bail!(
        "could not find {target}. Run `aeon dev` from within the Aeon workspace, \
         or ensure docker/docker-compose.dev.yml exists."
    )
}

// ── aeon dev watch ─────────────────────────────────────────────────────

#[cfg(feature = "rest-api")]
async fn cmd_dev_watch(artifact: &Path) -> Result<()> {
    use aeon_connectors::StdoutSink;
    use aeon_engine::{PipelineConfig, PipelineControl, PipelineMetrics, run_buffered_managed};
    use aeon_types::{AeonError, Event, Processor, Source};
    use notify::{RecommendedWatcher, RecursiveMode, Watcher, EventKind};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    // Validate artifact exists
    if !artifact.exists() {
        bail!("artifact not found: {}", artifact.display());
    }

    // Load the initial processor
    let load_processor = |path: &Path| -> Result<Box<dyn Processor + Send + Sync>> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
            "wasm" | "wat" => {
                let module = aeon_wasm::WasmModule::from_bytes(
                    &bytes,
                    aeon_wasm::WasmConfig::default(),
                )
                .map_err(|e| anyhow::anyhow!("failed to compile Wasm: {e}"))?;
                let proc = aeon_wasm::WasmProcessor::new(Arc::new(module))
                    .map_err(|e| anyhow::anyhow!("failed to instantiate Wasm processor: {e}"))?;
                Ok(Box::new(proc))
            }
            "so" | "dylib" | "dll" => {
                let proc = aeon_engine::native_loader::NativeProcessor::load(path, &[])
                    .map_err(|e| anyhow::anyhow!("failed to load native processor: {e}"))?;
                Ok(Box::new(proc))
            }
            other => bail!("unsupported artifact extension: .{other} (expected .wasm, .so, .dll)"),
        }
    };

    let processor = load_processor(artifact)?;
    println!("Loaded processor from {}", artifact.display());

    // Create a tick source that produces one synthetic event per second.
    struct TickSource {
        counter: u64,
        shutdown: Arc<AtomicBool>,
    }
    impl Source for TickSource {
        async fn next_batch(&mut self) -> Result<Vec<Event>, AeonError> {
            if self.shutdown.load(Ordering::Relaxed) {
                return Ok(Vec::new());
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            if self.shutdown.load(Ordering::Relaxed) {
                return Ok(Vec::new());
            }
            self.counter += 1;
            let source: Arc<str> = Arc::from("dev-tick");
            let event = Event::new(
                ::uuid::Uuid::new_v4(),
                chrono_now_nanos(),
                source,
                aeon_types::PartitionId::new(0),
                bytes::Bytes::from(format!("{{\"tick\":{}}}", self.counter)),
            );
            Ok(vec![event])
        }
        async fn pause(&mut self) {}
        async fn resume(&mut self) {}
    }

    fn chrono_now_nanos() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64
    }

    let source_shutdown = Arc::new(AtomicBool::new(false));
    let source = TickSource {
        counter: 0,
        shutdown: Arc::clone(&source_shutdown),
    };
    let sink = StdoutSink::new();
    let config = PipelineConfig::default();
    let metrics = Arc::new(PipelineMetrics::default());
    let shutdown = Arc::new(AtomicBool::new(false));
    let control = PipelineControl::new();

    let control_watcher = Arc::clone(&control);
    let shutdown_watcher = Arc::clone(&shutdown);
    let artifact_path = artifact.to_path_buf();

    // Spawn the pipeline
    let control_pipeline = Arc::clone(&control);
    let metrics_pipeline = Arc::clone(&metrics);
    let shutdown_pipeline = Arc::clone(&shutdown);
    let pipeline_handle = tokio::spawn(async move {
        run_buffered_managed(
            source,
            processor,
            sink,
            config,
            metrics_pipeline,
            shutdown_pipeline,
            None,
            control_pipeline,
        )
        .await
    });

    println!();
    println!("Pipeline running (1 event/sec → processor → stdout)");
    println!("Watching: {}", artifact.display());
    println!("Press Ctrl+C to stop.");
    println!();

    // Set up file watcher with debounce
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = RecommendedWatcher::new(tx, notify::Config::default())
        .context("failed to create file watcher")?;

    // Watch the parent directory (some editors delete+recreate files)
    let watch_dir = artifact.parent().unwrap_or(Path::new("."));
    watcher
        .watch(watch_dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("failed to watch {}", watch_dir.display()))?;

    let artifact_canonical = artifact.canonicalize().unwrap_or(artifact.to_path_buf());

    // Ctrl+C handler
    let shutdown_ctrlc = Arc::clone(&shutdown);
    let source_shutdown_ctrlc = Arc::clone(&source_shutdown);
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        println!("\nShutting down...");
        source_shutdown_ctrlc.store(true, Ordering::Release);
        shutdown_ctrlc.store(true, Ordering::Release);
    });

    // File watcher loop (runs on blocking thread to not block tokio)
    let watcher_handle = tokio::task::spawn_blocking(move || {
        let mut last_reload = std::time::Instant::now();
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(Ok(event)) => {
                    // Only react to Create/Modify for our artifact
                    let dominated = matches!(
                        event.kind,
                        EventKind::Create(_) | EventKind::Modify(_)
                    );
                    let is_our_file = event.paths.iter().any(|p| {
                        p.canonicalize().unwrap_or(p.clone()) == artifact_canonical
                    });
                    if !dominated || !is_our_file {
                        continue;
                    }

                    // Debounce: skip if last reload was <500ms ago
                    if last_reload.elapsed() < std::time::Duration::from_millis(500) {
                        continue;
                    }

                    // Small delay to let writes complete
                    std::thread::sleep(std::time::Duration::from_millis(100));

                    match load_processor(&artifact_path) {
                        Ok(new_processor) => {
                            println!("\n--- Reloading processor ---");
                            let ctrl = Arc::clone(&control_watcher);
                            // Use a new tokio runtime for the async swap
                            // (we're in a blocking thread)
                            let rt = tokio::runtime::Handle::current();
                            match rt.block_on(ctrl.drain_and_swap(new_processor)) {
                                Ok(()) => {
                                    println!("--- Processor reloaded ---\n");
                                    last_reload = std::time::Instant::now();
                                }
                                Err(e) => {
                                    eprintln!("Failed to swap processor: {e}");
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("Failed to load updated artifact: {e}");
                        }
                    }
                }
                Ok(Err(e)) => {
                    eprintln!("File watcher error: {e}");
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if shutdown_watcher.load(Ordering::Relaxed) {
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    // Wait for pipeline to finish (Ctrl+C triggers shutdown)
    let _ = pipeline_handle.await;
    let _ = watcher_handle.await;

    let processed = metrics.events_processed.load(Ordering::Relaxed);
    println!("Dev session complete. Processed {processed} events.");
    Ok(())
}

// ── aeon tls ──────────────────────────────────────────────────────────

fn cmd_tls(action: &TlsAction) -> Result<()> {
    match action {
        TlsAction::ExportCa { ca, output } => {
            let ca_pem = std::fs::read_to_string(ca)
                .with_context(|| format!("failed to read CA file: {}", ca.display()))?;

            // Validate it looks like a PEM certificate
            if !ca_pem.contains("-----BEGIN CERTIFICATE-----") {
                bail!("file does not contain a PEM certificate: {}", ca.display());
            }

            match output {
                Some(path) => {
                    std::fs::write(path, &ca_pem)
                        .with_context(|| format!("failed to write to {}", path.display()))?;
                    println!("CA certificate exported to {}", path.display());
                }
                None => {
                    print!("{ca_pem}");
                }
            }
            Ok(())
        }
        TlsAction::Info { cert } => {
            let pem_data = std::fs::read(cert)
                .with_context(|| format!("failed to read cert file: {}", cert.display()))?;

            let certs: Vec<_> = rustls_pemfile::certs(&mut pem_data.as_slice())
                .collect::<Result<Vec<_>, _>>()
                .with_context(|| "failed to parse PEM certificates")?;

            if certs.is_empty() {
                bail!("no certificates found in {}", cert.display());
            }

            for (i, der) in certs.iter().enumerate() {
                println!("Certificate #{} ({} bytes DER)", i, der.as_ref().len());
                if let Some(expiry) =
                    aeon_crypto::tls::CertificateStore::parse_cert_expiry_secs(der.as_ref())
                {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    let days = (expiry - now) / 86400;
                    println!("  Expires: epoch {expiry} ({days} days from now)");
                } else {
                    println!("  Expires: (could not parse)");
                }
            }
            Ok(())
        }
    }
}

// ── aeon doctor (DX-2) ────────────────────────────────────────────────

/// Status of a single doctor check.
#[derive(Debug, Clone, Copy, PartialEq)]
enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

impl CheckStatus {
    fn tag(&self) -> &'static str {
        match self {
            Self::Pass => "[PASS]",
            Self::Warn => "[WARN]",
            Self::Fail => "[FAIL]",
        }
    }
}

/// One line of doctor output.
struct CheckResult {
    status: CheckStatus,
    name: String,
    detail: String,
    /// Optional actionable fix suggestion on warn/fail.
    fix: Option<String>,
}

impl CheckResult {
    fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status: CheckStatus::Pass,
            name: name.into(),
            detail: detail.into(),
            fix: None,
        }
    }
    fn warn(name: impl Into<String>, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            status: CheckStatus::Warn,
            name: name.into(),
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }
    fn fail(name: impl Into<String>, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            status: CheckStatus::Fail,
            name: name.into(),
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }
    fn print(&self) {
        println!("  {} {}: {}", self.status.tag(), self.name, self.detail);
        if let Some(fix) = &self.fix {
            println!("         fix: {fix}");
        }
    }
}

fn cmd_doctor(api: &str, kafka: &str, state_dir: &Path, manifest: Option<&Path>) -> Result<()> {
    println!("Aeon doctor — environment readiness check");
    println!("aeon v{}\n", env!("CARGO_PKG_VERSION"));

    let mut results: Vec<CheckResult> = Vec::new();

    // (1) Compiled features — helps users diagnose "feature not compiled" errors.
    results.push(check_compiled_features());

    // (2) Port availability for in-process Aeon defaults.
    //
    // IMPORTANT: 4470 (QUIC) and 4472 (WebTransport/HTTP3) are UDP; probing them
    // with a TCP listener always succeeds even when Aeon is running, which
    // produces a false PASS. Probe with the correct L4 protocol per port.
    println!("Ports:");
    for (port, proto, label) in [
        (4470u16, PortProto::Udp, "cluster QUIC (Raft/PoH)"),
        (4471, PortProto::Tcp, "REST API + T4 WebSocket + /metrics"),
        (4472, PortProto::Udp, "T3 WebTransport + external QUIC"),
    ] {
        let r = check_port_available(port, proto, label);
        r.print();
        results.push(r);
    }

    // (3) REST API reachability.
    println!("\nREST API:");
    let r = check_rest_api(api);
    r.print();
    results.push(r);

    // (4) Kafka/Redpanda reachability.
    println!("\nKafka/Redpanda:");
    let r = check_tcp_reachable("kafka bootstrap", kafka);
    r.print();
    results.push(r);

    // (5) State directory writability.
    println!("\nState directory:");
    let r = check_state_dir(state_dir);
    r.print();
    results.push(r);

    // (6) Manifest validation (if provided).
    if let Some(path) = manifest {
        println!("\nManifest:");
        let (r, artifact_paths) = check_manifest(path);
        r.print();
        results.push(r);

        if !artifact_paths.is_empty() {
            println!("\nArtifacts referenced by manifest:");
            for p in artifact_paths {
                let r = check_artifact(&p);
                r.print();
                results.push(r);
            }
        }
    }

    // Summary.
    let (pass, warn, fail) = results.iter().fold((0, 0, 0), |(p, w, f), r| match r.status {
        CheckStatus::Pass => (p + 1, w, f),
        CheckStatus::Warn => (p, w + 1, f),
        CheckStatus::Fail => (p, w, f + 1),
    });
    println!("\n─────");
    println!("Summary: {pass} pass, {warn} warn, {fail} fail");

    if fail > 0 {
        bail!("{fail} check(s) failed");
    }
    Ok(())
}

/// List compile-time features so users can tell at a glance what this binary
/// supports (kafka/wasm/rest-api/webtransport-insecure, etc.).
fn check_compiled_features() -> CheckResult {
    let mut feats: Vec<&'static str> = Vec::new();
    if cfg!(feature = "native-validate") {
        feats.push("native-validate");
    }
    if cfg!(feature = "rest-api") {
        feats.push("rest-api");
    }
    let detail = if feats.is_empty() {
        "(none — running minimal build)".to_string()
    } else {
        feats.join(", ")
    };
    println!("Build:");
    let r = CheckResult::pass("compiled features", detail);
    r.print();
    r
}

/// L4 protocol for port availability probes. TCP binds a `TcpListener`; UDP binds
/// a `UdpSocket`. Using the wrong one produces a false PASS because TCP and UDP
/// port namespaces are independent.
#[derive(Clone, Copy)]
enum PortProto {
    Tcp,
    Udp,
}

impl PortProto {
    fn as_str(self) -> &'static str {
        match self {
            PortProto::Tcp => "tcp",
            PortProto::Udp => "udp",
        }
    }
}

/// A port is "available" if we can bind it on 127.0.0.1 with the correct L4
/// protocol. If bind fails, something is likely already listening — typically
/// fine for REST API (server running) but a conflict if the user is trying
/// to start a fresh Aeon instance.
fn check_port_available(port: u16, proto: PortProto, label: &str) -> CheckResult {
    let addr = format!("127.0.0.1:{port}");
    let bind_result: std::io::Result<()> = match proto {
        PortProto::Tcp => std::net::TcpListener::bind(&addr).map(|_| ()),
        PortProto::Udp => std::net::UdpSocket::bind(&addr).map(|_| ()),
    };
    let port_label = format!("port {port}/{}", proto.as_str());
    match bind_result {
        Ok(()) => CheckResult::pass(port_label, format!("{label}: available")),
        Err(e) => CheckResult::warn(
            port_label,
            format!("{label}: in use ({e})"),
            format!(
                "stop whatever is bound to {port}/{}, or set a different port in config",
                proto.as_str()
            ),
        ),
    }
}

/// GET {api}/health with a short timeout. A 200 means the engine is up; anything
/// else is reported as a warning (not fatal — doctor may run before server start).
fn check_rest_api(api: &str) -> CheckResult {
    let url = format!("{}/health", api.trim_end_matches('/'));
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_millis(500))
        .timeout_read(std::time::Duration::from_millis(500))
        .build();
    match agent.get(&url).call() {
        Ok(resp) if resp.status() == 200 => {
            CheckResult::pass("REST API", format!("reachable at {api}"))
        }
        Ok(resp) => CheckResult::warn(
            "REST API",
            format!("{api} returned HTTP {}", resp.status()),
            "check that the engine is healthy — `aeon serve` logs",
        ),
        Err(e) => CheckResult::warn(
            "REST API",
            format!("not reachable at {api}: {e}"),
            "start the engine with `aeon serve`, or pass --api <url>",
        ),
    }
}

/// Pure TCP probe — no protocol handshake. Good enough to distinguish "broker down"
/// from "broker up"; does not validate that it's actually Kafka/Redpanda.
fn check_tcp_reachable(label: &str, addr: &str) -> CheckResult {
    use std::net::{TcpStream, ToSocketAddrs};
    let timeout = std::time::Duration::from_millis(500);
    match addr.to_socket_addrs() {
        Ok(mut iter) => match iter.next() {
            Some(sock) => match TcpStream::connect_timeout(&sock, timeout) {
                Ok(_) => CheckResult::pass(label, format!("{addr} reachable")),
                Err(e) => CheckResult::warn(
                    label,
                    format!("{addr} not reachable: {e}"),
                    "start your broker (e.g., `aeon dev up` for local Redpanda) or pass --kafka <addr>",
                ),
            },
            None => CheckResult::fail(
                label,
                format!("{addr} resolved to no addresses"),
                "verify the address/port is correct",
            ),
        },
        Err(e) => CheckResult::fail(
            label,
            format!("cannot resolve {addr}: {e}"),
            "check DNS or use an explicit IP:port",
        ),
    }
}

/// Ensure the state dir exists and is writable. Creates the directory if it doesn't
/// exist (mirrors the engine's own startup behavior); the write test is a touch-file.
fn check_state_dir(dir: &Path) -> CheckResult {
    if !dir.exists() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return CheckResult::fail(
                "state dir",
                format!("{} does not exist and could not be created: {e}", dir.display()),
                "create the directory manually or run with elevated permissions",
            );
        }
    }
    let probe = dir.join(".aeon-doctor-write-test");
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            CheckResult::pass("state dir", format!("{} writable", dir.display()))
        }
        Err(e) => CheckResult::fail(
            "state dir",
            format!("{} not writable: {e}", dir.display()),
            "fix directory permissions or choose a different --state-dir",
        ),
    }
}

/// Parse the manifest and extract any artifact paths it references, so we can
/// validate those files separately. The manifest schema used by `cmd_apply` keeps
/// `pipelines` as generic JSON, so we walk it looking for `processor_path` /
/// `artifact` / `path` string fields under pipeline definitions.
fn check_manifest(path: &Path) -> (CheckResult, Vec<PathBuf>) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            return (
                CheckResult::fail(
                    "manifest",
                    format!("{} unreadable: {e}", path.display()),
                    "check the file path and permissions",
                ),
                Vec::new(),
            );
        }
    };
    let manifest: Manifest = match serde_yaml::from_str(&content) {
        Ok(m) => m,
        Err(e) => {
            return (
                CheckResult::fail(
                    "manifest",
                    format!("{} invalid YAML: {e}", path.display()),
                    "run `aeon validate <file>` for detailed schema errors",
                ),
                Vec::new(),
            );
        }
    };

    let mut artifacts: Vec<PathBuf> = Vec::new();
    for pipeline in &manifest.pipelines {
        if let Ok(v) = serde_json::to_value(pipeline) {
            collect_artifact_paths(&v, &mut artifacts);
        }
    }

    let detail = format!(
        "{}: {} pipeline(s), {} identity(ies), {} artifact ref(s)",
        path.display(),
        manifest.pipelines.len(),
        manifest.identities.len(),
        artifacts.len(),
    );
    (CheckResult::pass("manifest", detail), artifacts)
}

/// Walk a serde_json::Value looking for string fields likely to be artifact paths.
/// Conservative: only `processor_path`, `artifact`, and `path` keys — false positives
/// would cause spurious "artifact missing" warnings which are worse than silence.
fn collect_artifact_paths(value: &serde_json::Value, out: &mut Vec<PathBuf>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                if matches!(k.as_str(), "processor_path" | "artifact" | "path")
                    && let serde_json::Value::String(s) = v
                {
                    out.push(PathBuf::from(s));
                }
                collect_artifact_paths(v, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_artifact_paths(v, out);
            }
        }
        _ => {}
    }
}

/// Verify an artifact file exists and has a recognised extension. Does NOT fully
/// load/validate it — that's `aeon validate`'s job. This check is fast and
/// dependency-free so `aeon doctor` stays responsive.
fn check_artifact(path: &Path) -> CheckResult {
    if !path.exists() {
        return CheckResult::fail(
            "artifact",
            format!("{} missing", path.display()),
            "build the artifact (`aeon build`) or correct the path in the manifest",
        );
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "wasm" | "so" | "dll" | "dylib" => CheckResult::pass(
            "artifact",
            format!("{} (.{ext}) present", path.display()),
        ),
        _ => CheckResult::warn(
            "artifact",
            format!("{} has unexpected extension '.{ext}'", path.display()),
            "expected .wasm, .so, .dll, or .dylib",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cluster_body() -> serde_json::Value {
        serde_json::json!({
            "mode": "cluster",
            "node_id": 2,
            "leader_id": 1,
            "num_partitions": 4,
            "partitions": {
                "0": { "status": "owned", "owner": 1 },
                "1": { "status": "owned", "owner": 2 },
                "2": { "status": "transferring", "source": 1, "target": 3 },
                "3": { "status": "owned", "owner": 2 },
            },
            "raft": {
                "state": "Follower",
                "current_term": 5,
                "last_applied": 127,
                "last_log_index": 127,
                "membership": "[1, 2, 3]"
            }
        })
    }

    #[test]
    fn render_cluster_body_includes_leader_and_partition_rows_in_id_order() {
        let rendered = render_cluster_status(&sample_cluster_body(), 2.0);

        assert!(rendered.contains("node: 2"), "header missing node id: {rendered}");
        assert!(rendered.contains("leader: 1"), "header missing leader: {rendered}");
        assert!(rendered.contains("term: 5"), "term missing: {rendered}");
        assert!(rendered.contains("last_applied: 127"), "last_applied missing");

        let p0 = rendered.find("P0").expect("P0 row");
        let p1 = rendered.find("P1").expect("P1 row");
        let p2 = rendered.find("P2").expect("P2 row");
        let p3 = rendered.find("P3").expect("P3 row");
        assert!(
            p0 < p1 && p1 < p2 && p2 < p3,
            "partition rows must render in ascending partition-id order: {rendered}"
        );

        assert!(
            rendered.contains("transfer  1 → 3"),
            "transfer row shape wrong: {rendered}"
        );
        assert!(rendered.contains("(every 2s, Ctrl-C to exit)"));
    }

    #[test]
    fn render_cluster_body_falls_back_on_standalone_mode() {
        let body = serde_json::json!({
            "mode": "standalone",
            "message": "no cluster node configured"
        });
        let rendered = render_cluster_status(&body, 1.5);
        assert!(rendered.contains("mode: standalone"));
        assert!(rendered.contains("no cluster node configured"));
        // No partition table when not in cluster mode.
        assert!(!rendered.contains("Partitions ("));
    }

    #[test]
    fn render_cluster_body_handles_missing_leader_gracefully() {
        let mut body = sample_cluster_body();
        body["leader_id"] = serde_json::Value::Null;
        let rendered = render_cluster_status(&body, 2.0);
        assert!(
            rendered.contains("leader: <none>"),
            "no-leader must render as <none>: {rendered}"
        );
    }

    #[test]
    fn render_cluster_body_sorts_partitions_by_numeric_id_not_lexicographic() {
        // Keys "10" and "2" would sort lexicographically as "10" < "2".
        // Confirm we parse and sort numerically.
        let body = serde_json::json!({
            "mode": "cluster",
            "node_id": 1,
            "leader_id": 1,
            "num_partitions": 12,
            "partitions": {
                "2":  { "status": "owned", "owner": 1 },
                "10": { "status": "owned", "owner": 1 },
            },
            "raft": {
                "state": "Leader",
                "current_term": 1,
                "last_applied": 0,
                "last_log_index": 0,
                "membership": "[1]"
            }
        });
        let rendered = render_cluster_status(&body, 2.0);
        let p2 = rendered.find("P2 ").expect("P2");
        let p10 = rendered.find("P10").expect("P10");
        assert!(p2 < p10, "numeric sort: P2 must come before P10: {rendered}");
    }

    #[test]
    fn chrono_like_hms_returns_eight_char_utc_timestamp() {
        let ts = chrono_like_hms();
        assert_eq!(ts.len(), 9, "HH:MM:SSZ expected, got {ts:?}");
        assert!(ts.ends_with('Z'));
        let parts: Vec<&str> = ts.trim_end_matches('Z').split(':').collect();
        assert_eq!(parts.len(), 3);
        for p in &parts {
            assert_eq!(p.len(), 2);
            let n: u32 = p.parse().expect("numeric");
            assert!(n < 60);
        }
    }
}
