package link.bewith.nexus;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.content.Intent;
import android.content.Context;
import android.content.SharedPreferences;
import android.content.pm.PackageManager;
import android.content.pm.ServiceInfo;
import android.net.ConnectivityManager;
import android.net.LinkProperties;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.NetworkRequest;
import android.net.VpnService;
import android.os.Build;
import android.os.Handler;
import android.os.Looper;
import android.os.ParcelFileDescriptor;
import android.os.SystemClock;
import android.util.Log;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.UnknownHostException;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.util.HashMap;
import java.util.HashSet;
import java.util.Collections;
import java.util.Map;
import java.util.Set;

/**
 * Nexus 的入口：单 VpnService 接管完整节点的网段和名字服务，支持按应用绕过与 N2 省电策略。
 */
public class NexusVpnService extends VpnService {
    static final String ACTION_START = "link.bewith.nexus.START";
    static final String ACTION_STOP = "link.bewith.nexus.STOP";
    static final String VPN_ENABLED = "vpn_enabled";
    /** 按应用选出口（#43）：SharedPreferences 里 `app_route:<包名>` → 出口设备名或 SOCKS5 地址。 */
    static final String APP_ROUTE_PREFIX = "app_route:";
    static final String ACTION_REBUILD = "link.bewith.nexus.REBUILD";
    static final String ACTION_POWER_MODE = "link.bewith.nexus.POWER_MODE";
    static final String BYPASS_FILE = "app_bypass.txt";
    static final String POWER_MODE_FILE = "power_mode.txt";
    private static final String TAG = "nexus";
    static volatile String startError;

    static SharedPreferences settings(Context context) {
        return context.getSharedPreferences("vpn", Context.MODE_PRIVATE);
    }

    static {
        System.loadLibrary("nexus_android");
    }

    private static volatile boolean sRunning = false;
    private static volatile String sPowerMode = "balanced";

    static boolean isVpnRunning() {
        return sRunning;
    }

    static String getActivePowerMode() {
        return sPowerMode;
    }

    private native int nativeStart(String dataDir, String name, int tunFd, String relay, boolean discovery, String rendezvous,
                                   boolean fullCapture, boolean adaptiveCellular);
    private static native void nativeStop();
    private static native boolean nativeStopping();
    private static native void nativeSetNetworks(String[] names, boolean[] metered);

