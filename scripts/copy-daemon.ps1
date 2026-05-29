param(
  [Parameter(Mandatory = $true)]
  [ValidateSet("win32-x64", "win32-arm64")]
  [string]$Target
)

$ErrorActionPreference = "Stop"

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$binaryName = "import-lens-daemon.exe"
$source = Join-Path $repoRoot "target/release/$binaryName"
$targetDir = Join-Path $repoRoot "bin/$Target"
$destination = Join-Path $targetDir $binaryName

if (-not (Test-Path -LiteralPath $source)) {
  throw "Daemon binary not found at $source. Run pnpm build:daemon first."
}

New-Item -ItemType Directory -Force -Path $targetDir | Out-Null
Copy-Item -LiteralPath $source -Destination $destination -Force
Write-Host "Copied $source to $destination"
