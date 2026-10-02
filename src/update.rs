//! `fleet update`: bring the installed plugin to the newest release, and the
//! cheap "a newer version is out" check the doctor runs.
//!
//! herdr has no update command. Installing again is how a plugin moves:
//! herdr builds the new commit in a temporary checkout and swaps it in at the
//! same plugin root only when the build passes, so a failed update leaves the
//! old fleet working. The root is named after the plugin's id, not its
//! commit, so the `fleet` on an agent's PATH is the new one after the swap.
//!
//! A release is a `vX.Y.Z` tag. Two ways fleet can be installed:
//! - `herdr plugin install patrickkunzke/fleet`: install again, at the newest
//!   tag, through herdr, which shows what it is about to run.
//! - `herdr plugin link <checkout>`: `git pull --ff-only` on `main` and the
//!   build step, `scripts/install.sh`, which herdr does not run for a linked
//!   plugin.
//!
//! Adapted from herdr-projects (`src/update.rs`), Copyright (c) 2026 Elias
//! Stravik, MIT licensed — see NOTICE.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::herdr;

const PLUGIN_ID: &str = "fleet";

/// The doctor's check runs on every doctor and must not hold it up offline.
const QUICK: Duration = Duration::from_secs(5);
const SLOW: Duration = Duration::from_secs(30);

/// How this fleet came to be installed.
#[derive(Debug, Clone, PartialEq)]
pub enum Install {
    /// `herdr plugin install OWNER/REPO`.
    Github { owner: String, repo: String },
    /// `herdr plugin link PATH`.
    Local { root: PathBuf },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Installed {
    pub how: Install,
    /// The manifest's `version`, which is the release's.
    pub version: String,
}

type Version = (u64, u64, u64);

/// `0.2.3` → (0, 2, 3). Anything else (`0.3.0-rc1`, `nightly`) is not a
/// release, so a pre-release tag is never offered as an update.
fn parse(version: &str) -> Option<Version> {
    let mut parts = version.split('.').map(|p| p.parse::<u64>().ok());
    let v = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(v)
}

fn show((a, b, c): Version) -> String {
    format!("{a}.{b}.{c}")
}

/// The highest release in `git ls-remote --tags --refs` output.
fn newest_tag(ls_remote: &str) -> Option<Version> {
    ls_remote
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1)?.strip_prefix("refs/tags/v"))
        .filter_map(parse)
        .max()
}

/// fleet's row in `herdr plugin list --plugin fleet --json`.
fn installed_from(json: &serde_json::Value) -> Result<Installed> {
    let plugin = json["result"]["plugins"]
        .as_array()
        .and_then(|all| all.iter().find(|p| p["plugin_id"] == PLUGIN_ID))
        .context("herdr has no fleet plugin installed")?;
    let source = &plugin["source"];
    let text = |v: &serde_json::Value| v.as_str().map(str::to_string);
    let how = match source["kind"].as_str() {
        Some("github") => Install::Github {
            owner: text(&source["owner"]).context("herdr's record of fleet has no owner")?,
            repo: text(&source["repo"]).context("herdr's record of fleet has no repo")?,
        },
        Some("local") => Install::Local {
            root: text(&plugin["plugin_root"]).context("herdr's record of fleet has no plugin_root")?.into(),
        },
        other => bail!("fleet is installed in a way update does not know: {}", other.unwrap_or("none")),
    };
    // herdr reports the manifest it read when it loaded the plugin. A linked
    // checkout changes under it, by a pull and a build, until herdr restarts:
    // there the manifest on disk is the version that runs.
    let on_disk = match &how {
        Install::Local { root } => std::fs::read_to_string(root.join("herdr-plugin.toml"))
            .ok()
            .and_then(|m| manifest_version(&m)),
        Install::Github { .. } => None,
    };
    Ok(Installed {
        how,
        version: on_disk
            .or_else(|| text(&plugin["version"]))
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").into()),
    })
}

