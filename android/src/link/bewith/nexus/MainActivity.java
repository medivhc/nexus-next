package link.bewith.nexus;

import android.app.Activity;
import android.app.AlertDialog;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Intent;
import android.content.DialogInterface;
import android.content.pm.PackageManager;
import android.content.pm.ResolveInfo;
import android.graphics.Bitmap;
import android.graphics.Color;
import android.graphics.Typeface;
import android.net.VpnService;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.view.Gravity;
import android.view.View;
import android.widget.Button;
import android.widget.CheckBox;
import android.widget.CompoundButton;
import android.widget.EditText;
import android.widget.ImageView;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;
import android.widget.Toast;
import java.io.File;
import java.io.IOException;
import java.io.RandomAccessFile;
import java.nio.charset.StandardCharsets;
import java.util.ArrayDeque;
import java.util.Collections;
import java.util.HashSet;
import java.util.Map;
import java.util.Set;
import java.util.TreeMap;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

/**
 * 请求 VPN 授权（第一次会弹出系统对话框），启动 Nexus，然后显示这台设备的状态（M11.2，R79）：
 * 还没加入网络时显示短加入码、长加入码和二维码，管理设备批准后自动变成「已加入」。
 * 显示什么由核心决定（NexusCore.nativeScreen），这里只负责画和每 2 秒刷新一次。
 */
public class MainActivity extends Activity {
    private static final int REQUEST_VPN = 1;
    private static final long REFRESH_MS = 2000;

    private final Handler main = new Handler(Looper.getMainLooper());
    private final ExecutorService worker = Executors.newSingleThreadExecutor();
    private String held = "";
    private boolean renew;
    private boolean running;
    private boolean busy;
    private final Runnable ticker = new Runnable() {
        @Override
        public void run() {
            tick();
        }
    };

