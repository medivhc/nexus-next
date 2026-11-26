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
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.util.HashMap;
import java.util.Map;

/**
 * Nexus 的入口：单 VpnService 接管完整节点的网段和名字服务，支持按应用绕过与 N2 省电策略。
 */
public class NexusVpnService extends VpnService {
    static final String ACTION_START = "link.bewith.nexus.START";
    static final String ACTION_STOP = "link.bewith.nexus.STOP";
    static final String ACTION_REBUILD = "link.bewith.nexus.REBUILD";
    static final String ACTION_POWER_MODE = "link.bewith.nexus.POWER_MODE";
    static final String BYPASS_FILE = "app_bypass.txt";
    static final String POWER_MODE_FILE = "power_mode.txt";
    private static final String TAG = "nexus";

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

    private native int nativeStart(String dataDir, String name, int tunFd, String relay, boolean discovery, String rendezvous);
    private static native void nativeStop();
    private static native void nativeSetNetworks(String[] names, boolean[] metered);

    private ParcelFileDescriptor tun;
    private ConnectivityManager cm;
    private String lastName;
    private String lastRelay = "default";
    private boolean lastDiscovery = true;
    private String lastRendezvous = "default";
    /** 系统的每个网络（不含 VPN）：接口名、是否计费。 */
    private final Map<Network, String> ifaces = new HashMap<>();
    private final Map<Network, Boolean> meteredNets = new HashMap<>();
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
        foreground();
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
                sRunning = false;
                return false;
            }
            nativeStart(getFilesDir().getAbsolutePath(), name, tun.detachFd(), relay, discovery, rendezvous);
            sRunning = true;
            watchNetworks(powerMode);
            Log.i(TAG, "已启动：" + name);
            return true;
        } catch (PackageManager.NameNotFoundException | RuntimeException e) {
            Log.e(TAG, "无法启动", e);
            shutdown();
            return false;
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
        nativeStop();
        tun = null;
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
