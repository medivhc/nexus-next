# Nexus Agent 开发方法论 V2

状态为实验性。第一轮分别复验两个 PR。第二轮在两项 Nexus 调度基准问题上完成八次独立试跑。所有候选修复以补丁归档，实验分支上的产品代码仍来自固定基线。

## 第一轮交叉验收

| 固定版本 | Cargo 模型反例 | 各自定向测试 | Git 移出浏览器依赖目录 | 浏览器任务跳过时的门禁 |
| --- | ---: | ---: | --- | --- |
| 基线 `a54db991` | 32 / 1,197 | 19 / 19 | 只识别新路径，`web=false` | 放行 |
| A [PR #24](https://github.com/medivhc/nexus-next/pull/24) `bacc642a` | 0 / 1,197 | 21 / 21 | 识别前后两端，`web=true` | 拒绝 |
| B [PR #25](https://github.com/medivhc/nexus-next/pull/25) `3c96e640` | 0 / 1,197 | 21 / 21 | 识别前后两端，`web=true` | 拒绝 |

两份候选都通过这次定向交叉验收。B 留下的 Cargo 模型可直接对 A 运行。原始耗时取自 PR，A 310 秒，B 482 秒，均含环境启动，各一次生产修复迭代。差值为 172 秒（55%）。模型枚举的 32 个反例可追溯到源码与清单两条单文件漏选。两份候选的提交都没有 PR 工作流记录，浏览器 E2E 和完整 CI 未验收。[复跑原始数据](data/round1-audit.json)

第二轮基线 `2b75c737` 已包含 PR #23。该版本把 Drive core 纳入 Web，并将 crate 内 Markdown 当作可能的构建输入。#24/#25 两个历史候选在同一假设 Markdown 路径上仍输出 `web=false`。这项用例针对将来可能被 `include_str!` 引用的素材，未确认现有源码已引用该路径。与此同时，`2b75c737` 仍只看见重命名的新路径，移到 testkit 的 Web 源码依然能漏选。三个版本的端到端合入状态因此各有边界。[补充回放](data/round1-edges.json)

## 第二轮对照

A 使用常规工作流，B 加载 [试跑时冻结的 V2 指南](evaluated-skill.md)。每个单元一次，两个模型均使用 low effort。时间取求解者记录的开始和结束时间，含求解与自测，扣除了独立验收。普通组可以自然使用交叉检查或模型。模型 token、调用数和货币成本不可取得。

| 任务 | 模型 | A 用时 | B 用时 | A / B 独立验收 | 延迟发送首字节，A / B |
| --- | --- | ---: | ---: | --- | --- |
| 参数校验 | GPT-6.2 Sol | 32 秒 | 36 秒 | 通过 / 通过 | 不适用 |
| 参数校验 | GPT-6.2 Astra | 34 秒 | 32 秒 | 通过 / 通过 | 不适用 |
| 满载窗口 | GPT-6.2 Sol | 123 秒 | 156 秒 | 通过 / 通过 | 失败 / 失败 |
| 满载窗口 | GPT-6.2 Astra | 76 秒 | 207 秒 | 通过 / 通过 | 通过 / 通过 |

参数验收覆盖十组非法输入和五组正常控制。窗口验收在真实下载子进程与本地 SOCKS 服务之间运行，覆盖正常控制、慢探测越界、握手延迟和传输过早结束。延迟发首字节的用例单独归档。在该用例，连接已建立，服务端 4.2 秒后才开始发负载。Sol 两份候选会把连接已建立、负载尚未到达的部分时间记为满载，Astra 两份候选都要求样本落在首末有效载荷之间。真实 Nexus 节点、命名空间限速、R44 p95 和吞吐未验收，执行环境缺少 root。[机器可读数据](data/round2.json) · [补丁和原始判定](data/trials)

第一版窗口评估使用固定的假子进程输出，基线在这里出现四处失败并保留一个正控制。这份测试会固定实现允许变化的父子进程接口，因此在试跑启动后更换为共同的 loopback 判定，并对基线重新校准。基线在 loopback 四项中只通过正常控制。首字节延迟用例来自一次中间实现的验证教训，属于后加的探索性用例。窗口的 4/4 和后加检查的结果都分别保留。子任务使用独立的无 Git 快照与同样的工具范围，隔离依靠任务指令与目录约定。启动并非严格串行反平衡。原 CLI 试跑路径受只读状态目录和执行环境限制，八次试跑均改由独立子任务执行。[原始协议](protocol.md)

## 适用边界

- 清晰的局部规则从单条 CLI 或单元复现开始。两种模型的局部任务均通过，B 的时间变化在 4 秒以内。
- 跨依赖、进程或状态的风险适合一份可独立复跑的判定。先校准反例和正控制。允许修复改变内部接口时，用更稳定的外部观察面验证，例如真实子进程和 loopback。第一轮的 Cargo 模型以及第二轮脱离固定 IPC mock 的复验均符合这条原则。
- 窗口任务上，B 的额外用时在 Sol 为 33 秒，在 Astra 为 131 秒，只有后者超过事先记录的 60 秒且 25% 阈值。固定判定内，两者均未改变通过率。后加的负载用例体现出这批候选对「满载」的建模差异。样本只有一轮，无法区分模型、独立判断、运行波动及方法之间的因果关系。
- 针对模型升级，在反例表明当前验证漏了关键边界后重新确定检查。如果较低成本的外部复跑仍无法闭合风险，再记录已有成本并考虑更换配置。两项试跑均未进行实际模型升级，因此该策略未经对照测量。
- 相同的判定可以复用。只有代码边界、依赖、判定版本、工具链和环境相符时，旧结果才能作为当前证据。完成实验验收与生产 merge gate 分别记录。

[当前 Skill](../../../.pi/skills/evidence-driven-development/SKILL.md) 增加了 [可执行选择器](../../../.pi/skills/evidence-driven-development/scripts/select_validation.py)。五个路由场景已执行通过。选择器基于合同可判定性、边界交互、已有反例、剩余风险和执行环境输出下一步，并保留所需门禁。八次对照使用的是冻结的文字指南，选择器在对照后加入，当前证据只覆盖路由自身。

## 复跑

从仓库根目录执行。第一轮使用 Python 3.11+、Git 及本地 Cargo，第二轮复验使用 Python 3 和本地 loopback。

```sh
experiment=docs/experiments/agent-methodology-v2
git fetch origin refs/pull/24/head refs/pull/25/head
git worktree add --detach ../nexus-ab-base a54db991393645db5e9f13888fcc79b4a2d5cf3f
git worktree add --detach ../nexus-ab-a bacc642a821571ad981f350037b57a0c125bb120
git worktree add --detach ../nexus-ab-b 3c96e640389021564b4b7921397be33ca4d82c38
python3 -B "$experiment/audit_round1.py" --base ../nexus-ab-base --a ../nexus-ab-a --b ../nexus-ab-b
python3 -B "$experiment/audit_edges.py" --a ../nexus-ab-a --b ../nexus-ab-b --main .
python3 -B "$experiment/replay_trial.py" input-sol-b --judge input
python3 -B "$experiment/replay_trial.py" window-astra-a --judge window
python3 -B "$experiment/replay_trial.py" window-sol-a --judge payload
python3 -B "$experiment/replay_trial.py" window-astra-a --judge payload
python3 -B "$experiment/replay_trial.py" baseline --judge strategy
```

预期 `window-sol-a --judge payload` 返回 1。该用例对基线也返回 1。给 `audit_round1.py` 传入 `--cargo /path/to/cargo` 可使用 PATH 以外的 Cargo。边界脚本的 `--main` 应指向 `2b75c737` 或这条仅增加实验文件的分支。
