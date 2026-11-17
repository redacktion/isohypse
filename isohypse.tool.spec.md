# Isohypse Tool Spec

Status: proposal for the **tool environment** only. Every decision here is made;
there are no open questions. Section 12 records verified versus assumed.

Companion: `isohypse.agent-runtime.spec.md`, which covers roles, obligations, and
coordination. Nothing from that document is enforceable by the daemon alone, and
nothing here depends on it.

---

## 1. Scope and the boundary test

**Tool environment** is everything required for Isohypse to be a correct, safe
read/edit substrate under load. It is enforceable with one agent, one workspace,
and no peers.

The test applied to every rule below:

> Would this rule still exist with exactly one agent, one task, one workspace, and
> nobody to talk to? Yes -> tool. No -> agent runtime.

Three layers, one job each:

| Layer | Owns |
| --- | --- |
| Daemon | mechanism and the ledger |
| Operator | policy |
| Harness | the turn |

This document specifies the daemon's mechanism and the ledger.

---

## 2. Socket placement and discovery

### 2.1 No socket lives in a workspace

**Decision.** Every socket is created under the user's isohypse home. A
workspace is content the user owns: isohypse operates *in* it, and writes no
socket, lock, or staging file *into* it. Nothing about a repository changes
because a daemon is running on it.

| Purpose | Today | Spec |
| --- | --- | --- |
| Workspace worker | `<ws>/.isohypse.sock` | `~/.isohypse/run/<workspace-key>/daemon.sock` |
| Session listener | `<ws>/.isohypse.session.sock` | `~/.isohypse/run/<workspace-key>/session.sock` |
| Handover staging | `<ws>/.isohypse.sock.new` | `~/.isohypse/run/<workspace-key>/daemon.sock.new` |
| Control | `~/.isohypse/supervisor.sock` | unchanged |
| Services | `~/.isohypse/services.sock` | unchanged |

The key is the existing opaque one: hex blake3 of the canonical root path, the
same value `enforce_path` already derives for `~/.isohypse/enforce/<key>.json`
and §3.5 uses for `~/.isohypse/index/<key>/`. It is not a new identifier and not
a new convention.

The **run directory name is the first 16 hex characters of that key**, not all
64. This is not cosmetic. `sun_path` holds 104 bytes on macOS and a full key is
64 characters, so `~/.isohypse/run/<64-hex>/daemon.sock` is 104 bytes before the
NUL terminator and fails to bind — the relocation would have traded a deep-path
failure for an always-on one. Sixteen characters is the truncation the machine
already uses: `~/.omo/codegraph/projects/isohypse-4cdce2050af712fa` is the same
key, cut the same way. If `run/<16>` is ever claimed by a different root, the
name escalates to 24, then 32 hex characters; a collision across 64 bits is not
worth more machinery than that.

Each run directory carries a symlink back to the workspace it serves:

```
~/.isohypse/run/
  4cdce2050af712fa/
    workspace -> /path/to/repo
    daemon.sock
    session.sock
```

The symlink is the readable index into the namespace. `ls -l ~/.isohypse/run`
answers "which workspaces have a worker, and where are they" without running any
isohypse command, which is the whole reason the key alone is not enough. It is
never dereferenced to resolve a workspace — §2.3 derives the key from the path —
so a stale link cannot misroute a client. It is written at worker start,
retargeted when the recorded root differs, and removed with the run directory.

It is also never written *through*. A symlink pointing at the workspace and used
as the socket's parent directory would put the socket back inside the workspace,
which is the thing this section exists to prevent. The link is a signpost; the
sockets sit beside it, under a directory isohypse owns.

A workspace receives nothing at all from a running daemon: no socket, no lock
file, no `.gitignore` line, no state directory. The one transient that can appear
is `.isohypse.tmp.<pid>.<n>.<name>`, the same-directory temp an atomic file
replace requires. It exists for the duration of one write and is removed on
success and on failure alike.

Two properties fall out of placing the run tree outside the workspace:

