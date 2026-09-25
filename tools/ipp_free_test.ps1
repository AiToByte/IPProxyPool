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
& curl.exe --noproxy '*' --max-time 40 -s -o $bodyFile -w "HTTP:%{http_code} size:%{size_download}" @hargs "http://127.0.0.1:8916/ip"
Write-Output ""
$body = Get-Content $bodyFile -Raw -ErrorAction SilentlyContinue
Write-Output "body: $($body.Substring(0, [Math]::Min(120, $body.Length)))"

# 4. verdict
$G = $body
$verdict = "CLEAN"
if ($G -match [regex]::Escape($V)) { $verdict = "SUSPECT-VPN (G==V, re-run triple test)" }
elseif ($G -match [regex]::Escape($D)) { $verdict = "TRANSPARENT-or-DIRECT (G==D)" }
Write-Output "verdict: $verdict (D=$D V=$V)"

# 5. CH corroboration (best effort, 15s pump window)
Start-Sleep 15
$rows = curl.exe --max-time 10 -s "http://127.0.0.1:8123/" --user "proxy:123456" --data-binary "SELECT event_time, provider, out_ip, status_code FROM proxy.proxy_telemetry_log WHERE provider LIKE 'free-%' ORDER BY event_time DESC LIMIT 2"
Write-Output "ch-free-rows:"; Write-Output $rows
