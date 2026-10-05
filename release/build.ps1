param(
    [ValidateSet('all', 'windows', 'linux')]
    [string]$Target = 'all'
)

$ErrorActionPreference = 'Stop'
$Here = Split-Path -Parent $MyInvocation.MyCommand.Path
$Root = (Resolve-Path (Join-Path $Here '..')).Path
$Pkg = 'scrcpy-pad'
$VersionMatch = Select-String -Path (Join-Path $Root 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $VersionMatch) { throw '无法从 Cargo.toml 解析版本号' }
$Version = $VersionMatch.Matches[0].Groups[1].Value

function Invoke-Checked([string]$File, [string[]]$Arguments) {
    & $File @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "命令失败 ($LASTEXITCODE): $File $($Arguments -join ' ')"
    }
}

function Build-Windows {
    Write-Host '[release] 构建 Windows 版 ...'
    Push-Location $Root
    try {
        Invoke-Checked 'cargo' @('build', '--release', '--locked')
    } finally {
        Pop-Location
    }

    $out = Join-Path $Here 'windows-x86_64'
    if (Test-Path -LiteralPath $out) { Remove-Item -LiteralPath $out -Recurse }
    New-Item -ItemType Directory -Path (Join-Path $out 'icons') -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $Root 'target\release\scrcpy-pad.exe') -Destination (Join-Path $out 'scrcpy-pad.exe') -Force
    if (Test-Path -LiteralPath (Join-Path $Root 'icons\scrcpy-pad.png')) {
        Copy-Item -LiteralPath (Join-Path $Root 'icons\scrcpy-pad.png') -Destination (Join-Path $out 'icons\scrcpy-pad.png') -Force
    }
    if (Test-Path -LiteralPath (Join-Path $Root 'README.md')) {
        Copy-Item -LiteralPath (Join-Path $Root 'README.md') -Destination (Join-Path $out 'README.md') -Force
    }
    if (Test-Path -LiteralPath (Join-Path $Root 'LICENSE')) {
        Copy-Item -LiteralPath (Join-Path $Root 'LICENSE') -Destination (Join-Path $out 'LICENSE') -Force
    }
    $launcher = "@echo off`r`nchcp 65001 > nul`r`ncd /d %~dp0`r`n$Pkg.exe`r`n"
    [IO.File]::WriteAllText((Join-Path $out '启动.bat'), $launcher, [Text.UTF8Encoding]::new($false))

    $zip = Join-Path $Here "$Pkg-$Version-windows-x86_64.zip"
    if (Test-Path -LiteralPath $zip) { Remove-Item -LiteralPath $zip }
    Compress-Archive -LiteralPath $out -DestinationPath $zip -Force
    Write-Host "[release] Windows 包就绪: windows-x86_64/ 和 $Pkg-$Version-windows-x86_64.zip"
}

function Build-Linux {
    if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) {
        throw '未找到 wsl.exe，无法从 Windows 构建 Linux 包'
    }
    $distro = 'Ubuntu-20.04'
    $listed = (& wsl.exe -l -q) -replace "`0", '' | ForEach-Object { $_.Trim() }
    if ($listed -notcontains $distro) {
        throw "未找到 WSL 发行版 $distro；请先安装 Ubuntu-20.04"
    }
    if ($Root -notmatch '^([A-Za-z]):\\') {
        throw "WSL 构建目前只支持盘符路径: $Root"
    }
    $drive = $Matches[1].ToLowerInvariant()
    $rest = $Root.Substring(2).Replace('\', '/').TrimStart('/')
    $wslRoot = "/mnt/$drive/$rest"
    Write-Host '[release] 构建 Linux 版(Ubuntu 20.04 / WSL) ...'
    $envPath = '/home/azrl2004/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin'
    $command = "cd '$wslRoot' && bash release/build.sh linux"
    Invoke-Checked 'wsl.exe' @('-d', $distro, '--', 'env', "PATH=$envPath", 'CARGO_HTTP_TIMEOUT=180', 'bash', '-lc', $command)
}

switch ($Target) {
    'windows' { Build-Windows }
    'linux' { Build-Linux }
    default { Build-Windows; Build-Linux }
}
