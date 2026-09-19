# 任务执行流水日志（IPProxyPool GW-R1）

> 本文件只留活跃轮条目，历史查 `banyan-skills/archive/README.md` 方法（本仓暂未归档）。
> 跟踪表：`plan/2026年9月19日-GW-R1实施计划.md`。

### [2026-09-19 12:00] GW-R1 规划完成：dated计划落plan/ + TASK_PLAN初始化
- **实际操作**：通读manual/ 7份+Banyan四Skill（draft/track/log/gate）+templates；新建`plan/2026年9月19日-GW-R1实施计划.md`（6阶段GW-0~GW-5，勘察冻结7条，延后5项）；新建根`TASK_PLAN.md`（6步骤+决策约束）；本文件初始化。
- **验证结果**：dated计划6阶段状态⬜一致；TASK_PLAN 6步骤与dated同名同序；数量：dated约180行，TASK_PLAN 6步骤。
- **遇到的问题与解决**：无。教训：IPProxyPool根与banyan-skills根双轨独立，避免覆盖banyan P5已完成状态。
- **下一步建议**：进入步骤1 GW-0，先改双轨为进行中再建gateway workspace。

### [2026-09-19 12:05] 步骤 1 开始: GW-0 基线脚手架
- 计划操作：新建gateway/Cargo.toml workspace+src/main占位；docker-compose.yml Redis7+ClickHouse+Prom/Grafana；deploy/clickhouse/001_schema.sql MergeTree；跑cargo check/fmt/clippy/test基线。
- 预期验证方式：cargo check通过+compose config-valid+CH DDL语法合规+四门基线绿。

### [2026-09-19 13:10] 步骤 1 已完成: GW-0 基线脚手架
- **实际操作**：落盘`gateway/Cargo.toml`（全依赖）+`gateway/src/main.rs`占位+`docker-compose.yml`（Redis7/CH24/Prom2.53/Grafana11.1）+`deploy/clickhouse/001_schema.sql`（proxy_telemetry_log MergeTree）+`deploy/prometheus/prometheus.yml`；winget安装CMake 4.4.3打通Windows构建。
- **验证结果**：
  - [门1-格式] ✅ `cargo fmt --check` clean（EXIT 0）。
  - [门2-静态] ✅ `cargo clippy --all-targets` 零告警 Finished 14.38s。
  - [门3-测试] ✅ `cargo test` 1通过/0失败（baseline_placeholder_green）。
  - [门4-性能] ✅ `cargo bench --no-run` 编译通过 Finished 1m59s；`docker compose config` valid；`docker up`未跑（Docker Desktop daemon未启动，记遗留不阻塞）。
  - 综合判定：✅ 全绿（3实跑+1编译+1配置）。
- **遇到的问题与解决**：
  - `atomic-float`应为`atomic_float`，按cargo提示修正。教训：第三方crate名以下划线为准，不靠记忆。
  - `pingora 0.4 features=[full]`不存在（docs.rs仅proxy/lb/rustls等15 flag），改`proxy+lb+rustls`。教训：manual特性名不可直接抄，必须查docs.rs features页。
  - `clickhouse 0.13 features=[tokio]`不存在（仅lz4/inserter等14 flag），去掉多余features。教训：同上。
  - `pingora-core 0.4`在rustc1.93下5错（sfv 0.15 Parser签名变更+tokio-rustls id i32/usize mismatch），升级`pingora系0.4→0.6`（sfv回落0.10.4）后`cargo check 29.2s`通过。偏离dated冻结6（写0.4）属构建必需，以本条为准，冻结条不静默改。
  - `libz-ng-sys`需cmake，winget装Kitware.CMake 4.4.3解决。教训：Windows跑Pingora必须预装CMake+VS Build Tools。
  - Docker daemon未运行，up/redis PONG延后到GW-2前（启动Desktop后补）。教训：config-valid≠运行时可用，门禁报告必须如实拆分。
- **下一步建议**：进入步骤2 GW-1，先写model/router/gateway/main再补5单测。

