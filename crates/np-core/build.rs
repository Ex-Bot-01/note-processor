//! Builds and links the pinned llama.cpp into `np-core` — under `local-model`, and only there.
//!
//! Design authority: `docs/design-m2-local-embeddings.md` §14 (we own this build; no bindgen, no
//! pinned Rust binding) and §2's promise that a default `cargo test --workspace` never grows a C++
//! build. So the first thing this script does is check whether it was asked to exist at all:
//! with the feature off it prints one `rerun-if-changed` line and touches nothing — no cmake probe,
//! no filesystem read beyond cargo's own. That is what makes "default build unchanged" a fact rather
//! than a hope (measured in the round report: identical stripped bytes with and without this file).
//!
//! With the feature on it does four things, in this order, and refuses loudly at each failure:
//!
//! 1. reads `third_party/llama-cpp.toml` — the pin, which is the only place the tag, the digest and
//!    the CMake options are written;
//! 2. finds a source tree (`NP_LLAMA_CPP_SRC`, else `.vendor/llama.cpp-<tag>/`) and **verifies it is
//!    the pin** by reading its version numbers and the architecture token we pinned it for, rather
//!    than trusting a directory name;
//! 3. configures and builds target `llama` with cmake;
//! 4. emits the link search paths, the four static archives, and the system libraries they need.
//!
//! Two rules it exists to enforce:
//!
//!   * **Never fetch.** The fetch belongs to `scripts/vendor-llama-cpp.sh`, which a human or the one
//!     CI job runs. A build script that quietly reaches out fails in ways we cannot reproduce (§14).
//!   * **Never silently disable.** Every failure here is a panic naming what was missing and the
//!     command that fixes it. A release that quietly loses inference is worse than a loud failure —
//!     and unlike `cargo:warning=`, a panic cannot be scrolled past.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The pin file, relative to the workspace root. One copy of these values, two readers (the vendor
/// script reads `tag`/`url`/`sha256`; here we read `tag` and `[cmake]`).
const PIN: &str = "third_party/llama-cpp.toml";

/// The cmake target that produces everything we link. Building `install`, or the default target,
/// would compile binaries `np` never contains.
const TARGET: &str = "llama";

/// Static archives, in the order GNU ld resolves them: `llama` needs `ggml`, which needs `ggml-cpu`,
/// which needs `ggml-base`. Reversing this pair leaves `ggml_backend_*` undefined at link time.
const ARCHIVES: [&str; 4] = ["llama", "ggml", "ggml-base", "ggml-cpu"];

/// Libraries the archives need from outside themselves, measured rather than guessed:
/// `nm --undefined-only` over the four archives left `__cxa_*`/`_Unwind_Resume` (libstdc++),
/// `exp`/`log`/`pow`/`sqrt` (libm), `pthread_*` (libpthread) and `dlopen`/`dlsym`/`dlclose` from
/// `ggml.c`'s dynamic-backend loader (libdl). `llama.pc` and `ggml-config.cmake` add nothing else for
/// this configuration, and the link of the version/system-info probe confirmed it.
const SYSTEM_LIBS: [&str; 4] = ["stdc++", "m", "pthread", "dl"];

/// OpenMP is not in [`SYSTEM_LIBS`] because it is conditional: `libggml-cpu.a` and `libggml-base.a`
/// reference `GOMP_*` only when the tree was configured with OpenMP on. `ggml-config.cmake` gates its
/// own `OpenMP::OpenMP_C` interface on `GGML_OPENMP_ENABLED`, so we read that same cache entry rather
/// than hardcoding a second opinion — and a link error naming `GOMP_barrier` means this read is wrong.
const OPENMP_CACHE_KEY: &str = "GGML_OPENMP_ENABLED";
const OPENMP_LIB: &str = "gomp";

