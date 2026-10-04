//! Reading files a tracked user controls, as root, without letting them hang or flood the agent.

use std::io::Read;
use std::path::Path;

/// The cap for small config files (`.desktop`, `.acf`, `.vdf`, `sunshine.conf`).
pub const SMALL: u64 = 1 << 20;
/// The cap for Steam's binary `shortcuts.vdf`, which carries icons and tags for every shortcut.
pub const SHORTCUTS: u64 = 16 << 20;

/// Reads a regular file of at most `max` bytes. The kid can put a FIFO or a device where a config file
/// should be (which would block a plain read forever) or a huge file: both are errors.
pub fn read_capped(path: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Error, ErrorKind};
    use std::os::unix::fs::OpenOptionsExt;
    // O_NONBLOCK: opening a FIFO with no writer would otherwise wait for one. It changes nothing for a
    // regular file, and anything else is refused below.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    let too_big = || {
        Error::new(
            ErrorKind::InvalidData,
            format!("{} is larger than {max} bytes", path.display()),
        )
    };
    if meta.len() > max {
        return Err(too_big());
    }
    // The file can grow after the fstat
    let mut out = Vec::with_capacity(meta.len() as usize);
    file.take(max + 1).read_to_end(&mut out)?;
    if out.len() as u64 > max {
        return Err(too_big());
    }
    Ok(out)
}

/// `read_capped`, as text; invalid UTF-8 is replaced rather than refused.
pub fn read_capped_string(path: &Path, max: u64) -> std::io::Result<String> {
    read_capped(path, max).map(|b| String::from_utf8_lossy(&b).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from("/tmp/claude-1000/kidtime-sdd");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn a_small_regular_file_is_read() {
        let path = scratch("small.conf");
        std::fs::write(&path, "port = 48189\n").unwrap();
        assert_eq!(read_capped_string(&path, SMALL).unwrap(), "port = 48189\n");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let path = scratch("fifo.conf");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let (tx, rx) = mpsc::channel();
        let p = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_capped(&p, SMALL).map(|_| ()));
        });
        let result = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("reading a FIFO must not block");
        assert!(result.is_err());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_device_is_refused() {
        assert!(read_capped(Path::new("/dev/zero"), SMALL).is_err());
    }

    #[test]
    fn a_file_over_the_cap_is_refused() {
        let path = scratch("big.vdf");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(SMALL + 1).unwrap();
        assert!(read_capped(&path, SMALL).is_err());
        // Exactly at the cap is fine
        f.set_len(SMALL).unwrap();
        assert_eq!(read_capped(&path, SMALL).unwrap().len() as u64, SMALL);
        std::fs::remove_file(&path).unwrap();
    }
}
