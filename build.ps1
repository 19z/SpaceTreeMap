$ErrorActionPreference = 'Stop'
$env:CARGO_HOME = Join-Path $PSScriptRoot '.cargo-home'
cargo build --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Write-Host "Built: $(Join-Path $PSScriptRoot 'target\release\space_tree_map.exe')"
