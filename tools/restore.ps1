#Requires -Version 5.1
<#
.SYNOPSIS  IPProxyPool 恢复演练脚本 (OPT-R5 O10, dry-run only).
.DESCRIPTION
  默认 dry-run：只打印将执行的恢复步骤，不做任何真实写入（不停服、
  不拷回、不起服、不查库）。备份来源见 tools/backup.ps1 产出 backup/<stamp>/
  （redis-dump.rdb＋ch-telemetry-freeze/clickhouse-data.tar＋grafana-data.tar）；
  恢复口径对应 docs/OPERATION.md “OPT-R4 C12 备份恢复”节；演练用副本表验行数，不碰生产表。
  -Execute 为预留开关：当前未实现，直接 exit 2（如实注释，待真实恢复落地后再实现）。
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File tools/restore.ps1
  powershell -ExecutionPolicy Bypass -File tools/restore.ps1 -BackupDir backup\20260925-120000
  powershell -ExecutionPolicy Bypass -File tools/restore.ps1 -Execute
#>
param(
    [string]$BackupDir = "",
    [switch]$Execute
)

$ErrorActionPreference = "Continue"

# 预留开关：真实写入尚未实现，如实 exit 2（不做任何副作用）。
if ($Execute) {
    Write-Output "restore --execute not implemented (dry-run only)"
    exit 2
}

$src = if ($BackupDir -ne "") { $BackupDir } else { "backup\<stamp>" }
Write-Output ("restore dry-run plan (src=" + $src + ", no writes):")
Write-Output ("1. stop: powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 stop ; docker compose stop")
Write-Output ("2. redis RDB copy back: docker cp " + $src + "\redis-dump.rdb ipproxy-redis:/data/dump.rdb")
Write-Output ("3. clickhouse freeze shadow / volume tar replay: docker cp " + $src + "\ch-telemetry-freeze ipproxy-clickhouse:/var/lib/clickhouse/shadow/ ; tar -xf clickhouse-data.tar to volume ; tar -xf grafana-data.tar to volume")
Write-Output ("4. start: docker compose up -d ; powershell -ExecutionPolicy Bypass -File tools/ipp.ps1 start")
Write-Output ("5. reconcile: SELECT count() on restored copy table vs backup manifest (copy table only, not prod)")
exit 0
