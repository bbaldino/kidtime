//! The focused window of a Sway session, via Sway's IPC socket.
//!
//! Used for streaming sessions, which run their own headless Sway: the focused
//! window is what's on the stream, so it says which app is actually in use
//! (and names games launched through other launchers, like Battle.net).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use protocol::App;
use serde_json::Value;

const MAGIC: &[u8] = b"i3-ipc";
const GET_TREE: u32 = 4;
/// The whole exchange with Sway, however its replies are paced.
const DEADLINE: Duration = Duration::from_secs(2);
/// The largest tree accepted.
const MAX_REPLY: usize = 16 << 20;

pub struct Window {
    /// Wayland app_id, or the X11 class for Xwayland windows.
    pub class: String,
    pub title: String,
    pub pid: u32,
    /// Sway's container id, for closing this one window.
    pub con_id: i64,
}

/// The focused window, if Sway is reachable and a titled window has focus. `uid` is the account whose Sway
/// it is: the socket must be a socket owned by that account.
pub fn focused_window(socket: &Path, uid: u32) -> Option<Window> {
    if let Err(e) = check_socket(socket, uid) {
        tracing::debug!("sway {}: {e}", socket.display());
        return None;
    }
    let tree = get_tree(socket, DEADLINE)
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

/// The path is in a directory the kid owns: it must be a socket (not followed if a symlink) owned by them.
fn check_socket(path: &Path, uid: u32) -> std::io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_socket() {
        return Err(std::io::Error::other("not a socket"));
    }
    if meta.uid() != uid {
        return Err(std::io::Error::other(format!(
            "owned by uid {}, not {uid}",
            meta.uid()
        )));
    }
    Ok(())
}

/// Connects without ever blocking: a listener that never accepts fills its backlog, and a blocking connect
/// would then wait forever. A full backlog (EAGAIN) is an error; a connect still in progress gets at most
/// the time left before `end`.
fn connect_by(socket: &Path, end: Instant) -> std::io::Result<UnixStream> {
    use socket2::{Domain, SockAddr, Socket, Type};
    use std::os::fd::{AsRawFd, OwnedFd};
    let sock = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    sock.set_nonblocking(true)?;
    match sock.connect(&SockAddr::unix(socket)?) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(libc::EINPROGRESS) => {
            let mut pfd = libc::pollfd {
                fd: sock.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            let ms = remaining(end)?.as_millis().min(i32::MAX as u128) as libc::c_int;
            // SAFETY: one valid pollfd, and the count says one
            let ready = unsafe { libc::poll(&mut pfd, 1, ms) };
            if ready < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if ready == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "sway took too long to accept",
                ));
            }
            if let Some(e) = sock.take_error()? {
                return Err(e);
            }
        }
        Err(e) => return Err(e),
    }
    sock.set_nonblocking(false)?;
    Ok(UnixStream::from(OwnedFd::from(sock)))
}

fn get_tree(socket: &Path, deadline: Duration) -> std::io::Result<Value> {
    let end = Instant::now() + deadline;
    let mut stream = connect_by(socket, end)?;
    let mut request = MAGIC.to_vec();
    request.extend_from_slice(&0u32.to_le_bytes());
    request.extend_from_slice(&GET_TREE.to_le_bytes());
    stream.set_write_timeout(Some(remaining(end)?))?;
    stream.write_all(&request)?;

    let mut header = [0u8; 14];
    read_by(&mut stream, &mut header, end)?;
    if &header[..6] != MAGIC {
        return Err(std::io::Error::other("not a sway IPC socket"));
    }
    let len = u32::from_le_bytes(header[6..10].try_into().expect("4 bytes")) as usize;
    if len > MAX_REPLY {
        return Err(std::io::Error::other(format!(
            "reply too large ({len} bytes)"
        )));
    }
    let mut body = vec![0u8; len];
    read_by(&mut stream, &mut body, end)?;
    Ok(serde_json::from_slice(&body)?)
}

/// Time left before `end`, or a timeout error.
fn remaining(end: Instant) -> std::io::Result<Duration> {
    let left = end.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "sway took too long to answer",
        ));
    }
    Ok(left)
}

