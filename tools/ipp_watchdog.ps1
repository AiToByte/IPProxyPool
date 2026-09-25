<# IPProxyPool watchdog: process-outside keeper (Phase C1).
   The in-process supervisor cannot revive a dead process (Windows orderly
   exit ~every 5 min); this loop does: probe :8916/:9091, relaunch on miss
   with backoff, one line per action to log/ipp-watchdog.out.
    Run detached: python log/launch_detached.py powershell.exe log/ipp-watchdog.out log/ipp-watchdog.err -ExecutionPolicy Bypass -File D:\_MyProject\SuperSoft\IPProxyPool\tools\ipp_watchdog.ps1
    Autostart at boot (TEMPLATE, register manually once as admin;
      in sync with tools/install_watchdog.ps1; needs admin, doc only):
      schtasks /create /tn IPProxyWatchdog /tr "powershell -ExecutionPolicy Bypass -File D:\_MyProject\SuperSoft\IPProxyPool\tools\ipp_watchdog.ps1" /sc onstart /ru SYSTEM /rl HIGHEST
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
    $f = Get-Item -LiteralPath $Log -ErrorAction SilentlyContinue
    if ($f -and ($f.Length -gt $LogMaxBytes)) {
        $bak = "$Log.1"
        Remove-Item -LiteralPath $bak -ErrorAction SilentlyContinue
        Rename-Item -LiteralPath $Log -NewName (Split-Path -Leaf $bak) -ErrorAction SilentlyContinue
    }
    Add-Content -LiteralPath $Log -Value $line
}

# C1 硬化：$PSScriptRoot 解析仓库根（经相对路径从异 cwd 启动时
# $MyInvocation.MyCommand.Path 会锚错根，致日志写飞地＋重拉路径错）。
$ScriptDir = $PSScriptRoot
if (-not $ScriptDir) { $ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path }
$RepoRoot = Split-Path -Parent $ScriptDir
Set-Location -LiteralPath $RepoRoot | Out-Null
$Log = Join-Path $RepoRoot "log/ipp-watchdog.out"
# OPT-R4 C10：看护自有日志轮转（>50MB 切 .1 只留 1 代；launch_detached 只在
# 启动时转，长活看护中途不转会撑 C 盘，此处每次写前检查）。
$LogMaxBytes = 52428800
# OPT-R4 C6 联动：重拉的网关子进程继承本进程 env——看护必须自备与 ipp.ps1
# 同源的凭据（.env 优先，缺省开发缺省），否则相对带密 Redis 全 NOAUTH
# （live 实锤：无 env 重拉的网关 200 照常但遥测/泵/订阅全挂）。
$EnvFile = Join-Path $RepoRoot ".env"
if ($EnvFile -and (Test-Path -LiteralPath $EnvFile)) {
    foreach ($line in (Get-Content -LiteralPath $EnvFile)) {
        $t = $line.Trim()
        if ($t -eq "" -or $t.StartsWith("#")) { continue }
        $kv = $t -split "=", 2
        if ($kv.Count -eq 2 -and $kv[0].Trim() -ne "" -and
            ($null -eq (Get-Item -Path ("env:" + $kv[0].Trim()) -ErrorAction SilentlyContinue))) {
            Set-Item -Path ("env:" + $kv[0].Trim()) -Value $kv[1].Trim()
        }
    }
}
if (-not $env:REDIS_PASSWORD) { $env:REDIS_PASSWORD = "123456" }
if (-not $env:REDIS_URL) { $env:REDIS_URL = "redis://:$($env:REDIS_PASSWORD)@127.0.0.1:6379/" }
if (-not $env:CLICKHOUSE_USER) { $env:CLICKHOUSE_USER = "proxy" }
if (-not $env:CLICKHOUSE_PASSWORD) { $env:CLICKHOUSE_PASSWORD = "123456" }
if (-not $env:CLICKHOUSE_DB) { $env:CLICKHOUSE_DB = "proxy" }
Write-Log "watchdog started (cwd=$(Get-Location))"

function Test-GatewayIdentity {
    # 2026-09-24 起网关迁 8916（:8080 让给 cvat traefik）；只认“200＋mock 包体”。
    # 写法注记：直列式（本机 PS5.1 无 BOM＋LF＋中文文件曾报 try/catch 版
    # UnexpectedToken，根因为缺 BOM 致误解析，见 EXEC；全仓 .ps1 已补 BOM，
    # 此处保持可解析的直列形态不再改回，语义等价）。
    # D3：网关探针带开发 Key（无头 403 即判非我方，不会误杀；metrics 探针无门不动）。
    $code = curl.exe --max-time 5 -s -o NUL -w "%{http_code}" -H "X-Api-Key: default_key" http://127.0.0.1:8916/
    if ($code -ne "200") {
        return $code
    }
    $body = curl.exe --max-time 5 -s -H "X-Api-Key: default_key" http://127.0.0.1:8916/
    if ($body -like "*mock-*") {
        return "200"
    }
    return "404-foreign"
}

function Start-Detached-Gateway {
    # 网关路径用字面量内联：变量形式（$GW/$GwExe）在缺 BOM 误解析期间求值为空
    # （根因见 EXEC：LF 无 BOM＋中文致 PSParser 错位，与变量名无关；全仓补 BOM
    # 后未回退——内联形态经数十次 live 重拉验证，零改动风险，保留）。
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
