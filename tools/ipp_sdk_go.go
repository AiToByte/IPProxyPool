// IPProxyPool Go SDK（OPT-R5 E2，同语义复刻 tools/ipp_sdk.py，stdlib 零依赖 net/http）。
//
// 网关是反向式 egress 路由：请求打到网关地址，真实上游放 Host 头。
// 本 SDK 做最小封装：URL 拆分、粘滞 session、tier/proto 选择、503 延迟重试。
//
// Usage:
//
//	本文件是 package main 的单文件程序（含自检入口），直接运行：
//	    go run tools/ipp_sdk_go.go
//
//	要作为库引入时，把本文件复制/软链到你的 package 目录并**改掉
//	`package main` 这一行**（以及文件末尾的 `main()` 函数），
//	然后按普通 Go 包使用：
//	    c := NewIPPClient("http://127.0.0.1:8916", WithSession("job-42"))
//	    status, body, err := c.Get("http://httpbin.org/ip", nil, 1) // 503 自动延迟重试 1 次
//	    if err != nil {
//	        // 编程错误：target 不是 http（网关不做 CONNECT 隧道）
//	        log.Fatal(err)
//	    }
//	    if status != 200 {
//	        // 网络/网关失败：诊断信息在 body 里
//	        log.Printf("status=%d body=%s", status, body)
//	    }
//
//	注意（OPT-R9 A3 实测修正）：Go **禁止 import 一个 package main**，
//	故本文��不再写 `import ipp "path/to/ipp_sdk_go"` 那种示例——
//	那种写法 100% 编译失败（审阅发现的历史缺陷，本轮已修正文档）。
//
// Self-test（仅网关 http://127.0.0.1:8916 ＋ mocks http://127.0.0.1:8888 活着时手动跑，默认不自动跑）：
//
//	go run tools/ipp_sdk_go.go --self-test   # 普通200＋粘滞＋坏Key403＋无头403
//
// D3 注记：网关默认开 API Key 门；SDK 缺省带开发 Key `default_key`
// （生产传真 Key；WithAPIKey("") 即无头，用于验证 403）。
package main

import (
	"bytes"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"strings"
	"time"
)

// IPPClient 经网关 GET 公网 URL 的最小客户端。
type IPPClient struct {
	Base     string
	APIKey   string
	NoKey    bool
	Session  string
	Country  string
	Tier     string
	Proto    string
	Timeout  time.Duration
	HTTPDoer *http.Client
}

// Option 构造选项。
type Option func(*IPPClient)

// WithAPIKey 设置鉴权 Key；传 "" 表示无头（用于验证 403）。
func WithAPIKey(k string) Option {
	return func(c *IPPClient) {
		if k == "" {
			c.NoKey = true
			c.APIKey = ""
			return
		}
		c.NoKey = false
		c.APIKey = k
	}
}

// WithSession 设置粘滞 session。
func WithSession(s string) Option { return func(c *IPPClient) { c.Session = s } }

// WithCountry 设置国家约束。
func WithCountry(s string) Option { return func(c *IPPClient) { c.Country = s } }

// WithTier 设置档位。
func WithTier(s string) Option { return func(c *IPPClient) { c.Tier = s } }

// WithProto 设置协议。
func WithProto(s string) Option { return func(c *IPPClient) { c.Proto = s } }

// NewIPPClient 解析网关地址并返回客户端（缺省 apiKey=default_key）。
func NewIPPClient(gateway string, opts ...Option) *IPPClient {
	if gateway == "" {
		gateway = "http://127.0.0.1:8916"
	}
	u, err := url.Parse(gateway)
	if err != nil || u.Hostname() == "" {
		u, _ = url.Parse("http://127.0.0.1:8916")
	}
	host := u.Hostname()
	if host == "" {
		host = "127.0.0.1"
	}
	port := u.Port()
	if port == "" {
		port = "80"
	}
	c := &IPPClient{
		Base:    "http://" + host + ":" + port,
		APIKey:  "default_key",
		Timeout: 10 * time.Second,
	}
	for _, o := range opts {
		o(c)
	}
	c.HTTPDoer = &http.Client{Timeout: c.Timeout}
	return c
}

// headersFor 组装与网关 parse_routing_spec 严格同名的头；误名会被静默忽略。
func (c *IPPClient) headersFor(targetHost string) map[string]string {
	h := map[string]string{
		"User-Agent": "ipp-sdk/1.0",
	}
	if !c.NoKey && c.APIKey != "" {
		h["X-Api-Key"] = c.APIKey
	}
	if c.Session != "" {
		h["X-Proxy-Session"] = c.Session
	}
	if c.Country != "" {
		h["X-Proxy-Country"] = c.Country
	}
	if c.Tier != "" {
		h["X-Proxy-Tier"] = c.Tier
	}
	if c.Proto != "" {
		h["X-Proxy-Proto"] = c.Proto
	}
	_ = targetHost
	return h
}

