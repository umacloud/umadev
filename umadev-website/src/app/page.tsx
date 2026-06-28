"use client";

import Image from "next/image";
import { useEffect, useRef, useState } from "react";
import { TerminalDemo } from "./TerminalDemo";
import { asset, docs, gallery, i18n, releases, type DocBlock, type Lang, type View } from "./content";
import styles from "./page.module.css";

const githubUrl = "https://github.com/umacloud/umadev";
type DocItem = { id: string; title: string; blocks: readonly DocBlock[] };
type DocCategory = { cat: string; items: readonly DocItem[] };

const CHARS = "010101010101ABCDEF0123456789X%#$@";

export function ScrambledHoverText({ text, className }: { text: string; className?: string }) {
  const [displayText, setDisplayText] = useState(text);
  const [prevText, setPrevText] = useState(text);

  if (text !== prevText) {
    setPrevText(text);
    setDisplayText(text);
  }

  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const handleMouseEnter = () => {
    let iteration = 0;
    if (intervalRef.current) clearInterval(intervalRef.current);
    
    intervalRef.current = setInterval(() => {
      setDisplayText(
        text
          .split("")
          .map((char, index) => {
            if (char === " ") return " ";
            if (index < iteration) return text[index];
            return CHARS[Math.floor(Math.random() * CHARS.length)];
          })
          .join("")
      );
      
      iteration += 1 / 2;
      if (iteration >= text.length) {
        setDisplayText(text);
        if (intervalRef.current) clearInterval(intervalRef.current);
      }
    }, 25);
  };
  
  const handleMouseLeave = () => {
    if (intervalRef.current) clearInterval(intervalRef.current);
    setDisplayText(text);
  };
  
  useEffect(() => {
    return () => {
      if (intervalRef.current) clearInterval(intervalRef.current);
    };
  }, []);

  return (
    <span 
      onMouseEnter={handleMouseEnter} 
      onMouseLeave={handleMouseLeave} 
      className={className}
      style={{ position: "relative", display: "inline-block" }}
    >
      <span style={{ visibility: "hidden" }} aria-hidden="true">{text}</span>
      <span style={{ position: "absolute", left: 0, top: 0, width: "100%", height: "100%", whiteSpace: "nowrap" }}>
        {displayText}
      </span>
    </span>
  );
}


export function FloatingDiagnosticTerminal({
  setLang,
  setView,
  setHeroSlideIndex,
  setMode,
  setStageIndex,
}: {
  setLang: (l: Lang) => void;
  setView: (v: View) => void;
  setHeroSlideIndex: (s: number | ((prev: number) => number)) => void;
  setMode: (m: string) => void;
  setStageIndex: (i: number) => void;
}) {
  const [isOpen, setIsOpen] = useState(false);
  const [logs, setLogs] = useState<string[]>([
    "UMADEV CORE DIAGNOSTICS v1.0.6",
    "Initializing MCP channels... OK",
    "Status: ONLINE [SECURE_MODE]",
    "Type /help for diagnostic commands."
  ]);
  const [inputValue, setInputValue] = useState("");
  const bodyRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (bodyRef.current) {
      bodyRef.current.scrollTop = bodyRef.current.scrollHeight;
    }
  }, [logs, isOpen]);

  const handleCommand = (cmdStr: string) => {
    const raw = cmdStr.trim();
    if (!raw) return;
    
    setLogs((prev) => [...prev, `umadev_hud > ${raw}`]);
    const tokens = raw.split(/\s+/);
    const cmd = tokens[0].toLowerCase();
    const arg = tokens[1]?.toLowerCase();
    
    setTimeout(() => {
      if (cmd === "/help" || cmd === "help") {
        setLogs((prev) => [
          ...prev,
          "--- Active HUD Controller Commands ---",
          "  /lang <zh|en>     - Switch language",
          "  /slide <1-3|next> - Switch Hero visual slides",
          "  /mode <id>        - Switch base (claude-code | codex | opencode)",
          "  /stage <1-3>      - Switch pipeline stage",
          "  /view <home|docs> - Navigate page views",
          "  /stats            - Print system & browser diagnostics",
          "  /scan             - Run code & secret checks",
          "  /governance       - Verify governance checklists",
          "  /clear            - Clear console screen"
        ]);
      } else if (cmd === "/clear" || cmd === "clear") {
        setLogs([]);
      } else if (cmd === "/lang") {
        if (arg === "en") {
          setLang("en");
          setLogs((prev) => [...prev, "System: Language set to ENGLISH."]);
        } else if (arg === "zh") {
          setLang("zh");
          setLogs((prev) => [...prev, "系统：语言切换为【中文】。"]);
        } else {
          setLogs((prev) => [...prev, "Usage: /lang <zh|en>"]);
        }
      } else if (cmd === "/slide") {
        const val = parseInt(arg, 10);
        if (arg === "next") {
          setHeroSlideIndex((prev) => (prev + 1) % 3);
          setLogs((prev) => [...prev, "System: Rotated to next hero slide."]);
        } else if (val >= 1 && val <= 3) {
          setHeroSlideIndex(val - 1);
          setLogs((prev) => [...prev, `System: Loaded hero slide ${val}.`]);
        } else {
          setLogs((prev) => [...prev, "Usage: /slide <1-3|next>"]);
        }
      } else if (cmd === "/mode") {
        const valid = ["claude-code", "codex", "opencode"];
        if (valid.includes(arg)) {
          setMode(arg);
          setLogs((prev) => [...prev, `System: Base switched to ${arg}.`]);
        } else {
          setLogs((prev) => [...prev, "Usage: /mode <claude-code|codex|opencode>"]);
        }
      } else if (cmd === "/stage") {
        const val = parseInt(arg, 10);
        if (val >= 1 && val <= 3) {
          setStageIndex(val - 1);
          setLogs((prev) => [...prev, `System: Pipeline stage set to ${val}.`]);
        } else {
          setLogs((prev) => [...prev, "Usage: /stage <1-3>"]);
        }
      } else if (cmd === "/view") {
        if (arg === "home" || arg === "docs") {
          setView(arg);
          setLogs((prev) => [...prev, `System: Navigating to ${arg.toUpperCase()} view.`]);
        } else {
          setLogs((prev) => [...prev, "Usage: /view <home|docs>"]);
        }
      } else if (cmd === "/stats" || cmd === "stats") {
        const res = typeof window !== "undefined" ? `${window.screen.width}x${window.screen.height}` : "Unknown";
        const ua = typeof navigator !== "undefined" ? navigator.userAgent.split(" ").slice(-2).join(" ") : "Unknown";
        const conn = typeof navigator !== "undefined" && (navigator as unknown as { connection?: { rtt?: number; downlink?: number } }).connection
          ? `RTT: ${(navigator as unknown as { connection: { rtt: number } }).connection.rtt}ms, Speed: ${(navigator as unknown as { connection: { downlink: number } }).connection.downlink}Mbps`
          : "N/A";
        setLogs((prev) => [
          ...prev,
          "--- Browser & System Diagnostics ---",
          `  Resolution  : ${res}`,
          `  UserAgent   : ${ua}`,
          `  Network     : ${conn}`,
          `  App Status  : ONLINE (Ready)`,
          `  Engine      : Next.js 16.2.9 (Turbopack)`
        ]);
      } else if (cmd === "/scan" || cmd === "scan") {
        setLogs((prev) => [
          ...prev,
          "Scanning workspace... [■■■■■■■■■■] 100%",
          "  -> Found 9 modules in Rust workspace",
          "  -> 0 hardcoded secrets detected",
          "  -> 0 clippy warnings",
          "  -> Quality Gate: 94% PASS"
        ]);
      } else if (cmd === "/governance" || cmd === "governance") {
        setLogs((prev) => [
          ...prev,
          "Checking 112 governance rules...",
          "  [RULE_01] No raw unwraps - PASS",
          "  [RULE_02] Secure MCP transport - PASS",
          "  [RULE_03] No hardcoded styles - PASS",
          "Governance Status: SECURE / FAILS OPEN"
        ]);
      } else if (cmd === "/deploy" || cmd === "deploy") {
        setLogs((prev) => [
          ...prev,
          "Triggering local deploy simulator...",
          "  Running subprocess: next build...",
          "  Deploying static bundle to GitHub Pages...",
          "  Success! Domain: umadev.dev/preview"
        ]);
      } else if (cmd === "/proof" || cmd === "proof") {
        setLogs((prev) => [
          ...prev,
          "Compiling Proof Pack...",
          "  Archiving .umadev/audit/session.jsonl...",
          "  Generating scorecard-v1.0.6.html...",
          "  Created delivery: release/proof-pack.zip"
        ]);
      } else {
        setLogs((prev) => [
          ...prev,
          `Error: Command "${raw}" not recognized. Type /help for assistance.`
        ]);
      }
    }, 100);
    
    setInputValue("");
  };

  return (
    <>
      {!isOpen && (
        <button
          className={styles.hudFloatBtn}
          onClick={() => setIsOpen(true)}
          title="Open system diagnostics console"
        >
          <span className={styles.hudPulseLight} />
          <span>SYS_OK: 94%</span>
        </button>
      )}

      {isOpen && (
        <div className={styles.hudOverlayTerm}>
          <div className={styles.hudTermHeader}>
            <span>SYS DIAGNOSTICS</span>
            <button onClick={() => setIsOpen(false)}>×</button>
          </div>
          <div className={styles.hudTermBody} ref={bodyRef}>
            {logs.map((log, i) => (
              <div key={i} className={styles.hudTermLine}>
                {log}
              </div>
            ))}
          </div>
          <form
            onSubmit={(e) => {
              e.preventDefault();
              handleCommand(inputValue);
            }}
            className={styles.hudTermForm}
          >
            <span>&gt;</span>
            <input
              type="text"
              value={inputValue}
              onChange={(e) => setInputValue(e.target.value)}
              placeholder="Type /help..."
              autoFocus
            />
          </form>
        </div>
      )}
    </>
  );
}

