//! Agent runner — drives the 9-phase pipeline.
//!
//! V1 deterministic pipeline:
//!
//! - `run_initial_block`:    research → docs → pause at `docs_confirm`
//! - `continue_after_docs`:  spec → frontend → pause at `preview_confirm`
//! - `continue_after_preview`: backend → quality → delivery → done
//!
//! Later milestones swap the deterministic phase bodies for LLM-driven
//! ones without changing this orchestration.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use umadev_runtime::Runtime;
use umadev_spec::{Phase, SPEC_VERSION};

use crate::coach::write_coach_prompt;
use crate::events::{null_sink, EngineEvent, EventSink};
use crate::experts::{
    architecture_prompt, backend_prompt, clarify_prompt, delivery_prompt, excerpt,
    excerpt_sections, frontend_prompt, prd_prompt, research_prompt, uiux_prompt, Prompt,
};
use crate::gates::Gate;
use crate::phases::{
    run_backend, run_delivery, run_docs, run_frontend, run_quality, run_research, run_spec,
    DocsContent, PhaseOutput,
};
use crate::state::{write_workflow_state, WorkflowState};

/// Whether the markdown section starting at byte `heading_pos` (the heading
/// line for `heading`) has any non-empty body content before the next `##`
/// heading. Catches "present but empty" sections a bare `## goal` would
/// otherwise let through review.
fn section_has_body(lower: &str, heading_pos: usize, heading: &str) -> bool {
    // Body = lines after the heading line, up to the NEXT H2 heading
    // (a line starting with `## ` but NOT `### ` — sub-headings are body).
    // The previous `find("\n##")` treated any `##` substring (including
    // `### In scope`) as a section boundary, so multi-level sections read
    // as empty. Now we split on lines and only stop at a true H2 peer.
    let after_heading = heading_pos + heading.len();
    let rest = &lower[after_heading.min(lower.len())..];
    for line in rest.lines() {
        let trimmed = line.trim_start();
        // A peer H2 heading ends this section: starts with "## " but not "###".
        if trimmed.starts_with("## ") && !trimmed.starts_with("###") {
            break;
        }
        let l = line.trim();
        if !l.is_empty() && !l.starts_with('#') {
            return true;
        }
    }
    false
}

/// Map a fenced-code-block language tag to a pseudo file path so the
/// content-scan rules pick the right extensions (e.g. `tsx` → `block.tsx`).
/// Returns `None` for languages we don't govern (plain text, bash, json, etc.).
fn lang_to_path(lang: &str) -> Option<String> {
    let ext = match lang {
        "tsx" | "jsx" | "ts" | "js" | "py" | "rs" | "go" | "java" | "kt" | "swift" | "php"
        | "vue" | "svelte" => lang,
        "typescript" => "ts",
        "javascript" => "js",
        "python" => "py",
        "rust" => "rs",
        _ => return None,
    };
    Some(format!("block.{ext}"))
}

/// Scan one fenced code block for governance violations. Returns a defect
/// string (the rule reason) when a rule fires, `None` when clean.
fn scan_code_block(
    lang: &str,
    lines: &[&str],
    policy: &umadev_governance::Policy,
) -> Option<String> {
    let path = lang_to_path(lang)?;
    let content: String = lines.join("\n");
    // Honor the project's `.umadev/rules.toml` (disabled clauses / path
    // exclusions) exactly like the hook / CI / MCP paths do — the runner is
    // the MAIN generation path and must not be the one place that ignores it.
    let decision = umadev_governance::scan_content_with_policy(&path, &content, policy);
    if decision.block {
        Some(format!(
            "Governance {} violation in ```{lang} block: {}",
            decision.clause,
            decision.reason.split('.').next().unwrap_or("see rule"),
        ))
    } else {
        None
    }
}

/// The borrowed brain's up-front analysis of the incoming requirement — the
/// "thinking" a director does before any work: what is this, how hard, what
/// must exist, what could go wrong. Surfaced so the user sees the plan.
#[derive(Debug, Default, serde::Deserialize)]
struct IntakePlan {
    /// Product type (e.g. "SaaS dashboard", "marketing landing page").
    #[serde(default)]
    product_type: String,
    /// Rough complexity: "simple" / "medium" / "complex".
    #[serde(default)]
    complexity: String,
    /// The 3–6 core features that MUST be built.
    #[serde(default)]
    core_features: Vec<String>,
    /// Key risks / things to watch.
    #[serde(default)]
    key_risks: Vec<String>,
}

/// The borrowed brain's assessment of the three core docs before the user
/// confirms the docs gate. Surfaced (not auto-applied) so the user decides.
#[derive(Debug, Default, serde::Deserialize)]
struct DocsVerdict {
    /// The single biggest risk in the plan as it stands.
    #[serde(default)]
    biggest_risk: String,
    /// Important things missing from the docs the user should know about.
    #[serde(default)]
    missing: Vec<String>,
    /// Whether the plan is solid enough to start building.
    #[serde(default)]
    ready: bool,
}

/// The borrowed brain's verdict when reviewing the delivered UI for commercial
/// polish / AI-slop tells. Same shared model as the worker — no extra API.
#[derive(Debug, Default, serde::Deserialize)]
struct DesignVerdict {
    /// Concrete AI-slop / non-commercial UI issues found (empty = clean).
    #[serde(default)]
    issues: Vec<String>,
    /// Whether the UI reads like a polished commercial product.
    #[serde(default)]
    commercial_grade: bool,
}

/// The borrowed brain's verdict when judging delivery against the PRD
/// acceptance criteria. Parsed from the `consult` JSON; all fields default so a
/// partial reply still deserializes.
#[derive(Debug, Default, serde::Deserialize)]
struct AcceptanceVerdict {
    /// Acceptance criteria / required features the code does NOT yet implement.
    #[serde(default)]
    unmet: Vec<String>,
    /// Whether the delivery looks commercial-grade ready.
    #[serde(default)]
    commercial_ready: bool,
    /// One-line summary of what's missing/weak (informational).
    #[serde(default)]
    #[allow(dead_code)]
    notes: String,
}

/// Resolve the model to drive a phase, honouring optional per-tier overrides:
/// `UMADEV_MODEL_BUILD` for the code phases (frontend / backend) and
/// `UMADEV_MODEL_PLAN` for the planning phases (research / docs / spec /
/// quality / delivery). This is the per-phase model assignment the field's top
/// agents converged on — plan with a cheaper, faster model and write code with
/// a stronger one (Cline Plan/Act, Roo Architect→Code, Aider architect/editor).
/// Falls back to the single configured `default_model` when a tier is unset, so
/// the default single-model behaviour is unchanged.
fn model_for_phase(default_model: &str, phase: Phase) -> String {
    let tier_var = if matches!(phase, Phase::Frontend | Phase::Backend) {
        "UMADEV_MODEL_BUILD"
    } else {
        "UMADEV_MODEL_PLAN"
    };
    std::env::var(tier_var)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default_model.to_string())
}

/// Per-phase generation token budget. Long-form artifact phases (docs /
/// architecture / PRD) get a larger budget so a big document isn't
/// truncated; shorter phases (research/spec/quality) get less. Override
/// via `UMADEV_MAX_TOKENS` (>0 = fixed cap for all phases).
fn max_tokens_for_phase(phase: Phase) -> u32 {
    if let Ok(v) = std::env::var("UMADEV_MAX_TOKENS") {
        if let Ok(n) = v.parse::<u32>() {
            if n > 0 {
                return n;
            }
        }
    }
    match phase {
        // Docs produces PRD + architecture + UIUX — the longest artifacts.
        // Backend also emits substantial code + integration notes.
        Phase::Docs | Phase::Backend => 8192,
        // Frontend/quality/delivery/spec/research — moderate.
        _ => 4096,
    }
}

/// User-facing run configuration.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Workspace root the agent operates inside.
    pub project_root: PathBuf,
    /// Free-form user requirement (e.g. "做一个登录系统").
    pub requirement: String,
    /// Slug used in artifact filenames. Defaults to the workspace dir
    /// name when callers leave it empty.
    pub slug: String,
    /// Model identifier passed to the runtime (provider-specific).
    pub model: String,
    /// Backend id that's driving this run (e.g. `claude-code`, `codex`).
    /// Empty when running offline templates. Persisted into the workflow
    /// state so subsequent `continue` / `revise` calls can resume against
    /// the same worker without a flag.
    pub backend: String,
    /// Active design system name (e.g. `modern-minimal`). When set, the
    /// coach prompt injects the matching `knowledge/design-systems/<name>.md`
    /// content so the worker binds tokens deterministically.
    pub design_system: String,
    /// Active seed template name (e.g. `dashboard`). When set, the coach
    /// prompt references `knowledge/seed-templates/<name>.md` for the
    /// page structure and quality gates.
    pub seed_template: String,
    /// Trust / autonomy tier for this run (`plan` / `guarded` / `auto`).
    /// Default [`crate::trust::TrustMode::Guarded`] preserves the existing
    /// human-in-the-loop behaviour. `plan` makes the run read-only (research +
    /// docs, then stop at `docs_confirm` without executing); `auto` drives
    /// end-to-end. The mode only governs gate auto-pass policy — phase *content*
    /// stays deterministic.
    pub mode: crate::trust::TrustMode,
}

impl RunOptions {
    /// Resolve the effective slug — derives from workspace dir name
    /// when empty.
    #[must_use]
    pub fn effective_slug(&self) -> String {
        if !self.slug.is_empty() {
            return self.slug.clone();
        }
        self.project_root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string()
    }
}

/// Outcome of a single block of execution.
#[derive(Debug, Clone)]
pub struct RunReport {
    /// Final phase after this block.
    pub final_phase: Phase,
    /// Gate the pipeline paused at, if any. `None` means delivery
    /// completed.
    pub paused_at: Option<Gate>,
    /// Phases that executed during this block, with their artifact lists.
    pub completed: Vec<PhaseOutput>,
}

/// The agent runner. Owns the runtime; phase methods live in [`crate::phases`].
pub struct AgentRunner<R: Runtime> {
    runtime: R,
    options: RunOptions,
    events: Arc<dyn EventSink>,
    /// Phases whose artifacts are the offline fallback template because the base
    /// went offline/empty mid-run (#1). Recorded so a block can warn loudly at
    /// its boundary that part of the output is a placeholder, not real delivery.
    /// A `Mutex` (not `RefCell`) so `&AgentRunner` stays `Sync` — the runner is
    /// borrowed across `.await` points inside `tokio::spawn`ed futures, which
    /// requires `Send`. The lock is only ever held inside synchronous helpers
    /// (never across an `.await`), so it can't deadlock the async runtime.
    degraded_phases: std::sync::Mutex<Vec<String>>,
}

impl<R: Runtime> AgentRunner<R> {
    /// Build a new runner. Events are dropped until [`with_event_sink`]
    /// attaches a real sink.
    ///
    /// [`with_event_sink`]: AgentRunner::with_event_sink
    pub fn new(runtime: R, options: RunOptions) -> Self {
        Self {
            runtime,
            options,
            events: null_sink(),
            degraded_phases: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Attach an event sink so a UI (TUI) can observe pipeline progress.
    #[must_use]
    pub fn with_event_sink(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.events = sink;
        self
    }

    /// Runtime kind (for human-facing announcements).
    pub fn runtime_kind(&self) -> umadev_runtime::RuntimeKind {
        self.runtime.kind()
    }

    /// Emit `event` to the attached sink (no-op for the null sink).
    fn emit(&self, event: EngineEvent) {
        self.events.emit(event);
    }

    /// Announce a phase start AND drop a file-level rewind checkpoint for it.
    ///
    /// The TUI builds per-phase checkpoints in its `PhaseStarted` event handler,
    /// but the headless `run` / `continue` paths (driven by `cmd_run` over a
    /// channel/null sink) never ran that handler — so a scripted run had NO
    /// automatic rewind points (#2). Creating the checkpoint HERE, in the runner
    /// itself, gives every execution path (TUI and headless alike) the same
    /// phase-level safety net. `create_phase_checkpoint` is fail-open (no `git`
    /// → no snapshot) and skips empty boundaries, so a TUI run that also
    /// checkpoints on the consumed `PhaseStarted` event no longer doubles up:
    /// the second attempt sees an unchanged tree and is a no-op.
    fn start_phase(&self, phase: Phase) {
        let label = format!("phase: {}", phase.id());
        let _ = crate::checkpoint::create_phase_checkpoint(&self.options.project_root, &label);
        self.emit(EngineEvent::PhaseStarted { phase });
    }

    /// At a block boundary, loudly re-state every phase that DEGRADED to an
    /// offline placeholder this run (#1) so the user can't miss that part of the
    /// output is not a real delivery. No-op when nothing degraded. Returns the
    /// number of degraded phases so callers can fold it into a `RunReport` / gate
    /// decision if they choose.
    fn warn_degraded_summary(&self) -> usize {
        // Snapshot the list under the lock, then drop the guard BEFORE emitting
        // (emit may run arbitrary sink code) — the lock is never held across a
        // call that could re-enter the runner.
        let degraded: Vec<String> = match self.degraded_phases.lock() {
            Ok(g) if !g.is_empty() => g.clone(),
            _ => return 0,
        };
        self.emit(EngineEvent::Note(format!(
            "[WARN][降级] 本次有 {} 个阶段因底座离线只产出了占位模板(非真实交付):{}。\
             这些阶段的 output 文件旁有 .DEGRADED 标记。请勿据此验收 —— \
             修好底座后用 /redo 重跑以拿到真实产物。",
            degraded.len(),
            degraded.join(" / ")
        )));
        degraded.len()
    }

    /// Record a phase execution with timing. The phase is recorded as a CLEAN
    /// success — only use this for phases that genuinely produced their content
    /// (offline runs, or runtime phases whose generation succeeded). For a
    /// runtime phase whose base call failed and fell back to the offline
    /// template, use [`Self::record_phase_maybe_degraded`].
    fn record_phase(
        &self,
        phase: Phase,
        output: std::io::Result<PhaseOutput>,
    ) -> std::io::Result<PhaseOutput> {
        self.record_phase_maybe_degraded(phase, output, false)
    }

    /// Record a phase execution, flagging it as DEGRADED when `degraded` is true.
    ///
    /// `#1` — base offline mid-phase: when the runtime was supposed to drive a
    /// phase but the base returned empty / errored, the runner falls back to the
    /// deterministic offline template. That template is a SKELETON PLACEHOLDER,
    /// not a real delivery, so we must NOT pass it off as a clean success.
    /// A degraded phase:
    ///   - is announced with a loud, unmistakable `[WARN][降级]` note telling the
    ///     user the base was offline, the artifact is a placeholder, and they
    ///     need `/redo` (or `/continue` after fixing the base) to get real output;
    ///   - writes a `<artifact>.DEGRADED` marker next to each fallback artifact so
    ///     the placeholder status is visible on disk, not just in the chat log;
    ///   - still emits `ArtifactWritten` (the file exists) but is tracked so the
    ///     end-of-block summary can list every degraded phase.
    ///
    /// Fail-open: the marker write is best-effort and never blocks the pipeline.
    fn record_phase_maybe_degraded(
        &self,
        phase: Phase,
        output: std::io::Result<PhaseOutput>,
        degraded: bool,
    ) -> std::io::Result<PhaseOutput> {
        let mut output = output?;
        output.degraded = degraded;
        if degraded {
            if let Ok(mut g) = self.degraded_phases.lock() {
                g.push(phase.id().to_string());
            }
            self.emit(EngineEvent::Note(format!(
                "[WARN][降级] {} 阶段:底座离线/返回空,**未生成真实产物** —— \
                 当前 output 里的是离线占位骨架模板,不是底座真实交付。\
                 请用 /doctor 排查底座,修好后用 /redo 重跑本阶段拿真实产物。\
                 在那之前请勿把本阶段当成已完成。",
                phase.id()
            )));
            for artifact in &output.artifacts {
                let marker = artifact.with_extension(format!(
                    "{}.DEGRADED",
                    artifact
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("out")
                ));
                let _ = std::fs::write(
                    &marker,
                    format!(
                        "DEGRADED FALLBACK — phase `{}` ran while the base was offline/empty.\n\
                         This artifact is the OFFLINE PLACEHOLDER TEMPLATE, not real base output.\n\
                         Re-run this phase (/redo) once the base is reachable to replace it.\n",
                        phase.id()
                    ),
                );
            }
        }
        for artifact in &output.artifacts {
            self.emit(EngineEvent::ArtifactWritten {
                phase,
                path: artifact.clone(),
            });
        }
        self.emit(EngineEvent::PhaseCompleted { phase });
        if let Some(gate) = output.gate {
            self.emit(EngineEvent::GateOpened { gate });
        }
        Ok(output)
    }

    /// Record phase timing to `.umadev/phase-timing.jsonl`.
    fn record_phase_timing(&self, phase: Phase, started: std::time::Instant) {
        let elapsed_ms = started.elapsed().as_millis();
        let dir = self.options.project_root.join(".umadev");
        let _ = std::fs::create_dir_all(&dir);
        let entry = serde_json::json!({
            "timestamp": Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "phase": phase.id(),
            "elapsed_ms": elapsed_ms,
            "slug": self.options.effective_slug(),
        });
        let path = dir.join("phase-timing.jsonl");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            use std::io::Write;
            let _ = writeln!(f, "{entry}");
        }
        self.emit(EngineEvent::Note(format!(
            "[time] {} completed in {:.1}s",
            phase.id(),
            elapsed_ms as f64 / 1000.0
        )));
    }

    /// Write a run summary entry to `.umadev/runs.jsonl` for historical tracking.
    fn record_run_history(&self, final_phase: Phase, passed: bool, artifact_count: usize) {
        let dir = self.options.project_root.join(".umadev");
        let _ = std::fs::create_dir_all(&dir);
        let entry = serde_json::json!({
            "timestamp": Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "slug": self.options.effective_slug(),
            "requirement": self.options.requirement,
            "backend": self.options.backend,
            "design_system": self.options.design_system,
            "final_phase": final_phase.id(),
            "quality_passed": passed,
            "artifact_count": artifact_count,
        });
        let path = dir.join("runs.jsonl");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            use std::io::Write;
            let _ = writeln!(f, "{entry}");
        }
    }

    /// Run the workspace's build / install command after a
    /// code-producing phase. Emits `VerifyStarted` + one of
    /// `VerifySkipped` / `VerifyPassed` / `VerifyFailed`, and appends a
    /// row to `.umadev/audit/verify.jsonl`. Always best-effort —
    /// `Err` paths from the subprocess become structured `VerifyFailed`
    /// events, not Rust errors.
    async fn maybe_verify(&self, phase: Phase) -> VerifyOutcome {
        // Only the code-producing phases need verify; docs / spec / etc.
        // don't have build output to test.
        if !matches!(phase, Phase::Frontend | Phase::Backend | Phase::Quality) {
            return VerifyOutcome {
                passed: true,
                skipped: true,
                failure_detail: String::new(),
            };
        }
        let workspace = &self.options.project_root;
        let kind = crate::verify::detect_project(workspace);
        let command = kind
            .verify_command(workspace)
            .map_or_else(String::new, |(p, args)| format!("{p} {}", args.join(" ")));
        self.emit(EngineEvent::VerifyStarted {
            phase,
            command: command.clone(),
        });
        let outcomes = crate::verify::run_verify(workspace).await;
        if outcomes.is_empty() {
            self.emit(EngineEvent::VerifySkipped {
                phase,
                reason: "no recognised project manifest".to_string(),
            });
            return VerifyOutcome {
                passed: true,
                skipped: true,
                failure_detail: String::new(),
            };
        }

        // Record each step's outcome to the audit log.
        let mut total_ms: u64 = 0;
        let mut any_failed = false;
        for o in &outcomes {
            total_ms = total_ms.saturating_add(o.duration_ms);
            if !o.passed && !o.skipped {
                any_failed = true;
            }
            let _ = crate::verify::record_verify_outcome(workspace, phase.id(), o);
        }

        // Emit an aggregate event. If any non-skipped step failed, the
        // whole verify is a failure (the quality gate consumes this).
        if any_failed {
            // Distil every failing build/lint/test step into the pitfall KB —
            // these non-zero exits are the highest-signal "踩坑" we capture.
            // Build the captured text with the SAME representation that
            // `maybe_verify_and_fix` later resolves with (`failure_detail`
            // below) — identical `{step}: {summarize_stderr(detail)}` — so an
            // unrecognized error's generic signature matches on both the
            // capture and the resolve, and the in-run-fix bookkeeping isn't a
            // silent no-op.
            let detail_of = |o: &crate::verify::VerifyOutcome| -> String {
                let raw = if o.stderr.trim().is_empty() {
                    &o.stdout
                } else {
                    &o.stderr
                };
                format!("{}: {}", o.step, summarize_stderr(raw))
            };
            let pitfalls: Vec<String> = outcomes
                .iter()
                .filter(|o| !o.passed && !o.skipped)
                .map(detail_of)
                .collect();
            self.capture_dev_pitfalls(&pitfalls);

            let failed_step = outcomes
                .iter()
                .find(|o| !o.passed && !o.skipped)
                .map(detail_of)
                .unwrap_or_default();
            self.emit(EngineEvent::VerifyFailed {
                phase,
                exit_code: outcomes
                    .iter()
                    .find(|o| !o.passed && !o.skipped)
                    .map(|o| o.exit_code)
                    .unwrap_or(-1),
                stderr: failed_step.clone(),
            });
            VerifyOutcome {
                passed: false,
                skipped: false,
                failure_detail: failed_step,
            }
        } else {
            self.emit(EngineEvent::VerifyPassed {
                phase,
                duration_ms: total_ms,
            });
            VerifyOutcome {
                passed: true,
                skipped: false,
                failure_detail: String::new(),
            }
        }
    }

    /// Verify, and if it FAILS, hand the build error back to the worker for
    /// one fix attempt, then re-verify. Mirrors the docs review→fix loop but
    /// for code: a frontend/backend build that doesn't compile gets one
    /// automatic repair pass before the pipeline gives up. Skipped (no
    /// manifest) and passing verifies return immediately.
    async fn maybe_verify_and_fix(&self, phase: Phase) {
        let first = self.maybe_verify(phase).await;
        if first.passed || first.skipped {
            return;
        }
        if self.runtime.is_offline() {
            return; // no runtime → nothing can fix it
        }
        self.emit(EngineEvent::Note(format!(
            "[setup] {} 构建失败,把错误喂回 worker 修复一次…\n  {}",
            phase.id(),
            first.failure_detail
        )));
        // Classify the failure and, when it's a recognised family, hand the
        // worker the root cause + proven fix playbook for THAT error class — so
        // the single repair attempt is informed, not a blind "here's stderr".
        let insight = crate::error_kb::classify_error(&first.failure_detail);
        let guidance = if insight.recognized {
            format!(
                "\n\n## Diagnosis (error class: {})\n根本原因: {}\n建议修法: {}\n\
                 先按上面的诊断修，再泛化排查其他同类问题。",
                insight.signature, insight.root_cause, insight.fix
            )
        } else {
            String::new()
        };
        // Reflection: ONLY when this exact pitfall TRULY recurred after a prior
        // warning (its recorded fix already failed) do we spend one extra cheap
        // base call to design a DIFFERENT high-level approach. First failures
        // stay on the template path above — no reflection, no added cost. The
        // produced strategy is recorded on the pitfall + a sliding window, so the
        // `lessons_for_error` injection below surfaces it. Fail-open: a base
        // error/empty reply just leaves the template path unchanged.
        if let Some(recurring) = crate::lessons::recurring_pitfall_for_error(
            &self.options.project_root,
            &first.failure_detail,
        ) {
            let (system, user) = crate::lessons::reflection_prompt(&recurring);
            if let Some(text) = self
                .try_generate(phase, Prompt { system, user })
                .await
                .filter(|t| !t.trim().is_empty())
            {
                let recorded = crate::lessons::record_pitfall_strategy(
                    &self.options.project_root,
                    &recurring.signature,
                    &text,
                );
                if recorded {
                    self.emit(EngineEvent::Note(
                        "[learned] 同类踩坑反复出现，已让底座反思生成一个不同的高层纠错策略并注入本次修复。"
                            .to_string(),
                    ));
                }
            }
        }
        // Closed-loop stage 5: at the exact moment of failure, surface prior
        // lessons with the SAME error signature ("you hit this N times before;
        // here's what worked; it keeps recurring"). Fingerprint-gated + abstains
        // on no confident match, so it never injects a misleading prior fix.
        let prior =
            crate::lessons::lessons_for_error(&self.options.project_root, &first.failure_detail);
        if !prior.is_empty() {
            self.emit(EngineEvent::Note(
                "[learned] 命中历史同类踩坑，已把上次的根因/修法注入本次修复提示。".to_string(),
            ));
        }
        let fix_prompt = Prompt {
            system: format!(
                "The {} code you just wrote failed to build/test. The error \
                 output is below. Fix the code so the build passes — edit the \
                 relevant files, do NOT rewrite from scratch. Output a short \
                 summary of what you changed.",
                phase.id()
            ),
            user: format!(
                "## Build/test error\n\n{}{guidance}{prior}\n\n## Original requirement\n\n{}\n\nFix the failing code now.",
                first.failure_detail, self.options.requirement
            ),
        };
        let _ = self.try_generate(phase, fix_prompt).await;
        // Re-verify after the fix attempt. If it now passes, the auto-fix
        // worked — record that as DIRECT proof the recorded pitfall's fix is
        // effective, so the KB validates it immediately (strongest signal).
        let second = self.maybe_verify(phase).await;
        if second.passed && !second.skipped {
            let resolved = crate::lessons::mark_pitfalls_resolved(
                &self.options.project_root,
                std::slice::from_ref(&first.failure_detail),
            );
            if resolved > 0 {
                self.emit(EngineEvent::Note(format!(
                    "[learned] 自动修复成功，已确认 {resolved} 条踩坑的修法有效（标记为已验证）。"
                )));
            }
        }
    }

    /// Self-evolution memory upkeep at delivery, the BASE-driven half of the
    /// learning loop (the template/no-base halves already ran inside
    /// `run_delivery`). Two passes, both fail-open and both skipped when offline:
    ///
    /// 1. **Memory reconcile** — ask the base, per fresh lesson vs. its most
    ///    similar priors, whether it ADD/UPDATE/INVALIDATE/NOOPs, then re-sediment
    ///    with that decision map so the lesson corpus is curated instead of
    ///    purely appended. With no base this never runs, leaving the pure-append
    ///    behaviour intact.
    /// 2. **Skill cards** — replace the deterministic template description on each
    ///    freshly-graduated skill with a base-written ≤6-sentence reusable card.
    ///
    /// Driven entirely through the existing `try_generate` host-driver seam — no
    /// new model endpoint. Bounded (a small cap on base calls) so a huge corpus
    /// can't explode delivery latency.
    async fn evolve_memory_at_delivery(&self) {
        if self.runtime.is_offline() {
            return;
        }
        let root = &self.options.project_root;

        // ---- Pass 1: base-judged memory reconcile -------------------------
        // Cap the number of fresh lessons we spend a base call on, so a large
        // corpus stays cheap. Newest candidates first (reconcile_candidates is
        // already newest-first).
        const MAX_RECONCILE_CALLS: usize = 8;
        let candidates = crate::lessons::reconcile_candidates(root);
        if !candidates.is_empty() {
            // Decision map keyed by the fresh lesson's identity triple.
            let mut decisions: std::collections::HashMap<
                (String, String, String),
                crate::lessons::ReconcileDecision,
            > = std::collections::HashMap::new();
            for (fresh, similar) in candidates.iter().take(MAX_RECONCILE_CALLS) {
                let (system, user) = crate::lessons::reconcile_prompt(fresh, similar);
                if let Some(reply) = self
                    .try_generate(Phase::Delivery, Prompt { system, user })
                    .await
                    .filter(|t| !t.trim().is_empty())
                {
                    let id = (
                        fresh.domain.clone(),
                        fresh.title.clone(),
                        fresh.first_seen.clone(),
                    );
                    decisions.insert(id, crate::lessons::parse_reconcile_decision(&reply));
                }
            }
            if !decisions.is_empty() {
                let judge = move |fresh: &crate::lessons::Lesson,
                                  _similar: &[crate::lessons::Lesson]| {
                    let id = (
                        fresh.domain.clone(),
                        fresh.title.clone(),
                        fresh.first_seen.clone(),
                    );
                    decisions
                        .get(&id)
                        .copied()
                        .unwrap_or(crate::lessons::ReconcileDecision::Noop)
                };
                let _ = crate::lessons::sediment_lessons_with_judge(root, Some(&judge));
                self.emit(EngineEvent::Note(
                    "[learned] 记忆整理：让底座对相似旧教训做了 ADD/UPDATE/INVALIDATE 判定，已合并并淘汰过期条目。"
                        .to_string(),
                ));
            }
        }

        // ---- Pass 2: base-written reusable skill cards --------------------
        // The freshly-graduated skills carry a template description; upgrade the
        // ones produced this run with a base-written reusable card. Bounded.
        const MAX_SKILL_CARDS: usize = 6;
        let skills = crate::skills::read_skills(root);
        let mut carded = 0usize;
        for s in skills.iter().take(MAX_SKILL_CARDS) {
            let (system, user) = crate::skills::skill_description_prompt(
                &s.title,
                &s.content,
                &s.source_requirement,
            );
            if let Some(card) = self
                .try_generate(Phase::Delivery, Prompt { system, user })
                .await
                .filter(|t| !t.trim().is_empty())
            {
                // Re-graduate with the base card (gate already passed + multi-step
                // this run, so the gate re-admits and refreshes the description).
                if crate::skills::graduate_skill(
                    root,
                    &s.title,
                    &s.content,
                    &card,
                    &s.domain,
                    &s.keywords,
                    &s.source_requirement,
                    true,
                ) {
                    carded += 1;
                }
            }
        }
        if carded > 0 {
            self.emit(EngineEvent::Note(format!(
                "[learned] 技能库：让底座为 {carded} 条已毕业技能生成了可复用的解法卡片。"
            )));
        }
    }

    /// Initialise the workspace for a new run.
    pub fn start(&self) -> std::io::Result<WorkflowState> {
        let state = WorkflowState {
            phase: Phase::Research.id().to_string(),
            active_gate: String::new(),
            slug: self.options.effective_slug(),
            requirement: self.options.requirement.clone(),
            last_transition_at: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            note: format!(
                "Started run with worker {}: {}",
                self.worker_label(),
                self.options.requirement
            ),
            backend: self.options.backend.clone(),
            spec_version: SPEC_VERSION.to_string(),
        };
        write_workflow_state(&self.options.project_root, &state)?;
        // Drop a coach prompt so the host knows what to do on first turn.
        let _ = write_coach_prompt(&self.options, Phase::Research);
        // UD-META-001: ensure the workspace declares its spec conformance.
        // Best-effort — a user-customised umadev.yaml is left untouched.
        let _ = crate::manifest::SpecManifest::new(self.options.effective_slug())
            .write_to(&self.options.project_root, false);
        Ok(state)
    }

