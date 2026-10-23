//! Item candidates: the lines of a note that carry a status, and what that status is.
//!
//! Pure text in, structs out — no files, no database, no clock, and deliberately no identity or
//! timeline work. Deciding *which* lines claim a status is this module's whole job; M3 turns the
//! candidates into item timelines, Task 9 writes the rows.
//!
//! Three rules hold the module together.
//!
//! - **A marker is what makes a line an item.** Prose with no marker yields nothing, which is how
//!   "open" stays the absence of a marker *on an item* (spec §8.7) instead of a row for every
//!   sentence in the file. An explicit open claim is written, not inferred: `- [ ]`.
//! - **Two kinds of line are never items.** A fenced region and both of its fence lines come out of
//!   [`crate::parse::prose_lines`], the same iterator that decides headings, tags, words and links;
//!   an ATX heading comes out via [`crate::parse::is_heading_line`], because a heading names a topic
//!   rather than an item — `## Erledigt` is an outline entry and creates no candidate. Neither rule
//!   is re-implemented here.
//! - **One line yields at most one candidate.** The checkbox wins over the vocabulary, and among the
//!   configured markers the earliest in config order wins — every done marker before every paused
//!   one — wherever on the line each of them happens to sit. A line that both ticks a box and says
//!   `done` follows the box.

use crate::config::MarkerConfig;
use crate::parse::{is_heading_line, prose_lines};

/// The checkbox spellings and what each claims. These are not read from [`MarkerConfig`]: `- [ ]`,
/// the Open spelling, is in no marker list at all (spec §8.7 has open as everything else), and a box
/// has to be recognised before the vocabulary scan for it to outrank a keyword on the same line.
const CHECKBOXES: [(&str, ItemState); 2] = [("[ ]", ItemState::Open), ("[x]", ItemState::Done)];

/// The dashes that end an item's text when the marker behind them is cut out
/// (`vertrag angefragt — erledigt`).
const SENTENCE_DASHES: [char; 3] = ['-', '–', '—'];

/// The status one line claims about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemState {
    /// An item with no status marker on it — written as an unchecked box, never inferred from a
    /// line that carries no marker at all (see [`extract_items`]).
    Open,
    /// A line carrying one of [`MarkerConfig::done`], or a ticked box.
    Done,
    /// A line carrying one of [`MarkerConfig::paused`].
    Paused,
}

impl ItemState {
    /// The `items.state` spelling, which is inside the schema's CHECK domain in [`crate::db`] and
    /// nowhere else.
    pub fn as_str(self) -> &'static str {
        match self {
            ItemState::Open => "open",
            ItemState::Done => "done",
            ItemState::Paused => "paused",
        }
    }
}

/// One line that carries a status, before anything knows which item it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemCandidate {
    /// The line without its list bullet and without the matched marker text: ends trimmed and one
    /// trailing sentence dash dropped, but the interior is exactly what was written — including the
    /// gap the marker leaves behind. Nothing is reordered or re-flowed, so the text can still be
    /// found in the note by eye. Empty when the line is nothing but a marker, which keeps its row
    /// because the status claim is the fact worth having.
    pub text: String,
    /// 1-based line number in the file — the same number for an LF and a CRLF copy of the note,
    /// because the shared line model trims a trailing `\r` for content only.
    pub line: usize,
    /// The state the marker decided.
    pub state: ItemState,
    /// The marker that decided it, spelled exactly as config wrote it (never lowercased, never the
    /// line's own casing). A checkbox records its canonical form, `[ ]` or `[x]`, because a box comes
    /// from this module's rule rather than from config.
    pub marker: String,
}

