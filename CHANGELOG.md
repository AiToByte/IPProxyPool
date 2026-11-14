# Changelog

> 版本政策：本仓遵循 [语义化版本 2.0.0](https://semver.org/lang/zh-CN/)。
> `0.x` 阶段 minor 即 breaking（按 semver 规定），1.0 之前不承诺 API 稳定。
>
> - **何时 bump**：破坏性变更 → minor+1（如 0.1.0 → 0.2.0）；新功能（兼容）→
>   minor+1 同样处理（0.x 无 patch 位可用）；纯修复 → patch+1。
>   0.x 阶段不区分 minor/patch 的兼容语义，统一按"是否破坏"决定。
> - **谁来 bump**：改 `gateway/Cargo.toml` 的 `version` **同时**在本文件加
>   `## [x.y.z]` 小节（由 `tools/check_changelog.py` 在 CI 强制，缺小节即红）。
> - **tag 规则**：bump 提交后打 `vX.Y.Z` tag（如 `v0.2.0`）并推送。
>   本仓此前从未切过 release、无 tag（实测 `git tag` 为空），故历史版本
>   全部记在 `## [0.1.0]` 下，不补 tag、不伪造 release 记录。
> - **下一次 bump**：pingora 0.6 → 0.9 框架升级落地时 → `0.2.0`
>   （见 `docs/SUPPLY_CHAIN.md` §3）。

格式遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。
`Unreleased` 节记录已合并但未发布的内容；发布时将其改名为版本号。

---

## [Unreleased]

### Security

- **出站 TLS 校验改为显式失败关闭（步骤 71）**：源码级取证发现 pingora-core 0.9.0
  的 rustls 连接器在 `HttpPeer` 的 SNI 为空、或 `verify_cert=false` 时切到
  `VerificationMode::SkipAll`（`verify_server_cert` 直接放行，证书链与主机名全不验），
  且生产构建可达；而 `docs/OPERATION.md` 与 `gateway/upstream.rs` 的注释却写着
  「rustls 后端关闭校验无效」——**这句是错的，且是最危险的那类错**（让人误以为
  天然安全）。`build_http_peer` 现在显式固定 `verify_cert=true` /
  `verify_hostname=true`，并在 TLS peer 的 SNI 为空、全空白或含控制字符时返回 400；
  明文 peer 行为不变。新增 3 条单测（含 RED 证据与变异复验），更正运维文档的 TLS
  段，并把一处描述「已删除的跳过校验分支」的旧测试名改回真实行为。
  未验证：真实 TLS 握手下的 fail-open 活流量复现；Host 含端口的 SNI 形态未改。
- 升级 `rustls-webpki 0.103.15→0.103.16`（RUSTSEC-2026-0382：证书校验可能接受
  不受信任的中间证书）、`webpki-roots 1.0.9→1.0.10`（RUSTSEC-2026-0381：
  内置根证书信任了已撤销的 emSign 中间证书）与 `tokio 1.53.1→1.53.3`
  （RUSTSEC-2026-0386：`AsyncFd` 构造违反 I/O safety，`cargo deny` 判为
  `unsound`）。仅改 `Cargo.lock`，无 API 变更；本机 `cargo audit --no-fetch`
  由 2 vulnerabilities 归零、`cargo deny check` 四项全 ok，326 单测/clippy 仍绿。
  （该条为**当时**的验证记录，数字保持史实；当前基线见 `docs/QUALITY_BASELINE.md`。）

### Fixed

- **节点落盘在 async worker 线程上做 fsync**（OPT-R18 P2-4）：`NodeRegistry`
  的三个变更方法（`add`/`remove`/`set_group_weight`）原先是**同步**的，而唯一
  调用方是 Admin 的 axum handler（async），于是 `write_all + sync_all`（以及
  Unix 上的 `dir.sync_all()`）直接在 tokio worker 上执行。单次 fsync 在机械盘/
  网络盘上可达数十毫秒，而 worker 数 = CPU 核数，**几次并发的节点增删就能把
  数据面一起卡住**（Admin 与数据面共享同一 runtime）。修法：三个方法改 `async`，
  序列化 + 写 + fsync 整体搬进 `spawn_blocking`。
  重构中必须解决一个约束：`parking_lot::Mutex` 不得跨 await 持有，而"取锁算完
  就放锁、落盘后再取锁提交"会让两个并发 `add` 基于同一份旧清单各算一份 next
  ⇒ **lost update**。故新增 `write_gate`（`tokio::sync::Mutex`）串行化整个
  「读-改-写」序列，`nodes` 锁退化为只保护内存读写。副作用是**读路径变好**：
  `snapshot()` 原先在落盘期间会一直等锁，现在不会。
  两条新测试分别证明"落盘确实离开 worker"（把 `max_blocking_threads` 压到 1
  并用钉子占住，`add()` 必须处于 pending）与"并发不丢更新"（16 并发 add，
  断言最终 16 个）；两处修复各做了一次回退，均按预期判红（后者报
  `left: 1, right: 16`）。
- **`tmp_path` 的临时文件名只在进程内唯一**：原实现用 `pid` 单维度，同进程内
  两次并发写会指向同一个临时文件，先完成者的 `rename` 把它搬走、后者随即报
  `os error 2`。`write_gate` 恰好掩盖了这个耦合——原子写本身不该依赖外部锁
  才正确。改为 `pid + 进程内单调序号`，并让测试的 `cleanup` 扫描同前缀残留。
  该缺陷是**摘掉 `write_gate` 做变异测试时立刻暴露的**，属本次重构顺带修掉的
  既有隐患。
- **主动健康用「近似计数器」当正确性闸门**（OPT-R18，步骤 54 收口前发现）：
  `is_active_unhealthy` 曾以 `active_unhealthy_count == 0` 做热路径短路，而该计数器
  由各写入点**增量维护**：`sweep_active_health` 把「过期条目数」累加到循环外统一
  递减，递减时锁已释放，窗口期内条目可能已被 `record_probe_failure` 重新标记
  ⇒ 计数器**低估**。三重后果：① 短路误判「无人被摘除」，坏节点继续接流量（正是
  步骤 54 要消灭的行为）；② 同一计数器被安全阀复用，低估会放行「摘到全池 503」；
  ③ gauge 与真实状态漂移，排障时指标显示 0。
  处置：**删除派生状态**——热路径改为直接查表（表即真相），`active_unhealthy_len`
  改为全表精确扫描（仅每 tick 一次），安全阀改用「同一次池快照 + 同一时刻」的精确
  计数，并把摘除判定串行在 `probe_mark_lock` 上（跨条目事实 + 落状态需原子）。
  同时修掉该文件此前**无法编译**的 `AtomicUsize::try_update`（Rust 1.89 仍为
  unstable feature）——该调用正是旧计数器实现的一部分。
- **主动健康探测的日志风暴与预算挤占**（OPT-R18）：步骤 54 把探测范围改成全池后，
  免费池数百个死地址每 tick 各打一条 `warn`。处置：付费/托管节点 100% 探测，免费池
  按**轮转游标**抽样（`PROBE_FREE_SAMPLE_PERCENT`，默认 25%，`ceil(100/pct)` 轮内
  全覆盖；不用随机抽样——随机不保证覆盖，尾部节点会永远没有恢复机会）；日志改为
  「状态迁移逐条 warn/info，常规结果只计数、每 tick 一行汇总」，付费节点故障单独
  计数并在汇总行升 `warn`。
- **`trace_id` 只进遥测、不进日志**（OPT-R18）：`gateway.rs` 中 5 处 `trace_id` 无
  一处出现在 `log::` 宏里，导致「ClickHouse 查到一条 502，日志里对不上是哪行」。
  已补到数据面 6 类关键日志（每请求汇总行、凭据护栏 403、上游重试、SOCKS 桥三类失败），
  日志与遥测可按同一键互查。

- **Admin API 的「声明即失实」**（OPT-R17 复核实证）：`gateway/Cargo.toml` 声明
  `tower-http` 的 `cors`/`trace`/`timeout` 三个 feature，而 `src/` 全仓
  **零引用**；`admin.rs::router` 的文档注释更声称「含 CORS + 超时」——
  **两者都不存在**。三重后果：读者以为跨域已放开（实则 UI 只能靠 Vite
  proxy/同域反代）、以为有请求超时（实则慢请求可长期占满连接）、以为有
  访问审计（实则改权重/隔离/余额这类写操作**无任何轨迹**）。
  处置：
  - **真正补上 `TimeoutLayer`**（`ADMIN_TIMEOUT_SECS = 30`，套在认证中间件
    外层，使未认证的挂起请求同样受约束），新增 3 个测试锁层序与错误体契约；
  - **新增 `audit_logging` 中间件**（用 `log` 而非 `TraceLayer`：后者发的是
    `tracing` 事件，本项目装的是 `env_logger`，无 subscriber ⇒ 会得到
    「加了审计却一条不打印」的静默失效）。记录方法/路径/状态码/耗时，
    4xx/5xx 走 `warn`。**只记是否带 Key，绝不记值**（日志会进 `log/` 并被
    备份，写入密钥等于新建泄漏路径）；
  - **移除 `cors` 与 `trace` feature**，让依赖清单与真实代码一致。
    不补 CORS 是安全选择：`X-Admin-Key` 头认证 + permissive CORS 会让
    任意站点的 JS 都能发起带凭据的跨域请求。
  - 副作用：Cargo.lock 中 `tower-http` 的 `tracing` 传递依赖随之移除
    （实证 `tracing` 本就是死重量，非本仓其它代码所需）。
- **告警投递链路的注释与实际相反**（OPT-R17）：`docker-compose.yml` 曾注释着
  「取消注释＋重启 prom 即投递」并挂载 `alertmanager.yml` 到 prometheus
  容器——**该挂载无效且多余**（Prometheus 不读它；收件人配置由 Alertmanager
  自己读 `/etc/alertmanager/alertmanager.yml`）。误导性注释与挂载均已移除。
  澄清：`prometheus.yml` 的 `alerting:` 块**早已接好**，从不需要改动。
- **Alertmanager 默认不启动**：此前 `profiles: ["observability"]` 使
  `docker compose up -d` 不起它，而 `prometheus.yml` 的 `alerting:` 块
  无条件指向 `alertmanager:9093` ⇒ 默认部署下告警链路是断的。已移出
  profile，与 Redis/Prom 同级默认启动。结果是**链路默认可达、投递默认关闭**
  （receiver 仍是刻意的 `empty`）：运维可在 Alertmanager UI 分组/silence/ack，
  而非只能去 Prometheus UI 干看。
- **`docs/COMPONENTS.md` 的「行数」列已删除**（OPT-R18 P1-4）：该列任何一行
  代码改动都会过期，设精确门禁则每个 PR 假红、不设则长期是假的——保留一个
  无法维护的数字等于保留一句对外失实。体量信号改由「单测」承担（被门禁守住，
  且合计与 `QUALITY_BASELINE.md` 自洽）。同时修正 `vendor_arbitrage.rs`
  中英两行行数互相矛盾（208 / 218）、`socks_handshake.rs` 编造的 API 名
  （`socks_handshake` → 真实 `establish` / `greet_only`）、以及补登 5 个
  漏掉的模块。
- **`trace_id` 在 ClickHouse 落地链路被完整丢弃（OPT-R18 P1-1 的失实部分）**：
  代码注释与计划文档都声称「trace_id 与 CH 遥测**同键互查**」，实测发现只落地了
  一半——`TelemetryEvent.trace_id` 一直有（随 Redis Stream 的 JSON payload 传递），
  但 `TelemetryRow` **没有**对应字段、CH 表也**没有**该列 ⇒ `From<&TelemetryEvent>`
  转换时该字段被完整丢弃。于是「日志侧有 trace_id、仓库侧没有」，
  **排障时「CH 里有这条 502、日志里对不上是哪行」这个最初要解决的问题依然存在**。
  修法三处：① `001_schema.sql` 加 `trace_id String` 列（**不用** LowCardinality
  ——它是每请求一值的高基数列，字典编码只会退化）+ `bloom_filter(0.001)`
  GRANULARITY 1 点查索引（不加则按 trace_id 查 90 天表退化为分区全扫描）；
  ② `TelemetryRow.trace_id` + `From` 里**原样透传**（不得重新生成，否则两边
  不是同一个 ID）；③ `manual/003_trace_id.sql` 供存量部署就地 `ADD COLUMN`。
  新增 3 条单测：透传（**原测试写的是 `String::new()`，空串对空串，即使字段被
  完全丢弃也照样通过——零鉴别力**，已改为非空值）、缺字段容错、以及
  `row_columns_match_ddl_columns`——用 `include_str!` 读**真实 DDL** 并解析列名，
  与 `TelemetryRow::COLUMN_NAMES` 做**双向集合相等**断言。
  **这条断言初版是错的、已纠正**：初版断言「字段**顺序**必须与 DDL 列序一致」，
  理由写「顺序错位会静默串列」——实测推翻（把 `trace_id` 移到 `client_ip` 之后，
  live 插入与点查**照常成功**）。根因是 `insert` 拼的是
  `INSERT INTO t(col1, col2, ...) FORMAT RowBinary`，**列名显式列出、服务端按名映射**，
  故顺序无关。真正的不对称在**集合**上（CH 24 实测）：
  结构体有、DDL 没有 ⇒ **响亮报错** `NO_SUCH_COLUMN_IN_TABLE`；
  DDL 有、结构体没有 ⇒ **静默补默认值**（`String` → `''`）、插入不报错、该列永远查不到
  ——后者正是本轮 `trace_id` 缺陷的形态，故必须双向断言。
  三处变异实测：DDL 多一列 → 判红（指出「静默补默认值」）；DDL 删掉 `trace_id`
  → 判红（指出「插入会被拒」）；**结构体列序错位 → 通过**（证明误报已消除）。
  另加 `live_trace_id_roundtrip`（需真实 CH）验证「按 trace_id 点查恰好 1 行」——
  该条已在 CH 24 上**实跑通过**（见下）。

  **活流量实证（2026-10-31，CH 24.10.2 + Redis 7）**：① 对存量表执行
  `manual/003_trace_id.sql`：列出现在 `event_time` 之后、`bloom_filter` 索引就位、
  **重复执行无副作用**（`ADD ... IF NOT EXISTS` 幂等）；② `-- --ignored
  --test-threads=1` 跑全部 9 条活流量测试：**9 passed / 0 failed / 0 SKIP**，
  其中 `live_trace_id_roundtrip` 真插入真点查；③ 测试后 `provider LIKE 'livetest%'`
  残留 **0 行**（清理有效）。
  **全新部署路径也已实证**：起一个空 volume 的 CH 容器挂 `deploy/clickhouse/`，
  entrypoint 自己打印
  `running /docker-entrypoint-initdb.d/001_schema.sql` 与
  `ignoring /docker-entrypoint-initdb.d/manual` —— 目录约定被官方实现**直接验证**；
  产出的表只有 `proxy_telemetry_log`（**无 `_old`/`_v2` 垃圾表**），列含 `trace_id`、
  三个索引齐备；对这**全新**容器跑同一套 9 条活流量测试同样 **9 passed / 0 SKIP**。

- **`002_reorder.sql` 声称手动、实际每次全新部署都自动执行**：该文件原在
  `deploy/clickhouse/` 顶层，文件头写着「不会被 `docker-entrypoint-initdb.d`
  自动执行（它只跑 `001_schema.sql`）」——**这句是错的**。CH 官方
  `/entrypoint.sh` 的实现是 `for f in /docker-entrypoint-initdb.d/*` 后按
  `*.sql` 分支执行，即**顶层所有 `.sql` 全部自动跑**（子目录不递归），
  而 `docker-compose.yml` 把整个 `deploy/clickhouse/` 挂到该目录。
  危害不止「多跑一次」：它是破坏性的（`CREATE _v2` → `INSERT SELECT` 全表复制
  → `RENAME`），且 `_v2` 的列清单是**写死的快照** ⇒ 此后 `001` 新增的列
  （如本次的 `trace_id`）会在 `RENAME` 后被**覆盖丢失**，得到一个缺列的仓库表
  而代码侧照常写入。处置：移入 `deploy/clickhouse/manual/`（子目录不递归），
  并在 `001_schema.sql` 末尾写下**目录约定**（顶层只放幂等的权威 schema）。
  同时给 `tools/check_observability_truth.py` 加两条断言并实测判红：
  **C** 顶层只允许 `001_schema.sql`（白名单，不是黑名单——黑名单只拦已知的那一个）；
  **D** 迁移脚本与权威 schema 对同一列的类型必须逐字一致（`String` vs
  `LowCardinality(String)` 都能建表成功，却产出两种不同 schema，只在
  「新环境正常、老环境异常」时暴露）。


- **zstd 生态 4 条公告：逐条做可达性论证后**显式豁免**（不是"为了变绿加 ignore"）**。
  上轮 `check_supply_chain.py` 首次真正跑通（此前每轮都因离线拉不到 RustSec DB 而
  跳过——**门禁"绿"是因为没跑，不是因为没问题**），暴露 4 条：`RUSTSEC-2026-0391`
  （`zstd::stream::raw::{Encoder,Decoder}::with_context` 数据跨流泄漏）、
  `RUSTSEC-2026-0398`（`zstd_safe::Seekable::decompress` 越界设 `Vec` 长度）、
  `RUSTSEC-2026-0403`（`DCtx::decompress_stream` 旧式帧失败时暴露未初始化内存）、
  `RUSTSEC-2026-0410`（zstd-sys 的 wasm32 分配器 shim）。
  **不可修**：两个 zstd 版本（0.13.3 ← `pingora-header-serde` 锁 `zstd ^0.13.1`
  + `zstd-safe ^7.1.0`；0.14.0 ← `pingora-core`）都在树里，而公告的补丁只有
  `zstd >=0.14.3` / `zstd-safe >=8.2.0` / `zstd-sys >=2.1.2`——**0.13.x 与 7.x
  线不存在修复**；`cargo search pingora` 实测**最新就是 0.9.0**（即当前版本），
  没有含修复的 pingora 可升。
  **不可达**（源码级调用图，`grep` pingora 全树）：三个受影响符号命中数均为 0
  ——pingora-header-serde 只用 `zstd::dict` + `CCtx/DCtx::create` +
  `decompress_to_buffer`；0410 仅 wasm32，本仓只构建 x86_64；本仓代码对 zstd
  零引用。`deny.toml` 的 `ignore` 每条都写了这两类理由，并靠
  `unused-ignored-advisory = "deny"` 保证**上游一修就自动报错**、豁免不会变僵尸
  （实测注入不命中的 advisory ⇒ `error[advisory-not-detected]` 判红）。

- **`check_supply_chain.py` 把「不确定」变成「确定」**（三次迭代，前两次都错了）。
  第一版裸跑 `cargo audit` 非 0 即红 ⇒ 离线机器上**永远红**；第二版用字符串匹配
  网络错误 ⇒ **不稳定**（同机同代码实测时绿时红）。现行方案从根上消掉不确定性：
  ① `--no-fetch --stale` 彻底不联网；② `-d` 显式指向 **cargo-deny 的那份库**——
  两个工具的默认库**不是同一个**（deny 用 `~/.cargo/advisory-dbs/advisory-db-<hash>`，
  audit 默认 `~/.cargo/advisory-db`，本机实测后者是**旧快照**、连 0391 都没有），
  不指定的话 audit 会给出"0 漏洞"的**假绿**，比不跑更危险；③ `--ignore` 列表
  **从 `deny.toml` 读**（audit 不读 deny.toml，两份豁免清单必然漂移）。
  离线后失败原因只剩"真的发现漏洞"，非 0 即判红，不必再猜字符串；库不存在则显式
  SKIP 并说明"本次未做 audit 侧校验"。验证：连续 3 次 exit 0；删掉一条 ignore
  ⇒ 判红并指名 `RUSTSEC-2026-0410`。


- **`/metrics` 输出违反 Prometheus 文本格式 ⇒ 抓取端解析失败、整次抓取作废**。
  `proxy_stage_duration_ms` 的 `# HELP` / `# TYPE` 被写在 per-stage 循环**内部**，
  3 个 stage（connect/ttfb/transfer）输出 3 份。文本格式规定同一指标名只能有**一条**
  `# HELP`，违反时抓取端报 `second HELP line for metric name` 并**丢弃整次抓取**
  —— 后果不是丢这一个指标，而是**该实例所有指标一起丢**（等于可观测性整体失效）。
  **官方工具实测确认**：把 `render()` 的输出喂给 `promtool check metrics`（v2.53.0）
  报 `text format parsing error in line 76: second HELP line for metric name
  "proxy_stage_duration_ms"`；把 HELP/TYPE 提到循环外后同一命令**解析通过**，
  只剩 `_total` 后缀、`_ms` 缩写等命名 lint——那些是既有对外契约（Grafana 面板与
  告警规则依赖），改名会破坏下游，故**不动**。
  新增 3 条测试：`golden_render_full_exposition`（整篇输出**逐字节**快照，215 行）、
  `each_metric_has_single_help_and_type`（重复头行断言）与
  `render_is_valid_prometheus_text`（类型声明、样本归属、histogram 必需样本、重复序列、
  标签名）。第三条又抓出第二处真实错配：`free_pool_exit_total` 样本（也是 Grafana
  Dashboard 查询的名字）却挂在 `free_exit_total` 的 HELP/TYPE 下；已把头行改为
  `free_pool_exit_total`，并将 HELP/TYPE 名称集合一致、每条样本都有对应 TYPE 固化为断言。
  黄金快照、单头断言与完整校验器职责互补。前两条变异验证已做（改一个字符 / 塞回重复
  HELP ⇒ 判红）；新校验器首次运行即对该错配判红，修正后专项测试通过。

### Changed

- **`metrics/render.rs` 的 `render()` 由 538 行单函数拆为 7 段渲染器**：现只剩
  「按序调用」10 行，7 段各一个私有方法（`render_requests_and_probe` /
  `render_latency` / `render_breaker` / `render_telemetry` /
  `render_free_pool_supply` / `render_geoip` / `render_free_pool_funnel`）。
  **段序即输出序**——Prometheus 抓取端对同族样本重复敏感，且黄金快照逐字节断言
  整篇输出，故切分严格按**连续行段**、不重排任何语句。
  这是**行为敏感重构**，做法是先用黄金快照把当前输出钉住、再动刀：拆完快照
  **一字未改地通过**，证明输出逐字节不变。同时 `cargo llvm-cov` 从 73.87% 升到
  **74.15%**（新增的两条测试覆盖了 render 更多路径）。


- **`admin` 模块按职责拆为 7 个文件（OPT-R18 P2-1 第五批，P2-1 收官）**：原
  `admin.rs` **1791 行**，结构上是"**一个 800 行的 `build_router_with`**"——路由表、
  中间件、16 个 REST 处理器全挤在同一段区间，改一个端点要在 1200 行里定位。拆为
  父模块 `admin/mod.rs`（287：状态＋路由装配＋OpenAPI 聚合＋超时常量）＋
  `dto.rs`（235）＋`mw.rs`（102：审计/认证/404 三层中间件）＋
  `handlers_node.rs`（369）＋`handlers_tenant.rs`（146）＋`handlers_ops.rs`（138）＋
  `tests.rs`（604）。**纯搬移**：单测 **357 / 9 ignored 与拆分前完全一致**，
  fmt/clippy 全绿。
  **按"资源"切处理器而非按 HTTP 方法**：查"删除节点会发生什么"时要一次看到
  `delete_node_handler` 与相邻的 `delete_node_quarantine_handler`——它们共享同一套
  「先 `get_node_by_addr` 判存在（否则 404）再动作」的前置逻辑；按方法切会把
  GET/PATCH/DELETE 拆到三个文件、来回跳。
  两处实测踩坑已写入文档：① 中间件模块叫 **`mw`** 而非 `middleware`——`axum::middleware`
  已在父模块导入，同名子模块冲突（E0255）；② `pub(super)` 项**不能** `pub use` 再导出
  （E0364），故 `node_view`/`registry_error_response` 由处理器侧显式
  `use super::dto::{…}`。

  **拆出一个会让门禁"静默失明"的缺陷（本次最重要的收获）**：
  `check_observability_truth.py` 用 `SRC_DIR.glob("*.rs")` **只扫顶层**，于是 admin 变成
  目录模块后它**看不见 `admin/mod.rs` 里的 `TimeoutLayer`**，立刻误报「`tower-http/timeout`
  声明了但 src 里找不到」。假红本身好办（改 `rglob` 即绿），真正危险的是**同一缺陷的
  反向作用**：若某符号**只在子模块里用到**，门会以为"没人用"——那就是**假绿**，
  是本仓最忌讳的静默失效。已改 `rglob` 并写明这层理由。
  同一根因（门禁只认 `foo.rs`、不认 `foo/mod.rs`）至此已在 **5 个门禁**上各犯一次：
  `check_components_docs`、`check_free_pool_contract`、`check_promql_metrics`、
  `check_observability_truth`、`check_admin_routes_documented`。前者已收敛到
  `tools/_rustsrc.py`；本次把剩下两个也修好（`rglob` / `read_admin_source` 兼容两种布局），
  并**变异验证**：注释掉一条 `.route(...)` ⇒ 判红并指名 `GET /api/v1/freepool/summary`。

  **覆盖率口径（第五次复现同一现象）**：74.97% → **73.87%**（−1.10pp），生产代码覆盖
  未变。离开测量集 470 行、其中 **100.2%**（471 行）原本"覆盖"——即 `admin.rs` 内联的
  测试；>100% 是整数取整 + 拆分后内联决策变化的 1 行噪声。已汇总进
  `docs/COVERAGE_BASELINE.md` §4（五次：`router`/`free_pool`/`gateway`/`metrics`/`admin`）。


- **`metrics` 模块按数据流方向拆为 4 个文件（OPT-R18 P2-1 第四批）**：原
  `metrics.rs` **2097 行**。拆为父模块 `metrics/mod.rs`（438：常量＋`MetricsRegistry`
  结构体＋快照类型＋构造器＋`Default`）＋`record.rs`（519：**写入侧**
  `observe*`/`note_*`/`set_*`）＋`render.rs`（611：**读出侧** `render()` 与
  `serve_metrics()`）＋`tests.rs`（589）。**纯搬移**：单测 **357 / 9 ignored 与
  拆分前完全一致**，fmt/clippy 全绿。
  **切分轴是数据流方向，不是指标族**：指标族的记录方法在 impl 里是**交织**的
  （免费线那组散落在 6 个不相邻区间），按族切会把一个 impl 拆成十几段残片，
  读者要跳 6 个文件才能看完一个指标族的生命周期。而「写入 vs 读出」是一刀干净的
  切法：写侧只改原子量、读侧只读原子量，两侧唯一的契约是**指标名与标签集**
  （写进去的每个字段必须在 `render()` 里有对应 `# HELP`/`# TYPE` 行，否则指标
  写了却抓不到——静默失效）。

  **顺带归一一处约定不一致**：原文件有 **4 个 `#[test]` 直接写在模块级**
  （`opt_r9_c2_*`，不在 `mod tests` 内），与其余 40 个测试两套写法。两者都合法
  （`#[test]` 隐含 `cfg(test)` 门控），但同一文件两套约定会让读者以为"模块级那些
  是特殊测试"。拆文件时统一收进 `metrics/tests.rs`。

  **新增共享助手 `tools/_rustsrc.py`，终结一类反复出现的门禁缺陷**：本仓已把
  `router`/`free_pool`/`gateway`/`metrics` 四个巨型文件改成**目录模块**，而门禁
  脚本一律写死 `gateway/src/xxx.rs` 单文件路径 ⇒ **每拆一个模块就有一批门禁报
  "缺文件"**：`check_components_docs.py` 只 glob 顶层 `*.rs`（目录模块整个从模块表
  消失）、`check_free_pool_contract.py` 写死 `free_pool.rs`、`check_promql_metrics.py`
  写死 `metrics.rs`——**同一根因已犯三次**（"模块"在 Rust 里有两种合法物理形态，
  脚本只认一种）。现把"读模块源码"收敛到 `_rustsrc.module_source()`，新增门禁一律
  走它；并已用变异验证它**真的读子模块**（把 `render.rs` 里一个 `# HELP` 改名 ⇒
  `check_promql_metrics` 判红并指名受影响规则）。

  **覆盖率口径（第四次复现同一现象）**：75.80% → **74.97%**（−0.83pp），生产代码
  覆盖未变。离开测量集 388 行、其中 **388 行（100.0%）** 原本"覆盖"——即
  `metrics.rs` 内联的测试。前三次是 `router`/`free_pool`/`gateway`（96.5%~97.9%）；
  这次恰好 100%，因为离开的**全部**是测试代码、没夹带任何业务行。已汇总进
  `docs/COVERAGE_BASELINE.md` §4。

  **已知项（下一步）**：`render()` 单函数 **538 行**（逐族 `push_str`）。机械上可拆
  成 `render_<family>(&self, out: &mut String)`，但输出**行序**被部分测试逐字节
  断言，拆它属行为敏感重构，故本轮只做纯搬移、不混入。


- **`gateway` 模块按职责拆为 7 个文件（OPT-R18 P2-1 第三批）**：原 `gateway.rs`
  **2275 行**，把四类职责混在一起（鉴权/分档的纯判定、LinUCB 选路上下文、
  上游 peer 装配、SOCKS 桥转发）外加 40 个单测。拆为父模块 `gateway/mod.rs`
  （结构体＋追踪 ID＋`parse_routing_spec`＋再导出，200 行）＋
  `policy.rs`（纯判定 133）＋`bandit.rs`（LinUCB 258）＋`upstream.rs`（上游装配 113）＋
  `socks.rs`（SOCKS 桥 248）＋`hooks.rs`（`impl ProxyHttp` 9 个回调 383）＋
  `tests.rs`（1027）。**纯搬移**：条目逐字迁移，单测数 **357 / 9 ignored 与拆分前
  完全一致**，fmt/clippy 全绿。
  `impl SmartProxyGateway` 按职责分散在 `bandit.rs` 与 `socks.rs` 两处（Rust 允许
  同一类型的固有 impl 分布在同一 crate 的任意模块）——它们服务的是「选路」与
  「SOCKS 桥」两件不同的事，就近放置比硬塞进 `mod.rs` 更可读。构造函数不在本模块，
  由 `main.rs` 用结构体字面量装配。少数跨模块调用的私有项放宽为 `pub(super)`
  （`select_bandit_node*`/`pick_socks_candidate`/`serve_via_socks`/
  `build_http_peer`/`bridge_url_for`），`is_tls_for_node` 的再导出加
  `#[cfg_attr(not(test), allow(unused_imports))]`（消费方只有 `tests.rs`）。

  **顺带修掉一处真实的文档错挂**：原文件里 `build_http_peer` 的说明
  （「P2：HttpPeer 装配（含 socks 守卫）」）被粘在 `is_tls_for_node` 上方 ⇒
  `is_tls_for_node` 带着**完全无关**的说明（讲的是 socks 守卫与 503），而
  `build_http_peer` 反而**没有文档**。读者看 `is_tls_for_node` 会以为它管 socks 守卫。
  搬迁时已各归各位。这类错挂不会被任何测试或门禁发现——注释不参与编译，
  只能靠搬迁时逐段核对。

  **同时修正了我上一轮引入的一个不一致**：`free_pool/` 的 6 个文件被
  `Path::write_text` 写成了 **CRLF**（Python 在 Windows 上把 `\n` 转成 `\r\n`），
  而工作区其余文件多为 LF。本轮已把 `gateway/` 与 `free_pool/` 下由脚本生成的
  文件统一规范化为 LF。
  实测当前分布：`gateway/src` 共 42 个 `.rs`，**LF 34 / CRLF 8**。那 8 个
  （`router/{select,health,quarantine,node_mgmt,buckets}.rs`、`janitor.rs`、
  `socks_handshake.rs`、`tenant.rs`）是**更早的脚本拆分**留下的同类问题，
  本步未触碰；**刻意不做全仓规范化**——那会变成一个纯空白、动 8 个无关文件的
  diff，而 `.gitattributes` 的 `* text=auto` 在入库时本来就会归一。留作已知项。

  **覆盖率口径（第三次复现同一现象）**：总体 76.87% → **75.80%**（−1.07pp），
  但生产代码覆盖情况未变。离开测量集 632 行、其中 610 行（96.5%）原本"覆盖"
  ——即 `gateway.rs` 内联的 40 个测试。与前两次（`router`、`free_pool`，
  比例 97.9%）同一特征，已在 `docs/COVERAGE_BASELINE.md` §4 汇总记录：
  **拆分模块后覆盖率下移 1~2pp，第一反应应是查 `git diff` 里有没有 `mod tests`
  迁出，而不是去找"哪段代码没测"**。


- **`free_pool.rs` 按职责拆为 6 个文件（OPT-R18 P2-1 第二批）**：原文件 **3727 行**
  （抓取源＋两级质检＋注册表状态＋后台 worker 全混在一起）。拆为父模块
  `free_pool/mod.rs`（类型定义＋`shared_client`＋再导出，213 行）＋
  `sources.rs`（三类抓取源＋礼貌轮询缓存＋`fetch_all`，481）＋
  `verify.rs`（`Verifier`/`FullChecker`/匿名度分级，340）＋
  `registry.rs`（`Health`/`Registry`/`SourceGuard`/intake 配额，517）＋
  `worker.rs`（env 组装＋周期驱动＋geo 裁决＋Router 合并，520）＋
  `tests.rs`（1763）。**纯搬移**：条目逐字迁移，行为零变更，单测数
  **357 / 9 ignored 与拆分前完全一致**。
  与 `router` 拆分的两点差异（都踩过）：① 这里用 `free_pool/mod.rs` 而非
  `free_pool.rs` + `free_pool/` 并存——父模块只剩类型与再导出，独立 `.rs`
  反而多一层跳转；② 搬的是**类型与常量**，必须 `pub use` 再导出才能保住
  `crate::free_pool::X` 路径（`router` 搬固有方法，不需要）；被 `tests.rs`
  独用的再导出在非测试构建下会被判 `unused_imports`，故加
  `#[cfg_attr(not(test), allow(unused_imports))]`（只放宽非测试构建）。
  少数被 `tests.rs` 或兄弟模块用到的私有项（`is_idle_over` 上提到父模块、
  `cap_intake`/`intake_cap_limit`/`baseline_bases`/`apply_geo_verdict`/
  `Registry::entries`/`Entry::{health,node}`/`CHECKER_CLIENT_CAP`/`page_url`/
  `client_for`/`client_count`）放宽为 `pub(super)`——作用域是父模块**及其全部
  后代**，正好覆盖 `free_pool::tests`，不扩大到 crate 级。

  **拆分暴露并修掉了 3 个门禁缺陷**（都不是新代码的 bug，是门禁自身的脆点）：
  ① `check_components_docs.py` 只 glob 顶层 `*.rs` ⇒ 目录模块 `free_pool`
  **整个模块从表里消失**、60+ 个单测不再计入合计（靠"合计 == 基线"这条断言
  抓到，不是靠人眼）。已支持 `foo.rs` 与 `foo/mod.rs` 两种写法；
  ② 同一门禁的 `pick_table()` 只取**第一张**匹配表头的表 ⇒ 第二张子模块表
  （`free_pool` 的）整表被忽略，报「`free_pool/verify.rs` 存在但表里没有它」
  而表里明明写着——**门禁自己产生假红**，比没有门更糟（会被学会忽略）。
  已改为合并全部匹配表；③ `check_coverage_baseline.py` 用**末段文件名**做键
  ⇒ `router/tests.rs` 与 `free_pool/tests.rs` 会归约成同一个 `tests.rs`，
  后写的那份**静默覆盖**前一份、数字直接错且不报错（当时只因 `tests.rs` 不进
  报告才没爆）。已改为取 `src/` 之后的相对路径（`router/health.rs`、
  `free_pool/mod.rs`）。另 `check_free_pool_contract.py` 硬编码单文件路径，
  拆完立刻报"缺文件"，已改为读整个目录（两种布局都支持）。

  **一条测量口径的真相（已写入 `docs/COVERAGE_BASELINE.md` §4）**：拆分后总体
  覆盖率 78.98% → **76.87%**（−2.1pp），但**生产代码的覆盖情况一行没变**。
  原因是 `cargo llvm-cov` 的报告**不含 `tests.rs`**，而内联在业务文件里的
  `#[cfg(test)] mod tests` **会被计入**该文件——测试会执行，于是那段代码近乎
  全绿、凭空抬高数字。实测离开测量集 1364 行、其中 1336 行（97.9%）原本
  "覆盖"，正好是内联测试代码的形状。⇒ 这是数字变**诚实**，不是代码变差；
  反过来说，把测试塞进业务文件能"提高"覆盖率，是"为覆盖而覆盖"的隐蔽形态。

### Added

- **`docs/OPERATION.md` §4.3「按 trace_id 互查日志与仓库」**：给出仓库→日志与
  日志→仓库两个方向的**可执行**命令，并写明三个边界——空串 `trace_id` 表示
  「该行产生于落地之前」而非「trace 丢了」；日志侧有采样（5xx 全量、其余 1/1000）
  故「仓库有、日志没有」是正常现象，反向才要查泵链路；**`trace_id` 由客户端
  可控的 `X-Trace-Id` 沿用，不得用于鉴权/计费/审计定责**（计费与幂等仍以服务端
  发号的 `event_id` 为准）。附「存量部署升级必须先跑 `manual/003_trace_id.sql`」
  的说明与不执行的后果（CH 报错 → `ChSink` 保留 ack 重试 → 数据不丢但仓库停更，
  `telemetry_sink_up{backend="clickhouse"}=0` 告警）。

- **覆盖率门 `--refresh` 一并回写文档数字**：原先刷新基线后文档必然失同步、
  门禁立刻判红，而那个红是「文档没跟上」而非「覆盖率退化」，噪声会稀释门禁
  可信度；把「改 29 行表格」留给人工的另一个结局是人随便填个数、**文档开始骗人**。
  现在 `--refresh` 直接重写 `COVERAGE_BASELINE.md` 的表格与 §1 总体三行
  （机器生成的数字由机器改，§3/§4/§5 的论证文字保持人工维护）。
  同时把 §4 正文里硬编码的百分比改成「引用表格、只讲为什么」——那些数字会随
  每次测量漂移（与 `docs/COMPONENTS.md` 删掉「行数」列是同一条结论：
  维护不了的数字不该写）。基线也新增 `_counts`（count/covered 绝对量），
  让文档能写出「13634 行中 10800 行被覆盖」这种可核对分母的句子。
  文档另记录一条**结构性现象**：新增 `#[ignore]` 的 live 测试会拉低覆盖率
  （函数体默认不执行但计入分母），实测 `analytics.rs` 因新增
  `live_trace_id_roundtrip` 从 65.00% 掉到 57.02%——门禁当时**正确判红**，
  处置是刷新基线并记录原因，而不是删掉那条测试。

- **覆盖率基线门（OPT-R18 P2-5）**：本仓此前有 **355 个单测却没有任何覆盖率
  度量**——单测数量只说明「有多少个测试函数」，不说明「它们覆盖了多少条生产
  代码路径」。最阴险的退化正是**单测数上涨与覆盖率下降同时发生**：新增 200 行
  生产代码 + 0 个测试，单测从 340 涨到 355，全绿——只看单测数的门禁**完全
  看不出来**。
  本次落门分三部分：① `docs/coverage_baseline.json`（机器可读基线，29 个文件，
  `_totals.lines = 79.1043`，即 13553 行中 10721 行被覆盖）；
  ② `docs/COVERAGE_BASELINE.md`（读者看的逐模块表 + 「为什么这么设计」的
  论证）；③ `tools/check_coverage_baseline.py`（门禁本体 + 变异自测）。
  **关键设计：只卡「回退」、不卡「达标」。** 向上没有门槛——绝对门槛会逼出
  「为了点亮某几行而写的测试」，其断言往往无鉴别力；向下有门槛——低于基线超过
  **1.0pp** 判红。容差取 1pp 的理由：0 容差会随编译器版本、泛型单态化数量等
  **非本仓代码**的因素周期性假红（假红门禁会被学会忽略，比没有门更糟），
  而 5pp 足以放过「新增 800 行生产代码、0 个测试」这种真实退化。
  两处**实现期发现并修正的自身缺陷**（都是先写出来、再被自测或实测抓到的）：
  ① **总体覆盖率必须是「按行数加权重算」的 `Σcovered/Σcount`，不是逐文件
  百分比的算术平均**——`main.rs` 有 1737 行而 `fingerprint.rs` 只有 73 行，
  前者占一半代码量，算术平均会让大小文件等权，直接算错；
  ② 门禁曾用 `pct()` 解析基线，但基线 JSON 里存的是**裸浮点数**（实测数据才是
  summary 字典），混用会抛 `AttributeError: 'float' object has no attribute
  'get'` ——门禁**自己先崩**、而不是给出判红结论；已拆成 `pct()`（实测）与
  `base_pct()`（基线）两个函数，并在注释里记下「两者形状不可混用」。
  门禁另有**平台/工具链感知**：覆盖率数字**不是跨工具链、跨平台可比的**
  （单态化数量、内联决策、`cfg(unix)/cfg(windows)` 分支展开都会变），故按
  rustc 版本 + 目标三元组判定可比性——可比才做数值回退判定；不可比时**只**
  校验文件集一致与文档↔JSON 一致，并**显式打印**跳过原因，**绝不静默放行**。
  自测从 3 处注入扩到 **4 处**（新增「不可比模式只跳过数值判定」——锁住「跳过」
  这个行为本身：它**只**能跳过 [B]，若连文件集判定 [A] 也一并跳过，
  「工具链不同」就成了绕过整道门的万能钥匙）。CI 新增独立 `coverage` job
  （真装 `llvm-tools-preview` + `cargo-llvm-cov 0.9.1` 并真跑测量；锁
  `cargo-llvm-cov` 版本，因其 JSON 输出结构在版本间变过——不锁版本时上游一次
  升级就能让门禁读不到字段而崩，门禁崩掉 ≠ 门禁守住）。
  基线实测口径：`cargo-llvm-cov 0.9.1` + `llvm-tools-preview`，
  `rustc 1.99.1 (58cf8f9c3 2026-10-13)` / MSVC `x86_64-pc-windows-msvc`。
  已知缺口（如实标注）：本机 **Redis(6379) 与 ClickHouse(8123) 未运行**，
  故 `telemetry.rs`/`ch_sink.rs`/`analytics.rs` 三个依赖真实后端的模块覆盖率
  天然偏低（53%/49%/53%），这是**环境缺口不是测试缺失**，活流量验证见
  EXEC_LOG 步骤 40/43；`main.rs` 仅 32.60% 是**预期的**——它是装配层
  （env 接线、supervisor、Pingora 启动），给它凑单测只会写出
  「断言构造函数返回了构造函数」的假测试。

- **`router.rs` 按职责拆分为 5 个子模块 + 2 个测试文件（OPT-R18 P2-1）**：
  `router.rs` 曾是 3564 行的单文件巨型模块——选路、粘滞会话、域级隔离、主动健康
  探测、套利缩放、控制面增删全部混在一个文件里，任何一处改动都要在 3500 行里
  定位上下文。拆分后 `router.rs` 只剩 371 行（结构定义 + 构造器 + 子模块声明），
  职责落到 `router/select.rs`（选路/粘滞/加权）、`router/health.rs`（主动健康
  探测回灌）、`router/quarantine.rs`（两级隔离表）、`router/node_mgmt.rs`（控制面
  增删调权）、`router/buckets.rs`（GW-6 分桶索引），测试迁至 `router/tests.rs`
  与新增的 `router/health_tests.rs`。
  **拆分是纯搬移**：34 个方法逐字迁移，行为零变更，单测全绿。
  两处**必须留意的 Rust 语义**（拆分时踩到并已写入注释）：
  ① **不做 `pub(crate) use x::*` 再导出**——固有方法（inherent method）调用方直接
  写 `engine.foo()` 即可，Rust 不要求 import；再导出会触发 `unused_imports`（`use
  super::*` 的 glob-of-glob 不算"使用"），唯一需要跨模块可见的自由函数
  `pick_weighted_streaming` 由测试显式 `use super::select::pick_weighted_streaming;`。
  ② 子模块经 `use super::*;` 继承父模块的**私有**字段与自由函数（私有项对其
  定义模块的全部后代可见），故父模块的私有 import 仍是子模块名字的来源。
  配套把 `tools/check_components_docs.py` 升级为**懂模块目录**：子模块的测试数
  并入父模块行（`router.rs` 62 = 52 + 10，模块表仍 24 行——读者按职责建心智
  模型，拆成 7 行反而误导）；新增断言 F 校验「子模块表 ↔ 磁盘子模块集合」双向
  一致且子模块的关键 API 真实存在；断言 E 的死引用扫描扩展到子模块路径
  （`router/health.rs`）并归一 `gateway/src/` 前缀（否则仓库根写法会被误判为
  死引用——**假红**比没有门更糟）。自测从 3 处注入扩到 5 处（新增「漏登记子模块」
  与「幽灵子模块」），全部正确判红。
- **主动健康单测 10 项 + 10 处变异自检（OPT-R18 P2-1）**：主动健康摘除是一条
  **会直接把节点摘出数据面**的路径，三个危险面——① 误摘（一次抖动就摘）
  ② 摘光（全池 503）③ 摘了不恢复（永久损失）——都不会 panic，只会让线上
  "莫名 503/莫名变慢"，属最难靠日志定位的故障类别。故对每条危险面都设了
  可打红的断言：阈值语义、安全阀（**只算 `weight>0` 的节点**）、粘滞会话随摘除
  失效、冷却到期后**立即**重摘、换池清僵尸、探测候选**必须含被摘节点**、
  迟到探测不落状态、并发下「表即真相」、免费池抽样在 `ceil(100/pct)` 轮内全覆盖。
  10 处变异（改坏安全阀阈值/可用节点计数/粘滞失效/冷却清零/僵尸清理/抽样逻辑/
  NodeGone 守卫/选路侧强制/抽样步长/已在摘除中的刷新分支）**全部如实变红**。
  其中第 10 处首轮**未变红**，据此补上了「已在摘除中 ⇒ 只刷新冷却、不重复累计
  失败次数」的断言——变异自检不只是"验收"，它本身**发现了一处测试盲区**。

- **免费供给 SLO 三指标 + 三告警**（OPT-R18 P2-3）：免费线此前只有"池里几个节点"
  和"成功率"，而免费池有三种**节点数正常、实际已不可用**的失效方式，节点数一个
  都抓不到：
  ① `free_pool_distinct_exit_ips` —— 池内不同**出口身份**个数。几十个节点共用
  同一个出口 IP 时节点数很漂亮，WAF 侧却只看到一个身份，封禁一次全灭；
  ② `free_pool_median_age_seconds` —— 池内节点年龄**中位数**。贴近 0 说明池子是
  "漏斗"而非"池子"（高频死亡+补位），有效吞吐远低于节点数暗示的量。用中位数
  而非均值：免费池年龄分布长尾，均值会被少数长命节点拖高，读起来像"很健康"；
  ③ `free_pool_yield_ratio` —— 抓来的原始代理**多少能过质检**。转化率坍塌时
  节点数会滞后才掉（旧节点还在 TTL 内），此时已该查源站。
  配套告警 `FreePoolExitDiversityLow`（<3 且 30m）/ `FreePoolAgeCollapse`
  （<300s 且 1h）/ `FreePoolYieldCollapse`（<2% 且样本≥100），三条都以
  `free_pool_source_fetch_ms_count` 作**启用态门槛**——否则 `FREE_ENABLED=0`
  （缺省）时这些 gauge 停在初值会永久 firing（`FreePoolDry30m` 踩过该坑）。
  **两条有意为之的空值语义**：无样本时 `yield_ratio` 渲染 `NaN`（不是 0，0 会被
  读成"转化率极差"）；空池时 `median_age` 渲染 `NaN`（不是 0，0 会被读成"刚进池"）。
  Prometheus 里 NaN 参与比较恒 false ⇒ 两者在无样本/空池时天然不误报，空池由
  既有 `FreePoolDry30m` 负责，职责不重叠。
  实现上给 `Entry` 增加 `first_seen`（只在首次进池写入；不能用
  `expires_at - ttl` 反推——TTL 会变且 `expires_at` 每次续命都重写，反推出的
  "年龄"会恒等于当前 TTL，是个看着合理的假数字），并把"是否算在池内"的过滤
  谓词抽成 `Registry::is_pooled`，`snapshot()` 与 `median_age_secs()` 共用——
  两处各写一遍就会出现"节点数按 A 口径、年龄按 B 口径"的数字，而并排放时
  没人看得出它们描述的不是同一个集合。
- **租户状态 Redis 写穿 + 启动加载（OPT-R18 P2-2）**：此前 `TenantManager`
  纯内存，重启即丢——**实测** run1 `PATCH` 把 `balance_usd` 从 100 改成 42.5、
  run1 再读为 42.5，kill 后 run2 又读回 100.0（余额凭空归零）。修法：新增
  `TenantManager::with_store(Store::{Memory,Redis})` 双模，`TENANT_STORE`
  （`memory` / `redis`）显式开关——**缺省 `memory`**、保持 OPT-R18 之前行为，
  不悄悄改变升级语义；`redis` 需 `REDIS_URL`，缺失则响亮 warn 并降级 memory
  （不静默）。启用时四个写入点（`register_tenant` / `set_active` / `set_balance`
  / `release_and_meter`）标脏，后台 `spawn_flusher`（`TENANT_FLUSH_SECS` 默认 5）
  按 key 写穿，失败**逐条保留脏标记**下一轮重试（不先清空再写——那是把写失败
  变成静默丢余额）。启动时 `load_persisted()` **必须在 `spawn_flusher` 之前**：
  反序会让首轮 flush 把刚注册的初始余额（100）写回 Redis，覆盖掉上次的真实余额。
  **语义边界**：config 决定「有哪些租户」，persistence 只决定「它们的状态」——
  Redis 里一条对应不上 config 的租户记录**不复活**（避免被删租户靠 Redis 复活）。
  可测性：写穿算法（逐 key 删脏 / 失败保留 / 孤儿忽略）抽成 `TenantStoreBackend`
  trait + 进程内 `RecordingBackend`，`cargo test` 无需真 Redis 即可跑通往返；
  五处变异自测（① 写前删脏 ⇒ 失败仍丢脏标 ② 失败不再保留脏 ③ 漏标脏
  `release_and_meter` ⇒ 余额跨重启丢 ④ load 不复活孤儿 ⑤ load 不覆盖运行时状态）
  全部判红。另有 `TENANT_REDIS_PREFIX` 自定义 key 前缀。
- **选路决策质量基线（OPT-R18 P1-5）**：新增 `tools/routing_replay.py`
  （纯标准库离线策略评估台）＋ `docs/ROUTING_BASELINE.md`。此前所有选路验收
  只证明「选得动」（不锁死 / 供给可用 / 数学正确），**没有一条证明「选得好」**。
  回放台在日志拟合的结果模型上并排跑 4 个策略（轮询 / 加权 / LinUCB / oracle），
  在合成样本 600 条上测得 LinUCB **52.2%** vs 轮询 **29.7%**（+22.5pp）、
  距 oracle 上界 77.0% 还差 **24.8pp** —— 把「这次改动让选路变好了吗」从作者
  自述变成了可复现数字。文档头部明确写出三类偏差（支持偏差 / 相关当因果 /
  采样而非期望），并声明**绝对值不是线上预测，只有策略间相对排序有意义**。
- 跨语言一致性门 `tools/check_replay_parity.py`：`routing_replay.py` 的 `Arm`
  是 `bandit.rs::BanditArm` 的镜像，镜像漂移时报告算的是**另一个策略**而没有任何
  东西会变红。黄金向量由**生产实现自己生成**（`print_replay_parity_vectors`，
  `#[ignore]`），不是手写期望值——否则同一个理解错误会同时通过两份断言。
  当前两侧最大差异 `3.33e-16`；覆盖 fresh 臂 / free 溢价 / `theta` 裁剪 /
  free 档 1000 次遗忘 / 大 `t` / 高 WAF 风险上下文。
- 静态门 `tools/check_routing_baseline.py`：断言 `docs/ROUTING_BASELINE.md` §4
  发布的表格**逐格等于**回放台的可复现输出。实测四种漂移全部判红：改 linucb
  成功率 0.1pp / 改样本条数 / 改 seed / 删掉 oracle 行。
- **静态门 `tools/check_components_docs.py`（OPT-R18 P1-4）**：`gateway/src/*.rs`
  ↔ `docs/COMPONENTS.md` 双向结构门。落门时实测到四类失真**同时存在**且 CI 全绿：
  ① 文档声称「模块表（19 个）」而实际 **24** 个 `.rs`（漏掉的 `admin.rs` 是 1788 行
  的 Admin REST 面）；② 同一文件里 `vendor_arbitrage.rs` 中文行写 208 行、英文行
  写 218 行（描述同一个文件，必有一行是假的）；③ `socks_handshake.rs` 的「关键
  API」写作 `socks_handshake`，而该字符串在文件里**一次都没出现**（真实 API 是
  `establish` / `greet_only`）；④ 单测总数停在 **156**（基线 338），因为它此前
  不在任何门禁范围内。四类断言：模块集合三方相等、标题计数、每行单测数（且
  **合计 == 基线通过数 + 忽略数 == 346**）、关键 API 必须是真实标识符。全部
  **结构化比对**（解析表格后比单元格），不做全文 grep（OPT-R17 教训）。
  变异自测：漏登记模块 / 篡改单测数 / 编造 API 三处注入必须全部判红。
- **CI 补两个此前完全缺失的门**（OPT-R18）：`web` job（`npm ci` + `tsc --noEmit` +
  `vite build`）——前端 1888 行 / 7 页此前是**唯一不在门禁内的交付物**，后端契约一变
  可能静默崩而 CI 全绿；`windows-rust` job（`cargo test --locked --workspace`）——
  Windows 是主开发平台，但此前 windows job 只验 PowerShell 脚本、不跑任何 Rust 测试，
  而 `default_grace_secs(cfg!(windows))` 正是 OPT-R14「300 秒自杀」P0 的修复点。
- `node_registry::is_free_provider`：免费池 provider 前缀判定的**唯一真源**
  （与 `RESERVED_PROVIDER_PREFIX` 同文件），避免 `"free-"` 字面量散落多处导致
  「探测抽样把付费节点当免费节点」「免费档凭据护栏漏判」这类无编译期提示的错误。
- `RouterEngine::probe_candidates(free_sample_percent)` 分层探测：付费节点全量 +
  免费池轮转抽样，新增单测锁定「4 轮内全覆盖」与「付费恒覆盖」两个确定性性质。
- 第 15 道静态门 `tools/check_admin_routes_documented.py`：双向校验
  `gateway/src/admin.rs` 的 `.route(...)` 与 `docs/ADMIN_API.md` 的
  `### METHOD` 标题，防「代码加了路由文档没写（客户端 404）」与「文档写了
  代码没注册（调用必失败）」。只做结构化比对，**不做全文 grep**，避免说明
  文字/curl 示例让门假绿。变异自测：注入未文档化路由→红；文档写 ghost
  端点→红；路由只出现在注释→绿。
- **控制面节点热更新（步骤 52）**：`POST /api/v1/nodes` /
  `DELETE /api/v1/nodes/{addr}` 运行时增删托管节点，`PATCH .../weight`
  对托管节点同步落盘；`NODES_FILE`（缺省 `data/nodes.json`）用
  临时文件+fsync+rename 原子替换，文件存在即以文件为准（损坏拒绝启动），
  `NODES_FILE=""` 关闭持久化。`RouterEngine` 新增池写串行锁，修复
  FreePool merge 与 Admin 调权/增删并发时的 lost update；同时修复
  `scale_vendor_weights` 漏重建 buckets（factor=0 摘除对桶快路径无效）。
  Admin UI 节点页新增表单与删除；compose 暴露 `:9092` 并挂
  `gateway-data` 卷。Admin API 错误映射补 409/500；新增 20 个单测
  （306→326）与真实二进制 E2E（新增→重启仍在→删除→重启仍不在）。
- `docs/OPERATION.md` 新增「§4.1 告警投递（Alertmanager）」小节：
  写明三段链路各自的**真实状态**（Prometheus→Alertmanager ✅ 已接线、
  Alertmanager 服务 ✅ 默认起、Alertmanager→人 ❌ `receiver: "empty"`
  故不外发），并附 4 条常见误解澄清。此前 27KB 运维手册**全文不出现
  "alertmanager"**，运维读手册无法得知告警不会送达任何人。
- 第 13 道静态门 `tools/check_observability_truth.py`：锁两类「文档/配置
  声称存在、代码里不存在」的失实（依赖 feature 未被引用；告警投递未文档化）。
  **双向验证**：注入 `features = ["cors"]` → 判红指出 `CorsLayer` 缺失；
  删掉 OPERATION.md 告警小节 → 判红；还原 → 绿。
  过程中踩中一个值得记录的陷阱：门最初按「全文 grep 符号」实现，结果被
  **自己的说明注释**满足（注释里为解释「为什么不加 CORS」写了
  `CorsLayer::permissive()` 字样）而**假绿** —— 这比「门失效」更糟，它让门
  看起来在工作。修正为**剥离注释后**再检索。
- 第 14 道静态门 `tools/check_no_bom.py`：要求**配置类文件不得带 UTF-8 BOM**
  （与 `check_ps1_bom.py` 恰好反向，故独立成文件而非加分支）。实证缺陷：
  `gateway/Cargo.toml` 带 BOM ⇒ `Swatinem/rust-cache` 解析 manifest 失败、
  回退到**整文件缓存**，静默丢掉按 `Cargo.lock` 哈希的细粒度 key
  （OPT-R9 A4 引入 rust-cache 的初衷）。Cargo 本身容忍 BOM，所以**构建与
  测试全绿、只是缓存悄悄退化** —— 这类「不失败只变慢」的问题最难发现。
  **非本轮引入**：`70792ee`（本轮动手前的 HEAD）即已带 BOM。双向验证：
  加回 BOM → 判红；剥离 → 绿。

## [0.2.0] - 2026-10-17

### Added

- Admin Web UI（`web/admin` SPA，对齐 `docs/ADMIN_UI.md`）：登录 + 仪表盘/节点池/
  租户与计费/免费池/断路器/设置 六页（admin 契约同批收口，见 [0.1.0] Fixed）。
- 供应链治理门禁：`gateway/deny.toml`（advisories/bans/licenses/sources）、
  `tools/check_supply_chain.py`（cargo-deny + cargo audit 双跑）、
  CI `supply-chain` job、`docs/SUPPLY_CHAIN.md`（首次评估与 triage）。
- 端到端吞吐压测工具：`tools/bench/main.go`（Go 客户端+上游）、
  `tools/bench_gateway.py`（一键编排）、`docs/PERF_BASELINE.md`
  （峰值 17,215 QPS @ 100%，持续 10k 整形）。
- 文档一致性门覆盖 `docs/CAPABILITY-AUDIT.md`、`docs/SUPPLY_CHAIN.md`
 （ALLOWLIST + EXPECTED_KEYS 双表同步自检）。
- CI shell 兼容门：`tools/check_ci_shell_compat.py`
 （`shell: powershell` 的 run 块必须纯 ASCII，防 em dash 截断事故）。

### Fixed

- CI 全部 7 个失败点（R16-A 起 GitHub Actions 首次全绿，之前从未绿过）：
  .NET SDK 门（csproj 用 C# 注释致 MSB4025）、release bandit 绝对纳秒断言、
  PSScriptAnalyzer 位置参数、PS 5.1 parse 非 ASCII 截断、Docker 缺 cmake、
  零分配契约 glibc 平台差异。
- `free_pool.rs` 客户端缓存淘汰：`Instant` 基准过近时
  `checked_sub` 返 None 导致全清（改用相对时长判据）。
- OPT-R11 A1 红测的 profile 依赖（release 下稳定失败，改 profile 无关断言）。

### Changed

- 依赖升级：pingora 0.6 → 0.9.0（umbrella/core/proxy/load-balancing 四 crate
  同步，src 零代码改动）+ rustls 0.23.45 → 0.23.46（patch）。升级决策台账见
  `docs/SUPPLY_CHAIN.md` §9；端到端压测复跑 3 轮 PEAK
  12,739 / 19,819 / 19,786 QPS（conc=4，100%），与首测锚点
  17,215 QPS（`docs/PERF_BASELINE.md`）同量级，无吞吐回退；持续负载被
  默认租户 10,000 QPS 配额门整形为 ~10k（429，预期门控行为）。

### Security

- RustSec 漏洞公告 8 → **0**（deny advisories ok + audit exit 0 交叉验证）：
  RUSTSEC-2026-0033/0034（pingora-core HTTP 请求走私，critical）、
  RUSTSEC-2026-0035（pingora-cache 缓存投毒，high）、
  RUSTSEC-2026-0346~0349（rustls TLS 秘密暴露/降级/资源放大，medium/low）、
  RUSTSEC-2024-0437（protobuf 递归崩溃，经 prometheus 0.13 传递链）。
  处置为**升级而非 ignore**（pingora 0.6 → 0.9.0 + rustls 0.23.46，
  见 Changed；台账与验收见 `docs/SUPPLY_CHAIN.md` §9）。
- 残余 4 个传递依赖 unmaintained（No safe upgrade is available）：以
  `deny.toml unmaintained = "workspace"` 挡直依（直依 unmaintained 一律
  error、升级即红，传递仅 warn 可见）；不逐条 ignore，`ignore` 保持空。
- 已修：yoke-derive 0.8.3 → 0.8.4（去掉 yanked 版本）。

---

## [0.1.0] - 2026-09-19 ~ 2026-10-17

> 本节汇总仓库创建以来的全部工作（此前无 release、无 tag、无 changelog，
> 故按提交历史如实归纳，不拆分虚构的中间版本）。

### Added

- GW-R1 网关五模块 + LinUCB 自学习选路 + 会话粘滞 + 多租户限流。
- FreePool v2 免费代理供给（多源抓取、分级验证、匿名度分级、GeoIP）。
- Phase 2 SOCKS 出站（SOCKS4/5 握手、443 TLS）。
- Phase 3 画像与学习（R13 LinUCB 永久锁死修复、分档探索配额）。
- Phase 4 合规开关与 GeoIP 运维（D3 Key 门默认开、监听收环）。
- Phase 5 韧性演练（遥测落地端健康三指标 + 3 告警、断电自愈）。
- Windows 300 秒自杀修复（OPT-R14，pingora-core 0.6.0 兼容绕过）。
- 5 语言 SDK（Python/Go/Java/Node/.NET）+ 错误契约统一。
- 12→13 个 Python 静态门（全部进 CI，多数做过双向验证）。
- 31 个指标族 + 15 条 Prometheus 告警 + Grafana provisioning。
- 能力复核报告（`docs/CAPABILITY-AUDIT.md`，含首版 6 处错误更正）。

### Fixed

- 免费档零信任（`tier=free` + 凭据头 → 403）。
- 隔离三层（内存 → Redis SETEX → PubSub 扇出）。
- MaxMind 署名合规、文档漂移门、API Key 卫生。
- Admin API 契约收口（GW-5，对齐 docs/ADMIN_API.md §4）：`/nodes`、`/tenants`
  回归文档承诺的**裸数组**；PATCH balance 真实落库（原为只改本地副本的假实现），
  DELETE quarantine 真实解除（原为空操作）；全部非 2xx 响应统一为 `{error, code}`
  JSON（401/404/400，含未知路径 fallback）；非有限余额（NaN/±Inf）拒绝为 400。

> 本窗口（2026-10-17）内新增的 pingora 0.6 → 0.9.0 升级、RustSec 漏洞
> 8 → 0 清零与压测复跑记录，按本文件"Unreleased 节记录已合并未发布内容"
> 的政策放在 [Unreleased]（下次 bump 0.2.0 时并入版本节）。

<!-- 版本链接注记：本仓尚无 tag、无 GitHub Release（实测 git tag 为空），故不写 compare 链接，写了就是死链。第一次切 release（v0.2.0，随 pingora 升级）时再补上。 -->
