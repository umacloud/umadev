"use client";

import Image from "next/image";
import type { PointerEvent as ReactPointerEvent } from "react";
import { useRef, useState } from "react";
import { docs, i18n, releases, type DocBlock, type Lang, type View } from "./content";
import styles from "./page.module.css";

const githubUrl = "https://github.com/umacloud/umadev";
type DocItem = { id: string; title: string; blocks: readonly DocBlock[] };
type DocCategory = { cat: string; items: readonly DocItem[] };

export default function Home() {
  const [lang, setLang] = useState<Lang>("zh");
  const [view, setView] = useState<View>("home");
  const [stageIndex, setStageIndex] = useState(0);
  const [mode, setMode] = useState("claude-code");
  const [docId, setDocId] = useState("quickstart");
  const [copied, setCopied] = useState(false);
  const copyTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const stageButtonRefs = useRef<(HTMLButtonElement | null)[]>([]);

  const t = i18n[lang];
  const activeStage = t.stages[stageIndex] ?? t.stages[0];
  const activeTab = t.modes.tabs.find((tab) => tab.id === mode) ?? t.modes.tabs[0];
  const docCats = docs[lang] as readonly DocCategory[];
  const activeDoc =
    docCats.flatMap((cat) => cat.items).find((item) => item.id === docId) ??
    docCats[0].items[0];

  const heroTitle = `${t.hero.title1} ${t.hero.titleHi}${t.hero.title2}`;
  const titleLines =
    lang === "zh"
      ? [
          { text: "把 AI 编码工具", accent: false },
          { text: "变成真正的", accent: false },
          { text: "项目总监 Agent", accent: true },
        ]
      : [
          { text: "Turn AI coding tools into", accent: false },
          { text: "real project director", accent: true },
          { text: "agents", accent: false },
        ];
  const stageProgress = `${Math.round(((stageIndex + 1) / t.stages.length) * 100)}%`;

  function go(nextView: View) {
    setView(nextView);
    window.scrollTo({ top: 0, behavior: "smooth" });
  }

  function copyInstall() {
    navigator.clipboard?.writeText("npm install -g umadev").catch(() => undefined);
    setCopied(true);
    if (copyTimerRef.current) clearTimeout(copyTimerRef.current);
    copyTimerRef.current = setTimeout(() => setCopied(false), 1500);
  }

  function pickStage(index: number) {
    setStageIndex(index);
    stageButtonRefs.current[index]?.scrollIntoView({
      behavior: "smooth",
      block: "nearest",
      inline: "center",
    });
  }

  function trackPointer(event: ReactPointerEvent<HTMLDivElement>) {
    const rect = event.currentTarget.getBoundingClientRect();
    event.currentTarget.style.setProperty("--mx", `${event.clientX - rect.left}px`);
    event.currentTarget.style.setProperty("--my", `${event.clientY - rect.top}px`);
  }

  return (
    <div className={styles.shell} onPointerMove={trackPointer}>
      <div className={styles.gridBg} aria-hidden="true" />
      <div className={styles.topGlow} aria-hidden="true" />
      <div className={styles.pointerGlow} aria-hidden="true" />
      <div className={styles.scanlines} aria-hidden="true" />
      <div className={styles.noise} aria-hidden="true" />

      <nav className={styles.nav}>
        <button className={styles.brand} type="button" onClick={() => go("home")}>
          <Image
            className={styles.logo}
            src="/assets/umadev-icon.png"
            alt="UmaDev logo"
            width={42}
            height={42}
            priority
          />
          <span>UmaDev</span>
        </button>

        <div className={styles.navLinks}>
          <button className={navClass(view === "home")} type="button" onClick={() => go("home")}>
            {t.nav.product}
          </button>
          <button className={navClass(view === "docs")} type="button" onClick={() => go("docs")}>
            {t.nav.docs}
          </button>
          <button
            className={navClass(view === "changelog")}
            type="button"
            onClick={() => go("changelog")}
          >
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
          <a className={styles.githubButton} href={githubUrl} target="_blank" rel="noreferrer">
            <GitHubIcon />
            GitHub
          </a>
        </div>
      </nav>

      <main className={styles.main}>
        {view === "home" && (
          <>
            <section className={styles.hero}>
              <div className={styles.heroBackdrop} aria-hidden="true">
                <Image
                  src="/assets/umadev/hero-city-agent.png"
                  alt=""
                  fill
                  priority
                  sizes="100vw"
                />
              </div>
              <div className={styles.heroCopy}>
                <div className={styles.badge}>
                  <span className={styles.pulseDot} />
                  {t.hero.badge}
                </div>
                <div className={styles.heroHud}>
                  <span>RUN_MODE / LOCAL</span>
                  <span>QUALITY_GATE / 90+</span>
                  <span>HOSTS / 23</span>
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
                <p>{t.hero.sub}</p>

                <button className={styles.installCommand} type="button" onClick={copyInstall}>
                  <span className={styles.promptMark}>$</span>
                  <code>npm install -g umadev</code>
                  <span className={styles.copyPill}>{copied ? t.hero.copied : t.hero.copy}</span>
                </button>

                <div className={styles.heroActions}>
                  <a href={githubUrl} target="_blank" rel="noreferrer">
                    {t.hero.cta1}
                    <span aria-hidden="true">→</span>
                  </a>
                  <button type="button" onClick={() => go("docs")}>
                    {t.hero.cta2}
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
                <Image
                  className={styles.heroMark}
                  src="/assets/umadev/neon-logo-cut.png"
                  alt=""
                  width={760}
                  height={760}
                  priority
                  aria-hidden="true"
                />
                <div className={styles.codeTicker} aria-hidden="true">
                  <span>cargo test --workspace</span>
                  <span>quality_gate: 94 / 100</span>
                  <span>release/proof-pack.zip</span>
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
              </div>
              <div className={styles.mascotRail}>
                {t.mascots.cards.map((card, index) => (
                  <article className={styles.mascotCard} key={card.title}>
                    <Image src={card.img} alt={card.title} width={360} height={360} />
                    <div>
                      <small>
                        {String(index + 1).padStart(2, "0")} / {card.role}
                      </small>
                      <h3>{card.title}</h3>
                      <p>{card.desc}</p>
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
                <code>
                  <span>$ </span>
                  <b>{activeTab.bin}</b> {activeTab.cmd.replace(activeTab.bin, "").trim()}
                </code>
                <small>{t.modes.whoLabel}</small>
                <p>{activeTab.who}</p>
              </article>
              <div className={styles.modeCards}>
                {t.modes.cards.map((card) => (
                  <article key={card.title}>
                    <header>
                      <h3>{card.title}</h3>
                      <code>{card.cmd}</code>
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
                {t.ip.cards.map((card) => (
                  <figure key={card.cap}>
                    <Image src={card.img} alt={card.cap} width={390} height={390} />
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
              <code>{t.cta.note}</code>
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
                          window.scrollTo({ top: 0, behavior: "smooth" });
                        }}
                      >
                        {item.title}
                      </button>
                    ))}
                  </div>
                ))}
              </aside>
              <article className={styles.docArticle}>
                <h2>{activeDoc.title}</h2>
                {activeDoc.blocks.map((block, index) => (
                  <DocBlockView block={block} key={index} />
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
      </main>

      <footer className={styles.footer}>
        <div>
          <div className={styles.footerBrand}>
            <Image
              className={styles.logo}
              src="/assets/umadev-icon.png"
              alt="UmaDev logo"
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
