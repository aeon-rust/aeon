use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::process::Command;

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
    /// Start/stop the local development environment (Redpanda)
    Dev {
        #[command(subcommand)]
        action: DevAction,
    },
}

#[derive(Subcommand)]
enum DevAction {
    /// Start the development environment (Redpanda + Console)
    Up,
    /// Stop the development environment
    Down,
    /// Show development environment status
    Status,
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
        Some(Commands::Dev { action }) => cmd_dev(&action),
        None => {
            println!("Aeon v{}", env!("CARGO_PKG_VERSION"));
            println!("Run `aeon --help` for available commands.");
            Ok(())
        }
    }
}

// ── aeon new ───────────────────────────────────────────────────────────

fn cmd_new(name: &str, runtime: &Runtime, lang: &Lang) -> Result<()> {
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

fn process(event: Event) -> Vec<WasmOutput> {{
    // Passthrough: forward payload to "output" topic
    vec![WasmOutput::new("output", event.payload.clone())]
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
        Ok(vec![Output {{
            destination: Arc::from("output"),
            key: None,
            payload: event.payload.clone(),
            headers: Default::default(),
            source_ts: event.source_ts,
        }}])
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
