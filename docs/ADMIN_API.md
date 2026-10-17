# Admin API 参考 / Admin API Reference

> 双语：中文在前，English after. Bilingual: Chinese first, English second.

IPProxyPool Phase 1 引入独立的 **Admin REST API**（端口 `:9092`），与数据面 Pingora（`:8916`）和 Prometheus 指标（`:9091`）完全隔离。所有端点统一在 `/api/v1/*` 下，通过请求头 `X-Admin-Key` 认证，key 值由环境变量 `ADMIN_API_KEY` 提供；未设置时整个 Admin 服务拒绝所有请求（fail-closed）。

OpenAPI 3.1 文档由 `utoipa` 自动生成，可在 `GET /api/v1/openapi.json` 获取，供前端、Swagger UI 和后续多语言 SDK 代码生成使用。

---

## 1. 认证 / Authentication

| 头部 | 说明 |
|------|------|
| `X-Admin-Key` | 管理 API 密钥，必须等于环境变量 `ADMIN_API_KEY` |
| `Authorization: Admin <key>` | 等效替代方案，兼容 Bearer 风格 |

**安全边界**：
- `ADMIN_API_KEY` 未设置时，所有 Admin 请求返回 `401 Unauthorized`（fail-closed，不泄露任何节点/租户信息）。
- Admin Key 与网关 `X-API-Key` 独立，不可互换。

---

## 2. 端点列表 / Endpoints

### GET `/api/v1/health` — 健康检查

供负载均衡器或监控探活使用。

**响应**：
```json
{ "status": "ok", "service": "ipproxy-pool-admin" }
```

---

### GET `/api/v1/openapi.json` — OpenAPI 3.1 文档

返回整个 Admin API 的 OpenAPI 3.1 JSON 文档。

---

### GET `/api/v1/nodes` — 列举代理节点

**查询参数**：
| 参数 | 类型 | 说明 |
|------|------|------|
| `tier` | string | 过滤档位：`datacenter` / `residential` / `mobile` / `free` |
| `country` | string | 国家码，如 `US` / `JP` / `GB`（大小写不敏感）|
| `proto` | string | 出站协议：`http` / `socks5` / `socks4` |

**响应**：`NodeView[]`（**裸数组**，无包装对象）。

---

### GET `/api/v1/nodes/{addr}` — 节点详情

**路径参数**：`addr` = `ip:port`

**响应**：`NodeView`，未找到返回 `404`。

---

### PATCH `/api/v1/nodes/{addr}/weight` — 运行时调整节点权重

按该节点所属的 `provider + country` 维度整组调权（与 `router::adjust_vendor_weight` 口径一致），生效于下一轮选路，无需重启网关。

**请求体**：
```json
{ "weight": 100 }
```

**响应**：调权后的 `NodeView`，未找到返回 `404`。

---

### POST `/api/v1/nodes/{addr}/quarantine` — 隔离节点

将该节点所在国家/地区出口置入隔离表，TTL 秒后自动恢复（TTL 上限 24h，超限自动钳制）。

**请求体**：
```json
{ "ttl_secs": 600 }
```

**响应**：`NodeView`，未找到返回 `404`。

---

### DELETE `/api/v1/nodes/{addr}/quarantine` — 解除节点隔离

按 POST 隔离时的同一 `(country, ip)` 口径**精确解除**隔离条目（与写入侧共用域名归一化逻辑，大小写/端口变体命中同一条目）；解除后空内层连同外层域一并回收。

**幂等**：未隔离过 / 已被后台 sweep 清掉的条目也返回 `200`（目标态一致，客户端无需区分）。节点不存在返回 `404`。

**响应**：`NodeView`。

---

### GET `/api/v1/tenants` — 列举所有租户摘要

**响应**：`TenantSummary[]`（**裸数组**，无包装对象）。

---

### GET `/api/v1/tenants/{api_key}` — 租户详情

**路径参数**：`api_key` = 租户 API Key

**响应**：`TenantDetail`，未找到返回 `404`。

---

### PATCH `/api/v1/tenants/{api_key}/balance` — 修改租户余额 (USD)

写入即生效（`AtomicF64` 原子写）：欠费门（`balance <= 0` 拒付）下一帧读取即见新值，无需重启。

**请求体**：
```json
{ "balance_usd": 100.0 }
```

**约束**：
- 有限负数允许（扣减 / 罚金语义）；
- `NaN` / `±Infinity` 返回 `400`（会绕过欠费门并永久污染账务）。

**响应**：更新后的 `TenantDetail`，未找到返回 `404`。

---

### PATCH `/api/v1/tenants/{api_key}/active` — 启用/禁用租户

**请求体**：
```json
{ "active": true }
```

**响应**：更新后的 `TenantDetail`，未找到返回 `404`。

---

### GET `/api/v1/freepool/summary` — 免费池供给快照

