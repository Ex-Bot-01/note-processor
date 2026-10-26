//! The one owner of a path's two keys, and of the fold each one is built with.
//!
//! `files` carries two derived columns per row and they are **two different facts**, which is why they have
//! two names and two functions:
//!
//! - [`rel_bytes`] is the row's identity — the bytes of the path, stored in a BLOB column so an invalid byte
//!   survives instead of becoming U+FFFD (F13 §1). Uniqueness lives here and on its fold, never on the
//!   display text: two live files may spell themselves identically once the lossy spelling is admitted.
//! - [`case_fold`] of those bytes is the ASCII-case guard — it is what still reports `a.md` beside `A.md` as
//!   one file on a case-sensitive filesystem (`files.case_bytes UNIQUE`).
//! - [`display_fold`] is something else entirely: the Unicode fold of the *display* text, which answers how
//!   an author spelled a name and feeds link resolution (`links::resolve` looks a target up by it). It must
//!   not move with the byte key, or `[[ÄMLI]]` stops finding `Ämli.md`.
//!
//! No other module may write either column without calling through here (F13 §4: "one owner for the folds").
//! [`crate::walk`] builds a row's whole key set — [`rel_bytes`], [`case_fold`] and [`display_fold`] together,
//! so neither is ever written without the others — and migration 4's copy loop calls the same functions on the
//! one population it can only copy faithfully ([`rel_bytes_of_display`]). The statements that *carry* a key
//! ([`crate::db::upsert_file`], [`crate::db::apply_move`], [`crate::db::insert_note`]) take it as bytes from
//! their caller rather than deriving it, which is why they bind what the walk computed instead of inventing
//! their own rule.
//!
//! Both spellings of a path travel together, so they are one type: [`RelPath`]. A change list, a link corpus
//! or a write call that named only one of them would either lose the identity (text alone, which is F13) or
//! lose the line the report renders (bytes alone, which nobody can read).
//!
//! One thing this module cannot do is spell a Windows name exactly: its paths are UTF-16 code units and
//! `os_bytes` has to fall back to lossy text there. `name_is_lossless` is the refusal that keeps an
//! unspellable name from overwriting the twin whose key it collides with (F18) — a refusal, not byte
//! identity, which stays open in `docs/backlog-m2-followups.md`.

use std::ffi::OsStr;

/// One path in both spellings this program keeps: [`rel_bytes`] for identity, `rel` for display.
///
/// The pair exists so that no call site has to choose. Identity decisions — change classification, row
/// resolution, ordering, the note key — read [`RelPath::rel_bytes`]; reports and the rendered catalog read
/// [`RelPath::rel`], which stays the honest lossy spelling (F13 §3: display sites keep reading text *because*
/// they were always display-only). Two live rows may carry byte-identical `rel` values, so nothing that has
/// to tell rows apart may compare or store that field alone.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RelPath {
    /// The identity: OS bytes of the `/`-joined path (F13 §4.1).
    pub rel_bytes: Vec<u8>,
    /// The display spelling: `rel_bytes` decoded with replacements for anything UTF-8 cannot hold.
    pub rel: String,
}

impl RelPath {
    /// The pair from a caller that already holds both spellings — which is every caller that reached the disk.
    pub fn new(rel_bytes: Vec<u8>, rel: String) -> Self {
        Self { rel_bytes, rel }
    }

    /// The pair for a path spelled as text, so its key is the bytes of that spelling.
    ///
    /// This is the faithful-copy route, not OS fidelity: it exists for values that only ever existed as text
    /// (a rename target typed by hand in a test, a row written before byte keys). A path that came off the
    /// disk goes through [`crate::walk`], which has the real bytes.
    pub fn from_display(rel: &str) -> Self {
        Self {
            rel_bytes: rel_bytes_of_display(rel),
            rel: rel.to_string(),
        }
    }
}

/// The identity key of a workspace-relative path, from its OS components: their bytes joined with `/`.
///
/// `Normal` components only — the spelling [`crate::walk`] records — and the join is ASCII `/` on every
/// platform, because `rel` is this program's own relative spelling rather than the OS's absolute one. On Unix
/// the bytes are the raw ones, which is the whole point: `caf\xe9.md` and `caf\xe8.md` key apart while both
/// decode to `caf\u{FFFD}.md`. Windows has no byte spelling (its paths are UTF-16 and unpaired surrogates are
/// the lossy case), so it falls back to the lossy text and stays F13's documented residual (F13 §5).
pub fn rel_bytes<'a>(components: impl IntoIterator<Item = &'a OsStr>) -> Vec<u8> {
    let mut joined: Vec<u8> = Vec::new();
    for component in components {
        if !joined.is_empty() {
            joined.push(b'/');
        }
        joined.extend(os_bytes(component));
    }
    joined
}

