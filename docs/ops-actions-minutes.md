# GitHub Actions minutes: what this repository spends and what stops it from running

Measured 2026-11-07 from the Actions API (`gh api …/actions/runs?created=>=…` then one `/jobs` call per run, so the numbers are
job execution times rather than run wall-clock). The repository is **private** (`visibility: PRIVATE`, checked the same day), and a
private repository's Actions minutes are metered against the account's monthly allowance — which is what this note is about.

## The finding, in one line

**CI did not break on 2026-11-07; the account ran out of included minutes.** Every job after that moment fails in 1–2 seconds with
**zero steps created**, because nothing was ever provisioned — and the cumulative billed minutes crossed the allowance in the same
second the last green job finished.

| Evidence | Value |
| --- | --- |
| Last successful job, any OS | `2026-11-07T01:57:05Z` |
| Cumulative November billed minutes at that instant | **2,136.3** |
| First zero-step failure created | `2026-11-07T01:57:12Z` — seven seconds later |
| Attempts made on the first affected run | 3 (`--failed` rerun, full rerun) — identical shape each time |
| SHAs affected since | `03ef6d8`, `e4bdba3`, `de0e78c` (all markdown-only), `d05a70a` (the F28 merge), `d6aa0e7` |
| Minutes consumed by a zero-step job | 0 — the outage is not itself spending anything |

The same signature appeared at the **end of October** and was then unexplained: F34 recorded that jobs "created zero steps from
2026-10-29T14:39 through 2026-10-31T20:23". October's total came to **2,028 billed minutes** — so both months stopped just past the
same number, and F34's window was this month's event one cycle earlier.

## What is being spent, November (through 2026-11-07)

Billed = Linux wall minutes × 1 + Windows wall minutes × 2 (private-repo multipliers).

| Job | Billed minutes | Share |
| --- | --- | --- |
| `Test (windows-latest)` | 1,167 | **54.6 %** |
| `Feature local-model (ubuntu-latest)` | 507 | 23.7 % |
| `Test (ubuntu-latest)` | 383 | 17.9 % |
| `Feature local-model,vision (ubuntu-latest)` | 79 | 3.7 % |
| **Total, 84 runs in ~6.4 days** | **2,137** | pace ≈ 334/day, **≈ 10,000/month** |

Two comparisons that explain the jump. October spent 2,028 billed minutes across **192 runs** — 10.6 billed minutes per run.
November spent 2,137 across **84 runs** — **25.4 per run**. The count of pushes went down and the cost per push went up ~2.4×,
which is what adding a second C++ feature job did: both `Feature local-model` rows configure cmake once and then compile the vendored
llama.cpp from scratch on every run, with no cache between runs, and Windows pays double for its own build steps.

Money, at private-repo overage rates ($0.008/Linux-minute, $0.016/Windows-minute): November so far ≈ **$17**, October ≈ $16, so the
current pace is roughly **$80/month**. Not a fortune; not zero either, and it buys nothing that a local gate does not also buy.

## What cannot be read from here

The billing endpoints are out of this token's reach: `users/Ex-Bot-01/settings/billing/actions` answers **403 Resource not accessible
by personal access token** (it needs a billing-capable token), and there is no repository-level Actions-usage endpoint to fall back on.
So the allowance itself is inferred from where two months stopped, not read from a meter — which is why this note quotes both months.
Job annotations are equally unreachable (`check-runs/<id>/annotations` → 403), so "runner could not be provisioned" is inferred from
zero steps plus a 1–2 second lifetime rather than quoted from GitHub.

## Options, with the arithmetic attached

Nothing below has been done. Changing CI shape trades evidence for money, and the account is the human's.

| Option | Effect on minutes | What it costs this repository |
| --- | --- | --- |
| **A. Wait for the cycle to reset** | 0 now; ~10,000/month again after | Every merge between now and then lands ungraded. The convention in this repository is that a card closes on a graded tree — F28 is open right now precisely because of that, so this is not free |
| **B. Raise the spending limit (or move to Pro)** | CI returns immediately | ~$80/month at today's pace; Pro's 3,000 minutes covers about nine days of it |
| **C. Run Windows and both feature jobs on `main` pushes and PRs only** | ≈ −75 % → roughly 2,500/month | Branch pushes stop being CI-graded. Local gates (17 rows) become the branch evidence, CI grades `main` after merge |
| **D. C plus cache the vendored llama.cpp build** | the two feature jobs drop from 63 % of spend toward their Rust half; plausibly ≈ 1,200–1,800/month, inside the allowance | Same as C, plus a cache key to maintain (the vendor tree's content hash) and a slow first run after any llama.cpp bump |
| **E. D plus ubuntu-only on every push** | `Test (ubuntu-latest)` alone is ~1,830/month at today's push rate — under the allowance with little headroom | Per-push signal survives; Windows and C++ grading happen at merge time |

The recommendation is **E**, because it keeps a per-push gate that catches the class of defect this repository has actually been bitten
by (a lint or a link that only exists on one platform, or a feature-gated item rustdoc sees and clippy does not), while moving the two
expensive-to-build rows to the moment where they decide whether `main` is graded. If money is not the constraint, **B** needs no change
here at all and everything keeps working as documented.

One thing none of the options changes: a zero-step failure is not a code defect, and treating it as one — by re-triggering until it goes
green — destroys the signal that made this repository's CI worth reading in the first place. The check to run before believing a red
main is whether the jobs **have steps**:

```sh
gh api repos/Ex-Bot-01/note-processor/actions/runs/<run-id>/jobs \
  --jq '.jobs[] | "\(.name): \(.conclusion) steps=\([.steps[]?] | length)"'
```

`steps=0` on every job means the account, not the tree.
