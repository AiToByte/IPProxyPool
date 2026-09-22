//! P2 SOCKS egress 握手：RFC1928（SOCKS5 greeting＋CONNECT）＋RFC1929
//! （username/password 子协商）＋SOCKS4 CONNECT（含 4a 域名形态）。
//!
//! 纯 tokio 手写，零新依赖。超时由调用方包 `tokio::time::timeout`（Verifier／
//! prewarmer／bridge 各有各的超时口径，本模块只做协议，不管时间）。

use crate::model::EgressProto;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 经 SOCKS4/5 代理向 `target_host:target_port` 建 CONNECT 出站流。
/// Socks5 打法：greeting 只 offer `[NO_AUTH]`；服务端回 `0x02` 且调用方给了账密则
/// 做 RFC1929 子协商；回 `0xFF`／其他方法／版本≠`0x05` → Err。CONNECT 按 target
/// 形态选 ATYP（IPv4／DOMAIN；IPv6 显式 Err，见注释）。
/// Socks4 打法：`0x04` CONNECT（userid 取 username、无则空串）；域名目标用 4a 形态
/// （IP 填 `0.0.0.1`＋尾部跟域名）；回包第 2 字节须 `0x5A`。
/// 超时由调用方包 `tokio::time::timeout`（各调用方口径不同，本模块不管时间）。
/// 错误文案 String（沿 free_pool 惯例，零新依赖）。
pub async fn establish(
    proxy_ip: &str,
    proxy_port: u16,
    username: Option<&str>,
    password: Option<&str>,
    proto: EgressProto,
    target_host: &str,
    target_port: u16,
) -> Result<tokio::net::TcpStream, String> {
    let mut s = tokio::net::TcpStream::connect((proxy_ip, proxy_port))
        .await
        .map_err(|e| format!("socks tcp connect {proxy_ip}:{proxy_port} failed: {e}"))?;
    match proto {
        EgressProto::Socks5 => {
            establish_v5(&mut s, username, password, target_host, target_port).await?
        }
        EgressProto::Socks4 => establish_v4(&mut s, username, target_host, target_port).await?,
        EgressProto::Http => return Err("establish called with Http proto (not socks)".to_string()),
    }
    Ok(s)
}

