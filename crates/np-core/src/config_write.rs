//! The only writer of `config.toml` in this program (**F50**, issue **#7**).
//!
//! Until now every surface read config and none wrote it, so an application that had just been told *yes, download the
//! embedding model* had one move left: open np's TOML and edit it from outside. That is a caller re-implementing this module's
//! own rules badly — `provider = "openai-compatible"` refused in our words, `local-model` without a model refused, every
//! unknown key refused since **F35** — and the failure mode of getting them wrong is silent: an `[embeddings]` section that
//! resolves to nothing embeds zero notes and answers *success*. So np writes the file, and says what it changed.
//!
//! The one design decision worth stating before the code: **the write is validated by loading it.** The prospective bytes go
//! into a temporary file beside the real one and [`Config::load`] runs on *that* path before anything is renamed. Every rule
//! in [`crate::config`] then checks exactly what the next run would read, from the code that owns those rules — there is no
//! second validator here to drift out of step with the first, which is the failure this module exists to prevent.
//!
//! Everything else follows from the file being **the person's, not np's**: unrelated keys, comments, quoting style and blank
//! lines survive verbatim (there is deliberately no TOML re-serialiser in this path — `toml::to_string` would rewrite a whole
//! file alphabetically and take its comments with it), a file that does not load today is refused rather than half-edited, an
//! idempotent call writes nothing at all, publishing is a rename inside the directory, and what np touched is recorded as a
//! comment naming the command that did it.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{Config, ConfigError, EmbeddingProvider};
use crate::pins;

/// The `[embeddings]` keys this module can write, spelled as they appear in the file. `changed` in a caller's document lists
/// these, so the list is about **what np wrote** and never about what differs from some default.
pub const KEY_PROVIDER: &str = "embeddings.provider";
/// The model half of `[embeddings]`.
pub const KEY_MODEL: &str = "embeddings.model";

/// The comment prefix that marks a line as np's own provenance note. Recognised on rewrite so enabling twice leaves one such
/// line rather than one per click — the same reason `changed: []` writes nothing.
const PROVENANCE_PREFIX: &str = "# note-processor set";

/// What the caller wants `[embeddings]` to say afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingsTarget<'a> {
    /// Turn embeddings on for this build's local runtime, naming the pinned model to use. **Nothing is fetched here** —
    /// #7's second failure row is a state to avoid, and so is a method that surprises someone with 309 MB on the click that
    /// only *allowed* a download.
    On {
        /// A model id this build pins ([`pins::identity`]). Anything else refuses.
        model: &'a str,
    },
    /// Turn embeddings off: `provider = "none"`, and **`embeddings.model` is left exactly where it is**. Revoking consent to
    /// download is not a licence to erase which model was chosen; the file stays one key away from being on again, which is
    /// what someone who changed their mind wants.
    Off,
}

/// What a write did. The caller renders this into the document both surfaces answer (**F50**), where it travels beside the
/// provider, the model and the price of a fetch rather than instead of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingsWrite {
    /// The keys np wrote, in file order. **Empty means nothing was written** — no byte, no timestamp, no file that was not
    /// there before.
    pub changed: Vec<String>,
    /// Where it was written, or `None` when there is no file because there was nothing to change and none existed. A path is
    /// reported even when this call made no change but a file does exist: the question `persisted_to` answers is "which file
    /// now carries my decision?", and that question has the same answer before and after a repeat click.
    pub persisted_to: Option<PathBuf>,
}

/// One refusal, in our own words, carrying [`ConfigError`]'s shape so both surfaces print one sentence rather than two.
fn refused(message: String) -> ConfigError {
    ConfigError::Io(io::Error::new(io::ErrorKind::InvalidData, message))
}

/// The provider spellings as TOML writes them. Named because `serde`'s rename table is the only other place these strings
/// exist, and a drift here would write a value [`crate::config`] refuses to load — which the reload check below would catch,
/// loudly, on every call. `provider_spellings_match_the_loader` fails first if they ever separate.
const PROVIDER_LOCAL_MODEL: &str = "local-model";
const PROVIDER_NONE: &str = "none";

/// Which optional build-time capabilities a write must respect.
///
/// Production passes [`Runtime::of_this_build`]; the parameter exists because the refusal this drives is compiled away on
/// every host that has the runtime, and a rule whose failure arm nobody runs is a rule nobody checks — the same reasoning
/// that pushed the data-dir **shape** up to [`crate::config::Config::load_with_platform`] for **F37**, so the Windows fallback
/// could be graded on Linux. Here it means both arms of "may this build turn embeddings on?" are tested on every host,
/// including the default-feature CI row that has no runtime at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Runtime {
    /// Whether this binary was built with `local-model`, and so can embed anything.
    pub local_model: bool,
}

