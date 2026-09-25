// tools/ipp.pac — HTTP 经前置适配器 127.0.0.1:18080，HTTPS/内网直连；浏览器代理自动配置 URL 示例：file:///D:/_MyProject/SuperSoft/IPProxyPool/tools/ipp.pac
function FindProxyForURL(url, host) {
  if (isPlainHostName(host)) {
    return "DIRECT";
  }
  var ip = dnsResolve(host);
  if (isInNet(ip, "127.0.0.0", "255.0.0.0")) {
    return "DIRECT";
  }
  if (isInNet(ip, "10.0.0.0", "255.0.0.0")) {
    return "DIRECT";
  }
  if (isInNet(ip, "172.16.0.0", "255.240.0.0")) {
    return "DIRECT";
  }
  if (isInNet(ip, "192.168.0.0", "255.255.0.0")) {
    return "DIRECT";
  }
  if (shExpMatch(url, "https:*")) {
    return "DIRECT";
  }
  if (shExpMatch(url, "http:*")) {
    return "PROXY 127.0.0.1:18080";
  }
  return "DIRECT";
}
