//! `fleet chief`: the chief of staff in the terminal you are in.
//!
//! In herdr the fleet tab starts the chief beside it. Anywhere else this is
//! how a fleet begins: run it in the workspace, and the terminal becomes the
//! chief's Claude Code session, briefed, with the `/board` skill and fleet's
//! mod, on the workspace's board. The agents it starts are background
//! sessions, and the mod draws the fleet in this session: `/fleet`, the band
//! above the prompt, the toasts.
//!
//! The session's id is chosen here and given to Claude Code, which an
//! interactive session honours, so the board knows the chief before it has
//! said a word.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::agent;
use crate::brief;
use crate::db::Db;
use crate::registry::{self, Registry};
use crate::scope;

/// What starting the chief comes to: the arguments for `claude`, and what
/// the board recorded.
#[derive(Debug)]
pub struct Launch {
    pub args: Vec<String>,
    pub run: i64,
    pub session: String,
}

/// Start the chief in this terminal, in `root` or the directory it is run
/// in, and do not come back: the terminal is the chief's from here.
pub fn run(root: Option<PathBuf>, resume: bool) -> Result<()> {
    let root = match root {
        Some(r) => r,
        None => std::env::current_dir()?,
    };
    let root = root.canonicalize().with_context(|| format!("no such directory: {}", root.display()))?;
    let path = scope::board_for_root(&root)?;
    let db = Db::open(&path)?;

    let mut reg = Registry::new(registry::default_dir());
    reg.refresh();
    let live: Vec<String> = reg.sessions().map(|s| s.session_id.clone()).collect();
    if let Some(sid) = live_chief(&db, &live)? {
        bail!(
            "this workspace's chief is already running (session {sid}). \
             Two chiefs would each think the board is theirs: go to that one, or end it first"
        );
    }

    let plugin = brief::write_plugin(&brief::plugin_dir()).ok();
    let launch = prepare(&db, &root, resume, &registry::default_projects_dir(), plugin.as_deref())?;

    let bin = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
    let env = agent::variables(Some(launch.run), Some(&path), bin.as_deref());
    let program = brief::claude_program();
    // The last thing fleet says before the terminal is Claude Code's.
    eprintln!("fleet: chief of {} · session {} · board {}", root.display(), launch.session, path.display());
    exec(&program, &launch.args, &root, &env)
}

/// The board's chief, when its session is one of the live ones.
fn live_chief(db: &Db, live: &[String]) -> Result<Option<String>> {
    Ok(db
        .agents()?
        .into_iter()
        .find(|a| a.role == "chief")
        .and_then(|a| a.session_id)
        .filter(|sid| live.contains(sid)))
}

/// Record the chief on the board and say how to start it. A fresh chief
/// begins a run of its own; `resume` goes back into the last run's chief
/// conversation, and that run.
pub fn prepare(db: &Db, root: &Path, resume: bool, projects: &Path, plugin: Option<&Path>) -> Result<Launch> {
    let root_text = root.to_string_lossy().to_string();
    let brief = brief::chief(root);

    let (run, session, args) = if resume {
        let run = db
            .runs(&root_text)?
            .into_iter()
            .next()
            .context("nothing to resume: this workspace has had no chief yet. Run `fleet chief` without --resume")?;
        let session = run
            .agents
            .iter()
            .find(|a| a.role == "chief")
            .and_then(|a| a.session_id.clone())
            .context("the last run's chief never reported a session")?;
        if !agent::conversation_in(projects, root, &session) {
            bail!("the last chief's conversation is no longer on disk. Run `fleet chief` without --resume");
        }
        let args = brief::resume_args(&brief, &session, plugin);
        (run.id, session, args)
    } else {
        let run = db.start_run(&root_text)?;
        let session = new_session_id()?;
        let mut args = vec!["--session-id".to_string(), session.clone()];
        args.extend(brief::launch_args(&brief, plugin));
        (run, session, args)
    };

    db.upsert_agent("chief", Some("chief"), Some(&root_text), Some(&session), None, None)?;
    db.join_run(run, "chief", "chief", &root_text)?;
    db.set_run_session(run, "chief", &session)?;
    let said = if resume { "resumed in a terminal" } else { "started in a terminal" };
    db.log_event("note", Some("chief"), None, None, said, None, None)?;
    Ok(Launch { args, run, session })
}

/// A random version-4 UUID, which is what `--session-id` takes.
fn new_session_id() -> Result<String> {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .context("reading /dev/urandom for a session id")?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]))
}

/// Become `claude`: the terminal, its signals and its exit are the chief's.
fn exec(program: &str, args: &[String], root: &Path, env: &[(String, String)]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(program)
        .args(args)
        .current_dir(root)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .exec();
    Err(err).with_context(|| format!("starting {program}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_chief_is_on_the_board_with_its_session_before_it_starts() {
        let db = Db::open_in_memory().unwrap();
        let root = Path::new("/w/acme");
        let launch = prepare(&db, root, false, Path::new("/nowhere"), None).unwrap();

        assert_eq!(launch.args[..2], ["--session-id".to_string(), launch.session.clone()]);
        assert!(launch.args.contains(&"--append-system-prompt".to_string()));
        assert_eq!(&launch.args[launch.args.len() - 4..], ["--disallowed-tools", "Edit", "Write", "NotebookEdit"]);
        let chief = db.agents().unwrap().into_iter().find(|a| a.role == "chief").unwrap();
        assert_eq!(chief.session_id.as_deref(), Some(launch.session.as_str()));
        let run = db.run(launch.run).unwrap().unwrap();
        assert_eq!(run.agents[0].session_id.as_deref(), Some(launch.session.as_str()), "a later resume finds it");
    }

    #[test]
    fn resuming_goes_back_into_the_last_chiefs_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        let root = Path::new("/w/acme");
        let first = prepare(&db, root, false, dir.path(), None).unwrap();

        let err = prepare(&db, root, true, dir.path(), None).unwrap_err().to_string();
        assert!(err.contains("no longer on disk"), "{err}");

        let project = dir.path().join(registry::project_slug(root));
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(format!("{}.jsonl", first.session)), "").unwrap();
        let again = prepare(&db, root, true, dir.path(), None).unwrap();
        assert_eq!(again.session, first.session);
        assert_eq!(again.run, first.run, "the same run, with its crew");
        assert_eq!(again.args[..2], ["--resume".to_string(), first.session.clone()]);
    }

    #[test]
    fn nothing_to_resume_in_a_new_workspace_is_said_plainly() {
        let db = Db::open_in_memory().unwrap();
        let err = prepare(&db, Path::new("/w/new"), true, Path::new("/nowhere"), None).unwrap_err().to_string();
        assert!(err.contains("without --resume"), "{err}");
    }

    #[test]
    fn a_second_chief_is_refused_while_the_first_is_alive() {
        let db = Db::open_in_memory().unwrap();
        let launch = prepare(&db, Path::new("/w/acme"), false, Path::new("/nowhere"), None).unwrap();
        assert_eq!(live_chief(&db, std::slice::from_ref(&launch.session)).unwrap(), Some(launch.session.clone()));
        assert_eq!(live_chief(&db, &[]).unwrap(), None, "a chief whose process is gone is no obstacle");
    }

    #[test]
    fn session_ids_are_uuids() {
        let id = new_session_id().unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(id.as_bytes()[14], b'4');
        assert_ne!(id, new_session_id().unwrap());
    }
}
