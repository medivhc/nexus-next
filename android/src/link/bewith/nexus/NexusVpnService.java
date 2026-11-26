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
import java.util.HashMap;
import java.util.Map;

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
    private static final String TAG = "nexus";

    static {
        System.loadLibrary("nexus_android");
    }

    private static native int nativeStart(String dataDir, String name, int tunFd, String relay, boolean discovery, String rendezvous);
    private static native void nativeStop();
    private static native void nativeSetNetworks(String[] names, boolean[] metered);

    private ParcelFileDescriptor tun;
    private ConnectivityManager cm;
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
        if (tun != null) {
            return START_STICKY;
        }
        foreground();
        String name = intent != null && intent.getStringExtra("name") != null ? intent.getStringExtra("name") : defaultName();
        String relay = intent != null && intent.getStringExtra("relay") != null ? intent.getStringExtra("relay") : "default";
        boolean discovery = intent == null || intent.getBooleanExtra("discovery", true);
        boolean multipath = intent == null || intent.getBooleanExtra("multipath", true);
        // 碰头服务（短加入码，M11.2）：default、disabled 或地址（自建网络里用锚点的 https://<锚点>/pkarr）。
        String rendezvous = intent != null && intent.getStringExtra("rendezvous") != null ? intent.getStringExtra("rendezvous") : "default";
        try {
            Builder b = new Builder()
                .setSession("Nexus")
                .setMtu(1500)
                .addAddress("198.18.0.1", 16)
                .addRoute("198.18.0.0", 16)
                .addDnsServer("198.18.0.2");
            b.addDisallowedApplication(getPackageName());
            tun = b.establish();
            if (tun == null) {
                Log.e(TAG, "VPN 没有授权");
                stopSelf();
                return START_NOT_STICKY;
            }
            nativeStart(getFilesDir().getAbsolutePath(), name, tun.detachFd(), relay, discovery, rendezvous);
            watchNetworks(multipath);
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
        if (multipath) {
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