/// The architecture token that made v0.8.0 the pin (§13, §14): a tree without it cannot resolve
/// `general.architecture = gemma-embedding2`, whatever its version numbers say.
const REQUIRED_ARCH_TOKEN: &str = "LLM_ARCH_GEMMA_EMBEDDING2";

/// Ceiling on cmake's `-j`. Not a guess at the fastest build but at the largest safe one: §14 measured
/// 23 s at `-j 14` on a host with ~6.3 GB free RAM, and llama.cpp's translation units are large
/// enough that outrunning memory looks like a compiler being killed, not like a slow build. Override
/// with `NP_LLAMA_CPP_JOBS`.
const DEFAULT_JOB_CEILING: usize = 14;

fn main() {
    // Emitted first, and before anything is read: with the feature off this is the only line this
    // script prints and the only file it names.
    println!("cargo::rerun-if-changed=build.rs");

    if env::var_os("CARGO_FEATURE_LOCAL_MODEL").is_none() {
        return;
    }

    if let Err(problem) = link_pinned_llama() {
        panic!("{problem}");
    }
}

/// Everything the feature-on path does, so one `Err(String)` becomes one honest panic.
fn link_pinned_llama() -> Result<(), String> {
    // Every variable read below gets a `rerun-if-env-changed`, so re-pointing the build at a
    // different tree or a different cmake is enough to make cargo ask us again.
    for name in ["NP_LLAMA_CPP_SRC", "CMAKE", "NP_LLAMA_CPP_JOBS"] {
        println!("cargo::rerun-if-env-changed={name}");
    }

    let manifest = path_env("CARGO_MANIFEST_DIR")?;
    // crates/np-core -> the workspace root, which is where `.vendor/` and `third_party/` live.
    let root = manifest
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| format!("{manifest:?} is not inside a workspace"))?
        .to_path_buf();
    let out_dir = path_env("OUT_DIR")?;

    let pin_path = root.join(PIN);
    println!("cargo::rerun-if-changed={}", pin_path.display());
    let pin = Pin::read(&pin_path)?;
    eprintln!(
        "np-core: building llama.cpp {} for `local-model` ({} CMake options from {})",
        pin.tag,
        pin.options.len(),
        PIN
    );

    let source = resolve_source(&root, &pin)?;
    check_that_this_is_the_pin(&source, &pin)?;

    let cmake = cmake_command()?;
    let build_dir = choose_build_dir(&source, &out_dir);
    configure(&cmake, &source, &build_dir, &out_dir, &pin)?;
    compile(&cmake, &build_dir)?;
    emit_link_flags(&build_dir)
}

/// The C++ source to build: `NP_LLAMA_CPP_SRC` if set (offline work, CI caches, and the case where a
/// tree is already configured), else the directory `scripts/vendor-llama-cpp.sh` unpacks.
fn resolve_source(root: &Path, pin: &Pin) -> Result<PathBuf, String> {
    let override_dir = env::var("NP_LLAMA_CPP_SRC").unwrap_or_default();
    let trimmed = override_dir.trim();
    if !trimmed.is_empty() {
        let path = PathBuf::from(trimmed);
        if path.is_dir() {
            return Ok(path);
        }
        return Err(format!(
            "NP_LLAMA_CPP_SRC={trimmed} is not a directory.\n  \
             Unset it to use the vendored tree, or point it at llama.cpp {} unpacked from the pinned tarball.",
            pin.tag
        ));
    }

    let vendored = root.join(".vendor").join(format!("llama.cpp-{}", pin.tag));
    if vendored.is_dir() {
        return Ok(vendored);
    }

    Err(format!(
        "no llama.cpp {tag} source tree to build `local-model` from.\n  \
         tried: {}\n  \
         This build script never downloads anything (§14: no network during `cargo build`). Either run\n  \
           ./scripts/vendor-llama-cpp.sh\n  \
         once, which verifies the sha256 recorded in {PIN}, or name a tree you already have:\n  \
           NP_LLAMA_CPP_SRC=/path/to/llama.cpp-{tag} cargo build -p np-core --features local-model",
        vendored.display(),
        tag = pin.tag
    ))
}

