---
title: Tasks compatibility service and CLI
description: Explicit, durable task reports shared through one server endpoint.
---

Tasks remain server-owned shared facts, independent of panes, workspaces and
terminal scrollback. The service, persisted records, handoff drain, JSON API,
direct CLI and bounded adapter remain available for explicit compatibility use.
There is no automatic task execution or completion detection. Nothing in these
commands sends terminal input, prompts an agent or presses Enter.

The custom client's Files / Git / PDF / image / Tasks inspector and Kanban are
withdrawn, including all entry routes and background reads/previews. Fresh Pi
hosts register no Tasks tools or Tasks prompt injection, including in headless
mode. Fresh Claude SessionStart hooks do no task-adapter discovery and emit no
Tasks guidance. State/session/model reporters remain, with Pi 10 / Claude 12
asset versions unchanged. Existing hosts may retain old tools or guidance until
natural reload; this does not revoke explicit API/CLI access or delete data.

## Bound Tasks adapter (not MCP)

`herdr task adapter discover` and `herdr task adapter call METHOD` are one-shot,
noninteractive entry points retained for explicit compatibility calls.
Only these six existing methods are allowed; this is **not** an arbitrary API proxy
or an MCP transport. Discovery returns narrowed JSON schemas plus read-only
`source_context` / one-summary `list` probe evidence (`availability: "probed"`).
It does not invent a ping-advertised method list or test writes. Unsupported domain
methods still fail explicitly when called.

The exact launch environment must contain `HERDR_BIN_PATH`, `HERDR_SOCKET_PATH`,
`HERDR_PANE_ID`, and `HERDR_WORKSPACE_ID`. The binary must be absolute and resolve
to the adapter executable itself. Pane/workspace must match the endpoint's resolved
source context. Missing/invalid/offline binding fails closed: no PATH executable,
default local store, another endpoint, or guessed pane is tried. These bindings
prevent accidental cross-endpoint use; they are **not RBAC against same-UID code**.

```sh
"$HERDR_BIN_PATH" task adapter discover
printf '%s' '{"scope":"current","limit":10}' | "$HERDR_BIN_PATH" task adapter call list
# For NEW work, capture context once and save the complete logical operation.
"$HERDR_BIN_PATH" task adapter discover > task-context.json
python3 -c 'import json; c=json.load(open("task-context.json")); print(json.dumps({"expected_context":c["context_fingerprint"],"title":"Fix parser","nonce":"issue-42-create"}))' > task-create.json
"$HERDR_BIN_PATH" task adapter call create < task-create.json
# Retain task-create.json unchanged on uncertainty; do not rerun discovery to rebase it.
```

PowerShell uses the same stdin JSON and fixed argument vector:

```powershell
'{"scope":"all","limit":10}' | & $env:HERDR_BIN_PATH task adapter call list
# For a NEW operation only; save this JSON and reuse it unchanged on uncertainty.
$context = & $env:HERDR_BIN_PATH task adapter discover | ConvertFrom-Json
@{expected_context=$context.context_fingerprint; title='Fix parser'; nonce='issue-42-create'} | ConvertTo-Json -Compress | Set-Content task-create.json
Get-Content -Raw task-create.json | & $env:HERDR_BIN_PATH task adapter call create
```

| Adapter method | JSON fields (unknown/reserved fields are rejected) |
| --- | --- |
| `source_context` | `{}`; pane is always launch-bound |
| `list` | required `scope: "current"` or `"all"`; optional `after`, `status`, `limit` (1–50) |
| `get` | required `task_id` |
| `create` | required caller-retained `expected_context`, `title`, caller-retained `nonce`; optional `description`, `status`; workspace defaults to current |
| `update` | required caller-retained `expected_context`, `task_id`, positive `expected_revision`, and a change: `title`, `description`, `status`, `reported_verification`, `verification_evidence` |
| `append_conclusion` | required caller-retained `expected_context`, `task_id`, positive `expected_revision`, caller-retained `nonce`, `text` |

Discovery and adapter `source_context` return top-level `context_fingerprint`
envelope metadata, without changing the backend response type. Copy it into
`expected_context` on **all three mutations**. The strict format is
`hctx1:<64 lowercase hex>`. It hashes the JSON array
`["herdr:tasks-adapter-context",1,exact_launch_socket,store_id,boot_id,pane_id,workspace_id]`
with SHA-256. Version 1 covers adapter binding, enrichment and default semantics;
future semantic changes require a version bump. The tuple comes from validated
launch/server context; volatile agent labels are excluded. A hash is forgeable by
same-UID code: this is a retention precondition, not authenticated identity,
authorization or caller-selected provenance.

