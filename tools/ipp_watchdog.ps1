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

function Test-PortOwner {
    # OPT-R8 B2：判定「监听 :8916 的是不是我方网关」。
    #
    # 旧实现只认「200 + 响应体含 mock-」，而 `docs/OPERATION.md` 的真 Key 灰度
    # 流程第 2 步要求把 `mock-x` 换成真实 `ip:port` —— 换完后响应体永不含
    # `mock-`，`Test-GatewayIdentity` 恒返回 `404-foreign`，看护每 3 轮（约 90s）
    # 重拉一次进程，**无限重拉**（生产部署下即自杀式看护）。
    #
    # 改为客观事实判据：比对监听进程的可执行文件路径是否落在本仓 `target` 下。
    # 该判据与响应体内容**无关**，对 mock 与真供应商一视同仁。
    # 返回 $true=我方进程在监听；$false=未监听或非我方进程。
    #
    # 注记：按 PID 找进程所有者用 `Get-NetTCPConnection`——但该 cmdlet 在
    # Win11 上有卡死 20s+ 的实测记录（见 EXEC 步骤 2 教训），故此处改用
    # `netstat -ano` 文本解析（快且无副作用）。
    $lines = netstat -ano 2>$null | Select-String "LISTENING" | Select-String ":8916\s"
    if (-not $lines) { return $false }
    $pids = @()
    foreach ($l in $lines) {
        $parts = ($l -split "\s+") | Where-Object { $_ -ne "" }
        if ($parts.Count -ge 1) {
            $last = $parts[$parts.Count - 1]
            if ($last -match '^\d+$') { $pids += [int]$last }
        }
    }
    if ($pids.Count -eq 0) { return $false }
    foreach ($procId in $pids) {
        $p = Get-CimInstance Win32_Process -Filter "ProcessId=$procId" -ErrorAction SilentlyContinue
        if (-not $p) { continue }
        $exe = $p.ExecutablePath
        $cmd = $p.CommandLine
        $isOurs = ($exe -and $exe -like "*IPProxyPool*pingora-proxy-gateway.exe") -or
                  ($cmd -and $cmd -like "*IPProxyPool*pingora-proxy-gateway*")
        if ($isOurs) { return $true }
        Write-Log "port 8916 held by foreign pid=$procId exe=$exe"
    }
    return $false
}

function Test-GatewayIdentity {
    # OPT-R8 B2：两级判据，缺一不可。
    #   1) 端口归属：我方网关进程是否在监听 :8916（客观事实，见 Test-PortOwner）。
    #   2) HTTP 探活：带 D3 开发 Key 探测（无头 403 即判非我方，不会误杀）。
    #
    # `mock-` 前缀**降级为日志指纹**，不再作为存活判据——真供应商部署的响应体
    # 天然不含 mock，用它做判据会导致无限重拉（旧实现的根本缺陷）。
    # 返回：200=健康；其他=不健康（附原因，仅用于日志）。
    if (-not (Test-PortOwner)) {
        return "no-owner"
    }
    $code = curl.exe --max-time 5 -s -o NUL -w "%{http_code}" -H "X-Api-Key: default_key" http://127.0.0.1:8916/
    if ($code -ne "200") {
        return $code
    }
    # 日志指纹：mock 环境记 mock-，真供应商记非 mock（仅记录，不影响判定）。
    $body = curl.exe --max-time 5 -s -H "X-Api-Key: default_key" http://127.0.0.1:8916/
    if ($body -like "*mock-*") {
        Write-Log "identity ok (egress fingerprint: mock)"
    } else {
        Write-Log "identity ok (non-mock egress — real vendor pool or custom upstream)"
    }
    return "200"
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
