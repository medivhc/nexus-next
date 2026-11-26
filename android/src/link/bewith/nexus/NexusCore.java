package link.bewith.nexus;

/** 界面和核心之间（M11.2 / N1 / N2）：经控制套接字或纯逻辑 JNI 与 Rust 核心交互。 */
final class NexusCore {
    static {
        System.loadLibrary("nexus_android");
    }

    private NexusCore() {}

    /**
     * 刷新一次。held 是上次返回的 JSON（没有时为空串），renew 要求换一个加入码，nowMs 是现在的时间。
     * 返回 {"state": "starting"|"joined"|"joining"|"failed", ...}，字段见 nexus_cli::app::Screen。
     */
    static native String nativeScreen(String dataDir, String held, boolean renew, long nowMs);

    /** N2：判断给定电源模式（balanced / powersave / realtime）是否需要在有 Wi-Fi 时保持蜂窝在线。 */
    static native boolean nativeShouldKeepCellular(String powerMode);

    /** N1：将用户配置的绕过应用文本归一化为换行分隔包名，并强制包含自身包名防 VPN 环路。 */
    static native String nativeEffectiveBypass(String selfPackage, String rawLines);
}
