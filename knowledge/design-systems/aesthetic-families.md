# 美学家族目录（family picker · 强制选一个具名方向）

> 区别精品与 generic 的第一步：**commit 一个具名家族**，而不是"安全平均"。先按产品/品牌气质从下表选**一个**（最多再叠一个次方向），写出一行 **AVOID**（明确不走哪个），再据所选档位(`anti-ai-slop` 的 8 档)落地。我们的 8 个内置档位是其中最常用的几个；本表给全光谱，覆盖小众但有力的方向。

## 选择法（family-picker）
1. 这个产品的气质是？(冷峻工程 / 温暖亲和 / 高端克制 / 大胆张扬 / 未来科技 / 编辑文艺)
2. 信息密度高还是低？(高→data-dense/terminal；低→cinematic/luxury/editorial)
3. 选**一个**家族 + 一行 AVOID。**绝不"融合三个"**。

## 家族目录（定义 / 何时用 / 标志性动作 / 何时别用）

| 家族 | 一句话 | 何时用 | 标志动作 | 别用于 |
|---|---|---|---|---|
| **Editorial Minimalism**(modern-minimal) | 中性色+一个手术刀强调，Inter/Geist，≤8px 圆角，描边代替阴影 | dev工具/B2B/文档 | 单一 accent 只给 action/link/focus；4px 网格 | 需要温暖/张扬的品牌 |
| **Terminal-Core**(tech-utility) | 等宽字+纯黑底，无彩色强调("对比即特征")，0 圆角，hover 反色 | CLI/基础设施/监控 | mono 全场+瞬时反色+数据 tabular-nums | 消费营销 |
| **Warm Editorial**(editorial-clean) | 奶白+陶土色，衬线 display+人文 sans，扁平 | prosumer/长文/出版 | 衬线标题×humanist 正文，680px 文栏 | **禁紫渐变** |
| **Data-Dense Pro** | `#181818`+8 色分类系列，tabular-nums，28–32px 行，粘性表头 | BI/可观测/金融后台 | 高密度表格+热力+sticky header | 营销/衬线 |
| **Cinematic Dark**(bold-geometric 偏暗) | 纯黑+品红/青，**渐变可用**，96–140px 巨字，pill 按钮 | AI/creator/汽车发布 | 满幅影像当 chrome；唯一允许 scale(1.02) hover 的家族 | 数据密后台 |
| **Playful Color**(soft-warm) | 5 色品牌棱镜按区轮换，圆角卡/pill，扁平矢量插画 | 消费/教育 | 每区换一个品牌色；圆润友好 | 严肃金融/奢侈 |
| **Glass / Soft-Futurism**(glass-aurora) | 磨砂 `blur(20px) saturate(160%)`+柔和径向渐变 | 高端消费/AI | 玻璃层+1px 高光，**保 4.5:1 对比** | 数据密/严肃 |
| **Neon Brutalist / Swiss Bold**(brutalist-bold) | `#fff/#000`+一个饱和色，0 圆角，2px 线代替留白，巨型 tabular 数字 | 编辑/宣言/机构 | 暴露网格+硬边+sans×serif 对撞 | 需要亲和的产品 |
| **Premium Luxury / Kinpaku**(premium-luxury) | 近黑+单一金/宝石色，衬线细字重，慷慨留白，慢动效 | 奢侈/财富/汽车 | 单金强调面积极小+700ms 慢动+大留白 | 大众打折/儿童 |
| **Neumorphism** | 软 UI，凸凹双向阴影，单色，12–16px 圆角 | 极简工具/音乐/健康小件 | `-5px -5px 15px, 5px 5px 15px` 双阴影 | 对比要求高(易塌)/复杂界面 |
| **Claymorphism** | 黏土感，柔大圆角，膨胀阴影+亮色 | 教育/儿童/趣味 | 厚圆角+柔投影+饱和糖果色 | 严肃/数据 |
| **Aurora / Gradient-Mesh** | 极光网格渐变(低饱和大模糊)做氛围 | AI/旅行/creator | 固定背景极光层+玻璃内容 | 满屏当主体即 slop |
| **Liquid Glass** | 流体玻璃+色散，更强折射 | 高端发布/Apple 系 | 折射+流体动画(400–600ms) | 低端/快交互 |
| **Retro-Futurism / Synthwave** | 霓虹紫粉青+网格地平线+发光 | 游戏/音乐/夜店 | 霓虹描边+扫描线+发光文字 | 企业/医疗 |
| **Cyberpunk / HUD-FUI** | 高科技界面，霓虹+故障+数据流 | 游戏/科幻/硬核工具 | 角标框+故障文字+HUD 元素 | 亲和消费 |
| **Bento Box Grid** | 便当格：不等大模块拼贴 | 作品集/feature 展示/AI | 不等格 `2fr 1fr 1fr` 拼贴 | 线性长内容 |
| **Spatial UI (VisionOS)** | 空间层级，半透明玻璃，深度 | 空间计算/前沿 | 玻璃材质+深度+柔光 | 普通 web 性能敏感 |
| **Skeuomorphism / 3D Hyperrealism** | 拟物质感/超写实 3D | 创意/产品展示 | 真实材质+光影+3D 渲染 | 性能/简洁工具 |
| **Y2K / Gen-Z Maximalism** | 千禧/极繁混乱，撞色贴纸 | 潮流/Gen-Z/音乐 | 撞色+贴纸+故意混乱 | B2B/严肃 |
| **Organic / Biophilic** | 自然有机曲线+大地色 | 健康/可持续/食品 | 有机曲线+植物纹理+大地色 | 科技冷峻 |
| **E-Ink / Paper** | 纸感低对比单色 | 阅读/笔记/极简 | 纸白+墨黑+极少色 | 需要活力/转化 |
| **Material You** | 动态取色+大圆角+表面色调 | Android/消费 | 壁纸取色+tonal surface | 品牌强一致需求 |

## 通用收尾（任何家族都遵守）
- 选定后**只用这一套** token；颜色/字体都过 `:root`；遵守 `anti-ai-slop` 的硬规格与自评门。
- 所选家族的"标志动作"至少做出 1 个，让它有记忆点。
- 仍跑 thumbnail test：缩略图要像"这个产品"。

---
**注**：内置 8 档(modern-minimal/editorial-clean/tech-utility/soft-warm/bold-geometric/brutalist-bold/glass-aurora/premium-luxury)已自带完整 token，优先用；本表用于需要更小众方向时**具名 commit**，避免回退 generic。
