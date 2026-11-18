package link.bewith.nexus;

import android.app.Activity;
import android.content.Intent;
import android.net.VpnService;
import android.os.Bundle;
import android.widget.TextView;

/** 请求 VPN 授权（第一次会弹出系统对话框），然后启动或停止 Nexus。 */
public class MainActivity extends Activity {
    private static final int REQUEST_VPN = 1;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        TextView view = new TextView(this);
        view.setText("Nexus\n\n用 nexus 命令行管理这台设备（数据目录：" + getFilesDir() + "）");
        view.setPadding(48, 48, 48, 48);
        setContentView(view);
        if ("stop".equals(getIntent().getStringExtra("action"))) {
            startService(new Intent(this, NexusVpnService.class).setAction(NexusVpnService.ACTION_STOP));
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
        if (request == REQUEST_VPN && result == RESULT_OK) {
            Intent start = new Intent(this, NexusVpnService.class).setAction(NexusVpnService.ACTION_START);
            if (getIntent().getExtras() != null) {
                start.putExtras(getIntent().getExtras());
            }
            startForegroundService(start);
        }
    }
}
