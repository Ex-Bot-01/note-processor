//! The one spelling of the catalog file names this program writes (spec §7).
//!
//! `INDEX.md` at the workspace root, plus `INDEX-<first>-<last>.md` shards when a catalog is too big for
//! one file. Three callers read these names and none of them may hold its own copy: [`crate::catalog`]
//! builds them while it renders, [`crate::walk`] keeps them out of the corpus it discovers, and
//! [`mod@crate::scan`] decides which of them a rescan may replace. M2's `processed/` writer is the next reader
//! and must not have to copy the rule a fourth time.
//!
//! Only the *names* live here. The bytes inside a generated file belong to [`crate::catalog`], and the
//! rule about which generated file a scan may delete — its header fingerprint — stays in [`mod@crate::scan`],
//! which asks this module for names and predicates and holds no copy of either.

/// The catalog's entry point: the file every reader opens, and the one [`mod@crate::scan`] renames **last**,
/// because an entry point naming shards that were never written is a broken catalog while one that is
/// merely old is still a correct one.
pub const ENTRY: &str = "INDEX.md";
/// Stem shared by the entry point and every shard — the only place the word itself is spelled for
/// anything but [`ENTRY`], which a test pins to this stem plus [`MARKDOWN_SUFFIX`].
const STEM: &str = "INDEX";
/// Separator inside a generated name: between the stem and the bucket range, and between the range and an
/// occurrence number. ASCII hyphen, never the en dash the link label uses.
const SEPARATOR: &str = "-";
/// Extension of every generated file. This module owns its own copy on purpose: [`crate::walk`]'s
/// `MARKDOWN_SUFFIX` answers a different question — "is this note markdown?", folded case-insensitively —
/// while these names are matched at exact case because they are ours to write.
const MARKDOWN_SUFFIX: &str = ".md";

/// Name of the shard holding one bucket range's entries: `INDEX-<first>-<last>.md`.
///
/// `occurrence` is how often this range has already been rendered, counting from 1; from the second
/// occurrence on the number joins the name (`INDEX-m-m-2.md`), because two chunks of the same threshold
/// can cover one range and a repeated name would silently overwrite the earlier shard.
pub fn shard_name(first: char, last: char, occurrence: usize) -> String {
    // Zero is not "the first one": it would render the same name as occurrence 1 and let a second chunk
    // overwrite the first shard silently. The renderer counts from 1; anything else is a caller bug.
    debug_assert!(
        occurrence >= 1,
        "shard occurrences count from 1, got {occurrence}"
    );
    let repeat = if occurrence > 1 {
        format!("{SEPARATOR}{occurrence}")
    } else {
        String::new()
    };
    format!("{STEM}{SEPARATOR}{first}{SEPARATOR}{last}{repeat}{MARKDOWN_SUFFIX}")
}

/// Whether `name` — a bare file name, not a path — is one of the catalog names this program writes:
/// [`ENTRY`] itself or any shard.
///
/// Matching is exact-case, because these names are ours to write and a note called `index.md` is a note.
/// The rule is about names only; it decides what is *worth asking about*, never what is safe to remove —
/// that question is [`mod@crate::scan`]'s fingerprint to answer. Callers apply it to root-level names, since
/// generated files are placed at the workspace root: a nested `INDEX-old.md` is an ordinary note.
pub fn is_generated_index(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(MARKDOWN_SUFFIX) else {
        return false;
    };
    stem == STEM || is_shard_stem(stem)
}

/// Whether `stem` (a name without its extension) opens like a shard: the stem, then the separator. Any
/// range may follow — this asks only whose name it is, not whether the range is one a render produced.
fn is_shard_stem(stem: &str) -> bool {
    stem.strip_prefix(STEM)
        .is_some_and(|rest| rest.starts_with(SEPARATOR))
}

#[cfg(test)]
mod tests {
    use super::{is_generated_index, shard_name, ENTRY, MARKDOWN_SUFFIX, STEM};

    /// The one literal spelling of the entry point and the parts every other name is built from have to
    /// agree: a stem reworded on one side only would rename the shards while `ENTRY` kept pointing readers
    /// at the file nobody writes any more.
    #[test]
    fn entry_is_the_stem_with_the_markdown_suffix() {
        assert_eq!(ENTRY, format!("{STEM}{MARKDOWN_SUFFIX}"));
    }

    /// The shard name, byte for byte: hyphens around the range, and the occurrence number only from the
    /// second rendering of that range on.
    #[test]
    fn shard_names_carry_the_range_and_from_the_second_one_the_occurrence() {
        assert_eq!(shard_name('a', 'm', 1), "INDEX-a-m.md");
        assert_eq!(shard_name('1', '6', 1), "INDEX-1-6.md");
        assert_eq!(shard_name('m', 'm', 2), "INDEX-m-m-2.md");
        assert_eq!(shard_name('#', '#', 12), "INDEX-#-#-12.md");
    }

    #[test]
    fn generated_index_names_are_recognised_at_exact_case() {
        for name in [ENTRY, "INDEX-a-m.md", "INDEX-1-600.md"] {
            assert!(is_generated_index(name), "{name} is generated");
        }
        // Near misses, each a real file that must survive: a temp corpse (cleaned up by its own rule, never
        // by this one), a backup of a shard, the same name in another case, and things that merely resemble
        // our names.
        for name in [
            "INDEX-old.txt",
            "index.md",
            "My-INDEX.md",
            "notes.md",
            "INDEX.md.tmp",
            "INDEX-1-2.md.bak",
            "INDEX.MD",
        ] {
            assert!(!is_generated_index(name), "{name} is not generated");
        }
    }

    /// A shard the renderer can name must be a name the predicate recognises: [`crate::walk`] keeps these
    /// files out of the corpus and [`mod@crate::scan`] cleans up the superseded ones through this predicate,
    /// so a name one side builds and the other misses is a shard nothing ever removes.
    #[test]
    fn every_rendered_shard_name_is_recognised() {
        for occurrence in [1, 2, 12] {
            let name = shard_name('m', 'm', occurrence);
            assert!(is_generated_index(&name), "{name} is generated");
        }
    }
}
