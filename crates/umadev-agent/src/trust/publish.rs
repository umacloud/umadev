//! Registry publishes and production deploys — the outward, irrevocable network
//! actions the trust floor confirms on every tier.

/// Whether an already-lowercased command publishes a package / image / release
/// or deploys to production in any of its `;` / `&&` / `||` / `|` segments:
/// `npm|pnpm|yarn|bun publish`, `cargo publish`, `twine upload`, `gem push`,
/// `docker|podman push`, `gh release create`, `vercel --prod`,
/// `netlify deploy --prod`, `firebase deploy`, `fly deploy`. Matched on words
/// after the usual launchers (`sudo`, `env A=B`, `npx`, `bunx`, `pnpm dlx`,
/// `yarn dlx`, `python -m`), so a word that merely appears in an argument or a
/// message is not a publish.
pub(super) fn publishes_or_deploys(cmd: &str) -> bool {
    cmd.split([';', '|', '&', '\n'])
        .any(|segment| segment_publishes(&segment.split_whitespace().collect::<Vec<_>>()))
}

fn segment_publishes(words: &[&str]) -> bool {
    let words = skip_launchers(words);
    let Some((tool, args)) = words.split_first() else {
        return false;
    };
    let tool = tool.rsplit(['/', '\\']).next().unwrap_or(tool);
    let mut positional = args.iter().copied().filter(|arg| !arg.starts_with('-'));
    let first = positional.next().unwrap_or("");
    let second = positional.next().unwrap_or("");
    let has_flag = |flags: &[&str]| args.iter().any(|arg| flags.contains(arg));
    match tool {
        "npm" | "pnpm" | "bun" => first == "publish",
        "yarn" => first == "publish" || (first == "npm" && second == "publish"),
        "cargo" => first == "publish",
        "twine" => first == "upload",
        "gem" => first == "push",
        "docker" | "podman" => first == "push" || (first == "image" && second == "push"),
        "gh" => first == "release" && second == "create",
        "vercel" => has_flag(&["--prod", "--production"]),
        "netlify" | "netlify-cli" => first == "deploy" && has_flag(&["--prod", "--production"]),
        "firebase" | "firebase-tools" => first == "deploy",
        "fly" | "flyctl" => first == "deploy",
        _ => false,
    }
}

/// Drop leading privilege / environment / package-runner words so the real tool
/// is first: `sudo docker push` → `docker push`, `npx vercel --prod` → `vercel
/// --prod`, `python -m twine upload` → `twine upload`.
fn skip_launchers<'a, 'b>(mut words: &'b [&'a str]) -> &'b [&'a str] {
    loop {
        match words {
            [first, rest @ ..]
                if matches!(*first, "sudo" | "doas" | "npx" | "bunx") || first.starts_with('-') =>
            {
                words = rest;
            }
            [first, rest @ ..] if *first == "env" || first.contains('=') => words = rest,
            [first, sub, rest @ ..]
                if matches!(*first, "pnpm" | "yarn" | "bun") && matches!(*sub, "dlx" | "x") =>
            {
                words = rest;
            }
            [first, "-m", rest @ ..] if matches!(*first, "python" | "python3" | "py") => {
                words = rest;
            }
            _ => return words,
        }
    }
}
