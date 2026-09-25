//! **UD-SEC-002**: the bypass-immune guard against destructive shell commands.
//!
//! The guard reads a command the way a shell does — quotes, comments, heredocs,
//! command and process substitutions, pipelines and `;` / `&&` / `||` / newline
//! lists — and judges each simple command by its NAME and arguments, never by a
//! substring of the whole line. A commit message, a `grep` pattern, a heredoc
//! that writes a file, or a checksum pipe (`| sha256sum`) that merely mentions a
//! dangerous word therefore passes, while a real destructive command is caught
//! in every spelling: behind `sudo -E` / `env A=B` / `timeout` / `xargs`
//! wrappers, after a newline, inside `bash -c '…'`, `eval`, `$(…)`, `<(…)`, or
//! in a script piped into a shell.
//!
//! Fail-open: whatever it cannot read passes, and nested code deeper than
//! [`MAX_DEPTH`] levels is not followed.

use super::Decision;

/// How many levels of nested code (`bash -c`, `$(…)`, `eval`, a heredoc fed to
/// a shell) are followed.
const MAX_DEPTH: usize = 8;

/// Shell interpreters: code piped, fed or passed to them runs.
const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "ksh", "mksh", "ash", "fish", "csh", "tcsh",
];

/// Network downloaders, including the PowerShell cmdlets and their aliases.
const DOWNLOADERS: &[&str] = &[
    "curl",
    "wget",
    "fetch",
    "irm",
    "iwr",
    "invoke-restmethod",
    "invoke-webrequest",
];

/// Words that fetch remote content inside a PowerShell `iex (…)` argument.
const POWERSHELL_DOWNLOADS: &[&str] = &[
    "irm",
    "iwr",
    "invoke-restmethod",
    "invoke-webrequest",
    "downloadstring",
    "curl",
    "wget",
];

/// Database command-line clients whose arguments / stdin are SQL.
const SQL_CLIENTS: &[&str] = &[
    "psql",
    "mysql",
    "mariadb",
    "sqlite3",
    "sqlite",
    "sqlcmd",
    "mongosh",
    "mongo",
    "clickhouse-client",
    "duckdb",
    "cockroach",
];

/// A wrapper that runs the command after it: its name, the options that take a
/// separate argument, and how many positional arguments precede the command.
struct Wrapper {
    name: &'static str,
    options_with_argument: &'static [&'static str],
    positionals: usize,
}

const WRAPPERS: &[Wrapper] = &[
    Wrapper {
        name: "sudo",
        options_with_argument: &[
            "-u",
            "-g",
            "-C",
            "-D",
            "-h",
            "-p",
            "-r",
            "-t",
            "-T",
            "-U",
            "--user",
            "--group",
            "--chdir",
            "--prompt",
            "--role",
            "--type",
            "--other-user",
            "--host",
            "--close-from",
            "--command-timeout",
        ],
        positionals: 0,
    },
    Wrapper {
        name: "doas",
        options_with_argument: &["-u", "-C"],
        positionals: 0,
    },
    Wrapper {
        name: "env",
        options_with_argument: &["-u", "-C", "-S", "--unset", "--chdir", "--split-string"],
        positionals: 0,
    },
    Wrapper {
        name: "exec",
        options_with_argument: &["-a"],
        positionals: 0,
    },
    Wrapper {
        name: "stdbuf",
        options_with_argument: &["-i", "-o", "-e", "--input", "--output", "--error"],
        positionals: 0,
    },
    Wrapper {
        name: "nice",
        options_with_argument: &["-n", "--adjustment"],
        positionals: 0,
    },
    Wrapper {
        name: "ionice",
        options_with_argument: &["-c", "-n", "-p", "-P", "-u", "--class", "--classdata"],
        positionals: 0,
    },
    Wrapper {
        name: "timeout",
        options_with_argument: &["-s", "-k", "--signal", "--kill-after"],
        positionals: 1,
    },
    Wrapper {
        name: "xargs",
        options_with_argument: &[
            "-I",
            "-n",
            "-P",
            "-L",
            "-d",
            "-E",
            "-s",
            "-a",
            "--max-args",
            "--max-procs",
            "--max-lines",
            "--delimiter",
            "--eof",
            "--max-chars",
            "--arg-file",
            "--replace",
        ],
        positionals: 0,
    },
    Wrapper {
        name: "time",
        options_with_argument: &["-f", "-o", "--format", "--output"],
        positionals: 0,
    },
    Wrapper {
        name: "nohup",
        options_with_argument: &[],
        positionals: 0,
    },
    Wrapper {
        name: "builtin",
        options_with_argument: &[],
        positionals: 0,
    },
    Wrapper {
        name: "command",
        options_with_argument: &[],
        positionals: 0,
    },
    Wrapper {
        name: "setsid",
        options_with_argument: &[],
        positionals: 0,
    },
    Wrapper {
        name: "busybox",
        options_with_argument: &[],
        positionals: 0,
    },
];

/// Shell keywords that may precede a command in the same list element.
const KEYWORDS: &[&str] = &[
    "!", "{", "}", "if", "then", "else", "elif", "fi", "do", "done", "while", "until",
];