/// The identity key of a row written before byte keys existed: the UTF-8 bytes of its stored display text.
///
/// Migration cannot invent bytes it never held (F13 §2), so migration 4's copy uses this rule and every
/// affected path is counted into [`crate::db::PATHS_LOSSY_KEY`] for a person to read against the churn the
/// first byte-keyed scan reports (F13 §4.2). For an ASCII or valid-UTF-8 name these *are* the OS bytes and
/// nothing moves; for a lossy one the key is repaired by that first scan, not here.
pub fn rel_bytes_of_display(display: &str) -> Vec<u8> {
    display.as_bytes().to_vec()
}

/// The ASCII-case fold of a byte key: `A`–`Z` lowered, every other byte passed through untouched.
///
/// Deliberately not `str::to_lowercase`. This fold guards identity (`files.case_bytes UNIQUE`) and so has to
/// keep ruling the P3 case — one file spelled two ways on a case-sensitive filesystem — while being total on
/// bytes that form no character. It also has to be exactly the rule [F13 §4]'s migration copy applies to rows
/// written before this module existed, and any two paths equal under it are already equal under
/// `str::to_lowercase`, so the new `UNIQUE` cannot refuse a database that was legal at version 3.
///
/// The consequence stated out loud (F13 §3's residual): a fold that stops at ASCII no longer folds `Ä` onto
/// `ä`, so two names differing *only* in a non-ASCII letter become two live rows where the old
/// `case_key UNIQUE` reported one skipped path. The Unicode half of that question stays with
/// [`display_fold`], which still answers it for authors.
pub fn case_fold(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .map(|byte| {
            // `u8::to_ascii_lowercase` is the whole rule: it changes only 0x41..=0x5A.
            byte.to_ascii_lowercase()
        })
        .collect()
}

/// The display fold of a path: [`str::to_lowercase`], the full Unicode lowercase mapping.
///
/// This is what `files.case_key` holds and what [`crate::links`] resolves a target against, so it folds
/// `ÄPFEL` onto `äpfel`. It is *not* an identity key: twins share a display fold by definition, which is why
/// migration 4 drops `case_key UNIQUE` and leaves uniqueness to [`case_fold`].
pub fn display_fold(rel: &str) -> String {
    rel.to_lowercase()
}

/// The bytes of one name or path as the OS holds them.
///
/// Shared with [`crate::scan`], which keys the root entries it may delete by these bytes so two entries
/// that share a lossy spelling stay two entries — the same reason identity lives in [`rel_bytes`] rather
/// than in `display`.
#[cfg(unix)]
pub(crate) fn os_bytes(name: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    name.as_bytes().to_vec()
}

/// The bytes of one name or path. Windows paths are UTF-16 code units and reach this program as text, so its
/// spelling stays lossy — the same stance [`crate::config`] takes for the workspace key (F13 §5).
#[cfg(not(unix))]
pub(crate) fn os_bytes(name: &OsStr) -> Vec<u8> {
    name.to_string_lossy().into_owned().into_bytes()
}

/// Whether a UTF-16 spelling holds a surrogate that is not part of a pair.
///
/// A high surrogate (`U+D800..=U+DBFF`) is only meaningful immediately followed by a low one
/// (`U+DC00..=U+DFFF`); the two together spell one `char`. Anything else — a high with no low behind it, a
/// low with no high in front of it — converts to no character at all, so `os_bytes`' lossy arm replaces
/// it with U+FFFD and two different OS names land on one byte key (F18). Two highs in a row leave the first
/// unpaired for exactly that reason: only the pair `high, low` is consumed together.
///
/// Pure over `u16`, and public so it stays testable on every platform: only Windows can reach this shape
/// through a filesystem, and `name_is_lossless`'s Unix arm answers `true` without looking, so without a
/// callable predicate F18's rule would be covered by nothing that CI can run.
pub fn has_unpaired_surrogate(units: &[u16]) -> bool {
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        if !is_high_surrogate(unit) {
            // A low surrogate here has no high in front of it: either nothing preceded it, or the preceding
            // high was already consumed by the pair before that one.
            if is_low_surrogate(unit) {
                return true;
            }
            index += 1;
            continue;
        }
        // The low unit has to be the *immediately* following one; a pair travels together, so it is
        // skipped as one step and the next iteration starts on the first unit after it.
        let paired = units.get(index + 1).copied().is_some_and(is_low_surrogate);
        if !paired {
            return true;
        }
        index += 2;
    }
    false
}

/// Whether `unit` is a high (leading) surrogate.
fn is_high_surrogate(unit: u16) -> bool {
    (0xD800..=0xDBFF).contains(&unit)
}

/// Whether `unit` is a low (trailing) surrogate.
fn is_low_surrogate(unit: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&unit)
}