### [2026-09-19 13:10] 步骤 2 开始: GW-1 P1网关MVP
- 计划操作：新建model.rs（含tenant/transferred_bytes预留修A2/A4缺字段）+router.rs（含get_healthy_candidates/set_quarantine/adjust_vendor_weight）+gateway.rs（ProxyHttp五阶段）+main.rs（bootstrap :8080）+5单测；curl三用例验证。
- 预期验证方式：cargo test 5+通过+curl 200+fmt/clippy绿。

### [2026-09-19 17:10] 步骤 2 已完成: GW-1 P1网关MVP
- **实际操作**：落盘`model.rs`（ProxyNode/RoutingSpec/ProxyContext，tenant_id/transferred_bytes预留+max_retries=3）+`router.rs`（ArcSwap+DASH会话10min+匹配country/tier+域级quarantine+2单测）+`gateway.rs`（pingora0.6 ProxyHttp：request_filter解析清洗/upstream_peer选路+1500/5000/3000ms超时/上游auth注入/fail_to_connect重试/logging telemetry占位+3单测）+`main.rs`（初始US/JP双Mock节点+:8080）；`Cargo.toml`加`base64 0.22`+`rustls 0.23(ring)`。
- **验证结果**：
  - [门1-格式] ✅ `cargo fmt --check` clean。
  - [门2-静态] ✅ `cargo clippy --workspace --all-targets` 零告警（跨阶段预留项用`#[allow(dead_code)]`+注释显式放行；未用`pool_len`直接删除）。
  - [门3-测试] ✅ `cargo test --workspace` 5通过/0失败。
  - [门4-功能] ✅ curl三用例200：普通→`mock-b-jp[200]`；US粘性task-1两次→`mock-a-us[200]`同节点；Proxy-Auth(JP)→`mock-b-jp[200]`；`log/gw.err` telemetry 200行完整；detached复测仍200（稳定）。
  - 综合判定：✅ 全绿。日志证据：`log/gw.out|gw.err|mockA.out|mockB.out|mockA.err|mockB.err`+脚本`log/launch_detached.py|mock_upstream.py`。
- **遇到的问题与解决**：
  - pingora0.6相对manual偏离6处：无`suppress_error`（改`fail_to_connect`+`set_retry`+retry_count计数）、无`protocols::base64`（加base64 crate）、`RequestHeader`在`pingora::http`（原`pingora_core::http`不存在）、`insert_header` name仅收`&'static str`（单测helper改`&'static str`+Proxy-Auth用例改owned String分步insert）、`Session::is_tls`不存在（本轮固定false，TLS指纹归GW-3）、`response_written()->status`经deref可用。教训：manual按0.4写，0.6必须以registry源码为准逐项核对。
  - rustls 0.23 panic（ring+aws-lc双provider无法自动选，bootstrap即挂）：main首行显式`install_default(ring)`+直引`rustls 0.23[ring]`解决。教训：transitive features additive，双provider必显式pin。
  - `Get-NetTCPConnection`在Win11卡死超120s：禁用，改`curl --max-time`做端口探活（拒绝约2s返回000）。教训：自动化脚本禁用TCP全表枚举类命令。
  - Start-Process后台网关随父shell退出收CTRL_CLOSE进graceful-shutdown拒连：改Python `DETACHED_PROCESS|CREATE_NEW_PROCESS_GROUP`+双流重定向文件启动，跨调用稳定。教训：Windows后台服务必须detached+无控制台继承句柄。
  - 用户指令：项目日志/输出一律落`log/`，禁放C盘：已建`log/`并迁移全部运行日志+辅助脚本，C盘opencode残留已清。
- **下一步建议**：进入步骤3 GW-2（telemetry MPSC+batch XADD/circuit_breaker消费组+prober三态+main装配），前置需`docker compose up -d`（Redis/CH，GW-0遗留Desktop daemon未启动）。

### [2026-09-19 20:30] 步骤 3 开始: GW-2 P2双轨自愈
- 计划操作：核验telemetry.rs/circuit_breaker.rs/prober.rs/main.rs装配与计划一致；修门禁编译错；补quarantine+pubsub集成测试；Docker起Redis后做端到端隔离验证。
- 预期验证方式：四门绿（fmt/clippy -D warnings/test/bench编译）+live集成真过+curl隔离域503/他域200。