- **Path length.** A socket at `<ws>/.isohypse.sock` inherits the depth of the
  checkout, so a deep workspace can fail to bind at all. The longest path this
  section creates is the staging file, 62 bytes here, and it does not grow with
  the tree — fixed headroom regardless of where the workspace lives.
- **Read-only and shared mounts stop being special cases.** The old plan needed
  an escape hatch so it would not write into a read-only checkout. With the run
  tree outside the workspace there is nothing to escape from.

### 2.2 Relocation hook

`ISOHYPSE_RUN_DIR` overrides `~/.isohypse/run` as the parent of the
per-workspace run directories, mirroring how `ISOHYPSE_STORE` overrides the
store root. When set, sockets live at `$ISOHYPSE_RUN_DIR/<workspace-key>/daemon.sock`
and `session.sock`. It exists for containers with a read-only or absent `$HOME`,
for running two instances against one workspace in isolation, and for tests. It
is per instance, not per workspace: one value applies to every workspace the
instance serves.

### 2.3 Discovery

`find_socket` keeps its shape — walk ancestors from cwd — but derives a key per
level instead of looking for a socket file inside the ancestor:

1. Canonicalize `start` once. `/tmp/x` and `/private/tmp/x` are one workspace and
   must produce one key, so the hash is taken over the canonical string. If
   canonicalization fails, fall back to the absolute path as given.
2. Walk that canonical chain against `<run>/<run-name(ancestor)>/`, nearest
   first, and stop at the first that exists.
3. Return `<that>/daemon.sock`.

Cost is one `realpath` per call plus one hash and one `stat` per ancestor — the
same order as today — and no read of any ancestor's contents. A subdirectory
resolves to its workspace and a nested workspace to the nearest one, because the
walk starts at the caller's cwd. A registered key whose root has since been
deleted is treated as absent, not as an error.

### 2.4 Liveness is a connect probe, never a file check

**Decision.** Every liveness decision in the daemon and the harnesses is a
connect attempt, never `exists()` or `lstat().isSocket()`.

Today `harness/session.mjs` counts socket files and treats two present files as
"daemon is up". A SIGKILL leaves the file, the check passes, startup is skipped,
and the operator sees `session open timed out`. That is the same
busy-versus-broken conflation as the watchdog defect, one layer down, fixed the
same way: only a failed connect means dead.

With the run tree outside the workspace there are no files in the tree to count
anyway. A harness asks the binary: `isohypse state socket <dir>` prints the live
endpoint and exits non-zero when nothing answers, so no harness inspects the
filesystem itself. The probe is the check.

### 2.5 Sweep on supervisor start

The supervisor holds the instance lock and is therefore the unique legitimate
owner of `~/.isohypse/run/`. At startup, for each persisted workspace root it
removes:

1. every `*.new` staging file,
2. every `daemon.sock` and `session.sock` whose `connect()` returns
   `ECONNREFUSED`, and
3. any run directory whose `workspace` symlink is broken or whose root no longer
   exists, once (1) and (2) have run inside it.

It never removes a socket that answers, and never removes a run directory whose
root still exists. The run tree holds only sockets, staging files, and the
`workspace` symlink, so a sweep inside it cannot reach user data — the old plan
had to be written defensively because it shared a directory with `rust.scip` and
whatever else a repository happened to keep there.

### 2.6 Migration off the in-workspace paths

The new location was never populated, so there is nothing to convert: migration
is deletion, not rewriting.

Worker start unlinks the five names older builds wrote — `<ws>/.isohypse.sock`,
`<ws>/.isohypse.session.sock`, `<ws>/.isohypse.sock.new`, `<ws>/.isohypse.port`,
and `<ws>/.isohypse-lsp.sock` — logging one deprecation line per workspace. It is
a short retrying sweep, not a single attempt: during a handover the predecessor
still answers for a moment, and a socket that answers is never unlinked. The
sweep repeats on every worker start until the workspace is clean, so nothing
else has to run. `find_socket` checks the run directory first and falls back to
the legacy paths for one release, after which the fallback and the legacy unlink
are removed.

