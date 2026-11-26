package link.bewith.nexus;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.pm.ServiceInfo;
import android.net.VpnService;
import android.os.Build;
import android.os.ParcelFileDescriptor;
import android.util.Log;

/**
 * Nexus 的入口：VPN 只接管完整节点的网段和名字服务。
 *
 * 系统里所有程序的名字查询都到 198.18.0.2：.internal 和规则里不直连的域名得到 198.18.x.x 的本机临时地址，
 * 经这个网卡进入核心；其余名字照常解析，连接不经过 VPN。Nexus 自己的连接不经过自己的 VPN。
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

    private ParcelFileDescriptor tun;

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

    private void shutdown() {
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