impl Runtime {
    /// The capabilities of the build this call is compiled into.
    pub fn of_this_build() -> Self {
        Self {
            local_model: cfg!(feature = "local-model"),
        }
    }
}

/// [`set_embeddings_in`] with this build's capabilities — the entry point both surfaces call.
pub fn set_embeddings(
    path: &Path,
    target: EmbeddingsTarget<'_>,
) -> Result<EmbeddingsWrite, ConfigError> {
    set_embeddings_in(path, target, &Runtime::of_this_build())
}

/// Write `[embeddings]` so it says [`EmbeddingsTarget`], in the file at `path`.
///
/// Refuses, having written nothing, when:
///
/// * `On` is asked of a build with no inference runtime (`local-model-not-built` — the token `np search` already uses, so a
///   GUI sees one word for one fact on both sides of the pipe);
/// * `On` names an id this build does not pin, listing what it *does* pin (the discipline behind `np models list`'s `pinned`:
///   never quote a size or a name for bytes nobody would download);
/// * the file exists and does not load today — its own error is returned, because half-editing a file its owner cannot
///   currently read turns a small mistake into an unusable installation;
/// * the prospective bytes do not load either (which means np wrote something its own rules reject);
/// * the file, once renamed, does not read back as what was asked — in which case the previous bytes are put back by the same
///   rename route before the refusal.
pub fn set_embeddings_in(
    path: &Path,
    target: EmbeddingsTarget<'_>,
    runtime: &Runtime,
) -> Result<EmbeddingsWrite, ConfigError> {
    let (wanted_provider, wanted_value, wanted_model, all_keys) = match target {
        EmbeddingsTarget::On { model } => {
            if !runtime.local_model {
                return Err(refused(
                    "this build has no inference runtime (`local-model-not-built`): enabling embeddings would write a \
                     provider that cannot embed anything. Rebuild with `--features local-model`, or leave embeddings off and \
                     keep the keyword half"
                        .to_string(),
                ));
            }
            if pins::identity(model).is_none() {
                let pinned: Vec<&str> = pins::PINNED_IDENTITIES
                    .iter()
                    .map(|pinned| pinned.id)
                    .collect();
                return Err(refused(format!(
                    "no pinned model \"{model}\": this build can name {} — enable one of those, or install bytes you already \
                     hold with `np models install`",
                    if pinned.is_empty() {
                        "nothing".to_string()
                    } else {
                        pinned
                            .iter()
                            .map(|id| format!("\"{id}\""))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                )));
            }
            (
                PROVIDER_LOCAL_MODEL,
                EmbeddingProvider::LocalModel,
                Some(model),
                vec![KEY_PROVIDER.to_string(), KEY_MODEL.to_string()],
            )
        }
        EmbeddingsTarget::Off => (
            PROVIDER_NONE,
            EmbeddingProvider::None,
            None,
            vec![KEY_PROVIDER.to_string()],
        ),
    };

    // **Rule 1: a file that does not load today is refused, not edited.** This also refuses a config whose `data_dir` neither
    // the file nor the environment supplied — that config does not load either, and writing into it would leave the same
    // refusal standing with one more key in the file.
    // An absent file is not a refusal: writing the first `[embeddings]` into an installation that has no config yet is the
    // ordinary shape of "the person said yes". A file that exists and does **not** load is the refusal (rule 1).
    let current = match Config::load(Some(path)) {
        Ok(loaded) => Some(loaded),
        Err(ConfigError::Io(err)) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(err),
    };
    let existing = match std::fs::read(path) {
        Ok(bytes) => Some(String::from_utf8(bytes).map_err(|_| {
            refused(format!(
                "{} is not valid UTF-8, so np cannot edit it without rewriting bytes it cannot read",
                path.display()
            ))
        })?),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(ConfigError::Io(err)),
    };

    // **`changed` lists the keys np writes, so it is computed per key** — against what the file *resolves to*, not against its
    // text, because an `provider = "local-model"` written with a comment beside it is already on and rewriting that line to add
    // one byte of whitespace would report a change nobody made. Switching one model for another therefore answers
    // `["embeddings.model"]` rather than claiming the provider moved too, and an empty list means no write happened at all.
    // An absent file is compared against np's own defaults rather than treated as "everything is new": `provider` is already
    // `none` by default, so **disabling on an installation that has no config writes no file at all**. Creating a TOML to say
    // the thing that happens anyway would be exactly the byte-nobody-asked-for that rule 3 refuses elsewhere.
    let effective = current.unwrap_or_default();
    let changed: Vec<String> = all_keys
        .into_iter()
        .filter(|key| match key.as_str() {
            KEY_PROVIDER => effective.embeddings.provider != wanted_value,
            _ => match wanted_model {
                Some(model) => effective.embeddings.model.as_deref() != Some(model),
                None => false, // disabling does not clear the model, so it never "changes" it
            },
        })
        .collect();
    if changed.is_empty() {
        return Ok(EmbeddingsWrite {
            changed,
            persisted_to: existing.as_ref().map(|_| path.to_path_buf()),
        });
    }

    let text = existing.clone().unwrap_or_default();
    let rewritten = rewrite_embeddings(&text, wanted_provider, wanted_model, &changed, target);

    // **Rule 4 plus the validation seam:** publish into a sibling temp file and load *it*. A cross-device rename would copy
    // rather than link, so the temp lives in the same directory by construction; and if the bytes are wrong the real file is
    // still exactly what it was.
    let temp = temporary_sibling(path);
    write_and_load(&temp, &rewritten, |loaded| {
        matches_provider(loaded, wanted_provider) && matches_model(loaded, wanted_model)
    })?;

    std::fs::rename(&temp, path).map_err(|err| {
        let _ = std::fs::remove_file(&temp);
        ConfigError::Io(err)
    })?;

    // **Rule 6: read it back.** A rule added later that rejects what this writer wrote shows up here rather than as a config
    // that silently stops loading, and the previous bytes go back before anything is reported.
    match Config::load(Some(path)) {
        Ok(loaded)
            if matches_provider(&loaded, wanted_provider)
                && matches_model(&loaded, wanted_model) =>
        {
            Ok(EmbeddingsWrite {
                changed,
                persisted_to: Some(path.to_path_buf()),
            })
        }
        other => {
            let detail = match other {
                Err(err) => err.to_string(),
                Ok(_) => {
                    "the file reads back with different values than the ones written".to_string()
                }
            };
            if let Err(err) = restore(path, &temp, existing.as_deref()) {
                return Err(ConfigError::Io(io::Error::new(
                    err.kind(),
                    format!(
                        "{} did not read back as intended ({detail}); putting the previous bytes back also failed ({err}) — \
                         the file needs a human",
                        path.display()
                    ),
                )));
            }
            Err(refused(format!(
                "{} did not read back as intended after writing ({detail}); the previous contents were restored and nothing \
                 was changed",
                path.display()
            )))
        }
    }
}

/// Whether the loaded config names the provider we meant. Compared through [`EmbeddingProvider`], because that enum is what
/// every run branches on — a string compare here would be a second source of truth about what "on" means.
fn matches_provider(loaded: &Config, wanted: &str) -> bool {
    let expected = match wanted {
        PROVIDER_LOCAL_MODEL => EmbeddingProvider::LocalModel,
        _ => EmbeddingProvider::None,
    };
    loaded.embeddings.provider == expected
}

/// Whether the loaded config names the model we meant. `None` means this call had no opinion: **disabling does not clear
/// `embeddings.model`**, so a reload that still carries one is the intended result.
fn matches_model(loaded: &Config, wanted: Option<&str>) -> bool {
    match wanted {
        Some(model) => loaded.embeddings.model.as_deref() == Some(model),
        None => true,
    }
}

/// Write `text` to `temp`, load it back through [`Config`] and check `agrees`. Any failure removes the temp file and returns
/// the refusal — which for a load error *is* np's own sentence about the value, because that is what a person needs to fix.
fn write_and_load(
    temp: &Path,
    text: &str,
    agrees: impl Fn(&Config) -> bool,
) -> Result<(), ConfigError> {
    if let Some(parent) = temp.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(temp, text).map_err(ConfigError::Io)?;
    let loaded = match Config::load(Some(temp)) {
        Ok(loaded) => loaded,
        Err(err) => {
            let _ = std::fs::remove_file(temp);
            return Err(err);
        }
    };
    if !agrees(&loaded) {
        let _ = std::fs::remove_file(temp);
        return Err(refused(
            "the edited config did not resolve to the provider and model that were asked for"
                .to_string(),
        ));
    }
    Ok(())
}

/// Put the file back. `previous` of `None` means there was no file, so "back" is deleting it — which is why the temp file is
/// reused as the vehicle only when there were bytes to write.
fn restore(path: &Path, temp: &Path, previous: Option<&str>) -> Result<(), io::Error> {
    let _ = std::fs::remove_file(temp);
    match previous {
        Some(text) => {
            std::fs::write(temp, text)?;
            std::fs::rename(temp, path)
        }
        None => std::fs::remove_file(path).or(Ok(())),
    }
}

/// `<dir>/<file>.<pid>.tmp` — a name that cannot collide with another np process on the same file, and that no reader of
/// `config.toml` will mistake for a config.
fn temporary_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".to_string());
    let stamped = format!("{name}.{}.tmp", std::process::id());
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(stamped),
        _ => PathBuf::from(stamped),
    }
}

