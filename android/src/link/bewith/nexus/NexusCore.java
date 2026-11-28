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

    /** N1：获取当前生效的路由规则列表（JSON）。 */
    static native String nativeRouteList(String dataDir);

    /** N1：解析并（在非 dryRun 时）导入 Clash/Mihomo 规则子集，返回审计报告 JSON。 */
    static native String nativeRouteImport(
        String dataDir, String yaml, String viaExit, String socks5, boolean replace, boolean dryRun
    );

    /** N1：一键回滚路由规则到上一份 LKG 快照（JSON）。 */
    static native String nativeRouteRollback(String dataDir);

    /** N1：删除指定 1-based 序号的路由规则（JSON）。 */
    static native String nativeRouteRm(String dataDir, int index);

    /** N1：查询任意主机:端口的路由去向解释（JSON）。 */
    static native String nativeRouteExplain(String dataDir, String target);

    /** N1：对当前路由规则执行静态冲突/遮蔽/无效上游审计（JSON）。 */
    static native String nativeRouteAudit(String dataDir);
}