水位（`nodes_total`，`0` 即干旱）、协议分布、质检结果汇总、各源产出、匿名度分级、源站熔断、进检截断 / 基线不可达 / 容量淘汰累计。

**响应**：`FreePoolSnapshot`

---

### GET `/api/v1/freepool/sources` — 各源免费池产出明细

**响应**：`FreePoolSourceStat[]`（裸数组）。`verify` 结果按 result 全局汇总，不按源拆分。

---

### GET `/api/v1/freepool/autotune` — 免费池自动调权状态

**响应**：
```json
{ "enabled": false, "boost_percent": 100, "dry": false }
```

`dry = true` 表示当前免费池无可用节点（`nodes_total == 0`）。

---

### POST `/api/v1/freepool/autotune` — 更新免费池自动调权

字段缺省则不改。统一按**最终**开关/因子应用：开启时付费节点权重立即上调（基准权重 × 因子，`clamp(1, 200)`），关闭时还原满权（因子按 100 处理）。因子入参会钳制到 `1..500`。

**请求体**：
```json
{ "enabled": true, "boost_percent": 250 }
```

**响应**：应用后的 `AutotuneStatus`

---

### GET `/api/v1/circuit` — 断路器与隔离状态快照

熔断触发计数（429/403/502/504 分状态）、LinUCB 分档强制探索总次数、隔离域数（外层）/ 隔离节点数（内层）。

**响应**：`BreakerSnapshot`

---

## 3. 数据模型 / Data Models

### NodeView

| 字段 | 类型 | 说明 |
|------|------|------|
| `addr` | string | `ip:port` |
| `ip` | string | 节点 IP |
| `port` | integer | 端口 |
| `tier` | string | 档位：`datacenter` / `residential` / `mobile` / `free` |
| `country` | string | 国家码 |
| `provider` | string | 供应商标识 |
| `weight` | integer | 选路权重 |
| `proto` | string | 出站协议：`Http` / `Socks5` / `Socks4` |
| `exit_ip` | string\|null | 真实 egress 出口 IP（FreePool FullCheck 实测）|

### TenantSummary / TenantDetail

| 字段 | 类型 | 说明 |
|------|------|------|
| `tenant_id` | string | 租户 ID |
| `api_key` | string | API Key |
| `is_active` | boolean | 是否启用 |
| `qps` | integer | 限流 QPS |
| `max_concurrency` | integer | 最大并发 |
| `in_flight` | integer | 当前并发数 |
| `total_bytes` | integer | 累计输出字节 |
| `balance_usd` | number | 余额（美元）|

### FreePoolSnapshot

| 字段 | 类型 | 说明 |
|------|------|------|
| `nodes_total` | integer | 当前合并进池的免费节点总数（0 即干旱）|
| `dry` | boolean | 是否干旱（`nodes_total == 0` 即时镜像）|
| `verify` | `VerifyStat[]` | 质检结果汇总：`pass` / `tcp_fail` / `full_fail` / `geo_fail` |
| `by_proto` | `ProtoStat[]` | 按出站协议分布：`http` / `socks5` / `socks4` |
| `sources` | `FreePoolSourceStat[]` | 各源产出与抓取明细 |
| `anonymity` | `AnonymityStat[]` | 匿名度分级：`elite` / `anonymous` / `transparent` / `unknown` |
| `suspended` | `SuspendedStat[]` | 源站熔断状态 |
| `intake_capped` | integer | 进检截断丢弃累计 |
| `baseline_fail` | integer | 基线不可达降级轮次 |
| `evicted` | integer | 容量淘汰累计 |

### FreePoolSourceStat

| 字段 | 类型 | 说明 |
|------|------|------|
| `source` | string | 源名称 |
| `yield_total` | integer | 抓取产出节点数 |
| `elite_total` | integer | Elite 节点数（`elite/yield` 近似该源出货质量）|
| `fetch_ms_sum` | integer | 抓取耗时和（ms，用于前端算均值）|
| `fetch_count` | integer | 抓取次数 |

### AutotuneStatus / AutotuneRequest

| 字段 | 出现于 | 类型 | 说明 |
|------|--------|------|------|
| `enabled` | 两者 | boolean | 干旱自动上调付费权重开关 |
| `boost_percent` | 两者 | integer | 付费权重上调因子（百分比，100=满权，入参钳制 1..500）|
| `dry` | 仅 Status | boolean | 免费池当前是否干旱 |

### BreakerSnapshot / BreakerTriggerStat

| 字段 | 类型 | 说明 |
|------|------|------|
| `breaker_trigger` | `BreakerTriggerStat[]` | 熔断触发计数（per status：429/403/502/504）|
| `bandit_exploration` | integer | LinUCB 分档强制探索总次数（tier_quota 强制选路）|
| `quarantine_domains` | integer | 隔离域数（外层 domain 数）|
| `quarantine_nodes` | integer | 隔离条目总数（内层 `domain:ip` 数）|