/// Socks5 子握手（被 `establish` 调用）。
async fn establish_v5(
    s: &mut tokio::net::TcpStream,
    username: Option<&str>,
    password: Option<&str>,
    target_host: &str,
    target_port: u16,
) -> Result<(), String> {
    // 1. greeting：只 offer NO_AUTH（有账密也不预 offer 0x02——服务端要求才升级，
    //    少一次往返且兼容只认 NO_AUTH 的免费节点）。
    s.write_all(&[0x05, 0x01, 0x00])
        .await
        .map_err(|e| format!("socks5 greet write failed: {e}"))?;
    let mut choice = [0u8; 2];
    s.read_exact(&mut choice)
        .await
        .map_err(|e| format!("socks5 greet read failed: {e}"))?;
    if choice[0] != 0x05 {
        return Err(format!("socks5 bad version {:#04x}", choice[0]));
    }
    match choice[1] {
        0x00 => {}
        0x02 => {
            let (u, p) = match (username, password) {
                (Some(u), Some(p)) => (u, p),
                _ => return Err("socks5 server requires auth but no credentials".to_string()),
            };
            let ub = u.as_bytes();
            let pb = p.as_bytes();
            if ub.len() > 255 || pb.len() > 255 {
                return Err("socks5 credentials too long".to_string());
            }
            let mut req = Vec::with_capacity(3 + ub.len() + pb.len());
            req.push(0x01);
            req.push(ub.len() as u8);
            req.extend_from_slice(ub);
            req.push(pb.len() as u8);
            req.extend_from_slice(pb);
            s.write_all(&req)
                .await
                .map_err(|e| format!("socks5 auth write failed: {e}"))?;
            let mut rep = [0u8; 2];
            s.read_exact(&mut rep)
                .await
                .map_err(|e| format!("socks5 auth read failed: {e}"))?;
            if rep[0] != 0x01 || rep[1] != 0x00 {
                return Err("socks5 auth rejected".to_string());
            }
        }
        0xFF => return Err("socks5 no acceptable method".to_string()),
        m => return Err(format!("socks5 unsupported method {m:#04x}")),
    }
    // 2. CONNECT（IPv4 优先嗅探；域名走 ATYP=0x03；IPv6 显式不支持——
    //    本仓节点与验证目标皆 v4/hostname，v6 进来即 Err 而非静默错连）。
    let mut req = vec![0x05, 0x01, 0x00];
    if let Ok(v4) = target_host.parse::<std::net::Ipv4Addr>() {
        req.push(0x01);
        req.extend_from_slice(&v4.octets());
    } else if target_host.parse::<std::net::Ipv6Addr>().is_ok() {
        return Err("socks5 ipv6 target not supported".to_string());
    } else {
        let hb = target_host.as_bytes();
        if hb.is_empty() || hb.len() > 255 {
            return Err("socks5 bad domain target".to_string());
        }
        req.push(0x03);
        req.push(hb.len() as u8);
        req.extend_from_slice(hb);
    }
    req.extend_from_slice(&target_port.to_be_bytes());
    s.write_all(&req)
        .await
        .map_err(|e| format!("socks5 connect write failed: {e}"))?;
    // 3. 回复：VER REP RSV ATYP＋按 ATYP 消费 BND（内容丢弃，只验 REP）。
    let mut head = [0u8; 4];
    s.read_exact(&mut head)
        .await
        .map_err(|e| format!("socks5 connect read failed: {e}"))?;
    if head[0] != 0x05 {
        return Err(format!("socks5 bad connect version {:#04x}", head[0]));
    }
    if head[1] != 0x00 {
        return Err(format!("socks5 connect rejected rep={:#04x}", head[1]));
    }
    let bnd_len = match head[3] {
        0x01 => 4 + 2,
        0x03 => {
            let mut l = [0u8; 1];
            s.read_exact(&mut l)
                .await
                .map_err(|e| format!("socks5 bnd read failed: {e}"))?;
            l[0] as usize + 2
        }
        0x04 => 16 + 2,
        a => return Err(format!("socks5 bad atyp {a:#04x}")),
    };
    let mut bnd = vec![0u8; bnd_len];
    s.read_exact(&mut bnd)
        .await
        .map_err(|e| format!("socks5 bnd read failed: {e}"))?;
    Ok(())
}

/// Socks4 子握手（被 `establish` 调用）。
async fn establish_v4(
    s: &mut tokio::net::TcpStream,
    username: Option<&str>,
    target_host: &str,
    target_port: u16,
) -> Result<(), String> {
    // SOCKS4 无方法协商，直接 CONNECT。userid 取 username（无则空串，只占结尾 0x00）。
    let mut req = vec![0x04, 0x01];
    req.extend_from_slice(&target_port.to_be_bytes());
    let mut domain_suffix: Option<&[u8]> = None;
    if let Ok(v4) = target_host.parse::<std::net::Ipv4Addr>() {
        req.extend_from_slice(&v4.octets());
    } else {
        if target_host.parse::<std::net::Ipv6Addr>().is_ok() {
            return Err("socks4 ipv6 target not supported".to_string());
        }
        let hb = target_host.as_bytes();
        if hb.is_empty() || hb.len() > 255 {
            return Err("socks4 bad domain target".to_string());
        }
        req.extend_from_slice(&[0, 0, 0, 1]); // 4a 标记
        domain_suffix = Some(hb);
    }
    req.extend_from_slice(username.unwrap_or("").as_bytes());
    req.push(0x00);
    if let Some(d) = domain_suffix {
        req.extend_from_slice(d);
        req.push(0x00);
    }
    s.write_all(&req)
        .await
        .map_err(|e| format!("socks4 connect write failed: {e}"))?;
    let mut rep = [0u8; 8];
    s.read_exact(&mut rep)
        .await
        .map_err(|e| format!("socks4 connect read failed: {e}"))?;
    if rep[0] != 0x00 || rep[1] != 0x5A {
        return Err(format!("socks4 connect rejected cd={:#04x}", rep[1]));
    }
    Ok(())
}

