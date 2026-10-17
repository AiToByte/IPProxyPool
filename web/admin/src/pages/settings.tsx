"use client";

import { useState } from "react";
import { Shield, LogOut, Activity } from "lucide-react";

export function Settings({
  client,
  logout,
}: {
  client: any;
  logout: () => void;
}) {
  const [testLoading, setTestLoading] = useState(false);
  const [testResult, setTestResult] = useState<string | null>(null);

  const handleTestConnection = async () => {
    setTestLoading(true);
    setTestResult(null);
    try {
      await client.health();
      setTestResult("Admin API 连接正常");
    } catch (e: any) {
      setTestResult("连接失败：" + e.message);
    } finally {
      setTestLoading(false);
    }
  };

  const handleLogout = () => {
    logout();
  };

  return (
    <div className="space-y-6">
      <div className="rounded-lg border border-border p-6">
        <div className="flex items-center gap-3 mb-4">
          <Shield className="w-5 h-5 text-primary" />
          <h2 className="text-lg font-semibold">安全与认证</h2>
        </div>
        <p className="text-sm text-muted-foreground mb-4">
          Admin API 通过请求头 `X-Admin-Key` 进行认证。密钥由网关环境变量
          `ADMIN_API_KEY` 提供。本浏览器会话已将密钥保存在 localStorage。
        </p>
        <div className="space-y-3">
          <button
            onClick={handleTestConnection}
            disabled={testLoading}
            className="w-full py-2 px-4 bg-primary text-primary-foreground rounded-md hover:bg-primary/90 disabled:opacity-50 flex items-center justify-center gap-2"
          >
            {testLoading && <Activity className="w-4 h-4 animate-spin" />}
            测试 Admin API 连接
          </button>
          {testResult && (
            <p className={`text-sm ${
              testResult.includes("失败") ? "text-destructive" : "text-green-500"
            }`}>{testResult}</p>
          )}
        </div>
      </div>

      <div className="rounded-lg border border-border p-6">
        <div className="flex items-center gap-3 mb-4">
          <LogOut className="w-5 h-5 text-destructive" />
          <h2 className="text-lg font-semibold">会话</h2>
        </div>
        <p className="text-sm text-muted-foreground">
          退出登录将清除本地保存的管理密钥，下次访问需要重新输入。
        </p>
        <button
          onClick={handleLogout}
          className="w-full py-2 px-4 text-destructive bg-destructive/10 border border-destructive/20 rounded-md hover:bg-destructive/20"
        >
          退出登录
        </button>
      </div>
    </div>
  );
}
