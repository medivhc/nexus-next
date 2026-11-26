//! 本机的网络接口，以及哪些按流量计费（M5.2）。
//!
//! 自动识别（Linux、Android）：蜂窝网络的设备（`DEVTYPE=wwan`、原始 IP 类型的接口）和常见的蜂窝、
//! USB 网络共享接口名（`wwan`、`rmnet`、`ccmni`、`usb`、`rndis`）。用户可以按接口名手动覆盖。
//!
//! Android（M11.4）：系统知道每个网络是否计费（`NET_CAPABILITY_NOT_METERED`，包括计费的 Wi-Fi 热点），
//! 应用把它经 [`set_system_networks`] 交过来；系统报告过的接口按系统的结果，其余的仍按名字和设备类型识别。

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Mutex;

/// 系统报告的网络：接口名 → 是否计费。`None`：系统没有报告（Linux）。
static SYSTEM: Mutex<Option<BTreeMap<String, bool>>> = Mutex::new(None);

/// 系统（Android 的 ConnectivityManager）报告的当前网络：接口名和是否计费。每次网络变化时整体替换。
pub fn set_system_networks(list: impl IntoIterator<Item = (String, bool)>) {
    let map = list.into_iter().collect();
    *SYSTEM.lock().unwrap_or_else(|e| e.into_inner()) = Some(map);
}

/// 自动识别的结果：系统报告过这个接口就按系统的，否则按名字和设备类型。
fn auto_metered(name: &str, system: Option<&BTreeMap<String, bool>>) -> bool {
    match system.and_then(|s| s.get(name)) {
        Some(&m) => m,
        None => looks_metered(name),
    }
}

/// 一个接口：名字、地址、自动识别的结果。
pub(crate) struct Iface {
    pub name: String,
    pub addrs: Vec<IpAddr>,
    pub auto_metered: bool,
}

/// 现在启用的、不是回环的接口（按名字排序）。
pub(crate) fn scan() -> Vec<Iface> {
    #[cfg_attr(not(any(target_os = "linux", target_os = "android", target_os = "macos")), allow(unused_mut))]
    let mut by_name: BTreeMap<String, Vec<IpAddr>> = BTreeMap::new();
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    if let Ok(list) = nix::ifaddrs::getifaddrs() {
        use nix::net::if_::InterfaceFlags;
        for ifa in list {
            if !ifa.flags.contains(InterfaceFlags::IFF_UP) || ifa.flags.contains(InterfaceFlags::IFF_LOOPBACK) {
                continue;
            }
            let Some(addr) = ifa.address else { continue };
            let ip = if let Some(a) = addr.as_sockaddr_in() {
                IpAddr::V4(a.ip())
            } else if let Some(a) = addr.as_sockaddr_in6() {
                IpAddr::V6(a.ip())
            } else {
                continue;
            };
            by_name.entry(ifa.interface_name).or_default().push(ip);
        }
    }
    let system = SYSTEM.lock().unwrap_or_else(|e| e.into_inner()).clone();
    by_name
        .into_iter()
        .map(|(name, addrs)| {
            let auto_metered = auto_metered(&name, system.as_ref());
            Iface { name, addrs, auto_metered }
        })
        .collect()
}

/// 按接口名和系统信息判断是不是按流量计费的网络。
pub(crate) fn looks_metered(name: &str) -> bool {
    const PREFIXES: [&str; 6] = ["wwan", "ww", "rmnet", "ccmni", "usb", "rndis"];
    if PREFIXES.iter().any(|p| name.starts_with(p)) {
        return true;
    }
    let sys = format!("/sys/class/net/{name}");
    if std::fs::read_to_string(format!("{sys}/uevent")).is_ok_and(|u| u.lines().any(|l| l.trim() == "DEVTYPE=wwan")) {
        return true;
    }
    // ARPHRD_RAWIP（519）：蜂窝调制解调器的原始 IP 接口。
    std::fs::read_to_string(format!("{sys}/type")).is_ok_and(|t| t.trim() == "519")
}

#[cfg(test)]
mod tests {
    use super::{auto_metered, looks_metered};
    use std::collections::BTreeMap;

    // covers: R88
    #[test]
    fn the_system_report_wins_over_names() {
        // Android：计费的 Wi-Fi 热点（名字看不出来）按系统的结果计费；系统说不计费的蜂窝网络（不限流量的套餐）不计费。
        let system: BTreeMap<String, bool> = [("wlan0".to_string(), true), ("rmnet_data0".to_string(), false)].into();
        assert!(auto_metered("wlan0", Some(&system)));
        assert!(!auto_metered("rmnet_data0", Some(&system)));
        // 系统没有报告的接口（例如 USB 网络共享）仍按名字识别。
        assert!(auto_metered("rndis0", Some(&system)));
        assert!(!auto_metered("eth0", Some(&system)));
        // 没有系统报告（Linux）：和原来一样。
        assert!(!auto_metered("wlan0", None));
        assert!(auto_metered("rmnet_data0", None));
    }

    #[test]
    fn cellular_and_tethering_names_are_metered() {
        for n in ["wwan0", "wwp0s20f0u2i12", "rmnet_data0", "ccmni1", "usb0", "rndis0"] {
            assert!(looks_metered(n), "{n}");
        }
        for n in ["eth0", "wlan0", "enp3s0", "wlp2s0", "docker0"] {
            assert!(!looks_metered(n), "{n}");
        }
    }
}
