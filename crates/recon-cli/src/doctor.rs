//! `recon doctor` — health-check command.
//!
//! Runs a battery of probes covering the surfaces a stuck install can
//! fail on: license cache, credentials file, worker reachability, index
//! state, MCP wiring, agent rules, filesystem permissions. Each probe
//! produces an `OK` / `WARN` / `FAIL` outcome plus a one-line detail.
//!
//! Designed to short-circuit support emails. The structured `--json`
//! output is the same shape so monitoring scripts can parse it.
//!
//! Constraint: must NOT load `ReconServer` to inspect index state.
//! Loading the server takes seconds, opens write handles, bumps file
//! mtimes — none of which a passive health check should do. Index
//! probing goes through a read-only SQLite open instead.

use serde::Serialize;
use std::path::{Path, PathBuf};

/// One probe result.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    /// Human-readable name shown to the user.
    pub name: &'static str,
    /// Outcome severity.
    pub status: Status,
    /// One-line explanation. Pass-through into rendering — keep it short.
    pub detail: String,
}

/// Outcome severity for a single probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Healthy.
    Ok,
    /// Working but unusual — surfaced for awareness, not a failure.
    Warn,
    /// Broken; doctor exits 1.
    Fail,
}

/// JSON output envelope for `--json` mode.
#[derive(Debug, Serialize)]
struct DoctorReport {
    version: &'static str,
    target: &'static str,
    repo: String,
    checks: Vec<Check>,
    summary: Summary,
}

#[derive(Debug, Serialize)]
struct Summary {
    ok: usize,
    warn: usize,
    fail: usize,
}

/// Run all probes and render a report.
pub fn run(repo: &Path, json: bool) -> anyhow::Result<()> {
    let canon = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let config_dir = recon_server::license::global_config_dir();

    let mut checks: Vec<Check> = Vec::with_capacity(12);
    checks.push(check_binary());
    checks.push(check_repo_dir(&canon));
    checks.push(check_global_config_dir(&config_dir));
    checks.push(check_license_cache(&config_dir));
    checks.push(check_credentials_file(&config_dir));
    checks.extend(check_worker(&config_dir));
    checks.push(check_index(&canon));
    checks.extend(check_mcp_wiring(&canon));
    checks.extend(check_agent_rules(&canon));

    let summary = Summary {
        ok: checks.iter().filter(|c| c.status == Status::Ok).count(),
        warn: checks.iter().filter(|c| c.status == Status::Warn).count(),
        fail: checks.iter().filter(|c| c.status == Status::Fail).count(),
    };

    if json {
        let report = DoctorReport {
            version: env!("CARGO_PKG_VERSION"),
            target: target_triple(),
            repo: canon.display().to_string(),
            checks,
            summary,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        if report.summary.fail > 0 {
            std::process::exit(1);
        }
        return Ok(());
    }

    println!(
        "recon doctor — {} ({})",
        env!("CARGO_PKG_VERSION"),
        target_triple()
    );
    println!("repo: {}", canon.display());
    println!();
    for c in &checks {
        let tag = match c.status {
            Status::Ok => "[OK]  ",
            Status::Warn => "[WARN]",
            Status::Fail => "[FAIL]",
        };
        println!("{tag}  {:<28}  {}", c.name, c.detail);
    }
    println!();
    println!(
        "Summary: {} ok, {} warn, {} fail",
        summary.ok, summary.warn, summary.fail
    );
    if summary.fail > 0 {
        std::process::exit(1);
    }
    Ok(())
}

// ── Probes ────────────────────────────────────────────────────────────────────

fn check_binary() -> Check {
    Check {
        name: "binary",
        status: Status::Ok,
        detail: format!("recon {} ({})", env!("CARGO_PKG_VERSION"), target_triple()),
    }
}

fn check_repo_dir(canon: &Path) -> Check {
    if !canon.exists() {
        return Check {
            name: "repo dir",
            status: Status::Fail,
            detail: format!("{} does not exist", canon.display()),
        };
    }
    if !canon.is_dir() {
        return Check {
            name: "repo dir",
            status: Status::Fail,
            detail: format!("{} is not a directory", canon.display()),
        };
    }
    // Probe writability via tempfile in `.recon/` (creating if missing).
    let recon_dir = canon.join(".recon");
    if !recon_dir.exists() {
        match std::fs::create_dir_all(&recon_dir) {
            Ok(()) => {}
            Err(e) => {
                return Check {
                    name: "repo dir",
                    status: Status::Fail,
                    detail: format!("cannot create .recon/: {e}"),
                };
            }
        }
    }
    let probe = recon_dir.join(".doctor-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Check {
                name: "repo dir",
                status: Status::Ok,
                detail: format!("{} writable", canon.display()),
            }
        }
        Err(e) => Check {
            name: "repo dir",
            status: Status::Fail,
            detail: format!("cannot write to .recon/: {e}"),
        },
    }
}

