# 自学习踩坑知识库 (Self-Learning Pitfall KB)

UmaDev 在每次驱动底座（Claude Code / Codex / OpenCode）开发时，会**自动识别开发过程中的报错与踩坑**，把它整理成持久化知识，并在**下次遇到同类问题之前**主动提醒规避——让"同一个坑只踩一次"。整套机制无需用户配置，随流水线运行自动生效，全程 fail-open（任何环节出错都不会阻断开发）。

## 这套机制解决什么

传统 AI 编码会反复犯同样的错误：这次踩了 `Cannot find module`、CORS、类型不匹配，下次换个项目又从头踩一遍。UmaDev 把这些经验沉淀下来，形成一个**会自我验证**的知识库：记录坑、提醒规避、并检验"提醒到底有没有让你一次过"。

## 四个阶段

### 1. 识别 (Recognize) — `error_kb`
把原始报错文本归类到 14 个错误家族 + 通用兜底，给出**根因**与**修复建议**：

| 家族 | 典型报错 |
|---|---|
| dependency / module-not-found | `Cannot find module 'X'`、`unresolved import`、`No module named` |
| dependency / package-manager | `npm ERR! ERESOLVE`、peer dep 冲突 |
| type / type-mismatch | TS `is not assignable`、Rust `error[E0308]` |
| runtime / undefined-access | `Cannot read properties of undefined`、`is not a function` |
| runtime / panic | Rust `panicked at`、`unwrap() on None` |
| runtime / port-in-use | `EADDRINUSE` |
| network / cors · connection | `blocked by CORS`、`Failed to fetch`、`ECONNREFUSED` |
| api / http-error | `404 (Not Found)`、`500 (Internal Server Error)` |
| config / env-missing | 缺少环境变量 |
| build / syntax · build-failed | `SyntaxError`、`[vite] failed to compile` |
| test / assertion | 断言失败 |
| 其它 | runtime/permission、lint、generic 兜底 |

每个坑有一个**稳定签名**（如 `dependency/module-not-found/react-router-dom`）。签名归一化时会剥离文件路径、行号、十六进制地址，所以同一类错误无论发生在哪个文件，都会归并成同一条知识。

### 2. 记录 (Record)
- **频率**：同一个坑复发不会重复存储，而是累加命中次数——知道哪个坑反复咬人。
- **技术栈指纹**：记录时打上当前项目的技术栈标签（扫 `package.json` / `Cargo.toml` 依赖名），这是下一步精准触发的关键。
- **有界存储**：上限 300 条，超出时优先淘汰已验证（修复有效）的坑，长期仓库不膨胀。

### 3. 触发 (Trigger) — 精准、与需求措辞无关
下次开发时，UmaDev 用**技术栈指纹交集**决定提醒哪些坑，而不是用需求文字匹配：

> `react-router-dom` 的坑，**只在当前项目真的依赖 react-router-dom 时**才提醒——无论你的需求写的是"做博客"还是"做电商"。

匹配按"判别符命中 > 技术栈重叠 > 关键词重叠"加权，再叠加频率与新近度。命中的坑会以"已知踩坑（务必避免）"的形式注入到底座的开发提示词里，附带原因和规避方法。

### 4. 效能闭环 (Verify) — 自验证 + 自愈
知识库会检验"提醒到底有没有用"：

- **直接证据**：当次构建失败 → 自动修复 → 重新验证通过 → 立即标记该坑的修法 **已验证 (Validated)**。
- **间接推断**：提醒后连续多次运行都不再复发 → 标记 **已验证**，并降低提醒优先级（不再刷屏）。
- **升级**：已提醒过却仍然复发 → 标记 **仍复发 (Recurring)**，下次提示词里升级警告"上次已警示仍复发，必须换更彻底的方案"，并给修复一次新机会（自愈）。

### 跨项目共享
被准确识别出家族的技术坑，**第一次出现就晋升到全局** `~/.umadev/learned/`——换一个新项目，只要用到相同的库/栈，也能直接规避。

## 自动捕获点
开发过程中两处会自动捕获，无需手动操作：
- 底座工具调用失败（实时流式 `ToolResult`）。
- 构建 / lint / 测试非零退出的 stderr。

捕获后会提示一条 `[learned] 识别并记录了 N 条开发踩坑`。

## 查看与管理
- **TUI**：输入 `/pitfalls` 查看全部踩坑（状态 · 命中频率 · 技术栈 · 修法）。
- **CLI**：`umadev report` 输出踩坑库自验证统计（已验证 / 仍复发 / 待验证）。
- **磁盘**：原始记录 `.umadev/learned/_raw/dev-errors.jsonl`；沉淀的可检索 markdown `.umadev/learned/<domain>/`；全局 `~/.umadev/learned/`。

## 设计原则
- **纯函数、零新依赖**：识别器不碰文件/网络，仅做字符串归类。
- **fail-open**：捕获、记录、召回任一环节出错都只是少记一点，绝不阻断开发。
- **匹配技术情境而非措辞**：触发靠技术栈，所以准。
