<#Requires -Version 5.1
<#
.SYNOPSIS  IPProxyPool Windows 一键启停 (USE-便捷落地 U3).
.DESCRIPTION
  start  : compose up → PONG/Ok → 网关(已监听则跳过) → mocks(可选 -Mocks) → 5 用例探活
  stop   : 精确杀网关/mocks 进程 (Get-Process -Name, 禁 CommandLine 模糊)
  status : 四容器 + 三 mocks + 网关 + 指标各一行 (只读, 不拉起)
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 status
  powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 start -Mocks
#>
param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("start", "stop", "status")]
    [string]$Action,
    [switch]$Mocks
)

$ErrorActionPreference = "Continue"
$PY = "D:\DevSoft\Conda\Miniconda3\python.exe"
$GW = ".\gateway\target\debug\pingora-proxy-gateway.exe"

function Test-Port($Url) {
    try {
        return curl.exe --max-time 5 -s -o NUL -w "%{http_code}" $Url
    } catch {
        return "000"
    }
}

function Show-Port($Url, $Name) {
    Write-Output ("" + $Name + ":" + (Test-Port $Url))
}

# 注意：形参禁叫 $Args（自动变量 $args 会遮蔽它，V1 实测 mock 因此裸跑交互式 Python 即退）。
# 本函数故意无形参：调用点位置实参全进自动 $args，原样转给 launcher。
function Start-Detached {
    & $PY log/launch_detached.py @args
}

if ($Action -eq "status") {
    docker ps --format "{{.Names}} {{.Status}}" | Select-String "ipproxy"
    docker exec ipproxy-redis redis-cli ping
    curl.exe --max-time 10 -s "http://127.0.0.1:8123/ping" --user "proxy:123456"
    Write-Output ""
    Show-Port "http://127.0.0.1:8888/" "mockA"
    Show-Port "http://127.0.0.1:8889/" "mockB"
    Show-Port "http://127.0.0.1:8890/" "mockC"
    Show-Port "http://127.0.0.1:8080/" "gw"
    Show-Port "http://127.0.0.1:9091/metrics" "metrics"
    exit 0
}

if ($Action -eq "stop") {
    Get-Process -Name "pingora-proxy-gateway" -ErrorAction SilentlyContinue | Stop-Process -Force
    Get-Process -Name "python" -ErrorAction SilentlyContinue | Where-Object {
        (Get-CimInstance Win32_Process -Filter ("ProcessId=" + $_.Id)).CommandLine -like "*mock_upstream.py*"
    } | Stop-Process -Force
    Write-Output "stopped (gateway+mocks)"
    exit 0
}

# start
docker compose up -d
docker exec ipproxy-redis redis-cli ping
# V1-verdict fix: fixed sleeps lose the readiness race on cold start;
# poll until 200 (or timeout) instead.
function Wait-Port($Url, $Name, $Tries = 12) {
    # 无返回值（调用点无需接；接了反而吞打印，见 [void] 教训——要显示就别接）。
    for ($i = 1; $i -le $Tries; $i++) {
        $code = Test-Port $Url
        if ($code -eq "200") { Write-Output "$Name`:200 (ready after ${i}x5s)"; return }
        Start-Sleep 5
    }
    $code = Test-Port $Url
    Write-Output "$Name`:$code (not ready after ${Tries}x5s, see log/)"
}

if ((Test-Port "http://127.0.0.1:8080/") -eq "200") {
    Write-Output "gw already listening, skip launch"
} else {
    Start-Detached $GW "log/gw.out" "log/gw.err"
    [void](Wait-Port "http://127.0.0.1:8080/" "gw")
}
if ($Mocks) {
    foreach ($m in @(@(8888, "mock-a-us", "mockA"), @(8889, "mock-b-jp", "mockB"), @(8890, "mock-c-gb", "mockC"))) {
        if ((Test-Port "http://127.0.0.1:$($m[0])/") -eq "200") {
            Write-Output "$($m[2]) already listening, skip"
        } else {
            Start-Detached $PY "log/$($m[2]).out" "log/$($m[2]).err" "log/mock_upstream.py" "$($m[0])" $m[1]
            [void](Wait-Port "http://127.0.0.1:$($m[0])/" $m[2] 6)
        }
    }
}
Show-Port "http://127.0.0.1:9091/metrics" "metrics"
curl.exe --max-time 5 -s -o NUL -w "plain:%{http_code} " http://127.0.0.1:8080/
curl.exe --max-time 5 -s -o NUL -w "badkey:%{http_code}`n" -H "X-Api-Key: bad" http://127.0.0.1:8080/
