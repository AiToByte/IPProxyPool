// bench_server.go — 压测用的上游 + 客户端一体化工具。
//
// 为什么自己写客户端（而不用 curl 多进程）
//
// 实测记录（本轮踩坑，全部为真）：
//  1. Python 线程池客户端：96 线程抢一个 GIL，p50 从 1.42ms 涨到 20.41ms，
//     QPS 只从 1222 到 3622 —— 客户端先饱和，把客户端天花板当成了上游天花板。
//  2. curl --parallel：Windows 上未产生真实并发（time_total 全在 0.000x），
//     且 `--config` 多 URL 时 -o 只对第一个请求生效，body 混进 stdout，
//     3000 个请求只解析出 13 条 —— 差点拿这个假数字下结论。
//  3. curl 多进程（每进程单 URL）：32 进程时 QPS 走平在 380，
//     96 进程时 QPS 反降到 130、成功率 37% —— 进程创建成本压过吞吐。
//
// 结论：在 Windows 上用「外部进程」当压测客户端本身就构成瓶颈，
// 且数据不可信。正确做法是让客户端与被测组件处于同一语言生态
// （Go），用 goroutine 做真并发、单进程常驻、零进程创建开销。
//
// 本程序两种模式：
//
//	bench_server.go serve   -addr :19401 -procs 8   起 N 个上游实例
//	bench_server.go client  -target http://127.0.0.1:8916/ -c 64 -n 20000
package main

import (
	"flag"
	"fmt"
	"io"
	"math"
	"net"
	"net/http"
	"net/url"
	"os"
	"runtime"
	"sort"
	"sync"
	"sync/atomic"
	"time"
)

// ---------------------------------------------------------------- serve 模式

var hits int64

func serveHandler(body string) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		atomic.AddInt64(&hits, 1)
		w.Header().Set("Content-Type", "text/plain")
		w.Header().Set("Content-Length", fmt.Sprint(len(body)))
		_, _ = io.WriteString(w, body)
	})
}

func cmdServe(args []string) int {
	fs := flag.NewFlagSet("serve", flag.ExitOnError)
	addr := fs.String("addr", "127.0.0.1:19401", "listen address")
	body := fs.String("body", "ok", "response body")
	_ = fs.Parse(args)

	mux := http.NewServeMux()
	mux.Handle("/", serveHandler(*body))
	srv := &http.Server{Addr: *addr, Handler: mux}
	fmt.Fprintf(os.Stderr, "upstream listening %s GOMAXPROCS=%d\n", *addr, runtime.GOMAXPROCS(0))
	if err := srv.ListenAndServe(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		return 1
	}
	return 0
}

// --------------------------------------------------------------- client 模式

// 分位数：先排序再取下标，避免 reservoir 抽样的方差。
func pct(sorted []time.Duration, p float64) time.Duration {
	if len(sorted) == 0 {
		return 0
	}
	i := int(float64(len(sorted)) * p)
	if i >= len(sorted) {
		i = len(sorted) - 1
	}
	return sorted[i]
}