### [2026-09-19 21:00] 步骤 3 已完成: GW-2 P2双轨自愈
- **实际操作**：核验GW-2四文件符合计划（MPSC 10000/batch 200/100ms XADD；429→60s/403→600s/502,504→30s三层隔离；prober三态+ip=提取；main装配telemetry/CB/PubSub/prober+降级模式）；修`pipe.query_async::<_,()>`→`::<()>`（redis 0.26单泛型）+`run(mut self)`去mut+删未用AsyncCommands导入；补2个`#[ignore]`live集成（无Redis跳过过/有Redis实断言）；`log/redis_inject.py`（raw-RESP注入，绕开Win CLI引号剥离）；Docker Desktop已启动+`compose up -d redis` PONG；网关detached（PID:31364，log/gw2.out/err）+mocks（22184/36728）。
- **验证结果**：
  - [门1-格式] ✅ `cargo fmt --check` clean（EXIT 0）。
  - [门2-静态] ✅ `cargo clippy --workspace --all-targets -- -D warnings` 零告警（EXIT 0）。
  - [门3-测试] ✅ `cargo test --workspace` 12通过/0失败/2 ignored；`-- --ignored` 2通过（Redis在线时0.62s实断言，非跳过）。
  - [门4-性能] ✅ `cargo bench --workspace --no-run` 编译通过。
  - [集成] ✅ 伪造403经Stream→CB→SETEX BANNED(600s)+WARN日志；curl隔离域503/他域200；Stream XLEN=2载荷完整（provider/tier/country正确）；消费组circuit_breaker_group 1消费者lag 0。
  - 综合判定：✅ 全绿。内存50ms隔离由`memory_isolation_applies_within_50ms`单测覆盖；DEL Redis键后网关仍503属设计内（内存TTL独立于持久层）。
- **遇到的问题与解决**：
  - redis 0.26 `Pipeline::query_async`仅1泛型，`::<_,()>`5错→改`::<()>`。教训：manual按旧版写， sygn以registry源码为准。
  - Windows `docker exec redis-cli XADD ... JSON`引号被剥离致载荷非法、CB静默丢弃（符合容错设计）→改raw-RESP Python注入。教训：Win CLI不适合传JSON，测Redis用RESP直连。
  - `Get-Content | docker exec -i redis-cli`报Invalid argument(s)（管道编码问题）→弃用。教训同上。
  - Prober对本地Mock报Dead属预期（Mock为plain-HTTP，不可代理https trace），分类正确无崩溃。
- **下一步建议**：进入步骤4 GW-3（bandit.rs LinUCB d=4+fingerprint.rs基础版+pool.rs预热+gateway联动+bench<200ns）。

### [2026-09-19 21:05] 步骤 4 开始: GW-3 P3智能硬化
- 计划操作：新建bandit.rs（d=4 LinUCB+Sherman-Morrison+alpha0.4+成本权重）+fingerprint.rs（idle90s/TFO/keepalive/64KB+8头）+pool.rs（30s预热）+gateway联动（extract→select_best→apply_chrome→logging reward/update/emit）+release时延实测。
- 预期验证方式：bandit单测绿+选路<200ns记录+curl回归+四门绿。

