// IPProxyPool .NET SDK（OPT-R5 E2，同语义复刻 tools/ipp_sdk.py，HttpClient）。
//
// 网关是反向式 egress 路由：请求打到网关地址，真实上游放 Host 头。
// 本 SDK 做最小封装：URL 拆分、粘滞 session、tier/proto 选择、503 延迟重试。
//
// Usage:
//   var c = new IppSdk.IPPClient("http://127.0.0.1:8916", session: "job-42");
//   var (status, body) = await c.GetAsync("http://httpbin.org/ip"); // 503 自动延迟重试 1 次
//   错误契约（OPT-R10 C1，与 Python/Java/Node/Rust SDK 一致）：
//     * `ArgumentException` ⇒ 编程错误（url 非绝对 URI，或 scheme 非 http）。
//       调用方自己传错了，不该重试。
//     * 不抛、返回 status==0 ⇒ 网络/网关失败，body 里是诊断信息。可预期，
//       重试/退避由调用方决定，不要写成 catch-all 吞掉。
// Self-test（仅网关 http://127.0.0.1:8916 ＋ mocks http://127.0.0.1:8888 活着时手动跑，默认不自动跑）：
//   dotnet run --project tools/sdk-dotnet -- --self-test   # 普通200＋粘滞＋坏Key403＋无头403
//   注意（OPT-R9 A3 实测修正）：`dotnet run` **必须带 --project**，否则在
//   仓库根执行会因「当前目录无 .csproj」而直接报错——历史文档写的是
//   `dotnet run -- --self-test`，那条命令在无 .csproj 的目录下跑不起来。
//   `tools/sdk-dotnet/` 是**专为 CI 编译验证而存在的最小工程壳**（它
//   `<Compile Include>` 引用本文件、不复制内容）；你写自己的应用时把本文件
//   加进你的 .csproj 即可，无需用这个壳。
// D3 注记：网关默认开 API Key 门；SDK 缺省带开发 Key `default_key`
// （生产传真 Key；apiKey: null 即无头，用于验证 403）。
// 注意：Host 头必须用 request.Headers.Host 设置；HttpClient 默认保护 Host 头，Headers.Add("Host") 会抛。
using System;
using System.Collections.Generic;
using System.Net.Http;
using System.Threading.Tasks;

namespace IppSdk
{
    public sealed class IPPClient : IDisposable
    {
        private readonly string _base;
        private readonly HttpClient _http;

        public string ApiKey { get; set; }
        public string Session { get; set; }
        public string Country { get; set; }
        public string Tier { get; set; }
        public string Proto { get; set; }

        public IPPClient(string gateway = "http://127.0.0.1:8916",
            string apiKey = "default_key",
            string session = null,
            string country = null,
            string tier = null,
            string proto = null,
            int timeoutSeconds = 10)
        {
            Uri u;
            try { u = new Uri(gateway); }
            catch { u = new Uri("http://127.0.0.1:8916"); }
            string host = string.IsNullOrEmpty(u.Host) ? "127.0.0.1" : u.Host;
            int port = u.IsDefaultPort ? 80 : u.Port;
            _base = "http://" + host + ":" + port;
            ApiKey = apiKey;
            Session = session;
            Country = country;
            Tier = tier;
            Proto = proto;
            _http = new HttpClient();
            _http.Timeout = TimeSpan.FromSeconds(timeoutSeconds);
        }

        // 头名与网关 parse_routing_spec 严格同名；误名头会被网关静默忽略。
        private Dictionary<string, string> HeadersFor()
        {
            var h = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
            if (!string.IsNullOrEmpty(ApiKey)) h["X-Api-Key"] = ApiKey;
            if (!string.IsNullOrEmpty(Session)) h["X-Proxy-Session"] = Session;
            if (!string.IsNullOrEmpty(Country)) h["X-Proxy-Country"] = Country;
            if (!string.IsNullOrEmpty(Tier)) h["X-Proxy-Tier"] = Tier;
            if (!string.IsNullOrEmpty(Proto)) h["X-Proxy-Proto"] = Proto;
            return h;
        }

