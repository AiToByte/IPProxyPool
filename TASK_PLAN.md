# 任务总体执行规划: GW-R1 企业级IP代理池网关落地

> 创建/更新时间: 2026-09-19 12:00
> 当前状态: 步骤 33 OPT-R8 凭据与运维脚本加固已完成（凭据出 argv 收敛／backup 两道闸实测／watchdog 去 mock 化四场景／free_test 四态判定六场景／restore 真恢复实现待演练；226 单测+四门绿）；步骤 1~33 已完成（步骤 32 附：CH 迁移已执行验证）
> 跟踪表: `plan/2026年9月19日-GW-R1实施计划.md`（动态更新，状态以该文件为准）+ `plan/2026年9月21日-FreePool实施计划-v2.md`（FreePool 第二线，supersede v1）
> 优化表: `plan/2026年9月19日-OPT-R1优化方案.md`（已收官） + `plan/2026年9月21日-OPT-R2优化方案.md`（本轮）

## 执行进度清单
- [x] **步骤 1**: GW-0 基线脚手架 workspace+Docker+DDL+门禁基线（可验证标准：cargo check过+Redis PONG+CH建表成功+四门基线绿）
- [x] **步骤 2**: GW-1 P1网关MVP model/router/gateway/main+5单测+curl三用例（可验证标准：cargo test 5+通过+curl 200+clippy零告警） ✅ 已完成（5单测过+curl三用例200+四门绿，证据见EXEC_LOG 17:10条+log/运行日志）
- [x] **步骤 3**: GW-2 双轨自愈 telemetry/circuit/prober+Redis Stream/PubSub（可验证标准：Mock403 50ms隔离+集成测试绿） ✅ 已完成（12单测过+2 live真过+隔离域503/他域200+四门绿，证据见EXEC_LOG GW-2条+log/gw2.out/err）
- [x] **步骤 4**: GW-3 LinUCB d=4+基础指纹+预热池（可验证标准：bandit单测绿+选路bench<200ns记录） ✅ 已完成（24单测过+release 98ns+curl回归全过+四门绿，证据见EXEC_LOG GW-3条+log/gw3.out/err）
- [x] **步骤 5**: GW-4 企业运营 analytics/tenant/vendor三家+Grafana（可验证标准：落库集成过+租户/套利单测绿） ✅ 已完成（37单测过+3 live真过+租户429单测/403 curl+套利策略单测+Prom up=1+四门绿，证据见EXEC_LOG GW-4条+log/gw4.out/err）
- [x] **步骤 6**: GW-5 全链路门禁+hardening交付（可验证标准：四门全绿报告+OPERATION/sysctl落盘） ✅ 已完成（P99 65.6ms/QPS 986+sysctl/OPERATION+最终四门绿，证据见EXEC_LOG GW-5条+log/gw5.out/err+load_probe）
- [x] **步骤 7**: GW-R2(1) CH 流式泵 Stream→数仓常驻（可验证标准：curl 10 → CH +10行+四门绿） ✅ 已完成（39单测过+4 live真过+端到端+10行+SLA100+四门绿，证据见EXEC_LOG GW-R2泵线条+log/gw6.out/err）
- [x] **步骤 8**: OPT-R1 优化（P0×3+P1 4/5/6：sweep/API Key门/重试口径/落库重试/共享Client/真预热）✅ 已完成（49单测过+4 live过+curl六用例+四门绿，证据见EXEC_LOG OPT-R1条+log/gw7.out/err）方案见`plan/2026年9月19日-OPT-R1优化方案.md`
- [x] **步骤 9**: OPT-R2 优化（R2-1~R2-9：选路可恢复+真加权/重试换节点/租户计费门/遥测幂等/_sink背压/数据面性能/后台并发/运维安全收尾/最终回归）方案见`plan/2026年9月21日-OPT-R2优化方案.md` ✅ 已完成（82单测+4真live+四门绿+curl全回归，附 R2-4 XADD 非法 ID 的 P0 修复与 R2-5~R2-8 live 口径更正，证据见EXEC_LOG R2-9条+log/gw9.out/err）
- [x] **步骤 10**: FreePool 第二供应线（v2 迭代版，supersede v1：FullCheck匿名度三级+canary/EWMA健康分动态权重/指数backoff/SourceGuard熔断/ETag/容量淘汰/全env接线）方案见`plan/2026年9月21日-FreePool实施计划-v2.md`（Task 1~13） ✅ 已完成（107 单测+4 真 live+四门绿+ELITE 0/1 两档 curl 全回归，证据见 EXEC_LOG 步骤 10 条+log/gw10-free0|free1.out/err）
- [x] **步骤 11**: Phase 2 SOCKS egress（EgressProto+选路隔离/握手/翻译桥/filter短路/free全收/prober+pool/main env）方案见`plan/2026年9月22日-Phase2-SOCKS实施计划.md`（P2-1~P2-8） ✅ 已完成（127 单测+4 真 live+四门绿+存量零变化+本地 E2E 五断言，证据见 EXEC_LOG 步骤 11 条+log/gw11-socks.out/err）
- [x] **步骤 12**: Phase 3 画像与学习增强（分档遗忘+风险溢价/免费套利/composite/GeoIP降级/报表）方案见`plan/2026年9月22日-Phase3-画像与学习增强实施计划.md`（P3-1~P3-6） ✅ 已完成（136 单测+4 真 live+release bandit<200ns+四门绿+存量零变化+E2E重跑，证据见 EXEC_LOG 步骤 12 条+log/gw12.out/err）
- [x] **步骤 13**: OPT-R3 优化（R3-1 socks选路对齐bandit/R3-2 out_ip+exit隔离/R3-3 删冗余注记/R3-4 日志移出/R3-5 源礼貌轮询/R3-6门禁）方案见`plan/2026年9月22日-OPT-R3优化方案.md` ✅ 已完成（141 单测+4 真 live+release bandit+四门绿+存量零变化+E2E重跑+CH out_ip断言，证据见 EXEC_LOG 步骤 13 条+log/gw13-socks.out/err）
- [x] **步骤 14**: Phase 4 执法与运维（mismatch执法开关/GeoLite2更新脚本）方案见`plan/2026年9月22日-Phase4-执法与运维实施计划.md`（P4-1~P4-3） ✅ 已完成（143 单测+4 真 live+四门绿+存量零变化+E2E重跑+enforce对照，证据见 EXEC_LOG 步骤 14 条+log/gw14-socks.out/err+gw14-enforce.out/err）
- [x] **步骤 15**: Phase 5 韧性验证（Redis/CH/Mock断电演练）方案见`plan/2026年9月22日-Phase5-韧性验证实施计划.md`（P5-1~P5-4） ✅ 已完成（144 单测+4 真 live+四门绿+三演练全自愈+抓获P0真bug已修，证据见 EXEC_LOG 步骤 15 条+log/gw15*.out/err）
- [x] **步骤 16**: DOC-R1 文档一致性修复（dashboard零漂移/OPERATION 1中2小）方案见`plan/2026年9月22日-DOC-R1文档一致性修复实施计划.md` ✅ 已完成（dashboard逐名验证零漂移+OPERATION三处修复，证据见 EXEC_LOG 步骤 16 条）
- [x] **步骤 17**: 剩余事项执行计划（真Key灰度/ Linux验收/JA4复核/完工冻结）方案见`plan/2026年9月22日-剩余事项执行计划.md` ✅ 已完成（REM-1/REM-2 冻结等输入＋REM-3 JA4复核仍OUT＋REM-4 完工盘点；证据见 EXEC_LOG 步骤 17 条）
- [x] **步骤 18**: FreeProxy 实测（小批量拉取免费节点代理测试：直探对照+网关两档+存量回归）方案见`plan/2026年9月23日-FreeProxy实测计划.md`（F1~F6） ✅ 已完成（直探 13候选0过+网关 ELITE0 pool0/ELITE1 pool1 elite+存量回归全绿+147单测，证据见 EXEC_LOG 步骤 18 条+log/gw16*+free_probe_report）
- [x] **步骤 19**: FreeProxy大样本复测（生产默认limit=100单快照：并发直探+网关4tick稳定性）方案见`plan/2026年9月23日-FreeProxy大样本复测计划.md`（F1~F6） ✅ 已完成（直探57候选0过+网关Elite socks5偶发1+间隔无漂移+回归精确对账，证据见 EXEC_LOG 步骤 19 条+log/gw19*+free_big_report）
- [x] **步骤 20**: REVIEW-R2 全局复审修复优化（三路审计→2×P0+6×P1+P2批）方案见`plan/2026年9月23日-REVIEW-R2修复优化实施计划.md`（Q1~Q10） ✅ 已完成（156 单测+4 live+release bandit<200ns+curl回归XLEN/CH精确+10，证据见 EXEC_LOG 步骤 20 条+log/gw20.out/err）
- [x] **步骤 21**: DOC-S1 文档套件（README+LICENSE+架构/技术/组件/手册双语+CONTRIBUTING）方案见`plan/2026年9月23日-DOC-S1文档套件实施计划.md`（D1~D7＋V） ✅ 已完成（7 新文件双语+MIT/aitobyte+交叉核对零MISS，证据见 EXEC_LOG 步骤 21 条）
- [x] **步骤 22**: USE-便捷落地（前置适配器+SDK+启停脚本+USAGE）方案见`plan/2026年9月23日-USE-便捷落地实施计划.md`（U1~U4＋V） ✅ 已完成（adaptor 200 vs 直连400＋SDK自检＋status全绿，证据见 EXEC_LOG 步骤 22 条+log/ipp-forward.out/err）
- [x] **步骤 23**: VPN-IMMUNE 加固（共享Client禁用系统代理+探针基线直连+三路对照）方案见`plan/2026年9月23日-VPN-IMMUNE加固实施计划.md`（H1~H4） ✅ 已完成（157 单测+机制实锤双面验证+三路对照G≠V，证据见 EXEC_LOG 步骤 23 条+log/gw23*.out/err）
- [x] **步骤 24**: NEXT全迭代（B免费效果/A数据面/C运维产品化/D安全收紧，20项）方案见`plan/2026年9月24日-NEXT全迭代实施计划.md` ✅ 已完成（169 单测＋live 4＋bandit 12＋四门绿＋回归全绿；C6 容器 RUN 环境阻断诚实部分，D3 零执行；证据见 EXEC_LOG 步骤 24-C/D/V 条）
- [x] **步骤 25**: PORT-8916迁移（网关默认端口避开cvat traefik争用）方案见`plan/2026年9月24日-PORT-8916迁移实施计划.md` ✅ 已完成（168 单测＋四门绿＋:8916 全回归＋traefik 未动；证据见 EXEC_LOG 步骤 25 条）
- [x] **步骤 26**: OPT-R4 优化（A 数据面正确性/B 免费线效果/C 运维安全，36 项）方案见`plan/2026年9月25日-OPT-R4优化方案.md` ✅ 已完成（201 单测＋live 4＋bandit 12＋四门绿＋回归全绿；证据见 EXEC_LOG 步骤 26-A/B/C/V 条）
- [x] **步骤 27**: D3 破坏性收紧执行（REQUIRE_API_KEY 默认 1＋GATEWAY_ADDR 收 127.0.0.1）方案见 OPT-R4 计划 C13 清单 ✅ 已完成（用户拍板；两默认值＋compose/docs/SDK/脚本/CHANGELOG＋全回归；证据见 EXEC_LOG 步骤 27 条）
- [x] **步骤 28**: OPT-R5 优化（S 稳定性根治/E 代理能力/O 运维收尾，24 项）方案见`plan/2026年9月25日-OPT-R5优化方案.md` ✅ 已完成（209 单测＋live 4＋bandit 12＋四门绿＋回归全绿；E7 spike 后暂缓；证据见 EXEC_LOG 步骤 28-S/E/O/V 条）
- [x] **步骤 29**: DOC-S2 文档五件套（架构/功能/数据流/用户使用/相关开源）方案见`plan/2026年9月26日-DOC-S2文档五件套实施计划.md` ✅ 已完成（5 双语文档＋互链＋口径核对；证据见 EXEC_LOG 步骤 29 条）
- [x] **步骤 30**: 亲手操作指南落库（`docs/HANDS-ON.md` 双语＋README 索引） ✅ 已完成（命令逐条实测口径，证据见 EXEC_LOG 步骤 30 条）
- [x] **步骤 31**: OPT-R6 止血优化（3×P0：free 凭据护栏改节点侧判定／淘汰计数下溢根治／live 门禁修好；P1：4 条裸 spawn 进 supervise）方案见`plan/2026年9月28日-OPT-R6止血优化方案.md` ✅ 已完成（223 单测＋四门绿＋release bandit<200ns；S2 红测有效性经回退实证；S1 端到端已验 D3 语义/显式 free 403/付费零变化，完整绕过场景由集成单测锁定；证据见 EXEC_LOG 步骤 31 条）
- [x] **步骤 32**: OPT-R7 可观测性与部署加固（A：CH 排序键＋跳过索引＋迁移脚本；B：告警语义修复＋三类补齐；C：CH 探针／Prom 卷／receiver；D：PS1 BOM＋直方图桶扩展）方案见`plan/2026年9月28日-OPT-R7可观测性与部署加固方案.md` ✅ 已完成（226 单测＋四门绿＋release bandit<200ns；CH 剪枝 5/5→1/5 实测、CH 容器 unhealthy→healthy 实测、12 条告警 health=ok 实测；D1 的 .gitattributes 方案经实测证伪已改 CI 断言；**002_reorder.sql 已实际执行并验证：备份 3981 → v2 表 3981 → RENAME 后新旧备份三表 3981 对账一致、provider×status 分布逐行一致、SLA 查询正常、索引就位（_old 与 _backup 保留作回滚保险）**；证据见 EXEC_LOG 步骤 32 条）
- [x] **步骤 33**: OPT-R8 凭据与运维脚本加固（A：密码不出 argv 5 项；B：脚本健壮性 4 项——backup 校验／watchdog 去 mock 化／free_test 加 UNKNOWN／restore 真恢复）方案见`plan/2026年9月28日-OPT-R8凭据与运维脚本加固方案.md` ✅ 已完成（226 单测＋四门绿＋release bandit<200ns；B1 双向实测（错误项目名被两道闸拦截且未创建空卷）、B2 四场景、B3 六场景、B4 dry-run 全流程；A5 新增 creds CI 断言双向实测；A1 因 Redis 无 env 替代降级为只做 healthcheck 侧并如实标注；restore -Execute 真恢复属破坏性未演练已登记；证据见 EXEC_LOG 步骤 33 条）


## 关键决策与约束
- Docker一键起依赖；三家全Mock首轮，真Key后补灰度；LinUCB完整d=4 alpha0.4起；指纹基础版不碰utls/boring
- 边界：Windows验功能Linux验性能；eBPF/io_uring/MASQUE本轮spike不落地；BGP只给模板
- 禁止事项：规划外不私自接真供应商Key；不跨await持parking_lot锁；不跳门禁标✅；EXEC_LOG append-only
- 执行铁律（2026-09-19 GW-1沉淀）：bash调用一律显式短timeout（构建300s/常规≤60s）；禁用`Get-NetTCPConnection`（用`curl --max-time`探活）；后台进程必须Python DETACHED detached+双流重定向；项目日志/输出一律落`log/`禁放C盘；卡住先停→查日志/进程→修脚本→重跑
- 已知硬伤冻结7条详见dated计划勘察结论（A2 telemetry缺字段/A3缺函数/A4 clickhouse绑定等）

---
*本文件由 Agent 实时维护，严禁手动随意擦除或修改状态。*