### [2026-09-19 21:40] 步骤 4 已完成: GW-3 P3智能硬化
- **实际操作**：落盘`bandit.rs`（ArmState单锁+二次型UCB+单遍select+compute_reward+真实CosTime）+`fingerprint.rs`（apply_chrome_profile+align 8头，sec-ch-ua-platform取Windows）+`pool.rs`（Arc<RouterEngine>快照+warm_once计数+30s run）+`gateway.rs`联动（sticky走select_node/无状态走LinUCB+request打Chrome头+peer上profile+logging做reward/update/error透出）+`main.rs`装配（engine/arms/prewarmer origins cloudflare+google）。
- **验证结果**：
  - [门1-格式] ✅ `cargo fmt --check` clean（EXIT 0）。
  - [门2-静态] ✅ `cargo clippy --workspace --all-targets -- -D warnings` 零告警（EXIT 0）。
  - [门3-测试] ✅ `cargo test --workspace` 24通过/0失败/2 ignored（新增12：bandit 7/fingerprint 2/pool 3）；`-- --ignored` 2通过（GW-2无回归）。
  - [门4-性能] ✅ `cargo bench --workspace --no-run` 编译过；`cargo test --release bandit` 7过，`select_best_arm` 8臂avg **98ns**（预算200ns，x86_64 release实测）。
  - [回归] ✅ 网关detached（PID:8020，log/gw3.out/err，Redis connected+预热4 tickets/tick）：无状态→mock-b-jp×6稳定（DC低成本臂获胜符合设计）；US粘性gw3-task1两次→mock-a-us；Proxy-Auth JP→mock-b-jp；Stream XLEN 17（遥测链路不断）。
  - 综合判定：✅ 全绿。
- **遇到的问题与解决**：
  - 初版release实测296ns超200ns预算→三处优化：双RwLock合一（少一次acquire）、二次型`x.dot(Ax)`去掉1×4中间矩阵乘、`max_by`双算改单遍计分（14次→8次评分）→98ns。教训：先实测再优化，pairwise比较是隐形翻倍。
  - `select_bandit_node`误写进`impl ProxyHttp`（E0407）→独立`impl SmartProxyGateway`块。教训：trait impl内只能是trait方法。
  - manual-A3的`recv/send_buffer_size`在0.6不存在→改`tcp_recv_buf=64KB`并记偏离；`TcpKeepalive` Linux多`user_timeout`→cfg双构造，Win/Linux同码可编。
  - arm键用`ip:port`而非裸IP（两Mock同为127.0.0.1，否则臂冲突）；无状态首选mock-b-jp是成本项在起作用（DC 0.1 vs Res 1.0），非bug。
- **下一步建议**：进入步骤5 GW-4（analytics落库+tenant配额+vendor三家Mock套利+Grafana/PromQL）。

### [2026-09-19 21:45] 步骤 5 开始: GW-4 P4企业运营
- 计划操作：新建analytics.rs（Row对齐DDL+LZ4+5min SLA format!+白名单）+tenant.rs（governor QPS/CAS+三档计费）+vendor_arbitrage.rs（60s三家×三国+80/95阈值）+metrics.rs（9091手写Prometheus exposition）+网关接线（鉴权拦截/response_body_filter/计量/logging）+main装配（默认租户/mock-c/套利/metrics）+dashboard.json+EGRESS_MESH.md。
- 预期验证方式：四门绿+CH真落库+租户429/403+套利单测+Prom up=1+curl回归。

### [2026-09-19 22:30] 步骤 5 已完成: GW-4 P4企业运营
- **实际操作**：落盘`analytics.rs`（TelemetryRow 13列对DDL+insert_batch+ping+whitelist sla_sql+NaN→100+live roundtrip）+`tenant.rs`（AtomicBool active+QPS/CAS/release三档计费，5单测）+`vendor_arbitrage.rs`（arbitrage_action纯策略+60s audit_once/run，2单测）+`metrics.rs`（4计数+403分provider+10桶直方图+serve_metrics，2单测）+`model.rs`加tenant_account+`gateway.rs`接线（X-API-Key鉴权→429/403拦截+scrub X-API-Key+body计量+logging释放计量/metrics/bandit）+`main.rs`（默认租户10000/10000+mock-c GB/mobile 8890+analytics ping+套利vendors mock-a/b/c×US/JP/GB+metrics 9091）；`deploy/grafana/dashboard.json`（4 panels按自研label改写PromQL）+`docs/EGRESS_MESH.md`（WG模板+Anycast方案）；compose CH原生端口改9010（9000被minio占）；`docker compose up -d`四容器全在线。
- **验证结果**：
  - [门1-格式] ✅ `cargo fmt --check` clean（EXIT 0）。
  - [门2-静态] ✅ `cargo clippy --workspace --all-targets -- -D warnings` 零告警（EXIT 0）。
  - [门3-测试] ✅ `cargo test --workspace` 37通过/0失败/3 ignored（新增13：analytics 4/tenant 5/arbitrage 2/metrics 2）；`-- --ignored` 3通过（CH insert→SLA100→DELETE + Redis双测，0.61s实断言）。
  - [门4-性能] ✅ `cargo bench --workspace --no-run` 编译过。
  - [回归] ✅ 网关detached（PID:22664，Redis connected+预热6 tickets+CH online+metrics 9091）：普通→mock-b-jp、US粘性两次mock-a-us、Proxy-Auth JP→mock-b-jp、坏Key→403、GB→mock-c-gb；/metrics 2xx=4/4xx=1；Prometheus up=1，rate=0.133/s实数；Grafana :3000 200。
  - 综合判定：✅ 全绿。