/// Rewrite the `[embeddings]` table in the file's **text**, touching only the keys this call writes.
///
/// Line endings and the trailing-newline state are preserved (`\r\n` detected once per file, which is what a Windows editor
/// writes throughout), comments and unrelated keys are copied verbatim, a missing key is inserted under its table header, and
/// a missing table is appended at the end. np's provenance comment replaces any earlier one rather than piling up, so a second
/// `enable` leaves one line about a decision and not two about a click.
fn rewrite_embeddings(
    text: &str,
    provider: &str,
    model: Option<&str>,
    keys: &[String],
    target: EmbeddingsTarget<'_>,
) -> String {
    let crlf = text.contains("\r\n");
    let ends_newline = text.ends_with('\n');
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect();

    // Old provenance notes go first, wherever they were. Keeping them would make a file that changed nothing look like it
    // changed twice, which is the same lie `changed: []` refuses to tell.
    lines.retain(|line| !line.trim_start().starts_with(PROVENANCE_PREFIX));

    let wanted = wanted_keys(provider, model, keys);
    let note = provenance(target);

    match find_section(&lines, "embeddings") {
        Some((header, end)) => {
            // Replace in place first so an existing key keeps its position and its neighbours; then insert what is missing,
            // directly under the header, in the order that reads best (`provider` above `model`).
            let mut missing: Vec<(String, String)> = Vec::new();
            for (key, value) in wanted {
                match find_key(&lines, header, end, key) {
                    Some(at) => lines[at] = format!("{}{key} = {value}", indent_of(&lines[at])),
                    None => missing.push((key.to_string(), value)),
                }
            }
            // Reverse order into the same slot, so the finished file reads `provider` then `model`.
            for (key, value) in missing.into_iter().rev() {
                lines.insert(header + 1, format!("{key} = {value}"));
            }
            lines.insert(header, note);
        }
        None => {
            if !lines.is_empty() && !lines[lines.len() - 1].trim().is_empty() {
                lines.push(String::new());
            }
            let mut block = vec![note, "[embeddings]".to_string()];
            block.extend(
                wanted
                    .into_iter()
                    .map(|(key, value)| format!("{key} = {value}")),
            );
            lines.extend(block);
        }
    }

    let mut out = lines.join(if crlf { "\r\n" } else { "\n" });
    if ends_newline {
        out.push('\n');
    }
    out
}

