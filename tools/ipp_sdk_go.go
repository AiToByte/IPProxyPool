// IPProxyPool Go SDK（OPT-R5 E2，同语义复刻 tools/ipp_sdk.py，stdlib 零依赖 net/http）。
//
// 网关是反向式 egress 路由：请求打到网关地址，真实上游放 Host 头。
// 本 SDK 做最小封装：URL 拆分、粘滞 session、tier/proto 选择、503 延迟重试。
//
// Usage:
//
//	import ipp "path/to/ipp_sdk_go" // 或同一目录直接使用 IPPClient
//
//	c := NewIPPClient("http://127.0.0.1:8916", WithSession("job-42"))
//	status, body := c.Get("http://httpbin.org/ip", nil, 1) // 503 自动延迟重试 1 次
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

// Get 经网关 GET 公网 URL。返回 (status, body)。503 延迟 1s 重试一次。
func (c *IPPClient) Get(target string, extra map[string]string, retries int) (int, []byte) {
	t, err := url.Parse(target)
	if err != nil || t.Scheme != "http" {
		return 0, []byte("only plain http targets are supported (no CONNECT tunneling)")
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
			return lastStatus, lastBody
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
			return lastStatus, lastBody
		}
		body, _ := io.ReadAll(resp.Body)
		resp.Body.Close()
		lastStatus, lastBody = resp.StatusCode, body
		if resp.StatusCode == 503 && attempt < retries {
			time.Sleep(time.Second)
			continue
		}
		return lastStatus, lastBody
	}
	return lastStatus, lastBody
}

func fail(format string, args ...interface{}) {
	fmt.Printf("self-test FAIL: "+format+"\n", args...)
	os.Exit(1)
}

// selfTest 断言普通200＋粘滞＋坏Key403＋无头403；目标经网关打 mocks。
func selfTest() {
	gw := "http://127.0.0.1:8916"
	c := NewIPPClient(gw)
	s1, b1 := c.Get("http://127.0.0.1:8888/", nil, 1)
	if s1 != 200 {
		fail("plain expect 200, got %d %.80s", s1, string(b1))
	}
	if !bytes.Contains(b1, []byte("mock-")) {
		fail("body must carry mock marker, got %.80s", string(b1))
	}
	// 粘滞确定性证明：country=US 约束下同 session 两次必中 mock-a-us。
	s := NewIPPClient(gw, WithSession("sdk-selftest-1"), WithCountry("US"))
	_, b2 := s.Get("http://127.0.0.1:8888/", nil, 1)
	_, b3 := s.Get("http://127.0.0.1:8888/", nil, 1)
	if string(b2) != "mock-a-us" || string(b3) != "mock-a-us" {
		fail("sticky must pin mock-a-us, got %q %q", clip(b2), clip(b3))
	}
	bad := NewIPPClient(gw, WithAPIKey("bad"))
	s4, _ := bad.Get("http://127.0.0.1:8888/", nil, 1)
	if s4 != 403 {
		fail("bad key expect 403, got %d", s4)
	}
	// D3：无头请求同样 403（门默认开启）。
	nokey := NewIPPClient(gw, WithAPIKey(""))
	s5, _ := nokey.Get("http://127.0.0.1:8888/", nil, 1)
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
