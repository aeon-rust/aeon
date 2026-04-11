use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::process::Command;

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

fn main() -> Result<()> {
    let cli = Cli::parse();

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
        #[cfg(feature = "rest-api")]
        Some(Commands::Serve { addr, artifact_dir }) => cmd_serve(&addr, &artifact_dir),
        None => {
            println!("Aeon v{}", env!("CARGO_PKG_VERSION"));
            println!("Run `aeon --help` for available commands.");
            Ok(())
        }
    }
}

// ── Server ────────────────────────────────────────────────────────────

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
        let registry = aeon_engine::registry::ProcessorRegistry::new(&dir)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let pipelines = aeon_engine::pipeline_manager::PipelineManager::new();
        let identities = aeon_engine::identity_store::ProcessorIdentityStore::new();

        let state = Arc::new(aeon_engine::AppState {
            registry: Arc::new(registry),
            pipelines: Arc::new(pipelines),
            delivery_ledgers: dashmap::DashMap::new(),
            pipeline_controls: dashmap::DashMap::new(),
            identities: Arc::new(identities),
            authenticator: None,
            ws_host: None,
        });

        let listen_addr =
            std::env::var("AEON_API_ADDR").unwrap_or_else(|_| addr.to_string());
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
    pipelines: Vec<serde_json::Value>,
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

    for pipeline_def in &manifest.pipelines {
        let name = pipeline_def["name"].as_str().unwrap_or("<unnamed>");
        let exists = existing_names.contains(&name.to_string());

        if dry_run {
            if exists {
                println!("[dry-run] pipeline '{name}' — already exists (skip)");
            } else {
                println!("[dry-run] pipeline '{name}' — would create");
            }
        } else if exists {
            println!("pipeline '{name}' — already exists (skip)");
        } else {
            ureq::post(&format!("{api}/api/v1/pipelines"))
                .send_json(pipeline_def)
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

    let manifest = Manifest {
        pipelines: full_pipelines,
        identities,
    };
    let yaml = serde_yaml::to_string(&manifest).context("failed to serialize manifest")?;

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
        .filter_map(|p| p["name"].as_str().map(String::from))
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
