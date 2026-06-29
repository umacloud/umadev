---
id: cjk-in-exports-and-documents
title: 中文/CJK 与 i18n 在数据导出与生成文档中的正确性标准（商业级必读）
domain: backend
category: 01-standards
difficulty: intermediate
tags: [中文导出, cjk, i18n, 乱码, mojibake, csv, bom, excel, xlsx, pdf, 字体嵌入, content-disposition, 文件名, rfc5987, utf-8, gbk, 编码, 导出, 报表, 下载, 商业级]
quality_score: 95
last_updated: 2026-06-29
---

# 中文/CJK 与 i18n 在数据导出与生成文档中的正确性标准（商业级必读）

> 网页能跑通、界面也没问题，但一点「下载表格」导出的中文就变成 `乱码`/`口字框`/`?`——这是 demo 与可交付之间最典型的一道坎。导出文件（CSV/Excel/PDF）和生成文档脱离了浏览器的 UTF-8 渲染环境，进入 Excel/WPS/Numbers/PDF 阅读器这些**自己猜编码、自己挑字体**的程序，任何一个环节编码或字体不对，中文（以及一切非 ASCII：日文、韩文、带重音的拉丁文、西里尔文、阿拉伯文、表情符号）就坏掉。本标准把「中文导出不乱码」当作硬性交付项，逐格式给出确定性做法。

## 1. CSV：写 UTF-8 必须带 BOM（否则 Excel/WPS 必乱码）

- **Excel / WPS 打开 CSV 默认按系统 ANSI 代码页解析**（中文 Windows = GBK/GB18030），**不是** UTF-8。所以一个不带 BOM 的 UTF-8 CSV 在 Excel 里打开必然乱码。
- **确定性修法**：在文件最前面写 UTF-8 BOM（字节 `EF BB BF`，字符 `U+FEFF`）。Excel/WPS 见到 BOM 就会按 UTF-8 解析，中文正常。
  - 前端用 JS 生成下载时同理：`new Blob(["﻿" + csv], { type: "text/csv;charset=utf-8" })`，把 `﻿` 拼在最前。
  - 后端写文件/响应体时把 `EF BB BF` 三字节写在最前，再写正文。
- **BOM 的副作用**：BOM 会让朴素解析器把表头第一格读成带前导 `﻿` 的脏字符串。所以：**给人/给 Excel 看的导出加 BOM；给机器/管道消费的 CSV 不加 BOM**（或读取端显式 strip BOM）。两类用途分清。
- **分隔符**：RFC 4180 用逗号，但部分地区的 Excel（德/法等）按系统「列表分隔符」期望分号 `;`。需兼容时可在首行加 `sep=,`（Excel 私有提示）或按目标 locale 选分隔符。中文环境用逗号即可。
- **引用/转义**：字段含分隔符、双引号或换行时，整字段用双引号包裹，内部双引号写成两个（`""`）。
- **换行**：用 `CRLF`（`\r\n`）最稳；Python 写 csv 必须 `open(..., newline="")` 否则出现空行。
- **长数字 / 前导零（手机号、身份证、订单号）**：Excel 会把长数字转科学计数法、抹掉前导零。修法优先**改用真正的 xlsx 并把该列设为文本格式**；CSV 内的兜底是把值写成 `="00123"`（强制文本），但见下一条注意注入。
- **公式注入（CSV Injection，必须防）**：单元格以 `=` `+` `-` `@`（及 Tab/回车）开头时，Excel/WPS 打开会当公式执行，可被用于数据外泄甚至命令执行。**导出前对以这些字符开头的单元格加前缀单引号 `'` 或空格做无害化**；用 `="..."` 兜底前导零时要同时做注入清洗。

## 2. Excel（.xlsx）：内部恒为 UTF-8，但单元格格式与字体仍要管

- xlsx 本质是 zip 包里的 XML，内部**恒为 UTF-8**——内容不会乱码。**当消费方是 Excel/WPS 时，优先导出 xlsx 而不是 CSV**，直接绕开 CSV 的编码陷阱。
- **数值/日期 locale**：日期写成真正的日期值 + 数字格式（number format），不要写本地化字符串，让消费端按其 locale 渲染；金额用数值 + 货币格式。超过 15 位的整数会丢精度，ID/手机号列设为**文本格式**。
- **写库选成熟实现**：Python `openpyxl`/`xlsxwriter`、Node `exceljs`、Java Apache POI、Go `excelize`、.NET ClosedXML；不要手拼 XML/SpreadsheetML。
- **单元格 CJK 字体**：内容编码无碍，但若**显式设置**单元格字体，要选目标机器上存在且含中文字形的字体（微软雅黑/宋体/思源黑体/Noto Sans CJK），否则显式指定一个无中文字形的西文字体反而显示成框。自动列宽要考虑 CJK 是双倍宽。

