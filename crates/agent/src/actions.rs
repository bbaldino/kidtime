//! The real enforcement actions: commands and signals, run as root.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::enforce::{Actions, RunningApp};

const DEFAULT_SUNSHINE_PORT: u16 = 47989;
const KILL_AFTER: Duration = Duration::from_secs(10);
const DCONF_FILE: &str = "/etc/dconf/db/gdm.d/90-kidtime";

pub fn shadow_locked(shadow: &str, user: &str) -> Option<bool> {
    shadow.lines().find_map(|l| {
        let mut f = l.split(':');
        (f.next()? == user).then(|| f.next().is_some_and(|hash| hash.starts_with('!')))
    })
}

pub fn sunshine_port(conf: &str) -> u16 {
    conf.lines()
        .find_map(|l| {
            l.split_once('=')
                .filter(|(k, _)| k.trim() == "port")?
                .1
                .trim()
                .parse()
                .ok()
        })
        .unwrap_or(DEFAULT_SUNSHINE_PORT)
}

/// Sunshine uses base-5 to base+21; each account's base is 100 apart.
pub fn nft_block_commands(user: &str, port: u16) -> Vec<String> {
    let (lo, hi) = (port.saturating_sub(5), port.saturating_add(21));
    vec![
        "add table inet kidtime".into(),
        "add chain inet kidtime input { type filter hook input priority -10 ; policy accept ; }"
            .into(),
        format!(
            "add rule inet kidtime input iifname != \"lo\" tcp dport {lo}-{hi} drop comment \"{user}\""
        ),
        format!(
            "add rule inet kidtime input iifname != \"lo\" udp dport {lo}-{hi} drop comment \"{user}\""
        ),
    ]
}

/// A GVariant string literal: single quotes, with backslash, quote and newline escaped.
pub fn gvariant_string(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n");
    format!("'{escaped}'")
}

/// Run through `swaymsg exec`, which hands the string to `sh -c`: quote it for the shell, one line.
pub fn swaynag_command(text: &str) -> String {
    let one_line = text.replace('\n', " ");
    format!(
        "swaynag --layer overlay --edge top -t warning -m '{}'",
        one_line.replace('\'', r"'\''")
    )
}

pub fn dconf_keyfile(lines: &[String]) -> String {
    if lines.is_empty() {
        return "[org/gnome/login-screen]\nbanner-message-enable=false\n".into();
    }
    format!(
        "[org/gnome/login-screen]\nbanner-message-enable=true\nbanner-message-text={}\n",
        gvariant_string(&lines.join("\n"))
    )
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The `inet kidtime` table with rule handles, or None if there is no such table.
fn list_kidtime_table() -> Result<Option<String>> {
    let out = Command::new("nft")
        .args(["-a", "list", "table", "inet", "kidtime"])
        .output()
        .context("running nft")?;
    Ok(out
        .status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned()))
}

pub struct SystemActions {
    /// name -> (uid, home)
    users: HashMap<String, (u32, PathBuf)>,
    streaming_sway_socket: Option<String>,
    /// Processes sent SIGTERM, and when.
    closing: HashMap<u32, Instant>,
}

impl SystemActions {
    #[allow(dead_code)] // used from main in the next task
    pub fn new(
        users: HashMap<String, (u32, PathBuf)>,
        streaming_sway_socket: Option<String>,
    ) -> Self {
        Self {
            users,
            streaming_sway_socket,
            closing: HashMap::new(),
        }
    }

    fn uid(&self, user: &str) -> Result<u32> {
        self.users
            .get(user)
            .map(|(uid, _)| *uid)
            .with_context(|| format!("{user} is not tracked"))
    }

    /// Runs a command as the user, with their session bus.
    fn as_user(&self, user: &str, args: &[&str]) -> Result<()> {
        let uid = self.uid(user)?;
        let runtime = format!("XDG_RUNTIME_DIR=/run/user/{uid}");
        let bus = format!("DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus");
        let mut full = vec!["-u", user, "--", "env", runtime.as_str(), bus.as_str()];
        full.extend_from_slice(args);
        run("runuser", &full)
    }
}

impl Actions for SystemActions {
    fn lock_session(&mut self, session_id: &str) -> Result<()> {
        run("loginctl", &["lock-session", session_id])
    }

