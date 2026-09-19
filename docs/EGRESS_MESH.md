# Egress Mesh：WireGuard 骨干 + Anycast 入口（GW-4 模板，本轮不生产割接）

> 状态：模板与文档先行。BGP 真实宣告、隧道打通与 MASQUE/QUIC cutover 延后（见实施计划延后项）。
> 本轮三家供应商全 Mock，真 Key 由用户后补走 staging 1% 灰度（见 GW-5 OPERATION）。

## 拓扑

```
全球 Anycast 统一入口 VIP (198.51.100.1, 文档示例)
  └─ BGP 最短 AS-Path → Edge POP（法兰克福 / 硅谷：Pingora 网关 + 本地 LinUCB）
       └─ WireGuard 骨干隧道 → 区域出口汇聚（供应商 API 隧道 / 自建 4G 池）
```

## WireGuard 骨干模板（网关侧 `/etc/wireguard/wg0.conf`）

```ini
[Interface]
Address = 10.100.0.1/24
PrivateKey = <GATEWAY_PRIVATE_KEY>
ListenPort = 51820
MTU = 1420

[Peer]
# 法兰克福出口集群
PublicKey = <EGRESS_EU_PUBLIC_KEY>
Endpoint = 195.201.x.x:51820
AllowedIPs = 10.100.0.2/32
PersistentKeepalive = 25

[Peer]
# 北美出口集群
PublicKey = <EGRESS_US_PUBLIC_KEY>
Endpoint = 142.132.x.x:51820
AllowedIPs = 10.100.0.3/32
PersistentKeepalive = 25
```

密钥生成：`wg genkey | tee privatekey | wg pubkey > publickey`，
私钥仅落盘对应节点，公钥随工单交换。

## Anycast 说明（本轮只做方案）

- 同一 VIP 在多 POP 经 BGP 宣告，客户端自动命中最近 AS-Path 节点；
- POP 间状态不同步：quarantine 经 Redis `SETEX` + PubSub delta 已跨实例，
  Anycast 下天然可用；LinUCB 臂状态为节点本地，属预期设计；
- 生产割接前置：ASN/地址段、运营商 BGP 会话、健康检查摘流，另起 GW-R2 跟踪。

## 与本轮代码的对应

- 供应商三家 Mock（`mock-a/b/c`）→ 真 Key 到位后替换 `main.rs` 初始池，
  套利阈值（<80 降权 0 / >95 恢复 100）不变；
- SLA 数据源为 ClickHouse 5 分钟窗（`analytics::sla_sql`），看板见
  `deploy/grafana/dashboard.json`。
