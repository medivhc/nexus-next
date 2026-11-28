# Nexus Next 网络运行时设计文档（N6：显式 HTTP 改写、Map Local 与受控策略模拟）

- **文档版本**：v1.0
- **日期**：2026-11-27
- **前置分支**：`feat/n5-adaptive-path`
- **上游依据**：`nexus-next-network-runtime-design-v0.1.md` 第 6.1、10.2、12 节（覆盖 U08、U09）

## 1. 设计目标与安全边界

1. **受控显式 HTTP 变换（U08）**：在显式 HTTP 代理（`serve_http_proxy` 的 `Forward` 路径）支持 4 类确定性变换：
   - `MapLocal`：对测试域名/路径直接返回本地模拟响应（如 Mock JSON），不触达真实生产服务。
   - `RedirectTarget`：将 API 请求无缝指向本地/测试环境服务并同步改写 `Host` 头。
   - `ModifyRequestHeaders`：按规则增改/移除指定请求头。
   - `FaultInject`：开发环境可控故障注入（如 503/429）。
2. **默认零 MITM / 零侵入**：不默认安装 CA 或解密 HTTPS；未命中规则的请求 100% 原样透传；响应体大小硬限制 ≤ 64 KiB。
3. **Agent 受控策略模拟（RFC 10.2）**：新增只读 MCP 工具 `nexus_policy_simulate`，在纯逻辑隔离环境对候选 Clash 规则做解析、静态冲突审计与目标模拟解释，**绝不自动修改生效配置**。
