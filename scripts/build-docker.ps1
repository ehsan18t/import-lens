$ErrorActionPreference = "Stop"

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
Set-Location $repoRoot

Write-Host "Building Docker image import-lens-builder..."
docker build -t import-lens-builder -f Dockerfile.cross .

Write-Host "Running Docker container to build cross-platform VSIX files..."
docker run --rm -v "${repoRoot}:/workspace" import-lens-builder

Write-Host "Docker build completed! Check your repository root for the new .vsix files."
