# Changelog（Keep-a-Changelog 口径；Phase C7 回填 24 步大事记）

## [1.0.0-Unreleased]
### Added
- 步骤 1 GW-0：workspace＋Docker 依赖＋DDL＋门禁基线。
- 步骤 2 GW-1：网关 MVP（model/router/gateway/main＋5 单测＋curl 三用例）。
- 步骤 3 GW-2：双轨自愈（telemetry/circuit/prober＋Redis Stream/PubSub）。
- 步骤 4 GW-3：LinUCB d=4＋基础指纹＋预热池（select<200ns）。
- 步骤 5 GW-4：企业运营（analytics/tenant/vendor 三家＋Grafana）。
- 步骤 6 GW-5：全链路门禁＋hardening（P99/QPS 基线＋sysctl/OPERATION）。
- 步骤 7 GW-R2(1)：CH 流式泵（Stream→数仓常驻）。
- 步骤 8 OPT-R1：P0×3＋P1（sweep/API Key 门/重试口径/落库重试/共享 Client/真预热）。
- 步骤 9 OPT-R2：R2-1~R2-9（选路可恢复/真加权/重试换节点/租户计费门/遥测幂等/背压/性能/并发/安全收尾）。
- 步骤 10 FreePool v2：FullCheck 三级＋canary/EWMA/SourceGuard/ETag/淘汰/env 全接线。
- 步骤 11 Phase 2 SOCKS egress（翻译桥＋选路隔离＋filter 短路）。
- 步骤 12 Phase 3 画像学习（分档遗忘/风险溢价/composite/GeoIP 降级/报表）。
- 步骤 13 OPT-R3（socks 对齐 bandit/out_ip 隔离/日志移出/礼貌轮询）。
- 步骤 14 Phase 4 执法运维（mismatch 开关＋GeoLite2 更新脚本）。
- 步骤 15 Phase 5 韧性验证（Redis/CH/Mock 断电演练＋P0 真 bug 修复）。
- 步骤 16 DOC-R1：文档一致性（dashboard 零漂移/OPERATION 修复）。
- 步骤 17 剩余事项（真 Key 灰度冻结/Linux 验收/JA4 复核 OUT/完工盘点）。
- 步骤 18/19 FreeProxy 实测＋大样本复测（直探对照＋网关两档＋存量回归）。
- 步骤 20 REVIEW-R2（2×P0＋6×P1＋P2 批）。
- 步骤 21 DOC-S1：文档套件（README＋LICENSE＋架构/技术/组件/手册双语）。
- 步骤 22 USE 便捷落地（前置适配器/SDK/启停脚本/USAGE）。
- 步骤 23 VPN-IMMUNE（共享 Client 直连＋探针基线＋三路对照）。
- 步骤 24 NEXT 全迭代 B（免费线效果：预筛/分页/多源/复检/漏斗/因子沉入/intake 上限）。
- 步骤 24 NEXT 全迭代 A（数据面加固：PubSub 自愈/桥记账/排除集/prober 三路/指标/粘滞复核）。
- 步骤 24 NEXT 全迭代 C（运维产品化：看护/日志轮转/告警/CH TTL/CI/容器化/CHANGELOG）。
- 步骤 24 NEXT 全迭代 D（安全收紧：free 敏感头拒绝/敞口声明/破坏性变更提案冻结）。
- 步骤 25 PORT-8916：网关默认端口避开本机 cvat traefik 争用（8080→8916）。

### Changed
- 网关数据面默认端口 8080→8916；CH 原生端口避让 127.0.0.1:9010:9000。
- D3 安全收紧（用户拍板执行）：`REQUIRE_API_KEY` 缺省 0→1（无头 403，`0` 显式关闭）＋`GATEWAY_ADDR` 缺省 `0.0.0.0:8916`→`127.0.0.1:8916`（容器/局域网显式覆写）；SDK/脚本/文档同步带开发 Key。
- CH 遥测表加 90 天 TTL；prometheus 加 4 告警规则；CI/live 双 workflow。

### Fixed
- R2-4 XADD 非法 ID；Phase 5 演练抓获 P0 真 bug；VPN 代理污染（no_proxy＋直连基线）。
- 看护脚本解析/重拉路径/重复拉起三修（$PSScriptRoot＋字面量＋3-strike）。

## 发版流程
1. `TASK_PLAN.md` 定步骤＋dated 计划冻结＋EXEC 立项条。
2. TDD（红→绿单测）＋四门（fmt/clippy/test/bench＋release bandit）。
3. live 先验依赖（PONG/Ok，无 SKIP）＋curl 存量回归＋CHANGELOG 同步。
4. 落库三件套（计划状态表＋EXEC 完成条＋TASK ✅），禁未授权 commit。
5. 发版打 tag 前重跑 V 口径（门禁＋两档 free＋Grafana 有数）。
