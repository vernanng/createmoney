//! Dev tooling for this workspace. Run via `cargo xtask <command>`.
//!
//! Commands:
//! - `doc-check [commit]` — advisory docs reminder for a commit (default
//!   HEAD). Always exits 0 so it can run as a post-commit hook.
//! - `install-hooks` — symlink the versioned hooks into `.git/hooks`.
//! - `ablate` — S8 ablation test matrix, then the full workspace suite.
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
        _ => {
            eprintln!("usage: cargo xtask <doc-check [commit]|install-hooks|ablate>");
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
    let src = root.join("scripts/hook-post-commit");
    let dst = root.join(".git/hooks/post-commit");
    let _ = fs::remove_file(&dst);
    if let Err(e) = symlink(&src, &dst) {
        eprintln!("install-hooks: symlink {}: {e}", dst.display());
        return 1;
    }
    let exec = fs::Permissions::from_mode(0o755);
    for p in [&dst, &src] {
        if let Err(e) = fs::set_permissions(p, exec.clone()) {
            eprintln!("install-hooks: chmod {}: {e}", p.display());
            return 1;
        }
    }
    println!("installed: .git/hooks/post-commit");
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
            "crates/risk/src/lib.rs",
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
    fn hook_invocation_detected_by_name() {
        assert!(invoked_as_hook(Some("/repo/.git/hooks/post-commit")));
        assert!(invoked_as_hook(Some("post-commit")));
        assert!(!invoked_as_hook(Some("/repo/target/debug/xtask")));
        assert!(!invoked_as_hook(None));
    }
}
