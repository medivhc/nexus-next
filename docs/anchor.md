# 在 VPS 上部署锚点

锚点 = 网络里一台常开、有公网地址的设备，同时提供中继和网页节点的页面（`https://<域名>/`）。

## 准备

- 一台 Linux VPS，公网 IPv4（有 IPv6 更好），开放 TCP 80 和 443。
- 域名（例如 bewith.link）的 A / AAAA 记录指向这台 VPS。
- 页面文件：在能编译 WebAssembly 的机器上 `tools/build-web.sh`，把 `web/dist` 整个复制到 VPS，例如 `/srv/nexus/web`。
- nexus 可执行文件：`cargo build --release -p nexus-cli`，复制 `target/release/nexus` 到 VPS 的 `/usr/local/bin/`。

## 启动

```sh
# 先用 Let's Encrypt 的测试环境走一遍（证书不受浏览器信任，但能验证 DNS 和端口都对）
sudo nexus --data-dir /var/lib/nexus daemon --name anchor \
  --anchor bewith.link --page-dir /srv/nexus/web --acme-staging
# 日志里出现「证书：」且没有错误后，停掉，删掉 /var/lib/nexus/acme，去掉 --acme-staging 正式运行
```

证书缓存在 `/var/lib/nexus/acme/`，到期前自动续期。已经有证书时用 `--tls-cert` 和 `--tls-key`。
监听 443 和 80 需要 root 或 `CAP_NET_BIND_SERVICE`（`sudo setcap cap_net_bind_service=+ep /usr/local/bin/nexus`）。

## 加入网络

```sh
# VPS 上
nexus --data-dir /var/lib/nexus join
# 管理设备上
nexus device approve <加入码> --name anchor
nexus anchor set https://bewith.link
```

之后各设备自动把锚点加为中继；`nexus web enroll --label phone` 的链接指向 `https://bewith.link/`。

## 检查

```sh
curl -sI https://bewith.link/                 # 200，页面
curl -s https://bewith.link/ping              # 中继的探测路径
curl -sI http://bewith.link/x                 # 301 → https://bewith.link/x
```

在任何浏览器打开 `https://bewith.link/`，用登记过的通行密钥登录。
