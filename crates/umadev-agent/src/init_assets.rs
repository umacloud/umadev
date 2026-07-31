//! Knowledge assets installed by project-aware initialization.

use std::path::Path;

/// Outcome of installing the bundled initialization knowledge set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnowledgeScaffoldReport {
    /// Files newly created during this invocation.
    pub created: usize,
    /// Files that already existed and were left untouched.
    pub preserved: usize,
    /// Files that could not be created.
    pub failed: usize,
    /// Total files in the canonical initialization set.
    pub total: usize,
}

/// Install the canonical knowledge set without replacing existing files.
pub fn scaffold_init_knowledge(workspace: &Path) -> KnowledgeScaffoldReport {
    let files: &[(&str, &str)] = &[
        (
            "knowledge/design-systems/modern-minimal.md",
            include_str!("../../../knowledge/design-systems/modern-minimal.md"),
        ),
        (
            "knowledge/design-systems/editorial-clean.md",
            include_str!("../../../knowledge/design-systems/editorial-clean.md"),
        ),
        (
            "knowledge/design-systems/tech-utility.md",
            include_str!("../../../knowledge/design-systems/tech-utility.md"),
        ),
        (
            "knowledge/design-systems/soft-warm.md",
            include_str!("../../../knowledge/design-systems/soft-warm.md"),
        ),
        (
            "knowledge/design-systems/bold-geometric.md",
            include_str!("../../../knowledge/design-systems/bold-geometric.md"),
        ),
        (
            "knowledge/design-systems/00-craft-rules.md",
            include_str!("../../../knowledge/design-systems/00-craft-rules.md"),
        ),
        (
            "knowledge/design-systems/anti-ai-slop.md",
            include_str!("../../../knowledge/design-systems/anti-ai-slop.md"),
        ),
        (
            "knowledge/design-systems/brutalist-bold.md",
            include_str!("../../../knowledge/design-systems/brutalist-bold.md"),
        ),
        (
            "knowledge/design-systems/glass-aurora.md",
            include_str!("../../../knowledge/design-systems/glass-aurora.md"),
        ),
        (
            "knowledge/design-systems/premium-luxury.md",
            include_str!("../../../knowledge/design-systems/premium-luxury.md"),
        ),
        (
            "knowledge/design-systems/product-type-design-map.md",
            include_str!("../../../knowledge/design-systems/product-type-design-map.md"),
        ),
        (
            "knowledge/design-systems/aesthetic-families.md",
            include_str!("../../../knowledge/design-systems/aesthetic-families.md"),
        ),
        (
            "knowledge/design-systems/design-system-deep-dive.md",
            include_str!("../../../knowledge/design-systems/design-system-deep-dive.md"),
        ),
        (
            "knowledge/seed-templates/saas-landing.md",
            include_str!("../../../knowledge/seed-templates/saas-landing.md"),
        ),
        (
            "knowledge/seed-templates/dashboard.md",
            include_str!("../../../knowledge/seed-templates/dashboard.md"),
        ),
        (
            "knowledge/seed-templates/blog-content.md",
            include_str!("../../../knowledge/seed-templates/blog-content.md"),
        ),
        (
            "knowledge/seed-templates/e-commerce.md",
            include_str!("../../../knowledge/seed-templates/e-commerce.md"),
        ),
        (
            "knowledge/seed-templates/auth-system.md",
            include_str!("../../../knowledge/seed-templates/auth-system.md"),
        ),
        (
            "knowledge/seed-templates/settings-page.md",
            include_str!("../../../knowledge/seed-templates/settings-page.md"),
        ),
        (
            "knowledge/seed-templates/docs-site.md",
            include_str!("../../../knowledge/seed-templates/docs-site.md"),
        ),
        // Expert methodology knowledge
        (
            "knowledge/experts/product-manager/methodology.md",
            include_str!("../../../knowledge/experts/product-manager/methodology.md"),
        ),
        (
            "knowledge/experts/architect/api-design.md",
            include_str!("../../../knowledge/experts/architect/api-design.md"),
        ),
        (
            "knowledge/experts/architect/security.md",
            include_str!("../../../knowledge/experts/architect/security.md"),
        ),
        (
            "knowledge/experts/frontend-lead/methodology.md",
            include_str!("../../../knowledge/experts/frontend-lead/methodology.md"),
        ),
        (
            "knowledge/experts/backend-lead/methodology.md",
            include_str!("../../../knowledge/experts/backend-lead/methodology.md"),
        ),
        (
            "knowledge/experts/qa-lead/test-strategy.md",
            include_str!("../../../knowledge/experts/qa-lead/test-strategy.md"),
        ),
        (
            "knowledge/experts/uiux-designer/methodology.md",
            include_str!("../../../knowledge/experts/uiux-designer/methodology.md"),
        ),
        (
            "knowledge/experts/devops/methodology.md",
            include_str!("../../../knowledge/experts/devops/methodology.md"),
        ),
        // Engineering-structure standards — how to layer, package, and write
        // the service layer for a commercial-grade codebase. Seeded so they get
        // BM25-indexed and injected into the backend / frontend phases.
        (
            "knowledge/backend/01-standards/application-layering-and-packaging.md",
            include_str!(
                "../../../knowledge/backend/01-standards/application-layering-and-packaging.md"
            ),
        ),
        (
            "knowledge/frontend/01-standards/frontend-architecture-and-layering.md",
            include_str!(
                "../../../knowledge/frontend/01-standards/frontend-architecture-and-layering.md"
            ),
        ),
        (
            "knowledge/backend/01-standards/api-and-error-conventions.md",
            include_str!("../../../knowledge/backend/01-standards/api-and-error-conventions.md"),
        ),
        (
            "knowledge/backend/01-standards/data-modeling-and-persistence.md",
            include_str!(
                "../../../knowledge/backend/01-standards/data-modeling-and-persistence.md"
            ),
        ),
        (
            "knowledge/backend/01-standards/config-and-observability.md",
            include_str!("../../../knowledge/backend/01-standards/config-and-observability.md"),
        ),
        (
            "knowledge/security/01-standards/secure-coding-baseline.md",
            include_str!("../../../knowledge/security/01-standards/secure-coding-baseline.md"),
        ),
        (
            "knowledge/testing/01-standards/test-strategy-and-layering.md",
            include_str!("../../../knowledge/testing/01-standards/test-strategy-and-layering.md"),
        ),
        (
            "knowledge/cicd/01-standards/deployment-and-delivery-standard.md",
            include_str!(
                "../../../knowledge/cicd/01-standards/deployment-and-delivery-standard.md"
            ),
        ),
        (
            "knowledge/performance/01-standards/performance-and-scalability.md",
            include_str!(
                "../../../knowledge/performance/01-standards/performance-and-scalability.md"
            ),
        ),
        // Core feature standards — auth & forms are in every commercial app and
        // the most security/UX-critical to get right.
        (
            "knowledge/backend/01-standards/auth-implementation.md",
            include_str!("../../../knowledge/backend/01-standards/auth-implementation.md"),
        ),
        (
            "knowledge/frontend/01-standards/forms-and-validation.md",
            include_str!("../../../knowledge/frontend/01-standards/forms-and-validation.md"),
        ),
        (
            "knowledge/backend/01-standards/payment-integration.md",
            include_str!("../../../knowledge/backend/01-standards/payment-integration.md"),
        ),
        (
            "knowledge/backend/01-standards/file-upload-and-storage.md",
            include_str!("../../../knowledge/backend/01-standards/file-upload-and-storage.md"),
        ),
        (
            "knowledge/backend/01-standards/background-jobs-and-async.md",
            include_str!("../../../knowledge/backend/01-standards/background-jobs-and-async.md"),
        ),
        (
            "knowledge/backend/01-standards/email-and-notifications.md",
            include_str!("../../../knowledge/backend/01-standards/email-and-notifications.md"),
        ),
        (
            "knowledge/backend/01-standards/search-and-filtering.md",
            include_str!("../../../knowledge/backend/01-standards/search-and-filtering.md"),
        ),
        (
            "knowledge/backend/01-standards/realtime-and-websocket.md",
            include_str!("../../../knowledge/backend/01-standards/realtime-and-websocket.md"),
        ),
        (
            "knowledge/frontend/01-standards/i18n-and-localization.md",
            include_str!("../../../knowledge/frontend/01-standards/i18n-and-localization.md"),
        ),
        (
            "knowledge/frontend/01-standards/accessibility-standard.md",
            include_str!("../../../knowledge/frontend/01-standards/accessibility-standard.md"),
        ),
        // Deep design assets — the full token architecture + complete a11y
        // spec. These are the backbone of premium, non-AI-looking UI.
        (
            "knowledge/frontend/01-standards/design-tokens-complete.md",
            include_str!("../../../knowledge/frontend/01-standards/design-tokens-complete.md"),
        ),
        (
            "knowledge/frontend/01-standards/accessibility-complete.md",
            include_str!("../../../knowledge/frontend/01-standards/accessibility-complete.md"),
        ),
        // Multi-platform standards —商业开发不只 web：移动/桌面/小程序/鸿蒙/跨平台。
        (
            "knowledge/cross-platform/01-standards/platform-selection-and-architecture.md",
            include_str!(
                "../../../knowledge/cross-platform/01-standards/platform-selection-and-architecture.md"
            ),
        ),
        (
            "knowledge/cross-platform/01-standards/cross-platform-frameworks.md",
            include_str!(
                "../../../knowledge/cross-platform/01-standards/cross-platform-frameworks.md"
            ),
        ),
        (
            "knowledge/mobile/01-standards/mobile-app-standard.md",
            include_str!("../../../knowledge/mobile/01-standards/mobile-app-standard.md"),
        ),
        (
            "knowledge/harmony/01-standards/harmonyos-arkts-standard.md",
            include_str!("../../../knowledge/harmony/01-standards/harmonyos-arkts-standard.md"),
        ),
        (
            "knowledge/miniprogram/01-standards/miniprogram-standard.md",
            include_str!("../../../knowledge/miniprogram/01-standards/miniprogram-standard.md"),
        ),
        (
            "knowledge/desktop/01-standards/desktop-app-standard.md",
            include_str!("../../../knowledge/desktop/01-standards/desktop-app-standard.md"),
        ),
        // Official platform DESIGN guidelines — Apple HIG / Material 3 /
        // HarmonyOS Design / WeChat mini-program design. This is where a raw
        // base CLI most often produces non-native-looking UI.
        (
            "knowledge/mobile/01-standards/ios-design-hig.md",
            include_str!("../../../knowledge/mobile/01-standards/ios-design-hig.md"),
        ),
        (
            "knowledge/mobile/01-standards/android-material-design.md",
            include_str!("../../../knowledge/mobile/01-standards/android-material-design.md"),
        ),
        (
            "knowledge/harmony/01-standards/harmonyos-design.md",
            include_str!("../../../knowledge/harmony/01-standards/harmonyos-design.md"),
        ),
        (
            "knowledge/miniprogram/01-standards/miniprogram-design.md",
            include_str!("../../../knowledge/miniprogram/01-standards/miniprogram-design.md"),
        ),
        (
            "knowledge/desktop/01-standards/desktop-design.md",
            include_str!("../../../knowledge/desktop/01-standards/desktop-design.md"),
        ),
        // Web framework official best practices + AI/LLM application standard —
        // high-volume areas where a raw base CLI most often misses official
        // patterns (Next App Router caching/RSC) or builds unsafe AI apps.
        (
            "knowledge/frontend/01-standards/web-framework-best-practices.md",
            include_str!(
                "../../../knowledge/frontend/01-standards/web-framework-best-practices.md"
            ),
        ),
        (
            "knowledge/backend/01-standards/llm-application-standard.md",
            include_str!("../../../knowledge/backend/01-standards/llm-application-standard.md"),
        ),
        (
            "knowledge/frontend/01-standards/seo-and-web-vitals.md",
            include_str!("../../../knowledge/frontend/01-standards/seo-and-web-vitals.md"),
        ),
        (
            "knowledge/cicd/01-standards/release-and-store-submission.md",
            include_str!("../../../knowledge/cicd/01-standards/release-and-store-submission.md"),
        ),
        (
            "knowledge/backend/01-standards/analytics-and-growth.md",
            include_str!("../../../knowledge/backend/01-standards/analytics-and-growth.md"),
        ),
        (
            "knowledge/backend/01-standards/backend-framework-idioms.md",
            include_str!("../../../knowledge/backend/01-standards/backend-framework-idioms.md"),
        ),
        (
            "knowledge/backend/01-standards/microservices-and-distributed.md",
            include_str!("../../../knowledge/backend/01-standards/microservices-and-distributed.md"),
        ),
        (
            "knowledge/frontend/01-standards/admin-dashboard-and-crud.md",
            include_str!("../../../knowledge/frontend/01-standards/admin-dashboard-and-crud.md"),
        ),
    ];
    let mut created = 0;
    let mut preserved = 0;
    let mut failed = 0;
    for (rel, content) in files {
        let relative = Path::new(rel);
        let Some(parent) = relative.parent() else {
            failed += 1;
            continue;
        };
        let Some(filename) = relative.file_name() else {
            failed += 1;
            continue;
        };
        let Ok(managed_parent) = crate::bounded_fs::ensure_real_dir_beneath(workspace, parent)
        else {
            failed += 1;
            continue;
        };
        let target = managed_parent.join(filename);
        match std::fs::symlink_metadata(&target) {
            Ok(metadata) if umadev_state::fs::metadata_is_real_file(&metadata) => {
                preserved += 1;
                continue;
            }
            Ok(_) => {
                failed += 1;
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                failed += 1;
                continue;
            }
        }
        if umadev_state::fs::write_new_private(&target, content.as_bytes()).is_ok() {
            created += 1;
        } else {
            failed += 1;
        }
    }
    KnowledgeScaffoldReport {
        created,
        preserved,
        failed,
        total: files.len(),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::*;

    #[cfg(unix)]
    #[test]
    fn scaffold_never_follows_linked_knowledge_directories_or_files() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let sentinel = outside.path().join("sentinel");
        std::fs::write(&sentinel, "outside").unwrap();
        symlink(outside.path(), workspace.path().join("knowledge")).unwrap();
        let report = scaffold_init_knowledge(workspace.path());
        assert_eq!(report.failed, report.total);
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "outside");
        assert!(!outside
            .path()
            .join("design-systems/modern-minimal.md")
            .exists());

        std::fs::remove_file(workspace.path().join("knowledge")).unwrap();
        let target = workspace
            .path()
            .join("knowledge/design-systems/modern-minimal.md");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        symlink(&sentinel, &target).unwrap();
        let _ = scaffold_init_knowledge(workspace.path());
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "outside");
        assert!(std::fs::symlink_metadata(target)
            .unwrap()
            .file_type()
            .is_symlink());
    }
}