/// **UD-SEC-002**: block destructive shell commands before the host runs them.
///
/// This is the real-time guard for `Bash` tool calls (the hook also intercepts
/// `Write`/`Edit` via UD-SEC-001/UD-CODE-*). It reads the command like a shell
/// does and denies the catastrophic ones with a concrete reason the host can act
/// on: a recursive forced `rm` of the filesystem root, the home directory or a
/// Windows drive root; a network download run as code; a `git push` (including
/// every force push) or another history/branch/stash-destroying git verb;
/// `chmod 777`; a raw `dd` to a device; `mkfs`; `shutdown` / `init 0`; and
/// `DROP TABLE` / `DROP DATABASE` sent to a database client. Like UD-SEC-001 it
/// is bypass-immune and runs before any "skip governance" toggle could apply.
///
/// Fail-open: an unparseable command passes. It only blocks what it can
/// confidently identify as dangerous.
#[must_use]
pub fn check_dangerous_bash(command: &str) -> Decision {
    check_script(&parse(command, 0), 0).unwrap_or_else(Decision::pass)
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// A parsed command list: every simple command, and the pipelines that group
/// them (`a | b; c` → pipelines `[a, b]` and `[c]`).
#[derive(Default)]
struct Script {
    cmds: Vec<Cmd>,
    pipelines: Vec<Vec<usize>>,
}

#[derive(Default)]
struct Cmd {
    words: Vec<Word>,
    /// Heredoc and here-string bodies fed to the command's standard input.
    stdin: Vec<String>,
}

#[derive(Default)]
struct Word {
    /// The word with quotes removed; substitutions stay as written (`$(pwd)/`).
    text: String,
    /// The command / process substitutions inside the word, already parsed.
    nested: Vec<Script>,
}

/// Parse `src` as a command list. Nothing is parsed beyond [`MAX_DEPTH`].
fn parse(src: &str, depth: usize) -> Script {
    if depth > MAX_DEPTH {
        return Script::default();
    }
    let chars: Vec<char> = src.chars().collect();
    let mut parser = Parser {
        src: &chars,
        pos: 0,
        depth,
    };
    parser.list(false)
}

struct Parser<'a> {
    src: &'a [char],
    pos: usize,
    depth: usize,
}

/// Accumulates the words, commands and pipelines of one command list.
#[derive(Default)]
struct Builder {
    script: Script,
    pipeline: Vec<usize>,
    cmd: Cmd,
    word: Option<Word>,
    /// `(` characters kept inside the words of the current command (PowerShell
    /// `iex (irm …)`), so their `)` is not read as the end of a subshell.
    word_parens: usize,
    /// Heredocs declared by the command being built: `(delimiter, strip tabs)`.
    declared: Vec<(String, bool)>,
    /// Heredocs whose bodies start after the next newline, with the index of the
    /// command that reads them.
    awaiting: Vec<(Option<usize>, String, bool)>,
}

impl Builder {
    fn word(&mut self) -> &mut Word {
        self.word.get_or_insert_with(Word::default)
    }

    fn at_command_start(&self) -> bool {
        self.word.is_none() && self.cmd.words.is_empty()
    }

    fn end_word(&mut self) {
        if let Some(word) = self.word.take() {
            self.cmd.words.push(word);
        }
    }

    fn end_command(&mut self) {
        self.end_word();
        self.word_parens = 0;
        let cmd = std::mem::take(&mut self.cmd);
        let index = if cmd.words.is_empty() && cmd.stdin.is_empty() {
            None
        } else {
            self.script.cmds.push(cmd);
            self.pipeline.push(self.script.cmds.len() - 1);
            Some(self.script.cmds.len() - 1)
        };
        for (delimiter, strip_tabs) in std::mem::take(&mut self.declared) {
            self.awaiting.push((index, delimiter, strip_tabs));
        }
    }

    fn end_pipeline(&mut self) {
        self.end_command();
        if !self.pipeline.is_empty() {
            self.script
                .pipelines
                .push(std::mem::take(&mut self.pipeline));
        }
    }

    fn finish(mut self) -> Script {
        self.end_pipeline();
        self.script
    }
}

