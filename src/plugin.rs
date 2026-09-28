//! fleet as a herdr plugin: what `herdr-plugin.toml` calls.
//!
//! Small on purpose. herdr runs an action with the workspace it was invoked
//! from in its environment; fleet's part is to find or open the fleet tab in
//! it, and to say plainly what in the setup would get in the way.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::brief;
use crate::herdr::{self, Herdr};

/// The label of the tab the fleet view runs in. How `open` finds it again.
pub const TAB: &str = "fleet";

/// What herdr hands an action about where it was invoked.
#[derive(Debug, Default, PartialEq)]
struct Invoked {
    workspace: Option<String>,
    cwd: Option<String>,
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
    }
}

/// Focus the fleet tab in this workspace, opening it first if there is none.
pub fn open(root: Option<PathBuf>) -> Result<()> {
    let herdr = Herdr::from_env().context("not run by herdr: HERDR_SOCKET_PATH is not set")?;
    let ctx = context();
    let workspace = ctx
        .workspace
        .context("herdr did not say which workspace this was invoked in")?;

    // By the pane, not the tab: a plugin such as herdr-sidebar puts a pane of
    // its own in every tab, so a tab outlives the view that was in it.
    let root = match root.or(ctx.cwd.map(PathBuf::from)) {
        Some(r) => r,
        None => std::env::current_dir()?,
    };
    let exe = std::env::current_exe().context("cannot tell where fleet is installed")?;
    if let Some(pane) = herdr.panes(&workspace)?.into_iter().find(|p| p.label == TAB) {
        // After a herdr restart the pane is back, as a shell: the view that
        // was in it is not. Start it again where it was.
        if herdr.pane_idle(&pane.pane_id) {
            herdr.pane_run(&pane.pane_id, &view_line(&exe, &root))?;
        }
        herdr.tab_focus(&pane.tab_id)?;
        return Ok(());
    }

    let created = herdr.tab_create(&workspace, &root.to_string_lossy(), TAB, true)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !herdr.pane_idle(&created.pane_id) {
        if Instant::now() >= deadline {
            bail!("the fleet tab never came to a prompt");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    herdr.pane_rename(&created.pane_id, TAB)?;
    herdr.pane_run(&created.pane_id, &view_line(&exe, &root))?;
    Ok(())
}

/// When the view quits, the tab `open` made for it goes too — with whatever
/// another plugin added to it — rather than staying behind empty. A view
/// started by hand in a pane of the user's own leaves that pane alone.
pub fn close_own_tab() {
    if std::env::var_os(OWN_TAB).is_none() {
        return;
    }
    if let (Some(herdr), Ok(tab)) = (Herdr::from_env(), std::env::var("HERDR_TAB_ID")) {
        let _ = herdr.tab_close(&tab);
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

    let inside = herdr::inside();
    say(
        true,
        "running in",
        if inside { "a herdr pane".into() } else { "a plain terminal: fleet uses tmux here".into() },
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
        match claude {
            Some(v) => format!("{program} ({v})"),
            None => format!("{program} does not run; set FLEET_CLAUDE to the claude you use"),
        },
    );

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

    println!(
        "\nTo open fleet with a key, add to {}:\n\n  [[keys.command]]\n  key = \"prefix+f\"\n  type = \"plugin_action\"\n  command = \"fleet.open\"\n  description = \"fleet\"",
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
        assert_eq!(c, Invoked { workspace: Some("w1".into()), cwd: None });
    }

    #[test]
    fn the_view_replaces_the_tabs_shell() {
        let line = view_line(Path::new("/opt/fleet/target/release/fleet"), Path::new("/w/it's"));
        assert_eq!(line, r"FLEET_OWN_TAB=1 exec '/opt/fleet/target/release/fleet' tui --root '/w/it'\''s'");
    }
}
