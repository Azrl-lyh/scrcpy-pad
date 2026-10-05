[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$env:CARGO_NET_OFFLINE = 'true'
cargo build --release --bins --offline
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}
Write-Host "Built:"
Write-Host (Join-Path $PSScriptRoot 'target\release\scrcpy-pad-hdc.exe')
Write-Host (Join-Path $PSScriptRoot 'target\release\hdc-pad-gui.exe')