`isohypse setup` writes nothing to the workspace: it no longer appends
`.isohypse/` to a repository's `.gitignore`, and it no longer creates
`.isohypse/`. A workspace is left exactly as the user had it. External producers
of `<ws>/.isohypse/rust.scip` are outside this section and are not isohypse's to
manage.

---

## 3. Worker supervision, failover, and redundancy

### 3.1 The invariant

**At most one live worker per canonical workspace root.** Every replacement path
retires the incumbent before or as the replacement starts. Orphans are how 5 GB
became 15 GB.

### 3.2 Probe semantics

```rust
enum Probe { Answered, Busy, Unreachable }
```

| Result | Meaning | Counts as a miss |
| --- | --- | --- |
| `Answered` | replied within the timeout | no |
| `Busy` | connected, no reply yet | **no** |
| `Unreachable` | connect failed | yes |

A successful connect proves the accept loop is alive. A worker indexing a large
workspace cannot answer `state.status` within any useful timeout, and treating
that as death kills a working process whose cold re-index then misses again. That
loop is the entire cause of the observed thrash.

### 3.3 Constants

| Constant | Value | Why |
| --- | --- | --- |
| `WORKER_PROBE_INTERVAL` | 30 s | 8 workspaces x 1 probe is negligible load |
| `WORKER_PROBE_TIMEOUT` | 60 s | generous for a busy worker; irrelevant when connected |
| `WORKER_PROBE_MISSES` | 5 | ~2.5 min of nothing listening before action |

### 3.4 Replacement

On the fifth consecutive `Unreachable`: `kill_worker` (SIGKILL, map entry
removed) then `spawn_worker(root, handover = false)`. Nothing is listening, so
there is no session to preserve and no incumbent able to hand over. Handover is
used only where an incumbent is genuinely serving.

### 3.5 Restart cost and warm restart

A restart today is a full cold rebuild: `build_workspace` runs `full_index`,
spawns the precise-refs overlay, and rebuilds the semantic index. For intervert
that is minutes of CPU peaking near 7 GB.

**Decision.** Persist a warm artifact per workspace under the store:

```
~/.isohypse/index/<workspace-key>/
  symbols.bin      symbol rows, interning table, per-file content hashes
  semantic.bin     optional embeddings, f16
  meta.json        generation, format version, built-at, source digests
```

A restart loads `symbols.bin`, serves immediately, reports `ready`, and
re-validates files in the background against the watcher's change set. A cold
rebuild happens only when `meta.json` is absent, its format version is stale, or
its source digests mismatch beyond a threshold. `isohypse index --rebuild`
forces one.

The crash-loop guard must not fire on a legitimately slow first index.
**Decision:** a start that never reached `ready` is not counted as a crash; only
a start that reached `ready` and then exited counts. Without this, a 3-minute
cold index looks like a loop and the workspace is permanently abandoned.

### 3.6 Redundancy posture

Isohypse does not run redundant workers — two workers on one root is the orphan
bug. Redundancy lives one level up:

| Failure | Recovery |
| --- | --- |
| Worker process exits | supervisor waiter respawns |
| Worker unreachable | watchdog `Unreachable` x 5 -> replace |
| Supervisor exits | `state up` re-establishes; workspaces reload from the store |
| Supervisor wedged | password-gated `recover_supervisor` (existing) |
| Session transport lost | harness `session.reconnect` |
| Store corruption | `objects::migrate_store` on start (existing) |

---

## 4. Memory ceiling and degradation

### 4.1 Ceiling

Default per-worker soft ceiling: **2 GB**, overridable with
`ISOHYPSE_WORKER_MAX_RSS_MB`. Soft: crossing it degrades before it kills.

### 4.2 Degradation ladder

| Level | Trigger | Action |
| --- | --- | --- |
| 0 | under ceiling | none |
| 1 | >= 100% for 30 s | shed precise-refs overlay; state -> `degraded`; emit observation |
| 2 | >= 100% for 90 s | shed semantic index; keep symbols; emit observation |
| 3 | >= 150% for 60 s, or OOM-adjacent | restart with the warm artifact (§3.5) |