/// Every item candidate a note's text carries, in reading order, at most one per line.
///
/// A line is a candidate when it carries a checkbox or one of `markers`' entries — as a bullet, an
/// ordered item, or a standalone line. Nothing else is: a paragraph with no marker yields nothing,
/// and so does `- buy milk`, a bullet with neither a box nor a keyword. That is the spec's rule that
/// **open is the absence of a marker on an item** (§8.7) rather than a property of prose.
///
/// Two exclusions come from shared decisions, not from local rules: lines inside a ``` fenced region
/// (including both fence lines) and ATX heading lines, `# …` through `###### …`. A heading with a
/// marker word in it names a topic and yields no item; seven hashes is not a heading by the outline's
/// own test, so it is prose like any other.
///
/// Per line the order is the plan's: checkbox first, then the configured markers in config order —
/// every entry of [`MarkerConfig::done`] before every entry of [`MarkerConfig::paused`] — and the
/// first match decides both `state` and which text is cut out. Position on the line does not change
/// which state wins, so `verschoben, dann erledigt` is done in either wording order. A keyword or
/// phrase matches case-insensitively on **word boundaries** (`download` and `abandoned` are not
/// `done`, `fertigmaßig` is not `fertig`); a literal marker such as `✅` or `- [x]` matches verbatim
/// wherever it appears.
///
/// Cost: one pass over the lines and one pass over each line, with no pattern compiled per line and
/// no per-line copy of the marker list — a character that cannot start any marker costs one table
/// lookup ([`MarkerSet::starts_at`]), so the work stays linear in the input even for a 5 MB single
/// line (see the `#[ignore]`d timing case in `tests/items.rs`). Like
/// [`crate::links::extract_links`] this sees text and not a note: it starts at line 1, so a marker
/// word written inside frontmatter counts as if it were body text.
pub fn extract_items(raw: &str, markers: &MarkerConfig) -> Vec<ItemCandidate> {
    let set = MarkerSet::new(markers);
    let mut found = Vec::new();
    for (number, line) in prose_lines(raw, 1) {
        if is_heading_line(line) {
            continue;
        }
        let body = strip_bullet(line);
        let hit = checkbox(body).or_else(|| {
            set.find(body)
                .map(|(start, end, marker)| (start, end, marker.state, marker.text.as_str()))
        });
        let Some((start, end, state, marker)) = hit else {
            continue;
        };
        found.push(ItemCandidate {
            // Cut rather than re-flow: what was left of the marker stays left of it, byte for byte.
            text: tidy(&format!("{}{}", &body[..start], &body[end..])),
            line: number,
            state,
            marker: marker.to_string(),
        });
    }
    found
}

/// The line with its list bullet removed: `-`, `*`, `+`, or digits followed by `.` or `)`, each of
/// which needs whitespace behind it so that `2 + 2 = 4` and a line starting `-ish` stay as written.
/// Indentation goes with the bullet — an indented item is still the item. A line with no bullet is
/// returned trimmed of leading whitespace only, which is what makes a standalone line carrying a
/// marker a candidate too.
fn strip_bullet(line: &str) -> &str {
    let text = line.trim_start();
    if let Some(rest) = text.strip_prefix(['-', '*', '+']) {
        if rest.starts_with([' ', '\t']) {
            return rest.trim_start();
        }
    }
    // The digit run is ASCII, so its end is a character boundary.
    let digits = text
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(text.len());
    if digits > 0 {
        if let Some(rest) = text[digits..].strip_prefix(['.', ')']) {
            if rest.starts_with([' ', '\t']) {
                return rest.trim_start();
            }
        }
    }
    text
}

/// The checkbox on this line: its span, the state it claims, and the box as this module spells it.
///
/// A box has to stand on its own — preceded by whitespace or the start of the line, and not followed
/// by a bracket — because `[x](notes/a.md)` is a link whose text is one letter and `[[x]]` is a
/// wikilink, neither of which claims anything about a status. A box directly followed by its text
/// still counts: `- [ ]買い物する` is how a note that writes no spaces between CJK words spells a
/// task, and rejecting it would silently lose the item.
///
/// `[` is ASCII, so every offset here lands on a character boundary even inside multi-byte text, and
/// the only case fold involved is the box's `x`.
fn checkbox(body: &str) -> Option<(usize, usize, ItemState, &'static str)> {
    let mut cursor = 0;
    while let Some(offset) = body[cursor..].find('[') {
        let start = cursor + offset;
        cursor = start + 1;
        if !preceded_by_space(body, start) {
            continue;
        }
        for (spelling, state) in CHECKBOXES {
            // `get` rather than a slice: three bytes from `start` can land inside a character.
            let Some(box_text) = body.get(start..start + spelling.len()) else {
                continue;
            };
            if !box_text.eq_ignore_ascii_case(spelling) {
                continue;
            }
            let end = start + spelling.len();
            if !matches!(body[end..].chars().next(), Some('[' | ']' | '(')) {
                return Some((start, end, state, spelling));
            }
        }
    }
    None
}

