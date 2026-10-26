"use client";

import { useEffect, useState, useCallback, type FormEvent, type ReactNode } from "react";
import { Search, Filter, ShieldAlert, ShieldCheck, TrendingUp, Plus, Trash2 } from "lucide-react";
import { AdminApiClient, NodeView, type AddNodeRequest } from "@/hooks/useAdminApi";

const EMPTY_FORM = {
  ip: "",
  port: "",
  country: "",
  tier: "datacenter",
  provider: "",
  weight: "100",
  proto: "http",
  username: "",
  password: "",
};

const INPUT_CLS =
  "w-full px-3 py-2 bg-input border border-input rounded-md text-sm focus:outline-none focus:ring-2 focus:ring-ring";

/** 从 axios 错误里取后端统一错误体的 `error`（契约：{error, code}）。 */
function apiError(e: unknown): string {
  const err = e as { response?: { data?: { error?: string } }; message?: string };
  return err.response?.data?.error || err.message || String(e);
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <label className="block text-xs space-y-1">
      <span className="text-muted-foreground">{label}</span>
      {children}
    </label>
  );
}

export function Nodes({ client }: { client: AdminApiClient }) {
  const [nodes, setNodes] = useState<NodeView[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [filterTier, setFilterTier] = useState("");
  const [filterCountry, setFilterCountry] = useState("");
  const [search, setSearch] = useState("");
  const [showAdd, setShowAdd] = useState(false);
  const [saving, setSaving] = useState(false);
  const [form, setForm] = useState(EMPTY_FORM);

  const setField = (key: keyof typeof EMPTY_FORM, value: string) =>
    setForm((f) => ({ ...f, [key]: value }));

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
      alert("调权失败：" + apiError(e));
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
      alert("隔离失败：" + apiError(e));
    }
  };

  // 解除隔离：调用 DELETE /nodes/{addr}/quarantine（幂等，未隔离过也返回 200）。
  const handleUnquarantine = async (addr: string) => {
    try {
      await client.deleteNodeQuarantine(addr);
      load();
    } catch (e: any) {
      alert("解除隔离失败：" + apiError(e));
    }
  };

  const handleCreate = async (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const port = Number(form.port);
    const weight = Number(form.weight);
    const country = form.country.trim();
    if (!form.ip.trim()) return alert("IP 不能为空");
    if (!Number.isInteger(port) || port < 1 || port > 65535) {
      return alert("端口必须是 1..65535 的整数");
    }
    if (!/^[A-Za-z]{2}$/.test(country)) {
      return alert("国家码必须是两个字母（如 US）");
    }
    if (!form.provider.trim()) return alert("提供商不能为空");
    if (!Number.isInteger(weight) || weight < 0 || weight > 100) {
      return alert("权重必须是 0..100 的整数（0 = 摘除）");
    }
    const hasUser = form.username.trim().length > 0;
    const hasPass = form.password.length > 0;
    if (hasUser !== hasPass) return alert("用户名与密码必须成对填写（或都留空）");

    const req: AddNodeRequest = {
      ip: form.ip.trim(),
      port,
      country,
      tier: form.tier,
      provider: form.provider.trim(),
      weight,
      proto: form.proto,
    };
    if (hasUser) {
      req.username = form.username.trim();
      req.password = form.password;
    }

    setSaving(true);
    try {
      await client.addNode(req);
      setShowAdd(false);
      setForm(EMPTY_FORM);
      await load();
    } catch (e: any) {
      alert("新增失败：" + apiError(e));
    } finally {
      setSaving(false);
    }
  };

  const handleDelete = async (n: NodeView) => {
    const provider = n.provider.toLowerCase();
    if (n.tier === "free" || provider === "free" || provider.startsWith("free-")) {
      alert(
        "免费池节点由 FreePool 每轮 merge 重建，删除后很快会回来。\n如需摘除，请把该节点权重改为 0。",
      );
      return;
    }
    if (!window.confirm(`确认删除 ${n.addr}？\n删除会立即写入持久化文件，重启后仍不存在。`)) return;
    try {
      await client.deleteNode(n.addr);
      await load();
    } catch (e: any) {
      alert("删除失败：" + apiError(e));
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
          <button
            onClick={() => setShowAdd((v) => !v)}
            className="px-3 py-2 bg-primary text-primary-foreground rounded-md hover:bg-primary/90 flex items-center gap-2"
          >
            <Plus className="w-4 h-4" /> {showAdd ? "取消新增" : "新增节点"}
          </button>
        </div>
      </div>

      {showAdd && (
        <form onSubmit={handleCreate} className="rounded-lg border border-border p-4 space-y-4 bg-muted/20">
          <div className="text-sm font-medium">新增托管节点</div>
          <div className="grid grid-cols-1 md:grid-cols-3 gap-3">
            <Field label="IP（仅 IPv4）">
              <input
                className={INPUT_CLS}
                value={form.ip}
                onChange={(e) => setField("ip", e.target.value)}
                placeholder="203.0.113.10"
              />
            </Field>
            <Field label="端口">
              <input
                type="number"
                min={1}
                max={65535}
                className={INPUT_CLS}
                value={form.port}
                onChange={(e) => setField("port", e.target.value)}
                placeholder="8080"
              />
            </Field>
            <Field label="国家码（两位字母）">
              <input
                className={INPUT_CLS}
                value={form.country}
                maxLength={2}
                onChange={(e) => setField("country", e.target.value)}
                placeholder="US"
              />
            </Field>
            <Field label="档位">
              <select
                className={INPUT_CLS}
                value={form.tier}
                onChange={(e) => setField("tier", e.target.value)}
              >
                <option value="datacenter">Datacenter</option>
                <option value="residential">Residential</option>
                <option value="mobile">Mobile</option>
              </select>
            </Field>
            <Field label="提供商">
              <input
                className={INPUT_CLS}
                value={form.provider}
                onChange={(e) => setField("provider", e.target.value)}
                placeholder="vendor-a"
              />
            </Field>
            <Field label="权重（0 = 摘除，100 = 满权）">
              <input
                type="number"
                min={0}
                max={100}
                className={INPUT_CLS}
                value={form.weight}
                onChange={(e) => setField("weight", e.target.value)}
              />
            </Field>
            <Field label="协议">
              <select
                className={INPUT_CLS}
                value={form.proto}
                onChange={(e) => setField("proto", e.target.value)}
              >
                <option value="http">HTTP</option>
                <option value="socks5">SOCKS5</option>
                <option value="socks4">SOCKS4</option>
              </select>
            </Field>
            <Field label="用户名（可选）">
              <input
                className={INPUT_CLS}
                value={form.username}
                autoComplete="off"
                onChange={(e) => setField("username", e.target.value)}
              />
            </Field>
            <Field label="密码（可选）">
              <input
                type="password"
                className={INPUT_CLS}
                value={form.password}
                autoComplete="new-password"
                onChange={(e) => setField("password", e.target.value)}
              />
            </Field>
          </div>
          <div className="flex flex-col md:flex-row md:items-center justify-between gap-3">
            <p className="text-xs text-muted-foreground">
              新增立即生效并写入持久化文件；免费池（free-*）与测试注入节点不受此表单管理。
            </p>
            <div className="flex gap-2 shrink-0">
              <button
                type="button"
                onClick={() => {
                  setShowAdd(false);
                  setForm(EMPTY_FORM);
                }}
                className="px-3 py-2 border border-input rounded-md text-sm hover:bg-muted"
              >
                取消
              </button>
              <button
                type="submit"
                disabled={saving}
                className="px-3 py-2 bg-primary text-primary-foreground rounded-md text-sm hover:bg-primary/90 disabled:opacity-50"
              >
                {saving ? "提交中..." : "创建节点"}
              </button>
            </div>
          </div>
        </form>
      )}

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
                  <div className="flex flex-wrap gap-2">
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
                    <button
                      onClick={() => handleDelete(n)}
                      className="px-3 py-1 text-xs text-destructive border border-destructive/50 rounded hover:bg-destructive/10 flex items-center gap-1"
                    >
                      <Trash2 className="w-3 h-3" /> 删除
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
