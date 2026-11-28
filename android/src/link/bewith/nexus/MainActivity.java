package link.bewith.nexus;

import android.app.Activity;
import android.app.AlertDialog;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.DialogInterface;
import android.content.Intent;
import android.content.pm.ApplicationInfo;
import android.content.pm.PackageManager;
import android.content.pm.ResolveInfo;
import android.os.Build;
import android.graphics.Bitmap;
import android.graphics.Color;
import android.graphics.Typeface;
import android.graphics.drawable.GradientDrawable;
import android.net.VpnService;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.text.Editable;
import android.text.TextWatcher;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.widget.Button;
import android.widget.CheckBox;
import android.widget.CompoundButton;
import android.widget.EditText;
import android.widget.FrameLayout;
import android.widget.ImageView;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;
import android.widget.Toast;
import java.io.File;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Comparator;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.Set;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import org.json.JSONArray;
import org.json.JSONException;
import org.json.JSONObject;

/**
 * Nexus Next Android 客户端（N1 / N2 完整操作闭环）：
 * 底部 4 标签页：
 *   1. 首页：VPN 启停闭环、电源模式（平衡省电/极致省电/实时多路径）、加入网络（短码/二维码/长码）、
 *            已加入设备列表、以及「退出并重新加入其他网络」。
 *   2. 应用分流：列出已安装应用，勾选绕过 VPN，保存后经 Rust 防环路校验并热重建 VPN 生效。
 *   3. 规则：查看当前生效规则、导入 Clash/Mihomo 规则子集（先预检审计再确认导入）、一键回滚（LKG）。
 *   4. 诊断：路由去向解释（route explain）、规则静态审计（route audit）、核心日志查看。
 *
 * 兼容自动化测试：通过 Intent 传入 name/relay/discovery/power_mode/action=stop 时保留既有自动启停行为。
 */
public class MainActivity extends Activity {
    private static final int REQUEST_VPN = 1;
    private static final long REFRESH_MS = 2000;

    private static final int COLOR_BG = 0xFFF4F6F9;
    private static final int COLOR_CARD = 0xFFFFFFFF;
    private static final int COLOR_PRIMARY = 0xFF1A73E8;
    private static final int COLOR_DANGER = 0xFFD93025;
    private static final int COLOR_SUCCESS = 0xFF188038;
    private static final int COLOR_MUTED = 0xFF5F6368;
    private static final int COLOR_TEXT = 0xFF202124;
    private static final int COLOR_BORDER = 0xFFDADCE0;

    private final Handler main = new Handler(Looper.getMainLooper());
    private final ExecutorService worker = Executors.newSingleThreadExecutor();
    private String held = "";
    private boolean renew;
    private boolean busy;
    private int currentTab = 0;
    private String powerMode = "balanced";
    private Bundle launchExtras;

    private final Runnable ticker = new Runnable() {
        @Override
        public void run() {
            tick();
        }
    };

    // 底部导航与页面容器
    private View[] tabPages = new View[4];
    private Button[] tabButtons = new Button[4];

    // Tab 0：首页控件
    private TextView vpnBadge;
    private TextView vpnSubtext;
    private Button vpnToggleButton;
    private Button btnModeBalanced;
    private Button btnModePowerSave;
    private Button btnModeRealtime;
    private TextView modeHint;
    private TextView title;
    private TextView message;
    private TextView shortCode;
    private ImageView qr;
    private TextView longCode;
    private Button renewButton;
    private TextView devices;
    private Button switchNetworkButton;

    // Tab 1：应用分流控件
    private EditText appSearchInput;
    private CheckBox showSystemAppsCheck;
    private LinearLayout appListContainer;
    private TextView appSummaryText;
    private final List<AppEntry> allApps = new ArrayList<>();
    private final Set<String> selectedBypass = new HashSet<>();
    private boolean appsLoaded = false;

    // Tab 2：规则管理控件
    private TextView rulesCountText;
    private LinearLayout rulesListContainer;
    private EditText importViaInput;
    private EditText importSocks5Input;
    private EditText importYamlInput;
    private TextView importReportText;

    // Tab 3：诊断控件
    private EditText explainInput;
    private TextView explainResultText;
    private TextView auditResultText;
    private TextView flowEventsText;
    private TextView daemonLogText;

    private static final class AppEntry {
        final String label;
        final String pkg;
        final boolean isSystem;