/// greeting-only 存活探测（prewarmer 用：证明端口说 SOCKS，不对外 CONNECT）。
/// Socks5 打法：发 greeting，服务端回版本 `0x05` 且方法≠`0xFF` 即 Ok；
/// Socks4 无 greeting 语义，TCP 建链成功即 Ok（建链失败由 connect Err 覆盖）。
pub async fn greet_only(proxy_ip: &str, proxy_port: u16, proto: EgressProto) -> Result<(), String> {
    let mut s = tokio::net::TcpStream::connect((proxy_ip, proxy_port))
        .await
        .map_err(|e| format!("socks tcp connect {proxy_ip}:{proxy_port} failed: {e}"))?;
    match proto {
        EgressProto::Http => Err("greet_only called with Http proto".to_string()),
        EgressProto::Socks4 => Ok(()),
        EgressProto::Socks5 => {
            s.write_all(&[0x05, 0x01, 0x00])
                .await
                .map_err(|e| format!("socks5 greet write failed: {e}"))?;
            let mut choice = [0u8; 2];
            s.read_exact(&mut choice)
                .await
                .map_err(|e| format!("socks5 greet read failed: {e}"))?;
            if choice[0] != 0x05 || choice[1] == 0xFF {
                return Err("socks5 greet rejected".to_string());
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// stub 模式：NoAuth 成功／要求账密／CONNECT 拒绝／坏版本。
    enum StubMode {
        NoAuthOk,
        RequireAuth { user: String, pass: String },
        ConnectRefused,
        BadVersion,
    }

    /// 起本地 SOCKS5 stub，返回（端口，收到的原始字节记录）。
    async fn spawn_socks5_stub(
        mode: StubMode,
    ) -> (u16, std::sync::Arc<parking_lot::Mutex<Vec<u8>>>) {
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let (mut s, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            // 1. greeting：VER NMETHODS METHODS…
            let mut head = [0u8; 2];
            if s.read_exact(&mut head).await.is_err() {
                return;
            }
            let n = head[1] as usize;
            let mut methods = vec![0u8; n];
            if s.read_exact(&mut methods).await.is_err() {
                return;
            }
            seen2.lock().extend_from_slice(&head);
            seen2.lock().extend_from_slice(&methods);
            if matches!(mode, StubMode::BadVersion) {
                let _ = s.write_all(&[0x04, 0x00]).await;
                return;
            }
            // 2. 方法选择。
            if matches!(mode, StubMode::RequireAuth { .. }) {
                if s.write_all(&[0x05, 0x02]).await.is_err() {
                    return;
                }
                // RFC1929 子协商：01 ULEN U… PLEN P…
                let mut uh = [0u8; 2];
                if s.read_exact(&mut uh).await.is_err() {
                    return;
                }
                let ulen = uh[1] as usize;
                let mut ubuf = vec![0u8; ulen + 1];
                if s.read_exact(&mut ubuf).await.is_err() {
                    return;
                }
                let plen = ubuf[ulen] as usize;
                let mut pbuf = vec![0u8; plen];
                if s.read_exact(&mut pbuf).await.is_err() {
                    return;
                }
                let user = String::from_utf8_lossy(&ubuf[..ulen]).to_string();
                let pass = String::from_utf8_lossy(&pbuf).to_string();
                let ok = match &mode {
                    StubMode::RequireAuth { user: eu, pass: ep } => &user == eu && &pass == ep,
                    _ => false,
                };
                let _ = s.write_all(&[0x01, u8::from(!ok)]).await;
                if !ok {
                    return;
                }
            } else if s.write_all(&[0x05, 0x00]).await.is_err() {
                return;
            }
            // 3. CONNECT 请求：读 VER CMD RSV ATYP＋地址体。
            let mut req4 = [0u8; 4];
            if s.read_exact(&mut req4).await.is_err() {
                return;
            }
            seen2.lock().extend_from_slice(&req4);
            let atyp = req4[3];
            let rest_len = match atyp {
                0x01 => 4 + 2,
                0x03 => {
                    let mut l = [0u8; 1];
                    if s.read_exact(&mut l).await.is_err() {
                        return;
                    }
                    seen2.lock().extend_from_slice(&l);
                    l[0] as usize + 2
                }
                _ => return,
            };
            let mut rest = vec![0u8; rest_len];
            if s.read_exact(&mut rest).await.is_err() {
                return;
            }
            seen2.lock().extend_from_slice(&rest);
            // 4. 回复。
            if matches!(mode, StubMode::ConnectRefused) {
                let _ = s
                    .write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                    .await;
                return;
            }
            let _ = s
                .write_all(&[0x05, 0x00, 0x00, 0x01, 10, 9, 9, 9, 0, 80])
                .await;
        });
        (port, seen)
    }

    #[tokio::test]
    async fn socks5_noauth_connect_sends_expected_bytes() {
        let (port, seen) = spawn_socks5_stub(StubMode::NoAuthOk).await;
        let stream = establish(
            "127.0.0.1",
            port,
            None,
            None,
            EgressProto::Socks5,
            "93.184.216.34",
            80,
        )
        .await
        .expect("establish");
        drop(stream);
        let got = seen.lock().clone();
        // greeting `05 01 00` ＋ CONNECT `05 01 00 01 <v4> <port>`。
        assert_eq!(&got[..3], &[0x05, 0x01, 0x00]);
        assert_eq!(&got[3..7], &[0x05, 0x01, 0x00, 0x01]);
        assert_eq!(&got[7..11], &[93, 184, 216, 34]);
        assert_eq!(&got[11..13], &[0, 80]);
    }

    #[tokio::test]
    async fn socks5_userpass_negotiates() {
        // 账密正确→成功。
        let (port, _) = spawn_socks5_stub(StubMode::RequireAuth {
            user: "u".to_string(),
            pass: "p".to_string(),
        })
        .await;
        assert!(establish(
            "127.0.0.1",
            port,
            Some("u"),
            Some("p"),
            EgressProto::Socks5,
            "example.com",
            443
        )
        .await
        .is_ok());
        // 无账密→被拒（Err，不 panic）。
        let (port2, _) = spawn_socks5_stub(StubMode::RequireAuth {
            user: "u".to_string(),
            pass: "p".to_string(),
        })
        .await;
        assert!(establish(
            "127.0.0.1",
            port2,
            None,
            None,
            EgressProto::Socks5,
            "example.com",
            443
        )
        .await
        .is_err());
        // 错账密→Err。
        let (port3, _) = spawn_socks5_stub(StubMode::RequireAuth {
            user: "u".to_string(),
            pass: "p".to_string(),
        })
        .await;
        assert!(establish(
            "127.0.0.1",
            port3,
            Some("u"),
            Some("wrong"),
            EgressProto::Socks5,
            "example.com",
            443
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn socks5_refused_and_bad_reply_are_err() {
        // 拒连端口。
        assert!(establish(
            "127.0.0.1",
            1,
            None,
            None,
            EgressProto::Socks5,
            "example.com",
            80
        )
        .await
        .is_err());
        // CONNECT 被拒（REP=0x05）。
        let (port, _) = spawn_socks5_stub(StubMode::ConnectRefused).await;
        assert!(establish(
            "127.0.0.1",
            port,
            None,
            None,
            EgressProto::Socks5,
            "example.com",
            80
        )
        .await
        .is_err());
        // 坏版本。
        let (port2, _) = spawn_socks5_stub(StubMode::BadVersion).await;
        assert!(establish(
            "127.0.0.1",
            port2,
            None,
            None,
            EgressProto::Socks5,
            "example.com",
            80
        )
        .await
        .is_err());
        // Http 协议进来即 Err（防御分支）。
        assert!(establish(
            "127.0.0.1",
            port2,
            None,
            None,
            EgressProto::Http,
            "example.com",
            80
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn socks4_connect_ok() {
        // SOCKS4 stub：读 8 字节头＋userid 零结尾，回 00 5A。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.expect("accept");
            let mut head = [0u8; 8];
            if s.read_exact(&mut head).await.is_err() {
                return;
            }
            assert_eq!(&head[..2], &[0x04, 0x01]);
            // userid 读到 0x00。
            loop {
                let mut b = [0u8; 1];
                if s.read_exact(&mut b).await.is_err() || b[0] == 0x00 {
                    break;
                }
            }
            let _ = s.write_all(&[0x00, 0x5A, 0, 80, 10, 9, 9, 9]).await;
        });
        assert!(establish(
            "127.0.0.1",
            port,
            Some("proxy"),
            None,
            EgressProto::Socks4,
            "93.184.216.34",
            80
        )
        .await
        .is_ok());
        // 127.0.0.1:1 拒连→Err。
        assert!(establish(
            "127.0.0.1",
            1,
            None,
            None,
            EgressProto::Socks4,
            "example.com",
            80
        )
        .await
        .is_err());
    }
}
