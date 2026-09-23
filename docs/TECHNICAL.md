# Technical 技术说明

> 双语：中文在前，English after. Bilingual: Chinese first, English second.
> 数值锚点：阈值与源码一致；dev 性能数为 Windows 参考基线，非生产承诺。
> Numeric anchors match the source; dev performance figures are Windows reference baselines, not production commitments.

## 中文

### 1. 选路算法

- **LinUCB（d=4，alpha 0.4）**：臂键 `ip:port`；单锁 ArmState＋Sherman-Morrison 更新＋二次型 UCB＋单遍选择；release 8 臂约 98~126ns（预算 200ns，`cargo test --release bandit` 守护）。
- **加权随机**：累积权重 roll；`weight==0` 过滤但快照保留（derate→hold→restore 全周期可恢复）；总量 0 退化均匀。
- **粘滞会话**：`{tenant}:{session}` 命名空间；命中复核现池权重/proto/隔离/TTL（10min，saturating 防回拨 panic）；derate 迁移。
- **隔离**：domain 归一（小写＋剥端口＋去尾点）；节点级双检（代理 ip＋真 egress exit_ip）；TTL 钳制 24h（`checked_add` panic-free）。
- **tier 归一**：构造期＋选路入口各一次 (`canonical_tier`)，`matches` 内零分配。
- **免费风险溢价**：free 臂 UCB 扣 0.15；分档遗忘 free 每 1k 更新、其他每 10k。

### 2. 遥测与数仓泵语义

- 发布：MPSC 1 万封顶；`event_id` 发号 `{ms}-{pid}-{seq}`（payload 级幂等键）；满/关计数 `channel_dropped`。
- 刷新：批量组 pipe（空 pipe 不 query）；XADD 自动 ID＋`MAXLEN ~10万`；失败 100ms 重发一次，仍失败整批 `telemetry_dropped_total` 计数。
- 泵：独立消费组＋60s 滞留认领＋batch 5000/1s＋成功才 ack＋毒丸/重复即时 ack；hold 重放直通去重窗（BUG-5-1）；CH 中断 hold-ack＋内存有界，恢复追齐。

### 3. 熔断与套利阈值

| 信号 | 阈值 | 动作 |
|------|------|------|
| 上游 429／403／502,504 | — | 隔离 60s／600s／30s |
| 付费 SLA | <80／>95 | 降权 0／恢复 100（hold 之间） |
| 免费池级成功率 | <50／50~80／≥80 | 摘除 0.0／半权 0.5／hold（永不自动抬权；`scale` 下限 max(1)） |
| 免费健康 | EWMA α=0.3 成功率×延迟惩罚 | 权重 1..20（trusted 封顶 20；streak≥3＋Elite＋延迟<1500ms） |
| 失败 backoff | 60s×2ⁿ，封顶 1h | TTL 内保留，自动恢复 |
| 源站熔断 | 连续 3 轮零产出（含传输失败；304 豁免） | 暂停，每 3 tick 试探 |

### 4. 租户与计费

- 顺序：身份→开关→余额（≤0 得 402）→QPS→并发；`burst=qps/10≥1`。
- 并发槽：误释放丢弃＋warn（不 wrap）；失败 attempt 字节重试前清零，只计最后 attempt。
- 单价：DC $0.2／Res $3／Mobile $15／Free $0 每 GB；当次可扣负，下次拦截。

### 5. 并发纪律

- `parking_lot`/DashMap 守卫不跨 `await`（临时克隆后释放）；会话→隔离单向嵌套，无死锁。
- FreePool 信号量许可持有跨 await（TCP 50／FullCheck 20）；prewarmer 建链 100；prober 20＋Client 闲置 10min 淘汰。
- 后台 CB/sink/arbitrage/free_pool 由 supervisor 托管（panic/退出 backoff 1s→60s 重启＋计数）；release 用 `unwind`（abort 会架空 supervisor）。
- 生产代码零 `unwrap/expect`（仅 main 三处启动 fail-fast）；算术除零/`as` 转换逐点有界。

### 6. 环境变量总表（缺省见 `main.rs`，compose 有示例）