/// Fills `buf`, failing once `end` passes however the bytes are paced.
fn read_by(stream: &mut UnixStream, buf: &mut [u8], end: Instant) -> std::io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        stream.set_read_timeout(Some(remaining(end)?))?;
        match stream.read(&mut buf[filled..]) {
            Ok(0) => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
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
            pid: node["pid"].as_u64().unwrap_or(0) as u32,
            con_id: node["id"].as_i64().unwrap_or(0),
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

    use std::os::unix::net::UnixListener;

    /// A fake Sway that answers any request with `reply`, `chunk` bytes at a time, `pause` apart.
    fn fake_sway(name: &str, reply: Vec<u8>, chunk: usize, pause: Duration) -> std::path::PathBuf {
        let dir = std::path::PathBuf::from("/tmp/claude-1000/kidtime-sdd");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut request = [0u8; 14];
            let _ = conn.read_exact(&mut request);
            for part in reply.chunks(chunk) {
                if conn.write_all(part).is_err() {
                    return;
                }
                std::thread::sleep(pause);
            }
            std::thread::sleep(Duration::from_secs(5));
        });
        path
    }

    fn message(body: &[u8], claimed_len: u32) -> Vec<u8> {
        let mut m = MAGIC.to_vec();
        m.extend_from_slice(&claimed_len.to_le_bytes());
        m.extend_from_slice(&GET_TREE.to_le_bytes());
        m.extend_from_slice(body);
        m
    }

    use std::sync::mpsc;

    fn my_uid() -> u32 {
        // SAFETY: getuid has no preconditions
        unsafe { libc::getuid() }
    }

    #[test]
    fn a_listener_that_never_accepts_doesnt_block_the_connect() {
        use socket2::{Domain, SockAddr, Socket, Type};
        let dir = std::path::PathBuf::from("/tmp/claude-1000/kidtime-sdd");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("full-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = Socket::new(Domain::UNIX, Type::STREAM, None).unwrap();
        listener.bind(&SockAddr::unix(&path).unwrap()).unwrap();
        listener.listen(0).unwrap();
        // One pending connection fills a backlog of 0; nobody ever accepts
        let pending = UnixStream::connect(&path).unwrap();
        let (tx, rx) = mpsc::channel();
        let p = path.clone();
        std::thread::spawn(move || {
            let start = Instant::now();
            let result = focused_window(&p, my_uid()).is_none();
            let _ = tx.send((result, start.elapsed()));
        });
        let (none, elapsed) = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("connecting must not block");
        assert!(none);
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
        drop((pending, listener));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn only_a_socket_owned_by_the_kid_is_used() {
        let body = br#"{"nodes":[{"focused":true,"pid":5,"id":9,"name":"Game","app_id":"game"}]}"#;
        let socket = fake_sway(
            "owned",
            message(body, body.len() as u32),
            1 << 20,
            Duration::ZERO,
        );
        // Someone else's socket: not even connected to
        assert!(focused_window(&socket, my_uid() + 1).is_none());
        assert!(focused_window(&socket, my_uid()).is_some());
        std::fs::remove_file(&socket).unwrap();
        // A regular file where the socket should be
        let file = socket.with_extension("file");
        std::fs::write(&file, "").unwrap();
        assert!(focused_window(&file, my_uid()).is_none());
        std::fs::remove_file(&file).unwrap();
    }

    #[test]
    fn a_tree_is_read() {
        let body = br#"{"nodes":[]}"#;
        let socket = fake_sway(
            "ok",
            message(body, body.len() as u32),
            1 << 20,
            Duration::ZERO,
        );
        let tree = get_tree(&socket, Duration::from_millis(500)).unwrap();
        assert!(tree["nodes"].is_array());
        std::fs::remove_file(&socket).unwrap();
    }

    #[test]
    fn a_slow_reply_fails_at_the_overall_deadline() {
        // Each byte comes well within a per-read timeout, but the whole reply would take seconds
        let body = vec![b' '; 64];
        let socket = fake_sway("slow", message(&body, 64), 1, Duration::from_millis(50));
        let start = Instant::now();
        assert!(get_tree(&socket, Duration::from_millis(500)).is_err());
        assert!(
            start.elapsed() < Duration::from_millis(1500),
            "{:?}",
            start.elapsed()
        );
        std::fs::remove_file(&socket).unwrap();
    }

    #[test]
    fn an_oversized_reply_is_refused() {
        let socket = fake_sway(
            "big",
            message(b"", (MAX_REPLY + 1) as u32),
            1 << 20,
            Duration::ZERO,
        );
        let start = Instant::now();
        let err = get_tree(&socket, Duration::from_millis(500)).unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
        assert!(start.elapsed() < Duration::from_millis(400));
        std::fs::remove_file(&socket).unwrap();
    }

    #[test]
    fn finds_focused_window() {
        let tree = serde_json::json!({
            "type": "root", "nodes": [{ "type": "output", "nodes": [{ "type": "workspace", "nodes": [
                { "type": "con", "pid": 1, "focused": false, "name": "Steam", "app_id": null,
                  "window_properties": { "class": "steam" } },
                { "type": "con", "pid": 2, "focused": true, "id": 77, "name": "Heroes of the Storm", "app_id": null,
                  "window_properties": { "class": "steam_app_default" } }
            ]}]}]
        });
        let window = find_focused(&tree).unwrap();
        assert_eq!(window.title, "Heroes of the Storm");
        assert_eq!((window.pid, window.con_id), (2, 77));
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
            pid: 0,
            con_id: 0,
        };
        assert_eq!(app_for(&steam, |_| unreachable!()).id, "steam");
        let game = Window {
            class: "steam_app_1240440".into(),
            title: "Halo".into(),
            pid: 0,
            con_id: 0,
        };
        let app = app_for(&game, |id| format!("game {id}"));
        assert_eq!(
            (app.id.as_str(), app.name.as_str()),
            ("steam:1240440", "game 1240440")
        );
    }
}
