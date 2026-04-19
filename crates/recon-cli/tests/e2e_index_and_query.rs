//! E2E tests: index synthetic repos and verify incremental, merkle, and multi-lang behavior.

use std::path::Path;
use std::process::Command;

fn recon_binary() -> std::path::PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
    let debug = workspace_root.join("target/debug/recon");
    if debug.exists() {
        return debug;
    }
    let release = workspace_root.join("target/release/recon");
    if release.exists() {
        return release;
    }
    // Build it
    let status = Command::new("cargo")
        .args(["build", "--bin", "recon"])
        .current_dir(workspace_root)
        .status()
        .expect("cargo build failed");
    assert!(status.success());
    debug
}

fn init_git(dir: &Path) {
    for args in [
        &["init"][..],
        &["config", "user.email", "test@test.com"],
        &["config", "user.name", "Test"],
    ] {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
    }
}

fn git_commit(dir: &Path, msg: &str) {
    Command::new("git")
        .args(["add", "."])
        .current_dir(dir)
        .output()
        .unwrap();
    Command::new("git")
        .args(["commit", "-m", msg, "--allow-empty-message"])
        .current_dir(dir)
        .output()
        .unwrap();
}

fn run_index(dir: &Path) -> String {
    let output = Command::new(recon_binary())
        .args(["index", "--repo", dir.to_str().unwrap()])
        .output()
        .expect("failed to run recon index");
    assert!(
        output.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stderr).to_string()
}

#[test]
fn cold_index_creates_all_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/main.rs"),
        "/// Entry point.\npub fn main() { process(); }\n/// Worker.\npub fn process() {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/util.py"),
        "\"\"\"Utils.\"\"\"\ndef helper():\n    pass\n\nclass Config:\n    def load(self): pass\n",
    )
    .unwrap();

    init_git(root);
    git_commit(root, "init");

    let stderr = run_index(root);

    // Verify artifacts
    assert!(
        root.join(".recon/index.db").exists(),
        "SQLite index missing"
    );
    assert!(
        root.join(".recon/tantivy").exists(),
        "Tantivy index missing"
    );
    assert!(
        root.join(".recon/merkle_snapshot.json").exists(),
        "Merkle snapshot missing"
    );

    // Verify symbol count
    assert!(
        stderr.contains("symbols"),
        "should report symbols: {stderr}"
    );

    // Verify both languages indexed
    let stdout_text = std::fs::read_to_string(root.join(".recon/merkle_snapshot.json")).unwrap();
    assert!(stdout_text.contains("main.rs"), "should index Rust file");
    assert!(stdout_text.contains("util.py"), "should index Python file");
}

#[test]
fn head_unchanged_skips_entirely() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(root.join("lib.rs"), "pub fn a() {}").unwrap();
    init_git(root);
    git_commit(root, "init");

    // First index
    run_index(root);

    // Second index — HEAD unchanged
    let stderr = run_index(root);
    assert!(
        stderr.contains("HEAD matches") || stderr.contains("skipping"),
        "should skip on unchanged HEAD: {stderr}"
    );
    assert!(
        stderr.contains("Indexed 0 files"),
        "should index 0 files: {stderr}"
    );
}

#[test]
fn merkle_incremental_on_new_commit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(root.join("a.rs"), "pub fn original() {}").unwrap();
    init_git(root);
    git_commit(root, "init");

    // First full index
    run_index(root);

    // Add a new file and commit
    std::fs::write(root.join("b.rs"), "pub fn added() {}").unwrap();
    git_commit(root, "add b");

    // Second index — should detect 1 changed file via merkle diff
    let stderr = run_index(root);
    assert!(
        stderr.contains("merkle diff") || stderr.contains("incremental"),
        "should use merkle diff: {stderr}"
    );
}

#[test]
fn deleted_file_cascades() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(root.join("keep.rs"), "pub fn keep() {}").unwrap();
    std::fs::write(root.join("remove.rs"), "pub fn remove() {}").unwrap();
    init_git(root);
    git_commit(root, "init");

    run_index(root);

    // Delete a file and commit
    std::fs::remove_file(root.join("remove.rs")).unwrap();
    git_commit(root, "delete remove.rs");

    let stderr = run_index(root);
    assert!(
        stderr.contains("deleted") || stderr.contains("merkle diff"),
        "should detect deleted file: {stderr}"
    );
}

#[test]
fn multi_language_indexing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(root.join("main.rs"), "pub fn rust_fn() {}").unwrap();
    std::fs::write(root.join("app.py"), "def python_fn(): pass").unwrap();
    std::fs::write(root.join("index.ts"), "export function ts_fn() {}").unwrap();
    std::fs::write(root.join("main.go"), "package main\nfunc go_fn() {}").unwrap();
    std::fs::write(
        root.join("App.java"),
        "public class App { void java_method() {} }",
    )
    .unwrap();
    std::fs::write(root.join("util.c"), "void c_fn() {}").unwrap();

    init_git(root);
    git_commit(root, "init");

    let stderr = run_index(root);

    // Should report indexing completed with symbols
    assert!(
        stderr.contains("symbols") && stderr.contains("indexing complete"),
        "should complete indexing with symbols: {stderr}"
    );
}