| Key | 默认 | 说明 |
|-----|------|------|
| `GATEWAY_ADDR`／`METRICS_ADDR` | `0.0.0.0:8080`／`127.0.0.1:9091` | 监听地址 |
| `REDIS_URL` | `redis://127.0.0.1:6379/` | 缺失即降级（遥测仅日志） |
| `CLICKHOUSE_URL/_USER/_PASSWORD/_DB` | `http://127.0.0.1:8123/proxy/123456/proxy` | 缺失即 hold 权重 |
| `REQUIRE_API_KEY` | `0` | `1` 时无头 403 |
| `PROBE/PREWARM/SWEEP/ARBITRAGE_INTERVAL_SECS` | `60/30/60/60` | 后台节拍（三 ticker 启动错峰 0~5s） |
| `SOCKS_BRIDGE_TIMEOUT_SECS`／`SOCKS_MAX_BODY_BYTES` | `20`／`10485760` | 翻译桥 |
| `GEOIP_MMDB_PATH`／`GEOIP_ENFORCE_MISMATCH` | 空／`0` | 无库 Disabled；执法默认关 |
| `FREE_ENABLED` | `0` | 第二线总开关 |
| `FREE_API/HTML/GITHUB_URLS` | Geonode limit=100／fpl／clarketm raw | 仅 http/https（SSRF 过滤） |
| `FREE_FETCH/TTL/VERIFY_TIMEOUT_SECS` | `600/1800/3` | TTL 上限钳 30 天 |
| `FREE_MAX_LATENCY_MS` | `3000` | TCP 门＋EWMA 中性点 |
| `FREE_MAX/FULL_CONCURRENT` | `50/20` | 质检并发 |
| `FREE_MAX_NODES` | `2000` | 容量上限 |
| `FREE_FULL_CHECK_URL` | `https://httpbin.org` | 必须 https |
| `FREE_REQUIRE_ELITE` | `0` | `1` 时仅 Elite |
| `FREE_SOURCE_MAX_ZERO_CYCLES`／`FREE_SUSPEND_RETRY_EVERY` | `3/3` | 源站熔断 |

### 7. 门禁定义（见 `CONTRIBUTING.md`）

`fmt --check` clean＋`clippy -D warnings` 零告警＋`test` 156/4 ignored＋`--ignored` 4 真过（先验 PONG/Ok、无 SKIP）＋`bench --no-run`＋release bandit<200ns。

## English

### 1. Routing algorithms

- **LinUCB (d=4, alpha 0.4)**: arm key `ip:port`; single-lock ArmState + Sherman-Morrison update + quadratic UCB + single-pass select; release 8-arm ~98–126ns (budget 200ns, guarded by `cargo test --release bandit`).
- **Weighted random**: cumulative-weight roll; `weight==0` filtered but kept in snapshot (derate→hold→restore fully recoverable); zero total degrades to uniform.
- **Sticky sessions**: `{tenant}:{session}` namespace; hits re-check live-pool weight/proto/quarantine/TTL (10min, saturating against clock rollback); migrate on derate.
- **Quarantine**: normalized domains (lowercase + strip port/trailing dot); per-node dual check (proxy ip + true egress exit_ip); TTL clamped at 24h (`checked_add`, panic-free).
- **Tier canonicalization**: once at construction + once at routing entry (`canonical_tier`), zero-alloc compare in `matches`.
- **Free risk premium**: −0.15 on free-arm UCB; per-tier forgetting (free every 1k updates, others every 10k).

### 2. Telemetry & warehouse pump semantics

- Publish: MPSC cap 10k; numbered `event_id` `{ms}-{pid}-{seq}` (payload-level idempotency key); full/closed counting via `channel_dropped`.
- Flush: batched pipe (empty pipe never queried); XADD auto-ID + `MAXLEN ~100k`; one retry after 100ms, then whole-batch `telemetry_dropped_total` count.
- Pump: dedicated consumer group + 60s pending reclaim + batch 5000/1s + ack-on-success + poison/duplicate instant-ack; hold replay bypasses the dedup window (BUG-5-1); CH outage holds ack with bounded memory, catches up on recovery.

### 3. Breaker & arbitrage thresholds

