package link.bewith.nexus;

/** 界面和核心之间（M11.2）：经应用数据目录里的控制套接字问核心「现在该显示什么」，结果是 JSON。 */
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

    static native String nativeRoutes(String dataDir, String operation, String match, String endpoint);
}
