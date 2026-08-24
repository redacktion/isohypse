# Isohypse

Strip away the IDE, the language server, the linter, the formatter, and what you're left with is what programming actually is: read the code, understand what it touches, edit it, prove the edit worked. Human tools don't enforce any connection between those steps, and they don't need to, because humans have eyes and memory beyond 132k tokens.

We've all watched a model one-shot a full CRUD app with a test suite, then somehow lose the ability to center a modal. It tries four times, patches the wrong line, matches the wrong string, corrupts its own context... then somehow decides to rewrite the entire file because that's apparently easier than changing six lines. What is funny here, is that the model knew what to do on the first try, it just doesn't have the tools to do it right the first time.

Where grep and sed are hammers, Isohypse is a scalpel.

So when a model reads a file through Isohypse, it tags the exact bytes seen with a hash. Ask what a function touches and it answers from a live index; who calls it, what a change would break. The model edits knowing what depends on it, so that when it goes to edit, the hash has to match what's on disk or the edit doesn't go through. Isohypse can only touch the lines that were actually read, and if something else changed the file in the meantime, Isohypse merges the edit into the current version instead of blindly overwriting it - the editing stops when the exact lines being changed are the ones that moved.

One static Rust binary, under 50MB on disk/100MB in memory, no runtime. Read below for some other cool things Isohypse does, or point an agent at this repo like I know you will, and ask it to summarize it.

Give your agent the tool it deserves, you've got tokens to burn on better things.

## Security

Isohypse assumes multiple agents may share a machine and treats each client as untrusted. Snapshots, caches, and sessions are encrypted with ChaCha20-Poly1305 by default, while filenames are stored as keyed hashes so the store does not expose repo names or source paths through its layout. Encryption is configurable per category during setup, and changing a category re-encrypts the data already stored there.

On macOS, local socket connections authenticate both ends using the peer uid and the code signature exposed through the kernel audit token. That keeps out other users and prevents an arbitrary same-uid process from impersonating an Isohypse client or server. Unsigned builds warn and fall back to uid-only authentication; builds compiled with `--features strict-signature` refuse the connection instead.

The global store is also bound to the machine that created it. A Secure Enclave key signs a manifest covering the key and settings files, which Isohypse verifies at startup. If the store is copied to another machine or those files are modified outside the expected path, Isohypse quarantines it, falls back to a per-repo store, and disables agent mode until it is re-enabled with the account password. The same password can gate daemon shutdown and settings changes so an agent cannot disable the service or weaken its storage policy.

Before setup, Isohypse stays inside a gitignored per-repo store and does not persist data from other repositories. Linux keeps the encrypted store and the rest of the runtime behavior, but does not have the Secure Enclave binding, macOS audit-token signature check, or password prompts; socket authentication there is uid-only, which isolates other users but not another process running under the same account.

## Quick start

From crates.io:

```bash
cargo install isohypse
isohypse setup       # one-time: storage, encryption, whether settings need your password
isohypse state up .  # index the current repo and start serving it
```

From source:

```bash
git clone https://github.com/redacktion/isohypse.git && cd isohypse
cargo build --release
cp target/release/isohypse ~/bin/isohypse.staged
codesign --force --options runtime -s isohypse-local ~/bin/isohypse.staged   # macOS only
mv -f ~/bin/isohypse.staged ~/bin/isohypse
```

On macOS, keep the staged copy and final rename instead of overwriting a running binary in place. Replacing the live inode breaks its signature and the kernel terminates running Isohypse processes; renaming a separately signed binary into place leaves the existing process tree intact until `isohypse state reload` hands over to the new build. `setup` creates the self-signed `isohypse-local` identity once in its own keychain, and that identity should remain stable across rebuilds because the socket authentication layer uses it to recognize other Isohypse processes. Do not use ad-hoc signing for builds that need to participate in that trust chain.

On Linux, omit the codesign step; the binary warns that signature authentication is unavailable and uses the uid check instead. The daemon is optional for a single reader or writer because the content-hash check still protects edits against stale reads; it becomes useful once multiple agents share a repository, where it serializes writes and coordinates sessions.

## Getting the agent to use it

Most agent harnesses already ship with generic read, grep, and edit tools, and models tend to fall back to those unless Isohypse is part of the instructions loaded on every turn. Put the rule in `AGENTS.md`, a system prompt, a project rule, or whatever persistent instruction mechanism the harness supports rather than reminding the model once at the start of a session.

A minimal version is enough:

```text
Run `isohypse context prompt` first to learn the op grammar. Then use isohypse
for reading, searching, and editing code, not the built-in file tools, not
grep, not sed.
- Read with `isohypse context read <file>`. It gives you exact lines and a tag.
- Find callers and blast radius with `isohypse context explore <symbol>`.
- Search with `isohypse context find <text>`.
- Edit with `isohypse mutate edit`, anchored by symbol or lines, citing the tag
  from your read. Never hand-write a patch, and never re-read a file you already read.
```

