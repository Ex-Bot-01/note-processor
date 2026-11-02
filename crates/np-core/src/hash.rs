//! Content hashing: sha256 as lowercase hex, over a byte slice or a file on disk.
//!
//! The digest is the identity every later task compares (spec §2: size and mtime only
//! trigger the look, the hash decides what happened), so its spelling has to be stable —
//! lowercase hex, 64 characters, no separators — and reading a file must not depend on how
//! big that file happens to be.
//!
//! **[`Sha256Stream`] is the program's one SHA-256 definition** (`docs/design-m2-local-embeddings.md`
//! §12 rule 9), and [`sha256_file`] / [`sha256_bytes`] are its two callers rather than two more
//! implementations. That ordering exists because a third caller is coming and it cannot be served by
//! either helper: a fetcher hashes **while it counts bytes**, so the digest has to be fed piecewise,
//! from whatever buffer the transfer happens to hand over, and neither `sha256_file` (which owns its
//! read loop) nor `sha256_bytes` (which needs the whole artifact in memory — 310 MB of it) can take
//! part in that. Exposing the stream is therefore the reason this module still has one digest path:
//! a second home for `sha2` would be a second place where the hex spelling could drift, and install
//! design §3 refuses exactly that dependency spread.

use std::fs::File;
use std::io::{self, Read, Write};
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

/// One sha256, fed in pieces: the program's single SHA-256 definition (§12 rule 9).
///
/// It exists for the caller that cannot produce its input all at once — a fetch hashes each buffer as
/// it arrives, because the byte count and the digest are read off the same pass (§12 rule 5). Both
/// [`sha256_file`] and [`sha256_bytes`] are written on top of it so that "what a digest looks like"
/// (lowercase hex, 64 characters, spelled by the module's private `hex`) is decided once: delegating costs them one
/// extra `update` call and buys the guarantee that no later caller can spell the same bytes' digest
/// differently.
///
/// [`finish_hex`](Self::finish_hex) consumes `self`, so "digest it twice" is not expressible — a
/// stream that has been finalised has nothing left to feed, and a second digest of a half-fed stream
/// is exactly the wrong number a caller could reach for.
#[derive(Debug, Default)]
pub struct Sha256Stream {
    hasher: Sha256,
}

impl Sha256Stream {
    /// A stream that has digested nothing yet — which is a real state with a published digest, not an
    /// error (see [`sha256_bytes`] on an empty slice).
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `bytes` to what this stream has seen. Feeding it the same bytes in one call or in ten
    /// gives the same digest, which is the property that makes a chunked transfer and a whole file
    /// comparable — `tests/hash.rs` pins it across split points.
    pub fn update(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
    }

    /// The digest of everything fed so far, as lowercase hex — the same spelling [`sha256_file`]
    /// returns for the same bytes.
    pub fn finish_hex(self) -> String {
        hex(&self.hasher.finalize())
    }
}

/// So a transfer can hash with [`io::copy`] and count in the same statement: `copy` reports the byte
/// total while this writes every byte into the digest, which keeps the two numbers from being two
/// different readings of one stream.
impl Write for Sha256Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.update(buf);
        Ok(buf.len())
    }

    /// A digest has nothing buffered below `update`, so there is nothing to flush.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
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
///
/// The loop feeds [`Sha256Stream`]; it owns nothing about the digest itself. That is the shape §12
/// rule 9 asks for — one definition, and this is one of its callers.
pub fn sha256_file(p: &Path) -> Result<String, HashError> {
    let open_path = walk::to_open_path(p);
    let fail = |source: io::Error| HashError::Read {
        path: p.to_path_buf(),
        source,
    };
    let mut file = File::open(&open_path).map_err(fail)?;

    let mut stream = Sha256Stream::new();
    let mut chunk = vec![0u8; READ_CHUNK_BYTES];
    loop {
        let read = file.read(&mut chunk).map_err(fail)?;
        if read == 0 {
            break;
        }
        stream.update(&chunk[..read]);
    }
    Ok(stream.finish_hex())
}

/// sha256 of bytes already in memory, formatted exactly like [`sha256_file`] — because it is the same
/// stream with one `update` call. Two formatters would be two chances for the spelling to drift.
pub fn sha256_bytes(b: &[u8]) -> String {
    let mut stream = Sha256Stream::new();
    stream.update(b);
    stream.finish_hex()
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
