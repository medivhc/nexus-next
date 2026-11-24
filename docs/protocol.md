# 协议与版本兼容（开发者）

M7.1 起，同一协议主版本内的任何两个版本互相连得上（R65）。这份文档是改协议时要守的规则。

## 版本

- **主版本** `WIRE_VERSION`（`crates/nexus-wire/src/lib.rs`，现在是 19）：变了就互不兼容。
  连接的协议名（ALPN，`crates/nexus-fabric/src/lib.rs` 的 `Proto::alpn`）也一起改，旧版本的设备根本连不上，
  而不是连上之后读到看不懂的数据。
- **次版本** `WIRE_MINOR`：同一主版本内单调增加，只能**增加**东西。
  19.1（M8.3）：加了 `HardwareList`、`HardwareCapture` 请求和回应。
- 软件版本（`Cargo.toml` 的 `version`，`nexus --version`）和协议版本是两回事：
  不改协议的发布不动 `WIRE_MINOR`。

## 消息的形状

- 请求：`RequestHeader { version, minor, roster_version, principal, body }`，`body` 是单独编码的 `Request`。
  收到的一方先读请求头，再解请求体；解不出来（对方的请求比本机新）就回应 `ErrorCode::UnsupportedVersion`，
  说明里写着本机的名字、软件版本和双方的协议次版本，不断开。
- 回应：`ResponseEnvelope { minor, body }`，`body` 是单独编码的 `Response`。解不出来就当作
  「对方版本较新」的错误回应（`ResponseEnvelope::open`）。
- 错误码：`ErrorCode` 的最后一个是 `Unknown`（`#[serde(other)]`），不认识的错误码解成它；说明照常显示。
- 命令行把 `UnsupportedVersion` 映射为退出码 110。

## 可以做的（加次版本）

1. 在 `Request` 或 `Response` 的**末尾**加新的种类。
2. 在 `ErrorCode` 的 `Unknown` **之前**加新的错误码。
3. 要给已有的请求加字段：不要改它，加一个新的请求种类（例如 `ExecV2`），新版本对旧版本的设备仍发旧的。
4. 新的流帧（终端、exec、工作区推送这些请求之后在同一条流上来回的帧）：只有对方的次版本够新才发。
   服务端从请求头的 `minor` 知道对方的次版本，客户端从回应信封的 `minor` 知道。

然后：

- `WIRE_MINOR` 加一；
- 在 `crates/nexus-wire/tests/compat.rs` 的样例表里给新种类加样例（穷举匹配会提醒）；
- `NEXUS_BLESS=1 cargo test -p nexus-wire --test compat` 生成 `tests/fixtures/<主>.<次>.txt`，和代码一起提交；
- 新功能遇到旧版本的设备时，给用户的说明要写清楚哪台设备要升级。

## 不能做的（要升主版本）

- 改已有请求、回应、错误码的字段、顺序、类型或含义。
- 删掉已有的种类，或者在中间插入。
- 改请求头、回应信封、`Principal` 的结构。
- 改名册（`SignedRoster`）、agent 证书、网页会话的签名内容（签名内容变了，旧版本验证不过；
  名册加锚点时用的是「没有锚点时按旧格式签名」的办法，见 `roster.rs` 的 `signed_bytes`）。

`compat.rs` 会拦住前两类里的大部分：已有样例的编码和 fixtures 不一样就失败，这时不能 bless。
真要升主版本：`WIRE_VERSION` 加一、`WIRE_MINOR` 归零、改 ALPN、删掉 `tests/fixtures/` 里的旧文件再 bless，
发布说明里写明所有设备要一起升级。

## 检查

- `cargo test -p nexus-wire --test compat`：编码不变、不认识的消息不致命。
- `nexus-runtime` 的 `unknown_request_gets_a_version_answer`：常驻程序收到不认识的请求时回应版本错误。
- `tools/compat_check.py --old-bin <旧版本>`：真实的两个二进制混跑（组网、ls、status、exec、服务、工作区）。
  本地 CI 用同一个二进制自检脚本；GitHub 的 CI（`compat` 任务）找同一主版本里最新的发布标签，构建它来混跑。

## 常驻程序和终端持有进程之间（M7.2）

`crates/nexus-sys-real/src/holder.rs` 的 `Up`、`Down`：本机的套接字上，长度（4 字节）加 postcard。
升级后新的常驻程序接管旧的持有进程，规则和上面一样：只在末尾加新种类，已有的编码不变
（`crates/nexus-sys-real/tests/holder_compat.rs`）。连上时持有进程先发 `Hello { version }`；
`HOLDER_VERSION` 不同时常驻程序不接管这个会话（它照常运行，`session ls` 里看不到）。
