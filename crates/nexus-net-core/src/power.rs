//! 电源模式与移动端蜂窝保活策略（N2）。
//!
//! 解决 Android 默认 `multipath=true` 持续拉起蜂窝电台的耗电问题：
//! `Balanced`（默认）和 `PowerSave` 不主动维持蜂窝链路，仅 `Realtime` 显式开启。

use serde::{Deserialize, Serialize};

/// 设备的网络功耗模式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerMode {
    /// 默认平衡模式：不主动在有 Wi-Fi 时保持蜂窝在线，按需探测。
    #[default]
    Balanced,
    /// 省电模式：更长探测间隔，不主动保持蜂窝，不启用冗余发包。
    PowerSave,
    /// 实时多路径模式：显式请求蜂窝在线并允许交互冗余与快速故障接管。
    Realtime,
}

impl PowerMode {
    /// 解析外部传入的模式字符串；未知值安全回退到 `Balanced`。
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "powersave" | "power_save" | "idle" | "low_power" => Self::PowerSave,
            "realtime" | "real_time" | "performance" | "multipath" => Self::Realtime,
            _ => Self::Balanced,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Balanced => "balanced",
            Self::PowerSave => "powersave",
            Self::Realtime => "realtime",
        }
    }
}

/// 由电源模式派生的确定性执行参数。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerPolicy {
    pub mode: PowerMode,
    /// 在 Wi-Fi 可用时是否额外向系统请求保持蜂窝网络活跃。
    pub keep_cellular_with_wifi: bool,
    /// 空闲状态下的最小健康/路径探测间隔（秒）。
    pub idle_probe_interval_secs: u64,
    /// 是否允许交互流量多路径冗余发送。
    pub allow_multipath_redundancy: bool,
}

impl PowerPolicy {
    pub const fn for_mode(mode: PowerMode) -> Self {
        match mode {
            PowerMode::Balanced => Self {
                mode,
                keep_cellular_with_wifi: false,
                idle_probe_interval_secs: 60,
                allow_multipath_redundancy: false,
            },
            PowerMode::PowerSave => Self {
                mode,
                keep_cellular_with_wifi: false,
                idle_probe_interval_secs: 180,
                allow_multipath_redundancy: false,
            },
            PowerMode::Realtime => Self {
                mode,
                keep_cellular_with_wifi: true,
                idle_probe_interval_secs: 15,
                allow_multipath_redundancy: true,
            },
        }
    }
}
