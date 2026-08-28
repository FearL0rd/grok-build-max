#!/bin/sh
# Grok Build Max installer (macOS / Linux / Git Bash).
#
# Builds from source and installs the `grokmax` executable onto your PATH:
#   curl -fsSL https://raw.githubusercontent.com/FearL0rd/grok-build-max/main/install.sh | bash
#
# Layout:
#   ~/.grokmax/src     git clone (created/updated)
#   grokmax binary     /usr/local/bin if writable, else ~/.local/bin
#
# Env overrides: REPO_URL, REF, GROKMAX_HOME

set -eu

REPO_URL="${REPO_URL:-https://github.com/FearL0rd/grok-build-max.git}"
REF="${REF:-main}"
GROKMAX_HOME="${GROKMAX_HOME:-$HOME/.grokmax}"
SRC_DIR="$GROKMAX_HOME/src"

say()  { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

# --- prerequisites -----------------------------------------------------------

command -v git >/dev/null 2>&1 || die "git not found — install git and re-run"

if ! command -v cargo >/dev/null 2>&1; then
  say "Rust not found — installing rustup (non-interactive)..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
  export PATH="$HOME/.cargo/bin:$PATH"
  command -v cargo >/dev/null 2>&1 || die "rustup ran but cargo is still not on PATH — open a new shell and re-run"
fi

# The build needs a protoc. bin/protoc resolves via DotSlash, else a protoc on
# PATH / $PROTOC.
if ! command -v protoc >/dev/null 2>&1 && [ -z "${PROTOC:-}" ]; then
  if ! command -v dotslash >/dev/null 2>&1; then
    say "protoc/DotSlash not found — installing DotSlash via cargo (this can take a minute)..."
    cargo install dotslash || die "could not install DotSlash; install protoc (apt/brew) or DotSlash and re-run"
  fi
fi

# --- fetch -------------------------------------------------------------------

mkdir -p "$GROKMAX_HOME"
if [ -d "$SRC_DIR/.git" ]; then
  say "Updating existing clone at $SRC_DIR..."
  git -C "$SRC_DIR" fetch --depth 1 origin "$REF"
  git -C "$SRC_DIR" checkout -q FETCH_HEAD
else
  say "Cloning $REPO_URL ($REF) to $SRC_DIR..."
  git clone --depth 1 --branch "$REF" "$REPO_URL" "$SRC_DIR" \
    || git clone --depth 1 "$REPO_URL" "$SRC_DIR"
fi

# --- build -------------------------------------------------------------------

say "Building Grok Build Max (release) — first build can take several minutes..."
if ! ( cd "$SRC_DIR" && cargo build --release -p xai-grok-pager-bin ); then
  warn "build failed"
  warn "if the log mentions protoc, install a system protoc and re-run:"
  warn "  apt install protobuf-compiler | brew install protobuf | dnf install protobuf-compiler"
  exit 1
fi

BUILT="$SRC_DIR/target/release/grokmax"
[ -f "$BUILT" ] || die "build finished but binary missing at $BUILT"

# --- install -----------------------------------------------------------------

BIN_DIR=""
if [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then
  BIN_DIR="/usr/local/bin"
else
  BIN_DIR="$HOME/.local/bin"
  mkdir -p "$BIN_DIR"
fi

install -m 0755 "$BUILT" "$BIN_DIR/grokmax"

# Ensure BIN_DIR is on PATH for future shells when we installed to ~/.local/bin.
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    for profile in "$HOME/.profile" "$HOME/.bashrc" "$HOME/.zshrc"; do
      if [ -f "$profile" ] && ! grep -qs "$BIN_DIR" "$profile"; then
        printf '\nexport PATH="%s:$PATH"\n' "$BIN_DIR" >> "$profile"
        warn "added $BIN_DIR to PATH in $profile — restart your shell or: export PATH=\"$BIN_DIR:\$PATH\""
      fi
    done
    ;;
esac

say ""
say "Grok Build Max installed: $BIN_DIR/grokmax"
"$BIN_DIR/grokmax" --version || true
say "Run 'grokmax' to start. Type /providers inside the TUI to configure providers and failover order."
