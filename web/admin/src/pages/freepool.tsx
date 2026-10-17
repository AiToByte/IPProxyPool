"use client";

import { useEffect, useState, useCallback } from "react";
import {
  ShieldAlert,
  TrendingUp,
  Activity,
  Network,
  AlertCircle,
  CheckCircle2,
} from "lucide-react";
import { AdminApiClient, AutotuneStatus, FreePoolSnapshot, FreePoolSourceStat } from "@/hooks/useAdminApi";

function percent(n: number, d: number): string {
  if (!d || d <= 0) return "—";
  return ((n / d) * 100).toFixed(1) + "%";
}

function avgMs(sum: number, count: number): string {
  if (!count || count <= 0) return "—";
  return (sum / count).toFixed(0) + " ms";
}

function StatCard({
  title,
  value,
  sub,
  icon: Icon,
  color,
}: {
  title: string;
  value: string | number;
  sub?: string;
  icon: React.ElementType;
  color: "primary" | "success" | "warning" | "destructive";
}) {
  const colorClasses = {
    primary: "text-primary border-primary/20 bg-primary/5",
    success: "text-green-500 border-green-500/20 bg-green-500/5",
    warning: "text-amber-500 border-amber-500/20 bg-amber-500/5",
    destructive: "text-destructive border-destructive/20 bg-destructive/5",
  };
  return (
    <div className={`rounded-lg border p-4 ${colorClasses[color]}`}>
      <div className="flex items-center justify-between">
        <div>
          <p className="text-xs text-muted-foreground">{title}</p>
          <p className="text-xl font-bold">{value}</p>
          {sub && <p className="text-xs text-muted-foreground">{sub}</p>}
        </div>
        <Icon className={`w-6 h-6 ${colorClasses[color]}`} />
      </div>
    </div>
  );
}

