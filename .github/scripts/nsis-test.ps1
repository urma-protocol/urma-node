# Silent install, reinstall and uninstall of the NSIS setup against the real SCM.
param([Parameter(Mandatory)] [string] $Setup)
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

$dir = Join-Path $env:ProgramFiles 'URMA Node'
$token = Join-Path $env:ProgramData 'URMA Node\service.token'

function Run([string] $Exe, [string] $Arguments) {
    $p = Start-Process -FilePath $Exe -ArgumentList $Arguments -Wait -PassThru
    if ($p.ExitCode -ne 0) { throw "$Exe exited $($p.ExitCode)" }
}

function ServicePid {
    $s = Get-CimInstance Win32_Service -Filter "Name='urma-node'"
    if (-not $s) { throw 'service urma-node not registered' }
    $s.ProcessId
}

function WaitRunning {
    for ($i = 0; $i -lt 60; $i++) {
        $out = sc.exe query urma-node | Out-String
        if ($out -match 'RUNNING') { Write-Output $out; return }
        Start-Sleep 2
    }
    sc.exe query urma-node
    throw 'urma-node not RUNNING'
}

function WaitStatus {
    for ($i = 0; $i -lt 60; $i++) {
        & (Join-Path $dir 'urma-node-control.exe') --endpoint 127.0.0.1:28331 --token-file $token status
        if ($LASTEXITCODE -eq 0) { return }
        Start-Sleep 2
    }
    throw 'control status failed'
}

Run $Setup '/S'
WaitRunning
WaitStatus
$before = ServicePid

Run $Setup '/S'
WaitRunning
$after = ServicePid
Write-Output "service pid $before -> $after"
if ($before -eq $after) { throw 'reinstall did not restart the service' }
WaitStatus

Run (Join-Path $dir 'uninstall.exe') "/S _?=$dir"
sc.exe query urma-node
if ($LASTEXITCODE -ne 1060) { throw "service still registered ($LASTEXITCODE)" }
if (-not (Test-Path (Join-Path $env:ProgramData 'URMA Node\data'))) { throw 'data removed by uninstall' }
Write-Output 'nsis lifecycle ok'
exit 0
