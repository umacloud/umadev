use regex::Regex;
use std::sync::OnceLock;

use super::{extension_of, Decision};

/// Regex matching a broken/weak hash or cipher primitive being *constructed*
/// or *named* in code. Reused across `check_weak_crypto`; cached in a
/// `OnceLock` so it compiles once. Matches:
/// - `createHash('md5'|'sha1')` / `createHash("sha-1")` (Node crypto),
/// - `hashlib.md5(` / `hashlib.sha1(` (Python),
/// - `MessageDigest.getInstance("MD5"|"SHA-1")` (Java),
/// - `md5(` / `sha1(` standalone calls (PHP / Ruby / generic),
/// - `DES` / `RC4` / `Cipher.getInstance("DES")` weak ciphers,
/// - `MD5CryptoServiceProvider` / `SHA1Managed` (.NET).
///
/// The match is intentionally name-anchored (word boundaries / quotes) so a
/// substring like `sha1sum` in a comment URL or `address1` won't trip it.
fn weak_crypto_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)(",
            // Node crypto: createHash('md5') / createHash("sha-1")
            concat!(r#"createhash\s*\(\s*['"]\s*(md"#, r#"5|sha-?1)\s*['"]"#),
            r"|",
            // Python hashlib: hashlib.md5( / hashlib.sha1(
            concat!(r"hashlib\s*\.\s*(md", r"5|sha1)\s*\("),
            r"|",
            // Java MessageDigest.getInstance("MD5") / Cipher.getInstance("DES")
            concat!(
                r#"getinst"#,
                r#"ance\s*\(\s*['"]\s*(md"#,
                r#"5|sha-?1|de"#,
                r#"s|rc"#,
                r#"4|des/|triple"#,
                r#"des)['"/]"#
            ),
            r"|",
            // .NET providers
            concat!(
                r"\b(md",
                r"5cryptoserviceprovider|sha1man",
                r"aged|sha1crypto",
                r"serviceprovider|descrypto",
                r"serviceprovider|rc2crypto",
                r"serviceprovider)\b"
            ),
            r"|",
            // standalone weak-hash calls: md5( / sha1( (PHP/Ruby/Go/generic)
            concat!(r"\b(md", r"5|sha1)\s*\("),
            r"|",
            // weak symmetric ciphers named directly: DESede, DES, RC4, Blowfish
            concat!(
                r"\b(de",
                r"s-cbc|des-",
                r"ecb|rc",
                r"4|3",
                r"des|des",
                r"ede)\b"
            ),
            r")",
        ))
        .expect("weak-crypto regex is well-formed at compile time")
    })
}

/// **UD-SEC-032**: ban broken hash and cipher primitives — MD5, SHA-1, DES, RC4.
///
/// A fixable finding for the post-write QC loop, never the irreversible write
/// floor: MD5 / SHA-1 also have legitimate non-security uses (a Gravatar URL,
/// an S3 `Content-MD5` header, WeChat Pay v2 signing, content addressing), and
/// `.umadev/rules.toml` can disable this clause without touching the
/// plaintext-password floor (UD-SEC-018).
///
/// MD5 and SHA-1 are collision-broken and must never be used for integrity,
/// signatures, password hashing, or any security purpose; DES/3DES/RC4 are
/// broken symmetric ciphers. Matches `createHash('md5'|'sha1')`,
/// `hashlib.md5()`/`hashlib.sha1()`, `MessageDigest.getInstance("MD5"|"SHA-1")`,
/// `Cipher.getInstance("DES")`, bare `md5(`/`sha1(` calls, and the broken .NET
/// providers. Runs on common backend/source extensions. Fail-open: any
/// internal slip returns pass.
#[must_use]
pub fn check_weak_crypto(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(
        ext.as_str(),
        "ts" | "js" | "jsx" | "tsx" | "py" | "rb" | "go" | "java" | "kt" | "cs" | "php" | "rs"
    ) {
        return Decision::pass();
    }
    let re = weak_crypto_regex();
    for line in content.lines() {
        let trimmed = line.trim_start();
        // Skip comment lines — naming a banned primitive while explaining it
        // (e.g. "// don't use md5") shouldn't fire.
        if trimmed.starts_with("//")
            || trimmed.starts_with('#')
            || trimmed.starts_with('*')
            || trimmed.starts_with("/*")
        {
            continue;
        }
        if re.is_match(line) {
            return Decision::block(
                "UD-SEC-032",
                format!(
                    "UmaDev: broken crypto primitive (UD-SEC-032). \
                     `{file_path}` uses a collision-broken hash (MD5/SHA-1) or a \
                     broken legacy cipher. These primitives offer no real security. \
                     Use SHA-256/SHA-3 for integrity, AES-GCM for encryption, and \
                     bcrypt/scrypt/Argon2 for password hashing — never a raw hash \
                     for passwords.",
                ),
            );
        }
    }
    Decision::pass()
}

