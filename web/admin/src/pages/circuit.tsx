"use client";

import { useEffect, useState, useCallback } from "react";
import {
  ShieldAlert,
  TrendingUp,
  Activity,
  Database,
  AlertCircle,
} from "lucide-react";
import { AdminApiClient, BreakerSnapshot } from "@/hooks/useAdminApi";

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
          <p className="text-2xl font-bold">{value}</p>
          {sub && <p className="text-xs text-muted-foreground">{sub}</p>}
        </div>
        <Icon className={`w-6 h-6 ${colorClasses[color]}`} />
      </div>
    </div>
  );
}

export function Circuit({ client }: { client: AdminApiClient }) {
  const [snap, setSnap] = useState<BreakerSnapshot | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const s = await client.circuitSnapshot();
      setSnap(s);
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
    return <div className="p-6 text-destructive text-sm">加载失败：{error}</div>;
  }

  const s = snap!;
  const totalTrigger = s.breaker_trigger.reduce((a, x) => a + x.count, 0);

  return (
    <div className="space-y-6">
      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-4 gap-4">
        <StatCard
          title="隔离域数（外层）"
          value={s.quarantine_domains}
          sub="按目标域名归一的隔离条目域数"
          icon={Database}
          color={s.quarantine_domains > 0 ? "warning" : "success"}
        />
        <StatCard
          title="隔离节点数（内层）"
          value={s.quarantine_nodes}
          sub="被隔离的 domain:ip 组合总数"
          icon={ShieldAlert}
          color={s.quarantine_nodes > 0 ? "warning" : "success"}
        />
        <StatCard
          title="分档强制探索次数"
          value={s.bandit_exploration.toLocaleString()}
          sub="LinUCB tier_quota 强制选路"
          icon={TrendingUp}
          color="primary"
        />
        <StatCard
          title="熔断触发总数"
          value={totalTrigger.toLocaleString()}
          sub="429/403/502/504 触发隔离累计"
          icon={AlertCircle}
          color={totalTrigger > 0 ? "destructive" : "success"}
        />
      </div>

      <div className="rounded-lg border border-border">
        <div className="flex items-center justify-between border-b border-border bg-muted/50 px-3 py-2 text-sm font-medium">
          <span>熔断触发明细（按状态码）</span>
          <span className="text-muted-foreground font-normal">
            {s.breaker_trigger.length} 种状态码
          </span>
        </div>
        <div className="overflow-x-auto">
          <table className="w-full text-sm text-left">
            <thead className="bg-muted/30">
              <tr>
                <th className="p-3 font-medium">状态码</th>
                <th className="p-3 font-medium text-right">触发次数</th>
                <th className="p-3 font-medium text-right">含义</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-border">
              {s.breaker_trigger.length > 0 ? (
                s.breaker_trigger.map((x) => (
                  <tr key={x.status} className="hover:bg-muted/50">
                    <td className="p-3 font-mono text-sm">{x.status}</td>
                    <td className="p-3 text-right">{x.count.toLocaleString()}</td>
                    <td className="p-3 text-sm">
                      {x.status === "429" ? "速率限制（软隔离 60s）" : ""}
                      {x.status === "403" ? "WAF/封禁（深度隔离 600s）" : ""}
                      {x.status === "502" ? "上游故障（冷却 30s）" : ""}
                      {x.status === "504" ? "上游超时（冷却 30s）" : ""}
                    </td>
                  </tr>
                ))
              ) : (
                <tr>
                  <td colSpan={3} className="p-4 text-center text-muted-foreground">
                    暂无熔断触发
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </div>

      <div className="rounded-lg border border-border p-4">
        <h3 className="text-xs font-semibold mb-3">说明</h3>
        <ul className="text-sm text-muted-foreground space-y-2 list-disc pl-5">
          <li>
            熔断触发后，目标域在隔离表中锁定对应时长（429→60s／403→600s／502,504→30s），
            期间该域的节点不再被选路。
          </li>
          <li>
            隔离先在内存表写入（毫秒级），再持久化到 Redis 并通过 PubSub 扇出到其他网关实例。
          </li>
          <li>分档强制探索用于保证 free 等档位在低流量下仍可被持续评估。</li>
        </ul>
      </div>
    </div>
  );
}
