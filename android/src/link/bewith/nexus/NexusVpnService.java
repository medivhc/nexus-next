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
import java.util.HashMap;
import java.util.HashSet;
import java.util.Collections;
import java.util.Map;
import java.util.Set;

/**
 * Nexus 的入口：VPN 只接管完整节点的网段和名字服务。
 *
 * 系统里所有程序的名字查询都到 198.18.0.2：.internal 和规则里不直连的域名得到 198.18.x.x 的本机临时地址，
 * 经这个网卡进入核心；其余名字照常解析，连接不经过 VPN。Nexus 自己的连接不经过自己的 VPN。
 *
 * 多路径（M11.4）：系统在有 Wi-Fi 时会关掉移动数据，所以这里请求蜂窝网络一直在线（`multipath` 为 false 时不请求），
 * 核心才能同时经 Wi-Fi 和 5G 连到别的设备；每个网络的接口名和是否计费（系统的 NOT_METERED）交给核心，
 * 计费的网络只承担交互流量和故障接管。
 */
public class NexusVpnService extends VpnService {
    static final String ACTION_START = "link.bewith.nexus.START";
    static final String ACTION_STOP = "link.bewith.nexus.STOP";
    static final String BYPASS_APPS = "bypass_apps";
    static final String VPN_ENABLED = "vpn_enabled";
    static final String APP_ROUTE_PREFIX = "app_route:";
    static final String ADAPTIVE_CELLULAR = "adaptive_cellular";
    private static final String TAG = "nexus";
    private static final long CELLULAR_IDLE_MS = 90_000;
    static volatile String startError;

    static SharedPreferences settings(Context context) {
        return context.getSharedPreferences("vpn", Context.MODE_PRIVATE);
    }

    static {
        System.loadLibrary("nexus_android");
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
    private volatile boolean adaptiveCellular;
    private volatile long lastTunTraffic;
    private long nextCellularRequest;
    private final Runnable refreshCellular = new Runnable() {
        @Override
        public void run() {
            updateCellular();
        }
    };
    private ConnectivityManager cm;
    /** 系统的每个网络（不含 VPN）：接口名、是否计费。 */
    private final Map<Network, String> ifaces = new HashMap<>();
    private final Map<Network, Boolean> meteredNets = new HashMap<>();
    private final Set<Network> wifi = new HashSet<>();
    private ConnectivityManager.NetworkCallback watch;
    private ConnectivityManager.NetworkCallback keepCellular;

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        if (intent != null && ACTION_STOP.equals(intent.getAction())) {
            shutdown();
            stopSelf();
            return START_NOT_STICKY;
        }
        if (tun != null) {
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
        boolean multipath = intent == null || intent.getBooleanExtra("multipath", true);
        // 碰头服务（短加入码，M11.2）：default、disabled 或地址（自建网络里用锚点的 https://<锚点>/pkarr）。
        String rendezvous = intent != null && intent.getStringExtra("rendezvous") != null ? intent.getStringExtra("rendezvous") : "default";
        try {
            Set<String> bypass = settings(this).getStringSet(BYPASS_APPS, Collections.<String>emptySet());
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
                b.addRoute("0.0.0.0", 0).addAddress("fd00:198:18::1", 64).addRoute("::", 0);
            }
            b.addDisallowedApplication(getPackageName());
            for (String app : bypass) {
                if (!app.equals(getPackageName())) {
                    try {
                        b.addDisallowedApplication(app);
                    } catch (PackageManager.NameNotFoundException e) {
                        // An uninstalled app can still be removed from the saved list in settings.
                        Log.i(TAG, "绕过应用未安装：" + app);
                    }
                }
            }
            tun = b.establish();
            if (tun == null) {
                startError = "系统没有授予 VPN 权限";
                Log.e(TAG, "VPN 没有授权");
                stopSelf();
                return START_NOT_STICKY;
            }
            adaptiveCellular = multipath && settings(this).getBoolean(ADAPTIVE_CELLULAR, false);
            nativeStart(getFilesDir().getAbsolutePath(), name, tun.detachFd(), relay, discovery, rendezvous,
                fullCapture, adaptiveCellular);
            watchNetworks(multipath);
            Log.i(TAG, "已启动：" + name);
        } catch (PackageManager.NameNotFoundException | RuntimeException e) {
            startError = e.getMessage() == null ? "无法启动 VPN" : e.getMessage();
            Log.e(TAG, "无法启动", e);
            shutdown();
            stopSelf();
        }
        return START_STICKY;
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

    /** 跟踪系统的网络，把接口名和是否计费交给核心；`multipath` 时请求蜂窝网络一直在线。 */
    private void watchNetworks(boolean multipath) {
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
        if (multipath && !adaptiveCellular) {
            requestCellular();
        }
    }

    private void requestCellular() {
        if (keepCellular != null || SystemClock.elapsedRealtime() < nextCellularRequest) {
            return;
        }
        NetworkRequest request = new NetworkRequest.Builder()
            .addTransportType(NetworkCapabilities.TRANSPORT_CELLULAR)
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
            .build();
        ConnectivityManager.NetworkCallback callback = new ConnectivityManager.NetworkCallback();
        try {
            cm.requestNetwork(request, callback);
            keepCellular = callback;
        } catch (RuntimeException e) {
            nextCellularRequest = SystemClock.elapsedRealtime() + 30_000;
            Log.w(TAG, "不能请求蜂窝网络", e);
        }
    }

    /** Called by the Rust TUN reader, which samples application packets every two seconds. */
    private void onTunTraffic() {
        if (adaptiveCellular) {
            lastTunTraffic = SystemClock.elapsedRealtime();
            main.post(refreshCellular);
        }
    }

    private void updateCellular() {
        main.removeCallbacks(refreshCellular);
        if (!adaptiveCellular || watch == null) {
            return;
        }
        boolean hasWifi;
        synchronized (ifaces) {
            hasWifi = !wifi.isEmpty();
        }
        long remaining = lastTunTraffic == 0 ? 0 : lastTunTraffic + CELLULAR_IDLE_MS - SystemClock.elapsedRealtime();
        if (hasWifi && remaining > 0) {
            requestCellular();
            main.postDelayed(refreshCellular, remaining);
        } else if (keepCellular != null) {
            unregister(keepCellular);
            keepCellular = null;
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
        if (adaptiveCellular) {
            main.post(refreshCellular);
        }
    }

    private void shutdown() {
        cancelPendingStart();
        adaptiveCellular = false;
        lastTunTraffic = 0;
        nextCellularRequest = 0;
        main.removeCallbacks(refreshCellular);
        if (cm != null) {
            unregister(watch);
            unregister(keepCellular);
            watch = null;
            keepCellular = null;
        }
        synchronized (ifaces) {
            ifaces.clear();
            meteredNets.clear();
            wifi.clear();
        }
        nativeStop();
        tun = null; // 描述符已交给核心，核心停止时关闭
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