impl Parser<'_> {
    fn peek(&self, offset: usize) -> Option<char> {
        self.src.get(self.pos + offset).copied()
    }

    fn text(&self, start: usize) -> String {
        self.src
            .get(start..self.pos)
            .unwrap_or_default()
            .iter()
            .collect()
    }

    /// Parse a command list up to the end of the input or, inside `$(…)` /
    /// `<(…)` (`in_sub`), the `)` that closes it.
    fn list(&mut self, in_sub: bool) -> Script {
        let mut b = Builder::default();
        let mut subshells = 0_usize;
        while let Some(c) = self.peek(0) {
            match c {
                ' ' | '\t' | '\r' => {
                    self.pos += 1;
                    b.end_word();
                }
                '\n' => {
                    self.pos += 1;
                    b.end_pipeline();
                    self.read_heredocs(&mut b);
                }
                ';' => {
                    self.pos += 1;
                    b.end_pipeline();
                }
                '&' if self.peek(1) == Some('>') => self.redirect(&mut b),
                '&' => {
                    self.pos += if self.peek(1) == Some('&') { 2 } else { 1 };
                    b.end_pipeline();
                }
                '|' if self.peek(1) == Some('|') => {
                    self.pos += 2;
                    b.end_pipeline();
                }
                '|' => {
                    self.pos += if self.peek(1) == Some('&') { 2 } else { 1 };
                    b.end_command();
                }
                '(' if b.at_command_start() => {
                    self.pos += 1;
                    subshells += 1;
                    b.end_pipeline();
                }
                ')' if b.word_parens == 0 => {
                    self.pos += 1;
                    b.end_pipeline();
                    if subshells > 0 {
                        subshells -= 1;
                    } else if in_sub {
                        return b.finish();
                    }
                }
                '#' if b.word.is_none() => {
                    while self.peek(0).is_some_and(|c| c != '\n') {
                        self.pos += 1;
                    }
                }
                // A standalone `{` opens a group (`{ …; }`, a function body): the
                // commands inside are judged on their own.
                '{' if b.word.is_none() && self.peek(1).is_none_or(char::is_whitespace) => {
                    self.pos += 1;
                    b.end_pipeline();
                }
                '<' | '>' if self.peek(1) == Some('(') => self.word_part(&mut b),
                '<' | '>' => self.redirect(&mut b),
                _ => self.word_part(&mut b),
            }
        }
        b.finish()
    }

    /// Consume one piece of a word: a quoted string, an escape, a substitution,
    /// or a plain character.
    fn word_part(&mut self, b: &mut Builder) {
        let Some(c) = self.peek(0) else {
            return;
        };
        match c {
            '\\' => {
                match self.peek(1) {
                    Some('\n') => {}
                    Some(next) => b.word().text.push(next),
                    None => b.word().text.push('\\'),
                }
                self.pos += 2;
            }
            '\'' => {
                self.pos += 1;
                let text = self.until_quote('\'', false);
                b.word().text.push_str(&text);
            }
            '$' if self.peek(1) == Some('\'') => {
                self.pos += 2;
                let text = self.until_quote('\'', true);
                b.word().text.push_str(&text);
            }
            '"' => {
                self.pos += 1;
                self.double_quoted(b);
            }
            '$' if self.peek(1) == Some('(') => self.substitution(b, 2),
            '<' | '>' => self.substitution(b, 2),
            '`' => self.backtick(b),
            '(' => {
                b.word_parens += 1;
                b.word().text.push('(');
                self.pos += 1;
            }
            ')' => {
                b.word_parens = b.word_parens.saturating_sub(1);
                b.word().text.push(')');
                self.pos += 1;
            }
            _ => {
                b.word().text.push(c);
                self.pos += 1;
            }
        }
    }

    /// The text up to the closing `quote` (consumed). `escapes` honours `\'`
    /// inside `$'…'`.
    fn until_quote(&mut self, quote: char, escapes: bool) -> String {
        let mut text = String::new();
        while let Some(c) = self.peek(0) {
            self.pos += 1;
            if c == quote {
                break;
            }
            if escapes && c == '\\' {
                if let Some(next) = self.peek(0) {
                    self.pos += 1;
                    text.push(next);
                    continue;
                }
            }
            text.push(c);
        }
        text
    }

    /// The rest of a `"…"` string: escapes, and substitutions that still run.
    fn double_quoted(&mut self, b: &mut Builder) {
        b.word();
        while let Some(c) = self.peek(0) {
            match c {
                '"' => {
                    self.pos += 1;
                    return;
                }
                '\\' => {
                    match self.peek(1) {
                        Some('\n') => {}
                        Some(next @ ('$' | '`' | '"' | '\\')) => b.word().text.push(next),
                        Some(next) => {
                            b.word().text.push('\\');
                            b.word().text.push(next);
                        }
                        None => b.word().text.push('\\'),
                    }
                    self.pos += 2;
                }
                '$' if self.peek(1) == Some('(') => self.substitution(b, 2),
                '`' => self.backtick(b),
                _ => {
                    b.word().text.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    /// `$(…)`, `<(…)` or `>(…)` (whose opener is `open_len` chars): the inner
    /// command list is parsed and kept; the word keeps the written text.
    /// `$((…))` is arithmetic and only skipped.
    fn substitution(&mut self, b: &mut Builder, open_len: usize) {
        let start = self.pos;
        let arithmetic = self.peek(0) == Some('$') && self.peek(2) == Some('(');
        self.pos += open_len;
        if arithmetic || self.depth >= MAX_DEPTH {
            self.skip_parens();
        } else {
            self.depth += 1;
            let nested = self.list(true);
            self.depth -= 1;
            b.word().nested.push(nested);
        }
        let text = self.text(start);
        b.word().text.push_str(&text);
    }

    /// Skip to the `)` that balances an already-consumed `(`.
    fn skip_parens(&mut self) {
        let mut open = 1_usize;
        while let Some(c) = self.peek(0) {
            self.pos += 1;
            match c {
                '(' => open += 1,
                ')' => {
                    open -= 1;
                    if open == 0 {
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    /// A `` `…` `` command substitution.
    fn backtick(&mut self, b: &mut Builder) {
        let start = self.pos;
        self.pos += 1;
        let mut inner = String::new();
        while let Some(c) = self.peek(0) {
            self.pos += 1;
            match c {
                '`' => break,
                '\\' => {
                    if let Some(next) = self.peek(0) {
                        self.pos += 1;
                        inner.push(next);
                    }
                }
                _ => inner.push(c),
            }
        }
        let nested = parse(&inner, self.depth + 1);
        let text = self.text(start);
        let word = b.word();
        word.nested.push(nested);
        word.text.push_str(&text);
    }

    /// A redirection. Its target stays an ordinary word; a heredoc is recorded
    /// for the current command and a here-string becomes its stdin.
    fn redirect(&mut self, b: &mut Builder) {
        b.end_word();
        if self.peek(0) == Some('<') && self.peek(1) == Some('<') {
            if self.peek(2) == Some('<') {
                self.pos += 3;
                let text = self.plain_word();
                b.cmd.stdin.push(text);
                return;
            }
            self.pos += 2;
            let strip_tabs = self.peek(0) == Some('-');
            if strip_tabs {
                self.pos += 1;
            }
            let delimiter = self.plain_word();
            if !delimiter.is_empty() {
                b.declared.push((delimiter, strip_tabs));
            }
            return;
        }
        // `>`, `>>`, `>|`, `>&`, `<`, `<&`, `<>`, `&>`, `&>>`.
        self.pos += 1;
        while matches!(self.peek(0), Some('>' | '|' | '&')) {
            self.pos += 1;
        }
    }

    /// A word read without substitutions (a heredoc delimiter, a here-string).
    fn plain_word(&mut self) -> String {
        while matches!(self.peek(0), Some(' ' | '\t')) {
            self.pos += 1;
        }
        let mut text = String::new();
        while let Some(c) = self.peek(0) {
            match c {
                ' ' | '\t' | '\r' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')' => break,
                '\'' | '"' => {
                    self.pos += 1;
                    let quoted = self.until_quote(c, c == '"');
                    text.push_str(&quoted);
                }
                '\\' => {
                    if let Some(next) = self.peek(1) {
                        text.push(next);
                    }
                    self.pos += 2;
                }
                _ => {
                    text.push(c);
                    self.pos += 1;
                }
            }
        }
        text
    }

    /// After a newline: the bodies of the heredocs declared on the line that
    /// just ended, in order, each up to its delimiter line.
    fn read_heredocs(&mut self, b: &mut Builder) {
        for (index, delimiter, strip_tabs) in std::mem::take(&mut b.awaiting) {
            let mut body = String::new();
            while self.pos < self.src.len() {
                let start = self.pos;
                while self.peek(0).is_some_and(|c| c != '\n') {
                    self.pos += 1;
                }
                let line = self.text(start);
                self.pos += 1;
                let bare = if strip_tabs {
                    line.trim_start_matches('\t')
                } else {
                    line.as_str()
                };
                if bare.trim_end_matches('\r') == delimiter {
                    break;
                }
                body.push_str(&line);
                body.push('\n');
            }
            if let Some(cmd) = index.and_then(|i| b.script.cmds.get_mut(i)) {
                cmd.stdin.push(body);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Judgement
// ---------------------------------------------------------------------------

fn check_script(script: &Script, depth: usize) -> Option<Decision> {
    if depth > MAX_DEPTH {
        return None;
    }
    let nested = script
        .cmds
        .iter()
        .flat_map(|cmd| &cmd.words)
        .flat_map(|word| &word.nested);
    for inner in nested {
        if let Some(decision) = check_script(inner, depth + 1) {
            return Some(decision);
        }
    }
    for pipeline in &script.pipelines {
        let stages: Vec<&Cmd> = pipeline
            .iter()
            .filter_map(|&index| script.cmds.get(index))
            .collect();
        for (at, cmd) in stages.iter().enumerate() {
            if let Some(decision) = check_command(cmd, &stages[..at], depth) {
                return Some(decision);
            }
        }
    }
    None
}

/// Judge one simple command. `upstream` are the pipeline stages feeding it.
fn check_command(cmd: &Cmd, upstream: &[&Cmd], depth: usize) -> Option<Decision> {
    let words = strip_wrappers(&cmd.words)?;
    let (first, args) = words.split_first()?;
    let name = command_name(&first.text);
    let verdict = match name.as_str() {
        "rm" => catastrophic_rm(args).then(root_rm_blocked),
        "git" => check_git(args),
        "chmod" => args
            .iter()
            .any(|w| matches!(w.text.as_str(), "777" | "0777" | "00777"))
            .then(|| blocked(
                "chmod 777",
                "`chmod 777` makes a file world-readable/writable/executable — a security hole.",
                "grant only the needed bits, e.g. `chmod 755` (owner rwx, others rx) or `chmod +x`.",
            )),
        "dd" => args
            .iter()
            .any(|w| w.text.starts_with("of=/dev/"))
            .then(|| blocked(
                "of=/dev/",
                "Writing to a device node (`of=/dev/…`) can overwrite a disk, partition, or memory device — `dd` makes this destructive and silent.",
                "confirm the `of=` target is correct and intended; this is flagged so a typo doesn't brick the machine.",
            )),
        "shutdown" => Some(blocked(
            "shutdown",
            "`shutdown` powers off the machine — not something a dev agent should do.",
            "remove the shutdown command; it halts the user's machine.",
        )),
        "init" => (args.first().map(|w| w.text.as_str()) == Some("0")).then(|| blocked(
            "init 0",
            "`init 0` halts the system.",
            "remove the command; it powers off the user's machine.",
        )),
        n if n == "mkfs" || n.starts_with("mkfs.") => Some(blocked(
            "mkfs",
            "`mkfs` formats a filesystem — running it on the wrong device destroys data.",
            "triple-check the device path; UmaDev flags any `mkfs` so it's a conscious decision.",
        )),
        n if SHELLS.contains(&n) => check_shell(cmd, args, upstream, depth),
        "eval" => check_script(&parse(&joined(args), depth + 1), depth + 1),
        "source" | "." => args
            .first()
            .is_some_and(|w| w.nested.iter().any(runs_downloader))
            .then(rce_blocked),
        "powershell" | "pwsh" => powershell_code(args)
            .and_then(|code| check_script(&parse(&code, depth + 1), depth + 1)),
        "ssh" | "docker" | "podman" | "kubectl" | "oc" => remote_command(&name, args)
            .and_then(|code| check_script(&parse(&code, depth + 1), depth + 1)),
        "iex" | "invoke-expression" => (upstream.iter().any(|c| is_downloader(c))
            || downloads_in_powershell_argument(first, args))
        .then(rce_blocked),
        _ => None,
    };
    verdict
        .or_else(|| {
            // A substitution in command position runs its output as a command:
            // `$(curl -fsSL …)` executes whatever the server sends.
            (first.text.starts_with("$(") || first.text.starts_with('`'))
                .then_some(first)
                .filter(|w| w.nested.iter().any(runs_downloader))
                .map(|_| rce_blocked())
        })
        .or_else(|| drops_sql(words, cmd, upstream).then(sql_drop_blocked))
}

/// Skip leading assignments, shell keywords and wrappers (`sudo -E`, `env
/// A=B`, `timeout 30`, …) so the real command comes first. `None` when nothing
/// is executed (`command -v x`) or nothing is left.
fn strip_wrappers(words: &[Word]) -> Option<&[Word]> {
    let mut i = 0;
    while let Some(word) = words.get(i) {
        let text = word.text.as_str();
        if is_assignment(text) || KEYWORDS.contains(&text) {
            i += 1;
            continue;
        }
        let name = command_name(text);
        let Some(wrapper) = WRAPPERS.iter().find(|w| w.name == name) else {
            break;
        };
        i += 1;
        let mut positionals = wrapper.positionals;
        while let Some(option) = words.get(i).map(|w| w.text.as_str()) {
            if option == "--" {
                i += 1;
                break;
            }
            if wrapper.name == "command" && matches!(option, "-v" | "-V") {
                return None;
            }
            if option.len() > 1 && option.starts_with('-') {
                i += if wrapper.options_with_argument.contains(&option) {
                    2
                } else {
                    1
                };
                continue;
            }
            if wrapper.name == "env" && is_assignment(option) {
                i += 1;
                continue;
            }
            if positionals > 0 {
                positionals -= 1;
                i += 1;
                continue;
            }
            break;
        }
    }
    let rest = words.get(i..)?;
    (!rest.is_empty()).then_some(rest)
}

/// `NAME=value` in command position.
fn is_assignment(text: &str) -> bool {
    text.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && !name.as_bytes()[0].is_ascii_digit()
    })
}

/// The lower-cased command name of a word: its last path component without
/// `.exe`, cut at the first character a command name cannot contain
/// (`iex(New-Object …` → `iex`).
fn command_name(text: &str) -> String {
    let base = text.rsplit(['/', '\\']).next().unwrap_or(text);
    let lower = base.to_ascii_lowercase();
    let lower = lower.strip_suffix(".exe").unwrap_or(&lower);
    lower
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
        .collect()
}

fn joined(words: &[Word]) -> String {
    words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a command (after its wrappers) is a network downloader.
fn is_downloader(cmd: &Cmd) -> bool {
    strip_wrappers(&cmd.words)
        .and_then(<[Word]>::first)
        .is_some_and(|w| DOWNLOADERS.contains(&command_name(&w.text).as_str()))
}

fn runs_downloader(script: &Script) -> bool {
    script.cmds.iter().any(is_downloader)
}

/// `iex (irm …)` / `iex (New-Object Net.WebClient).DownloadString(…)`.
fn downloads_in_powershell_argument(first: &Word, args: &[Word]) -> bool {
    std::iter::once(first)
        .chain(args)
        .flat_map(|w| {
            w.text
                .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_')))
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>()
        })
        .skip(1)
        .any(|token| POWERSHELL_DOWNLOADS.contains(&token.as_str()))
}

/// The command line a remote / container exec runs: `ssh host <cmd…>`,
/// `docker|podman [compose] exec <container> <cmd…>`, `kubectl|oc exec <pod> --
/// <cmd…>`. `None` for anything else (`docker build`, `ssh host` with no
/// command, a `kubectl exec` without `--`).
fn remote_command(name: &str, args: &[Word]) -> Option<String> {
    let (with_argument, rest): (&[&str], &[Word]) = match name {
        "ssh" => (
            &[
                "-b", "-c", "-D", "-E", "-e", "-F", "-I", "-i", "-J", "-L", "-l", "-m", "-O", "-o",
                "-p", "-Q", "-R", "-S", "-W", "-w",
            ],
            args,
        ),
        "kubectl" | "oc" => {
            let exec = args.first().is_some_and(|w| w.text == "exec");
            let dashes = args.iter().position(|w| w.text == "--")?;
            return exec.then(|| joined(&args[dashes + 1..]));
        }
        _ => {
            let start = match args {
                [first, ..] if first.text == "exec" => 1,
                [first, second, ..]
                    if matches!(first.text.as_str(), "compose" | "container")
                        && second.text == "exec" =>
                {
                    2
                }
                _ => return None,
            };
            (
                &[
                    "-e",
                    "--env",
                    "--env-file",
                    "-u",
                    "--user",
                    "-w",
                    "--workdir",
                    "--detach-keys",
                    "--index",
                ],
                &args[start..],
            )
        }
    };
    let mut i = 0;
    while let Some(word) = rest.get(i) {
        if !word.text.starts_with('-') {
            break;
        }
        i += if with_argument.contains(&word.text.as_str()) {
            2
        } else {
            1
        };
    }
    let command = rest.get(i + 1..)?;
    (!command.is_empty()).then(|| joined(command))
}

/// The code a `powershell` / `pwsh` invocation runs (`-c`, `-Command`, or the
/// implicit command after the options), or `None` for a script file.
fn powershell_code(args: &[Word]) -> Option<String> {
    const WITH_ARGUMENT: &[&str] = &[
        "-executionpolicy",
        "-ep",
        "-ex",
        "-windowstyle",
        "-w",
        "-inputformat",
        "-outputformat",
        "-configurationname",
        "-workingdirectory",
        "-wd",
        "-version",
        "-psconsolefile",
        "-settingsfile",
    ];
    let mut i = 0;
    while let Some(word) = args.get(i) {
        let option = word.text.to_ascii_lowercase();
        match option.as_str() {
            "-c" | "-command" | "/c" | "/command" => return Some(joined(&args[i + 1..])),
            "-file" | "-f" | "-encodedcommand" | "-enc" | "-e" | "-ec" => return None,
            o if WITH_ARGUMENT.contains(&o) => i += 2,
            o if o.starts_with('-') || o.starts_with('/') => i += 1,
            o if std::path::Path::new(o)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("ps1")) =>
            {
                return None
            }
            _ => return Some(joined(&args[i..])),
        }
    }
    None
}

/// A shell interpreter: code piped into it, fed to it through a heredoc, passed
/// with `-c`, or given as a `<(curl …)` script file.
fn check_shell(cmd: &Cmd, args: &[Word], upstream: &[&Cmd], depth: usize) -> Option<Decision> {
    if upstream.iter().any(|c| is_downloader(c)) {
        return Some(rce_blocked());
    }
    let invocation = shell_invocation(args);
    if let Some(script) = invocation.script_file {
        return script.nested.iter().any(runs_downloader).then(rce_blocked);
    }
    if let Some(code) = invocation.code {
        return check_script(&parse(&code.text, depth + 1), depth + 1);
    }
    let fed = cmd
        .stdin
        .iter()
        .cloned()
        .chain(upstream.iter().flat_map(|up| {
            let echoed = strip_wrappers(&up.words)
                .and_then(|words| words.split_first())
                .filter(|(first, _)| {
                    matches!(command_name(&first.text).as_str(), "echo" | "printf")
                })
                .map(|(_, rest)| {
                    let text: Vec<&str> = rest
                        .iter()
                        .map(|w| w.text.as_str())
                        .filter(|t| !t.starts_with('-'))
                        .collect();
                    text.join(" ").replace("\\n", "\n")
                });
            up.stdin.iter().cloned().chain(echoed)
        }));
    for code in fed {
        if let Some(decision) = check_script(&parse(&code, depth + 1), depth + 1) {
            return Some(decision);
        }
    }
    None
}

struct ShellInvocation<'a> {
    /// The `-c` code.
    code: Option<&'a Word>,
    /// The script file argument (the shell then does not read code from stdin).
    script_file: Option<&'a Word>,
}

fn shell_invocation(args: &[Word]) -> ShellInvocation<'_> {
    let mut code_flag = false;
    let mut stdin_flag = false;
    let mut i = 0;
    while let Some(word) = args.get(i) {
        let text = word.text.as_str();
        if text == "--" {
            i += 1;
            break;
        }
        if text.len() < 2 || !(text.starts_with('-') || text.starts_with('+')) {
            break;
        }
        i += 1;
        if text.starts_with("--") {
            if matches!(text, "--rcfile" | "--init-file") {
                i += 1;
            }
            continue;
        }
        let cluster = &text[1..];
        code_flag |= cluster.contains('c');
        stdin_flag |= cluster.contains('s');
        if cluster.contains(['o', 'O']) {
            i += 1;
        }
    }
    let operand = args.get(i).filter(|w| w.text != "-");
    ShellInvocation {
        code: operand.filter(|_| code_flag),
        script_file: operand.filter(|_| !code_flag && !stdin_flag),
    }
}

/// A recursive + forced `rm` of a catastrophic target, in any flag spelling
/// (`-rf`, `-fr`, `-r -f`, `--recursive --force`, a `--` separator).
fn catastrophic_rm(args: &[Word]) -> bool {
    let mut recursive = false;
    let mut force = false;
    let mut end_of_options = false;
    let mut dangerous_target = false;
    for arg in args {
        let text = arg.text.as_str();
        if !end_of_options && text == "--" {
            end_of_options = true;
            continue;
        }
        if !end_of_options && text.len() > 1 && text.starts_with('-') {
            match text.strip_prefix("--") {
                Some("recursive") => recursive = true,
                Some("force") => force = true,
                Some(_) => {}
                None => {
                    recursive |= text.contains(['r', 'R']);
                    force |= text.contains('f');
                }
            }
            continue;
        }
        dangerous_target |= is_dangerous_rm_target(text);
    }
    recursive && force && dangerous_target
}

/// The filesystem root, the home directory, a wildcard directly under either,
/// or a Windows drive root in Git Bash (`/c`, `/c/`, `/c/*`) or native
/// (`C:/`, `C:\`) spelling. In-tree targets (`./build`, `target/`) and
/// subpaths (`/tmp/x`, `~/.cache`, `/c/Users/me/app/build`) are not.
fn is_dangerous_rm_target(target: &str) -> bool {
    let lower = target.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "/" | "/*"
            | "/."
            | "~"
            | "~/"
            | "~/*"
            | "$home"
            | "$home/"
            | "$home/*"
            | "${home}"
            | "${home}/"
            | "${home}/*"
    ) {
        return true;
    }
    let bytes = lower.as_bytes();
    let (drive, rest) = match bytes {
        [b'/', letter, rest @ ..] => (*letter, rest),
        [letter, b':', rest @ ..] => (*letter, rest),
        _ => return false,
    };
    drive.is_ascii_lowercase() && matches!(rest, [] | [b'/' | b'\\'] | [b'/' | b'\\', b'*' | b'.'])
}

fn check_git(args: &[Word]) -> Option<Decision> {
    let (sub, rest) = git_subcommand(args)?;
    let has = |flag: &str| rest.iter().any(|w| w.text == flag);
    let first = rest.first().map_or("", |w| w.text.as_str());
    match sub.as_str() {
        "push" if !has("--dry-run") => Some(if is_force_push(rest) {
            blocked(
                "git push --force",
                "A force push — including `--force-with-lease` — rewrites the remote's history and can clobber teammates' work.",
                "let the user run the force push; `git push --dry-run` is allowed for inspection.",
            )
        } else {
            Decision::block(
                "UD-SEC-002",
                "UmaDev: destructive command blocked (UD-SEC-002). `git push` reaches a remote \
                 and (per UmaDev's trust contract) UmaDev never auto-pushes — this holds even \
                 behind a `git -C <dir>` or other global-option prefix. fix: let the user run \
                 the push, or use `git push --dry-run` to inspect.",
            )
        }),
        "reset" if has("--hard") => Some(blocked(
            "reset --hard",
            "`git reset --hard` discards all uncommitted changes with no recovery.",
            "stash first (`git stash`) or target a specific file; UmaDev flags it so the decision is conscious.",
        )),
        "clean" if forced_clean(rest) => Some(Decision::block(
            "UD-SEC-002",
            "UmaDev: destructive command blocked (UD-SEC-002). `git clean -f…` irreversibly \
             deletes untracked files (and with `-d`/`-x`, whole untracked directories and \
             ignored files) in any flag order. fix: inspect first with `git clean -n` (dry \
             run), then remove only what you mean to.",
        )),
        "merge" if !["--abort", "--continue", "--quit"].iter().any(|f| has(f)) => Some(blocked(
            "git merge",
            "`git merge` mutates the current branch's history — UmaDev isolates work on `umadev/<slug>` and never auto-merges into the user's branch.",
            "leave the merge to the user after they review the diff (`git merge --abort` / `--continue` and read-only `git merge-base` are allowed).",
        )),
        "rm" if !has("--cached") => Some(blocked(
            "git rm",
            "`git rm` deletes tracked files from the working tree and the index.",
            "delete the file in a reviewed change; `git rm --cached` (stop tracking, keep the file) is allowed.",
        )),
        "branch" if deletes_branch(rest) => Some(blocked(
            "git branch -d",
            "`git branch -d`/`-D`/`--delete` deletes a branch; `-D` force-deletes even unmerged commits, losing work.",
            "confirm the branch is fully merged/pushed before deleting it.",
        )),
        "stash" if matches!(first, "drop" | "clear") => Some(blocked(
            "git stash drop/clear",
            "`git stash drop` / `git stash clear` permanently discards stashed changes with no recovery.",
            "apply or inspect the stash first (`git stash show -p`); drop only when you're sure.",
        )),
        "update-ref" if has("-d") => Some(blocked(
            "git update-ref -d",
            "`git update-ref -d` deletes a ref directly, bypassing the usual branch/tag safety — history can become unreachable.",
            "delete branches/tags via `git branch`/`git tag` instead, or confirm the ref is recoverable from a reflog.",
        )),
        "reflog" if first == "delete" => Some(blocked(
            "git reflog delete",
            "`git reflog delete` removes reflog entries, the last safety net for recovering rewritten/lost commits.",
            "avoid pruning the reflog; it's what lets you undo a bad reset/rebase.",
        )),
        "worktree" if first == "remove" => Some(blocked(
            "git worktree remove",
            "`git worktree remove` deletes a linked worktree and any uncommitted changes inside it.",
            "commit or stash inside the worktree first; UmaDev flags the removal so it's conscious.",
        )),
        _ => None,
    }
}

/// The git subcommand and its arguments, past global options — including the
/// ones that take a separate argument (`-C <dir>`, `-c <k=v>`, `--git-dir <p>`).
fn git_subcommand(args: &[Word]) -> Option<(String, &[Word])> {
    let mut i = 0;
    while let Some(word) = args.get(i) {
        let text = word.text.as_str();
        if !text.starts_with('-') {
            return Some((text.to_ascii_lowercase(), &args[i + 1..]));
        }
        let takes_argument = matches!(
            text,
            "-C" | "-c"
                | "--git-dir"
                | "--work-tree"
                | "--namespace"
                | "--super-prefix"
                | "--config-env"
        );
        i += if takes_argument { 2 } else { 1 };
    }
    None
}

fn is_force_push(args: &[Word]) -> bool {
    args.iter().any(|w| {
        let text = w.text.as_str();
        matches!(
            text,
            "--force" | "--force-with-lease" | "--force-if-includes"
        ) || text.starts_with("--force-with-lease=")
            || (text.starts_with('-') && !text.starts_with("--") && text.contains('f'))
            || (text.len() > 1 && text.starts_with('+'))
    })
}

/// A forced `git clean` (`-f`, `-fd`, `-xdf`, `--force`) that is not a dry run.
fn forced_clean(args: &[Word]) -> bool {
    let mut force = false;
    let mut dry_run = false;
    for arg in args {
        let text = arg.text.as_str();
        if let Some(long) = text.strip_prefix("--") {
            force |= long == "force";
            dry_run |= long == "dry-run";
        } else if text.len() > 1 && text.starts_with('-') {
            force |= text.contains('f');
            dry_run |= text.contains('n');
        }
    }
    force && !dry_run
}

fn deletes_branch(args: &[Word]) -> bool {
    args.iter().any(|w| {
        let text = w.text.as_str();
        text == "--delete"
            || (text.starts_with('-') && !text.starts_with("--") && text.contains(['d', 'D']))
    })
}

/// `DROP TABLE` / `DROP DATABASE` sent to a database client: in its arguments
/// (`psql -c`, `mysql -e`, `sqlite3 db '…'`, `docker exec db psql -c …`), a
/// heredoc / here-string, or text piped into it. SQL that is only searched for,
/// committed or written to a file never reaches a client and passes.
fn drops_sql(words: &[Word], cmd: &Cmd, upstream: &[&Cmd]) -> bool {
    let Some(client) = words
        .iter()
        .position(|w| SQL_CLIENTS.contains(&command_name(&w.text).as_str()))
    else {
        return false;
    };
    let mut sql = joined(&words[client + 1..]);
    for body in cmd
        .stdin
        .iter()
        .chain(upstream.iter().flat_map(|up| &up.stdin))
    {
        sql.push(' ');
        sql.push_str(body);
    }
    for up in upstream {
        sql.push(' ');
        sql.push_str(&joined(&up.words));
    }
    let normalized = sql
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    normalized.contains("drop table") || normalized.contains("drop database")
}

fn blocked(pattern: &str, why: &str, fix: &str) -> Decision {
    Decision::block(
        "UD-SEC-002",
        format!(
            "UmaDev: destructive command blocked (UD-SEC-002). The command matches a known \
             catastrophic pattern (`{pattern}`). {why} fix: {fix}"
        ),
    )
}

fn root_rm_blocked() -> Decision {
    Decision::block(
        "UD-SEC-002",
        "UmaDev: destructive command blocked (UD-SEC-002). This is a recursive, forced `rm` \
         targeting the filesystem root, the home directory, or a drive root — every \
         equivalent form is caught (`-rf`, `-fr`, `-r -f`, `--recursive --force`, and `--` \
         separators). fix: scope the deletion to a project-local directory, e.g. \
         `rm -rf ./build` or `rm -rf target/`.",
    )
}

fn rce_blocked() -> Decision {
    Decision::block(
        "UD-SEC-002",
        "UmaDev: remote-code-execution blocked (UD-SEC-002). This runs a network download \
         as code with no integrity check — piped into a shell (`curl … | sh`, `| sudo -E \
         bash -`), as a script file (`bash <(curl …)`), as `-c` code (`sh -c \"$(curl …)\"`), \
         or through PowerShell (`irm … | iex`). fix: download to a file, inspect it, then run \
         it: `curl -fsSL <url> -o s.sh && less s.sh && sh s.sh`.",
    )
}

fn sql_drop_blocked() -> Decision {
    blocked(
        "drop table",
        "`DROP TABLE` / `DROP DATABASE` sent to a database deletes the data irreversibly (`IF EXISTS` only silences the error when it is absent).",
        "put the statement in a reviewed migration, or back up first (`pg_dump` / `mysqldump`) and let the user run it.",
    )
}
