#Requires -Version 5.1
<#
.SYNOPSIS  IPProxyPool 备份脚本 (OPT-R4 C12).
.DESCRIPTION
  备份三具名卷关键数据到 ./backup/<stamp>/：
    - Redis：BGSAVE 后拷出 dump.rdb（内存快照：会话/隔离/遥测缓冲）
    - ClickHouse：BACKUP TABLE proxy.proxy_telemetry_log 到容器内文件再拷出
      （CH 已有 90 天 TTL，备份防误删/盘坏；恢复见 docs/OPERATION.md）
    - Grafana：provisioning 是代码（随仓），运行时改动（datasource 密码等）
      靠卷 tar 兜底
  恢复=逆操作（停服→拷回→起服），详见 OPERATION “备份恢复”节。
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File tools/backup.ps1
#>
param(
    [string]$OutDir = ""
)

$ErrorActionPreference = "Continue"
# 注记：本机执行宿主下 $PSScriptRoot/$MyInvocation 常为空（相对 -File 调用），
# $root 为空即回退到启动 cwd（调用方 workdir 即仓库根，见 C6 调用）；下文所有
# 路径判空后再用，避免 Test-Path 绑定空值报错。
$here = $PSScriptRoot
if (-not $here) { $here = Split-Path -Parent $MyInvocation.MyCommand.Path }
$root = Split-Path -Parent $here
if (-not $root) { $root = Get-Location }
Set-Location -LiteralPath $root | Out-Null

# 凭据与 ipp.ps1 同源（.env 优先，缺省开发缺省）。
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
if (-not $env:REDIS_PASSWORD) { $env:REDIS_PASSWORD = "123456" }
if (-not $env:CLICKHOUSE_USER) { $env:CLICKHOUSE_USER = "proxy" }
if (-not $env:CLICKHOUSE_PASSWORD) { $env:CLICKHOUSE_PASSWORD = "123456" }
if (-not $env:CLICKHOUSE_DB) { $env:CLICKHOUSE_DB = "proxy" }