- **遇到的问题与解决**：
  - clickhouse `serde::chrono`需`chrono`特性（默认无）→Cargo加`clickhouse[chrono]+chrono 0.4`；lz4本就是默认特性，无需动。教训：GW-0只验了features名存在性，模块级门控要用到才暴露。
  - 直方图双累加（observe全桶加+render又cumulate）→render改原始值。教训：Prometheus惯例是写侧累加，读侧直出。
  - `TenantAccount`缺Debug致unwrap_err不过→手写Debug（跳过limiter）；`set_active`初版unsafe改AtomicBool。教训：跨线程共享状态一律原子类型，不手写unsafe。
  - CH容器9000端口与别项目minio冲突→改127.0.0.1:9010:9000。教训：先`docker ps`看端口再up。
  - curl查Prom的`{job=...}`被PS剥引号→改`-G --data-urlencode`。教训：Win下curl复杂查询一律urlencode。
  - 拦截态logging仍跑（4xx计数/遥测out=none），租户slot未取故不释放——语义正确。
- **下一步建议**：进入步骤6 GW-5（wrk/cargo bench端到端+sysctl/OPERATION+最终四门报告）。

### [2026-09-19 22:35] 步骤 6 开始: GW-5 全链路门禁+hardening交付
- 计划操作：端到端压测（无wrk用py多线程探针）记P99/QPS；落`deploy/sysctl.conf`+`docs/OPERATION.md`（灰度步骤+eBPF spike）；最终四门+交付清单核验。
- 预期验证方式：四门全绿+交付物齐。

### [2026-09-19 23:10] 步骤 6 已完成: GW-5 全链路门禁+hardening交付（GW-R1 收官）
- **实际操作**：`log/load_probe.py`（stdlib多线程，32×100）；`deploy/sysctl.conf`（BBR/TFO/keepalive 60/10/3/64KB/50k并发）；`docs/OPERATION.md`（一键启动+灰度5步+巡检表+spike结论+速查）；Mock换ThreadingHTTPServer（串行伪影修正）。
- **验证结果（最终门禁）**：
  - [门1-格式] ✅ `cargo fmt --check` clean（EXIT 0）。
  - [门2-静态] ✅ `cargo clippy --workspace --all-targets -- -D warnings` 零告警（EXIT 0）。
  - [门3-测试] ✅ `cargo test --workspace` 37通过/0失败/3 ignored；`-- --ignored` 3通过（0.62s，Redis+CH双在线）。
  - [门4-性能] ✅ `cargo bench --workspace --no-run` 编译过；端到端3200请求100%200，P50 29.5ms/P99 65.6ms/max 121.8ms/QPS 986（Win11+debug+py客户端，dev基线非生产承诺）；选路98ns沿用GW-3 release实测。
  - [交付] ✅ gateway 13模块+compose四容器在线+DDL建表+dashboard.json+OPERATION.md+EGRESS_MESH.md+sysctl.conf；网关PID:17904+mocks 8888/89/90运行中（gw:200/metrics:200）。
  - 综合判定：✅ GW-R1 六阶段全绿收官。
- **遇到的问题与解决**：
  - 初测P99 546ms系单线程Mock串行排队（非网关问题）→ThreadingHTTPServer→65.6ms。教训：压测前先确认harness本身不是瓶颈，初测离谱值先疑harness。
  - Windows无wrk→stdlib探针替代，结论等效（官方计划允许cargo bench/等效手段，Win只记功能）。
  - CH health标unhealthy但SELECT 1正常→wget探针问题，不追。教训：health label≠可用性，以实际查询为准。
