//! The real enforcement actions: commands and signals, run as root.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::enforce::{Actions, Login, RunningApp};
use crate::files::{SMALL, read_capped_string};

const DEFAULT_SUNSHINE_PORT: u16 = 47989;
const KILL_AFTER: Duration = Duration::from_secs(10);
/// A pid not passed to `close` for this long is no longer being closed: start over with SIGTERM.
const CLOSING_STALE_AFTER: Duration = Duration::from_secs(30);
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

/// The `port = N` line of a sunshine.conf, if it has one.
fn sunshine_conf_port(conf: &str) -> Option<u16> {
    conf.lines().find_map(|l| {
        l.split_once('=')
            .filter(|(k, _)| k.trim() == "port")?
            .1
            .trim()
            .parse()
            .ok()
    })
}

/// Where a kid's Sunshine base port came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortSource {
    /// `sunshine_ports` in the agent config: the kid can't change it.
    Config,
    /// The kid's own sunshine.conf.
    SunshineConf,
    Default,
}

/// The agent config first, the kid's sunshine.conf second, Sunshine's default last.
fn resolve_port(configured: Option<u16>, conf: Option<&str>) -> (u16, PortSource) {
    if let Some(port) = configured {
        return (port, PortSource::Config);
    }
    match conf.and_then(sunshine_conf_port) {
        Some(port) => (port, PortSource::SunshineConf),
        None => (DEFAULT_SUNSHINE_PORT, PortSource::Default),
    }
}

/// What to change in the `inet kidtime` table (`nft -a list` output) so the account's TCP and UDP rules
/// both exist with the range for `port`: handles of the account's rules with a wrong range, to delete, and
/// the `nft -f` lines to add what is missing (empty when nothing is).
fn stream_rule_plan(table: &str, user: &str, port: u16) -> (Vec<String>, Vec<String>) {
    let wanted = nft_block_commands(user, port);
    let range = format!("{}-{}", port.saturating_sub(5), port.saturating_add(21));
    let tag = format!("comment \"{user}\"");
    let mut delete = Vec::new();
    let mut ok = [false, false]; // tcp, udp
    for line in table
        .lines()
        .filter(|l| l.contains(&tag) && l.contains(" drop"))
    {
        let words: Vec<&str> = line.split_whitespace().collect();
        let Some(i) = words.iter().position(|w| *w == "dport") else {
            continue;
        };
        let proto = match i.checked_sub(1).map(|p| words[p]) {
            Some("tcp") => 0,
            Some("udp") => 1,
            _ => continue,
        };
        if words.get(i + 1) == Some(&range.as_str()) && !ok[proto] {
            ok[proto] = true;
        } else if let Some((_, handle)) = line.rsplit_once("# handle ") {
            // A wrong range, or a duplicate
            delete.push(handle.trim().to_string());
        }
    }
    let mut add: Vec<String> = wanted
        .iter()
        .filter(|c| (c.contains(" tcp dport ") && !ok[0]) || (c.contains(" udp dport ") && !ok[1]))
        .cloned()
        .collect();
    if !add.is_empty() {
        // Adding the table and the chain is idempotent
        let mut lines: Vec<String> = wanted
            .iter()
            .filter(|c| !c.contains(" dport "))
            .cloned()
            .collect();
        lines.append(&mut add);
        add = lines;
    }
    (delete, add)
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

/// Where a pid is in being closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Closing {
    first_term: Instant,
    last_seen: Instant,
}

/// The signal to send now (15 first, 9 once `KILL_AFTER` has passed since the first, however far
/// apart the calls are) and the updated entry. An entry not seen for `CLOSING_STALE_AFTER` is
/// forgotten, so a round much later starts with SIGTERM again.
fn signal_for(entry: Option<Closing>, now: Instant) -> (Option<i32>, Closing) {
    match entry.filter(|e| now.duration_since(e.last_seen) < CLOSING_STALE_AFTER) {
        None => (
            Some(15),
            Closing {
                first_term: now,
                last_seen: now,
            },
        ),
        Some(e) => {
            let signal = (now.duration_since(e.first_term) >= KILL_AFTER).then_some(9);
            (
                signal,
                Closing {
                    last_seen: now,
                    ..e
                },
            )
        }
    }
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

/// The real uid (the first field of `Uid:`) in a `/proc/<pid>/status`.
fn real_uid(status: &str) -> Option<u32> {
    status_field(status, "Uid:")
}

/// The first number of a `/proc/<pid>/status` line, e.g. the real id of `Uid:` or `Gid:`.
fn status_field(status: &str, key: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|l| l.strip_prefix(key)?.split_whitespace().next()?.parse().ok())
}

