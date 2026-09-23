# VPN-IMMUNE VPN免疫加固 Implementation Plan（2026年9月23日）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 网关全部出站（抓取＋基线＋复检＋桥＋探针）与 Clash/系统代理无关，结果可复现，不受操作员本机 VPN 状态影响。

**Architecture:** 显式代理客户端已免疫（reqwest 0.12.28 `proxy()` 置 `auto_sys_proxy=false`，源码实锤）；只修无配置共享 Client（一处生产入口 `main.rs:314`）＋探针基线（urllib 默认走系统代理）＋文档注记。

**Tech Stack:** Rust（`ClientBuilder::no_proxy()`）＋Python（`ProxyHandler({})`）；零新依赖。

> **起因：** 用户实测命令返回疑似 VPN 出口 13.213.72.105（经显式 Clash 复测确为当前 VPN 出口）。
> **已证实：** reqwest 显式代理禁用系统代理（`async_impl/client.rs:1414-1418`）；Windows 系统代理 ON（`127.0.0.1:7890`），env 无代理；13.x 不在历史快照、无 CH 行（回溯无结论）。
> **影响面（诚实）：** 无配置 Client（Geonode 抓取＋FullCheck 基线）与 urllib 默认基线跟随系统代理→匿名度分级是“vs VPN 出口”比较（活性/canary 结论不受影响：走显式代理或 fail-closed）；curl.exe 永不走系统代理。
> **铁律：** 禁未授权 commit；`EXEC_LOG.md` append-only；全量门禁；live 复测为本项主验证（env 行为单测会污染并行测试，显式不出，见 H1 Step 注）。
> **跟踪：** `TASK_PLAN.md` 步骤 23；日志：`EXEC_LOG.md`。

---

## §1. File map

| # | 文件 | 改动 | 验收 |
|---|------|------|------|
| H1 | `gateway/src/free_pool.rs`（新增 `shared_client()`）＋`gateway/src/main.rs:314`（改调） | 共享 Client 构建期 `no_proxy()` | 全量门禁绿；live 基线＝直连出口 |
| H2 | `log/probe_free.py`＋`log/probe_free_big.py`（基线 opener 各 2 行） | 基线用 `ProxyHandler({})` 绕系统代理 | `py_compile` 过；重跑快照基线＝curl 直连值 |
| H3 | `docs/OPERATION.md`＋`docs/USAGE.md`（各 2~3 行） | VPN/系统代理敏感性注记 | grep 可见 |
| H4 | live 三路对照 | 直连 D／显式 Clash V／网关 G 同测 | G≠V 或 G∈快照节点即非 Clash 链 |

显式不出：env 行为单测（并行污染，见上）；Clash 开关操作（用户系统，不碰）。

---

## §2. 任务

### Task 1（H1）: 共享 Client 免疫

- [ ] **Step 1:** `free_pool.rs` 新增（`FreePoolConfig` 附近）：

```rust
/// REVIEW-VPN：共享出站 Client（抓取＋基线）：显式禁用系统代理。
/// reqwest 默认跟随 OS 系统代理（Windows 注册表 Clash `127.0.0.1:7890`），
/// 操作员开 VPN 即污染基线/抓取；`no_proxy()` 后行为与操作员 VPN 状态无关。
/// 显式代理客户端（桥/复检/探针）本就免疫（`proxy()` 置 auto_sys_proxy=false）。
pub fn shared_client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().expect("shared client builds")
}
```

- [ ] **Step 2:** `main.rs:314` `reqwest::Client::new()` → `free_pool::shared_client()`（`FreePoolWorker` 已在作用域，`use` 段 `free_pool::{...}` 追加）。
- [ ] **Step 3:** 加冒烟单测（构造不断言行为；行为由 H4 live 覆盖，原因见头部）：

```rust
#[test]
fn shared_client_builds_without_system_proxy() {
    let _ = shared_client();
}
```

- [ ] **Step 4:** `cargo fmt/clippy/test` 全绿（147+9+1 以实测为准；特别看存量本地 stub 单测：此前走直连、修后仍直连，行为一致）

### Task 2（H2）: 探针基线直连

- [ ] **Step 1:** 两脚本基线处 `urllib.request.urlopen(BASE + "/ip")` 改经
`build_opener(ProxyHandler({}))`（空 dict 即禁用系统代理，注释写明；`via_proxy` 显式 handler 本就免疫不动）。
- [ ] **Step 2:** `python -m py_compile` 过；`log/free_demo_sample.json` 重跑基线，对照 `curl.exe http://httpbin.org/ip`（两者一致即免疫生效）。

### Task 3（H3）: 文档注记（OPERATION＋USAGE 各中英 2~3 行）

- 要点：Windows 系统代理 ON 时，未加固前基线跟随 VPN；加固后网关全链路直连（除候选节点本身）；curl.exe 不受系统代理影响；Clash TUN 开启时直连读数以 TUN 规则为准。

### Task 4（H4）: 三路对照复测＋落库

- [ ] **Step 1:** 重启 FREE 网关（60s 节拍），等 pool≥1：D＝`curl httpbin` 直连；V＝`curl -x 127.0.0.1:7890 httpbin`；G＝网关 tier(+socks) 同目标。
- [ ] **Step 2:** 判定：G≠V（且 G≠D 若节点非透明）即非 Clash 链；G∈Geonode 快照 IP 即节点自出口实锤；CH 行 corroborate。
- [ ] **Step 3:** 回滚默认＋落库（本计划 §3＋EXEC 完成条＋TASK 23✅；禁未授权 commit）。

---

## §3. 状态总览（2026-09-23 实施完成）

| 子项 | 内容 | 状态 | 验收（实测值） |
|------|------|------|------|
| H1 | 共享 Client 免疫 | ✅ 已完成 | 157 全绿；`main.rs:314` 改调＋冒烟单测 |
| H2 | 探针基线直连 | ✅ 已完成 | py_compile 过；bypass 基线 27.x＝curl 直连；默认 opener 基线 13.x＝VPN（机制实锤正反两面） |
| H3 | 文档注记 | ✅ 已完成 | OPERATION＋USAGE 中英注记 |
| H4 | 三路对照复测 | ✅ 已完成 | D=27.x 稳定／V=13.x 稳定／G 出口 104.x·122.x·43.x（皆≠D/V）＋Squid 第三方错误页＋CH 行；回滚默认 200 |

## Self-Review

1. **覆盖性：** -request 全链路（抓取/基线/复检/桥/探针/脚本）逐条定级：免疫处不动，敏感处全修。
2. **诚实性：** 单测不出 env 行为（并行污染，如实声明）；回溯 13.x 不下断言（无快照/CH 行证据）。
3. **最小性：** 生产改动 2 行＋1 helper；无语义变化（Clash 关时行为逐字一致）。