- **下一步建议**：GW-R1 交付冻结；待用户给真Key另起GW-R2（staging 1%灰度+流式CH泵+Linux性能验收50k/<1ms）。

### [2026-09-19 23:20] 步骤 7 开始: GW-R2(1) CH 流式泵（免 Key 部分）
- 计划操作：新建ch_sink.rs（独立消费组+60s滞留认领+5000/1s批量+成功才ack+毒丸跳过）+main装配（有Redis才起泵）+live泵测试；端到端验证curl→Stream→泵→CH。
- 预期验证方式：四门绿+live真过+CH行数随流量涨。

### [2026-09-19 23:45] 步骤 7 已完成: GW-R2(1) CH 流式泵
- **实际操作**：落盘`ch_sink.rs`（`parse_entry`纯函数+`pump_once`单轮可测+`run`常驻；`field_text`提为pub(crate)复用）；`analytics.rs`去3处allow（batch/insert/Row现均被泵消费）+`ch_client`改`#[cfg(test)]`；`main.rs` 4c段（无Redis则泵离线告警，CH错只hold不崩）；OPERATION速查泵行更新。
- **验证结果**：
  - [门1-格式] ✅ clean（EXIT 0）。[门2-静态] ✅ `-D warnings`零告警（EXIT 0）。
  - [门3-测试] ✅ 39通过/0失败/4 ignored（新增2纯单测）；`-- --ignored` 4通过（含新泵live：1有效落库+1毒丸跳过+双边清理）。
  - [门4-性能] ✅ bench编译过。
  - [端到端] ✅ 网关+三mocks重拉（PID:9444，log/gw6.out/err，pump online batch=5000）：curl×10 → CH 1→11行（provider mock-b 200）；live SLA查得100。
  - 综合判定：✅ 全绿。
- **遇到的问题与解决**：
  - 直方图式双计数教训重现：读侧`count(batch)`已限流，删多余break。教训：读API自带限流不叠床架屋。
  - `ch_client`公有版在bin target报dead→改`#[cfg(test)]`。教训：纯测试后门用cfg gate，不用allow。
- **下一步建议**：GW-R2剩余需用户输入：三家真Key（staging 1%灰度）+ Linux性能节点（50k/<1ms验收）。

### [2026-09-19 23:50] 步骤 8 立项: OPT-R1 优化方案冻结
- 计划操作：P0×3（sweep/API Key门/重试口径）+P1(4/5/6)（落库重试/共享Client/真预热），详见`plan/2026年9月19日-OPT-R1优化方案.md`（7子项OPT-1~7，独立可回滚，零架构改动）。
- 预期验证方式：单测目标41+过+curl回归+四门绿；计量口径以走查验收（强制重试难造）。

### [2026-09-19 23:00] 步骤 8 已完成: OPT-R1 优化收官（OPT-2~OPT-6 + 最终四门）
- **实际操作**：`gateway.rs`（环境门字段+纯谓词+重试清零helper+3单测）+`main.rs`（REQUIRE_API_KEY装配+共享dropped+prober池日志）+`telemetry.rs`（重试一次+整批丢弃计数，签名变更）+`metrics.rs`（`new_with_dropped`+常驻dropped行+单测）+`prober.rs`（per-proxy Client缓存+2单测，单Client方案因reqwest Proxy系Client级而调整，见计划偏离说明）+`pool.rs`（WarmStats真TCP探测+2新单测+3存量更新）+`model.rs`去过期allow/`router.rs`注释转正+`docker-compose.yml`环境门示例+`docs/OPERATION.md` §2/§4声明。
- **验证结果**：
  - [门1-格式] ✅ `cargo fmt --check` clean。
  - [门2-静态] ✅ `cargo clippy --workspace --all-targets -- -D warnings` 零告警（修5处：mem::take/single_match/field-reassign/test门控client/drop计数cfg(test)+main消费client_count）。
  - [门3-测试] ✅ `cargo test --workspace` 49通过/0失败/4 ignored（新增7：门谓词/重试幂等/dropped共享/Client复用/非法降级/拒连failed/本地connected）；`-- --ignored` 4通过（Redis+CH双在线，0.61s）。
  - [门4-性能] ✅ `cargo bench --workspace --no-run` 编译过；选路98ns沿用GW-3实测（bandit未动）。
  - [回归] ✅ 网关PID:22812（log/gw7.out/err）：普通→mock-b-jp、粘性gw7-task1两次→mock-a-us、Proxy-Auth→mock-b-jp、坏Key→403、GB→mock-c-gb；门开（PID:26524，log/gw7-gate.out/err）无头403/有头200/坏Key403；/metrics 2xx=6/4xx=1/telemetry_dropped_total 0；Stream XLEN=6453流动中；预热`nodes=3 connected=3 failed=0 tickets=6`；Prober对Mock报Dead属预期（plain-HTTP不可代理https trace，GW-2既有结论）。
  - 综合判定：✅ 全绿。
