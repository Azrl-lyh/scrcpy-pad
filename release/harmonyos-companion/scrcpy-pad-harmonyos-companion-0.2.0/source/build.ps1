[CmdletBinding()]
param(
    [ValidateSet('debug', 'release')]
    [string]$Mode = 'debug'
)

$ErrorActionPreference = 'Stop'

$DevEcoHome = if ($env:DEVECO_HOME) { $env:DEVECO_HOME } else { 'C:\Huawei\DevEco Studio' }
$SdkHome = Join-Path $DevEcoHome 'sdk'
$JavaHome = Join-Path $DevEcoHome 'jbr'
$NodeHome = Join-Path $DevEcoHome 'tools\node'
$Hvigor = Join-Path $DevEcoHome 'tools\hvigor\bin\hvigorw.bat'

foreach ($required in @($SdkHome, $JavaHome, $NodeHome, $Hvigor)) {
    if (-not (Test-Path -LiteralPath $required)) {
        throw "DevEco component not found: $required"
    }
}

$env:DEVECO_SDK_HOME = $SdkHome
$env:JAVA_HOME = $JavaHome
$env:NODE_HOME = $NodeHome
$env:Path = "$JavaHome\bin;$NodeHome;$(Join-Path $DevEcoHome 'tools\ohpm\bin');$(Join-Path $DevEcoHome 'tools\hvigor\bin');$env:Path"

Write-Host "Building HarmonyOS entry HAP ($Mode)..."
& $Hvigor --mode module -p product=default -p module=entry@default -p "buildMode=$Mode" assembleHap --no-daemon
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

Write-Host "Build completed. Output directory:"
Write-Host (Join-Path $PSScriptRoot 'entry\build\default\outputs\default')