## 3. PDF：默认 base-14 字体没有中文字形，必须嵌入 CJK 字体

- PDF 的 14 个标准内置字体（Helvetica/Times/Courier/Symbol/ZapfDingbats）**只覆盖拉丁字符，没有任何 CJK 字形**。用它们画中文 → 缺字、口字框（tofu）、或整段空白。
- **确定性修法**：**嵌入一款含 CJK 字形的字体并先注册再绘制**。选 Noto Sans CJK / 思源黑体（Source Han Sans）这类覆盖全的字体，或系统宋体/黑体。
  - ReportLab：`pdfmetrics.registerFont(TTFont("NotoSansCJK", "...otf"))` 后再 `setFont`；或用内置 CID 字体（`STSong-Light` 等）。
  - HTML→PDF（wkhtmltopdf / Puppeteer/headless Chrome）：**容器里必须装中文字体包**（如 `fonts-noto-cjk`），CSS `font-family` 指到它；瘦镜像（alpine/slim）默认无中文字体是「容器里中文全是框」的头号原因。
  - jsPDF `addFont`；iText/PDFBox `PDFont` 且 `embedded=true`；Go `gofpdf` 用 `AddUTF8Font`。
- **子集化（subsetting）**：只嵌入用到的字形。整套 CJK 字体 10–20MB，子集化后只剩几 KB。多数库默认子集化，要**验证**最终 PDF 体积没把整库塞进去。
- **CJK 断行**：中文没有空格，渲染引擎要在汉字之间断行；只按空格断词的西文换行逻辑遇到长串中文会溢出版心。HTML→PDF 用 `word-break`/`line-break` 控制，或依赖引擎的 CJK 断行。
- **粗体/斜体**：CJK 字体常无真正粗体字形，需嵌入对应字重，否则得到伪粗体。
- **RTL（阿拉伯/希伯来）**：需要支持塑形（shaping）与从右到左排版的引擎 + 含该字形的字体，不能只换字体。

## 4. HTML / 网页表格下载：charset、文件名编码、MIME 一个都不能错

- **声明 charset**：导出响应头必须带 `Content-Type: text/csv; charset=utf-8`（HTML 页面再加 `<meta charset="utf-8">`）。不声明 charset，浏览器/Excel 只能猜，结果就是乱码。
- **中文文件名必须用 RFC 5987/8187 扩展编码**：传统 `Content-Disposition: attachment; filename="发票.csv"` 不允许非 ASCII，会被乱码或截断。正确写法**同时给 ASCII 兜底和 `filename*`**：
  ```
  Content-Disposition: attachment; filename="invoice.csv"; filename*=UTF-8''%E5%8F%91%E7%A5%A8.csv
  ```
  `filename*` 的值是 `UTF-8''` + 对 UTF-8 字节做百分号编码后的文件名。
- **MIME 类型要对**：CSV `text/csv`；xlsx `application/vnd.openxmlformats-officedocument.spreadsheetml.sheet`；PDF `application/pdf`。MIME 写错（如 xlsx 标成 `text/plain`）会让浏览器误处理或加错扩展名。
- **前端 Blob 下载**：CSV 内容前拼 `﻿`，`type` 带 `charset=utf-8`，用 `URL.createObjectURL` + 带 `download` 属性的 `<a>`，下载文件名做好编码。

## 5. 下载链路端到端：每一步都可能毁掉中文

1. **生成（内存里建字符串）**：源数据本身若已是乱码（GBK 库被当 latin1 读出来），出生即损坏。先确保 DB/连接 charset 是 `utf8mb4` 且读取按真实编码解码。
2. **编码成字节**：**显式编码为 UTF-8**，别依赖平台默认（Windows 默认 cp1252/GBK）。Java `getBytes(StandardCharsets.UTF_8)`；Python 写文件 `encoding="utf-8"`（csv 还要 `newline=""`）。
3. **声明 charset**：响应头 charset 必须与实际字节一致。
4. **传输**：别让代理/网关/gzip/转码层二次编码；文件名不要被中间件二次百分号编码（双重编码）。
5. **浏览器**：按 `Content-Disposition` 命名、按 charset 处理；CSV 只是落字节。
6. **目标 App（Excel/WPS/Numbers）**：**Excel(Windows) 是最薄弱一环**——读 UTF-8 CSV 必须靠 BOM；Numbers/LibreOffice 多能自动探测；WPS 同 Excel。**改用 xlsx 可整体绕开这一步的不确定性。**

