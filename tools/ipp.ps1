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

# OPT-R4 C5/C6：加载仓库根 .env（若存在）导出凭据；已有进程 env 优先（CI/显式
# 导出胜过 .env）；缺失则沿用开发缺省（与 compose :-fallback 一致）。
function Import-DotEnv($Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return }
    foreach ($line in (Get-Content -LiteralPath $Path)) {
        $t = $line.Trim()
        if ($t -eq "" -or $t.StartsWith("#")) { continue }
        $kv = $t -split "=", 2
        if ($kv.Count -eq 2) {
            $name = $kv[0].Trim()
            if ($name -ne "" -and ($null -eq (Get-Item -Path ("env:" + $name) -ErrorAction SilentlyContinue))) {
                Set-Item -Path ("env:" + $name) -Value $kv[1].Trim()
            }
        }
    }
}
$Here = $PSScriptRoot
if (-not $Here) { $Here = Split-Path -Parent $MyInvocation.MyCommand.Path }
Import-DotEnv (Join-Path (Split-Path -Parent $Here) ".env")
if (-not $env:REDIS_PASSWORD) { $env:REDIS_PASSWORD = "123456" }
if (-not $env:REDIS_URL) { $env:REDIS_URL = "redis://:$($env:REDIS_PASSWORD)@127.0.0.1:6379/" }
if (-not $env:CLICKHOUSE_USER) { $env:CLICKHOUSE_USER = "proxy" }
if (-not $env:CLICKHOUSE_PASSWORD) { $env:CLICKHOUSE_PASSWORD = "123456" }
if (-not $env:CLICKHOUSE_DB) { $env:CLICKHOUSE_DB = "proxy" }

function Test-Port($Url) {
    try {
        return curl.exe --max-time 5 -s -o NUL -w "%{http_code}" $Url
    } catch {
        return "000"
    }
}

