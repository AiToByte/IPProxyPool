"use client";

import { useEffect, useState, useCallback } from "react";
import { CreditCard, Power, TrendingUp } from "lucide-react";
import { AdminApiClient, TenantSummary } from "@/hooks/useAdminApi";

export function Tenants({ client }: { client: AdminApiClient }) {
  const [tenants, setTenants] = useState<TenantSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const data = await client.listTenants();
      setTenants(data);
    } catch (e: any) {
      setError(e.message);
    } finally {
      setLoading(false);
    }
  }, [client]);

  useEffect(() => { load(); }, [load]);

  const handleBalanceChange = async (apiKey: string, balance: number) => {
    try {
      await client.patchTenantBalance(apiKey, balance);
      load();
    } catch (e: any) {
      alert("修改余额失败：" + e.message);
    }
  };

  const handleActiveChange = async (apiKey: string, active: boolean) => {
    try {
      await client.patchTenantActive(apiKey, active);
      load();
    } catch (e: any) {
      alert("修改状态失败：" + e.message);
    }
  };

  if (loading) {
    return (
      <div className="flex h-[400px] items-center justify-center">
        <TrendingUp className="w-8 h-8 animate-spin" />
      </div>
    );
  }

  if (error) {
    return <div className="p-6 text-destructive text-sm">加载失败：{error}</div>;
  }

  const totalBytes = tenants.reduce((sum, t) => sum + t.total_bytes, 0);
  const activeCount = tenants.filter((t) => t.is_active).length;

  return (
    <div className="space-y-6">
      <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
        <div className="bg-card border border-border p-4 rounded-lg">
          <p className="text-xs text-muted-foreground">总租户数</p>
          <p className="text-xl font-bold">{tenants.length}</p>
        </div>
        <div className="bg-card border border-border p-4 rounded-lg">
          <p className="text-xs text-muted-foreground">活跃租户</p>
          <p className="text-xl font-bold text-green-500">{activeCount}</p>
        </div>
        <div className="bg-card border border-border p-4 rounded-lg">
          <p className="text-xs text-muted-foreground">总输出字节</p>
          <p className="text-xl font-bold font-mono">{totalBytes.toLocaleString()} B</p>
        </div>
      </div>

      <div className="rounded-lg border border-border overflow-x-auto">
        <table className="w-full text-sm text-left">
          <thead className="bg-muted/50 text-muted-foreground">
            <tr>
              <th className="p-3 font-medium">租户 ID</th>
              <th className="p-3 font-medium">API Key</th>
              <th className="p-3 font-medium">QPS</th>
              <th className="p-3 font-medium">并发</th>
              <th className="p-3 font-medium">总字节</th>
              <th className="p-3 font-medium">余额 (USD)</th>
              <th className="p-3 font-medium">状态</th>
              <th className="p-3 font-medium">操作</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-border">
            {tenants.map((t) => (
              <tr key={t.api_key} className="hover:bg-muted/50">
                <td className="p-3 font-mono text-xs">{t.tenant_id}</td>
                <td className="p-3 font-mono text-xs truncate max-w-[200px]">{t.api_key}</td>
                <td className="p-3 text-sm">{t.qps}</td>
                <td className="p-3 text-sm">{t.max_concurrency}</td>
                <td className="p-3 font-mono text-sm">{t.total_bytes.toLocaleString()} B</td>
                <td className="p-3 text-sm">
                  <input
                    type="number"
                    step="0.01"
                    value={t.balance_usd}
                    onChange={(e) =>
                      handleBalanceChange(t.api_key, parseFloat(e.target.value) || 0)
                    }
                    className="w-24 px-2 py-1 bg-input border border-input rounded-md text-sm focus:outline-none focus:ring-2 focus:ring-ring"
                  />
                </td>
                <td className="p-3 text-sm">
                  <span className={`px-2 py-0.5 rounded-full text-xs ${
                    t.is_active
                      ? "bg-green-500/10 text-green-500 border border-green-500/20"
                      : "bg-muted/50 text-muted-foreground"
                  }`}>
                    {t.is_active ? "启用" : "禁用"}
                  </span>
                </td>
                <td className="p-3 text-sm">
                  <button
                    onClick={() => handleActiveChange(t.api_key, !t.is_active)}
                    className="px-3 py-1 text-xs text-primary border border-primary/50 rounded hover:bg-primary/10 flex items-center gap-1"
                  >
                    <Power className="w-3 h-3" /> {t.is_active ? "禁用" : "启用"}
                  </button>
                  <button
                    onClick={() => handleBalanceChange(t.api_key, t.balance_usd)}
                    className="ml-2 px-3 py-1 text-xs text-muted-foreground border border-input rounded hover:bg-muted flex items-center gap-1"
                  >
                    <CreditCard className="w-3 h-3" /> 保存
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {tenants.length === 0 && !loading && !error && (
        <div className="text-center py-12 text-muted-foreground">
          暂无租户数据
        </div>
      )}
    </div>
  );
}