/// Verify the tree is what we pinned, by reading it. A directory called `llama.cpp-v0.8.0` proves
/// nothing; a version triple and an architecture token do.
fn check_that_this_is_the_pin(source: &Path, pin: &Pin) -> Result<(), String> {
    if !source.join("include").join("llama.h").is_file() {
        return Err(format!(
            "{} has no include/llama.h, so it is not a llama.cpp source tree.",
            source.display()
        ));
    }

    let (want_major, want_minor, want_patch) = version_of_tag(&pin.tag)?;
    let cmake_lists = source.join("CMakeLists.txt");
    let text = read(&cmake_lists)?;
    for (field, want) in [
        ("MAJOR", want_major),
        ("MINOR", want_minor),
        ("PATCH", want_patch),
    ] {
        let found = cmake_set_value(&text, &format!("LLAMA_VERSION_{field}")).ok_or_else(|| {
            format!(
                "{} does not declare LLAMA_VERSION_{field}, so it is not the llama.cpp we pinned ({}).",
                cmake_lists.display(),
                pin.tag
            )
        })?;
        if found != want.to_string() {
            return Err(format!(
                "{} declares LLAMA_VERSION_{field} {found}, but {} pins {}.\n  \
                 Refusing to build a tree that is not the one whose sha256 we recorded.",
                cmake_lists.display(),
                pin.tag,
                PIN
            ));
        }
    }

    let arch = source.join("src").join("llama-arch.cpp");
    if !read(&arch)?.contains(REQUIRED_ARCH_TOKEN) {
        return Err(format!(
            "{} does not mention {REQUIRED_ARCH_TOKEN}. That token is why {} was pinned (§14): a tree \
             without it cannot resolve `general.architecture = gemma-embedding2`, so building it would \
             buy 10 MB of runtime that cannot run our model.",
            arch.display(),
            pin.tag
        ));
    }

    Ok(())
}

/// `v0.8.0` -> `(0, 8, 0)`. The tag is the version; there is no second place to read it from.
fn version_of_tag(tag: &str) -> Result<(u8, u8, u8), String> {
    let mut parts = tag.trim_start_matches('v').split('.');
    let mut next = |what: &str| -> Result<u8, String> {
        parts
            .next()
            .ok_or_else(|| format!("{PIN}: tag {tag} has no {what} number"))?
            .parse::<u8>()
            .map_err(|_| format!("{PIN}: tag {tag} has a non-numeric {what} part"))
    };
    let version = (next("major")?, next("minor")?, next("patch")?);
    if parts.next().is_some() {
        return Err(format!("{PIN}: tag {tag} is not a plain X.Y.Z tag"));
    }
    Ok(version)
}

/// Value of `set(<name> <value>)` in a CMakeLists file, or None when absent.
fn cmake_set_value(text: &str, name: &str) -> Option<String> {
    let prefix = format!("set({name}");
    let line = text
        .lines()
        .find(|line| line.trim_start().starts_with(&prefix))?;
    let after = line[line.find(&prefix)? + prefix.len()..].trim_start();
    Some(after.split([')', ' ', '\t']).next()?.to_string())
}