    private ParcelFileDescriptor tun;
    private final Handler main = new Handler(Looper.getMainLooper());
    private Runnable pendingStart;
    private volatile Map<String, String> appRoutes = Collections.emptyMap();
    private volatile Map<Integer, String> configuredUids = Collections.emptyMap();
    private ConnectivityManager cm;
    private String lastName;
    private String lastRelay = "default";
    private boolean lastDiscovery = true;
    private String lastRendezvous = "default";
    /** 系统的每个网络（不含 VPN）：接口名、是否计费。 */
    private final Map<Network, String> ifaces = new HashMap<>();
    private final Map<Network, Boolean> meteredNets = new HashMap<>();
    private final Set<Network> wifi = new HashSet<>();
    private ConnectivityManager.NetworkCallback watch;
    private ConnectivityManager.NetworkCallback keepCellular;

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        String action = intent == null ? null : intent.getAction();
        if (ACTION_STOP.equals(action)) {
            shutdown();
            stopSelf();
            return START_NOT_STICKY;
        }
        if (ACTION_POWER_MODE.equals(action)) {
            String requested = intent.getStringExtra("power_mode");
            if (requested != null) {
                applyPowerMode(requested, true);
            }
            return START_STICKY;
        }
        if (ACTION_REBUILD.equals(action)) {
            if (!sRunning) {
                return START_NOT_STICKY;
            }
            shutdown();
            try {
                Thread.sleep(300);
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
            }
            startTunnel(lastName != null ? lastName : defaultName(), lastRelay, lastDiscovery, sPowerMode, lastRendezvous);
            return START_STICKY;
        }
        if (tun != null) {
            if (intent != null && intent.hasExtra("power_mode")) {
                applyPowerMode(intent.getStringExtra("power_mode"), true);
            }
            return START_STICKY;
        }
        if (intent == null && !settings(this).getBoolean(VPN_ENABLED, true)) {
            stopSelf();
            return START_NOT_STICKY;
        }
        if (pendingStart == null) {
            foreground();
        }
        cancelPendingStart();
        if (nativeStopping()) {
            pendingStart = new Runnable() {
                @Override
                public void run() {
                    onStartCommand(intent, flags, startId);
                }
            };
            main.postDelayed(pendingStart, 100);
            return START_STICKY;
        }
        String name = intent != null && intent.getStringExtra("name") != null ? intent.getStringExtra("name") : defaultName();
        String relay = intent != null && intent.getStringExtra("relay") != null ? intent.getStringExtra("relay") : "default";
        boolean discovery = intent == null || intent.getBooleanExtra("discovery", true);
        String powerMode = resolvePowerMode(intent);
        String rendezvous = intent != null && intent.getStringExtra("rendezvous") != null ? intent.getStringExtra("rendezvous") : "default";
        if (!startTunnel(name, relay, discovery, powerMode, rendezvous)) {
            stopSelf();
            return START_NOT_STICKY;
        }
        return START_STICKY;
    }

    private boolean startTunnel(String name, String relay, boolean discovery, String powerMode, String rendezvous) {
        lastName = name;
        lastRelay = relay;
        lastDiscovery = discovery;
        lastRendezvous = rendezvous;
        try {
            Set<String> bypass = bypassPackages();
            appRoutes = readAppRoutes(this);
            boolean fullCapture = !appRoutes.isEmpty();
            validateAppRoutes(bypass);
            Builder b = new Builder()
                .setSession("Nexus")
                .setMtu(1500)
                .addAddress("198.18.0.1", 16)
                .addRoute("198.18.0.0", 16)
                .addDnsServer("198.18.0.2");
            if (fullCapture) {
                // 有应用指定了出口时接管全部流量，按连接所属的应用分流（routeForConnection）。
                b.addRoute("0.0.0.0", 0).addAddress("fd00:198:18::1", 64).addRoute("::", 0);
            }
            applyDisallowedApps(b);
            tun = b.establish();
            if (tun == null) {
                startError = "系统没有授予 VPN 权限";
                Log.e(TAG, "VPN 没有授权");
                sRunning = false;
                return false;
            }
            // 蜂窝网络按电源模式保持（R104），不用核心的流量采样，所以最后一个参数是 false。
            nativeStart(getFilesDir().getAbsolutePath(), name, tun.detachFd(), relay, discovery, rendezvous,
                fullCapture, false);
            sRunning = true;
            watchNetworks(powerMode);
            Log.i(TAG, "已启动：" + name);
            return true;
        } catch (PackageManager.NameNotFoundException | RuntimeException e) {
            startError = e.getMessage() == null ? "无法启动 VPN" : e.getMessage();
            Log.e(TAG, "无法启动", e);
            shutdown();
            return false;
        }
    }

    static Map<String, String> readAppRoutes(Context context) {
        Map<String, String> routes = new HashMap<>();
        for (Map.Entry<String, ?> entry : settings(context).getAll().entrySet()) {
            if (entry.getKey().startsWith(APP_ROUTE_PREFIX) && entry.getValue() instanceof String) {
                routes.put(entry.getKey().substring(APP_ROUTE_PREFIX.length()), (String) entry.getValue());
            }
        }
        return routes;
    }

    private void validateAppRoutes(Set<String> bypass) throws PackageManager.NameNotFoundException {
        configuredUids = Collections.emptyMap();
        if (appRoutes.isEmpty()) {
            return;
        }
        if (Build.VERSION.SDK_INT < 29) {
            throw new IllegalArgumentException("按应用选出口需要 Android 10 或更高版本");
        }
        PackageManager packages = getPackageManager();
        Map<Integer, String> bypassUids = new HashMap<>();
        bypassUids.put(getApplicationInfo().uid, getPackageName());
        for (String name : bypass) {
            try {
                bypassUids.put(packages.getApplicationInfo(name, 0).uid, name);
            } catch (PackageManager.NameNotFoundException ignored) {
                // The system does not exclude an absent package.
            }
        }
        Map<Integer, String> force = new HashMap<>();
        for (Map.Entry<String, String> entry : appRoutes.entrySet()) {
            String name = entry.getKey();
            int uid = packages.getApplicationInfo(name, 0).uid;
            if (bypassUids.containsKey(uid)) {
                throw new IllegalArgumentException(name + " 与绕过 VPN 的应用 " + bypassUids.get(uid) + " 使用同一个 UID");
            }
            String previous = force.put(uid, entry.getValue());
            if (previous != null && !previous.equals(entry.getValue())) {
                throw new IllegalArgumentException(name + " 与共享 UID 的其他应用使用不同出口");
            }
        }
        configuredUids = force;
    }

    /** JNI 查询连接的 UID。未知、缺少归属或共享 UID 策略冲突返回 null，核心关闭该流。 */
    private String routeForConnection(int protocol, String src, int srcPort, String dst, int dstPort) {
        if (Build.VERSION.SDK_INT < 29) {
            return null;
        }
        try {
            InetSocketAddress local = new InetSocketAddress(InetAddress.getByName(src), srcPort);
            InetSocketAddress remote = new InetSocketAddress(InetAddress.getByName(dst), dstPort);
            int uid = getSystemService(ConnectivityManager.class).getConnectionOwnerUid(protocol, local, remote);
            String[] packages = uid < 0 ? null : getPackageManager().getPackagesForUid(uid);
            if (packages == null || packages.length == 0) {
                return null;
            }
            Map<String, String> routes = appRoutes;
            String route = routes.getOrDefault(packages[0], "");
            for (String name : packages) {
                if (!route.equals(routes.getOrDefault(name, ""))) {
                    return null;
                }
            }
            String configured = configuredUids.get(uid);
            if (configured != null && !configured.equals(route)) {
                return null;
            }
            return route;
        } catch (RuntimeException | UnknownHostException e) {
            return null;
        }
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
        String saved = readSmallFile(new File(getFilesDir(), POWER_MODE_FILE)).trim();
        return saved.isEmpty() ? "balanced" : saved;
    }

    /** 绕过 VPN 的应用（R103 的名单，归一化后，含自己）。按应用选出口时用来检查两者是否冲突。 */
    private Set<String> bypassPackages() {
        String raw = readSmallFile(new File(getFilesDir(), BYPASS_FILE));
        String effective = NexusCore.nativeEffectiveBypass(getPackageName(), raw);
        Set<String> out = new HashSet<>();
        if (effective != null) {
            for (String pkg : effective.split("\n")) {
                if (!pkg.trim().isEmpty()) {
                    out.add(pkg.trim());
                }
            }
        }
        return out;
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

    static String readSmallFile(File file) {
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

    static void writeSmallFile(File file, String content) {
        try (FileOutputStream out = new FileOutputStream(file)) {
            out.write(content.getBytes(StandardCharsets.UTF_8));
        } catch (IOException e) {
            Log.w(TAG, "写入文件失败：" + file.getName(), e);
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
                    if (caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)) {
                        wifi.add(net);
                    } else {
                        wifi.remove(net);
                    }
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
                    wifi.remove(net);
                }
                report();
            }
        };
        cm.registerNetworkCallback(all, watch);
        applyPowerMode(powerMode, false);
    }

    private void applyPowerMode(String requested, boolean persist) {
        String normalized = requested == null ? "balanced" : requested.trim().toLowerCase();
        boolean wantCellular = NexusCore.nativeShouldKeepCellular(normalized);
        sPowerMode = wantCellular ? "realtime" : ("powersave".equals(normalized) ? "powersave" : "balanced");
        if (persist) {
            writeSmallFile(new File(getFilesDir(), POWER_MODE_FILE), sPowerMode);
        }
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
                    Log.w(TAG, "不能请求蜂窝网络", e);
                    keepCellular = null;
                }
            }
            Log.i(TAG, "电源模式：" + sPowerMode + "（已请求保持蜂窝）");
        } else {
            if (keepCellular != null) {
                try {
                    cm.unregisterNetworkCallback(keepCellular);
                } catch (RuntimeException ignored) {
                }
                keepCellular = null;
            }
            Log.i(TAG, "电源模式：" + sPowerMode + "（不主动保持蜂窝）");
        }
    }

    static boolean isMetered(NetworkCapabilities caps) {
        if (caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED)) {
            return false;
        }
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
        cancelPendingStart();
        sRunning = false;
        if (cm != null) {
            for (ConnectivityManager.NetworkCallback cb : new ConnectivityManager.NetworkCallback[] {watch, keepCellular}) {
                if (cb != null) {
                    try {
                        cm.unregisterNetworkCallback(cb);
                    } catch (RuntimeException ignored) {
                    }
                }
            }
            watch = null;
            keepCellular = null;
        }
        synchronized (ifaces) {
            ifaces.clear();
            meteredNets.clear();
            wifi.clear();
        }
        nativeStop();
        tun = null;
    }

    private void unregister(ConnectivityManager.NetworkCallback callback) {
        if (callback != null) {
            try {
                cm.unregisterNetworkCallback(callback);
            } catch (RuntimeException ignored) {
                // 已经注销过
            }
        }
    }

    private void cancelPendingStart() {
        if (pendingStart != null) {
            main.removeCallbacks(pendingStart);
            pendingStart = null;
        }
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