fn check_global_config_dir(dir: &Path) -> Check {
    if !dir.exists() {
        return Check {
            name: "global config",
            status: Status::Warn,
            detail: format!("{} does not exist (run `recon login`)", dir.display()),
        };
    }
    let probe = dir.join(".doctor-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Check {
                name: "global config",
                status: Status::Ok,
                detail: format!("{} writable", dir.display()),
            }
        }
        Err(e) => Check {
            name: "global config",
            status: Status::Fail,
            detail: format!("cannot write to {}: {e}", dir.display()),
        },
    }
}

fn check_license_cache(config_dir: &Path) -> Check {
    let path = config_dir.join("license.json");
    if !path.exists() {
        return Check {
            name: "license",
            status: Status::Fail,
            detail: "no cached license — run `recon login <key>`".into(),
        };
    }
    // Use the public validate path so signature + expiry are checked
    // exactly the way the rest of the CLI checks them.
    match recon_server::license::validate_license(None, config_dir) {
        Ok(lic) => {
            let exp = if lic.expires_at == 0 {
                "no expiry".to_string()
            } else {
                format!("expires unix={}", lic.expires_at)
            };
            Check {
                name: "license",
                status: Status::Ok,
                detail: format!("{} ({exp})", lic.tier.name()),
            }
        }
        Err(e) => Check {
            name: "license",
            status: Status::Fail,
            detail: format!("validation failed: {e}"),
        },
    }
}

fn check_credentials_file(config_dir: &Path) -> Check {
    let path = recon_server::license::credentials_path(config_dir);
    if !path.exists() {
        return Check {
            name: "credentials",
            status: Status::Warn,
            detail: format!(
                "{} not present — `recon repos` and `recon init` need it",
                path.display()
            ),
        };
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode != 0o600 {
                return Check {
                    name: "credentials",
                    status: Status::Warn,
                    detail: format!(
                        "mode is 0{mode:o}, expected 0600 (chmod 600 {})",
                        path.display()
                    ),
                };
            }
        }
    }
    Check {
        name: "credentials",
        status: Status::Ok,
        detail: "present, mode 0600".into(),
    }
}

fn check_worker(config_dir: &Path) -> Vec<Check> {
    let mut out = Vec::with_capacity(2);
    match recon_server::account::ping_health() {
        Ok(()) => out.push(Check {
            name: "worker /v1/health",
            status: Status::Ok,
            detail: "reachable".into(),
        }),
        Err(e) => {
            out.push(Check {
                name: "worker /v1/health",
                status: Status::Fail,
                detail: format!("{e}"),
            });
            // No point trying the authenticated calls if /health fails.
            return out;
        }
    }

    let api_key = match recon_server::license::read_credentials(config_dir) {
        Some(k) => k,
        None => {
            out.push(Check {
                name: "worker repo list",
                status: Status::Warn,
                detail: "skipped (no credentials)".into(),
            });
            return out;
        }
    };
    match recon_server::account::list_repos(&api_key) {
        Ok(resp) => out.push(Check {
            name: "worker repo list",
            status: Status::Ok,
            detail: format!("{}/{} on {}", resp.repos.len(), resp.limit, resp.tier),
        }),
        Err(e) => out.push(Check {
            name: "worker repo list",
            status: Status::Fail,
            detail: format!("{e}"),
        }),
    }
    out
}