/// The cmake binary: `CMAKE` if set, else whatever is on PATH. Probed with `--version`, because the
/// failure we are trying to explain is "cmake is not installed", and cargo's own message for a failed
/// `Command::spawn` is `No such file or directory (os error 2)`.
fn cmake_command() -> Result<OsString, String> {
    let configured = env::var("CMAKE").unwrap_or_default();
    let binary: OsString = if configured.trim().is_empty() {
        OsString::from("cmake")
    } else {
        OsString::from(configured.trim())
    };

    let probe = Command::new(&binary).arg("--version").output();
    match probe {
        Ok(out) if out.status.success() => Ok(binary),
        Ok(out) => Err(format!(
            "`{:?} --version` failed: {}",
            binary,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(error) => Err(format!(
            "cannot run cmake as {binary:?} ({error}).\n  \
             Point the build at it — cmake needs no root, a Kitware tarball unpacked anywhere works:\n  \
               CMAKE=$HOME/tools/cmake/bin/cmake cargo build -p np-core --features local-model\n  \
             (§14 measured this host with exactly that path.)"
        )),
    }
}

/// Reuse a build directory the source tree already has — that is how `NP_LLAMA_CPP_SRC` avoids
/// recompiling C++ on every cargo run — and otherwise build inside `OUT_DIR`, where cargo owns it.
fn choose_build_dir(source: &Path, out_dir: &Path) -> PathBuf {
    let in_tree = source.join("build");
    if in_tree.join("CMakeCache.txt").is_file() {
        return in_tree;
    }
    out_dir.join(format!("llama-cpp-{}", TARGET))
}

/// Two paths name one directory when both exist and resolve the same — `/tmp` behind a symlink is ordinary on
/// macOS, and a trailing separator is ordinary in an environment variable.
fn same_directory(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Configure unless the cache already says what the pin asks for. A cache that disagrees is
/// reconfigured rather than trusted: linking a tree built with different options (shared libs, or
/// curl on) would put bytes in `np` that nothing in this repository chose.
fn configure(
    cmake: &OsString,
    source: &Path,
    build_dir: &Path,
    out_dir: &Path,
    pin: &Pin,
) -> Result<(), String> {
    let mut cache = read_cache(&build_dir.join("CMakeCache.txt"));

    // **A cmake cache remembers which source tree it was generated from, and refuses another.** Pointed at a
    // different `-S` it fails with "does not match the source … used to generate cache", so switching between
    // `.vendor/llama.cpp-<tag>` and `NP_LLAMA_CPP_SRC` — the two ways this build is legitimately reached, and the
    // difference between a CI cache and a developer's tree — would break the build with an error that names
    // neither cause nor cure. Wipe the directory cargo owns and configure fresh; never touch someone else's build
    // tree without saying so.
    if let Some(found) = cache.get("CMAKE_HOME_DIRECTORY") {
        let cached = Path::new(found);
        if !same_directory(cached, source) {
            if !build_dir.starts_with(out_dir) {
                return Err(format!(
                    "{} was configured for {} and is not inside cargo's OUT_DIR, so this build script \n  \
                     will not delete it. Remove that directory (it is generated), or point NP_LLAMA_CPP_SRC \n  \
                     back at {found}.",
                    build_dir.display(),
                    cached.display()
                ));
            }
            eprintln!(
                "np-core: {} holds a cache generated for {}; wiping it to configure {}",
                build_dir.display(),
                cached.display(),
                source.display()
            );
            fs::remove_dir_all(build_dir)
                .map_err(|e| format!("cannot wipe {}: {e}", build_dir.display()))?;
            cache.clear();
        }
    }
    let stale: Vec<String> = pin
        .options
        .iter()
        .filter(|(key, want)| {
            // cmake stores an `-DFOO=BAR` for a not-yet-declared option under the `UNINITIALIZED`
            // type, so only the value is comparable here, not the whole line.
            !cache
                .get(key.as_str())
                .is_some_and(|found| found.eq_ignore_ascii_case(want))
        })
        .map(|(key, want)| format!("{key}={want}"))
        .collect();

    if cache.contains_key("CMAKE_BUILD_TYPE") && stale.is_empty() {
        eprintln!(
            "np-core: {} already configured for the pin's options; skipping configure",
            build_dir.display()
        );
        return Ok(());
    }

    fs::create_dir_all(build_dir)
        .map_err(|e| format!("cannot create {}: {e}", build_dir.display()))?;
    let mut args: Vec<String> = vec![
        "-S".to_string(),
        source.display().to_string(),
        "-B".to_string(),
        build_dir.display().to_string(),
    ];
    for (key, value) in &pin.options {
        args.push(format!("-D{key}={value}"));
    }
    eprintln!(
        "np-core: configuring llama.cpp in {} ({} changed: {})",
        build_dir.display(),
        stale.len(),
        if stale.is_empty() {
            "first configure".to_string()
        } else {
            stale.join(", ")
        }
    );
    run(cmake, &args, "configure").map(drop)
}

fn compile(cmake: &OsString, build_dir: &Path) -> Result<(), String> {
    let args = [
        "--build".to_string(),
        build_dir.display().to_string(),
        "--target".to_string(),
        TARGET.to_string(),
        "--parallel".to_string(),
        jobs().to_string(),
    ];
    run(cmake, &args, "build").map(drop)
}

/// `-j` for cmake: `NP_LLAMA_CPP_JOBS`, else cores capped at [`DEFAULT_JOB_CEILING`].
fn jobs() -> usize {
    let configured = env::var("NP_LLAMA_CPP_JOBS").unwrap_or_default();
    if let Ok(parsed) = configured.trim().parse::<usize>() {
        if parsed > 0 {
            return parsed;
        }
    }
    let cores = fs::metadata("/proc/cpuinfo")
        .and_then(|_| std::thread::available_parallelism().map(|n| n.get()))
        .unwrap_or(1);
    cores.min(DEFAULT_JOB_CEILING)
}

/// Run cmake, capturing both streams. Output is only interesting when something failed, which is
/// exactly when it becomes part of the panic message; on success a build script that echoed 40 lines
/// of configure chatter would be noise.
fn run(cmake: &OsString, args: &[String], step: &str) -> Result<String, String> {
    let output = Command::new(cmake)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run {:?}: {e}", cmake))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        return Err(format!(
            "cmake {step} failed ({})\n  command: {cmake:?} {}\n{}",
            output.status,
            args.join(" "),
            indent(&tail(&text, 4000))
        ));
    }
    Ok(text)
}

