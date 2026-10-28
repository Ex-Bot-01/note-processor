//! Content hashing: sha256 as lowercase hex, over a byte slice or a file on disk.
//!
//! The digest is the identity every later task compares (spec §2: size and mtime only
//! trigger the look, the hash decides what happened), so its spelling has to be stable —
//! lowercase hex, 64 characters, no separators — and reading a file must not depend on how
//! big that file happens to be.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::walk::{self, DiscoveredFile};

/// Bytes read per iteration when hashing a file. Notes are small, but one pathological
/// multi-megabyte file must not decide how much memory a scan needs.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// What is known about one file at one moment: enough to classify what happened to it,
/// and enough to store in the registry row. `rel_bytes`, `case_bytes`, `rel`, `case_key`, `size` and
/// `mtime` come from [`crate::walk::DiscoveredFile`]; `hash` comes from [`sha256_file`] over its
/// contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The identity: OS bytes of the `/`-separated path. Both sides of every comparison key on this and
    /// on [`Snapshot::case_bytes`] — matching two snapshots by display `rel` is what merged `caf\xe9.md`
    /// with `caf\xe8.md` before F13 §4.1.
    pub rel_bytes: Vec<u8>,
    /// [`crate::path_keys::case_fold`] of [`Snapshot::rel_bytes`]: the identity on a case-insensitive
    /// filesystem.
    pub case_bytes: Vec<u8>,
    /// Display spelling of the path, `/`-separated — reports and registry rows only.
    pub rel: String,
    /// `rel` lowercased — the Unicode display fold link resolution reads. Not an identity: two live
    /// rows share it whenever their names differ only in a non-ASCII letter.
    pub case_key: String,
    /// Lowercase-hex sha256 of the contents. **An empty string is a marker, not a
    /// digest**: it means the file could not be read, and [`crate::change::classify`]
    /// drops such a snapshot instead of comparing it.
    pub hash: String,
    /// Size in bytes.
    pub size: u64,
    /// Modification time in whole seconds since the Unix epoch.
    pub mtime: i64,
    /// The OS's durable identity for this file: device and inode, widened to fixed width and encoded, on the
    /// platforms that expose one (design-a4 §2).
    ///
    /// **`None` means unknown, never "different".** It is what every non-Unix platform reports, what a row
    /// written before migration 6 carries until its path is next walked, and what a filesystem without the
    /// concept reports forever. Every route that reads it treats unknown as "no opinion", because the one
    /// failure this feature must never produce is a match refused.
    pub file_id: Option<Vec<u8>>,
}

impl Snapshot {
    /// The snapshot of one discovered file with the given content digest.
    ///
    /// All four spellings travel together from [`DiscoveredFile`] rather than being spelled out at each
    /// call site, because [`crate::change::classify`] matches identity on the byte keys and reports the
    /// display text: a snapshot whose `case_bytes` is not the fold of its own `rel_bytes`, or whose
    /// `case_key` is not its own `rel` lowercased, would match files that are not it. Pass an empty
    /// `hash` for a file whose contents could not be read — that is the marker
    /// [`crate::change::classify`] reads as "unknown".
    pub fn new(found: &DiscoveredFile, hash: String) -> Self {
        Snapshot {
            rel_bytes: found.rel_bytes.clone(),
            case_bytes: found.case_bytes.clone(),
            rel: found.rel.clone(),
            case_key: found.case_key.clone(),
            hash,
            size: found.size,
            mtime: found.mtime,
            file_id: found.file_id.clone(),
        }
    }
}

/// What can fail while hashing a file.
#[derive(Debug, Error)]
pub enum HashError {
    /// The file could not be opened or read to the end; the message names the path as the
    /// caller spelled it, because that is the spelling their report has to use.
    #[error("cannot read {}: {source}", .path.display())]
    Read {
        /// The file that could not be read, as the caller spelled it.
        path: PathBuf,
        /// Why the read failed.
        source: io::Error,
    },
}

/// sha256 of a file's contents as lowercase hex.
///
/// The file is opened through [`walk::to_open_path`], so a Windows path past `MAX_PATH`
/// still opens instead of failing or being truncated (spec §11), and it is consumed in
/// fixed-size chunks rather than read whole.
///
/// A failure is reported, never swallowed: an empty string here would be read downstream
/// as a real digest, so callers that want to keep going must record the path as unreadable
/// (an empty `Snapshot::hash`) instead of hashing on.
pub fn sha256_file(p: &Path) -> Result<String, HashError> {
    let open_path = walk::to_open_path(p);
    let fail = |source: io::Error| HashError::Read {
        path: p.to_path_buf(),
        source,
    };
    let mut file = File::open(&open_path).map_err(fail)?;

    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; READ_CHUNK_BYTES];
    loop {
        let read = file.read(&mut chunk).map_err(fail)?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

/// sha256 of bytes already in memory, formatted exactly like [`sha256_file`].
pub fn sha256_bytes(b: &[u8]) -> String {
    hex(&Sha256::digest(b))
}

/// Lowercase hex, the one place this module formats a digest.
fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // `{:02x}` keeps the leading zero, which a numeric conversion would drop.
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::hex;

    #[test]
    fn hex_pads_leading_zeroes_and_lowercases() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        assert_eq!(hex(&[]), "");
    }
}
