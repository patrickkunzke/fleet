//! Typed calls to the herdr CLI.
//!
//! herdr owns the terminals when fleet runs inside it: every agent is a real
//! pane herdr draws, scrolls, selects and pastes into, and fleet only asks it
//! to open one, type a command, name what appears, and knock with a message.
//! Differences between herdr versions stay in this file.
//!
//! Adapted from herdr-projects (`src/herdr.rs`, `src/runner.rs`),
//! Copyright (c) 2026 Elias Stravik, MIT licensed — see NOTICE. What it
//! learned the hard way is kept in the comments where it applies.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;

const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Who fleet says it is when it writes metadata, so herdr keeps fleet's
/// labels apart from another plugin's.
const SOURCE: &str = "fleet";

/// Whether this process is running in a herdr pane. herdr sets both in
/// every pane it opens; either alone is someone's leftover export.
pub fn inside() -> bool {
    std::env::var("HERDR_ENV").is_ok_and(|v| v == "1")
        && std::env::var_os("HERDR_SOCKET_PATH").is_some()
}

/// herdr, bound to one session's socket.
#[derive(Debug, Clone)]
pub struct Herdr {
    bin: String,
    /// The session's API socket. Taken from the pane fleet runs in, so an
    /// agent's `fleet board msg` reaches the session its own pane is in.
    socket: PathBuf,
}

/// A herdr call that failed. `code` is herdr's own (`agent_blocked`,
/// `pane_not_found`, …), or `timeout` / `unreachable` / `failed` when herdr
/// never answered with one.
#[derive(Debug, Clone, PartialEq)]
pub struct HerdrError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for HerdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "herdr: {} ({})", self.message, self.code)
    }
}

