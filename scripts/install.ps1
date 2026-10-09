# install.ps1 — download, verify, and install the cpm-planner MCP server on Windows.
#
# Usage:
#   irm https://raw.githubusercontent.com/praxec/cpm-planner/main/scripts/install.ps1 | iex
#   .\install.ps1 -Version vX.Y.Z -InstallDir "$env:LOCALAPPDATA\Programs\cpm-planner"
#   .\install.ps1 -PrintTarget
#
# Options:
#   -Version TAG        Release tag to install (default: latest)
#   -InstallDir DIR     Managed destination directory (default: $env:LOCALAPPDATA\Programs\cpm-planner)
#   -BaseUrl URL        Release base URL (default: https://github.com/praxec/cpm-planner/releases)
#   -PrintTarget        Print the resolved target triple and exit (no network)
#   -DryRun             Print resolved URLs and exit (no install)
#   -AddToPath          Add the install dir to your user PATH (opt-in)
#
# Environment overrides: PRAXEC_REPO PRAXEC_BIN PRAXEC_VERSION PRAXEC_INSTALL_DIR
#                        PRAXEC_BASE_URL PRAXEC_ALLOW_INSECURE=1 (allow non-https; testing only) PRAXEC_OS PRAXEC_ARCH PRAXEC_MAX_BYTES
#
# Writes only the binary into -InstallDir; application state and configuration
# live elsewhere (see the README) and are never touched.
[CmdletBinding()]
param(
  [string]$Version = $env:PRAXEC_VERSION,
  [string]$InstallDir = $env:PRAXEC_INSTALL_DIR,
  [string]$BaseUrl = $env:PRAXEC_BASE_URL,
  [switch]$PrintTarget,
  [switch]$DryRun,
  [switch]$AddToPath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$DefaultRepo = 'praxec/cpm-planner'
$DefaultBin  = 'cpm-planner'
$Repo    = if ($env:PRAXEC_REPO) { $env:PRAXEC_REPO } else { $DefaultRepo }
$Bin     = if ($env:PRAXEC_BIN)  { $env:PRAXEC_BIN }  else { $DefaultBin }
$Version = if ($Version) { $Version } else { 'latest' }
$LocalPrograms = if ($env:LOCALAPPDATA) { $env:LOCALAPPDATA } else { [Environment]::GetFolderPath('UserProfile') }
$InstallDir = if ($InstallDir) { $InstallDir } else { Join-Path $LocalPrograms "Programs\${Bin}" }
$BaseUrl = if ($BaseUrl) { $BaseUrl } else { "https://github.com/$Repo/releases" }
$MaxBytes = if ($env:PRAXEC_MAX_BYTES) { [int64]$env:PRAXEC_MAX_BYTES } else { 134217728 }

# Windows PowerShell 5.1 may default to TLS 1.0/1.1; make sure TLS 1.2 is enabled.
if ($PSVersionTable.PSVersion.Major -lt 6) {
  [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
}
# Invoke-WebRequest's progress bar makes downloads dramatically slower on 5.1.
$ProgressPreference = 'SilentlyContinue'

function Write-Say([string]$Message) { Write-Host "install: $Message" }
function Write-Die([string]$Message) { Write-Error "install: error: $Message"; exit 1 }

if (($BaseUrl -notlike 'https://*') -and ($env:PRAXEC_ALLOW_INSECURE -ne '1')) {
  Write-Die "refusing non-https base URL '$BaseUrl' (set PRAXEC_ALLOW_INSECURE=1 to override for local testing)"
}

function Get-HostOs {
  if ($env:PRAXEC_OS) { return $env:PRAXEC_OS }
  if ($env:OS -eq 'Windows_NT') { return 'windows' }
  if ((Get-Variable -Name IsMacOS -ErrorAction SilentlyContinue) -and $IsMacOS) { return 'darwin' }
  if ((Get-Variable -Name IsLinux -ErrorAction SilentlyContinue) -and $IsLinux) { return 'linux' }
  return 'unknown'
}

function Get-HostArch {
  if ($env:PRAXEC_ARCH) { return $env:PRAXEC_ARCH }
  # OSArchitecture reports the OS/native architecture even for an emulated process.
  $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
  switch ($arch) {
    'X64'   { return 'x86_64' }
    'Arm64' { return 'aarch64' }
    default { Write-Die "unsupported CPU architecture '$arch'; no prebuilt binary is published for it. No source-build fallback is attempted." }
  }
}

function Get-TargetTriple([string]$Os, [string]$Arch) {
  switch ("$Os-$Arch") {
    'windows-x86_64'  { return 'x86_64-pc-windows-msvc' }
    'windows-aarch64' { return 'aarch64-pc-windows-msvc' }
    default { Write-Die "unsupported platform $Os/$Arch; no prebuilt binary is published for it. No source-build fallback is attempted." }
  }
}

$OS = Get-HostOs
$ARCH = Get-HostArch
$Target = Get-TargetTriple $OS $ARCH
$Asset = "${Bin}-${Target}.zip"
function Set-Urls {
  $script:ReleaseUrl = if ($Version -eq 'latest') { "$BaseUrl/latest/download" } else { "$BaseUrl/download/$Version" }
  $script:DownloadUrl = "$ReleaseUrl/$Asset"
  $script:ChecksumUrl = "$ReleaseUrl/checksums.sha256"
}
Set-Urls

# Resolve "latest" to a concrete tag so the archive and checksums come from the same release.
function Resolve-LatestTag {
  $location = $null
  try {
    Invoke-WebRequest -Uri "$BaseUrl/latest" -Method Head -MaximumRedirection 0 -UseBasicParsing -TimeoutSec 60 -ErrorAction Stop | Out-Null
  } catch {
    $resp = $_.Exception.Response
    if ($resp) {
      $location = $resp.Headers.Location
      if (-not $location) { $location = $resp.Headers['Location'] }
    }
  }
  if (-not $location) { Write-Die "could not resolve the latest release from $BaseUrl/latest" }
  $tag = ([string]$location).TrimEnd('/').Split('/')[-1]
  if ($tag -notmatch '^v\d') { Write-Die "could not determine latest release tag from '$location'" }
  return $tag
}

if ($PrintTarget) { Write-Output $Target; exit 0 }
if ($DryRun) {
  Write-Say "platform: $OS/$ARCH -> $Target"
  Write-Say "asset:    $DownloadUrl"
  Write-Say "checksum: $ChecksumUrl"
  exit 0
}

$tmpDest = $null
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("${Bin}.install." + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work | Out-Null
try {
  if ($Version -eq 'latest') {
    $Version = Resolve-LatestTag
    Write-Say "resolved latest release: $Version"
    Set-Urls
  }
  $archivePath = Join-Path $work $Asset
  $checksumPath = Join-Path $work 'checksums.sha256'

  Write-Say "downloading $DownloadUrl"
  Invoke-WebRequest -Uri $DownloadUrl -OutFile $archivePath -UseBasicParsing -TimeoutSec 180
  Invoke-WebRequest -Uri $ChecksumUrl -OutFile $checksumPath -UseBasicParsing -TimeoutSec 60

  foreach ($p in @($archivePath, $checksumPath)) {
    $len = (Get-Item $p).Length
    if ($len -gt $MaxBytes) { Write-Die "downloaded file exceeds $MaxBytes bytes: $p" }
  }

  $expected = $null
  foreach ($line in Get-Content $checksumPath) {
    $parts = -split ($line.Trim())
    if ($parts.Count -ge 2) {
      $name = $parts[-1].TrimStart('*')
      if ($name -eq $Asset) { $expected = $parts[0]; break }
    }
  }
  if (-not $expected) { Write-Die "no checksum entry for $Asset in checksums.sha256" }
  $actual = (Get-FileHash -Algorithm SHA256 -Path $archivePath).Hash.ToLowerInvariant()
  if ($actual -ne $expected.ToLowerInvariant()) {
    Write-Die "checksum mismatch for $Asset (expected $expected, got $actual)"
  }
  Write-Say "checksum verified: $actual"

  # Validate archive entries (no rooted paths, no traversal, no symlinks) before extracting.
  Add-Type -AssemblyName System.IO.Compression.FileSystem
  $zip = [System.IO.Compression.ZipFile]::OpenRead($archivePath)
  try {
    foreach ($entry in $zip.Entries) {
      $name = $entry.FullName
      if ([System.IO.Path]::IsPathRooted($name)) { Write-Die "refusing absolute path in archive: $name" }
      if ($name -match '(^|[\\/])\.\.([\\/]|$)') { Write-Die "refusing parent-directory path in archive: $name" }
      $unixMode = ($entry.ExternalAttributes -shr 16) -band 0xF000
      if ($unixMode -eq 0xA000) { Write-Die "refusing archive containing a symlink: $name" }
    }
  } finally { $zip.Dispose() }

  $extractDir = Join-Path $work 'extract'
  Expand-Archive -Path $archivePath -DestinationPath $extractDir -Force
  $src = Get-ChildItem -Path $extractDir -Recurse -File -Filter "${Bin}.exe" | Select-Object -First 1
  if (-not $src) { Write-Die "binary '${Bin}.exe' not found inside $Asset" }

  New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
  $dest = Join-Path $InstallDir "${Bin}.exe"
  $old = "$dest.old"
  # Remove leftovers from a previous upgrade (a running exe cannot be deleted, so it was renamed).
  Get-ChildItem -Path $InstallDir -Filter "${Bin}.exe.old" -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
  $tmpDest = Join-Path $InstallDir ".${Bin}.tmp.$PID.exe"
  Copy-Item -Force -Path $src.FullName -Destination $tmpDest
  if (Test-Path $dest) {
    # Windows lets a running exe be renamed but not overwritten: move it aside first.
    Move-Item -Force -Path $dest -Destination $old
  }
  Move-Item -Force -Path $tmpDest -Destination $dest
  $tmpDest = $null
  Write-Say "installed $Bin $Version -> $dest"
  Write-Say "restart your MCP client to load the new version"

  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  $onPath = ($userPath -split ';') -contains $InstallDir -or (($env:Path -split ';') -contains $InstallDir)
  if (-not $onPath) {
    if ($AddToPath) {
      [Environment]::SetEnvironmentVariable('Path', "$InstallDir;" + $userPath, 'User')
      Write-Say "added $InstallDir to your user PATH (open a new terminal to use it)"
    } else {
      Write-Say "$InstallDir is not on your PATH. Add it with:"
      Write-Say "  [Environment]::SetEnvironmentVariable('Path', `"$InstallDir;`" + [Environment]::GetEnvironmentVariable('Path','User'), 'User')"
      Write-Say "or re-run with -AddToPath. GUI MCP clients do not inherit your shell PATH, so use the absolute path $dest in their configs."
    }
  }
} finally {
  if ($tmpDest) { Remove-Item -Force -Path $tmpDest -ErrorAction SilentlyContinue }
  Remove-Item -Recurse -Force -Path $work -ErrorAction SilentlyContinue
}
