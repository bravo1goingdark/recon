#!/usr/bin/env bash
# recon installer — Linux / macOS
#
# What this script does, in order:
#   1. Detects OS + architecture, computes the target triple.
#   2. Resolves the version to install (env VERSION or latest.json on CDN).
#   3. Downloads the tarball AND the signed SHA256SUMS manifest.
#   4. Verifies the tarball's SHA256 against the manifest.
#   5. If cosign is present, also verifies the manifest's sigstore signature
#      — this proves the artifact was produced by our GitHub Actions workflow.
#   6. Extracts to $INSTALL_DIR.
#
# The verification steps are the point: `curl | tar xz` is a supply-chain
# trap. Every download goes through sha256sum -c before it's extracted.
#
# Override knobs:
#   VERSION=v1.2.3   pin a specific version instead of latest
#   INSTALL_DIR=...  install location (default ~/.local/bin)
#   CDN=...          alternative CDN base (default https://mcprecon.pages.dev)
#   SKIP_COSIGN=1    skip sigstore signature verification (sha256 still enforced)

set -euo pipefail

CDN="${CDN:-https://mcprecon.pages.dev}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

# Clean up any temp state on any exit path.
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

# ── Platform detection ────────────────────────────────────────────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)  os="unknown-linux-gnu" ;;
  Darwin) os="apple-darwin" ;;
  *)      echo "Unsupported OS: $OS" >&2; exit 1 ;;
esac

case "$ARCH" in
  x86_64)        arch="x86_64" ;;
  aarch64|arm64) arch="aarch64" ;;
  *)             echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

TARGET="${arch}-${os}"
ASSET="recon-${TARGET}.tar.gz"

# ── Version resolution ────────────────────────────────────────────────────────
# `--tlsv1.2 --proto =https` pins the transport to HTTPS + TLS 1.2+ so a
# corporate MITM proxy that downgrades cannot silently serve a modified
# installer. -f fails fast on 4xx/5xx, -S shows errors, -L follows redirects.
CURL=(curl --proto '=https' --tlsv1.2 -fSL --retry 3 --retry-delay 2)

if [ -z "${VERSION:-}" ]; then
  VERSION="$("${CURL[@]}" "$CDN/latest.json" | grep -o '"version":"[^"]*"' | cut -d'"' -f4)"
  if [ -z "$VERSION" ]; then
    echo "Could not determine latest version." >&2
    echo "Set VERSION=vX.Y.Z and re-run, or visit mcprecon.pages.dev." >&2
    exit 1
  fi
fi

URL_ASSET="$CDN/releases/$VERSION/$ASSET"
URL_SUMS="$CDN/releases/$VERSION/SHA256SUMS.txt"
URL_SUMS_SIG="$CDN/releases/$VERSION/SHA256SUMS.txt.sig"
URL_SUMS_PEM="$CDN/releases/$VERSION/SHA256SUMS.txt.pem"

echo "Installing recon $VERSION for $TARGET..."
echo "  From: $URL_ASSET"
echo "  To:   $INSTALL_DIR/recon"
echo ""

# ── Download artifacts ────────────────────────────────────────────────────────
"${CURL[@]}" -o "$TMPDIR/$ASSET"          "$URL_ASSET"
"${CURL[@]}" -o "$TMPDIR/SHA256SUMS.txt"  "$URL_SUMS"

# ── SHA256 verification (mandatory) ───────────────────────────────────────────
# sha256sum -c matches lines of the form "<hex>  <filename>". We filter
# the manifest to this asset so unrelated entries don't produce noise.
echo "Verifying SHA256..."
cd "$TMPDIR"
grep -E "[[:space:]]${ASSET}\$" SHA256SUMS.txt > "$TMPDIR/ASSET.sha256"
if [ ! -s "$TMPDIR/ASSET.sha256" ]; then
  echo "Manifest SHA256SUMS.txt does not list $ASSET — aborting." >&2
  exit 1
fi
if ! sha256sum -c "$TMPDIR/ASSET.sha256"; then
  echo "SHA256 verification FAILED for $ASSET — aborting." >&2
  exit 1
fi
cd - > /dev/null
echo "  ok: $ASSET matches published SHA256"

# ── cosign verification (optional but recommended) ────────────────────────────
# If cosign is installed, verify that SHA256SUMS.txt was signed by our
# release workflow's OIDC identity. This proves provenance: even if an
# attacker could replace both the tarball and the sums file, they would
# need a valid Fulcio-issued cert from our specific workflow ref.
if [ "${SKIP_COSIGN:-}" != "1" ] && command -v cosign >/dev/null 2>&1; then
  echo "Verifying cosign signature..."
  "${CURL[@]}" -o "$TMPDIR/SHA256SUMS.txt.sig" "$URL_SUMS_SIG" || true
  "${CURL[@]}" -o "$TMPDIR/SHA256SUMS.txt.pem" "$URL_SUMS_PEM" || true
  if [ -s "$TMPDIR/SHA256SUMS.txt.sig" ] && [ -s "$TMPDIR/SHA256SUMS.txt.pem" ]; then
    if cosign verify-blob \
        --certificate "$TMPDIR/SHA256SUMS.txt.pem" \
        --signature "$TMPDIR/SHA256SUMS.txt.sig" \
        --certificate-identity-regexp '^https://github\.com/bravo1goingdark/intel/\.github/workflows/release\.yml@.*' \
        --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
        "$TMPDIR/SHA256SUMS.txt" >/dev/null 2>&1; then
      echo "  ok: cosign signature valid (GitHub Actions OIDC)"
    else
      echo "  warn: cosign signature verification FAILED — installer will proceed with SHA256 only" >&2
    fi
  else
    echo "  info: signature artifacts not yet published for this version; SHA256 gate still enforced"
  fi
else
  echo "  info: cosign not installed — SHA256 gate enforced; install cosign for full provenance check"
fi

# ── Extract ───────────────────────────────────────────────────────────────────
mkdir -p "$INSTALL_DIR"
tar xz -C "$INSTALL_DIR" -f "$TMPDIR/$ASSET"
chmod +x "$INSTALL_DIR/recon"

echo ""
echo "Installed recon to $INSTALL_DIR/recon"
echo ""

# ── PATH advice ───────────────────────────────────────────────────────────────
if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
  echo "Add to your shell profile:"
  echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
  echo ""
fi

echo "Next steps:"
echo ""
echo "  recon login <your-api-key>     # get a key at mcprecon.pages.dev/login"
echo "  cd your-project"
echo "  recon init --mcp cc            # index + wire Claude Code"
echo "  recon init --mcp cursor        # or Cursor"
echo "  recon init --mcp windsurf      # or Windsurf"
echo "  recon init --mcp oc            # or OpenCode"
echo ""
