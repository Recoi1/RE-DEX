//! The games the start screen lists: `<data>/library.json`, one entry per identified dump (X and Y are two).
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use redex_builder::{build_state, BuildState};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GameEntry {
    pub build_id: String,
    pub dump: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Library {
    pub games: Vec<GameEntry>,
}

impl Library {
    /// a missing or broken file is an empty library
    pub fn load(data: &Path) -> Library {
        std::fs::read_to_string(data.join("library.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn save(&self, data: &Path) -> Result<(), String> {
        std::fs::create_dir_all(data).map_err(|e| e.to_string())?;
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        // whole or not at all
        let (tmp, file) = (data.join("library.json.new"), data.join("library.json"));
        std::fs::write(&tmp, text).and_then(|()| std::fs::rename(&tmp, &file)).map_err(|e| e.to_string())
    }

    /// identifies the dump (the builder's Check stage does, off the UI thread); a build id already listed keeps its
    /// entry, with the dump's new place (a moved file)
    pub fn add_with(&mut self, dump: PathBuf, identify: impl Fn(&Path) -> Result<String, String>) -> Result<&GameEntry, String> {
        let build_id = identify(&dump)?;
        let at = match self.games.iter().position(|g| g.build_id == build_id) {
            Some(at) => {
                self.games[at].dump = dump;
                at
            }
            None => {
                self.games.push(GameEntry { build_id, dump });
                self.games.len() - 1
            }
        };
        Ok(&self.games[at])
    }
}

/// what a game's button says: Build, Resume (a build was started), Update (built by older tools) or Ready
pub fn label(data: &Path, build_id: &str, stamp: Option<&str>) -> &'static str {
    match build_state(data, build_id, stamp) {
        BuildState::Missing => "Build",
        BuildState::Partial => "Resume",
        BuildState::Outdated => "Update",
        BuildState::Ready => "Ready",
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("redex-lib-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// the file name says which game: "x.cci" is X, "y.cci" is Y, "x2.cci" is X again
    fn fake_identify(p: &Path) -> Result<String, String> {
        match p.file_name().and_then(|n| n.to_str()) {
            Some(n) if n.starts_with('x') => Ok("x-usa-v0.2.0".into()),
            Some(n) if n.starts_with('y') => Ok("y-usa-v0.2.0".into()),
            _ => Err("not a known dump".into()),
        }
    }

    #[test]
    fn adding_same_dump_twice_keeps_one_entry() {
        let mut lib = Library::default();
        let first = lib.add_with("a/x.cci".into(), fake_identify).unwrap().clone();
        // the same game from another path is the same game: the existing entry comes back
        let again = lib.add_with("b/x2.cci".into(), fake_identify).unwrap().clone();
        assert_eq!(first.build_id, again.build_id);
        assert_eq!(lib.games.len(), 1);
        // the file moved: the entry follows it, else Play would point at a file that is gone
        assert_eq!(lib.games[0].dump, PathBuf::from("b/x2.cci"));
        assert!(lib.add_with("c/z.cci".into(), fake_identify).is_err());
    }

    #[test]
    fn x_and_y_are_two_entries() {
        let mut lib = Library::default();
        lib.add_with("x.cci".into(), fake_identify).unwrap();
        lib.add_with("y.cci".into(), fake_identify).unwrap();
        assert_eq!(lib.games.iter().map(|g| g.build_id.as_str()).collect::<Vec<_>>(), ["x-usa-v0.2.0", "y-usa-v0.2.0"]);
    }

    #[test]
    fn library_survives_restart() {
        let data = temp("restart");
        let mut lib = Library::default();
        lib.add_with("x.cci".into(), fake_identify).unwrap();
        lib.save(&data).unwrap();
        let back = Library::load(&data);
        assert_eq!(back.games, lib.games);
        // no file, or a broken one, is an empty library, never a crash
        assert!(Library::load(&temp("empty")).games.is_empty());
        std::fs::write(data.join("library.json"), "{not json").unwrap();
        assert!(Library::load(&data).games.is_empty());
    }

    #[test]
    fn partial_build_shows_resume() {
        let data = temp("partial");
        let stamp = Some("xr-recomp 0.1.0 1 2\nabi 6");
        assert_eq!(label(&data, "x-usa-v0.2.0", stamp), "Build");
        std::fs::create_dir_all(data.join("build/x-usa-v0.2.0")).unwrap();
        assert_eq!(label(&data, "x-usa-v0.2.0", stamp), "Resume");
        std::fs::write(data.join("build/x-usa-v0.2.0/xgame.dll"), b"").unwrap();
        std::fs::write(data.join("build/x-usa-v0.2.0/verified.txt"), "x-usa-v0.2.0\nxr-recomp 0.1.0 1 2\nabi 6\n").unwrap();
        assert_eq!(label(&data, "x-usa-v0.2.0", stamp), "Ready");
    }

    #[test]
    fn build_by_older_tools_shows_update() {
        let data = temp("outdated");
        std::fs::create_dir_all(data.join("build/x-usa-v0.2.0")).unwrap();
        std::fs::write(data.join("build/x-usa-v0.2.0/xgame.dll"), b"").unwrap();
        // what plan 1's first builds wrote: the build id only
        std::fs::write(data.join("build/x-usa-v0.2.0/verified.txt"), "x-usa-v0.2.0").unwrap();
        assert_eq!(label(&data, "x-usa-v0.2.0", Some("xr-recomp 0.1.0 1 2\nabi 6")), "Update");
        // no tools to build with (a dev tree): a verified build is ready as it is
        assert_eq!(label(&data, "x-usa-v0.2.0", None), "Ready");
    }
}
