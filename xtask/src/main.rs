//! Dev tooling for this workspace. Run via `cargo xtask <command>`.
//!
//! Commands:
//! - `doc-check [commit]` — advisory docs reminder for a commit (default
//!   HEAD). Always exits 0 so it can run as a post-commit hook.
//! - `install-hooks` — install this binary as the `.git/hooks/post-commit`
//!   hook (a symlink; no shell shim).
//! - `ablate` — S8 ablation test matrix, then the full workspace suite.
//! - `autoloop check|ci|ship <title> [body]|deploy` — local paper-soak
//!   loop: judge the running engine's PnL after a 24 h soak (`check`,
//!   exit 3 when losing), run the local CI gates (`ci`), push + PR +
//!   merge the current branch on green local CI (`ship`), and rebuild +
//!   redeploy the local Docker soak from this checkout (`deploy`).
//!   Paper only: config mode stays `paper`, no keys are passed to the
//!   CD stack, and Watchtower is stopped so only this loop deploys.
//!
//! Invoking the binary as `post-commit` runs `doc-check`.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};

fn main() {
    exit(run());
}

fn run() -> i32 {
    let args: Vec<String> = env::args().collect();
    if invoked_as_hook(args.first().map(String::as_str)) {
        return doc_check(None);
    }
    match args.get(1).map(String::as_str) {
        Some("doc-check") => doc_check(args.get(2).map(String::as_str)),
        Some("install-hooks") => install_hooks(),
        Some("ablate") => ablate(),
        Some("autoloop") => autoloop(&args[2..]),
        _ => {
            eprintln!(
                "usage: cargo xtask <doc-check [commit]|install-hooks|ablate|autoloop <check|ci|ship|deploy>>"
            );
            2
        }
    }
}

fn invoked_as_hook(argv0: Option<&str>) -> bool {
    argv0
        .and_then(|a| Path::new(a).file_name())
        .map(|n| n == "post-commit")
        .unwrap_or(false)
}

/// Which `.md` docs a set of changed paths may require updating.
#[derive(Debug, Default, PartialEq, Eq)]
struct DocNeeds {
    venues: bool,
    layout: bool,
    config: bool,
    agents: bool,
}

impl DocNeeds {
    fn any(&self) -> bool {
        self.venues || self.layout || self.config || self.agents
    }
}

fn classify<'a>(files: impl Iterator<Item = &'a str>) -> DocNeeds {
    let mut need = DocNeeds::default();
    for f in files {
        if f.starts_with("crates/feed/")
            || f.starts_with("crates/exec/")
            || f.starts_with("crates/dex-")
            || f.starts_with("crates/strategy/")
        {
            need.venues = true;
        } else if f.starts_with("bins/arb-engine/") || f.starts_with("crates/api/") {
            need.layout = true;
        } else if f == "config.toml" || f == "config.example.toml" {
            need.config = true;
        } else if f.starts_with(".github/") {
            need.agents = true;
        } else if f.ends_with(".md") {
            // Doc-only change: nothing to remind about.
        } else if f.starts_with("crates/") || f.starts_with("bins/") {
            need.layout = true;
        }
    }
    need
}

fn render(need: &DocNeeds) -> Option<String> {
    if !need.any() {
        return None;
    }
    let mut out = String::from("[docs] HEAD touched code \u{2014} consider updating:\n");
    if need.venues {
        out.push_str("  - README.md 'Venues & strategies' table\n");
    }
    if need.layout {
        out.push_str("  - README.md 'Quick start' / 'Layout' sections\n");
    }
    if need.config {
        out.push_str("  - README.md 'Config' + config.example.toml comments\n");
    }
    if need.agents {
        out.push_str("  - AGENTS.md 'Commands' (must match CI)\n");
    }
    Some(out)
}

fn doc_check(commit: Option<&str>) -> i32 {
    let commit = commit.unwrap_or("HEAD");
    let out = match Command::new("git")
        .args(["show", "--name-only", "--pretty=format:", commit, "--"])
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return 0,
    };
    let text = String::from_utf8_lossy(&out.stdout);
    if let Some(msg) = render(&classify(text.split_whitespace())) {
        print!("{msg}");
    }
    0
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives at the workspace root")
        .to_path_buf()
}