Presence/type/format, all remaining arguments and reserved keys are validated
before any network access. The adapter removes `expected_context` before backend
serialization, compares it with freshly observed trusted context, and constructs
provenance from that **same captured context**, never a later re-observation.
Valid but mismatching guards return `stale_context` with zero mutation requests.
No wrapper automatically reads/substitutes a new guard on mutations.

Models cannot supply binary/socket/endpoint/pane/provenance/boot/workspace fields.
Create/append use server-resolved source metadata; update cannot move workspaces or
change provenance. Task text travels as JSON stdin, never shell interpolation.
The adapter bounds stdin to 16 KiB, response frames to 256 KiB, and the entire
operation (including input/probes) to three seconds, with one worker per one-shot
process. Titles are limited to 256 UTF-8 bytes, description/evidence to 8192,
conclusions to 4096; control/bidi characters are rejected (newline/tab remain valid
for multiline text). IDs/types/store/source context are correlated before success.
Stdout is exactly one machine JSON envelope, `{"ok":true,"result":...}` or
`{"ok":false,"error":{"code":...,"message":...}}`; failures exit nonzero.
Help/diagnostics are separate from normal machine output.

There are **no automatic retries**. Explicit CAS conflicts need caller review.
`task_commit_uncertain`, `commit_uncertain`, and `response_uncertain` preserve an
unknown write outcome, including malformed, oversized, mismatched or lost replies
after send. Retain `expected_context`, original nonce **and entire original
payload/revision** for that logical operation. Within the same boot and binding,
create/append replay retains the same effective backend payload and durable nonce.
After boot/store/binding change, identical caller JSON fails closed with
`stale_context` **whether the previous write committed or left no ledger entry**.
That response says only this attempt was not dispatched; it does **not** determine
the previous attempt's outcome or prove absence. Do not automatically replace the
guard, nonce, revision or payload, and do not fetch fresh context to silently rebase
an uncertain old operation. Reconcile saved IDs/current records with deliberate
human/caller judgment. A new context read is appropriate for genuinely new work.
Backend/raw CLI durable cross-restart nonce replay is unchanged; the adapter's
cross-restart policy is deliberately fail-closed, not transparent replay.

Source context is observation, not authenticated authorship. Tasks v1 has **no
atomic expected-store/boot write precondition**: read-only preflight does not close
the restart race between successful guard comparison and write. A restart can
still occur in that interval; this fingerprint is not an atomic backend fence. The
saved source observations do not authorize guessing a replacement pane when a retained report becomes stale.

### Withdrawn automatic agent entry

Automatic Tasks entry is withdrawn. `integration status` labels this separately
from the retained direct compatibility CLI. Disk assets and reporter activity do
not prove that previously loaded tools or guidance have disappeared. No current
session is reloaded automatically. Managed-asset ownership checks still fail
closed for foreign files; withdrawal is not an uninstall or permission to replace
custom assets.

## Agent workflow

Use a stable, explicit nonce for each logical create or conclusion. Keep that nonce
and the original arguments for retries, including after reconnect or restart:

```sh
herdr task create --title 'Fix parser edge case' --nonce issue-42-create --json
herdr task list --json
herdr task get task_ID --json
herdr task update task_ID --expected-revision 1 --status working --json
```

Task IDs and the store ID are server-allocated. Copy the actual `task_id` and
`revision` from the response, rather than assuming a pane number is a task ID.
Read the current revision before changing a task another reporter may have changed:

```sh
herdr task update task_ID --expected-revision 2 --status review --json
cat conclusion.txt | herdr task add-conclusion task_ID \
  --expected-revision 3 --nonce issue-42-conclusion --text-stdin --json
```

All task commands output the normal JSON success envelope on stdout, and API errors
on stderr with a nonzero exit code (as other Herdr JSON commands do). `--json` is
accepted explicitly; task output is always JSON. Every command has examples in
`--help`. Missing required flags fail immediately, without prompts or reading stdin.

For multiline descriptions use `--description-file PATH` or `--description-stdin`
on create/update. For conclusions use `--text-file PATH` or `--text-stdin`.
These are alternatives to `--description TEXT` / `--text TEXT`, not combined inputs.
File paths are read by the CLI; the server never accepts a task-store path from a
client. An explicit stdin flag reads until EOF within the documented text bound.

## Reported completion is not verified completion

