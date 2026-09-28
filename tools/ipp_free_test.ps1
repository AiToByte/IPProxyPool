<# IPProxyPool free-path baseline test (one-command regression).
   Runs D/V baselines, waits for pool, fires tier request, verdict vs D/V, CH check.
   All curl calls force --noproxy '*' (shell proxy env must never hijack local checks).
   Usage: powershell -ExecutionPolicy Bypass -File tools/ipp_free_test.ps1 [-Target http://httpbin.org/ip] [-Clash http://127.0.0.1:7890] [-PoolWaitSecs 300]
#>
param(
    [string]$Target = "http://httpbin.org/ip",
    [string]$Clash = "http://127.0.0.1:7890",
    [int]$PoolWaitSecs = 300
)

$ErrorActionPreference = "Continue"
function Fail($msg) { Write-Output "FAIL: $msg"; exit 1 }

# 1. D/V baselines
$D = curl.exe --noproxy '*' --max-time 15 -s $Target
if (-not $D) { Fail "direct baseline unreachable" }
Write-Output "D-direct: $D"
$V = curl.exe --max-time 15 -s -x $Clash $Target
if (-not $V) { Write-Output "WARN: clash unreachable, V unknown"; $V = "UNKNOWN" }
Write-Output "V-vpn: $V"

# 2. wait for pool (gauge + proto)
$deadline = (Get-Date).AddSeconds($PoolWaitSecs)
$proto = ""
while ((Get-Date) -lt $deadline) {
    # curl 多行输出是数组，-match 会变过滤器且不填 $Matches，必须先拼成单串。
    $m = (curl.exe --noproxy '*' --max-time 5 -s http://127.0.0.1:9091/metrics) -join "`n"
    if ($m -match 'free_pool_nodes_total (\d+)') {
        $n = [int]$Matches[1]
        if ($n -ge 1) {
            if ($m -match 'by_proto\{proto="http"\} ([1-9])') { $proto = "http" }
            elseif ($m -match 'by_proto\{proto="socks5"\} ([1-9])') { $proto = "socks5" }
            Write-Output "pool ready: total=$n proto=$proto"
            break
        }
    }
    Start-Sleep 20
}
if (-not $proto) { Fail "pool empty after ${PoolWaitSecs}s (normal for free pool, retry later)" }

# 3. fire via gateway (forced direct; D3: key gate on, send dev key)
$headers = @("X-Api-Key: default_key", "X-Proxy-Tier: free", "Host: httpbin.org")
if ($proto -eq "socks5") { $headers += "X-Proxy-Proto: socks5" }
$hargs = @()
foreach ($h in $headers) { $hargs += "-H"; $hargs += $h }
$bodyFile = Join-Path ([System.IO.Path]::GetTempPath()) "ipp_free_body.txt"
# OPT-R8 B3：记录 HTTP 状态码——判定需要它（502/503/超时必须判 UNKNOWN，
# 不能与「响应体里没有 V/D 特征」混为一谈；旧实现两者都落到初始值 CLEAN）。
$httpCode = curl.exe --noproxy '*' --max-time 40 -s -o $bodyFile -w "%{http_code}" @hargs "http://127.0.0.1:8916/ip"
Write-Output ""
$body = Get-Content $bodyFile -Raw -ErrorAction SilentlyContinue
$bodyLen = 0
if ($body) { $bodyLen = $body.Length }
Write-Output ("body: " + $(if ($body) { $body.Substring(0, [Math]::Min(120, $bodyLen)) } else { "<empty>" }))
Write-Output ("http_code: " + $httpCode)

# 4. verdict
#
# OPT-R8 B3：**四态判定，默认失败**。
#
# 旧实现的缺陷（P1）：`$verdict` 初始值是 `"CLEAN"`，而 PowerShell 的 `-match`
# 对 `$null`/空返回 `$false` —— 于是当网关请求**失败**（502/503/超时，$body 为空）
# 时，两个 if 都不命中，判定**停留在初始值 `CLEAN`**。
# 即「一个用来证明免费出口干净的脚本，把完全失败的请求判为干净通过」——
# 失败方向与安全方向相反。
#
# 修法：① 无法取到响应体 / 非 2xx → 判 `UNKNOWN` 并**非零退出**（默认不通过）；
#      ② `CLEAN` 只能在「拿到 2xx 响应体且既不等于 V 也不等于 D」时给出。
$verdict = "UNKNOWN"
$exitCode = 1
if ([string]::IsNullOrWhiteSpace($body)) {
    $verdict = "UNKNOWN (no response body — gateway request failed or timed out; NOT a clean result)"
} elseif ($httpCode -notmatch '^2\d\d$') {
    $verdict = "UNKNOWN (http $httpCode — gateway rejected/failed; NOT a clean result)"
} else {
    $G = $body
    # 注意 V 可能是 "UNKNOWN"（Clash 不可达时），此时 Escape("UNKNOWN") 会去匹配
    # 响应体里是否含字面 "UNKNOWN" —— 语义失真，故仅在 V/D 均取到真值时比对。
    if (($V -ne "UNKNOWN") -and ($G -match [regex]::Escape($V))) {
        $verdict = "SUSPECT-VPN (G==V, re-run triple test)"
        $exitCode = 1
    } elseif (($D -ne "UNKNOWN") -and ($G -match [regex]::Escape($D))) {
        $verdict = "TRANSPARENT-or-DIRECT (G==D)"
        $exitCode = 1
    } else {
        $verdict = "CLEAN"
        $exitCode = 0
    }
}
Write-Output "verdict: $verdict (D=$D V=$V)"
Write-Output "exit_code: $exitCode"

# 5. CH corroboration (best effort, 15s pump window)
# OPT-R8 A4：凭据改读 .env／环境变量（原为硬编码 `proxy:123456`，生产密码下
# 静默失败且把开发口令写死在仓里）。与同族 ipp.ps1/backup.ps1 同源加载。
$RepoRoot = Split-Path -Parent $PSScriptRoot
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
if (-not $env:CLICKHOUSE_USER) { $env:CLICKHOUSE_USER = "proxy" }
if (-not $env:CLICKHOUSE_PASSWORD) { $env:CLICKHOUSE_PASSWORD = "123456" }
Start-Sleep 15
# OPT-R8 A3：凭据走请求头，不进 argv（`--user u:p` 会把口令写进命令行）。
$rows = curl.exe --max-time 10 -s "http://127.0.0.1:8123/" `
    -H "X-ClickHouse-User: $env:CLICKHOUSE_USER" `
    -H "X-ClickHouse-Key: $env:CLICKHOUSE_PASSWORD" `
    --data-binary "SELECT event_time, provider, out_ip, status_code FROM proxy.proxy_telemetry_log WHERE provider LIKE 'free-%' ORDER BY event_time DESC LIMIT 2"
Write-Output "ch-free-rows:"; Write-Output $rows

exit $exitCode
