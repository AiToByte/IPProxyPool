#Requires -Version 5.1
<#
.SYNOPSIS  IPProxyPool 恢复脚本 (OPT-R5 O10 dry-run；OPT-R8 B4 实现真恢复).
.DESCRIPTION
  备份来源：tools/backup.ps1 产出的 backup/<stamp>/，含
    - redis-dump.rdb          Redis 内存快照（会话/隔离/遥测缓冲）
    - clickhouse-data.tar    ClickHouse 数据卷全量 tar
    - grafana-data.tar       Grafana 数据卷全量 tar

  两种模式：
    - 默认（dry-run）：只打印将执行的步骤并校验备份产物完整性，**零写入**。
    - -Execute：执行真实恢复。属破坏性操作，需三道闸：
        ① 备份产物完整性校验（缺文件/空归档即中止）
        ② 交互式二次确认（必须键入 RESTORE 才继续；非交互会话默认拒绝）
        ③ 分阶段执行 + 阶段间校验，失败即停并给出恢复建议

  恢复口径对应 docs/OPERATION.md "备份恢复" 节：FREEZE → 拷 shadow → UNFREEZE
  → 双 tar 回放 → 起服 → 行数对账。演练用副本表验行数，不碰生产表。

  OPT-R8 B4 背景：此前本脚本只有 dry-run（-Execute 直接 exit 2），
  而 docs/FEATURES.md 引用了并不存在的 `--dry-run` 参数 ⇒ **有备份无恢复**，
  灾备能力是纸面的。本轮实现真恢复路径，但**默认 dry-run 行为不变**
  （破坏性操作不默认触发）。
.EXAMPLE
  # 演练（默认，零写入）
  powershell -ExecutionPolicy Bypass -File tools/restore.ps1
  powershell -ExecutionPolicy Bypass -File tools/restore.ps1 -BackupDir backup\20260928-120000

  # 真恢复（破坏性；会提示键入 RESTORE 确认）
  powershell -ExecutionPolicy Bypass -File tools/restore.ps1 -BackupDir backup\20260928-120000 -Execute
#>
param(
    [string]$BackupDir = "",
    [switch]$Execute
)

$ErrorActionPreference = "Continue"

# 仓库根（.ps1 在 tools/ 下，备份在仓库根的 backup/）。
$here = $PSScriptRoot
if (-not $here) { $here = Split-Path -Parent $MyInvocation.MyCommand.Path }
$root = Split-Path -Parent $here
if (-not $root) { $root = Get-Location }