Shedding is reversible; both structures rebuild on next use. A worker that
reached `ready` and then restarts counts as a restart, not a crash (§3.5).

### 4.3 Why a ceiling rather than a bigger machine

The observed 6.9 GB is per workspace and eight workspaces are persisted. Without
a ceiling the failure mode is the machine swapping and the operator losing
everything, which is what happened. A ceiling converts one catastrophic failure
into one degraded workspace.

---

## 5. Indexing: memory and cost

Five changes, ordered by expected effect. §5.6 is the measurement that confirms
the order before work starts.

### 5.1 Stream the merge, stop materialising the tree

`full_index_cancelable` extracts every file into
`fresh: Vec<(String, String, Extraction)>` and merges afterwards, so peak memory
is the whole tree's extractions. **Change:** extract and merge per chunk of
`available_parallelism()` files, so peak is one chunk — `O(chunk x extraction)`
instead of `O(files x extraction)`.

### 5.2 Drop `extractions` after the store is built

`GraphIndex` retains `extractions: BTreeMap<String, Extraction>` alongside
`store`. Once symbols are in the store, the raw per-file extractions are
duplicated state. **Change:** retain only per-file content hashes (needed for
incremental re-extraction, §3.5) and drop the extractions. Expected to be the
single largest win; first thing §5.6 measures.

### 5.3 Intern strings

Symbol rows, paths, and identifiers repeat heavily and are owned `String`s.
**Change:** an interning table with `u32` ids for path and symbol name. Expect
roughly a halving of the symbol-store footprint, and a more compact persisted
format for §3.5.

### 5.4 Precise refs: lazy and size-aware

The stack-graph overlay is spawned eagerly for every workspace, time-boxed at
60 s, and abandons a partial index while holding large structures. Log evidence:
`graph 60.0s (1670 files), paths 60.1s, budget exhausted, partial index`, with
repeated `stitch panicked: ScopeStackUnsatisfied`.

**Changes:**

1. Build on first `context.explore` or `context.path`, never at startup.
2. Refuse to build above `ISOHYPSE_REFS_MAX_FILES` (default 2000), returning a
   structured `refs_unavailable` result naming the limit rather than burning 60 s.
3. Treat the stitch panic as a caught, reported condition: refs stay unavailable
   for that workspace until the next successful build.

### 5.5 Index on demand, unload when idle

Eight persisted workspaces hold eight full indexes whether or not anyone touches
them. `lifecycle::sweep` already demotes idle workspaces to cold storage.

**Change:** after `ISOHYPSE_INDEX_IDLE_RELEASE_S` (default 900) with no session
attached and no watcher event, release the in-memory symbol and semantic indexes
and reload from the warm artifact on next use. Fixed 8x cost becomes a function
of what is actually being worked on.

### 5.6 The measurement that decides the order

Instrument `build_workspace` for intervert and record RSS at each phase boundary:
after `full_index`, after the store build, after `extractions` is populated,
after refs spawn, after semantic build. Two numbers decide everything: peak RSS
per phase, and bytes held by `extractions` versus `store` at end of `full_index`.

If extractions dominate, §5.1–5.2 is most of the fix. §5.3 is not started before
this measurement, because the memory could be in the stack-graph structures
instead, in which case §5.4 is the fix.

### 5.7 Explicit non-goal

Compressing the stream is rejected. The model cannot read gzip; the lever is
bounded windows with explicit offsets (§8).

---

## 6. PANOPTCN: the workspace sentry

### 6.1 What it is

PANOPTCN is a per-workspace sentry that supervises **resource use**, not
coordination. It answers "is this workspace's tool usage healthy", never "is this
agent doing the right work". The second question has no mechanical criterion and
therefore does not belong to a daemon.

### 6.2 Placement

**Decision.** PANOPTCN is a component inside the worker process — not a separate
process, and not in the supervisor.

