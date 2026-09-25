// IPProxyPool Node SDK（OPT-R5 E2，同语义复刻 tools/ipp_sdk.py，零依赖，Node 18+ 全局 fetch）。
//
// 网关是反向式 egress 路由：请求打到网关地址，真实上游放 Host 头。
// 本 SDK 做最小封装：URL 拆分、粘滞 session、tier/proto 选择、503 延迟重试。
//
// Usage:
//   const { IPPClient } = require("./ipp_sdk_node.js");
//   const c = new IPPClient("http://127.0.0.1:8916", { session: "job-42" });
//   const { status, body } = await c.get("http://httpbin.org/ip");
// Self-test（仅网关 http://127.0.0.1:8916 ＋ mocks http://127.0.0.1:8888 活着时手动跑，默认不自动跑）：
//   node tools/ipp_sdk_node.js --self-test   # 普通200＋粘滞＋坏Key403＋无头403
// D3 注记：网关默认开 API Key 门；SDK 缺省带开发 Key `default_key`
// （生产传真 Key；apiKey=null 即无头，用于验证 403）。
"use strict";

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

class IPPClient {
  constructor(gateway = "http://127.0.0.1:8916", opts = {}) {
    const u = new URL(gateway);
    const host = u.hostname || "127.0.0.1";
    const port = u.port ? Number(u.port) : 80;
    this.base = `http://${host}:${port}`;
    this.apiKey = opts.apiKey !== undefined ? opts.apiKey : "default_key";
    this.session = opts.session || null;
    this.country = opts.country || null;
    this.tier = opts.tier || null;
    this.proto = opts.proto || null;
    this.timeoutMs = opts.timeoutMs || 10000;
  }

  _headersFor(targetHost) {
    // 头名与网关 parse_routing_spec 严格同名；误名头会被网关静默忽略。
    const h = { Host: targetHost, "User-Agent": "ipp-sdk/1.0" };
    if (this.apiKey) {
      h["X-Api-Key"] = this.apiKey;
    }
    if (this.session) {
      h["X-Proxy-Session"] = this.session;
    }
    if (this.country) {
      h["X-Proxy-Country"] = this.country;
    }
    if (this.tier) {
      h["X-Proxy-Tier"] = this.tier;
    }
    if (this.proto) {
      h["X-Proxy-Proto"] = this.proto;
    }
    return h;
  }

  async get(url, extraHeaders = {}, retries = 1) {
    // 经网关 GET 公网 URL。返回 {status, body:Buffer}。503 延迟 1s 重试一次。
    const t = new URL(url);
    if (t.protocol !== "http:") {
      throw new Error("only plain http targets are supported (no CONNECT tunneling)");
    }
    const targetHost = t.host;
    const path = (t.pathname || "/") + (t.search || "");
    const headers = Object.assign(this._headersFor(targetHost), extraHeaders);
    let last = { status: 0, body: Buffer.alloc(0) };
    for (let attempt = 0; attempt <= retries; attempt++) {
      const ctrl = new AbortController();
      const timer = setTimeout(() => ctrl.abort(), this.timeoutMs);
      try {
        const resp = await fetch(this.base + path, {
          method: "GET",
          headers,
          signal: ctrl.signal,
        });
        const buf = Buffer.from(await resp.arrayBuffer());
        last = { status: resp.status, body: buf };
        if (resp.status === 503 && attempt < retries) {
          await sleep(1000);
          continue;
        }
        return last;
      } catch (e) {
        last = { status: 0, body: Buffer.from(String((e && e.message) || e).slice(0, 200)) };
        if (attempt < retries) {
          await sleep(1000);
          continue;
        }
        return last;
      } finally {
        clearTimeout(timer);
      }
    }
    return last;
  }
}

async function selfTest() {
  const gw = "http://127.0.0.1:8916";
  const assert = require("node:assert/strict");
  const c = new IPPClient(gw);
  const r1 = await c.get("http://127.0.0.1:8888/");
  assert.equal(r1.status, 200, `plain expect 200, got ${r1.status} ${r1.body.slice(0, 80)}`);
  assert.ok(r1.body.includes("mock-"), `body must carry mock marker, got ${r1.body.slice(0, 80)}`);
  // 粘滞确定性证明：country=US 约束下同 session 两次必中 mock-a-us。
  const s = new IPPClient(gw, { session: "sdk-selftest-1", country: "US" });
  const r2 = await s.get("http://127.0.0.1:8888/");
  const r3 = await s.get("http://127.0.0.1:8888/");
  assert.ok(r2.body.equals(r3.body) && r2.body.toString() === "mock-a-us",
    `sticky must pin mock-a-us, got ${r2.body.slice(0, 16)} ${r3.body.slice(0, 16)}`);
  const bad = new IPPClient(gw, { apiKey: "bad" });
  const r4 = await bad.get("http://127.0.0.1:8888/");
  assert.equal(r4.status, 403, `bad key expect 403, got ${r4.status}`);
  // D3：无头请求同样 403（门默认开启）。
  const nokey = new IPPClient(gw, { apiKey: null });
  const r5 = await nokey.get("http://127.0.0.1:8888/");
  assert.equal(r5.status, 403, `missing key expect 403, got ${r5.status}`);
  console.log(`self-test OK: plain=${r1.status} sticky=${r2.body.toString()} badkey=${r4.status} nokey=${r5.status}`);
}

if (require.main === module) {
  if (process.argv.includes("--self-test")) {
    selfTest().catch((e) => {
      console.error(`self-test FAIL: ${(e && e.message) || e}`);
      process.exit(1);
    });
  } else {
    console.log("IPProxyPool Node SDK. Run with --self-test only while gateway/mocks are alive.");
  }
}

module.exports = { IPPClient };