/// Regex matching server-side template rendering fed *directly* from a
/// concatenation/interpolation that includes a user-input-looking token —
/// i.e. Server-Side Template Injection (SSTI). Cached in a `OnceLock`.
/// Matches things like:
/// - `render_template_string("..." + user)` / `render_template_string(f"...{req...}")` (Flask/Jinja),
/// - `Template(user_input).render(` / `Template(... + x).render(` (Jinja/Mako/Tornado),
/// - `new Function(...)`-style template engines are covered by UD-SEC-007 instead.
///
/// The key signal is *dynamic construction* of the template SOURCE from
/// request/user data, which is the SSTI hole — passing user data as render
/// *context* (the safe pattern) does not match.
fn ssti_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)(",
            // Flask: render_template_string( ... <dynamic> )
            r"render_template_string\s*\(",
            r"|",
            // Jinja2/Mako/Tornado/Django Template(...).render where the
            // Template SOURCE is built dynamically.
            r"\btemplate\s*\(",
            r"|",
            // express/handlebars/ejs compile from a dynamic string
            r"\b(handlebars|hbs|ejs|pug|nunjucks)\s*\.\s*compile\s*\(",
            r")",
        ))
        .expect("ssti regex is well-formed at compile time")
    })
}

/// **UD-SEC-007** (extends the injection family): ban Server-Side Template
/// Injection — feeding user input into the *template source*, not the context.
///
/// `render_template_string(base + user_input)`, `Template(user_input).render()`,
/// or `handlebars.compile(userString)` let an attacker inject template syntax
/// that the engine executes (RCE in Jinja2/Twig/Freemarker). The rule fires
/// only when a dynamic template-rendering call is combined with a
/// user-input-looking token (`user`, `req`, `request`, `params`, `body`,
/// `query`, `input`, a template literal `${...}`, an f-string, or string
/// concatenation) on the same line. Runs on JS/TS/Python. Fail-open.
#[must_use]
pub fn check_template_injection(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "ts" | "js" | "jsx" | "tsx" | "py") {
        return Decision::pass();
    }
    let re = ssti_regex();
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with('*') {
            continue;
        }
        if !re.is_match(line) {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        // The template SOURCE must be built from dynamic / user-ish data.
        let dynamic_user_source = (lower.contains("user")
            || lower.contains("req.")
            || lower.contains("request")
            || lower.contains("params")
            || lower.contains("req.body")
            || lower.contains("body")
            || lower.contains("query")
            || lower.contains("input")
            || lower.contains("${"))
            && (lower.contains(" + ")
                || lower.contains("+ ")
                || lower.contains("${")
                || lower.contains("f\"")
                || lower.contains("f'")
                || lower.contains(".format(")
                || lower.contains("%s")
                || lower.contains("user")
                || lower.contains("input"));
        if dynamic_user_source {
            return Decision::block(
                "UD-SEC-007",
                format!(
                    "UmaDev: server-side template injection (UD-SEC-007). \
                     `{file_path}` builds a template's SOURCE from user input \
                     (e.g. `render_template_string(... + user)` / \
                     `Template(user_input).render()`). The engine executes \
                     injected template syntax — a classic RCE. Render a STATIC \
                     template and pass user data only as the render CONTEXT: \
                     `render_template('page.html', name=user_name)`.",
                ),
            );
        }
    }
    Decision::pass()
}

