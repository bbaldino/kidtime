//! Finds the apps a user is running.
//!
//! Desktop apps come from the systemd scopes GNOME launches them into
//! (`app-gnome-org.freecad.FreeCAD-12345.scope`), resolved to a name through
//! their .desktop file. Steam games run inside Steam's own cgroup, so they are
//! found by the `SteamLaunch ... AppId=N` command line Steam starts them with.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use protocol::App;

use crate::enforce::RunningApp;

/// Launcher prefixes systemd scope names may carry before the app id.
const LAUNCHERS: &[&str] = &["gnome", "flatpak", "kde", "KDE", "xdg"];

pub struct UserApps {
    pub apps: Vec<App>,
    /// Apps running inside one of the configured streaming units: the Steam
    /// client and the games it launched.
    pub streamed_apps: Vec<App>,
    /// Every app in `apps` and `streamed_apps`, with its processes.
    pub running: Vec<RunningApp>,
}

#[derive(Default)]
pub struct Scanner {
    /// Desktop id -> display name, or None for ids that should be ignored.
    desktop_names: HashMap<String, Option<String>>,
    /// (uid, Steam app id) -> game name.
    steam_names: HashMap<(u32, u32), String>,
}

impl Scanner {
    pub fn clear_caches(&mut self) {
        self.desktop_names.clear();
        self.steam_names.clear();
    }

    pub fn scan(&mut self, uid: u32, home: &Path, streaming_units: &[String]) -> UserApps {
        let mut result = UserApps {
            apps: Vec::new(),
            streamed_apps: Vec::new(),
            running: Vec::new(),
        };
        // Built once per scan, and only when a Steam game turns up
        let mut parents: Option<HashMap<u32, u32>> = None;
        let root = PathBuf::from(format!("/sys/fs/cgroup/user.slice/user-{uid}.slice"));
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(entry.path());
                }
            }
            let pids = read_pids(&dir);
            if pids.is_empty() {
                continue;
            }
            let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if let Some(id) = parse_app_scope(dir_name)
                && let Some(name) = self.desktop_name(&id, home)
            {
                push_running(
                    &mut result.running,
                    RunningApp {
                        id: id.clone(),
                        name: name.clone(),
                        pids: pids.clone(),
                        window: None,
                    },
                );
                push_unique(&mut result.apps, App { id, name });
            }

            let streaming = dir
                .components()
                .any(|c| streaming_units.iter().any(|u| c.as_os_str() == u.as_str()));
            for pid in pids {
                if let Some((appid, program)) = steam_game(pid) {
                    let game = App {
                        id: format!("steam:{appid}"),
                        name: self.steam_name(uid, home, appid, &program),
                    };
                    let parents = parents.get_or_insert_with(process_parents);
                    push_running(
                        &mut result.running,
                        RunningApp {
                            id: game.id.clone(),
                            name: game.name.clone(),
                            pids: descendants(pid, parents),
                            window: None,
                        },
                    );
                    if streaming {
                        push_unique(&mut result.streamed_apps, game.clone());
                    }
                    push_unique(&mut result.apps, game);
                } else if streaming && is_steam_client(pid) {
                    // Desktop sessions get Steam from its app scope; streaming sessions have none
                    push_unique(
                        &mut result.streamed_apps,
                        App {
                            id: "steam".into(),
                            name: "Steam".into(),
                        },
                    );
                }
            }
        }
        result.apps.sort_by(|a, b| a.name.cmp(&b.name));
        result
    }

    fn desktop_name(&mut self, id: &str, home: &Path) -> Option<String> {
        self.desktop_names
            .entry(id.to_string())
            .or_insert_with(|| {
                desktop_dirs(home)
                    .iter()
                    .find_map(|dir| fs::read_to_string(dir.join(format!("{id}.desktop"))).ok())
                    .and_then(|contents| parse_desktop_entry(&contents))
            })
            .clone()
    }

    pub fn steam_name(&mut self, uid: u32, home: &Path, appid: u32, program: &str) -> String {
        self.steam_names
            .entry((uid, appid))
            .or_insert_with(|| {
                let libraries = steam_libraries(home);
                libraries
                    .iter()
                    .find_map(|lib| {
                        let manifest = lib.join(format!("steamapps/appmanifest_{appid}.acf"));
                        vdf_values(&fs::read_to_string(manifest).ok()?, "name")
                            .into_iter()
                            .next()
                    })
                    // Non-Steam games added to the library live in shortcuts.vdf instead
                    .or_else(|| {
                        libraries.iter().take(2).find_map(|root| {
                            fs::read_dir(root.join("userdata"))
                                .ok()?
                                .flatten()
                                .find_map(|account| {
                                    let vdf = fs::read(account.path().join("config/shortcuts.vdf"))
                                        .ok()?;
                                    shortcut_name(&vdf, appid)
                                })
                        })
                    })
                    .unwrap_or_else(|| {
                        if program.is_empty() {
                            format!("Steam game {appid}")
                        } else {
                            program.to_string()
                        }
                    })
            })
            .clone()
    }
}

