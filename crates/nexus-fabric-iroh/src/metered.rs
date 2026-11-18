//! 本机的网络接口，以及哪些按流量计费（M5.2）。
//!
//! 自动识别（Linux、Android）：蜂窝网络的设备（`DEVTYPE=wwan`、原始 IP 类型的接口）和常见的蜂窝、
//! USB 网络共享接口名（`wwan`、`rmnet`、`ccmni`、`usb`、`rndis`）。用户可以按接口名手动覆盖。

use std::collections::BTreeMap;
use std::net::IpAddr;

/// 一个接口：名字、地址、自动识别的结果。
pub(crate) struct Iface {
    pub name: String,
    pub addrs: Vec<IpAddr>,
    pub auto_metered: bool,
}

/// 现在启用的、不是回环的接口（按名字排序）。
pub(crate) fn scan() -> Vec<Iface> {
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
    by_name
        .into_iter()
        .map(|(name, addrs)| {
            let auto_metered = looks_metered(&name);
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
    use super::looks_metered;

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
