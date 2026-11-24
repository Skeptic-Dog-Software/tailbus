//! Attachments: files too large for a message. Each is held by its hash in
//! the bus's state directory, and reaches another machine's bus before the
//! message that names it, which is how an agent there sees a local file.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::proto::Attachment;
use crate::util::new_id;

const LONGEST_NAME: usize = 255;

/// Why an attachment wasn't taken, with the HTTP status that says so.
#[derive(Debug)]
pub struct Refusal {
    pub code: u16,
    pub why: String,
}

/// The attachments on this machine.
pub struct Attachments {
    dir: PathBuf,
    pub max_file: u64,
    max_total: u64,
}

pub fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A file's name that's safe to create in a directory, from any sender.
pub fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= LONGEST_NAME
        && !name.starts_with('.')
        && !name.contains('/')
        && !name.chars().any(char::is_control)
}

/// Whether an attachment's description could be real, which keeps a
/// sender's hash and name out of any path until they've passed.
pub fn well_formed(a: &Attachment) -> bool {
    is_sha256(&a.sha256) && safe_name(&a.name)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Attachments {
    pub fn new(dir: PathBuf, max_file: u64, max_total: u64) -> Attachments {
        Attachments {
            dir,
            max_file,
            max_total,
        }
    }

    #[cfg(test)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where an attachment with this hash is, or would be.
    pub fn path(&self, sha256: &str) -> PathBuf {
        self.dir.join(sha256)
    }

    /// Whether this exact attachment is here.
    pub fn has(&self, a: &Attachment) -> bool {
        well_formed(a) && fs::metadata(self.path(&a.sha256)).is_ok_and(|m| m.len() == a.size)
    }

    fn total(&self) -> u64 {
        fs::read_dir(&self.dir).map_or(0, |dir| {
            dir.flatten()
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
    }

    /// Keeps a stream, up to `size` bytes if that's known. Returns its hash
    /// and size. It's written beside the store and moved in once it's whole,
    /// so a partial file never has a hash's name.
    fn keep(
        &self,
        reader: &mut dyn Read,
        expected: Option<&str>,
    ) -> Result<(String, u64), Refusal> {
        let internal = |e: io::Error| Refusal {
            code: 500,
            why: format!("storing an attachment: {e}"),
        };
        fs::create_dir_all(self.dir.join("incoming")).map_err(internal)?;
        let tmp = self.dir.join("incoming").join(new_id());
        let mut file = File::options()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(internal)?;
        let mut hash = Sha256::new();
        let mut size = 0u64;
        let mut buf = vec![0u8; 256 * 1024];
        let kept = loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break Ok(()),
                Ok(n) => n,
                Err(e) => {
                    break Err(Refusal {
                        code: 400,
                        why: format!("the upload stopped: {e}"),
                    });
                }
            };
            size += n as u64;
            if size > self.max_file {
                break Err(Refusal {
                    code: 413,
                    why: self.too_large(size),
                });
            }
            hash.update(&buf[..n]);
            if let Err(e) = file.write_all(&buf[..n]) {
                break Err(internal(e));
            }
        };
        let digest = hex(&hash.finalize());
        let checked = kept.and_then(|()| match expected {
            Some(sha256) if sha256 != digest => Err(Refusal {
                code: 422,
                why: format!("the attachment's hash is {digest}, not {sha256}"),
            }),
            _ => Ok(()),
        });
        let moved = checked.and_then(|()| fs::rename(&tmp, self.path(&digest)).map_err(internal));
        if moved.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        moved.map(|()| (digest, size))
    }

    fn too_large(&self, size: u64) -> String {
        format!(
            "the attachment is at least {size} bytes; the limit is {} (max_attachment_bytes)",
            self.max_file
        )
    }

    fn check_room(&self, size: u64) -> Result<(), Refusal> {
        if size > self.max_file {
            return Err(Refusal {
                code: 413,
                why: self.too_large(size),
            });
        }
        if self.total() + size > self.max_total {
            return Err(Refusal {
                code: 413,
                why: format!(
                    "attachments here would pass {} bytes (attachment_store_bytes)",
                    self.max_total
                ),
            });
        }
        Ok(())
    }

    /// Takes an upload, keeping it only if it's what it claims to be.
    pub fn receive(&self, sha256: &str, size: u64, reader: &mut dyn Read) -> Result<(), Refusal> {
        if !is_sha256(sha256) {
            return Err(Refusal {
                code: 400,
                why: "that isn't a SHA-256 hash".into(),
            });
        }
        self.check_room(size)?;
        self.keep(reader, Some(sha256)).map(|_| ())
    }

    /// Adds a stream as an attachment called `name`.
    pub fn add(&self, name: &str, reader: &mut dyn Read, size: u64) -> Result<Attachment> {
        if !safe_name(name) {
            bail!("{name:?} can't be an attachment's name");
        }
        self.check_room(size).map_err(|r| anyhow::anyhow!(r.why))?;
        let (sha256, size) = self
            .keep(reader, None)
            .map_err(|r| anyhow::anyhow!(r.why))?;
        Ok(Attachment {
            name: name.to_string(),
            size,
            sha256,
            path: None,
        })
    }

    /// Adds a file, under its own name.
    pub fn add_file(&self, path: &Path) -> Result<Attachment> {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .with_context(|| format!("{} has no file name", path.display()))?;
        let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let size = file.metadata()?.len();
        self.add(name.trim_start_matches('.'), &mut file, size)
            .with_context(|| format!("attaching {}", path.display()))
    }

    /// Removes attachments older than `age` that `used` doesn't name, and
    /// uploads that never finished. Returns how many went.
    pub fn prune(&self, age: Duration, used: &HashSet<String>) -> usize {
        let now = SystemTime::now();
        let old = |path: &Path| {
            fs::metadata(path)
                .and_then(|m| m.modified())
                .is_ok_and(|t| now.duration_since(t).is_ok_and(|d| d >= age))
        };
        let mut gone = 0;
        for dir in [self.dir.clone(), self.dir.join("incoming")] {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if path.is_file() && !used.contains(&name) && old(&path) {
                    gone += usize::from(fs::remove_file(&path).is_ok());
                }
            }
        }
        gone
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(max: u64, total: u64) -> Attachments {
        let dir = std::env::temp_dir().join(format!("tb-att-{}", new_id()));
        Attachments::new(dir, max, total)
    }

    const HELLO: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn keeps_a_file_by_its_hash() {
        let s = store(100, 1000);
        let a = s.add("hello.txt", &mut &b"hello"[..], 5).unwrap();
        assert_eq!(a.sha256, HELLO);
        assert_eq!(a.size, 5);
        assert!(s.has(&a));
        assert_eq!(fs::read(s.path(HELLO)).unwrap(), b"hello");
        let wrong_size = Attachment {
            size: 4,
            ..a.clone()
        };
        assert!(!s.has(&wrong_size));
        assert_eq!(s.add("again", &mut &b"hello"[..], 5).unwrap().sha256, HELLO);
    }

    #[test]
    fn refuses_what_isnt_what_it_claims() {
        let s = store(100, 1000);
        let bad = "0".repeat(64);
        let refused = s.receive(&bad, 5, &mut &b"hello"[..]).unwrap_err();
        assert_eq!(refused.code, 422);
        assert!(refused.why.contains("hash"), "{}", refused.why);
        assert!(!s.path(&bad).exists());
        assert!(!s.path(HELLO).exists());
        assert_eq!(fs::read_dir(s.dir.join("incoming")).unwrap().count(), 0);
        s.receive(HELLO, 5, &mut &b"hello"[..]).unwrap();
        assert!(s.path(HELLO).exists());
        assert_eq!(
            s.receive("../../etc", 1, &mut &b"x"[..]).unwrap_err().code,
            400
        );
    }

    #[test]
    fn keeps_to_its_limits() {
        let s = store(4, 1000);
        assert_eq!(
            s.receive(HELLO, 5, &mut &b"hello"[..]).unwrap_err().code,
            413
        );
        // A stream can't get past a size it lied about.
        let refused = s.receive(HELLO, 1, &mut &b"hello"[..]).unwrap_err();
        assert_eq!(refused.code, 413);
        let err = s.add("big", &mut &b"hello"[..], 5).unwrap_err();
        assert!(err.to_string().contains("max_attachment_bytes"), "{err}");

        let s = store(100, 6);
        s.add("a", &mut &b"hello"[..], 5).unwrap();
        let err = s.add("b", &mut &b"world"[..], 5).unwrap_err();
        assert!(err.to_string().contains("attachment_store_bytes"), "{err}");
    }

    #[test]
    fn names_and_hashes_stay_out_of_paths_until_checked() {
        for bad in ["", ".", "..", "../x", "a/b", ".hidden", "a\nb", "a\0b"] {
            assert!(!safe_name(bad), "{bad:?}");
        }
        assert!(safe_name("report final.txt"));
        assert!(safe_name(&"x".repeat(255)));
        assert!(!safe_name(&"x".repeat(256)));
        let a = |name: &str, sha: &str| Attachment {
            name: name.into(),
            size: 1,
            sha256: sha.into(),
            path: None,
        };
        assert!(well_formed(&a("x", HELLO)));
        assert!(!well_formed(&a("../x", HELLO)));
        assert!(!well_formed(&a("x", "../../etc/passwd")));
        assert!(!well_formed(&a("x", &HELLO.to_uppercase())));
    }

    #[test]
    fn prunes_what_nothing_needs() {
        let s = store(100, 1000);
        let keep = s.add("keep", &mut &b"keep"[..], 4).unwrap();
        let drop = s.add("drop", &mut &b"drop"[..], 4).unwrap();
        let used = HashSet::from([keep.sha256.clone()]);
        assert_eq!(s.prune(Duration::from_secs(3600), &used), 0);
        assert_eq!(s.prune(Duration::ZERO, &used), 1);
        assert!(s.has(&keep));
        assert!(!s.has(&drop));
    }
}
