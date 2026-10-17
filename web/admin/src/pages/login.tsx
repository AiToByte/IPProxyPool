"use client";

import { useState } from "react";
import { Shield, Activity } from "lucide-react";

export function Login({ onLogin }: { onLogin: (key: string) => void }) {
  const [key, setKey] = useState("");
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError("");
    setLoading(true);
    try {
      // 向 Admin API 健康检查端点验证 Key 是否有效
      const res = await fetch("/api/v1/health", {
        headers: { "X-Admin-Key": key },
      });
      if (res.ok) {
        onLogin(key);
      } else {
        setError("管理密钥无效，请检查 ADMIN_API_KEY 配置");
      }
    } catch {
      setError("无法连接到 Admin API，请确认网关已启动并监听 :9092");
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="flex h-screen items-center justify-center bg-background">
      <div className="w-full max-w-md p-8 bg-card border border-border rounded-lg shadow-lg">
        <div className="flex items-center gap-2 mb-6">
          <Shield className="w-6 h-6 text-primary" />
          <h1 className="text-xl font-bold">IPProxyPool 管理后台</h1>
        </div>
        <p className="text-muted-foreground text-sm mb-6">
          请输入管理 API 密钥（对应网关环境变量 ADMIN_API_KEY）
        </p>
        <form onSubmit={handleSubmit} className="space-y-4">
          <div className="space-y-2">
            <label className="text-sm font-medium">管理密钥</label>
            <input
              type="password"
              value={key}
              onChange={(e) => setKey(e.target.value)}
              className="w-full px-3 py-2 bg-input border border-input rounded-md focus:outline-none focus:ring-2 focus:ring-ring disabled:opacity-50"
              placeholder="Admin API Key"
              disabled={loading}
            />
          </div>
          {error && (
            <p className="text-sm text-destructive">{error}</p>
          )}
          <button
            type="submit"
            disabled={loading}
            className="w-full py-2 px-4 bg-primary text-primary-foreground rounded-md hover:bg-primary/90 disabled:opacity-50 flex items-center justify-center gap-2"
          >
            {loading && <Activity className="w-4 h-4 animate-spin" />}
            登录
          </button>
        </form>
        <p className="text-xs text-muted-foreground mt-6 text-center">
          提示：网关需启用 Admin 服务（:9092）并设置 ADMIN_API_KEY
        </p>
      </div>
    </div>
  );
}
