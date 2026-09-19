# SPIKE-R2 延后项技术评估（2026年9月19日，只评估不落地）

> 范围：GW-R1 计划“延后”清单 5 项。结论全部为 **NO-GO（本轮不落地）**，
> 每项给出落地路径与重评触发器。运维摘要见 `docs/OPERATION.md §5`。
> 评估方法：Pingora 0.6 registry 源码实查 + 2026 年生态现状检索 + 本机内核实测。

## 证据基线

- 本机 Docker 宿主内核：`6.6.87.2-microsoft-standard-WSL2`（eBPF / io_uring 能力具备）。
- `pingora-core 0.6.0` TLS 后端为特性门：`boringssl` / `openssl` / `rustls` 三选；
  本仓启用的是 `proxy + lb + rustls`（见 `gateway/Cargo.toml`），上游 TLS 走 `pingora-rustls`。
- `PeerOptions`（`pingora-core-0.6.0/src/upstreams/peer.rs:321-355`）的 TLS 旋钮只有
  `curves` / `second_keyshare` / `alpn` / `verify_cert|hostname` / `ca` / `alternative_cn`，
  **无 ClientHello 扩展排序/密码套件排序/自定义 TLS 连接器的注入点**
  （`custom_l4` 仅覆盖 L4，不覆盖 TLS 握手）。
- 网关现状：上游一律 plain-HTTP（`is_tls=false` 冻结），即 **TLS 指纹面当前不存在**，
  uTLS 在“TLS 上游”立项前无附着点。

## Spike-1 uTLS 全栈拟真（ClientHello + HTTP/2 帧）→ NO-GO

2026 年生态（已验证存在）：拟真= **BoringSSL 路线为主**（`boring` + `tokio-boring`，
`set_permute_extensions / GREASE / X25519MLKEM768 双 key share / ALPS / ECH GREASE`，
参考 Chrome 147/148 抓包常量 + JA4 漂移门单测）；rustls 路线靠 fork
（`craftls` 自定义 ClientHello、`webclaw-tls` 经 `[patch.crates-io]` 全工作区替换
rustls/h2/hyper/reqwest）。

不可行的两条路（本仓约束）：

1. `PeerOptions` 微调：只有 `curves`/`second_keyshare` 两档，凑不出 Chrome JA4
   （扩展顺序/GREASE/证书压缩/ECH 均不可控）。
2. `[patch.crates-io]` 换 rustls fork：与 `pingora-rustls` 版本钉死冲突，
   违反“不碰 Pingora 版本”铁律，且 Pingora 自有 h2 栈的 SETTINGS/伪头顺序仍不可控。

可行落地路径（留待 TLS 上游立项时）：

1. 切 `pingora` 特性 `rustls` → `boringssl`（BoringSSL 源码构建，需 cmake+perl，
   Windows 构建已有 cmake 经验，见 EXEC_LOG GW-0）。
2. 按 `boring` crate 做 per-connection 配置（扩展洗牌/GREASE/双 key share/ALPS/ECH），
   照抄 honk / browser_oxide / mihomo-rust 的已验证模式。
3. JA4 漂移门：常量钉死 + 新鲜度检查（Chrome 约 1~2 个大版本一变，模板过期即告警，
   参考 phantom-protocol `PROFILE_CAPTURED` 机制）。
4. HTTP/2 层：需另查 Pingora h2 SETTINGS/伪头顺序可配性（本次未展开，列为落地前置）。

重评触发器：① 上游切 TLS（HTTPS 源站/TLS 包装 egress）立项；② 供应商开始按 JA4 封禁。

## Spike-2 eBPF Sockmap → NO-GO

2026 年研究（Beeline/XLB，arXiv 2605/2602）证实 L7 入核可行，但需自研合成管线/
内核模块级改造，工程量与 GW-R1 整轮相当；且 sockmap 小包存在固有短板
（bpfconf2025：sk_msg 无批处理、`lock_sock` 昂贵， egress 路径多一次拷贝，
2026-03 splice 零拷贝补丁仍在合入中）。

本仓瓶颈在出站 TLS + 供应商 RTT（实测选路 98ns，网关附加 P99 约 50ms），
用户态拷贝占比 <5%，与 OPERATION §5 结论一致。另需 `CAP_SYS_ADMIN` + Linux 生产节点，
Windows 验证环境不可用。

重评触发器： profiling 显示 loopback/用户态拷贝主导时延，或生产切 Linux 后 QPS 瓶颈在网关自身。

## Spike-3 io_uring → NO-GO

2026 年 tokio 的 io_uring 仍是 `tokio_unstable` 特性（SQPOLL PR #7960 未合入、
ring-per-thread #8249 草案）；`tokio-uring` 要求单线程 `!Send` 任务模型，
与 Pingora 多线程 `Send` 执行器根本不兼容——采用=换 runtime，得不偿失。
tokio 文件 IO 已在可用处自行走 uring，无需动作。

重评触发器：tokio LTS 稳定 io_uring 且 Pingora 官方切换执行器。

## Spike-4 MASQUE/QUIC 隧道 → NO-GO（对端缺失）

标准与服务端生态已就绪：CONNECT-UDP（RFC 9298）/ CONNECT-IP（RFC 9484），
quiche / Envoy 1.32+ / NGINX 1.27+ / HAProxy 3.0+ 均支持，iCloud Private Relay 与
Cloudflare WARP 已大规模生产运行（2026-05/09 文献）。

卡点唯一且致命：三家供应商（Oxylabs/BrightData/NetNut）只收 TCP 代理，
**无 MASQUE 对端**。自建 Mesh 继续沿用 `docs/EGRESS_MESH.md` 的 WireGuard 方案；
只有“UDP/QUIC 实测优于 WG 且有对端”时才值得把 WG 换成 MASQUE。

重评触发器：供应商或自建对端支持 CONNECT-IP。

## Spike-5 BGP Anycast → NO-GO（组织级事项）

需 ASN/地址段与运营商建会话，非代码问题。沿用 EGRESS_MESH 模板，生产割接另起项目。

## 总表

| 项 | 结论 | 重评触发器 |
|---|---|---|
| uTLS 全栈拟真 | NO-GO（无 TLS 面 + 无注入点） | TLS 上游立项 / 按 JA4 封禁 |
| eBPF Sockmap | NO-GO（收益<5%，工程量一轮） | profiling 指认拷贝瓶颈 |
| io_uring | NO-GO（tokio 未稳定 + 与 Pingora 执行器不兼容） | tokio LTS + Pingora 跟进 |
| MASQUE/QUIC | NO-GO（供应商无对端） | 对端支持 CONNECT-IP |
| BGP Anycast | NO-GO（组织级） | 生产割接立项 |
