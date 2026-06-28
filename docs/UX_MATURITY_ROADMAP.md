# UmaDev 交互成熟度路线图

> 本文档由对一套成熟参考 TUI Agent 的逐行源码研究 + UmaDev 自身复发 bug 模式的对照分析得出。
> 核心论断:**UmaDev 的 bug 不是几十个独立缺陷,而是 5 个不成熟的「模型」,每个模型制造一整类 bug。
> 成熟产品的标志是:每个关注点只有一个权威模型。** 修好这 5 个根模型,对应的 bug 类整体消失。

---

## 总览:5 个根模型 → 5 个根因 → 5 个根修复

| 领域 | 根因(一句话) | 复发的 bug 类 | 根修复 | 量级 |
|---|---|---|---|---|
| **输入** | crossterm 解析器在一次 read 正好结束在鼠标序列的 ESC 字节时,**急切地把孤立 ESC 当退出键并丢弃该字节**,续接字节失去锚点 | 滚动出 `[<64;…M` 乱码 · 假"本轮已中止" · Esc 卡顿 · Alt 组合误判 | **自己拥有 stdin + 一个健壮的字节 tokenizer**(缓冲未完成序列,孤立 ESC 用 50ms FD 感知超时裁决)| L |
| **渲染** | ratatui 只对比"自己的上一帧 vs 下一帧",**从不和终端真实状态对账**,一旦漂移就持续到手动 clear | "跑久了界面错乱,只有 Ctrl+L 能恢复" · resize 残影 · 宽字符串行 | **自愈式定期重绘**(BSU/ESU 内 `clear()+draw()`,同步输出下不可见)+ **原子 resize 擦除** + 单写入者 | S–M |
| **会话/续接** | `/continue` **开新底座会话** + **丢弃构建好的 directive** + 用**裸步骤标题**驱动新大脑 | "不记得上次的任务" · "领回错的/上次落盘的任务" | **恢复底座会话**(claude `--resume` / codex `thread/resume`)+ 持久化底座 session_id + run 标识 | S–M |
| **交互 UX** | **三套漂移的命令源**(palette / dispatch / help)各自手维护;只有最近一条折叠项能展开 | 命令找不到/提示错(`/model` 不在 palette、12 个命令不在 help)· 旧输出无法展开 · 面板执行完不清理 | **一个命令注册表**(palette+help+dispatch 全读它,测试锁一致)+ **一个全局展开开关** | M+S |
| **错误/恢复** | 只有一个 idle 看门狗 + 原始 stderr 透传,**无分类、无重试** | "base session idle"无原因 · 瞬时网络/过载即终止 · Ctrl+C 杀掉热会话 | **底座失败分类器**(auth/限流/网络/上下文/过载 → 可操作提示)+ 重试退避 + 中断保活 | S–M |

---

## 各领域成熟设计(实现对照)

### 1. 渲染 — 抄"原子性 + 自愈",不抄"diff 引擎"
ratatui 全缓冲 diff 本就免疫"视口内污染"(参考实现费大力气维护的 `prevFrameContaminated`/blit 在我们这反而是退步,**不要抄**)。ratatui 的弱点相反:从不和终端对账,漂移静默累积。补:
- **R1 自愈 scrub**(量级 S,杀头号 bug):`force_full_repaint` 标志,流式时每 ~2–3s 或 diff 过大时,`terminal.clear()` 紧跟 `terminal.draw()`,**整体包在一个 BSU/ESU 里**(同步输出下不可见)→ 任何累积漂移自动愈合,无需用户按 Ctrl+L。
- **R4 原子 resize 擦除**(S):resize 改为置 `force_clear_next_frame`,在循环顶部 `clear()+draw()` 背靠背、包在一个 BSU/ESU 里(旧内容保留到新帧原子换入)。去抖同尺寸 resize。
- **R3 单写入者**(S–M):BSU/ESU、OSC52、标题 OSC 都走 ratatui backend 的同一个 writer / 同一把锁,杜绝 worker 线程的 stdout 写入插进帧中间。
- **R5 睡眠唤醒自愈**(M):记 `last_input_instant`,>5s 间隔后到来的事件触发 `reassert_terminal_modes()`(重发 EnableMouseCapture/BracketedPaste,必要时重进 alt 屏)+ 置 force_full_repaint;Unix 装 SIGCONT 处理。治"睡眠/ssh 重连后鼠标死、画面残留"。
- **R2 宽字符补偿**(M):含模糊宽度字形(emoji 新区段、VS16)的帧触发 R1 全重绘(廉价版),或自己布局时补位。
- 现状已对的(别退):同步输出已实现、预折叠到视觉行的滚动模型、panic 钩子恢复终端。

