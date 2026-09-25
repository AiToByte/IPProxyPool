// IPProxyPool Java SDK（OPT-R5 E2，同语义复刻 tools/ipp_sdk.py，java.net.http.HttpClient，Java 11+）。
//
// 网关是反向式 egress 路由：请求打到网关地址，真实上游放 Host 头。
// 本 SDK 做最小封装：URL 拆分、粘滞 session、tier/proto 选择、503 延迟重试。
//
// Usage:
//   ipp_sdk_java.IPPClient c = new ipp_sdk_java.IPPClient("http://127.0.0.1:8916");
//   c.session = "job-42";
//   int[] out = new int[1];
//   byte[] body = c.get("http://httpbin.org/ip", out); // 503 自动延迟重试 1 次，out[0] 为 status
// Self-test（仅网关 http://127.0.0.1:8916 ＋ mocks http://127.0.0.1:8888 活着时手动跑，默认不自动跑）：
//   javac tools/ipp_sdk_java.java && java -cp tools ipp_sdk_java --self-test   # 普通200＋粘滞＋坏Key403＋无头403
// D3 注记：网关默认开 API Key 门；SDK 缺省带开发 Key `default_key`
// （生产传真 Key；apiKey=null 即无头，用于验证 403）。
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.util.HashMap;
import java.util.Map;

public class ipp_sdk_java {

    public static class IPPClient {
        public final String base;
        public String apiKey = "default_key";
        public String session = null;
        public String country = null;
        public String tier = null;
        public String proto = null;
        public final HttpClient http;

        public IPPClient(String gateway) {
            if (gateway == null || gateway.isEmpty()) {
                gateway = "http://127.0.0.1:8916";
            }
            URI u;
            try {
                u = URI.create(gateway);
            } catch (Exception e) {
                u = URI.create("http://127.0.0.1:8916");
            }
            String host = u.getHost();
            if (host == null || host.isEmpty()) {
                host = "127.0.0.1";
            }
            int port = u.getPort() != -1 ? u.getPort() : 80;
            this.base = "http://" + host + ":" + port;
            this.http = HttpClient.newBuilder()
                    .connectTimeout(Duration.ofSeconds(10))
                    .build();
        }

        // 头名与网关 parse_routing_spec 严格同名；误名头会被网关静默忽略。
        private Map<String, String> headersFor() {
            Map<String, String> h = new HashMap<>();
            if (apiKey != null && !apiKey.isEmpty()) {
                h.put("X-Api-Key", apiKey);
            }
            if (session != null && !session.isEmpty()) {
                h.put("X-Proxy-Session", session);
            }
            if (country != null && !country.isEmpty()) {
                h.put("X-Proxy-Country", country);
            }
            if (tier != null && !tier.isEmpty()) {
                h.put("X-Proxy-Tier", tier);
            }
            if (proto != null && !proto.isEmpty()) {
                h.put("X-Proxy-Proto", proto);
            }
            return h;
        }

        private static byte[] cut(String s) {
            if (s == null) {
                s = "";
            }
            if (s.length() > 200) {
                s = s.substring(0, 200);
            }
            return s.getBytes(StandardCharsets.UTF_8);
        }

        // 经网关 GET 公网 URL。返回 body，status 写入 out[0]。503 延迟 1s 重试一次。
        public byte[] get(String url, int[] out) throws Exception {
            return get(url, null, 1, out);
        }

