"""Loopback fixtures for android_proxy_e2e.py. The SOCKS5 server only echoes probes."""

import contextlib
import io
import ipaddress
import selectors
import socket
import socketserver
import threading


def read_exact(stream, size):
    data = stream.read(size)
    if len(data) != size:
        raise EOFError
    return data


def socks_address(stream):
    kind = read_exact(stream, 1)
    if kind in (b"\x01", b"\x04"):
        value = read_exact(stream, 4 if kind == b"\x01" else 16)
        host = str(ipaddress.ip_address(value))
    elif kind == b"\x03":
        size = read_exact(stream, 1)
        name = read_exact(stream, size[0])
        value, host = size + name, name.decode("ascii")
    else:
        raise ValueError("unsupported SOCKS address type")
    port = read_exact(stream, 2)
    return kind + value + port, host, int.from_bytes(port, "big")


class TcpServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


class UdpServer(socketserver.ThreadingUDPServer):
    daemon_threads = True


class EchoTcp(socketserver.StreamRequestHandler):
    def handle(self):
        self.request.settimeout(5)
        data = self.rfile.readline(256).rstrip(b"\n")
        self.server.record("direct-tcp", None, data)
        self.wfile.write(b"direct-tcp:" + data + b"\n")


class EchoUdp(socketserver.DatagramRequestHandler):
    def handle(self):
        data, sock = self.request
        self.server.record("direct-udp", None, data)
        sock.sendto(b"direct-udp:" + data, self.client_address)


class Socks5(socketserver.StreamRequestHandler):
    def handle(self):
        self.request.settimeout(20)
        try:
            version, count = read_exact(self.rfile, 2)
            if version != 5 or 0 not in read_exact(self.rfile, count):
                return
            self.wfile.write(b"\x05\x00")
            _, command, _ = read_exact(self.rfile, 3)
            _, host, port = socks_address(self.rfile)
            if command == 1:
                self.reply(0)
                data = self.rfile.readline(256).rstrip(b"\n")
                self.server.record("socks-tcp", (host, port), data)
                self.wfile.write(b"socks-tcp:" + data + b"\n")
            elif command == 3:
                if self.server.refuse_udp:
                    self.server.record("socks-udp-refused", None, b"")
                    self.reply(7)
                else:
                    self.associate()
        except (EOFError, OSError, ValueError):
            return

    def reply(self, status, port=0):
        self.wfile.write(bytes((5, status, 0, 1)) + bytes(4) + port.to_bytes(2, "big"))

    def associate(self):
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as relay:
            relay.bind(("127.0.0.1", 0))
            self.reply(0, relay.getsockname()[1])
            with selectors.DefaultSelector() as events:
                events.register(relay, selectors.EVENT_READ)
                events.register(self.request, selectors.EVENT_READ)
                while ready := events.select(45):
                    for key, _ in ready:
                        if key.fileobj == self.request:
                            return
                        packet, client = relay.recvfrom(2048)
                        if packet[:3] != bytes(3):
                            continue
                        frame = io.BytesIO(packet[3:])
                        address, host, port = socks_address(frame)
                        data = frame.read()
                        self.server.record("socks-udp", (host, port), data)
                        relay.sendto(bytes(3) + address + b"socks-udp:" + data, client)


class LocalConnect(socketserver.BaseRequestHandler):
    """Direct CONNECT only to the test ports. Send every other request to the original HTTP proxy."""

    def handle(self):
        client = self.request
        client.settimeout(5)
        data = bytearray()
        try:
            while b"\r\n\r\n" not in data:
                chunk = client.recv(4096)
                if not chunk or len(data) + len(chunk) > 16384:
                    return
                data.extend(chunk)
            headers, pending = data.split(b"\r\n\r\n", 1)
            method, host, _ = headers.split(b"\r\n", 1)[0].decode("ascii").split()
            if method != "CONNECT":
                return
            target = self.server.local_ports.get(host)
            with socket.create_connection(target or self.server.upstream, timeout=5) as upstream:
                if target:
                    print(f"本地测试转发 {host}", flush=True)
                    client.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    if pending:
                        upstream.sendall(pending)
                else:
                    upstream.sendall(data)
                self.relay(client, upstream)
        except (OSError, ValueError):
            return

    @staticmethod
    def relay(client, upstream):
        with selectors.DefaultSelector() as events:
            events.register(client, selectors.EVENT_READ, upstream)
            events.register(upstream, selectors.EVENT_READ, client)
            while events.get_map() and (ready := events.select(65)):
                for key, _ in ready:
                    chunk = key.fileobj.recv(65536)
                    if chunk:
                        key.data.sendall(chunk)
                    else:
                        events.unregister(key.fileobj)
                        key.data.shutdown(socket.SHUT_WR)


@contextlib.contextmanager
def fixtures(socks, refusal, echo, connect_port=None, upstream=None):
    events = []
    lock = threading.Lock()

    def record(protocol, destination, payload):
        with lock:
            events.append((protocol, destination, payload))
        print(f"测试端点 {protocol} {destination or ''} {payload.decode('ascii', 'replace')}", flush=True)

    def snapshot():
        with lock:
            return list(events)

    with contextlib.ExitStack() as stack:
        def start(server_type, port, handler, **options):
            server = stack.enter_context(server_type(("127.0.0.1", port), handler))
            server.record = record
            vars(server).update(options)
            threading.Thread(target=server.serve_forever, daemon=True).start()
            stack.callback(server.shutdown)

        start(TcpServer, echo, EchoTcp)
        start(UdpServer, echo, EchoUdp)
        start(TcpServer, socks, Socks5, refuse_udp=False)
        start(TcpServer, refusal, Socks5, refuse_udp=True)
        if connect_port:
            local = {f"127.0.0.1:{port}": ("127.0.0.1", port) for port in (socks, refusal, echo)}
            start(TcpServer, connect_port, LocalConnect, upstream=upstream, local_ports=local)
        yield snapshot