        AppEntry(String label, String pkg, boolean isSystem) {
            this.label = label;
            this.pkg = pkg;
            this.isSystem = isSystem;
        }
    }

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        loadSavedPowerMode();
        buildRootUi();
        handleIncomingIntent(getIntent(), true);
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        handleIncomingIntent(intent, false);
    }

    private void handleIncomingIntent(Intent intent, boolean isInitialCreate) {
        if (intent == null) {
            updateVpnCardUi();
            return;
        }
        if ("stop".equals(intent.getStringExtra("action"))) {
            stopVpn();
            return;
        }
        String extraMode = intent.getStringExtra("power_mode");
        if (extraMode != null && !extraMode.isEmpty()) {
            setPowerMode(extraMode.trim().toLowerCase());
        } else if (intent.hasExtra("multipath")) {
            setPowerMode(intent.getBooleanExtra("multipath", false) ? "realtime" : "balanced");
        }
        if (intent.getExtras() != null) {
            launchExtras = new Bundle(intent.getExtras());
        }
        // 自动化脚本带参数拉起（name/relay/discovery/rendezvous）时自动启动 VPN；
        // 普通用户点图标启动时停留在首页，由用户主动控制启停（如果 VPN 已在运行则直接同步状态）。
        boolean hasAutoStartExtra = intent.hasExtra("name")
            || intent.hasExtra("relay")
            || intent.hasExtra("discovery")
            || intent.hasExtra("rendezvous");
        if (isInitialCreate && hasAutoStartExtra) {
            requestStartVpn();
        } else {
            updateVpnCardUi();
            if (NexusVpnService.isVpnRunning()) {
                tick();
            }
        }
    }

    private void loadSavedPowerMode() {
        String saved = NexusVpnService.readSmallFile(new File(getFilesDir(), NexusVpnService.POWER_MODE_FILE)).trim();
        if (!saved.isEmpty()) {
            powerMode = saved;
        } else {
            powerMode = NexusVpnService.getActivePowerMode();
        }
    }

    private void requestStartVpn() {
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
            show(message, "Nexus 通过系统 VPN 接入个人网络与分流规则。请点击上方「启动 VPN」并同意授权。");
            updateVpnCardUi();
            return;
        }
        Intent start = new Intent(this, NexusVpnService.class).setAction(NexusVpnService.ACTION_START);
        if (launchExtras != null) {
            start.putExtras(launchExtras);
        }
        start.putExtra("power_mode", powerMode);
        startForegroundService(start);
        updateVpnCardUi();
        main.postDelayed(ticker, 400);
    }

    @Override
    protected void onResume() {
        super.onResume();
        updateVpnCardUi();
        if (NexusVpnService.isVpnRunning()) {
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
        main.removeCallbacks(ticker);
        worker.shutdownNow();
    }

    private void stopVpn() {
        main.removeCallbacks(ticker);
        startService(new Intent(this, NexusVpnService.class).setAction(NexusVpnService.ACTION_STOP));
        held = "";
        hideNetworkViews();
        show(title, "VPN 已停止");
        show(message, "系统已恢复原有网络。随时点击上方「启动 VPN」重新连接，或切换到「应用分流」调整绕过名单。");
        updateVpnCardUi();
    }

    private void setPowerMode(String mode) {
        if (!"powersave".equals(mode) && !"realtime".equals(mode)) {
            mode = "balanced";
        }
        powerMode = mode;
        NexusVpnService.writeSmallFile(new File(getFilesDir(), NexusVpnService.POWER_MODE_FILE), powerMode);
        if (NexusVpnService.isVpnRunning()) {
            Intent intent = new Intent(this, NexusVpnService.class)
                .setAction(NexusVpnService.ACTION_POWER_MODE)
                .putExtra("power_mode", powerMode);
            startService(intent);
        }
        updatePowerModeUi();
    }

    private void confirmSwitchNetwork() {
        new AlertDialog.Builder(this)
            .setTitle("重新加入其他网络？")
            .setMessage("这将退出当前网络、清除本机名册与设备密钥（保留你的应用绕过名单与电源模式），随后生成全新的加入码和二维码。")
            .setNegativeButton("取消", null)
            .setPositiveButton("确认重置并重新加入", new DialogInterface.OnClickListener() {
                @Override
                public void onClick(DialogInterface dialog, int which) {
                    resetAndRejoinNetwork();
                }
            })
            .show();
    }

    private void resetAndRejoinNetwork() {
        final boolean wasRunning = NexusVpnService.isVpnRunning();
        main.removeCallbacks(ticker);
        if (wasRunning) {
            startService(new Intent(this, NexusVpnService.class).setAction(NexusVpnService.ACTION_STOP));
        }
        held = "";
        hideNetworkViews();
        show(title, "正在重置网络身份…");
        worker.execute(new Runnable() {
            @Override
            public void run() {
                // 等待核心完全停止并释放文件
                try {
                    Thread.sleep(800);
                } catch (InterruptedException ignored) {
                    Thread.currentThread().interrupt();
                }
                File dir = getFilesDir();
                File[] files = dir.listFiles();
                if (files != null) {
                    for (File f : files) {
                        String n = f.getName();
                        // 保留用户偏好：应用绕过名单、电源模式
                        if (NexusVpnService.BYPASS_FILE.equals(n) || NexusVpnService.POWER_MODE_FILE.equals(n)) {
                            continue;
                        }
                        deleteRecursive(f);
                    }
                }
                main.post(new Runnable() {
                    @Override
                    public void run() {
                        Toast.makeText(MainActivity.this, "已清除旧网络身份，正在生成新加入码…", Toast.LENGTH_SHORT).show();
                        requestStartVpn();
                    }
                });
            }
        });
    }

    private static void deleteRecursive(File f) {
        if (f.isDirectory()) {
            File[] children = f.listFiles();
            if (children != null) {
                for (File c : children) {
                    deleteRecursive(c);
                }
            }
        }
        f.delete();
    }

    private void tick() {
        main.removeCallbacks(ticker);
        updateVpnCardUi();
        if (!NexusVpnService.isVpnRunning()) {
            return;
        }
        if (busy) {
            return;
        }
        busy = true;
        final String dir = getFilesDir().getAbsolutePath();
        final String h = held;
        final boolean r = renew;
        renew = false;
        worker.execute(new Runnable() {
            @Override
            public void run() {
                final String json = NexusCore.nativeScreen(dir, h, r, System.currentTimeMillis());
                main.post(new Runnable() {
                    @Override
                    public void run() {
                        busy = false;
                        updateVpnCardUi();
                        if (!NexusVpnService.isVpnRunning()) {
                            return;
                        }
                        renderNetworkState(json);
                        if (!isFinishing() && !isDestroyed()) {
                            main.postDelayed(ticker, REFRESH_MS);
                        }
                    }
                });
            }
        });
    }

    private void updateVpnCardUi() {
        boolean running = NexusVpnService.isVpnRunning();
        if (running) {
            vpnBadge.setText("● VPN 运行中");
            vpnBadge.setTextColor(COLOR_SUCCESS);
            vpnSubtext.setText("虚拟网段 198.18.0.0/16 · 本地 DNS 198.18.0.2");
            vpnToggleButton.setText("停止 VPN（快速回退）");
            styleButton(vpnToggleButton, COLOR_DANGER, Color.WHITE);
        } else {
            vpnBadge.setText("○ VPN 未启动");
            vpnBadge.setTextColor(COLOR_MUTED);
            vpnSubtext.setText("当前直连系统原网络，未接管任何流量");
            vpnToggleButton.setText("启动 VPN");
            styleButton(vpnToggleButton, COLOR_PRIMARY, Color.WHITE);
        }
        updatePowerModeUi();
    }

    private void updatePowerModeUi() {
        styleSegment(btnModeBalanced, "balanced".equals(powerMode));
        styleSegment(btnModePowerSave, "powersave".equals(powerMode));
        styleSegment(btnModeRealtime, "realtime".equals(powerMode));
        if ("realtime".equals(powerMode)) {
            modeHint.setText("实时多路径：在 Wi-Fi 下同时保持蜂窝在线，优先低延迟与无缝切换（耗电较高）。");
        } else if ("powersave".equals(powerMode)) {
            modeHint.setText("极致省电：不主动保持蜂窝，拉长后台探测间隔，最大化待机续航。");
        } else {
            modeHint.setText("平衡省电（默认）：有 Wi-Fi 时不主动拉起蜂窝电台，兼顾连通性与续航。");
        }
    }

    private void renderNetworkState(String json) {
        JSONObject s;
        try {
            s = new JSONObject(json == null ? "" : json);
        } catch (JSONException e) {
            s = new JSONObject();
        }
        String state = s.optString("state", "failed");
        hideNetworkViews();
        switch (state) {
            case "starting":
                show(title, "正在启动 Nexus…");
                break;
            case "joined":
                held = "";
                show(title, "已加入网络");
                show(message, "本机：" + s.optString("name"));
                show(devices, deviceList(s.optJSONArray("devices")));
                switchNetworkButton.setVisibility(View.VISIBLE);
                break;
            case "joining":
                held = json;
                renderJoining(s);
                break;
            default:
                show(title, "核心状态异常");
                show(message, s.optString("message", "核心的回应无法理解"));
        }
    }

    private void renderJoining(JSONObject s) {
        show(title, "加入个人网络");
        String shortText = s.isNull("short_code") ? null : s.optString("short_code");
        long left = Math.max(0, (s.optLong("expires_at_ms") - System.currentTimeMillis()) / 60000);
        if (shortText != null) {
            show(shortCode, shortText);
            show(message, "在管理设备上运行：\nnexus device approve " + shortText
                + "\n或扫描下方二维码。约 " + (left + 1) + " 分钟内有效，到期前自动换新。");
        } else {
            show(message, "离线/自建模式：扫描下方二维码，或点击下方长加入码复制后在管理设备执行 nexus device approve <加入码>。约 "
                + (left + 1) + " 分钟内有效。");
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

    private static Bitmap qrBitmap(JSONArray rows) {
        int n = rows == null ? 0 : rows.length();
        int margin = 4, scale = 6, px = (n + 2 * margin) * scale;
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

    // ==================== 界面构建 ====================

    private void buildRootUi() {
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setBackgroundColor(COLOR_BG);

        // 顶部标题栏
        LinearLayout topBar = new LinearLayout(this);
        topBar.setBackgroundColor(COLOR_CARD);
        topBar.setPadding(36, 36, 36, 24);
        topBar.setOrientation(LinearLayout.HORIZONTAL);
        TextView brand = new TextView(this);
        brand.setText("Nexus Next");
        brand.setTextSize(20);
        brand.setTextColor(COLOR_TEXT);
        brand.setTypeface(Typeface.DEFAULT_BOLD);
        topBar.addView(brand);
        root.addView(topBar, new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));

        // 中部内容容器（4 个 Tab 页）
        FrameLayout content = new FrameLayout(this);
        tabPages[0] = buildHomeTab();
        tabPages[1] = buildAppsTab();
        tabPages[2] = buildRulesTab();
        tabPages[3] = buildDiagTab();
        for (int i = 0; i < 4; i++) {
            content.addView(tabPages[i], new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT));
        }
        LinearLayout.LayoutParams contentLp = new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f);
        root.addView(content, contentLp);

        // 底部导航栏
        LinearLayout bottomNav = new LinearLayout(this);
        bottomNav.setOrientation(LinearLayout.HORIZONTAL);
        bottomNav.setBackgroundColor(COLOR_CARD);
        bottomNav.setPadding(8, 12, 8, 12);
        String[] labels = new String[] {"首页", "应用分流", "规则", "诊断"};
        for (int i = 0; i < 4; i++) {
            final int tabIdx = i;
            Button b = new Button(this);
            b.setText(labels[i]);
            b.setTextSize(13);
            b.setAllCaps(false);
            b.setContentDescription("tab_" + i);
            b.setOnClickListener(new View.OnClickListener() {
                @Override
                public void onClick(View v) {
                    switchTab(tabIdx);
                }
            });
            tabButtons[i] = b;
            LinearLayout.LayoutParams blp = new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f);
            blp.setMargins(6, 0, 6, 0);
            bottomNav.addView(b, blp);
        }
        root.addView(bottomNav, new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));

        setContentView(root);
        switchTab(0);
    }

    private void switchTab(int idx) {
        currentTab = idx;
        for (int i = 0; i < 4; i++) {
            tabPages[i].setVisibility(i == idx ? View.VISIBLE : View.GONE);
            styleSegment(tabButtons[i], i == idx);
        }
        if (idx == 1 && !appsLoaded) {
            loadInstalledApps();
        } else if (idx == 2) {
            refreshRulesList();
        } else if (idx == 3) {
            refreshFlowEvents();
            refreshDaemonLog();
        }
    }

    private View buildHomeTab() {
        ScrollView scroll = new ScrollView(this);
        LinearLayout col = new LinearLayout(this);
        col.setOrientation(LinearLayout.VERTICAL);
        col.setPadding(24, 24, 24, 32);

        // 卡片 1：VPN 状态与启停
        LinearLayout vpnCard = makeCard(col);
        vpnBadge = makeText(vpnCard, "vpn_badge", 18, true, COLOR_MUTED);
        vpnSubtext = makeText(vpnCard, "vpn_subtext", 13, false, COLOR_MUTED);
        vpnToggleButton = new Button(this);
        vpnToggleButton.setAllCaps(false);
        vpnToggleButton.setContentDescription("vpn_toggle");
        vpnToggleButton.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                if (NexusVpnService.isVpnRunning()) {
                    stopVpn();
                } else {
                    requestStartVpn();
                }
            }
        });
        LinearLayout.LayoutParams btnLp = new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT);
        btnLp.topMargin = 16;
        vpnCard.addView(vpnToggleButton, btnLp);

        // 卡片 2：电源模式（N2）
        LinearLayout powerCard = makeCard(col);
        makeText(powerCard, "power_title", 15, true, COLOR_TEXT).setText("电源与多路径模式");
        LinearLayout modeRow = new LinearLayout(this);
        modeRow.setOrientation(LinearLayout.HORIZONTAL);
        LinearLayout.LayoutParams rowLp = new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT);
        rowLp.topMargin = 12;
        btnModeBalanced = makeModeButton(modeRow, "平衡省电", "balanced");
        btnModePowerSave = makeModeButton(modeRow, "极致省电", "powersave");
        btnModeRealtime = makeModeButton(modeRow, "实时多路径", "realtime");
        powerCard.addView(modeRow, rowLp);
        modeHint = makeText(powerCard, "mode_hint", 12, false, COLOR_MUTED);
        modeHint.setPadding(0, 12, 0, 0);

        // 卡片 3：个人网络（加入 / 设备列表 / 重新加入）
        LinearLayout netCard = makeCard(col);
        title = makeText(netCard, "title", 17, true, COLOR_TEXT);
        shortCode = makeText(netCard, "short_code", 32, true, COLOR_PRIMARY);
        shortCode.setTypeface(Typeface.MONOSPACE, Typeface.BOLD);
        shortCode.setTextIsSelectable(true);
        message = makeText(netCard, "message", 14, false, COLOR_MUTED);
        qr = new ImageView(this);
        qr.setAdjustViewBounds(true);
        qr.setPadding(0, 16, 0, 16);
        LinearLayout.LayoutParams qrLp = new LinearLayout.LayoutParams(480, 480);
        qrLp.gravity = Gravity.CENTER_HORIZONTAL;
        netCard.addView(qr, qrLp);
        longCode = makeText(netCard, "long_code", 11, false, COLOR_TEXT);
        longCode.setTypeface(Typeface.MONOSPACE);
        longCode.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                copyText(longCode.getText().toString(), "已复制长加入码");
            }
        });
        renewButton = new Button(this);
        renewButton.setText("换一个加入码");
        renewButton.setAllCaps(false);
        renewButton.setContentDescription("renew");
        styleButton(renewButton, COLOR_BG, COLOR_TEXT);
        renewButton.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                renew = true;
                tick();
            }
        });
        netCard.addView(renewButton, btnLp);
        devices = makeText(netCard, "devices", 14, false, COLOR_TEXT);
        switchNetworkButton = new Button(this);
        switchNetworkButton.setText("退出并重新加入其他网络");
        switchNetworkButton.setAllCaps(false);
        switchNetworkButton.setContentDescription("switch_network");
        styleButton(switchNetworkButton, 0xFFFCE8E6, COLOR_DANGER);
        switchNetworkButton.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                confirmSwitchNetwork();
            }
        });
        netCard.addView(switchNetworkButton, btnLp);

        hideNetworkViews();
        show(title, "个人网络");
        show(message, "点击上方「启动 VPN」后，即可查看本机加入码或已加入的个人设备。");

        scroll.addView(col);
        return scroll;
    }

    private Button makeModeButton(LinearLayout parent, String label, final String mode) {
        Button b = new Button(this);
        b.setText(label);
        b.setTextSize(12);
        b.setAllCaps(false);
        b.setContentDescription("mode_" + mode);
        b.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                setPowerMode(mode);
            }
        });
        LinearLayout.LayoutParams lp = new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f);
        lp.setMargins(4, 0, 4, 0);
        parent.addView(b, lp);
        return b;
    }

    // ==================== Tab 1：应用分流 ====================

    private View buildAppsTab() {
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setPadding(24, 24, 24, 16);

        LinearLayout headerCard = makeCard(root);
        makeText(headerCard, "apps_title", 16, true, COLOR_TEXT).setText("应用绕过 VPN（直连原网络）");
        makeText(headerCard, "apps_desc", 12, false, COLOR_MUTED)
            .setText("勾选的应用将不经过 Nexus VPN（也不会获得 .internal 私有解析）；Nexus 自身始终强制排除防环路。");
        appSummaryText = makeText(headerCard, "apps_summary", 13, true, COLOR_PRIMARY);

        appSearchInput = new EditText(this);
        appSearchInput.setHint("搜索应用名或包名…");
        appSearchInput.setSingleLine(true);
        appSearchInput.addTextChangedListener(new TextWatcher() {
            @Override public void beforeTextChanged(CharSequence s, int start, int count, int after) {}
            @Override public void onTextChanged(CharSequence s, int start, int before, int count) { renderAppList(); }
            @Override public void afterTextChanged(Editable s) {}
        });
        headerCard.addView(appSearchInput);

        LinearLayout optRow = new LinearLayout(this);
        optRow.setOrientation(LinearLayout.HORIZONTAL);
        optRow.setGravity(Gravity.CENTER_VERTICAL);
        showSystemAppsCheck = new CheckBox(this);
        showSystemAppsCheck.setText("显示系统应用");
        showSystemAppsCheck.setOnCheckedChangeListener(new CompoundButton.OnCheckedChangeListener() {
            @Override
            public void onCheckedChanged(CompoundButton buttonView, boolean isChecked) {
                renderAppList();
            }
        });
        optRow.addView(showSystemAppsCheck, new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f));

        Button saveAppsBtn = new Button(this);
        saveAppsBtn.setText("保存并生效");
        saveAppsBtn.setAllCaps(false);
        styleButton(saveAppsBtn, COLOR_PRIMARY, Color.WHITE);
        saveAppsBtn.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                saveBypassApps();
            }
        });
        optRow.addView(saveAppsBtn);
        headerCard.addView(optRow);

        // 按应用选出口（R112）：指定应用的全部流量走某台设备的出口或某个 SOCKS5 上游。
        Button appRoutesBtn = new Button(this);
        appRoutesBtn.setText("按应用选择出口");
        appRoutesBtn.setAllCaps(false);
        styleButton(appRoutesBtn, COLOR_BG, COLOR_TEXT);
        appRoutesBtn.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                chooseAppRoutes();
            }
        });
        headerCard.addView(appRoutesBtn);

        ScrollView scroll = new ScrollView(this);
        appListContainer = new LinearLayout(this);
        appListContainer.setOrientation(LinearLayout.VERTICAL);
        scroll.addView(appListContainer);
        root.addView(scroll, new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f));
        return root;
    }

    private void loadInstalledApps() {
        appsLoaded = true;
        appSummaryText.setText("正在读取已安装应用…");
        worker.execute(new Runnable() {
            @Override
            public void run() {
                String raw = NexusVpnService.readSmallFile(new File(getFilesDir(), NexusVpnService.BYPASS_FILE));
                final Set<String> saved = new HashSet<>();
                for (String line : raw.split("\n")) {
                    String t = line.trim();
                    if (!t.isEmpty() && !t.startsWith("#")) {
                        saved.add(t);
                    }
                }
                PackageManager pm = getPackageManager();
                List<ApplicationInfo> installed = pm.getInstalledApplications(0);
                final List<AppEntry> entries = new ArrayList<>();
                for (ApplicationInfo ai : installed) {
                    if (getPackageName().equals(ai.packageName)) {
                        continue;
                    }
                    CharSequence cs = pm.getApplicationLabel(ai);
                    String label = cs == null ? ai.packageName : cs.toString();
                    boolean isSys = (ai.flags & ApplicationInfo.FLAG_SYSTEM) != 0;
                    entries.add(new AppEntry(label, ai.packageName, isSys));
                }
                Collections.sort(entries, new Comparator<AppEntry>() {
                    @Override
                    public int compare(AppEntry a, AppEntry b) {
                        boolean aSel = saved.contains(a.pkg);
                        boolean bSel = saved.contains(b.pkg);
                        if (aSel != bSel) {
                            return aSel ? -1 : 1;
                        }
                        return a.label.compareToIgnoreCase(b.label);
                    }
                });
                main.post(new Runnable() {
                    @Override
                    public void run() {
                        selectedBypass.clear();
                        selectedBypass.addAll(saved);
                        allApps.clear();
                        allApps.addAll(entries);
                        renderAppList();
                    }
                });
            }
        });
    }

    private void renderAppList() {
        appListContainer.removeAllViews();
        String q = appSearchInput.getText().toString().trim().toLowerCase();
        boolean showSys = showSystemAppsCheck.isChecked();
        appSummaryText.setText("已勾选绕过应用：" + selectedBypass.size() + " 个（含 Nexus 自身防环路）");
        int shown = 0;
        for (final AppEntry app : allApps) {
            if (!showSys && app.isSystem && !selectedBypass.contains(app.pkg)) {
                continue;
            }
            if (!q.isEmpty() && !app.label.toLowerCase().contains(q) && !app.pkg.toLowerCase().contains(q)) {
                continue;
            }
            CheckBox cb = new CheckBox(this);
            cb.setText(app.label + "\n" + app.pkg);
            cb.setTextSize(13);
            cb.setPadding(12, 16, 12, 16);
            cb.setChecked(selectedBypass.contains(app.pkg));
            cb.setOnCheckedChangeListener(new CompoundButton.OnCheckedChangeListener() {
                @Override
                public void onCheckedChanged(CompoundButton buttonView, boolean isChecked) {
                    if (isChecked) {
                        selectedBypass.add(app.pkg);
                    } else {
                        selectedBypass.remove(app.pkg);
                    }
                    appSummaryText.setText("已勾选绕过应用：" + selectedBypass.size() + " 个（含 Nexus 自身防环路）");
                }
            });
            appListContainer.addView(cb);
            shown++;
            if (shown >= 120) {
                break;
            }
        }
        if (shown == 0) {
            makeText(appListContainer, "apps_empty", 13, false, COLOR_MUTED).setText("没有匹配的应用；可勾选「显示系统应用」。");
        }
    }

    private void chooseAppRoutes() {
        if (Build.VERSION.SDK_INT < 29) {
            Toast.makeText(this, "按应用选择出口需要 Android 10 或更高版本", Toast.LENGTH_LONG).show();
            return;
        }
        Map<String, String> routes = NexusVpnService.readAppRoutes(this);
        Map<String, String> apps = launchableApps(routes.keySet());
        final String[] packages = apps.keySet().toArray(new String[0]);
        String[] labels = new String[packages.length];
        for (int i = 0; i < packages.length; i++) {
            String name = packages[i];
            String route = routes.get(name);
            labels[i] = apps.get(name) + "\n" + (route == null ? "按域名规则" : route);
        }
        new AlertDialog.Builder(this).setTitle("按应用选择出口")
            .setItems(labels, new DialogInterface.OnClickListener() {
                @Override
                public void onClick(DialogInterface dialog, int which) {
                    editAppRoute(packages[which]);
                }
            }).setNegativeButton("关闭", null).show();
    }

    /** 能从桌面打开的应用，以及已经设了出口的应用（即使不在桌面上）。 */
    private Map<String, String> launchableApps(Set<String> saved) {
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

    private void editAppRoute(final String pkg) {
        String[] choices = {"按域名规则", "强制走 Nexus 设备出口", "强制走 SOCKS5"};
        new AlertDialog.Builder(this).setTitle(pkg)
            .setItems(choices, new DialogInterface.OnClickListener() {
                @Override
                public void onClick(DialogInterface dialog, int which) {
                    if (which == 0) {
                        NexusVpnService.settings(MainActivity.this).edit()
                            .remove(NexusVpnService.APP_ROUTE_PREFIX + pkg).apply();
                        rebuildVpnIfRunning();
                        return;
                    }
                    if (selectedBypass.contains(pkg)) {
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
        String previous = NexusVpnService.readAppRoutes(this).get(pkg);
        if (previous != null && previous.startsWith(prefix)) {
            address.setText(previous.substring(prefix.length()));
        }
        new AlertDialog.Builder(this).setTitle(pkg).setView(address)
            .setMessage("有应用指定了出口时，VPN 接管所有未绕过应用的 IPv4 和 IPv6，按连接所属的应用分流；无法确认归属的连接会关闭。")
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
                    rebuildVpnIfRunning();
                }
            }).show();
    }

    private void rebuildVpnIfRunning() {
        if (NexusVpnService.isVpnRunning()) {
            startService(new Intent(this, NexusVpnService.class).setAction(NexusVpnService.ACTION_REBUILD));
            Toast.makeText(this, "已保存，VPN 正在按新设置重建", Toast.LENGTH_SHORT).show();
        } else {
            Toast.makeText(this, "已保存，下次启动 VPN 时生效", Toast.LENGTH_SHORT).show();
        }
    }

    private void saveBypassApps() {
        StringBuilder sb = new StringBuilder();
        for (String pkg : selectedBypass) {
            sb.append(pkg).append("\n");
        }
        String effective = NexusCore.nativeEffectiveBypass(getPackageName(), sb.toString());
        NexusVpnService.writeSmallFile(new File(getFilesDir(), NexusVpnService.BYPASS_FILE), sb.toString());
        int totalExcluded = (effective == null || effective.isEmpty()) ? 1 : effective.split("\n").length;
        if (NexusVpnService.isVpnRunning()) {
            startService(new Intent(this, NexusVpnService.class).setAction(NexusVpnService.ACTION_REBUILD));
            Toast.makeText(this, "已保存并热重建 VPN（生效排除 " + totalExcluded + " 个应用）", Toast.LENGTH_SHORT).show();
        } else {
            Toast.makeText(this, "已保存（下次启动 VPN 时生效，共排除 " + totalExcluded + " 个应用）", Toast.LENGTH_SHORT).show();
        }
    }

    // ==================== Tab 2：规则导入与回滚 ====================

    private View buildRulesTab() {
        ScrollView scroll = new ScrollView(this);
        LinearLayout col = new LinearLayout(this);
        col.setOrientation(LinearLayout.VERTICAL);
        col.setPadding(24, 24, 24, 32);

        // 导入 Clash 规则卡
        LinearLayout impCard = makeCard(col);
        makeText(impCard, "imp_title", 16, true, COLOR_TEXT).setText("导入 Clash / Mihomo 规则子集");
        makeText(impCard, "imp_desc", 12, false, COLOR_MUTED)
            .setText("支持 DOMAIN、DOMAIN-SUFFIX、IP-CIDR、IP-CIDR6；自动拦截并报告 SCRIPT/MITM 等不支持项，导入前自动保存 LKG 快照。");

        importViaInput = new EditText(this);
        importViaInput.setHint("代理组默认出口设备名（例如 home，留空则用下方 SOCKS5）");
        importViaInput.setText("home");
        importViaInput.setSingleLine(true);
        impCard.addView(importViaInput);

        importSocks5Input = new EditText(this);
        importSocks5Input.setHint("或上游 SOCKS5 地址（例如 127.0.0.1:7890）");
        importSocks5Input.setSingleLine(true);
        impCard.addView(importSocks5Input);

        importYamlInput = new EditText(this);
        importYamlInput.setHint("在此粘贴 Clash YAML rules: 片段…");
        importYamlInput.setMinLines(4);
        importYamlInput.setTypeface(Typeface.MONOSPACE);
        importYamlInput.setTextSize(12);
        impCard.addView(importYamlInput);

        LinearLayout btnRow1 = new LinearLayout(this);
        btnRow1.setOrientation(LinearLayout.HORIZONTAL);
        Button sampleBtn = makeActionBtn(btnRow1, "填入示例", COLOR_BG, COLOR_TEXT, new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                importYamlInput.setText(
                    "rules:\n"
                    + "  - DOMAIN,claude.ai,PROXY\n"
                    + "  - DOMAIN-SUFFIX,ads.example.com,REJECT\n"
                    + "  - DOMAIN-SUFFIX,cn,DIRECT\n"
                    + "  - IP-CIDR,192.168.0.0/16,DIRECT,no-resolve\n"
                    + "  - SCRIPT,demo,PROXY\n"
                );
            }
        });
        Button dryBtn = makeActionBtn(btnRow1, "预检审计", 0xFFE8F0FE, COLOR_PRIMARY, new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                runRuleImport(true);
            }
        });
        impCard.addView(btnRow1);

        LinearLayout btnRow2 = new LinearLayout(this);
        btnRow2.setOrientation(LinearLayout.HORIZONTAL);
        makeActionBtn(btnRow2, "确认导入（替换）", COLOR_PRIMARY, Color.WHITE, new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                runRuleImport(false);
            }
        });
        makeActionBtn(btnRow2, "一键回滚 (LKG)", 0xFFFCE8E6, COLOR_DANGER, new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                runRuleRollback();
            }
        });
        impCard.addView(btnRow2);

        importReportText = makeText(impCard, "import_report", 12, false, COLOR_TEXT);
        importReportText.setTypeface(Typeface.MONOSPACE);

        // 当前生效规则列表卡
        LinearLayout listCard = makeCard(col);
        LinearLayout listHead = new LinearLayout(this);
        listHead.setOrientation(LinearLayout.HORIZONTAL);
        listHead.setGravity(Gravity.CENTER_VERTICAL);
        rulesCountText = makeText(listHead, "rules_count", 15, true, COLOR_TEXT);
        rulesCountText.setText("当前生效规则");
        ((LinearLayout.LayoutParams) rulesCountText.getLayoutParams()).weight = 1f;
        Button refreshRulesBtn = new Button(this);
        refreshRulesBtn.setText("刷新");
        refreshRulesBtn.setAllCaps(false);
        styleButton(refreshRulesBtn, COLOR_BG, COLOR_TEXT);
        refreshRulesBtn.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                refreshRulesList();
            }
        });
        listHead.addView(refreshRulesBtn);
        listCard.addView(listHead);

        rulesListContainer = new LinearLayout(this);
        rulesListContainer.setOrientation(LinearLayout.VERTICAL);
        listCard.addView(rulesListContainer);

        scroll.addView(col);
        return scroll;
    }

    private void runRuleImport(final boolean dryRun) {
        final String yaml = importYamlInput.getText().toString();
        final String via = importViaInput.getText().toString().trim();
        final String socks5 = importSocks5Input.getText().toString().trim();
        final String dir = getFilesDir().getAbsolutePath();
        importReportText.setText(dryRun ? "正在预检审计…" : "正在原子导入并保存 LKG 快照…");
        worker.execute(new Runnable() {
            @Override
            public void run() {
                final String raw = NexusCore.nativeRouteImport(dir, yaml, via, socks5, true, dryRun);
                main.post(new Runnable() {
                    @Override
                    public void run() {
                        renderImportReport(raw, dryRun);
                        if (!dryRun) {
                            refreshRulesList();
                        }
                    }
                });
            }
        });
    }

    private void renderImportReport(String raw, boolean dryRun) {
        try {
            JSONObject obj = new JSONObject(raw);
            if (!obj.optBoolean("ok")) {
                importReportText.setText("失败：" + obj.optString("error"));
                return;
            }
            StringBuilder sb = new StringBuilder();
            if (dryRun) {
                sb.append("【预检结果】可导入 ").append(obj.optInt("imported_count")).append(" 条");
            } else {
                sb.append("【导入成功】当前生效共 ").append(obj.optInt("total")).append(" 条（旧规则已保存为 LKG 快照）");
            }
            JSONArray skipped = obj.optJSONArray("skipped");
            if (skipped != null && skipped.length() > 0) {
                sb.append("\n跳过 ").append(skipped.length()).append(" 条不支持/未映射规则：");
                for (int i = 0; i < skipped.length(); i++) {
                    JSONObject s = skipped.optJSONObject(i);
                    sb.append("\n  - 第").append(s.optInt("line")).append("行 ").append(s.optString("raw"))
                      .append("：").append(s.optString("reason"));
                }
            }
            JSONArray notes = obj.optJSONArray("notes");
            if (notes != null && notes.length() > 0) {
                for (int i = 0; i < notes.length(); i++) {
                    sb.append("\n提示：").append(notes.optString(i));
                }
            }
            JSONObject audit = obj.optJSONObject("audit");
            JSONArray issues = audit == null ? null : audit.optJSONArray("issues");
            if (issues != null && issues.length() > 0) {
                sb.append("\n审计告警 ").append(issues.length()).append(" 项：");
                for (int i = 0; i < issues.length(); i++) {
                    sb.append("\n  ! ").append(issues.optJSONObject(i).optString("message"));
                }
            }
            importReportText.setText(sb.toString());
        } catch (JSONException e) {
            importReportText.setText("解析报告失败：" + raw);
        }
    }

    private void runRuleRollback() {
        final String dir = getFilesDir().getAbsolutePath();
        worker.execute(new Runnable() {
            @Override
            public void run() {
                final String raw = NexusCore.nativeRouteRollback(dir);
                main.post(new Runnable() {
                    @Override
                    public void run() {
                        try {
                            JSONObject obj = new JSONObject(raw);
                            if (obj.optBoolean("ok")) {
                                importReportText.setText("【已回滚】规则表已恢复至上一份 LKG 快照。");
                                refreshRulesList();
                            } else {
                                importReportText.setText("回滚失败：" + obj.optString("error"));
                            }
                        } catch (JSONException e) {
                            importReportText.setText("回滚失败：" + raw);
                        }
                    }
                });
            }
        });
    }

    private void refreshRulesList() {
        final String dir = getFilesDir().getAbsolutePath();
        worker.execute(new Runnable() {
            @Override
            public void run() {
                final String raw = NexusCore.nativeRouteList(dir);
                main.post(new Runnable() {
                    @Override
                    public void run() {
                        rulesListContainer.removeAllViews();
                        try {
                            JSONObject obj = new JSONObject(raw);
                            if (!obj.optBoolean("ok")) {
                                rulesCountText.setText("当前生效规则（未连接核心）");
                                makeText(rulesListContainer, "rules_err", 13, false, COLOR_MUTED)
                                    .setText(obj.optString("error"));
                                return;
                            }
                            JSONArray rules = obj.optJSONArray("rules");
                            int count = rules == null ? 0 : rules.length();
                            rulesCountText.setText("当前生效规则（" + count + " 条）");
                            if (count == 0) {
                                makeText(rulesListContainer, "rules_empty", 13, false, COLOR_MUTED)
                                    .setText("暂无自定义规则：.internal 走个人服务，其余默认直连。");
                                return;
                            }
                            for (int i = 0; i < count; i++) {
                                JSONObject r = rules.optJSONObject(i);
                                final int ruleIndex = r.optInt("index");
                                LinearLayout row = new LinearLayout(MainActivity.this);
                                row.setOrientation(LinearLayout.HORIZONTAL);
                                row.setGravity(Gravity.CENTER_VERTICAL);
                                row.setPadding(0, 8, 0, 8);
                                TextView tv = new TextView(MainActivity.this);
                                tv.setText(ruleIndex + ". " + r.optString("match") + "  →  " + r.optString("outbound"));
                                tv.setTextSize(13);
                                tv.setTextColor(COLOR_TEXT);
                                row.addView(tv, new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f));
                                Button del = new Button(MainActivity.this);
                                del.setText("删");
                                del.setTextSize(11);
                                del.setAllCaps(false);
                                styleButton(del, 0xFFFCE8E6, COLOR_DANGER);
                                del.setOnClickListener(new View.OnClickListener() {
                                    @Override
                                    public void onClick(View v) {
                                        deleteRule(ruleIndex);
                                    }
                                });
                                row.addView(del);
                                rulesListContainer.addView(row);
                            }
                        } catch (JSONException e) {
                            makeText(rulesListContainer, "rules_parse_err", 13, false, COLOR_DANGER).setText(raw);
                        }
                    }
                });
            }
        });
    }

    private void deleteRule(final int index) {
        final String dir = getFilesDir().getAbsolutePath();
        worker.execute(new Runnable() {
            @Override
            public void run() {
                NexusCore.nativeRouteRm(dir, index);
                main.post(new Runnable() {
                    @Override
                    public void run() {
                        refreshRulesList();
                    }
                });
            }
        });
    }

    // ==================== Tab 3：诊断与日志 ====================

    private View buildDiagTab() {
        ScrollView scroll = new ScrollView(this);
        LinearLayout col = new LinearLayout(this);
        col.setOrientation(LinearLayout.VERTICAL);
        col.setPadding(24, 24, 24, 32);

        // 路由去向解释卡
        LinearLayout expCard = makeCard(col);
        makeText(expCard, "exp_title", 16, true, COLOR_TEXT).setText("路由去向解释（route explain）");
        explainInput = new EditText(this);
        explainInput.setHint("输入域名或 主机:端口，例如 claude.ai:443");
        explainInput.setText("web.home.internal:80");
        explainInput.setSingleLine(true);
        expCard.addView(explainInput);

        LinearLayout quickRow = new LinearLayout(this);
        quickRow.setOrientation(LinearLayout.HORIZONTAL);
        makeActionBtn(quickRow, "web.home.internal", COLOR_BG, COLOR_TEXT, new View.OnClickListener() {
            @Override public void onClick(View v) { explainInput.setText("web.home.internal:80"); runExplain(); }
        });
        makeActionBtn(quickRow, "claude.ai", COLOR_BG, COLOR_TEXT, new View.OnClickListener() {
            @Override public void onClick(View v) { explainInput.setText("claude.ai:443"); runExplain(); }
        });
        makeActionBtn(quickRow, "查询去向", COLOR_PRIMARY, Color.WHITE, new View.OnClickListener() {
            @Override public void onClick(View v) { runExplain(); }
        });
        expCard.addView(quickRow);
        explainResultText = makeText(expCard, "explain_result", 13, false, COLOR_TEXT);

        // 规则静态审计卡
        LinearLayout audCard = makeCard(col);
        makeText(audCard, "aud_title", 16, true, COLOR_TEXT).setText("规则静态审计（冲突 / 遮蔽 / 无效上游）");
        Button audBtn = new Button(this);
        audBtn.setText("执行静态审计");
        audBtn.setAllCaps(false);
        styleButton(audBtn, 0xFFE8F0FE, COLOR_PRIMARY);
        audBtn.setOnClickListener(new View.OnClickListener() {
            @Override public void onClick(View v) { runRouteAudit(); }
        });
        audCard.addView(audBtn);
        auditResultText = makeText(audCard, "audit_result", 13, false, COLOR_TEXT);

        // N3 本地脱敏流事件卡
        LinearLayout flowCard = makeCard(col);
        LinearLayout flowHead = new LinearLayout(this);
        flowHead.setOrientation(LinearLayout.HORIZONTAL);
        flowHead.setGravity(Gravity.CENTER_VERTICAL);
        TextView flowTitle = makeText(flowHead, "flow_title", 15, true, COLOR_TEXT);
        flowTitle.setText("最近流与 DNS 观测（脱敏）");
        ((LinearLayout.LayoutParams) flowTitle.getLayoutParams()).weight = 1f;
        Button refreshFlowBtn = new Button(this);
        refreshFlowBtn.setText("刷新事件");
        refreshFlowBtn.setAllCaps(false);
        styleButton(refreshFlowBtn, COLOR_BG, COLOR_TEXT);
        refreshFlowBtn.setOnClickListener(new View.OnClickListener() {
            @Override public void onClick(View v) { refreshFlowEvents(); }
        });
        flowHead.addView(refreshFlowBtn);
        flowCard.addView(flowHead);
        flowEventsText = makeText(flowCard, "flow_events", 11, false, COLOR_TEXT);
        flowEventsText.setTypeface(Typeface.MONOSPACE);

        // 运行日志卡
        LinearLayout logCard = makeCard(col);
        LinearLayout logHead = new LinearLayout(this);
        logHead.setOrientation(LinearLayout.HORIZONTAL);
        logHead.setGravity(Gravity.CENTER_VERTICAL);
        TextView logTitle = makeText(logHead, "log_title", 15, true, COLOR_TEXT);
        logTitle.setText("核心运行日志（daemon.log）");
        ((LinearLayout.LayoutParams) logTitle.getLayoutParams()).weight = 1f;
        Button refreshLogBtn = new Button(this);
        refreshLogBtn.setText("刷新日志");
        refreshLogBtn.setAllCaps(false);
        styleButton(refreshLogBtn, COLOR_BG, COLOR_TEXT);
        refreshLogBtn.setOnClickListener(new View.OnClickListener() {
            @Override public void onClick(View v) { refreshDaemonLog(); }
        });
        logHead.addView(refreshLogBtn);
        logCard.addView(logHead);
        daemonLogText = makeText(logCard, "daemon_log", 11, false, COLOR_MUTED);
        daemonLogText.setTypeface(Typeface.MONOSPACE);

        scroll.addView(col);
        return scroll;
    }

    private void runExplain() {
        final String dir = getFilesDir().getAbsolutePath();
        final String target = explainInput.getText().toString().trim();
        explainResultText.setText("查询中…");
        worker.execute(new Runnable() {
            @Override
            public void run() {
                final String raw = NexusCore.nativeRouteExplain(dir, target);
                main.post(new Runnable() {
                    @Override
                    public void run() {
                        try {
                            JSONObject obj = new JSONObject(raw);
                            explainResultText.setText(obj.optBoolean("ok") ? obj.optString("summary") : "失败：" + obj.optString("error"));
                        } catch (JSONException e) {
                            explainResultText.setText(raw);
                        }
                    }
                });
            }
        });
    }

    private void runRouteAudit() {
        final String dir = getFilesDir().getAbsolutePath();
        auditResultText.setText("审计中…");
        worker.execute(new Runnable() {
            @Override
            public void run() {
                final String raw = NexusCore.nativeRouteAudit(dir);
                main.post(new Runnable() {
                    @Override
                    public void run() {
                        try {
                            JSONObject obj = new JSONObject(raw);
                            if (!obj.optBoolean("ok")) {
                                auditResultText.setText("失败：" + obj.optString("error"));
                                return;
                            }
                            if (obj.optBoolean("clean")) {
                                auditResultText.setText("✓ 静态审计通过：无重复、无遮蔽、无无效上游。");
                            } else {
                                JSONArray issues = obj.optJSONArray("issues");
                                StringBuilder sb = new StringBuilder("发现 " + issues.length() + " 项问题：");
                                for (int i = 0; i < issues.length(); i++) {
                                    sb.append("\n  ! ").append(issues.optJSONObject(i).optString("message"));
                                }
                                auditResultText.setText(sb.toString());
                            }
                        } catch (JSONException e) {
                            auditResultText.setText(raw);
                        }
                    }
                });
            }
        });
    }

    private void refreshFlowEvents() {
        final String dir = getFilesDir().getAbsolutePath();
        flowEventsText.setText("加载中…");
        worker.execute(new Runnable() {
            @Override
            public void run() {
                final String raw = NexusCore.nativeFlowRecent(dir, 15);
                main.post(new Runnable() {
                    @Override
                    public void run() {
                        try {
                            JSONObject obj = new JSONObject(raw);
                            if (!obj.optBoolean("ok")) {
                                flowEventsText.setText("未连接核心：" + obj.optString("error"));
                                return;
                            }
                            JSONArray arr = obj.optJSONArray("events");
                            if (arr == null || arr.length() == 0) {
                                flowEventsText.setText("（暂无最近流/DNS 事件）");
                                return;
                            }
                            StringBuilder sb = new StringBuilder();
                            for (int i = 0; i < arr.length(); i++) {
                                JSONObject e = arr.optJSONObject(i);
                                sb.append("#").append(e.optLong("flow_id"))
                                  .append(" [").append(e.optString("ingress")).append("] ")
                                  .append(e.optString("target")).append(" -> ")
                                  .append(e.optString("decision")).append(" (")
                                  .append(e.optString("result")).append(")\n");
                            }
                            flowEventsText.setText(sb.toString().trim());
                        } catch (JSONException e) {
                            flowEventsText.setText(raw);
                        }
                    }
                });
            }
        });
    }

    private void refreshDaemonLog() {
        String text = NexusVpnService.readSmallFile(new File(getFilesDir(), "daemon.log"));
        if (text.isEmpty()) {
            daemonLogText.setText("（暂无日志）");
            return;
        }
        String[] lines = text.split("\n");
        int start = Math.max(0, lines.length - 25);
        StringBuilder sb = new StringBuilder();
        for (int i = start; i < lines.length; i++) {
            sb.append(lines[i]).append("\n");
        }
        daemonLogText.setText(sb.toString().trim());
    }

    // ==================== 通用 UI 辅助 ====================

    private LinearLayout makeCard(LinearLayout parent) {
        LinearLayout card = new LinearLayout(this);
        card.setOrientation(LinearLayout.VERTICAL);
        card.setPadding(28, 24, 28, 24);
        GradientDrawable bg = new GradientDrawable();
        bg.setColor(COLOR_CARD);
        bg.setCornerRadius(24);
        bg.setStroke(2, COLOR_BORDER);
        card.setBackground(bg);
        LinearLayout.LayoutParams lp = new LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT);
        lp.bottomMargin = 20;
        parent.addView(card, lp);
        return card;
    }

    private TextView makeText(LinearLayout parent, String tag, int sp, boolean bold, int color) {
        TextView t = new TextView(this);
        t.setTextSize(sp);
        t.setTextColor(color);
        t.setPadding(0, 6, 0, 6);
        t.setContentDescription(tag);
        if (bold) {
            t.setTypeface(Typeface.DEFAULT_BOLD);
        }
        parent.addView(t, new LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));
        return t;
    }

    private Button makeActionBtn(LinearLayout row, String label, int bgColor, int textColor, View.OnClickListener l) {
        Button b = new Button(this);
        b.setText(label);
        b.setTextSize(12);
        b.setAllCaps(false);
        styleButton(b, bgColor, textColor);
        b.setOnClickListener(l);
        LinearLayout.LayoutParams lp = new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f);
        lp.setMargins(4, 8, 4, 4);
        row.addView(b, lp);
        return b;
    }

    private static void styleButton(Button b, int bgColor, int textColor) {
        GradientDrawable d = new GradientDrawable();
        d.setColor(bgColor);
        d.setCornerRadius(16);
        b.setBackground(d);
        b.setTextColor(textColor);
        b.setPadding(16, 14, 16, 14);
    }

    private static void styleSegment(Button b, boolean active) {
        if (active) {
            styleButton(b, COLOR_PRIMARY, Color.WHITE);
        } else {
            styleButton(b, COLOR_BG, COLOR_TEXT);
        }
    }

    private void hideNetworkViews() {
        for (View v : new View[] {title, message, shortCode, qr, longCode, renewButton, devices, switchNetworkButton}) {
            v.setVisibility(View.GONE);
        }
    }

    private static void show(TextView v, String text) {
        v.setText(text);
        v.setVisibility(View.VISIBLE);
    }

    private void copyText(String text, String toastMsg) {
        ClipboardManager cm = getSystemService(ClipboardManager.class);
        cm.setPrimaryClip(ClipData.newPlainText("Nexus", text));
        Toast.makeText(this, toastMsg, Toast.LENGTH_SHORT).show();
    }
}
