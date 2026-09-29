//! Which fleet a command belongs to, and so which board it reads and writes.
//!
//! A fleet is one root directory — a landscape such as `~/Code/acme` —
//! with a board of its own at `~/.claude-fleet/fleets/<name>/fleet.db`. There
//! used to be one board for everything, and it leaked: a worker left over
//! from last week, in another workspace, messaged `chief` and the chief of a
//! different fleet put its work on its own board.
//!
//! Membership comes from how a session was started, never from where it
//! stands. Fleet hands every agent it starts its board in `FLEET_DB`; a
//! session fleet did not start has none, and the board refuses it. Guessing
//! from the directory would be wrong exactly when it matters: a repository
//! inside the landscape is not thereby in the landscape's fleet.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// fleet's own directory: briefs, the auto-open list, and every fleet.
pub fn home() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    PathBuf::from(home).join(".claude-fleet")
}

fn fleets_dir() -> PathBuf {
    home().join("fleets")
}

/// The file in a fleet's directory that says which root it is.
const ROOT_FILE: &str = "root";

/// The board of the fleet rooted at `root`, made on first use. The fleet is
/// named after the directory; a second root with the same name gets `-2`.
pub fn board_for_root(root: &Path) -> Result<PathBuf> {
    board_for_root_in(&fleets_dir(), root)
}

fn board_for_root_in(fleets: &Path, root: &Path) -> Result<PathBuf> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let base = crate::host::herdr_safe(
        &root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "root".into()),
    );
    for n in 1.. {
        let name = if n == 1 { base.clone() } else { format!("{base}-{n}") };
        let dir = fleets.join(&name);
        match std::fs::read_to_string(dir.join(ROOT_FILE)) {
            Ok(owner) if Path::new(owner.trim()) == root => return Ok(dir.join("fleet.db")),
            Ok(_) => continue,
            Err(_) => {
                std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
                std::fs::write(dir.join(ROOT_FILE), root.to_string_lossy().as_bytes())?;
                return Ok(dir.join("fleet.db"));
            }
        }
    }
    unreachable!("an unbounded range ends only by returning")
}

/// The board a command works on: the one it was given (`--db`, or the
/// `FLEET_DB` every agent fleet starts carries), or a fleet named with
/// `--fleet`. Anything else is not part of a fleet, and is told so.
pub fn board_for_command(given: Option<PathBuf>, fleet: Option<&str>) -> Result<PathBuf> {
    board_for_command_in(&fleets_dir(), given, fleet)
}

fn board_for_command_in(fleets: &Path, given: Option<PathBuf>, fleet: Option<&str>) -> Result<PathBuf> {
    if let Some(path) = given.filter(|p| !p.as_os_str().is_empty()) {
        return Ok(path);
    }
    if let Some(name) = fleet {
        let path = fleets.join(name).join("fleet.db");
        if !path.is_file() {
            bail!("no fleet called {name}; `fleet fleets` lists them");
        }
        return Ok(path);
    }
    bail!(
        "not part of a fleet: this session was not started by one, so it has no board. \
         Carry on without it. From a shell, name a fleet with --fleet (`fleet fleets` lists them)."
    )
}

/// The fleet a board belongs to, when it is one of fleet's own: the name
/// herdr's agents are qualified with. A board given by path elsewhere — a
/// test's — belongs to none.
pub fn fleet_of(board: &Path) -> Option<String> {
    fleet_of_in(&fleets_dir(), board)
}

fn fleet_of_in(fleets: &Path, board: &Path) -> Option<String> {
    let dir = board.parent()?;
    (dir.parent()? == fleets).then(|| dir.file_name()?.to_str().map(String::from))?
}

/// Every fleet there is: its name and its root.
pub fn fleets() -> Vec<(String, PathBuf)> {
    fleets_in(&fleets_dir())
}

fn fleets_in(fleets: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(fleets) else { return Vec::new() };
    let mut all: Vec<(String, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let root = std::fs::read_to_string(e.path().join(ROOT_FILE)).ok()?;
            Some((e.file_name().to_str()?.to_string(), PathBuf::from(root.trim())))
        })
        .collect();
    all.sort();
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_is_one_fleet_and_the_same_one_every_time() {
        let home = tempfile::tempdir().unwrap();
        let fleets = home.path().join("fleets");
        let root = home.path().join("Code/acme");
        std::fs::create_dir_all(&root).unwrap();
        let board = board_for_root_in(&fleets, &root).unwrap();
        assert_eq!(board, fleets.join("acme/fleet.db"));
        assert_eq!(board_for_root_in(&fleets, &root).unwrap(), board);
        assert_eq!(fleet_of_in(&fleets, &board).as_deref(), Some("acme"));
    }

    #[test]
    fn two_roots_with_one_name_are_two_fleets() {
        let home = tempfile::tempdir().unwrap();
        let fleets = home.path().join("fleets");
        let a = home.path().join("a/acme");
        let b = home.path().join("b/acme");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let first = board_for_root_in(&fleets, &a).unwrap();
        let second = board_for_root_in(&fleets, &b).unwrap();
        assert_ne!(first, second);
        assert_eq!(fleet_of_in(&fleets, &second).as_deref(), Some("acme-2"));
        assert_eq!(fleets_in(&fleets).len(), 2);
    }

    #[test]
    fn a_session_fleet_did_not_start_has_no_board() {
        let home = tempfile::tempdir().unwrap();
        let fleets = home.path().join("fleets");
        let err = board_for_command_in(&fleets, None, None).unwrap_err().to_string();
        assert!(err.contains("not part of a fleet"), "{err}");
        assert!(board_for_command_in(&fleets, None, Some("nowhere")).is_err());
        let given = PathBuf::from("/tmp/x.db");
        assert_eq!(board_for_command_in(&fleets, Some(given.clone()), None).unwrap(), given);
    }

    #[test]
    fn a_board_given_by_path_belongs_to_no_fleet() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(fleet_of_in(&home.path().join("fleets"), Path::new("/tmp/board/fleet.db")), None);
    }
}
