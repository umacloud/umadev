//! **UD-SEC-026**: server-side environment secrets read in code that ships to
//! the browser.

use super::{extension_of, Decision};

/// Sensitive env var names that must never reach the client.
const SENSITIVE_ENV: &[&str] = &[
    "process.env.secret",
    "process.env.database_url",
    "process.env.db_url",
    "process.env.private_key",
    "process.env.api_key",
    "process.env.jwt_secret",
    "process.env.stripe",
    "process.env.aws_secret",
    "process.env.password",
    "process.env.token",
    "process.env.redis",
];

/// Next.js App Router special files: Server Components (or server route
/// handlers) unless the module opts into the client with `'use client'`.
const APP_ROUTER_FILES: &[&str] = &[
    "page",
    "layout",
    "template",
    "loading",
    "not-found",
    "default",
    "route",
    "head",
    "opengraph-image",
    "twitter-image",
    "icon",
    "apple-icon",
];

/// Exports that only ever run on the server: Next.js data functions and
/// metadata, Remix / React Router `loader` / `action`.
const SERVER_ONLY_EXPORTS: &[&str] = &[
    "getserversideprops",
    "getstaticprops",
    "getstaticpaths",
    "generatemetadata",
    "generatestaticparams",
    "export async function loader",
    "export function loader",
    "export const loader",
    "export async function action",
    "export function action",
    "export const action",
];

/// **UD-SEC-026**: ban server-side env secrets leaked into client bundles.
///
/// `process.env.SECRET_KEY` / `process.env.DATABASE_URL` in a module that ships
/// to the browser can be bundled into the client-side JS, where anyone can read
/// it. Only `NEXT_PUBLIC_*` / `VITE_*` prefixed vars are safe for the client.
///
/// Only client modules are judged: `.vue` / `.svelte` / `.html`, and a `.jsx` /
/// `.tsx` module that is not server code. Server code is a module with a
/// `'use server'` directive, a `.server.*` module, a Next.js App Router special
/// file (`app/**/page.tsx`, `layout.tsx`, …) or an async component without
/// `'use client'`, and a module exporting a server-only data function
/// (`getServerSideProps`, `getStaticProps`, a Remix `loader` / `action`) —
/// reading a server secret there is the correct pattern.
#[must_use]
pub fn check_client_secret_leak(file_path: &str, content: &str) -> Decision {
    if !is_client_module(file_path, content) {
        return Decision::pass();
    }
    let lower = content.to_ascii_lowercase();
    for pattern in SENSITIVE_ENV {
        if lower.contains(pattern) {
            return Decision::block(
                "UD-SEC-026",
                format!(
                    "UmaDev: server secret leaked into client bundle (UD-SEC-026). \
                     `{file_path}` accesses `{pattern}` in client-side code — this \
                     can be bundled into the browser JS where anyone can read it. \
                     Only `NEXT_PUBLIC_*` / `VITE_*` prefixed vars are safe for the \
                     client. Read the secret in server code instead (a Server \
                     Component, a route handler / API route, a loader or action)."
                ),
            );
        }
    }
    Decision::pass()
}

/// Whether the module at `file_path` ships to the browser.
fn is_client_module(file_path: &str, content: &str) -> bool {
    match extension_of(file_path).as_str() {
        "vue" | "svelte" | "html" => true,
        "jsx" | "tsx" => match leading_directive(content) {
            Some(Directive::UseClient) => true,
            Some(Directive::UseServer) => false,
            None => !is_server_module(file_path, content),
        },
        _ => false,
    }
}

/// A `.jsx` / `.tsx` module without a directive that is nonetheless server code.
fn is_server_module(file_path: &str, content: &str) -> bool {
    let normalized = file_path.replace('\\', "/").to_ascii_lowercase();
    let mut segments = normalized.rsplit('/');
    let name = segments.next().unwrap_or("");
    let stem = name.split('.').next().unwrap_or(name);
    let under_app = segments.any(|segment| segment == "app");
    let lower = content.to_ascii_lowercase();
    name.contains(".server.")
        || (under_app && APP_ROUTER_FILES.contains(&stem))
        || lower.contains("export default async function")
        || SERVER_ONLY_EXPORTS
            .iter()
            .any(|export| lower.contains(export))
}

enum Directive {
    UseClient,
    UseServer,
}

/// The `'use client'` / `'use server'` directive that opens a module, after any
/// leading comments.
fn leading_directive(content: &str) -> Option<Directive> {
    let mut rest = content.trim_start_matches('\u{feff}');
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("//") {
            rest = after.split_once('\n').map_or("", |(_, tail)| tail);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, tail)| tail);
        } else {
            break;
        }
    }
    if rest.starts_with("'use client'") || rest.starts_with("\"use client\"") {
        Some(Directive::UseClient)
    } else if rest.starts_with("'use server'") || rest.starts_with("\"use server\"") {
        Some(Directive::UseServer)
    } else {
        None
    }
}
