package link.bewith.nexus;

import android.app.Activity;
import android.app.AlertDialog;
import android.content.DialogInterface;
import android.text.InputType;
import android.widget.EditText;
import android.widget.LinearLayout;
import java.util.concurrent.Executor;

/** 编辑核心的域名路由。规则按添加顺序匹配，并和命令行共用同一份配置。 */
final class RouteSettings {
    private final Activity activity;
    private final Executor worker;

    RouteSettings(Activity activity, Executor worker) {
        this.activity = activity;
        this.worker = worker;
    }

    void show() {
        final String[] labels = {"查看规则", "添加设备出口规则", "添加 SOCKS5 规则", "添加拒绝规则",
            "按编号删除规则", "导入规则到设备出口", "导入规则到 SOCKS5"};
        final String[] actions = {"list", "exit", "socks5", "reject", "remove", "import_exit", "import_socks5"};
        new AlertDialog.Builder(activity).setTitle("代理规则")
            .setItems(labels, new DialogInterface.OnClickListener() {
                @Override
                public void onClick(DialogInterface dialog, int which) {
                    String action = actions[which];
                    if ("list".equals(action)) {
                        run(action, "", "");
                    } else {
                        edit(labels[which], action);
                    }
                }
            }).show();
    }

    private void edit(String title, final String action) {
        boolean remove = "remove".equals(action);
        boolean importing = action.startsWith("import_");
        final EditText match = new EditText(activity);
        match.setSingleLine(!importing);
        match.setHint(importing ? "DOMAIN-SUFFIX,example.com,PROXY" : remove ? "规则编号" : "域名，例如 example.com");
        if (importing) {
            match.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_MULTI_LINE);
            match.setMinLines(4);
        }
        if (remove) {
            match.setInputType(InputType.TYPE_CLASS_NUMBER);
        }
        final EditText endpoint = new EditText(activity);
        endpoint.setSingleLine(true);
        endpoint.setHint(action.endsWith("exit") ? "出口设备名" : "SOCKS5 主机:端口");
        LinearLayout fields = new LinearLayout(activity);
        fields.setOrientation(LinearLayout.VERTICAL);
        fields.addView(match);
        if (importing || "exit".equals(action) || "socks5".equals(action)) {
            fields.addView(endpoint);
        }
        AlertDialog.Builder dialog = new AlertDialog.Builder(activity).setTitle(title).setView(fields);
        if (importing) {
            dialog.setMessage("可粘贴 Clash/Mihomo 的 DOMAIN-SUFFIX 规则。DIRECT、REJECT 保留；一个命名策略映射到填写的出口。规则追加到已有规则之后。");
        }
        dialog
            .setNegativeButton("取消", null)
            .setPositiveButton(importing ? "导入" : remove ? "删除" : "添加", new DialogInterface.OnClickListener() {
                @Override
                public void onClick(DialogInterface dialog, int which) {
                    run(action, match.getText().toString(), endpoint.getText().toString());
                }
            }).show();
    }

    private void run(final String action, final String match, final String endpoint) {
        final String dataDir = activity.getFilesDir().getAbsolutePath();
        worker.execute(new Runnable() {
            @Override
            public void run() {
                final String result = NexusCore.nativeRoutes(dataDir, action, match, endpoint);
                activity.runOnUiThread(new Runnable() {
                    @Override
                    public void run() {
                        if (!activity.isFinishing() && !activity.isDestroyed()) {
                            new AlertDialog.Builder(activity).setTitle("代理规则")
                                .setMessage(result == null ? "核心的回应无法理解" : result.trim())
                                .setPositiveButton("关闭", null).show();
                        }
                    }
                });
            }
        });
    }
}