/// The last `max` bytes — for a failing compile, the reason is at the end, and the full log of a
/// 40-line configure plus a 2-minute build would bury it.
fn tail(text: &str, max: usize) -> String {
    let bytes = text.as_bytes();
    if bytes.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("... (earlier output trimmed)\n{}", &text[start..])
}

fn indent(text: &str) -> String {
    text.lines().map(|line| format!("  | {line}\n")).collect()
}

/// Emit the link instructions: four static archives from our build, then the libraries they need.
fn emit_link_flags(build_dir: &Path) -> Result<(), String> {
    let mut searched: Vec<PathBuf> = Vec::new();
    for name in ARCHIVES {
        let archive = find_archive(build_dir, name)?;
        let dir = archive
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", archive.display()))?;
        if !searched.contains(&dir.to_path_buf()) {
            searched.push(dir.to_path_buf());
            println!("cargo::rustc-link-search=native={}", dir.display());
        }
        println!("cargo::rustc-link-lib=static={name}");
    }

    for name in SYSTEM_LIBS {
        println!("cargo::rustc-link-lib=dylib={name}");
    }

    let cache = read_cache(&build_dir.join("CMakeCache.txt"));
    let openmp_on = cache
        .get(OPENMP_CACHE_KEY)
        .or_else(|| cache.get("GGML_OPENMP"))
        .is_some_and(|value| value.eq_ignore_ascii_case("ON") || value == "1");
    if openmp_on {
        println!("cargo::rustc-link-lib=dylib={OPENMP_LIB}");
    }
    eprintln!(
        "np-core: linked [{ARCHIVES:?}] + system libs (OpenMP: {})",
        if openmp_on { "on" } else { "off" }
    );
    Ok(())
}