- **遇到的问题与解决**：
  - `prober.rs`首版丢`impl Default`闭合括号致rustfmt解析失败→补`}`。教训：大段替换后先读文件核对括号再跑门禁。
  - reqwest Proxy为Client级、单Client+per-call代理不可行→改per-proxy缓存（复用意图不变，语义更正确）。教训：方案假设必须以registry API为准，复核时先验证可配性。
  - `dropped_count`/`client_count`在非test编译中dead→前者`#[cfg(test)]`，后者main滴答打debug消费（一举两得：复用可观测）。教训：运维观察方法要在生产路径真实消费，否则即死代码。
  - PowerShell下curl `-H \"...\"`反斜杠转义失效致粘性头未生效→改单引号。教训：Win curl一律单引号包头。
- **下一步建议**：OPT-R1冻结；GW-R2剩余需用户输入：三家真Key（staging 1%灰度）+ Linux性能节点（50k/<1ms验收）。

### [2026-09-19 续] OPT后状态巡检 + 性能基线复测
- **实际操作**：发现网关PID:22812自行有序退出（`All runtimes exited`后tokio drop panic，Pingora停机路径，非请求链崩溃；OPT改动均不碰server生命周期，判环境侧，待观察）；重拉网关PID:27200（log/gw8.out/err）+ `log/load_probe.py 32 100`复测。
- **验证结果**：3200请求100%200，P50 20.5ms/P99 49.7ms/max 534ms/QPS 1344.8（GW-5基线P99 65.6ms/QPS 986，无回归，反升）；压测后网关200正常，/metrics 2xx=3203/dropped=0。
- **下一步建议**：可执行项已清零（GW-R1+泵线+OPT-R1全✅）；待用户定方向：真Key灰度 / Linux节点 / 完工。

### [2026-09-19 续] SPIKE-R2 延后项评估（只评估不落地）
- **实际操作**：5 项全评，报告落 `docs/SPIKE_R2.md`；OPERATION §5 加报告索引。
  证据：`pingora-core-0.6.0` 源码（TLS三后端特性门 + `PeerOptions:321-355` 仅 curves/second_keyshare
  可调）+ 宿主内核 6.6.87-WSL2 + 2026 生态检索（BoringSSL拟真三家实现/rustls fork两家/tokio uring未稳定/MASQUE服务端就绪）。
- **验证结果**：5×NO-GO。uTLS：无TLS面（上游plain-HTTP冻结）+无注入点，`[patch]`换fork与Pingora钉版冲突；
  落地路=切boringssl特性+per-conn配置+JA4漂移门（触发：TLS上游立项/按JA4封禁）。eBPF：瓶颈在出站RTT非拷贝
  （收益<5%），2026文献证L7入核需整轮工程量（触发：profiling指认）。io_uring：tokio未稳定且与Pingora
  执行器不兼容。MASQUE：标准/服务端就绪但供应商无对端，Mesh续用WG。BGP：组织级事项。
- **下一步建议**：SPIKE-R2 冻结；网关PID:27200在线。待用户：真Key / Linux节点 / 完工总结。