For work that involves more than one command, keep a session open and pipeline operations through it instead of paying the process and context overhead of starting the CLI for every step. Multi-file changes or edit-plus-test workflows should use multi-op, which applies the ordered operations as one transaction and can roll the entire set back when validation fails.

```text
Open one session with `isohypse session` and pipeline your commands over it
instead of running the CLI once per step.
When a task is several edits, or edits plus checks, do it as one `isohypse
multi-op`: put the steps in a single JSON transaction with a `validate` command
(a build or a test), so the whole thing applies atomically or reverts.
```

If the harness exposes pre-tool hooks, they can redirect or block its built-in read, grep, and edit calls so the model does not have to remember the rule itself. Once the model is actually using Isohypse, `isohypse context prompt` gives it the grammar it needs; the persistent instruction is mainly there to make Isohypse the default path instead of the harness-native tools.

## See it

```text
$ isohypse context read greet.py
[greet.py#3b90f2a]
1:def greet(name):
2:    msg = "Hello, " + name
3:    print(msg)
$ isohypse mutate edit <<'EOF'
{"op":"mutate.edit","path":"greet.py","tag":"3b90f2a","edits":[
  {"replace":{"symbol":"greet"},"with":"def greet(name):\n    print(f\"Hi, {name}\")"}
]}
EOF
updated [greet.py#a41c09d]
```

The read returns `#3b90f2a`, which records the exact file contents the agent saw. The edit cites that tag; if the file changed in the meantime, Isohypse either merges against the current version or stops when the lines being changed are the conflicting lines instead of overwriting newer work. A successful write returns the new tag, so another edit can chain from that state without re-reading the file.

## Verify or roll back

Every stored file version is addressed by its content hash, which makes rollback a write of a previously known version rather than a reverse patch. Multi-file edits are staged and committed together, and `--verify` can attach a build or test command to the transaction; if validation fails, every file touched by that transaction is restored.

Before the build command runs, Isohypse reports parser errors, unresolved references, and compiler diagnostics scoped to the changed lines, then returns a receipt with the files and hashes produced by the edit, the diagnostics it found, and the result of the requested build or test. Large logs are stored behind a handle and fetched only when needed instead of being dumped into the model's context by default.

## The index

Instead of running a language server for every language in the workspace, Isohypse maintains one in-memory source index and resolves relationships in layers, using the cheapest signal that can answer the query before falling through to more expensive ones.

Tree-sitter provides the structural layer: symbols, scopes, and call sites across every supported language. Stack-graphs then resolve imports, scopes, and names for Python, JavaScript, TypeScript, and Java using GitHub's stack-graph engine adapted to Isohypse's tree-sitter representation. The result is language-server-style caller and dependency resolution without keeping a language server resident for each language.

Semantic lookup covers the cases syntax and name binding cannot answer cleanly. Every symbol carries a model2vec embedding, using a transformer distilled into a lookup table rather than an attention model or inference service. Embeddings are stored as int8 vectors with a one-bit sign vector; queries first narrow candidates by Hamming distance and then rescore the shortlist, which makes natural-language searches such as "the function that resizes images" useful even when the query shares no identifier text with the implementation.

All supported languages share the same vector space, so semantic neighbors can cross language boundaries without a separate cross-language index. Common identifiers such as `new` and `get` are down-weighted according to their corpus frequency instead of a fixed stopword list. The index itself is only a few megabytes, typical queries run in under a millisecond, and a full rebuild from source takes about a second, so it can stay memory-only and be recreated whenever necessary. After a change, relationship resolution proceeds outward from the modified code so nearby callers and dependencies become available before the rest of the workspace finishes refreshing.

## Multi-agent

Multi-repo operation is split between a supervisor, one worker per repository, and a shared services process. Each worker owns its repository index and local socket, which contains indexing failures to that repo and lets the supervisor restart a failed worker independently. The shared process loads the embedding model once and serves it to every worker, so adding another repository adds index state without loading another model. Read-only cross-repo queries use `-w '*'` to fan out across live workers and merge their results.

Writes are serialized through a per-repo queue, preserving the same hash checks and verify-or-revert behavior when several agents are editing at once. Agents interact through sessions identified by a harness prefix such as `cdx`, `cld`, or `pi` plus a daemon-assigned id; short commands can open and close a session automatically, while long-lived agents can reuse one session for thousands of commands. Commands within a session can be pipelined, responses are tagged so they may arrive out of order, individual operations can be cancelled, and a dropped connection leaves the session available for about a minute so the client can reconnect with its token.

Workspace changes are delivered through subscriptions rather than polling. Clients can subscribe to file moves, index rebuilds, resolution progress, shutdown events, and other workspace activity; slow consumers lose the oldest queued events and receive a gap marker instead of blocking the daemon. Every command also receives a global sequence number, giving concurrent sessions one ordered workspace timeline. Traces are written both as a merged log keyed by sequence and session and as per-session histories, then compressed and archived when they age out of the active set rather than being deleted.

A separate read-only workspace stream exposes the same activity without mutation privileges. It can be consumed by a sentinel agent or another observer that needs to inspect what the rest of the workspace is doing without being able to modify code.

