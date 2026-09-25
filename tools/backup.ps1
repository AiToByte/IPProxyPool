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

# 1. Redis：BGSAVE 等后台写完，拷 dump.rdb。
docker exec ipproxy-redis redis-cli -a "$env:REDIS_PASSWORD" BGSAVE 2>$null | Out-Null
Start-Sleep 3
docker cp ipproxy-redis:/data/dump.rdb (Join-Path $dst "redis-dump.rdb")
Write-Output "redis dump.rdb done"

# 2. ClickHouse：在线 FREEZE 快照（File BACKUP 需服务端 backups.allowed_path
# 白名单，本镜像未配；FREEZE 无需改配置）＋数据卷 tar 全量兜底。
# 注记：同文件 $bk 类变量存储曾恒为空——根因为文件缺 BOM 致 PSParser 误解析
# （LF 无 BOM＋中文；全仓 .ps1 已补 BOM，见 EXEC）。本节保持内联表达式
# （经 live 验证），不再改回变量形态。
docker exec ipproxy-clickhouse clickhouse-client --user $env:CLICKHOUSE_USER --password $env:CLICKHOUSE_PASSWORD --query "ALTER TABLE proxy.proxy_telemetry_log FREEZE"
# shadow 下混有 increment.txt 等非数字项，先过滤纯数字目录再取最大（冒烟实锤）。
$shadow = (docker exec ipproxy-clickhouse ls /var/lib/clickhouse/shadow/ | Where-Object { $_ -match '^\d+$' } | Sort-Object { [int]$_ } | Select-Object -Last 1)
if ($shadow) {
    $shadow = $shadow.Trim()
    docker cp ("ipproxy-clickhouse:/var/lib/clickhouse/shadow/" + $shadow) (Join-Path $dst "ch-telemetry-freeze")
    docker exec ipproxy-clickhouse clickhouse-client --user $env:CLICKHOUSE_USER --password $env:CLICKHOUSE_PASSWORD --query "ALTER TABLE proxy.proxy_telemetry_log UNFREEZE WITH NAME '$shadow'"
} else {
    Write-Output "ch freeze skipped (no shadow dir)"
}
docker run --rm -v ipproxypool-gw-r1_clickhouse-data:/src:ro -v "${dst}:/dst" redis:7-alpine tar -cf /dst/clickhouse-data.tar -C /src .
Write-Output "clickhouse freeze+volume done"

# 3. Grafana 卷 tar（运行时改动兜底；provisioning 本体随仓）。
# 卷用 compose 全名（project ipproxypool-gw-r1 前缀；裸名会建空卷，血泪注记）。
docker run --rm -v ipproxypool-gw-r1_grafana-data:/src:ro -v "${dst}:/dst" redis:7-alpine tar -cf /dst/grafana-data.tar -C /src .
Write-Output "grafana volume tar done"
Write-Output "ALL BACKUP DONE (restore: see docs/OPERATION.md)"
