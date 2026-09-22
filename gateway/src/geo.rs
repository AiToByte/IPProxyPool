//! P3 exit-IP 画像（maxminddb 可选集成）。
//!
//! - 有库（`GEOIP_MMDB_PATH` 可读）：`country(ip)` 返回 ISO 国家码（大写归一）；
//! - 无库／不可读／IP 非法／库内无记录：一律降级为观察跳过（`None`），不执法
//!   （见计划 F0 与 `exit_matches_source` 的 fail-open 语义）；
//! - 沙箱无库：全路径编译通过＋走查，单测只锁 Disabled／纯逻辑／指标（诚实声明）。

/// P3 exit-IP 画像库（`Arc` 共享给 FreePoolWorker；缺库即 Disabled）。
pub enum GeoDb {
    Disabled { reason: &'static str },
    Live(maxminddb::Reader<Vec<u8>>),
}

impl GeoDb {
    /// 缺省构造（单测／无库运营）：永不命中。生产路径走 `open`；
    /// 按 `reload_nodes` 惯例放行 dead（单测消费＋未来热重载入口）。
    #[allow(dead_code)]
    pub fn disabled() -> Self {
        Self::Disabled { reason: "disabled" }
    }

    /// 按路径开库：空串→`unset`；打不开→`unreadable`（warn＋降级，不抛错）。
    /// 生产配库步骤见 OPERATION §6（GeoLite2 license＋挂载＋路径＋重启）。
    pub fn open(path: &str) -> Self {
        if path.is_empty() {
            return Self::Disabled { reason: "unset" };
        }
        match maxminddb::Reader::open_readfile(path) {
            Ok(reader) => {
                log::info!("[GeoIP] live DB loaded from {path}");
                Self::Live(reader)
            }
            Err(e) => {
                log::warn!("[GeoIP] unreadable DB {path}: {e:?} (degraded to observe-skip)");
                Self::Disabled {
                    reason: "unreadable",
                }
            }
        }
    }

    pub fn enabled(&self) -> bool {
        matches!(self, Self::Live(_))
    }

    /// 降级原因（`unset`／`unreadable`／`disabled`；main 启动日志消费，可观测）。
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Disabled { reason } => reason,
            Self::Live(_) => "live",
        }
    }

    /// 有库且命中返回 ISO 国家码（大写归一）；否则 `None`
    /// （调用方按 lookup 结果记 `hit`／`miss`，Disabled 调用方记 `disabled`）。
    pub fn country(&self, ip: &str) -> Option<String> {
        let Self::Live(reader) = self else {
            return None;
        };
        let addr: std::net::IpAddr = ip.parse().ok()?;
        let city: maxminddb::geoip2::City = reader.lookup(addr).ok()?.decode().ok()??;
        city.country.iso_code.map(|s| s.to_ascii_uppercase())
    }
}

/// 纯策略：exit 国家 vs 源站声明国家。
/// fail-open：任一缺失（声明缺省／库无记录）＝无法验证，按跳过计 `true`；
/// 仅两码皆知、声明非 ZZ 且不等（大小写无关）才判分歧 `false`。
///  rationale：mismatch 指标只收“实锤分歧”，未知不刷噪音；执法留 Phase 4。
pub fn exit_matches_source(declared: Option<&str>, looked_up: Option<&str>) -> bool {
    match (declared, looked_up) {
        (Some(d), Some(l)) => d.eq_ignore_ascii_case("zz") || d.eq_ignore_ascii_case(l),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_db_skips_lookup() {
        // 无库降级：country 一律 None（调用方记 disabled，不执法）。
        let g = GeoDb::disabled();
        assert!(g.country("1.2.3.4").is_none());
        assert!(g.country("not-an-ip").is_none());
        assert!(!g.enabled());
    }

    #[test]
    fn open_missing_path_disables_with_reason() {
        // 空串→unset；不存在路径→unreadable（warn 已打，单测只断形态）。
        assert!(!GeoDb::open("").enabled());
        assert!(!GeoDb::open("C:\\no\\such\\db.mmdb").enabled());
    }

    #[test]
    fn exit_matches_source_matrix() {
        // 大小写无关相等；声明缺省（ZZ／None）免检；库无记录免检；
        // 仅两码皆知且不等才 false（fail-open，指标只收实锤分歧）。
        assert!(exit_matches_source(Some("US"), Some("us")));
        assert!(exit_matches_source(Some("ZZ"), Some("DE")));
        assert!(exit_matches_source(None, Some("DE")));
        assert!(exit_matches_source(Some("US"), None));
        assert!(exit_matches_source(None, None));
        assert!(!exit_matches_source(Some("US"), Some("DE")));
    }
}