# D3：网关默认开 Key 门——网关探针一律带开发 Key（mocks/metrics 无门，走 Test-Port）。
$GWKEY = "default_key"
function Test-GwPort($Url) {
    try {
        return curl.exe --max-time 5 -s -o NUL -w "%{http_code}" -H "X-Api-Key: $GWKEY" $Url
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
    # OPT-R16 E3：原实现是纯 `@args` 转发（无 param 块），调用点只能靠位置
    # 猜含义。现给出显式签名，要点：
    #  1. Exe/OutFile/ErrFile 必填且具名——这三个是固定契约，具名可读且
    #     防错位（原先传反了不会报错，只会写到错误的日志文件）。
    #  2. 尾部改用**具名数组** `$Rest` 而非 `ValueFromRemainingArguments`。
    #     这不是风格偏好，是被 PSSA 逼出来的：那条规则在**调用方**报错，
    #     而「任意多个透传参数」用剩余参数只能按位置传，等于无解。改成显式
    #     数组参数后，调用点写 `-Rest @(...)`，既过门又保住变长契约。
    param(
        [Parameter(Mandatory = $true)]
        [string]$Exe,
        [Parameter(Mandatory = $true)]
        [string]$OutFile,
        [Parameter(Mandatory = $true)]
        [string]$ErrFile,
        [Parameter()]
        # 允许空数组但**不能省略**：网关启动就是「无尾部」的调用。若标
        # Mandatory=$true，PowerShell 会抛 EmptyArrayNotAllowed（该约束是
        # 「必须传值」，空数组不算值）——本轮实测踩到，会让网关启动路径直接失败。
        # 显式写 -Rest @() 则是合法的「传了空数组」，故不标 Mandatory，
        # 但调用点仍显式传空数组以表明「尾部确实为空」而非「忘了传」。
        [AllowEmptyCollection()]
        [string[]]$Rest = @()
    )
    # 必须保持 `launch_detached.py` 的**变长** argv 契约：该脚本读
    # `sys.argv[4:]` 并原样透传给子进程，故 `$Rest` 原样展开，
    # 不补空串、不加壳（补空串会让 mock 上游收到多余空 argv 而错位）。
    & $PY log/launch_detached.py $Exe $OutFile $ErrFile @Rest
}

if ($Action -eq "status") {
    docker ps --format "{{.Names}} {{.Status}}" | Select-String "ipproxy"
    # OPT-R8 A3：凭据走环境变量，不进 argv。
    #   redis-cli：REDISCLI_AUTH 是 redis-cli 官方专设的认证环境变量，设置后
    #     自动认证，命令行不再出现 -a <password>（否则同机任何用户读
    #     `Get-CimInstance Win32_Process` / `/proc/<pid>/cmdline` 即可拿到密码）。
    #   ClickHouse：改用 X-ClickHouse-User / X-ClickHouse-Key 请求头，
    #     头值不出现在 argv（`--user u:p` 会）。
    docker exec -e REDISCLI_AUTH="$env:REDIS_PASSWORD" ipproxy-redis redis-cli ping 2>$null
    curl.exe --max-time 10 -s "http://127.0.0.1:8123/ping" `
        -H "X-ClickHouse-User: $env:CLICKHOUSE_USER" `
        -H "X-ClickHouse-Key: $env:CLICKHOUSE_PASSWORD"
    Write-Output ""
    Show-Port "http://127.0.0.1:8888/" "mockA"
    Show-Port "http://127.0.0.1:8889/" "mockB"
    Show-Port "http://127.0.0.1:8890/" "mockC"
    Write-Output ("" + "gw:" + (Test-GwPort "http://127.0.0.1:8916/"))
    Show-Port "http://127.0.0.1:9091/metrics" "metrics"
    exit 0
}

if ($Action -eq "stop") {
    Get-Process -Name "pingora-proxy-gateway" -ErrorAction SilentlyContinue | Stop-Process -Force
    # OPT-R4 C4：单次 CIM 查询全部 python.exe，按 mock 脚本命令行精确匹配再杀
    # （旧写法逐进程 WMI 且易被误读为全杀；本机其他 Python 任务不受影响）。
    Get-CimInstance Win32_Process -Filter "Name='python.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -like "*mock_upstream.py*" } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    Write-Output "stopped (gateway+mocks)"
    exit 0
}

# start （网关子进程自动继承本脚本 env：REDIS_URL/CLICKHOUSE_* 已在顶部备好）
docker compose up -d
# OPT-R8 A3：同 status 分支——REDISCLI_AUTH 传 env，密码不入 argv。
docker exec -e REDISCLI_AUTH="$env:REDIS_PASSWORD" ipproxy-redis redis-cli ping 2>$null
# V1-verdict fix: fixed sleeps lose the readiness race on cold start;
# poll until 200 (or timeout) instead.
function Wait-Port($Url, $Name, $Tries = 12, $Keyed = $false) {
    # 无返回值（调用点无需接；接了反而吞打印，见 [void] 教训——要显示就别接）。
    # D3：$Keyed 为真时走带 Key 网关探针。
    for ($i = 1; $i -le $Tries; $i++) {
        if ($Keyed) { $code = Test-GwPort $Url } else { $code = Test-Port $Url }
        if ($code -eq "200") { Write-Output "$Name`:200 (ready after ${i}x5s)"; return }
        Start-Sleep 5
    }
    if ($Keyed) { $code = Test-GwPort $Url } else { $code = Test-Port $Url }
    Write-Output "$Name`:$code (not ready after ${Tries}x5s, see log/)"
}

if ((Test-GwPort "http://127.0.0.1:8916/") -eq "200") {
    Write-Output "gw already listening, skip launch"
} else {
    # OPT-R16 E3：Start-Detached 已改为具名参数；此处无变长尾部。
    Start-Detached -Exe $GW -OutFile "log/gw.out" -ErrFile "log/gw.err" -Rest @()
    [void](Wait-Port -Url "http://127.0.0.1:8916/" -Name "gw" -Tries 12 -Keyed $true)
}
if ($Mocks) {
    foreach ($m in @(@(8888, "mock-a-us", "mockA"), @(8889, "mock-b-jp", "mockB"), @(8890, "mock-c-gb", "mockC"))) {
        if ((Test-Port "http://127.0.0.1:$($m[0])/") -eq "200") {
            Write-Output "$($m[2]) already listening, skip"
        } else {
            # OPT-R16 E3：三个固定契约参数具名；变长尾部用 -Rest 数组显式传入。
            # 上游 mock 的 argv 契约：<script> <port> <keyed>（见 log/mock_upstream.py）。
            Start-Detached -Exe $PY -OutFile "log/$($m[2]).out" -ErrFile "log/$($m[2]).err" `
                -Rest @("log/mock_upstream.py", "$($m[0])", "$($m[1])")
            [void](Wait-Port -Url "http://127.0.0.1:$($m[0])/" -Name $m[2] -Tries 6)
        }
    }
}
Show-Port "http://127.0.0.1:9091/metrics" "metrics"
curl.exe --max-time 5 -s -o NUL -w "plain:%{http_code} " -H "X-Api-Key: default_key" http://127.0.0.1:8916/
curl.exe --max-time 5 -s -o NUL -w "nokey:%{http_code} " http://127.0.0.1:8916/
curl.exe --max-time 5 -s -o NUL -w "badkey:%{http_code}`n" -H "X-Api-Key: bad" http://127.0.0.1:8916/
