# Isohypse Agent Runtime Spec

Status: proposal for the **agent environment** — roles, obligations, and
coordination. Every decision here is made; there are no open questions. §15
resolves the questions left open by the earlier draft.

Companion: `isohypse.tool.spec.md`, which covers everything the daemon enforces
by itself. This document depends on that one. That one does not depend on this.

---

## 1. Scope and the boundary test

**Agent environment** is coordination: who does what, who decides, who gets
woken. It only exists because there is more than one participant.

The test:

> Would this rule still exist with exactly one agent, one task, one workspace, and
> nobody to talk to? Yes -> tool spec. No -> this document.

The critical consequence: **nothing here is enforceable by the daemon alone.**
The daemon supplies mechanism — op admission, a ledger, a push path. It cannot
make an agent adjudicate, cannot make an orchestrator delegate, and cannot make a
turn start. Those are operator policy applied by the daemon, plus a harness that
opens turns. Pretending otherwise is how this becomes a system nobody can debug.

Three layers, one job each:

| Layer | Owns |
| --- | --- |
| Daemon | mechanism and the ledger |
| Operator | policy (this document's role and obligation definitions) |
| Harness | the turn (this document's waking section) |

---

## 2. Roles

### 2.1 Assertion at connect

A role is asserted by the operator as part of the connection grant, on the same
prompt as read-only / ask-per-edit / full-write. It is an additional axis, not a
replacement.

```
/pisohypse connect all orchestrator
/pisohypse connect all worker
/pisohypse connect all archivist
```

### 2.2 Four constraints

1. **Closed vocabulary.** The role name must resolve to a role declared in the
   environment (§10). Free text is rejected.
2. **Operator-asserted.** The role cannot be set by a tool call, a signal,
   another session, or the model. Only the connect path sets it, and only the
   operator drives the connect path.
3. **Recorded.** Persisted with the session and re-verified on reconnect, the way
   the workspace binding already is.
4. **Not hierarchical.** Roles are capability sets, not ranks. An orchestrator is
   not above a worker; it holds different capabilities and different obligations.

### 2.3 Role shape

```jsonc
{
  "role": "orchestrator",
  "capabilities": {
    "allow_ops": ["context.*", "mutate.*", "session.signal", "session.expect", "session.obligations"],
    "deny_ops":  ["mutate.undo"],
    "path_scopes": { "write": ["**/*.md", "docs/**", "plans/**"] }
  },
  "obligations": [
    { "id": "reconcile", "every_s": 90, "grace_s": 15, "on_miss": "nudge_then_deny", "max_misses": 3 },
    { "id": "intent", "satisfied_by": "validate", "on_miss": "deny" }
  ],
  "adjudicates": ["worker", "archivist"]
}
```

Capability matching reuses the existing `EnforcePolicy` semantics: a prefix covers
a domain, and `deny_ops` wins over `allow_ops`.

### 2.4 The three core roles

**orchestrator** — adjudicates and records. Reads everything. Writes are
path-scoped to documentation and plan surfaces, never to code, so it cannot do
the work itself and must delegate. The write scope exists for provenance: an
orchestrator that cannot record what it decided is useless.

Obligations: `reconcile` (drain its mailbox within a bounded interval) and
`intent` (a validation gate on its own writes, asking whether the in-flight
workload still matches operator intent).

Note on honesty: the `intent` obligation enforces **that a check happened**, not
that the check was correct. That is a legible, enforceable contract; the
judgement itself lives in Intenference and Eforme (§11). Offloading the decision
to a daemon is viable only where the criterion is mechanical.

**worker** — executes. Full read/write inside its granted workspace and path
scope. Obligation: terminate a stretch of work in a declared terminal state
(§3), never silently.

**archivist** — records. Reads everything; writes restricted to documentation
paths. Obligation: fold completed work into the record within a bounded interval
after a `done`. Woken in batches, not per-`done` (§5.5).

### 2.5 Adding a role

A role is declared with three things: a capability set, an obligation set, and an
`adjudicates` list. A role that can enter `blocked` but adjudicates nothing and is
adjudicated by nothing is refused at declaration — a role that can block with
nowhere to escalate is a dead end, not a role.

The role registry is local to the workspace. NGHBRS carries peer identity between
workspaces; this registry is the authority inside one. They must not fork into two
notions of who a peer is (§9).

---

## 3. Session state

### 3.1 States

| State | Meaning | Set by |
| --- | --- | --- |
| `starting` | connected, grammar not yet loaded | daemon |
| `working` | actively issuing ops | daemon, on first op after `context.prompt` |
| `blocked{need}` | cannot proceed without a decision | session, explicitly |
| `waiting{until, on}` | progressing; nothing due until a deadline or signal | session, explicitly |
| `done{summary}` | work complete | session, explicitly |
| `degraded` | over a resource ceiling (§4 of the tool spec) | daemon |
| `closed` | session ended | either |

`blocked`, `waiting`, and `done` are the three terminal declarations. Their
purpose is that "the turn ended" stops being ambiguous.

### 3.2 Why three, not one

Requiring a turn to end only on a genuine blocker would spin: an agent with
nothing to do is not blocked. The implementable rule is that **a session may go
quiescent only by declaring one of the three.** Ending a turn without a
declaration is the thing the environment refuses to accept.

### 3.3 Transitions

```
starting -> working     any op after context.prompt
working  -> blocked     session.blocked{need}
working  -> waiting     session.wait{until, on}
working  -> done        session.done{summary}
blocked  -> working     adjudication, or session.signal{kind:"unblock"}
waiting  -> working     deadline reached, or matching signal arrives
working  -> degraded    daemon, resource ceiling
degraded -> working     after shedding completes
any      -> closed      session.close, disconnect, transport failure
```

Any other transition is refused, naming the current state. `blocked` and
`waiting` are the only states the sentinel may exit unilaterally. `done` is
terminal for that stretch and clears that session's obligations.

### 3.4 Op permissibility by state

| State | Reads | Mutates | Signals | Declarations |
| --- | --- | --- | --- | --- |
| `working` | yes | per role | yes | yes |
| `blocked` | yes | **no** | yes | yes |
| `waiting` | yes | **no** | yes | yes |
| `done` | yes | **no** | yes | yes |
| `degraded` | yes | per role | yes | yes |

This is what prevents the common failure where an agent declares it is waiting
and then keeps editing.

### 3.5 Visibility

**Decision.** `waiting` is visible to other sessions in the workspace, pull-only,
via `state.status`. It is not broadcast. A worker waiting on a peer is information
the orchestrator needs, and pull-only keeps it off everyone else's hot path.

---

## 4. Ops

Seven ops, all in the existing `session.` domain, so the category prefix
discipline holds and the observer and enforce allowlists extend naturally.

### 4.1 `session.blocked`

```json
{"op":"session.blocked","label":"awaiting schema decision","need":"which of the two layouts is canonical","to":"orchestrator"}
```

Required: `need`. Optional: `to` — a role; defaults to the role's adjudication
target. Sets state to `blocked`, enqueues a signal into the target's mailbox,
returns an acknowledgement (not a resolution).

### 4.2 `session.wait`

```json
{"op":"session.wait","label":"waiting on peer edit","on":"signal:unblock","until_s":120}
```

Required: at least one of `on` (a signal kind or a session id) and `until_s`.
`until_s` is bounded by the environment's `max_wait_s` so a session cannot park
forever to dodge its obligations. Sets state to `waiting`.

### 4.3 `session.done`

```json
{"op":"session.done","label":"kernel tag reviewed","summary":"compared 88a1a2c against live; restored nothing"}
```

Required: `summary`. Sets state to `done`, clears obligations for the stretch,
and notifies the archivist role if one is connected, subject to the coalescing
window (§5.5).

### 4.4 `session.signal`

```json
{"op":"session.signal","label":"unblock peer","to_role":"worker","kind":"unblock","payload":{"decision":"layout B"}}
```

Required: exactly one of `to_role` or `to_session`, plus `kind`. Optional:
`payload`, bounded. Delivery is at-least-once with dedup on `(from_session, kind,
seq)`. Receiver handling is idempotent.

### 4.5 `session.expect` (orchestrator-only)

```json
{"op":"session.expect","label":"declare expected load","for_session":"pi.0004","rate_per_min":[15,30],"duration_s":600,"tools":["context.read","mutate.edit"],"paths":["src/**"]}
```

Declares machine-checkable expectations that PANOPTCN patrols against. Required:
`for_session`, and at least one of `rate_per_min`, `duration_s`, `tools`, or
`paths`. Allowed only for roles with `session.expect` in `allow_ops`.

**This op accepts structured expectations only.** Prose is refused, because prose
cannot be evaluated by a daemon, and an unevaluatable expectation is worse than
none — it creates the appearance that something is being watched.

### 4.6 `session.obligations` / `session.mailbox`

Reads. Return the caller's own pending obligations (id, due-at, miss count) and
queued signals (from, kind, seq, payload). Permitted in every state, and they are
how a woken agent learns why it was woken.

### 4.7 Schema cost

Six new ops and one read require: a schema version bump, additions to each
harness's tool enum, the observer allowlist, the readonly profile's
`allowed_ops`, and the grammar. That cost is why the tool spec adds fields rather
than ops for paging — a concept has to be genuinely new to earn an op name.

---

## 5. Obligations

### 5.1 Shape

```jsonc
{ "id": "reconcile", "every_s": 90, "grace_s": 15, "satisfied_by": "signal|op",
  "on_miss": "nudge_then_deny", "max_misses": 3 }
```

### 5.2 Semantics

An obligation is satisfied by an op matching its `satisfied_by` predicate. The
clock runs only while the session is neither `done` nor in a state that makes
satisfaction physically impossible.

**Decision.** A session in `blocked` or `waiting` does not accrue misses against
obligations whose satisfaction requires being woken. Otherwise an agent is
punished for correctly waiting, which teaches it to never wait, which is the
behaviour this whole design exists to prevent.

### 5.3 Escalation ladder

| Miss | Action |
| --- | --- |
| 1 | `nudge` — inject a turn naming the obligation (§6) |
| 2 | `warn` — nudge plus a mailbox entry visible to the adjudicating role |
| 3 | `deny` — refuse all mutating and coordination ops |
| beyond `max_misses` | `escalate` — mark the session `degraded`, notify the adjudicator |

### 5.4 What `deny` blocks

**Decision.** `deny` blocks mutating and coordination ops. Reads and declaration
ops stay available, so a session can always recover or explain itself. Blocking
everything would trap a session in violation with no route back into compliance.

### 5.5 Batching

**Decision.** The archivist is woken on a coalescing window (default 60 s), not
per `done`. Per-`done` waking is deterministic and cheap for the daemon but
wasteful for the model's context, which is the scarce resource here.

---

## 6. Waking a session

### 6.1 The mechanism exists and is verified

Pi's `ExtensionAPI` exposes:

```ts
/**
 * Send a user message to the agent. Always triggers a turn.
 * When the agent is streaming, use deliverAs to specify how to queue the message.
 */
sendUserMessage(content, options?: { deliverAs?: "steer" | "followUp" }): void;

sendMessage(message, options?: { triggerTurn?: boolean; deliverAs?: "steer" | "followUp" | "nextTurn" }): void;
```

The daemon does not start turns. The harness does, and the extension sitting on
the session socket can do it on demand.

### 6.2 Chain

1. The extension registers `pi.on("turn_end", ...)` and `agent_end`.
2. It subscribes to the daemon's per-session event sink.
3. The sentinel emits a nudge; the frame arrives on the sink.
4. The extension calls `sendUserMessage(...)`; a turn starts with no human
   involvement.

### 6.3 Injected-turn hygiene

An injected message must be attributable and must not impersonate the operator.
It carries the obligation or signal id, the environment, and the reason, and is
stored as a custom message with its own `customType` so the transcript shows
unambiguously that the environment — not the user — started that turn. This
matters for audit, and for keeping the model's model of "who is asking" correct.

### 6.4 Harness matrix

| Harness | Turn start from an extension | Status |
| --- | --- | --- |
| Pi | `sendUserMessage`, `sendMessage({triggerTurn})` | verified present |
| Codex | turn-item emission only | gap; no turn-start path found |
| Claude Code | tool registration only | gap |

Pi-first is not a preference; it is the only harness where the loop closes today.

### 6.5 If a harness cannot be woken

The environment degrades to **declaration-only**: sessions still record `blocked`,
`waiting`, and `done`, obligations still accrue and still `deny`, but nothing is
ever woken. That is strictly better than today — an unattended session ends up
refused rather than silently looping — but it is not autonomy. The distinction is
recorded in `state.status` so the operator knows which mode they are in.

---

## 7. Mailbox and routing

Signals are **role-addressed by default, session-addressed by exception.** A
`blocked` from a worker routes to the role declared as its adjudication target,
not to a hard-coded session, so the environment survives the orchestrator
reconnecting as a different session.

Delivery is at-least-once with dedup on `(from_session, kind, seq)`. Ordering is
per-pair, not global. A mailbox is bounded; overflow drops the oldest
non-actionable entry and increments a counter, because an unbounded mailbox is a
memory leak with better manners.

Routing is deterministic: `role -> adjudication target -> sessions holding that
role`. If no session holds the target role, the obligation is recorded as
**unroutable** and escalates to the operator rather than being dropped. This is
the case where the environment has an orchestrator-shaped hole in it, and
pretending otherwise is how work stalls invisibly.

### 7.1 Adjudication depth

**Decision.** Depth limit of 1. If the adjudicator is itself `blocked`, its
`blocked` escalates directly to the operator. Unbounded escalation chains are the
classic failure of this pattern and are refused by construction.

---

## 8. Environment definition

### 8.1 Ownership

**Decision.** The source of truth is `.isohypse/environment.json` in the
workspace — version-controllable, reviewable, and diffable alongside the code it
governs. The store holds the resolved, validated copy plus the hash it was
resolved from, for audit and for detecting drift between a running environment
and its file.

Precedence: the workspace file wins. A mismatch between the file and the running
resolution is reported in `state.status` and the log; it never silently fails
back to the store copy.

### 8.2 Shape

```jsonc
{
  "version": 1,
  "roles": { "orchestrator": { }, "worker": { }, "archivist": { } },
  "defaults": { "max_wait_s": 300, "announce_window_s": 60 },
  "panoptcn": { "drift": "observe" }
}
```

An absent file means no environment: every session is roleless, no obligations
apply, and PANOPTCN runs in its tool-spec defaults. Isohypse must be fully usable
with no environment defined, or adoption becomes a cliff.

---

## 9. Boundary with NGHBRS

**Decision.** One decision-maker per concern. Isohypse's sentry owns resource
supervision; NGHBRS owns coordination supervision. When NGHBRS is present, the
sentry keeps its detectors and changes its sink: observations go to the mesh as
events instead of to the orchestrator's mailbox.

The sentry never decides to communicate; it decides what it observed, and the
environment decides who consumes it. That is what prevents two systems
independently deciding an agent needs correcting, and it is the principled form
of "turn the functionality off under NGHBRS": the mechanism stays, the consumer
changes, and there is exactly one authority at any moment.

Peer identity is NGHBRS's. Role assignment is Isohypse's. A peer traversing the
mesh does not carry a role with it; a role is asserted when a session connects
here.

---

## 10. Interface with Intenference and Eforme

The environment enforces **that** a check happened. It never claims to have made
the check.

| Concern | Owner |
| --- | --- |
| Does the in-flight workload match operator intent? | Intenference / Eforme |
| Is the produced work correct? | the model, reviewed by the orchestrator |
| Did an intent check happen? | this environment (the `intent` obligation) |
| Is the workspace being hammered? | PANOPTCN (tool spec §6) |
| Who is asked to adjudicate? | this document (§2, §7) |

The contract is narrow on purpose. An environment that claimed to enforce intent
would be lying, and an environment that enforced nothing would be the status quo.
Enforcing that the gate was passed is the honest, checkable middle.

---

## 11. Security and failure model

| Threat / failure | Response |
| --- | --- |
| Session claims an ungranted role | role is set only by the connect path; an unvalidated `role` on `session.open` refuses the open |
| Signal replay | dedup on `(from, kind, seq)`; idempotent handling |
| Clock skew vs deadlines | deadlines computed daemon-side from a monotonic clock; no session-supplied timestamps |
| Sentinel down | grants and op permissions persist; obligations suspend and are logged as suspended, never silently passed |
| No session holds the adjudicator role | `blocked` escalates as unroutable |
| Session refuses to declare a terminal state | obligation misses accrue; the `deny` rung stops its mutating and coordination ops |
| Adjudicator itself blocked | depth limit 1; escalates to the operator |
| Harness cannot be woken | declaration-only mode, reported in `state.status` |
| Environment file absent | roleless, obligation-free, fully usable |

Default posture: reads keep working, mutations fail closed, and anything the
environment cannot adjudicate surfaces to the operator rather than vanishing.

---

## 12. Observability

Every decision must be reconstructible from a log without re-running it:

- state transitions with timestamps and the op that caused them
- obligation misses, with the due-at and the miss count
- nudges, with the obligation or observation that triggered them
- refusals, with the rule and the triggering signal
- mailbox depths and every unroutable routing outcome
- environment resolution hash and any file-versus-store mismatch

If a refusal cannot be explained from the record, the design has failed. This is
the property that makes the deterministic claim checkable rather than
aspirational — and it is the same standard the tool spec holds PANOPTCN to.

---

## 13. Phasing

| Phase | Content | Depends on |
| --- | --- | --- |
| 1 | Session states and the four declaration ops, no waking | tool spec phase 2 |
| 2 | Obligation ledger and the escalation ladder | 1 |
| 3 | Waking via the Pi hook; `reconcile` end to end | 1, 2 |
| 4 | Roles as authority: connect assertion, capability enforcement | 1 |
| 5 | Mailbox routing and adjudication between roles | 1, 3, 4 |
| 6 | `session.expect` and PANOPTCN patrol against declared expectations | 5, tool spec phase 7 |
| 7 | NGHBRS sink: observations to the mesh | 5, 9 |

Phase 3 is the value inflection: the first point at which a turn happens without
the operator asking for it. Phases 1–3 are the whole answer to "stop re-prompting
the harness". Roles, mailboxes, and adjudication are phase 4 onward, and they only
matter once there are peers.

---

## 14. What this deliberately does not do

| Not doing | Why |
| --- | --- |
| Judging work quality | no mechanical criterion; belongs to Intenference/Eforme |
| Spawning agents | the harness owns agent lifecycle |
| Carrying peer identity across workspaces | NGHBRS owns that |
| Accepting prose expectations | unevaluatable, and worse than none |
| Chaining escalations without a depth limit | unbounded chains are the classic failure |
| Running a generative model in the decision path | probabilistic enforcement is the problem being solved |

---

## 15. Resolved questions

These were left open by the earlier draft. They are decided.

1. **Is `waiting` visible to peers?** Yes, pull-only via `state.status`. Not
   broadcast. A worker waiting on a peer is information the orchestrator needs.
2. **Who owns the environment definition?** `.isohypse/environment.json` in the
   workspace is the source of truth; the store keeps the resolved copy and its
   hash. The workspace file wins, and any mismatch is surfaced rather than
   silently resolved.
3. **Does `deny` block all non-read ops or only relevant ones?** All mutating and
   coordination ops. Reads and declarations stay open so a session can recover.
4. **Is `blocked` on the adjudicator's side itself blocking?** Yes, with a depth
   limit of 1 — it escalates straight to the operator.
5. **Is the archivist woken per `done` or batched?** Batched on a 60 s coalescing
   window. The model's context is the scarce resource, not the daemon's cycles.

Two further decisions were made while writing this document, recorded here so
they are not silently inherited:

6. **`session.expect` accepts structured expectations only.** Prose is refused.
7. **Absence of an environment file is a supported mode.** Roleless,
   obligation-free, fully usable — otherwise adoption is a cliff.

---

## 16. Status: verified, assumed

**Verified:**

- Pi exposes `sendUserMessage` (documented as "Always triggers a turn"),
  `sendMessage` with `triggerTurn`, and the `turn_end` / `agent_end` events.
- `EnforcePolicy { allowed_ops, denied_ops }`, `builtin_profile("readonly")`,
  and `check_observer` operate on op names, so role capabilities compose with the
  existing enforcement rather than replacing it.
- `registry.attach_sink` gives the daemon a per-session push path.
- A2A's `role` is a sender role, not a permission level; ACP merged into A2A in
  August 2025; AHP is agent-to-human handoff.

**Assumed, to be proven by phase 3:**

- That a nudge delivered at `turn_end` reliably starts a turn in a live Pi
  session. The API says it does; nobody has run it.
- That Codex has no turn-start path. Searched, not exhaustive.

**Not found, so not claimed:** prior art for role-contract enforcement with an
obligation-driven supervisor. Four of six searches on adjacent terms did not
return; "not found" is the honest ceiling, not proof of novelty.