/// The `version = "…"` of a herdr-plugin.toml.
fn manifest_version(manifest: &str) -> Option<String> {
    manifest.lines().find_map(|l| {
        let (key, value) = l.split_once('=')?;
        (key.trim() == "version").then(|| value.trim().trim_matches('"').to_string())
    })
}

fn herdr_bin() -> String {
    std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into())
}

/// How fleet is installed, as herdr has it.
pub fn installed(timeout: Duration) -> Result<Installed> {
    let mut cmd = Command::new(herdr_bin());
    cmd.args(["plugin", "list", "--plugin", PLUGIN_ID, "--json"]);
    let out = herdr::run(cmd, timeout)?;
    if !out.success {
        bail!("`herdr plugin list` failed: {}", out.stderr.trim());
    }
    installed_from(&serde_json::from_str(out.stdout.trim()).context("`herdr plugin list` did not answer in JSON")?)
}

/// The newest release where this install comes from; `None` when there are
/// no release tags yet.
fn latest(how: &Install, timeout: Duration) -> Result<Option<Version>> {
    let mut cmd = Command::new("git");
    match how {
        Install::Github { owner, repo } => {
            cmd.args(["ls-remote", "--tags", "--refs", &format!("https://github.com/{owner}/{repo}.git")]);
        }
        Install::Local { root } => {
            cmd.arg("-C").arg(root).args(["ls-remote", "--tags", "--refs", "origin"]);
        }
    }
    // A prompt for credentials would hang the doctor; a public repo needs none.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    let out = herdr::run(cmd, timeout)?;
    if out.timed_out {
        bail!("git ls-remote timed out");
    }
    if !out.success {
        bail!("git ls-remote failed: {}", out.stderr.trim());
    }
    Ok(newest_tag(&out.stdout))
}

/// For the doctor: the newer release, when there is one. Offline, no git, or
/// no herdr all mean nothing to say.
pub fn newer() -> Option<(String, String)> {
    let installed = installed(QUICK).ok()?;
    let current = parse(&installed.version)?;
    let latest = latest(&installed.how, QUICK).ok()??;
    (latest > current).then(|| (installed.version, show(latest)))
}