/// Whether one OS name survives this module's byte spelling unchanged.
///
/// The Unix arm returns `true` for **every** input, and that is a fact about the platform rather than a
/// shortcut: [`os_bytes`]'s Unix arm takes the bytes straight out of the `OsStr`, so the key *is* the name
/// and there is no lossy step for a predicate to catch — not for a raw non-UTF-8 byte, and not for anything
/// else a path can hold. Checking here could therefore only ever refuse a file Unix indexes correctly today,
/// which F18 names as the worse failure (a silently skipped note costs more than the overwrite this refusal
/// prevents), so this answer is pinned by a test rather than by the argument above.
///
/// The other arm is Windows's — the only non-Unix target this program builds for, and the platform whose
/// paths are UTF-16 code units (see [`os_bytes`]). There a name holding an unpaired surrogate reaches
/// `to_string_lossy` as U+FFFD, so two names can share one key and round 2's lookup overwrites the twin's
/// row without any constraint firing. Such a name is refused instead: [`crate::walk`] leaves it out of the
/// catalog, its existing row untouched, and names it in the report (F18 §3).
#[cfg(unix)]
pub(crate) fn name_is_lossless(_name: &OsStr) -> bool {
    true
}

/// [`name_is_lossless`] for a platform whose names arrive as UTF-16 code units.
#[cfg(not(unix))]
pub(crate) fn name_is_lossless(name: &OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt;
    !has_unpaired_surrogate(&name.encode_wide().collect::<Vec<u16>>())
}

#[cfg(test)]
mod tests {
    use super::{case_fold, display_fold, rel_bytes, rel_bytes_of_display, RelPath};
    use std::ffi::OsStr;

    #[test]
    fn rel_bytes_joins_the_component_bytes_with_slash() {
        let components = [OsStr::new("notes"), OsStr::new("a.md")];
        assert_eq!(rel_bytes(components), b"notes/a.md".to_vec());
        // No components at all is the workspace root, which has no relative path (F13 §4.1).
        assert_eq!(rel_bytes([]), Vec::<u8>::new());

        // The one case F13 is about, as far as this module can show it on a platform that will not let a
        // `&str` hold the byte: a raw byte survives the key while the display text cannot. The end-to-end
        // form — a name built with `OsStrExt::from_bytes`, read off the disk, stored and reopened — is
        // `crates/np-core/tests/walk.rs` and `crates/np-core/tests/scan.rs`.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let latin1 = OsStr::from_bytes(b"caf\xe9.md");
            let key = rel_bytes([latin1]);
            assert_eq!(key, b"caf\xe9.md".to_vec());
            assert_ne!(
                String::from_utf8_lossy(&key).as_ref(),
                "caf\u{e9}.md",
                "the display spelling is where the byte stops being itself"
            );
        }
    }

    #[test]
    fn rel_bytes_of_display_is_the_faithful_copy() {
        // A migrated row can only be keyed by what it holds: the U+FFFD a lossy read produced stays in the
        // key, and an ASCII name is its own bytes.
        assert_eq!(
            rel_bytes_of_display("caf\u{fffd}.md"),
            vec![0x63, 0x61, 0x66, 0xEF, 0xBF, 0xBD, 0x2E, 0x6D, 0x64]
        );
        assert_eq!(
            rel_bytes_of_display("café.md"),
            "café.md".as_bytes().to_vec()
        );
        assert_eq!(RelPath::from_display("a.md").rel_bytes, b"a.md".to_vec());
    }

    #[test]
    fn case_fold_lowers_ascii_letters_and_nothing_else() {
        assert_eq!(case_fold(b"A/README.MD"), b"a/readme.md".to_vec());
        // A non-letter byte, a byte that is not ASCII, and a byte that is already lower case all pass
        // through: the fold is total, so it can be applied to bytes that form no character.
        assert_eq!(
            case_fold(&[0x00, 0xC3, 0x89, b'A', b'a']),
            vec![0x00, 0xC3, 0x89, b'a', b'a']
        );
    }

    #[test]
    fn the_two_folds_are_different_facts() {
        // The ASCII fold keeps P3's case pair together ...
        assert_eq!(case_fold(b"A.md"), case_fold(b"a.md"));
        // ... and stops short of the Unicode pair, while the display fold goes on to `ÄMLI` -> `ämli`.
        assert_ne!(
            case_fold("Ämli.md".as_bytes()),
            case_fold("ämli.md".as_bytes())
        );
        assert_eq!(display_fold("ÄMLI.md"), display_fold("ämli.md"));
        // A key and its fold are comparable byte strings, which is what the CHECK on both columns asks for.
        assert_eq!(case_fold(&rel_bytes_of_display("A.md")), b"a.md".to_vec());
    }
}