/// The `(key, value)` pairs this call writes, in file order: `provider` first because that is what turns the feature on.
fn wanted_keys<'a>(
    provider: &'a str,
    model: Option<&'a str>,
    keys: &[String],
) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if keys.iter().any(|key| key == KEY_PROVIDER) {
        out.push(("provider", quote(provider)));
    }
    if keys.iter().any(|key| key == KEY_MODEL) {
        if let Some(model) = model {
            out.push(("model", quote(model)));
        }
    }
    out
}

/// A TOML basic string, escaped by `toml` itself rather than by hand — the id is pinned and therefore boring, and this module
/// should still not be the place that decides a quote in a value is safe.
fn quote(value: &str) -> String {
    toml::Value::String(value.to_string()).to_string()
}

/// The provenance line: what np set, with which command, at which instant, in this build's version. A comment because the
/// file must stay valid TOML for a run that knows nothing about this feature, and worded as a sentence a person can act on.
fn provenance(target: EmbeddingsTarget<'_>) -> String {
    let command = match target {
        EmbeddingsTarget::On { model } => format!("np embeddings enable {model}"),
        EmbeddingsTarget::Off => "np embeddings disable".to_string(),
    };
    format!(
        "{PROVENANCE_PREFIX} by `{command}` (np {}) at {}",
        env!("CARGO_PKG_VERSION"),
        utc_stamp(SystemTime::now())
    )
}

/// The leading whitespace of one line, so a key written into an indented table stays in that table's style.
fn indent_of(line: &str) -> String {
    let trimmed = line.trim_start();
    line[..line.len() - trimmed.len()].to_string()
}

/// `(header index, end index)` of `[name]`, where `end` is the first line of the next table or the end of the file.
fn find_section(lines: &[String], name: &str) -> Option<(usize, usize)> {
    let header = lines
        .iter()
        .position(|line| table_name(line) == Some(name))?;
    let end = lines[header + 1..]
        .iter()
        .position(|line| table_name(line).is_some())
        .map(|offset| header + 1 + offset)
        .unwrap_or(lines.len());
    Some((header, end))
}

