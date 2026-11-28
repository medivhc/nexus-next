package link.bewith.nexus.probe;

import android.app.Activity;
import android.os.Bundle;
import android.util.Log;
import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.nio.charset.StandardCharsets;

/** Built as two packages so Android gives the forced-proxy and bypass probes distinct UIDs. */
public final class ProbeActivity extends Activity {
    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        String host = getIntent().getStringExtra("host");
        String token = getIntent().getStringExtra("token");
        int port = getIntent().getIntExtra("port", 0);
        new Thread(new Runnable() {
            @Override public void run() {
                try {
                    InetAddress address = InetAddress.getByName(host);
                    InetSocketAddress target = new InetSocketAddress(address, port);
                    byte[] payload = token.getBytes(StandardCharsets.US_ASCII);
                    Log.i("NexusProxyE2E", token + " dns=" + address.getHostAddress()
                        + " tcp=" + tcp(target, payload) + " udp=" + udp(target, payload));
                } catch (Exception error) {
                    Log.i("NexusProxyE2E", token + " dns=" + error.getClass().getSimpleName());
                }
            }
        }).start();
    }

    private static String tcp(InetSocketAddress target, byte[] payload) {
        try (Socket socket = new Socket()) {
            socket.connect(target, 15000);
            socket.setSoTimeout(22000);
            socket.getOutputStream().write(payload);
            socket.getOutputStream().write('\n');
            try (BufferedReader reader = new BufferedReader(new InputStreamReader(
                    socket.getInputStream(), StandardCharsets.US_ASCII))) {
                String response = reader.readLine();
                return response == null ? "EOF" : response;
            }
        } catch (Exception error) {
            return error.getClass().getSimpleName();
        }
    }

    private static String udp(InetSocketAddress target, byte[] payload) {
        try (DatagramSocket socket = new DatagramSocket()) {
            socket.connect(target);
            socket.setSoTimeout(7000);
            socket.send(new DatagramPacket(payload, payload.length));
            DatagramPacket reply = new DatagramPacket(new byte[256], 256);
            socket.receive(reply);
            return new String(reply.getData(), 0, reply.getLength(), StandardCharsets.US_ASCII);
        } catch (Exception error) {
            return error.getClass().getSimpleName();
        }
    }
}