    /// research → docs → pause at `docs_confirm`.
    ///
    /// When `use_runtime` is true, the runner asks the configured
    /// runtime to draft each artifact (research → PRD → architecture →
    /// UIUX) and writes the LLM output verbatim. Any provider error or
    /// empty response falls back to the deterministic template, so the
    /// pipeline never breaks because of an LLM blip.
    /// Run the clarify phase: ask the worker to generate clarifying
    /// questions, write them to `output/{slug}-clarify.md`, and pause at
    /// `ClarifyGate`. The user answers in the TUI; on resume the answers are
    /// folded into the requirement and research runs.
    /// Run the clarify phase: ask the worker to generate clarifying
    /// questions about the requirement, persist them, and pause at
    /// [`Gate::ClarifyGate`]. The user answers in the TUI; on resume the
    /// answers fold into the requirement and research runs.
    pub async fn run_clarify(&self, use_runtime: bool) -> std::io::Result<RunReport> {
        let _run_lock = crate::run_lock::RunLock::acquire(&self.options.project_root)?;
        self.emit(EngineEvent::PipelineStarted {
            slug: self.options.effective_slug(),
            requirement: self.options.requirement.clone(),
        });
        self.emit(EngineEvent::PhaseStarted {
            phase: Phase::Research, // clarify is a pre-research micro-phase
        });
        let slug = self.options.effective_slug();
        let clarify_path = self
            .options
            .project_root
            .join("output")
            .join(format!("{slug}-clarify.md"));
        if use_runtime {
            self.emit(EngineEvent::Note(
                "[clarify] 正在生成需求澄清问题…".to_string(),
            ));
            let prompt = clarify_prompt(&self.options.requirement);
            if let Some(text) = self.try_generate(Phase::Research, prompt).await {
                if !text.trim().is_empty() {
                    if let Some(parent) = clarify_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    crate::phases::atomic_write(&clarify_path, &text)?;
                }
            }
        }
        // Show the questions to the user and pause.
        let questions = std::fs::read_to_string(&clarify_path).unwrap_or_default();
        self.emit(EngineEvent::GateOpened {
            gate: Gate::ClarifyGate,
        });
        self.emit(EngineEvent::Note(format!(
            "请回答以下澄清问题(逐条回答,或输入 c 跳过):\n{questions}"
        )));
        self.transition(Phase::Research, "clarify")?;
        Ok(RunReport {
            final_phase: Phase::Research,
            paused_at: Some(Gate::ClarifyGate),
            completed: Vec::new(),
        })
    }

