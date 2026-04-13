//! Aeon workspace developer workflows (DX-5).
//!
//! Usage:
//!   cargo xtask ci       # fmt check + clippy (deny warnings) + workspace tests
//!   cargo xtask doctor   # Rust toolchain / required targets / env check
//!   cargo xtask bench    # workspace benches (criterion) with a readable summary
//!
//! Design principles:
//!   * Zero external dependencies — keep builds fast and the xtask itself
//!     trivially auditable.
//!   * Non-destructive only. We never tag, push, publish, or mutate versions.
//!     If you want release automation, run the command we suggest.
//!   * Commands just shell out to `cargo` / `rustup` — they exist to remove
//!     the "remembered incantations" barrier, not to hide what `cargo` does.

use std::env;
use std::process::{Command, ExitCode};

const HELP: &str = "\
aeon xtask — developer workflow runner

USAGE:
    cargo xtask <COMMAND>

COMMANDS:
    ci         Format check, clippy (-D warnings), full workspace test
    doctor     Report Rust toolchain state and required build targets
    bench      Run workspace benchmarks (cargo bench --workspace)
    help       Show this message

All commands exit non-zero on the first failed step.";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");

    let result = match cmd {
        "ci" => task_ci(),
        "doctor" => task_doctor(),
        "bench" => task_bench(),
        "help" | "--help" | "-h" => {
            println!("{HELP}");
            Ok(())
        }
        other => {
            eprintln!("xtask: unknown command '{other}'\n\n{HELP}");
            return ExitCode::from(2);
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("\nxtask: {msg}");
            ExitCode::FAILURE
        }
    }
}

// ── commands ────────────────────────────────────────────────────────────

fn task_ci() -> Result<(), String> {
    section("fmt check");
    run("cargo", &["fmt", "--all", "--", "--check"])?;

    section("clippy (workspace, all targets, -D warnings)");
    run(
        "cargo",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    )?;

    section("test --workspace");
    run("cargo", &["test", "--workspace", "--no-fail-fast"])?;

    println!("\nci: all checks passed.");
    Ok(())
}

fn task_doctor() -> Result<(), String> {
    section("rustc");
    run_best_effort("rustc", &["--version"]);

    section("cargo");
    run_best_effort("cargo", &["--version"]);

    section("installed toolchains");
    run_best_effort("rustup", &["toolchain", "list"]);

    section("installed targets");
    run_best_effort("rustup", &["target", "list", "--installed"]);

    // Required build targets for Aeon workflows.
    let required_targets = ["wasm32-unknown-unknown", "wasm32-wasip1"];
    section("required targets for Aeon");
    let installed = list_installed_targets();
    let mut missing: Vec<&str> = Vec::new();
    for t in required_targets {
        if installed.iter().any(|l| l == t) {
            println!("  [OK]      {t}");
        } else {
            println!("  [MISSING] {t}");
            missing.push(t);
        }
    }
    if !missing.is_empty() {
        println!("\nfix: rustup target add {}", missing.join(" "));
    }

    // Optional tools (warn only).
    section("optional tools");
    report_tool("cargo-deny", &["--version"]);
    report_tool("cargo-audit", &["--version"]);
    report_tool("helm", &["version", "--short"]);
    report_tool("kubectl", &["version", "--client", "--short"]);

    Ok(())
}

fn task_bench() -> Result<(), String> {
    section("cargo bench --workspace");
    run("cargo", &["bench", "--workspace"])
}

// ── helpers ─────────────────────────────────────────────────────────────

fn section(title: &str) {
    println!("\n=== {title} ===");
}

/// Run a command, failing if it exits non-zero or can't be spawned.
fn run(bin: &str, args: &[&str]) -> Result<(), String> {
    println!("$ {bin} {}", args.join(" "));
    let status = Command::new(bin)
        .args(args)
        .status()
        .map_err(|e| format!("failed to spawn `{bin}`: {e}"))?;
    if !status.success() {
        return Err(format!(
            "`{bin} {}` exited with {}",
            args.join(" "),
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into())
        ));
    }
    Ok(())
}

/// Run a command for informational purposes; never fails the whole task.
fn run_best_effort(bin: &str, args: &[&str]) {
    println!("$ {bin} {}", args.join(" "));
    match Command::new(bin).args(args).status() {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("  (exited with {s})"),
        Err(e) => eprintln!("  (not available: {e})"),
    }
}

fn list_installed_targets() -> Vec<String> {
    let output = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

fn report_tool(bin: &str, args: &[&str]) {
    match Command::new(bin).args(args).output() {
        Ok(o) if o.status.success() => {
            let first = String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            println!("  [OK]      {bin:<14} {first}");
        }
        _ => println!("  [missing] {bin}"),
    }
}
