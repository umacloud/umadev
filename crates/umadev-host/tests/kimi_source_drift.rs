use std::path::{Path, PathBuf};

use umadev_host::kimi_contract::{
    KIMI_CODE_AUDITED_BASELINE_ACP_VERSION, KIMI_CODE_AUDITED_BASELINE_ADAPTER_VERSION,
    KIMI_CODE_AUDITED_BASELINE_COMMIT, KIMI_CODE_AUDITED_BASELINE_VERSION,
};

fn source_root() -> Option<PathBuf> {
    std::env::var_os("UMADEV_KIMI_SOURCE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn read(root: &Path, relative: &str) -> String {
    std::fs::read_to_string(root.join(relative))
        .unwrap_or_else(|error| panic!("read audited Kimi source {relative}: {error}"))
}

fn source_head(root: &Path) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap_or_else(|error| panic!("read audited Kimi source commit: {error}"));
    assert!(
        output.status.success(),
        "git rev-parse failed for audited Kimi source: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("audited Kimi source commit is UTF-8")
        .trim()
        .to_string()
}

fn assert_markers(source: &str, contract: &str, markers: &[&str]) {
    for marker in markers {
        assert!(
            source.contains(marker),
            "missing audited {contract} marker {marker}"
        );
    }
}

#[test]
fn audited_kimi_baseline_matches_the_standard_acp_contract() {
    let Some(root) = source_root() else {
        eprintln!("skipped: UMADEV_KIMI_SOURCE_DIR is set by the source-contract CI job");
        return;
    };
    assert_eq!(source_head(&root), KIMI_CODE_AUDITED_BASELINE_COMMIT);

    let app_manifest = read(&root, "apps/kimi-code/package.json");
    assert!(app_manifest.contains(&format!(
        "\"version\": \"{KIMI_CODE_AUDITED_BASELINE_VERSION}\""
    )));
    let adapter_manifest = read(&root, "packages/acp-adapter/package.json");
    assert!(adapter_manifest.contains(&format!(
        "\"version\": \"{KIMI_CODE_AUDITED_BASELINE_ADAPTER_VERSION}\""
    )));
    assert!(adapter_manifest.contains(&format!(
        "\"@agentclientprotocol/sdk\": \"^{KIMI_CODE_AUDITED_BASELINE_ACP_VERSION}\""
    )));

    let server = read(&root, "packages/acp-adapter/src/server.ts");
    assert_markers(
        &server,
        "ACP lifecycle",
        &[
            "loadSession: true",
            "image: true",
            "embeddedContext: true",
            "mcpCapabilities:",
            "sessionCapabilities:",
            "list: {}",
            "resume: {}",
            "async newSession",
            "async loadSession",
            "async resumeSession",
            "async listSessions",
            "const cwd = params.cwd ?? undefined",
            "cwd === undefined ? {} : { workDir: cwd }",
            "sessionSummaryToSessionInfo(summary)",
            "sessionId: summary.id",
            "cwd: summary.workDir",
            "updatedAt",
            "async setSessionConfigOption",
            "await acpSession.setThinking(String(value))",
            "async authenticate",
            "async cancel",
            "RequestError.authRequired()",
        ],
    );

    let config_options = read(&root, "packages/acp-adapter/src/config-options.ts");
    assert_markers(
        &config_options,
        "model, thinking, and mode configuration",
        &[
            "id: 'model'",
            "id: 'thinking'",
            "category: 'thought_level'",
            "currentThinkingEffort: string",
            "export function buildThinkingOption",
            "const efforts = supportEfforts.filter",
            "currentValue: alwaysThinking || currentEffort !== 'off' ? 'on' : 'off'",
            "const values = alwaysThinking ? [...efforts] : ['off', ...efforts]",
            "effortDisplayName(value)",
            "id: 'mode'",
            "buildSessionConfigOptions",
        ],
    );

    let modes = read(&root, "packages/acp-adapter/src/modes.ts");
    assert_markers(
        &modes,
        "permission modes",
        &[
            "id: 'default'",
            "id: 'plan'",
            "id: 'auto'",
            "id: 'yolo'",
            "permission: 'manual'",
            "permission: 'auto'",
            "permission: 'yolo'",
        ],
    );

    let session = read(&root, "packages/acp-adapter/src/session.ts");
    assert_markers(
        &session,
        "streaming and cancellation",
        &[
            "session.cancel()",
            "agent_message_chunk",
            "toolCallStartToSessionUpdate",
            "toolCallLazyCreateToSessionUpdate",
            "toolCallStartedUpgradeToSessionUpdate",
            "toolResultToSessionUpdate",
            "event.agentId !== MAIN_AGENT_ID",
            "turn.ended",
            "handleQuestion",
            "questionItemToPermissionOptions(q, 0)",
            "title: 'AskUserQuestion'",
            "detectLeadingSlashIntent(blocks, this.skillCommandMap)",
        ],
    );

    let builtin_commands = read(&root, "packages/acp-adapter/src/builtin-commands.ts");
    assert_markers(
        &builtin_commands,
        "native command catalog",
        &[
            "name: 'compact'",
            "name: 'status'",
            "name: 'usage'",
            "name: 'mcp'",
            "name: 'tasks'",
            "name: 'help'",
        ],
    );

    let question = read(&root, "packages/acp-adapter/src/question.ts");
    assert_markers(
        &question,
        "AskUserQuestion permission bridge",
        &[
            "`q${questionIndex}_opt_${optionIndex}`",
            "`q${questionIndex}_skip`",
            "question.options.map((opt, i)",
            "kind: 'allow_once'",
            "name: 'Skip'",
            "kind: 'reject_once'",
            "optionId === skipOptionId(0)",
        ],
    );

    let event_mapping = read(&root, "packages/acp-adapter/src/events-map.ts");
    assert_markers(
        &event_mapping,
        "stream, plan, and turn terminal mapping",
        &[
            "sessionUpdate: 'plan'",
            "planFromDisplayBlock",
            "status: mapTodoStatus(item.status)",
            "toolCallLazyCreateToSessionUpdate",
            "toolCallStartedUpgradeToSessionUpdate",
            "rawInput: event.args",
            "event.update.kind === 'status'",
            "title: event.update.text",
            "case 'cancelled':",
            "if (error?.code === 'provider.filtered') return 'refusal'",
            "case 'blocked':",
            "return 'end_turn'",
        ],
    );

    let content_conversion = read(&root, "packages/acp-adapter/src/convert.ts");
    assert_markers(
        &content_conversion,
        "embedded-resource conversion",
        &[
            "if ('text' in resource)",
            "acp: dropping blob embedded resource",
            "acp: dropping unsupported prompt content block",
        ],
    );

    let approval = read(&root, "packages/acp-adapter/src/approval.ts");
    assert_markers(
        &approval,
        "approval mapping",
        &[
            "approve_once",
            "name: 'Approve once'",
            "approve_always",
            "name: 'Approve for this session'",
            "reject_once",
            "name: 'Reject'",
            "decision: 'cancelled'",
            "scope: 'session'",
            "PLAN_APPROVE_OPTION_ID = 'plan_approve'",
            "PLAN_REVISE_OPTION_ID = 'plan_revise'",
            "PLAN_REJECT_AND_EXIT_OPTION_ID = 'plan_reject_and_exit'",
            "return `plan_opt_${i}`",
            "display.options.map((opt, i)",
            "selectedLabel: opts[i]!.label",
        ],
    );

    let command = read(&root, "apps/kimi-code/src/cli/sub/acp.ts");
    assert_markers(
        &command,
        "stdio entrypoint",
        &[
            "command('acp')",
            "runAcpServer",
            "uiMode: 'acp'",
            "--login",
            "session.listSkills()",
            "buildSkillSlashCommands(skills)",
            "skillCommandMap: built.commandMap",
        ],
    );

    let skill_commands = read(&root, "apps/kimi-code/src/tui/commands/skills.ts");
    assert_markers(
        &skill_commands,
        "native skill command catalog",
        &[
            "export function buildSkillSlashCommands",
            "? skill.name",
            ": `skill:${skill.name}`",
            "commandMap.set(commandName, skill.name)",
        ],
    );

    let skill_roots = read(
        &root,
        "packages/agent-core-v2/src/app/skillCatalog/skillRoots.ts",
    );
    assert_markers(
        &skill_roots,
        "native project skill discovery",
        &[
            "const PROJECT_BRAND_DIRS = ['.kimi-code/skills']",
            "const PROJECT_GENERIC_DIRS = ['.agents/skills']",
            "const projectRoot = await findProjectRoot(workDir)",
        ],
    );

    let mcp = read(&root, "packages/agent-core/src/mcp/config-loader.ts");
    assert_markers(
        &mcp,
        "project MCP isolation",
        &[
            "mcp.json",
            "mcpServers",
            "projectRoot",
            "normalizeMcpServers",
        ],
    );

    let environment = read(&root, "packages/kaos/src/environment.ts");
    assert_markers(
        &environment,
        "Windows Git Bash preflight",
        &[
            "KIMI_SHELL_PATH",
            "C:\\\\Program Files\\\\Git\\\\bin\\\\bash.exe",
            "usr\\\\bin\\\\bash.exe",
            "LOCALAPPDATA",
            "Git Bash was not found",
        ],
    );

    let hook_config = read(
        &root,
        "packages/agent-core-v2/src/agent/externalHooks/configSection.ts",
    );
    assert_markers(
        &hook_config,
        "native hook config",
        &[
            "[[hooks]]",
            "event: z.enum(HOOK_EVENT_TYPES)",
            "matcher: z.string().optional()",
            "command: z.string().min(1)",
            "timeout: z.number().int().min(1).max(600)",
        ],
    );
    let hook_runtime = read(
        &root,
        "packages/agent-core-v2/src/agent/externalHooks/externalHooksService.ts",
    );
    assert_markers(
        &hook_runtime,
        "native Pre/PostToolUse lifecycle",
        &[
            "toolExecutor.onBeforeExecuteTool(async (event) =>",
            "event.veto(denyToolExecution(reason))",
            "onDidExecuteTool.register('externalHooks'",
            "toolInput",
            "toolCallId",
        ],
    );
    let hook_runner = read(
        &root,
        "packages/agent-core-v2/src/agent/externalHooks/runner.ts",
    );
    assert_markers(
        &hook_runner,
        "native hook decision protocol",
        &[
            "if (exitCode === 2)",
            "hookSpecificOutput?.permissionDecision !== 'deny'",
            "hookSpecificOutput.permissionDecisionReason",
        ],
    );
}