$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$dst = if ($OutDir -ne "") { $OutDir } else { Join-Path $root ("backup\" + $stamp) }
# 注记：$dst 必须绝对路径（docker -v 相对路径会被当成具名卷，tar 进黑洞；
# 上轮冒烟实锤）。$root 为空（本机宿主 quirk）即回退启动 cwd。
if (-not [IO.Path]::IsPathRooted($dst)) {
    $dst = Join-Path (Get-Location).Path $dst
}
New-Item -ItemType Directory -Path $dst -Force | Out-Null
Write-Output ("backup dir: " + $dst)

# OPT-R8 B1：解析 compose **实际项目名**（原硬编码 `ipproxypool-gw-r1`）。
# 用 `-p other` 或设 `COMPOSE_PROJECT_NAME` 时，硬编码名会让 `docker run -v` 自动
# **创建空卷**（Docker 行为），tar 打包空目录后脚本仍打印 `ALL BACKUP DONE`
# ——即「静默备份空卷」。这里从实际容器反查真实卷名，杜绝该路径。
# 优先级：COMPOSE_PROJECT_NAME 环境变量 > 容器标签 com.docker.compose.project > 缺省。
$composeProject = $env:COMPOSE_PROJECT_NAME
if (-not $composeProject) {
    # 注记（本机两处实测踩坑，故不走 docker inspect -f 模板）：
    #   ① `inspect -f "{{ index .Config.Labels \"com.docker.compose.project\" }}"`
    #      在本机返回 0 行——PowerShell 双引号串里 `\"` 不构成对 docker 的转义；
    #   ② 简化模板 `{{.Config.Labels.com.docker.compose.project}}` 返回字面
    #      `<no value>`（键名含点被当嵌套路径）。
    # 改为全量 JSON + ConvertFrom-Json 直读标签（本机实测正确）。
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
Write-Output ("compose project: " + $composeProject)

# OPT-R8 B1：卷存在性前置校验——卷不存在时**立即失败**，不再让 docker 自动建空卷。
# 这是「静默备份空卷」的第一道闸（第二道是备份后的产物校验）。
#
# 判定方式说明（本机实测踩坑，故留注记）：**不用** `docker volume inspect` 的退出码
# 或输出——本机 Docker 对**存在**的卷 inspect 也会把告警写 stderr 并返回 exit=1
# （实测：existing → exit=1、missing → exit=1，两者无法区分），PowerShell 里
# `if (-not (docker volume inspect $v 2>$null))` 因输出非空恒为 false，闸门失效、
# 空卷照旧被创建。改用 `docker volume ls` 列出真实卷名后做**字符串比对**，
# 该路径无歧义。
$existingVolumes = @(docker volume ls --format "{{.Name}}")
foreach ($v in @($chVolume, $gfVolume)) {
    if ($existingVolumes -notcontains $v) {
        Write-Error ("volume not found: " + $v + " (compose project='" + $composeProject + "')")
        Write-Error "aborting: refusing to let docker auto-create an empty volume and 'back up' nothing"
        Write-Error ("existing volumes: " + ($existingVolumes -join ", "))
        exit 1
    }
}
Write-Output "volume precheck OK (ch=$chVolume, grafana=$gfVolume)"

# 1. Redis：BGSAVE 等后台写完，拷 dump.rdb。
# OPT-R8 A3：REDISCLI_AUTH 传 env，密码不入 argv。
docker exec -e REDISCLI_AUTH="$env:REDIS_PASSWORD" ipproxy-redis redis-cli BGSAVE 2>$null | Out-Null
# OPT-R8 B1：原为固定 Start-Sleep 3，可能拷到**上一版** dump.rdb（Redis 写
# temp-*.rdb 再 rename，3s 不足时拿到旧文件且无任何提示）。改为轮询
# rdb_bgsave_in_progress 直到归零，再加短暂 settle。
for ($i = 1; $i -le 30; $i++) {
    $inProgress = (docker exec -e REDISCLI_AUTH="$env:REDIS_PASSWORD" ipproxy-redis redis-cli info persistence 2>$null |
        Select-String "^rdb_bgsave_in_progress:(\d+)")
    if ($inProgress -and $inProgress.Matches[0].Groups[1].Value -eq "0") { break }
    Start-Sleep 1
}
Start-Sleep 1
docker cp ipproxy-redis:/data/dump.rdb (Join-Path $dst "redis-dump.rdb")
if (-not (Test-Path -LiteralPath (Join-Path $dst "redis-dump.rdb"))) {
    Write-Error "redis dump.rdb missing after docker cp — aborting"
    exit 1
}
Write-Output "redis dump.rdb done"

# 2. ClickHouse：在线 FREEZE 快照（File BACKUP 需服务端 backups.allowed_path
# 白名单，本镜像未配；FREEZE 无需改配置）＋数据卷 tar 全量兜底。
# 注记：同文件 $bk 类变量存储曾恒为空——根因为文件缺 BOM 致 PSParser 误解析
# （LF 无 BOM＋中文；全仓 .ps1 已补 BOM，见 EXEC）。本节保持内联表达式
# （经 live 验证），不再改回变量形态。
# OPT-R8 A3：clickhouse-client 走 CLICKHOUSE_PASSWORD 环境变量，不入 argv。
docker exec -e CLICKHOUSE_PASSWORD="$env:CLICKHOUSE_PASSWORD" ipproxy-clickhouse `
    clickhouse-client --user $env:CLICKHOUSE_USER --query "ALTER TABLE proxy.proxy_telemetry_log FREEZE"
# shadow 下混有 increment.txt 等非数字项，先过滤纯数字目录再取最大（冒烟实锤）。
$shadow = (docker exec ipproxy-clickhouse ls /var/lib/clickhouse/shadow/ | Where-Object { $_ -match '^\d+$' } | Sort-Object { [int]$_ } | Select-Object -Last 1)
if ($shadow) {
    $shadow = $shadow.Trim()
    docker cp ("ipproxy-clickhouse:/var/lib/clickhouse/shadow/" + $shadow) (Join-Path $dst "ch-telemetry-freeze")
    docker exec -e CLICKHOUSE_PASSWORD="$env:CLICKHOUSE_PASSWORD" ipproxy-clickhouse `
        clickhouse-client --user $env:CLICKHOUSE_USER --query "ALTER TABLE proxy.proxy_telemetry_log UNFREEZE WITH NAME '$shadow'"
} else {
    Write-Output "ch freeze skipped (no shadow dir)"
}
docker run --rm -v "${chVolume}:/src:ro" -v "${dst}:/dst" redis:7-alpine tar -cf /dst/clickhouse-data.tar -C /src .
Write-Output "clickhouse freeze+volume done"

# 3. Grafana 卷 tar（运行时改动兜底；provisioning 本体随仓）。
docker run --rm -v "${gfVolume}:/src:ro" -v "${dst}:/dst" redis:7-alpine tar -cf /dst/grafana-data.tar -C /src .
Write-Output "grafana volume tar done"

# OPT-R8 B1：产物校验（第二道闸）——备份「成功」的判据不是命令退出码，而是
# **产物里真有东西**。此前无任何校验，tar 空目录也算 done。
# 判据：tar 必须含预期条目数（CH 库至少有 store/ 与 metadata/；Grafana 至少有 grafana.db）。
function Assert-TarHasEntries($TarPath, $MinEntries, $Label) {
    if (-not (Test-Path -LiteralPath $TarPath)) {
        Write-Error ($Label + " archive missing: " + $TarPath + " — backup FAILED")
        exit 1
    }
    $size = (Get-Item -LiteralPath $TarPath).Length
    if ($size -lt 1024) {
        Write-Error ($Label + " archive suspiciously small (" + $size + " bytes) — likely an empty volume; backup FAILED")
        exit 1
    }
    $entries = @(docker run --rm -v "${TarPath}:/t:ro" redis:7-alpine sh -c "tar -tf /t 2>/dev/null | head -n 2000")
    if ($entries.Count -lt $MinEntries) {
        Write-Error ($Label + " archive has only " + $entries.Count + " entries (< " + $MinEntries + ") — likely an empty volume; backup FAILED")
        exit 1
    }
    Write-Output ($Label + " archive OK (" + $entries.Count + " entries, " + [math]::Round($size / 1KB, 1) + " KB)")
}
Assert-TarHasEntries (Join-Path $dst "clickhouse-data.tar") 3 "clickhouse"
Assert-TarHasEntries (Join-Path $dst "grafana-data.tar") 1 "grafana"

Write-Output "ALL BACKUP DONE (restore: see docs/OPERATION.md)"