fn git_toplevel() -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

#[cfg(unix)]
fn install_hooks() -> i32 {
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};

    let Some(root) = git_toplevel() else {
        eprintln!("install-hooks: not inside a git work tree");
        return 1;
    };
    // The hook is this binary, symlinked under the name `post-commit`.
    // `run` dispatches on that name, so no shell shim is needed. Re-run
    // after changing xtask to refresh the link target profile if desired.
    let src = match env::current_exe().and_then(fs::canonicalize) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("install-hooks: locate own binary: {e}");
            return 1;
        }
    };
    let dst = root.join(".git/hooks/post-commit");
    let _ = fs::remove_file(&dst);
    if let Err(e) = symlink(&src, &dst) {
        eprintln!("install-hooks: symlink {}: {e}", dst.display());
        return 1;
    }
    if let Err(e) = fs::set_permissions(&dst, fs::Permissions::from_mode(0o755)) {
        eprintln!("install-hooks: chmod {}: {e}", dst.display());
        return 1;
    }
    println!("installed: .git/hooks/post-commit -> {}", src.display());
    0
}

#[cfg(not(unix))]
fn install_hooks() -> i32 {
    eprintln!("install-hooks: symlinked git hooks require a unix platform");
    1
}

fn run_cargo(root: &Path, args: &[&str]) -> i32 {
    match Command::new("cargo").args(args).current_dir(root).status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("cargo {}: {e}", args.join(" "));
            1
        }
    }
}

fn ablate() -> i32 {
    let root = workspace_root();
    println!("== S8 ablation matrix (arb-strategy ablate + cross/cex/fee/imbalance/lag/fx) ==");
    let code = run_cargo(&root, &["test", "-p", "arb-strategy", "--", "--nocapture"]);
    if code != 0 {
        return code;
    }
    println!("== full workspace gate ==");
    run_cargo(&root, &["test", "--workspace"])
}

/// Soak length before PnL judges an iteration (owner-set profit bar).
const SOAK_HOURS: f64 = 24.0;

fn autoloop(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("check") => autoloop_check(),
        Some("ci") => autoloop_ci(&workspace_root()),
        Some("ship") => match args.get(1) {
            Some(title) => autoloop_ship(title, args.get(2).map(String::as_str)),
            None => {
                eprintln!("usage: cargo xtask autoloop ship <title> [body]");
                2
            }
        },
        Some("deploy") => autoloop_deploy(&workspace_root()),
        _ => {
            eprintln!("usage: cargo xtask autoloop <check|ci|ship <title> [body]|deploy>");
            2
        }
    }
}

/// Run a command in `root`, inheriting stdio; return its exit code.
fn run_cmd(root: &Path, cmd: &str, args: &[&str]) -> i32 {
    println!("$ {cmd} {}", args.join(" "));
    match Command::new(cmd).args(args).current_dir(root).status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("{cmd}: {e}");
            1
        }
    }
}

