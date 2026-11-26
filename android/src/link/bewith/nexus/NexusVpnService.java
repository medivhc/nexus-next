package link.bewith.nexus;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.pm.ServiceInfo;
import android.net.ConnectivityManager;
import android.net.LinkProperties;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.NetworkRequest;
import android.net.VpnService;
import android.os.Build;
import android.os.ParcelFileDescriptor;
import android.util.Log;
import java.io.File;
import java.io.FileInputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.util.HashMap;
import java.util.Map;

/**
 * Nexus 的入口：单 VpnService 接管完整节点的网段和名字服务，支持按应用绕过与 N2 省电策略。
 *
 * - 系统里所有未绕过应用的名字查询都到 198.18.0.2：.internal 和规则里不直连的域名得到 198.18.x.x 的本机临时地址，
 *   经这个网卡进入核心；其余名字照常解析，连接不经过 VPN。
 * - 防环路保护（N1）：通过 NexusCore.nativeEffectiveBypass 合并用户配置的 app_bypass.txt 与本机包名，
 *   确保 Nexus 自身连接永远不经过自己的 VPN。
 * - 省电策略（N2）：默认采用 balanced 模式（不主动在有 Wi-Fi 时维持蜂窝电台）；仅在显式切换为 realtime
 *   或传入 --ez multipath true 时请求蜂窝网络，切回或停止时立即注销 NetworkCallback。
 */
public class NexusVpnService extends VpnService {
    static final String ACTION_START = "link.bewith.nexus.START";
    static final String ACTION_STOP = "link.bewith.nexus.STOP";
    static final String ACTION_POWER_MODE = "link.bewith.nexus.POWER_MODE";
    private static final String TAG = "nexus";
    private static final String BYPASS_FILE = "app_bypass.txt";

    static {
        System.loadLibrary("nexus_android");
    }

    private native int nativeStart(String dataDir, String name, int tunFd, String relay, boolean discovery, String rendezvous);
    private static native void nativeStop();
    private static native void nativeSetNetworks(String[] names, boolean[] metered);