const partnerColors: Record<string, { glow: string; border: string }> = {
  yoma: { glow: "rgba(1, 203, 241, 0.22)", border: "#01cbf1" },
  yinghepai: { glow: "rgba(242, 234, 83, 0.18)", border: "#f2ea53" },
  paopai: { glow: "rgba(189, 91, 255, 0.18)", border: "#bd5bff" },
  clawtime: { glow: "rgba(255, 75, 75, 0.18)", border: "#ff4b4b" },
  seeai: { glow: "rgba(50, 205, 50, 0.18)", border: "#32cd32" },
  gopc: { glow: "rgba(0, 191, 255, 0.18)", border: "#00bfff" },
  xinze: { glow: "rgba(255, 105, 180, 0.18)", border: "#ff69b4" },
};

export default function Home({ initialView }: { initialView?: View } = {}) {
  const [lang, setLang] = useState<Lang>("zh");
  const [view, setView] = useState<View>(initialView ?? "home");
  const [wechatOpen, setWechatOpen] = useState(false);

  // Auto language detection based on browser locale
  useEffect(() => {
    /* eslint-disable react-hooks/set-state-in-effect */
    const savedLang = localStorage.getItem("umadev_lang") as Lang | null;
    if (savedLang === "zh" || savedLang === "en") {
      setLang(savedLang);
      return;
    }
    const browserLang = navigator.language || (navigator as unknown as { userLanguage?: string }).userLanguage || "";
    if (browserLang.toLowerCase().includes("zh")) {
      setLang("zh");
    } else {
      setLang("en");
    }
    /* eslint-enable react-hooks/set-state-in-effect */
  }, []);

  // Synchronize language selection to localStorage
  useEffect(() => {
    localStorage.setItem("umadev_lang", lang);
  }, [lang]);

  // Sync initial view from URL pathname and handle browser back/forward buttons
  useEffect(() => {
    /* eslint-disable react-hooks/set-state-in-effect */
    if (typeof window !== "undefined") {
      const pathToView = (path: string): View => {
        const cleanPath = path.replace(process.env.NEXT_PUBLIC_BASE_PATH ?? "", "").replace(/\/$/, "");
        if (cleanPath === "/docs" || cleanPath === "docs") return "docs";
        if (cleanPath === "/gallery" || cleanPath === "gallery") return "gallery";
        if (cleanPath === "/changelog" || cleanPath === "changelog") return "changelog";
        if (cleanPath === "/contributors" || cleanPath === "contributors") return "contributors";
        return "home";
      };

      const currentView = pathToView(window.location.pathname);
      if (!initialView && currentView !== view) {
        setView(currentView);
      }
      
      const handlePopState = () => {
        setView(pathToView(window.location.pathname));
      };
      window.addEventListener("popstate", handlePopState);
      return () => window.removeEventListener("popstate", handlePopState);
    }
    /* eslint-enable react-hooks/set-state-in-effect */
  }, [initialView, view]);

  const [stageIndex, setStageIndex] = useState(0);
  const [mode, setMode] = useState("claude-code");
  const [docId, setDocId] = useState("quickstart");
  const [lightbox, setLightbox] = useState<number | null>(null);
  const [copied, setCopied] = useState(false);
  const [copiedMode, setCopiedMode] = useState<string | null>(null);
  const [showcaseIndex, setShowcaseIndex] = useState(0);
  const [heroSlideIndex, setHeroSlideIndex] = useState(0);
  const copyTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const modeCopyTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const stageButtonRefs = useRef<(HTMLButtonElement | null)[]>([]);

  const t = i18n[lang];
  const activeStage = t.stages[stageIndex] ?? t.stages[0];
  const activeTab = t.modes.tabs.find((tab) => tab.id === mode) ?? t.modes.tabs[0];
  const docCats = docs[lang] as readonly DocCategory[];
  const activeDoc =
    docCats.flatMap((cat) => cat.items).find((item) => item.id === docId) ??
    docCats[0].items[0];

  // Dynamically update document title and description based on language and view
  useEffect(() => {
    let title = "";
    if (view === "home") {
      title = lang === "zh"
        ? "UmaDev - 一支 AI 开发团队 Agent"
        : "UmaDev - a whole AI development team";
    } else if (view === "docs") {
      title = lang === "zh"
        ? "文档中心 | UmaDev - 一支 AI 开发团队 Agent"
        : "Documentation | UmaDev - a whole AI development team";
    } else if (view === "gallery") {
      title = lang === "zh"
        ? "形象相册 | UmaDev - 一支 AI 开发团队 Agent"
        : "Mascot Gallery | UmaDev - a whole AI development team";
    } else if (view === "changelog") {
      title = lang === "zh"
        ? "更新日志 | UmaDev - 一支 AI 开发团队 Agent"
        : "Changelog | UmaDev - a whole AI development team";
    } else if (view === "contributors") {
      title = lang === "zh"
        ? "特别贡献荣誉殿堂 | UmaDev - 一支 AI 开发团队 Agent"
        : "Special Contributors | UmaDev - a whole AI development team";
    }

    document.title = title;

    const descMeta = document.querySelector('meta[name="description"]');
    if (descMeta) {
      descMeta.setAttribute(
        "content",
        lang === "zh"
          ? "UmaDev 是一支 AI 开发团队：产品经理、架构师、设计师、前端、后端、QA、安全、DevOps 八个角色分工协作，借你已登录的 Claude Code / Codex / OpenCode 大脑，把一句需求做成能上线的商业级应用。"
          : "UmaDev is a whole AI development team — eight specialists collaborating like a real team — that borrows your logged-in Claude Code / Codex / OpenCode brain to turn one idea into a shippable, commercial-grade app."
      );
    }
  }, [lang, view]);

  // Docs: scroll-spy — highlight the sidebar entry for the section near the top
  // of the viewport as the reader scrolls (the big-docs pattern).
  useEffect(() => {
    if (view !== "docs") return;
    const sections = Array.from(
      document.querySelectorAll<HTMLElement>("[data-doc-section]"),
    );
    if (sections.length === 0) return;
    const observer = new IntersectionObserver(
      (entries) => {
        const visible = entries
          .filter((e) => e.isIntersecting)
          .sort((a, b) => a.boundingClientRect.top - b.boundingClientRect.top);
        if (visible[0]) setDocId(visible[0].target.id);
      },
      { rootMargin: "-15% 0px -75% 0px" },
    );
    sections.forEach((sec) => observer.observe(sec));
    return () => observer.disconnect();
  }, [view, lang]);

  // Lightbox: Esc closes, arrows navigate, lock background scroll while open.
  useEffect(() => {
    if (lightbox === null) return;
    document.body.style.overflow = "hidden";
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setLightbox(null);
      else if (e.key === "ArrowLeft")
        setLightbox((p) => (p === null ? null : (p - 1 + gallery.length) % gallery.length));
      else if (e.key === "ArrowRight")
        setLightbox((p) => (p === null ? null : (p + 1) % gallery.length));
    };
    window.addEventListener("keydown", onKey);
    return () => {
      document.body.style.overflow = "";
      window.removeEventListener("keydown", onKey);
    };
  }, [lightbox]);

  const heroSlides =
    lang === "zh"
      ? [
          {
            key: "team",
            overline: "一句需求，交给一支团队",
            lines: [
              { text: "把一句需求，交给", accent: false },
              { text: "一支 AI 开发团队", accent: true },
              { text: "做成能上线的商业级应用", accent: false },
            ],
            sub: t.hero.sub,
            visual: "/assets/1_v2.png",
            hud: ["专家角色 / 8", "质量门槛 / 90+", "共用底座 / 1"],
            ticker: ["seat: architect → openapi.json", "seat: frontend → src/", "release/proof-pack.zip"],
          },
          {
            key: "workflow",
            overline: "团队跑完整条交付",
            lines: [
              { text: "产品 · 架构 · 设计 · 前后端", accent: false },
              { text: "QA · 安全 · DevOps", accent: false },
              { text: "一条交付闭环跑通", accent: true },
            ],
            sub: "需求拆清、契约对齐、设计立稳、真建真测、安全审查、打包交付——每个角色交付真实产物，每一步都有状态和可追溯证据。",
            visual: "/assets/2_v2.png",
            hud: ["交付闭环 / 10步", "确认门 / 2", "交付证据 / 开启"],
            ticker: ["phase: docs_confirm", "output/prd.md + architecture.md", ".umadev/audit/session.jsonl"],
          },
          {
            key: "runtime",
            overline: "团队共用你已登录的大脑",
            lines: [
              { text: "全队借你已登录的", accent: false },
              { text: "Claude Code", accent: true },
              { text: "Codex CLI OpenCode", accent: false },
            ],
            sub: "八个角色共用同一个已登录的底座大脑做思考与写码——无需多买一份 API。UmaDev 负责分工、治理、质量门和证据链。",
            visual: "/assets/3_v2.png",
            hud: ["CLAUDE CODE", "CODEX CLI", "OPENCODE"],
            ticker: ["driver: codex exec", "sandbox: workspace-write", "secrets: local only"],
          },
        ]
      : [
          {
            key: "team",
            overline: "One idea, handed to a team",
            lines: [
              { text: "A whole", accent: false },
              { text: "AI development team", accent: true },
              { text: "ships your idea, commercial-grade", accent: false },
            ],
            sub: t.hero.sub,
            visual: "/assets/1_v2.png",
            hud: ["SPECIALISTS / 8", "QUALITY_GATE / 90+", "SHARED BASE / 1"],
            ticker: ["seat: architect → openapi.json", "seat: frontend → src/", "release/proof-pack.zip"],
          },
          {
            key: "workflow",
            overline: "The team runs the whole delivery",
            lines: [
              { text: "PM · architect · design · FE/BE", accent: false },
              { text: "QA · security · DevOps", accent: false },
              { text: "one delivery loop, end to end", accent: true },
            ],
            sub: "Clarify the need, align the contract, lock the design system, build it, test it, audit it, package it — each role ships a real artifact, every step leaves status and traceable proof.",
            visual: "/assets/2_v2.png",
            hud: ["DELIVERY / 10 STEPS", "GATES / 2", "PROOF / ON"],
            ticker: ["phase: docs_confirm", "output/prd.md + architecture.md", ".umadev/audit/session.jsonl"],
          },
          {
            key: "runtime",
            overline: "The team shares your logged-in brain",
            lines: [
              { text: "The whole team borrows your", accent: false },
              { text: "Claude Code", accent: true },
              { text: "Codex CLI OpenCode", accent: false },
            ],
            sub: "All eight roles share the one base brain you already logged into to think and write code — no extra API to buy. UmaDev owns the division of labor, governance, gates and proof.",
            visual: "/assets/3_v2.png",
            hud: ["CLAUDE CODE", "CODEX CLI", "OPENCODE"],
            ticker: ["driver: codex exec", "sandbox: workspace-write", "secrets: local only"],
          },
        ];
  const activeHeroSlide = heroSlides[heroSlideIndex] ?? heroSlides[0];
  const heroTitle = activeHeroSlide.lines.map((line) => line.text).join(" ");
  const titleLines = activeHeroSlide.lines;
  const heroSlideLabels =
    lang === "zh" ? ["团队", "交付", "底座"] : ["Team", "Delivery", "Runtime"];
  const showcaseItems =
    lang === "zh"
      ? [
          {
            key: "clarify",
            tab: "需求澄清",
            kicker: "01 / CLARIFY",
            title: "先问清楚，再让 AI 动手",
            body: "把一句模糊需求拆成目标、边界、角色、验收标准和风险点。需要确认的地方停下来，不需要确认的地方自动推进。",
            image: "/assets/umadev/ip/uma-ip-41.png",
            command: "umadev new booking --mode auto",
            status: "waiting_for_requirements",
            bullets: ["目标 / 范围 / 验收标准", "缺失信息追问", "确认门或自动跳过"],
            files: ["output/<slug>-clarify.md", "output/<slug>-clarify-answers.md"],
          },
          {
            key: "docs",
            tab: "文档生成",
            kicker: "02 / SPEC",
            title: "PRD、架构、UI/UX 一次成套",
            body: "把调研结论转成真实项目文件：产品说明、技术架构、界面方案、任务拆解和契约草案，后续编码全部回链这些产物。",
            image: "/assets/umadev/ip/uma-ip-34.png",
            command: "umadev continue --phase docs",
            status: "writing_prd_architecture_uiux",
            bullets: ["PRD 与验收标准", "架构与接口边界", "UI/UX 页面与组件逻辑"],
            files: ["output/<slug>-prd.md", "output/<slug>-architecture.md", "output/<slug>-uiux.md"],
          },
          {
            key: "quality",
            tab: "质量门",
            kicker: "03 / QUALITY",
            title: "不是写完就交，是过门后交付",
            body: "构建、lint、契约、安全、设计规范、证据链一起检查。默认 90 分通过，失败时回到对应阶段修复。",
            image: "/assets/umadev/ip/uma-ip-36.png",
            command: "umadev gate --threshold 90",
            status: "quality_gate: 94 / 100",
            bullets: ["构建 / 测试 / lint", "契约和安全规则", "proof pack 与成绩单"],
            files: ["output/<slug>-quality-gate.md", "release/proof-pack-*.zip", "release/scorecard-*.html"],
          },
        ]
      : [
          {
            key: "clarify",
            tab: "Clarify",
            kicker: "01 / CLARIFY",
            title: "Clarify first, then let AI act",
            body: "Turn a vague request into goals, boundaries, roles, acceptance criteria and risks. Pause where confirmation is needed, continue where it is not.",
            image: "/assets/umadev/ip/uma-ip-41.png",
            command: "umadev new booking --mode auto",
            status: "waiting_for_requirements",
            bullets: ["Goals / scope / acceptance", "Missing-info questions", "Gate or auto-continue"],
            files: ["output/<slug>-clarify.md", "output/<slug>-clarify-answers.md"],
          },
          {
            key: "docs",
            tab: "Docs",
            kicker: "02 / SPEC",
            title: "PRD, architecture and UI/UX as real files",
            body: "Research becomes project artifacts: product spec, technical architecture, interface plan, task breakdown and contract drafts.",
            image: "/assets/umadev/ip/uma-ip-34.png",
            command: "umadev continue --phase docs",
            status: "writing_prd_architecture_uiux",
            bullets: ["PRD and acceptance", "Architecture and contracts", "UI/UX page logic"],
            files: ["output/<slug>-prd.md", "output/<slug>-architecture.md", "output/<slug>-uiux.md"],
          },
          {
            key: "quality",
            tab: "Quality",
            kicker: "03 / QUALITY",
            title: "Ship after the gate, not after the chat",
            body: "Build, lint, contracts, security, design rules and proof chain are checked together. Default pass line is 90.",
            image: "/assets/umadev/ip/uma-ip-36.png",
            command: "umadev gate --threshold 90",
            status: "quality_gate: 94 / 100",
            bullets: ["Build / test / lint", "Contracts and security", "Proof pack and scorecard"],
            files: ["output/<slug>-quality-gate.md", "release/proof-pack-*.zip", "release/scorecard-*.html"],
          },
        ];
  const showcase = showcaseItems[showcaseIndex] ?? showcaseItems[0];
  const stageProgress = `${Math.round(((stageIndex + 1) / t.stages.length) * 100)}%`;

  useEffect(() => {
    if (view !== "home") return;
    const timer = window.setInterval(() => {
      setHeroSlideIndex((index) => (index + 1) % heroSlides.length);
    }, 6500);
    return () => window.clearInterval(timer);
  }, [heroSlides.length, view]);

  function go(nextView: View) {
    setView(nextView);
    if (typeof window !== "undefined") {
      const base = process.env.NEXT_PUBLIC_BASE_PATH ?? "";
      const targetPath = nextView === "home" ? `${base}/` : `${base}/${nextView}/`;
      if (window.location.pathname !== targetPath) {
        window.history.pushState(null, "", targetPath);
      }
    }
    window.scrollTo({ top: 0, behavior: "smooth" });
  }

  function copyInstall() {
    navigator.clipboard?.writeText("npm install -g umadev").catch(() => undefined);
    setCopied(true);
    if (copyTimerRef.current) clearTimeout(copyTimerRef.current);
    copyTimerRef.current = setTimeout(() => setCopied(false), 1500);
  }

  function copyModeCommand(command: string, key: string) {
    navigator.clipboard?.writeText(command).catch(() => undefined);
    setCopiedMode(key);
    if (modeCopyTimerRef.current) clearTimeout(modeCopyTimerRef.current);
    modeCopyTimerRef.current = setTimeout(() => setCopiedMode(null), 1500);
  }

  function pickStage(index: number) {
    setStageIndex(index);
    stageButtonRefs.current[index]?.scrollIntoView({
      behavior: "smooth",
      block: "nearest",
      inline: "center",
    });
  }

  return (
    <div className={styles.shell}>

      <div className={styles.gridBg} aria-hidden="true" />
      <div className={styles.topGlow} aria-hidden="true" />
      <div className={styles.pointerGlow} aria-hidden="true" />
      <div className={styles.scanlines} aria-hidden="true" />
      <div className={styles.noise} aria-hidden="true" />

      <nav className={styles.nav}>
        <button className={styles.brand} type="button" onClick={() => go("home")}>
          <Image
            className={styles.logo}
            src={asset("/assets/umadev-icon.png")}
            alt={lang === "zh" ? "UmaDev 图标" : "UmaDev logo"}
            width={42}
            height={42}
            priority
          />
          <span>UmaDev</span>
        </button>

        <div className={styles.navLinks}>
          <button className={navClass(view === "home")} type="button" onClick={() => go("home")}>
            <ScrambledHoverText text={t.nav.product} />
          </button>
          <button className={navClass(view === "docs")} type="button" onClick={() => go("docs")}>
            <ScrambledHoverText text={t.nav.docs} />
          </button>
          <button className={navClass(view === "gallery")} type="button" onClick={() => go("gallery")}>
            <ScrambledHoverText text={t.nav.gallery} />
          </button>
          <button
            className={navClass(view === "contributors")}
            type="button"
            onClick={() => go("contributors")}
          >
            <ScrambledHoverText text={t.nav.contributors} />
          </button>
          <button
            className={navClass(view === "changelog")}
            type="button"
            onClick={() => go("changelog")}
          >
            <ScrambledHoverText text={t.nav.changelog} />
          </button>
        </div>

        <div className={styles.navActions}>
          <div className={styles.langSwitch} aria-label="Language switcher">
            <button
              className={lang === "zh" ? styles.langActive : styles.langButton}
              type="button"
              onClick={() => setLang("zh")}
            >
              中
            </button>
            <button
              className={lang === "en" ? styles.langActive : styles.langButton}
              type="button"
              onClick={() => setLang("en")}
            >
              EN
            </button>
          </div>
          <a className={styles.githubButton} href={githubUrl} target="_blank" rel="noreferrer">
            <GitHubIcon />
            GitHub
          </a>
          <button
            className={`${styles.githubButton} ${styles.wechatButton}`}
            type="button"
            onClick={() => setWechatOpen(true)}
          >
            {lang === "zh" ? "微信群" : "WeChat"}
          </button>
        </div>
      </nav>
      {wechatOpen && (
        <div
          className={styles.wechatOverlay}
          onClick={() => setWechatOpen(false)}
          role="dialog"
          aria-modal="true"
        >
          <div className={styles.wechatCard} onClick={(e) => e.stopPropagation()}>
            <button
              className={styles.wechatClose}
              type="button"
              onClick={() => setWechatOpen(false)}
              aria-label={lang === "zh" ? "关闭" : "Close"}
            >
              ×
            </button>
            <h3 className={styles.wechatTitle}>{lang === "zh" ? "官方微信群" : "Official WeChat Group"}</h3>
            <Image
              src="/assets/wechat-qr.png"
              alt={lang === "zh" ? "官方微信群二维码" : "Official WeChat group QR code"}
              width={240}
              height={240}
              className={styles.wechatQr}
            />
            <p className={styles.wechatHint}>
              {lang === "zh" ? "微信扫码加入官方交流群" : "Scan with WeChat to join the official group"}
            </p>
          </div>
        </div>
      )}

      <main className={styles.main}>
        {view === "home" && (
          <>
            <section className={styles.hero}>
              <div className={styles.heroBackdrop} aria-hidden="true">
                <Image
                  src={asset("/assets/umadev/hero-agent-backdrop.png")}
                  alt=""
                  fill
                  priority
                  sizes="100vw"
                />
              </div>
              <div className={styles.heroCopy}>
                <span className={styles.heroOverline}>{activeHeroSlide.overline}</span>
                <div className={styles.badge}>
                  <span className={styles.pulseDot} />
                  {t.hero.badge}
                </div>
                <div className={styles.heroHud}>
                  {activeHeroSlide.hud.map((item) => (
                    <span key={item}>{item}</span>
                  ))}
                </div>
                <h1 className={styles.heroTitle} data-text={heroTitle}>
                  {titleLines.map((line) => (
                    <span
                      className={line.accent ? styles.titleAccentLine : styles.titleLine}
                      key={line.text}
                    >
                      {line.text}
                    </span>
                  ))}
                </h1>
                <p>{activeHeroSlide.sub}</p>

                <button className={styles.installCommand} type="button" onClick={copyInstall}>
                  <span className={styles.promptMark}>$</span>
                  <code>npm install -g umadev</code>
                  <span className={styles.copyPill}>{copied ? t.hero.copied : t.hero.copy}</span>
                </button>

                <div className={styles.heroActions}>
                  <a href={githubUrl} target="_blank" rel="noreferrer">
                    <ScrambledHoverText text={t.hero.cta1} />
                    <span aria-hidden="true">→</span>
                  </a>
                  <button type="button" onClick={() => go("docs")}>
                    <ScrambledHoverText text={t.hero.cta2} />
                  </button>
                </div>

                <dl className={styles.stats}>
                  {t.hero.stats.map(([value, label]) => (
                    <div key={label}>
                      <dt>{value}</dt>
                      <dd>{label}</dd>
                    </div>
                  ))}
                </dl>

              </div>

              <div className={styles.heroVisual}>
                <div className={styles.productFrame}>
                  <Image
                    className={styles.productShot}
                    src={asset(activeHeroSlide.visual)}
                    alt=""
                    width={832}
                    height={520}
                    priority
                    aria-hidden="true"
                  />
                </div>
                <Image
                  className={styles.heroMark}
                  src={asset("/assets/umadev/neon-logo-cut.png")}
                  alt=""
                  width={760}
                  height={760}
                  priority
                  aria-hidden="true"
                />
                <div className={styles.codeTicker} aria-hidden="true">
                  {activeHeroSlide.ticker.map((item) => (
                    <span key={item}>{item}</span>
                  ))}
                </div>
              </div>

              <div className={styles.heroCarouselControls} aria-label="Hero carousel">
                {heroSlides.map((slide, index) => (
                  <button
                    key={slide.key}
                    type="button"
                    aria-label={`Show ${slide.overline}`}
                    data-state={index < heroSlideIndex ? "past" : index === heroSlideIndex ? "active" : "future"}
                    onClick={() => setHeroSlideIndex(index)}
                  >
                    {index === heroSlideIndex && (
                      <svg className={styles.carouselBtnBorder} aria-hidden="true">
                        <rect
                          x="1"
                          y="1"
                          rx="18"
                          ry="18"
                          style={{ width: "calc(100% - 2px)", height: "calc(100% - 2px)" }}
                          pathLength="100"
                        />
                      </svg>
                    )}
                    <span>{String(index + 1).padStart(2, "0")}</span>
                    {heroSlideLabels[index]}
                  </button>
                ))}
              </div>
            </section>

            {/* Cooperative Communities Section */}
            <section className={styles.partnersSection}>
              <div className={styles.partnersContainer}>
                <span className={styles.partnersEyebrow}>{t.partners.eyebrow}</span>
                <h2 className={styles.partnersTitle}>{t.partners.title}</h2>
                <div className={styles.partnersGrid}>
                  {t.partners.list.map((partner, idx) => {
                    const colors = partnerColors[partner.logoName] || { glow: "rgba(255,255,255,0.08)", border: "rgba(255,255,255,0.2)" };
                    return (
                      <a
                        key={idx}
                        href={partner.url}
                        target={partner.url === "#" ? undefined : "_blank"}
                        rel={partner.url === "#" ? undefined : "noopener noreferrer"}
                        className={styles.partnerCard}
                        onClick={(e) => {
                          if (partner.url === "#") {
                            e.preventDefault();
                          }
                        }}
                        style={{
                          "--partner-glow": colors.glow,
                          "--partner-border": colors.border,
                        } as React.CSSProperties}
                      >
                        <div className={styles.partnerGlow} />
                        <div className={styles.partnerLogoContainer}>
                          <Image
                            src={asset(`/assets/partners/${partner.logoName}.png?v=4`)}
                            alt={partner.name}
                            width={180}
                            height={80}
                            style={{ objectFit: "contain", width: "auto", height: "auto", maxWidth: "100%", maxHeight: "100%" }}
                          />
                        </div>
                        <div className={styles.partnerTextContainer}>
                          <span className={styles.partnerCardName}>{partner.name}</span>
                          <span className={styles.partnerCardRole}>{partner.role}</span>
                        </div>
                      </a>
                    );
                  })}
                </div>
              </div>
            </section>

            <section className={styles.premiumShowcase}>
              <div className={styles.showcaseBrand}>
                <Image
                  src={asset("/assets/umadev-icon.png")}
                  alt=""
                  width={34}
                  height={34}
                  aria-hidden="true"
                />
                <span>UMADEV</span>
              </div>
              <div className={styles.showcaseMascot} aria-hidden="true">
                <Image
                  src={asset("/assets/mascot-thumb.png")}
                  alt=""
                  width={520}
                  height={520}
                />
              </div>
              <div className={styles.showcaseCopy}>
                <div className={styles.showcaseTabs}>
                  {showcaseItems.map((item, index) => (
                    <button
                      key={item.key}
                      type="button"
                      data-active={index === showcaseIndex ? "true" : undefined}
                      onClick={() => setShowcaseIndex(index)}
                    >
                      <span>{String(index + 1).padStart(2, "0")}</span>
                      <ScrambledHoverText text={item.tab} />
                    </button>
                  ))}
                </div>
                <p className={styles.showcaseKicker}>{showcase.kicker}</p>
                <h2>{showcase.title}</h2>
                <p>{showcase.body}</p>
                <ul>
                  {showcase.bullets.map((bullet) => (
                    <li key={bullet}>{bullet}</li>
                  ))}
                </ul>
                <div className={styles.showcaseTerminal}>
                  <code>$ {showcase.command}</code>
                  <span>{showcase.status}</span>
                </div>
              </div>
              <div className={styles.showcaseScreen}>
                <TerminalDemo key={showcaseIndex} slideIndex={showcaseIndex} lang={lang} />
                <div className={styles.showcaseNote}>{lang === "zh" ? "团队协作模式" : "TEAM AT WORK"}</div>
                <div className={styles.showcaseFiles}>
                  {showcase.files.map((file) => (
                    <code key={file}>{file}</code>
                  ))}
                </div>
              </div>
            </section>

            <section className={styles.trust}>
              <p>{t.trust}</p>
              <div>
                {t.backends.map((backend) => (
                  <span key={backend}>{backend}</span>
                ))}
              </div>
            </section>

            <section className={styles.mascotRoster}>
              <div className={styles.mascotIntro}>
                <span>{`// ${t.mascots.eyebrow}`}</span>
                <h2>{t.mascots.title}</h2>
                <p>{t.mascots.desc}</p>
                <div className={styles.mascotLead}>
                  <Image
                    src={asset(t.mascots.lead.img)}
                    alt=""
                    width={56}
                    height={56}
                    aria-hidden="true"
                  />
                  <div>
                    <small>{t.mascots.lead.role}</small>
                    <p>{t.mascots.lead.desc}</p>
                  </div>
                </div>
              </div>
              <div className={styles.mascotRail}>
                {t.mascots.cards.map((card, index) => (
                  <article className={styles.mascotCard} key={card.title}>
                    <div className={styles.mascotImageWrapper}>
                      <Image src={asset(card.img)} alt={card.title} width={360} height={360} />
                      {card.type === "doer" && (
                        <span className={styles.mascotTypeTagDoer}>Doer · Serial Write</span>
                      )}
                      {card.type === "critic" && (
                        <span className={styles.mascotTypeTagCritic}>Critic · Parallel Review</span>
                      )}
                    </div>
                    <div>
                      <small>
                        {String(index + 1).padStart(2, "0")} / {card.role}
                      </small>
                      <h3>{card.title}</h3>
                      <div className={styles.mascotProduces}>
                        <span>{t.mascots.deliversLabel}</span>
                        <code>{card.produces}</code>
                      </div>
                      <p>{card.desc}</p>
                      {card.details && (
                        <ul className={styles.mascotDetails}>
                          {card.details.map((detail) => (
                            <li key={detail}>{detail}</li>
                          ))}
                        </ul>
                      )}
                    </div>
                  </article>
                ))}
              </div>
            </section>

            <SectionIntro eyebrow={t.flow.eyebrow} title={t.flow.title} desc={t.flow.desc} />
            <section className={styles.layers}>
              {t.flow.layers.map((layer, index) => (
                <article key={layer.k}>
                  <span>0{index + 1}</span>
                  <h3>{layer.k}</h3>
                  <p>{layer.d}</p>
                </article>
              ))}
            </section>

            <SectionIntro eyebrow={t.pipe.eyebrow} title={t.pipe.title} desc={t.pipe.desc} />
            <section className={styles.pipeline}>
              <div className={styles.stageList}>
                {t.stages.map((stage, index) => (
                  <button
                    className={index === stageIndex ? styles.stageActive : styles.stageButton}
                    key={stage.key}
                    ref={(node) => {
                      stageButtonRefs.current[index] = node;
                    }}
                    type="button"
                    onClick={() => pickStage(index)}
                  >
                    <span>{stage.n}</span>
                    <strong>{stage.label}</strong>
                    {stage.gate && <em>{t.pipe.gate}</em>}
                  </button>
                ))}
              </div>
              <article className={styles.stageDetail}>
                <div className={styles.stageProgress} aria-hidden="true">
                  <span style={{ width: stageProgress }} />
                </div>
                <div className={styles.stageHeader}>
                  <span>{activeStage.n}</span>
                  <div>
                    <small>{activeStage.key}</small>
                    <h3>{activeStage.label}</h3>
                  </div>
                </div>
                <p>{activeStage.role}</p>
                <h4>{t.pipe.filesLabel}</h4>
                <div className={styles.fileList}>
                  {activeStage.files.map((file) => (
                    <code key={file}>› {file}</code>
                  ))}
                </div>
              </article>
            </section>

            <SectionIntro eyebrow={t.modes.eyebrow} title={t.modes.title} desc={t.modes.desc} />
            <section className={styles.modes}>
              <article className={styles.modePanel}>
                <div className={styles.tabs}>
                  {t.modes.tabs.map((tab) => (
                    <button
                      className={tab.id === mode ? styles.tabActive : styles.tab}
                      key={tab.id}
                      type="button"
                      onClick={() => setMode(tab.id)}
                    >
                      {tab.name}
                    </button>
                  ))}
                </div>
                <small>{t.modes.callLabel}</small>
                <button
                  className={styles.modeCommand}
                  type="button"
                  onClick={() => copyModeCommand(activeTab.cmd, activeTab.id)}
                >
                  <code>
                    <span>$ </span>
                    <b>{activeTab.bin}</b> {activeTab.cmd.replace(activeTab.bin, "").trim()}
                  </code>
                  <em>{copiedMode === activeTab.id ? t.hero.copied : t.hero.copy}</em>
                </button>
                <small>{t.modes.whoLabel}</small>
                <p>{activeTab.who}</p>
              </article>
              <div className={styles.modeCards}>
                {t.modes.cards.map((card) => (
                  <article key={card.title}>
                    <header>
                      <h3>{card.title}</h3>
                      <button
                        className={styles.modeCardCommand}
                        type="button"
                        onClick={() => copyModeCommand(card.cmd, card.cmd)}
                      >
                        <code>{card.cmd}</code>
                        <span>{copiedMode === card.cmd ? t.hero.copied : t.hero.copy}</span>
                      </button>
                    </header>
                    <p>{card.desc}</p>
                  </article>
                ))}
              </div>
              <div className={styles.modeNotes}>
                {t.modes.notes.map((note) => (
                  <span key={note}>✓ {note}</span>
                ))}
              </div>
            </section>

            <SectionIntro eyebrow={t.gov.eyebrow} title={t.gov.title} desc={t.gov.desc} />
            <section className={styles.govGrid}>
              {t.gov.blocks.map((block) => (
                <article key={block.title}>
                  <div>
                    <strong>{block.stat}</strong>
                    <span>{block.unit}</span>
                  </div>
                  <h3>{block.title}</h3>
                  <p>{block.desc}</p>
                  <ul>
                    {block.bullets.map((bullet) => (
                      <li key={bullet}>{bullet}</li>
                    ))}
                  </ul>
                </article>
              ))}
            </section>
            <div className={styles.compliance}>
              <span>{t.gov.compliance}</span>
              {t.gov.standards.map((standard) => (
                <code key={standard}>{standard}</code>
              ))}
            </div>

            <section className={styles.brandIp}>
              <div>
                <span>{t.ip.eyebrow}</span>
                <h2>{t.ip.title}</h2>
                <p>{t.ip.desc}</p>
              </div>
              <div className={styles.ipCards}>
                {t.ip.cards.map((card, index) => (
                  <figure key={card.cap}>
                    <Image
                      src={asset(card.img)}
                      alt={card.cap}
                      width={390}
                      height={390}
                      loading={index === 0 ? undefined : "eager"}
                      priority={index === 0}
                      sizes="(max-width: 720px) 100vw, (max-width: 1080px) 50vw, 32vw"
                    />
                    <figcaption>{card.cap}</figcaption>
                  </figure>
                ))}
              </div>
            </section>

            <section className={styles.cta}>
              <div>
                <h2>{t.cta.title}</h2>
                <p>{t.cta.sub}</p>
                <div>
                  <a href={githubUrl} target="_blank" rel="noreferrer">
                    {t.cta.btn1} →
                  </a>
                  <button type="button" onClick={() => go("docs")}>
                    {t.cta.btn2}
                  </button>
                </div>
              </div>
              <button className={styles.ctaCommand} type="button" onClick={copyInstall}>
                <code>{t.cta.note}</code>
                <span>{copied ? t.hero.copied : t.hero.copy}</span>
              </button>
            </section>
          </>
        )}

        {view === "docs" && (
          <section className={styles.docsPage}>
            <PageHero title={t.docsPage.title} sub={t.docsPage.sub} />
            <div className={styles.docsLayout}>
              <aside className={styles.docsNav}>
                {docCats.map((cat) => (
                  <div key={cat.cat}>
                    <h3>{cat.cat}</h3>
                    {cat.items.map((item) => (
                      <button
                        className={item.id === activeDoc.id ? styles.docActive : styles.docLink}
                        key={item.id}
                        type="button"
                        onClick={() => {
                          setDocId(item.id);
                          document
                            .getElementById(item.id)
                            ?.scrollIntoView({ behavior: "smooth", block: "start" });
                        }}
                      >
                        {item.title}
                      </button>
                    ))}
                  </div>
                ))}
              </aside>
              <article className={styles.docArticle}>
                {docCats
                  .flatMap((cat) => cat.items)
                  .map((item) => (
                    <section
                      className={styles.docSection}
                      data-doc-section
                      id={item.id}
                      key={item.id}
                    >
                      <h2>{item.title}</h2>
                      {item.blocks.map((block, index) => (
                        <DocBlockView block={block} key={index} />
                      ))}
                    </section>
                  ))}
              </article>
            </div>
          </section>
        )}

        {view === "changelog" && (
          <section className={styles.logPage}>
            <PageHero title={t.logPage.title} sub={t.logPage.sub} />
            <div className={styles.releaseList}>
              {releases[lang].map((release) => (
                <article key={release.ver}>
                  <header>
                    <div>
                      <span>{release.ver}</span>
                      <time>{release.date}</time>
                      {"current" in release && release.current && <em>{t.logPage.current}</em>}
                    </div>
                    <h2>{release.title}</h2>
                  </header>
                  <ul>
                    {release.changes.map(([tag, text]) => (
                      <li key={`${tag}-${text}`}>
                        <span className={tagClass(tag)}>{tag}</span>
                        <p>{text}</p>
                      </li>
                    ))}
                  </ul>
                </article>
              ))}
            </div>
          </section>
        )}

        {view === "gallery" && (
          <section className={styles.docsPage}>
            <PageHero title={t.galleryPage.title} sub={t.galleryPage.sub} />
            <div className={styles.galleryGrid}>
              {gallery.map((src, i) => (
                <button className={styles.galleryItem} key={src} onClick={() => setLightbox(i)} type="button">
                  <Image alt={`UmaDev IP ${i + 1}`} className={styles.galleryImg} height={420} src={src} unoptimized width={420} />
                </button>
              ))}
            </div>
          </section>
        )}

        {view === "contributors" && (
          <section className={styles.contributorsPage}>
            <PageHero title={t.contributorsPage.title} sub={t.contributorsPage.sub} />
            
            <div className={styles.contributorsMatrix}>
              {[
                { ...t.contributors.featured, isVip: true },
                ...t.contributors.list.map((item) => ({ ...item, isVip: false }))
              ].map((member) => (
                <div
                  key={member.avatarKey}
                  className={styles.matrixCard}
                  data-vip={member.isVip ? "true" : undefined}
                >
                  <div className={styles.matrixCardGlow} />
                  {member.isVip && <div className={styles.matrixVipBadge}>#1 核心贡献</div>}
                  <div className={styles.matrixAvatarWrapper}>
                    <Image
                      src={asset(`/assets/contributors/${member.avatarKey}.png`)}
                      alt={member.name}
                      width={88}
                      height={88}
                      className={styles.matrixAvatarImg}
                    />
                  </div>
                  <div className={styles.matrixTextInfo}>
                    <span className={styles.matrixName}>{member.name}</span>
                    <span className={styles.matrixRole}>{member.role}</span>
                  </div>
                </div>
              ))}
            </div>
          </section>
        )}
      </main>

      {lightbox !== null && (
        <div aria-modal="true" className={styles.lightbox} onClick={() => setLightbox(null)} role="dialog">
          <button aria-label="Close" className={styles.lightboxClose} onClick={() => setLightbox(null)} type="button">
            ×
          </button>
          <button
            aria-label="Previous"
            className={styles.lightboxNav}
            data-side="prev"
            onClick={(e) => {
              e.stopPropagation();
              setLightbox((p) => (p === null ? null : (p - 1 + gallery.length) % gallery.length));
            }}
            type="button"
          >
            ‹
          </button>
          <Image
            alt={`UmaDev IP ${lightbox + 1}`}
            className={styles.lightboxImg}
            height={1100}
            onClick={(e) => e.stopPropagation()}
            src={gallery[lightbox]}
            unoptimized
            width={1100}
          />
          <button
            aria-label="Next"
            className={styles.lightboxNav}
            data-side="next"
            onClick={(e) => {
              e.stopPropagation();
              setLightbox((p) => (p === null ? null : (p + 1) % gallery.length));
            }}
            type="button"
          >
            ›
          </button>
          <span className={styles.lightboxCount}>
            {lightbox + 1} / {gallery.length}
          </span>
        </div>
      )}

      <footer className={styles.footer}>
        <div>
          <div className={styles.footerBrand}>
            <Image
              className={styles.logo}
              src={asset("/assets/umadev-icon.png")}
              alt={lang === "zh" ? "UmaDev 图标" : "UmaDev logo"}
              width={42}
              height={42}
            />
            <strong>UmaDev</strong>
          </div>
          <p>{t.footer.blurb}</p>
        </div>
        {t.footer.cols.map((col) => (
          <nav key={col.h}>
            <h3>{col.h}</h3>
            {col.links.map((link) =>
              "u" in link ? (
                <a key={link.t} href={link.u} target="_blank" rel="noreferrer">
                  {link.t}
                </a>
              ) : (
                <button key={link.t} type="button" onClick={() => go(col.h === "文档" || col.h === "Docs" ? "docs" : "home")}>
                  {link.t}
                </button>
              ),
            )}
          </nav>
        ))}
        <small>{t.footer.rights}</small>
      </footer>

      {/* Floating System Diagnostic Console */}
      <FloatingDiagnosticTerminal
        setLang={setLang}
        setView={setView}
        setHeroSlideIndex={setHeroSlideIndex}
        setMode={setMode}
        setStageIndex={setStageIndex}
      />
    </div>
  );

  function navClass(active: boolean) {
    return active ? styles.navActive : styles.navButton;
  }

  function tagClass(tag: string) {
    const map: Record<string, string> = {
      新增: styles.tagAdded,
      Added: styles.tagAdded,
      改进: styles.tagImproved,
      Improved: styles.tagImproved,
      安全: styles.tagSecurity,
      Security: styles.tagSecurity,
      修复: styles.tagFixed,
      Fixed: styles.tagFixed,
      平台: styles.tagPlatform,
      Platform: styles.tagPlatform,
    };
    return `${styles.releaseTag} ${map[tag] ?? styles.tagImproved}`;
  }
}