| Signal | Threshold | Action |
|--------|-----------|--------|
| Upstream 429 / 403 / 502,504 | — | quarantine 60s / 600s / 30s |
| Paid SLA | <80 / >95 | derate to 0 / restore 100 (hold between) |
| Free pool success | <50 / 50–80 / ≥80 | cut 0.0 / half 0.5 / hold (never auto-up; `scale` floored at max(1)) |
| Free health | EWMA α=0.3 success × latency penalty | weight 1..20 (trusted cap 20; streak≥3 + Elite + latency<1500ms) |
| Failure backoff | 60s×2ⁿ, capped 1h | kept within TTL, auto-recover |
| Source breaker | 3 straight zero-yield rounds (incl. transport errors; 304 exempt) | suspend, probe every 3rd tick |

### 4. Tenants & billing

- Order: identity → active → balance (≤0 → 402) → QPS → concurrency; `burst=qps/10≥1`.
- Concurrency slots: mistaken release dropped + warn (never wraps); failed-attempt bytes zeroed before retry, only the last attempt metered.
- Prices: DC $0.2 / Res $3 / Mobile $15 / Free $0 per GB; one charge may go negative, next request blocked.

### 5. Concurrency discipline

- `parking_lot`/DashMap guards never cross `await` (clone-then-release); single nesting direction session→quarantine, deadlock-free.
- FreePool semaphore permits held across await (TCP 50 / FullCheck 20); prewarmer link cap 100; prober 20 + 10min idle client eviction.
- Background CB/sink/arbitrage/free_pool supervised (panic/exit backoff 1s→60s restart + counting); release uses `unwind` (abort would defeat the supervisor).
- Zero production `unwrap/expect` (only three startup fail-fasts in main); division-by-zero/`as` casts audited bounded.

### 6. Environment variables (defaults in `main.rs`, examples in compose)

| Key | Default | Notes |
|-----|---------|-------|
| `GATEWAY_ADDR` / `METRICS_ADDR` | `0.0.0.0:8080` / `127.0.0.1:9091` | listen addresses |
| `REDIS_URL` | `redis://127.0.0.1:6379/` | degraded (log-only telemetry) when missing |
| `CLICKHOUSE_URL/_USER/_PASSWORD/_DB` | `http://127.0.0.1:8123/proxy/123456/proxy` | hold weights when missing |
| `REQUIRE_API_KEY` | `0` | `1` → headerless 403 |
| `PROBE/PREWARM/SWEEP/ARBITRAGE_INTERVAL_SECS` | `60/30/60/60` | background cadences (0–5s staggered start) |
| `SOCKS_BRIDGE_TIMEOUT_SECS` / `SOCKS_MAX_BODY_BYTES` | `20` / `10485760` | translation bridge |
| `GEOIP_MMDB_PATH` / `GEOIP_ENFORCE_MISMATCH` | empty / `0` | Disabled without DB; enforcement off |
| `FREE_ENABLED` | `0` | second-line master switch |
| `FREE_API/HTML/GITHUB_URLS` | Geonode limit=100 / fpl / clarketm raw | http/https only (SSRF filter) |
| `FREE_FETCH/TTL/VERIFY_TIMEOUT_SECS` | `600/1800/3` | TTL clamped at 30 days |
| `FREE_MAX_LATENCY_MS` | `3000` | TCP gate + EWMA neutral point |
| `FREE_MAX/FULL_CONCURRENT` | `50/20` | verification concurrency |
| `FREE_MAX_NODES` | `2000` | capacity cap |
| `FREE_FULL_CHECK_URL` | `https://httpbin.org` | must be https |
| `FREE_REQUIRE_ELITE` | `0` | `1` → Elite only |
| `FREE_SOURCE_MAX_ZERO_CYCLES` / `FREE_SUSPEND_RETRY_EVERY` | `3/3` | source breaker |

### 7. Gates (see `CONTRIBUTING.md`)

`fmt --check` clean + `clippy -D warnings` zero + `test` 156/4 ignored + `--ignored` 4 true passes (PONG/Ok first, no SKIP) + `bench --no-run` + release bandit <200ns.