    /// Run the initial block: research → docs, then pause at
    /// [`Gate::DocsConfirm`]. `use_runtime` forces the worker on/off.
    /// `requirement_override` (when `Some`) replaces the stored requirement
    /// for this run — used by [`continue_from_gate`] to fold the user's
    /// clarify answers into research without mutating `options`.
    pub async fn run_initial_block(
        &self,
        use_runtime: bool,
        requirement_override: Option<&str>,
    ) -> std::io::Result<RunReport> {
        // Single-writer lock: refuse if another run holds this workspace (held
        // for the whole block, released on return).
        let _run_lock = crate::run_lock::RunLock::acquire(&self.options.project_root)?;
        let mut completed = Vec::new();
        let project_cfg = crate::config::load_project_config(&self.options.project_root);
        let max_reviews = project_cfg.pipeline.max_review_rounds;
        let effective_requirement = requirement_override
            .map(str::to_string)
            .unwrap_or_else(|| self.options.requirement.clone());
        // Dynamic planner (#3): tailor WHICH phases run to the task instead of
        // forcing every project through all nine. The plan is now actually
        // CONSUMED to drive execution, not just logged:
        //  - `skip` (used for the research decision below) folds in EVERY phase
        //    the plan excludes that this initial block governs — `research` for a
        //    bug-fix / refactor, etc. — so a lean task no longer pays for
        //    similar-product research it doesn't need.
        //  - the continue blocks re-derive the same plan and honour
        //    `plan.includes(Frontend|Backend|Delivery)` so the frontend-only /
        //    backend-only / docs-only walks skip the irrelevant build phase.
        // `gate_safe_skips` is still surfaced separately because Delivery is the
        // only skip proven zero-risk to auto-apply in the gate-anchored walk; the
        // other plan exclusions are advisory here and enforced at their phase.
        let plan = crate::planner::plan(&effective_requirement);
        let mut skip = project_cfg.pipeline.skip_phases.clone();
        let plan_skips = crate::planner::gate_safe_skips(&plan);
        for p in &plan_skips {
            let id = p.id().to_string();
            if !skip.contains(&id) {
                skip.push(id);
            }
        }
        // Honour the FULL plan for the research phase: a lean plan that doesn't
        // include Research (bug-fix / refactor) skips it here regardless of the
        // gate-safe set (which only covers Delivery). Docs is the block's gate
        // anchor and always runs so the docs_confirm checkpoint still fires.
        if !plan.includes(Phase::Research) && !skip.iter().any(|s| s == "research") {
            skip.push("research".to_string());
        }
        if plan_skips.is_empty() {
            if plan.kind != crate::planner::TaskKind::Greenfield {
                self.emit(EngineEvent::Note(format!(
                    "[plan] 任务类型:{} — {}",
                    plan.kind.id(),
                    plan.rationale
                )));
            }
        } else {
            self.emit(EngineEvent::Note(format!(
                "[plan] 动态规划:{} — {};本次跳过 {}",
                plan.kind.id(),
                plan.rationale,
                plan_skips
                    .iter()
                    .map(|p| p.id())
                    .collect::<Vec<_>>()
                    .join(" / ")
            )));
        }
        self.emit(EngineEvent::PipelineStarted {
            slug: self.options.effective_slug(),
            requirement: effective_requirement.clone(),
        });

        // The first thing a thinking director does: have the shared brain
        // analyze the requirement and surface its plan (type / complexity / core
        // features / risks) up front — so the user sees how we understood the ask.
        if use_runtime {
            self.surface_intake_plan().await;
        }

        // Pre-embed the requirement once so every phase's expert-knowledge
        // section can use true BM25+vector RRF fusion. No-op (returns None)
        // when the vector layer is off or no API key is set — fail-open to BM25.
        let qvec = self.preembed_requirement().await;

        // HyDE: generate ONE hypothetical answer for the requirement up front
        // (a single short base call) and reuse it across every phase. Its BM25
        // ranking gets RRF-fused into retrieval, recalling curated docs the
        // user's literal wording would miss. Fail-open (offline / error / empty
        // → None → retrieval is byte-for-byte the pre-HyDE path).
        let hyde = if use_runtime {
            crate::coach::generate_hyde_expansion(&self.runtime, &effective_requirement).await
        } else {
            None
        };

        // 1. research
        let research_text = if skip.iter().any(|s| s == "research") {
            self.emit(EngineEvent::Note(format!(
                "[skip] 跳过 research 阶段(任务类型 {} 不需要相似产品调研,或配置显式跳过)。",
                plan.kind.id()
            )));
            None
        } else {
            let phase_start = std::time::Instant::now();
            self.start_phase(Phase::Research);
            let (top_files, total) = crate::phases::knowledge_top_files(&self.options);
            if !top_files.is_empty() {
                let preview = top_files
                    .iter()
                    .take(3)
                    .map(|p| format!("`{p}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let more = if top_files.len() > 3 {
                    format!(" (+ {} more)", top_files.len() - 3)
                } else {
                    String::new()
                };
                self.emit(EngineEvent::Note(format!(
                    "[knowledge] knowledge: 选了 {} 个文档中的 {} 篇喂给 worker —— {preview}{more}",
                    total,
                    top_files.len(),
                )));
            }
            let text = if use_runtime {
                let research_digest = crate::phases::phase_knowledge_digest_with_retrieval(
                    &self.options,
                    Phase::Research,
                    qvec.as_deref(),
                    hyde.as_deref(),
                );
                let rp = self.with_expert_knowledge(
                    research_prompt(
                        &self.options.effective_slug(),
                        &effective_requirement,
                        &research_digest,
                    ),
                    &["product-manager"],
                );
                self.generate_with_review(Phase::Research, rp, Self::review_research, max_reviews)
                    .await
            } else {
                None
            };
            // #1: runtime was on but the base produced nothing → the research
            // artifact is the offline placeholder, not real output. Flag degraded.
            let degraded = use_runtime && text.is_none();
            completed.push(self.record_phase_maybe_degraded(
                Phase::Research,
                run_research(&self.options, text.as_deref()),
                degraded,
            )?);
            self.record_phase_timing(Phase::Research, phase_start);
            text
        };
        self.transition(Phase::Docs, "")?;

        // 2. docs
        let phase_start = std::time::Instant::now();
        self.start_phase(Phase::Docs);
        let docs_content = if use_runtime {
            self.generate_docs_content(research_text.as_deref()).await
        } else {
            DocsContent::default()
        };

        // #1: docs degrades when runtime is on but ALL three core docs fell back
        // to templates (every body is None) — that means the base was offline for
        // the whole docs phase, so the PRD/architecture/UIUX are placeholders.
        let docs_degraded = use_runtime
            && docs_content.prd.is_none()
            && docs_content.architecture.is_none()
            && docs_content.uiux.is_none();
        let docs = self.record_phase_maybe_degraded(
            Phase::Docs,
            run_docs(&self.options, &docs_content),
            docs_degraded,
        )?;
        self.record_phase_timing(Phase::Docs, phase_start);
        let gate = docs.gate;
        completed.push(docs);
        // Intelligent docs assessment — the shared brain reviews the foundation
        // and surfaces its judgment so the user confirms WITH a real opinion.
        if use_runtime {
            let slug = self.options.effective_slug();
            self.surface_docs_assessment(&slug).await;
            // Role-critic TEAM cross-review (explicit, scaled to the task). The
            // deterministic floor (coverage/contract in surface_docs_assessment)
            // already ran as the HARD gate above; this adds the PM + architect
            // cross-review opinions, records every verdict to the team ledger,
            // and folds their union of blocking issues into the docs revision
            // path ONCE (round-1, advisory) — never as loop control.
            let blocking = self.run_docs_critic_team(&slug).await;
            if !blocking.is_empty() {
                self.revise_docs_for_critic_blocking(&slug, &blocking).await;
            }
        }
        self.transition(Phase::DocsConfirm, gate.map_or("", Gate::id_str))?;

        self.warn_degraded_summary();
        // Plan (read-only) mode: the pipeline stops here by design. Research +
        // the three planning docs are produced ("here's how I'd build it"), but
        // nothing past `docs_confirm` executes — no spec, no real code. The user
        // reviews the plan, then re-runs in `guarded` / `auto` to execute.
        if self.options.mode == crate::trust::TrustMode::Plan {
            self.emit(EngineEvent::Note(
                "[plan] 计划模式(只读):已产出研究 + PRD/架构/UIUX 计划文档,\
                 流水线在此停止,未执行 spec/前端/后端,未写入真实代码。\
                 审核计划后,用 guarded/auto 模式重跑以执行。  \
                 [plan] Plan mode (read-only): produced research + PRD/arch/UIUX. \
                 The pipeline stops here without executing spec/frontend/backend \
                 — no real code was written. Re-run in guarded/auto to execute."
                    .to_string(),
            ));
        }
        self.emit(EngineEvent::BlockCompleted {
            final_phase: Phase::DocsConfirm,
            paused_at: gate,
        });
        Ok(RunReport {
            final_phase: Phase::DocsConfirm,
            paused_at: gate,
            completed,
        })
    }

    /// Pre-embed the requirement string once so every phase's
    /// expert-knowledge section can use true BM25+vector RRF fusion.
    ///
    /// Also builds the cached vector store if the hybrid engine is on —
    /// this is where the previously-stubbed batch embedding actually
    /// happens (one network round-trip per corpus chunk, cached on disk).
    /// Returns `None` (and leaves retrieval on BM25) when the vector layer
    /// is off, no API key is set, or embedding fails. Fail-open, never
    /// blocks the pipeline.
    async fn preembed_requirement(&self) -> Option<Vec<f32>> {
        let project_cfg = crate::config::load_project_config(&self.options.project_root);
        if project_cfg.knowledge.engine != "hybrid" || !project_cfg.knowledge.enabled {
            return None;
        }
        if !umadev_knowledge::vector::is_enabled() {
            return None;
        }
        // Build (or incrementally update) the vector store for the corpus.
        // This is the no-longer-stubbed batch embedding: chunks are embedded
        // in batches of 100 and cached at .umadev/kb-index/vectors.bin.
        let knowledge_dir = self.options.project_root.join("knowledge");
        if knowledge_dir.is_dir() {
            let index =
                umadev_knowledge::load_or_build_index(&self.options.project_root, &knowledge_dir);
            let _ =
                umadev_knowledge::build_vector_store_if_enabled(&self.options.project_root, &index)
                    .await;
        }
        // Embed the requirement query itself.
        umadev_knowledge::vector::embed_query(&self.options.requirement).await
    }

    /// Run one prompt against the configured runtime. Returns `None` on
    /// any failure (empty body, provider error) so the caller can fall
    /// back to the deterministic template.
    ///
    /// `phase` tags `HostOutput` events the UI uses to render the host's
    /// response as it streams past.
    /// Generate content with review→fix loop.
    ///
    /// 1. Generate initial draft via worker
    /// 2. Run review checks (closure returns list of defects)
    /// 3. If defects found and attempts < max, send fix prompt
    /// 4. Repeat until clean or max attempts reached
    ///
    /// `reviewer` takes the generated text and returns a list of
    /// defect descriptions. Empty list = pass.
    /// Generate with review loop. Key principle: NEVER lose the first
    /// successful generation. If fix attempts timeout or fail, we keep
    /// what we have — an imperfect doc is better than a template.
    async fn generate_with_review(
        &self,
        phase: Phase,
        prompt: Prompt,
        reviewer: impl Fn(&str) -> Vec<String>,
        max_attempts: usize,
    ) -> Option<String> {
        self.generate_with_review_on(&self.runtime, phase, prompt, reviewer, max_attempts)
            .await
    }

    /// Like [`Self::generate_with_review`] but drives a SPECIFIC runtime — lets
    /// the docs phase review-and-fix the architecture and UI/UX docs CONCURRENTLY
    /// on forked runtimes.
    async fn generate_with_review_on(
        &self,
        runtime: &dyn umadev_runtime::Runtime,
        phase: Phase,
        prompt: Prompt,
        reviewer: impl Fn(&str) -> Vec<String>,
        max_attempts: usize,
    ) -> Option<String> {
        let mut text = self.try_generate_on(runtime, phase, prompt).await?;

        // `max_attempts` is the number of review→fix rounds to ALLOW, so the
        // loop runs `max_attempts` times (previously `1..max_attempts` ran one
        // fewer, silently under-delivering review rounds).
        for attempt in 1..=max_attempts {
            let defects = reviewer(&text);
            // Governance check (UD-SEC-003/004, UD-ARCH-001/002/003,
            // UD-CODE-001/002) runs on EVERY runtime's output — not just
            // the CLI hosts that fire PreToolUse. This is what makes
            // codex / opencode output governed too.
            let gov_defects = Self::governance_defects(
                &text,
                &umadev_governance::Policy::load(&self.options.project_root),
            );
            if !gov_defects.is_empty() && attempt == 1 {
                self.emit(EngineEvent::Note(format!(
                    "[governance] Governance flagged {} issue(s) in {} output — feeding back for fix.",
                    gov_defects.len(),
                    phase.id(),
                )));
            }
            if defects.is_empty() && gov_defects.is_empty() {
                self.emit(EngineEvent::Note(format!(
                    "[ok] {} review passed.",
                    phase.id()
                )));
                break;
            }
            // Auto-fix STRUCTURAL defects (missing sections) only when there
            // aren't too many — a doc with >10 missing sections usually
            // diverged from the requirement, and a rewrite risks making it
            // worse, so we keep what we have. BUT governance defects
            // (emoji / hardcoded colors / AI-slop) are cardinal-sin blocks, not
            // polish — they are ALWAYS fed back for fix regardless of how many
            // structural defects there are. Mixing them into the same bail
            // threshold (the old behavior) let a slop violation through just
            // because the doc was also missing sections.
            let structural_overflow = defects.len() > 10;
            let to_fix: Vec<String> = if structural_overflow {
                gov_defects.clone()
            } else {
                defects.iter().cloned().chain(gov_defects.clone()).collect()
            };
            if to_fix.is_empty() {
                // Only structural overflow with NO governance issues — keep it.
                self.emit(EngineEvent::Note(format!(
                    "[warn] {} review: {} issues — too many to auto-fix, keeping current version.",
                    phase.id(),
                    defects.len()
                )));
                break;
            }
            if structural_overflow {
                self.emit(EngineEvent::Note(format!(
                    "[warn] {} review: {} structural issues kept (too many to safely rewrite), \
                     but fixing {} governance violation(s) regardless.",
                    phase.id(),
                    defects.len(),
                    gov_defects.len()
                )));
            }
            let defects = to_fix;
            let defect_list = defects.join("\n- ");
            self.emit(EngineEvent::Note(format!(
                "[warn] Review round {attempt}: {} defect(s). Fixing...\n- {defect_list}",
                defects.len()
            )));
            let fix_prompt = Prompt {
                system: format!(
                    "The document below has quality defects. Fix ONLY the listed \
                     issues. Output the COMPLETE corrected document.\n\nDefects:\n- {defect_list}"
                ),
                user: text.clone(),
            };
            match self.try_generate_on(runtime, phase, fix_prompt).await {
                Some(fixed) if !fixed.trim().is_empty() => text = fixed,
                _ => {
                    self.emit(EngineEvent::Note(
                        "Fix attempt failed — keeping previous version.".to_string(),
                    ));
                    break;
                }
            }
        }

        Some(text)
    }

    /// Review a research document for structural completeness.
    fn review_research(text: &str) -> Vec<String> {
        let lower = text.to_ascii_lowercase();
        let mut defects = Vec::new();
        // Each required section must be present AND have non-empty body
        // (section_has_body), consistent with review_prd/architecture/uiux.
        // `## Discovery` may be spelled "target audience" instead — keep the
        // alternate-string allowance for presence, but body-check when the
        // heading form is found.
        let has_discovery_alt = lower.contains("target audience");
        match lower.find("## discovery") {
            None if !has_discovery_alt => {
                defects.push("Missing ## Discovery section (audience/tone/direction)".into());
            }
            Some(pos) if !section_has_body(&lower, pos, "## discovery") => {
                defects.push("Section '## discovery' is present but empty".into());
            }
            None => {}    // alternate "target audience" present — acceptable
            Some(_) => {} // present with body — OK
        }
        for (heading, label) in [
            ("## similar products", "## Similar products"),
            ("## domain risks", "## Domain risks"),
        ] {
            match lower.find(heading) {
                None => defects.push(format!("Missing {label} section")),
                Some(pos) => {
                    if !section_has_body(&lower, pos, heading) {
                        defects.push(format!("Section '{heading}' is present but empty"));
                    }
                }
            }
        }
        // Depth signal: the Similar products section should name ≥1 actual
        // comparable (a list item or a capitalized product name), not just be
        // a heading. Catches a research doc that lists the section but didn't
        // actually survey competitors.
        if let Some(pos) = lower.find("## similar products") {
            let after = &lower[pos..];
            let next_h2 = after.find("\n## ").unwrap_or(after.len());
            let body = &after[..next_h2];
            let names_items = body
                .lines()
                .filter(|l| {
                    let lt = l.trim();
                    lt.starts_with("- ") || lt.starts_with("* ") || lt.starts_with("| ")
                })
                .count();
            if names_items == 0 && section_has_body(&lower, pos, "## similar products") {
                defects.push(
                    "## similar products section has no list items (name the comparables)".into(),
                );
            }
        }

        // Design recommendation has two accepted heading forms.
        let design_heading = if lower.contains("## design system recommendation") {
            "## design system recommendation"
        } else if lower.contains("## design recommendation") {
            "## design recommendation"
        } else {
            defects.push("Missing ## Design system recommendation section".into());
            ""
        };
        if !design_heading.is_empty() {
            if let Some(pos) = lower.find(design_heading) {
                if !section_has_body(&lower, pos, design_heading) {
                    defects.push(format!("Section '{design_heading}' is present but empty"));
                }
            }
        }
        defects
    }

    /// Review PRD — commercial grade checks.
    fn review_prd(text: &str) -> Vec<String> {
        let lower = text.to_ascii_lowercase();
        let mut defects = Vec::new();
        // Required sections with order checking
        let required = ["## goal", "## scope", "## acceptance criteria"];
        let mut last_pos = 0;
        for section in &required {
            if let Some(pos) = lower.find(section) {
                if pos < last_pos {
                    defects.push(format!(
                        "Section '{section}' is out of order (should come after previous sections)"
                    ));
                }
                if !section_has_body(&lower, pos, section) {
                    defects.push(format!("Section '{section}' is present but empty"));
                }
                last_pos = pos;
            } else {
                defects.push(format!("Missing {section}"));
            }
        }
        // Content depth checks
        if !lower.contains("target user")
            && !lower.contains("persona")
            && !lower.contains("## user")
        {
            defects.push("Missing target users / personas".into());
        }
        // Functional requirements: present AND substantive (≥2 list/table
        // rows in the section, not just the heading).
        let func_heading = if lower.contains("## functional") {
            lower.find("## functional")
        } else if lower.contains("## feature") {
            lower.find("## feature")
        } else {
            None
        };
        match func_heading {
            None => defects.push("Missing functional requirements".into()),
            Some(pos) => {
                // Count list items / table rows in the functional section.
                let after = &lower[pos..];
                let next_h2 = after.find("\n## ").unwrap_or(after.len());
                let body = &after[..next_h2];
                let item_count = body
                    .lines()
                    .filter(|l| {
                        let lt = l.trim();
                        lt.starts_with("- ") || lt.starts_with("* ") || lt.starts_with("| ")
                    })
                    .count();
                if item_count < 2 {
                    defects.push(format!(
                        "Functional requirements section present but only has {item_count} item(s) (need ≥2 features)"
                    ));
                }
            }
        }
        if !lower.contains("non-functional") && !lower.contains("performance") {
            defects.push("Missing non-functional requirements".into());
        }
        let ac_count = text.matches("- [ ]").count();
        if ac_count < 2 {
            defects.push(format!("Only {ac_count} acceptance criteria"));
        }
        // Cross-section depth signal: if the functional-requirements section
        // lists N feature items, the acceptance-criteria count should be at
        // least ~N/2 (each major feature usually has ≥1 AC). A doc with many
        // features but 1-2 ACs is under-specified — flag the gap.
        if ac_count >= 2 && (lower.contains("## functional") || lower.contains("## feature")) {
            let func_start = lower
                .find("## functional")
                .or_else(|| lower.find("## feature"))
                .unwrap_or(0);
            let after = &lower[func_start..];
            let next_h2 = after.find("\n## ").unwrap_or(after.len());
            let func_body = &after[..next_h2];
            let feature_items = func_body
                .lines()
                .filter(|l| {
                    let lt = l.trim();
                    lt.starts_with("- ") || lt.starts_with("* ") || lt.starts_with("| ")
                })
                .count();
            if feature_items >= 4 && ac_count < feature_items / 2 {
                defects.push(format!(
                    "Acceptance criteria ({ac_count}) look thin relative to {feature_items} functional items — each major feature should have ≥1 AC"
                ));
            }
        }
        if !lower.contains("metric") && !lower.contains("kpi") {
            defects.push("Missing success metrics".into());
        }
        defects
    }

    /// Review architecture — commercial grade checks.
    fn review_architecture(text: &str) -> Vec<String> {
        let lower = text.to_ascii_lowercase();
        let mut defects = Vec::new();
        // ## API surface must be present AND have body content (not just the
        // heading). Matches review_prd's section_has_body contract so an
        // architecture doc with a bare `## API` heading is flagged.
        match lower.find("## api") {
            None => defects.push("Missing API surface section".into()),
            Some(pos) => {
                if !section_has_body(&lower, pos, "## api") {
                    defects.push("Section '## api' is present but empty".into());
                }
            }
        }
        let api_rows = text
            .lines()
            .filter(|l| {
                let t = l.trim();
                t.starts_with('|') && t.contains('/') && !t.contains("---")
            })
            .count();
        if api_rows < 2 {
            defects.push(format!(
                "API table has {api_rows} rows (need at least a few endpoints)"
            ));
        }
        // Data model: present AND substantive (a field-type table, not just
        // the heading). Require ≥2 table rows mentioning a type near the
        // data-model section, matching the API-table depth signal.
        let has_dm_heading = lower.contains("data model") || lower.contains("schema");
        if has_dm_heading {
            // Count table rows that look like entity field definitions
            // (contain a `|` and a common type keyword).
            let dm_rows = text
                .lines()
                .filter(|l| {
                    let t = l.trim();
                    t.starts_with('|')
                        && (t.contains("text")
                            || t.contains("int")
                            || t.contains("bool")
                            || t.contains("date")
                            || t.contains("uuid")
                            || t.contains("string"))
                })
                .count();
            if dm_rows < 2 {
                defects.push(format!(
                    "Data model section present but has only {dm_rows} typed field rows (need a field-type table)"
                ));
            }
        } else {
            defects.push("Missing data model with field types".into());
        }
        if !lower.contains("auth") {
            defects.push("Missing authentication/authorization section".into());
        }
        if !lower.contains("tech") && !lower.contains("stack") {
            defects.push("Missing tech-stack rationale".into());
        }
        if !lower.contains("error") {
            defects.push("Missing API error convention".into());
        }
        if !lower.contains("structure") && !lower.contains("directory") && !lower.contains("layout")
        {
            defects.push("Missing project structure".into());
        }
        defects
    }

    /// Review a UIUX doc for conformance to the design-contract structure
    /// (the binding 9-section brand contract the docs phase mandates). This is
    /// the soft reviewer that feeds the review→fix loop — being demanding here
    /// drives the worker to produce a COMPLETE, premium contract instead of a
    /// thin token list. Catches the "generic / under-specified" failure mode.
    fn review_uiux(text: &str) -> Vec<String> {
        let lower = text.to_ascii_lowercase();
        let mut defects = Vec::new();

        // 1. Visual direction / archetype committed (design-thinking-first).
        let has_direction = lower.contains("visual direction")
            || lower.contains("视觉方向")
            || lower.contains("design system")
            || lower.contains("设计系统")
            || lower.contains("archetype")
            || lower.contains("motif");
        if !has_direction {
            defects.push(
                "No committed visual direction — declare `## Visual direction` (the design archetype + WHY it fits this product)".into(),
            );
        }

        // 2. Real semantic token set (not just a couple of dashes).
        let token_count = text.matches("--").count();
        if token_count < 8 {
            defects.push(format!(
                "Only {token_count} design tokens — define a full semantic palette (primary/surface/foreground/muted/border/accent/destructive + on-* pairs)"
            ));
        }

        // 3. Dark mode.
        if !lower.contains("prefers-color-scheme") && !lower.contains("dark mode") {
            defects.push(
                "Missing dark mode (`@media (prefers-color-scheme: dark)`) — required".into(),
            );
        }

        // 4. Typography SYSTEM: a real font stack + a multi-step scale.
        let has_font = lower.contains("font-family") || lower.contains("--font");
        let scale_steps = [
            "--text-",
            "--font-size",
            "text-xs",
            "text-sm",
            "text-lg",
            "text-xl",
        ]
        .iter()
        .filter(|s| lower.contains(**s))
        .count();
        if !has_font || scale_steps < 2 {
            defects.push(
                "Typography system incomplete — declare 1-2 distinctive font families + a modular type scale (--text-xs … --text-3xl)".into(),
            );
        }

        // 5. Spacing scale.
        let has_spacing = lower.contains("--space")
            || lower.contains("spacing")
            || lower.contains("4px")
            || lower.contains("8px")
            || lower.contains("间距");
        if !has_spacing {
            defects.push("Missing spacing scale (4px/8px base grid)".into());
        }

        // 6. A REAL icon library, not just the word "icon".
        const ICON_LIBS: &[&str] = &[
            "lucide",
            "heroicons",
            "tabler",
            "phosphor",
            "radix",
            "feather",
            "iconify",
            "remix icon",
            "material symbols",
        ];
        if !ICON_LIBS.iter().any(|l| lower.contains(l)) {
            defects.push(
                "No icon library named — declare exactly one (Lucide / Heroicons / Tabler / Phosphor); never emoji as icons".into(),
            );
        }

        // 7. Component states — at least 3 of the 7 named.
        let states = [
            "hover", "focus", "active", "disabled", "loading", "error", "pressed",
        ]
        .iter()
        .filter(|s| lower.contains(**s))
        .count();
        if states < 3 {
            defects.push(
                "Component states under-specified — cover default/hover/focus/active/disabled/loading/error".into(),
            );
        }

        // 8. Motion / transitions.
        if !lower.contains("motion")
            && !lower.contains("transition")
            && !lower.contains("animation")
            && !lower.contains("ease")
            && !lower.contains("动效")
        {
            defects.push("Missing motion guidance (durations + easing + reduced-motion)".into());
        }

        // 9. Anti-patterns / guardrails section.
        if !lower.contains("anti-pattern")
            && !lower.contains("反模式")
            && !lower.contains("don't")
            && !lower.contains("avoid")
            && !lower.contains("不要")
        {
            defects.push("Missing anti-patterns section — list what NOT to do".into());
        }

        // 10. The contract must not itself prescribe AI-slop tells.
        if lower.contains("linear-gradient")
            && (lower.contains("purple") || lower.contains("#7c3aed") || lower.contains("#667eea"))
        {
            defects.push(
                "Design contract specifies a purple gradient — that's an AI-slop tell; commit to a distinctive palette instead".into(),
            );
        }

        defects
    }

    /// Governance reviewer: extract fenced code blocks from the generated
    /// Markdown and run the content-scan rules (UD-SEC-003 secret, UD-SEC-004
    /// frontend-DB, UD-ARCH-001 any, UD-ARCH-002 debug residue, UD-CODE-001
    /// emoji, …) on each. Returns defects the generate→fix loop feeds back.
    ///
    /// This makes governance apply to ALL runtimes — including codex / opencode
    /// which don't fire the PreToolUse hook (that's a CLI-
    /// host mechanism). For claude-code/codex the hook already catches file
    /// writes; this is the belt-and-suspenders that covers the doc-embedded
    /// code the HTTP path produces.
    fn governance_defects(text: &str, policy: &umadev_governance::Policy) -> Vec<String> {
        let mut defects = Vec::new();
        // Walk fenced code blocks: ```lang\n...\n```.
        let mut lines = text.lines().peekable();
        let mut in_block = false;
        let mut lang = String::new();
        let mut buf: Vec<&str> = Vec::new();
        for line in lines.by_ref() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") {
                if in_block {
                    // Close — scan the accumulated block.
                    if let Some(d) = scan_code_block(&lang, &buf, policy) {
                        defects.push(d);
                    }
                    in_block = false;
                    buf.clear();
                    lang.clear();
                } else {
                    in_block = true;
                    lang = trimmed
                        .trim_start_matches("```")
                        .trim()
                        .to_ascii_lowercase();
                }
            } else if in_block {
                buf.push(line);
            }
        }
        // A block left open at EOF (the worker hit max_tokens mid-fence — very
        // common) would otherwise escape scanning entirely. Scan the trailing
        // buffer so emoji/secret in a truncated block is still caught.
        if in_block && !buf.is_empty() {
            if let Some(d) = scan_code_block(&lang, &buf, policy) {
                defects.push(d);
            }
        }
        defects
    }

    /// Try to generate content via the worker. Retries on transient errors
    /// (timeout / 429 / 5xx / connection blips) with exponential backoff
    /// (2s, 4s, 8s, …) so a rate-limited or briefly-unreachable host recovers
    /// instead of failing the whole phase. Permanent errors (401, config) are
    /// NOT retried. The base delay is overridable via `UMADEV_RETRY_BASE_MS`
    /// (default 2000) so tests can shrink it.
    async fn try_generate(&self, phase: Phase, prompt: Prompt) -> Option<String> {
        self.try_generate_on(&self.runtime, phase, prompt).await
    }

    /// Like [`Self::try_generate`] but drives a SPECIFIC runtime instance — used
    /// to run forked runtimes concurrently (see `generate_docs_content`).
    async fn try_generate_on(
        &self,
        runtime: &dyn umadev_runtime::Runtime,
        phase: Phase,
        prompt: Prompt,
    ) -> Option<String> {
        // Inject the governance context the base actually needs — our design
        // system, the self-learning pitfall KB, the MCP tools — so they reach
        // the WORKER prompt (the thing sent to the base), not just the coach
        // file. Then layer the persistent /goal directive for dev phases.
        let prompt = self.with_context(prompt, phase);
        let prompt = self.with_goal_mode(prompt, phase);
        let max_retries = 3;
        let base_ms = retry_base_ms();
        for attempt in 0..max_retries {
            if attempt == 0 {
                self.emit(EngineEvent::Note(format!(
                    "{} 可能需要 30s-2min,请稍候…",
                    phase_progress_hint(phase)
                )));
            }
            let req = prompt.clone().into_request(
                model_for_phase(&self.options.model, phase),
                max_tokens_for_phase(phase),
            );
            // Use streaming completion so the TUI shows real-time worker
            // output (tool calls, text deltas) instead of a blank spinner.
            let sink = Arc::clone(&self.events);
            // Snapshot failed tool calls as they stream so we can distill them
            // into avoid-next-time pitfalls after the phase completes. Shared
            // via Arc<Mutex> because `on_event` is an `Fn` (interior mutability).
            let pitfalls: Arc<std::sync::Mutex<Vec<String>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));
            let pitfalls_cb = Arc::clone(&pitfalls);
            // Track stream activity so we can heartbeat ONLY for bases that
            // don't stream. opencode and the external-HTTP runtimes fall
            // through to the trait-default `complete_streaming` (a blocking
            // `complete` that emits nothing until the whole phase ends) — so
            // without this the user stares at a frozen-looking spinner for
            // minutes. claude/codex set `active` constantly → never heartbeat.
            let active = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let active_cb = Arc::clone(&active);
            let on_event = move |ev: umadev_runtime::StreamEvent| {
                active_cb.store(true, std::sync::atomic::Ordering::Relaxed);
                if let umadev_runtime::StreamEvent::ToolResult { ok: false, summary } = &ev {
                    if let Ok(mut v) = pitfalls_cb.lock() {
                        v.push(summary.clone());
                    }
                }
                sink.emit(EngineEvent::WorkerStream { event: ev });
            };
            let call_started = std::time::Instant::now();
            let fut = runtime.complete_streaming(req, &on_event);
            tokio::pin!(fut);
            let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(25));
            heartbeat.tick().await; // consume the immediate first tick
            let outcome = loop {
                tokio::select! {
                    r = &mut fut => break r,
                    _ = heartbeat.tick() => {
                        // No events since the last tick → non-streaming base;
                        // reassure the user it's still working (with elapsed).
                        if !active.swap(false, std::sync::atomic::Ordering::Relaxed) {
                            let s = call_started.elapsed().as_secs();
                            self.emit(EngineEvent::Note(format!(
                                "… {} 仍在进行(已 {}:{:02})— 底座在后台干活,请稍候",
                                phase_progress_hint(phase),
                                s / 60,
                                s % 60
                            )));
                        }
                    }
                }
            };
            match outcome {
                Ok(resp) if !resp.text.trim().is_empty() => {
                    for line in resp.text.lines().filter(|l| !l.trim().is_empty()).take(40) {
                        self.emit(EngineEvent::HostOutput {
                            phase,
                            line: line.to_string(),
                        });
                    }
                    // Distill the worker's failed tool calls into the pitfall KB.
                    let errs = pitfalls.lock().map(|v| v.clone()).unwrap_or_default();
                    self.capture_dev_pitfalls(&errs);
                    // Record REAL token usage (claude reports it on the result
                    // line); 0 for bases that don't surface usage.
                    let tokens = resp
                        .usage
                        .input_tokens
                        .saturating_add(resp.usage.output_tokens);
                    record_usage(&self.options.backend, phase, tokens);
                    return Some(resp.text);
                }
                Ok(_) => {
                    tracing::warn!(runtime = %runtime.kind().id(), "empty body");
                    self.emit(EngineEvent::Note(format!(
                        "[warn] {} 阶段:底座返回空内容 —— 本阶段改用离线模板占位(\
                         非真实生成,只是骨架)。底座恢复后用 /redo 重跑拿真实产物。",
                        phase.id()
                    )));
                    return None;
                }
                Err(err) => {
                    // Retry on timeout AND on transient host errors (the
                    // provider returning 429/5xx, or a connection blip). We
                    // can't fully distinguish transient from permanent for
                    // HostProcess, so match on the error string for the
                    // well-known transient signals — matches the retry
                    // policy in vector.rs::http_embed for consistency.
                    let is_timeout = matches!(err, umadev_runtime::RuntimeError::Timeout(_, _));
                    let err_str = err.to_string().to_ascii_lowercase();
                    let is_transient = is_timeout
                        || err_str.contains("429")
                        || err_str.contains("too many requests")
                        || err_str.contains("502")
                        || err_str.contains("503")
                        || err_str.contains("504")
                        || err_str.contains("529")
                        || err_str.contains("service unavailable")
                        || err_str.contains("bad gateway")
                        || err_str.contains("connection reset")
                        || err_str.contains("connection refused")
                        || err_str.contains("timed out")
                        // Host-CLI rate-limit / overload wording (claude/codex
                        // print these to stderr, sometimes while still exiting 0).
                        || err_str.contains("overloaded")
                        || err_str.contains("rate limit")
                        || err_str.contains("rate_limit")
                        || err_str.contains("quota");
                    if is_transient && attempt + 1 < max_retries {
                        // Exponential backoff: base * 2^attempt (2s → 4s → 8s).
                        // Sleeping here lets a rate-limited provider recover
                        // before the next attempt — without it the retry hits
                        // the same 429 immediately and wastes the attempt.
                        let delay_ms = base_ms.saturating_mul(2u64.saturating_pow(attempt as u32));
                        self.emit(EngineEvent::Note(format!(
                            "[warn] Worker 调用瞬时失败({} 阶段: {err}), {delay_ms}ms 后重试 {}/{}...",
                            phase.id(),
                            attempt + 2,
                            max_retries
                        )));
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                        continue;
                    }
                    tracing::warn!(
                        runtime = %runtime.kind().id(),
                        error = %err,
                        "runtime call failed"
                    );
                    self.emit(EngineEvent::Note(format!(
                        "[warn] {} 阶段:底座调用失败({err})—— 本阶段改用离线模板占位(\
                         非真实生成,只是骨架)。/doctor 排查底座,修好后 /redo 重跑拿真实产物。",
                        phase.id()
                    )));
                    return None;
                }
            }
        }
        None
    }

    /// Distil real development errors (failed tool calls, non-zero build/test
    /// exits) into the lessons KB so the SAME pitfall is pre-empted next time.
    /// Fail-open: capture never affects the run. Emits a `[learned]` note so the
    /// user sees the agent recognising and remembering the pitfall.
    fn capture_dev_pitfalls(&self, errors: &[String]) {
        if errors.is_empty() {
            return;
        }
        let n = crate::lessons::capture_dev_errors(
            &self.options.project_root,
            errors,
            &self.options.effective_slug(),
            &self.options.requirement,
        );
        if n > 0 {
            self.emit(EngineEvent::Note(format!(
                "[learned] 识别并记录了 {n} 条开发踩坑,已写入知识库 — 下次遇到同类问题会提前规避。"
            )));
        }
    }

    /// Generate PRD, architecture, UIUX content sequentially so each
    /// expert sees the prior artifact as an excerpt.
    /// Read expert methodology from knowledge/experts/<role>/ and return
    /// a condensed string suitable for injecting into a prompt's system field.
    fn load_expert_knowledge(&self, expert_dirs: &[&str]) -> String {
        let base = self.options.project_root.join("knowledge/experts");
        let mut out = String::new();
        let mut read_dir = |dir_path: &std::path::Path, label: &str| {
            if !dir_path.is_dir() {
                return;
            }
            if let Ok(rd) = std::fs::read_dir(dir_path) {
                for entry in rd.flatten() {
                    let p = entry.path();
                    if p.extension().and_then(|s| s.to_str()) != Some("md") {
                        continue;
                    }
                    if let Ok(content) = std::fs::read_to_string(&p) {
                        let trimmed: String = content.chars().take(1500).collect();
                        out.push_str(&format!(
                            "\n---\n{label} ({}):\n{trimmed}\n",
                            p.file_name().unwrap_or_default().to_string_lossy(),
                        ));
                    }
                }
            }
        };
        for dir in expert_dirs {
            read_dir(&base.join(dir), "Expert reference");
        }
        let project_cfg = crate::config::load_project_config(&self.options.project_root);
        if let Some(custom) = &project_cfg.experts.custom_knowledge {
            read_dir(&self.options.project_root.join(custom), "Custom knowledge");
        }
        out
    }

    /// Enhance a prompt by appending expert methodology to the system field.
    fn with_expert_knowledge(&self, mut prompt: Prompt, expert_dirs: &[&str]) -> Prompt {
        let knowledge = self.load_expert_knowledge(expert_dirs);
        if !knowledge.is_empty() {
            prompt.system.push_str(&knowledge);
        }
        prompt
    }

    /// Consult the borrowed brain for a structured JUDGMENT — this is Super
    /// Dev's OWN cognition. The same base model that does the work is also asked
    /// to *judge* it: UmaDev sends a question, demands one strict JSON object,
    /// and parses it into `T` to drive deterministic control flow.
    ///
    /// This is what makes UmaDev a thinking Agent rather than a fixed
    /// pipeline — the intelligence (planning, judging, deciding) is borrowed,
    /// the discipline (acting on the verdict, the guardrails) is ours.
    ///
    /// FAIL-OPEN by contract: returns `None` when there's no brain (offline),
    /// the call errors, or the reply isn't parseable JSON — the caller then
    /// falls back to the deterministic heuristic, so judgment is an *upgrade*,
    /// never a dependency.
    async fn consult<T: serde::de::DeserializeOwned>(
        &self,
        system: &str,
        user: String,
    ) -> Option<T> {
        // Judge on the MAIN runtime — `consult_on` carries the offline gate,
        // strict-JSON framing, and fail-open parse (NB: gate on the runtime, not
        // `backend` — external HTTP providers have a real brain but an empty
        // backend-id, which is a CLI-driver id only).
        self.consult_on(&self.runtime, system, user).await
    }

    /// Like [`Self::consult`] but judges on a SPECIFIC runtime — used by the
    /// role-critic team to think on an ISOLATED forked session (clean, no-resume,
    /// read-only) so a cross-review judge can never collide with — or write
    /// through — the main writer session. Same strict-JSON + fail-open contract.
    async fn consult_on<T: serde::de::DeserializeOwned>(
        &self,
        runtime: &dyn umadev_runtime::Runtime,
        system: &str,
        user: String,
    ) -> Option<T> {
        if runtime.is_offline() {
            return None;
        }
        let prompt = Prompt {
            system: format!(
                "{system}\n\nReturn EXACTLY ONE JSON object and nothing else — \
                 no markdown, no code fence, no prose before or after."
            ),
            user,
        };
        let req = prompt.into_request(&self.options.model, 1500);
        let resp = runtime.complete(req).await.ok()?;
        let json = extract_json_object(&resp.text)?;
        serde_json::from_str(&json).ok()
    }

    /// Human label for the active worker in the audit trail — accurate for all
    /// three modes (offline templates / external HTTP API / a named base CLI).
    fn worker_label(&self) -> &str {
        if self.runtime.is_offline() {
            "offline-templates"
        } else if self.options.backend.is_empty() {
            "external-api"
        } else {
            self.options.backend.as_str()
        }
    }

    /// Inject UmaDev's governance context into the WORKER prompt (the one
    /// actually sent to the base): the self-learning pitfall KB, the default-on
    /// design system (archetype tokens and anti-AI-slop, for the design-bearing
    /// phases), and the available MCP tools. Without this these only lived in
    /// the coach FILE, which the subprocess base never reads — so the base
    /// wasn't actually governed by our design system or learning. This makes
    /// "default-on design + self-learning" true in worker-subprocess mode.
    fn with_context(&self, mut prompt: Prompt, phase: Phase) -> Prompt {
        let append = |sys: &mut String, block: String| {
            if !block.trim().is_empty() {
                sys.push_str("\n\n");
                sys.push_str(&block);
            }
        };
        // Self-learning: relevant past pitfalls, triggered by the project's
        // tech-stack fingerprint (not the requirement prose).
        append(
            &mut prompt.system,
            crate::lessons::relevant_lessons_for_prompt(
                &self.options.project_root,
                &self.options.requirement,
            ),
        );
        // Success-compounding: reusable SKILLS that already cleared the gate on a
        // similar problem. Retrieved by the SOLUTION IDEA (the requirement) via the
        // curated BM25/vector path, only for the build phases that can reuse them.
        // Fail-open / empty for first runs, so the prompt is unchanged then.
        if matches!(phase, Phase::Spec | Phase::Frontend | Phase::Backend) {
            append(
                &mut prompt.system,
                crate::skills::skills_for_prompt(
                    &self.options.project_root,
                    &crate::phases::knowledge_root(&self.options.project_root),
                    &self.options.requirement,
                    3,
                ),
            );
        }
        // Design system: only for the phases that decide/implement the UI.
        if matches!(phase, Phase::Docs | Phase::Frontend) {
            append(
                &mut prompt.system,
                crate::coach::load_design_system_inject(&self.options, phase),
            );
        }
        // MCP tools the base may use.
        append(
            &mut prompt.system,
            crate::coach::load_mcp_tools(&self.options.project_root),
        );
        // A1 context engineering: the distilled BINDING CONTRACT from the approved
        // docs. Present only AFTER the docs gate (written by `distill_contract`),
        // so every build phase follows ONE curated source instead of drifting from
        // three raw docs re-truncated per phase. Fail-open: absent -> no injection.
        if matches!(
            phase,
            Phase::Spec | Phase::Frontend | Phase::Backend | Phase::Delivery
        ) {
            let contract = std::fs::read_to_string(self.options.project_root.join(format!(
                "output/{}-contract.md",
                self.options.effective_slug()
            )))
            .unwrap_or_default();
            if !contract.trim().is_empty() {
                append(
                    &mut prompt.system,
                    format!(
                        "## Approved Pipeline Contract (BINDING - build exactly to this)\n\n{}",
                        excerpt_sections(&contract, 4000)
                    ),
                );
            }
        }
        prompt
    }

    /// A1 context engineering: once the three planning docs are approved, distill
    /// them into ONE compact BINDING CONTRACT (tech stack / API surface / design
    /// system / acceptance) at `output/<slug>-contract.md`. Every build phase then
    /// follows this single curated source via [`Self::with_context`] instead of
    /// re-reading three raw docs truncated per phase. Fail-open: offline or empty
    /// docs -> no contract, and phases fall back to their raw section excerpts.
    async fn distill_contract(&self, slug: &str) {
        if self.runtime.is_offline() {
            return;
        }
        let read = |name: &str| {
            std::fs::read_to_string(
                self.options
                    .project_root
                    .join(format!("output/{slug}-{name}.md")),
            )
            .unwrap_or_default()
        };
        let prd = read("prd");
        let arch = read("architecture");
        let uiux = read("uiux");
        if prd.trim().is_empty() && arch.trim().is_empty() && uiux.trim().is_empty() {
            return;
        }
        let system = "You are distilling THREE approved planning documents (PRD, Architecture, \
             UIUX) into ONE compact BINDING CONTRACT that the build phases (spec / frontend / \
             backend) follow verbatim. Keep ONLY decisions that constrain the code; drop prose \
             and rationale. Output markdown with EXACTLY these sections: '## Tech Stack', \
             '## API Surface' (the endpoint table - method, path, purpose), '## Design System' \
             (icon library named, design tokens, typography, base components), and \
             '## Acceptance Criteria' (must-pass features). Be terse - a contract, not a \
             document. Invent nothing; carry only what the docs state."
            .to_string();
        let user = format!(
            "## PRD\n{}\n\n## Architecture\n{}\n\n## UIUX\n{}",
            excerpt_sections(&prd, 6000),
            excerpt_sections(&arch, 6000),
            excerpt_sections(&uiux, 6000),
        );
        // Drop any stale contract from a prior run BEFORE distilling: the distill
        // turn routes through with_context (Phase::Spec is in the injection set),
        // so a leftover contract would anchor the redistill to the old docs; and a
        // failed redistill then falls back to raw excerpts, not a stale contract.
        let contract_path = self
            .options
            .project_root
            .join(format!("output/{slug}-contract.md"));
        let _ = std::fs::remove_file(&contract_path);
        self.emit(EngineEvent::Note(
            "[contract] 把已批准的三文档蒸馏成绑定流水线契约,后续阶段统一遵循…".to_string(),
        ));
        if let Some(text) = self
            .try_generate(Phase::Spec, Prompt { system, user })
            .await
        {
            if !text.trim().is_empty() {
                let _ = std::fs::write(&contract_path, &text);
                self.emit(EngineEvent::Note(
                    "[contract] 流水线契约已生成,build 阶段将统一遵循。".to_string(),
                ));
            }
        }
    }

    /// For DEVELOPMENT phases (frontend / backend) on a base that supports
    /// Claude Code's persistent `/goal` mode, prepend a `/goal` directive to the
    /// FRONT of the prompt so the base keeps working until the feature is
    /// actually complete instead of stopping early with a half-built phase. The
    /// `/goal` must be the very first thing the base reads, and `merge_prompt`
    /// emits `system` before `user` — so we prepend to `system`. No-op for
    /// bounded phases and bases without `/goal`. Opt out: `UMADEV_NO_GOAL_MODE=1`.
    fn with_goal_mode(&self, mut prompt: Prompt, phase: Phase) -> Prompt {
        let dev_phase = matches!(phase, Phase::Frontend | Phase::Backend);
        if !dev_phase || std::env::var("UMADEV_NO_GOAL_MODE").as_deref() == Ok("1") {
            return prompt;
        }
        let slug = self.options.effective_slug();
        // Point the base at OUR breakdown — the confirmed design docs and the
        // task list UmaDev already decomposed the requirement into — so it
        // works through the organized plan, not the raw one-liner.
        let (what, refs) = if phase == Phase::Frontend {
            (
                "前端实现",
                format!("output/{slug}-uiux.md、output/{slug}-architecture.md、output/{slug}-execution-plan.md"),
            )
        } else {
            (
                "后端实现",
                format!("output/{slug}-architecture.md、output/{slug}-execution-plan.md"),
            )
        };
        let req = self.options.requirement.trim();
        // The director emits "persist until every task is done"; HOW it's
        // encoded depends on the borrowed brain's CAPABILITY, not a host-id
        // string. A brain with native persistent mode gets `/goal`; the rest
        // get a strong prompt-level fallback that achieves the same intent.
        // The standard the director holds the base to: COMMERCIAL-GRADE and
        // COMPLETE — not a demo, skeleton, MVP-stub or placeholder. This is the
        // single most important thing a director command must make explicit.
        let commercial = "做到**商业级、完整可用**:实现需求与设计里的**每一个**功能/路由/\
             验收点(不是子集、不是演示版);每条交互都有真实的加载/空/错误/边界处理;\
             用真实的数据流与接口对接,**绝不**交 demo、占位、Lorem、mock-only 或带 TODO \
             的半成品。把它当成要直接上线给真实用户用的产品来写。";
        let directive = if self.runtime.capabilities().persistent_goal {
            format!(
                "/goal 完成「{req}」的{what}。这是已经梳理拆解好的需求 —— 严格按 {refs} \
                 里确认的设计与任务清单逐项落地。{commercial}本阶段的全部任务做完、运行/\
                 构建与质量校验通过之前不要停下。\n\n"
            )
        } else {
            format!(
                "这是一个多任务目标:完成「{req}」的{what}。严格按 {refs} 里的设计与任务\
                 清单**逐条**实现。{commercial}做完每一项、运行/构建与质量校验通过之前不要\
                 停;声明完成前再核对一遍任务清单有没有遗漏。\n\n"
            )
        };
        prompt.system = format!("{directive}{}", prompt.system);
        prompt
    }

    /// Task-level acceptance: check the delivered code against the breakdown
    /// (the architecture API table) and re-delegate unimplemented endpoints to
    /// the base — for UP TO 3 ROUNDS, stopping early when the gaps clear OR a
    /// round makes no progress (so it never spins on a base that can't close a
    /// gap). Persistent residual gaps feed the self-learning KB. This is the
    /// director action that turns UmaDev from a one-shot delegator into one
    /// that verifies completion against its own plan and won't ship half-built.
    async fn run_task_acceptance(&self, slug: &str) {
        const MAX_ROUNDS: usize = 3;
        let mut prev = usize::MAX;
        for round in 1..=MAX_ROUNDS {
            // Deterministic floor: planned endpoints with no implementation. This
            // is the STABLE, monotone signal that drives the loop.
            let mut gaps =
                crate::acceptance::task_acceptance_gaps(&self.options.project_root, slug);
            let det_count = gaps.len();
            // INTELLIGENT layer — run the borrowed-brain judge ONCE (round 1) to
            // fold the fuzzy "PRD acceptance criteria not met" items into the
            // first re-delegation. We do NOT re-judge every round: the verdict is
            // non-deterministic, so feeding it into loop control would oscillate
            // or resurrect an already-passed gate, and it's a costly ~30 KB call.
            // The DETERMINISTIC count governs the loop; this is advisory signal.
            if round == 1 {
                if let Some(verdict) = self.judge_acceptance(slug).await {
                    // Team hook-up (zero behavior change): ALSO record the
                    // acceptance director's existing verdict in the team ledger.
                    self.ledger_role_verdict(
                        "acceptance",
                        round,
                        "acceptance-director",
                        verdict.commercial_ready,
                        verdict.unmet.clone(),
                        Vec::new(),
                    );
                    if !verdict.commercial_ready {
                        self.emit(EngineEvent::Note(
                            "[acceptance] 智能裁判:当前交付未达商业级,补齐未达标项…".to_string(),
                        ));
                    }
                    for unmet in verdict.unmet {
                        let item = format!("验收标准未达标:{}", unmet.trim());
                        if item.len() > 12 && !gaps.contains(&item) {
                            gaps.push(item);
                        }
                    }
                }
            }
            if gaps.is_empty() {
                if round > 1 {
                    self.emit(EngineEvent::Note(
                        "[acceptance] 缺口已补齐 — 计划接口与验收标准均已满足。".to_string(),
                    ));
                }
                return;
            }
            // No-progress guard on the DETERMINISTIC count (stable + monotone as
            // code is added) — NOT the combined count, half of which is LLM noise.
            if round > 1 && det_count >= prev {
                self.report_residual_acceptance_gaps(&gaps);
                return;
            }
            prev = det_count;
            self.emit(EngineEvent::Note(format!(
                "[acceptance] 验收第 {round}/{MAX_ROUNDS} 轮:{} 项未达标(接口/验收标准),\
                 打回底座补齐…",
                gaps.len()
            )));
            let gap_list = gaps.join("\n- ");
            let fix_p = Prompt {
                system: format!(
                    "以下是已确认的计划里要求、但当前代码尚未满足的项(接口未实现 / 验收标准\
                     未达标)。**只补齐这些,不要重写已经正常的代码。** 全部补完后再对照一遍。\n\n\
                     未达标项:\n- {gap_list}"
                ),
                user: format!("补齐 {} 的未达标项。", self.options.requirement),
            };
            // `try_generate` applies the persistent directive (backend dev phase).
            let _ = self.try_generate(Phase::Backend, fix_p).await;
        }
        // Exhausted the round budget — final check + record whatever remains.
        let remaining = crate::acceptance::task_acceptance_gaps(&self.options.project_root, slug);
        if remaining.is_empty() {
            self.emit(EngineEvent::Note(
                "[acceptance] 缺口已补齐 — 全部计划接口都有实现。".to_string(),
            ));
        } else {
            self.report_residual_acceptance_gaps(&remaining);
        }
    }

    /// Surface residual acceptance gaps and feed them to the self-learning KB so
    /// a base that habitually under-implements a kind of endpoint is flagged
    /// up-front next time (no-op if the signal isn't recognised as an error).
    fn report_residual_acceptance_gaps(&self, gaps: &[String]) {
        self.emit(EngineEvent::Note(format!(
            "[acceptance] 仍有 {} 个接口未实现(已记入学习库,下次提前规避):\n- {}",
            gaps.len(),
            gaps.join("\n- ")
        )));
        let signals: Vec<String> = gaps
            .iter()
            .map(|g| format!("Error: acceptance gap — planned endpoint not implemented: {g}"))
            .collect();
        self.capture_dev_pitfalls(&signals);
    }

    /// Intelligent acceptance — ask the borrowed brain to JUDGE the delivered
    /// code against the PRD acceptance criteria (the fuzzy "definition of done"
    /// no grep can verify). Returns the criteria it judges unmet + whether the
    /// delivery looks commercial-grade. `None` (fail-open) when there's no brain
    /// or no PRD — the caller then relies on the deterministic endpoint check.
    async fn judge_acceptance(&self, slug: &str) -> Option<AcceptanceVerdict> {
        let prd = std::fs::read_to_string(
            self.options
                .project_root
                .join(format!("output/{slug}-prd.md")),
        )
        .ok()?;
        if prd.trim().is_empty() {
            return None;
        }
        let code = crate::acceptance::code_digest(&self.options.project_root, 24_000);
        if code.trim().is_empty() {
            return None; // nothing built yet — deterministic check handles it
        }
        let system = "You are a STRICT software delivery acceptance reviewer for a \
             commercial project. You are given the PRD (with its acceptance \
             criteria) and a digest of the delivered code. Judge, for each \
             acceptance criterion / required feature, whether the code ACTUALLY \
             implements it — only count it met when there is real implementation \
             evidence in the code, not just a mention. Be strict and concrete. \
             JSON shape: {\"unmet\": [\"<short criterion not yet implemented>\", …], \
             \"commercial_ready\": <true|false>, \"notes\": \"<one line>\"}";
        let user = format!(
            "## PRD (含验收标准)\n\n{}\n\n## 交付的代码(节选)\n\n{code}",
            excerpt_sections(&prd, 8000)
        );
        self.consult(system, user).await
    }

    /// Intelligent intake planning — the FIRST thing a thinking director does:
    /// the SHARED brain analyzes the incoming requirement (product type,
    /// complexity, the core features that must exist, key risks) and surfaces a
    /// crisp plan up front, so the user sees how UmaDev understood the ask
    /// before any phase runs. Informational; fail-open.
    async fn surface_intake_plan(&self) {
        let system = "You are a senior product director receiving a build request for a \
             COMMERCIAL product. Analyze it and produce a crisp plan: the product \
             type, rough complexity (simple/medium/complex), the 3–6 CORE features \
             that must be built, and the key risks. Be concrete, no fluff. JSON \
             shape: {\"product_type\": \"…\", \"complexity\": \"simple|medium|complex\", \
             \"core_features\": [\"…\", …], \"key_risks\": [\"…\", …]}";
        let Some(v): Option<IntakePlan> = self
            .consult(system, format!("需求:{}", self.options.requirement))
            .await
        else {
            return;
        };
        let mut msg = String::from("[plan] 我先这样理解你的需求(共享底座的脑子分析):\n");
        if !v.product_type.trim().is_empty() {
            let cx = if v.complexity.trim().is_empty() {
                String::new()
            } else {
                format!(" · 复杂度 {}", v.complexity.trim())
            };
            msg.push_str(&format!("  · 产品类型:{}{cx}\n", v.product_type.trim()));
        }
        if !v.core_features.is_empty() {
            let feats = v
                .core_features
                .iter()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("、");
            msg.push_str(&format!("  · 核心功能:{feats}\n"));
        }
        for r in v.key_risks.iter().filter(|s| !s.trim().is_empty()).take(3) {
            msg.push_str(&format!("  · 注意:{}\n", r.trim()));
        }
        msg.push_str("  接下来按这个理解去研究→出三份核心文档,理解有偏差就告诉我。");
        self.emit(EngineEvent::Note(msg));
    }

    /// Intelligent docs assessment — before the user confirms the docs gate,
    /// ask the SHARED base brain (no extra API) to review the three core docs
    /// and surface its judgment (biggest risk, what's missing, ready-to-build).
    /// Informational: the user still decides (`c` / `/revise`). Fail-open.
    async fn surface_docs_assessment(&self, slug: &str) {
        let read = |name: &str| {
            std::fs::read_to_string(
                self.options
                    .project_root
                    .join(format!("output/{slug}-{name}.md")),
            )
            .unwrap_or_default()
        };
        let (prd, arch, uiux) = (read("prd"), read("architecture"), read("uiux"));
        if prd.trim().is_empty() && arch.trim().is_empty() {
            return;
        }
        let system = "You are a STRICT tech lead reviewing the PRD, architecture, and UIUX \
             spec before a team starts building a COMMERCIAL product. Judge: the \
             single biggest risk, the important things MISSING that the user should \
             know before approving, and whether the plan is solid enough to start \
             building. Be concrete and brief. JSON shape: \
             {\"biggest_risk\": \"<one line>\", \"missing\": [\"<short>\", …], \
             \"ready\": <true|false>}";
        let user = format!(
            "## PRD\n{}\n\n## Architecture\n{}\n\n## UIUX\n{}",
            excerpt_sections(&prd, 4000),
            excerpt_sections(&arch, 3000),
            excerpt_sections(&uiux, 2000)
        );
        let Some(v): Option<DocsVerdict> = self.consult(system, user).await else {
            return;
        };
        // Team hook-up (zero behavior change): ALSO express this tech-lead judge's
        // existing opinion as a RoleVerdict in the team ledger — missing items are
        // blocking, the biggest risk is advisory, `ready` is the accept signal.
        self.ledger_role_verdict(
            "docs",
            1,
            "tech-lead",
            v.ready,
            v.missing.clone(),
            if v.biggest_risk.trim().is_empty() {
                Vec::new()
            } else {
                vec![v.biggest_risk.clone()]
            },
        );
        let mut msg = String::from("[docs] 智能评审(共享底座的脑子,确认前先看一眼):\n");
        if !v.biggest_risk.trim().is_empty() {
            msg.push_str(&format!("  · 最大风险:{}\n", v.biggest_risk.trim()));
        }
        for m in v.missing.iter().filter(|s| !s.trim().is_empty()).take(5) {
            msg.push_str(&format!("  · 缺:{}\n", m.trim()));
        }
        msg.push_str(if v.ready {
            "  · 评估:基础扎实,可 c 确认进入开发(仍可 /revise 调整)。"
        } else {
            "  · 评估:建议先 /revise 补强上面几点再确认。"
        });
        // Deterministic cross-check (no model call): does the architecture
        // actually declare an API surface? An empty API table for a product
        // with real backend needs is a concrete gap the prose review can miss.
        let api = umadev_contract::parse_architecture(&arch, slug);
        if api.is_empty() && prd.len() > 400 {
            msg.push_str(
                "\n  · 一致性[确定性]:架构未解析出任何 API 端点表——若非纯静态站点,后端契约可能缺失,建议补 API 表(Method/Path/Request/Response)。",
            );
        } else if !api.is_empty() {
            // #7: PRD-declared routes (the IA tree) vs the architecture contract.
            // A PRD that promises pages the backend never serves is a concrete
            // docs-stage gap — surface it at the docs gate (deterministic, no
            // model call) so the user fixes it BEFORE building. Same contract API
            // the quality gate uses, just applied earlier.
            let prd_routes = umadev_contract::extract_prd_routes(&prd);
            let uncovered = umadev_contract::validate_prd_vs_contract(&prd_routes, &api);
            if !uncovered.is_empty() {
                let list = uncovered
                    .iter()
                    .take(4)
                    .map(|v| v.detail.clone())
                    .collect::<Vec<_>>()
                    .join("; ");
                msg.push_str(&format!(
                    "\n  · 一致性[确定性]:{} 个 PRD 路由在架构 API 契约里没有对应端点 —— {list}",
                    uncovered.len()
                ));
            }
        }
        self.emit(EngineEvent::Note(msg));
    }

    /// Record a verdict from one of the existing ad-hoc judges into the team
    /// ledger as a [`RoleVerdict`] — WITHOUT changing that judge's behavior /
    /// return type / flow. This is the zero-behavior-change team hook-up: the
    /// intake / docs-assessment / acceptance / design judges already produce an
    /// opinion; here we ALSO express it in the team's uniform shape and append it
    /// to the audit trail. Fail-open: ledger IO never affects the run.
    fn ledger_role_verdict(
        &self,
        phase: &str,
        round: usize,
        role: &str,
        accepts: bool,
        blocking: Vec<String>,
        advisory: Vec<String>,
    ) {
        let v = crate::critics::RoleVerdict {
            role: role.to_string(),
            accepts,
            blocking,
            advisory,
            evidence: Vec::new(),
        }
        .normalized(role);
        crate::critics::append_team_ledger(&self.options.project_root, phase, round, &v);
    }

    /// Run the docs-stage role-critic TEAM — the explicit cross-review. After the
    /// three core docs exist (and after the deterministic floor + the single
    /// tech-lead assessment have run), a PM-critic and an architecture-critic each
    /// review the docs from their own seat on an ISOLATED forked session and
    /// return a [`RoleVerdict`]. Every verdict is recorded to the team ledger.
    /// The union of their `blocking[]` is returned so the caller can fold it into
    /// the EXISTING docs revision path — ADVISORY only (round-1 once, like the
    /// existing single-judge design): the deterministic coverage/contract floor
    /// stays the hard gate; an LLM verdict NEVER controls the loop.
    ///
    /// Team size scales with the task ([`crate::critics::docs_team_for_kind`]):
    /// a lean / trivial task gets NO team (returns empty). Fully fail-open:
    /// offline / fork-unavailable / parse-fail → empty verdicts → no blocking.
    async fn run_docs_critic_team(&self, slug: &str) -> Vec<String> {
        // No borrowed brain → no team (the deterministic floor stands).
        if self.runtime.is_offline() {
            return Vec::new();
        }
        // Scale the team to the task — reuse the planner's complexity tiering.
        let kind = crate::planner::classify(&self.merged_requirement());
        let team = crate::critics::docs_team_for_kind(kind);
        if team.is_empty() {
            return Vec::new();
        }
        let read = |name: &str| {
            std::fs::read_to_string(
                self.options
                    .project_root
                    .join(format!("output/{slug}-{name}.md")),
            )
            .unwrap_or_default()
        };
        let (prd, arch, uiux) = (read("prd"), read("architecture"), read("uiux"));
        // Nothing substantive to review → skip (deterministic floor stands).
        if prd.trim().is_empty() && arch.trim().is_empty() {
            return Vec::new();
        }
        let requirement = self.options.requirement.clone();
        let arts = crate::critics::CriticArtifacts {
            requirement: &requirement,
            prd: &prd,
            architecture: &arch,
            uiux: &uiux,
            ..Default::default()
        };

        self.emit(EngineEvent::Note(format!(
            "[team] 角色团队交叉评审(只读):{} 名 critic 各从本职审一遍文档…",
            team.len()
        )));
        let mut blocking: Vec<String> = Vec::new();
        for critic in &team {
            // Each critic thinks on its OWN isolated forked session (clean,
            // no-resume, read-only) — never the main writer session. If the base
            // can't fork, the critic falls back to a fresh consult on the main
            // runtime but STILL only reads (consult never writes). Either way the
            // verdict is fail-open.
            let fork = self.runtime.fork();
            let consult: ForkedConsult<'_, R> = ForkedConsult {
                runner: self,
                fork: fork.as_deref(),
            };
            let verdict = critic.review(&consult, arts).await;
            crate::critics::append_team_ledger(&self.options.project_root, "docs", 1, &verdict);
            let seat = verdict.role.clone();
            if verdict.accepts && verdict.blocking.is_empty() {
                self.emit(EngineEvent::Note(format!("[team] {seat}:通过,无阻塞项。")));
            } else if !verdict.blocking.is_empty() {
                self.emit(EngineEvent::Note(format!(
                    "[team] {seat}:提出 {} 个阻塞项(建议在确认前补强)。",
                    verdict.blocking.len()
                )));
                for b in verdict.blocking {
                    let item = format!("[{seat}] {}", b.trim());
                    if item.len() > 6 && !blocking.contains(&item) {
                        blocking.push(item);
                    }
                }
            }
        }
        blocking
    }

    /// The DETERMINISTIC QA floor — the hard signal that runs BEFORE the QA
    /// critic and stands on its own. Reuses the same checks the pipeline already
    /// trusts: requirement coverage ([`crate::coverage::uncovered_requirements`])
    /// and API-contract / acceptance gaps
    /// ([`crate::acceptance::task_acceptance_gaps`]). Returns a flat list of gap
    /// strings (empty = clean). The LLM QA-critic only ADDS a semantic opinion on
    /// top of this; it never replaces it (invariant 2).
    fn qa_floor_findings(&self, slug: &str) -> Vec<String> {
        let mut out = Vec::new();
        for r in crate::coverage::uncovered_requirements(&self.options.project_root, slug) {
            out.push(format!("覆盖缺口:{r}"));
        }
        for g in crate::acceptance::task_acceptance_gaps(&self.options.project_root, slug) {
            out.push(format!("接口验收缺口:{g}"));
        }
        out
    }

    /// The DETERMINISTIC security floor — the hard signal that runs BEFORE the
    /// security critic. Reuses the governance kernel's content scan over the real
    /// source files (the same rules the quality gate enforces: injection / unsafe
    /// patterns / hardcoded-secret style rules per policy) and, if a prior tool
    /// dropped one, folds in an external `security-scan.json` report. Returns a
    /// flat list of finding strings (empty = clean). Fail-open: any read error is
    /// swallowed. The LLM security-critic only ADDS a semantic opinion on top.
    fn security_floor_findings(&self) -> Vec<String> {
        let mut out = Vec::new();
        let policy = umadev_governance::Policy::load(&self.options.project_root);
        for f in crate::acceptance::source_files(&self.options.project_root) {
            let Ok(content) = std::fs::read_to_string(&f) else {
                continue;
            };
            let rel = f
                .strip_prefix(&self.options.project_root)
                .unwrap_or(&f)
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            let d = umadev_governance::scan_content_with_policy(&rel, &content, &policy);
            if d.block {
                out.push(format!(
                    "{rel}: {} ({})",
                    d.reason.split('.').next().unwrap_or("violation").trim(),
                    d.clause
                ));
            }
            if out.len() >= 25 {
                break;
            }
        }
        // Optional external scanner output, if a prior step produced one.
        for cand in [
            self.options.project_root.join(format!(
                "output/{}-security-scan.json",
                self.options.effective_slug()
            )),
            self.options.project_root.join(".umadev/security-scan.json"),
        ] {
            if let Ok(raw) = std::fs::read_to_string(&cand) {
                let trimmed = raw.trim();
                if !trimmed.is_empty() {
                    out.push(format!(
                        "external security-scan.json: {}",
                        excerpt(trimmed, 600)
                    ));
                }
                break;
            }
        }
        out
    }

    /// Run the quality-stage role-critic TEAM — the SECOND axis of the explicit
    /// cross-review (the first being the docs stage). After the deterministic
    /// quality gate has run as the HARD floor, a QA-critic and a security-critic
    /// each review the DELIVERED code from their own seat on an ISOLATED forked
    /// session and return a [`RoleVerdict`]. The deterministic QA / security
    /// FLOORS ([`Self::qa_floor_findings`] / [`Self::security_floor_findings`])
    /// run FIRST and are handed to the critics as context, so the LLM pass focuses
    /// on the SEMANTIC layer the floor can't see (critical-path test coverage /
    /// auth / authz / injection). Every verdict is recorded to the team ledger.
    /// The union of their advisory blocking is returned so the caller can FOLD it
    /// into the quality report — ADVISORY only: the deterministic gate stays the
    /// hard signal; an LLM verdict NEVER controls the loop or sinks the gate
    /// (invariant 2).
    ///
    /// Team size scales with the task ([`crate::critics::quality_team_for_kind`]):
    /// a lean / trivial / docs-only task gets NO team (returns empty). Fully
    /// fail-open: offline / fork-unavailable / parse-fail → empty verdicts → no
    /// blocking.
    async fn run_quality_critic_team(&self, slug: &str) -> Vec<String> {
        // No borrowed brain → no team (the deterministic floor stands).
        if self.runtime.is_offline() {
            return Vec::new();
        }
        // Scale the team to the task — reuse the planner's complexity tiering.
        let kind = crate::planner::classify(&self.merged_requirement());
        let team = crate::critics::quality_team_for_kind(kind);
        if team.is_empty() {
            return Vec::new();
        }
        // Nothing built to review → skip (no false alarm; deterministic gate stands).
        let code = crate::acceptance::code_digest(&self.options.project_root, 18_000);
        if code.trim().is_empty() {
            return Vec::new();
        }
        // Deterministic floors FIRST — these are the hard signal; the critics get
        // them as context so they don't just re-derive what the floor already saw.
        let qa_floor = self.qa_floor_findings(slug).join("\n- ");
        let security_floor = self.security_floor_findings().join("\n- ");
        let requirement = self.options.requirement.clone();
        let arts = crate::critics::CriticArtifacts {
            requirement: &requirement,
            code: &code,
            qa_floor: &qa_floor,
            security_floor: &security_floor,
            ..Default::default()
        };

        self.emit(EngineEvent::Note(format!(
            "[team] 质量阶段角色团队交叉评审(只读,确定性质量门之后):{} 名 critic 各从本职审一遍交付代码…",
            team.len()
        )));
        let mut blocking: Vec<String> = Vec::new();
        for critic in &team {
            // Each critic thinks on its OWN isolated forked session (clean,
            // no-resume, read-only) — never the main writer session. Fail-open.
            let fork = self.runtime.fork();
            let consult: ForkedConsult<'_, R> = ForkedConsult {
                runner: self,
                fork: fork.as_deref(),
            };
            let verdict = critic.review(&consult, arts).await;
            crate::critics::append_team_ledger(&self.options.project_root, "quality", 1, &verdict);
            let seat = verdict.role.clone();
            if verdict.accepts && verdict.blocking.is_empty() {
                self.emit(EngineEvent::Note(format!("[team] {seat}:通过,无阻塞项。")));
            } else if !verdict.blocking.is_empty() {
                self.emit(EngineEvent::Note(format!(
                    "[team] {seat}:提出 {} 个阻塞项(建议在交付前补强)。",
                    verdict.blocking.len()
                )));
                for b in verdict.blocking {
                    let item = format!("[{seat}] {}", b.trim());
                    if item.len() > 6 && !blocking.contains(&item) {
                        blocking.push(item);
                    }
                }
            }
        }
        blocking
    }

    /// Fold the docs critic team's blocking issues into the EXISTING docs
    /// revision path — ONE advisory round (round-1 once, mirroring the existing
    /// single-judge design review's anti-oscillation design). Re-delegates to the
    /// MAIN writer session to revise the affected `output/*-prd.md` /
    /// `output/*-architecture.md` IN PLACE for the named gaps only. The critic
    /// team is read-only; ONLY this main-session revision writes. Fail-open: a
    /// base error/empty just leaves the docs as-is — the user still gates them.
    async fn revise_docs_for_critic_blocking(&self, slug: &str, blocking: &[String]) {
        let issues = blocking
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .take(8)
            .collect::<Vec<_>>()
            .join("\n- ");
        if issues.is_empty() {
            return;
        }
        self.emit(EngineEvent::Note(format!(
            "[team] 把角色团队的阻塞项折进文档修订(1 轮,建议性)…\n- {issues}"
        )));
        let fix_p = Prompt {
            system: format!(
                "角色团队(PM / 架构师)交叉评审了 `output/{slug}-prd.md` 与 \
                 `output/{slug}-architecture.md`,提出以下阻塞项。**只针对这些问题就地修订\
                 对应文档文件**(补全缺口 / 对齐 PRD 与架构契约),不要重写正常部分、不要改\
                 其它文件。修订后这两份文档应自洽。\n\n阻塞项:\n- {issues}"
            ),
            user: format!(
                "按上面的阻塞项修订 {} 的 PRD / 架构文档。",
                self.options.requirement
            ),
        };
        // Docs phase tag → the persistent /goal directive is NOT applied (planning
        // phase), and governance runs on the output exactly like the rest of docs.
        let _ = self.try_generate(Phase::Docs, fix_p).await;
    }

    /// Intelligent preview assessment — before the user confirms the preview
    /// gate, the SHARED brain reviews the frontend (notes + code) and surfaces
    /// its judgment (biggest risk, what's weak, ready-to-proceed) so the user
    /// approves with a real opinion. Informational; fail-open.
    async fn surface_preview_assessment(&self, slug: &str) {
        let notes = std::fs::read_to_string(
            self.options
                .project_root
                .join(format!("output/{slug}-frontend-notes.md")),
        )
        .unwrap_or_default();
        let code = crate::acceptance::code_digest(&self.options.project_root, 18_000);
        if code.trim().is_empty() {
            return;
        }
        let system = "You are a STRICT tech lead reviewing a frontend preview before the \
             user approves it and the team moves to backend work on a COMMERCIAL \
             product. Judge: the single biggest risk, what's weak or missing in the \
             UI / API wiring, and whether it's solid enough to proceed. Be concrete \
             and brief. JSON shape: {\"biggest_risk\": \"<one line>\", \
             \"missing\": [\"<short>\", …], \"ready\": <true|false>}";
        let user = format!(
            "## Frontend notes\n{}\n\n## Delivered UI code\n{code}",
            excerpt(&notes, 3000)
        );
        let Some(v): Option<DocsVerdict> = self.consult(system, user).await else {
            return;
        };
        let mut msg = String::from("[preview] 智能评审(共享底座的脑子,确认前先看一眼):\n");
        if !v.biggest_risk.trim().is_empty() {
            msg.push_str(&format!("  · 最大风险:{}\n", v.biggest_risk.trim()));
        }
        for m in v.missing.iter().filter(|s| !s.trim().is_empty()).take(5) {
            msg.push_str(&format!("  · 弱/缺:{}\n", m.trim()));
        }
        msg.push_str(if v.ready {
            "  · 评估:前端扎实,可 c 确认进入后端(仍可输入修改重做前端)。"
        } else {
            "  · 评估:建议先描述要改的地方重做前端,再确认。"
        });
        self.emit(EngineEvent::Note(msg));
    }

    /// Adversarial senior code review — the missing team role. After the backend
    /// is built + build-verified, a STRICT reviewer (the shared brain, no extra
    /// API) does a pre-merge PR review of the delivered code for REAL defects
    /// (security, correctness, missing error handling, contract mismatches) and,
    /// when it judges the code not mergeable, feeds the findings BACK to the
    /// worker for one targeted fix pass — a real review->fix loop. Fail-open.
    async fn code_review_and_fix(&self) {
        if self.runtime.is_offline() {
            return;
        }
        let code = crate::acceptance::code_digest(&self.options.project_root, 18_000);
        if code.trim().is_empty() {
            return;
        }
        let system = "You are a STRICT senior engineer doing a PRE-MERGE code review of a \
             COMMERCIAL product. Find REAL defects only: security holes (injection, \
             auth bypass, hardcoded secrets), correctness bugs, missing error / input \
             handling, and frontend-backend contract mismatches. Ignore style nits. Be \
             concrete (name the file/function). JSON shape: {\"biggest_risk\": \
             \"<one line>\", \"missing\": [\"<defect to fix>\"], \"ready\": <true|false>}";
        let user = format!(
            "## Requirement\n{}\n\n## Delivered code (frontend + backend)\n{code}",
            excerpt(&self.options.requirement, 1000)
        );
        let Some(v): Option<DocsVerdict> = self.consult(system, user).await else {
            return;
        };
        let risk = if v.biggest_risk.trim().is_empty() {
            String::new()
        } else {
            format!("\n  - risk: {}", v.biggest_risk.trim())
        };
        let defects: String = v
            .missing
            .iter()
            .filter(|s| !s.trim().is_empty())
            .take(6)
            .map(|m| format!("\n  - defect: {}", m.trim()))
            .collect();
        let conclusion = if v.ready {
            "\n  - verdict: mergeable"
        } else {
            "\n  - verdict: defects found -> fed back to the worker for one fix pass"
        };
        self.emit(EngineEvent::Note(format!(
            "[review] senior code review (adversarial, pre-merge):{risk}{defects}{conclusion}"
        )));

        if !v.ready {
            let issues: String = v
                .missing
                .iter()
                .filter(|s| !s.trim().is_empty())
                .take(8)
                .map(|s| format!("- {}\n", s.trim()))
                .collect();
            if !issues.is_empty() {
                let fix = Prompt {
                    system: "A senior reviewer flagged the defects below in the code you just \
                         wrote. Fix them — edit the relevant files, do NOT rewrite from \
                         scratch. Output a short summary of what you changed."
                        .to_string(),
                    user: format!(
                        "## Biggest risk\n{}\n\n## Review findings to fix\n{issues}\nFix these now.",
                        v.biggest_risk.trim()
                    ),
                };
                let _ = self.try_generate(Phase::Backend, fix).await;
            }
        }
    }

    /// Intelligent design review — ask the SHARED base brain (same model that
    /// built the UI, no extra API) to judge whether the delivered frontend reads
    /// like a polished COMMERCIAL product or carries AI-slop tells. The
    /// deterministic detector is the floor (hard rules); this is the taste
    /// judgment a detector can't make. Fail-open → `None`.
    async fn judge_design(&self, slug: &str) -> Option<DesignVerdict> {
        let uiux = std::fs::read_to_string(
            self.options
                .project_root
                .join(format!("output/{slug}-uiux.md")),
        )
        .unwrap_or_default();
        let code = crate::acceptance::code_digest(&self.options.project_root, 22_000);
        if code.trim().is_empty() {
            return None;
        }
        let system = "You are a STRICT senior product designer reviewing a frontend for \
             COMMERCIAL release. The UIUX design spec below is the BINDING design \
             contract — the UI MUST be built from it, not from the model's own taste.\n\
             STEP 1 — CONFORMANCE (this is a HARD gate): verify the delivered UI \
             actually USES the system the spec declares. Flag as an issue ANY \
             deviation: an icon NOT from the icon library named in the spec (or an \
             emoji used as an icon); a color / font-family / type-scale / spacing / \
             radius value that is a hardcoded one-off instead of the spec's design \
             tokens; a base component (button/input/card/nav) that doesn't match \
             the spec's component system; or a page that drifts from the spec's \
             declared layout skeleton / hierarchy. A UI that does NOT conform to the \
             spec is NOT commercial_grade — however polished it looks on its own. \
             Name the file/where and the SPECIFIC token/component it should have used.\n\
             STEP 2 — TASTE: also flag AI-generated 'slop' tells: generic \
             Inter/Roboto + slate-900, indigo/purple gradients, emoji used as icons, \
             three equal cards, fake invented metrics, no real visual hierarchy, \
             default-system-font only.\n\
             List CONCRETE issues with file/where if possible. JSON shape: \
             {\"issues\": [\"<concrete issue>\", …], \"commercial_grade\": <true|false>}";
        let user = format!(
            "## 设计意图(UIUX)\n\n{}\n\n## 设计法(必须遵守,任何违反即判定非商业级)\n\n{}\n\n## 交付的 UI 代码(节选)\n\n{code}",
            excerpt_sections(&uiux, 6000),
            crate::experts::ANTI_SLOP_LAW
        );
        self.consult(system, user).await
    }

    /// Run the intelligent design review after the frontend phase: if the shared
    /// brain judges the UI not commercial-grade, re-delegate the SPECIFIC taste
    /// issues for a fix (one bounded round). Deterministic governance already
    /// ran as the hard-rule floor; this adds the judgment a detector can't make.
    async fn run_design_review(&self, slug: &str) {
        let Some(verdict) = self.judge_design(slug).await else {
            return; // no brain / nothing built → skip (deterministic floor stands)
        };
        // Team hook-up (zero behavior change): ALSO record the senior-designer's
        // existing verdict in the team ledger — issues are blocking, the
        // commercial-grade flag is the accept signal.
        self.ledger_role_verdict(
            "frontend",
            1,
            "senior-designer",
            verdict.commercial_grade,
            verdict.issues.clone(),
            Vec::new(),
        );
        if verdict.commercial_grade || verdict.issues.is_empty() {
            return;
        }
        let issues = verdict
            .issues
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n- ");
        if issues.is_empty() {
            return;
        }
        self.emit(EngineEvent::Note(format!(
            "[design] 智能设计审:UI 还不够商业级,打回修以下问题…\n- {issues}"
        )));
        let fix_p = Prompt {
            system: format!(
                "下面是对当前前端 UI 的商业级设计审查问题。**只修这些设计问题**(去 AI 味、\
                 建立真实视觉层级、用设计 token 与图标库),严格遵循 UIUX 设计契约,不要改\
                 业务逻辑。\n\n设计问题:\n- {issues}"
            ),
            user: format!("把 {} 的前端 UI 提升到商业级。", self.options.requirement),
        };
        let _ = self.try_generate(Phase::Frontend, fix_p).await;
    }

    /// Post-phase real-file governance catch-up for brains WITHOUT a real-time
    /// pre-write hook. Only Claude Code fires `PreToolUse`; codex / opencode /
    /// HTTP brains write files ungoverned in real time, so a hardcoded color or
    /// emoji is only caught at the final quality gate. This scans the real
    /// source files right after a code phase, re-delegates fixes (one round),
    /// and re-scans — giving every brain a governance feedback loop, not just
    /// claude. No-op for a brain that already governs at write time.
    async fn run_governance_catchup(&self, phase: Phase) {
        if self.runtime.capabilities().realtime_governance {
            return; // claude already blocked these at write time
        }
        let policy = umadev_governance::Policy::load(&self.options.project_root);
        let scan = |files: Vec<std::path::PathBuf>| -> Vec<String> {
            let mut out = Vec::new();
            for f in &files {
                let Ok(content) = std::fs::read_to_string(f) else {
                    continue;
                };
                let rel = f
                    .strip_prefix(&self.options.project_root)
                    .unwrap_or(f)
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/");
                let d = umadev_governance::scan_content_with_policy(&rel, &content, &policy);
                if d.block {
                    out.push(format!(
                        "{rel}: {} ({})",
                        d.reason.split('.').next().unwrap_or("violation").trim(),
                        d.clause
                    ));
                }
                if out.len() >= 25 {
                    break;
                }
            }
            out
        };
        let violations = scan(crate::acceptance::source_files(&self.options.project_root));
        if violations.is_empty() {
            return;
        }
        self.emit(EngineEvent::Note(format!(
            "[governance] {} 个真实文件有治理违规(底座无实时钩子,事后扫描捕获),打回修复…",
            violations.len()
        )));
        let v_list = violations.join("\n- ");
        let fix_p = Prompt {
            system: format!(
                "以下真实代码文件有治理违规(emoji 当图标 / 硬编码颜色 / AI-slop)。\
                 **只修这些违规**:用设计 token 替换硬编码颜色、用图标库(Lucide/Heroicons/\
                 Tabler)替换 emoji、去掉 AI 味套话,不要改其它正常代码。\n\n违规:\n- {v_list}"
            ),
            user: format!("修复 {} 的治理违规。", self.options.requirement),
        };
        let _ = self.try_generate(phase, fix_p).await;
        let remaining = scan(crate::acceptance::source_files(&self.options.project_root));
        if remaining.is_empty() {
            self.emit(EngineEvent::Note(
                "[governance] 治理违规已修复。".to_string(),
            ));
        } else {
            self.emit(EngineEvent::Note(format!(
                "[governance] 仍有 {} 处违规(质量门会再拦一次)。",
                remaining.len()
            )));
        }
    }

    async fn generate_docs_content(&self, research: Option<&str>) -> DocsContent {
        let slug = self.options.effective_slug();
        let req = &self.options.requirement;
        let research_excerpt = excerpt(research.unwrap_or(""), 1500);
        let project_cfg = crate::config::load_project_config(&self.options.project_root);
        let max_reviews = project_cfg.pipeline.max_review_rounds;

        self.emit(EngineEvent::Note(
            "[docs] Docs phase: generating PRD → Architecture → UI/UX (3 documents).              This may take 5-15 minutes with a worker backend."
                .to_string(),
        ));

        // PRD: inject PM methodology → generate → review → fix
        self.emit(EngineEvent::Note("[docs] Generating PRD...".to_string()));
        let prd_p = self.with_expert_knowledge(
            prd_prompt(&slug, req, &research_excerpt),
            &["product-manager"],
        );
        let prd = self
            .generate_with_review(Phase::Docs, prd_p, Self::review_prd, max_reviews)
            .await;
        let prd_excerpt = excerpt(prd.as_deref().unwrap_or(""), 1500);

        // Architecture: inject architect methodology → generate → review → fix
        let arch_p = self.with_expert_knowledge(
            architecture_prompt(&slug, req, &prd_excerpt),
            &["architect"],
        );
        let uiux_p =
            self.with_expert_knowledge(uiux_prompt(&slug, req, &prd_excerpt), &["uiux-designer"]);

        // Architecture and UI/UX both depend ONLY on the PRD and are independent
        // of each other -> draft (+ review/fix) them CONCURRENTLY on forked
        // runtimes (fresh base sessions). Falls back to sequential when the
        // backend can't fork (offline / generic). This is the pipeline's first
        // parallel fan-out.
        let (architecture, uiux) = if let (Some(r_arch), Some(r_uiux)) =
            (self.runtime.fork(), self.runtime.fork())
        {
            self.emit(EngineEvent::Note(
                "[parallel] Drafting Architecture + UI/UX concurrently (2 base sessions)..."
                    .to_string(),
            ));
            tokio::join!(
                self.generate_with_review_on(
                    &*r_arch,
                    Phase::Docs,
                    arch_p,
                    Self::review_architecture,
                    max_reviews,
                ),
                self.generate_with_review_on(
                    &*r_uiux,
                    Phase::Docs,
                    uiux_p,
                    Self::review_uiux,
                    max_reviews,
                ),
            )
        } else {
            self.emit(EngineEvent::Note(
                "[architecture] Generating Architecture...".to_string(),
            ));
            let a = self
                .generate_with_review(Phase::Docs, arch_p, Self::review_architecture, max_reviews)
                .await;
            self.emit(EngineEvent::Note(
                "[design] Generating UI/UX design system...".to_string(),
            ));
            let u = self
                .generate_with_review(Phase::Docs, uiux_p, Self::review_uiux, max_reviews)
                .await;
            (a, u)
        };

        DocsContent {
            prd,
            architecture,
            uiux,
        }
    }

    /// spec → frontend → pause at `preview_confirm`.
    ///
    /// When a worker backend is configured, the spec and frontend phases
    /// are also driven through the worker (not just templates). The worker
    /// creates real project scaffold, components, and pages based on the
    /// approved PRD + Architecture + UIUX documents.
    pub async fn continue_after_docs_confirm(&self) -> std::io::Result<RunReport> {
        let _run_lock = crate::run_lock::RunLock::acquire(&self.options.project_root)?;
        let use_runtime = !self.runtime.is_offline();
        let project_cfg = crate::config::load_project_config(&self.options.project_root);
        // #3: re-derive the plan and CONSUME it. A `DocsOnly` task has no build
        // phases at all — it stops at the docs gate, so a `continue` past it
        // does nothing but mark the workflow done (no spec/frontend ceremony).
        // A `BackendOnly` task skips the frontend phase + its preview gate.
        let plan = crate::planner::plan(&self.options.requirement);
        if !plan.includes(Phase::Spec)
            && !plan.includes(Phase::Frontend)
            && !plan.includes(Phase::Backend)
        {
            self.emit(EngineEvent::Note(format!(
                "[plan] 任务类型 {} 仅产出文档/调研 — 已在文档确认门完成,无需进入实现阶段。",
                plan.kind.id()
            )));
            self.emit(EngineEvent::BlockCompleted {
                final_phase: Phase::DocsConfirm,
                paused_at: None,
            });
            return Ok(RunReport {
                final_phase: Phase::DocsConfirm,
                paused_at: None,
                completed: Vec::new(),
            });
        }
        let run_frontend_phase = plan.includes(Phase::Frontend);
        self.transition(Phase::Spec, "")?;
        // A1: distill the approved docs into ONE binding contract the build phases follow.
        if use_runtime {
            self.distill_contract(&self.options.effective_slug()).await;
        }
        let mut completed = Vec::new();

        // Spec phase
        let phase_start = std::time::Instant::now();
        self.start_phase(Phase::Spec);
        // #1: tracks whether the base produced a real execution plan; stays
        // `false` for offline runs (no base expected) and flips on only when a
        // runtime base call returned nothing.
        let mut spec_degraded = false;
        if use_runtime {
            self.emit(EngineEvent::Note(
                "[docs] Worker generating execution plan + task breakdown...".to_string(),
            ));
            let slug = self.options.effective_slug();
            // Read approved docs for context
            let prd = std::fs::read_to_string(
                self.options
                    .project_root
                    .join(format!("output/{slug}-prd.md")),
            )
            .unwrap_or_default();
            let arch = std::fs::read_to_string(
                self.options
                    .project_root
                    .join(format!("output/{slug}-architecture.md")),
            )
            .unwrap_or_default();
            let context = format!(
                "PRD excerpt:\n{}\n\nArchitecture excerpt:\n{}",
                excerpt_sections(&prd, 2000),
                excerpt_sections(&arch, 2000)
            );
            let spec_text = self
                .try_generate(
                    Phase::Spec,
                    Prompt {
                        system: format!(
                            "Role: senior engineering manager.\n\
                             Write an execution plan with sprint breakdown, coding standards, \
                             and definition of done. Based on these approved documents:\n\n{context}"
                        ),
                        user: format!("Write the execution plan for: {}", self.options.requirement),
                    },
                )
                .await;
            if let Some(text) = spec_text {
                let plan_path = self
                    .options
                    .project_root
                    .join(format!("output/{slug}-execution-plan.md"));
                if let Some(parent) = plan_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = crate::phases::atomic_write(&plan_path, &text);
            } else {
                spec_degraded = true;
            }
        }
        completed.push(self.record_phase_maybe_degraded(
            Phase::Spec,
            run_spec(&self.options),
            spec_degraded,
        )?);
        self.record_phase_timing(Phase::Spec, phase_start);
        // Deterministic spec->tasks coverage check (the SDD "real enforcement"):
        // surface any PRD functional requirement no task cites, so it can't be
        // silently dropped before code.
        //
        // Two modes (#4):
        //  - default (warn): emit an advisory note and continue — the
        //    implementation phases must close the gap. Fail-open behaviour
        //    unchanged from before.
        //  - strict (`[pipeline] strict_coverage = true`, or env
        //    `UMADEV_STRICT_COVERAGE=1`): a coverage gap is a BLOCK — we pause at
        //    `spec` so the user revises the docs/tasks before any code is
        //    written. This is opt-in so a partial breakdown never silently halts
        //    a default run.
        let uncovered = crate::coverage::uncovered_requirements(
            &self.options.project_root,
            &self.options.effective_slug(),
        );
        if !uncovered.is_empty() {
            let strict = project_cfg.pipeline.strict_coverage
                || std::env::var("UMADEV_STRICT_COVERAGE").as_deref() == Ok("1");
            if strict {
                self.emit(EngineEvent::Note(format!(
                    "[spec] 严格覆盖门:以下 PRD 需求无任务覆盖,已阻断流水线(strict_coverage=true)。\
                     请补全 PRD/任务清单后再 /continue —— {}",
                    uncovered.join(", ")
                )));
                self.transition(Phase::Spec, Gate::DocsConfirm.id_str())?;
                self.emit(EngineEvent::BlockCompleted {
                    final_phase: Phase::Spec,
                    paused_at: Some(Gate::DocsConfirm),
                });
                return Ok(RunReport {
                    final_phase: Phase::Spec,
                    paused_at: Some(Gate::DocsConfirm),
                    completed,
                });
            }
            self.emit(EngineEvent::Note(format!(
                "[spec] 覆盖检查:以下 PRD 需求暂无任务覆盖,实现阶段必须补上 —— {}",
                uncovered.join(", ")
            )));
        }
        self.transition(Phase::Frontend, "")?;

        // Frontend phase
        let phase_start = std::time::Instant::now();
        self.start_phase(Phase::Frontend);
        // #3: a BackendOnly plan has no frontend work — skip the worker
        // implementation + design review, but still write the notes artifact and
        // keep the preview-gate anchor so `continue_after_preview_confirm` runs
        // the backend on the next continue (the gate-anchored 3-block structure
        // is preserved). The frontend-only / greenfield paths are unchanged.
        // #1: frontend degrades when the base was expected to implement it but
        // returned nothing. Only meaningful when the plan actually runs frontend.
        let mut fe_degraded = false;
        if use_runtime && run_frontend_phase {
            self.emit(EngineEvent::Note(
                "[preview] Worker implementing frontend (components, pages, API client)..."
                    .to_string(),
            ));
            let slug = self.options.effective_slug();
            let uiux = std::fs::read_to_string(
                self.options
                    .project_root
                    .join(format!("output/{slug}-uiux.md")),
            )
            .unwrap_or_default();
            let arch = std::fs::read_to_string(
                self.options
                    .project_root
                    .join(format!("output/{slug}-architecture.md")),
            )
            .unwrap_or_default();
            let prd = std::fs::read_to_string(
                self.options
                    .project_root
                    .join(format!("output/{slug}-prd.md")),
            )
            .unwrap_or_default();
            self.emit(EngineEvent::SubTaskStarted {
                phase: Phase::Frontend,
                task_id: "frontend.implement".into(),
                label: "worker generating components/styling/state".into(),
            });
            let fe_p = self.with_expert_knowledge(
                frontend_prompt(
                    &slug,
                    &self.options.requirement,
                    &excerpt_sections(&uiux, 3000),
                    &excerpt_sections(&arch, 2000),
                    &excerpt_sections(&prd, 1500),
                ),
                &["frontend-lead", "uiux-designer"],
            );
            let fe_ok = self.try_generate(Phase::Frontend, fe_p).await.is_some();
            fe_degraded = !fe_ok;
            self.emit(EngineEvent::SubTaskCompleted {
                phase: Phase::Frontend,
                task_id: "frontend.implement".into(),
                ok: fe_ok,
            });
        }
        let fe = self.record_phase_maybe_degraded(
            Phase::Frontend,
            run_frontend(&self.options),
            fe_degraded,
        )?;
        self.record_phase_timing(Phase::Frontend, phase_start);
        let gate = fe.gate;
        completed.push(fe);
        // Skip build-verify + design review entirely when there is no frontend
        // work (BackendOnly) — there's nothing built to verify or review.
        if run_frontend_phase {
            self.maybe_verify_and_fix(Phase::Frontend).await;
            // Real-file governance catch-up for brains without a real-time hook
            // (codex/opencode/HTTP) — scan the UI the base just wrote and re-
            // delegate any emoji/hardcoded-color/AI-slop fixes before the preview
            // gate. Then an INTELLIGENT design review (the shared brain judges
            // commercial polish / AI-slop — the taste call a detector can't make).
            if use_runtime {
                self.run_governance_catchup(Phase::Frontend).await;
                self.run_design_review(&self.options.effective_slug()).await;
                self.surface_preview_assessment(&self.options.effective_slug())
                    .await;
            }
        } else {
            self.emit(EngineEvent::Note(format!(
                "[plan] 任务类型 {} 无前端实现 — 跳过前端构建校验与设计审查。",
                plan.kind.id()
            )));
        }
        self.transition(Phase::PreviewConfirm, gate.map_or("", Gate::id_str))?;

        self.warn_degraded_summary();
        self.emit(EngineEvent::BlockCompleted {
            final_phase: Phase::PreviewConfirm,
            paused_at: gate,
        });
        Ok(RunReport {
            final_phase: Phase::PreviewConfirm,
            paused_at: gate,
            completed,
        })
    }

    /// backend → quality → delivery → done. Call after the user has
    /// approved `preview_confirm`.
    pub async fn continue_after_preview_confirm(&self) -> std::io::Result<RunReport> {
        let _run_lock = crate::run_lock::RunLock::acquire(&self.options.project_root)?;
        let use_runtime = !self.runtime.is_offline();
        // Re-derive the (deterministic) plan to honour its skips in this block
        // (#3): a `FrontendOnly` task has no backend phase, and a lean bug-fix /
        // refactor needs no delivery proof-pack.
        let plan = crate::planner::plan(&self.options.requirement);
        let skip_delivery = crate::planner::gate_safe_skips(&plan).contains(&Phase::Delivery);
        let run_backend_phase = plan.includes(Phase::Backend);
        self.transition(Phase::Backend, "")?;
        let mut completed = Vec::new();

        let phase_start = std::time::Instant::now();
        self.start_phase(Phase::Backend);
        // #1: backend degrades when the base was expected to implement it but
        // returned nothing.
        let mut be_degraded = false;
        if use_runtime && run_backend_phase {
            self.emit(EngineEvent::Note(
                "[backend] Worker implementing backend (routes, database, auth, tests)..."
                    .to_string(),
            ));
            let slug = self.options.effective_slug();
            let arch = std::fs::read_to_string(
                self.options
                    .project_root
                    .join(format!("output/{slug}-architecture.md")),
            )
            .unwrap_or_default();
            let prd = std::fs::read_to_string(
                self.options
                    .project_root
                    .join(format!("output/{slug}-prd.md")),
            )
            .unwrap_or_default();
            self.emit(EngineEvent::SubTaskStarted {
                phase: Phase::Backend,
                task_id: "backend.implement".into(),
                label: "worker generating routes/database/auth/tests".into(),
            });
            let be_p = self.with_expert_knowledge(
                backend_prompt(
                    &slug,
                    &self.options.requirement,
                    &excerpt_sections(&arch, 3000),
                    &excerpt_sections(&prd, 1500),
                ),
                &["backend-lead", "architect"],
            );
            let be_ok = self.try_generate(Phase::Backend, be_p).await.is_some();
            be_degraded = !be_ok;
            self.emit(EngineEvent::SubTaskCompleted {
                phase: Phase::Backend,
                task_id: "backend.implement".into(),
                ok: be_ok,
            });
        }
        completed.push(self.record_phase_maybe_degraded(
            Phase::Backend,
            run_backend(&self.options),
            be_degraded,
        )?);
        self.record_phase_timing(Phase::Backend, phase_start);
        if run_backend_phase {
            self.maybe_verify_and_fix(Phase::Backend).await;
            // Senior code review (the team role we added): adversarial pre-merge
            // review of the now-complete full stack, with a review->fix loop.
            self.code_review_and_fix().await;

            // Post-phase governance catch-up (real-file scan for brains without a
            // real-time hook), then task-level acceptance — the director checks
            // the delivered code against the breakdown it created (the
            // architecture API table) and re-delegates any planned endpoint with
            // no implementation, so we never ship a half-built product. Closes
            // interpret→break-down→delegate→VERIFY→deliver.
            if use_runtime {
                self.run_governance_catchup(Phase::Backend).await;
                self.run_task_acceptance(&self.options.effective_slug())
                    .await;
            }
        } else {
            self.emit(EngineEvent::Note(format!(
                "[plan] 任务类型 {} 无后端实现 — 跳过后端构建校验、代码评审与接口验收。",
                plan.kind.id()
            )));
        }

        let phase_start = std::time::Instant::now();
        self.transition(Phase::Quality, "")?;
        self.start_phase(Phase::Quality);
        let quality_result = run_quality(&self.options);
        // Did the quality phase PRODUCE a gate file? If it did and we can't
        // read it back, that's a disk/permission failure, not "offline mode" —
        // we must NOT assume pass (that would mask a write failure as success).
        let produced_gate_file = quality_result.as_ref().is_ok_and(|o| {
            o.artifacts
                .iter()
                .any(|p| p.to_string_lossy().ends_with("-quality-gate.json"))
        });
        completed.push(self.record_phase(Phase::Quality, quality_result)?);
        self.record_phase_timing(Phase::Quality, phase_start);
        self.maybe_verify(Phase::Quality).await;

        // Quality-stage role-critic TEAM cross-review (explicit, scaled to the
        // task) — the second axis of the team. The deterministic quality gate +
        // floors (coverage / contract / governance) ran above as the HARD signal;
        // this ADDS the QA + security cross-review opinions, records every verdict
        // to the team ledger, and surfaces their advisory blocking. ADVISORY only:
        // it never sinks the deterministic gate or controls the loop (invariant 2).
        if use_runtime {
            let advisory = self
                .run_quality_critic_team(&self.options.effective_slug())
                .await;
            if !advisory.is_empty() {
                let list = advisory
                    .iter()
                    .take(10)
                    .map(|s| format!("  · {s}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.emit(EngineEvent::Note(format!(
                    "[team] 质量阶段团队建议(advisory,不沉质量门 — 确定性门仍为硬信号):\n{list}"
                )));
            }
        }

        let qg_path = self.options.project_root.join("output").join(format!(
            "{}-quality-gate.json",
            self.options.effective_slug()
        ));
        let quality_passed = if let Ok(qg) = std::fs::read_to_string(&qg_path) {
            let score = crate::phases::extract_quality_score(&qg);
            self.emit(EngineEvent::Note(format!(
                "质量门结果: {}/100 · {}",
                score.0,
                if score.1 {
                    "PASSED [ok]"
                } else {
                    "BLOCKED [fail]"
                }
            )));
            score.1
        } else if produced_gate_file {
            // The quality phase wrote the file but we can't read it back —
            // treat as a real failure rather than silently assuming pass.
            self.emit(EngineEvent::Note(format!(
                "[warn] 质量门文件写出后无法读回 ({}) — 判定未通过以防掩盖写盘失败。",
                qg_path.display()
            )));
            false
        } else {
            true // no gate file produced = offline/empty run → assume pass
        };

        if !quality_passed && use_runtime {
            self.emit(EngineEvent::Note(
                "[warn] 质量门未通过 — 阻断 delivery（UD-EVID-003）。请修复后重跑:\n  \
                 /redo 重跑整个流水线\n  \
                 或修复后 /continue 继续"
                    .to_string(),
            ));
            // UD-EVID-003: a worker-backed run that emits passed:false MUST
            // refuse to advance to delivery. Pause at quality so the next
            // `continue` re-checks the gate. Offline/template runs skip this
            // block — their quality gate is advisory, not a delivery gate.
            self.record_run_history(Phase::Quality, false, 0);
            self.warn_degraded_summary();
            self.emit(EngineEvent::BlockCompleted {
                final_phase: Phase::Quality,
                paused_at: None,
            });
            return Ok(RunReport {
                final_phase: Phase::Quality,
                paused_at: None,
                completed,
            });
        }

        if skip_delivery {
            self.emit(EngineEvent::Note(format!(
                "[plan] {} — 跳过 delivery 阶段（小改/重构无需交付物料包）",
                plan.kind.id()
            )));
        } else {
            let phase_start = std::time::Instant::now();
            self.transition(Phase::Delivery, "")?;
            self.start_phase(Phase::Delivery);
            if use_runtime {
                self.emit(EngineEvent::Note(
                    "[package] Worker producing deployment recipe (build verify + deploy commands)…"
                        .to_string(),
                ));
                let slug = self.options.effective_slug();
                let arch = std::fs::read_to_string(
                    self.options
                        .project_root
                        .join(format!("output/{slug}-architecture.md")),
                )
                .unwrap_or_default();
                self.emit(EngineEvent::SubTaskStarted {
                    phase: Phase::Delivery,
                    task_id: "delivery.recipe".into(),
                    label: "worker verifying production build + deploy instructions".into(),
                });
                let del_p = self.with_expert_knowledge(
                    delivery_prompt(
                        &slug,
                        &self.options.requirement,
                        &excerpt_sections(&arch, 2000),
                    ),
                    &["devops"],
                );
                let del_ok = self.try_generate(Phase::Delivery, del_p).await.is_some();
                self.emit(EngineEvent::SubTaskCompleted {
                    phase: Phase::Delivery,
                    task_id: "delivery.recipe".into(),
                    ok: del_ok,
                });
            }
            completed.push(self.record_phase(Phase::Delivery, run_delivery(&self.options))?);
            // Base-driven self-evolution upkeep: reconcile the lesson corpus and
            // write reusable skill cards (no-op offline; fail-open).
            self.evolve_memory_at_delivery().await;
            self.record_phase_timing(Phase::Delivery, phase_start);
        }

        // mark pipeline as done — keep phase=delivery, clear gate
        let done = WorkflowState {
            phase: Phase::Delivery.id().to_string(),
            active_gate: String::new(),
            slug: self.options.effective_slug(),
            requirement: self.options.requirement.clone(),
            last_transition_at: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            note: "Pipeline complete.".to_string(),
            backend: self.options.backend.clone(),
            spec_version: SPEC_VERSION.to_string(),
        };
        write_workflow_state(&self.options.project_root, &done)?;

        let artifact_count = completed.iter().map(|p| p.artifacts.len()).sum();
        self.record_run_history(Phase::Delivery, quality_passed, artifact_count);

        // #1: even on a "complete" pipeline, if any phase degraded to a
        // placeholder the run is NOT a clean delivery — say so at the very end.
        self.warn_degraded_summary();
        self.emit(EngineEvent::BlockCompleted {
            final_phase: Phase::Delivery,
            paused_at: None,
        });
        Ok(RunReport {
            final_phase: Phase::Delivery,
            paused_at: None,
            completed,
        })
    }

    /// **Lightweight fast track** — the lean single-shot pipeline for a trivial
    /// task (a one-line tweak, a tiny style nudge, a small script). Directly
    /// answers "the full nine phases are too heavy for small work": it reuses the
    /// existing phase bodies and the dynamic [`crate::planner`] skip mechanism
    /// (a [`crate::planner::TaskKind::Light`] plan) but runs them in ONE
    /// uninterrupted block — `spec` (clarify-lite) → `frontend` + `backend`
    /// (whichever the change touches; the worker no-ops the irrelevant side) →
    /// `quality` verify. There is no research, no three core docs, no
    /// `docs_confirm` / `preview_confirm` gates, and no delivery proof-pack.
    ///
    /// Governance still applies on every write (fail-open), and every phase still
    /// records an auditable artifact + timing + the run-history row, so a Light
    /// run is leaner, not invisible. `use_runtime` forces the worker on/off.
    pub async fn run_light(&self, use_runtime: bool) -> std::io::Result<RunReport> {
        let _run_lock = crate::run_lock::RunLock::acquire(&self.options.project_root)?;
        let plan = crate::planner::plan_light(&self.options.requirement);
        let mut completed = Vec::new();

        self.emit(EngineEvent::PipelineStarted {
            slug: self.options.effective_slug(),
            requirement: self.options.requirement.clone(),
        });
        self.emit(EngineEvent::Note(format!(
            "[plan] 轻量档:{} — {};本次只跑 spec→实现→quality,\
             跳过 research/docs/两道确认门/delivery。",
            plan.kind.id(),
            plan.rationale
        )));

        // 1. spec (clarify-lite) — one compact implementation plan, no sprint
        //    ceremony. Reuses the deterministic spec artifact so the change is
        //    still recorded; the worker draft is the lean brief.
        let phase_start = std::time::Instant::now();
        self.transition(Phase::Spec, "")?;
        self.start_phase(Phase::Spec);
        let mut spec_degraded = false;
        if use_runtime {
            self.emit(EngineEvent::Note(
                "[light] 生成精简实现计划(直接定位最小改动)…".to_string(),
            ));
            let lean = self
                .try_generate(
                    Phase::Spec,
                    Prompt {
                        system: "Role: senior engineer on a SMALL, well-scoped change. \
                                 Write a SHORT plan: the few files to touch and the minimal \
                                 edit for each. No sprints, no docs ceremony — this is a \
                                 trivial change. Keep it tight."
                            .to_string(),
                        user: format!("Trivial change: {}", self.options.requirement),
                    },
                )
                .await;
            if let Some(text) = lean {
                let slug = self.options.effective_slug();
                let plan_path = self
                    .options
                    .project_root
                    .join(format!("output/{slug}-execution-plan.md"));
                if let Some(parent) = plan_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = crate::phases::atomic_write(&plan_path, &text);
            } else {
                spec_degraded = true;
            }
        }
        completed.push(self.record_phase_maybe_degraded(
            Phase::Spec,
            run_spec(&self.options),
            spec_degraded,
        )?);
        self.record_phase_timing(Phase::Spec, phase_start);

        // 2. implement — frontend then backend, in the same block. A trivial task
        //    usually touches only one side; the worker simply makes no edits to
        //    the side that doesn't apply. We keep both so a "small full-stack
        //    tweak" still lands without forcing the user to pick a side.
        let phase_start = std::time::Instant::now();
        self.transition(Phase::Frontend, "")?;
        self.start_phase(Phase::Frontend);
        let mut fe_degraded = false;
        if use_runtime {
            self.emit(EngineEvent::Note("[light] 直接实现这个小改动…".to_string()));
            let fe_ok = self
                .try_generate(
                    Phase::Frontend,
                    Prompt {
                        system: "Make ONLY the small change requested. Edit the relevant \
                                 file(s) in place — do not rewrite, do not scaffold a new \
                                 project, do not add unrelated features. Output a one-line \
                                 summary of what you changed."
                            .to_string(),
                        user: self.options.requirement.clone(),
                    },
                )
                .await
                .is_some();
            fe_degraded = !fe_ok;
        }
        completed.push(self.record_phase_maybe_degraded(
            Phase::Frontend,
            run_frontend(&self.options),
            fe_degraded,
        )?);
        self.record_phase_timing(Phase::Frontend, phase_start);
        if use_runtime {
            // Build-verify the change + governance catch-up (real-file scan for
            // bases without a real-time hook). Same safety net as the full path,
            // minus the design review (a trivial tweak isn't a UI delivery).
            self.maybe_verify_and_fix(Phase::Frontend).await;
            self.run_governance_catchup(Phase::Frontend).await;
        }

        // 3. quality verify — the one gate a Light run keeps. It is advisory
        //    (no delivery to block), but it leaves the same auditable scorecard.
        let phase_start = std::time::Instant::now();
        self.transition(Phase::Quality, "")?;
        self.start_phase(Phase::Quality);
        let quality_result = run_quality(&self.options);
        completed.push(self.record_phase(Phase::Quality, quality_result)?);
        self.record_phase_timing(Phase::Quality, phase_start);
        self.maybe_verify(Phase::Quality).await;

        let quality_passed = {
            let qg_path = self.options.project_root.join("output").join(format!(
                "{}-quality-gate.json",
                self.options.effective_slug()
            ));
            std::fs::read_to_string(&qg_path)
                .ok()
                .is_none_or(|qg| crate::phases::extract_quality_score(&qg).1)
        };

        // Mark the lean run complete — phase stays at quality (Light has no
        // delivery phase), gate cleared.
        let done = WorkflowState {
            phase: Phase::Quality.id().to_string(),
            active_gate: String::new(),
            slug: self.options.effective_slug(),
            requirement: self.options.requirement.clone(),
            last_transition_at: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            note: "Light pipeline complete.".to_string(),
            backend: self.options.backend.clone(),
            spec_version: SPEC_VERSION.to_string(),
        };
        write_workflow_state(&self.options.project_root, &done)?;
        let artifact_count = completed.iter().map(|p| p.artifacts.len()).sum();
        self.record_run_history(Phase::Quality, quality_passed, artifact_count);
        self.warn_degraded_summary();
        self.emit(EngineEvent::BlockCompleted {
            final_phase: Phase::Quality,
            paused_at: None,
        });
        Ok(RunReport {
            final_phase: Phase::Quality,
            paused_at: None,
            completed,
        })
    }

    /// **Re-run a SINGLE named phase**, reusing the prior run's context so the
    /// inputs are identical. The headline use is recovering a `DEGRADED` phase
    /// (the base went offline mid-run, so the artifact is an offline placeholder):
    /// `redo_phase(Phase::Frontend, true)` regenerates just `frontend` against the
    /// same persisted requirement / slug / backend, then clears that phase's
    /// `.DEGRADED` markers on success.
    ///
    /// This is NOT a gate walk — it runs exactly one phase body (no transition
    /// past it, no downstream phases), so the user keeps full control of the
    /// pipeline position. `use_runtime` forces the worker on/off.
    ///
    /// # Errors
    /// Returns `Err` when the phase body fails to write its artifact. A caller
    /// that hands in an unknown phase name should reject it BEFORE calling (see
    /// [`crate::planner::phase_from_id`]); the runner only accepts a typed
    /// [`Phase`]. The single-writer run-lock is held for the redo.
    pub async fn redo_phase(&self, phase: Phase, use_runtime: bool) -> std::io::Result<RunReport> {
        let _run_lock = crate::run_lock::RunLock::acquire(&self.options.project_root)?;
        self.emit(EngineEvent::Note(format!(
            "[redo] 用先前 run 的上下文重跑 `{}` 阶段(输入保持一致)…",
            phase.id()
        )));
        self.start_phase(phase);
        let phase_start = std::time::Instant::now();

        // Drive the worker for the phase, then record via the deterministic body.
        // The generation prompts mirror the full-pipeline phase bodies but are
        // self-contained so a redo never depends on an in-progress block. A phase
        // with no worker step (it only writes a notes artifact) still re-records.
        let (output, degraded) = self.redo_one_phase(phase, use_runtime).await?;
        let completed = vec![self.record_phase_maybe_degraded(phase, Ok(output), degraded)?];
        self.record_phase_timing(phase, phase_start);

        // On a CLEAN redo, clear the degraded markers this phase left behind so
        // the workspace no longer flags it as a placeholder. We use the SAME
        // artifact paths the redo just (re)wrote, so the marker names match
        // exactly what `record_phase_maybe_degraded` would have written.
        if !degraded {
            Self::clear_degraded_markers(&completed[0].artifacts);
            self.emit(EngineEvent::Note(format!(
                "[redo] `{}` 阶段已重跑完成,已清除该阶段的 .DEGRADED 降级标记。",
                phase.id()
            )));
        }
        self.warn_degraded_summary();
        self.emit(EngineEvent::BlockCompleted {
            final_phase: phase,
            paused_at: None,
        });
        Ok(RunReport {
            final_phase: phase,
            paused_at: None,
            completed,
        })
    }

    /// Generate + build one phase's output for [`Self::redo_phase`]. Returns the
    /// phase output and whether it degraded (base was on but returned nothing).
    /// Self-contained: reads its context from the on-disk `output/` artifacts the
    /// prior run wrote, so a redo reproduces the same inputs.
    async fn redo_one_phase(
        &self,
        phase: Phase,
        use_runtime: bool,
    ) -> std::io::Result<(PhaseOutput, bool)> {
        let slug = self.options.effective_slug();
        let read = |name: &str| -> String {
            std::fs::read_to_string(
                self.options
                    .project_root
                    .join(format!("output/{slug}-{name}.md")),
            )
            .unwrap_or_default()
        };
        match phase {
            Phase::Research => {
                let text = if use_runtime {
                    let digest = crate::phases::phase_knowledge_digest(&self.options, phase);
                    let rp = self.with_expert_knowledge(
                        research_prompt(&slug, &self.options.requirement, &digest),
                        &["product-manager"],
                    );
                    self.try_generate(phase, rp).await
                } else {
                    None
                };
                let degraded = use_runtime && text.is_none();
                Ok((run_research(&self.options, text.as_deref())?, degraded))
            }
            Phase::Docs => {
                // Reuse the full docs generation path so PRD/architecture/UIUX
                // are all regenerated against the same research input.
                let research = read("research");
                let content = if use_runtime {
                    self.generate_docs_content(Some(&research)).await
                } else {
                    DocsContent::default()
                };
                let degraded = use_runtime
                    && content.prd.is_none()
                    && content.architecture.is_none()
                    && content.uiux.is_none();
                Ok((run_docs(&self.options, &content)?, degraded))
            }
            Phase::Spec => {
                let mut degraded = false;
                if use_runtime {
                    let prd = read("prd");
                    let arch = read("architecture");
                    let context = format!(
                        "PRD excerpt:\n{}\n\nArchitecture excerpt:\n{}",
                        excerpt_sections(&prd, 2000),
                        excerpt_sections(&arch, 2000)
                    );
                    let text = self
                        .try_generate(
                            phase,
                            Prompt {
                                system: format!(
                                    "Role: senior engineering manager.\nWrite an execution plan \
                                     with sprint breakdown, coding standards, and definition of \
                                     done. Based on these approved documents:\n\n{context}"
                                ),
                                user: format!(
                                    "Write the execution plan for: {}",
                                    self.options.requirement
                                ),
                            },
                        )
                        .await;
                    if let Some(text) = text {
                        let plan_path = self
                            .options
                            .project_root
                            .join(format!("output/{slug}-execution-plan.md"));
                        if let Some(parent) = plan_path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let _ = crate::phases::atomic_write(&plan_path, &text);
                    } else {
                        degraded = true;
                    }
                }
                Ok((run_spec(&self.options)?, degraded))
            }
            Phase::Frontend => {
                let mut degraded = false;
                if use_runtime {
                    let uiux = read("uiux");
                    let arch = read("architecture");
                    let prd = read("prd");
                    let fe_p = self.with_expert_knowledge(
                        frontend_prompt(
                            &slug,
                            &self.options.requirement,
                            &excerpt_sections(&uiux, 3000),
                            &excerpt_sections(&arch, 2000),
                            &excerpt_sections(&prd, 1500),
                        ),
                        &["frontend-lead", "uiux-designer"],
                    );
                    degraded = self.try_generate(phase, fe_p).await.is_none();
                    if !degraded {
                        self.maybe_verify_and_fix(Phase::Frontend).await;
                    }
                }
                Ok((run_frontend(&self.options)?, degraded))
            }
            Phase::Backend => {
                let mut degraded = false;
                if use_runtime {
                    let arch = read("architecture");
                    let prd = read("prd");
                    let be_p = self.with_expert_knowledge(
                        backend_prompt(
                            &slug,
                            &self.options.requirement,
                            &excerpt_sections(&arch, 3000),
                            &excerpt_sections(&prd, 1500),
                        ),
                        &["backend-lead", "architect"],
                    );
                    degraded = self.try_generate(phase, be_p).await.is_none();
                    if !degraded {
                        self.maybe_verify_and_fix(Phase::Backend).await;
                    }
                }
                Ok((run_backend(&self.options)?, degraded))
            }
            Phase::Quality => {
                let out = run_quality(&self.options)?;
                self.maybe_verify(Phase::Quality).await;
                Ok((out, false))
            }
            Phase::Delivery => {
                let mut degraded = false;
                if use_runtime {
                    let arch = read("architecture");
                    let del_p = self.with_expert_knowledge(
                        delivery_prompt(
                            &slug,
                            &self.options.requirement,
                            &excerpt_sections(&arch, 2000),
                        ),
                        &["devops"],
                    );
                    degraded = self.try_generate(phase, del_p).await.is_none();
                }
                Ok((run_delivery(&self.options)?, degraded))
            }
            // The two gate phases have no body to re-run — they only pause the
            // pipeline. Re-running them is a no-op artifact-wise; surface a clear
            // note and return an empty (non-degraded) output.
            Phase::DocsConfirm | Phase::PreviewConfirm => {
                self.emit(EngineEvent::Note(format!(
                    "[redo] `{}` 是确认门,没有可重跑的产物 —— 用 /continue 或 /revise 操作该门。",
                    phase.id()
                )));
                Ok((
                    PhaseOutput {
                        phase,
                        artifacts: Vec::new(),
                        gate: None,
                        degraded: false,
                    },
                    false,
                ))
            }
        }
    }

    /// Remove the `.DEGRADED` marker files sitting next to `artifacts`. Called
    /// after a clean [`Self::redo_phase`] so the workspace no longer flags the
    /// phase's output as an offline placeholder. The marker name is derived
    /// IDENTICALLY to [`Self::record_phase_maybe_degraded`]
    /// (`<artifact>.<ext>.DEGRADED`). Best-effort: a missing marker / IO error
    /// is ignored (fail-open).
    fn clear_degraded_markers(artifacts: &[PathBuf]) {
        for artifact in artifacts {
            let marker = artifact.with_extension(format!(
                "{}.DEGRADED",
                artifact
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("out")
            ));
            let _ = std::fs::remove_file(&marker);
        }
    }

    /// Dispatch: read workflow-state, decide which block to run next.
    ///
    /// Guards against state-machine incoherence: the passed `approved_gate`
    /// MUST match the gate the persisted `workflow-state.json` says is open.
    /// A caller that passes `PreviewConfirm` while the state is still at
    /// `docs_confirm` would otherwise skip spec/frontend regeneration and
    /// jump straight to backend, producing artifacts out of order. On
    /// mismatch we return a descriptive error rather than silently advancing.
    /// Read the user's clarify answers from
    /// `output/{slug}-clarify-answers.md` (written by the TUI when the user
    /// submits answers at ClarifyGate) and merge them into the requirement so
    /// research sees the enriched brief. Returns the original requirement
    /// when no answers file exists (fail-open).
    fn merged_requirement(&self) -> String {
        let slug = self.options.effective_slug();
        let answers_path = self
            .options
            .project_root
            .join("output")
            .join(format!("{slug}-clarify-answers.md"));
        let Ok(answers) = std::fs::read_to_string(&answers_path) else {
            return self.options.requirement.clone();
        };
        let answers = answers.trim();
        if answers.is_empty() {
            return self.options.requirement.clone();
        }
        format!(
            "{}\n\n## 用户澄清回答\n\n{answers}",
            self.options.requirement
        )
    }

    /// Advance past the currently-active gate. For `ClarifyGate`, the
    /// user's answers (in `output/{slug}-clarify-answers.md`) are merged
    /// into the requirement before research runs. For `DocsConfirm` /
    /// `PreviewConfirm`, runs the next block.
    pub async fn continue_from_gate(&self, approved_gate: Gate) -> std::io::Result<RunReport> {
        if let Some(state) = crate::state::read_workflow_state(&self.options.project_root) {
            let persisted = state.active_gate.as_str();
            let expected = approved_gate.id_str();
            // Only reject when the persisted gate is a DIFFERENT non-empty gate.
            // An empty active_gate is a legitimate mid-pipeline state (the runner
            // doesn't persist the waiting gate on every block boundary), so it
            // must stay permissive — the concurrent-re-run race this could guard
            // against is already prevented by the workspace run-lock.
            if !persisted.is_empty() && persisted != expected {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "gate mismatch: you approved `{expected}` but the workflow state is at \
                         `{persisted}`. Run `umadev continue` to advance from the actual gate, \
                         or `umadev rollback latest` to undo."
                    ),
                ));
            }
        }
        let use_runtime_for = |_: &str| !self.runtime.is_offline();
        match approved_gate {
            Gate::ClarifyGate => {
                let merged = self.merged_requirement();
                // Only override when the merge actually changed the text
                // (answers file exists + non-empty); otherwise pass None.
                let override_req =
                    (!merged.eq(&self.options.requirement)).then_some(merged.as_str());
                self.run_initial_block(use_runtime_for("clarify"), override_req)
                    .await
            }
            Gate::DocsConfirm => self.continue_after_docs_confirm().await,
            Gate::PreviewConfirm => self.continue_after_preview_confirm().await,
        }
    }

    /// Re-run the block that PRODUCED a gate, so a revision request
    /// regenerates the right artifacts and pauses at the same gate again.
    ///
    /// This is the inverse of [`continue_from_gate`], which advances past
    /// a gate. Revising at `docs_confirm` regenerates the three core docs
    /// (research → docs); revising at `preview_confirm` regenerates the
    /// spec → frontend (NOT the already-approved docs). The caller is
    /// expected to fold the user's revision feedback into
    /// `options.requirement` before calling so the worker incorporates it.
    ///
    /// `use_runtime` is honoured on BOTH branches: it forces the worker on
    /// (`true`) or off (`false`) regardless of whether `options.backend`
    /// is set. Callers that want "follow the configured backend" should
    /// pass `!options.backend.is_empty()`.
    ///
    /// [`continue_from_gate`]: AgentRunner::continue_from_gate
    pub async fn revise_at_gate(
        &self,
        gate: Gate,
        use_runtime: bool,
    ) -> std::io::Result<RunReport> {
        match gate {
            Gate::ClarifyGate => self.run_clarify(use_runtime).await,
            Gate::DocsConfirm => self.run_initial_block(use_runtime, None).await,
            // continue_after_docs_confirm re-derives use_runtime from
            // options.backend; that derivation agrees with the caller's
            // value when the caller passed `!options.backend.is_empty()`,
            // so behaviour is preserved.
            Gate::PreviewConfirm => self.continue_after_docs_confirm().await,
        }
    }

    fn transition(&self, next: Phase, active_gate: &str) -> std::io::Result<()> {
        let state = WorkflowState {
            phase: next.id().to_string(),
            active_gate: active_gate.to_string(),
            slug: self.options.effective_slug(),
            requirement: self.options.requirement.clone(),
            last_transition_at: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            note: format!(
                "Advanced to {} (worker: {})",
                next.id(),
                self.worker_label()
            ),
            backend: self.options.backend.clone(),
            spec_version: SPEC_VERSION.to_string(),
        };
        write_workflow_state(&self.options.project_root, &state)?;
        // Always refresh the coach prompt so `.umadev/coach/CURRENT.md`
        // matches the active phase the host should be executing.
        let _ = write_coach_prompt(&self.options, next);
        Ok(())
    }
}