- It observes the op stream where ops are dispatched, with no IPC.
- Its lifetime follows the worker, which the supervisor already restarts.
- Its baseline is persisted (§6.7), so a restart does not relearn.
- It reports to the supervisor over the existing control socket so workspace
  health is visible without reading logs.

### 6.3 Two detectors, because both failure shapes occurred

| Detector | Catches | Basis |
| --- | --- | --- |
| Drift | slow trend, tunnel vision, starvation | rolling rate vs learned baseline |
| Loop | step functions | repeat ratio on normalized args + result fingerprint |

The thrash was a tight loop — an impulse, not a trend. The 6.9 GB worker was a
single cold index — one event, not a trend. A drift detector alone catches
neither. The loop detector needs no baseline and is therefore the one enabled by
default (§6.8).

### 6.4 Baseline

**Decision.** Baseline learning is gated on the worker reaching `ready`, never on
a wall clock. Fixed sample counts; windows of 60 s, 300 s, and 900 s. A workspace
that never reaches `ready` never establishes a baseline and the drift detector
stays disabled rather than learning from a cold index.

The first 60 seconds of a workspace's life is its most CPU- and memory-intensive
period. A baseline learned there skews high, making the sentry least sensitive
exactly when the workspace is riskiest.

### 6.5 Observation format

```jsonc
{
  "workspace": "/abs/path",
  "session": "cld.0002",
  "kind": "loop" | "drift" | "memory" | "silence" | "budget",
  "severity": "info" | "notice" | "warn" | "critical",
  "baseline": 20.5,
  "observed": 4.0,
  "window_s": 60,
  "evidence": { "op": "context.read", "repeats": 9, "fingerprint": "a1b2c3" },
  "at": 1789509985
}
```

### 6.6 Sinks are pluggable; PANOPTCN never decides to communicate

**Decision.** PANOPTCN emits observations to a sink chosen by the environment:

| Environment | Sink |
| --- | --- |
| Single-agent workspace | the session's nudge path (§6.8) |
| Role workspace | the orchestrator's mailbox (agent-runtime §7) |
| NGHBRS | the mesh, as an event |

This is what stops two systems independently deciding an agent needs correcting,
and it is the principled form of "turn it off under NGHBRS": the detector stays
on and the consumer changes. PANOPTCN owns what it observed; whatever owns the
environment owns what to do about it.

### 6.7 Persistence

Baselines persist at `~/.isohypse/panoptcn/<workspace-key>.json`, keyed by
workspace rather than session, so they survive worker restarts, session churn,
and reboots. The file stores per-window sample counts and the running estimate,
never raw op history — history is bounded by the ring buffer and dies with the
process.

### 6.8 Actions, in order

**Decision.** Silent enforcement precedes speech:

| Order | Action | Requires model cooperation |
| --- | --- | --- |
| 1 | shrink read windows; enforce an inter-call floor for loops | no |
| 2 | refuse the next identical call in a detected loop | no |
| 3 | nudge, carrying the observation and the reason | yes |
| 4 | escalate per the budget ladder (§7.4) | no |

Defaults at first release:

- **Loop detector: enforcing.** It is an exact match on normalized arguments
  plus result fingerprint, so a false positive costs one refused duplicate call.
- **Drift detector: observe-only.** Baselines are per-workspace and must earn
  trust before throttling anyone. Promotion is a config change
  (`ISOHYPSE_PANOPTICON_DRIFT=enforce`).

### 6.9 Nudge cap

A nudge that does not change behaviour within `ISOHYPSE_NUDGE_CAP` (default 3)
consecutive detections escalates automatically. A nudge without a cap is a
suggestion box, indistinguishable from the status quo this spec replaces.

### 6.10 Cost

No model on the path. No allocation beyond a fixed-size ring buffer and a
fingerprint map. Target steady-state footprint under 4 MB per workspace — which
is why a resident generative model is rejected: 0.5 GB at 4-bit against a 2 GB
worker ceiling spends a quarter of the budget on a probabilistic decision.