/// Whether a box at `start` opens a token rather than sitting inside one: `foo[ ]bar` is not a claim
/// about status.
fn preceded_by_space(body: &str, start: usize) -> bool {
    match body[..start].chars().next_back() {
        Some(ch) => ch.is_whitespace(),
        None => true,
    }
}

/// The final shape of [`ItemCandidate::text`]: surrounding whitespace gone, then one trailing
/// sentence dash with the whitespace in front of it gone too — that is what `vertrag angefragt —
/// erledigt` leaves once the marker is cut. A dash anywhere else is the author's own and stays.
fn tidy(text: &str) -> String {
    let text = text.trim();
    let dash = match text.chars().next_back() {
        Some(ch) if SENTENCE_DASHES.contains(&ch) => ch.len_utf8(),
        _ => 0,
    };
    text[..text.len() - dash].trim().to_string()
}

/// One configured marker in the shape the line scan wants.
struct Marker {
    /// The marker exactly as config wrote it — this is what [`ItemCandidate::marker`] records, so no
    /// folded spelling ever reaches a row.
    text: String,
    /// `text` lowercased, one character at a time, for matching without touching the line.
    folded: Vec<char>,
    /// The state this marker claims: which list it came from and nothing else.
    state: ItemState,
    /// Whether the word-boundary rule applies. A keyword or phrase is built only from word
    /// characters and spaces (`done`, `on hold`, `erledigt`); anything else — `✅`, `- [x]` — is a
    /// literal that matches verbatim wherever it appears.
    wordish: bool,
}

impl Marker {
    /// A marker ready to match, or nothing for an empty configured entry: an empty string occurs at
    /// every position, which would make every line an item.
    fn new(text: &str, state: ItemState) -> Option<Marker> {
        let folded: Vec<char> = text.to_lowercase().chars().collect();
        if folded.is_empty() {
            return None;
        }
        Some(Marker {
            text: text.to_string(),
            wordish: folded
                .iter()
                .all(|ch| ch.is_alphanumeric() || ch.is_whitespace()),
            state,
            folded,
        })
    }

    /// The offset just past this marker where it occurs at `offset`, and nothing when it does not.
    ///
    /// Case is folded one character at a time instead of lowercasing the line first:
    /// `to_lowercase` changes byte length for characters such as `İ`, while every offset here has to
    /// index the original line so the marker can be cut out of `text`. A source character whose
    /// lowercase spelling is longer than one character therefore never matches — the safe direction,
    /// since a missed hit loses one row while a wrong offset corrupts the text.
    fn match_end(&self, line: &str, offset: usize) -> Option<usize> {
        let mut rest = &line[offset..];
        for &want in &self.folded {
            let first = rest.chars().next()?;
            if !folded_eq(first, want) {
                return None;
            }
            rest = &rest[first.len_utf8()..];
        }
        let end = line.len() - rest.len();
        if self.wordish && !self.on_word_edges(line, offset, end) {
            return None;
        }
        Some(end)
    }

    /// Whether the span `offset..end` sits between two word edges. Only keyword markers are checked,
    /// which is what keeps `download` and `abandoned` from being `done` and `fertigmaßig` from being
    /// `fertig`, while a literal such as `✅` still matches anywhere it appears.
    fn on_word_edges(&self, line: &str, offset: usize, end: usize) -> bool {
        let before = match line[..offset].chars().next_back() {
            Some(ch) => !is_word_char(ch),
            None => true,
        };
        let after = match line[end..].chars().next() {
            Some(ch) => !is_word_char(ch),
            None => true,
        };
        before && after
    }
}