fn push_unique(apps: &mut Vec<App>, app: App) {
    if !apps.iter().any(|a| a.id == app.id || a.name == app.name) {
        apps.push(app);
    }
}

fn push_running(running: &mut Vec<RunningApp>, app: RunningApp) {
    if !running.iter().any(|a| a.id == app.id) {
        running.push(app);
    }
}

/// The pid and every process below it.
pub fn descendants(root: u32, parents: &HashMap<u32, u32>) -> Vec<u32> {
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        let pid = out[i];
        out.extend(
            parents
                .iter()
                .filter(|&(_, &pp)| pp == pid)
                .map(|(&p, _)| p),
        );
        i += 1;
    }
    out
}

fn parse_ppid(stat: &str) -> Option<u32> {
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn process_parents() -> HashMap<u32, u32> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            let ppid = parse_ppid(&fs::read_to_string(e.path().join("stat")).ok()?)?;
            Some((pid, ppid))
        })
        .collect()
}

fn read_pids(cgroup: &Path) -> Vec<u32> {
    fs::read_to_string(cgroup.join("cgroup.procs"))
        .map(|s| s.lines().filter_map(|l| l.parse().ok()).collect())
        .unwrap_or_default()
}

/// `app-gnome-org.freecad.FreeCAD-2058620.scope` -> `org.freecad.FreeCAD`
fn parse_app_scope(name: &str) -> Option<String> {
    let name = unescape(name.strip_prefix("app-")?.strip_suffix(".scope")?);
    // Drop the random/pid suffix
    let (rest, suffix) = name.rsplit_once('-')?;
    if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let id = LAUNCHERS
        .iter()
        .find_map(|l| rest.strip_prefix(l).and_then(|r| r.strip_prefix('-')))
        .unwrap_or(rest);
    (!id.is_empty()).then(|| id.to_string())
}

/// Undo systemd unit-name escaping (`google\x2dchrome` -> `google-chrome`).
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("\\x") {
        out.push_str(&rest[..i]);
        match rest
            .get(i + 2..i + 4)
            .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            Some(b) => {
                out.push(b as char);
                rest = &rest[i + 4..];
            }
            None => {
                out.push_str("\\x");
                rest = &rest[i + 2..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn desktop_dirs(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".local/share/applications"),
        home.join(".local/share/flatpak/exports/share/applications"),
        PathBuf::from("/var/lib/flatpak/exports/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
        PathBuf::from("/usr/share/applications"),
    ]
}

/// The app's display name, or None for entries hidden from app menus
/// (helpers and background services, which aren't worth tracking).
fn parse_desktop_entry(contents: &str) -> Option<String> {
    let mut in_entry = false;
    let mut name = None;
    for line in contents.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        match line.split_once('=').map(|(k, v)| (k.trim(), v.trim())) {
            Some(("NoDisplay" | "Hidden", "true")) => return None,
            Some(("Name", v)) if name.is_none() => name = Some(v.to_string()),
            _ => {}
        }
    }
    name
}

/// Steam starts every game through `reaper SteamLaunch AppId=<id> -- <program> ...`,
/// and the reaper process stays alive as the game's parent until it exits.
/// Returns the app id and the launched program's file name.
fn steam_game(pid: u32) -> Option<(u32, String)> {
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = cmdline
        .split(|&b| b == 0)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    if !args.iter().any(|a| a == "SteamLaunch") {
        return None;
    }
    let appid = args
        .iter()
        .find_map(|a| a.strip_prefix("AppId=")?.parse().ok())
        .filter(|&id| id != 0)?;
    let program = args
        .iter()
        .skip_while(|a| *a != "--")
        .nth(1)
        .and_then(|p| Path::new(p).file_name()?.to_str().map(str::to_string))
        .unwrap_or_default();
    Some((appid, program))
}

fn is_steam_client(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim() == "steam")
}