---

## 7. Dynamic budgets

### 7.1 Dimensions

| Dimension | Default posture |
| --- | --- |
| tool calls | soft; throttled |
| expensive calls (`multi-op`, validation shell) | hard cap; fails closed |
| retries after error | hard cap; fails closed |
| wall clock per working stretch | soft until a terminal declaration |
| bytes returned | soft; drives window sizing (§8) |

### 7.2 Reserve and settle

For ops whose cost is knowable in advance — a `multi-op` batch, a validation
shell — reserve before execution and settle after, with a TTL so a crashed
session does not hold budget permanently. This removes the case where a session
is admitted into work it cannot finish.

### 7.3 Graded policy

| Level | Trigger | Action |
| --- | --- | --- |
| `normal` | — | none |
| `throttled` | sustained call rate | inter-call floor via the existing `retry`/`backoff_ms` field |
| `constrained` | high repeat ratio, or budget pressure | shrink read windows; require outline-first on large reads |
| `refused` | budget exhausted | the existing hard refusal |

Reads fail soft with a bounded fallback. Mutations fail closed, consistent with
the existing `is_mutating_op` split.

### 7.4 Escalation

Three consecutive `constrained` events on one session within 300 s escalate to
`refused` for mutating and coordination ops only. Reads and declaration ops stay
available so a session can always recover or explain itself.

---

## 8. Bounded reads

### 8.1 The defect is triple clipping

Three layers clip, and only the first knows what was cut:

1. `maybe_handle` spills any response over 16 KiB to the object store and says
   `fetch` it — but `fetch` is in neither tool schema, so the handle is a dead end.
2. The native worker clips model-visible text at 24 KiB with "request a narrower
   range" and no resume point.
3. Pi clips at 50 KiB / 2000 lines and writes the remainder to a temp file.

The last clipper is the harness, which has no idea the daemon intended a
continuation, so the model receives a blob ending mid-file with no way forward.

### 8.2 One clipping layer

**Decision.** The daemon is the only clipper. The session declares its byte
budget at open (`session.open.budget_bytes`), the daemon produces text that
already fits, and the harnesses stop clipping. A harness receiving more than its
declared budget is a bug, asserted against in debug builds.

### 8.3 Fields, not a new op

`context.read` gains `offset` and `limit` in lines. `range` remains and is
mutually exclusive with them. No new op: op names are load-bearing in each
harness's tool enum, the observer allowlist, the readonly profile's `allowed_ops`,
and the grammar, and a paging concern does not justify that surface.

### 8.4 Continuation

Whenever a read does not finish, the response ends with a continuation carrying
both a plain range and an opaque cursor:

```
[showing 1-400 of 13989; next: range 401-800; cursor eyJwIjoic3JjL3gu...]
```

The range keeps it stateless and debuggable; the cursor stops the model doing
arithmetic wrong. The cursor encodes `(path, tag, next_line, total)` and is
validated against the file's current tag — a stale cursor is refused exactly as a
stale edit tag is, so chaining inherits the existing tag discipline rather than
inventing a second consistency story.

### 8.5 Chaining is multi-op plus a cursor

`[read page 1, read page 2, read page 3]` in one `multi-op` is the chain. No new
primitive.

### 8.6 Unbounded reads cannot be satisfied silently

A read that would exceed the cap returns the window it can afford plus the
continuation — never a truncated blob and never a refusal. An error would waste a
round trip to teach a lesson.

### 8.7 `fetch`

`fetch` is added to both tool schemas with `offset` and `length`, so the
object-store spill becomes real paging instead of a dead end.

### 8.8 Budget-aware windows

When a session is `constrained` (§7.3) the daemon shrinks the window and, for
large files, returns the outline plus a continuation instead of a body. With the
existing remaining-budget line, this pushes toward outline-first and targeted
spans without a rule anyone has to remember.

---

## 9. Wire protocol

No new transport; additions ride the existing NDJSON frames.