## 6. 常见乱码根因与确定性修法对照

- **编码不匹配**（声明 utf-8 实为 gbk，或反之）→ 全链路统一 UTF-8，响应头 charset 与字节一致。
- **CSV 缺 BOM**，Excel 按系统 GBK 解析 → 加 UTF-8 BOM，或直接改 xlsx。
- **PDF 缺字体嵌入** → 嵌入并注册 CJK 字体；容器装中文字体包。
- **双重编码**（已 UTF-8 又被当 latin1 再编一次，典型形如 `ä¸­æ–‡` 这类乱码）→ 定位多余的那一次 decode/encode 去掉；全链路只做一次 `decode → 处理 → encode`。
- **GBK vs UTF-8 历史数据**（旧库/旧文件是 GBK，新链路是 UTF-8）→ 读取时**按真实编码 GBK 解码再转 UTF-8**，不要直接当 UTF-8 读。
- **中文文件名乱码/被截断** → 用 RFC 5987 `filename*=UTF-8''...`。
- **口字框 / `?`**（字节其实对，是缺字形）→ 不是编码问题，换一款含该字形的字体。

## 7. 推广到 i18n（不止中文）

- 上述全部规则对**一切非 ASCII**同样适用：日文/韩文、带重音的拉丁文（é, ü）、西里尔文、希腊文、阿拉伯/希伯来文、数据里的表情符号。
- 通用法则只有三条：(1) **全链路显式 UTF-8**，一次解码一次编码；(2) **给 Excel 的 CSV 加 BOM，给 PDF 嵌入含目标字形的字体**；(3) **文件名/响应头按 RFC 5987 + 正确 charset/MIME 声明**。覆盖范围最广的字体优先选 Noto/思源系列。

## 8. 反模式（出现即不合格）

- 导出 CSV 不加 UTF-8 BOM 就交付（Excel 打开必乱码）。
- 用平台默认编码写文件，不显式指定 UTF-8。
- PDF 生成不嵌入中文字体，靠 base-14 内置字体画中文（缺字/框）。
- HTML→PDF 的瘦容器里不装中文字体包，线上中文全是框。
- 中文文件名直接塞进 `filename="..."`，不做 RFC 5987 编码。
- 响应不声明 `charset=utf-8`，MIME 类型写错。
- CSV 导出不做公式注入无害化（`=`/`+`/`-`/`@` 开头单元格）。
- 长数字/前导零（手机号/身份证）被 Excel 转科学计数法或抹零，列不设文本格式。
- 中文导出「看代码觉得没问题」就交付，**没在真机用 Excel/WPS 实际打开验证**。

## 9. 最低交付 checklist（中文/i18n 导出必须真机验证）

- [ ] CSV 用 Excel **和** WPS 实际打开，中文不乱码（已加 UTF-8 BOM，或改用 xlsx）。
- [ ] xlsx 内容、日期、长数字/前导零列正确（ID/手机号设文本格式）。
- [ ] PDF 打开中文不缺字、不出框（已嵌入并注册 CJK 字体；容器装了中文字体包；体积没把整库塞进去）。
- [ ] 中文文件名下载后不乱、不被截断（`Content-Disposition` 用 `filename*` + ASCII 兜底）。
- [ ] 响应头 `Content-Type` 带正确 charset 与 MIME 类型。
- [ ] CSV 已做公式注入无害化。
- [ ] 全链路显式 UTF-8（DB 连接 / 读取 / 编码 / 声明），无双重编码。
- [ ] 其它非 ASCII 内容（日韩/重音拉丁/RTL/emoji）按同规则一并验证。

---
**参考**：RFC 4180（CSV）、RFC 6266/5987/8187（Content-Disposition 与扩展文件名）、UTF-8 BOM、OpenXML/xlsx、Noto Sans CJK / 思源黑体 字体嵌入与子集化、CSV Injection 防护。相关：`frontend/01-standards/i18n-and-localization.md`、`frontend/02-playbooks/i18n-internationalization-playbook.md`、`backend/01-standards/file-upload-and-storage.md`。
