#Requires -Version 5.1
<#
.SYNOPSIS  IPProxyPool watchdog 安装器 (OPT-R4 C7).
.DESCRIPTION
  install   : 幂等注册开机自启任务（先删同名残留；SYSTEM 身份；启动触发；
               任务计划默认“已在运行则不再启动新实例”，与看护 30s 常驻循环配套）
  uninstall : 删除任务（看护进程本身需另行停止）
  status    : 任务状态＋看护进程是否在跑
  需管理员权限（SCHTASKS 注册要求）；DETACHED powershell 在受限会话存活不了，
  生产值守必须走本脚本注册的 schtasks（见 tools/ipp_watchdog.ps1 头注释）。
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File tools/install_watchdog.ps1 status
  powershell -ExecutionPolicy Bypass -File tools/install_watchdog.ps1 install
#>
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("install", "uninstall", "status")]
    [string]$Action
)

$ErrorActionPreference = "Continue"
$TaskName = "IPProxyWatchdog"
$Script = Join-Path $PSScriptRoot "ipp_watchdog.ps1"

function Test-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    return ([Security.Principal.WindowsPrincipal]$id).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)
}

if ($Action -eq "status") {
    schtasks /query /tn $TaskName 2>$null
    if ($LASTEXITCODE -ne 0) { Write-Output "task: not registered" }
    $watchers = Get-CimInstance Win32_Process -Filter "Name='powershell.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -like "*ipp_watchdog.ps1*" -and $_.CommandLine -notlike "*-Command*" }
    if ($watchers) {
        $watchers | ForEach-Object { Write-Output ("watchdog running: PID " + $_.ProcessId) }
    } else {
        Write-Output "watchdog process: not running"
    }
    exit 0
}

if (-not (Test-Admin)) {
    Write-Output "install/uninstall needs admin (run as Administrator)"
    exit 2
}

if ($Action -eq "uninstall") {
    schtasks /delete /tn $TaskName /f
    exit 0
}

# install（幂等：先删残留再建；装完立即跑一次，返回任务状态行）。
schtasks /delete /tn $TaskName /f 2>$null | Out-Null
schtasks /create /tn $TaskName /tr "powershell -NoProfile -ExecutionPolicy Bypass -File `"$Script`"" /sc onstart /ru SYSTEM /rl HIGHEST /f
schtasks /run /tn $TaskName
schtasks /query /tn $TaskName /v /fo list | Select-String "Status|上次运行|Last Run|下次运行|Next Run"
