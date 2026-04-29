//! xtask — workspace utility commands.
//!
//! Subcommands:
//! - `measured-baselines-corpus --repo <path>`: calibration harness for
//!   the v0.4 measured-savings rollout. Runs the migrated bucket-1 tools
//!   against a real repo, prints a TSV of (static_estimate, measured),
//!   and asserts the divergence is within the 15% gate per the
//!   measured-savings plan.

use std::path::PathBuf;

mod measured_baselines;

fn main() -> std::process::ExitCode {
    let mut args = std::env::args().skip(1);
    let cmd = args.next();
    match cmd.as_deref() {
        Some("measured-baselines-corpus") => {
            let mut repo: Option<PathBuf> = None;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--repo" => {
                        repo = args.next().map(PathBuf::from);
                    }
                    other => {
                        eprintln!("unknown arg: {other}");
                        return std::process::ExitCode::from(2);
                    }
                }
            }
            let Some(repo) = repo else {
                eprintln!("usage: cargo xtask measured-baselines-corpus --repo <path>");
                return std::process::ExitCode::from(2);
            };
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            match rt.block_on(measured_baselines::run(&repo)) {
                Ok(true) => std::process::ExitCode::SUCCESS,
                Ok(false) => std::process::ExitCode::from(1),
                Err(e) => {
                    eprintln!("calibration failed: {e}");
                    std::process::ExitCode::from(1)
                }
            }
        }
        _ => {
            eprintln!("Usage: cargo xtask <command>");
            eprintln!("Commands:");
            eprintln!("  measured-baselines-corpus --repo <path>");
            std::process::ExitCode::from(2)
        }
    }
}
