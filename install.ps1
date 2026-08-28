# Grok Build Max installer (Windows PowerShell).
#
# Builds from source and installs the `grokmax` executable onto your PATH:
#   irm https://raw.githubusercontent.com/FearL0rd/grok-build-max/main/install.ps1 | iex
#
# Layout:
#   ~\.grokmax\src                        git clone (created/updated)
#   ~\.grokmax\build.log                  full build output (last run)
#   %LOCALAPPDATA%\grokmax\bin\grokmax.exe installed binary (added to user PATH)
#
# Env overrides: REPO_URL, REF, GROKMAX_HOME

$ErrorActionPreference = 'Stop'

$RepoUrl     = if ($env:REPO_URL) { $env:REPO_URL } else { 'https://github.com/FearL0rd/grok-build-max.git' }
$Ref         = if ($env:REF) { $env:REF } else { 'main' }
$GrokmaxHome = if ($env:GROKMAX_HOME) { $env:GROKMAX_HOME } else { Join-Path $env:USERPROFILE '.grokmax' }
$SrcDir      = Join-Path $GrokmaxHome 'src'

function Fail($msg) { Write-Host "ERROR: $msg" -ForegroundColor Red; exit 1 }

# Run a native tool with stderr redirected into the pipeline. Windows
# PowerShell 5.1 raises terminating NativeCommandErrors for redirected native
# stderr when $ErrorActionPreference is 'Stop', so native tools always run
# under 'Continue' and report via $LASTEXITCODE instead.
$ErrorActionPreference = 'Continue'

# --- prerequisites -----------------------------------------------------------

if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
  Fail "git not found - install git (https://git-scm.com) and re-run"
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
  Write-Host 'Rust not found - installing rustup (non-interactive)...'
  Invoke-WebRequest -UseBasicParsing 'https://win.rustup.rs/x86_64' -OutFile "$env:TEMP\rustup-init.exe"
  & "$env:TEMP\rustup-init.exe" -y --default-toolchain stable
  if ($LASTEXITCODE -ne 0) { Fail "rustup install failed (exit $LASTEXITCODE)" }
  $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
  if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Fail 'rustup ran but cargo is still not on PATH - open a new shell and re-run'
  }
}

# The build needs a protoc. The repo's bin/protoc is a DotSlash wrapper whose
# manifest has no Windows entry, so on Windows we install a real protoc release
# instead and point $env:PROTOC at it. A protoc already on PATH or in $env:PROTOC wins.
$ProtocHome = Join-Path $GrokmaxHome 'protoc'
if (-not (Get-Command protoc -ErrorAction SilentlyContinue) -and -not $env:PROTOC) {
  $ProtocVersion = '29.3'
  $ProtocExe = Join-Path $ProtocHome 'bin\protoc.exe'
  if (-not (Test-Path $ProtocExe)) {
    $Zip = Join-Path $env:TEMP "protoc-$ProtocVersion-win64.zip"
    Write-Host "protoc not found - downloading protoc $ProtocVersion for Windows..."
    Invoke-WebRequest -UseBasicParsing `
      "https://github.com/protocolbuffers/protobuf/releases/download/v$ProtocVersion/protoc-$ProtocVersion-win64.zip" `
      -OutFile $Zip
    if ($LASTEXITCODE -ne 0) { Fail "protoc download failed (exit $LASTEXITCODE)" }
    if (Test-Path $ProtocHome) { Remove-Item -Recurse -Force $ProtocHome }
    Expand-Archive -Path $Zip -DestinationPath $ProtocHome -Force
    Remove-Item -Force $Zip
  }
  if (-not (Test-Path $ProtocExe)) { Fail "protoc downloaded but missing at $ProtocExe" }
  $env:PROTOC = $ProtocExe
  Write-Host "Using protoc at $ProtocExe"
}

# --- fetch -------------------------------------------------------------------

New-Item -ItemType Directory -Force -Path $GrokmaxHome | Out-Null
if (Test-Path (Join-Path $SrcDir '.git')) {
  Write-Host "Updating existing clone at $SrcDir..."
  git -C $SrcDir fetch --quiet --depth 1 origin $Ref
  if ($LASTEXITCODE -ne 0) { Fail "git fetch failed (exit $LASTEXITCODE)" }
  git -C $SrcDir checkout --quiet FETCH_HEAD
  if ($LASTEXITCODE -ne 0) { Fail "git checkout failed (exit $LASTEXITCODE)" }
} else {
  # A previous interrupted run can leave a partial clone behind - remove it.
  if (Test-Path $SrcDir) {
    Write-Host "Removing incomplete clone at $SrcDir..."
    Remove-Item -Recurse -Force $SrcDir
  }
  Write-Host "Cloning $RepoUrl ($Ref) to $SrcDir..."
  git clone --quiet --depth 1 --branch $Ref $RepoUrl $SrcDir
  if ($LASTEXITCODE -ne 0) {
    git clone --quiet --depth 1 $RepoUrl $SrcDir
    if ($LASTEXITCODE -ne 0) { Fail "git clone failed (exit $LASTEXITCODE)" }
  }
}

# --- build -------------------------------------------------------------------

$BuildLog = Join-Path $GrokmaxHome 'build.log'
Write-Host 'Building Grok Build Max (release) - first build can take several minutes...'
Write-Host "Full build output: $BuildLog"
Push-Location $SrcDir
try {
  cargo build --release -p xai-grok-pager-bin 2>&1 |
    Tee-Object -FilePath $BuildLog | Out-Host
  if ($LASTEXITCODE -ne 0) {
    Write-Host '--- last 40 build log lines ---'
    Get-Content $BuildLog -Tail 40 | Write-Host
    Write-Host '--- end of excerpt ---'
    $LogText = Get-Content $BuildLog -Raw
    if ($LogText -match 'link\.exe not found|LNK1104|linker.*not found') {
      Write-Host 'Hint: the MSVC linker is missing. Install "Visual Studio Build Tools"'
      Write-Host 'with the "Desktop development with C++" workload, then re-run:'
      Write-Host '  https://visualstudio.microsoft.com/visual-cpp-build-tools/'
    }
    Fail "cargo build failed (exit $LASTEXITCODE) - full log: $BuildLog"
  }
} finally { Pop-Location }

$ErrorActionPreference = 'Stop'

# --- install -----------------------------------------------------------------

$Built = Join-Path $SrcDir 'target\release\grokmax.exe'
if (-not (Test-Path $Built)) { Fail "build finished but binary missing at $Built" }

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
