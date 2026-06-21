# 产品类型 → 设计推荐表（concrete palette + 字体对，照抄起步）

> 别从空白页开始猜。先按产品类型查这张表拿到**具体起步 token（已做 WCAG 调整）+ 字体对 + 落地结构 + 必避反模式**，再结合所选档位(`anti-ai-slop.md` 的家族)细化。颜色都是语义 token 的起点，不是终点——可调，但要保持一致与对比。

## 用法
1. 按需求匹配最接近的产品类型行。
2. 取它的 Primary/Accent/Background 作为 `--color-*` token 起点（仍写进 `:root`，组件里用 var）。
3. 取字体对作为 display×body（标题/正文对比轴）。
4. 落地结构与必避项作为硬约束。

## 推荐表

| 产品类型 | 主风格 | Primary | Accent | Background | 字体对(display / body) | 落地结构 | 必避 |
|---|---|---|---|---|---|---|---|
| SaaS(通用) | 玻璃+扁平 | `#2563EB` | `#EA580C` | `#F8FAFC` | Geist / Inter | Hero+Features+CTA | 过度动效、默认深色 |
| 微 SaaS / indie | 扁平+活力 | `#6366F1` | `#059669` | `#F5F3FF` | Space Grotesk / DM Sans | Minimal+Demo | 静态无演示、移动端差 |
| 电商 | 鲜明块面 | `#059669` | `#EA580C` | `#ECFDF5` | Clash Display / Inter | 商品showcase | 无深度的纯扁平、文字堆砌 |
| 电商-奢侈 | premium-luxury | `#1C1917` | `#A16207` | `#FAFAF9` | Playfair / Inter | 大图+留白 | 鲜艳/廉价实心按钮 |
| B2B 服务 | 信任+极简 | `#0F172A` | `#0369A1` | `#F8FAFC` | Söhne / Inter | 信任模块+案例 | 花哨、第二强调色 |
| 金融仪表盘 | 数据密+深色 | `#0F172A` | `#22C55E` | `#020617` | Inter / Inter | 数据看板 | 涨跌只用色不用符号 |
| 分析后台 | 数据密+热力 | `#1E40AF` | `#D97706` | `#F8FAFC` | Inter / Inter | 表格+图表 | 小字、gray-on-gray |
| 医疗健康 | 柔和+可达 | `#0891B2` | `#059669` | `#ECFEFF` | Lora / Raleway | 信任+预约 | AI 紫粉渐变、低对比 |
| 教育 | claymorphism+微交互 | `#4F46E5` | `#EA580C` | `#EEF2FF` | Fredoka / Nunito | 课程+进度 | 冷峻严肃、密集 |
| 创意机构 | brutalist-bold+motion | `#0A0A0A` | `#E6FF00` | `#0A0A0A` | Archivo Expanded / Inter Tight | 作品流 | 圆角柔和、居中弱字 |
| 作品集 | motion+极简 | `#18181B` | `#2563EB` | `#FAFAFA` | Clash Display / Inter | 项目网格 | 千篇一律模板 |
| 游戏 | 3D+赛博/合成波 | `#7C3AED` | `#F43F5E` | `#0F0F23` | Orbitron / Rajdhani | 沉浸 hero | 平淡无能量 |
| 金融科技/Crypto | glass-aurora+深色 | `#5E8BFF` | `#36E0C8` | `#07080D` | General Sans / Inter | 实时数据+信任 | 满屏紫渐变、夸大收益 |
| 约会/社交 | 活力+motion | `#FF4D6D` | `#FF8CC6` | `#FFF0F4` | Cabinet Grotesk / Inter | 卡片流 | 冷淡、低饱和 |
| 餐饮/美食 | 暖色+motion | `#E2571E` | `#F2B705` | `#FFF8F0` | Recoleta / Inter | 大图诱食 | 冷色、无食物图 |
| 健身 | 活力+深色 OLED | `#FF6B35` | `#00D4FF` | `#0A0A0A` | Druk / Inter | 强动感 hero | 柔弱、低对比 |
| 房产 | 玻璃+极简 | `#0077B6` | `#C8A96A` | `#FFFFFF` | Canela / Inter | 大图+地图 | 廉价、密集 |
| 旅行 | aurora+motion | `#0EA5E9` | `#F59E0B` | `#F0F9FF` | Tiempos / Inter | 目的地大图 | 灰暗无憧憬感 |
| 音乐流媒体 | 深色 OLED+专辑色 | `#1DB954` | 取自封面 | `#121212` | Inter / Inter | 沉浸播放 | 浅底、低对比 |
| 开发者工具/IDE | tech-utility/terminal | `#E0E0E0` | `#4AF626` | `#0A0A0A` | Berkeley Mono / Inter | 暗色+蓝焦点 | 花哨、圆润可爱 |
| AI/大模型产品 | glass-aurora | `#5E8BFF` | `#36E0C8` | `#07080D` | General Sans / Inter | 对话/生成展示 | **`#6366F1` 这种 AI 紫**、满屏渐变 |

## 字体对速查（按气质）
- 古典优雅(奢侈)：Playfair/Canela × Inter
- 现代专业(SaaS)：Geist/Poppins × Inter
- 科技初创(dev/AI)：Space Grotesk/General Sans × DM Sans
- 极简瑞士(后台)：Inter × Inter（密度优先时唯一可用 Inter 的场景）
- 活泼创意(儿童/教育)：Fredoka/Cabinet × Nunito
- 大胆宣言(机构/文化)：Archivo Expanded/Druk × Source Sans
- 康养平和(健康)：Lora × Raleway
- 编辑经典(出版)：Cormorant/Tiempos × Libre Baskerville

---
**注**：`#6366F1 / #7c3aed / #667eea→#764ba2` 是公认的"AI-slop 紫"——即便做 AI 产品也**别**用它当主色/hero 渐变；用上表的电蓝+青绿(glass-aurora)来表达"AI/未来感"。
