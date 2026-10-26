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
//! The three writers of the key columns — [`crate::db::upsert_file`], [`crate::db::apply_move`] and
//! [`crate::db::insert_note`] — plus migration 4's copy loop all derive both keys from these functions, so a
//! key is never derived twice under two rules. `crate::walk` builds `case_key` through [`display_fold`] for
//! the same reason: SQLite's `LOWER()` folds ASCII only, so a fold computed in SQL would be a second,
//! different rule.
//!
//! **Round 1 keys are a faithful copy, not OS fidelity.** Every caller here hands over *display* text —
//! [`crate::hash::Snapshot`] holds strings and the statements take `&str` — so [`rel_bytes`] stores the bytes
//! of that spelling, U+FFFD included. Deriving the key from the byte-exact path instead, while `note_id`, the
//! catalog join and `known_rows` still match display text, is the half-moved state F13 §7 refuses; the
//! byte-exact input arrives with the round that moves those readers.

/// The identity key of one path: its bytes, unchanged.
///
/// Round 1's argument is display text (see the module note), so a name whose raw bytes were not valid UTF-8
/// arrives already carrying U+FFFD and is stored that way — `caf\u{FFFD}.md` yields
/// `63 61 66 EF BF BD 2E 6D 64`, not the file's real `63 61 66 E9 2E 6D 64`. That is deliberate and asserted
/// in F13 §5; the byte-exact form (`OsStrExt::as_bytes`) replaces this function's input in the round that
/// keys identity on bytes, for every writer at once.
pub fn rel_bytes(display: &str) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    use super::{case_fold, display_fold, rel_bytes};

    #[test]
    fn rel_bytes_keeps_the_spelling_it_was_given() {
        // Round 1's keys are a copy: the U+FFFD a lossy read produced stays in the key, and an ASCII name is
        // its own bytes. The byte-exact form is round 2's assertion (F13 §5).
        assert_eq!(
            rel_bytes("caf\u{fffd}.md"),
            vec![0x63, 0x61, 0x66, 0xEF, 0xBF, 0xBD, 0x2E, 0x6D, 0x64]
        );
        assert_eq!(rel_bytes("café.md"), "café.md".as_bytes().to_vec());
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
        assert_eq!(case_fold(&rel_bytes("A.md")), b"a.md".to_vec());
    }
}
