#!/bin/sh
# Grok Build Max installer (macOS / Linux / Git Bash).
#
# Builds from source and installs the `grokmax` executable onto your PATH:
#   curl -fsSL https://raw.githubusercontent.com/FearL0rd/grok-build-max/main/install.sh | bash
#
# Layout:
#   ~/.grokmax/src            git clone (created/updated)
#   ~/.grokmax/protoc         protoc downloaded by this installer (if needed)
#   grokmax binary            /usr/local/bin if writable, else ~/.local/bin
#
# Env overrides: REPO_URL, REF, GROKMAX_HOME

set -eu

REPO_URL="${REPO_URL:-https://github.com/FearL0rd/grok-build-max.git}"
REF="${REF:-main}"
GROKMAX_HOME="${GROKMAX_HOME:-$HOME/.grokmax}"
SRC_DIR="$GROKMAX_HOME/src"
PROTOC_VERSION="29.3"

say()  { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

# --- prerequisites -----------------------------------------------------------

command -v git >/dev/null 2>&1 || die "git not found — install git and re-run"
command -v curl >/dev/null 2>&1 || die "curl not found — install curl and re-run"

if ! command -v cargo >/dev/null 2>&1; then
  say "Rust not found — installing rustup (non-interactive)..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
  export PATH="$HOME/.cargo/bin:$PATH"
  command -v cargo >/dev/null 2>&1 || die "rustup ran but cargo is still not on PATH — open a new shell and re-run"
fi

# The build needs a protoc. bin/protoc in the repo is a DotSlash wrapper whose
# manifest lacks Windows and macOS-x86_64 entries, so we download a protoc
# release matching the platform instead. A protoc already on PATH or in
# $PROTOC wins.
if ! command -v protoc >/dev/null 2>&1 && [ -z "${PROTOC:-}" ]; then
  case "$(uname -s)/$(uname -m)" in
    Darwin/arm64)  PROTOC_ZIP="protoc-$PROTOC_VERSION-osx-aarch_64.zip" ;;
    Darwin/x86_64) PROTOC_ZIP="protoc-$PROTOC_VERSION-osx-x86_64.zip" ;;
    Linux/x86_64)  PROTOC_ZIP="protoc-$PROTOC_VERSION-linux-x86_64.zip" ;;
    Linux/aarch64) PROTOC_ZIP="protoc-$PROTOC_VERSION-linux-aarch_64.zip" ;;
    *) die "unsupported platform $(uname -s)/$(uname -m): install protoc manually and re-run" ;;
  esac
  PROTOC_BIN="$GROKMAX_HOME/protoc/bin/protoc"
  if [ ! -x "$PROTOC_BIN" ]; then
    say "protoc not found — downloading protoc $PROTOC_VERSION for $(uname -s)/$(uname -m)..."
    ZIP_URL="https://github.com/protocolbuffers/protobuf/releases/download/v$PROTOC_VERSION/$PROTOC_ZIP"
    ZIP_FILE="$GROKMAX_HOME/$PROTOC_ZIP"
    mkdir -p "$GROKMAX_HOME"
    curl -fsSL -o "$ZIP_FILE" "$ZIP_URL" \
      || die "protoc download failed — or install protoc via your package manager (apt/brew/dnf) and re-run"
    rm -rf "$GROKMAX_HOME/protoc"
    if command -v unzip >/dev/null 2>&1; then
      unzip -oq "$ZIP_FILE" -d "$GROKMAX_HOME/protoc" || die "could not extract $PROTOC_ZIP"
    elif command -v python3 >/dev/null 2>&1; then
      python3 -c "import zipfile,sys; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])" \
        "$ZIP_FILE" "$GROKMAX_HOME/protoc" || die "could not extract $PROTOC_ZIP"
    else
      die "need unzip or python3 to extract protoc"
    fi
    rm -f "$ZIP_FILE"
    chmod +x "$PROTOC_BIN"
  fi
  [ -x "$PROTOC_BIN" ] || die "protoc downloaded but not executable at $PROTOC_BIN"
  PROTOC="$PROTOC_BIN"
  export PROTOC
  say "Using protoc at $PROTOC_BIN"
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
  warn "if the log mentions a missing linker or cc:"
  warn "  macOS: xcode-select --install   Linux: install build-essential / gcc"
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
    if [ ! -f "$HOME/.profile" ] && [ ! -f "$HOME/.bashrc" ] && [ ! -f "$HOME/.zshrc" ] && [ ! -f "$HOME/.zshenv" ]; then
      : > "$HOME/.zshenv"
    fi
    for profile in "$HOME/.profile" "$HOME/.bash_profile" "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.zshenv"; do
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