impl std::error::Error for HerdrError {}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct AgentSession {
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct Agent {
    pub pane_id: String,
    #[serde(default)]
    pub tab_id: String,
    #[serde(default)]
    pub workspace_id: String,
    #[serde(default)]
    pub name: String,
    /// The kind herdr recognised: `claude`, `codex`, …
    #[serde(default)]
    pub agent: String,
    /// `idle`, `working`, `blocked`, `done` or `unknown`.
    #[serde(default)]
    pub agent_status: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub agent_session: Option<AgentSession>,
}

impl Agent {
    /// The conversation herdr says is running here, once Claude Code has
    /// reported one — which it does only after its first dialog is answered.
    pub fn session_id(&self) -> Option<&str> {
        self.agent_session
            .as_ref()
            .map(|s| s.value.as_str())
            .filter(|v| !v.is_empty())
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct Pane {
    pub pane_id: String,
    #[serde(default)]
    pub tab_id: String,
    /// What `pane rename` set. Other plugins label theirs too
    /// (herdr-sidebar's is `Sidebar`).
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub cwd: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct Tab {
    pub tab_id: String,
    #[serde(default)]
    pub label: String,
}

/// Where a new tab's first pane is.
#[derive(Debug, Clone, PartialEq)]
pub struct Created {
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
}

impl Herdr {
    /// The session this pane belongs to, or none when fleet is not in herdr.
    pub fn from_env() -> Option<Herdr> {
        let socket = std::env::var_os("HERDR_SOCKET_PATH")?;
        let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
        Some(Herdr {
            bin,
            socket: socket.into(),
        })
    }

    /// herdr at a given binary and socket: a test's fake, or the preview's
    /// stand-in, which never calls it.
    #[cfg(test)]
    pub fn with(bin: impl Into<String>, socket: impl Into<PathBuf>) -> Herdr {
        Herdr {
            bin: bin.into(),
            socket: socket.into(),
        }
    }

    /// Runs one herdr command and returns the `result` of its JSON reply.
    pub fn call(&self, args: &[&str], timeout: Duration) -> Result<serde_json::Value, HerdrError> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args)
            // The socket this was built with, and no inherited session name
            // competing with it.
            .env("HERDR_SOCKET_PATH", &self.socket)
            .env_remove("HERDR_SESSION");
        let out = run(cmd, timeout).map_err(|e| HerdrError {
            code: "unreachable".into(),
            message: format!("{e:#}"),
        })?;
        if out.timed_out {
            return Err(HerdrError {
                code: "timeout".into(),
                message: format!("`herdr {}` timed out", args.join(" ")),
            });
        }
        // One JSON object; on failure it carries `error`, and which stream
        // it lands on is not something to depend on.
        let reply = [&out.stdout, &out.stderr]
            .into_iter()
            .find_map(|text| serde_json::from_str::<serde_json::Value>(text.trim()).ok());
        if let Some(reply) = reply {
            if let Some(error) = reply.get("error") {
                return Err(HerdrError {
                    code: error["code"].as_str().unwrap_or("failed").to_string(),
                    message: error["message"].as_str().unwrap_or("").to_string(),
                });
            }
            if out.success {
                return Ok(reply.get("result").cloned().unwrap_or(serde_json::Value::Null));
            }
        } else if out.success && out.stdout.trim().is_empty() {
            // Metadata writes acknowledge with the exit status alone.
            return Ok(serde_json::Value::Null);
        }
        let text = if out.stderr.trim().is_empty() { out.stdout } else { out.stderr };
        Err(HerdrError {
            code: "failed".into(),
            message: format!("`herdr {}`: {}", args.join(" "), text.trim()),
        })
    }

    fn call_as<T: serde::de::DeserializeOwned>(&self, args: &[&str], field: &str) -> Result<T, HerdrError> {
        let result = self.call(args, CALL_TIMEOUT)?;
        serde_json::from_value(result[field].clone()).map_err(|e| HerdrError {
            code: "failed".into(),
            message: format!("`herdr {}` reply changed: {e}", args.join(" ")),
        })
    }

    pub fn agents(&self) -> Result<Vec<Agent>, HerdrError> {
        self.call_as(&["agent", "list"], "agents")
    }

    /// The live agent with this name, if herdr has one.
    pub fn agent(&self, name: &str) -> Option<Agent> {
        self.agents().ok()?.into_iter().find(|a| a.name == name)
    }

    /// A new tab with one shell pane in it, at `cwd`.
    pub fn tab_create(&self, workspace: &str, cwd: &str, label: &str, focus: bool) -> Result<Created, HerdrError> {
        let focus = if focus { "--focus" } else { "--no-focus" };
        let result = self.call(
            &["tab", "create", "--workspace", workspace, "--cwd", cwd, "--label", label, focus],
            CALL_TIMEOUT,
        )?;
        root_pane(&result, "tab")
    }

    /// A new workspace at `cwd`, with one tab and one shell pane in it.
    pub fn workspace_create(&self, cwd: &str, label: &str, focus: bool) -> Result<Created, HerdrError> {
        let focus = if focus { "--focus" } else { "--no-focus" };
        let result = self.call(
            &["workspace", "create", "--cwd", cwd, "--label", label, focus],
            CALL_TIMEOUT,
        )?;
        root_pane(&result, "workspace")
    }

    /// A new shell pane to the right of `pane`, which keeps `ratio` of the
    /// width. Returns the new pane's id.
    pub fn pane_split(&self, pane: &str, cwd: &str, ratio: f32, focus: bool) -> Result<String, HerdrError> {
        let focus = if focus { "--focus" } else { "--no-focus" };
        let ratio = ratio.to_string();
        let result = self.call(
            &["pane", "split", pane, "--direction", "right", "--ratio", &ratio, "--cwd", cwd, focus],
            CALL_TIMEOUT,
        )?;
        result["pane"]["pane_id"].as_str().map(String::from).ok_or_else(|| HerdrError {
            code: "failed".into(),
            message: "herdr's split reply has no pane id".into(),
        })
    }

    /// Trade the places of two panes in their layout.
    pub fn pane_swap(&self, source: &str, target: &str) -> Result<(), HerdrError> {
        self.call(&["pane", "swap", "--source-pane", source, "--target-pane", target], CALL_TIMEOUT)
            .map(|_| ())
    }

    pub fn pane_close(&self, pane: &str) -> Result<(), HerdrError> {
        self.call(&["pane", "close", pane], CALL_TIMEOUT).map(|_| ())
    }

    pub fn tab_rename(&self, tab: &str, label: &str) -> Result<(), HerdrError> {
        self.call(&["tab", "rename", tab, label], CALL_TIMEOUT).map(|_| ())
    }

    pub fn tabs(&self, workspace: &str) -> Result<Vec<Tab>, HerdrError> {
        self.call_as(&["tab", "list", "--workspace", workspace], "tabs")
    }

    pub fn panes(&self, workspace: &str) -> Result<Vec<Pane>, HerdrError> {
        self.call_as(&["pane", "list", "--workspace", workspace], "panes")
    }

    pub fn pane_rename(&self, pane: &str, label: &str) -> Result<(), HerdrError> {
        self.call(&["pane", "rename", pane, label], CALL_TIMEOUT).map(|_| ())
    }

    pub fn tab_close(&self, tab: &str) -> Result<(), HerdrError> {
        self.call(&["tab", "close", tab], CALL_TIMEOUT).map(|_| ())
    }

    pub fn tab_focus(&self, tab: &str) -> Result<(), HerdrError> {
        self.call(&["tab", "focus", tab], CALL_TIMEOUT).map(|_| ())
    }

    /// Types a command into a pane's shell and presses Enter, in one go.
    pub fn pane_run(&self, pane: &str, command: &str) -> Result<(), HerdrError> {
        self.call(&["pane", "run", pane, command], CALL_TIMEOUT).map(|_| ())
    }

    /// The pane's shell process: what the agent started in it descends from,
    /// which is how its Claude Code session is found in the registry.
    pub fn shell_pid(&self, pane: &str) -> Result<i32, HerdrError> {
        let result = self.call(&["pane", "process-info", "--pane", pane], CALL_TIMEOUT)?;
        result["process_info"]["shell_pid"]
            .as_i64()
            .map(|p| p as i32)
            .ok_or_else(|| HerdrError {
                code: "failed".into(),
                message: "herdr's process-info reply has no shell_pid".into(),
            })
    }

    /// Whether the pane's shell is at its prompt with nothing running in it.
    /// Unknown counts as busy: typing a command into a running program is
    /// how a prompt gets a stray line of shell in it.
    pub fn pane_idle(&self, pane: &str) -> bool {
        let Ok(result) = self.call(&["pane", "process-info", "--pane", pane], CALL_TIMEOUT) else {
            return false;
        };
        let info = &result["process_info"];
        let Some(shell) = info["shell_pid"].as_u64() else {
            return false;
        };
        info["foreground_process_group_id"].as_u64() == Some(shell)
            && info["foreground_processes"]
                .as_array()
                .is_some_and(|all| all.iter().all(|p| p["pid"].as_u64() == Some(shell)))
    }

    /// The agent herdr has recognised in a pane, named or not.
    pub fn agent_in(&self, pane: &str) -> Option<Agent> {
        self.agents().ok()?.into_iter().find(|a| a.pane_id == pane)
    }

    pub fn agent_rename(&self, target: &str, name: &str) -> Result<(), HerdrError> {
        self.call(&["agent", "rename", target, name], CALL_TIMEOUT).map(|_| ())
    }

    /// Submits text and Enter as one ordered write, honouring the pane's
    /// bracketed paste. herdr's parser wants positionals first and has no
    /// `--` here; text in the second slot may start with a dash.
    ///
    /// Refused with `agent_blocked` while the agent sits at a question or
    /// permission dialog, before anything is typed.
    pub fn agent_prompt(&self, target: &str, text: &str) -> Result<(), HerdrError> {
        self.call(&["agent", "prompt", target, text], CALL_TIMEOUT).map(|_| ())
    }

    /// What herdr's sidebar shows on the agent's row in place of the agent
    /// kind. Expires after `ttl` unless reported again, so a label outlives
    /// fleet by that long and no longer.
    pub fn report_display(&self, pane: &str, text: &str, ttl: Duration) -> Result<(), HerdrError> {
        let ttl = ttl.as_millis().to_string();
        self.call(
            &["pane", "report-metadata", pane, "--source", SOURCE, "--display-agent", text, "--ttl-ms", &ttl],
            CALL_TIMEOUT,
        )
        .map(|_| ())
    }

    /// A herdr notification: a toast, a desktop notice or nothing, as the
    /// user's `[ui.toast]` says.
    pub fn notify(&self, title: &str, body: &str) -> Result<(), HerdrError> {
        self.call(&["notification", "show", title, "--body", body], CALL_TIMEOUT).map(|_| ())
    }

    pub fn socket(&self) -> &std::path::Path {
        &self.socket
    }

    /// Brings the agent's pane to the front, switching tab if it has to.
    pub fn agent_focus(&self, target: &str) -> Result<(), HerdrError> {
        self.call(&["agent", "focus", target], CALL_TIMEOUT).map(|_| ())
    }

    /// Wait for herdr to recognise the agent a command started in `pane`,
    /// and give it `name`. herdr names agents only when it starts them
    /// itself, and fleet cannot let it: `agent start` runs whichever `claude`
    /// the pane's shell finds first, which on a machine with an old npm
    /// install is not the one the user runs.
    pub fn name_when_up(&self, pane: &str, name: &str, timeout: Duration) -> Option<Agent> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(found) = self.agent_in(pane)
                && (found.name == name || self.agent_rename(pane, name).is_ok())
            {
                return Some(Agent {
                    name: name.to_string(),
                    ..found
                });
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        None
    }

}

/// The first pane of a tab or workspace herdr has just created.
fn root_pane(result: &serde_json::Value, what: &str) -> Result<Created, HerdrError> {
    let pane = &result["root_pane"];
    match (pane["workspace_id"].as_str(), pane["tab_id"].as_str(), pane["pane_id"].as_str()) {
        (Some(w), Some(t), Some(p)) => Ok(Created {
            workspace_id: w.into(),
            tab_id: t.into(),
            pane_id: p.into(),
        }),
        _ => Err(HerdrError {
            code: "failed".into(),
            message: format!("herdr's {what} reply has no root_pane ids"),
        }),
    }
}

/// Whether herdr brings agents back by itself after a restart, which it does
/// unless told not to. It starts plain `claude --resume <id>`: no role, and a
/// chief with its editing tools back.
pub fn resumes_agents_itself() -> bool {
    let Ok(text) = std::fs::read_to_string(config_path()) else {
        return true;
    };
    !setting_is_false(&text, "session", "resume_agents_on_restore")
}

/// The running server's version, which is not the client's: an update
/// replaces the binary and leaves the server that was running as it was,
/// until herdr is restarted.
pub fn server_version() -> Option<String> {
    let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
    let out = Command::new(bin).args(["status", "server"]).output().ok()?;
    version_in(&String::from_utf8_lossy(&out.stdout))
}

fn version_in(status: &str) -> Option<String> {
    status
        .lines()
        .find_map(|l| l.trim().strip_prefix("version:"))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Whether `version` is at least `wanted`, both as `major.minor.patch`.
pub fn at_least(version: &str, wanted: (u64, u64, u64)) -> bool {
    let mut parts = version.split(['.', '-', '+']).map(|p| p.parse::<u64>().unwrap_or(0));
    let got = (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    got >= wanted
}

/// herdr's config file, where `herdr --default-config` says it is.
pub fn config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("HERDR_CONFIG_PATH") {
        return p.into();
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".config/herdr/config.toml")
}

/// Whether `[table] key = false` is set in a TOML text.
fn setting_is_false(text: &str, table: &str, key: &str) -> bool {
    setting(text, table, key).as_deref() == Some("false")
}

/// A value from a TOML text, as written, quotes removed. Read by hand rather
/// than with a TOML parser: these are the only two settings fleet reads, and
/// a dependency for two lines of a file is not worth carrying.
fn setting(text: &str, table: &str, key: &str) -> Option<String> {
    let mut in_table = false;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_table = line == format!("[{table}]");
            continue;
        }
        if !in_table {
            continue;
        }
        if let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return Some(v.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// How herdr delivers a notification: `herdr`, `terminal`, `system`, or
/// `off`, which is its default — fleet's notifications then go nowhere.
pub fn toast_delivery() -> String {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|t| setting(&t, "ui.toast", "delivery"))
        .unwrap_or_else(|| "off".into())
}

struct Output {
    success: bool,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

/// Run to completion or the deadline, whichever is first. The pipes are read
/// on their own threads so a full one cannot deadlock against the wait.
fn run(mut cmd: Command, timeout: Duration) -> Result<Output> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().context("could not run herdr")?;
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_string(&mut text);
            }
            text
        })
    };
    let stdout = read(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let stderr = read(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            break child.wait().ok();
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ok(Output {
        success: !timed_out && status.is_some_and(|s| s.success()),
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
        timed_out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_version_is_read_from_its_status() {
        let status = "status: running\nversion: 0.9.0\nendpoint_compatible: yes\n";
        assert_eq!(version_in(status).as_deref(), Some("0.9.0"));
        assert_eq!(version_in("status: stopped\n"), None);
    }

    #[test]
    fn versions_compare_by_number_not_by_text() {
        assert!(!at_least("0.9.0", (0, 9, 1)), "the server without the focus fix");
        assert!(at_least("0.9.1", (0, 9, 1)));
        assert!(at_least("0.10.0", (0, 9, 1)), "10 is more than 9");
        assert!(at_least("1.0.0-rc1", (0, 9, 1)));
    }
    use std::os::unix::fs::PermissionsExt;

    /// A herdr that answers every call with the given reply, and writes the
    /// arguments it was called with next to itself.
    fn fake(dir: &std::path::Path, reply: &str, exit: i32) -> Herdr {
        let script = dir.join("herdr");
        let log = dir.join("calls");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '%s\\n' \"$HERDR_SOCKET_PATH\" >> '{}'\ncat <<'EOF'\n{reply}\nEOF\nexit {exit}\n",
                log.display(),
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        Herdr::with(script.to_string_lossy(), dir.join("herdr.sock"))
    }

    fn calls(dir: &std::path::Path) -> String {
        std::fs::read_to_string(dir.join("calls")).unwrap_or_default()
    }

    #[test]
    fn a_reply_gives_its_result_and_the_call_goes_to_its_own_socket() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake(
            dir.path(),
            r#"{"id":"x","result":{"agents":[{"pane_id":"w1:p2","name":"chief","agent":"claude","agent_status":"idle","agent_session":{"value":"abc"}}]}}"#,
            0,
        );
        let agents = h.agents().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "chief");
        assert_eq!(agents[0].session_id(), Some("abc"));
        let log = calls(dir.path());
        assert!(log.contains("agent list"), "{log}");
        assert!(log.contains("herdr.sock"), "the socket it was given:\n{log}");
    }

    #[test]
    fn an_error_reply_carries_herdrs_own_code() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake(
            dir.path(),
            r#"{"id":"x","error":{"code":"agent_blocked","message":"at a dialog"}}"#,
            1,
        );
        let err = h.agent_prompt("chief", "go").unwrap_err();
        assert_eq!(err.code, "agent_blocked");
        assert_eq!(err.message, "at a dialog");
    }

    #[test]
    fn an_empty_success_is_a_success() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake(dir.path(), "", 0);
        h.agent_rename("w1:p1", "chief").unwrap();
    }

    #[test]
    fn text_is_passed_as_one_argument_even_with_quotes_and_dashes() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake(dir.path(), r#"{"result":{}}"#, 0);
        h.agent_prompt("chief", "-x it's \"quoted\"").unwrap();
        assert!(calls(dir.path()).contains("agent prompt chief -x it's \"quoted\""));
    }

    #[test]
    fn a_herdr_that_hangs_is_given_up_on() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("herdr");
        std::fs::write(&script, "#!/bin/sh\nsleep 5\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let h = Herdr::with(script.to_string_lossy(), dir.path().join("s"));
        let err = h.call(&["agent", "list"], Duration::from_millis(200)).unwrap_err();
        assert_eq!(err.code, "timeout");
    }

    #[test]
    fn a_tab_reply_names_the_pane_to_run_in() {
        let dir = tempfile::tempdir().unwrap();
        let h = fake(
            dir.path(),
            r#"{"result":{"root_pane":{"workspace_id":"w1","tab_id":"w1:t3","pane_id":"w1:p7"}}}"#,
            0,
        );
        let c = h.tab_create("w1", "/w/repo", "repo", false).unwrap();
        assert_eq!(c.pane_id, "w1:p7");
        let log = calls(dir.path());
        assert!(log.contains("tab create --workspace w1 --cwd /w/repo --label repo --no-focus"), "{log}");
    }

    #[test]
    fn the_resume_setting_is_read_from_its_table_only() {
        assert!(setting_is_false("[session]\nresume_agents_on_restore = false\n", "session", "resume_agents_on_restore"));
        assert!(!setting_is_false("[session]\n# resume_agents_on_restore = false\n", "session", "resume_agents_on_restore"));
        assert!(!setting_is_false("[other]\nresume_agents_on_restore = false\n", "session", "resume_agents_on_restore"));
        assert!(!setting_is_false("[session]\nresume_agents_on_restore = true\n", "session", "resume_agents_on_restore"));
        assert_eq!(setting("[ui.toast]\ndelivery = \"system\"\n", "ui.toast", "delivery").as_deref(), Some("system"));
        assert_eq!(setting("[ui.toast.herdr]\ndelivery = \"x\"\n", "ui.toast", "delivery"), None, "a subtable is not the table");
    }
}