| Change | Detail |
| --- | --- |
| `session.open` | optional `budget_bytes`, optional `role` (validated; unknown role refuses the open) |
| `state.status` | adds `panoptcn` (sample counts, current level), `rss_bytes`, `index_phase`, `degraded` |
| nudge | unsolicited frame on the existing event-sink path, carrying the observation |
| throttle | unchanged: `Outbound::Error { retry, backoff_ms }` |

Unsolicited frames are additive; a client that does not understand them ignores
them, which is what the current adapters do.

---

## 10. Security and failure model

| Threat / failure | Response |
| --- | --- |
| Stale socket file | connect probe fails -> sweep removes -> restart (§2.4, §2.5) |
| Orphan worker | single-worker invariant (§3.1); replacement kills the incumbent |
| Two workers, one root | refused at spawn; the worker map is the authority |
| Supervisor wedged | password-gated recovery (existing) |
| Worker over ceiling | degrade, then restart with the warm artifact (§4) |
| PANOPTCN false positive | loop detector refuses one duplicate call; drift is observe-only (§6.8) |
| Baseline poisoning | baseline gated on `ready`; persisted per workspace (§6.4, §6.7) |
| Run dir left behind by a SIGKILL | connect probe fails -> sweep removes the socket, then the empty dir (§2.4, §2.5) |
| Workspace root deleted while a worker runs | sweep drops the broken `workspace` symlink and its run dir (§2.5) |
| Store corruption | `migrate_store` on start (existing) |

Default posture: reads keep working, mutations fail closed, and anything the tool
cannot adjudicate surfaces in `state.status` and the log rather than vanishing.

---

## 11. Observability

Every PANOPTCN decision must be reconstructible from a log without re-running it:
the baseline value, the threshold, the observation, the action, and the session
state at the time. If a refusal cannot be explained from the record, the design
has failed — this is what makes the deterministic claim checkable.

- `~/.isohypse/panoptcn/<workspace-key>.jsonl` — observations, actions, baseline
  updates. Bounded ring, rotated.
- `daemon.log` — worker lifecycle, probe results, degradation level changes,
  warm-artifact rebuilds, socket sweeps.

---

## 12. Status: verified, assumed

**Verified in this workspace:**

- `EnforcePolicy { allowed_ops, denied_ops }`, `builtin_profile("readonly")`,
  and `check_observer` operate on op names.
- `admit_context` charges a single scalar on the dispatch path.
- `Outbound::Error` carries `retry` and `backoff_ms`.
- `registry.attach_sink` gives the daemon a per-session push path.
- The watchdog busy-versus-dead defect, with the thrash log and the 5.1 GB /
  5.4 GB orphan measurements.
- Intervert worker at 6.9 GB and 100% CPU during a cold index.
- `GraphIndex` retains `extractions` alongside `store`; `full_index_cancelable`
  materialises all extractions before merging.

**Assumed, confirmed by §5.6 or by phase work:**

- Which of `extractions`, `store`, or the stack-graph structures dominates the
  6.9 GB. §5.1–5.2 are ordered first on expectation, not measurement.
- That the precise-refs stitch panic is containment-safe rather than indicative
  of deeper corruption.

---

## 13. Phasing

| Phase | Content | Depends on |
| --- | --- | --- |
| 1 | §5.6 measurement | — |
| 2 | §3 probe semantics and replacement (shipped) | — |
| 3 | §4 memory ceiling and degradation ladder | 2 |
| 4 | §3.5 warm artifact and restart cost | 1 |
| 5 | §2 out-of-workspace sockets, liveness probe, sweep | — |
| 6 | §8 bounded reads, single clipper, `fetch` | — |
| 7 | §6 PANOPTCN loop detector, enforcing | 2 |
| 8 | §5.1–5.3 indexing memory work | 1 |
| 9 | §6 PANOPTCN drift detector, observe-only | 7 |
| 10 | §7 dynamic budgets, reserve/settle | 7 |

Phase 1 is first because it decides the order of phase 8. Phase 2 is shipped.
Phase 7 is the first phase that protects the machine without operator action.
