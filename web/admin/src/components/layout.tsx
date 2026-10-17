"use client";

import { useState } from "react";
import {
  LayoutDashboard,
  Network,
  Users,
  LogOut,
  Menu,
  ChevronLeft,
  Settings,
} from "lucide-react";

const NAV_ITEMS = [
  { path: "/dashboard", label: "仪表盘", icon: LayoutDashboard },
  { path: "/nodes", label: "节点池", icon: Network },
  { path: "/tenants", label: "租户与计费", icon: Users },
  { path: "/settings", label: "设置", icon: Settings },
] as const;

// 顶栏标题映射：侧边栏 NAV_ITEMS 仅含 4 主项，免费池/断路器走顶栏 tab，
// 需在此补齐页面标题（缺项会回落成"管理后台"——走查实证）。
const PAGE_TITLES: Record<string, string> = {
  "/dashboard": "仪表盘",
  "/nodes": "节点池",
  "/tenants": "租户与计费",
  "/freepool": "免费池",
  "/circuit": "断路器",
  "/settings": "设置",
};

interface LayoutProps {
  children: React.ReactNode;
  currentPath: string;
  onNavigate: (path: string) => void;
}

export function Layout({ children, currentPath, onNavigate }: LayoutProps) {
  const [sidebarOpen, setSidebarOpen] = useState(false);

  const isActive = (path: string) => currentPath === path;

  return (
    <div className="flex h-screen bg-background text-foreground">
      {/* 移动端侧边栏遮罩 */}
      {sidebarOpen && (
        <div
          className="fixed inset-0 z-20 bg-black/50 backdrop-blur-sm lg:hidden"
          onClick={() => setSidebarOpen(false)}
        />
      )}
      {/* 侧边栏 */}
      <aside
        className={`fixed left-0 top-0 h-full w-64 bg-card border-r border-border z-30 transform transition-transform duration-300 ${
          sidebarOpen ? "translate-x-0" : "-translate-x-full"
        } lg:relative lg:translate-x-0 lg:static lg:bg-card`}
      >
        <div className="flex h-16 items-center justify-between px-4 border-b border-border">
          <span className="font-semibold text-lg tracking-tight">IPProxyPool</span>
          <button
            onClick={() => setSidebarOpen(false)}
            className="lg:hidden p-1 hover:bg-accent rounded-md"
          >
            <ChevronLeft className="w-5 h-5" />
          </button>
        </div>
        <nav className="flex-1 px-3 space-y-1">
          {NAV_ITEMS.map((item) => {
            const Icon = item.icon;
            return (
              <a
                key={item.path}
                href={item.path}
                onClick={(e) => {
                  e.preventDefault();
                  onNavigate(item.path);
                  setSidebarOpen(false);
                }}
                className={`flex items-center gap-3 px-3 py-2 text-sm rounded-md transition-colors ${
                  isActive(item.path)
                    ? "bg-primary text-primary-foreground"
                    : "text-muted-foreground hover:bg-accent hover:text-accent-foreground"
                }`}
              >
                <Icon className="w-4 h-4" />
                {item.label}
              </a>
            );
          })}
        </nav>
        <div className="p-4 border-t border-border">
          <button
            onClick={() => onNavigate("/login")}
            className="flex w-full items-center gap-3 px-3 py-2 text-sm text-destructive hover:bg-destructive/10 rounded-md transition-colors"
          >
            <LogOut className="w-4 h-4" />
            退出登录
          </button>
        </div>
      </aside>
      {/* 主内容 */}
      <div className="flex-1 flex flex-col relative z-10">
        {/* 顶部导航 */}
        <header className="h-16 border-b border-border bg-background/80 backdrop-blur flex items-center justify-between px-4 sticky top-0 z-20">
          <div className="flex items-center gap-2">
            <button
              onClick={() => setSidebarOpen((o) => !o)}
              className="p-1 hover:bg-accent rounded-md lg:hidden"
            >
              <Menu className="w-5 h-5" />
            </button>
            <h1 className="text-sm font-medium hidden lg:block">
              {PAGE_TITLES[currentPath] || "管理后台"}
            </h1>
          </div>
          <div className="flex items-center gap-3">
            <span className="text-xs text-muted-foreground">v0.2.0 · Phase 1</span>
          </div>
        </header>
        <main className="flex-1 overflow-auto p-6">{children}</main>
      </div>
    </div>
  );
}