/// The table a line opens, if it opens one — tolerating a trailing comment and surrounding whitespace, because `[embeddings]
/// # mine` is a header a hand-written file legitimately holds.
fn table_name(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix('[')?;
    let close = rest.find(']')?;
    let name = rest[..close].trim();
    if name.is_empty() {
        return None;
    }
    let after = rest[close + 1..].trim_start();
    if after.is_empty() || after.starts_with('#') {
        Some(name)
    } else {
        None
    }
}

/// The line index of `key` inside `[header, end)`, or `None`. A key matches only at a word boundary and outside a comment, so
/// `provider_id` is not `provider` and `# model = "x"` is not a value.
fn find_key(lines: &[String], header: usize, end: usize, key: &str) -> Option<usize> {
    (header + 1..end).find(|index| {
        let line = lines[*index].trim_start();
        if line.starts_with('#') {
            return false;
        }
        // The key is everything up to its first separator, so `provider_id` and `provider` are different keys and a bare word
        // on a line of its own matches nothing.
        match line.split_once(['=', ' ', '\t']) {
            Some((name, _)) => name == key,
            None => false,
        }
    })
}

/// Seconds since the Unix epoch as `2026-11-14T16:31:55Z`, computed here because this workspace depends on no date crate and
/// a provenance comment that says "recently" is not a timestamp. The civil-from-days inverse is Howard Hinnant's public-domain
/// algorithm; `utc_stamp_names_the_instant_it_was_given` pins it against known instants.
fn utc_stamp(now: SystemTime) -> String {
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0);
    let days = seconds.div_euclid(86_400);
    let time_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time_of_day / 3_600,
        (time_of_day % 3_600) / 60,
        time_of_day % 60
    )
}

/// The calendar date `z` days after 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_stamp_names_the_instant_it_was_given() {
        // Three instants whose spellings are known: the epoch, the moment this test was written against, and a leap day.
        assert_eq!(
            utc_stamp(SystemTime::UNIX_EPOCH),
            "1970-01-01T00:00:00Z",
            "the epoch is not off by a day"
        );
        assert_eq!(
            utc_stamp(UNIX_EPOCH + std::time::Duration::from_secs(1_763_124_715)),
            "2025-11-14T12:51:55Z",
            "a known instant spells itself"
        );
        assert_eq!(
            utc_stamp(UNIX_EPOCH + std::time::Duration::from_secs(1_698_019_200)),
            "2023-10-23T00:00:00Z",
            "and midnight on a non-leap boundary keeps its date"
        );
    }

    #[test]
    fn provider_spellings_match_the_loader() {
        // The two strings this module writes are serde's, not ours. If they ever separate, every call would fail the reload
        // check in `set_embeddings` — loudly, and on every platform, but here is where it should fail first.
        let on: crate::config::EmbeddingsConfig = toml::from_str(&format!(
            "provider = {}\nmodel = {}",
            quote(PROVIDER_LOCAL_MODEL),
            quote("x")
        ))
        .expect("`local-model` is the spelling the loader takes");
        assert_eq!(on.provider, EmbeddingProvider::LocalModel);
        let off: crate::config::EmbeddingsConfig =
            toml::from_str(&format!("provider = {}", quote(PROVIDER_NONE)))
                .expect("`none` is the spelling the loader takes");
        assert_eq!(off.provider, EmbeddingProvider::None);
    }

    #[test]
    fn a_table_header_is_recognised_with_a_comment_beside_it() {
        assert_eq!(table_name("[embeddings]"), Some("embeddings"));
        assert_eq!(table_name("  [embeddings] # mine"), Some("embeddings"));
        assert_eq!(table_name("[models]"), Some("models"));
        assert_eq!(table_name("# [embeddings]"), None, "a commented-out header");
        assert_eq!(table_name("provider = \"x\""), None);
        assert_eq!(
            table_name("[]"),
            None,
            "an empty name is not a table we look for"
        );
    }

    #[test]
    fn a_key_matches_at_a_word_boundary_and_never_inside_a_comment() {
        let lines: Vec<String> = [
            "[embeddings]",
            "# model = \"old\"",
            "provider_id = 3",
            "provider = \"none\"",
        ]
        .iter()
        .map(|line| line.to_string())
        .collect();
        assert_eq!(find_key(&lines, 0, lines.len(), "model"), None);
        assert_eq!(find_key(&lines, 0, lines.len(), "provider"), Some(3));
    }
}
