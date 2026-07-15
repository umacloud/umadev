"use client";

import Image from "next/image";
import React, { useEffect, useRef, useState } from "react";
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
export default function Home({ initialView }: { initialView?: View } = {}) {
  const [lang, setLang] = useState<Lang>("zh");
  const [view, setView] = useState<View>(initialView ?? "home");
  const [wechatOpen, setWechatOpen] = useState(false);
  const [activeStageIdx, setActiveStageIdx] = useState(0);
  const [autoplay, setAutoplay] = useState(true);

  useEffect(() => {
    if (!autoplay) return;
    const interval = setInterval(() => {
      setActiveStageIdx((prev) => (prev + 1) % 10);
    }, 4000);
    return () => clearInterval(interval);
  }, [autoplay]);

  const handleTiltMove = (e: React.MouseEvent<HTMLElement>) => {
    const c = e.currentTarget;
    const r = c.getBoundingClientRect();
    const px = (e.clientX - r.left) / r.width - 0.5;
    const py = (e.clientY - r.top) / r.height - 0.5;
    c.style.setProperty("--tilt-x", `${-py * 5}deg`);
    c.style.setProperty("--tilt-y", `${px * 6}deg`);
    c.style.setProperty("--spot-x", `${(px + 0.5) * 100}%`);
    c.style.setProperty("--spot-y", `${(py + 0.5) * 100}%`);
    c.style.transform = `perspective(700px) rotateX(${-py * 8}deg) rotateY(${px * 8}deg) translateZ(10px) translateY(-5px)`;
    c.style.transition = "transform 0.1s cubic-bezier(0.25, 0.8, 0.25, 1)";
  };
  const handleTiltLeave = (e: React.MouseEvent<HTMLElement>) => {
    const c = e.currentTarget;
    c.style.setProperty("--tilt-x", "0deg");
    c.style.setProperty("--tilt-y", "0deg");
    c.style.setProperty("--spot-x", "50%");
    c.style.setProperty("--spot-y", "50%");
    c.style.transform = "perspective(700px) rotateX(0) rotateY(0) translateZ(0) translateY(0)";
    c.style.transition = "transform 0.4s cubic-bezier(0.25, 0.8, 0.25, 1)";
  };

  const handleMagnetMove = (e: React.MouseEvent<HTMLElement>) => {
    const btn = e.currentTarget;
    const r = btn.getBoundingClientRect();
    const mx = e.clientX - r.left - r.width / 2;
    const my = e.clientY - r.top - r.height / 2;
    btn.style.transform = `translate(${mx * 0.35}px, ${my * 0.4}px) scale(1.04)`;
    btn.style.transition = "transform 0.1s cubic-bezier(.16,1,.3,1)";
  };
  const handleMagnetLeave = (e: React.MouseEvent<HTMLElement>) => {
    const btn = e.currentTarget;
    btn.style.transform = "translate(0,0) scale(1)";
    btn.style.transition = "transform 0.4s cubic-bezier(.16,1,.3,1)";
  };

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

  // Keep persisted preference and document semantics aligned with the visible copy.
  useEffect(() => {
    localStorage.setItem("umadev_lang", lang);
    document.documentElement.lang = lang === "zh" ? "zh-CN" : "en";
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

  const scrollProgressRef = useRef<HTMLDivElement | null>(null);
  const [docId, setDocId] = useState("quickstart");
  const [lightbox, setLightbox] = useState<number | null>(null);
  const [copied, setCopied] = useState(false);
  const copyTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  // Spotlight, tilt, magnetic, and scroll reveal animations from Umadevweb
  useEffect(() => {
    // 1. Magnetic Buttons
    const magnets = document.querySelectorAll(`.${styles.umaMagnet}`);
    const magnetCleanups: (() => void)[] = [];
    magnets.forEach((node) => {
      const btn = node as HTMLElement;
      const handleMove = (e: MouseEvent) => {
        const r = btn.getBoundingClientRect();
        const mx = e.clientX - r.left - r.width / 2;
        const my = e.clientY - r.top - r.height / 2;
        btn.style.transform = `translate(${mx * 0.28}px, ${my * 0.32}px)`;
      };
      const handleLeave = () => {
        btn.style.transform = "translate(0,0)";
      };
      btn.addEventListener("mousemove", handleMove);
      btn.addEventListener("mouseleave", handleLeave);
      btn.style.transition = "transform .18s cubic-bezier(.16,1,.3,1)";
      magnetCleanups.push(() => {
        btn.removeEventListener("mousemove", handleMove);
        btn.removeEventListener("mouseleave", handleLeave);
      });
    });

    // 2. Tilt Cards
    const tilts = document.querySelectorAll(`.${styles.tilt}`);
    const tiltCleanups: (() => void)[] = [];
    tilts.forEach((node) => {
      const c = node as HTMLElement;
      const handleMove = (e: MouseEvent) => {
        const r = c.getBoundingClientRect();
        const px = (e.clientX - r.left) / r.width - 0.5;
        const py = (e.clientY - r.top) / r.height - 0.5;
        c.style.transform = `perspective(700px) rotateX(${-py * 7}deg) rotateY(${px * 7}deg) translateZ(6px)`;
      };
      const handleLeave = () => {
        c.style.transform = "perspective(700px) rotateX(0) rotateY(0)";
      };
      c.addEventListener("mousemove", handleMove);
      c.addEventListener("mouseleave", handleLeave);
      c.style.transition = "transform .18s ease";
      tiltCleanups.push(() => {
        c.removeEventListener("mousemove", handleMove);
        c.removeEventListener("mouseleave", handleLeave);
      });
    });

    // 3. Scroll Reveal
    const observer = new IntersectionObserver(
      (entries) => {
        entries.forEach((entry) => {
          if (entry.isIntersecting) {
            entry.target.classList.add(styles.in);
            observer.unobserve(entry.target);
          }
        });
      },
      { threshold: 0.12 }
    );
    const reveals = document.querySelectorAll(`.${styles.reveal}`);
    reveals.forEach((r) => observer.observe(r));

    // 4. Count up animation
    const countObserver = new IntersectionObserver(
      (entries) => {
        entries.forEach((entry) => {
          if (!entry.isIntersecting) return;
          const el = entry.target as HTMLElement;
          const target = +(el.getAttribute("data-count") || "0");
          let t0: number | null = null;
          const step = (ts: number) => {
            if (!t0) t0 = ts;
            const progress = Math.min((ts - t0) / 1400, 1);
            const eased = 1 - Math.pow(1 - progress, 3);
            el.textContent = String(Math.round(target * eased));
            if (progress < 1) requestAnimationFrame(step);
          };
          requestAnimationFrame(step);
          countObserver.unobserve(el);
        });
      },
      { threshold: 0.5 }
    );
    const counters = document.querySelectorAll(`[data-count]`);
    counters.forEach((el) => countObserver.observe(el));

    return () => {
      magnetCleanups.forEach((c) => c());
      tiltCleanups.forEach((c) => c());
      observer.disconnect();
      countObserver.disconnect();
    };
  }, [view]);

  useEffect(() => {
    let raf = 0;
    const updateScrollProgress = () => {
      if (raf) return;
      raf = requestAnimationFrame(() => {
        const max = document.documentElement.scrollHeight - window.innerHeight;
        const progress = max > 0 ? Math.min(window.scrollY / max, 1) : 0;
        scrollProgressRef.current?.style.setProperty("--scroll-progress", String(progress));
        raf = 0;
      });
    };
    updateScrollProgress();
    window.addEventListener("scroll", updateScrollProgress, { passive: true });
    window.addEventListener("resize", updateScrollProgress);
    return () => {
      window.removeEventListener("scroll", updateScrollProgress);
      window.removeEventListener("resize", updateScrollProgress);
      cancelAnimationFrame(raf);
    };
  }, [view]);

  const t = i18n[lang];
  const docCats = docs[lang] as readonly DocCategory[];
  const activeDoc =
    docCats.flatMap((cat) => cat.items).find((item) => item.id === docId) ??
    docCats[0].items[0];

  // Dynamically update document title and description based on language and view
  useEffect(() => {
    let title = "";
    if (view === "home") {
      title = lang === "zh"
        ? "UmaDev — 一个模拟真实开发团队、驱动你的底座干活的 Agent"
        : "UmaDev — One agent. A whole development team at work.";
    } else if (view === "docs") {
      title = lang === "zh"
        ? "文档中心 | UmaDev 真实开发团队 Agent"
        : "Documentation | UmaDev real dev team agent";
    } else if (view === "gallery") {
      title = lang === "zh"
        ? "形象相册 | UmaDev"
        : "Mascot Gallery | UmaDev";
    } else if (view === "changelog") {
      title = lang === "zh"
        ? "更新日志 | UmaDev"
        : "Changelog | UmaDev";
    } else if (view === "contributors") {
      title = lang === "zh"
        ? "荣誉殿堂 | UmaDev"
        : "Hall of Fame | UmaDev";
    }

    document.title = title;

    const descMeta = document.querySelector('meta[name="description"]');
    if (descMeta) {
      descMeta.setAttribute(
        "content",
        lang === "zh"
          ? "UmaDev 驱动你已登录的 Claude Code、Codex 或 OpenCode，由八个专家角色完成任务路由、可视计划、独立评审、持续上下文、真实验证与交付证明。"
          : "UmaDev drives your logged-in Claude Code, Codex, or OpenCode with task routing, a visible plan, eight specialist roles, independent review, persistent context, deterministic verification, and delivery evidence."
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

  return (
    <div className={styles.shell}>
      <div className={styles.scrollProgress} ref={scrollProgressRef} aria-hidden="true"><i /></div>
      <div className={styles.gridBg} aria-hidden="true" />
      <div className={styles.topGlow} aria-hidden="true" />
      <div className={styles.pointerGlow} aria-hidden="true" />
      <div className={styles.scanlines} aria-hidden="true" />
      <div className={styles.noise} aria-hidden="true" />

      <nav className={styles.spaceNav}>
        <button className={styles.spaceNavBrand} type="button" onClick={() => go("home")}>
          <Image
            alt="UmaDev"
            className={styles.spaceNavBrandLogo}
            height={36}
            priority
            src={asset("/assets/umadev-icon.png")}
            width={36}
          />
          <span className={styles.spaceNavBrandText}>UmaDev</span>
        </button>

        <div className={styles.spaceNavLinks}>
          <button className={view === "home" ? styles.spaceNavLinkActive : styles.spaceNavLink} type="button" onClick={() => go("home")}>
            {t.nav.product}
          </button>
          <button className={view === "docs" ? styles.spaceNavLinkActive : styles.spaceNavLink} type="button" onClick={() => go("docs")}>
            {t.nav.docs}
          </button>
          <button className={view === "gallery" ? styles.spaceNavLinkActive : styles.spaceNavLink} type="button" onClick={() => go("gallery")}>
            {t.nav.gallery}
          </button>
          <button className={view === "contributors" ? styles.spaceNavLinkActive : styles.spaceNavLink} type="button" onClick={() => go("contributors")}>
            {t.nav.contributors}
          </button>
          <button className={view === "changelog" ? styles.spaceNavLinkActive : styles.spaceNavLink} type="button" onClick={() => go("changelog")}>
            {t.nav.changelog}
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
          <a className={`${styles.spaceNavCta} ${styles.spaceNavGithub} ${styles.umaMagnet}`} href={githubUrl} target="_blank" rel="noreferrer">
            <svg aria-hidden="true" viewBox="0 0 24 24">
              <path d="M12 2.4a9.8 9.8 0 0 0-3.1 19.1c.5.1.7-.2.7-.5v-1.9c-2.8.6-3.4-1.2-3.4-1.2-.5-1.2-1.1-1.5-1.1-1.5-.9-.6.1-.6.1-.6 1 0 1.6 1.1 1.6 1.1.9 1.6 2.4 1.1 2.9.8.1-.7.4-1.1.7-1.3-2.2-.3-4.6-1.1-4.6-4.9 0-1.1.4-2 1-2.7-.1-.3-.4-1.3.1-2.7 0 0 .8-.3 2.7 1a9.4 9.4 0 0 1 4.9 0c1.9-1.3 2.7-1 2.7-1 .5 1.4.2 2.4.1 2.7.6.7 1 1.6 1 2.7 0 3.8-2.3 4.6-4.6 4.9.4.3.7.9.7 1.8V21c0 .3.2.6.7.5A9.8 9.8 0 0 0 12 2.4Z" />
            </svg>
            <span>GitHub</span>
            <i aria-hidden="true">↗</i>
          </a>
          <button
            className={`${styles.spaceNavCta} ${styles.spaceNavWechat} ${styles.umaMagnet}`}
            type="button"
            onClick={() => setWechatOpen(true)}
          >
            <svg aria-hidden="true" viewBox="0 0 24 24">
              <path d="M9.3 4.2c-4 0-7.2 2.6-7.2 5.8 0 1.8 1 3.4 2.7 4.5l-.7 2.4 2.7-1.3c.8.2 1.6.3 2.5.3h.5a5.4 5.4 0 0 1-.2-1.5c0-3.5 3.3-6.3 7.4-6.3h.3c-1-2.3-4.1-3.9-8-3.9Zm-2.4 3a.9.9 0 1 1 0 1.8.9.9 0 0 1 0-1.8Zm4.8 0a.9.9 0 1 1 0 1.8.9.9 0 0 1 0-1.8Z" />
              <path d="M17 9.5c-3.4 0-6.2 2.2-6.2 4.9s2.8 4.9 6.2 4.9c.7 0 1.4-.1 2.1-.3l2.3 1.1-.6-2a4.6 4.6 0 0 0 2.2-3.7c0-2.7-2.7-4.9-6-4.9Zm-2.1 2.6a.8.8 0 1 1 0 1.6.8.8 0 0 1 0-1.6Zm4.1 0a.8.8 0 1 1 0 1.6.8.8 0 0 1 0-1.6Z" />
            </svg>
            <span>{lang === "zh" ? "微信群" : "WeChat"}</span>
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
            {/* HERO SECTION */}
            <section className={styles.spaceHero}>
              <div className={styles.heroAtmosphere} aria-hidden="true" />
              <div className={styles.heroStage}>
                <div className={styles.heroCopy}>
                  <div className={styles.heroMeta}>
                    <span className={styles.heroMetaDot} />
                    <span className={styles.heroMetaText}>
                      v{releases[lang][0].ver} · {lang === "zh" ? "真实开发团队 Agent · 驱动你的本机底座" : "REAL DEV TEAM AGENT · DRIVES YOUR LOCAL BASE"}
                    </span>
                  </div>

                  <h1 className={styles.heroWordmark}>
                    Uma<span>Dev</span>
                  </h1>

                  <h2 className={styles.heroStatement}>
                    {lang === "zh" ? (
                      <>一个 Agent，<br /><span>带着整支团队干活。</span></>
                    ) : (
                      <>One agent.<br /><span>A whole team at work.</span></>
                    )}
                  </h2>

                  <p className={styles.heroDescription}>
                    {lang === "zh"
                      ? "UmaDev 不提供模型，也不替代 Claude Code、Codex 或 OpenCode。它让你已登录的底座进入一套真实团队工作系统：先判断任务，再由产品、架构、设计、前后端、QA、安全和 DevOps 计划、执行、独立评审、验收与交付。"
                      : "UmaDev provides no model and does not replace Claude Code, Codex, or OpenCode. It puts your logged-in base inside a real team system that routes, plans, builds, independently reviews, verifies, and delivers."}
                  </p>

                  <div className={styles.heroActionRow}>
                    <button className={styles.heroInstall} type="button" onClick={copyInstall}>
                      <span>$</span>
                      <code>npm install -g umadev</code>
                      <strong>{copied ? (lang === "zh" ? "已复制" : "COPIED") : (lang === "zh" ? "复制" : "COPY")}</strong>
                    </button>
                  </div>

                  <dl className={styles.heroFacts}>
                    <div><dt>8</dt><dd>{lang === "zh" ? "专家角色" : "SPECIALIST ROLES"}</dd></div>
                    <div><dt>3</dt><dd>{lang === "zh" ? "本机底座" : "LOCAL BASES"}</dd></div>
                    <div><dt>113</dt><dd>{lang === "zh" ? "治理检查" : "GOVERNANCE CHECKS"}</dd></div>
                  </dl>
                </div>

                <div className={styles.heroSystem} aria-label={lang === "zh" ? "UmaDev 交付流水线动态预览" : "UmaDev delivery pipeline preview"}>
                  <div className={styles.heroSystemBar}>
                    <div className={styles.heroSystemLights}><i /><i /><i /></div>
                    <span>UMADEV / LIVE RUN</span>
                    <strong><i /> ONLINE</strong>
                  </div>

                  <div className={styles.heroPrompt}>
                    <span>{lang === "zh" ? "你的需求" : "YOUR BRIEF"}</span>
                    <p>{lang === "zh" ? "驱动我已登录的 Codex，把支付模块做到可上线。" : "Drive my logged-in Codex and make the payments module production-ready."}</p>
                  </div>

                  <div className={styles.heroPhaseTrack} aria-hidden="true">
                    {t.stages.map((stage, index) => (
                      <i
                        key={stage.key}
                        className={index === activeStageIdx ? styles.heroPhaseActive : index < activeStageIdx ? styles.heroPhaseDone : undefined}
                      />
                    ))}
                  </div>

                  {(() => {
                    const stage = t.stages[activeStageIdx];
                    return (
                      <div className={styles.heroLiveStage} key={stage.key}>
                        <div className={styles.heroStageIndex}>{String(activeStageIdx + 1).padStart(2, "0")}</div>
                        <div className={styles.heroStageCopy}>
                          <span>{lang === "zh" ? "团队正在执行计划" : "TEAM EXECUTING THE PLAN"}</span>
                          <h3>{stage.label}</h3>
                          <p>{stage.role}</p>
                        </div>
                        <span className={styles.heroStagePulse} />
                      </div>
                    );
                  })()}

                  <div className={styles.heroOutputs}>
                    <div className={styles.heroOutputsHead}>
                      <span>{lang === "zh" ? "实时产物" : "LIVE ARTIFACTS"}</span>
                      <small>{lang === "zh" ? "写入项目" : "WRITING TO PROJECT"}</small>
                    </div>
                    {t.stages[activeStageIdx].files.slice(0, 3).map((file, index) => (
                      <div className={styles.heroOutputFile} key={file}>
                        <i>{index + 1}</i>
                        <code>{file}</code>
                        <span>{index === 0 ? "SYNC" : "READY"}</span>
                      </div>
                    ))}
                  </div>

                  <div className={styles.heroSystemFoot}>
                    <div><span>BASE</span><strong>CLAUDE CODE / CODEX / OPENCODE</strong></div>
                    <div><span>PLAN</span><strong>OWNED DAG · LIVE STEERING</strong></div>
                    <div><span>PROOF</span><strong>VERIFY · SCORECARD</strong></div>
                  </div>
                </div>
              </div>
              <a className={styles.heroScrollCue} href="#pipeline" aria-label={lang === "zh" ? "滚动查看交付流水线" : "Scroll to the delivery pipeline"}>
                <span>{lang === "zh" ? "查看完整交付系统" : "EXPLORE THE DELIVERY SYSTEM"}</span>
                <i />
              </a>
            </section>

            {/* MARQUEE */}
            <section className={styles.marqueeSection}>
              <div className={styles.marqueeContainer}>
                <div className={styles.marqueeTrack}>
                  {Array.from({ length: 4 }).flatMap((_, outerIdx) => (
                    <React.Fragment key={outerIdx}>
                      <span className={styles.marqueeText}>{lang === "zh" ? "驱动你已登录的底座 —" : "DRIVES YOUR LOGGED-IN BASE —"}</span>
                      <span className={styles.marqueeItem}>Claude Code</span><span className={styles.marqueeSep}>◆</span>
                      <span className={styles.marqueeItem}>Codex</span><span className={styles.marqueeSepPurple}>◆</span>
                      <span className={styles.marqueeItem}>OpenCode</span><span className={styles.marqueeSep}>◆</span>
                      <span className={styles.marqueeText}>{lang === "zh" ? "不接外部 API · 不保存登录 · 底座用它自己的模型" : "NO EXTERNAL API KEY · NO LOGIN SAVED · BASE USES ITS OWN MODEL"}</span><span className={styles.marqueeSepPurple}>◆</span>
                    </React.Fragment>
                  ))}
                </div>
              </div>
            </section>

            {/* COOPERATIVE COMMUNITIES */}
            <section className={styles.partnersSection} id="community">
              <div className={styles.partnersContainer}>
                <div className={styles.communityHalo} aria-hidden="true" />
                <h2 className={styles.partnersTitle} data-title={t.partners.title}>{t.partners.title}</h2>
                {["top", "bottom"].map((position) => {
                  const items = position === "top" ? t.partners.list : [...t.partners.list].reverse();
                  return (
                    <div key={position} className={`${styles.communityBelt} ${position === "top" ? styles.communityBeltTop : styles.communityBeltBottom}`}>
                      <div className={styles.communityBeltTrack}>
                        {[...items, ...items].map((partner, idx) => (
                          <a
                            key={`${position}-${partner.logoName}-${idx}`}
                            aria-label={partner.name}
                            href={partner.url}
                            target={partner.url === "#" ? undefined : "_blank"}
                            rel={partner.url === "#" ? undefined : "noopener noreferrer"}
                            className={styles.communityLogo}
                            data-partner={partner.logoName}
                            onClick={(event) => {
                              if (partner.url === "#") event.preventDefault();
                            }}
                          >
                            <Image
                              src={asset(`/assets/partners/${partner.logoName === "paopai" ? "paopai-transparent" : partner.logoName}.png?v=5`)}
                              alt={partner.name}
                              width={220}
                              height={92}
                              loading="eager"
                            />
                          </a>
                        ))}
                      </div>
                    </div>
                  );
                })}
              </div>
            </section>

            {/* ROLE / PROBLEM */}
            <section className={styles.painSection}>
              <h2 className={styles.painTitle}>
                {lang === "zh" ? "不是套壳" : "Not a wrapper"}
              </h2>

              <div className={styles.painGrid}>
                <div className={`${styles.painCard} ${styles.reveal} ${styles.tilt}`} onMouseMove={handleTiltMove} onMouseLeave={handleTiltLeave}>
                  <span className={styles.painCardTagCross}>✕ {lang === "zh" ? "常见" : "COMMON"}</span>
                  <p className={styles.painCardText}>
                    {lang === "zh" ? "每句话都走同一套重流程，小修也开一整条流水线。" : "Every request enters the same heavy workflow, even a one-line fix."}
                  </p>
                </div>
                <div className={`${styles.painCard} ${styles.reveal} ${styles.tilt}`} onMouseMove={handleTiltMove} onMouseLeave={handleTiltLeave}>
                  <span className={styles.painCardTagCross}>✕ {lang === "zh" ? "常见" : "COMMON"}</span>
                  <p className={styles.painCardText}>
                    {lang === "zh" ? "一个模型既写代码又给自己验收，最后只剩一句“完成了”。" : "One model writes the code and certifies its own work, ending with a vague 'done'."}
                  </p>
                </div>
                <div className={`${styles.painCard} ${styles.reveal} ${styles.tilt}`} onMouseMove={handleTiltMove} onMouseLeave={handleTiltLeave}>
                  <span className={styles.painCardTagCross}>✕ {lang === "zh" ? "常见" : "COMMON"}</span>
                  <p className={styles.painCardText}>
                    {lang === "zh" ? "长任务看不到计划，换会话、切底座或压缩上下文就丢进度。" : "Long jobs hide the plan, then lose progress across sessions, base switches, or compaction."}
                  </p>
                </div>
                <div className={`${styles.painCardActive} ${styles.reveal} ${styles.tilt}`} onMouseMove={handleTiltMove} onMouseLeave={handleTiltLeave}>
                  <span className={styles.painCardTagCheck}>✓ UmaDev {lang === "zh" ? "按任务组队" : "ROUTES THE WORK"}</span>
                  <p className={styles.painCardActiveText}>
                    {lang === "zh" ? "底座先独立判断 Chat / Explain / QuickEdit / Debug / Build；小事快速做，真构建才展开计划与团队。" : "The base independently routes Chat / Explain / QuickEdit / Debug / Build. Small work stays small; real builds get the plan and team."}
                  </p>
                </div>
                <div className={`${styles.painCardActive} ${styles.reveal} ${styles.tilt}`} onMouseMove={handleTiltMove} onMouseLeave={handleTiltLeave}>
                  <span className={styles.painCardTagCheck}>✓ UmaDev {lang === "zh" ? "独立交叉评审" : "INDEPENDENT REVIEW"}</span>
                  <p className={styles.painCardActiveText}>
                    {lang === "zh" ? "主会话保持单写者；产品、架构、设计、QA、安全等角色在新的只读会话里审查，结构化结论回到同一计划。" : "The main session stays single-writer while product, architecture, design, QA, and security review in fresh read-only sessions."}
                  </p>
                </div>
                <div className={`${styles.painCardActive} ${styles.reveal} ${styles.tilt}`} onMouseMove={handleTiltMove} onMouseLeave={handleTiltLeave}>
                  <span className={styles.painCardTagCheck}>✓ UmaDev {lang === "zh" ? "计划与证据" : "PLAN & PROOF"}</span>
                  <p className={styles.painCardActiveText}>
                    {lang === "zh" ? "可见 DAG 逐步推进，随时 /plan 调整；确定性底线、运行证明、部署证明和交付包共同决定是否真的完成。" : "A visible DAG advances step by step and stays steerable with /plan; deterministic checks and runtime/deploy evidence decide completion."}
                  </p>
                </div>
              </div>
            </section>

            {/* VERTICAL PIPELINE SECTION */}
            <section id="pipeline" className={styles.pipelineSection}>
              <div className={styles.pipelineHeader}>
                <h2 className={styles.pipelineTitle}>
                  {lang === "zh" ? "完整构建" : "Full build"}
                </h2>
              </div>

              <div
                className={styles.pipelineContainer}
                onMouseEnter={() => setAutoplay(false)}
                onMouseLeave={() => setAutoplay(true)}
              >
                {/* Horizontal Progress Bar */}
                <div className={styles.pipelineProgressBarWrapper}>
                  <div
                    className={styles.pipelineProgressBarLine}
                    style={{
                      "--progress-width": `${(activeStageIdx / Math.max(t.stages.length - 1, 1)) * 100}%`
                    } as React.CSSProperties}
                  />
                  <div className={styles.pipelineProgressNodes}>
                    {t.stages.map((stage, index) => {
                      const isGate = stage.gate;
                      const color = isGate ? "#ff2a85" : "#00d2ff";
                      const stepNum = String(index + 1).padStart(2, "0");
                      const isActive = index === activeStageIdx;
                      const isCompleted = index <= activeStageIdx;

                      return (
                        <button
                          key={stage.key}
                          type="button"
                          className={`${styles.pipelineNodeButton} ${isActive ? styles.pipelineNodeActive : ""} ${isCompleted ? styles.pipelineNodeCompleted : ""}`}
                          onClick={() => setActiveStageIdx(index)}
                          style={{
                            "--node-color": color,
                            "--node-color-rgba": `${color}2a`
                          } as React.CSSProperties}
                        >
                          <span className={styles.pipelineNodeDot}>{stepNum}</span>
                          <span className={styles.pipelineNodeLabel}>{stage.label}</span>
                        </button>
                      );
                    })}
                  </div>
                </div>

                {/* Dashboard Detail Panel */}
                <div className={styles.pipelineDetailPanel}>
                  {(() => {
                    const stage = t.stages[activeStageIdx];
                    const isGate = stage.gate;
                    const isMicro = stage.key === "clarify";
                    const color = isGate ? "#ff2a85" : "#00d2ff";

                    return (
                      <div className={styles.pipelinePanelContent} key={stage.key}>
                        <div className={styles.pipelinePanelLeft}>
                          <div className={styles.pipelineMetaRow}>
                            <span className={styles.pipelineName}>{stage.label}</span>
                            <span
                              className={`${styles.pipelineBadge} ${styles.mono}`}
                              style={{ background: `${color}14`, color: color }}
                            >
                              {stage.key}
                            </span>
                            {isGate && <span className={styles.pipelineGateLabel}>{lang === "zh" ? "确认门禁" : "CONFIRM GATE"}</span>}
                            {isMicro && <span className={styles.pipelineMicroLabel}>{lang === "zh" ? "微阶段" : "MICRO PHASE"}</span>}
                          </div>
                          <div className={styles.pipelineDetailText} style={{ marginTop: "14px", fontSize: "16px", minHeight: "60px" }}>
                            {stage.role}
                          </div>
                        </div>

                        <div className={styles.pipelinePanelRight}>
                          <div className={styles.pipelineExplorer}>
                            <div className={styles.pipelineExplorerHeader}>
                              <span style={{ fontSize: "12px", textTransform: "uppercase", letterSpacing: "0.08em", color: "#8a8f9c" }}>
                                {lang === "zh" ? "OUTPUT / 生成交付产物" : "OUTPUT / DELIVERABLES"}
                              </span>
                            </div>
                            <div className={styles.pipelineExplorerList}>
                              {stage.files.map((file) => (
                                <div key={file} className={styles.pipelineExplorerItem}>
                                  <span style={{ color: color, fontSize: "10px", fontFamily: "var(--font-mono), monospace" }}>FILE</span>
                                  <span className={`${styles.pipelineExplorerName} ${styles.mono}`}>{file}</span>
                                </div>
                              ))}
                            </div>
                          </div>
                        </div>
                      </div>
                    );
                  })()}
                </div>
              </div>
            </section>

            {/* GOVERNANCE STATS */}
            <section id="governance" className={styles.statsSection}>
              <div className={styles.statsSectionHeader}>
                <div style={{ maxWidth: "640px" }}>
                  <h2 className={styles.statsSectionTitle}>
                    {lang === "zh" ? "治理与质量" : "Governance & quality"}
                  </h2>
                </div>
              </div>

              <div className={styles.statsGrid}>
                {[
                  { count: 34, label: lang === "zh" ? "条正式规范 Clause" : "Normative Spec Clauses", color: "#00d2ff", active: false },
                  { count: 113, label: lang === "zh" ? "条内容治理检查" : "Governance Content Checks", color: "#ff2a85", active: false },
                  { count: 2, label: lang === "zh" ? "道真正暂停的人工确认门" : "Human Gates That Actually Pause", color: "#00d2ff", active: false },
                  { count: 90, label: lang === "zh" ? "默认质量门通过线" : "Default Quality Gate Bar", color: "#00d2ff", active: true, suffix: lang === "zh" ? "分" : " pts" },
                ].map((item, idx) => (
                  <div
                    key={idx}
                    className={`${item.active ? styles.statCardActive : styles.statCard} ${styles.reveal} ${styles.tilt}`}
                    onMouseMove={handleTiltMove}
                    onMouseLeave={handleTiltLeave}
                    style={{ transitionDelay: `${idx * 40}ms` }}
                  >
                    <div style={{ display: "flex", alignItems: "baseline", gap: "6px" }}>
                      <div
                        className={`${styles.statNumber} ${styles.mono}`}
                        style={{ color: item.color }}
                        data-count={item.count}
                      >
                        {item.count}
                      </div>
                      {item.suffix && <span style={{ fontSize: "24px", color: item.color }}>{item.suffix}</span>}
                    </div>
                    <div style={{ marginTop: "12px", fontSize: "15px", color: item.active ? "#e2ff8a" : "#b4b9c4" }}>{item.label}</div>
                  </div>
                ))}
              </div>
            </section>

            {/* CURRENT CAPABILITIES */}
            <section className={styles.cratesSection}>
              <div className={styles.cratesHeader}>
                <h2 className={styles.cratesTitle}>
                  {lang === "zh" ? "像一支团队" : "Works like a team"}
                </h2>
              </div>

              <div className={styles.cratesGrid}>
                {[
                  ["ROUTE", lang === "zh" ? "先判断，再开工" : "Route before work", lang === "zh" ? "Chat · Explain · QuickEdit · Debug · Build 五条路径，投入与任务规模匹配。" : "Chat · Explain · QuickEdit · Debug · Build: effort stays proportional to the task."],
                  ["PLAN", lang === "zh" ? "协调者拥有计划" : "Coordinator-owned plan", lang === "zh" ? "依赖 DAG 写入 .umadev/plan.json，逐步执行，而不是让底座一口气盲跑。" : "An owned dependency DAG lives in .umadev/plan.json and advances step by step."],
                  ["TEAM", lang === "zh" ? "八个专家角色" : "Eight specialist roles", lang === "zh" ? "产品、架构、设计、前端、后端、QA、安全、DevOps 各自对产物负责。" : "Product, architecture, design, frontend, backend, QA, security, and DevOps own artifacts."],
                  ["REVIEW", lang === "zh" ? "单写者，独立评审" : "Single writer, independent critics", lang === "zh" ? "主会话负责写入；角色评审在新的只读分叉中进行，避免多人同时改主干。" : "The main session writes; role critics review in fresh read-only forks."],
                  ["STEER", lang === "zh" ? "运行中也能转向" : "Steer while it runs", lang === "zh" ? "追问和新要求折回下一步；用 /plan 跳过、否决、新增或重排任务。" : "Questions and new direction feed the next step; /plan can skip, veto, add, or reorder."],
                  ["CONTEXT", lang === "zh" ? "长任务不断线" : "Long-running continuity", lang === "zh" ? "会话持久化、压缩、恢复和底座切换都会携带对话、计划与团队黑板。" : "Resume, compaction, and base switching carry transcript, plan, and blackboard context."],
                  ["CODEBASE", lang === "zh" ? "理解现有代码库" : "Understands the repository", lang === "zh" ? "repo-map 解析符号与 import 边，按任务把最相关文件和结构交给底座。" : "The repo map resolves symbols and import edges, then ranks files relevant to the task."],
                  ["MEMORY", lang === "zh" ? "项目越做越懂" : "Learns the project", lang === "zh" ? "项目事实、踩坑、验证过的经验和运行笔记跨任务保留，并在需要时召回。" : "Project facts, pitfalls, verified lessons, and run notes persist and are recalled when useful."],
                  ["NATIVE", lang === "zh" ? "底座能力不打折" : "Native base tools preserved", lang === "zh" ? "搜索、研究、子 Agent 与后台任务继续可用；完成前会收齐后台结果。" : "Search, research, sub-agents, and background tasks stay available and are collected before completion."],
                  ["TRUST", lang === "zh" ? "可控的自动化" : "Controlled autonomy", lang === "zh" ? "plan / guarded / auto 信任梯度；推送、部署等不可逆动作始终需要确认。" : "Plan / guarded / auto trust tiers; irreversible push and deploy actions always confirm."],
                ].map(([name, role, desc], i) => (
                  <div
                    key={name}
                    className={`${styles.crateCard} ${styles.reveal} ${styles.tilt}`}
                    onMouseMove={handleTiltMove}
                    onMouseLeave={handleTiltLeave}
                    style={{ transitionDelay: `${i * 40}ms` }}
                  >
                    <div className={styles.crateName}>{name}</div>
                    <div className={styles.crateRole}>{role}</div>
                    <div className={styles.crateDesc}>{desc}</div>
                  </div>
                ))}
              </div>
            </section>

            {/* KNOWLEDGE RETRIEVAL SECTION */}
            <section id="knowledge" className={styles.retrieveSection}>
              <div className={styles.retrieveGrid}>
                <div className={styles.reveal}>
                  <h2 className={styles.retrieveTitle}>
                    {lang === "zh" ? "理解你的项目" : "Understands your project"}
                  </h2>
                  <p style={{ maxWidth: "540px", marginTop: "18px", color: "#9aa1ad", lineHeight: 1.8, fontSize: "15px" }}>
                    {lang === "zh"
                      ? "不是把一大段通用 Prompt 塞给底座。UmaDev 会把代码结构、当前任务、项目事实、踩坑经验和相关工程标准压缩成这一轮真正需要的上下文。"
                      : "It does not dump a generic mega-prompt into the base. UmaDev composes the repository structure, current task, project facts, recalled pitfalls, and relevant standards into the context this turn needs."}
                  </p>
                  <div className={`${styles.mono} ${styles.reveal}`} style={{ display: "flex", flexDirection: "column", gap: "8px", fontSize: "13.5px", color: "#8a8f9c", marginTop: "24px" }}>
                    <span><span style={{ color: "#00d2ff" }}>$</span> umadev knowledge-manage add ./team-docs</span>
                    <span><span style={{ color: "#00d2ff" }}>$</span> umadev knowledge-manage search {lang === "zh" ? '"支付 webhook 幂等"' : '"payment webhook idempotency"'}</span>
                  </div>
                </div>

                <div className={`${styles.reveal}`} style={{ position: "relative", padding: "30px", borderRadius: "20px", background: "#0c0e14", border: "1px solid rgba(255, 255, 255, 0.08)", overflow: "hidden" }}>
                  <div className={styles.mono} style={{ display: "flex", flexDirection: "column", gap: "11px", fontSize: "13px" }}>
                    {[
                      { num: "01", name: lang === "zh" ? "任务路由" : "Task route", desc: lang === "zh" ? "只取当前路径需要的上下文" : "only the context this route needs", color: "#ff2a85" },
                      { num: "02", name: lang === "zh" ? "repo-map" : "Repo map", desc: lang === "zh" ? "符号 · import 边 · 相关文件" : "symbols · import edges · ranked files", color: "#00d2ff" },
                      { num: "03", name: lang === "zh" ? "本地混合检索" : "Local hybrid retrieval", desc: lang === "zh" ? "BM25 + 本地向量 + HyDE" : "BM25 + local vectors + HyDE", color: "#ff2a85" },
                      { num: "04", name: lang === "zh" ? "项目记忆" : "Project memory", desc: lang === "zh" ? "事实 · 踩坑 · 经验 · 运行笔记" : "facts · pitfalls · lessons · run notes", color: "#00d2ff" },
                      { num: "05", name: lang === "zh" ? "按需注入" : "Proportional firmware", desc: lang === "zh" ? "→ 你已登录的底座" : "→ your logged-in base", color: "#00d2ff" },
                    ].map((step) => (
                      <div key={step.num} style={{ display: "flex", alignItems: "center", gap: "12px", padding: "12px 14px", borderRadius: "10px", background: "#0a0b10", borderLeft: `2px solid ${step.color}` }}>
                        <span style={{ color: step.color, fontSize: "12px" }}>{step.num}</span>
                        <span style={{ color: "#f4f6f2", fontSize: "13.5px", flex: 1 }}>{step.name}</span>
                        <span style={{ color: "#8a8f9c", fontSize: "12px" }}>{step.desc}</span>
                      </div>
                    ))}
                  </div>
                </div>
              </div>
            </section>

            {/* PROOF OF DELIVERY */}
            <section id="deliver" className={styles.deliverSection}>
              <div className={styles.deliverHeader}>
                <h2 className={styles.deliverTitle}>
                  {lang === "zh" ? "交付证据" : "Delivery proof"}
                </h2>
              </div>

              <div className={styles.deliverGrid}>
                <div className={`${styles.deliverCard} ${styles.reveal} ${styles.tilt}`} onMouseMove={handleTiltMove} onMouseLeave={handleTiltLeave}>
                  <div className={styles.deliverCardPath}>output/</div>
                  <h3 className={styles.deliverCardTitle}>{lang === "zh" ? "团队黑板" : "Team blackboard"}</h3>
                  <p className={styles.deliverCardDesc}>PRD · architecture · UI/UX · OpenAPI · execution plan · open decisions</p>
                </div>
                <div className={`${styles.deliverCard} ${styles.reveal} ${styles.tilt}`} onMouseMove={handleTiltMove} onMouseLeave={handleTiltLeave}>
                  <div className={styles.deliverCardPath}>.umadev/audit/</div>
                  <h3 className={styles.deliverCardTitle}>{lang === "zh" ? "真实验证" : "Runtime evidence"}</h3>
                  <p className={styles.deliverCardDesc}>runtime-proof.json · deploy-proof.json · contract reconciliation · security review</p>
                </div>
                <div className={`${styles.deliverCardActive} ${styles.reveal} ${styles.tilt}`} onMouseMove={handleTiltMove} onMouseLeave={handleTiltLeave}>
                  <div className={styles.deliverCardPathActive}>release/</div>
                  <h3 className={styles.deliverCardTitleActive}>{lang === "zh" ? "可审阅的交接" : "Reviewable handoff"}</h3>
                  <p className={styles.deliverCardDescActive}>
                    {lang === "zh" ? "评审报告、scorecard、proof pack、PR 正文与可访问地址——让“完成”有可检查的依据。" : "Review report, scorecard, proof pack, PR body, and live URL — so 'done' has inspectable evidence."}
                  </p>
                </div>
              </div>
            </section>

            {/* SPACE CTA */}
            <section className={styles.spaceCta}>
              <div className={styles.spaceCtaOverlay} />
              <div className={styles.spaceCtaContent}>
                <h2 className={styles.spaceCtaTitle}>
                  {lang === "zh" ? "把你的底座，变成一支团队" : "Turn your base into a team"}
                </h2>
                <div className={styles.spaceCtaConsole}>
                  <span className={styles.spaceCtaConsolePrompt}>$</span> npm install -g umadev
                  <button className={`${styles.umaCopy} ${styles.umaMagnet}`} style={{ marginLeft: "12px", padding: "6px 12px", borderRadius: "8px", border: "none", background: "#00d2ff", color: "#07080c", fontFamily: "var(--font-mono), monospace", fontSize: "12px", fontWeight: 600, cursor: "pointer" }} onClick={copyInstall} onMouseMove={handleMagnetMove} onMouseLeave={handleMagnetLeave}>
                    {copied ? (lang === "zh" ? "已复制 ✓" : "Copied ✓") : (lang === "zh" ? "复制" : "Copy")}
                  </button>
                </div>
                <div className={styles.spaceCtaActions}>
                  <a className={`${styles.spaceCtaBtn1} ${styles.umaMagnet}`} href="https://github.com/umacloud/umadev" target="_blank" rel="noreferrer" onMouseMove={handleMagnetMove} onMouseLeave={handleMagnetLeave}>
                    {lang === "zh" ? "在 GitHub 查看" : "View on GitHub"}
                  </a>
                  <button className={`${styles.spaceCtaBtn2} ${styles.umaMagnet}`} type="button" onClick={() => go("docs")} onMouseMove={handleMagnetMove} onMouseLeave={handleMagnetLeave}>
                    {lang === "zh" ? "阅读文档" : "Read Docs"}
                  </button>
                </div>
              </div>
              <div className={styles.spaceCtaArt} aria-hidden="true">
                <div className={styles.spaceCtaOrbit} />
                <Image
                  alt=""
                  className={styles.spaceCtaIp}
                  height={530}
                  src={asset("/assets/umadev/generated/changelog-ip.png")}
                  unoptimized
                  width={428}
                />
              </div>
            </section>
          </>
        )}

        {view === "docs" && (
          <section className={styles.docsPage}>
            <PageHero title={t.docsPage.title} sub={t.docsPage.sub} variant="docs" />
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
            <PageHero title={t.logPage.title} sub={t.logPage.sub} variant="changelog" />
            <ol className={styles.releaseTimeline}>
              {releases[lang].map((release, index) => (
                <ReleaseEntry
                  key={release.ver}
                  release={release}
                  isCurrent={index === 0}
                  latestLabel={t.logPage.current}
                  moreLabel={t.logPage.more}
                  lessLabel={t.logPage.less}
                />
              ))}
            </ol>
          </section>
        )}

        {view === "gallery" && (
          <section className={`${styles.docsPage} ${styles.galleryPage}`}>
            <PageHero title={t.galleryPage.title} sub={t.galleryPage.sub} variant="gallery" />
            <div className={styles.galleryGrid}>
              {gallery.map((src, i) => (
                <button
                  className={`${styles.galleryItem} ${styles.tilt}`}
                  key={src}
                  onClick={() => setLightbox(i)}
                  type="button"
                  onMouseMove={handleTiltMove}
                  onMouseLeave={handleTiltLeave}
                >
                  <Image alt={`UmaDev IP ${i + 1}`} className={styles.galleryImg} height={420} priority={i === 0} src={src} unoptimized width={420} />
                </button>
              ))}
            </div>
          </section>
        )}

        {view === "contributors" && (
          <section className={styles.contributorsPage}>
            <PageHero title={t.contributorsPage.title} sub={t.contributorsPage.sub} variant="contributors" />

            <div className={styles.contributorsMatrix}>
              {[
                { ...t.contributors.featured, isVip: true, vipText: lang === "zh" ? "创始荣誉席" : "Founding Honor", rank: "01" },
                ...t.contributors.list.filter((item) => (item as { isVip?: boolean }).isVip).map((item) => ({
                  ...item,
                  isVip: true,
                  vipText: lang === "zh" ? "核心荣誉席" : "Core Honor",
                  rank: "02"
                })),
                ...t.contributors.list.filter((item) => !(item as { isVip?: boolean }).isVip).map((item) => ({
                  ...item,
                  isVip: false,
                  vipText: undefined,
                  rank: undefined
                }))
              ].map((member) => (
                <div
                  key={member.avatarKey}
                  className={`${styles.matrixCard} ${styles.tilt}`}
                  data-rank={member.rank}
                  data-vip={member.isVip ? "true" : undefined}
                  onMouseMove={handleTiltMove}
                  onMouseLeave={handleTiltLeave}
                >
                  <div className={styles.matrixCardGlow} />
                  {member.isVip && <div className={styles.matrixVipOrbit} />}
                  {member.isVip && <span className={styles.matrixVipIndex}>{member.rank}</span>}
                  {member.isVip && <div className={styles.matrixVipBadge}>{member.vipText}</div>}
                  <div className={styles.matrixAvatarWrapper}>
                    <Image
                      src={asset(`/assets/contributors/${member.avatarKey}.png?v=2`)}
                      alt={member.name}
                      width={88}
                      height={88}
                      priority={member.isVip}
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
            <svg aria-hidden="true" viewBox="0 0 24 24"><path d="m6 6 12 12M18 6 6 18" /></svg>
          </button>
          <div className={styles.lightboxStage} onClick={(event) => event.stopPropagation()}>
            <Image
              alt={`UmaDev IP ${lightbox + 1}`}
              className={styles.lightboxImg}
              height={1100}
              src={gallery[lightbox]}
              unoptimized
              width={1100}
            />
          </div>
          <div className={styles.lightboxControls} onClick={(event) => event.stopPropagation()}>
            <button
              aria-label="Previous"
              className={styles.lightboxNav}
              data-side="prev"
              onClick={() => setLightbox((p) => (p === null ? null : (p - 1 + gallery.length) % gallery.length))}
              type="button"
            >
              <svg aria-hidden="true" viewBox="0 0 24 24"><path d="m15 5-7 7 7 7" /></svg>
              <span>{lang === "zh" ? "上一张" : "Previous"}</span>
            </button>
            <span className={styles.lightboxCount}>
              <b>{String(lightbox + 1).padStart(2, "0")}</b>
              <i>/</i>
              <span>{String(gallery.length).padStart(2, "0")}</span>
            </span>
            <button
              aria-label="Next"
              className={styles.lightboxNav}
              data-side="next"
              onClick={() => setLightbox((p) => (p === null ? null : (p + 1) % gallery.length))}
              type="button"
            >
              <span>{lang === "zh" ? "下一张" : "Next"}</span>
              <svg aria-hidden="true" viewBox="0 0 24 24"><path d="m9 5 7 7-7 7" /></svg>
            </button>
          </div>
        </div>
      )}

      <footer className={styles.spaceFooter}>
        <div className={styles.spaceFooterBrand}>
          <Image
            alt=""
            aria-hidden="true"
            className={styles.spaceFooterBrandLogo}
            height={30}
            src={asset("/assets/umadev-icon.png")}
            width={30}
          />
          <span className={styles.spaceFooterBrandText}>
            {lang === "zh" ? "UmaDev · umacloud 开源项目 · MIT License" : "UmaDev · an umacloud open-source project · MIT License"}
          </span>
        </div>
        <div className={`${styles.spaceFooterLink} ${styles.mono}`}>umadev.goder.ai</div>
      </footer>
    </div>
  );

}

/** Map a localized change tag (zh / en) to its colored chip class. */
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
    变更: styles.tagChanged,
    Changed: styles.tagChanged,
  };
  return `${styles.releaseTag} ${map[tag] ?? styles.tagImproved}`;
}

/** One changelog release. Array order determines the latest highlighted entry. */
type Release = {
  ver: string;
  date: string;
  title: string;
  current?: boolean;
  changes: readonly (readonly [string, string])[];
};

/** How many changes a release shows before the "show more" toggle appears,
 *  and how many stay visible while collapsed — keeps long entries scannable. */
const RELEASE_PEEK = 5;
const RELEASE_COLLAPSE_AT = 6;

/** A single changelog entry: a timeline node (version + date + Latest pill) on
 *  the rail, a short title and well-spaced, tagged changes on the right. Long
 *  entries collapse behind a "show more" toggle so the page stays scannable. */
function ReleaseEntry({
  release,
  isCurrent,
  latestLabel,
  moreLabel,
  lessLabel,
}: {
  release: Release;
  isCurrent: boolean;
  latestLabel: string;
  moreLabel: string;
  lessLabel: string;
}) {
  const [expanded, setExpanded] = useState(false);
  const collapsible = release.changes.length > RELEASE_COLLAPSE_AT;
  const shown = !collapsible || expanded ? release.changes : release.changes.slice(0, RELEASE_PEEK);
  const hiddenCount = release.changes.length - shown.length;

  return (
    <li className={`${styles.releaseEntry} ${isCurrent ? styles.releaseEntryCurrent : ""}`}>
      <div className={styles.releaseMeta}>
        <div className={styles.releaseVer}>{release.ver}</div>
        <time className={styles.releaseDate}>{release.date}</time>
        {isCurrent && <span className={styles.releaseLatest}>{latestLabel}</span>}
      </div>
      <div className={styles.releaseBody}>
        <h2 className={styles.releaseTitle}>{release.title}</h2>
        <ul className={styles.releaseChanges}>
          {shown.map(([tag, text]) => (
            <li key={`${tag}-${text}`} className={styles.releaseChange}>
              <span className={tagClass(tag)}>{tag}</span>
              <p>{text}</p>
            </li>
          ))}
        </ul>
        {collapsible && (
          <button
            type="button"
            className={styles.releaseToggle}
            onClick={() => setExpanded((v) => !v)}
            aria-expanded={expanded}
          >
            {expanded ? lessLabel : moreLabel.replace("{n}", String(hiddenCount))}
            <Chevron up={expanded} />
          </button>
        )}
      </div>
    </li>
  );
}

/** Small chevron used by the show-more toggle (rotates when expanded). */
function Chevron({ up }: { up: boolean }) {
  return (
    <svg
      aria-hidden="true"
      width="11"
      height="11"
      viewBox="0 0 16 16"
      fill="none"
      className={up ? styles.chevronUp : styles.chevronDown}
    >
      <path d="M4 6l4 4 4-4" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}



type PageHeroVariant = "docs" | "gallery" | "contributors" | "changelog";

const pageHeroVisuals: Record<PageHeroVariant, { art: string; ip: string; label: string; metrics: readonly string[] }> = {
  docs: {
    art: "/assets/umadev/vectors/docs-orbit.svg",
    ip: "/assets/umadev/generated/docs-ip.png",
    label: "UmaDev / Documentation",
    metrics: ["08 TEAM ROLES", "05 TASK ROUTES", "03 LOCAL BASES"],
  },
  gallery: {
    art: "/assets/umadev/vectors/gallery-aperture.svg",
    ip: "/assets/umadev/generated/gallery-ip.png",
    label: "UmaDev / Visual Archive",
    metrics: ["45 IP SCENES", "ORIGINAL SERIES", "VISUAL ARCHIVE"],
  },
  contributors: {
    art: "/assets/umadev/vectors/contributor-constellation.svg",
    ip: "/assets/umadev/generated/honor-ip.png",
    label: "UmaDev / Open Source",
    metrics: ["MIT LICENSE", "COMMUNITY BUILT", "SPECIAL THANKS"],
  },
  changelog: {
    art: "/assets/umadev/vectors/changelog-flow.svg",
    ip: "/assets/umadev/generated/changelog-ip.png",
    label: "UmaDev / Release Stream",
    metrics: [`CURRENT v${releases.en[0].ver}`, "RUST BINARY", "VERIFIED RELEASES"],
  },
};

function PageHero({ title, sub, variant }: { title: string; sub: string; variant?: PageHeroVariant }) {
  if (variant) {
    const visual = pageHeroVisuals[variant];
    return (
      <header className={`${styles.pageHero} ${styles.routeHero}`} data-hero={variant}>
        <div className={styles.routeHeroCopy}>
          <span>{visual.label}</span>
          <h1>{title}</h1>
          <p>{sub}</p>
        </div>
        <div aria-hidden="true" className={styles.routeHeroArt}>
          <Image
            alt=""
            className={styles.routeHeroVector}
            height={800}
            priority
            src={asset(visual.art)}
            unoptimized
            width={800}
          />
          <Image
            alt=""
            className={styles.routeHeroIp}
            height={1254}
            priority
            src={asset(visual.ip)}
            unoptimized
            width={1254}
          />
          <div className={styles.routeHeroMetrics}>
            {visual.metrics.map((metric) => <span key={metric}>{metric}</span>)}
          </div>
        </div>
      </header>
    );
  }

  return (
    <header className={styles.pageHero}>
      <span>UmaDev</span>
      <h1>{title}</h1>
      <p>{sub}</p>
    </header>
  );
}

function DocCode({ code }: { code: string }) {
  const [copied, setCopied] = useState(false);
  const handleCopy = () => {
    navigator.clipboard.writeText(code);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  };
  return (
    <div className={styles.docCodeWrapper}>
      <div className={styles.docCodeHeader}>
        <div style={{ display: "flex", gap: "6px", alignItems: "center" }}>
          <span style={{ width: "9px", height: "9px", borderRadius: "50%", background: "#ff5f56" }} />
          <span style={{ width: "9px", height: "9px", borderRadius: "50%", background: "#ffbd2e" }} />
          <span style={{ width: "9px", height: "9px", borderRadius: "50%", background: "#27c93f" }} />
        </div>
        <span style={{ fontFamily: "var(--font-mono), monospace", fontSize: "11px", color: "#6b7080" }}>Terminal / Config</span>
        <button className={styles.docCodeCopy} onClick={handleCopy} type="button">
          {copied ? "Copied ✓" : "Copy"}
        </button>
      </div>
      <pre className={styles.docCode}><code>{code}</code></pre>
    </div>
  );
}

function DocBlockView({ block }: { block: DocBlock }) {
  if ("h" in block) return <h3 className={styles.docHeading}>{block.h}</h3>;
  if ("p" in block) return <p className={styles.docPara}>{block.p}</p>;
  if ("c" in block) return <DocCode code={block.c} />;
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