fn check_index(repo: &Path) -> Check {
    let db = repo.join(".recon").join("index.db");
    if !db.exists() {
        return Check {
            name: "index",
            status: Status::Warn,
            detail: format!("{} not present (run `recon init`)", db.display()),
        };
    }
    // Read-only open. We don't go through Store::open because that runs
    // migrations / sets PRAGMAs / takes a write lock — none of which a
    // doctor probe should do.
    let conn = match rusqlite::Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(e) => {
            return Check {
                name: "index",
                status: Status::Fail,
                detail: format!("cannot open {}: {e}", db.display()),
            };
        }
    };
    let symbols: i64 = conn
        .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))
        .unwrap_or(-1);
    let files: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap_or(-1);
    if symbols < 0 || files < 0 {
        return Check {
            name: "index",
            status: Status::Fail,
            detail: "schema unreadable — try `recon reindex`".into(),
        };
    }
    Check {
        name: "index",
        status: Status::Ok,
        detail: format!("{files} files, {symbols} symbols"),
    }
}

// MCP wiring + agent rules: probe every IDE we know about. Most repos
// only have one wired, so warns turn into "[WARN] cursor mcp: not present"
// which is informative without being a failure signal.

fn check_mcp_wiring(repo: &Path) -> Vec<Check> {
    [
        ("cc mcp", repo.join(".mcp.json"), "mcpServers"),
        ("oc mcp", repo.join("opencode.jsonc"), "mcp"),
        (
            "cursor mcp",
            repo.join(".cursor").join("mcp.json"),
            "mcpServers",
        ),
    ]
    .into_iter()
    .map(|(name, path, key)| check_one_mcp(name, &path, key))
    .collect()
}

fn check_one_mcp(name: &'static str, path: &Path, servers_key: &str) -> Check {
    if !path.exists() {
        return Check {
            name,
            status: Status::Warn,
            detail: format!("{} not present", path.display()),
        };
    }
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            return Check {
                name,
                status: Status::Fail,
                detail: format!("cannot read {}: {e}", path.display()),
            };
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            return Check {
                name,
                status: Status::Fail,
                detail: format!("malformed JSON: {e}"),
            };
        }
    };
    match value
        .get(servers_key)
        .and_then(|s| s.get("recon"))
        .and_then(|s| s.get("command").or_else(|| s.get("args")))
    {
        Some(_) => Check {
            name,
            status: Status::Ok,
            detail: format!("{} → recon entry present", path.display()),
        },
        None => Check {
            name,
            status: Status::Warn,
            detail: "config exists but recon entry missing".into(),
        },
    }
}

fn check_agent_rules(repo: &Path) -> Vec<Check> {
    let targets: [(&'static str, PathBuf, bool); 4] = [
        ("CLAUDE.md rules", repo.join("CLAUDE.md"), true),
        ("AGENTS.md rules", repo.join("AGENTS.md"), true),
        (
            "cursor rules",
            repo.join(".cursor").join("rules").join("recon.mdc"),
            false,
        ),
        (
            "windsurf rules",
            repo.join(".windsurf").join("rules").join("recon.md"),
            false,
        ),
    ];
    targets
        .into_iter()
        .map(|(name, path, shared)| check_one_agent_rule(name, &path, shared))
        .collect()
}

fn check_one_agent_rule(name: &'static str, path: &Path, shared: bool) -> Check {
    if !path.exists() {
        return Check {
            name,
            status: Status::Warn,
            detail: format!("{} not present", path.display()),
        };
    }
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            return Check {
                name,
                status: Status::Fail,
                detail: format!("cannot read {}: {e}", path.display()),
            };
        }
    };
    let marker = if shared {
        "<!-- recon:start -->"
    } else {
        "strict policy"
    };
    if content.contains(marker) {
        Check {
            name,
            status: Status::Ok,
            detail: format!("{} → recon block present", path.display()),
        }
    } else {
        Check {
            name,
            status: Status::Warn,
            detail: format!("{} exists but no recon block", path.display()),
        }
    }
}

/// Best-effort target triple. `cargo` doesn't expose the build triple as
/// an env var by default; we read it from a build.rs-injected env if
/// present, otherwise return `"unknown"`.
fn target_triple() -> &'static str {
    option_env!("TARGET").unwrap_or("unknown")
}
