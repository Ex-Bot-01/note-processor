# Decision memo — build M3, or keep `np` an indexer

Status: **for the human's ruling.** Written 2026-11-20 against `main` at `3df6e2d`. No code was written for this
memo and no existing file was changed. Every claim below names the file it came from; anything not measured here or
read in the repository is marked *unmeasured*.

## 1. The question

<!-- three sentences: envisioned / exists now / why this is a decision, not a backlog item -->

## 2. What is already paid for

<!-- catalog, chunking, embeddings, np serve, hybrid search — named files; plus the four-writes constraint quoted with its source -->

## 3. Three options

### Option A — stay a read-only indexer and push writing features into a client

<!-- deliverables / cost / risk (including what writing into someone's note folder costs np's trust) / what it unlocks for a front end like Jott -->

### Option B — build only the daily-file half of M3

<!-- deliverables / cost / risk / unlock -->

### Option C — build M3 as described

<!-- deliverables / cost / risk / unlock -->

## 4. The open sub-question: where daily files live

<!-- root YYYY-MM-DD.md vs daily/ ; collisions, sort order, catalog behaviour, what someone who already keeps daily/ gets; my pick; evidence that would change it; cite todo.md's "Open items" entry -->

## 5. What I would need to know to decide

<!-- at most five questions, each answerable in ten minutes -->
