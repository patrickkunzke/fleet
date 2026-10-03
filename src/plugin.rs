//! fleet as a herdr plugin: what `herdr-plugin.toml` calls.
//!
//! Small on purpose. herdr runs an action with the workspace it was invoked
//! from in its environment; fleet's part is to find or open the fleet view in
//! it, and to say plainly what in the setup would get in the way.
//!
//! Three ways in, one place they land: `open` in the workspace you are in,
//! `new` in a workspace it makes for the purpose, and the `workspace.created`
//! hook in any workspace opened at a directory listed in `auto-open`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};

use crate::brief;
use crate::herdr::{self, Created, Herdr};
use crate::host::CHIEF_SHARE;

/// The label of the tab and the pane the fleet view runs in. How `open`
/// finds it again.
pub const TAB: &str = "fleet";

/// What herdr hands an action about where it was invoked.
#[derive(Debug, Default, PartialEq)]
struct Invoked {
    workspace: Option<String>,
    cwd: Option<String>,
    /// The focused pane's own directory, which `cwd` puts second.
    here: Option<String>,
}

fn context() -> Invoked {
    let json = std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .unwrap_or(serde_json::Value::Null);
    context_from(&json, std::env::var("HERDR_WORKSPACE_ID").ok())
}

fn context_from(json: &serde_json::Value, env_workspace: Option<String>) -> Invoked {
    let text = |k: &str| json[k].as_str().filter(|s| !s.is_empty()).map(String::from);
    Invoked {
        workspace: text("workspace_id").or(env_workspace.filter(|w| !w.is_empty())),
        // The workspace's own directory before the focused pane's: a
        // workspace opened at the landscape is the fleet's root, and the
        // pane in front may be one agent's repository inside it.
        cwd: text("workspace_cwd").or_else(|| text("focused_pane_cwd")),
        here: text("focused_pane_cwd"),
    }
}

/// Focus the fleet view in this workspace, opening it first if there is none.
pub fn open(root: Option<PathBuf>) -> Result<()> {
    let herdr = Herdr::from_env().context("not run by herdr: HERDR_SOCKET_PATH is not set")?;
    let ctx = context();
    let workspace = ctx
        .workspace
        .context("herdr did not say which workspace this was invoked in")?;
    let root = match root.or(ctx.cwd.map(PathBuf::from)) {
        Some(r) => r,
        None => std::env::current_dir()?,
    };
    open_in(&herdr, &workspace, &root, None, true)
}

/// A new workspace in fleet mode: the fleet view in its first tab, and the
/// chief on its left once the view has started one. At the directory given,
/// or the focused pane's: where you are, not the workspace you are in.
pub fn new(root: Option<PathBuf>) -> Result<()> {
    let herdr = Herdr::from_env().context("not run by herdr: HERDR_SOCKET_PATH is not set")?;
    let ctx = context();
    let root = match root.or(ctx.here.map(PathBuf::from)).or(ctx.cwd.map(PathBuf::from)) {
        Some(r) => r,
        None => std::env::current_dir()?,
    };
    let root = root
        .canonicalize()
        .with_context(|| format!("no such directory: {}", root.display()))?;
    let label = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| TAB.into());
    let created = herdr.workspace_create(&root.to_string_lossy(), &label, true)?;
    open_in(&herdr, &created.workspace_id, &root, Some(&created), true)
}