## Hot reload

`isohypse state reload` replaces the running binary without dropping active workspaces. The new build starts beside the old one, restores the repository and session state, and rebuilds its indexes while the existing process continues serving requests. Once the replacement is ready it takes ownership of the sockets, the old process drains in-flight work, and connected agents resume through their existing session tokens.

## Macros and requests

Repeated operation sequences can be saved as macros, kept private to the current session or promoted for reuse later. Agents can also file requests for capabilities that Isohypse does not currently expose; small requests can include a proposed patch that is still required to pass the build gate, while larger requests remain queued for review.

## The op grammar

Every operation is represented as one JSON object: `{"op":"<category.action>","arg":...}`. The category describes the effect: `context` reads, `mutate` writes, `verify` validates, while `state`, `macro`, and `session` cover daemon and store control, reusable command sequences, and the session protocol. Edits use structural or positional anchors such as `{"replace":{"symbol":"greet"}}`, `{"lines":[10,14]}`, or `{"block":42}` rather than hand-encoded patch strings, and every write must cite the tag for the content it was based on.

`multi-op` executes an ordered set of operations as one transaction and rejects mutations unless the transaction declares a `validate` step, which may be a command or an explicit `"none"`. Run `isohypse context prompt` for the compact reference; `isohypse.schema_v0.2.md` and `isohypse.schema_v0.2.json` contain the full specification.

## Getting started with an agent

From the repository the agent will work on, run `isohypse context prompt` to load the grammar and `isohypse state up .` to bring the workspace online. From there the same binary handles reads, dependency exploration, search, edits, validation, sessions, and state over stdin/stdout; persistent harness instructions are only needed to keep the agent from falling back to its built-in file tools.

## Commands

The CLI maps directly to the JSON op grammar as `isohypse <category> <action>`.

| Op | What it does |
| --- | --- |
| `context read [--outline] [--max-bytes N] <targets...>` | Exact bytes plus a hash; files whole or with inline ranges: `context read a.rs:10-40,90-120 b.rs`. Capped lines stay unseen. |
| `context explore <query...>` | Symbols with tagged source, tiered callers, blast radius, semantic and cross-language kin. Two symbol arguments return the shortest call path between them. |
| `context find [--name] [--any] <text...>` | Substring search over tracked source including config and text files. `--name` matches paths; `--any` treats each argument as its own pattern. |
| `context log [path]` | Recorded versions; with no path, the workspace changelog. |
| `context prompt` | Print the op-grammar reference. |
| `mutate edit` | Anchored edits from JSON on stdin: symbol, lines, or block anchors; replace, insert, delete, move, remove. |
| `mutate create <path>` | Author a new file from stdin. Fails if it already exists. |
| `mutate undo <path> [TAG]` | Restore the previous version, a specific `TAG`, or the last parse-valid with `--recover`. |
| `verify build` | Run the workspace's detected build (Cargo/npm/make/go, or a `.isohypse.build` override). |
| `verify check` | Validate a proposed edit end to end without writing. |
| `verify diagnose [path]` | Parser and reference problems scoped to what changed. |
| `state status` | Per-workspace state: index, watcher, queue, and session state. |
| `state get <tag>` / `state put` | Fetch stored content by tag / store stdin content and get a tag back. |
| `state up [root]` / `state down [root]` | Start the supervisor or add a repo worker / remove one. `--reindex` forces a rebuild; `--foreground` serves in this terminal. |
| `state reload` | Hand over to a freshly built binary without dropping any agent. |
| `state stop` | Stop the supervisor and every worker; password-gated while workspaces are live. |
| `macro save` / `macro run <name>` / `macro list` | Save a named step sequence, replay it later, list what is saved. |
| `session` | Open a live NDJSON session (`session.open`, `session.command`, ...). `session.request` and `session.requests` leave and list notes for other agents. |
| `multi-op [file]` | Ordered steps as one atomic transaction from JSON; any mutation requires `validate`. |
| `setup [--agent]` | Configure the store: global vs micro, lifecycle windows, per-category encryption, and the settings password. |

Read-only operations accept `-w SEL` to target a workspace or `-w '*'` to fan out across all live workspaces and merge the result. Without a workspace selector, an operation targets the repository containing the current directory. Add `--json` to any operation for the structured response instead of the human-readable form.

## Storage

Persistent state lives under `~/.isohypse` unless `ISOHYPSE_STORE` overrides the location. Snapshots, seen-line sets, and journals are keyed by content hash, so identical content deduplicates to one object and survives daemon restarts without rebuilding durable state. The symbol index is intentionally memory-only because it rebuilds from source in about a second. Active session traces remain plain logs for cheap append and inspection, then roll into compressed archives when they go cold; repositories that have not been used for a few days are reaped from the active set.

Store migrations happen at startup, so a newer Isohypse build upgrades an older on-disk format in place before it begins serving the store. The source indexer skips `.git`, `.isohypse`, `target`, and `node_modules` by default; add repository-specific exclusions to `.isohypseignore`, one directory per line.