function SectionIntro({ eyebrow, title, desc }: { eyebrow: string; title: string; desc: string }) {
  return (
    <section className={styles.sectionIntro}>
      <span>{`// ${eyebrow}`}</span>
      <h2>{title}</h2>
      <p>{desc}</p>
    </section>
  );
}

function PageHero({ title, sub }: { title: string; sub: string }) {
  return (
    <header className={styles.pageHero}>
      <span>UmaDev</span>
      <h1>{title}</h1>
      <p>{sub}</p>
    </header>
  );
}

function DocBlockView({ block }: { block: DocBlock }) {
  if ("h" in block) return <h3 className={styles.docHeading}>{block.h}</h3>;
  if ("p" in block) return <p className={styles.docPara}>{block.p}</p>;
  if ("c" in block) return <pre className={styles.docCode}>{block.c}</pre>;
  if ("l" in block) {
    return (
      <ul className={styles.docList}>
        {block.l.map((item) => (
          <li key={item}>{item}</li>
        ))}
      </ul>
    );
  }
  return (
    <div className={styles.cmdTable}>
      {block.cmds.map(([cmd, desc]) => (
        <div key={cmd}>
          <code>{cmd}</code>
          <span>{desc}</span>
        </div>
      ))}
    </div>
  );
}

function GitHubIcon() {
  return (
    <svg aria-hidden="true" width="16" height="16" viewBox="0 0 16 16" fill="currentColor">
      <path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0 0 16 8c0-4.42-3.58-8-8-8z" />
    </svg>
  );
}