The statuses are `todo`, `working`, `blocked`, `review`, and `done`.
Verification is an independent **reported** fact: `unknown`, `pass`, or `fail`.
Herdr does not execute or independently attest the reported check:

```sh
herdr task update task_ID --expected-revision 4 --status done \
  --reported-verification pass \
  --verification-evidence 'cargo test: 12 passed; reviewed diff' --json
```

Setting `done` alone leaves verification unchanged. Being idle, closing a pane,
or terminal output mentioning success never updates a task. Provenance flags
`--reporter`, `--agent`, `--source-boot`, `--source-pane`, and `--agent-session`
are self-reported display/context metadata, **not authenticated authorship or RBAC**.
A source pane requires its boot ID. The server qualifies it with its stable store
ID so colliding pane numbers on different endpoints are not the same source.
No live pane is required to create or retain a task or conclusion.

Conclusions are bounded immutable snapshots with stable conclusion IDs and recorded
times. They remain readable after scrollback loss, workspace closure or source
pane exit. A saved source does not promise an exact historical terminal-row jump;
the panel offers only explicit best-effort source focus. Treat saved text and labels as
untrusted reports, not commands or authority to open arbitrary local artifacts.

## Supplying optional source focus metadata

Inside a Herdr pane, obtain endpoint-qualified context with the advertised
`task.source_context` method, or its noninteractive CLI:

```sh
herdr task source --pane "${HERDR_PANE_ID:?Run inside the reporting Herdr pane}" --json
```

The response's `result.context` contains authoritative current `store_id` and
`boot_id`, the resolved existing `pane_id`, its `workspace_id`, and optional observed
`agent`. The server checks that the pane exists at the read, not at a later write or
focus. It supplies its own runtime boot ID and reads the same endpoint's task store
in a bounded background worker. It accepts neither a boot override nor a storage
path. This is navigation context, **not authenticated provider/agent authorship,
authorization, or agent-session proof**. There is deliberately no agent-session
flag to auto-populate.

This copyable example uses standalone Python 3's standard library to extract actual
metadata and response IDs; no `jq` or package installation is required. Use the same
endpoint/session selection for every command. Choose stable nonces once for this
logical report, and preserve the original payload for retries:

```sh
python3 - <<'PYTHON'
import json, os, subprocess

def task(*args):
    return json.loads(subprocess.check_output(["herdr", "task", *args, "--json"], text=True))["result"]

source = task("source", "--pane", os.environ["HERDR_PANE_ID"])["context"]
flags = ["--source-boot", source["boot_id"], "--source-pane", source["pane_id"]]
created = task("create", "--title", "Fix parser edge case", "--nonce", "issue-42-source-create",
               "--workspace", source["workspace_id"], *flags)
assert created["store_id"] == source["store_id"], "endpoint/store changed; reconcile before continuing"
record = created["task"]
# Read the authoritative current revision before a new logical append.
current = task("get", record["task_id"])
assert current["store_id"] == source["store_id"]
result = task("add-conclusion", record["task_id"], "--expected-revision", str(current["task"]["revision"]),
              "--nonce", "issue-42-source-conclusion", "--text", "Saved result for review", *flags)
print(json.dumps(result, indent=2))
PYTHON
```

For retrying an uncertain append, reuse its **original** revision, text, nonce and
source flags, not a newly read revision. A restart between context and write can
leave the saved reference stale; the report is retained but focus rejects it. For
a new logical report, read new context. Never guess a boot ID, infer it from a pane
number/socket/store ID, or substitute a replacement pane.

A reported `--agent` additionally must match the live snapshot identity for focus;
copying the optional observed label is not evidence of provider authorship. Omit it
unless useful. `--agent-session` reports remain readable but disable source focus
because the client cannot prove the exact session from its snapshot. Older servers
without `task.source_context`, unavailable Tasks storage, missing panes, or missing
Python are local workflow limitations: omit **both** source flags to save an
ordinary report without navigation, or extract the JSON with a parser already
available to you. Do not weaken the source guard or auto-trust an agent label.

## Retries and concurrent writers

- `task.create` and `task.append_conclusion` deduplicate durable nonces across
  restart. Nonces are endpoint-local and scoped to the operation (`create` or
  `append`). A replay returns `replayed: true`, the stable identity and the
  **current** task record; `conclusion_id` identifies the original append.
- Reusing a nonce with a different parsed payload returns `task_nonce_mismatch`.
  Do not change the expected revision on a replay. For a new logical append, use
  a new nonce and the newly read revision.