/// The configured markers as one flat sequence — every done marker before every paused marker, which
/// *is* the precedence rule rather than a detail of iteration — grouped by the character each one can
/// start with, so that scanning a line costs one lookup per character.
///
/// Built once per [`extract_items`] call and never per line: the plan's performance rule is that a
/// line costs one pass, which rules out both a pattern compiled per line and a fresh copy of the
/// vocabulary for every line.
struct MarkerSet {
    /// Every marker, in config order: done list first, then paused list.
    markers: Vec<Marker>,
    /// The positions of the markers that could begin with each lowercased ASCII character, each list
    /// in config order and therefore empty for a character no marker starts with. An ordinary word
    /// costs one array lookup; a plausible start character tests only the handful of markers sharing
    /// that letter rather than the whole vocabulary.
    ascii_starts: [Vec<usize>; ASCII_STARTS],
    /// The same lists for starting characters outside ASCII — `✅` is one — few enough to scan, and
    /// kept out of the table because a byte cannot hold them.
    other_starts: Vec<(char, Vec<usize>)>,
}

/// One slot per byte value, so an ASCII character's lookup needs no bounds check beyond its own.
const ASCII_STARTS: usize = 256;

impl MarkerSet {
    fn new(config: &MarkerConfig) -> MarkerSet {
        let markers: Vec<Marker> = config
            .done
            .iter()
            .map(|text| Marker::new(text, ItemState::Done))
            .chain(
                config
                    .paused
                    .iter()
                    .map(|text| Marker::new(text, ItemState::Paused)),
            )
            .flatten()
            .collect();
        let mut ascii_starts: [Vec<usize>; ASCII_STARTS] = std::array::from_fn(|_| Vec::new());
        let mut other_starts: Vec<(char, Vec<usize>)> = Vec::new();
        for (order, marker) in markers.iter().enumerate() {
            let first = marker.folded[0];
            if first.is_ascii() {
                ascii_starts[first.to_ascii_lowercase() as usize].push(order);
                continue;
            }
            match other_starts.iter_mut().find(|(ch, _)| *ch == first) {
                Some((_, orders)) => orders.push(order),
                None => other_starts.push((first, vec![order])),
            }
        }
        MarkerSet {
            markers,
            ascii_starts,
            other_starts,
        }
    }

    /// The marker positions that could begin at `ch`, in config order. A character whose lowercase
    /// spelling is several characters looks like its first character here and is then rejected by
    /// [`Marker::match_end`], so this may be over-eager but never wrong.
    fn starts_at(&self, ch: char) -> Option<&[usize]> {
        if ch.is_ascii() {
            return Some(&self.ascii_starts[ch.to_ascii_lowercase() as usize]);
        }
        self.other_starts
            .iter()
            .find(|(first, _)| folded_eq(ch, *first))
            .map(|(_, orders)| orders.as_slice())
    }

    /// The earliest configured marker occurring anywhere on this line, with the span of its leftmost
    /// occurrence.
    fn find(&self, body: &str) -> Option<(usize, usize, &Marker)> {
        // The lowest config position matched so far. A later position cannot displace it — which is
        // what makes done-list-before-paused-list a precedence rather than a tie-break — and once the
        // very first marker has matched there is nothing left to look for.
        let mut best = usize::MAX;
        let mut hit: Option<(usize, usize, &Marker)> = None;
        for (offset, ch) in body.char_indices() {
            let Some(orders) = self.starts_at(ch) else {
                continue;
            };
            for &order in orders {
                // Config order within the group, so anything at or after the best position found so
                // far is worse by definition and the list can stop here.
                if order >= best {
                    break;
                }
                let marker = &self.markers[order];
                if let Some(end) = marker.match_end(body, offset) {
                    best = order;
                    hit = Some((offset, end, marker));
                    break;
                }
            }
            if best == 0 {
                break;
            }
        }
        hit
    }
}

/// What continues a word, and therefore blocks a keyword match: letters and digits in any script
/// (`download`, `abandoned`, `fertigmaßig`) and the underscore that joins identifiers
/// (`done_done`). `/` deliberately does not block, so `status done/paused` still reads.
fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// Whether one character folds to an already-lowercase character. Both sides go through
/// `to_lowercase` so a character that folds to more than one never compares equal to a single
/// character of a marker.
fn folded_eq(ch: char, lower: char) -> bool {
    ch.to_lowercase().eq(lower.to_lowercase())
}