export function FreePool({ client }: { client: AdminApiClient }) {
  const [snapshot, setSnapshot] = useState<FreePoolSnapshot | null>(null);
  const [sources, setSources] = useState<FreePoolSourceStat[]>([]);
  const [auto, setAuto] = useState<AutotuneStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const [snap, src, at] = await Promise.all([
        client.freePoolSummary(),
        client.freePoolSources(),
        client.autotuneStatus(),
      ]);
      setSnapshot(snap);
      setSources(src);
      setAuto(at);
    } catch (e: any) {
      setError(e.message);
    } finally {
      setLoading(false);
    }
  }, [client]);

  useEffect(() => { load(); }, [load]);

  if (loading) {
    return (
      <div className="flex h-[400px] items-center justify-center">
        <Activity className="w-8 h-8 animate-spin" />
      </div>
    );
  }

  if (error) {
    return (
      <div className="p-6 text-destructive text-sm">
        加载失败：{error}
      </div>
    );
  }

  const s = snapshot!;
  const totalYield = s.sources.reduce((a, x) => a + x.yield_total, 0);
  const totalElite = s.sources.reduce((a, x) => a + x.elite_total, 0);
  const verifyPass = s.verify.find((v) => v.result === "pass")?.count ?? 0;
  const verifyTotal =
    s.verify.reduce((a, x) => a + x.count, 0);
  const successRate = percent(verifyPass, verifyTotal);

  return (
    <div className="space-y-6">
      {/* 顶部概览 */}
      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-4 gap-4">
        <StatCard
          title="活跃免费节点"
          value={s.nodes_total}
          sub={s.dry ? "干旱中（无可用节点）" : "正常供给"}
          icon={Network}
          color={s.dry ? "destructive" : "success"}
        />
        <StatCard
          title="总抓取产出"
          value={totalYield.toLocaleString()}
          sub="免费源抓取节点累计"
          icon={TrendingUp}
          color="primary"
        />
        <StatCard
          title="Elite 节点"
          value={totalElite.toLocaleString()}
          sub={percent(totalElite, totalYield) + " 占比"}
          icon={ShieldAlert}
          color={totalElite > 0 ? "success" : "warning"}
        />
        <StatCard
          title="整体验证成功率"
          value={successRate}
          sub={verifyTotal ? `基于 ${verifyTotal} 次质检` : "无质检数据"}
          icon={CheckCircle2}
          color="primary"
        />
      </div>

      {/* 干旱预警 */}
      {s.dry && (
        <div className="rounded-lg border border-destructive/50 bg-destructive/5 p-4">
          <div className="flex items-center gap-2">
            <AlertCircle className="w-5 h-5 text-destructive" />
            <span className="font-semibold text-destructive">
              免费池干旱预警：当前无可用免费节点，建议关注付费出口质量。
            </span>
          </div>
        </div>
      )}

      {/* 自动调权 */}
      {auto && (
        <div className="rounded-lg border border-border p-4">
          <div className="flex items-center justify-between mb-3">
            <h3 className="text-xs font-semibold">自动调权（免费池干旱保护）</h3>
            <span className={`text-xs font-mono ${auto.enabled ? "text-success" : "text-muted-foreground"}`}>
              {auto.enabled ? "启用" : "禁用"}
            </span>
          </div>
          <div className="space-y-3">
            <div className="flex items-center justify-between">
              <span className="text-sm">付费权重上调</span>
              <button
                onClick={() => {
                  const next = !auto.enabled;
                  client.autotuneUpdate({ enabled: next }).then((r) => setAuto(r));
                }}
                className="px-3 py-1 text-xs rounded border border-input bg-input hover:bg-accent"
              >
                {auto.enabled ? "关闭" : "开启"}
              </button>
            </div>
            <div className="flex items-center gap-2">
              <label className="text-xs text-muted-foreground">上调因子 (%)</label>
              <input
                type="number"
                min="1"
                max="500"
                value={auto.boost_percent}
                onChange={(e) => {
                  const v = parseInt(e.target.value, 10) || 100;
                  client.autotuneUpdate({ boost_percent: v }).then((r) => setAuto(r));
                }}
                className="w-24 px-2 py-1 bg-input border border-input rounded-md text-sm focus:outline-none focus:ring-2 focus:ring-ring"
              />
              <span className="text-xs text-muted-foreground">100＝满权</span>
            </div>
            <div className={`flex items-center gap-2 text-xs ${auto.dry ? "text-destructive" : "text-success"}`}>
              {auto.dry ? (
                <>
                  <AlertCircle className="w-3 h-3" />
                  <span>免费池干旱：已自动上调付费节点权重</span>
                </>
              ) : (
                <>
                  <CheckCircle2 className="w-3 h-3" />
                  <span>免费池供给正常</span>
                </>
              )}
            </div>
          </div>
        </div>
      )}

      <div className="grid grid-cols-1 lg:grid-cols-3 gap-6">
        {/* 各源明细表 */}
        <div className="lg:col-span-2 rounded-lg border border-border">
          <div className="flex items-center justify-between border-b border-border bg-muted/50 px-3 py-2 text-sm font-medium">
            <span>免费源明细</span>
            <span className="text-muted-foreground font-normal">
              {s.sources.length} 个源
            </span>
          </div>
          <div className="overflow-x-auto">
            <table className="w-full text-sm text-left">
              <thead className="bg-muted/30">
                <tr>
                  <th className="p-3 font-medium">源名称</th>
                  <th className="p-3 font-medium text-right">产出</th>
                  <th className="p-3 font-medium text-right">Elite</th>
                  <th className="p-3 font-medium text-right">Elite 占比</th>
                  <th className="p-3 font-medium text-right">抓取次数</th>
                  <th className="p-3 font-medium text-right">平均抓取耗时</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-border">
                {sources.length > 0 ? (
                  sources.map((x) => (
                    <tr key={x.source} className="hover:bg-muted/50">
                      <td className="p-3 font-mono text-xs">{x.source}</td>
                      <td className="p-3 text-right">{x.yield_total.toLocaleString()}</td>
                      <td className="p-3 text-right">{x.elite_total.toLocaleString()}</td>
                      <td className="p-3 text-right">{percent(x.elite_total, x.yield_total)}</td>
                      <td className="p-3 text-right">{x.fetch_count.toLocaleString()}</td>
                      <td className="p-3 text-right">{avgMs(x.fetch_ms_sum, x.fetch_count)}</td>
                    </tr>
                  ))
                ) : (
                  <tr>
                    <td colSpan={6} className="p-4 text-center text-muted-foreground">
                      暂无源产出数据
                    </td>
                  </tr>
                )}
              </tbody>
            </table>
          </div>
        </div>

        {/* 右侧分布卡片 */}
        <div className="space-y-4">
          <div className="rounded-lg border border-border p-4">
            <h3 className="text-xs font-semibold mb-3">按出站协议分布</h3>
            <div className="space-y-2">
              {s.by_proto.length > 0 ? (
                s.by_proto.map((x) => (
                  <div key={x.proto} className="flex items-center justify-between text-sm">
                    <span className="text-muted-foreground">{x.proto}</span>
                    <span className="font-mono">{x.count.toLocaleString()}</span>
                  </div>
                ))
              ) : (
                <p className="text-xs text-muted-foreground">无数据</p>
              )}
            </div>
          </div>

          <div className="rounded-lg border border-border p-4">
            <h3 className="text-xs font-semibold mb-3">匿名度分级</h3>
            <div className="space-y-2">
              {s.anonymity.length > 0 ? (
                s.anonymity.map((x) => (
                  <div key={x.level} className="flex items-center justify-between text-sm">
                    <span className="text-muted-foreground capitalize">{x.level}</span>
                    <span className="font-mono">{x.count.toLocaleString()}</span>
                  </div>
                ))
              ) : (
                <p className="text-xs text-muted-foreground">无数据</p>
              )}
            </div>
          </div>

          <div className="rounded-lg border border-border p-4">
            <h3 className="text-xs font-semibold mb-3">质检结果</h3>
            <div className="space-y-2">
              {s.verify.length > 0 ? (
                s.verify.map((x) => (
                  <div key={x.result} className="flex items-center justify-between text-sm">
                    <span className="text-muted-foreground capitalize">{x.result}</span>
                    <span className="font-mono">{x.count.toLocaleString()}</span>
                  </div>
                ))
              ) : (
                <p className="text-xs text-muted-foreground">无数据</p>
              )}
            </div>
          </div>

          <div className="rounded-lg border border-border p-4">
            <h3 className="text-xs font-semibold mb-3">熔断与降级</h3>
            <div className="space-y-2">
              {s.suspended.length > 0 ? (
                s.suspended.map((x) => (
                  <div key={x.source} className="flex items-center justify-between text-sm">
                    <span className="text-muted-foreground">{x.source}</span>
                    <span className="text-destructive font-mono">{x.suspended === 1 ? "熔断" : "正常"}</span>
                  </div>
                ))
              ) : (
                <p className="text-xs text-muted-foreground">无熔断源</p>
              )}
              <div className="pt-2 border-t border-border">
                <div className="flex items-center justify-between text-sm">
                  <span className="text-muted-foreground">进检截断丢弃</span>
                  <span className="font-mono">{s.intake_capped.toLocaleString()}</span>
                </div>
                <div className="flex items-center justify-between text-sm">
                  <span className="text-muted-foreground">基线不可达轮次</span>
                  <span className="font-mono">{s.baseline_fail.toLocaleString()}</span>
                </div>
                <div className="flex items-center justify-between text-sm">
                  <span className="text-muted-foreground">容量淘汰累计</span>
                  <span className="font-mono">{s.evicted.toLocaleString()}</span>
                </div>
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
