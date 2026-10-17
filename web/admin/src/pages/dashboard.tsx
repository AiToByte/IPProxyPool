"use client";

import { useEffect, useState } from "react";
import { Activity, Users, Database, Network } from "lucide-react";
import { AdminApiClient, NodeView, TenantSummary } from "@/hooks/useAdminApi";

interface StatCardProps {
  title: string;
  value: string | number;
  icon: React.ElementType;
  color: "primary" | "success" | "warning" | "destructive";
}

function StatCard({ title, value, icon: Icon, color }: StatCardProps) {
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
          <p className="text-sm text-muted-foreground">{title}</p>
          <p className="text-2xl font-bold">{value}</p>
        </div>
        <Icon className={`w-6 h-6 ${colorClasses[color]}`} />
      </div>
    </div>
  );
}

export function Dashboard({ client }: { client: AdminApiClient }) {
  const [nodes, setNodes] = useState<NodeView[]>([]);
  const [tenants, setTenants] = useState<TenantSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    Promise.all([
      client.listNodes().then((n) => { if (!cancelled) setNodes(n); }),
      client.listTenants().then((t) => { if (!cancelled) setTenants(t); }),
    ])
      .finally(() => { if (!cancelled) setLoading(false); })
      .catch((e) => { if (!cancelled) setError(e.message); });
    return () => { cancelled = true; };
  }, [client]);

  const freeNodes = nodes.filter((n) => n.tier === "free").length;
  const paidNodes = nodes.length - freeNodes;
  const activeTenants = tenants.filter((t) => t.is_active).length;
  const totalBytes = tenants.reduce((sum, t) => sum + t.total_bytes, 0);

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

  return (
    <div className="space-y-6">
      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-4 gap-4">
        <StatCard title="节点总数" value={nodes.length} icon={Network} color="primary" />
        <StatCard title="付费节点" value={paidNodes} icon={Database} color="success" />
        <StatCard title="免费节点" value={freeNodes} icon={Network} color="warning" />
        <StatCard title="租户数" value={activeTenants} icon={Users} color="primary" />
      </div>

      <div className="grid grid-cols-1 lg:grid-cols-3 gap-6">
        <div className="bg-card border border-border rounded-lg p-6">
          <h3 className="text-sm font-medium mb-4">流量汇总</h3>
          <div className="space-y-3">
            <div className="flex justify-between text-sm">
              <span className="text-muted-foreground">总输出字节</span>
              <span className="font-mono text-sm">{totalBytes.toLocaleString()} B</span>
            </div>
            <div className="flex justify-between text-sm">
              <span className="text-muted-foreground">付费租户占比</span>
              <span className="font-mono text-sm">
                {tenants.length > 0 ? ((activeTenants / tenants.length) * 100).toFixed(1) : 0}%
              </span>
            </div>
          </div>
        </div>

        <div className="bg-card border border-border rounded-lg p-6">
          <h3 className="text-sm font-medium mb-4">节点分布</h3>
          <div className="space-y-3">
            <div className="flex justify-between text-sm">
              <span className="text-muted-foreground">付费节点</span>
              <span className="font-mono text-sm">{paidNodes}</span>
            </div>
            <div className="flex justify-between text-sm">
              <span className="text-muted-foreground">免费节点</span>
              <span className="font-mono text-sm">{freeNodes}</span>
            </div>
          </div>
        </div>

        <div className="bg-card border border-border rounded-lg p-6">
          <h3 className="text-sm font-medium mb-4">租户状态</h3>
          <div className="space-y-3">
            <div className="flex justify-between text-sm">
              <span className="text-muted-foreground">活跃租户</span>
              <span className="font-mono text-sm text-green-500">{activeTenants}</span>
            </div>
            <div className="flex justify-between text-sm">
              <span className="text-muted-foreground">禁用租户</span>
              <span className="font-mono text-sm text-muted-foreground">{tenants.length - activeTenants}</span>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