    private TextView title;
    private TextView message;
    private TextView shortCode;
    private ImageView qr;
    private TextView longCode;
    private Button renewButton;
    private TextView devices;
    private Button vpnButton;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        buildViews();
        if ("stop".equals(getIntent().getStringExtra("action"))) {
            stopVpn();
            return;
        }
        if (!NexusVpnService.settings(this).getBoolean(NexusVpnService.VPN_ENABLED, true)
            && getIntent().getStringExtra("name") == null) {
            showStopped();
            return;
        }
        requestVpn();
    }

    private void requestVpn() {
        Intent consent = VpnService.prepare(this);
        if (consent != null) {
            startActivityForResult(consent, REQUEST_VPN);
        } else {
            onActivityResult(REQUEST_VPN, RESULT_OK, null);
        }
    }

    @Override
    protected void onActivityResult(int request, int result, Intent data) {
        if (request != REQUEST_VPN) {
            return;
        }
        if (result != RESULT_OK) {
            show(title, "需要 VPN 授权");
            show(message, "Nexus 通过系统的 VPN 接入你的网络。重新打开应用并同意授权。");
            return;
        }
        Intent start = new Intent(this, NexusVpnService.class).setAction(NexusVpnService.ACTION_START);
        if (getIntent().getExtras() != null) {
            start.putExtras(getIntent().getExtras());
        }
        NexusVpnService.settings(this).edit().putBoolean(NexusVpnService.VPN_ENABLED, true).apply();
        NexusVpnService.startError = null;
        startForegroundService(start);
        running = true;
        vpnButton.setText("停用 VPN");
        tick();
    }

    private void stopVpn() {
        running = false;
        held = "";
        renew = false;
        main.removeCallbacks(ticker);
        startService(new Intent(this, NexusVpnService.class).setAction(NexusVpnService.ACTION_STOP));
        showStopped();
    }

    private void showStopped() {
        hideAll();
        show(title, "Nexus 已停止");
        vpnButton.setText("启用 VPN");
    }

    @Override
    protected void onResume() {
        super.onResume();
        if (running) {
            tick();
        }
    }

    @Override
    protected void onPause() {
        super.onPause();
        main.removeCallbacks(ticker);
    }

    @Override
    protected void onDestroy() {
        super.onDestroy();
        worker.shutdownNow();
    }

    /** 在后台线程里问核心，回到界面线程画出来，2 秒后再来。 */
    private void tick() {
        // 只撤掉排着的下一次刷新；后台线程投递回来的结果不能撤，否则 busy 一直为真、界面再也不刷新。
        main.removeCallbacks(ticker);
        if (busy) {
            return;
        }
        busy = true;
        final String dir = getFilesDir().getAbsolutePath();
        final String h = held;
        final boolean r = renew;
        renew = false;
        // 用匿名类而不是 lambda：构建不经 Gradle（javac -source 8 + d8），android.jar 里没有 LambdaMetafactory。
        worker.execute(new Runnable() {
            @Override
            public void run() {
                final String json = NexusCore.nativeScreen(dir, h, r, System.currentTimeMillis());
                main.post(new Runnable() {
                    @Override
                    public void run() {
                        busy = false;
                        if (running && !isFinishing() && !isDestroyed()) {
                            render(json);
                            if (running) {
                                main.postDelayed(ticker, REFRESH_MS);
                            }
                        }
                    }
                });
            }
        });
    }

    private void render(String json) {
        String startError = NexusVpnService.startError;
        if (startError != null) {
            running = false;
            showStopped();
            show(message, startError);
            return;
        }
        JSONObject s;
        try {
            s = new JSONObject(json == null ? "" : json);
        } catch (JSONException e) {
            s = new JSONObject();
        }
        String state = s.optString("state", "failed");
        hideAll();
        switch (state) {
            case "starting":
                show(title, "正在启动 Nexus…");
                break;
            case "joined":
                held = "";
                show(title, "已加入网络");
                show(message, "本机：" + s.optString("name"));
                show(devices, deviceList(s.optJSONArray("devices")));
                break;
            case "joining":
                held = json;
                renderJoining(s);
                break;
            default:
                show(title, "出错了");
                show(message, s.optString("message", "核心的回应无法理解"));
        }
    }

    private void renderJoining(JSONObject s) {
        show(title, "加入你的网络");
        String shortText = s.isNull("short_code") ? null : s.optString("short_code");
        long left = Math.max(0, (s.optLong("expires_at_ms") - System.currentTimeMillis()) / 60000);
        if (shortText != null) {
            show(shortCode, shortText);
            show(message, "在管理设备上运行：\nnexus device approve " + shortText
                + "\n\n也可以扫下面的二维码，把扫出的文字交给 nexus device approve。约 " + (left + 1) + " 分钟内有效，到期前自动换新。");
        } else {
            show(message, "短加入码不可用（" + s.optString("short_code_error") + "）。\n扫下面的二维码，或复制长加入码，"
                + "在管理设备上运行 nexus device approve <加入码>。约 " + (left + 1) + " 分钟内有效。");
        }
        JSONObject q = s.optJSONObject("qr");
        if (q != null) {
            qr.setImageBitmap(qrBitmap(q.optJSONArray("rows")));
            qr.setVisibility(View.VISIBLE);
            qr.setContentDescription(s.optString("qr_text"));
        }
        show(longCode, s.optString("code"));
        renewButton.setVisibility(View.VISIBLE);
    }

    private static String deviceList(JSONArray list) {
        StringBuilder b = new StringBuilder("网络里的设备：\n");
        for (int i = 0; list != null && i < list.length(); i++) {
            JSONObject d = list.optJSONObject(i);
            if (d == null) {
                continue;
            }
            b.append("\n").append(d.optBoolean("online") ? "●  " : "○  ").append(d.optString("name"));
            if (d.optBoolean("is_self")) {
                b.append("（本机）");
            }
            if (d.optBoolean("admin")) {
                b.append("（管理设备）");
            }
        }
        return b.toString();
    }

    /** 点阵画成位图：每格 8 像素，四周留 4 格白边（扫码工具需要）。 */
    private static Bitmap qrBitmap(JSONArray rows) {
        int n = rows == null ? 0 : rows.length();
        int margin = 4, scale = 8, px = (n + 2 * margin) * scale;
        int[] pixels = new int[px * px];
        for (int y = 0; y < px; y++) {
            for (int x = 0; x < px; x++) {
                int mx = x / scale - margin, my = y / scale - margin;
                boolean dark = mx >= 0 && my >= 0 && mx < n && my < n && rows.optString(my).charAt(mx) == '1';
                pixels[y * px + x] = dark ? Color.BLACK : Color.WHITE;
            }
        }
        return Bitmap.createBitmap(pixels, px, px, Bitmap.Config.ARGB_8888);
    }

    private void buildViews() {
        LinearLayout col = new LinearLayout(this);
        col.setOrientation(LinearLayout.VERTICAL);
        col.setPadding(48, 64, 48, 64);
        col.setGravity(Gravity.CENTER_HORIZONTAL);
        title = text(col, "title", 24, true);
        shortCode = text(col, "short_code", 40, true);
        shortCode.setTypeface(Typeface.MONOSPACE, Typeface.BOLD);
        shortCode.setTextIsSelectable(true);
        message = text(col, "message", 16, false);
        qr = new ImageView(this);
        qr.setAdjustViewBounds(true);
        qr.setPadding(0, 32, 0, 32);
        col.addView(qr, new LinearLayout.LayoutParams(720, 720));
        longCode = text(col, "long_code", 12, false);
        longCode.setTypeface(Typeface.MONOSPACE);
        longCode.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                copy(longCode.getText().toString());
            }
        });
        renewButton = new Button(this);
        renewButton.setText("换一个加入码");
        renewButton.setContentDescription("renew");
        renewButton.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                renew = true;
                tick();
            }
        });
        col.addView(renewButton);
        devices = text(col, "devices", 16, false);
        devices.setGravity(Gravity.START);
        vpnButton = new Button(this);
        vpnButton.setText("启用 VPN");
        vpnButton.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                if (running) {
                    NexusVpnService.settings(MainActivity.this).edit()
                        .putBoolean(NexusVpnService.VPN_ENABLED, false).apply();
                    stopVpn();
                } else {
                    requestVpn();
                }
            }
        });
        col.addView(vpnButton);
        Button bypassButton = new Button(this);
        bypassButton.setText("绕过 VPN 的应用");
        bypassButton.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                chooseBypassApps();
            }
        });
        col.addView(bypassButton);
        Button appRoutesButton = new Button(this);
        appRoutesButton.setText("按应用选择出口");
        appRoutesButton.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                chooseAppRoutes();
            }
        });
        col.addView(appRoutesButton);
        Button routesButton = new Button(this);
        routesButton.setText("代理规则");
        routesButton.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                new RouteSettings(MainActivity.this, worker).show();
            }
        });
        col.addView(routesButton);
        Button diagnosticsButton = new Button(this);
        diagnosticsButton.setText("代理失败记录");
        diagnosticsButton.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                showProxyFailures();
            }
        });
        col.addView(diagnosticsButton);
        CheckBox powerSaving = new CheckBox(this);
        powerSaving.setText("按需保持蜂窝备用链路");
        powerSaving.setChecked(NexusVpnService.settings(this).getBoolean(NexusVpnService.ADAPTIVE_CELLULAR, false));
        powerSaving.setOnCheckedChangeListener(new CompoundButton.OnCheckedChangeListener() {
            @Override
            public void onCheckedChanged(CompoundButton button, boolean checked) {
                NexusVpnService.settings(MainActivity.this).edit()
                    .putBoolean(NexusVpnService.ADAPTIVE_CELLULAR, checked).apply();
                Toast.makeText(MainActivity.this, "下次启用 VPN 时生效", Toast.LENGTH_SHORT).show();
            }
        });
        col.addView(powerSaving);
        hideAll();
        show(title, "Nexus");
        ScrollView scroll = new ScrollView(this);
        scroll.addView(col);
        setContentView(scroll);
    }

    private Map<String, String> visibleApps(Set<String> saved) {
        Map<String, String> options = new TreeMap<>();
        for (String pkg : saved) {
            options.put(pkg, pkg);
        }
        PackageManager packages = getPackageManager();
        Intent launcher = new Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER);
        for (ResolveInfo app : packages.queryIntentActivities(launcher, 0)) {
            String pkg = app.activityInfo.packageName;
            if (!pkg.equals(getPackageName())) {
                options.put(pkg, app.loadLabel(packages) + "\n" + pkg);
            }
        }
        return options;
    }

    private void chooseBypassApps() {
        Set<String> saved = NexusVpnService.settings(this).getStringSet(
            NexusVpnService.BYPASS_APPS, Collections.<String>emptySet());
        Map<String, String> options = visibleApps(saved);
        final String[] names = options.keySet().toArray(new String[0]);
        final boolean[] selected = new boolean[names.length];
        String[] labels = new String[names.length];
        for (int i = 0; i < names.length; i++) {
            labels[i] = options.get(names[i]);
            selected[i] = saved.contains(names[i]);
        }
        new AlertDialog.Builder(this)
            .setTitle("绕过 VPN 的应用")
            .setMultiChoiceItems(labels, selected, new DialogInterface.OnMultiChoiceClickListener() {
                @Override
                public void onClick(DialogInterface dialog, int which, boolean checked) {
                    selected[which] = checked;
                }
            })
            .setNegativeButton("取消", null)
            .setPositiveButton("保存", new DialogInterface.OnClickListener() {
                @Override
                public void onClick(DialogInterface dialog, int which) {
                    Set<String> bypass = new HashSet<>();
                    for (int i = 0; i < names.length; i++) {
                        if (selected[i]) {
                            bypass.add(names[i]);
                        }
                    }
                    NexusVpnService.settings(MainActivity.this).edit()
                        .putStringSet(NexusVpnService.BYPASS_APPS, bypass).apply();
                    Toast.makeText(MainActivity.this, "下次启用 VPN 时生效", Toast.LENGTH_SHORT).show();
                }
            }).show();
    }

    private void chooseAppRoutes() {
        if (Build.VERSION.SDK_INT < 29) {
            Toast.makeText(this, "按应用选择出口需要 Android 10 或更高版本", Toast.LENGTH_LONG).show();
            return;
        }
        Map<String, String> routes = NexusVpnService.readAppRoutes(this);
        Map<String, String> apps = visibleApps(routes.keySet());
        final String[] packages = apps.keySet().toArray(new String[0]);
        String[] labels = new String[packages.length];
        for (int i = 0; i < packages.length; i++) {
            String name = packages[i];
            labels[i] = apps.get(name) + "\n" + routes.getOrDefault(name, "按域名规则");
        }
        new AlertDialog.Builder(this).setTitle("选择应用")
            .setItems(labels, new DialogInterface.OnClickListener() {
                @Override
                public void onClick(DialogInterface dialog, int which) {
                    editAppRoute(packages[which]);
                }
            }).setNegativeButton("关闭", null).show();
    }

    private void editAppRoute(final String pkg) {
        String[] choices = {"按域名规则", "强制走 Nexus 设备出口", "强制走 SOCKS5"};
        new AlertDialog.Builder(this).setTitle(pkg)
            .setItems(choices, new DialogInterface.OnClickListener() {
                @Override
                public void onClick(DialogInterface dialog, int which) {
                    if (which == 0) {
                        NexusVpnService.settings(MainActivity.this).edit()
                            .remove(NexusVpnService.APP_ROUTE_PREFIX + pkg).apply();
                        Toast.makeText(MainActivity.this, "下次启用 VPN 时生效", Toast.LENGTH_SHORT).show();
                        return;
                    }
                    Set<String> bypass = NexusVpnService.settings(MainActivity.this).getStringSet(
                        NexusVpnService.BYPASS_APPS, Collections.<String>emptySet());
                    if (bypass.contains(pkg)) {
                        Toast.makeText(MainActivity.this, "请先取消绕过此应用", Toast.LENGTH_LONG).show();
                        return;
                    }
                    promptAppExit(pkg, which == 1);
                }
            }).show();
    }

    private void promptAppExit(final String pkg, final boolean nexus) {
        final String prefix = nexus ? "exit:" : "socks5:";
        final EditText address = new EditText(this);
        address.setSingleLine(true);
        address.setHint(nexus ? "出口设备名" : "SOCKS5 主机:端口");
        String previous = NexusVpnService.readAppRoutes(this).getOrDefault(pkg, "");
        if (previous.startsWith(prefix)) {
            address.setText(previous.substring(prefix.length()));
        }
        new AlertDialog.Builder(this).setTitle(pkg).setView(address)
            .setMessage("启用按应用选出口时，VPN 接管未绕过应用的 IPv4 和 IPv6。无法确认应用归属的连接会关闭。")
            .setNegativeButton("取消", null)
            .setPositiveButton("保存", new DialogInterface.OnClickListener() {
                @Override
                public void onClick(DialogInterface dialog, int which) {
                    String value = address.getText().toString().trim();
                    if (value.isEmpty()) {
                        Toast.makeText(MainActivity.this, "请填写出口", Toast.LENGTH_SHORT).show();
                        return;
                    }
                    NexusVpnService.settings(MainActivity.this).edit()
                        .putString(NexusVpnService.APP_ROUTE_PREFIX + pkg, prefix + value).apply();
                    Toast.makeText(MainActivity.this, "下次启用 VPN 时生效", Toast.LENGTH_SHORT).show();
                }
            }).show();
    }

    private void showProxyFailures() {
        worker.execute(new Runnable() {
            @Override
            public void run() {
                final String failures = readProxyFailures();
                main.post(new Runnable() {
                    @Override
                    public void run() {
                        if (!isFinishing() && !isDestroyed()) {
                            new AlertDialog.Builder(MainActivity.this)
                                .setTitle("最近的代理失败")
                                .setMessage(failures)
                                .setPositiveButton("关闭", null)
                                .show();
                        }
                    }
                });
            }
        });
    }

    private String readProxyFailures() {
        File log = new File(getFilesDir(), "daemon.log");
        if (!log.isFile()) {
            return "暂无代理失败记录";
        }
        ArrayDeque<String> recent = new ArrayDeque<>();
        try (RandomAccessFile file = new RandomAccessFile(log, "r")) {
            long length = file.length();
            byte[] tail = new byte[(int) Math.min(65536, length)];
            file.seek(length - tail.length);
            file.readFully(tail);
            for (String line : new String(tail, StandardCharsets.UTF_8).split("\n")) {
                if (line.startsWith("nexus daemon: TCP ") || line.startsWith("nexus daemon: UDP ")) {
                    if (recent.size() == 8) {
                        recent.removeFirst();
                    }
                    recent.addLast(line.substring("nexus daemon: ".length()));
                }
            }
        } catch (IOException e) {
            return "无法读取代理失败记录：" + e.getMessage();
        }
        return recent.isEmpty() ? "暂无代理失败记录" : android.text.TextUtils.join("\n", recent);
    }

    private TextView text(LinearLayout parent, String tag, int sp, boolean bold) {
        TextView t = new TextView(this);
        t.setTextSize(sp);
        t.setGravity(Gravity.CENTER_HORIZONTAL);
        t.setPadding(0, 16, 0, 16);
        t.setContentDescription(tag);
        if (bold) {
            t.setTypeface(Typeface.DEFAULT_BOLD);
        }
        parent.addView(t);
        return t;
    }

    private void hideAll() {
        for (View v : new View[] {title, message, shortCode, qr, longCode, renewButton, devices}) {
            v.setVisibility(View.GONE);
        }
    }

    private static void show(TextView v, String text) {
        v.setText(text);
        v.setVisibility(View.VISIBLE);
    }

    private void copy(String text) {
        ClipboardManager cm = getSystemService(ClipboardManager.class);
        cm.setPrimaryClip(ClipData.newPlainText("Nexus 加入码", text));
        Toast.makeText(this, "已复制长加入码", Toast.LENGTH_SHORT).show();
    }
}
