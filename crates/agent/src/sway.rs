//! The focused window of a Sway session, via Sway's IPC socket.
//!
//! Used for streaming sessions, which run their own headless Sway: the focused
//! window is what's on the stream, so it says which app is actually in use
//! (and names games launched through other launchers, like Battle.net).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use protocol::App;
use serde_json::Value;

const MAGIC: &[u8] = b"i3-ipc";
const GET_TREE: u32 = 4;

pub struct Window {
    /// Wayland app_id, or the X11 class for Xwayland windows.
    pub class: String,
    pub title: String,
}

/// The focused window, if Sway is reachable and a titled window has focus.
pub fn focused_window(socket: &Path) -> Option<Window> {
    let tree = get_tree(socket)
        .map_err(|e| tracing::debug!("sway {}: {e}", socket.display()))
        .ok()?;
    find_focused(&tree)
}

/// What to attribute time in `window` to.
pub fn app_for(window: &Window, steam_name: impl FnOnce(u32) -> String) -> App {
    if window.class == "steam" {
        return App {
            id: "steam".into(),
            name: "Steam".into(),
        };
    }
    // Proton names windows after the Steam app id, except for non-Steam shortcuts (steam_app_default)
    if let Some(appid) = window
        .class
        .strip_prefix("steam_app_")
        .and_then(|id| id.parse().ok())
    {
        return App {
            id: format!("steam:{appid}"),
            name: steam_name(appid),
        };
    }
    App {
        id: format!("window:{}", window.title),
        name: window.title.clone(),
    }
}

fn get_tree(socket: &Path) -> std::io::Result<Value> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let mut request = MAGIC.to_vec();
    request.extend_from_slice(&0u32.to_le_bytes());
    request.extend_from_slice(&GET_TREE.to_le_bytes());
    stream.write_all(&request)?;

    let mut header = [0u8; 14];
    stream.read_exact(&mut header)?;
    if &header[..6] != MAGIC {
        return Err(std::io::Error::other("not a sway IPC socket"));
    }
    let len = u32::from_le_bytes(header[6..10].try_into().expect("4 bytes")) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

fn find_focused(node: &Value) -> Option<Window> {
    if node["focused"].as_bool() == Some(true) && node["pid"].is_number() {
        let title = node["name"].as_str().unwrap_or_default().trim();
        if title.is_empty() {
            return None;
        }
        let class = node["app_id"]
            .as_str()
            .or_else(|| node["window_properties"]["class"].as_str())
            .unwrap_or_default();
        return Some(Window {
            class: class.to_string(),
            title: title.to_string(),
        });
    }
    ["nodes", "floating_nodes"]
        .iter()
        .filter_map(|key| node[*key].as_array())
        .flatten()
        .find_map(find_focused)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_focused_window() {
        let tree = serde_json::json!({
            "type": "root", "nodes": [{ "type": "output", "nodes": [{ "type": "workspace", "nodes": [
                { "type": "con", "pid": 1, "focused": false, "name": "Steam", "app_id": null,
                  "window_properties": { "class": "steam" } },
                { "type": "con", "pid": 2, "focused": true, "name": "Heroes of the Storm", "app_id": null,
                  "window_properties": { "class": "steam_app_default" } }
            ]}]}]
        });
        let window = find_focused(&tree).unwrap();
        assert_eq!(window.title, "Heroes of the Storm");
        let app = app_for(&window, |_| unreachable!());
        assert_eq!(
            (app.id.as_str(), app.name.as_str()),
            ("window:Heroes of the Storm", "Heroes of the Storm")
        );
    }

    #[test]
    fn untitled_focus_is_ignored() {
        let tree = serde_json::json!({ "nodes": [{ "type": "con", "pid": 3, "focused": true, "name": null,
            "window_properties": { "class": "steam_app_default" } }] });
        assert!(find_focused(&tree).is_none());
    }

    #[test]
    fn steam_windows() {
        let steam = Window {
            class: "steam".into(),
            title: "Steam Big Picture Mode".into(),
        };
        assert_eq!(app_for(&steam, |_| unreachable!()).id, "steam");
        let game = Window {
            class: "steam_app_1240440".into(),
            title: "Halo".into(),
        };
        let app = app_for(&game, |id| format!("game {id}"));
        assert_eq!(
            (app.id.as_str(), app.name.as_str()),
            ("steam:1240440", "game 1240440")
        );
    }
}
