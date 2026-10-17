"use client";

import React from "react";
import { useState, useEffect } from "react";
import { Layout } from "./components/layout";
import { Login } from "./pages/login";
import { AdminApiClient } from "./hooks/useAdminApi";

const ADMIN_KEY_KEY = "ipp_admin_key";

export function App() {
  const [adminKey, setAdminKey] = useState<string | null>(null);
  const [client, setClient] = useState<AdminApiClient | null>(null);
  const [path, setPath] = useState<string>("/dashboard");
  const [ready, setReady] = useState(false);

  useEffect(() => {
    const key = localStorage.getItem(ADMIN_KEY_KEY);
    if (key) {
      setAdminKey(key);
      setClient(new AdminApiClient(key));
    }
    setReady(true);
  }, []);

  const login = (key: string) => {
    localStorage.setItem(ADMIN_KEY_KEY, key);
    setAdminKey(key);
    setClient(new AdminApiClient(key));
    setPath("/dashboard");
  };

  const logout = () => {
    localStorage.removeItem(ADMIN_KEY_KEY);
    setAdminKey(null);
    setClient(null);
    setPath("/login");
  };

  if (!ready) {
    return <div className="flex h-screen items-center justify-center">加载中...</div>;
  }

  if (!adminKey || !client) {
    return <Login onLogin={login} />;
  }

  const navItems = [
    { path: "/dashboard", label: "仪表盘" },
    { path: "/nodes", label: "节点池" },
    { path: "/tenants", label: "租户与计费" },
    { path: "/freepool", label: "免费池" },
    { path: "/circuit", label: "断路器" },
    { path: "/settings", label: "设置" },
  ];

  return (
    <Layout currentPath={path} onNavigate={setPath}>
      <div className="flex flex-col h-full">
        <nav className="flex gap-4 pb-4 border-b border-border mb-6">
          {navItems.map((item) => (
            <a
              key={item.path}
              href={item.path}
              onClick={(e) => {
                e.preventDefault();
                setPath(item.path);
              }}
              className={`px-3 py-2 text-sm font-medium transition-colors border-b-2 ${
                path === item.path
                  ? "border-primary text-primary"
                  : "border-transparent text-muted-foreground hover:text-foreground"
              }`}
            >
              {item.label}
            </a>
          ))}
        </nav>
        <div className="flex-1 overflow-y-auto">
          {path === "/dashboard" && (
            <DashboardView client={client} />
          )}
          {path === "/nodes" && (
            <NodesView client={client} />
          )}
          {path === "/tenants" && (
            <TenantsView client={client} />
          )}
          {path === "/freepool" && (
            <FreePoolView client={client} />
          )}
          {path === "/circuit" && (
            <CircuitView client={client} />
          )}
          {path === "/settings" && (
            <SettingsView client={client} logout={logout} />
          )}
        </div>
      </div>
    </Layout>
  );
}

function DashboardView({ client }: { client: AdminApiClient }) {
  const [View, setView] = useState<any>(null);
  useEffect(() => {
    import("./pages/dashboard").then((m) => setView(() => m.Dashboard));
  }, []);
  return View ? React.createElement(View, { client }) : null;
}

function NodesView({ client }: { client: AdminApiClient }) {
  const [View, setView] = useState<any>(null);
  useEffect(() => {
    import("./pages/nodes").then((m) => setView(() => m.Nodes));
  }, []);
  return View ? React.createElement(View, { client }) : null;
}

function TenantsView({ client }: { client: AdminApiClient }) {
  const [View, setView] = useState<any>(null);
  useEffect(() => {
    import("./pages/tenants").then((m) => setView(() => m.Tenants));
  }, []);
  return View ? React.createElement(View, { client }) : null;
}

function FreePoolView({ client }: { client: AdminApiClient }) {
  const [View, setView] = useState<any>(null);
  useEffect(() => {
    import("./pages/freepool").then((m) => setView(() => m.FreePool));
  }, []);
  return View ? React.createElement(View, { client }) : null;
}

function CircuitView({ client }: { client: AdminApiClient }) {
  const [View, setView] = useState<any>(null);
  useEffect(() => {
    import("./pages/circuit").then((m) => setView(() => m.Circuit));
  }, []);
  return View ? React.createElement(View, { client }) : null;
}

function SettingsView({ client, logout }: { client: AdminApiClient; logout: () => void }) {
  const [View, setView] = useState<any>(null);
  useEffect(() => {
    import("./pages/settings").then((m) => setView(() => m.Settings));
  }, []);
  return View ? React.createElement(View, { client, logout }) : null;
}