/// Find `lib<name>.a` (or `<name>.lib`) under the build directory. Recursive rather than hardcoded to
/// `src/` and `ggml/src/`, because those paths are this revision's layout, not a promise — but bounded
/// so a stray tree cannot make a build script wander forever.
fn find_archive(build_dir: &Path, name: &str) -> Result<PathBuf, String> {
    let names = [format!("lib{name}.a"), format!("{name}.lib")];
    let mut found: Vec<PathBuf> = Vec::new();
    let mut queue: Vec<PathBuf> = vec![build_dir.to_path_buf()];
    while let Some(dir) = queue.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                // Build-system bookkeeping, never an archive we want.
                if file_name != "CMakeFiles" {
                    queue.push(path);
                }
            } else if names.contains(&file_name) {
                found.push(path);
            }
        }
    }

    found.sort();
    found.into_iter().next().ok_or_else(|| {
        format!(
            "the build produced no {name} archive under {}.\n  \
             It was configured with the options in {PIN} and built with `--target {TARGET}`; \
             if that target ever stops producing this library, the pin needs a new archive list — \
             not a silent skip.",
            build_dir.display()
        )
    })
}

/// The parsed subset of the pin we build from: the tag, and the `[cmake]` table in file order.
struct Pin {
    tag: String,
    options: Vec<(String, String)>,
}

impl Pin {
    /// A line scan for `key = "value"`, with `[section]` headers — no TOML crate here on purpose
    /// (build-dependencies are dependencies, and §14's rule is that we add none to avoid bindgen).
    /// Anything that is not that shape is refused rather than ignored: a silently dropped option is a
    /// binary built with settings nobody wrote down.
    fn read(path: &Path) -> Result<Self, String> {
        let text = read(path)?;
        let mut tag = None;
        let mut options = Vec::new();
        let mut section = None;

        for (number, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
            {
                section = Some(name.to_string());
                continue;
            }
            let (key, value) = line.split_once('=').ok_or_else(|| {
                format!(
                    "{}:{number}: not a `key = \"value\"` line: {line}",
                    path.display()
                )
            })?;
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                .ok_or_else(|| {
                    format!(
                        "{}:{number}: {} must be a quoted string on one line: {line}",
                        path.display(),
                        key.trim()
                    )
                })?;
            match section.as_deref() {
                None if key.trim() == "tag" => tag = Some(value.to_string()),
                // `url` and `sha256` are the vendor script's business; a build script that could
                // download them would be the thing §14 rules out.
                None => {}
                Some("cmake") => options.push((key.trim().to_string(), value.to_string())),
                Some(other) => {
                    return Err(format!(
                        "{}:{number}: unknown section [{other}] in the pin file",
                        path.display()
                    ));
                }
            }
        }

        let tag = tag.ok_or_else(|| format!("{}: no `tag` in the pin file", path.display()))?;
        if options.is_empty() {
            return Err(format!(
                "{}: no `[cmake]` options, so the build would use llama.cpp's own defaults — which \
                 include building tests, tools, examples and the app (§14 chose otherwise on size grounds).",
                path.display()
            ));
        }
        Ok(Self { tag, options })
    }
}

/// `KEY:TYPE=VALUE` lines of a CMakeCache file; missing or unreadable is an empty map, which the
/// callers read as "not configured yet".
fn read_cache(path: &Path) -> std::collections::BTreeMap<String, String> {
    let mut entries = std::collections::BTreeMap::new();
    let Ok(text) = fs::read_to_string(path) else {
        return entries;
    };
    for line in text.lines() {
        if line.starts_with('#') || !line.contains(':') {
            continue;
        }
        if let Some((key, rest)) = line.split_once(':') {
            if let Some((_, value)) = rest.split_once('=') {
                entries.insert(key.to_string(), value.trim().to_string());
            }
        }
    }
    entries
}

fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

fn path_env(name: &str) -> Result<PathBuf, String> {
    env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| format!("{name} is not set"))
}
