# PRD：跨设备随需审批（Just-in-Time Approval / Nexus Guard）

- **目标项目**：[medivhc/nexus-next](https://github.com/medivhc/nexus-next)
- **文档状态**：草案（v0.1）
- **日期**：2026-11-26
- **建议里程碑**：M12（接续已完成至 M11.5 的 [docs/roadmap.md](https://github.com/medivhc/nexus-next/blob/main/docs/roadmap.md)）
- **拟新增需求编号**：R93–R97（接续 [FEATURES.md](https://github.com/medivhc/nexus-next/blob/main/FEATURES.md) 现有 R00–R92）

## 1. 产品背景与问题陈述（Problem Statement）

### 1.1 产品现状
截至 M11.5，[medivhc/nexus-next](https://github.com/medivhc/nexus-next) 已经跑通了「单人全设备 + AI Agent 统一身份」的核心基础设施：
- **多端互联**：覆盖 Linux、macOS、Windows、Android、浏览器通行密钥网页节点（WebAssembly）以及锚点中继，支持多路径故障转移与交互冗余（[README.md](https://github.com/medivhc/nexus-next/blob/main/README.md)）。
- **Agent 原生接口**：具备独立 Agent 身份、能力与资源级授权、过期与撤销、本地审计（R11–R17），以及覆盖设备、终端、Drive 工作区与网络诊断的 16 工具完整 MCP 服务（R76，[docs/feature-points/M10.2.md](https://github.com/medivhc/nexus-next/blob/main/docs/feature-points/M10.2.md)）。
- **敏感资产管理**：支持临时访客分享（R73）与端到端加密的 Vault 凭据同步（R74）。

### 1.2 核心产品痛点：「静态授权悖论」
当前 Agent 授权模型是**二元静态授权**——要么提前授予常驻权限（`allow`），要么直接拒绝并返回退出码 `105`（[docs/manual.md](https://github.com/medivhc/nexus-next/blob/main/docs/manual.md)）。在真实高频使用 AI Agent（如 Claude Code、Cursor、Gemini CLI 经 `nexus mcp` 操作家中主机或云端服务器）时，这会引发两难：
1. **权限给大了不安全**：若提前给 Agent 授予 `exec`、`drive.write` 或 `vault.get` 的常驻权限，一旦遭遇 Prompt Injection、模型幻觉或误操作，Agent 可以在无人知晓的情况下执行破坏性命令或窃取凭据。
2. **权限给小了不好用**：若按最小权限原则只给只读能力（如 `drive.read`、`node.info`），Agent 在执行构建、部署、修改关键配置或读取一次性密钥时就会直接被 `105 denied` 截断，用户必须手动切回管理设备重新签发凭据，打断自动化心流。
3. **本地弹窗无法跨设备触达**：现有 AI 工具的本地终端确认（如 CLI 交互式 y/n）只适用于用户正坐在该终端前的场景。当 Agent 跑在家里工作站或云端 VPS 上，而用户正拿着手机或外出用笔记本时，缺乏统一、可信、跨设备的即时审批通道。

### 1.3 为什么这是 Nexus 的独特产品机会
Nexus 的核心价值在于「把一个人的所有设备组成一个整体」。用户的随身笔记本（CLI）、手机（Android）和浏览器（Web 通行密钥节点）**本来就已经在同一个加密身份网格中**。将「人在回路（Human-in-the-Loop）的随需审批」原生做到授权层，能让 Nexus 从「个人设备组网工具」跃升为「个人 AI Agent 的零信任安全控制面」。

## 2. 产品方案概述（Proposed Solution）

引入 **Just-in-Time Approval（随需审批）** 机制：在现有权限模型的 `allow` / `deny` 之外增加第三态 **`ask`（需本人即时批准）**，使高危能力默认处于零常驻特权（Zero Standing Privilege）状态，按需向用户的在线审批设备发起即时二次确认。

### 核心工作流
1. **声明 `ask` 策略**：用户在签发 Agent 凭据（或配置目标设备保护策略）时，将高危能力（如 `term.exec`、`drive.write`、`vault.read`、`svc.connect`、`term.session.kill`）标记为 `ask`，并可指定默认等待超时（默认 `45s`）。
2. **触发与挂起**：当 Agent（经 `nexus exec` / `nexus mcp` 等）调用命中 `ask` 的能力时，目标设备守护进程不立即拒绝，而是生成一条带唯一 ID 的**审批单（`ApprovalRequest`）**，将当前 RPC/MCP 请求挂起等待。
3. **跨设备实时推送与展示**：目标设备通过 Nexus 网格将审批单暴露给用户当前在线的**审批设备**（管理设备及用户显式标记为 `approver` 的受信任设备，覆盖 CLI、Web 网页节点与 Android 应用）。
4. **一键裁决**：用户在任意一台审批设备上查看结构化上下文——谁（Agent 名称与 ID）、在哪台设备、要做什么（完整命令/文件路径/Vault 条目名）、为什么（Agent 经 MCP 传入的 `reason`）——并在超时前选择：
   - **单次放行（Approve Once）**：仅放行当前这一笔请求，执行完即失效。
   - **临时提权（Approve for TTL）**：在指定时长（如 5m / 15m / 1h）内对同一 Agent 的同一能力+资源自动放行，减少高频连续操作的打扰。
   - **拒绝（Deny）**：立即终止挂起请求，可选附带拒绝原因反馈给 Agent。
5. **自动消单与全链路审计**：任一审批端完成裁决后，其他审批端的同一审批单自动标记为已处理；请求恢复执行或返回拒绝；审批全过程写入目标设备的 `nexus audit`。

## 3. 目标用户与核心使用场景（User Stories）

| 场景 | 用户故事 | 解决的痛点 |
| --- | --- | --- |
| **外出远程放行** | 作为开发者，我在通勤路上让家里工作站上的 AI Agent 自动跑回归并部署测试环境；Agent 需要重启容器时，我的 Android 手机弹出审批卡片，我看清命令 `docker compose up -d` 后点「单次放行」。 | 无需在手机上连 SSH 或给 Agent 长期 `exec` 权限，安全与效率兼得。 |
| **编码期零常驻写权限** | 作为使用 `nexus mcp` 的用户，我给本地 Cursor 仅授予 `drive.read` 常驻权限，并将 `drive.write` 设为 `ask`；当 Agent 需要落盘改动时，我在笔记本终端或 Web 面板一键授予「15 分钟临时提权」。 | 避免失控 Agent 静默改坏工作区，同时不被逐文件弹窗烦扰。 |
| **Vault 秘密零常驻取用** | 作为个人基础设施维护者，我将生产 API Key 存在 Vault 中，对 Agent 仅配置 `vault.read` 为 `ask`；Agent 仅在调试接口需要时请求一次，我确认用途后单次放行。 | 消除 Agent 凭据泄露导致 Vault 密钥被批量拖库的风险。 |
| **多端防重放与应急拒绝** | 作为多设备用户，我在 Web 端看到某 Agent 突然请求执行陌生 `curl \| sh` 命令，我立即点击「拒绝」并联动撤销该 Agent 凭据。 | 将事后审计（R17）前移为事中拦截，形成主动防线。 |

## 4. 需求规格与验收标准（Requirements & Acceptance Criteria）

以下需求按 [FEATURES.md](https://github.com/medivhc/nexus-next/blob/main/FEATURES.md) 规范编号，并沿用 `docs/feature-points/` 的 `Given / When / Then` 格式编写。

### R93 `ask` 授权级别与审批单状态机
- **描述**：授权策略支持 `allow`、`ask`、`deny` 三态；命中 `ask` 时进入确定性状态机：`Pending → ApprovedOnce | ApprovedTtl | Denied | Expired | Cancelled`。
- **优先级**：P0
- **验收标准**：
  - **Given** Agent `ci-bot` 被授予 `term.exec@home` 为 `ask`（超时 `30s`）
  - **When** `ci-bot` 发起 `nexus exec home -- uname -a`
  - **Then** 目标设备创建状态为 `Pending` 的审批单并挂起请求；若 `30s` 内无人处理，状态转为 `Expired`，`ci-bot` 收到退出码 `105` 及结构化错误 `{"error":"approval_timeout", ...}`。

### R94 跨设备审批分发与多端单次裁决
- **描述**：审批单实时暴露至用户的在线审批设备（默认管理设备，支持通过 `nexus device set-approver <dev> on|off` 扩展到常用手机或笔记本）；多端同时可见，首个有效裁决生效后其余端自动消单，禁止重复裁决。
- **优先级**：P0
- **验收标准**：
  - **Given** `laptop`（管理设备）与 `phone`（已设为审批设备）均在线
  - **When** Agent 在 `home` 触发 `ask` 审批，用户在 `phone` 上批准「单次放行」
  - **Then** `home` 立即执行该命令并将结果返回 Agent；随后若 `laptop` 再对该审批单提交 `deny`，返回状态冲突（退出码 `106`）。

### R95 临时提权（TTL 授权缓存）与撤销联动
- **描述**：审批时可选择临时提权（如 `--ttl 15m`），目标设备在内存中缓存该 `(agent_id, capability, resource)` 的临时放行令牌（不落盘持久化，重启失效）；一旦该 Agent 被撤销（R14）或提前执行 `nexus approval revoke-grant`，所有临时提权立即失效。
- **优先级**：P0
- **验收标准**：
  - **Given** 用户对 `ci-bot` 的 `term.exec@home` 批准了 `--ttl 15m`
  - **When** 有效期内 `ci-bot` 再次执行命令
  - **Then** 第二次直接放行无需再次审批；若用户此时执行 `nexus agent revoke ci-bot`，随后的请求立即返回未授权（退出码 `105`）。

### R96 MCP 协议透传 `reason` 与结构化等待/失败语义
- **描述**：`nexus mcp`（R76）中可能触发审批的工具新增可选字符串参数 `reason`（最长 256 字符），用于在审批卡片上向用户解释操作意图；当请求被拒或超时，`structuredContent` 明确区分 `denied`（无权限）、`denied_by_user`（被本人拒绝，含可选拒绝理由 `note`）与 `approval_timeout`（审批超时）。
- **优先级**：P0
- **验收标准**：
  - **Given** MCP 客户端以 `ask` 权限的 Agent 身份调用 `nexus_exec`，传入 `reason: "重启测试环境容器以验证补丁"`
  - **When** 审批端查看待审批列表并选择拒绝，附带理由 `"先跑单测再部署"`
  - **Then** 审批端展示完整 `reason`；MCP 工具返回 `isError: true`，且 `structuredContent` 包含 `code: "denied_by_user"` 与 `note: "先跑单测再部署"`。

### R97 安全边界与全链路审计
- **描述**：
  1. **防自批**：Agent 凭据本身严禁调用任何审批裁决接口（即使持有 `*@**` 也不生效），审批权仅限设备本人身份（或经通行密钥认证的网页节点）。
  2. **离线默认拒绝（Fail-Closed）**：等待超时后默认拒绝，绝不降级为自动放行。
  3. **不可篡改审计**：审批单触发（`ask`）、放行、拒绝与超时均写入目标设备 `nexus audit`。
- **优先级**：P0
- **验收标准**：
  - **Given** 某 Agent 尝试调用 `nexus approval approve <id>`
  - **When** 目标设备收到该请求
  - **Then** 直接返回 `105`（`denied`）并记录违规尝试到 `nexus audit`。

## 5. 交互与接口设计（UX & Interface Spec）

### 5.1 CLI 交互设计

```sh
# 签发带 ask 级别的 Agent 凭据（原有 --grant 语法完全向后兼容）
nexus agent issue --label claude-code \
  --grant drive.read@home/drive/work \
  --ask term.exec@home \
  --ask drive.write@home/drive/work \
  --ttl 4h

# 查看本机或全网在线设备上的待审批请求
nexus approval ls [--node home] [--all] [--json]

# 显式裁决某条审批单
nexus approval approve <approval-id> [--node home] [--ttl 15m] [--json]
nexus approval deny <approval-id> [--node home] [--note "原因"] [--json]

# 清除内存临时提权缓存 / 配置默认超时
nexus approval revoke-grant [--node home] [--agent claude-code]
nexus approval timeout 45s

# 管理审批设备资格（默认仅管理设备具备审批权）
nexus device set-approver phone on
```

### 5.2 Web 网页节点与 Android 审批卡片

| 字段 | 说明 | 示例 |
| --- | --- | --- |
| **发起主体** | Agent 名称与短 ID | `claude-code (ag_9f3a)` |
| **目标设备与能力** | 目标节点名 + 能力标签 | `home` · `term.exec` |
| **精确操作内容** | 等宽字体完整展示命令 / 路径 / Vault 键名 | `docker compose -f prod.yml up -d` |
| **操作理由（Reason）** | 来自 MCP `reason` 字段，未提供则显式标注「未说明原因」 | `部署修复到测试环境` |
| **剩余倒计时** | 实时递减进度条，归零自动置灰失效 | `剩余 42s` |
| **操作按钮** | 「拒绝」、「单次放行」、「15 分钟放行」 | 支持可选 Passkey / 生物识别二次确认 |

## 6. 架构接缝与模块集成点（Module Seams & Integration Points）

| 层级 / Crate | 文件路径 | 改动内容与边界约束 |
| --- | --- | --- |
| **纯逻辑层** `nexus-core-proto` | [crates/nexus-core-proto/src/authz.rs](https://github.com/medivhc/nexus-next/blob/main/crates/nexus-core-proto/src/authz.rs) + `approval.rs` | 以 `ask.<cap>` 编码 `ask` 授权，零破坏兼容 `AgentCert` 签名与 19.0–19.5 快照；新增纯状态机 `ApprovalBook`，严守 R25/R26 确定性仿真。 |
| **协议层** `nexus-wire` | [crates/nexus-wire/src/lib.rs](https://github.com/medivhc/nexus-next/blob/main/crates/nexus-wire/src/lib.rs) | 升至 `WIRE_MINOR = 6`（19.6）：末尾新增 `ApprovalList`、`ApprovalDecide`、`ApprovalRevokeGrants`、`ExecWithReason` 及对应回应与错误码，生成 `fixtures/19.6.txt`。 |
| **运行时层** `nexus-runtime` | [crates/nexus-runtime/src/lib.rs](https://github.com/medivhc/nexus-next/blob/main/crates/nexus-runtime/src/lib.rs) + `approval.rs` | 异步审批挂起、内存 TTL 缓存、撤销联动、防自批校验与全链路审计。 |
| **CLI 与 MCP 层** `nexus-cli` | [crates/nexus-cli/src/lib.rs](https://github.com/medivhc/nexus-next/blob/main/crates/nexus-cli/src/lib.rs) | 扩展 `agent issue --ask`、`exec --reason`、`approval` 子命令、`device set-approver`、`nexus mcp` `reason` 透传与结构化错误。 |
| **Web 与移动端** `nexus-web-core` / `web/` / `nexus-android` / `android/` | WebAssembly 节点与 Android 客户端 | M12.2 / M12.3 接入审批订阅与一键裁决卡片。 |

## 7. 非目标（Non-Goals）
1. **多用户/多角色会签审批流**：Nexus 定位单人全设备网络，不引入 RBAC 或团队审批队列。
2. **复杂自定义脚本策略引擎**：不引入 OPA/Rego 等重型策略语言。
3. **离线持久化排队重放**：审批单仅驻留内存且严格绑定实时超时（Fail-Closed）。

## 8. 分阶段落地计划（Phased Rollout）

| 里程碑 | 交付范围 | 验收方式 |
| --- | --- | --- |
| **M12.1 核心与 CLI/MCP** | `nexus-core-proto` 状态机、`nexus-wire` 19.6 扩展、`nexus-runtime` 挂起与审计、CLI 审批命令、`nexus mcp` `reason` 与错误语义 | `crates/nexus-cli/tests/approval.rs` 同时通过 `sim`（双跑确定性）与 `real` 后端，覆盖新旧版本混跑。 |
| **M12.2 网页节点审批** | `nexus-web-core` 订阅审批流、`web/` 审批弹窗与实时倒计时、可选 Passkey 复验开关 | `tools/web-e2e` 真实浏览器端到端验证。 |
| **M12.3 Android 移动端审批** | `nexus-android` JNI 回调、Android 本地通知栏提醒与 App 内一键放行/拒绝、生物识别确认 | `tools/android_e2e.py` 与 redroid 实测。 |