/// Finds a shortcut's `AppName` in Steam's binary shortcuts.vdf, where each
/// entry starts with `\x02appid\0<u32 LE>` followed by `\x01AppName\0<name>\0`.
fn shortcut_name(vdf: &[u8], appid: u32) -> Option<String> {
    let needle: Vec<u8> = [b"\x02appid\0".as_slice(), &appid.to_le_bytes()].concat();
    let start = vdf.windows(needle.len()).position(|w| w == needle)? + needle.len();
    let rest = &vdf[start..];
    let key_end = rest
        .windows(9)
        .position(|w| w.eq_ignore_ascii_case(b"\x01appname\0"))?
        + 9;
    let name = &rest[key_end..];
    let len = name.iter().position(|&b| b == 0)?;
    Some(String::from_utf8_lossy(&name[..len]).into_owned())
}

fn steam_libraries(home: &Path) -> Vec<PathBuf> {
    let roots = [home.join(".local/share/Steam"), home.join(".steam/steam")];
    let mut libs: Vec<PathBuf> = roots.to_vec();
    for root in &roots {
        if let Ok(vdf) = fs::read_to_string(root.join("steamapps/libraryfolders.vdf")) {
            libs.extend(vdf_values(&vdf, "path").into_iter().map(PathBuf::from));
        }
    }
    libs
}

/// Values of `"key"  "value"` lines in a Valve KeyValues file.
fn vdf_values(contents: &str, key: &str) -> Vec<String> {
    let quoted_key = format!("\"{key}\"");
    contents
        .lines()
        .filter_map(|l| l.trim().strip_prefix(&quoted_key))
        .filter_map(|v| {
            Some(
                v.trim()
                    .strip_prefix('"')?
                    .strip_suffix('"')?
                    .replace("\\\\", "\\"),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_names() {
        assert_eq!(
            parse_app_scope("app-gnome-org.freecad.FreeCAD-2058620.scope").as_deref(),
            Some("org.freecad.FreeCAD")
        );
        assert_eq!(
            parse_app_scope("app-gnome-google\\x2dchrome-145676.scope").as_deref(),
            Some("google-chrome")
        );
        assert_eq!(
            parse_app_scope("app-flatpak-com.bambulab.BambuStudio-4090952777.scope").as_deref(),
            Some("com.bambulab.BambuStudio")
        );
        assert_eq!(
            parse_app_scope("app-com.google.Chrome-145676.scope").as_deref(),
            Some("com.google.Chrome")
        );
        assert_eq!(
            parse_app_scope("app-gnome-Example\\x20Background\\x20Service-35722.scope").as_deref(),
            Some("Example Background Service")
        );
        assert_eq!(parse_app_scope("sway-sunshine.service"), None);
        assert_eq!(
            parse_app_scope("app-dbus\\x2d:1.5\\x2dorg.a11y.atspi.Registry.slice"),
            None
        );
    }

    #[test]
    fn descendants_walks_the_whole_tree() {
        let parents = HashMap::from([(2, 1), (3, 2), (4, 2), (5, 4), (9, 8)]);
        let mut d = descendants(2, &parents);
        d.sort_unstable();
        assert_eq!(d, [2, 3, 4, 5]);
    }

    #[test]
    fn ppid_is_read_after_the_command_name() {
        // The command name can contain spaces and parentheses
        assert_eq!(
            parse_ppid("1234 (Web Content (x)) S 99 1234 1234 0"),
            Some(99)
        );
    }

    #[test]
    fn desktop_entries() {
        let entry = "[Desktop Entry]\nName=FreeCAD\nName[de]=FreeCAD DE\nExec=freecad\n[Desktop Action New]\nName=New\n";
        assert_eq!(parse_desktop_entry(entry).as_deref(), Some("FreeCAD"));
        assert_eq!(
            parse_desktop_entry("[Desktop Entry]\nName=Chrome\nNoDisplay=true\n"),
            None
        );
    }

    #[test]
    fn shortcuts() {
        let vdf = b"\0shortcuts\0\x000\0\x02appid\0\x1e\x8f\xf0\x98\x01AppName\0Battle.net-Setup.exe\0\x01Exe\0x\0\x08\x08";
        assert_eq!(
            shortcut_name(vdf, 0x98f08f1e).as_deref(),
            Some("Battle.net-Setup.exe")
        );
        assert_eq!(shortcut_name(vdf, 1), None);
    }

    #[test]
    fn vdf() {
        let acf =
            "\"AppState\"\n{\n\t\"appid\"\t\t\"1091500\"\n\t\"name\"\t\t\"Cyberpunk 2077\"\n}";
        assert_eq!(vdf_values(acf, "name"), vec!["Cyberpunk 2077"]);
    }
}