/// Why a linked checkout cannot be pulled, or `None` when it can.
fn unpullable(root: &Path) -> Result<Option<String>> {
    let git = |args: &[&str]| -> Result<String> {
        let out = Command::new("git").arg("-C").arg(root).args(args).output().context("could not run git")?;
        if !out.status.success() {
            bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    if branch != "main" {
        return Ok(Some(format!("{} is on {branch}, not main", root.display())));
    }
    if !git(&["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
        return Ok(Some(format!("{} has uncommitted changes", root.display())));
    }
    Ok(None)
}

fn step(what: &str, cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("could not run {what}"))?;
    if !status.success() {
        bail!("{what} failed; the fleet you had is still installed");
    }
    Ok(())
}

/// `fleet update`.
pub fn update(check: bool, yes: bool) -> Result<()> {
    let installed = installed(SLOW)?;
    let current = parse(&installed.version);
    println!("installed: {}", installed.version);
    let Some(latest) = latest(&installed.how, SLOW)? else {
        println!("no release has been published yet");
        return Ok(());
    };
    println!("newest:    {}", show(latest));
    if current.is_some_and(|c| c >= latest) {
        println!("fleet is up to date");
        return Ok(());
    }
    if check {
        println!("\nrun `fleet update` to install it");
        return Ok(());
    }

    let tag = format!("v{}", show(latest));
    match &installed.how {
        Install::Github { owner, repo } => {
            // herdr asks before it runs anything: the commit, the build, the
            // actions. --yes is for a script that has already decided.
            println!("\ninstalling {owner}/{repo} {tag} with herdr…");
            let mut cmd = Command::new(herdr_bin());
            cmd.args(["plugin", "install", &format!("{owner}/{repo}"), "--ref", &tag]);
            if yes {
                cmd.arg("--yes");
            }
            step("herdr plugin install", &mut cmd)?;
        }
        Install::Local { root } => {
            if let Some(why) = unpullable(root)? {
                bail!("{why}. fleet is linked from a checkout, and update pulls main into it");
            }
            println!("\npulling main in {}…", root.display());
            step("git pull", Command::new("git").arg("-C").arg(root).args(["pull", "--ff-only"]))?;
            // The prebuilt binary when main is the release commit, a build
            // otherwise; its own lines say which.
            println!("installing the binary…");
            step("scripts/install.sh", Command::new("sh").current_dir(root).arg("scripts/install.sh"))?;
        }
    }

    println!(
        "\nfleet {} is installed.\n\
         - A fleet view that is already open still runs {}: press q in it, then prefix+f.\n\
         - Agents use the new `fleet` from their next command, and get the new /board skill when they are next started.\n\
         - Run the \"fleet: check the setup\" action to see whether the new version wants anything changed.",
        show(latest),
        installed.version,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_plain_release_is_a_version() {
        assert_eq!(parse("0.2.3"), Some((0, 2, 3)));
        assert_eq!(parse("0.3.0-rc1"), None);
        assert_eq!(parse("1.2"), None);
        assert_eq!(parse("1.2.3.4"), None);
        assert_eq!(parse("nightly"), None);
    }

    #[test]
    fn the_newest_tag_is_compared_as_numbers_and_pre_releases_are_skipped() {
        let out = "a\trefs/tags/v0.9.0\nb\trefs/tags/v0.10.0\nc\trefs/tags/v0.11.0-rc1\nd\trefs/tags/latest\n";
        assert_eq!(newest_tag(out), Some((0, 10, 0)));
        assert_eq!(newest_tag(""), None);
    }

    #[test]
    fn a_github_install_is_read_from_herdrs_list() {
        let json = serde_json::json!({"result": {"plugins": [{
            "plugin_id": "fleet",
            "plugin_root": "/p/github/fleet-abc",
            "version": "0.1.0",
            "source": {"kind": "github", "owner": "patrickkunzke", "repo": "fleet", "resolved_commit": "e4f"},
        }]}});
        assert_eq!(
            installed_from(&json).unwrap(),
            Installed {
                how: Install::Github { owner: "patrickkunzke".into(), repo: "fleet".into() },
                version: "0.1.0".into(),
            }
        );
    }

    #[test]
    fn a_linked_checkout_is_updated_where_it_is() {
        let json = serde_json::json!({"result": {"plugins": [{
            "plugin_id": "fleet",
            "plugin_root": "/code/fleet",
            "version": "0.1.0",
            "source": {"kind": "local"},
        }]}});
        assert_eq!(installed_from(&json).unwrap().how, Install::Local { root: "/code/fleet".into() });
    }

    #[test]
    fn a_linked_checkout_reports_the_version_on_disk_not_herdrs_old_reading() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("herdr-plugin.toml"), "id = \"fleet\"\nmin_herdr_version = \"0.9.1\"\nversion = \"0.2.0\"\n").unwrap();
        let json = serde_json::json!({"result": {"plugins": [{
            "plugin_id": "fleet",
            "plugin_root": root.path(),
            "version": "0.1.0",
            "source": {"kind": "local"},
        }]}});
        assert_eq!(installed_from(&json).unwrap().version, "0.2.0");
    }

    #[test]
    fn no_fleet_in_the_list_says_so() {
        let json = serde_json::json!({"result": {"plugins": []}});
        assert!(installed_from(&json).unwrap_err().to_string().contains("no fleet plugin"));
    }

    #[test]
    fn the_manifest_and_the_crate_carry_the_same_version() {
        // The doctor and update read the manifest's; `fleet --version` and
        // the /board plugin carry the crate's. A release bumps both.
        let manifest = include_str!("../herdr-plugin.toml");
        let line = manifest.lines().find(|l| l.starts_with("version")).expect("a version line");
        assert_eq!(line, format!("version = \"{}\"", env!("CARGO_PKG_VERSION")));
    }
}
