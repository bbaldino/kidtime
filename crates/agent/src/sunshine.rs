//! Detects whether a user's Sunshine instance is actually streaming.
//!
//! Sunshine only encodes video while a Moonlight client is connected, so the
//! GPU encoder time the kernel reports for the sunshine process (the
//! `drm-engine-enc` counter in /proc/<pid>/fdinfo) only grows during a stream.
//! Sunshine opens its encoder's GPU client per stream, so clients (and their
//! counters) appear and disappear; each one is tracked separately.

use std::collections::HashMap;
use std::fs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// The encoder ran since the last check.
    Connected,
    /// Sunshine is running but hasn't encoded anything since the last check.
    Disconnected,
    /// No readable sunshine process (e.g. not running as root).
    Unknown,
}

#[derive(Default)]
pub struct Monitor {
    /// uid -> (DRM client id -> encoder nanoseconds) at the last check.
    last: HashMap<u32, HashMap<String, u64>>,
}

impl Monitor {
    pub fn check(&mut self, uid: u32) -> Stream {
        let Some(now) = encoder_ns(uid) else {
            self.last.remove(&uid);
            return Stream::Unknown;
        };
        let before = self.last.insert(uid, now.clone());
        compare(before.as_ref(), &now)
    }
}

/// Stream state from two readings of per-client encoder time.
fn compare(before: Option<&HashMap<String, u64>>, now: &HashMap<String, u64>) -> Stream {
    let encoding = now
        .iter()
        .any(|(client, &ns)| match before.and_then(|b| b.get(client)) {
            Some(&prev) => ns > prev,
            // A client that appeared since the last check and has already encoded
            None => before.is_some() && ns > 0,
        });
    match (encoding, before) {
        (true, _) => Stream::Connected,
        (false, Some(_)) => Stream::Disconnected,
        // First look: nothing to compare against, so only rule out a stream if nothing has ever encoded
        (false, None) if now.values().all(|&ns| ns == 0) => Stream::Disconnected,
        (false, None) => Stream::Unknown,
    }
}

/// Encoder time per DRM client across the user's sunshine processes,
/// or None if no sunshine process of theirs is readable.
fn encoder_ns(uid: u32) -> Option<HashMap<String, u64>> {
    let mut clients: Option<HashMap<String, u64>> = None;
    for entry in fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|c| c.trim() != "sunshine")
            .unwrap_or(true)
        {
            continue;
        }
        if !owned_by(pid, uid) {
            continue;
        }
        let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fdinfo")) else {
            continue;
        };
        let clients = clients.get_or_insert_with(HashMap::new);
        for fd in fds.flatten() {
            let Ok(info) = fs::read_to_string(fd.path()) else {
                continue;
            };
            let field = |key: &str| {
                info.lines()
                    .find_map(|l| l.strip_prefix(key))
                    .map(str::trim)
            };
            // Several fds can share one DRM client; it's the same counter either way
            let Some(client) = field("drm-client-id:") else {
                continue;
            };
            let ns = field("drm-engine-enc:")
                .and_then(|v| v.trim_end_matches("ns").trim().parse::<u64>().ok())
                .unwrap_or(0);
            clients.insert(format!("{pid}/{client}"), ns);
        }
    }
    clients
}

fn owned_by(pid: u32, uid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/status")).is_ok_and(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("Uid:"))
            .and_then(|v| v.split_whitespace().next()?.parse::<u32>().ok())
            == Some(uid)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(m: &mut Monitor, clients: &[(&str, u64)]) -> Stream {
        let now: HashMap<String, u64> = clients.iter().map(|(c, n)| (c.to_string(), *n)).collect();
        let before = m.last.insert(1, now.clone());
        compare(before.as_ref(), &now)
    }

    #[test]
    fn stream_lifecycle() {
        let mut m = Monitor::default();
        assert_eq!(step(&mut m, &[("a", 0)]), Stream::Disconnected); // paused: only a long-lived client, never encoded
        assert_eq!(step(&mut m, &[("a", 0), ("b", 900)]), Stream::Connected); // stream starts, new encoder client
        assert_eq!(step(&mut m, &[("a", 0), ("b", 5000)]), Stream::Connected);
        assert_eq!(step(&mut m, &[("a", 0)]), Stream::Disconnected); // stream ends, client gone
        assert_eq!(step(&mut m, &[("a", 0), ("c", 300)]), Stream::Connected); // resumed
    }
}