        /// <summary>
        /// 经网关 GET 公网 URL。返回 (status, body)。503 延迟 1s 重试一次。
        /// <para>
        /// <b>错误契约（OPT-R10 C1：与其余 4 语言 SDK 统一）</b>：两类失败分两个通道——
        /// <list type="number">
        /// <item><b>编程错误</b>（调用方把参数用错了）⇒ 抛 <see cref="ArgumentException"/>，
        /// 状态值不可用。当前两种：url 不是绝对 URI；scheme 不是 http（网关不做
        /// CONNECT 隧道，见 docs/SPIKE_R2.md 的 E7 结论）。</item>
        /// <item><b>网络/网关失败</b>（连接失败、超时、503 重试耗尽）⇒ <b>不抛</b>，
        /// 返回 status=0 且 body 为诊断信息。这些是运行期可预期的瞬时故障，
        /// 抛异常只会诱导调用方写 catch 吞掉它们，反而丢失可观测性。</item>
        /// </list>
        /// </summary>
        /// <remarks>
        /// 为何原实现要改：原代码对<b>同一个函数内的同一类错误</b>用了两个通道——
        /// url 解析失败走 <c>return (0, msg)</c>、scheme 非 http 走
        /// <c>throw ArgumentException</c>。调用方无法从返回值区分「我传错了」
        /// 与「网关挂了」，只能一律 try/catch 兜住，于是真正的参数错误被静默。
        /// 此外原 <c>catch (Exception)</c> 会把 SDK 自身的编程错误（如误用
        /// <see cref="System.Net.Http.HttpClient"/> API 抛的
        /// <see cref="InvalidOperationException"/>）也吞成 (0, message)——
        /// 真 bug 被伪装成网络故障。现只捕获网络类异常。
        /// </remarks>
        public async Task<(int status, byte[] body)> GetAsync(
            string url, IDictionary<string, string> extraHeaders = null, int retries = 1)
        {
            // ── 编程错误通道：不做重试、不返回假状态，直接抛 ──
            Uri t;
            if (!Uri.TryCreate(url, UriKind.Absolute, out t))
                throw new ArgumentException($"url is not an absolute URI: {url}", nameof(url));
            if (t.Scheme != "http")
                throw new ArgumentException(
                    "only plain http targets are supported (no CONNECT tunneling), got scheme: " + t.Scheme,
                    nameof(url));

            string targetHost = t.Authority;
            string path = string.IsNullOrEmpty(t.PathAndQuery) ? "/" : t.PathAndQuery;
            var headers = HeadersFor();
            if (extraHeaders != null)
                foreach (var kv in extraHeaders) headers[kv.Key] = kv.Value;
            int lastStatus = 0;
            byte[] lastBody = Array.Empty<byte>();
            for (int attempt = 0; attempt <= retries; attempt++)
            {
                try
                {
                    using (var request = new HttpRequestMessage(HttpMethod.Get, _base + path))
                    {
                        // Host 头必须走 Headers.Host，Add("Host") 会被保护拒绝。
                        request.Headers.Host = targetHost;
                        request.Headers.TryAddWithoutValidation("User-Agent", "ipp-sdk/1.0");
                        foreach (var kv in headers)
                            request.Headers.TryAddWithoutValidation(kv.Key, kv.Value);
                        using (var resp = await _http.SendAsync(request).ConfigureAwait(false))
                        {
                            byte[] body = await resp.Content.ReadAsByteArrayAsync().ConfigureAwait(false);
                            lastStatus = (int)resp.StatusCode;
                            lastBody = body;
                            if (lastStatus == 503 && attempt < retries)
                            {
                                await Task.Delay(1000).ConfigureAwait(false);
                                continue;
                            }
                            return (lastStatus, lastBody);
                        }
                    }
                }
                // ── 网络失败通道：只捕获「网络/取消」类异常 ──
                // HttpRequestException：DNS/连接/重置等。
                // TaskCanceledException（继承 OperationCanceledException）：超时/取消。
                // 其它异常类型（InvalidOperationException、ArgumentException…）**故意不捕获**——
                // 它们是 SDK 或调用方的编程错误，应冒泡到调用方而不是伪装成网络故障。
                catch (Exception e) when (e is HttpRequestException || e is OperationCanceledException)
                {
                    lastStatus = 0;
                    lastBody = Cut(e.Message);
                    if (attempt < retries)
                    {
                        await Task.Delay(1000).ConfigureAwait(false);
                        continue;
                    }
                    return (lastStatus, lastBody);
                }
            }
            return (lastStatus, lastBody);
        }

