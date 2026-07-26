use super::{extension_of, rust_shipping_prefix, Decision};

/// **UD-ARCH-052**: ban shared mutable state without synchronization.
#[must_use]
pub fn check_unsynchronized_mutation(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "go" | "rs" | "py") {
        return Decision::pass();
    }
    let content = if ext == "rs" {
        rust_shipping_prefix(content)
    } else {
        content
    };
    let tokenized = crate::tokenizer::Tokenized::new(content);
    let structural = tokenized.code_only_preserving_lines(content);
    let lower = structural.to_ascii_lowercase();
    let has_concurrency = lower.contains("async")
        || lower.contains("await")
        || lower.contains("promise")
        || lower.contains("go func")
        || lower.contains("goroutine")
        || lower.contains("spawn")
        || lower.contains("thread::")
        || lower.contains("tokio::")
        || lower.contains("asyncio");
    if !has_concurrency {
        return Decision::pass();
    }

    let hits = if ext == "rs" {
        structural
            .lines()
            .filter(|line| line.trim_start().starts_with("static mut "))
            .count()
    } else {
        count_unsynchronized_module_bindings(&structural, &lower)
    };
    if hits == 0 {
        return Decision::pass();
    }
    Decision::block(
        "UD-ARCH-052",
        format!(
            "UmaDev: shared mutable state without synchronization (UD-ARCH-052). \
             `{file_path}` has module-scope mutable variable(s) ({hits}) in \
             concurrent code (async/goroutine/thread) — this is a data race. \
             Use a `Mutex`/`AtomicUsize`/`Arc<Mutex<T>>` (Rust), `sync.Mutex` \
             (Go), or move the state into a class/actor.",
        ),
    )
}

fn count_unsynchronized_module_bindings(structural: &str, lower: &str) -> usize {
    let synchronized = lower.contains("mutex")
        || lower.contains("atomic")
        || lower.contains("rwlock")
        || lower.contains("sync.")
        || lower.contains("lock()");
    if synchronized {
        return 0;
    }

    let mut depth = 0i32;
    let mut hits = 0usize;
    for line in structural.lines() {
        let trimmed = line.trim_start();
        if depth == 0
            && (trimmed.starts_with("let ") || trimmed.starts_with("var "))
            && [
                "= 0", "= 1", "= []", "= {}", "= new ", "= \"", "= Some", "= Mutex", "= Atomic",
            ]
            .iter()
            .any(|pattern| trimmed.contains(pattern))
        {
            hits += 1;
        }
        for ch in line.chars() {
            match ch {
                '{' => depth += 1,
                '}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    hits
}
