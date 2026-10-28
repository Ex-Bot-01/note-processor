# Plan: what is built, what is next, and the steps in between

Date: 2026-10-28 · Companion to `docs/roadmap-2026-10-27.md` (milestones) and `todo.md` (handoff).
This document answers a different question: **what does the machine do today, and in what order does the rest arrive?**

Legend used by every diagram below:

| style | meaning |
|---|---|
| solid, filled | shipped on `main`, tested, released |
| dashed | exists on branch `m12/embeddings`, not merged |
| grey | planned, no code yet |

---

## 1. The scan pipeline as it runs today

```mermaid
flowchart TD
    A["workspace root"] --> B["walk: deterministic, byte-exact<br/>symlinks never followed"]
    B --> C{"path readable and decodable?"}
    C -- "no" --> C1["record UnknownPath<br/>report it, keep its row"]
    C -- "yes" --> D["decode: strip BOM, lossy replace<br/>hash the RAW bytes"]
    D --> E["classify: Added / Modified / Moved / Removed<br/>size+mtime short-circuit"]
    E --> F["identity tally A4:<br/>st_dev + st_ino, one eligibility set"]
    F --> G{"known row matched?"}
    G -- "by key" --> H["re-parse if digest moved"]
    G -- "leftover" --> I["pair by file identity<br/>else claim by content"]
    I --> J["parse: title with provenance,<br/>headings, tags, items, links"]
    H --> J
    J --> K[("ws.sqlite registry")]
    K --> L["render INDEX.md + shards<br/>byte-stable, golden-pinned"]

    classDef shipped fill:#1f6f3f,color:#fff,stroke:#0d3d22
    class A,B,C,C1,D,E,G,H,I,J,K,L shipped
    classDef planned fill:#eee,stroke:#999,color:#333,stroke-dasharray: 4 3
    class F planned
```

**Nothing below `K` reads vectors yet.** That is a deliberate fence: it is why the golden bytes have not moved since M1 and why M2a can merge before the model question is answered.

---

## 2. Milestones: shipped, in flight, planned

```mermaid
flowchart LR
    subgraph DONE["Shipped — main, CI green Linux + Windows"]
        M1["M1 core catalog<br/>np scan + INDEX.md"]
        H1["Hardening line<br/>F13 F18 F19 F20 F21 F22 F23"]
        H2["Correctness residue<br/>F24 F17 F16 A4"]
        REL["Releases v0.1.0<br/>Windows Linux macOS"]
    end

    subgraph BRANCH["In flight — branch m12/embeddings"]
        A["M2a R1-R3a<br/>migration 7, chunker,<br/>provider seam"]
        B["M2a R3b<br/>chunk writes + staleness"]
        C["M2a R4<br/>model download + benchmark"]
    end

    subgraph NEXT["Planned — each needs a design reviewed first"]
        M2B["M2b clustering<br/>clusters, membership,<br/>review queue with evidence"]
        SRCH["np search / np related<br/>FTS5 + cosine scan"]
        M3["M3 daily files +<br/>item timelines"]
        M4["M4 daemon npd,<br/>watcher, loopback API"]
        M5["M5 web UI<br/>React Vite CodeMirror"]
        M6["M6 desktop shell<br/>Tauri client"]
    end

    A --> B --> C --> M2B --> SRCH --> M3 --> M4 --> M5 --> M6
    H2 -.-> A
    REL -.-> M4

    classDef done fill:#1f6f3f,color:#fff,stroke:#0d3d22
    classDef wip fill:#b8860b,color:#fff,stroke:#7a5a08,stroke-dasharray: 4 3
    classDef todo fill:#eee,stroke:#999,color:#333
    class M1,H1,H2,REL done
    class A,B,C wip
    class M2B,SRCH,M3,M4,M5,M6 todo
```

---

## 3. What is left inside M2a, in order

Each box is one bounded round: fresh implementer, fresh scoped reviewer, fix loop for Critical/Important, whole-branch review at the end, merge only on your word.

```mermaid
flowchart TD
    R1["R1 migration 7 DONE<br/>models chunks embeddings,<br/>FK + typeof + length CHECKs pinned"]
    R2["R2 chunker DONE<br/>decoded-text domain, MAX_CHUNK_BYTES,<br/>seam and one-byte mutation tests"]
    R3["R3a provider seam DONE<br/>embeddings config as enum with<br/>deny_unknown_fields,<br/>NoEmbeddingProvider holds no client"]
    R3B["R3b NEXT<br/>chunk writes by per-row diff,<br/>vectors via ON CONFLICT DO UPDATE,<br/>EMBED_VERSION = coverage only"]
    R4["R4 models install + benchmark<br/>sha256-pinned download,<br/>EmbeddingGemma first,<br/>one-evening arithmetic applied"]
    R5["R5 whole-branch review<br/>then merge on your word"]

    R1 --> R2 --> R3 --> R3B --> R4 --> R5

    R3B -.-> N1["typeof vector string-bind probe,<br/>the deferred P2 batch"]
    R4 -.-> N2["endpoint candidate asked whether it<br/>serves /v1/embeddings at all;<br/>secrets only via NP_EMBED_* at runtime"]

    classDef done fill:#1f6f3f,color:#fff,stroke:#0d3d22
    classDef next fill:#b8860b,color:#fff,stroke:#7a5a08
    classDef todo fill:#eee,stroke:#999,color:#333
    class R1,R2,R3 done
    class R3B next
    class R4,R5 todo
```

**Why the order is not negotiable.** Writing vectors before `EMBED_VERSION` means splitting coverage from availability later, under time pressure — and that split (design §5) is the one place in this milestone where a wrong choice makes every scan re-chunk ~18k chunks forever.

---

## 4. Where each of your problems gets solved

| Your stated problem | Solved at | Status |
|---|---|---|
| "I cannot find things" — navigation | M1 catalog, links, tags | shipped today |
| "I cannot find things" — semantic recall | M2b + `np search` | R3b → R4 → M2b |
| "What is still open?" — item status | M1 items with markers; **timelines at M3** | partial: statuses stored, timelines not |
| Daily note with generated TODO segment | M3 + the endpoint | planned; needs your answer on `YYYY-MM-DD.md` vs `daily/` |
| Contradictions shown on both sides | M2b review queue | planned |
| Chat with `file:line` citations | after M2b search, before M5 UI | planned |
| Something I can click | M5 web UI, M6 desktop | planned; today it is a CLI plus `INDEX.md` |

---

## 5. Debts recorded rather than hidden

- **A4** merged on your word without its A/B/A benchmark or whole-branch review.
- **F18**: Windows file identity needs real hardware; CI proves only that the path is inert.
- **F25**: an over-wide token can render a physical line holding only `-`. Filed, unadjudicated.
- **Chunker deferrals**: no overlap between chunks yet, and fence-blindness — a fenced block containing a blank line can be split in half. Both in `docs/backlog-m2-followups.md`.
- **No FTS5 yet**: keyword search waits until there is summary text to index over, so the derived-data path is built once.
- Two rounds of M2a were pushed without running `cargo doc --deny warnings`; that gate caught a real break two rounds late and is back in the per-round list for good.