/// What the `workspace.created` hook runs: fleet mode, for a workspace
/// opened at a directory listed in `auto-open`, and nothing for any other.
/// Every workspace would be one Claude session too many.
pub fn event() -> Result<()> {
    let herdr = Herdr::from_env().context("not run by herdr: HERDR_SOCKET_PATH is not set")?;
    let json = std::env::var("HERDR_PLUGIN_EVENT_JSON")
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .unwrap_or(serde_json::Value::Null);
    let Some(workspace) = find_text(&json, "workspace_id")
        .or_else(|| std::env::var("HERDR_WORKSPACE_ID").ok().filter(|w| !w.is_empty()))
    else {
        return Ok(());
    };
    // herdr's workspace has no directory of its own; its first pane does.
    // The fleet pane's, when there is one: a workspace herdr restores comes
    // back with it, and its other panes may be anywhere.
    // The hook runs as the workspace is born, before its first pane has
    // been listed with a directory: wait for one rather than give up.
    let deadline = Instant::now() + Duration::from_secs(5);
    let panes = loop {
        let panes = herdr.panes(&workspace)?;
        if panes.iter().any(|p| !p.cwd.is_empty()) || Instant::now() >= deadline {
            break panes;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let Some(first) = panes
        .iter()
        .find(|p| p.label == TAB)
        .or_else(|| panes.iter().find(|p| p.label.is_empty()))
    else {
        return Ok(());
    };
    let root = PathBuf::from(&first.cwd);
    if !auto_open_roots().iter().any(|r| same_dir(r, &root)) {
        return Ok(());
    }
    // A workspace just made has one tab with a shell in it, and the view
    // takes that shell rather than leaving it beside a tab of its own.
    let fresh = (first.label.is_empty() && herdr.tabs(&workspace)?.len() == 1).then(|| Created {
        workspace_id: workspace.clone(),
        tab_id: first.tab_id.clone(),
        pane_id: first.pane_id.clone(),
    });
    open_in(&herdr, &workspace, &root, fresh.as_ref(), false)
}

/// Find or start the fleet view in `workspace`. `fresh` is a workspace's
/// first pane for the view to take over.
fn open_in(herdr: &Herdr, workspace: &str, root: &Path, fresh: Option<&Created>, focus: bool) -> Result<()> {
    // `new` makes a workspace, and the hook hears it made: without this,
    // both would open a fleet in it.
    let Some(_opening) = Opening::take(workspace) else { return Ok(()) };
    let exe = std::env::current_exe().context("cannot tell where fleet is installed")?;

    // By the pane, not the tab: a plugin such as herdr-sidebar puts a pane of
    // its own in every tab, so a tab outlives the view that was in it.
    if let Some(pane) = herdr.panes(workspace)?.into_iter().find(|p| p.label == TAB) {
        // After a herdr restart the pane is back, as a shell: the view that
        // was in it is not. Start it again where it was.
        if herdr.pane_idle(&pane.pane_id) {
            herdr.pane_run(&pane.pane_id, &view_line(&exe, root))?;
        }
        if focus {
            herdr.tab_focus(&pane.tab_id)?;
        }
        return Ok(());
    }

    let root_text = root.to_string_lossy();
    let fleet = crate::scope::board_for_root(root).ok().and_then(|b| crate::scope::fleet_of(&b));
    let chief_name = crate::host::herdr_agent_name(fleet.as_deref(), "chief");
    let chief = herdr
        .agents()?
        .into_iter()
        .find(|a| a.name == chief_name && a.workspace_id == workspace);
    let pane = match (fresh, chief) {
        (Some(created), _) => {
            herdr.tab_rename(&created.tab_id, TAB)?;
            created.pane_id.clone()
        }
        // The view was closed and the chief kept on: back beside it, on
        // the right where it was. The chief keeps its share on the left.
        (None, Some(chief)) => herdr.pane_split(&chief.pane_id, &root_text, CHIEF_SHARE, focus)?,
        (None, None) => herdr.tab_create(workspace, &root_text, TAB, focus)?.pane_id,
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !herdr.pane_idle(&pane) {
        if Instant::now() >= deadline {
            bail!("the fleet pane never came to a prompt");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    herdr.pane_rename(&pane, TAB)?;
    herdr.pane_run(&pane, &view_line(&exe, root))?;
    Ok(())
}

/// A claim on opening the view in one workspace, given up when dropped. A
/// claim older than a minute is from an `open` that died holding it.
struct Opening(PathBuf);

impl Opening {
    fn take(workspace: &str) -> Option<Opening> {
        let dir = crate::scope::home();
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("opening-{}", workspace.replace(['/', ':'], "-")));
        for _ in 0..2 {
            match std::fs::File::create_new(&path) {
                Ok(_) => return Some(Opening(path)),
                Err(_) => {
                    let age = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok());
                    if age.is_some_and(|a| a < Duration::from_secs(60)) {
                        return None;
                    }
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        None
    }
}

impl Drop for Opening {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The first string under `key` anywhere in `json`. herdr's event payloads
/// nest the workspace differently from one event to the next.
fn find_text(json: &serde_json::Value, key: &str) -> Option<String> {
    match json {
        serde_json::Value::Object(map) => map
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| map.values().find_map(|v| find_text(v, key))),
        serde_json::Value::Array(all) => all.iter().find_map(|v| find_text(v, key)),
        _ => None,
    }
}

/// Where the list of directories that open in fleet mode lives.
pub fn auto_open_path() -> PathBuf {
    crate::scope::home().join("auto-open")
}

fn auto_open_roots() -> Vec<PathBuf> {
    let text = std::fs::read_to_string(auto_open_path()).unwrap_or_default();
    roots_from(&text, &std::env::var("HOME").unwrap_or_default())
}

/// One directory a line; `#` starts a comment, and `~` is the home directory.
fn roots_from(text: &str, home: &str) -> Vec<PathBuf> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(|l| match l.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => PathBuf::from(format!("{home}{rest}")),
            _ => PathBuf::from(l),
        })
        .collect()
}

/// The same directory, however it was spelled: macOS's /tmp is /private/tmp,
/// and herdr reports the resolved one.
fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// When the view quits, the tab `open` made for it goes too — with whatever
/// another plugin added to it — rather than staying behind empty. With the
/// chief beside it, only the view's own pane goes: the chief is still
/// working. A view started by hand in a pane of the user's own leaves that
/// pane alone.
pub fn close_own_tab() {
    if std::env::var_os(OWN_TAB).is_none() {
        return;
    }
    let Some(herdr) = Herdr::from_env() else { return };
    let Ok(tab) = std::env::var("HERDR_TAB_ID") else { return };
    let shared = herdr.agents().is_ok_and(|all| all.iter().any(|a| a.tab_id == tab));
    match std::env::var("HERDR_PANE_ID") {
        Ok(pane) if shared => {
            let _ = herdr.pane_close(&pane);
        }
        _ => {
            let _ = herdr.tab_close(&tab);
        }
    }
}

/// Set on the view `open` starts, so it knows the tab is its own.
const OWN_TAB: &str = "FLEET_OWN_TAB";

/// `exec`, so that quitting the view ends the tab's shell with it.
fn view_line(exe: &Path, root: &Path) -> String {
    format!(
        "{OWN_TAB}=1 exec {} tui --root {}",
        brief::quote(&exe.to_string_lossy()),
        brief::quote(&root.to_string_lossy())
    )
}

/// The first Claude Code that loads mods.
const MODS_SINCE: (u64, u64, u64) = (2, 1, 287);

/// Whether the agents' sessions will load fleet's mod, from what `claude
/// --version` printed and the user's Claude Code settings, and what to say.
fn mods(version: &str, settings: &str) -> (bool, String) {
    let (major, minor, patch) = MODS_SINCE;
    // `2.1.287 (Claude Code)`: the number is the first word.
    let number = version.split_whitespace().next().unwrap_or("");
    if !herdr::at_least(number, MODS_SINCE) {
        return (
            false,
            format!(
                "Claude Code {number} has none, so messages are typed into the agent's pane on one line. Update Claude Code to {major}.{minor}.{patch} or newer"
            ),
        );
    }
    let off = serde_json::from_str::<serde_json::Value>(settings)
        .ok()
        .and_then(|s| s.get("disableAllHooks")?.as_bool())
        .unwrap_or(false);
    if off {
        return (
            false,
            format!(
                "turned off by disableAllHooks in {}, so messages are typed into the agent's pane on one line",
                crate::registry::claude_home().join("settings.json").display()
            ),
        );
    }
    (true, "on — messages arrive when an agent's turn has ended".into())
}

/// What in this machine's setup would stop fleet working inside herdr.
pub fn doctor() -> Result<()> {
    let mut problems = 0;
    let mut say = |ok: bool, what: &str, detail: String| {
        if !ok {
            problems += 1;
        }
        println!("{} {what:<18} {detail}", if ok { "ok  " } else { "FIX " });
    };

    let version = std::process::Command::new(std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into()))
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    say(
        version.is_some(),
        "herdr",
        version.unwrap_or_else(|| "not found on PATH".into()),
    );

    // herdr before 0.9.1 moves its own focus on `agent focus` but not the
    // client's screen: ↵ on an agent in another tab does nothing you can
    // see. An update leaves the old server running until herdr restarts.
    let server = herdr::server_version();
    let current = server.as_deref().is_some_and(|v| herdr::at_least(v, (0, 9, 1)));
    say(
        current,
        "herdr server",
        match &server {
            Some(v) if current => v.clone(),
            Some(v) => format!(
                "{v}, which cannot switch your screen to an agent's tab. Restart herdr to run the updated server"
            ),
            None => "not running".into(),
        },
    );

    // Outside herdr the crew runs as Claude Code background sessions, which
    // need a Claude Code that has them.
    let inside = herdr::inside();
    let background = !inside && crate::background::Background::detect().available();
    say(
        inside || background,
        "running in",
        if inside {
            "a herdr pane: agents get tabs of their own".into()
        } else if background {
            "a plain terminal: agents run as Claude Code background sessions (`claude agents`)".into()
        } else {
            "a plain terminal, and `claude agents` does not answer: update Claude Code, or run fleet in herdr".into()
        },
    );

    let program = brief::claude_program();
    let claude = std::process::Command::new(&program)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    say(
        claude.is_some(),
        "claude",
        match &claude {
            Some(v) => format!("{program} ({v})"),
            None => format!("{program} does not run; set FLEET_CLAUDE to the claude you use"),
        },
    );

    // The mod that hands an agent its messages once its turn has ended.
    // Without it they are still delivered, typed into the pane on one line,
    // so this is worth fixing rather than fatal.
    if let Some(v) = &claude {
        let settings = std::fs::read_to_string(crate::registry::claude_home().join("settings.json")).unwrap_or_default();
        let (ok, detail) = mods(v, &settings);
        say(ok, "mods", detail);
    }

    // herdr's own resume starts plain `claude --resume <id>` in each pane:
    // the chief would come back able to edit, and every agent without the
    // role it was given. fleet resumes its crew itself, with both.
    let herdr_resumes = herdr::resumes_agents_itself();
    say(
        !herdr_resumes,
        "herdr resume",
        if herdr_resumes {
            format!(
                "herdr resumes agents itself, without fleet's briefing. Add to {}:\n{:24}[session]\n{:24}resume_agents_on_restore = false\n{:24}and bring a crew back with r in the fleet tab instead",
                herdr::config_path().display(),
                "",
                "",
                ""
            )
        } else {
            "off — fleet resumes its crew, briefed".into()
        },
    );

    // fleet tells herdr when a task is blocked, ready for review or done;
    // herdr shows nothing unless told how.
    let delivery = herdr::toast_delivery();
    say(
        delivery != "off",
        "notifications",
        if delivery == "off" {
            format!(
                "herdr shows none, so a blocked task goes unannounced. Add to {}:\n{:24}[ui.toast]\n{:24}delivery = \"system\"   # or \"herdr\" for in-app toasts",
                herdr::config_path().display(),
                "",
                ""
            )
        } else {
            format!("delivered as {delivery}")
        },
    );

    // Directories whose new workspaces open in fleet mode by themselves.
    let roots = auto_open_roots();
    let missing: Vec<_> = roots.iter().filter(|r| !r.is_dir()).collect();
    say(
        missing.is_empty(),
        "auto-open",
        if roots.is_empty() {
            format!("none: list a directory a line in {} to open its workspaces in fleet mode", auto_open_path().display())
        } else if !missing.is_empty() {
            format!(
                "not a directory: {}",
                missing.iter().map(|m| m.display().to_string()).collect::<Vec<_>>().join(", ")
            )
        } else {
            roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", ")
        },
    );

    // Not a problem to fix: a check of what is out.
    if let Some((installed, newest)) = crate::update::newer() {
        println!("note {:<18} fleet {newest} is out, and this is {installed}: run `fleet update`", "update");
    }

    println!(
        "\nTo open fleet with a key, and a new workspace in fleet mode with another, add to {}:\n\n  [[keys.command]]\n  key = \"prefix+f\"\n  type = \"plugin_action\"\n  command = \"fleet.open\"\n  description = \"fleet\"\n\n  [[keys.command]]\n  key = \"prefix+shift+f\"\n  type = \"plugin_action\"\n  command = \"fleet.new\"\n  description = \"new workspace in fleet mode\"",
        herdr::config_path().display()
    );
    if problems > 0 {
        println!("\n{problems} thing{} to fix", if problems == 1 { "" } else { "s" });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_workspace_directory_is_the_root_not_the_pane_in_front() {
        let json = serde_json::json!({
            "workspace_id": "w2",
            "workspace_cwd": "/w",
            "focused_pane_cwd": "/w/service/billing-service",
        });
        let c = context_from(&json, None);
        assert_eq!(c.workspace.as_deref(), Some("w2"));
        assert_eq!(c.cwd.as_deref(), Some("/w"));
    }

    #[test]
    fn without_the_context_the_panes_own_workspace_is_used() {
        let c = context_from(&serde_json::Value::Null, Some("w1".into()));
        assert_eq!(c, Invoked { workspace: Some("w1".into()), cwd: None, here: None });
    }

    #[test]
    fn a_new_workspace_is_made_where_the_focused_pane_is() {
        let json = serde_json::json!({ "workspace_cwd": "/w", "focused_pane_cwd": "/w/service" });
        assert_eq!(context_from(&json, None).here.as_deref(), Some("/w/service"));
    }

    #[test]
    fn the_workspace_is_found_wherever_the_event_puts_it() {
        // As herdr 0.9 sends it on the stream.
        let json = serde_json::json!({
            "event": "workspace_created",
            "data": { "type": "workspace_created", "workspace": { "label": "acme", "workspace_id": "w8" } },
        });
        assert_eq!(find_text(&json, "workspace_id").as_deref(), Some("w8"));
        assert_eq!(find_text(&serde_json::json!({ "workspace_id": "" }), "workspace_id"), None);
    }

    #[test]
    fn the_auto_open_list_takes_comments_and_the_home_directory() {
        let text = "# fleet mode here\n~/Code/acme\n\n/srv/landscape  # the other one\n~other/x\n~\n";
        assert_eq!(
            roots_from(text, "/Users/me"),
            vec![
                PathBuf::from("/Users/me/Code/acme"),
                PathBuf::from("/srv/landscape"),
                PathBuf::from("~other/x"),
                PathBuf::from("/Users/me"),
            ]
        );
    }

    #[test]
    fn a_directory_is_the_same_however_it_is_spelled() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("a");
        std::fs::create_dir(&inner).unwrap();
        assert!(same_dir(&inner, &dir.path().join("a/../a")));
        assert!(!same_dir(&inner, dir.path()));
    }

    #[test]
    fn mods_need_a_claude_code_new_enough_to_load_them() {
        assert!(mods("2.1.287 (Claude Code)", "").0);
        assert!(mods("2.2.0 (Claude Code)", "{}").0);
        let (ok, detail) = mods("2.1.286 (Claude Code)", "");
        assert!(!ok);
        assert!(detail.contains("2.1.286") && detail.contains("2.1.287 or newer"), "{detail}");
    }

    #[test]
    fn mods_turned_off_in_the_settings_are_off() {
        assert!(!mods("2.1.287 (Claude Code)", r#"{ "disableAllHooks": true }"#).0);
        assert!(mods("2.1.287 (Claude Code)", r#"{ "disableAllHooks": false }"#).0);
        assert!(mods("2.1.287 (Claude Code)", "not json").0, "a settings file it cannot read says nothing");
    }

    #[test]
    fn the_view_replaces_the_tabs_shell() {
        let line = view_line(Path::new("/opt/fleet/target/release/fleet"), Path::new("/w/it's"));
        assert_eq!(line, r"FLEET_OWN_TAB=1 exec '/opt/fleet/target/release/fleet' tui --root '/w/it'\''s'");
    }
}