    fn login_disabled(&mut self, user: &str) -> Result<bool> {
        let shadow = std::fs::read_to_string("/etc/shadow").context("reading /etc/shadow")?;
        shadow_locked(&shadow, user).with_context(|| format!("{user} not in /etc/shadow"))
    }

    fn disable_login(&mut self, user: &str) -> Result<()> {
        run("usermod", &["-L", user])
    }

    fn enable_login(&mut self, user: &str) -> Result<()> {
        run("usermod", &["-U", user])
    }

    fn notify(&mut self, user: &str, text: &str) {
        let body = gvariant_string(text);
        let desktop = self.as_user(
            user,
            &[
                "gdbus",
                "call",
                "--session",
                "--dest",
                "org.freedesktop.Notifications",
                "--object-path",
                "/org/freedesktop/Notifications",
                "--method",
                "org.freedesktop.Notifications.Notify",
                "Kidtime",
                "0",
                "dialog-warning",
                "Kidtime",
                &body,
                "[]",
                "{'urgency': <byte 2>}",
                "0",
            ],
        );
        if let Err(e) = desktop {
            tracing::debug!("desktop notification for {user}: {e:#}");
        }
        if let (Some(pattern), Ok(uid)) = (self.streaming_sway_socket.clone(), self.uid(user)) {
            let socket = pattern.replace("{uid}", &uid.to_string());
            if std::path::Path::new(&socket).exists() {
                let cmd = swaynag_command(text);
                if let Err(e) = self.as_user(user, &["swaymsg", "-s", &socket, "exec", &cmd]) {
                    tracing::debug!("stream notification for {user}: {e:#}");
                }
            }
        }
    }

    fn close(&mut self, _user: &str, app: &RunningApp) -> Result<()> {
        if let Some(w) = &app.window {
            let criteria = format!("[con_id={}] kill", w.con_id);
            let _ = run("swaymsg", &["-s", &w.socket.to_string_lossy(), &criteria]);
        }
        let now = Instant::now();
        for &pid in &app.pids {
            let signal = match self.closing.get(&pid) {
                Some(&since) if now.duration_since(since) >= KILL_AFTER => libc_kill(pid, 9),
                Some(_) => continue,
                None => {
                    self.closing.insert(pid, now);
                    libc_kill(pid, 15)
                }
            };
            if let Err(e) = signal {
                tracing::debug!("signal to {pid}: {e:#}");
            }
        }
        // Forget processes that have exited
        self.closing
            .retain(|pid, _| std::path::Path::new(&format!("/proc/{pid}")).exists());
        Ok(())
    }

    fn block_stream(&mut self, user: &str) -> Result<()> {
        let (_, home) = self
            .users
            .get(user)
            .with_context(|| format!("{user} is not tracked"))?;
        let conf = std::fs::read_to_string(home.join(".config/sunshine/sunshine.conf"))
            .unwrap_or_default();
        // Rules left from before an agent restart must not be doubled up
        let tag = format!("comment \"{user}\"");
        let already = list_kidtime_table()?
            .is_some_and(|t| t.lines().any(|l| l.contains("drop") && l.contains(&tag)));
        for cmd in nft_block_commands(user, sunshine_port(&conf)) {
            if already && cmd.starts_with("add rule") {
                continue;
            }
            // `add table` and `add chain` are idempotent
            let args: Vec<&str> = cmd.split(' ').collect();
            run("nft", &args)?;
        }
        Ok(())
    }

    fn unblock_stream(&mut self, user: &str) -> Result<()> {
        // Remove this user's rules by handle; drop the table when no user's rule remains
        let Some(text) = list_kidtime_table()? else {
            return Ok(()); // no table: nothing blocked
        };
        let tag = format!("comment \"{user}\"");
        for line in text.lines().filter(|l| l.contains(&tag)) {
            if let Some(handle) = line.rsplit_once("# handle ").map(|(_, h)| h.trim()) {
                run(
                    "nft",
                    &[
                        "delete", "rule", "inet", "kidtime", "input", "handle", handle,
                    ],
                )?;
            }
        }
        let remaining = list_kidtime_table()?.unwrap_or_default();
        if !remaining.lines().any(|l| l.contains("drop comment")) {
            run("nft", &["delete", "table", "inet", "kidtime"])?;
        }
        Ok(())
    }