/// Regex matching a shell-spawning call. Cached in a `OnceLock`. Matches the
/// shell-exec sinks across languages: `exec(` / `execSync(` / `spawn(` (Node),
/// `os.system(` / `subprocess.` / `Popen(` (Python), `Runtime.exec(` (Java),
/// `Process(`/backticks left to the per-line concat check. The *injection*
/// decision is made by `check_command_injection`, which additionally requires
/// dynamic string construction (or `shell=True`) on the same line.
fn shell_exec_sink_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)(",
            r"\bexec(sync)?\s*\(", // Node child_process exec/execSync
            r"|",
            r"\bspawn(sync)?\s*\(", // Node spawn/spawnSync
            r"|",
            r"\bos\s*\.\s*system\s*\(", // Python os.system
            r"|",
            r"\bos\s*\.\s*popen\s*\(", // Python os.popen
            r"|",
            r"\bsubprocess\s*\.\s*(call|run|popen|check_output|check_call)\s*\(", // Python subprocess
            r"|",
            r"\bpopen\s*\(", // generic popen
            r"|",
            r"\bruntime\s*\.\s*getruntime\s*\(\s*\)\s*\.\s*exec\s*\(", // Java
            r")",
        ))
        .expect("shell-exec sink regex is well-formed at compile time")
    })
}

fn has_shell_exec_sink(line: &str, regex: &Regex) -> bool {
    regex.find_iter(line).any(|hit| {
        if !hit
            .as_str()
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("exec")
        {
            return true;
        }
        let prefix = line[..hit.start()].trim_end();
        let Some(receiver_prefix) = prefix.strip_suffix('.') else {
            return true;
        };
        let receiver = receiver_prefix
            .rsplit(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        matches!(receiver.as_str(), "child_process" | "childprocess" | "cp")
    })
}

/// **UD-ARCH-023** (extends the shell-exec family): ban OS command injection —
/// user input concatenated into a shell-spawning call.
///
/// `exec(\`... ${user}\`)`, `os.system("cmd " + user)`, or any
/// `subprocess.*(..., shell=True)` with a built-up string lets an attacker
/// inject `; rm -rf /`. The rule fires when a shell-exec sink (see
/// [`shell_exec_sink_regex`]) appears on a line that ALSO shows dynamic
/// construction (template literal `${...}`, f-string, `.format(`, `%`-format,
/// or string concatenation) OR uses `shell=True`. A static literal command
/// passes. Runs on JS/TS/Python/Java. Fail-open.
#[must_use]
pub fn check_command_injection(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(
        ext.as_str(),
        "ts" | "js" | "jsx" | "tsx" | "py" | "java" | "kt"
    ) {
        return Decision::pass();
    }
    let re = shell_exec_sink_regex();
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('#') || trimmed.starts_with('*') {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        // `shell=True` is dangerous on its own when paired with dynamic input.
        let shell_true = lower.contains("shell=true");
        let is_sink = has_shell_exec_sink(line, re) || (shell_true && lower.contains("subprocess"));
        if !is_sink {
            continue;
        }
        // Dynamic construction signals — string interpolation / concatenation.
        let dynamic = lower.contains("${")
            || lower.contains("` +")
            || lower.contains("+ `")
            || lower.contains("\" +")
            || lower.contains("+ \"")
            || lower.contains("' +")
            || lower.contains("+ '")
            || lower.contains("f\"")
            || lower.contains("f'")
            || lower.contains(".format(")
            || lower.contains("% (")
            || lower.contains("%s")
            || lower.contains("\" + ")
            || lower.contains("str(");
        // `shell=True` plus ANY non-list argument is the canonical injection.
        if dynamic || shell_true {
            return Decision::block(
                "UD-ARCH-023",
                format!(
                    "UmaDev: OS command injection (UD-ARCH-023). \
                     `{file_path}` builds a shell command from interpolated / \
                     concatenated input (or uses `shell=True`). An attacker can \
                     inject `; rm -rf /`. Pass an argument ARRAY to a non-shell \
                     exec — `execFile('git', ['clone', url])` (Node), \
                     `subprocess.run(['git','clone',url])` with `shell=False` \
                     (Python) — and never string-build the command line.",
                ),
            );
        }
    }
    Decision::pass()
}