        private static byte[] Cut(string s)
        {
            if (s == null) s = "";
            if (s.Length > 200) s = s.Substring(0, 200);
            return System.Text.Encoding.UTF8.GetBytes(s);
        }

        public void Dispose() { _http.Dispose(); }
    }

    public static class SelfTest
    {
        private static void Check(bool ok, string msg)
        {
            if (!ok) throw new Exception("self-test FAIL: " + msg);
        }
        private static bool Contains(byte[] body, string needle)
        {
            string s = System.Text.Encoding.UTF8.GetString(body);
            return s.Contains(needle);
        }

        // 断言普通200＋粘滞＋坏Key403＋无头403；目标经网关打 mocks。
        public static async Task RunAsync()
        {
            string gw = "http://127.0.0.1:8916";
            using (var c = new IPPClient(gw))
            {
                var (s1, b1) = await c.GetAsync("http://127.0.0.1:8888/").ConfigureAwait(false);
                Check(s1 == 200, "plain expect 200, got " + s1);
                Check(Contains(b1, "mock-"), "body must carry mock marker");

                // OPT-R10 C1：编程错误通道断言——https 与非绝对 URI 必须抛
                // ArgumentException，而不是返回 (0, msg) 让调用方误判成网络故障。
                try
                {
                    await c.GetAsync("https://example.com/").ConfigureAwait(false);
                    throw new Exception("self-test FAIL: https target must throw ArgumentException");
                }
                catch (ArgumentException) { /* 预期 */ }
                try
                {
                    await c.GetAsync("not-a-uri").ConfigureAwait(false);
                    throw new Exception("self-test FAIL: non-absolute uri must throw ArgumentException");
                }
                catch (ArgumentException) { /* 预期 */ }
                using (var s = new IPPClient(gw, session: "sdk-selftest-1", country: "US"))
                {
                    var rb2 = await s.GetAsync("http://127.0.0.1:8888/").ConfigureAwait(false);
                    var rb3 = await s.GetAsync("http://127.0.0.1:8888/").ConfigureAwait(false);
                    string v2 = System.Text.Encoding.UTF8.GetString(rb2.body);
                    string v3 = System.Text.Encoding.UTF8.GetString(rb3.body);
                    Check(v2 == "mock-a-us" && v3 == "mock-a-us",
                        "sticky must pin mock-a-us, got " + v2 + " " + v3);
                }
                using (var bad = new IPPClient(gw, apiKey: "bad"))
                {
                    var (s4, b4) = await bad.GetAsync("http://127.0.0.1:8888/").ConfigureAwait(false);
                    _ = b4;
                    Check(s4 == 403, "bad key expect 403, got " + s4);
                }
                // D3：无头请求同样 403（门默认开启）。
                using (var nokey = new IPPClient(gw, apiKey: null))
                {
                    var (s5, b5) = await nokey.GetAsync("http://127.0.0.1:8888/").ConfigureAwait(false);
                    _ = b5;
                    Check(s5 == 403, "missing key expect 403, got " + s5);
                }
                Console.WriteLine("self-test OK: plain=" + s1 + " sticky=mock-a-us badkey=403 nokey=403");
            }
        }
    }

    public static class Program
    {
        public static async Task<int> Main(string[] args)
        {
            bool want = false;
            foreach (var a in args) if (a == "--self-test") want = true;
            if (!want)
            {
                Console.WriteLine("IPProxyPool .NET SDK. Run with --self-test only while gateway/mocks are alive.");
                return 0;
            }
            try
            {
                await SelfTest.RunAsync().ConfigureAwait(false);
                return 0;
            }
            catch (Exception e)
            {
                Console.Error.WriteLine(e.Message);
                return 1;
            }
        }
    }
}