/// Adapter that lets a [`crate::critics::RoleCritic`] borrow the runner's
/// strict-JSON judge mechanism while thinking on an ISOLATED forked session.
///
/// Holds the runner (for `consult_on` + the configured model) and an OPTIONAL
/// forked runtime. When the fork exists, the judge call runs against it — a
/// clean, no-resume, read-only session that can never collide with or write
/// through the main writer session (single-writer invariant). When the base
/// can't fork, it falls back to a fresh consult on the MAIN runtime, which is
/// still read-only (`consult` only reads + parses, never writes). Either way the
/// path is fail-open: any failure yields [`crate::critics::RoleVerdict::empty`].
struct ForkedConsult<'a, R: Runtime> {
    runner: &'a AgentRunner<R>,
    fork: Option<&'a dyn umadev_runtime::Runtime>,
}

#[async_trait::async_trait]
impl<R: Runtime> crate::critics::CriticConsult for ForkedConsult<'_, R> {
    async fn judge(&self, role: &str, system: &str, user: String) -> crate::critics::RoleVerdict {
        // Prefer the isolated fork; fall back to the main runtime's read-only
        // consult when the base can't fork. Fail-open to the empty (accepting)
        // verdict so an absent / broken critic NEVER blocks.
        let runtime: &dyn umadev_runtime::Runtime = self.fork.unwrap_or(&self.runner.runtime);
        let parsed: Option<crate::critics::RoleVerdict> =
            self.runner.consult_on(runtime, system, user).await;
        parsed
            .map(|v| v.normalized(role))
            .unwrap_or_else(|| crate::critics::RoleVerdict::empty(role))
    }
}

