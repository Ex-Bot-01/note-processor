//! Writing `config.toml` from inside np (**F50**, issue **#7**): what a consent answer changes, what it must not touch, and
//! what a refusal is allowed to leave behind.
//!
//! The cases are written against the **file**, because that is what a person opens afterwards: byte-for-byte comparisons of
//! unrelated keys and comments, a listing of the directory after a refusal, and a reload through [`Config`] rather than a
//! string match on np's own output. Both arms of "may this build turn embeddings on?" run on every host — the capability is a
//! parameter for the same reason F37 made the data-dir *shape* one, so the default-feature CI row that has no runtime still
//! grades the enable path.

use std::env;
use std::ffi::OsString;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use np_core::config::Config;
use np_core::config_write::{self, EmbeddingsTarget, Runtime, KEY_MODEL, KEY_PROVIDER};
use tempfile::TempDir;

/// The environment `Config::load` reads when a file does not name a `data_dir` itself — which is the state of a config written
/// by hand before this feature, and the only state in which `enable` on a *fresh* installation can be tested at all. Same shape
/// as `tests/config.rs`'s fixture: one lock, snapshot the keys, restore on drop.
static ENV_LOCK: Mutex<()> = Mutex::new(());
const ENV_KEYS: [&str; 5] = [
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "APPDATA",
    "LOCALAPPDATA",
];

struct TestEnv {
    _guard: MutexGuard<'static, ()>,
    _tmp: TempDir,
    /// What each variable held before, restored on drop. Held as `OsString` rather than `String` so a host whose `$HOME` is
    /// not valid UTF-8 puts back the bytes it had instead of nothing.
    previous: Vec<(&'static str, Option<OsString>)>,
}

impl TestEnv {
    fn install() -> Self {
        let guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tmp = TempDir::new().expect("test setup: tempdir");
        let previous = ENV_KEYS
            .iter()
            .map(|key| (*key, env::var_os(key)))
            .collect();
        // All five spellings the two lookups accept, on every platform: the XDG pair for unix, `APPDATA`/`LOCALAPPDATA` for
        // Windows, and `HOME` as the fallback underneath them.
        env::set_var("HOME", tmp.path());
        env::set_var("XDG_CONFIG_HOME", tmp.path().join("config"));
        env::set_var("XDG_DATA_HOME", tmp.path().join("data"));
        env::set_var("APPDATA", tmp.path().join("config"));
        env::set_var("LOCALAPPDATA", tmp.path().join("data"));
        // A developer's own mirror override would decide what an unrelated case resolves, exactly as a stray `$HOME` would.
        env::remove_var("NP_MODELS_BASE_URL");
        env::remove_var("NP_MODELS_SHA256");
        Self {
            _guard: guard,
            _tmp: tmp,
            previous,
        }
    }

    fn config_path(&self) -> PathBuf {
        self._tmp.path().join("config.toml")
    }

    /// A config that loads, written where `enable` will look. The `data_dir` line is what lets a case assert the *file's* word
    /// rather than the environment's.
    fn write(&self, text: &str) -> PathBuf {
        let path = self.config_path();
        fs::write(&path, text).expect("test setup: write config");
        path
    }

