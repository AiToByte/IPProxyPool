"use client";

import { useEffect, useState, useCallback } from "react";
import { Search, Filter, ShieldAlert, ShieldCheck, TrendingUp } from "lucide-react";
import { AdminApiClient, NodeView } from "@/hooks/useAdminApi";

export function Nodes({ client }: { client: AdminApiClient }) {
  const [nodes, setNodes] = useState<NodeView[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [filterTier, setFilterTier] = useState("");
  const [filterCountry, setFilterCountry] = useState("");
  const [search, setSearch] = useState("");

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const data = await client.listNodes({ tier: filterTier || undefined, country: filterCountry || undefined });
      setNodes(data.filter((n) => n.addr.toLowerCase().includes(search.toLowerCase())));
    } catch (e: any) {
      setError(e.message);
    } finally {
      setLoading(false);
    }
  }, [client, filterTier, filterCountry, search]);

  useEffect(() => { load(); }, [load]);

  const handleWeightChange = async (addr: string, weight: number) => {
    try {
      await client.patchNodeWeight(addr, weight);
      load();
    } catch (e: any) {
      alert("调权失败：" + e.message);
    }
  };

  const handleQuarantine = async (addr: string) => {
    const ttl = prompt("隔离 TTL（秒，例如 600）", "600");
    if (ttl === null) return;
    const secs = parseInt(ttl, 10);
    if (!isFinite(secs) || secs <= 0) {
      alert("TTL 必须是正整数");
      return;
    }
    try {
      await client.postNodeQuarantine(addr, secs);
      load();
    } catch (e: any) {
      alert("隔离失败：" + e.message);
    }
  };

  // 解除隔离：调用 DELETE /nodes/{addr}/quarantine（幂等，未隔离过也返回 200）。
  const handleUnquarantine = async (addr: string) => {
    try {
      await client.deleteNodeQuarantine(addr);
      load();
    } catch (e: any) {
      alert("解除隔离失败：" + e.message);
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

  return (
    <div className="space-y-6">
      <div className="flex flex-col lg:flex-row lg:items-center justify-between gap-4">
        <div className="flex items-center gap-2">
          <Search className="w-4 h-4 text-muted-foreground" />
          <input
            type="text"
            placeholder="搜索节点地址..."
            value={search}
            onChange={(e) => setSearch(e.target.value)}
            className="w-full md:w-64 px-3 py-2 bg-input border border-input rounded-md focus:outline-none focus:ring-2 focus:ring-ring"
          />
        </div>
        <div className="flex items-center gap-3">
          <select
            value={filterTier}
            onChange={(e) => setFilterTier(e.target.value)}
            className="px-3 py-2 bg-input border border-input rounded-md text-sm focus:outline-none focus:ring-2 focus:ring-ring"
          >
            <option value="">全部档位</option>
            <option value="datacenter">Datacenter</option>
            <option value="residential">Residential</option>
            <option value="mobile">Mobile</option>
            <option value="free">Free</option>
          </select>
          <input
            type="text"
            placeholder="国家码（如 US）"
            value={filterCountry}
            onChange={(e) => setFilterCountry(e.target.value)}
            className="w-32 px-3 py-2 bg-input border border-input rounded-md text-sm focus:outline-none focus:ring-2 focus:ring-ring"
          />
          <button
            onClick={load}
            className="px-3 py-2 bg-primary text-primary-foreground rounded-md hover:bg-primary/90 flex items-center gap-2"
          >
            <Filter className="w-4 h-4" /> 刷新
          </button>
        </div>
      </div>

      <div className="rounded-lg border border-border overflow-x-auto">
        <table className="w-full text-sm text-left">
          <thead className="bg-muted/50 text-muted-foreground">
            <tr>
              <th className="p-3 font-medium">地址</th>
              <th className="p-3 font-medium">IP</th>
              <th className="p-3 font-medium">端口</th>
              <th className="p-3 font-medium">档位</th>
              <th className="p-3 font-medium">国家</th>
              <th className="p-3 font-medium">提供商</th>
              <th className="p-3 font-medium">协议</th>
              <th className="p-3 font-medium">权重</th>
              <th className="p-3 font-medium">出口 IP</th>
              <th className="p-3 font-medium">操作</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-border">
            {nodes.map((n) => (
              <tr key={n.addr} className="hover:bg-muted/50">
                <td className="p-3 font-mono text-sm">{n.addr}</td>
                <td className="p-3 font-mono text-sm">{n.ip}</td>
                <td className="p-3 text-sm">{n.port}</td>
                <td className="p-3 text-sm capitalize">{n.tier}</td>
                <td className="p-3 text-sm uppercase">{n.country}</td>
                <td className="p-3 text-sm">{n.provider}</td>
                <td className="p-3 text-sm">{n.proto}</td>
                <td className="p-3 text-sm">
                  <input
                    type="number"
                    value={n.weight}
                    onChange={(e) => handleWeightChange(n.addr, parseInt(e.target.value, 10) || 0)}
                    className="w-20 px-2 py-1 bg-input border border-input rounded-md text-sm focus:outline-none focus:ring-2 focus:ring-ring"
                  />
                </td>
                <td className="p-3 font-mono text-xs">{n.exit_ip || "—"}</td>
                <td className="p-3 text-sm">
                  <div className="flex gap-2">
                    <button
                      onClick={() => handleQuarantine(n.addr)}
                      className="px-3 py-1 text-xs text-destructive border border-destructive/50 rounded hover:bg-destructive/10 flex items-center gap-1"
                    >
                      <ShieldAlert className="w-3 h-3" /> 隔离
                    </button>
                    <button
                      onClick={() => handleUnquarantine(n.addr)}
                      className="px-3 py-1 text-xs text-muted-foreground border border-input rounded hover:bg-muted flex items-center gap-1"
                    >
                      <ShieldCheck className="w-3 h-3" /> 解除隔离
                    </button>
                  </div>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {nodes.length === 0 && !loading && !error && (
        <div className="text-center py-12 text-muted-foreground">
          暂无节点数据
        </div>
      )}
    </div>
  );
}