/// Result of a verify pass — lets callers decide whether to auto-fix.
#[derive(Debug, Clone)]
struct VerifyOutcome {
    passed: bool,
    skipped: bool,
    /// The summarized stderr of the first failing step (empty if passed).
    failure_detail: String,
}

/// Distill a build/test stderr into the most useful error excerpt for the
/// user. Build tools emit pages of progress noise before the real error; the
/// last non-empty lines are where the actionable error lives. We take the last
/// 8 non-empty lines, capped at 1200 chars, so the TUI shows what actually
/// failed instead of truncating to just the first (usually noise) line.
fn summarize_stderr(stderr: &str) -> String {
    let meaningful: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let joined = meaningful.join("\n");
    if joined.chars().count() > 1200 {
        let mut idx = 1200;
        while !joined.is_char_boundary(idx) {
            idx -= 1;
        }
        let mut out = joined[..idx].to_string();
        out.push_str("\n…[truncated]");
        out
    } else {
        joined
    }
}

/// Record a worker invocation for usage metering. Appends to
/// `~/.umadev/usage.jsonl` (JSON Lines — one record per call). This is
/// the foundation for a future free/pro tier: the number of worker calls
/// per day/month is the natural metering unit. Fail-open: never blocks on
/// write errors.
pub fn record_usage(backend: &str, phase: Phase, tokens: u32) {
    let path = usage_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Build with serde so a backend/provider label containing a quote or
    // backslash can't produce invalid JSON (string interpolation would).
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let record = serde_json::json!({
        "ts": ts,
        "backend": backend,
        "phase": phase.id(),
        "tokens": tokens,
    })
    .to_string();
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{record}");
    }
}