    private ParcelFileDescriptor tun;
    private ConnectivityManager cm;
    private String currentPowerMode = "balanced";
    /** 系统的每个网络（不含 VPN）：接口名、是否计费。 */
    private final Map<Network, String> ifaces = new HashMap<>();
    private final Map<Network, Boolean> meteredNets = new HashMap<>();
    private ConnectivityManager.NetworkCallback watch;
    private ConnectivityManager.NetworkCallback keepCellular;

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        if (intent != null && ACTION_STOP.equals(intent.getAction())) {
            shutdown();
            stopSelf();
            return START_NOT_STICKY;
        }
        if (intent != null && ACTION_POWER_MODE.equals(intent.getAction())) {
            String requested = intent.getStringExtra("power_mode");
            if (requested != null) {
                applyPowerMode(requested);
            }
            return START_STICKY;
        }
        if (tun != null) {
            if (intent != null && intent.hasExtra("power_mode")) {
                applyPowerMode(intent.getStringExtra("power_mode"));
            }
            return START_STICKY;
        }
        foreground();
        String name = intent != null && intent.getStringExtra("name") != null ? intent.getStringExtra("name") : defaultName();
        String relay = intent != null && intent.getStringExtra("relay") != null ? intent.getStringExtra("relay") : "default";
        boolean discovery = intent == null || intent.getBooleanExtra("discovery", true);
        String powerMode = resolvePowerMode(intent);
        String rendezvous = intent != null && intent.getStringExtra("rendezvous") != null ? intent.getStringExtra("rendezvous") : "default";
        try {
            Builder b = new Builder()
                .setSession("Nexus")
                .setMtu(1500)
                .addAddress("198.18.0.1", 16)
                .addRoute("198.18.0.0", 16)
                .addDnsServer("198.18.0.2");
            applyDisallowedApps(b);
            tun = b.establish();
            if (tun == null) {
                Log.e(TAG, "VPN 没有授权");
                stopSelf();
                return START_NOT_STICKY;
            }
            nativeStart(getFilesDir().getAbsolutePath(), name, tun.detachFd(), relay, discovery, rendezvous);
            watchNetworks(powerMode);
            Log.i(TAG, "已启动：" + name);
        } catch (PackageManager.NameNotFoundException | RuntimeException e) {
            Log.e(TAG, "无法启动", e);
            shutdown();
            stopSelf();
        }
        return START_STICKY;
    }

    @Override
    public void onRevoke() {
        shutdown();
        super.onRevoke();
    }

    @Override
    public void onDestroy() {
        shutdown();
        super.onDestroy();
    }

    private String resolvePowerMode(Intent intent) {
        if (intent != null && intent.getStringExtra("power_mode") != null) {
            return intent.getStringExtra("power_mode");
        }
        if (intent != null && intent.hasExtra("multipath")) {
            return intent.getBooleanExtra("multipath", false) ? "realtime" : "balanced";
        }
        // N2 默认平衡省电模式：不主动在有 Wi-Fi 时拉起蜂窝网络。
        return "balanced";
    }

    private void applyDisallowedApps(Builder b) throws PackageManager.NameNotFoundException {
        String raw = readSmallFile(new File(getFilesDir(), BYPASS_FILE));
        String effective = NexusCore.nativeEffectiveBypass(getPackageName(), raw);
        boolean selfExcluded = false;
        if (effective != null && !effective.isEmpty()) {
            for (String pkg : effective.split("\n")) {
                String trimmed = pkg.trim();
                if (trimmed.isEmpty()) {
                    continue;
                }
                try {
                    b.addDisallowedApplication(trimmed);
                    if (getPackageName().equals(trimmed)) {
                        selfExcluded = true;
                    }
                } catch (PackageManager.NameNotFoundException e) {
                    if (getPackageName().equals(trimmed)) {
                        throw e;
                    }
                    Log.w(TAG, "忽略未安装的绕过应用：" + trimmed);
                }
            }
        }
        if (!selfExcluded) {
            b.addDisallowedApplication(getPackageName());
        }
    }

    private static String readSmallFile(File file) {
        if (!file.exists() || !file.isFile()) {
            return "";
        }
        try (FileInputStream in = new FileInputStream(file)) {
            byte[] buf = new byte[(int) Math.min(file.length(), 65536)];
            int n = in.read(buf);
            return n > 0 ? new String(buf, 0, n, StandardCharsets.UTF_8) : "";
        } catch (IOException e) {
            return "";
        }
    }

    /** 跟踪系统的网络，把接口名和是否计费交给核心；按 N2 电源模式决定是否保持蜂窝在线。 */
    private void watchNetworks(String powerMode) {
        cm = getSystemService(ConnectivityManager.class);
        NetworkRequest all = new NetworkRequest.Builder()
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
            .build();
        watch = new ConnectivityManager.NetworkCallback() {
            @Override
            public void onCapabilitiesChanged(Network net, NetworkCapabilities caps) {
                synchronized (ifaces) {
                    meteredNets.put(net, isMetered(caps));
                }
                report();
            }

            @Override
            public void onLinkPropertiesChanged(Network net, LinkProperties lp) {
                synchronized (ifaces) {
                    if (lp.getInterfaceName() != null) {
                        ifaces.put(net, lp.getInterfaceName());
                    }
                }
                report();
            }

            @Override
            public void onLost(Network net) {
                synchronized (ifaces) {
                    ifaces.remove(net);
                    meteredNets.remove(net);
                }
                report();
            }
        };
        cm.registerNetworkCallback(all, watch);
        applyPowerMode(powerMode);
    }

    private void applyPowerMode(String requested) {
        String normalized = requested == null ? "balanced" : requested.trim().toLowerCase();
        boolean wantCellular = NexusCore.nativeShouldKeepCellular(normalized);
        currentPowerMode = wantCellular ? "realtime" : ("powersave".equals(normalized) ? "powersave" : "balanced");
        if (cm == null) {
            return;
        }
        if (wantCellular) {
            if (keepCellular == null) {
                NetworkRequest cellular = new NetworkRequest.Builder()
                    .addTransportType(NetworkCapabilities.TRANSPORT_CELLULAR)
                    .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
                    .build();
                keepCellular = new ConnectivityManager.NetworkCallback();
                try {
                    cm.requestNetwork(cellular, keepCellular);
                } catch (RuntimeException e) {
                    // 没有蜂窝网络的设备（平板、模拟器）或没有权限：只用现有的网络。
                    Log.w(TAG, "不能请求蜂窝网络", e);
                    keepCellular = null;
                }
            }
            Log.i(TAG, "电源模式：" + currentPowerMode + "（已请求保持蜂窝）");
        } else {
            if (keepCellular != null) {
                try {
                    cm.unregisterNetworkCallback(keepCellular);
                } catch (RuntimeException ignored) {
                    // 已经注销过
                }
                keepCellular = null;
            }
            Log.i(TAG, "电源模式：" + currentPowerMode + "（不主动保持蜂窝）");
        }
    }

    /** 系统认为计费（没有 NOT_METERED，也不是临时不计费）。 */
    static boolean isMetered(NetworkCapabilities caps) {
        if (caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED)) {
            return false;
        }
        // API 30 起：运营商临时不计费（例如 5G 不限量时段）。
        return Build.VERSION.SDK_INT < 30 || !caps.hasCapability(25 /* NET_CAPABILITY_TEMPORARILY_NOT_METERED */);
    }

    private void report() {
        String[] names;
        boolean[] metered;
        synchronized (ifaces) {
            names = new String[ifaces.size()];
            metered = new boolean[ifaces.size()];
            int i = 0;
            for (Map.Entry<Network, String> e : ifaces.entrySet()) {
                names[i] = e.getValue();
                Boolean m = meteredNets.get(e.getKey());
                metered[i] = m != null && m;
                i++;
            }
        }
        StringBuilder sb = new StringBuilder("网络：");
        for (int i = 0; i < names.length; i++) {
            sb.append(names[i]).append(metered[i] ? "（计费） " : " ");
        }
        Log.i(TAG, sb.toString().trim());
        nativeSetNetworks(names, metered);
    }

    private void shutdown() {
        if (cm != null) {
            for (ConnectivityManager.NetworkCallback cb : new ConnectivityManager.NetworkCallback[] {watch, keepCellular}) {
                if (cb != null) {
                    try {
                        cm.unregisterNetworkCallback(cb);
                    } catch (RuntimeException ignored) {
                        // 已经注销过
                    }
                }
            }
            watch = null;
            keepCellular = null;
        }
        nativeStop();
        tun = null; // 描述符已交给核心，核心停止时关闭
    }

    private void foreground() {
        NotificationManager nm = getSystemService(NotificationManager.class);
        nm.createNotificationChannel(new NotificationChannel("nexus", "Nexus", NotificationManager.IMPORTANCE_LOW));
        Notification n = new Notification.Builder(this, "nexus")
            .setContentTitle("Nexus")
            .setContentText("已连接")
            .setSmallIcon(android.R.drawable.stat_sys_upload_done)
            .build();
        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(1, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE);
        } else {
            startForeground(1, n);
        }
    }

    private static String defaultName() {
        String m = Build.MODEL == null ? "phone" : Build.MODEL;
        String s = m.toLowerCase().replaceAll("[^a-z0-9-]", "-");
        return s.isEmpty() ? "phone" : s;
    }
}