    /// A file that loads and says nothing about embeddings — the state #7 measured as "no section at all".
    fn untouched(&self) -> PathBuf {
        self.write("# my notes, my settings\n[catalog]\ncontinuation_indent = 3\n")
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        for (key, previous) in &self.previous {
            match previous {
                Some(value) => env::set_var(key, value),
                None => env::remove_var(key),
            }
        }
    }
}

/// The runtime every enable case asks with: a build that has the inference runtime. The other arm gets its own case.
const WITH_RUNTIME: Runtime = Runtime { local_model: true };
const WITHOUT_RUNTIME: Runtime = Runtime { local_model: false };

/// A model this build pins, read from the pin table rather than hardcoded, so a re-pin moves these cases with it instead of
/// failing on a string nobody changed.
fn a_pinned_id() -> String {
    np_core::pins::PINNED_IDENTITIES
        .first()
        .expect("test setup: this build pins at least one model")
        .id
        .to_string()
}

#[test]
fn enabling_writes_the_two_keys_and_leaves_everything_else_exactly_as_it_was() {
    let env = TestEnv::install();
    let path = env.write(
        "# my notes, my settings\n\
         data_dir = \"/home/agent/data\"\n\
         \n\
         [catalog]\n\
         max_entries_per_file = 40 # keep this comment\n\
         continuation_indent = 3\n\
         \n\
         [embeddings]\n\
         provider = \"none\"\n\
         # model = \"something-else\"\n\
         max_tokens = 2048\n\
         \n\
         [models]\n\
         auto_fetch = false\n",
    );

    let written = config_write::set_embeddings_in(
        &path,
        EmbeddingsTarget::On {
            model: &a_pinned_id(),
        },
        &WITH_RUNTIME,
    )
    .expect("a build with the runtime can enable a pinned model");
    assert_eq!(
        written.changed,
        vec![KEY_PROVIDER.to_string(), KEY_MODEL.to_string()],
        "both keys moved: the provider was off and no model was named"
    );
    assert_eq!(written.persisted_to.as_deref(), Some(path.as_path()));

    let text = fs::read_to_string(&path).expect("test step: read the file back");
    // **Everything np was not asked about is byte-identical**, in place, with its comment.
    for kept in [
        "# my notes, my settings",
        "data_dir = \"/home/agent/data\"",
        "[catalog]",
        "max_entries_per_file = 40 # keep this comment",
        "continuation_indent = 3",
        "# model = \"something-else\"",
        "max_tokens = 2048",
        "[models]",
        "auto_fetch = false",
    ] {
        assert!(
            text.contains(kept),
            "np rewrote a line it was not asked about: {kept}\n{text}"
        );
    }
    assert!(text.contains("provider = \"local-model\""), "{text}");
    assert!(
        text.contains(&format!("model = \"{}\"", a_pinned_id())),
        "{text}"
    );
    // **Provenance**, naming the command rather than a timestamp, because the sentence a person acts on is the command.
    assert!(
        text.contains(&format!("(np {}) at ", env!("CARGO_PKG_VERSION")))
            && text.contains("np embeddings enable"),
        "the file must say that np changed it and with what command:\n{text}"
    );
    // One section, in one place: the append path must not have fired when a table already existed.
    assert_eq!(text.matches("[embeddings]").count(), 1, "{text}");

    let reloaded = Config::load(Some(&path)).expect("the file np wrote must load");
    assert_eq!(
        reloaded.embeddings.model.as_deref(),
        Some(a_pinned_id().as_str())
    );
    assert_eq!(
        reloaded.catalog.continuation_indent, 3,
        "an unrelated key survived as a value, not just as text"
    );
    assert!(
        !reloaded.models.auto_fetch,
        "`auto_fetch` is not np's to set (**F50**)"
    );

    // Mutation this catches: re-serialising the file with `toml::to_string`, which drops every comment above.
}

#[test]
fn enabling_twice_changes_nothing_and_writes_no_bytes() {
    let env = TestEnv::install();
    let path = env.write("data_dir = \"/home/agent/data\"\n");
    let first = config_write::set_embeddings_in(
        &path,
        EmbeddingsTarget::On {
            model: &a_pinned_id(),
        },
        &WITH_RUNTIME,
    )
    .expect("test setup: enable once");
    assert!(!first.changed.is_empty(), "the first call has work to do");
    let bytes = fs::read_to_string(&path).expect("test step: read the file");

    let second = config_write::set_embeddings_in(
        &path,
        EmbeddingsTarget::On {
            model: &a_pinned_id(),
        },
        &WITH_RUNTIME,
    )
    .expect("enabling what is already enabled is not an error");
    assert_eq!(
        second.changed,
        Vec::<String>::new(),
        "nothing moved, and the answer says so"
    );
    assert_eq!(
        second.persisted_to.as_deref(),
        Some(path.as_path()),
        "the question `persisted_to` answers has the same answer before and after a repeat click"
    );
    assert_eq!(
        fs::read_to_string(&path).expect("test step: read it again"),
        bytes,
        "a second *Allow* must not append a second `[embeddings]`, a second provenance line, or one byte of whitespace"
    );

    // Mutation this catches: comparing against the file's **text** rather than what it resolves to — then a value written
    // with an unusual spacing is "changed" on every call, and a GUI that writes config on each start leaves a file np touched
    // a hundred times.
}

#[test]
fn switching_one_model_for_another_reports_only_the_model() {
    let env = TestEnv::install();
    let path = env.write(
        "data_dir = \"/home/agent/data\"\n\n[embeddings]\nprovider = \"local-model\"\nmodel = \"some-other-id\"\n",
    );

    let written = config_write::set_embeddings_in(
        &path,
        EmbeddingsTarget::On {
            model: &a_pinned_id(),
        },
        &WITH_RUNTIME,
    )
    .expect("switching models is an enable");
    assert_eq!(
        written.changed,
        vec![KEY_MODEL.to_string()],
        "the provider was already on, so claiming it moved would be a claim about a line np left alone"
    );

    // Mutation this catches: reporting the whole target's key list rather than what changed — which is also how a caller
    // learns that `changed` means "keys np wrote" and not "keys involved in the request".
}

#[test]
fn enabling_into_an_installation_with_no_config_file_creates_one() {
    let env = TestEnv::install();
    let path = env.config_path();
    assert!(!path.exists(), "test setup: nothing to write into yet");

    let written = config_write::set_embeddings_in(
        &path,
        EmbeddingsTarget::On {
            model: &a_pinned_id(),
        },
        &WITH_RUNTIME,
    )
    .expect("the first `[embeddings]` an installation ever gets is the ordinary case");
    assert_eq!(
        written.changed,
        vec![KEY_PROVIDER.to_string(), KEY_MODEL.to_string()],
        "both keys are new to a file that did not exist"
    );
    let text = fs::read_to_string(&path).expect("test step: the file exists now");
    assert!(text.contains("[embeddings]"), "{text}");
    assert!(text.contains("provider = \"local-model\""), "{text}");
    // The environment named the data dir here, so np must not invent one.
    assert!(
        !text.contains("data_dir"),
        "np writes the keys it was asked about and no others:\n{text}"
    );

    let reloaded = Config::load(Some(&path)).expect("a created config loads");
    assert_eq!(
        reloaded.embeddings.model.as_deref(),
        Some(a_pinned_id().as_str())
    );

    // No leftovers: the temp file is a sibling by design, and it must not outlive the call.
    let strays: Vec<String> = fs::read_dir(path.parent().expect("test setup: parent dir"))
        .expect("test step: list the directory")
        .filter_map(|entry| {
            entry
                .ok()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
        })
        .filter(|name| name.contains(".tmp"))
        .collect();
    assert_eq!(
        strays,
        Vec::<String>::new(),
        "a publish left a temp file behind"
    );

    // Mutation this catches: `Config::load(Some(path))` refusing an absent file and the refusal being read as "this config is
    // broken" — then no fresh installation could ever be enabled from a GUI.
}

#[test]
fn disabling_on_an_installation_with_no_config_writes_no_file() {
    // `provider` is `none` by default, so there is nothing to turn off — and creating a TOML to say the thing that happens
    // anyway is the byte-nobody-asked-for that this module's rule 3 refuses everywhere else.
    let env = TestEnv::install();
    let path = env.config_path();

    let written = config_write::set_embeddings_in(&path, EmbeddingsTarget::Off, &WITHOUT_RUNTIME)
        .expect("turning nothing on is not an error");
    assert_eq!(
        written.changed,
        Vec::<String>::new(),
        "the default was already off"
    );
    assert_eq!(written.persisted_to, None, "and there is no file to name");
    assert!(!path.exists(), "np did not create one to agree with itself");

    // Mutation this catches: treating an absent file as "every key is new", which writes a config that changes nothing and
    // then tells the person np edited their settings.
}

#[test]
fn disabling_turns_the_provider_off_and_leaves_the_model_where_it_is() {
    let env = TestEnv::install();
    let path = env.write(&format!(
        "data_dir = \"/home/agent/data\"\n\n[embeddings]\nprovider = \"local-model\"\nmodel = \"{}\"\n",
        a_pinned_id()
    ));

    let written = config_write::set_embeddings_in(&path, EmbeddingsTarget::Off, &WITHOUT_RUNTIME)
        .expect("turning embeddings off needs no runtime — that is the point of an off switch");
    assert_eq!(
        written.changed,
        vec![KEY_PROVIDER.to_string()],
        "revoking consent writes one key"
    );

    let text = fs::read_to_string(&path).expect("test step: read it back");
    assert!(text.contains("provider = \"none\""), "{text}");
    assert!(
        text.contains(&format!("model = \"{}\"", a_pinned_id())),
        "revoking consent to download is not a licence to erase which model was chosen:\n{text}"
    );

    let reloaded = Config::load(Some(&path)).expect("a disabled config still loads");
    assert_eq!(
        reloaded.embeddings.model.as_deref(),
        Some(a_pinned_id().as_str())
    );

    // And it is idempotent on the same terms as enable.
    let again = config_write::set_embeddings_in(&path, EmbeddingsTarget::Off, &WITHOUT_RUNTIME)
        .expect("disabling twice is not an error");
    assert_eq!(again.changed, Vec::<String>::new());
}

#[test]
fn a_build_with_no_runtime_refuses_and_writes_nothing() {
    let env = TestEnv::install();
    let path = env.untouched();
    let before = fs::read_to_string(&path).expect("test setup: read the file");

    let err = config_write::set_embeddings_in(
        &path,
        EmbeddingsTarget::On {
            model: &a_pinned_id(),
        },
        &WITHOUT_RUNTIME,
    )
    .expect_err("a build that cannot embed must not claim it can");
    let message = err.to_string();
    assert!(
        message.contains("local-model-not-built"),
        "the refusal carries the token `np search` already uses, so both surfaces name one fact one way: {message}"
    );
    assert_eq!(
        fs::read_to_string(&path).expect("test step: read it back"),
        before,
        "a refusal writes nothing"
    );

    // The same refusal on a machine with no config at all must not create one either.
    let fresh = env.config_path();
    fs::remove_file(&path).expect("test setup: remove the config");
    let err = config_write::set_embeddings_in(
        &fresh,
        EmbeddingsTarget::On {
            model: &a_pinned_id(),
        },
        &WITHOUT_RUNTIME,
    )
    .expect_err("no runtime is still no runtime");
    assert!(err.to_string().contains("local-model-not-built"), "{err}");
    assert!(!fresh.exists(), "a refusal does not install a config file");
}

#[test]
fn an_id_this_build_does_not_pin_refuses_and_names_the_ids_it_does() {
    let env = TestEnv::install();
    let path = env.untouched();
    let before = fs::read_to_string(&path).expect("test setup: read the file");

    let err = config_write::set_embeddings_in(
        &path,
        EmbeddingsTarget::On {
            model: "not-a-real-model",
        },
        &WITH_RUNTIME,
    )
    .expect_err("an unpinned id has no size, no digest and no code path");
    let message = err.to_string();
    assert!(
        message.contains("not-a-real-model"),
        "the refusal names what was refused: {message}"
    );
    for pinned in np_core::pins::PINNED_IDENTITIES {
        assert!(
            message.contains(pinned.id),
            "and lists what this build does pin, the way `np models list`'s `pinned` does: {message}"
        );
    }
    assert_eq!(
        fs::read_to_string(&path).expect("test step: read it back"),
        before,
        "a refusal writes nothing"
    );
    let strays: Vec<String> = fs::read_dir(path.parent().expect("test setup: parent dir"))
        .expect("test step: list the directory")
        .filter_map(|entry| {
            entry
                .ok()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
        })
        .filter(|name| name.contains(".tmp"))
        .collect();
    assert_eq!(
        strays,
        Vec::<String>::new(),
        "a refusal leaves no temp file"
    );
}

#[test]
fn a_file_that_does_not_load_today_is_refused_and_left_untouched() {
    // #7's front end must not be able to turn a small hand-edit into an unusable installation: the config's own error comes
    // back, and np's edit never happens.
    let env = TestEnv::install();
    let path = env.write(
        "data_dir = \"/home/agent/data\"\n\n[embeddings]\nprovider = \"none\"\nnot_a_key = 1\n",
    );
    let before = fs::read_to_string(&path).expect("test setup: read the file");

    let err = config_write::set_embeddings_in(
        &path,
        EmbeddingsTarget::On {
            model: &a_pinned_id(),
        },
        &WITH_RUNTIME,
    )
    .expect_err("F35 refuses unknown keys, so this file does not load");
    assert!(
        err.to_string().contains("not_a_key"),
        "the caller gets the config's own sentence about its own problem: {err}"
    );
    assert_eq!(
        fs::read_to_string(&path).expect("test step: read it back"),
        before,
        "np did not 'fix' anything while it was in there"
    );

    // Mutation this catches: editing the text first and loading afterwards, which "fixes" a broken file by overwriting the
    // owner's own mistake with np's opinion.
}

#[test]
fn a_file_whose_provider_this_build_refuses_is_refused_too() {
    // The same rule in a different shape: valid TOML that this build refuses to load. `openai-compatible` is the case F50's
    // rule 1 was written with in mind, because it is *parseable* and only validation objects — the exact class of file a
    // text-first editor would happily rewrite.
    let env = TestEnv::install();
    let path = env.write(
        "data_dir = \"/home/agent/data\"\n\n[embeddings]\nprovider = \"openai-compatible\"\n",
    );
    let before = fs::read_to_string(&path).expect("test setup: read the file");
    let err = config_write::set_embeddings_in(
        &path,
        EmbeddingsTarget::On {
            model: &a_pinned_id(),
        },
        &WITH_RUNTIME,
    )
    .expect_err(
        "`openai-compatible` is parseable and refused at load (**F50**'s rule 1 covers it)",
    );
    assert!(err.to_string().contains("openai-compatible"), "{err}");
    assert_eq!(
        fs::read_to_string(&path).expect("test step: read it back"),
        before
    );
}

#[cfg(unix)]
#[test]
fn a_publish_that_cannot_happen_leaves_the_file_alone_and_names_the_path() {
    // The publish is a rename inside the directory, so a directory that cannot take a file fails before anything is replaced.
    let env = TestEnv::install();
    let dir = env._tmp.path().join("locked");
    fs::create_dir(&dir).expect("test setup: create the directory");
    let path = dir.join("config.toml");
    fs::write(&path, "data_dir = \"/home/agent/data\"\n").expect("test setup: write the config");
    let before = fs::read_to_string(&path).expect("test setup: read the file");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500))
        .expect("test setup: make the directory read-only");

    let outcome = config_write::set_embeddings_in(
        &path,
        EmbeddingsTarget::On {
            model: &a_pinned_id(),
        },
        &WITH_RUNTIME,
    );
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
        .expect("test cleanup: unlock the directory");
    if let Err(err) = outcome {
        assert!(
            err.to_string().contains("config.toml")
                || format!("{err:?}").contains("PermissionDenied"),
            "an I/O refusal should still be recognisable: {err}"
        );
        assert_eq!(
            fs::read_to_string(&path).expect("test step: read it back"),
            before,
            "a failed publish leaves the previous file exactly as it was"
        );
    } else {
        // Running as root (some containers) ignores the mode bits. Say so rather than passing silently on a case that did not
        // test what it names.
        eprintln!(
            "note: the directory stayed writable, so this host ignores the permission being tested"
        );
    }
}

#[test]
fn a_config_that_names_its_data_dir_does_not_need_the_environment() {
    // The file's own `data_dir` is an instruction, and loading it is how np validates its edit. A case that needed neither
    // variable proves the validation seam reads the same rules a run does — including `[models]`, which this file sets to a
    // value load refuses.
    let env = TestEnv::install();
    let path = env.write("data_dir = \"/home/agent/data\"\n\n[models]\nbase_url = \"not a url\"\n");
    let err = config_write::set_embeddings_in(
        &path,
        EmbeddingsTarget::On {
            model: &a_pinned_id(),
        },
        &WITH_RUNTIME,
    )
    .expect_err("a config whose `[models] base_url` cannot be parsed does not load today");
    assert!(err.to_string().contains("base_url"), "{err}");
    assert!(
        Path::new(&path).exists(),
        "and np was not in there when it failed"
    );
}