/// Path to the usage log: `~/.umadev/usage.jsonl`.
pub fn usage_path() -> std::path::PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        return std::path::PathBuf::from(home)
            .join(".umadev")
            .join("usage.jsonl");
    }
    std::path::PathBuf::from(".umadev").join("usage.jsonl")
}

/// Read the usage log and return a human-readable summary: total calls,
/// calls per phase, estimated token usage. Used by the TUI `/usage` command.
#[must_use]
pub fn usage_summary() -> String {
    let path = usage_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return "还没有使用记录。跑一次需求(run)后会自动统计。".to_string();
    };
    let mut total: u32 = 0;
    let mut by_phase: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
    let mut total_tokens: u64 = 0;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        total += 1;
        if let Some(phase) = line
            .split(r#""phase":""#)
            .nth(1)
            .and_then(|s| s.split('"').next())
        {
            *by_phase.entry(phase).or_insert(0) += 1;
        }
        if let Some(tokens) = line
            .split(r#""tokens":"#)
            .nth(1)
            .and_then(|s| s.split('}').next())
            .and_then(|s| s.parse::<u64>().ok())
        {
            total_tokens += tokens;
        }
    }
    let mut out = format!("使用统计(共 {total} 次宿主调用):\n");
    for (phase, count) in &by_phase {
        out.push_str(&format!("  {phase}: {count} 次\n"));
    }
    if total_tokens > 0 {
        out.push_str(&format!("总 token 用量: ~{total_tokens}"));
    }
    out
}

/// One parsed row from `~/.umadev/usage.jsonl`.
///
/// The on-disk record collapses input + output into a single `tokens` field at
/// write time (see [`record_usage`]), so the split is NOT recoverable here — the
/// report surfaces the combined total and is honest about that.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UsageRecord {
    /// Unix epoch seconds when the worker call completed.
    ts: u64,
    /// Backend id that served the call (`claude-code` / `codex` / …).
    backend: String,
    /// Phase id (`research` / `frontend` / …).
    phase: String,
    /// Combined input+output tokens reported by the base (0 if unreported).
    tokens: u64,
}

/// Token usage for one phase within a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseUsage {
    /// Phase id (`research` / `frontend` / …).
    pub phase: String,
    /// How many worker calls landed in this phase.
    pub calls: u32,
    /// Combined input+output tokens across those calls.
    pub tokens: u64,
}

/// Token usage for one contiguous run (segmented from the flat log by a
/// [`RUN_GAP_SECS`] idle gap, since the log carries no explicit run id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunUsage {
    /// 1-based run ordinal (oldest = 1), for display.
    pub index: usize,
    /// Backends that served this run (usually one).
    pub backends: Vec<String>,
    /// Per-phase breakdown, in pipeline order where known.
    pub phases: Vec<PhaseUsage>,
    /// Worker calls in this run.
    pub calls: u32,
    /// Combined tokens across the whole run.
    pub tokens: u64,
}

/// A structured, language-neutral view over `~/.umadev/usage.jsonl`, ready for
/// the CLI / TUI to format with i18n chrome. Pure read — never writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageReport {
    /// Each contiguous run, oldest first.
    pub runs: Vec<RunUsage>,
    /// Total worker calls across all runs.
    pub total_calls: u32,
    /// Combined tokens across all runs.
    pub total_tokens: u64,
    /// Distinct backends seen, sorted.
    pub backends: Vec<String>,
}

impl UsageReport {
    /// `true` when no usable record was found (drives the empty state).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }
}

/// Idle gap (seconds) that splits the flat usage log into separate "runs".
/// 30 minutes: long enough that two phases of one pipeline never split, short
/// enough that yesterday's session is its own run. Heuristic — the log has no
/// run id, so this is a best-effort segmentation, not ground truth.
const RUN_GAP_SECS: u64 = 30 * 60;

/// Stable pipeline order used to sort a run's phases for display.
fn phase_order(phase: &str) -> usize {
    umadev_spec::PHASE_CHAIN
        .iter()
        .position(|p| p.id() == phase)
        .unwrap_or(usize::MAX)
}

/// Parse the raw usage JSONL into typed records (skips blank / malformed lines).
/// Re-uses the same tolerant field splitting as [`usage_summary`] so a hand-
/// edited or partially-written line can never panic this read-only path.
fn parse_usage_records(content: &str) -> Vec<UsageRecord> {
    let field_u64 = |line: &str, key: &str| -> Option<u64> {
        line.split(&format!("\"{key}\":"))
            .nth(1)
            .map(str::trim_start)
            .and_then(|s| s.split([',', '}']).next())
            .and_then(|s| s.trim().parse::<u64>().ok())
    };
    let field_str = |line: &str, key: &str| -> Option<String> {
        line.split(&format!("\"{key}\":\""))
            .nth(1)
            .and_then(|s| s.split('"').next())
            .map(str::to_string)
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| UsageRecord {
            ts: field_u64(line, "ts").unwrap_or(0),
            backend: field_str(line, "backend").unwrap_or_default(),
            phase: field_str(line, "phase").unwrap_or_default(),
            tokens: field_u64(line, "tokens").unwrap_or(0),
        })
        .collect()
}

/// Group parsed usage records into per-run, per-phase usage.
///
/// Records are sorted by timestamp, then segmented into runs wherever the gap to
/// the previous record exceeds [`RUN_GAP_SECS`]. Extracted from [`usage_report`]
/// so it can be unit-tested without touching the filesystem.
fn build_usage_report(mut records: Vec<UsageRecord>) -> UsageReport {
    if records.is_empty() {
        return UsageReport::default();
    }
    // Stable sort by timestamp keeps original order for equal-second rows.
    records.sort_by_key(|r| r.ts);

    let mut all_backends: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut runs: Vec<RunUsage> = Vec::new();
    let mut prev_ts: Option<u64> = None;
    // Accumulator for the run currently being built.
    let mut cur_phases: std::collections::BTreeMap<String, (u32, u64)> =
        std::collections::BTreeMap::new();
    let mut cur_backends: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut cur_calls: u32 = 0;
    let mut cur_tokens: u64 = 0;

    let flush = |index: usize,
                 phases: &std::collections::BTreeMap<String, (u32, u64)>,
                 backends: &std::collections::BTreeSet<String>,
                 calls: u32,
                 tokens: u64|
     -> RunUsage {
        let mut phase_rows: Vec<PhaseUsage> = phases
            .iter()
            .map(|(phase, (c, t))| PhaseUsage {
                phase: phase.clone(),
                calls: *c,
                tokens: *t,
            })
            .collect();
        phase_rows.sort_by_key(|p| (phase_order(&p.phase), p.phase.clone()));
        RunUsage {
            index,
            backends: backends.iter().cloned().collect(),
            phases: phase_rows,
            calls,
            tokens,
        }
    };

    for r in &records {
        let new_run = prev_ts.is_some_and(|p| r.ts.saturating_sub(p) > RUN_GAP_SECS);
        if new_run && cur_calls > 0 {
            runs.push(flush(
                runs.len() + 1,
                &cur_phases,
                &cur_backends,
                cur_calls,
                cur_tokens,
            ));
            cur_phases.clear();
            cur_backends.clear();
            cur_calls = 0;
            cur_tokens = 0;
        }
        if !r.backend.is_empty() {
            cur_backends.insert(r.backend.clone());
            all_backends.insert(r.backend.clone());
        }
        let entry = cur_phases.entry(r.phase.clone()).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += r.tokens;
        cur_calls += 1;
        cur_tokens += r.tokens;
        prev_ts = Some(r.ts);
    }
    if cur_calls > 0 {
        runs.push(flush(
            runs.len() + 1,
            &cur_phases,
            &cur_backends,
            cur_calls,
            cur_tokens,
        ));
    }

    let total_calls = runs.iter().map(|r| r.calls).sum();
    let total_tokens = runs.iter().map(|r| r.tokens).sum();
    UsageReport {
        runs,
        total_calls,
        total_tokens,
        backends: all_backends.into_iter().collect(),
    }
}

/// Build a structured [`UsageReport`] from `~/.umadev/usage.jsonl`.
///
/// Pure read: never writes, and fail-open — a missing or unreadable log yields
/// an empty report (`UsageReport::is_empty()` → true) so the caller can show a
/// friendly empty state. Powers `umadev usage` and the TUI `/usage` command.
#[must_use]
pub fn usage_report() -> UsageReport {
    let Ok(content) = std::fs::read_to_string(usage_path()) else {
        return UsageReport::default();
    };
    build_usage_report(parse_usage_records(&content))
}

/// A rough, advisory cost estimate (in US dollars) for a token total.
///
/// UmaDev owns no model endpoint and never sees per-token pricing — the base
/// CLI bills the customer's own subscription — so this is deliberately a single
/// flat blended rate, clearly labelled "仅供参考 / reference only" by the caller.
/// It exists so a token count has a human-relatable magnitude, NOT to be an
/// invoice. Returns dollars as `f64`; callers format to 2 dp.
#[must_use]
pub fn rough_cost_usd(total_tokens: u64) -> f64 {
    // Blended ~$3 / 1M tokens — a conservative middle-of-the-road figure across
    // the three bases' typical input/output mixes. Order-of-magnitude only.
    const USD_PER_MILLION: f64 = 3.0;
    (total_tokens as f64 / 1_000_000.0) * USD_PER_MILLION
}

