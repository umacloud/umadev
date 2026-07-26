use super::{extension_of, looks_like_secret_test_path, rust_shipping_prefix, Decision};

/// **UD-ARCH-051**: ban TOCTOU race conditions (check-then-use file access).
#[must_use]
pub fn check_toctou_race(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "py" | "rb" | "go" | "rs") {
        return Decision::pass();
    }
    if looks_like_secret_test_path(file_path) {
        return Decision::pass();
    }
    let shipping = if ext == "rs" {
        rust_shipping_prefix(content)
    } else {
        content
    };
    let code = crate::tokenizer::Tokenized::new(shipping).code_only(shipping);
    let lines: Vec<String> = code.lines().map(str::to_ascii_lowercase).collect();
    let is_exists_check = |line: &str| {
        line.contains(".existssync(")
            || line.contains("exists_sync(")
            || line.contains("os.path.exists(")
            || line.contains("path.exists(")
            || line.contains(".exists()")
            || line.contains(".exists (")
            || line.contains("access(")
    };
    let is_file_use = |line: &str| {
        line.contains("readfile")
            || line.contains("read_file")
            || line.contains("fs::read(")
            || line.contains("fs::read_to_string(")
            || line.contains("file::open(")
            || line.contains("openoptions::new(")
            || line.contains("open(")
            || line.contains("fopen(")
            || line.contains("createreadstream(")
            || line.contains("os.open(")
    };
    let has_local_check_then_use = lines.iter().enumerate().any(|(index, line)| {
        is_exists_check(line)
            && lines[index..lines.len().min(index + 8)]
                .iter()
                .any(|candidate| is_file_use(candidate))
    });
    if has_local_check_then_use {
        return Decision::block(
            "UD-ARCH-051",
            format!(
                "UmaDev: TOCTOU race condition (UD-ARCH-051). \
                 `{file_path}` checks file existence then accesses it — the \
                 file can change between check and use (race condition). Use \
                 EAFP: wrap the file access in error handling and handle \
                 not-found errors instead of pre-checking existence.",
            ),
        );
    }
    Decision::pass()
}

/// **UD-SEC-025**: ban insecure file permissions for sensitive files.
#[must_use]
pub fn check_insecure_file_perms(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(
        ext.as_str(),
        "ts" | "js" | "py" | "rb" | "go" | "rs" | "c" | "cpp"
    ) || looks_like_secret_test_path(file_path)
    {
        return Decision::pass();
    }
    let content = if ext == "rs" {
        rust_shipping_prefix(content)
    } else {
        content
    };
    let context_lower = content.to_ascii_lowercase();
    // Permission modes must be executable syntax. Detector examples in comments
    // and shell snippets held in strings do not grant filesystem permissions.
    let tokenized = crate::tokenizer::Tokenized::new(content);
    let lower = tokenized
        .code_only_preserving_lines(content)
        .to_ascii_lowercase();
    let insecure_modes = [
        concat!("0o", "666"),
        concat!("0o", "777"),
        concat!("0o", "644"),
        concat!("0", "666"),
        concat!("0", "777"),
        concat!("chmod ", "666"),
        concat!("chmod ", "777"),
        concat!("chmod ", "644"),
        concat!("create(\"\", ", "06", "66)"),
        concat!("create(\"\", ", "07", "77)"),
        concat!("mode: 0o", "666"),
        concat!("mode: 0o", "777"),
        concat!("S_IRWXU | S_IRWXG | ", "S_IRWXO"),
        concat!("S_IRWXU|S_IRWXG|", "S_IRWXO"),
    ];
    let sensitive_context = context_lower.contains("secret")
        || context_lower.contains("key")
        || context_lower.contains("password")
        || context_lower.contains("token")
        || context_lower.contains("config")
        || context_lower.contains("credential")
        || context_lower.contains("private");
    for mode in insecure_modes {
        let uses_insecure_mode = lower.lines().any(|line| {
            line.contains(mode)
                // Reading Unix permission bits uses 0777 as a mask; it neither
                // creates a file nor grants those permissions.
                && !line.contains(".mode() &")
                && !line.contains(".mode()&")
        });
        if uses_insecure_mode && sensitive_context {
            return Decision::block(
                "UD-SEC-025",
                format!(
                    "UmaDev: insecure file permissions for sensitive file (UD-SEC-025). \
                     `{file_path}` creates a file with overly permissive mode (`{mode}`) \
                     in a context handling secrets/keys. Use `0600` (owner-only \
                     read/write).",
                ),
            );
        }
    }
    Decision::pass()
}

/// **UD-ARCH-053**: require recoverable deletion for commercial records.
#[must_use]
pub fn check_hard_delete(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "py" | "rb" | "go" | "java") {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    let has_record_destroy = lower.lines().any(|line| {
        line.contains("destroy(")
            && ![
                "req.destroy(",
                "request.destroy(",
                "res.destroy(",
                "response.destroy(",
                "socket.destroy(",
                "stream.destroy(",
            ]
            .iter()
            .any(|network_call| line.contains(network_call))
    });
    let has_hard_delete = lower.contains("delete from")
        || lower.contains(".delete(")
        || lower.contains(".remove(")
        || has_record_destroy
        || lower.contains("deleteone(")
        || lower.contains("deletemany(")
        || lower.contains("dropcollection")
        || lower.contains("delete_many(");
    if !has_hard_delete {
        return Decision::pass();
    }
    let has_soft_delete = lower.contains("is_deleted")
        || lower.contains("isdeleted")
        || lower.contains("deleted_at")
        || lower.contains("deletedat")
        || lower.contains("soft_delete")
        || lower.contains("softdelete")
        || lower.contains("active = false")
        || lower.contains("status = 'deleted'")
        || lower.contains("archived");
    if !has_soft_delete {
        return Decision::block(
            "UD-ARCH-053",
            format!(
                "UmaDev: hard-delete without soft-delete (UD-ARCH-053). \
                 `{file_path}` permanently deletes data. Commercial apps must \
                 use recoverable soft-delete with an audit trail.",
            ),
        );
    }
    Decision::pass()
}
