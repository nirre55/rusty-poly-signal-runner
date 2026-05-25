param(
    [string[]]$Configs = @(
        "configs/btc_combined.env",
        "configs/eth_ensemble.env",
        "configs/btc_15m_ensemble.env",
        "configs/eth_15m_ensemble.env",
        "configs/btc_1h_ensemble.env",
        "configs/eth_1h_ensemble.env"
    ),
    [switch]$DebugBuild,
    [switch]$NoRestart,
    [switch]$ValidateOnly,
    [switch]$SkipBuild,
    [int]$RestartDelaySeconds = 15
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$supervisorDir = Join-Path $root "logs/supervisor"
New-Item -ItemType Directory -Force -Path $supervisorDir | Out-Null

$buildArgs = if ($DebugBuild) { @("build") } else { @("build", "--release") }
$binaryRelative = if ($DebugBuild) {
    "target/debug/rusty-poly-signal-runner.exe"
} else {
    "target/release/rusty-poly-signal-runner.exe"
}
$binaryPath = Join-Path $root $binaryRelative
$restartEnabled = -not $NoRestart

if (-not $ValidateOnly -and -not $SkipBuild) {
    Write-Host "Building once: cargo $($buildArgs -join ' ')"
    & cargo @buildArgs
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }
}

if (-not $ValidateOnly -and -not (Test-Path $binaryPath)) {
    throw "Binaire introuvable: $binaryPath"
}

foreach ($cfg in $Configs) {
    $cfgPath = Join-Path $root $cfg
    if (-not (Test-Path $cfgPath)) {
        Write-Warning "Config introuvable: $cfg"
        continue
    }

    $title = (($cfg -replace "^configs[\\/]", "") -replace "\.env$", "")
    $logPath = Join-Path $supervisorDir "$title.console.log"
    $supervisorScript = Join-Path $supervisorDir "$title.supervisor.ps1"
    $restartLiteral = if ($restartEnabled) { '$true' } else { '$false' }

    $scriptContent = @"
`$ErrorActionPreference = "Continue"
Set-Location "$root"
`$Host.UI.RawUI.WindowTitle = "$title"
`$env:STRATEGY_CONFIG = "$cfg"
`$logPath = "$logPath"
`$restartEnabled = $restartLiteral
`$restartDelaySeconds = $RestartDelaySeconds
`$binaryPath = "$binaryPath"
`$title = "$title"

try {
    chcp 65001 | Out-Null
    [Console]::InputEncoding = [System.Text.UTF8Encoding]::new()
    [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new()
    `$OutputEncoding = [System.Text.UTF8Encoding]::new()
} catch {
}

New-Item -ItemType Directory -Force -Path (Split-Path -Parent `$logPath) | Out-Null

function Write-SupervisorLog {
    param([string]`$Message)
    `$line = "[{0}] [{1}] {2}" -f (Get-Date -Format o), `$title, `$Message
    `$line | Tee-Object -FilePath `$logPath -Append
}

Write-SupervisorLog "supervisor started | config=$cfg | binary=`$binaryPath | restart=`$restartEnabled"

while (`$true) {
    Write-SupervisorLog "process starting"
    & `$binaryPath *>&1 | ForEach-Object { `$_.ToString() } | Tee-Object -FilePath `$logPath -Append
    `$exitCode = `$LASTEXITCODE
    Write-SupervisorLog "process exited code=`$exitCode"

    if (-not `$restartEnabled) {
        break
    }

    Write-SupervisorLog "restart in `$restartDelaySeconds seconds"
    Start-Sleep -Seconds `$restartDelaySeconds
}
"@

    Set-Content -Path $supervisorScript -Value $scriptContent -Encoding UTF8

    if ($ValidateOnly) {
        $tokens = $null
        $errors = $null
        [System.Management.Automation.Language.Parser]::ParseFile(
            $supervisorScript,
            [ref]$tokens,
            [ref]$errors
        ) | Out-Null
        if ($errors.Count -gt 0) {
            Write-Error "Supervisor script invalide: $supervisorScript`n$($errors | Out-String)"
            continue
        }
        Write-Host "Validated $title -> $supervisorScript"
        continue
    }

    Start-Process powershell.exe `
        -ArgumentList @("-NoExit", "-ExecutionPolicy", "Bypass", "-File", $supervisorScript) `
        -WorkingDirectory $root

    Write-Host "Started $title -> $logPath"
    Start-Sleep -Milliseconds 500
}

if ($ValidateOnly) {
    Write-Host "Validated $($Configs.Count) strategies. No process started."
} else {
    Write-Host "Requested start for $($Configs.Count) strategies."
    Write-Host "Default mode: cargo build --release once, then direct binary execution with auto-restart."
    Write-Host "Use -DebugBuild, -NoRestart, or -SkipBuild if needed."
}