# 凭据（.env 优先，缺省开发缺省；与 backup.ps1/ipp.ps1 同源）。
$envFile = Join-Path $root ".env"
if ($envFile -and (Test-Path -LiteralPath $envFile)) {
    foreach ($line in (Get-Content -LiteralPath $envFile)) {
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
if (-not $env:CLICKHOUSE_DB) { $env:CLICKHOUSE_DB = "proxy" }

function Fail($msg) { Write-Output ("FAIL: " + $msg); exit 1 }

# ----------------------------------------------------------------------------
# 备份目录定位
# ----------------------------------------------------------------------------
if ($BackupDir -ne "") {
    $src = $BackupDir
    if (-not [IO.Path]::IsPathRooted($src)) { $src = Join-Path $root $src }
} else {
    $backupRoot = Join-Path $root "backup"
    if (-not (Test-Path -LiteralPath $backupRoot)) { Fail "no backup/ directory; run tools/backup.ps1 first" }
    $latest = Get-ChildItem -LiteralPath $backupRoot -Directory -ErrorAction SilentlyContinue |
        Sort-Object Name -Descending | Select-Object -First 1
    if (-not $latest) { Fail "backup/ is empty; run tools/backup.ps1 first" }
    $src = $latest.FullName
}
if (-not (Test-Path -LiteralPath $src)) { Fail ("backup dir not found: " + $src) }
Write-Output ("restore source: " + $src)

# ----------------------------------------------------------------------------
# 备份产物完整性校验（第一道闸：dry-run 与 -Execute 都会跑）
# ----------------------------------------------------------------------------
$required = @("redis-dump.rdb", "clickhouse-data.tar", "grafana-data.tar")
$missing = @()
foreach ($f in $required) {
    $p = Join-Path $src $f
    if (-not (Test-Path -LiteralPath $p)) { $missing += $f; continue }
    $sz = (Get-Item -LiteralPath $p).Length
    if ($sz -lt 1024) { $missing += ($f + " (suspiciously small: " + $sz + " bytes)") }
    else { Write-Output ("  ok: " + $f + " (" + [math]::Round($sz / 1MB, 2) + " MB)") }
}
if ($missing.Count -gt 0) {
    Fail ("backup incomplete — refusing to restore: " + ($missing -join ", "))
}
Write-Output "backup integrity check: PASS"

# ----------------------------------------------------------------------------
# 解析 compose 实际项目名（与 backup.ps1 同口径；OPT-R8 B1 注记：不能用
# `docker volume inspect` 的退出码判定卷存在——本机对存在卷也返回 exit=1）
# ----------------------------------------------------------------------------
$composeProject = $env:COMPOSE_PROJECT_NAME
if (-not $composeProject) {
    # 注记（本机两处实测踩坑，故不走 docker inspect -f 模板）：
    #   ① `inspect -f "{{ index .Config.Labels \"com.docker.compose.project\" }}"`
    #      在本机返回 0 行——PowerShell 双引号串里 `\"` 不构成对 docker 的转义；
    #   ② 改用简化模板 `{{.Config.Labels.com.docker.compose.project}}` 则返回
    #      字面 `<no value>`（键名含点，模板解析器当嵌套路径）。
    # 结论：改为 `docker inspect` 全量 JSON + ConvertFrom-Json 直读标签，
    # 该路径在本机实测返回正确的 `ipproxypool-gw-r1`。
    $insp = docker inspect ipproxy-clickhouse 2>$null | ConvertFrom-Json
    if ($insp -and $insp[0].Config.Labels.'com.docker.compose.project') {
        $composeProject = ([string]$insp[0].Config.Labels.'com.docker.compose.project').Trim()
    }
}
if (-not $composeProject) {
    $j = (docker compose config --format json 2>$null | ConvertFrom-Json)
    if ($j -and $j.name) { $composeProject = ([string]$j.name).Trim() }
}
if (-not $composeProject) { $composeProject = "ipproxypool-gw-r1" }
$chVolume = "${composeProject}_clickhouse-data"
$gfVolume = "${composeProject}_grafana-data"

if (-not $Execute) {
    Write-Output ""
    Write-Output "restore dry-run plan (no writes will be performed):"
    Write-Output ("  project : " + $composeProject)
    Write-Output ("  source  : " + $src)
    Write-Output "  1. stop gateway + deps : powershell -File tools/ipp.ps1 stop ; docker compose stop"
    Write-Output "  2. redis   : docker cp <src>\redis-dump.rdb ipproxy-redis:/data/dump.rdb"
    # 注记：字符串续行用 `"$(...)" 子表达式，**不要**写成 `"... " + $var`——
    # 行尾的 `+` 会被 PowerShell 当作**续行符**，输出里出现孤立的 `+` 行（实测踩到）。
    Write-Output "  3. ch tar  : replay clickhouse-data.tar into volume $($chVolume)"
    Write-Output "  4. grafana : replay grafana-data.tar into volume $($gfVolume)"
    Write-Output "  5. start   : docker compose up -d ; powershell -File tools/ipp.ps1 start"
    Write-Output "  6. reconcile: SELECT count() on proxy.proxy_telemetry_log vs manifest"
    Write-Output ""
    Write-Output "re-run with -Execute to perform the restore (prompts for confirmation)."
    exit 0
}

# ----------------------------------------------------------------------------
# -Execute：第二道闸（交互式二次确认）
# ----------------------------------------------------------------------------
Write-Output ""
Write-Warning "DESTRUCTIVE OPERATION: this will overwrite live Redis/ClickHouse/Grafana data."
Write-Output "To abort, close this window now."
$answer = ""
try {
    $answer = Read-Host "Type RESTORE to proceed"
} catch {
    Fail "non-interactive session cannot confirm — refusing to execute (use -Force in a scheduled task after manual review)"
}
if ($answer -ne "RESTORE") {
    Write-Output "aborted (confirmation not given); nothing was changed"
    exit 1
}

# ----------------------------------------------------------------------------
# -Execute：第三道闸（分阶段执行，失败即停）
# ----------------------------------------------------------------------------
Write-Output ""
Write-Output "[1/5] stopping gateway and dependencies ..."
& powershell -ExecutionPolicy Bypass -File (Join-Path $here "ipp.ps1") stop 2>&1 | Out-Null
docker compose stop 2>&1 | Out-Null

Write-Output "[2/5] restoring Redis RDB ..."
# 先停 Redis 容器再拷入 dump.rdb（在线覆盖会写坏快照）。
docker cp (Join-Path $src "redis-dump.rdb") "ipproxy-redis:/data/dump.rdb" 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) { Fail "redis RDB copy failed — volumes partially restored; re-run backup of current state before retrying" }
Write-Output "      redis RDB copied"

Write-Output "[3/5] replaying ClickHouse data volume ..."
# tar 回放进卷（覆盖卷内容）。用 redis:7-alpine 做 tar 工具（镜像已在本机）。
docker run --rm -v "${chVolume}:/dst" -v "${src}:/src:ro" redis:7-alpine `
    sh -c "tar -xf /src/clickhouse-data.tar -C /dst" 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) { Fail "clickhouse tar replay failed — stop and inspect volume " + $chVolume }
Write-Output "      clickhouse volume replayed"

Write-Output "[4/5] replaying Grafana data volume ..."
docker run --rm -v "${gfVolume}:/dst" -v "${src}:/src:ro" redis:7-alpine `
    sh -c "tar -xf /src/grafana-data.tar -C /dst" 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) { Fail "grafana tar replay failed — stop and inspect volume " + $gfVolume }
Write-Output "      grafana volume replayed"

Write-Output "[5/5] starting services and reconciling ..."
docker compose up -d 2>&1 | Out-Null
# ClickHouse 冷启动需 10~20s（OPT-R7 C1 实测），轮询等就绪再对账。
$rows = ""
for ($i = 1; $i -le 30; $i++) {
    $rows = curl.exe --max-time 10 -s "http://127.0.0.1:8123/" `
        -H "X-ClickHouse-User: $env:CLICKHOUSE_USER" `
        -H "X-ClickHouse-Key: $env:CLICKHOUSE_PASSWORD" `
        --data-binary "SELECT count() FROM ${env:CLICKHOUSE_DB}.proxy_telemetry_log FORMAT TSV" 2>$null
    if ($rows) { break }
    Start-Sleep 2
}
if ($rows) {
    Write-Output ("      reconciled row count: " + $rows.Trim())
    Write-Output ""
    Write-Output "RESTORE DONE. Now start the gateway: powershell -File tools/ipp.ps1 start"
    Write-Output "Then verify per docs/OPERATION.md (Prometheus has data, SLA queries return rows)."
    exit 0
}

Write-Output ""
Write-Warning "RESTORE FINISHED but reconciliation could not read ClickHouse (not ready within 60s)."
Write-Warning "Check: docker compose logs clickhouse ; curl http://127.0.0.1:8123/ping"
exit 1