func cmdClient(args []string) int {
	fs := flag.NewFlagSet("client", flag.ExitOnError)
	target := fs.String("target", "http://127.0.0.1:8916/", "target URL (via proxy)")
	conc := fs.Int("c", 64, "concurrency")
	total := fs.Int("n", 20000, "total requests")
	timeout := fs.Duration("timeout", 10*time.Second, "per-request timeout")
	label := fs.String("label", "", "label for output")
	apiKey := fs.String("api-key", "", "value for X-API-Key header")
	country := fs.String("country", "", "value for X-Proxy-Country header")
	// mode:
	//   "host"      — connect to the gateway, send origin-form path, and set the
	//                 Host header to `host-header` (default: the target URL's host).
	//                 IPProxyPool builds the upstream URL from that Host header
	//                 ("origin-form -> Host 头拼 http://", see gateway.rs), so
	//                 pointing Host at one of the injected upstream ports makes
	//                 the request land on that node.
	//   "absolute"  — absolute-form in the request line.
	//
	// 【踩坑记录】第一版只有 absolute-form，实测 40/40 全失败。原因是
	// Go 的 http.Client 在 URL 只有 path 时会直连该 path 对应的默认主机，
	// 请求根本没到网关。要走网关必须显式给出对端地址，故拆成两个字段：
	// `-target`（连谁=网关）与 `-host-header`（Host 头=上游）。
	mode := fs.String("mode", "host", `"host" or "absolute"`)
	hostHeader := fs.String("host-header", "", "Host header value (default: target URL host)")
	_ = fs.Parse(args)

	// 关键：每个 worker 一个独立 Transport，绝不共享 http.Client。
	// 共享 Client 会让 Transport 的连接池串行化，并把排队延迟计入
	// 延迟统计 —— 那样测出来的 p99 是客户端排队，不是被测组件。
	var latMu sync.Mutex
	var all []time.Duration
	var okCount, errCount int64

	newTransport := func() *http.Transport {
		return &http.Transport{
			MaxIdleConns:        4096,
			MaxIdleConnsPerHost: 4096,
			MaxConnsPerHost:     0,
			IdleConnTimeout:     90 * time.Second,
			DisableCompression:  true,
			DialContext: (&net.Dialer{
				Timeout:   5 * time.Second,
				KeepAlive: 30 * time.Second,
			}).DialContext,
		}
	}

	// connectURL 决定实际连到哪个地址（网关），path 决定请求行内容。
	connectURL := *target
	sendPath := "/"
	hh := *hostHeader
	if *mode == "host" {
		if u, err := url.Parse(*target); err == nil {
			// 连接地址 = target（网关），请求行 = path+query，Host 头单独给。
			sendPath = u.RequestURI()
			if hh == "" {
				hh = u.Host
			}
		}
	}

	per := *total / *conc
	if per < 1 {
		per = 1
	}
	actualTotal := per * *conc

	var wg sync.WaitGroup
	start := time.Now()
	for w := 0; w < *conc; w++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			cl := &http.Client{Transport: newTransport(), Timeout: *timeout}
			local := make([]time.Duration, 0, per)
			lok, lerr := int64(0), int64(0)
			for i := 0; i < per; i++ {
				// 始终用完整 URL 构造请求（否则 http.NewRequest 拒绝裸 path），
				// 再用 URL.Opaque 强制请求行只发 path 部分。
				r, err := http.NewRequest(http.MethodGet, connectURL, nil)
				if err != nil {
					lerr++
					continue
				}
				if *mode == "host" {
					r.URL.Opaque = sendPath
					if hh != "" {
						r.Host = hh
					}
				} else {
					r.URL.Opaque = connectURL
				}
				if *apiKey != "" {
					r.Header.Set("X-API-Key", *apiKey)
				}
				if *country != "" {
					r.Header.Set("X-Proxy-Country", *country)
				}
				t0 := time.Now()
				resp, err := cl.Do(r)
				d := time.Since(t0)
				if err != nil {
					lerr++
					local = append(local, d)
					continue
				}
				_, _ = io.Copy(io.Discard, resp.Body)
				_ = resp.Body.Close()
				local = append(local, d)
				if resp.StatusCode >= 200 && resp.StatusCode < 400 {
					lok++
				} else {
					lerr++
				}
			}
			latMu.Lock()
			all = append(all, local...)
			okCount += lok
			errCount += lerr
			latMu.Unlock()
		}()
	}
	wg.Wait()
	wall := time.Since(start)

	sort.Slice(all, func(i, j int) bool { return all[i] < all[j] })
	qps := float64(okCount) / wall.Seconds()

	fmt.Printf("=== bench: %s ===\n", *label)
	fmt.Printf("  target     = %s  path=%s  host=%s  mode=%s\n", connectURL, sendPath, hh, *mode)
	fmt.Printf("  concurrency= %d   requests = %d\n", *conc, actualTotal)
	fmt.Printf("  success    = %d   failure = %d\n", okCount, errCount)
	fmt.Printf("  wall       = %.2fs\n", wall.Seconds())
	fmt.Printf("  QPS        = %.1f\n", qps)
	fmt.Printf("  latency    = p50 %v | p90 %v | p99 %v | p999 %v | max %v\n",
		pct(all, .50), pct(all, .90), pct(all, .99), pct(all, .999),
		func() time.Duration {
			if len(all) == 0 {
				return 0
			}
			return all[len(all)-1]
		}())
	if len(all) > 0 {
		sum := time.Duration(0)
		for _, d := range all {
			sum += d
		}
		fmt.Printf("  mean       = %v\n", sum/time.Duration(len(all)))
		var ss float64
		m := float64(sum) / float64(len(all))
		for _, d := range all {
			dd := float64(d) - m
			ss += dd * dd
		}
		fmt.Printf("  stdev      = %.2fms\n", math.Sqrt(ss/float64(len(all)))/1e6)
	}
	fmt.Println()
	return 0
}

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "usage: bench_server serve|client [flags]")
		os.Exit(2)
	}
	switch os.Args[1] {
	case "serve":
		os.Exit(cmdServe(os.Args[2:]))
	case "client":
		os.Exit(cmdClient(os.Args[2:]))
	default:
		fmt.Fprintf(os.Stderr, "unknown mode %q\n", os.Args[1])
		os.Exit(2)
	}
}