- Updates and conclusions require `expected_revision`. Only one competing writer
  at a revision can win. `task_revision_conflict` includes the current revision;
  get the task, reconcile your intended change and then retry deliberately.
- `task_busy` means bounded workers or the transaction lock are busy. Retry with
  the original nonce/payload. `task_quiescing` means the server is draining for a
  handoff; reconnect to that same endpoint before retrying.
- `task_commit_uncertain` means atomic replacement occurred but durability could
  not be confirmed. **Do not assume nothing changed.** For create/append, resolve
  with the same nonce and identical payload. For update, get the current revision
  and compare the reported fields before deciding on another CAS operation.

## Endpoint isolation and durability

A task board belongs to the server's stable configuration namespace **and actual
JSON API endpoint path**, not to a client label, boot ID, pane or workspace.
Distinct actual socket paths get separate boards even if they share the same
configuration directory. Existing parent directories are resolved with filesystem
semantics, so parent-directory symlink aliases share a board and `link/..` is never
collapsed lexically. The replaceable final socket inode is not used; a final
socket-name symlink alias is not merged. On Windows the IPC endpoint is a named
pipe, so its supplied name spelling is retained instead of applying filesystem
parent resolution; spelling aliases are not merged. Configuration/session namespaces use the
same semantic ancestor resolution. Unresolvable parents disable only Tasks. Named sessions are isolated. Restart and corresponding
live handoff retain the board. Use the same endpoint/session selection as other
Herdr CLI commands; choosing another endpoint intentionally selects another board.

Tasks live in a separate private version-1 bounded JSON store under the server
configuration/session namespace's `tasks-v1/` directory, keyed by endpoint identity.
They are not part of `session.json`, session snapshots, layout snapshots or handoff
manifests. There is no lifetime store lock: each background transaction acquires a
bounded cross-process lock, reloads authoritative disk state, validates CAS/nonces,
writes and syncs a private temporary file, atomically replaces the store and syncs
the directory before acknowledging. Handoff stops admission and drains pending
transactions before importing; a drain failure aborts handoff and rollback resumes
admission. Rollback does not roll back already acknowledged task data.

Corrupt, future-version, oversized or inaccessible storage disables only Tasks
with `task_unavailable`; ordinary terminal and JSON API use continue. The service
never replaces invalid storage with an empty board. On Windows, existing and new
leaf directories and opened/created files must have a same-process-account owner
and a recognized protected private DACL; unknown/permissive policies fail closed
without being rewritten. SYSTEM access is permitted, not other users/groups.
This is not protection against administrator takeover, the same user, previously
opened handles, or hostile ancestor replacement. The strict policy may reject
otherwise private restored ACLs outside the recognized family. Preserve the file and resolve
the error before retrying; do not delete it as a recovery shortcut.

## Bounds and pagination

The initial store is limited to 8 MiB, 256 tasks and 8,192 durable nonce records.
Task titles are at most 512 UTF-8 bytes, descriptions 8,192 bytes, evidence 4,096
bytes and each conclusion 4,096 bytes. There are at most 16 conclusions per task.
Workspace IDs/provenance fields are at most 256 bytes; workspace labels at most
512 bytes. A serialized task is at most 256 KiB. Capacity errors reject the whole
transaction; there is no destructive eviction or deletion command in this phase.

Lists return bounded summaries, not full descriptions/conclusions. The default page
is 50 records, maximum 100. Follow `next_after` with the same filters:

```sh
herdr task list --workspace ws_ID --limit 10 --json
herdr task list --workspace ws_ID --limit 10 --after task_LAST_ID --json
```

`next_after: null` marks the last page. `--status` filters the reported status.
Pagination is ordered by stable task ID, not a snapshot across concurrent writes;
refresh the list to observe newly inserted records. Workspace association is an
optional grouping attribute, not a live foreign key. Unfiltered lists include
closed-workspace tasks.

## JSON methods and compatibility

The advertised neutral endpoint methods are `task.create`, `task.list`, `task.get`,
`task.update`, `task.append_conclusion`, and the separately shaped
`task.source_context` (`{"pane_id":"..."}` → `task_source_context` response). They use the existing opaque JSON
endpoint lane; no generation-1 codec, binary envelope or published method shape is
changed. Discover support through the advertised method list, not a new capability
boolean or event subscription. Task responses are only sent in response to explicit
task requests. Older servers lacking these methods do not support Tasks, but remain
usable for ordinary terminal operations. No task events are emitted in phase 1.
