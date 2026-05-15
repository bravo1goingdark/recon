#!/bin/bash
# PGO (Profile-Guided Optimization) build for recon.
#
# Usage: ./scripts/pgo-build.sh [TARGET]
#   TARGET defaults to the host triple.
#
# Requirements:
#   - rustup component add llvm-tools-preview
#   - cargo-pgo OR llvm-profdata on PATH
#
# This script:
#   1. Builds an instrumented binary
#   2. Runs a representative workload to collect profiles
#   3. Merges profiles
#   4. Rebuilds with PGO applied
#
# Expected gains: 5–15% faster queries, 10–20% faster cold index.
# Only works for native targets (can't PGO cross-compiled binaries).

set -euo pipefail

TARGET="${1:-$(rustc -vV | grep host | awk '{print $2}')}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
PGO_DIR="$REPO/target/pgo-profiles"
LLVM_PROFDATA="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | grep host | awk '{print $2}')/bin/llvm-profdata"

# Fallback to PATH llvm-profdata if rustc's bundled one isn't found
if [ ! -x "$LLVM_PROFDATA" ]; then
  LLVM_PROFDATA="llvm-profdata"
fi

echo "══════════════════════════════════════════════"
echo "  PGO build for target: $TARGET"
echo "══════════════════════════════════════════════"

# ── Step 1: Instrumented build ────────────────────────────────────────────────
echo ""
echo "▶ Step 1/4: Building instrumented binary..."
rm -rf "$PGO_DIR"
mkdir -p "$PGO_DIR"

RUSTFLAGS="-Cprofile-generate=$PGO_DIR" \
  cargo build --release --target "$TARGET" -p recon-cli

RECON="$REPO/target/$TARGET/release/recon"
if [ ! -x "$RECON" ]; then
  # Windows
  RECON="$REPO/target/$TARGET/release/recon.exe"
fi

# ── Step 2: Collect profiles ──────────────────────────────────────────────────
echo ""
echo "▶ Step 2/4: Running workload to collect profiles..."

# Create a temp project to index (avoids needing a license for the real repo)
WORKLOAD_DIR=$(mktemp -d)
trap "rm -rf $WORKLOAD_DIR" EXIT

# Generate a realistic source tree
mkdir -p "$WORKLOAD_DIR/src"
for i in $(seq 1 50); do
  cat > "$WORKLOAD_DIR/src/mod_$i.rs" << 'RUST'
pub struct Handler {
    name: String,
    count: u64,
}

impl Handler {
    pub fn new(name: &str) -> Self {
        Self { name: name.to_string(), count: 0 }
    }

    pub fn process(&mut self, input: &str) -> String {
        self.count += 1;
        format!("{}: processed {} (call #{})", self.name, input, self.count)
    }
}

pub fn create_handler(name: &str) -> Handler {
    Handler::new(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_handler() {
        let mut h = create_handler("test");
        assert_eq!(h.process("x"), "test: processed x (call #1)");
    }
}
RUST
done

cat > "$WORKLOAD_DIR/src/main.rs" << 'RUST'
mod mod_1;
use mod_1::Handler;
fn main() {
    let mut h = Handler::new("main");
    println!("{}", h.process("hello"));
}
RUST

# Run the workload (these commands work without a license for local-only ops)
cd "$WORKLOAD_DIR"
"$RECON" index 2>/dev/null || true
"$RECON" find Handler 2>/dev/null || true
"$RECON" find process 2>/dev/null || true
"$RECON" search "TODO" 2>/dev/null || true
"$RECON" search "fn.*new" --mode regex 2>/dev/null || true
"$RECON" map --budget 2000 2>/dev/null || true
"$RECON" outline src/mod_1.rs 2>/dev/null || true
"$RECON" skeleton src/main.rs 2>/dev/null || true
"$RECON" refs Handler 2>/dev/null || true
"$RECON" ls --lang rust 2>/dev/null || true
"$RECON" stats 2>/dev/null || true
"$RECON" strings "processed" 2>/dev/null || true
"$RECON" multi Handler process create_handler 2>/dev/null || true

# Run index a few more times to weight the hot path
for _ in 1 2 3; do
  "$RECON" reindex 2>/dev/null || true
done

cd "$REPO"

# ── Step 3: Merge profiles ────────────────────────────────────────────────────
echo ""
echo "▶ Step 3/4: Merging profiles..."

PROFILE_COUNT=$(find "$PGO_DIR" -name "*.profraw" | wc -l)
echo "  Found $PROFILE_COUNT profile files"

"$LLVM_PROFDATA" merge -o "$PGO_DIR/merged.profdata" "$PGO_DIR"

# ── Step 4: Optimized build ───────────────────────────────────────────────────
echo ""
echo "▶ Step 4/4: Building PGO-optimized binary..."

RUSTFLAGS="-Cprofile-use=$PGO_DIR/merged.profdata" \
  cargo build --release --target "$TARGET" -p recon-cli

echo ""
echo "══════════════════════════════════════════════"
echo "  ✓ PGO build complete"
echo "  Binary: target/$TARGET/release/recon"
echo "  Size: $(du -h "target/$TARGET/release/recon" 2>/dev/null | cut -f1 || echo 'N/A')"
echo "══════════════════════════════════════════════"
