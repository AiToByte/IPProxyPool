<# IPProxyPool watchdog: process-outside keeper (Phase C1).
   The in-process supervisor cannot revive a dead process (Windows orderly
   exit ~every 5 min); this loop does: probe :8916/:9091, relaunch on miss
   with backoff, one line per action to log/ipp-watchdog.out.
    Run detached: python log/launch_detached.py powershell.exe log/ipp-watchdog.out log/ipp-watchdog.err -ExecutionPolicy Bypass -File D:\_MyProject\SuperSoft\IPProxyPool\tools\ipp_watchdog.ps1
   Autostart at boot (TEMPLATE, register manually once as admin):
     schtasks /create /tn IPProxyWatchdog /tr "powershell -ExecutionPolicy Bypass -File D:\_MyProject\SuperSoft\IPProxyPool\tools\ipp_watchdog.ps1" /sc minute /mo 5 /ru SYSTEM
   Stop: kill the powershell process running this file + schtasks /delete /tn IPProxyWatchdog /f
#>
$ErrorActionPreference = "Continue"
$PY = "D:\DevSoft\Conda\Miniconda3\python.exe"
$Log = "log/ipp-watchdog.out"
$backoff = 5
# 连续不健康计数：进程在但探针不过（启动中/假死）先等，满 3 轮才重拉，
# 避免启动窗口内重复拉起造成 :8916 争抢（C1 live 抓获重复 PID，见 EXEC）。
$UnhealthyStrikes = 0

function Test-Port($Url) {
    try { return curl.exe --max-time 5 -s -o NUL -w "%{http_code}" $Url }
    catch { return "000" }
}

function Write-Log($msg) {
    $line = "[$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')] $msg"
    Write-Output $line
    Add-Content -LiteralPath $Log -Value $line
}

# C1 硬化：$PSScriptRoot 解析仓库根（经相对路径从异 cwd 启动时
# $MyInvocation.MyCommand.Path 会锚错根，致日志写飞地＋重拉路径错）。
$ScriptDir = $PSScriptRoot
if (-not $ScriptDir) { $ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path }
$RepoRoot = Split-Path -Parent $ScriptDir
Set-Location -LiteralPath $RepoRoot | Out-Null
$Log = Join-Path $RepoRoot "log/ipp-watchdog.out"
Write-Log "watchdog started (cwd=$(Get-Location))"

function Test-GatewayIdentity {
    # 2026-09-24 起网关迁 8916（:8080 让给 cvat traefik）；只认“200＋mock 包体”。
    # 写法注记：旧版 try/catch＋-match 单行 return 在本机 PS5.1 报
    # UnexpectedToken（逐段二分定位到该函数，改直列式后解析通过，语义等价）。
    $code = Test-Port "http://127.0.0.1:8916/"
    if ($code -ne "200") {
        return $code
    }
    $body = curl.exe --max-time 5 -s http://127.0.0.1:8916/
    if ($body -like "*mock-*") {
        return "200"
    }
    return "404-foreign"
}

function Start-Detached-Gateway {
    # 网关路径用字面量内联：变量形式（$GW/$GwExe）在本机 PS5.1 前台/DETACHED
    # 运行时求值为空（Get-Variable 查无此变量；赋值语句字节级正常，原因未明，
    # 已逐字节 hex 核对＋哈希对齐；改内联后 live 重拉连续出 PID，见 EXEC C1 条）。
    # 无参形态：连 @args 转发一并省掉，杜绝收参移位类问题。
    Write-Log "relaunch enter"
    & $PY log/launch_detached.py ".\gateway\target\debug\pingora-proxy-gateway.exe" "log/gw.out" "log/gw.err"
    Write-Log "relaunch exit"
}

while ($true) {
    $ProbeGw = Test-GatewayIdentity
    $ProbeMetrics = Test-Port "http://127.0.0.1:9091/metrics"
    if (($ProbeGw -eq "200") -and ($ProbeMetrics -eq "200")) {
        $backoff = 5
        $UnhealthyStrikes = 0
        Start-Sleep 30
        continue
    }
    Write-Log "gateway unhealthy"
    Start-Sleep $backoff
    $LiveProc = Get-Process -Name "pingora-proxy-gateway" -ErrorAction SilentlyContinue
    if ($LiveProc) {
        $UnhealthyStrikes = $UnhealthyStrikes + 1
        if ($UnhealthyStrikes -ge 3) {
            Write-Log "gateway wedged (3 strikes), relaunch"
            Start-Detached-Gateway
            $UnhealthyStrikes = 0
        }
    } else {
        Start-Detached-Gateway
        $UnhealthyStrikes = 0
    }
    $backoff = [Math]::Min($backoff * 2, 120)
    Start-Sleep 10
}
