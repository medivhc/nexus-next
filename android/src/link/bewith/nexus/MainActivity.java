package link.bewith.nexus;

import android.app.Activity;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Intent;
import android.graphics.Bitmap;
import android.graphics.Color;
import android.graphics.Typeface;
import android.net.VpnService;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.view.Gravity;
import android.view.View;
import android.widget.Button;
import android.widget.ImageView;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;
import android.widget.Toast;
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

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        buildViews();
        if ("stop".equals(getIntent().getStringExtra("action"))) {
            startService(new Intent(this, NexusVpnService.class).setAction(NexusVpnService.ACTION_STOP));
            show(title, "Nexus 已停止");
            return;
        }
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
        startForegroundService(start);
        running = true;
        tick();
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
        main.removeCallbacksAndMessages(null);
    }

    @Override
    protected void onDestroy() {
        super.onDestroy();
        worker.shutdownNow();
    }

    /** 在后台线程里问核心，回到界面线程画出来，2 秒后再来。 */
    private void tick() {
        main.removeCallbacksAndMessages(null);
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
                        render(json);
                        if (!isFinishing() && !isDestroyed()) {
                            main.postDelayed(ticker, REFRESH_MS);
                        }
                    }
                });
            }
        });
    }

    private void render(String json) {
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
        hideAll();
        show(title, "Nexus");
        ScrollView scroll = new ScrollView(this);
        scroll.addView(col);
        setContentView(scroll);
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