        public byte[] get(String url, Map<String, String> extra, int retries, int[] out) throws Exception {
            URI t = URI.create(url);
            if (!"http".equals(t.getScheme())) {
                throw new IllegalArgumentException("only plain http targets are supported (no CONNECT tunneling)");
            }
            String targetHost = t.getAuthority();
            String path = t.getRawPath();
            if (path == null || path.isEmpty()) {
                path = "/";
            }
            if (t.getRawQuery() != null && !t.getRawQuery().isEmpty()) {
                path += "?" + t.getRawQuery();
            }
            Map<String, String> headers = headersFor();
            if (extra != null) {
                headers.putAll(extra);
            }
            int lastStatus = 0;
            byte[] lastBody = new byte[0];
            for (int attempt = 0; attempt <= retries; attempt++) {
                try {
                    HttpRequest.Builder b = HttpRequest.newBuilder(URI.create(this.base + path))
                            .header("Host", targetHost)
                            .header("User-Agent", "ipp-sdk/1.0")
                            .GET()
                            .timeout(Duration.ofSeconds(10));
                    for (Map.Entry<String, String> e : headers.entrySet()) {
                        b.header(e.getKey(), e.getValue());
                    }
                    HttpRequest req = b.build();
                    HttpResponse<byte[]> resp = http.send(req, HttpResponse.BodyHandlers.ofByteArray());
                    lastStatus = resp.statusCode();
                    lastBody = resp.body() != null ? resp.body() : new byte[0];
                    if (lastStatus == 503 && attempt < retries) {
                        Thread.sleep(1000);
                        continue;
                    }
                    out[0] = lastStatus;
                    return lastBody;
                } catch (InterruptedException ie) {
                    Thread.currentThread().interrupt();
                    lastStatus = 0;
                    lastBody = cut(ie.getMessage());
                    if (attempt < retries) {
                        Thread.sleep(1000);
                        continue;
                    }
                    out[0] = lastStatus;
                    return lastBody;
                } catch (Exception e) {
                    lastStatus = 0;
                    lastBody = cut(e.getMessage());
                    if (attempt < retries) {
                        try {
                            Thread.sleep(1000);
                        } catch (InterruptedException ie2) {
                            Thread.currentThread().interrupt();
                        }
                        continue;
                    }
                    out[0] = lastStatus;
                    return lastBody;
                }
            }
            out[0] = lastStatus;
            return lastBody;
        }
    }

    private static void check(boolean ok, String msg) throws Exception {
        if (!ok) {
            throw new Exception("self-test FAIL: " + msg);
        }
    }

    // 自检断言普通200＋粘滞＋坏Key403＋无头403；目标经网关打 mocks。
    public static void selfTest() throws Exception {
        String gw = "http://127.0.0.1:8916";
        IPPClient c = new IPPClient(gw);
        int[] s1 = new int[1];
        byte[] b1 = c.get("http://127.0.0.1:8888/", s1);
        check(s1[0] == 200, "plain expect 200, got " + s1[0]);
        check(new String(b1, StandardCharsets.UTF_8).contains("mock-"), "body must carry mock marker");
        // 粘滞确定性证明：country=US 约束下同 session 两次必中 mock-a-us。
        IPPClient s = new IPPClient(gw);
        s.session = "sdk-selftest-1";
        s.country = "US";
        int[] t2 = new int[1];
        int[] t3 = new int[1];
        byte[] b2 = s.get("http://127.0.0.1:8888/", t2);
        byte[] b3 = s.get("http://127.0.0.1:8888/", t3);
        String v2 = new String(b2, StandardCharsets.UTF_8);
        String v3 = new String(b3, StandardCharsets.UTF_8);
        check("mock-a-us".equals(v2) && "mock-a-us".equals(v3),
                "sticky must pin mock-a-us, got " + v2 + " " + v3);
        IPPClient bad = new IPPClient(gw);
        bad.apiKey = "bad";
        int[] s4 = new int[1];
        bad.get("http://127.0.0.1:8888/", s4);
        check(s4[0] == 403, "bad key expect 403, got " + s4[0]);
        // D3：无头请求同样 403（门默认开启）。
        IPPClient nokey = new IPPClient(gw);
        nokey.apiKey = null;
        int[] s5 = new int[1];
        nokey.get("http://127.0.0.1:8888/", s5);
        check(s5[0] == 403, "missing key expect 403, got " + s5[0]);
        System.out.println("self-test OK: plain=" + s1[0] + " sticky=" + v2 + " badkey=" + s4[0] + " nokey=" + s5[0]);
    }

    public static void main(String[] args) throws Exception {
        boolean want = false;
        for (String a : args) {
            if ("--self-test".equals(a)) {
                want = true;
            }
        }
        if (!want) {
            System.out.println("IPProxyPool Java SDK. Run with --self-test only while gateway/mocks are alive.");
            return;
        }
        selfTest();
    }
}
