"use client";

import { useEffect, useState } from "react";
import Image from "next/image";
import { asset } from "./content";
import styles from "./page.module.css";

const scripts_en = [
  [
    { text: "$ umadev new booking --mode auto", type: "prompt" },
    { text: "uma_00 > Clarifying goals, scope and roles...", type: "sys" },
    { text: "phase: docs_confirm", type: "stage" },
    { text: "output/prd.md written", type: "file" },
    { text: "output/architecture.md written", type: "file" },
    { text: "uma_01 > Generating Next.js frontend pages", type: "sys" },
    { text: "cargo test --workspace", type: "default" },
    { text: "quality_gate: 94 / 100", type: "ok" },
    { text: "release/proof-pack.zip generated", type: "file" },
    { text: "Delivery complete.", type: "done" },
  ],
  [
    { text: "$ umadev continue --phase docs", type: "prompt" },
    { text: "uma_docs > Syncing context from PRD...", type: "sys" },
    { text: "phase: frontend_implement", type: "stage" },
    { text: "output/uiux.md read", type: "file" },
    { text: "uma_ui > Building React components...", type: "sys" },
    { text: "lint: 0 errors, 0 warnings", type: "ok" },
    { text: "status: pending_backend", type: "done" },
  ],
  [
    { text: "$ umadev gate --threshold 90", type: "prompt" },
    { text: "uma_gate > Running compliance and tests...", type: "sys" },
    { text: "phase: quality_gate", type: "stage" },
    { text: "✓ build success", type: "ok" },
    { text: "✓ contracts matched", type: "ok" },
    { text: "✓ security scan passed", type: "ok" },
    { text: "release/scorecard.html generated", type: "file" },
    { text: "quality_gate: 98 / 100", type: "done" },
  ]
];

const scripts_zh = [
  [
    { text: "$ umadev new booking --mode auto", type: "prompt" },
    { text: "uma_00 > 澄清目标、范围与角色...", type: "sys" },
    { text: "phase: docs_confirm", type: "stage" },
    { text: "output/prd.md 已生成", type: "file" },
    { text: "output/architecture.md 已生成", type: "file" },
    { text: "uma_01 > 生成 Next.js 前端页面...", type: "sys" },
    { text: "cargo test --workspace", type: "default" },
    { text: "质量门得分: 94 / 100", type: "ok" },
    { text: "release/proof-pack.zip 已打包", type: "file" },
    { text: "交付闭环完成。", type: "done" },
  ],
  [
    { text: "$ umadev continue --phase docs", type: "prompt" },
    { text: "uma_docs > 从 PRD 同步上下文...", type: "sys" },
    { text: "phase: frontend_implement", type: "stage" },
    { text: "output/uiux.md 读取完毕", type: "file" },
    { text: "uma_ui > 构建 React 组件...", type: "sys" },
    { text: "lint检查: 0 错误, 0 警告", type: "ok" },
    { text: "状态: 等待后端接口", type: "done" },
  ],
  [
    { text: "$ umadev gate --threshold 90", type: "prompt" },
    { text: "uma_gate > 执行合规性与自动化测试...", type: "sys" },
    { text: "phase: quality_gate", type: "stage" },
    { text: "✓ 构建成功", type: "ok" },
    { text: "✓ API 契约匹配", type: "ok" },
    { text: "✓ 安全扫描通过", type: "ok" },
    { text: "release/scorecard.html 报告已生成", type: "file" },
    { text: "质量门得分: 98 / 100", type: "done" },
  ]
];

export function TerminalDemo({ slideIndex, lang }: { slideIndex: number, lang?: string }) {
  const [step, setStep] = useState(0);
  const scripts = lang === "zh" ? scripts_zh : scripts_en;
  const lines = scripts[slideIndex] || scripts[0];


  useEffect(() => {
    if (step >= lines.length) return;
    const timer = setTimeout(() => {
      setStep((s) => s + 1);
    }, step === 0 ? 800 : Math.random() * 600 + 300);
    return () => clearTimeout(timer);
  }, [step, lines.length]);

  return (
    <div className={styles.terminalWrap}>
      <div className={styles.terminalGlow} />
      <Image
        className={styles.mascot}
        src={asset("/assets/mascot-thumb.png")}
        alt=""
        width={148}
        height={148}
        priority
      />
      <div className={styles.terminal}>
        <div className={styles.terminalBeam} />
        <div className={styles.terminalTop}>
          <div className={styles.windowDots}>
            <span />
            <span />
            <span />
          </div>
          <span>umadev — bash</span>
          <button type="button" onClick={() => setStep(0)}>{lang === "zh" ? "重新开始" : "RESTART"}</button>
        </div>
        <div className={styles.terminalBody}>
          {lines.slice(0, step).map((line, i) => (
            <div
              key={i}
              className={
                line.type === "prompt"
                  ? styles.promptLine
                  : line.type === "sys"
                    ? styles.lineSys
                    : line.type === "stage"
                      ? styles.lineStage
                      : line.type === "file"
                        ? styles.lineFile
                        : line.type === "ok"
                          ? styles.lineOk
                          : line.type === "done"
                            ? styles.lineDone
                            : styles.lineDefault
              }
            >
              {line.type === "prompt" && <span>$ </span>}
              {line.text.replace("$ ", "")}
            </div>
          ))}
          {step < lines.length && (
            <div className={styles.promptLine}>
              <span className={styles.cursor} />
            </div>
          )}
          {step >= lines.length && (
            <div className={styles.promptLine}>
              <span>$</span>
              <span className={styles.cursorGreen} />
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
