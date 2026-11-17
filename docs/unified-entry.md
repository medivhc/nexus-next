# 本机统一入口（M3.2）

常驻程序可以在本机开一个代理入口，SOCKS5 和 HTTP 代理用同一个端口：

```sh
nexus daemon --proxy 127.0.0.1:1080
```

入口只能监听本机地址（127.0.0.1、::1），给出别的地址时常驻程序拒绝启动（退出码 107）。

## 按名字访问服务

`<服务>.<设备>.internal` 是网络里发布的服务。浏览器或程序把代理设成入口后：

```sh
curl -x socks5h://127.0.0.1:1080 http://web.home.internal/
curl -x http://127.0.0.1:1080 http://web.home.internal/
```

和 `nexus connect` 一样：按本机设备的身份授权，由服务所在的设备审计。
服务或设备不存在时，SOCKS5 回应「主机不可达」，HTTP 代理回应 502。

## 路由规则

其余连接按规则走，第一条匹配的规则生效，都不匹配时直连：

```sh
nexus route add --domain example.com --reject   # 这个域名和它的子域名
nexus route add --cidr 10.0.0.0/8 --direct
nexus route ls
nexus route explain www.example.com:443
nexus route rm 1
```

被拒绝的连接：SOCKS5 回应「规则不允许」，HTTP 代理回应 403。

## 出口（M3.3）

```sh
# 在 anchor 上：开放出口（之后名册里自己的设备默认可用；agent 需要授权 net.exit@anchor/exit）
nexus exit on
# 在 laptop 上：这些域名经 anchor 出去
nexus route add --domain example.com --via anchor
# 交给本机已有的代理工具
nexus route add --cidr 10.0.0.0/8 --socks5 127.0.0.1:7890
```

- 经设备出去：判定和审计在出口设备上；出口设备没有开放出口时，SOCKS5 回应「规则不允许」，HTTP 403。
  出口不替别人连出口设备自己的本机地址（127.0.0.0/8、::1、0.0.0.0）——本机程序要给别人用，应当发布成服务。
  出口设备所在局域网里的地址是可以连的（这正是「经家里的设备访问家里的网络」）。出口设备直接连目标，不再按自己的规则转发。
- 上游 SOCKS5：目标原样交给上游（不在本机解析），暂不支持需要认证的上游。
  上游拒绝时回应「连接被拒绝」，上游没有在运行时回应「主机不可达」；HTTP 代理都是 502。

## nexus run

```sh
nexus publish llm --port 8000 --proto http --env OPENAI_BASE_URL=http://{addr}/v1   # 在 home 上
nexus run --use home/llm -- python app.py                                          # 在任何设备上
```

`nexus run` 为每个用到的服务在本机开一个临时端口，按服务的说明设置环境变量（`{addr}` 换成这个端口），
另外设置 `NEXUS_SVC_<设备>_<服务>=127.0.0.1:<端口>`，然后启动程序。程序退出后端口关闭，退出码就是程序的退出码。
用到的服务不存在或没有授权时程序不启动，退出码同 `nexus connect`。
