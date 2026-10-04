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

#[derive(Debug, PartialEq, Eq)]
pub enum ShadowState {
    /// The hash starts with `!`: password login is off.
    Locked,
    Unlocked,
    /// An empty hash: locking would make the account impossible to unlock again.
    NoPassword,
}

pub fn shadow_state(shadow: &str, user: &str) -> Option<ShadowState> {
    shadow.lines().find_map(|l| {
        let mut f = l.split(':');
        if f.next()? != user {
            return None;
        }
        let hash = f.next()?;
        Some(if hash.is_empty() {
            ShadowState::NoPassword
        } else if hash.starts_with('!') {
            ShadowState::Locked
        } else {
            ShadowState::Unlocked
        })
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

/// Run through `swaymsg exec`: sway's parser and then `sh -c` both see the string. Make the text
/// inert for both: no backslash, no single quote, no line breaks, so the one pair of quotes holds.
pub fn swaynag_command(text: &str) -> String {
    let inert: String = text
        .chars()
        .map(|c| match c {
            '\\' => '/',
            '\'' => '\u{2019}',
            '\n' | '\r' => ' ',
            c => c,
        })
        .collect();
    format!("swaynag --layer overlay --edge top -t warning -m '{inert}'")
}

/// Commands for `nft -f -`, one per line.
pub fn nft_script(lines: &[String]) -> String {
    let mut script = lines.join("\n");
    script.push('\n');
    script
}

/// Pids that are safe to signal: never init, never anything that would wrap to a group or "all".
fn signalable(pid: u32) -> bool {
    pid > 1 && pid <= i32::MAX as u32
}

/// `gnome-shell --mode=gdm` itself, not a shell that merely mentions it.
fn is_gdm_greeter(argv: &[&str]) -> bool {
    argv.first()
        .is_some_and(|a| a.rsplit('/').next() == Some("gnome-shell"))
        && argv.contains(&"--mode=gdm")
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

fn run_stdin(program: &str, args: &[&str], input: &str) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {program}"))?;
    child
        .stdin
        .take()
        .context("no stdin")?
        .write_all(input.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("{program}: {}", String::from_utf8_lossy(&out.stderr).trim());
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
        match shadow_state(&shadow, user).with_context(|| format!("{user} not in /etc/shadow"))? {
            ShadowState::Locked => Ok(true),
            ShadowState::Unlocked | ShadowState::NoPassword => Ok(false),
        }
    }

    fn disable_login(&mut self, user: &str) -> Result<()> {
        let shadow = std::fs::read_to_string("/etc/shadow").context("reading /etc/shadow")?;
        if shadow_state(&shadow, user) == Some(ShadowState::NoPassword) {
            bail!("{user} has no password; not disabling login, the session lock still applies");
        }
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
        // A close round hours later must start over with SIGTERM
        self.closing
            .retain(|_, since| now.duration_since(*since) < KILL_AFTER * 2);
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
        let conf_path = home.join(".config/sunshine/sunshine.conf");
        let conf = match std::fs::read_to_string(&conf_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    "{}: {e}; blocking the default Sunshine port range for {user}",
                    conf_path.display()
                );
                String::new()
            }
        };
        // Rules left from before an agent restart must not be doubled up: add only the missing ones
        let tag = format!("comment \"{user}\"");
        let table = list_kidtime_table()?.unwrap_or_default();
        let has_rule = |proto: &str| {
            let dport = format!("{proto} dport");
            table
                .lines()
                .any(|l| l.contains(&dport) && l.contains("drop") && l.contains(&tag))
        };
        let lines: Vec<String> = nft_block_commands(user, sunshine_port(&conf))
            .into_iter()
            .filter(|c| {
                !(c.contains("tcp dport") && has_rule("tcp")
                    || c.contains("udp dport") && has_rule("udp"))
            })
            .collect();
        // Over stdin: argv would treat the negative priority as an option, and it applies atomically
        run_stdin("nft", &["-f", "-"], &nft_script(&lines))
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
        } else {
            tracing::warn!(
                "/etc/dconf/db/gdm.d is missing: banner for later login screens skipped"
            );
        }
        // The login screens running now: as their temporary users, over their session buses
        let mut first_error = None;
        for (uid, gid) in greeter_ids() {
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
            let result = if lines.is_empty() {
                base(&["banner-message-enable", "false"])
            } else {
                // gsettings takes a GVariant, so a bare string with quotes or a leading digit is not safe
                base(&["banner-message-text", &gvariant_string(&lines.join("\n"))])
                    .and_then(|()| base(&["banner-message-enable", "true"]))
            };
            if let Err(e) = result {
                first_error.get_or_insert(e);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

/// uid and gid of every process running a login screen (`gnome-shell --mode=gdm`), never root.
fn greeter_ids() -> Vec<(u32, u32)> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|e| {
            let cmdline = std::fs::read(e.path().join("cmdline")).ok()?;
            let argv: Vec<String> = cmdline
                .split(|&b| b == 0)
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect();
            let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
            if !is_gdm_greeter(&argv) {
                return None;
            }
            let status = std::fs::read_to_string(e.path().join("status")).ok()?;
            let id = |key: &str| {
                status
                    .lines()
                    .find_map(|l| l.strip_prefix(key)?.split_whitespace().next()?.parse().ok())
            };
            let (uid, gid) = (id("Uid:")?, id("Gid:")?);
            (uid != 0).then_some((uid, gid))
        })
        .collect()
}

fn libc_kill(pid: u32, signal: i32) -> Result<()> {
    if !signalable(pid) {
        bail!("refusing to signal pid {pid}");
    }
    run("kill", &[&format!("-{signal}"), &pid.to_string()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_lock_state() {
        let shadow = "root:$6$abc:1::::::\nkid1:!$6$def:1::::::\nkid2:$6$ghi:1::::::\nkid3::1::::::\nkid4:!!:1::::::\nkid5:*:1::::::\n";
        assert_eq!(shadow_state(shadow, "kid1"), Some(ShadowState::Locked));
        assert_eq!(shadow_state(shadow, "kid2"), Some(ShadowState::Unlocked));
        assert_eq!(shadow_state(shadow, "kid3"), Some(ShadowState::NoPassword));
        assert_eq!(shadow_state(shadow, "kid4"), Some(ShadowState::Locked));
        assert_eq!(shadow_state(shadow, "kid5"), Some(ShadowState::Unlocked));
        assert_eq!(shadow_state(shadow, "nobody"), None);
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

    #[test]
    fn swaynag_text_is_inert_for_sway_and_sh() {
        let text = "a\\b \\';exec foo;' x, $(id)\n-y\r";
        let cmd = swaynag_command(text);
        let prefix = "swaynag --layer overlay --edge top -t warning -m ";
        let arg = cmd.strip_prefix(prefix).unwrap();
        assert!(arg.starts_with('\'') && arg.ends_with('\''));
        let inner = &arg[1..arg.len() - 1];
        assert!(!inner.contains(['\\', '\'', '\n', '\r']));
        assert!(inner.contains("$(id)") && inner.contains("-y"));
    }

    #[test]
    fn nft_script_is_one_command_per_line() {
        let script = nft_script(&[
            "add table inet kidtime".into(),
            "add chain x { priority -10 ; }".into(),
        ]);
        assert_eq!(
            script,
            "add table inet kidtime\nadd chain x { priority -10 ; }\n"
        );
    }

    #[test]
    fn only_real_pids_are_signalable() {
        assert!(!signalable(0));
        assert!(!signalable(1));
        assert!(signalable(2));
        assert!(signalable(i32::MAX as u32));
        assert!(!signalable(i32::MAX as u32 + 1));
        assert!(libc_kill(1, 15).is_err());
    }

    #[test]
    fn greeter_argv_is_matched_exactly() {
        assert!(is_gdm_greeter(&[
            "/usr/bin/gnome-shell",
            "--mode=gdm",
            "--wayland"
        ]));
        assert!(is_gdm_greeter(&["gnome-shell", "--mode=gdm"]));
        assert!(!is_gdm_greeter(&["/usr/bin/gnome-shell", "--mode=user"]));
        assert!(!is_gdm_greeter(&["sh", "-c", "gnome-shell --mode=gdm"]));
        assert!(!is_gdm_greeter(&[]));
    }
}