/// Capture a git command's stdout in `root` (None on failure).
fn git_out(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The three AGENTS.md gates, in CI order. Stops at the first failure.
fn autoloop_ci(root: &Path) -> i32 {
    for args in [
        vec!["fmt", "--check"],
        vec![
            "clippy",
            "--locked",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
        vec!["test", "--locked", "--workspace"],
    ] {
        let code = run_cargo(root, &args);
        if code != 0 {
            eprintln!("autoloop ci: FAILED at cargo {}", args.join(" "));
            return code;
        }
    }
    println!("autoloop ci: all gates green");
    0
}

/// Field extractors over the `/status` JSON (xtask stays dependency-free;
/// these keys are unique or first-occurrence by construction).
fn extract_num(body: &str, key: &str) -> Option<f64> {
    let pat = format!("\"{key}\":");
    let rest = body.split(&pat).nth(1)?;
    let num: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    num.parse().ok()
}

fn extract_str(body: &str, key: &str) -> Option<f64> {
    let pat = format!("\"{key}\":\"");
    let rest = body.split(&pat).nth(1)?;
    rest.split('"').next()?.parse().ok()
}

/// Profit bar: an iteration is judged only after the soak; before that
/// it is neither winning nor losing.
fn verdict(pnl: f64, soak_h: f64) -> &'static str {
    if soak_h < SOAK_HOURS {
        "SOAKING"
    } else if pnl > 0.0 {
        "PROFITABLE"
    } else {
        "LOSING"
    }
}

fn http_get_status() -> Option<String> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    let mut stream = TcpStream::connect("127.0.0.1:8787").ok()?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok()?;
    stream
        .write_all(b"GET /status HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
        .ok()?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf).ok()?;
    buf.split("\r\n\r\n").nth(1).map(str::to_string)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// One-line soak verdict. Exit 3 = LOSING (caller should iterate),
/// 2 = engine unreachable, 0 = SOAKING or PROFITABLE.
/// shortcut: PnL parsed as f64 for the verdict only; money paths stay
/// Decimal. Upgrade if sub-cent verdicts ever matter.
fn autoloop_check() -> i32 {
    let Some(body) = http_get_status() else {
        eprintln!("autoloop check: engine unreachable at 127.0.0.1:8787");
        return 2;
    };
    let (Some(realized), Some(unrealized), Some(started), Some(trades)) = (
        extract_str(&body, "realized_pnl_quote"),
        extract_str(&body, "unrealized_pnl_quote"),
        extract_num(&body, "started_ms"),
        extract_num(&body, "trades"),
    ) else {
        eprintln!("autoloop check: /status fields missing");
        return 2;
    };
    let pnl = realized + unrealized;
    let soak_h = (now_ms() - started as i64) as f64 / 3_600_000.0;
    let v = verdict(pnl, soak_h);
    println!(
        "autoloop: verdict={v} pnl={pnl:.2} (realized {realized:.2} + unrealized {unrealized:.2}) trades={trades:.0} soak_h={soak_h:.1} bar={SOAK_HOURS:.0}h"
    );
    if v == "LOSING" {
        3
    } else {
        0
    }
}

/// Local CI, then push + PR + merge. The merge gate is the local CI
/// above (owner instruction overrides the review step for loop
/// branches; AGENTS.md still governs everything else).
fn autoloop_ship(title: &str, body: Option<&str>) -> i32 {
    let root = workspace_root();
    if run_cmd(&root, "git", &["status", "--porcelain"]) != 0 {
        return 1;
    }
    if git_out(&root, &["status", "--porcelain"]).is_some_and(|s| !s.is_empty()) {
        eprintln!("autoloop ship: working tree dirty; commit first");
        return 1;
    }
    let Some(branch) = git_out(&root, &["branch", "--show-current"]) else {
        return 1;
    };
    if branch.is_empty() || branch == "main" {
        eprintln!("autoloop ship: on '{branch}'; loop work needs its own branch");
        return 1;
    }
    let code = autoloop_ci(&root);
    if code != 0 {
        return code;
    }
    if run_cmd(&root, "git", &["push", "-u", "origin", &branch]) != 0 {
        return 1;
    }
    let body = body.unwrap_or(
        "Autoloop iteration. Local CI green (fmt/clippy/test); paper soak judges it over 24h.",
    );
    if run_cmd(
        &root,
        "gh",
        &["pr", "create", "--title", title, "--body", body],
    ) != 0
    {
        return 1;
    }
    run_cmd(&root, "gh", &["pr", "merge", "--merge"])
}

/// Local-only CD: build the soak image from this checkout (must hold
/// the merged commit), rotate the previous paper-trade log aside so
/// the iteration starts clean, and restart the CD stack. Watchtower
/// is paused during the swap and resumed before returning.
fn autoloop_deploy(root: &Path) -> i32 {
    let code = run_cmd(
        root,
        "docker",
        &["build", "-t", "ghcr.io/vernanng/createmoney:latest", "."],
    );
    if code != 0 {
        return code;
    }
    let jsonl = root.join("cd-data/paper-trades.jsonl");
    if jsonl.exists() {
        let bak = root.join(format!(
            "cd-data/paper-trades.jsonl.bak-{}",
            now_ms() / 1000
        ));
        if let Err(e) = std::fs::rename(&jsonl, &bak) {
            eprintln!("autoloop deploy: rotate {}: {e}", jsonl.display());
            return 1;
        }
        println!("rotated previous soak log to {}", bak.display());
    }
    let code = run_cmd(
        root,
        "docker",
        &["compose", "-f", "docker-compose.cd.yml", "up", "-d"],
    );
    if code != 0 {
        return code;
    }
    // Pause Watchtower during the swap so a poll cannot race the retag.
    run_cmd(
        root,
        "docker",
        &[
            "compose",
            "-f",
            "docker-compose.cd.yml",
            "stop",
            "watchtower",
        ],
    );
    let mut code = 1;
    for _ in 0..30 {
        if http_get_status().is_some() {
            println!("autoloop deploy: engine healthy at 127.0.0.1:8787");
            code = 0;
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    if code != 0 {
        eprintln!("autoloop deploy: engine did not answer /status within 60s");
    }
    // Resume Watchtower either way: stopped, it silently disables GHCR CD.
    run_cmd(
        root,
        "docker",
        &[
            "compose",
            "-f",
            "docker-compose.cd.yml",
            "start",
            "watchtower",
        ],
    );
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn need(files: &[&str]) -> DocNeeds {
        classify(files.iter().copied())
    }

    #[test]
    fn venues_paths() {
        for f in [
            "crates/feed/src/lib.rs",
            "crates/exec/src/lib.rs",
            "crates/dex-evm/src/lib.rs",
            "crates/dex-sol/src/lib.rs",
            "crates/strategy/src/lib.rs",
        ] {
            assert!(need(&[f]).venues, "{f}");
        }
    }

    #[test]
    fn layout_paths_and_fallback() {
        for f in [
            "bins/arb-engine/src/main.rs",
            "crates/api/src/lib.rs",
            "crates/paper/src/lib.rs",
            "crates/core/src/lib.rs",
            "bins/other/src/main.rs",
        ] {
            assert!(need(&[f]).layout, "{f}");
        }
    }

    #[test]
    fn config_and_agents_paths() {
        assert!(need(&["config.toml"]).config);
        assert!(need(&["config.example.toml"]).config);
        assert!(need(&[".github/workflows/ci.yml"]).agents);
    }

    #[test]
    fn docs_and_unrelated_need_nothing() {
        assert_eq!(need(&["README.md", "docs/plan.md"]), DocNeeds::default());
        assert_eq!(need(&["Cargo.toml"]), DocNeeds::default());
        assert_eq!(need(&[]), DocNeeds::default());
    }

    #[test]
    fn render_matches_shell_wording() {
        let n = DocNeeds {
            venues: true,
            layout: true,
            config: false,
            agents: false,
        };
        assert_eq!(
            render(&n).as_deref(),
            Some(
                "[docs] HEAD touched code \u{2014} consider updating:\n  - README.md 'Venues & strategies' table\n  - README.md 'Quick start' / 'Layout' sections\n"
            )
        );
        assert_eq!(render(&DocNeeds::default()), None);
    }

    #[test]
    fn autoloop_status_fields_and_verdict() {
        let body = r#"{"started_ms":1000,"trades":17,"realized_pnl_quote":"0","portfolio":{"realized_pnl_quote":"1.5","unrealized_pnl_quote":"-2.25"}}"#;
        assert_eq!(extract_num(body, "started_ms"), Some(1000.0));
        assert_eq!(extract_num(body, "trades"), Some(17.0));
        // First occurrence wins: top-level realized, not the portfolio copy.
        assert_eq!(extract_str(body, "realized_pnl_quote"), Some(0.0));
        assert_eq!(extract_str(body, "unrealized_pnl_quote"), Some(-2.25));
        assert_eq!(extract_str(body, "missing"), None);
        assert_eq!(verdict(-5.0, 1.0), "SOAKING");
        assert_eq!(verdict(5.0, 1.0), "SOAKING");
        assert_eq!(verdict(0.01, 24.0), "PROFITABLE");
        assert_eq!(verdict(0.0, 24.0), "LOSING");
        assert_eq!(verdict(-0.01, 48.0), "LOSING");
    }

    #[test]
    fn hook_invocation_detected_by_name() {
        assert!(invoked_as_hook(Some("/repo/.git/hooks/post-commit")));
        assert!(invoked_as_hook(Some("post-commit")));
        assert!(!invoked_as_hook(Some("/repo/target/debug/xtask")));
        assert!(!invoked_as_hook(None));
    }
}