### 2. 输入 — 自己拥有 stdin + 健壮 tokenizer(治本)
**根因已在 crossterm 源码确认**:`parse.rs` 在 `buffer.len()==1 && !input_available` 时急切产出 `KeyCode::Esc`,`mio.rs` 随后 `buffer.clear()` 丢掉 ESC;而交互 stdin 几乎不会满 1024 字节读,所以读正好结束在 ESC 上时必中招。防御式过滤器只能在"已被错误解析后"打补丁,且给每个真 Esc 加 80ms 延迟。
- **P1 自己拥有 fd 0 + tokenizer**(L,治本):ratatui 只写不读,crossterm raw mode 只设 termios — 所以可只替换输入源。读 fd 0(阻塞线程→mpsc 或 AsyncFd),跑一个 ~250 行字节状态机(Ground/Escape/Csi/Ss3/Osc/Dcs/Apc,持久 buffer 缓冲未完成序列),SGR 鼠标报告无论怎么切都是一个 token;孤立 ESC **缓冲不猜**,用 50ms FD 感知超时裁决。产出 `InputEvent` enum 给现有 `apply_key_with_mods` 消费(下游几乎不变)。`MouseSeqFilter` 降级为兜底。
- **P2 单一键派发真相源**(M):`enum InputContext { Picker, ChatEditing, ChatPaletteOpen, ChatHistoryRecall, OverlayOpen, RunActive }` + 单一 `resolve(context, key, mods) -> Action`,把"Up 在状态 X 干什么"声明化,消灭散落的 `if has_palette` 隐患。
- **P3 规范化 Key 结构**(S):预算各键 + ctrl/alt/shift/super 布尔,一次算好;tokenizer 已产出 kitty `CSI u`,可区分 Shift+Enter/Ctrl+Enter。

### 3. 会话/续接 — 借底座的持久化(近零额外存储)
底座本就持久化自己的完整转录(claude `~/.claude/projects/<dir>/<session_id>.jsonl`、codex thread store、opencode 服务端 session)。UmaDev **不该重复落盘分析**,只持久化指针:
- **P0 持久化底座 session_id + run 标识**(S):`WorkflowState` 加 `base_session_id` + `run_id`,或 `.umadev/run.json {run_id, backend, base_session_id, requirement, slug, plan_path}`。id 已在内存,统一 `BaseSession::session_id()` 暴露并在开会话时序列化。这是基石。
- **P0 /continue 恢复底座会话而非开新的**(M):`session_for` 加可选 resume-id;claude 加 `resume_args`(`--resume <id>` 无 `--fork-session`)、codex 用 `thread/resume {threadId}`(去掉只读沙箱)。底座带回完整上下文。
- **P0 别在续接路径丢弃 directive**(S):把 requirement/goal directive 也传进 `drive_director_loop_resume`,`drive_build_step` 前置原始需求 + "续接第 N 步"框架。
- **P1 选对 run**(M):校验 plan.json 属于当前 run(match run_id/slug)+ base_session_id 仍可恢复;多个可恢复 run 时给选择器。以底座转录为"上次到哪了"的真相源,而非仅 plan 步骤状态。
- **P1 中断标记**(S):记录被中断的步 + 最后 directive,续接时发"从上次中断处继续"。
- **P2 opencode 续接 / 优雅降级**(M):服务端还在就重指向 session_id;否则降级=新会话+重放有界转录。
- 存储结论:额外 ≈ 一个 36 字节 UUID + 小 run.json。

### 4. 交互 UX — 每个关注点收敛到一个模型
- **P0 统一命令模型**(M,最高价值):`SLASH_VERBS` 从元组改结构体 `SlashCommand { name, aliases, arg_hint, group, desc_key, hidden }`;palette + help + dispatch **全读这一个表**;加测试断言每个非隐藏命令都有 dispatch 分支、反之亦然。当前 `/model` 不在 palette、12 个命令不在 help、一堆别名只在 dispatch — 全因三套源漂移。
- **P0 一个全局"展开全部"开关**(S):app 级 `verbose: bool` + 单键(Ctrl+O)翻转,所有可折叠渲染器读 `verbose || collapsed`。治"旧折叠项无法展开"。
- **P1 面板生命周期 + 单键折叠**(M):完成后自动收起/清理(不再 12 行"全完成"长驻);单键(Ctrl+T)折叠,critics 与 plan 解耦。
- **P1 一个权威页脚**(M):标题行=身份/上下文(把 mode chip 移上来);底部行=实时状态 + 空闲时 `? 看快捷键`;input 下 meta 行=瞬时提示 + `[排队 N]`,去掉重复的 backend。
- **P1 连贯 mode + 循环键**(S–M):`TrustMode`(Plan/Guarded/Auto)为唯一概念,`/manual`/`/auto` 降为别名,加 Shift+Tab 循环 + 标题行彩色显示。
- **P2 闸门可选择决策**(M–L):闸门处加固定选择器(Continue/Revise…/View diff/Quit,方向键+Enter+编号),复用 palette popover 组件;闸门清单固定在 input 上方不滚走。

