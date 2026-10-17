# 管理后台快速入门 / Admin UI Quick Start

> 双语：中文在前，English after. Bilingual: Chinese first, English second.

IPProxyPool Phase 1 交付专属 **管理后台 Web UI**（`web/admin/`），基于 React + Vite + Tailwind CSS + shadcn/ui 风格，提供仪表盘、节点池、租户与计费、免费池、断路器、设置等页面，替代/补充纯指标的 Grafana 面板，实现“可视化操作”而非仅“可观测”。

---

## 1. 技术栈 / Tech Stack

| 层 | 选型 | 用途 |
|----|------|------|
| 框架 | React 18 + Vite | UI 构建与开发服务器 |
| 样式 | Tailwind CSS | 原子化样式，深色/浅色主题 |
| 图标 | Lucide React | 统一图标集 |
| 全局快照 | Zustand（未用，可选） | 跨页面状态 |
| HTTP | Axios | 调用 Admin API (`/api/v1/*`) |
| 工具 | clsx / tailwind-merge | 条件类名合并 |
| 语言 | TypeScript | 类型安全 |

---

## 2. 目录结构 / Directory Layout

```
web/admin/
├── package.json          # 依赖与脚本
├── vite.config.ts        # Vite 配置（含 /api 代理到 :9092）
├── tailwind.config.js    # Tailwind 主题（深色/浅色配色）
├── postcss.config.cjs    # PostCSS 插件
├── index.html            # HTML 入口
├── tsconfig.json         # TypeScript 配置
├── src/
│   ├── main.tsx          # React 入口
│   ├── index.css         # Tailwind 样式入口
│   ├── lib/utils.ts      # cn() 工具函数
│   ├── App.tsx           # 路由 + 认证守卫 + 页面分发
│   ├── components/
│   │   └── layout.tsx    # 侧边栏 + 顶部导航布局
│   ├── hooks/
│   │   └── useAdminApi.ts# Admin API 客户端类与方法
│   └── pages/
│       ├── login.tsx     # 登录页（输入 ADMIN_API_KEY）
│       ├── dashboard.tsx # 概览（节点总数/付费/免费/租户/流量）
│       ├── nodes.tsx     # 节点列表（搜索/过滤/调权重/隔离/解除隔离）
│       ├── tenants.tsx   # 租户列表（余额/启停/详情）
│       ├── freepool.tsx  # 免费池（水位/质检/各源产出/干旱预警/自动调权）
│       ├── circuit.tsx   # 断路器（熔断触发/隔离域与节点/强制探索计数）
│       └── settings.tsx  # 设置（API 连接测试/退出登录）
└── dist/                 # vite build 产物
```

---

## 3. 前置条件 / Prerequisites

1. 网关已编译并运行（`cargo build --release` + 启动 `pingora-proxy-gateway`）。
2. 环境变量 `ADMIN_API_KEY` 已设置（否则 Admin API 全部 401，前端无法登录）。
3. Node.js ≥ 20 与 npm 已安装。

---

## 4. 本地开发 / Local Development

```powershell
# 进入前端目录
cd web/admin

# 安装依赖（首次）
npm install

# 启动开发服务器（http://localhost:5173，自动代理 /api 到 :9092）
npm run dev
```

开发服务器通过 Vite `server.proxy` 将 `/api` 请求代理到网关 Admin 端口 `:9092`，避免 CORS 问题。

访问 `http://localhost:5173`，输入管理密钥（`ADMIN_API_KEY`）后进入后台。

---

## 5. 生产构建 / Production Build

```powershell
cd web/admin
npm run build
```

产物输出到 `web/admin/dist/`，可由任意静态文件服务器（Nginx/Caddy/Cloudflare Pages）托管。建议与网关同域同端口部署，或通过反向代理将 `/api/v1/*` 转发到 `:9092`。

---

## 6. 页面功能 / Page Features

| 页面 | 路径 | 主要功能 |
|------|------|---------|
| 登录 | `/login` | 输入 `X-Admin-Key`，调用 `/api/v1/health` 验证，成功后写入 `localStorage` |
| 仪表盘 | `/dashboard` | 节点总数 / 付费节点 / 免费节点 / 活跃租户 / 总输出字节 / 租户状态 |
| 节点池 | `/nodes` | 节点列表（地址/IP/端口/档位/国家/提供商/协议/权重/出口 IP），支持搜索、按档位/国家过滤、运行时调权重、隔离节点、解除隔离（幂等）|
| 租户与计费 | `/tenants` | 租户列表（租户 ID/API Key/QPS/并发/总字节/余额/状态），支持修改余额（写入即生效，欠费门立即跟上）、启用/禁用 |
| 免费池 | `/freepool` | 供给水位（干旱预警）、整体验证成功率、各源产出与抓取耗时、Elite 占比、匿名度分布、源站熔断；干旱保护自动调权（开关 + 上调因子 1~500）实时下发 |
| 断路器 | `/circuit` | 隔离域数（外层）/ 隔离节点数（内层）、分档强制探索次数、按状态码（429/403/502/504）的熔断触发明细 |
| 设置 | `/settings` | Admin API 连接测试、退出登录 |

---

## 7. 与 Grafana 的关系 / Relationship with Grafana

- **Grafana**：面向指标与告警（时序图、Stat、告警规则），适合 SRE 和运营查看历史趋势。
- **管理后台**：面向操作与配置（节点调权/隔离、租户余额/启停），适合运营和客户经理进行日常干预。

两者互补：Grafana 看“发生了什么”，管理后台做“要怎么改”。

---

## 8. 故障排查 / Troubleshooting

| 现象 | 可能原因 | 解决 |
|------|---------|------|
| 登录后提示“无法连接到 Admin API” | 网关未启动或 Admin 端口未监听 | 检查网关进程，确认 `:9092` 可达 |
| 登录后提示“管理密钥无效” | `ADMIN_API_KEY` 未设置或不匹配 | 设置环境变量后重启网关 |
| 页面空白 / 构建报错 | Node 依赖未安装 | `cd web/admin && npm install` |
| 调权/隔离后页面未刷新 | 前端缓存 | 点击“刷新”按钮重新加载 |

---

## 中文 / English 对照表

| 中文 | English |
|------|---------|
| 仪表盘 | Dashboard |
| 节点池 | Node Pool |
| 租户与计费 | Tenants & Billing |
| 免费池 | Free Pool |
| 干旱预警 | Dry Warning |
| 断路器 | Circuit Breaker |
| 调权 | Adjust Weight |
| 隔离 | Quarantine |
| 解除隔离 | Lift Quarantine |
| 质检 | Verification |
| 活跃租户 | Active Tenants |
| 退出登录 | Sign Out |