/// Whether `pid` exists and runs as `uid` (its real uid). The pid of a stream's window comes from the
/// kid's own Sway (and, for Xwayland windows, from a property the client sets), so it is checked before
/// root signals it.
fn owned_by(pid: u32, uid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|status| real_uid(&status))
        == Some(uid)
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

/// How long any command may take before it is killed.
const RUN_DEADLINE: Duration = Duration::from_secs(5);

/// Runs a command with `input` on stdin (if any), killing it (and its process group) if it hasn't exited
/// by `deadline`; the killed child is reaped. Output is read on helper threads, so a chatty child can't
/// block on a full pipe, and a process it left running with the pipes open can't block us past the deadline.
fn output_with_deadline(
    program: &str,
    args: &[&str],
    input: Option<&str>,
    deadline: Duration,
) -> Result<std::process::Output> {
    use std::io::{Read, Write};
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    let end = Instant::now() + deadline;
    let mut child = Command::new(program)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so a timeout also kills whatever it started
        .process_group(0)
        .spawn()
        .with_context(|| format!("running {program}"))?;
    let (tx, rx) = std::sync::mpsc::channel::<(bool, Vec<u8>)>();
    let pipes: [(bool, Option<Box<dyn Read + Send>>); 2] = [
        (true, child.stdout.take().map(|p| Box::new(p) as _)),
        (false, child.stderr.take().map(|p| Box::new(p) as _)),
    ];
    for (is_stdout, pipe) in pipes {
        if let Some(mut pipe) = pipe {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = pipe.read_to_end(&mut buf);
                let _ = tx.send((is_stdout, buf));
            });
        }
    }
    drop(tx);
    if let (Some(mut stdin), Some(input)) = (child.stdin.take(), input) {
        let input = input.to_owned();
        // A child that never reads its input can't block us; the write fails once it is gone
        std::thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });
    }
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= end {
            // SAFETY: kill(2) with a negative pid signals that process group; no memory is involved
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.kill();
            let _ = child.wait();
            bail!("{program} {}: timed out after {deadline:?}", args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    for _ in 0..2 {
        match rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
            Ok((true, buf)) => stdout = buf,
            Ok((false, buf)) => stderr = buf,
            Err(_) => break,
        }
    }
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let out = output_with_deadline(program, args, None, RUN_DEADLINE)?;
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
    let out = output_with_deadline(program, args, Some(input), RUN_DEADLINE)?;
    if !out.status.success() {
        bail!("{program}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// The `inet kidtime` table with rule handles, or None if there is no such table.
fn list_kidtime_table() -> Result<Option<String>> {
    let out = output_with_deadline(
        "nft",
        &["-a", "list", "table", "inet", "kidtime"],
        None,
        RUN_DEADLINE,
    )?;
    Ok(out
        .status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned()))
}

pub struct SystemActions {
    /// name -> (uid, home)
    users: HashMap<String, (u32, PathBuf)>,
    streaming_sway_socket: Option<String>,
    /// Where `persist` writes the enforcement state.
    state_path: PathBuf,
    /// Sunshine base ports from the agent config, by account.
    sunshine_ports: HashMap<String, u16>,
    /// Accounts whose port source has been logged.
    port_logged: HashSet<String>,
    /// Processes sent SIGTERM, and when.
    closing: HashMap<u32, Closing>,
    /// Accounts already warned about having no password.
    warned_no_password: HashSet<String>,
}

impl SystemActions {
    pub fn new(
        users: HashMap<String, (u32, PathBuf)>,
        streaming_sway_socket: Option<String>,
        state_path: PathBuf,
        sunshine_ports: HashMap<String, u16>,
    ) -> Self {
        Self {
            users,
            streaming_sway_socket,
            state_path,
            sunshine_ports,
            port_logged: HashSet::new(),
            closing: HashMap::new(),
            warned_no_password: HashSet::new(),
        }
    }

    fn uid(&self, user: &str) -> Result<u32> {
        self.users
            .get(user)
            .map(|(uid, _)| *uid)
            .with_context(|| format!("{user} is not tracked"))
    }

    /// The account's Sunshine base port. The source is logged once per account.
    fn sunshine_port(&mut self, user: &str) -> Result<u16> {
        let (_, home) = self
            .users
            .get(user)
            .with_context(|| format!("{user} is not tracked"))?;
        let configured = self.sunshine_ports.get(user).copied();
        let conf_path = home.join(".config/sunshine/sunshine.conf");
        let conf = match configured {
            Some(_) => None,
            None => match read_capped_string(&conf_path, SMALL) {
                Ok(c) => Some(c),
                Err(e) => {
                    if !self.port_logged.contains(user) {
                        tracing::warn!("{}: {e}", conf_path.display());
                    }
                    None
                }
            },
        };
        let (port, source) = resolve_port(configured, conf.as_deref());
        if self.port_logged.insert(user.to_string()) {
            let from = match source {
                PortSource::Config => "sunshine_ports in the agent config".to_string(),
                PortSource::SunshineConf => conf_path.display().to_string(),
                PortSource::Default => "Sunshine's default".to_string(),
            };
            tracing::info!("{user}'s Sunshine base port is {port}, from {from}");
        }
        Ok(port)
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

    /// An account with no password reports `NoPassword`: locking it would make it impossible to unlock
    /// again, so the enforcement logic leaves its login alone (no disable, no record, no `usermod -U`).
    /// The session lock still applies.
    fn login(&mut self, user: &str) -> Result<Login> {
        let shadow = std::fs::read_to_string("/etc/shadow").context("reading /etc/shadow")?;
        Ok(
            match shadow_state(&shadow, user)
                .with_context(|| format!("{user} not in /etc/shadow"))?
            {
                ShadowState::Locked => Login::Disabled,
                ShadowState::Unlocked => Login::Enabled,
                ShadowState::NoPassword => {
                    if self.warned_no_password.insert(user.to_string()) {
                        tracing::warn!("{user} has no password; its login can't be disabled");
                    }
                    Login::NoPassword
                }
            },
        )
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
                "--timeout",
                "5",
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

    fn close(&mut self, user: &str, app: &RunningApp) -> Result<()> {
        let uid = self.uid(user)?;
        if let Some(w) = &app.window {
            let criteria = format!("[con_id={}] kill", w.con_id);
            let _ = run("swaymsg", &["-s", &w.socket.to_string_lossy(), &criteria]);
        }
        let now = Instant::now();
        // Entries for pids that stopped being closed are forgotten (a later round starts with SIGTERM)
        self.closing
            .retain(|_, e| now.duration_since(e.last_seen) < CLOSING_STALE_AFTER);
        for &pid in &app.pids {
            // Only the kid's own processes: never something the kid pointed us at
            if !owned_by(pid, uid) {
                if std::path::Path::new(&format!("/proc/{pid}")).exists() {
                    tracing::warn!(
                        "not closing pid {pid} for {user} ({}): it isn't {user}'s process",
                        app.name
                    );
                }
                continue;
            }
            let (signal, entry) = signal_for(self.closing.get(&pid).copied(), now);
            self.closing.insert(pid, entry);
            if let Some(signal) = signal
                && let Err(e) = libc_kill(pid, signal)
            {
                tracing::debug!("signal to {pid}: {e:#}");
            }
        }
        // Forget processes that have exited
        self.closing
            .retain(|pid, _| std::path::Path::new(&format!("/proc/{pid}")).exists());
        Ok(())
    }

    /// Checks the account's rules and repairs only what is wrong, so it is safe to call every tick.
    fn block_stream(&mut self, user: &str) -> Result<()> {
        let port = self.sunshine_port(user)?;
        let table = list_kidtime_table()?.unwrap_or_default();
        let (delete, add) = stream_rule_plan(&table, user, port);
        for handle in &delete {
            run(
                "nft",
                &[
                    "delete", "rule", "inet", "kidtime", "input", "handle", handle,
                ],
            )?;
        }
        if !add.is_empty() {
            // Over stdin: argv would treat the negative priority as an option, and it applies atomically
            run_stdin("nft", &["-f", "-"], &nft_script(&add))?;
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
        } else {
            tracing::warn!(
                "/etc/dconf/db/gdm.d is missing: banner for later login screens skipped"
            );
        }
        // The login screens running now: as their temporary users, over their session buses
        let mut first_error = None;
        let tracked: HashSet<u32> = self.users.values().map(|(uid, _)| *uid).collect();
        for (uid, gid) in greeter_ids(&tracked) {
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

    fn persist(&mut self, state: &crate::enforce::Persisted) -> Result<()> {
        crate::save_state(&self.state_path, state)
    }
}

/// uid and gid of every process running a login screen (`gnome-shell --mode=gdm`), never root and never
/// a tracked account (a kid could start a process with that command line).
fn greeter_ids(tracked: &HashSet<u32>) -> Vec<(u32, u32)> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|e| {
            let cmdline = std::fs::read(e.path().join("cmdline")).ok()?;
            let status = std::fs::read_to_string(e.path().join("status")).ok()?;
            greeter_id(&cmdline, &status, tracked)
        })
        .collect()
}

/// A login screen's uid and gid, from its `cmdline` and `status`.
fn greeter_id(cmdline: &[u8], status: &str, tracked: &HashSet<u32>) -> Option<(u32, u32)> {
    let argv: Vec<String> = cmdline
        .split(|&b| b == 0)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    if !is_gdm_greeter(&argv) {
        return None;
    }
    let (uid, gid) = (real_uid(status)?, status_field(status, "Gid:")?);
    (uid != 0 && !tracked.contains(&uid)).then_some((uid, gid))
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
    fn a_command_past_its_deadline_is_killed_and_reaped() {
        let dir = std::path::PathBuf::from("/tmp/claude-1000/kidtime-sdd");
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join(format!("slow-{}.pid", std::process::id()));
        let script = format!("echo $$ > {}; exec sleep 3", pid_file.display());
        let start = Instant::now();
        let err = output_with_deadline("sh", &["-c", &script], None, Duration::from_millis(300))
            .unwrap_err();
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        // Killed and reaped: not even a zombie is left
        assert!(!std::path::Path::new(&format!("/proc/{}", pid.trim())).exists());
        std::fs::remove_file(&pid_file).unwrap();
    }

    #[test]
    fn a_command_gets_its_input_and_its_output_is_read_in_full() {
        let out = output_with_deadline("cat", &[], Some("hello"), RUN_DEADLINE).unwrap();
        assert_eq!(out.stdout, b"hello");
        // More than a pipe holds
        let out = output_with_deadline("head", &["-c", "1000000", "/dev/zero"], None, RUN_DEADLINE)
            .unwrap();
        assert_eq!(out.stdout.len(), 1_000_000);
        assert!(run("false", &[]).is_err());
        assert!(run("true", &[]).is_ok());
    }

    #[test]
    fn a_background_process_holding_the_output_open_doesnt_hang_the_call() {
        let start = Instant::now();
        let out = output_with_deadline(
            "sh",
            &["-c", "sleep 3 & echo started"],
            None,
            Duration::from_millis(500),
        )
        .unwrap();
        assert!(out.status.success());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn the_real_uid_is_the_first_uid_field() {
        let status = "Name:\tgame\nUmask:\t0022\nState:\tS (sleeping)\nUid:\t1001\t0\t0\t0\nGid:\t1001\t1001\t1001\t1001\n";
        assert_eq!(real_uid(status), Some(1001));
        assert_eq!(real_uid("Name:\tx\nUid:\t0\t1001\t1001\t1001\n"), Some(0));
        assert_eq!(real_uid("Name:\tx\n"), None);
        assert_eq!(real_uid("Uid:\tabc\n"), None);
    }

    #[test]
    fn only_processes_of_the_kid_are_signalled() {
        let me = std::process::id();
        // SAFETY: getuid has no preconditions
        let uid = unsafe { libc::getuid() };
        assert!(owned_by(me, uid));
        assert!(!owned_by(me, uid + 1));
        assert!(!owned_by(u32::MAX - 1, uid), "a pid that doesn't exist");
    }

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
        assert_eq!(
            resolve_port(None, Some("capture = wlr\nport = 48189\n")).0,
            48189
        );
        assert_eq!(resolve_port(None, Some("capture = wlr\n")).0, 47989);
        assert_eq!(resolve_port(None, Some("port=48089")).0, 48089);
    }

    #[test]
    fn the_port_comes_from_the_config_then_sunshine_conf_then_the_default() {
        assert_eq!(
            resolve_port(Some(48189), Some("port = 50000")),
            (48189, PortSource::Config)
        );
        assert_eq!(
            resolve_port(None, Some("port = 50000")),
            (50000, PortSource::SunshineConf)
        );
        assert_eq!(
            resolve_port(None, Some("capture = wlr")),
            (47989, PortSource::Default)
        );
        assert_eq!(resolve_port(None, None), (47989, PortSource::Default));
    }

    const TABLE: &str = "table inet kidtime { # handle 5
\tchain input { # handle 1
\t\ttype filter hook input priority -10; policy accept;
\t\tiifname != \"lo\" tcp dport 48184-48210 drop comment \"kid1\" # handle 2
\t\tiifname != \"lo\" udp dport 48184-48210 drop comment \"kid1\" # handle 3
\t\tiifname != \"lo\" tcp dport 48084-48110 drop comment \"kid10\" # handle 4
\t}
}
";

    #[test]
    fn rules_already_right_are_left_alone() {
        assert_eq!(stream_rule_plan(TABLE, "kid1", 48189), (vec![], vec![]));
    }

    #[test]
    fn missing_rules_are_added_with_their_table_and_chain() {
        let (delete, add) = stream_rule_plan("", "kid2", 48089);
        assert!(delete.is_empty());
        assert_eq!(add, nft_block_commands("kid2", 48089));
        // kid10's TCP rule isn't kid1's, and kid10's missing UDP rule is added alone
        let (delete, add) = stream_rule_plan(TABLE, "kid10", 48089);
        assert!(delete.is_empty());
        assert_eq!(add.len(), 3, "{add:?}");
        assert!(add[2].contains("udp dport 48084-48110 drop comment \"kid10\""));
    }

    #[test]
    fn rules_with_a_wrong_range_are_replaced() {
        // The kid's port moved to 50000
        let (delete, add) = stream_rule_plan(TABLE, "kid1", 50000);
        assert_eq!(delete, ["2", "3"]);
        assert_eq!(add, nft_block_commands("kid1", 50000));
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

    #[test]
    fn a_greeter_is_never_root_or_a_tracked_account() {
        let argv = b"/usr/bin/gnome-shell\0--mode=gdm\0--wayland\0";
        let status = |uid: u32| {
            format!(
                "Name:\tgnome-shell\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\nGid:\t60578\t60578\t60578\t60578\n"
            )
        };
        let tracked = HashSet::from([1001, 1002]);
        assert_eq!(
            greeter_id(argv, &status(60578), &tracked),
            Some((60578, 60578))
        );
        assert_eq!(greeter_id(argv, &status(0), &tracked), None);
        assert_eq!(greeter_id(argv, &status(1001), &tracked), None);
        assert_eq!(greeter_id(b"bash\0", &status(60578), &tracked), None);
    }

    #[test]
    fn sigkill_follows_sigterm_however_far_apart_the_calls_are() {
        let t0 = Instant::now();
        let at = |secs| t0 + Duration::from_secs(secs);
        let (sig, e) = signal_for(None, t0);
        assert_eq!(sig, Some(15));
        // 5 s later: still waiting
        let (sig, e5) = signal_for(Some(e), at(5));
        assert_eq!(sig, None);
        // 12 s after the first (7 s since the last call)
        assert_eq!(signal_for(Some(e5), at(12)).0, Some(9));
        // one call 25 s after the first, nothing in between
        assert_eq!(signal_for(Some(e), at(25)).0, Some(9));
        // not seen for 30 s: a fresh round
        let (sig, fresh) = signal_for(Some(e), at(30));
        assert_eq!(sig, Some(15));
        assert_eq!(fresh.first_term, at(30));
    }
}
