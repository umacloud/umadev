# Changelog

本文件记录 UmaDev 的所有重要变更。格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)。

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
