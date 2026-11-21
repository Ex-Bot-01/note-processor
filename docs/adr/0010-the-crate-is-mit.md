# ADR 0010 — The crate is MIT, and not dual-licensed

**Status:** accepted. **Asked:** what licence `np` itself ships under.
**Recorded here:** 2026-11-20, moved out of `Cargo.toml`'s comment during the comment diet. Ruled by the operator on
2026-11-08 (F47) — a licence is not an agent's call, which is why this file records who made it.

`LICENSE` at the repository root is the authority. The `license` field lives in `[workspace.package]` so that no member crate
can drift from it while still being publishable metadata: a per-member field that disagrees with the root is a bug nobody
notices until publication.

**Why MIT.** It matches what this program statically links — llama.cpp and ggml are MIT — so no obligation imposed here is
stricter than one already being satisfied, and a downstream user's obligation reduces to "keep the notices attached". See
`docs/licensing.md` for what ships in the binary.

**Why not `MIT OR Apache-2.0`.** Nothing in this workspace is dual-licensed by its authors. Offering a grant nobody made is
worse than offering nothing: it reads as permission and would have to be walked back from a release.

**Consequences.**

* A new dependency that is *only* copyleft fails `.github/scripts/check-deps.py` at the pull request, rather than quietly
  narrowing what a downstream user can do with `np`.
* Changing this ruling means re-reading `THIRD_PARTY_NOTICES.md`, `docs/licensing.md` and `deny.toml` together — the three
  places that assume it.