    fn set_banner(&mut self, lines: &[String]) -> Result<()> {
        // Later login screens: the GDM system settings file (the install script enables it)
        if std::path::Path::new("/etc/dconf/db/gdm.d").is_dir() {
            std::fs::write(DCONF_FILE, dconf_keyfile(lines))?;
            run("dconf", &["update"])?;
        }
        // The login screen running now: as its temporary user, over its session bus
        if let Some((uid, gid)) = greeter_ids() {
            let env = [
                format!("XDG_RUNTIME_DIR=/run/user/{uid}"),
                format!("DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus"),
            ];
            let base = |extra: &[&str]| -> Result<()> {
                let reuid = format!("--reuid={uid}");
                let regid = format!("--regid={gid}");
                let mut args = vec![
                    reuid.as_str(),
                    regid.as_str(),
                    "--clear-groups",
                    "env",
                    env[0].as_str(),
                    env[1].as_str(),
                    "gsettings",
                    "set",
                    "org.gnome.login-screen",
                ];
                args.extend_from_slice(extra);
                run("setpriv", &args)
            };
            if lines.is_empty() {
                base(&["banner-message-enable", "false"])?;
            } else {
                base(&["banner-message-text", &lines.join("\n")])?;
                base(&["banner-message-enable", "true"])?;
            }
        }
        Ok(())
    }
}

/// uid and gid of the process running the login screen (`gnome-shell --mode=gdm`), if one is running.
fn greeter_ids() -> Option<(u32, u32)> {
    std::fs::read_dir("/proc").ok()?.flatten().find_map(|e| {
        let cmdline = std::fs::read(e.path().join("cmdline")).ok()?;
        let args = String::from_utf8_lossy(&cmdline).replace('\0', " ");
        if !args.contains("gnome-shell") || !args.contains("--mode=gdm") {
            return None;
        }
        let status = std::fs::read_to_string(e.path().join("status")).ok()?;
        let id = |key: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(key)?.split_whitespace().next()?.parse().ok())
        };
        Some((id("Uid:")?, id("Gid:")?))
    })
}

fn libc_kill(pid: u32, signal: i32) -> Result<()> {
    run("kill", &[&format!("-{signal}"), &pid.to_string()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_lock_state() {
        let shadow = "root:$6$abc:1::::::\nkid1:!$6$def:1::::::\nkid2:$6$ghi:1::::::\n";
        assert_eq!(shadow_locked(shadow, "kid1"), Some(true));
        assert_eq!(shadow_locked(shadow, "kid2"), Some(false));
        assert_eq!(shadow_locked(shadow, "nobody"), None);
    }

    #[test]
    fn sunshine_port_with_default() {
        assert_eq!(sunshine_port("capture = wlr\nport = 48189\n"), 48189);
        assert_eq!(sunshine_port("capture = wlr\n"), 47989);
        assert_eq!(sunshine_port("port=48089"), 48089);
    }

    #[test]
    fn nft_rules_cover_the_port_range_except_loopback() {
        let cmds = nft_block_commands("kid1", 48189);
        assert!(
            cmds.iter().any(
                |c| c.contains("tcp dport 48184-48210 drop") && c.contains("iifname != \"lo\"")
            )
        );
        assert!(
            cmds.iter()
                .any(|c| c.contains("udp dport 48184-48210 drop"))
        );
        assert!(
            cmds.iter()
                .all(|c| c.contains("comment \"kid1\"") || !c.contains("dport"))
        );
    }

    #[test]
    fn text_is_escaped_for_each_destination() {
        let text = "until 7:00pm (Sam's \"party\")\nsecond line";
        assert_eq!(
            gvariant_string(text),
            r#"'until 7:00pm (Sam\'s "party")\nsecond line'"#
        );
        let cmd = swaynag_command(text);
        assert!(!cmd.contains('\n'));
        assert!(cmd.starts_with("swaynag --layer overlay --edge top -t warning -m "));
        assert_eq!(
            dconf_keyfile(&[]),
            "[org/gnome/login-screen]\nbanner-message-enable=false\n"
        );
        assert!(dconf_keyfile(&["a'b".into()]).contains("banner-message-text='a\\'b'"));
    }
}
