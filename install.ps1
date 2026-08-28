# Grok Build Max installer (Windows PowerShell).
#
# Builds from source and installs the `grokmax` executable onto your PATH:
#   irm https://raw.githubusercontent.com/FearL0rd/grok-build-max/main/install.ps1 | iex
#
# Layout:
#   ~\.grokmax\src                        git clone (created/updated)
#   %LOCALAPPDATA%\grokmax\bin\grokmax.exe installed binary (added to user PATH)
#
# Env overrides: REPO_URL, REF, GROKMAX_HOME

$ErrorActionPreference = 'Stop'

$RepoUrl     = if ($env:REPO_URL) { $env:REPO_URL } else { 'https://github.com/FearL0rd/grok-build-max.git' }
$Ref         = if ($env:REF) { $env:REF } else { 'main' }
$GrokmaxHome = if ($env:GROKMAX_HOME) { $env:GROKMAX_HOME } else { Join-Path $env:USERPROFILE '.grokmax' }
$SrcDir      = Join-Path $GrokmaxHome 'src'

function Fail($msg) { Write-Error $msg; exit 1 }

# --- prerequisites -----------------------------------------------------------

if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
  Fail "git not found - install git (https://git-scm.com) and re-run"
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
  Write-Host 'Rust not found - installing rustup (non-interactive)...'
  Invoke-WebRequest -UseBasicParsing 'https://win.rustup.rs/x86_64' -OutFile "$env:TEMP\rustup-init.exe"
  & "$env:TEMP\rustup-init.exe" -y --default-toolchain stable
  $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
  if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Fail 'rustup ran but cargo is still not on PATH - open a new shell and re-run'
  }
}

# The build needs a protoc: bin/protoc resolves via DotSlash, else a protoc on PATH / $env:PROTOC.
if (-not (Get-Command protoc -ErrorAction SilentlyContinue) -and -not $env:PROTOC) {
  if (-not (Get-Command dotslash -ErrorAction SilentlyContinue)) {
    Write-Host 'protoc/DotSlash not found - installing DotSlash via cargo (this can take a minute)...'
    cargo install dotslash
    if (-not (Get-Command dotslash -ErrorAction SilentlyContinue)) {
      Fail 'could not install DotSlash; install protoc or DotSlash and re-run'
    }
  }
}

# --- fetch -------------------------------------------------------------------
# NOTE: Windows PowerShell 5.1 turns native-command stderr (git progress, cargo
# "Compiling" lines) into terminating NativeCommandErrors when it is redirected
# under $ErrorActionPreference = 'Stop'. Run native tools with 'Continue' and
# check $LASTEXITCODE explicitly instead of redirecting stderr.

function Invoke-Native {
  param([scriptblock]$Command)
  $prev = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  try { & $Command } finally { $ErrorActionPreference = $prev }
  return $LASTEXITCODE
}

New-Item -ItemType Directory -Force -Path $GrokmaxHome | Out-Null
if (Test-Path (Join-Path $SrcDir '.git')) {
  Write-Host "Updating existing clone at $SrcDir..."
  $rc = Invoke-Native { git -C $SrcDir fetch --quiet --depth 1 origin $Ref }
  if ($rc -ne 0) { Fail "git fetch failed (exit $rc)" }
  $rc = Invoke-Native { git -C $SrcDir checkout --quiet FETCH_HEAD }
  if ($rc -ne 0) { Fail "git checkout failed (exit $rc)" }
} else {
  # A previous interrupted run can leave a partial clone behind - remove it.
  if (Test-Path $SrcDir) {
    Write-Host "Removing incomplete clone at $SrcDir..."
    Remove-Item -Recurse -Force $SrcDir
  }
  Write-Host "Cloning $RepoUrl ($Ref) to $SrcDir..."
  $rc = Invoke-Native { git clone --quiet --depth 1 --branch $Ref $RepoUrl $SrcDir }
  if ($rc -ne 0) {
    $rc = Invoke-Native { git clone --quiet --depth 1 $RepoUrl $SrcDir }
    if ($rc -ne 0) { Fail "git clone failed (exit $rc)" }
  }
}

# --- build -------------------------------------------------------------------

Write-Host 'Building Grok Build Max (release) - first build can take several minutes...'
Push-Location $SrcDir
try {
  $rc = Invoke-Native { cargo build --release -p xai-grok-pager-bin }
  if ($rc -ne 0) { Fail "cargo build failed (exit $rc)" }
} finally { Pop-Location }

$Built = Join-Path $SrcDir 'target\release\grokmax.exe'
if (-not (Test-Path $Built)) { Fail "build finished but binary missing at $Built" }

# --- install -----------------------------------------------------------------

$BinDir = Join-Path $env:LOCALAPPDATA 'grokmax\bin'
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
Copy-Item -Force $Built (Join-Path $BinDir 'grokmax.exe')

# Add to the user PATH (persistent) if not already there.
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not $userPath -or ($userPath -split ';') -notcontains $BinDir) {
  [Environment]::SetEnvironmentVariable('Path', "$userPath;$BinDir".TrimStart(';'), 'User')
  $env:PATH = "$env:PATH;$BinDir"
  Write-Host "Added $BinDir to your user PATH - restart your terminal for it to take effect."
}

Write-Host ''
Write-Host "Grok Build Max installed: $(Join-Path $BinDir 'grokmax.exe')"
& (Join-Path $BinDir 'grokmax.exe') --version
Write-Host "Run 'grokmax' to start. Type /providers inside the TUI to configure providers and failover order."
