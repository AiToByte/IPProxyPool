# 任务总体执行规划: GW-R1 企业级IP代理池网关落地

> 创建/更新时间: 2026-09-19 12:00
> 当前状态: 步骤 8 OPT-R1 方案已冻结待执行（P0×3+P1 4/5/6）；GW-R2剩余待真 Key / Linux 节点
> 跟踪表: `plan/2026年9月19日-GW-R1实施计划.md`（动态更新，状态以该文件为准）
> 优化表: `plan/2026年9月19日-OPT-R1优化方案.md`

## 执行进度清单
- [x] **步骤 1**: GW-0 基线脚手架 workspace+Docker+DDL+门禁基线（可验证标准：cargo check过+Redis PONG+CH建表成功+四门基线绿）
- [x] **步骤 2**: GW-1 P1网关MVP model/router/gateway/main+5单测+curl三用例（可验证标准：cargo test 5+通过+curl 200+clippy零告警） ✅ 已完成（5单测过+curl三用例200+四门绿，证据见EXEC_LOG 17:10条+log/运行日志）
- [x] **步骤 3**: GW-2 双轨自愈 telemetry/circuit/prober+Redis Stream/PubSub（可验证标准：Mock403 50ms隔离+集成测试绿） ✅ 已完成（12单测过+2 live真过+隔离域503/他域200+四门绿，证据见EXEC_LOG GW-2条+log/gw2.out/err）
- [x] **步骤 4**: GW-3 LinUCB d=4+基础指纹+预热池（可验证标准：bandit单测绿+选路bench<200ns记录） ✅ 已完成（24单测过+release 98ns+curl回归全过+四门绿，证据见EXEC_LOG GW-3条+log/gw3.out/err）
- [x] **步骤 5**: GW-4 企业运营 analytics/tenant/vendor三家+Grafana（可验证标准：落库集成过+租户/套利单测绿） ✅ 已完成（37单测过+3 live真过+租户429单测/403 curl+套利策略单测+Prom up=1+四门绿，证据见EXEC_LOG GW-4条+log/gw4.out/err）
- [x] **步骤 6**: GW-5 全链路门禁+hardening交付（可验证标准：四门全绿报告+OPERATION/sysctl落盘） ✅ 已完成（P99 65.6ms/QPS 986+sysctl/OPERATION+最终四门绿，证据见EXEC_LOG GW-5条+log/gw5.out/err+load_probe）
- [x] **步骤 7**: GW-R2(1) CH 流式泵 Stream→数仓常驻（可验证标准：curl 10 → CH +10行+四门绿） ✅ 已完成（39单测过+4 live真过+端到端+10行+SLA100+四门绿，证据见EXEC_LOG GW-R2泵线条+log/gw6.out/err）
- [x] **步骤 8**: OPT-R1 优化（P0×3+P1 4/5/6：sweep/API Key门/重试口径/落库重试/共享Client/真预热）✅ 已完成（49单测过+4 live过+curl六用例+四门绿，证据见EXEC_LOG OPT-R1条+log/gw7.out/err）方案见`plan/2026年9月19日-OPT-R1优化方案.md`

## 关键决策与约束
- Docker一键起依赖；三家全Mock首轮，真Key后补灰度；LinUCB完整d=4 alpha0.4起；指纹基础版不碰utls/boring
- 边界：Windows验功能Linux验性能；eBPF/io_uring/MASQUE本轮spike不落地；BGP只给模板
- 禁止事项：规划外不私自接真供应商Key；不跨await持parking_lot锁；不跳门禁标✅；EXEC_LOG append-only
- 执行铁律（2026-09-19 GW-1沉淀）：bash调用一律显式短timeout（构建300s/常规≤60s）；禁用`Get-NetTCPConnection`（用`curl --max-time`探活）；后台进程必须Python DETACHED detached+双流重定向；项目日志/输出一律落`log/`禁放C盘；卡住先停→查日志/进程→修脚本→重跑
- 已知硬伤冻结7条详见dated计划勘察结论（A2 telemetry缺字段/A3缺函数/A4 clickhouse绑定等）

---
*本文件由 Agent 实时维护，严禁手动随意擦除或修改状态。*