### 5. 错误/恢复 — 分类 → 重试瞬时 → 可操作提示 → 保留部分 → 续接
- **D1 底座失败分类器**(S→M,杀"盲 idle"):`enum BaseFailure { Auth, RateLimit{reset}, Network{ssl}, Context, Overloaded, Unknown }` + `classify(exit, stderr_tail, jsonrpc_err)` + `actionable_message(failure, backend)` 出 i18n 提示(claude→"未登录,运行 claude /login";codex→"过载(-32001),稍后重试或 /model 切换";网络→"连不上,检查代理/NODE_EXTRA_CA_CERTS")。在 `enrich_idle_reason`/`enrich_base_failure` 单一铸造点先 classify 再前置提示,原始 stderr 作详情附后。
- **D2 有界重试退避 + 心跳**(M):`with_base_retry` 包住 turn,只重试 RateLimit/Overloaded/Network(Auth/Context 需用户处理),指数退避 `min(500ms·2^n,32s)+jitter` 尊重 retry-after,封顶 ~3–5 次或 deadline。每次重试发 Note(兼作心跳,把长 sleep 切成 ≤30s yield,idle 看门狗和用户都见活着)。claude 自带重试 → 只在 UmaDev 层/传输失败时重试避免双重。
- **D3 取消统一为"中断并保留会话"**(M):`Action::Cancel` 改为 `session.interrupt()` + 等 `Interrupted` + **保留会话复用**(而非 abort+end 杀掉),保留部分文本进转录;abort 仅作 5s 兜底。Ctrl+C 停活、留热脑、及时停止 token 消耗、不破坏 tool_use/result 配对。
- **D4 失败也落盘 + 保留部分文本**(S→M):失败路径返回已流式的 `text`,每次退出(含失败)都 `persist_plan`,接上续接模型 → 失败/取消后从最后好步续接。
- **D5 chat 路径 idle 对齐**(S):`CHAT_SESSION_IDLE` 改读 `idle_timeout()`,chat turn 走共享 `next_event_idle`(同样的有界中断 + 诊断捕获)。
- **D6 永不弄脏终端的兜底**(M):退出/panic/SIGINT 先同步恢复终端模式,装失效计时器(cleanup 超 2–5s 强制退出),退出前打 `/resume` 提示。

---

## 分阶段落地计划

### Phase 1 — 5 个根修复(杀掉复发 bug 类,每个一处聚焦+测试)
1. 输入 P1(自己拥有 stdin + tokenizer)—— 治本,杀鼠标乱码/假中止/Esc 卡顿
2. 渲染 R1+R4+R3(自愈 scrub + 原子 resize + 单写入者)—— 杀"跑久了错乱"
3. 会话 P0(恢复底座会话 + 持久化 session_id/run 标识 + 不丢 directive)—— 杀"不记得任务/领错任务"
4. UX P0(统一命令注册表 + 全局展开)—— 杀"命令找不到/旧输出看不到"
5. 错误 D1(失败分类器)—— 杀"盲 idle"

### Phase 2 — 连贯性 + 健壮性(P1)
渲染 R5/R2 · 输入 P2/P3 · 会话 P1(选对 run + 中断标记)/P2(opencode)· UX P1(面板生命周期/单一页脚/mode 循环)· 错误 D2/D3/D5

### Phase 3 — 打磨(P2)
UX 闸门选择器 / 页脚显实时状态 · 错误 D4/D6

---

## 贯穿原则
成熟感来自:**每个关注点恰好一个模型** —— 一个命令注册表、一个展开开关、一个 mode 枚举、一个页脚、一个失败分类器、一个输入 tokenizer —— 且**每个交互元素都自报快捷键**。UmaDev 的"骨头"已不错(palette 窗口化、滚动指示、did-you-mean、卡顿变红诚实),要做的是把重复/三套的模型各自收敛成一个,并给计划面板、折叠输出、mode 补上缺失的单键交互。