// Get 经网关 GET 公网 URL。返回 (status, body, err)。
//
// # 错误契约（OPT-R10 C1：与其余 4 语言 SDK 统一）
//
// 三类失败按**是否可预期**分两个通道：
//
//  1. **编程错误**（本函数的参数用错了）→ 返回 **非 nil 的 err**，
//     status 为 0。当前只有一种：target 不是 `http://` scheme——网关不做
//     CONNECT 隧道（见 docs/SPIKE_R2.md 的 E7 结论），传 https 必然失败，
//     与其静默返回一个 status=0 让调用方误判成「网络故障」，不如显式报错。
//  2. **网络/网关失败**（连接失败、超时、503 重试耗尽）→ **不返回 err**，
//     只把诊断信息放进 body、status 置 0。这些是运行期**可预期**的瞬时
//     故障，让调用方写 try/catch 吞掉它们反而丢失可观测性。
//
// Go 用返回值 `error` 而非 panic，这是**语言惯例差异**，不是契约不一致：
// 其余 4 语言「抛异常」在 Go 里的等价物就是返回 `error`。跨语言看，
// 语义完全一致：**编程错误走独立通道，网络失败走返回值。**
//
// 503 会延迟 1s 自动重试 `retries` 次（沿 OPT-R5 E2 口径）。
func (c *IPPClient) Get(target string, extra map[string]string, retries int) (int, []byte, error) {
	t, err := url.Parse(target)
	if err != nil {
		return 0, nil, fmt.Errorf("invalid target %q: %w", target, err)
	}
	if t.Scheme != "http" {
		return 0, nil, fmt.Errorf("only plain http targets are supported, got %q (no CONNECT tunneling)", t.Scheme)
	}
	targetHost := t.Host
	path := t.EscapedPath()
	if path == "" {
		path = "/"
	}
	if t.RawQuery != "" {
		path += "?" + t.RawQuery
	}
	headers := c.headersFor(targetHost)
	for k, v := range extra {
		headers[k] = v
	}
	lastStatus := 0
	lastBody := []byte{}
	for attempt := 0; attempt <= retries; attempt++ {
		req, err := http.NewRequest("GET", c.Base+path, nil)
		if err != nil {
			msg := err.Error()
			if len(msg) > 200 {
				msg = msg[:200]
			}
			lastStatus, lastBody = 0, []byte(msg)
			if attempt < retries {
				time.Sleep(time.Second)
				continue
			}
			return lastStatus, lastBody, nil
		}
		// Host 头必须走 req.Host（Header.Set("Host") 不生效）。
		req.Host = targetHost
		for k, v := range headers {
			req.Header.Set(k, v)
		}
		resp, err := c.HTTPDoer.Do(req)
		if err != nil {
			msg := err.Error()
			if len(msg) > 200 {
				msg = msg[:200]
			}
			lastStatus, lastBody = 0, []byte(msg)
			if attempt < retries {
				time.Sleep(time.Second)
				continue
			}
			return lastStatus, lastBody, nil
		}
		body, _ := io.ReadAll(resp.Body)
		resp.Body.Close()
		lastStatus, lastBody = resp.StatusCode, body
		if resp.StatusCode == 503 && attempt < retries {
			time.Sleep(time.Second)
			continue
		}
		return lastStatus, lastBody, nil
	}
	return lastStatus, lastBody, nil
}

func fail(format string, args ...interface{}) {
	fmt.Printf("self-test FAIL: "+format+"\n", args...)
	os.Exit(1)
}

// selfTest 断言普通200＋粘滞＋坏Key403＋无头403；目标经网关打 mocks。
func selfTest() {
	gw := "http://127.0.0.1:8916"
	c := NewIPPClient(gw)
	// OPT-R10 C1：Get 现返回 (status, body, err)。err 只在**编程错误**时非 nil
	// （此处即「target 不是 http」）；网络失败走 (0, body, nil)。
	s1, b1, err := c.Get("http://127.0.0.1:8888/", nil, 1)
	if err != nil {
		fail("plain: unexpected programming error: %v", err)
	}
	if s1 != 200 {
		fail("plain expect 200, got %d %.80s", s1, string(b1))
	}
	if !bytes.Contains(b1, []byte("mock-")) {
		fail("body must carry mock marker, got %.80s", string(b1))
	}
	// 编程错误路径：https target 必须返回 err，而不是静默的 status=0。
	if _, _, e := c.Get("https://example.com/", nil, 1); e == nil {
		fail("https target must return a programming error (got nil err)")
	}
	// 粘滞确定性证明：country=US 约束下同 session 两次必中 mock-a-us。
	s := NewIPPClient(gw, WithSession("sdk-selftest-1"), WithCountry("US"))
	_, b2, err := s.Get("http://127.0.0.1:8888/", nil, 1)
	if err != nil {
		fail("sticky: unexpected programming error: %v", err)
	}
	_, b3, _ := s.Get("http://127.0.0.1:8888/", nil, 1)
	if string(b2) != "mock-a-us" || string(b3) != "mock-a-us" {
		fail("sticky must pin mock-a-us, got %q %q", clip(b2), clip(b3))
	}
	bad := NewIPPClient(gw, WithAPIKey("bad"))
	s4, _, err := bad.Get("http://127.0.0.1:8888/", nil, 1)
	if err != nil {
		fail("badkey: unexpected programming error: %v", err)
	}
	if s4 != 403 {
		fail("bad key expect 403, got %d", s4)
	}
	// D3：无头请求同样 403（门默认开启）。
	nokey := NewIPPClient(gw, WithAPIKey(""))
	s5, _, err := nokey.Get("http://127.0.0.1:8888/", nil, 1)
	if err != nil {
		fail("nokey: unexpected programming error: %v", err)
	}
	if s5 != 403 {
		fail("missing key expect 403, got %d", s5)
	}
	fmt.Printf("self-test OK: plain=%d sticky=%q badkey=%d nokey=%d\n", s1, string(b2), s4, s5)
}

func clip(b []byte) string {
	s := string(b)
	s = strings.ReplaceAll(s, "\n", "\\n")
	if len(s) > 16 {
		return s[:16]
	}
	return s
}

func main() {
	for _, a := range os.Args[1:] {
		if a == "--self-test" {
			selfTest()
			return
		}
	}
	fmt.Println("IPProxyPool Go SDK. Run with --self-test only while gateway/mocks are alive.")
}