`BreakerTriggerStat` = `{ "status": "403", "count": 12 }`。

---

## 4. 错误响应 / Error Response

**所有非 2xx 响应的 body 一律是统一 JSON 错误体**（与 OpenAPI `ErrorResponse` schema 一致）：

```json
{ "error": "错误描述", "code": 401 }
```

| 状态码 | 场景 | 说明 |
|--------|------|------|
| `401` | 认证失败 | 缺失/错误 `X-Admin-Key`，或 `ADMIN_API_KEY` 未配置。**错误文案刻意不区分「未配置」与「不对」**（fail-closed，不给探测者区分信号）|
| `404` | 资源不存在 | 节点/租户；未匹配路径同样返回本形状（fallback）|
| `400` | 请求体非法 | 非 JSON / 字段类型错；`balance_usd` 非有限值（NaN/±Infinity）|

未匹配方法（如对只读端点发 DELETE）由 axum 返回 `405`，body 不在统一错误体口径内。

---

## 5. 示例 / Examples

使用 `curl` 调用 Admin API（假设 `ADMIN_API_KEY=ops-secret-key`）：

```bash
# 健康检查
curl -s http://127.0.0.1:9092/api/v1/health \
  -H "X-Admin-Key: ops-secret-key"

# 列举节点
curl -s http://127.0.0.1:9092/api/v1/nodes?tier=free \
  -H "X-Admin-Key: ops-secret-key"

# 调整节点权重
curl -s -X PATCH http://127.0.0.1:9092/api/v1/nodes/1.2.3.4:8080/weight \
  -H "X-Admin-Key: ops-secret-key" \
  -H "Content-Type: application/json" \
  -d '{"weight": 50}'

# 隔离节点 600 秒
curl -s -X POST http://127.0.0.1:9092/api/v1/nodes/1.2.3.4:8080/quarantine \
  -H "X-Admin-Key: ops-secret-key" \
  -H "Content-Type: application/json" \
  -d '{"ttl_secs": 600}'

# 解除节点隔离（幂等，未隔离过也返回 200）
curl -s -X DELETE http://127.0.0.1:9092/api/v1/nodes/1.2.3.4:8080/quarantine \
  -H "X-Admin-Key: ops-secret-key"

# 列举租户
curl -s http://127.0.0.1:9092/api/v1/tenants \
  -H "X-Admin-Key: ops-secret-key"

# 免费池供给快照 / 各源明细 / 自动调权状态
curl -s http://127.0.0.1:9092/api/v1/freepool/summary \
  -H "X-Admin-Key: ops-secret-key"
curl -s http://127.0.0.1:9092/api/v1/freepool/sources \
  -H "X-Admin-Key: ops-secret-key"
curl -s http://127.0.0.1:9092/api/v1/freepool/autotune \
  -H "X-Admin-Key: ops-secret-key"

# 干旱保护：开启自动调权并把付费权重上调到 250%
curl -s -X POST http://127.0.0.1:9092/api/v1/freepool/autotune \
  -H "X-Admin-Key: ops-secret-key" \
  -H "Content-Type: application/json" \
  -d '{"enabled": true, "boost_percent": 250}'

# 断路器与隔离状态快照
curl -s http://127.0.0.1:9092/api/v1/circuit \
  -H "X-Admin-Key: ops-secret-key"

# 获取 OpenAPI 文档
curl -s http://127.0.0.1:9092/api/v1/openapi.json \
  -H "X-Admin-Key: ops-secret-key" | jq .info.title

# 认证失败的统一错误体（401）
curl -s http://127.0.0.1:9092/api/v1/health
# => {"error":"missing or invalid admin key","code":401}
```

---

## 6. 部署注意 / Deployment

- Admin 服务缺省绑定 `127.0.0.1:9092`，可通过环境变量 `ADMIN_ADDR` 覆写。
- 生产环境 **必须** 设置 `ADMIN_API_KEY` 为强随机值，否则 Admin 接口全部返回 401。
- Admin 服务由网关进程内托管（`tokio::spawn`），随网关一起启停，无需额外进程。

---

## 中文 / English 对照表

| 中文 | English |
|------|---------|
| 管理密钥 | Admin API Key |
| 节点池 | Node Pool |
| 档位 | Tier |
| 权重 | Weight |
| 隔离 | Quarantine |
| 解除隔离 | Lift Quarantine / Unquarantine |
| 自动调权 | Auto Weight Boost / Autotune |
| 免费池 | Free Pool |
| 干旱 | Dry (no free nodes available) |
| 匿名度 | Anonymity Level |
| 质检 | Verification |
| 熔断 | Circuit Break |
| 断路器快照 | Breaker Snapshot |
| 租户 | Tenant |
| 余额 | Balance |
| 启用/禁用 | Active / Disabled |