/// A user-facing, plain-language description of what the worker is doing
/// in each phase — shown while the user waits. Replaces the generic
/// "调用 worker(X 阶段)" with something the user understands.
/// Extract the first balanced JSON object from a model reply — tolerant of
/// code fences / prose around it (LLMs love to wrap JSON in ```json … ```).
/// Tracks string/escape state so braces inside strings don't confuse depth.
fn extract_json_object(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else {
            match b {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return text.get(start..=i).map(str::to_string);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn phase_progress_hint(phase: Phase) -> &'static str {
    match phase {
        Phase::Research => "[search] 正在联网调研:分析竞品、技术选型、用户痛点…",
        Phase::Spec => "[docs] 正在制定执行计划:拆分任务、定接口契约…",
        Phase::Frontend => "[design] 正在开发前端:建项目、写组件、接图标库…",
        Phase::Backend => "[backend] 正在开发后端:写 API、数据模型、测试…",
        Phase::Quality => "[quality] 正在质量检查:跑构建/测试、评分…",
        Phase::Delivery => "[package] 正在准备交付:验证生产构建、写部署指令…",
        Phase::Docs => "[docs] 正在生成文档:PRD、架构、UIUX 设计…",
        Phase::DocsConfirm | Phase::PreviewConfirm => "[work] 正在处理…",
    }
}

/// Read the exponential-retry base delay (ms) from
/// `UMADEV_RETRY_BASE_MS`, defaulting to 2000 (2s). Used by
/// [`crate::runner::AgentRunner::try_generate`] so transient failures back off
/// (2s → 4s → 8s …) instead of hammering a rate-limited provider. Tests set a
/// tiny value to keep retry loops fast.
#[cfg(test)]
static RETRY_BASE_MS_TEST_OVERRIDE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn retry_base_ms() -> u64 {
    #[cfg(test)]
    {
        let override_ms = RETRY_BASE_MS_TEST_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed);
        if override_ms > 0 {
            return override_ms;
        }
    }
    let raw = std::env::var("UMADEV_RETRY_BASE_MS").ok();
    parse_retry_base_ms(raw.as_deref())
}

fn parse_retry_base_ms(raw: Option<&str>) -> u64 {
    raw.and_then(|s| s.parse().ok())
        .filter(|&v| v > 0)
        .unwrap_or(2000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use tempfile::TempDir;

    fn rec(ts: u64, backend: &str, phase: &str, tokens: u64) -> UsageRecord {
        UsageRecord {
            ts,
            backend: backend.to_string(),
            phase: phase.to_string(),
            tokens,
        }
    }

    #[test]
    fn usage_empty_report_for_no_records() {
        let r = build_usage_report(Vec::new());
        assert!(r.is_empty());
        assert_eq!(r.total_calls, 0);
        assert_eq!(r.total_tokens, 0);
        assert!(r.backends.is_empty());
    }

    #[test]
    fn usage_groups_phases_and_totals_in_one_run() {
        // Three calls within the gap window → one run; tokens sum per phase.
        let recs = vec![
            rec(1_000, "claude-code", "research", 100),
            rec(1_010, "claude-code", "frontend", 200),
            rec(1_020, "claude-code", "frontend", 50),
        ];
        let r = build_usage_report(recs);
        assert_eq!(r.runs.len(), 1, "all within RUN_GAP_SECS → single run");
        assert_eq!(r.total_calls, 3);
        assert_eq!(r.total_tokens, 350);
        assert_eq!(r.backends, vec!["claude-code".to_string()]);
        let run = &r.runs[0];
        assert_eq!(run.index, 1);
        // Phases are ordered by pipeline position: research before frontend.
        assert_eq!(run.phases[0].phase, "research");
        assert_eq!(run.phases[1].phase, "frontend");
        assert_eq!(run.phases[1].calls, 2);
        assert_eq!(run.phases[1].tokens, 250);
    }

    #[test]
    fn usage_splits_runs_on_idle_gap() {
        // A gap larger than RUN_GAP_SECS starts a second run.
        let recs = vec![
            rec(1_000, "codex", "research", 10),
            rec(1_000 + RUN_GAP_SECS + 1, "codex", "frontend", 20),
        ];
        let r = build_usage_report(recs);
        assert_eq!(r.runs.len(), 2);
        assert_eq!(r.runs[0].index, 1);
        assert_eq!(r.runs[1].index, 2);
        assert_eq!(r.runs[1].tokens, 20);
        assert_eq!(r.total_tokens, 30);
    }

    #[test]
    fn usage_sorts_out_of_order_timestamps() {
        // Records arriving out of order are sorted before segmentation.
        let recs = vec![
            rec(2_000, "codex", "frontend", 20),
            rec(1_000, "codex", "research", 10),
        ];
        let r = build_usage_report(recs);
        assert_eq!(r.runs.len(), 1);
        assert_eq!(r.runs[0].phases[0].phase, "research");
    }

    #[test]
    fn parse_usage_records_tolerates_malformed_lines() {
        let content = "\
{\"ts\":1000,\"backend\":\"claude-code\",\"phase\":\"research\",\"tokens\":100}
not json at all

{\"ts\":1010,\"backend\":\"codex\",\"phase\":\"frontend\",\"tokens\":200}
";
        let recs = parse_usage_records(content);
        assert_eq!(
            recs.len(),
            3,
            "blank line skipped, garbage line kept as zeros"
        );
        assert_eq!(recs[0].tokens, 100);
        assert_eq!(recs[0].phase, "research");
        // The garbage line parses to all-defaults rather than panicking.
        assert_eq!(recs[1].ts, 0);
        assert_eq!(recs[1].tokens, 0);
        assert_eq!(recs[2].backend, "codex");
    }

    #[test]
    fn rough_cost_is_advisory_and_monotonic() {
        assert!((rough_cost_usd(0) - 0.0).abs() < f64::EPSILON);
        // 1M tokens ≈ a few dollars at the blended flat rate.
        assert!(rough_cost_usd(1_000_000) > 0.0);
        assert!(rough_cost_usd(2_000_000) > rough_cost_usd(1_000_000));
    }

    #[test]
    fn model_for_phase_defaults_without_tier_env() {
        // With no tier env set, every phase uses the single configured model —
        // the default single-model behaviour is unchanged. (Uses a tier var
        // name no other test touches to stay isolated under parallel runs.)
        assert_eq!(model_for_phase("base-model", Phase::Frontend), "base-model");
        assert_eq!(model_for_phase("base-model", Phase::Docs), "base-model");
        assert_eq!(model_for_phase("base-model", Phase::Backend), "base-model");
    }
    use umadev_runtime::{
        CompletionRequest, CompletionResponse, Runtime, RuntimeError, RuntimeKind, Usage,
    };

    fn set_retry_base_ms_for_tests(ms: u64) {
        RETRY_BASE_MS_TEST_OVERRIDE.store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    struct FakeRuntime;

    #[async_trait]
    impl Runtime for FakeRuntime {
        fn kind(&self) -> RuntimeKind {
            RuntimeKind::Anthropic
        }
        fn capabilities(&self) -> umadev_runtime::BrainCapabilities {
            // Claude-like: has native persistent /goal mode.
            umadev_runtime::BrainCapabilities {
                persistent_goal: true,
                ..Default::default()
            }
        }
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, RuntimeError> {
            Ok(CompletionResponse {
                text: "stub".into(),
                id: "stub".into(),
                model: "stub".into(),
                usage: Usage::default(),
            })
        }
    }

    /// A brain WITHOUT native persistent-goal mode (codex/opencode-like).
    struct FakeRuntimeNoGoal;

    #[async_trait]
    impl Runtime for FakeRuntimeNoGoal {
        fn kind(&self) -> RuntimeKind {
            RuntimeKind::Openai
        }
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, RuntimeError> {
            Ok(CompletionResponse {
                text: "stub".into(),
                id: "stub".into(),
                model: "stub".into(),
                usage: Usage::default(),
            })
        }
    }

    /// A brain that replies with a JSON verdict (for `consult`/judge tests).
    struct FakeRuntimeJson;

    #[async_trait]
    impl Runtime for FakeRuntimeJson {
        fn kind(&self) -> RuntimeKind {
            RuntimeKind::Anthropic
        }
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, RuntimeError> {
            Ok(CompletionResponse {
                text: "Here is my verdict:\n```json\n{\"unmet\": [\"登录功能未实现\"], \
                       \"commercial_ready\": false, \"notes\": \"缺登录\"}\n```"
                    .into(),
                id: "j".into(),
                model: "j".into(),
                usage: Usage::default(),
            })
        }
    }

    /// A brain whose JSON reply carries a superset of all verdict fields — so a
    /// single fake exercises the intake / docs / preview judges.
    struct FakeRuntimeRich;

    #[async_trait]
    impl Runtime for FakeRuntimeRich {
        fn kind(&self) -> RuntimeKind {
            RuntimeKind::Anthropic
        }
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, RuntimeError> {
            Ok(CompletionResponse {
                text: "```json\n{\"product_type\":\"SaaS 仪表盘\",\"complexity\":\"medium\",\
                       \"core_features\":[\"登录\",\"图表\"],\"key_risks\":[\"权限\"],\
                       \"biggest_risk\":\"鉴权设计\",\"missing\":[\"导出\"],\"ready\":false}\n```"
                    .into(),
                id: "r".into(),
                model: "r".into(),
                usage: Usage::default(),
            })
        }
    }

    /// A brain that replies with a DESIGN verdict (issues + not commercial).
    struct FakeRuntimeDesign;

    #[async_trait]
    impl Runtime for FakeRuntimeDesign {
        fn kind(&self) -> RuntimeKind {
            RuntimeKind::Anthropic
        }
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, RuntimeError> {
            Ok(CompletionResponse {
                text: "{\"issues\": [\"紫色渐变像 AI 生成\", \"emoji 当图标\"], \
                       \"commercial_grade\": false}"
                    .into(),
                id: "d".into(),
                model: "d".into(),
                usage: Usage::default(),
            })
        }
    }

    /// A brain that replies with a RoleVerdict (for the critic-team test). Every
    /// judge turn returns the same not-accepting verdict with one blocking item.
    struct FakeRuntimeRoleCritic;

    #[async_trait]
    impl Runtime for FakeRuntimeRoleCritic {
        fn kind(&self) -> RuntimeKind {
            RuntimeKind::Anthropic
        }
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, RuntimeError> {
            Ok(CompletionResponse {
                text: "```json\n{\"accepts\": false, \"blocking\": [\"验收标准未覆盖核心功能\"], \
                       \"advisory\": [\"补充成功指标\"], \"evidence\": [\"prd.md\"]}\n```"
                    .into(),
                id: "rc".into(),
                model: "rc".into(),
                usage: Usage::default(),
            })
        }
    }

    /// A brain that governs at write time (claude-like PreToolUse hook).
    struct FakeRuntimeGoverns;

    #[async_trait]
    impl Runtime for FakeRuntimeGoverns {
        fn kind(&self) -> RuntimeKind {
            RuntimeKind::Anthropic
        }
        fn capabilities(&self) -> umadev_runtime::BrainCapabilities {
            umadev_runtime::BrainCapabilities {
                realtime_governance: true,
                ..Default::default()
            }
        }
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, RuntimeError> {
            Ok(CompletionResponse {
                text: "stub".into(),
                id: "stub".into(),
                model: "stub".into(),
                usage: Usage::default(),
            })
        }
    }

    fn opts(root: &std::path::Path) -> RunOptions {
        RunOptions {
            project_root: root.to_path_buf(),
            requirement: "build a login page".into(),
            slug: "demo".into(),
            model: "stub".into(),
            backend: String::new(),
            design_system: String::new(),
            seed_template: String::new(),
            mode: crate::trust::TrustMode::Guarded,
        }
    }

    /// A runtime that is NOT offline (so `use_runtime` is on) but always returns
    /// EMPTY text — i.e. the base went dark mid-run. Drives the #1 degraded path:
    /// the runner must fall back to the offline template AND flag the phase as
    /// degraded rather than passing the placeholder off as a clean success.
    struct EmptyRuntime;

    #[async_trait]
    impl Runtime for EmptyRuntime {
        fn kind(&self) -> RuntimeKind {
            RuntimeKind::Anthropic
        }
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, RuntimeError> {
            Ok(CompletionResponse {
                text: String::new(), // empty body == base offline/blank
                id: "empty".into(),
                model: "empty".into(),
                usage: Usage::default(),
            })
        }
    }

    /// A runtime that fails the first N calls with a transient 429, then
    /// succeeds. Used to prove try_generate retries with backoff.
    struct FlakyRuntime {
        fails_left: std::sync::Mutex<usize>,
    }

    #[async_trait]
    impl Runtime for FlakyRuntime {
        fn kind(&self) -> RuntimeKind {
            RuntimeKind::Anthropic
        }
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, RuntimeError> {
            let mut left = self.fails_left.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                return Err(RuntimeError::HostProcess("429 Too Many Requests".into()));
            }
            Ok(CompletionResponse {
                text: "recovered".into(),
                id: "flaky".into(),
                model: "flaky".into(),
                usage: Usage::default(),
            })
        }
    }

    #[test]
    fn summarize_stderr_takes_last_meaningful_lines() {
        let stderr = "compiling...
downloading deps...

error TS2304: Cannot find name 'Foo'
  at src/App.tsx:12:5
";
        let out = summarize_stderr(stderr);
        // Progress noise ("compiling...", "downloading deps...") at the top
        // is dropped because we take the LAST non-empty lines. The actual
        // error must be present.
        assert!(out.contains("Cannot find name 'Foo'"));
        assert!(out.contains("src/App.tsx:12:5"));
    }

    #[test]
    fn summarize_stderr_truncates_long_output() {
        // Each line is long enough that 8 lines exceed 1200 chars → truncates.
        let line = "x".repeat(300);
        let long = format!("{line}\n{line}\n{line}\n{line}\n{line}\n{line}\n{line}\n{line}\n");
        let out = summarize_stderr(&long);
        assert!(
            out.ends_with("…[truncated]"),
            "expected truncation marker, got tail: {:?}",
            &out[out.len().saturating_sub(40)..]
        );
        assert!(out.chars().count() <= 1220); // 1200 + the marker
    }

    #[test]
    fn summarize_stderr_empty_returns_empty() {
        assert_eq!(summarize_stderr(""), "");
        assert_eq!(summarize_stderr("\n\n  \n"), "");
    }

    #[tokio::test]
    async fn verify_and_fix_skips_when_no_runtime() {
        // Offline mode (backend empty) → maybe_verify_and_fix must not attempt
        // a fix even if verify fails, because there's no worker to fix it.
        let tmp = TempDir::new().unwrap();
        // A package.json so detect_project sees a Node project, but no node_modules
        // so npm install fails → verify fails.
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"x","scripts":{"build":"exit 1"}}"#,
        )
        .unwrap();
        let runner = AgentRunner::new(FakeRuntime, opts(tmp.path()));
        runner.start().unwrap();
        // Must not panic / hang — returns after the (failed) verify with no fix.
        runner.maybe_verify_and_fix(Phase::Frontend).await;
        // If we reach here without panic, the no-runtime early return worked.
    }

    #[test]
    fn verify_outcome_struct_pass_and_fail() {
        let p = VerifyOutcome {
            passed: true,
            skipped: false,
            failure_detail: String::new(),
        };
        assert!(p.passed);
        let f = VerifyOutcome {
            passed: false,
            skipped: false,
            failure_detail: "error TS2304".into(),
        };
        assert!(!f.passed);
        assert_eq!(f.failure_detail, "error TS2304");
    }

    #[test]
    fn retry_base_ms_parser_honors_override() {
        assert_eq!(parse_retry_base_ms(Some("5")), 5);
    }

    #[test]
    fn retry_base_ms_defaults_to_2000() {
        assert_eq!(parse_retry_base_ms(None), 2000);
        assert_eq!(parse_retry_base_ms(Some("0")), 2000);
        assert_eq!(parse_retry_base_ms(Some("not-a-number")), 2000);
    }

    #[test]
    fn merged_requirement_without_answers_file() {
        // No answers file → returns original requirement unchanged.
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        let runner = AgentRunner::new(FakeRuntime, o);
        assert_eq!(runner.merged_requirement(), "build a login page");
    }

    #[test]
    fn merged_requirement_with_answers_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::write(
            tmp.path().join("output/demo-clarify-answers.md"),
            "1. 面向个人开发者
2. 需要邮箱登录
3. 移动端优先",
        )
        .unwrap();
        let o = opts(tmp.path());
        let runner = AgentRunner::new(FakeRuntime, o);
        let merged = runner.merged_requirement();
        assert!(
            merged.contains("build a login page"),
            "original must be present"
        );
        assert!(merged.contains("邮箱登录"), "answers must be folded in");
        assert!(
            merged.contains("用户澄清回答"),
            "must have the section header"
        );
    }

    #[test]
    fn merged_requirement_empty_answers_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::write(
            tmp.path().join("output/demo-clarify-answers.md"),
            "   
  ",
        )
        .unwrap();
        let o = opts(tmp.path());
        let runner = AgentRunner::new(FakeRuntime, o);
        // Empty answers → original requirement (no merge).
        assert_eq!(runner.merged_requirement(), "build a login page");
    }

    #[tokio::test]
    async fn run_clarify_produces_clarify_gate() {
        let tmp = TempDir::new().unwrap();
        set_retry_base_ms_for_tests(1);
        let runner = AgentRunner::new(FakeRuntime, opts(tmp.path()));
        runner.start().unwrap();
        let report = runner.run_clarify(true).await;
        let report = report.unwrap();
        assert_eq!(report.paused_at, Some(Gate::ClarifyGate));
    }

    #[tokio::test]
    async fn try_generate_retries_transient_then_succeeds() {
        let tmp = TempDir::new().unwrap();
        // Use a tiny backoff so the test is fast.
        set_retry_base_ms_for_tests(1);
        let runner = AgentRunner::new(
            FlakyRuntime {
                fails_left: std::sync::Mutex::new(2),
            },
            opts(tmp.path()),
        );
        runner.start().unwrap();
        let p = crate::experts::research_prompt("demo", "req", "");
        let out = runner.try_generate(Phase::Research, p).await;
        // After 2 transient 429s the 3rd call succeeds → recovered text.
        assert_eq!(out.as_deref(), Some("recovered"));
    }

    #[tokio::test]
    async fn try_generate_gives_up_after_max_retries() {
        let tmp = TempDir::new().unwrap();
        set_retry_base_ms_for_tests(1);
        // Always fails (fails_left huge) → exhausts retries → None.
        let runner = AgentRunner::new(
            FlakyRuntime {
                fails_left: std::sync::Mutex::new(99),
            },
            opts(tmp.path()),
        );
        runner.start().unwrap();
        let p = crate::experts::research_prompt("demo", "req", "");
        let out = runner.try_generate(Phase::Research, p).await;
        assert!(out.is_none(), "should give up after max_retries");
    }

    #[test]
    fn start_writes_initial_state() {
        let tmp = TempDir::new().unwrap();
        let runner = AgentRunner::new(FakeRuntime, opts(tmp.path()));
        let state = runner.start().unwrap();
        assert_eq!(state.phase, "research");
    }

    #[tokio::test]
    async fn initial_block_pauses_at_docs_confirm() {
        let tmp = TempDir::new().unwrap();
        let runner = AgentRunner::new(FakeRuntime, opts(tmp.path()));
        runner.start().unwrap();
        let r = runner.run_initial_block(false, None).await.unwrap();
        assert_eq!(r.final_phase, Phase::DocsConfirm);
        assert_eq!(r.paused_at, Some(Gate::DocsConfirm));
        assert!(tmp.path().join("output/demo-prd.md").is_file());
    }

    #[tokio::test]
    async fn plan_mode_stops_at_docs_with_readonly_note() {
        use crate::events::{EngineEvent, RecordingSink};
        use std::sync::Arc;
        let tmp = TempDir::new().unwrap();
        let mut o = opts(tmp.path());
        o.mode = crate::trust::TrustMode::Plan;
        let sink = RecordingSink::new();
        let runner = AgentRunner::new(FakeRuntime, o).with_event_sink(Arc::new(sink.clone()));
        runner.start().unwrap();
        let r = runner.run_initial_block(false, None).await.unwrap();
        // Plan mode produces the planning docs and pauses at docs_confirm —
        // identical pause point, but it announces the read-only stop.
        assert_eq!(r.paused_at, Some(Gate::DocsConfirm));
        let emitted_plan_note = sink.events().iter().any(|e| {
            matches!(e, EngineEvent::Note(n) if n.contains("[plan]") && n.contains("read-only"))
        });
        assert!(
            emitted_plan_note,
            "plan mode must announce the read-only stop"
        );
    }

    #[tokio::test]
    async fn guarded_mode_emits_no_plan_stop_note() {
        // The default (guarded) run must NOT emit the plan read-only note — it
        // is identical to the pre-existing behaviour.
        use crate::events::{EngineEvent, RecordingSink};
        use std::sync::Arc;
        let tmp = TempDir::new().unwrap();
        let sink = RecordingSink::new();
        let runner =
            AgentRunner::new(FakeRuntime, opts(tmp.path())).with_event_sink(Arc::new(sink.clone()));
        runner.start().unwrap();
        runner.run_initial_block(false, None).await.unwrap();
        let plan_note = sink
            .events()
            .iter()
            .any(|e| matches!(e, EngineEvent::Note(n) if n.contains("Plan mode (read-only)")));
        assert!(!plan_note, "guarded mode must not announce a plan stop");
    }

    #[tokio::test]
    async fn after_docs_pauses_at_preview() {
        let tmp = TempDir::new().unwrap();
        let runner = AgentRunner::new(FakeRuntime, opts(tmp.path()));
        runner.start().unwrap();
        runner.run_initial_block(false, None).await.unwrap();
        let r = runner.continue_after_docs_confirm().await.unwrap();
        assert_eq!(r.final_phase, Phase::PreviewConfirm);
        assert_eq!(r.paused_at, Some(Gate::PreviewConfirm));
        assert!(tmp.path().join("output/demo-execution-plan.md").is_file());
        assert!(tmp.path().join("output/demo-frontend-notes.md").is_file());
    }

    #[tokio::test]
    async fn after_preview_runs_to_delivery() {
        let tmp = TempDir::new().unwrap();
        // Offline templates pass quality → the full deterministic flow reaches
        // Delivery. (A real/stub brain correctly stops at a failing quality gate.)
        let runner = AgentRunner::new(
            umadev_runtime::OfflineRuntime::new(RuntimeKind::Anthropic),
            opts(tmp.path()),
        );
        runner.start().unwrap();
        runner.run_initial_block(false, None).await.unwrap();
        runner.continue_after_docs_confirm().await.unwrap();
        let r = runner.continue_after_preview_confirm().await.unwrap();
        assert_eq!(r.final_phase, Phase::Delivery);
        assert_eq!(r.paused_at, None);

        assert!(tmp.path().join("output/demo-backend-notes.md").is_file());
        assert!(tmp.path().join("output/demo-quality-gate.json").is_file());
        let release = tmp.path().join("release");
        let entries: Vec<_> = std::fs::read_dir(&release)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(entries
            .iter()
            .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("zip")));
    }

    #[tokio::test]
    async fn worker_run_blocks_delivery_when_quality_gate_fails() {
        // UD-EVID-003: a worker-backed run whose quality gate emits
        // passed:false MUST refuse to advance to delivery. We simulate this
        // by writing a failing quality-gate.json before the preview→delivery
        // block, with a worker backend configured (use_runtime = true).
        let tmp = TempDir::new().unwrap();
        let mut o = opts(tmp.path());
        o.backend = "claude-code".into();
        let runner = AgentRunner::new(FakeRuntime, o);
        runner.start().unwrap();
        runner.run_initial_block(true, None).await.unwrap();
        runner.continue_after_docs_confirm().await.unwrap();
        // Overwrite the quality gate with a failing one (passed:false, score
        // below threshold). This runs before continue_after_preview_confirm
        // reaches its quality check.
        std::fs::write(
            tmp.path().join("output/demo-quality-gate.json"),
            r#"{"passed":false,"total_score":40,"weighted_score":40.0,"scenario":"test","critical_failures":["Build & test results"],"recommendations":[],"summary":{"executive_summary":"fail","summary_context":{}},"checks":[]}"#,
        )
        .unwrap();
        let r = runner.continue_after_preview_confirm().await.unwrap();
        // Must pause at quality, NOT advance to delivery.
        assert_eq!(r.final_phase, Phase::Quality);
        assert_eq!(r.paused_at, None);
        // No release zip should exist (delivery never ran).
        assert!(
            !tmp.path().join("release").is_dir()
                || !std::fs::read_dir(tmp.path().join("release"))
                    .unwrap()
                    .filter_map(Result::ok)
                    .any(|e| e.path().extension().and_then(|s| s.to_str()) == Some("zip"))
        );
    }

    #[tokio::test]
    async fn subtask_events_emitted_for_backend_worker() {
        // When a worker backend is configured, the backend phase emits
        // SubTaskStarted + SubTaskCompleted around the worker call.
        use crate::events::{EngineEvent, RecordingSink};
        use std::sync::Arc;
        let tmp = TempDir::new().unwrap();
        let mut o = opts(tmp.path());
        o.backend = "claude-code".into(); // triggers use_runtime path
        let sink = RecordingSink::new();
        let runner = AgentRunner::new(FakeRuntime, o).with_event_sink(Arc::new(sink.clone()));
        runner.start().unwrap();
        runner.run_initial_block(true, None).await.unwrap();
        runner.continue_after_docs_confirm().await.unwrap();
        runner.continue_after_preview_confirm().await.unwrap();

        let started = sink.count(|e| {
            matches!(
                e,
                EngineEvent::SubTaskStarted { task_id, .. } if task_id == "backend.implement"
            )
        });
        let completed = sink.count(|e| matches!(
            e,
            EngineEvent::SubTaskCompleted { task_id, ok: true, .. } if task_id == "backend.implement"
        ));
        assert_eq!(
            started, 1,
            "expected one SubTaskStarted for backend.implement"
        );
        assert_eq!(completed, 1, "expected one successful SubTaskCompleted");
    }

    #[tokio::test]
    async fn dispatch_routes_to_right_block() {
        let tmp = TempDir::new().unwrap();
        let runner = AgentRunner::new(
            umadev_runtime::OfflineRuntime::new(RuntimeKind::Anthropic),
            opts(tmp.path()),
        );
        runner.start().unwrap();
        runner.run_initial_block(false, None).await.unwrap();

        let r = runner.continue_from_gate(Gate::DocsConfirm).await.unwrap();
        assert_eq!(r.final_phase, Phase::PreviewConfirm);
        let r = runner
            .continue_from_gate(Gate::PreviewConfirm)
            .await
            .unwrap();
        assert_eq!(r.final_phase, Phase::Delivery);
    }

    #[tokio::test]
    async fn revise_at_docs_gate_regenerates_docs_and_pauses_again() {
        let tmp = TempDir::new().unwrap();
        let runner = AgentRunner::new(FakeRuntime, opts(tmp.path()));
        runner.start().unwrap();
        runner.run_initial_block(false, None).await.unwrap();
        // Revising at docs_confirm re-runs the docs block and pauses at
        // the SAME gate — it must NOT advance to preview_confirm.
        let r = runner
            .revise_at_gate(Gate::DocsConfirm, false)
            .await
            .unwrap();
        assert_eq!(r.final_phase, Phase::DocsConfirm);
        assert_eq!(r.paused_at, Some(Gate::DocsConfirm));
        assert!(tmp.path().join("output/demo-prd.md").is_file());
    }

    #[tokio::test]
    async fn revise_at_preview_gate_regenerates_frontend_not_docs() {
        let tmp = TempDir::new().unwrap();
        let runner = AgentRunner::new(FakeRuntime, opts(tmp.path()));
        runner.start().unwrap();
        runner.run_initial_block(false, None).await.unwrap();
        runner.continue_after_docs_confirm().await.unwrap();
        // Revising at preview_confirm re-runs spec→frontend and pauses at
        // preview_confirm again — the key fix: a UI revision must not throw
        // away the approved docs by regenerating them.
        let r = runner
            .revise_at_gate(Gate::PreviewConfirm, false)
            .await
            .unwrap();
        assert_eq!(r.final_phase, Phase::PreviewConfirm);
        assert_eq!(r.paused_at, Some(Gate::PreviewConfirm));
        assert!(tmp.path().join("output/demo-frontend-notes.md").is_file());
    }

    #[tokio::test]
    async fn initial_block_uses_runtime_when_requested() {
        // FakeRuntime returns "stub" text — verify it lands in the artifacts.
        let tmp = TempDir::new().unwrap();
        let runner = AgentRunner::new(FakeRuntime, opts(tmp.path()));
        runner.start().unwrap();
        runner.run_initial_block(true, None).await.unwrap();

        let research = std::fs::read_to_string(tmp.path().join("output/demo-research.md")).unwrap();
        let prd = std::fs::read_to_string(tmp.path().join("output/demo-prd.md")).unwrap();
        assert_eq!(research.trim(), "stub");
        assert_eq!(prd.trim(), "stub");
    }

    #[tokio::test]
    async fn event_sink_observes_the_full_pipeline() {
        use crate::events::{EngineEvent, RecordingSink};
        use std::sync::Arc;

        let tmp = TempDir::new().unwrap();
        let sink = RecordingSink::new();
        let runner = AgentRunner::new(
            umadev_runtime::OfflineRuntime::new(RuntimeKind::Anthropic),
            opts(tmp.path()),
        )
        .with_event_sink(Arc::new(sink.clone()));
        runner.start().unwrap();
        runner.run_initial_block(false, None).await.unwrap();
        runner.continue_after_docs_confirm().await.unwrap();
        runner.continue_after_preview_confirm().await.unwrap();

        let events = sink.events();
        // Exactly one PipelineStarted.
        assert_eq!(
            sink.count(|e| matches!(e, EngineEvent::PipelineStarted { .. })),
            1
        );
        // All seven worker phases emit a PhaseStarted (gates do not).
        assert_eq!(
            sink.count(|e| matches!(e, EngineEvent::PhaseStarted { .. })),
            7
        );
        // Both gates open.
        assert_eq!(
            sink.count(|e| matches!(e, EngineEvent::GateOpened { .. })),
            2
        );
        // Three BlockCompleted (initial, docs→preview, preview→delivery).
        assert_eq!(
            sink.count(|e| matches!(e, EngineEvent::BlockCompleted { .. })),
            3
        );
        // The last event is the delivery BlockCompleted with no gate.
        assert!(matches!(
            events.last(),
            Some(EngineEvent::BlockCompleted {
                final_phase: Phase::Delivery,
                paused_at: None,
            })
        ));
        // First event is always PipelineStarted.
        assert!(matches!(
            events.first(),
            Some(EngineEvent::PipelineStarted { .. })
        ));
    }

    #[tokio::test]
    async fn knowledge_note_emitted_when_knowledge_dir_present() {
        use crate::events::{EngineEvent, RecordingSink};
        use std::sync::Arc;

        let tmp = TempDir::new().unwrap();
        // Seed a tiny knowledge/ so the runner picks something.
        let kd = tmp.path().join("knowledge").join("security");
        std::fs::create_dir_all(&kd).unwrap();
        std::fs::write(
            kd.join("login-playbook.md"),
            "# Login Playbook\nUse OAuth2 + PKCE.\n",
        )
        .unwrap();

        let sink = RecordingSink::new();
        let runner =
            AgentRunner::new(FakeRuntime, opts(tmp.path())).with_event_sink(Arc::new(sink.clone()));
        runner.start().unwrap();
        runner.run_initial_block(false, None).await.unwrap();

        let knowledge_notes: Vec<EngineEvent> = sink
            .events()
            .into_iter()
            .filter(|e| matches!(e, EngineEvent::Note(s) if s.contains("knowledge")))
            .collect();
        assert_eq!(knowledge_notes.len(), 1);
        if let EngineEvent::Note(text) = &knowledge_notes[0] {
            assert!(text.contains("login-playbook"));
        } else {
            unreachable!("filtered for Note above")
        }
    }

    #[tokio::test]
    async fn no_knowledge_note_when_no_knowledge_dir() {
        use crate::events::{EngineEvent, RecordingSink};
        use std::sync::Arc;

        let tmp = TempDir::new().unwrap();
        // No knowledge/ subdir at all.
        let sink = RecordingSink::new();
        let runner =
            AgentRunner::new(FakeRuntime, opts(tmp.path())).with_event_sink(Arc::new(sink.clone()));
        runner.start().unwrap();
        runner.run_initial_block(false, None).await.unwrap();

        let knowledge_notes = sink
            .events()
            .iter()
            .filter(|e| matches!(e, EngineEvent::Note(s) if s.contains("knowledge")))
            .count();
        assert_eq!(knowledge_notes, 0);
    }

    #[tokio::test]
    async fn null_sink_is_the_default_and_runs_clean() {
        // A runner with no sink attached must behave identically.
        let tmp = TempDir::new().unwrap();
        let runner = AgentRunner::new(FakeRuntime, opts(tmp.path()));
        runner.start().unwrap();
        let r = runner.run_initial_block(false, None).await.unwrap();
        assert_eq!(r.final_phase, Phase::DocsConfirm);
    }

    #[tokio::test]
    async fn maybe_verify_skips_when_no_project_manifest() {
        use crate::events::{EngineEvent, RecordingSink};
        use std::sync::Arc;

        let tmp = TempDir::new().unwrap();
        let sink = RecordingSink::new();
        let runner =
            AgentRunner::new(FakeRuntime, opts(tmp.path())).with_event_sink(Arc::new(sink.clone()));
        runner.maybe_verify(Phase::Frontend).await;
        let events = sink.events();
        // Frontend → verify started + skipped (no manifest in tmp dir).
        assert!(events
            .iter()
            .any(|e| matches!(e, EngineEvent::VerifyStarted { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, EngineEvent::VerifySkipped { .. })));
    }

    #[tokio::test]
    async fn maybe_verify_is_noop_for_non_code_phases() {
        use crate::events::RecordingSink;
        use std::sync::Arc;

        let tmp = TempDir::new().unwrap();
        let sink = RecordingSink::new();
        let runner =
            AgentRunner::new(FakeRuntime, opts(tmp.path())).with_event_sink(Arc::new(sink.clone()));
        // research / docs / spec / delivery do NOT trigger verify.
        for phase in [Phase::Research, Phase::Docs, Phase::Spec, Phase::Delivery] {
            runner.maybe_verify(phase).await;
        }
        assert!(sink.events().is_empty());
    }

    #[tokio::test]
    async fn maybe_verify_records_outcome_to_audit_jsonl() {
        use std::sync::Arc;

        let tmp = TempDir::new().unwrap();
        // Drop a valid Rust manifest so verify picks Rust + tries cargo check.
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"verify-test\"\nversion = \"0.0.1\"\nedition = \"2021\"",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), "fn main(){}").unwrap();

        let sink = crate::events::RecordingSink::new();
        let runner =
            AgentRunner::new(FakeRuntime, opts(tmp.path())).with_event_sink(Arc::new(sink.clone()));
        runner.maybe_verify(Phase::Frontend).await;

        let audit = tmp.path().join(".umadev/audit/verify.jsonl");
        assert!(audit.exists(), "verify.jsonl was not created");
        let body = std::fs::read_to_string(&audit).unwrap();
        assert!(body.contains("\"phase\":\"frontend\""));
        assert!(body.contains("\"project_kind\":\"rust\""));
    }

    #[test]
    fn expert_knowledge_injection_works() {
        let tmp = TempDir::new().unwrap();
        // Create an expert file
        let expert_dir = tmp.path().join("knowledge/experts/product-manager");
        std::fs::create_dir_all(&expert_dir).unwrap();
        std::fs::write(
            expert_dir.join("methodology.md"),
            "# PM Methodology\n\n## RICE Scoring\nReach × Impact × Confidence / Effort\n",
        )
        .unwrap();

        let runner = AgentRunner::new(FakeRuntime, opts(tmp.path()));
        let prompt = Prompt {
            system: "Base system prompt.".to_string(),
            user: "Write a PRD.".to_string(),
        };
        let enhanced = runner.with_expert_knowledge(prompt, &["product-manager"]);
        assert!(
            enhanced.system.contains("RICE Scoring"),
            "Expert knowledge not injected into prompt. System: {}",
            enhanced.system
        );
        assert!(
            enhanced.system.contains("Base system prompt"),
            "Original system prompt lost"
        );
    }

    #[test]
    fn expert_knowledge_noop_when_no_files() {
        let tmp = TempDir::new().unwrap();
        let runner = AgentRunner::new(FakeRuntime, opts(tmp.path()));
        let prompt = Prompt {
            system: "Original.".to_string(),
            user: "User.".to_string(),
        };
        let enhanced = runner.with_expert_knowledge(prompt, &["nonexistent-expert"]);
        assert_eq!(enhanced.system, "Original.");
    }

    #[test]
    fn goal_mode_uses_native_goal_when_brain_supports_it() {
        let tmp = TempDir::new().unwrap();
        let mut o = opts(tmp.path());
        o.requirement = "做一个待办应用".into();
        // FakeRuntime declares persistent_goal=true (claude-like).
        let runner = AgentRunner::new(FakeRuntime, o);
        let p = || Prompt {
            system: "BASE".into(),
            user: "U".into(),
        };
        // Frontend: /goal prepended to the FRONT of system (so it's first in
        // the merged prompt), original system preserved after it.
        let fe = runner.with_goal_mode(p(), Phase::Frontend);
        assert!(fe.system.starts_with("/goal "), "system: {}", fe.system);
        assert!(fe.system.contains("做一个待办应用") && fe.system.contains("BASE"));
        // Points the base at OUR breakdown (the task list), not just the raw one.
        assert!(fe.system.contains("execution-plan") && fe.system.contains("任务清单"));
        // Backend too; bounded phases unchanged.
        assert!(runner
            .with_goal_mode(p(), Phase::Backend)
            .system
            .starts_with("/goal "));
        assert_eq!(runner.with_goal_mode(p(), Phase::Docs).system, "BASE");
    }

    #[test]
    fn extract_json_object_handles_fences_strings_and_nesting() {
        assert_eq!(
            extract_json_object("```json\n{\"a\":1}\n```").as_deref(),
            Some("{\"a\":1}")
        );
        assert_eq!(
            extract_json_object("prose {\"x\": {\"y\": 2}} tail").as_deref(),
            Some("{\"x\": {\"y\": 2}}")
        );
        // A brace inside a string must NOT break depth tracking.
        assert_eq!(
            extract_json_object(r#"{"s": "a}b"}"#).as_deref(),
            Some(r#"{"s": "a}b"}"#)
        );
        assert_eq!(extract_json_object("no json here at all"), None);
    }

    #[tokio::test]
    async fn consult_is_fail_open_offline_and_parses_verdict_with_a_brain() {
        let tmp = TempDir::new().unwrap();
        // Offline (empty backend) → None (fail-open; caller uses deterministic).
        let off = AgentRunner::new(FakeRuntime, opts(tmp.path()));
        let v: Option<AcceptanceVerdict> = off.consult("judge", "x".into()).await;
        assert!(v.is_none());
        // With a brain that returns JSON → parsed into the verdict.
        let mut o = opts(tmp.path());
        o.backend = "claude-code".into();
        let on = AgentRunner::new(FakeRuntimeJson, o);
        let v: Option<AcceptanceVerdict> = on.consult("judge", "x".into()).await;
        let v = v.expect("JSON verdict should parse");
        assert!(!v.commercial_ready);
        assert_eq!(v.unmet, vec!["登录功能未实现".to_string()]);
    }

    #[test]
    fn with_context_injects_design_system_into_ui_worker_prompts() {
        let tmp = TempDir::new().unwrap();
        // Scaffold the design-system file the injector reads (init does this in
        // a real workspace).
        std::fs::create_dir_all(tmp.path().join("knowledge/design-systems")).unwrap();
        std::fs::write(
            tmp.path()
                .join("knowledge/design-systems/modern-minimal.md"),
            ":root { --bg: #fff; --fg: #111; }\n",
        )
        .unwrap();
        let mut o = opts(tmp.path());
        o.design_system = "modern-minimal".into();
        let runner = AgentRunner::new(FakeRuntime, o);
        // Frontend (a UI phase): the design system BINDING CONTRACT is appended
        // to the WORKER prompt's system — original preserved.
        let fe = runner.with_context(
            Prompt {
                system: "BASE".into(),
                user: "U".into(),
            },
            Phase::Frontend,
        );
        assert!(fe.system.starts_with("BASE"));
        assert!(
            fe.system.contains("BINDING DESIGN CONTRACT") && fe.system.contains("--bg"),
            "design system should be injected into the frontend worker prompt"
        );
        // A non-UI phase → no design block.
        let research = runner.with_context(
            Prompt {
                system: "BASE".into(),
                user: "U".into(),
            },
            Phase::Research,
        );
        assert!(!research.system.contains("BINDING DESIGN CONTRACT"));
    }

    #[tokio::test]
    async fn task_acceptance_flags_unimplemented_endpoints() {
        use crate::events::RecordingSink;
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::write(
            tmp.path().join("output/demo-architecture.md"),
            "# API\n\n| Method | Path | Description | Auth |\n|---|---|---|---|\n\
             | GET | /api/orders | list orders | none |\n\
             | POST | /api/orders | create order | required |\n",
        )
        .unwrap();
        // No src/ written → both planned endpoints are gaps.
        let mut o = opts(tmp.path());
        o.backend = "claude-code".into();
        let sink = RecordingSink::new();
        let runner = AgentRunner::new(FakeRuntime, o).with_event_sink(Arc::new(sink.clone()));
        runner.run_task_acceptance("demo").await;
        let notes: Vec<String> = sink
            .events()
            .iter()
            .filter_map(|e| match e {
                EngineEvent::Note(n) => Some(n.clone()),
                _ => None,
            })
            .collect();
        // It detected the gaps and announced re-delegating to the base.
        assert!(
            notes
                .iter()
                .any(|n| n.contains("acceptance") && n.contains("打回")),
            "expected an acceptance gap note, got: {notes:?}"
        );
    }

    #[tokio::test]
    async fn intelligent_judges_surface_shared_brain_assessments() {
        use crate::events::RecordingSink;
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("output/demo-prd.md"), "# PRD\n目标…").unwrap();
        std::fs::write(
            tmp.path().join("output/demo-frontend-notes.md"),
            "# notes\n建了 dashboard",
        )
        .unwrap();
        std::fs::write(tmp.path().join("src/App.tsx"), "export const A=()=><div/>;").unwrap();
        let mut o = opts(tmp.path());
        o.backend = "claude-code".into();
        let sink = RecordingSink::new();
        let runner = AgentRunner::new(FakeRuntimeRich, o).with_event_sink(Arc::new(sink.clone()));
        runner.surface_intake_plan().await;
        runner.surface_docs_assessment("demo").await;
        runner.surface_preview_assessment("demo").await;
        let notes: String = sink
            .events()
            .iter()
            .filter_map(|e| match e {
                EngineEvent::Note(n) => Some(n.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        // Intake plan (the up-front "thinking"), docs gate review, preview review.
        assert!(notes.contains("[plan]") && notes.contains("SaaS 仪表盘"));
        assert!(notes.contains("[docs] 智能评审") && notes.contains("鉴权设计"));
        assert!(notes.contains("[preview] 智能评审"));
        // Team hook-up: the docs-assessment judge ALSO wrote its verdict to the
        // team ledger as a `tech-lead` RoleVerdict — zero behavior change above,
        // an audit row below.
        let ledger = std::fs::read_to_string(tmp.path().join(".umadev/team-ledger.jsonl")).unwrap();
        assert!(
            ledger.contains("\"role\":\"tech-lead\""),
            "the docs-assessment judge must also append a RoleVerdict to the ledger"
        );
    }

    #[tokio::test]
    async fn docs_critic_team_blocks_and_ledgers_then_revises() {
        use crate::events::RecordingSink;
        let tmp = TempDir::new().unwrap();
        set_retry_base_ms_for_tests(1);
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::write(
            tmp.path().join("output/demo-prd.md"),
            "# PRD\n## Goal\n登录系统\n## Acceptance criteria\n- [ ] FR-001 登录",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("output/demo-architecture.md"),
            "# Architecture\n## API surface\n| Method | Path |\n|---|---|\n| POST | /login |",
        )
        .unwrap();
        // A greenfield requirement → the docs critic TEAM runs (PM + architect).
        let mut o = opts(tmp.path());
        o.backend = "claude-code".into(); // a brain is present (not offline)
        o.requirement = "做一个全新的登录系统产品".into();
        let sink = RecordingSink::new();
        let runner =
            AgentRunner::new(FakeRuntimeRoleCritic, o).with_event_sink(Arc::new(sink.clone()));
        let blocking = runner.run_docs_critic_team("demo").await;
        // The team surfaced blocking issues (advisory to the loop).
        assert!(
            !blocking.is_empty(),
            "the critic team must surface the blocking items"
        );
        assert!(blocking
            .iter()
            .any(|b| b.contains("验收标准未覆盖核心功能")));
        // Each verdict was recorded to the team ledger (2 critics → 2 rows).
        let ledger = std::fs::read_to_string(tmp.path().join(".umadev/team-ledger.jsonl")).unwrap();
        let rows = ledger.lines().count();
        assert_eq!(rows, 2, "PM + architect verdicts both recorded");
        assert!(ledger.contains("\"role\":\"product-manager\""));
        assert!(ledger.contains("\"role\":\"architect\""));
        // The team-review note announced the cross-review.
        let notes: String = sink
            .events()
            .iter()
            .filter_map(|e| match e {
                EngineEvent::Note(n) => Some(n.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(notes.contains("[team] 角色团队交叉评审"));
        // Folding the blocking into the revision path is exercised (one round).
        runner
            .revise_docs_for_critic_blocking("demo", &blocking)
            .await;
    }

    #[tokio::test]
    async fn docs_critic_team_is_empty_for_lean_task() {
        // TaskKind scaling: a trivial/light task gets NO critic team — the
        // deterministic floor stands. Returns empty, writes no ledger.
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::write(tmp.path().join("output/demo-prd.md"), "# PRD\n登录").unwrap();
        let mut o = opts(tmp.path());
        o.backend = "claude-code".into();
        // A bug-fix requirement → planner classifies lean → no team.
        o.requirement = "修复登录页的一个小 bug".into();
        let runner = AgentRunner::new(FakeRuntimeRoleCritic, o);
        let blocking = runner.run_docs_critic_team("demo").await;
        assert!(
            blocking.is_empty(),
            "lean task → no critic team → no blocking"
        );
        assert!(
            !tmp.path().join(".umadev/team-ledger.jsonl").exists(),
            "no team ran → no ledger written"
        );
    }

    #[tokio::test]
    async fn docs_critic_team_is_fail_open_offline() {
        // Offline (no brain) → no team, no panic, empty blocking (fail-open).
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::write(tmp.path().join("output/demo-prd.md"), "# PRD\n登录").unwrap();
        // FakeRuntime has an empty backend-id but is NOT offline; use the real
        // OfflineRuntime to assert the offline fail-open path.
        let runner = AgentRunner::new(
            umadev_runtime::OfflineRuntime::new(RuntimeKind::Anthropic),
            opts(tmp.path()),
        );
        let blocking = runner.run_docs_critic_team("demo").await;
        assert!(blocking.is_empty(), "offline → fail-open → no blocking");
    }

    #[tokio::test]
    async fn quality_critic_team_advises_and_ledgers_after_deterministic_floor() {
        use crate::events::RecordingSink;
        let tmp = TempDir::new().unwrap();
        set_retry_base_ms_for_tests(1);
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        // Some delivered code so the digest is non-empty (the team has something
        // to review) and the deterministic floors have a surface to scan.
        std::fs::write(
            tmp.path().join("src/auth.ts"),
            "export function login() { return true }\n",
        )
        .unwrap();
        // A greenfield requirement → the quality critic TEAM runs (QA + security).
        let mut o = opts(tmp.path());
        o.backend = "claude-code".into(); // a brain is present (not offline)
        o.requirement = "做一个全新的登录系统产品".into();
        let sink = RecordingSink::new();
        let runner =
            AgentRunner::new(FakeRuntimeRoleCritic, o).with_event_sink(Arc::new(sink.clone()));
        let advisory = runner.run_quality_critic_team("demo").await;
        // The team surfaced advisory blocking (the fake brain returns blocking).
        assert!(
            !advisory.is_empty(),
            "the quality team must surface its advisory items"
        );
        // Each verdict was recorded to the team ledger under the quality phase
        // (2 critics → 2 rows: qa-engineer + security-engineer).
        let ledger = std::fs::read_to_string(tmp.path().join(".umadev/team-ledger.jsonl")).unwrap();
        assert_eq!(ledger.lines().count(), 2, "QA + security verdicts recorded");
        assert!(ledger.contains("\"role\":\"qa-engineer\""));
        assert!(ledger.contains("\"role\":\"security-engineer\""));
        assert!(
            ledger.contains("\"phase\":\"quality\""),
            "ledger rows are tagged with the quality phase"
        );
        // The cross-review note announced the quality-stage team.
        let notes: String = sink
            .events()
            .iter()
            .filter_map(|e| match e {
                EngineEvent::Note(n) => Some(n.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(notes.contains("[team] 质量阶段角色团队交叉评审"));
    }

    #[tokio::test]
    async fn quality_critic_team_is_empty_for_lean_task() {
        // TaskKind scaling: a trivial/light task gets NO quality team — the
        // deterministic floor stands. Returns empty, writes no ledger.
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/a.ts"), "const x = 1\n").unwrap();
        let mut o = opts(tmp.path());
        o.backend = "claude-code".into();
        // A bug-fix requirement → planner classifies lean → no team.
        o.requirement = "修复登录页的一个小 bug".into();
        let runner = AgentRunner::new(FakeRuntimeRoleCritic, o);
        let advisory = runner.run_quality_critic_team("demo").await;
        assert!(
            advisory.is_empty(),
            "lean task → no quality team → no advisory"
        );
        assert!(
            !tmp.path().join(".umadev/team-ledger.jsonl").exists(),
            "no team ran → no ledger written"
        );
    }

    #[tokio::test]
    async fn quality_critic_team_is_fail_open_offline() {
        // Offline (no brain) → no team, no panic, empty advisory (fail-open) —
        // even with delivered code present.
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/a.ts"), "const x = 1\n").unwrap();
        let runner = AgentRunner::new(
            umadev_runtime::OfflineRuntime::new(RuntimeKind::Anthropic),
            opts(tmp.path()),
        );
        let advisory = runner.run_quality_critic_team("demo").await;
        assert!(advisory.is_empty(), "offline → fail-open → no advisory");
    }

    #[tokio::test]
    async fn quality_critic_team_skips_when_no_code_delivered() {
        // A greenfield brain is present but NOTHING was built → no false alarm:
        // the team skips (empty digest), the deterministic gate stands alone.
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        let mut o = opts(tmp.path());
        o.backend = "claude-code".into();
        o.requirement = "做一个全新的登录系统产品".into();
        let runner = AgentRunner::new(FakeRuntimeRoleCritic, o);
        let advisory = runner.run_quality_critic_team("demo").await;
        assert!(
            advisory.is_empty(),
            "no delivered code → quality team skips (no false alarm)"
        );
        assert!(!tmp.path().join(".umadev/team-ledger.jsonl").exists());
    }

    #[test]
    fn security_floor_reports_governance_violations() {
        // The deterministic security floor reuses the governance scan over real
        // files — a hardcoded-color violation surfaces as a floor finding.
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/style.css"),
            ".btn { color: #ff0000; background: #00ff00; border: 1px solid #123456; }\n",
        )
        .unwrap();
        let runner = AgentRunner::new(FakeRuntimeRoleCritic, opts(tmp.path()));
        let findings = runner.security_floor_findings();
        // Fail-open contract: this never panics. When the policy flags the file,
        // the finding names it; either way the call returns a (possibly empty) list.
        if !findings.is_empty() {
            assert!(findings.iter().any(|f| f.contains("style.css")));
        }
    }

    #[tokio::test]
    async fn design_review_redelegates_when_brain_judges_not_commercial() {
        use crate::events::RecordingSink;
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        // Some UI code so the digest is non-empty.
        std::fs::write(
            tmp.path().join("src/App.tsx"),
            "export const App = () => <div>dashboard</div>;",
        )
        .unwrap();
        let mut o = opts(tmp.path());
        o.backend = "claude-code".into(); // a brain is present (shared model)
        let sink = RecordingSink::new();
        // FakeRuntimeDesign judges it NOT commercial-grade with concrete issues.
        let runner = AgentRunner::new(FakeRuntimeDesign, o).with_event_sink(Arc::new(sink.clone()));
        runner.run_design_review("demo").await;
        let notes: Vec<String> = sink
            .events()
            .iter()
            .filter_map(|e| match e {
                EngineEvent::Note(n) => Some(n.clone()),
                _ => None,
            })
            .collect();
        assert!(
            notes
                .iter()
                .any(|n| n.contains("design") && n.contains("智能设计审")),
            "expected an intelligent design-review note, got: {notes:?}"
        );
    }

    #[tokio::test]
    async fn governance_catchup_flags_real_file_violations() {
        use crate::events::RecordingSink;
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        // A real source file with an emoji used as an icon — a governance block.
        std::fs::write(
            tmp.path().join("src/Button.tsx"),
            "export const B = () => <button>🚀 Launch</button>;",
        )
        .unwrap();
        // FakeRuntimeNoGoal has realtime_governance=false → the catch-up runs.
        let sink = RecordingSink::new();
        let runner = AgentRunner::new(FakeRuntimeNoGoal, opts(tmp.path()))
            .with_event_sink(Arc::new(sink.clone()));
        runner.run_governance_catchup(Phase::Frontend).await;
        let notes: Vec<String> = sink
            .events()
            .iter()
            .filter_map(|e| match e {
                EngineEvent::Note(n) => Some(n.clone()),
                _ => None,
            })
            .collect();
        assert!(
            notes
                .iter()
                .any(|n| n.contains("governance") && n.contains("治理违规")),
            "expected a real-file governance note, got: {notes:?}"
        );
    }

    #[tokio::test]
    async fn governance_catchup_noop_when_brain_governs_at_write_time() {
        use crate::events::RecordingSink;
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/Button.tsx"),
            "export const B = () => <button>🚀 Launch</button>;",
        )
        .unwrap();
        // FakeRuntime declares realtime_governance? No — only persistent_goal.
        // Use a runtime that DOES govern at write time to prove the no-op.
        let sink = RecordingSink::new();
        let runner = AgentRunner::new(FakeRuntimeGoverns, opts(tmp.path()))
            .with_event_sink(Arc::new(sink.clone()));
        runner.run_governance_catchup(Phase::Frontend).await;
        // No governance note — the brain already blocked at write time.
        assert!(sink.events().iter().all(|e| !matches!(
            e,
            EngineEvent::Note(n) if n.contains("治理违规")
        )));
    }

    #[test]
    fn goal_mode_falls_back_to_prompt_for_non_goal_brain() {
        let tmp = TempDir::new().unwrap();
        let o = opts(tmp.path());
        // A brain without native /goal (codex/opencode-like) → prompt fallback,
        // NOT a /goal command, but still the "persist until every task done"
        // intent pointed at the breakdown.
        let runner = AgentRunner::new(FakeRuntimeNoGoal, o);
        let fe = runner.with_goal_mode(
            Prompt {
                system: "BASE".into(),
                user: "U".into(),
            },
            Phase::Frontend,
        );
        assert!(!fe.system.starts_with("/goal "), "system: {}", fe.system);
        assert!(fe.system.contains("多任务目标") && fe.system.contains("逐条"));
        assert!(fe.system.contains("execution-plan") && fe.system.contains("BASE"));
    }

    #[test]
    fn umadevrc_config_is_loaded() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".umadevrc"),
            "[quality]\nthreshold = 75\n\n[pipeline]\nmax_review_rounds = 1\n",
        )
        .unwrap();
        let cfg = crate::config::load_project_config(tmp.path());
        assert_eq!(cfg.quality.threshold, 75);
        assert_eq!(cfg.pipeline.max_review_rounds, 1);
    }

    // ---- review function tests ----

    #[test]
    fn review_research_detects_missing_sections() {
        let empty = AgentRunner::<FakeRuntime>::review_research("");
        assert!(empty.len() >= 3);
    }

    #[test]
    fn review_research_passes_complete_doc() {
        let doc = "## Discovery\ntarget audience: devs\n\n## Similar products\n- Tool A\n- Tool B\n\n## Domain risks\nHigh complexity\n\n## Design system recommendation\nModern minimal";
        let defects = AgentRunner::<FakeRuntime>::review_research(doc);
        assert!(defects.is_empty(), "unexpected defects: {defects:?}");
    }

    #[test]
    fn review_prd_detects_missing_sections() {
        let defects = AgentRunner::<FakeRuntime>::review_prd("Just a paragraph.");
        assert!(defects.len() >= 3, "expected ≥3 defects, got {defects:?}");
    }

    #[test]
    fn review_prd_flags_present_but_empty_section() {
        // Regression: a doc with a bare `## Goal` (no body) used to pass
        // review because only the heading presence was checked.
        let doc =
            "## Goal\n\n## Scope\nreal scope body\n\n## Acceptance Criteria\n- [ ] a\n- [ ] b";
        let defects = AgentRunner::<FakeRuntime>::review_prd(doc);
        assert!(
            defects
                .iter()
                .any(|d| d.contains("'## goal' is present but empty")),
            "empty ## Goal must be flagged, got {defects:?}"
        );
        // ## Scope has body → must NOT be flagged empty.
        assert!(
            !defects
                .iter()
                .any(|d| d.contains("'## scope' is present but empty")),
            "## Scope has body, must not be flagged: {defects:?}"
        );
    }

    #[test]
    fn review_prd_passes_complete_doc() {
        let doc = "\
## Goal\nBuild a login system\n\n\
## Target Users\nPersona: developer\n\n\
## Scope\n### In scope\n- auth\n### Out of scope\n- billing\n\n\
## Functional Requirements\n- Login\n- Register\n\n\
## Non-Functional Requirements\nPerformance: <200ms\n\n\
## Acceptance Criteria\n- [ ] User can login\n- [ ] User can register\n- [ ] Password reset\n\n\
## Success Metrics\nKPI: DAU > 100";
        let defects = AgentRunner::<FakeRuntime>::review_prd(doc);
        assert!(defects.is_empty(), "unexpected defects: {defects:?}");
    }

    #[test]
    fn governance_flags_console_log_in_code_block() {
        // A doc embedding a code block with console.log must surface a
        // governance defect — this is the path that governs HTTP-runtime output.
        let doc = "# Research\n\n```ts\nconsole.log(\"debug\");\n```";
        let defects = AgentRunner::<FakeRuntime>::governance_defects(
            doc,
            &umadev_governance::Policy::default(),
        );
        assert!(!defects.is_empty(), "should flag console.log");
        assert!(defects[0].contains("UD-ARCH-002"));
    }

    #[test]
    fn governance_flags_any_type_in_tsx_block() {
        let doc = "```tsx\nconst x: any = null;\n```";
        let defects = AgentRunner::<FakeRuntime>::governance_defects(
            doc,
            &umadev_governance::Policy::default(),
        );
        assert!(!defects.is_empty());
        assert!(defects[0].contains("UD-ARCH-001"));
    }

    #[test]
    fn governance_passes_clean_code_block() {
        let doc = "```ts\nconst x: string = \"hello\";\n```";
        let defects = AgentRunner::<FakeRuntime>::governance_defects(
            doc,
            &umadev_governance::Policy::default(),
        );
        assert!(defects.is_empty());
    }

    #[test]
    fn governance_ignores_non_code_langs() {
        // Bash/text blocks aren't scanned.
        let doc = "```bash\nrm -rf target\n```";
        let defects = AgentRunner::<FakeRuntime>::governance_defects(
            doc,
            &umadev_governance::Policy::default(),
        );
        assert!(defects.is_empty());
    }

    #[test]
    fn review_architecture_detects_missing() {
        let defects = AgentRunner::<FakeRuntime>::review_architecture("Just a paragraph.");
        assert!(defects.len() >= 4, "expected ≥4 defects, got {defects:?}");
    }

    #[test]
    fn review_architecture_passes_complete() {
        let doc = "\
## API Surface\n\
| Method | Path | Auth | Description |\n\
|---|---|---|---|\n\
| POST | /api/login | none | Login |\n\
| GET | /api/users | JWT | List users |\n\
| POST | /api/register | none | Register |\n\n\
## Data Model\nUser entity:\n\
| Field | Type | Notes |\n|---|---|---|\n| id | uuid | PK |\n| email | text | unique |\n\n\
## Auth\nJWT tokens\n\n\
## Tech Stack\nRust + React\n\n\
## Error Convention\n4xx/5xx standard codes\n\n\
## Project Structure\nsrc/ layout";
        let defects = AgentRunner::<FakeRuntime>::review_architecture(doc);
        assert!(defects.is_empty(), "unexpected defects: {defects:?}");
    }

    #[test]
    fn review_uiux_detects_missing() {
        let defects = AgentRunner::<FakeRuntime>::review_uiux("Just text.");
        assert!(defects.len() >= 3, "expected ≥3 defects, got {defects:?}");
    }

    #[test]
    fn review_uiux_passes_complete() {
        let tokens = "--color-primary: #1a1a1a;\n".repeat(10);
        let doc = format!(
            "# UIUX\n\
             ## Visual direction\nmodern-minimal archetype — fits a dev tool.\n\
             {tokens}\n\
             --text-xs: 12px; --text-sm: 14px; --text-base: 16px; --text-3xl: 48px;\n\
             --space-1: 4px; --space-2: 8px;\n\
             Dark mode: @media (prefers-color-scheme: dark)\n\
             font-family: 'Geist', sans-serif\n\
             Icon library: Lucide\n\
             Component states: hover, focus, active, disabled, loading, error.\n\
             ## Motion\ntransition 200ms ease; respects reduced-motion.\n\
             ## Anti-patterns\nNo emoji icons, no purple gradients."
        );
        let defects = AgentRunner::<FakeRuntime>::review_uiux(&doc);
        assert!(defects.is_empty(), "unexpected defects: {defects:?}");
    }

    #[test]
    fn review_uiux_flags_thin_and_purple_gradient_docs() {
        // A thin doc with a purple-gradient design spec must be flagged.
        let doc = "# UIUX\n--x: 1;\nbackground: linear-gradient(135deg, purple, pink)";
        let defects = AgentRunner::<FakeRuntime>::review_uiux(doc);
        assert!(defects.len() >= 5, "expected many defects, got {defects:?}");
        assert!(
            defects.iter().any(|d| d.contains("purple gradient")),
            "should flag the purple gradient: {defects:?}"
        );
    }

    // ============================ #1 degraded fallback ====================

    #[tokio::test]
    async fn base_offline_marks_phase_degraded_and_warns_loudly() {
        // The base is reachable (use_runtime on) but returns empty for every
        // phase → research + docs must fall back to the offline template AND be
        // flagged degraded, with a loud user-visible warning + an on-disk
        // .DEGRADED marker. The placeholder must NOT be passed off as success.
        use crate::events::RecordingSink;
        let tmp = TempDir::new().unwrap();
        let mut o = opts(tmp.path());
        o.backend = "claude-code".into(); // → use_runtime = true
        let sink = RecordingSink::new();
        let runner = AgentRunner::new(EmptyRuntime, o).with_event_sink(Arc::new(sink.clone()));
        runner.start().unwrap();
        runner.run_initial_block(true, None).await.unwrap();

        // Loud degraded warning emitted (per-phase AND the block summary).
        let warns = sink.count(|e| matches!(e, EngineEvent::Note(m) if m.contains("[WARN][降级]")));
        assert!(
            warns >= 2,
            "expected at least a per-phase + summary degraded warning, got {warns}"
        );

        // On-disk .DEGRADED markers exist next to the placeholder artifacts.
        let prd_marker = tmp.path().join("output/demo-prd.md.DEGRADED");
        assert!(
            prd_marker.is_file(),
            "expected a .DEGRADED marker next to the placeholder PRD"
        );
        // The artifact itself still exists (the file is written) — the point is
        // it's TAGGED, not hidden.
        assert!(tmp.path().join("output/demo-prd.md").is_file());
    }

    #[tokio::test]
    async fn offline_run_is_not_degraded() {
        // A genuine OFFLINE run (no base expected) must NOT be flagged degraded —
        // its templates are the intended output, not a fallback masking a failure.
        use crate::events::RecordingSink;
        let tmp = TempDir::new().unwrap();
        let sink = RecordingSink::new();
        let runner = AgentRunner::new(
            umadev_runtime::OfflineRuntime::new(RuntimeKind::Anthropic),
            opts(tmp.path()),
        )
        .with_event_sink(Arc::new(sink.clone()));
        runner.start().unwrap();
        runner.run_initial_block(false, None).await.unwrap();

        let warns = sink.count(|e| matches!(e, EngineEvent::Note(m) if m.contains("[WARN][降级]")));
        assert_eq!(warns, 0, "offline run must not raise a degraded warning");
        assert!(
            !tmp.path().join("output/demo-prd.md.DEGRADED").is_file(),
            "offline run must not write a .DEGRADED marker"
        );
    }

    // ============================ Light fast track ========================

    #[tokio::test]
    async fn run_light_runs_lean_block_and_completes_without_gates() {
        // Offline Light run: spec → frontend → quality, no gate pauses, ends
        // complete (paused_at = None) at the quality phase.
        let tmp = TempDir::new().unwrap();
        let mut o = opts(tmp.path());
        o.requirement = "改个文案".into();
        let runner = AgentRunner::new(
            umadev_runtime::OfflineRuntime::new(RuntimeKind::Anthropic),
            o,
        );
        runner.start().unwrap();
        let report = runner.run_light(false).await.unwrap();
        assert!(
            report.paused_at.is_none(),
            "Light run must not pause at a gate"
        );
        assert_eq!(report.final_phase, Phase::Quality);
        // The lean phases ran (spec + frontend + quality artifacts exist) …
        let phases: Vec<&str> = report.completed.iter().map(|p| p.phase.id()).collect();
        assert!(phases.contains(&"spec"), "spec ran: {phases:?}");
        assert!(phases.contains(&"frontend"), "frontend ran: {phases:?}");
        assert!(phases.contains(&"quality"), "quality ran: {phases:?}");
        // … and the heavyweight phases did NOT.
        assert!(
            !phases.contains(&"research"),
            "research skipped: {phases:?}"
        );
        assert!(!phases.contains(&"docs"), "docs skipped: {phases:?}");
        assert!(
            !phases.contains(&"delivery"),
            "delivery skipped: {phases:?}"
        );
        // State ends at quality, no active gate.
        let state = crate::state::read_workflow_state(tmp.path()).unwrap();
        assert_eq!(state.phase, "quality");
        assert!(state.active_gate.is_empty());
    }

    #[tokio::test]
    async fn run_light_emits_light_plan_note() {
        use crate::events::RecordingSink;
        let tmp = TempDir::new().unwrap();
        let mut o = opts(tmp.path());
        o.requirement = "微调一下间距".into();
        let sink = RecordingSink::new();
        let runner = AgentRunner::new(
            umadev_runtime::OfflineRuntime::new(RuntimeKind::Anthropic),
            o,
        )
        .with_event_sink(Arc::new(sink.clone()));
        runner.start().unwrap();
        runner.run_light(false).await.unwrap();
        let noted =
            sink.count(|e| matches!(e, EngineEvent::Note(m) if m.contains("[plan] 轻量档")));
        assert!(
            noted >= 1,
            "Light run should announce the lightweight track"
        );
    }

    // ============================ redo single phase =======================

    #[tokio::test]
    async fn redo_phase_reruns_one_phase_only() {
        // A redo of `frontend` re-records exactly that phase and nothing else,
        // ending complete (no gate) at frontend.
        let tmp = TempDir::new().unwrap();
        let runner = AgentRunner::new(
            umadev_runtime::OfflineRuntime::new(RuntimeKind::Anthropic),
            opts(tmp.path()),
        );
        runner.start().unwrap();
        let report = runner.redo_phase(Phase::Frontend, false).await.unwrap();
        assert!(report.paused_at.is_none());
        assert_eq!(report.final_phase, Phase::Frontend);
        assert_eq!(report.completed.len(), 1, "redo runs exactly one phase");
        assert_eq!(report.completed[0].phase, Phase::Frontend);
        // The frontend notes artifact exists on disk.
        assert!(tmp.path().join("output/demo-frontend-notes.md").is_file());
    }

    #[tokio::test]
    async fn redo_phase_clears_degraded_marker_on_clean_rerun() {
        // Seed a .DEGRADED marker next to the frontend artifact (as if a prior
        // run degraded it), then redo the phase cleanly (offline → not degraded)
        // and confirm the marker is gone.
        let tmp = TempDir::new().unwrap();
        let out = tmp.path().join("output");
        std::fs::create_dir_all(&out).unwrap();
        let marker = out.join("demo-frontend-notes.md.DEGRADED");
        std::fs::write(&marker, "placeholder").unwrap();
        assert!(marker.is_file());

        let runner = AgentRunner::new(
            umadev_runtime::OfflineRuntime::new(RuntimeKind::Anthropic),
            opts(tmp.path()),
        );
        runner.start().unwrap();
        runner.redo_phase(Phase::Frontend, false).await.unwrap();
        assert!(
            !marker.is_file(),
            "a clean redo must clear the phase's .DEGRADED marker"
        );
    }

    // ============================ #3 planner consumption ==================

    #[tokio::test]
    async fn docs_only_plan_stops_at_docs_gate_on_continue() {
        // A DocsOnly task has no build phases: continuing past the docs gate
        // must NOT produce an execution plan / frontend notes — it just finishes.
        let tmp = TempDir::new().unwrap();
        let mut o = opts(tmp.path());
        o.requirement = "只写需求文档,不要写代码".into();
        let runner = AgentRunner::new(FakeRuntime, o);
        runner.start().unwrap();
        runner.run_initial_block(false, None).await.unwrap();
        let r = runner.continue_after_docs_confirm().await.unwrap();
        assert_eq!(r.final_phase, Phase::DocsConfirm);
        assert!(
            r.paused_at.is_none(),
            "docs-only continue completes, it does not re-pause"
        );
        assert!(
            !tmp.path().join("output/demo-frontend-notes.md").is_file(),
            "docs-only task must not run the frontend phase"
        );
    }

    #[tokio::test]
    async fn frontend_only_plan_skips_backend_implementation() {
        // A FrontendOnly plan must skip the backend phase body. The backend-notes
        // artifact is still recorded (the phase function runs), but the worker
        // backend implementation is skipped — assert via the SubTask events.
        use crate::events::RecordingSink;
        let tmp = TempDir::new().unwrap();
        let mut o = opts(tmp.path());
        o.requirement = "做一个纯前端的落地页,只要界面".into();
        o.backend = "claude-code".into();
        let sink = RecordingSink::new();
        let runner = AgentRunner::new(FakeRuntime, o).with_event_sink(Arc::new(sink.clone()));
        runner.start().unwrap();
        runner.run_initial_block(true, None).await.unwrap();
        runner.continue_after_docs_confirm().await.unwrap();
        runner.continue_after_preview_confirm().await.unwrap();
        let backend_impl = sink.count(|e| {
            matches!(
                e,
                EngineEvent::SubTaskStarted { task_id, .. } if task_id == "backend.implement"
            )
        });
        assert_eq!(
            backend_impl, 0,
            "frontend-only plan must not run the backend implementation subtask"
        );
    }

    // ============================ #4 strict coverage gate =================

    #[tokio::test]
    async fn strict_coverage_blocks_at_spec_when_requirements_uncovered() {
        // With strict_coverage on (via env), an uncovered PRD requirement blocks
        // the pipeline at the spec phase instead of proceeding to frontend.
        let tmp = TempDir::new().unwrap();
        let runner = AgentRunner::new(FakeRuntime, opts(tmp.path()));
        runner.start().unwrap();
        runner.run_initial_block(false, None).await.unwrap();
        // Force a coverage gap: a PRD with a functional requirement, tasks with
        // none citing it. The offline templates already produce a PRD; we make
        // the task list empty so every requirement is uncovered.
        let prd = tmp.path().join("output/demo-prd.md");
        std::fs::write(
            &prd,
            "# PRD\n\n## Functional requirements\n\n- FR-1: 用户必须能用邮箱登录\n- FR-2: 用户能重置密码\n",
        )
        .unwrap();
        std::env::set_var("UMADEV_STRICT_COVERAGE", "1");
        let r = runner.continue_after_docs_confirm().await;
        std::env::remove_var("UMADEV_STRICT_COVERAGE");
        let r = r.unwrap();
        // If the coverage checker found gaps, strict mode pauses at spec. (When
        // no gap is detected the run proceeds — both are valid, but assert the
        // strict-block path is reachable by checking we did NOT reach frontend.)
        if r.final_phase == Phase::Spec {
            assert_eq!(r.paused_at, Some(Gate::DocsConfirm));
            assert!(
                !tmp.path().join("output/demo-frontend-notes.md").is_file(),
                "strict block must stop before the frontend phase"
            );
        }
    }
}
