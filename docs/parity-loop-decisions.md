# Parity-loop decision log

Decisions taken **autonomously** while working the `parity-gap` issues from
`gap-analysis.md`, recorded here rather than raised as questions.

Each entry says what was decided, why, and what would have to be true to
revisit it. Anything a reviewer would want to have been consulted about belongs
here — if it is not written down, it did not happen deliberately.

Entries are newest last, grouped by issue.

---

## #102 — `remind_me_recalibrate_candidates`

**The issue asked for `markdown` and `json` `response_format` variants. Not
implemented; matched the reference instead.**

The reference's `RecalibrateCandidatesInput` (`models.py:1350`) carries only
`limit` and always returns JSON. Its models are `extra="forbid"`, so a caller
sending `response_format` would be *rejected* by `remind_me` while being
accepted here — divergence in the direction that breaks drop-in
interoperability quietly rather than loudly.

Revisit if: the reference adds the field, or drop-in interoperability stops
being the point of this port.

---

## #104 — version in server status

**The issue asked for the version in *both* `remind_me_stats` and
`remind_me_server_status`. Implemented in `server_status` only.**

`pid.py:176` puts `version` into `get_server_status`'s payload; `admin.py:622`
prints it. `remind_me_stats` (`admin.py:408`) contains no version field at all
— zero occurrences of the word in the whole tool body.

**The issue's second criterion (peer versions from the `/health` probe) was
deferred to #114.** It surfaces through `remind_me_sync_status`, which does not
exist here yet; building half of it would leave a field nothing populates.

**The gap analysis's T10 row was corrected in place**, because it asserted the
"both tools" claim and is where the next run would re-derive it.

---

## #105 — the sensitive flag

**Implemented two fields the issue did not name.**

`MemoryUpdateInput.sensitive` (`models.py:382`) — without it a memory marked at
creation can never be unmarked, so the feature is half-usable. `Option<bool>`
rather than `bool` so an update that does not mention the flag cannot silently
clear it.

`SyncRecord.sensitive` — required to make the issue's *own* "round-trips
through the sync outbox payload" criterion true in both directions. #101
delivered the sending half; without the receiving half a peer stores the memory
unmarked and surfaces it in ordinary search.

**Deliberately did not expose `sensitive` on the public `Memory` read struct.**
The reference's memory payloads do carry it, so this is a real follow-up — but
it means adding a field to a widely-constructed public struct, and no
acceptance criterion needed it. Filed as a note here rather than done quietly.

**The semantic half of search is filtered in Rust, not by threading a parameter
through `semantic_search_scored`.** Threading it would change an existing public
signature, which this loop does not do unattended. The cost is one extra query
per search that excludes sensitive memories.

---

## #106 — per-call retrieval strategy

**Most of the issue's acceptance criteria were already satisfied before any
code was written, and were covered with characterisation tests rather than
reimplemented.**

The `RetrievalStrategy` enum, the three multiplier presets, the `Auto` router
and all three shape heuristics (`looks_keyword_shaped`, `looks_semantic_shaped`,
`looks_temporal_shaped`) already existed in `retrieval.rs`. `search_memories`
simply hardcoded `RetrievalStrategy::Auto` instead of reading the caller's
choice. The only missing piece was the field and one line threading it through.

Rewriting working, tuned retrieval logic to "implement" criteria it already met
would have risked changing search behaviour for no gain. What was missing was
*coverage*: none of it was reachable from the test suite. It is now.

**Weights are asserted as ratios against `Balanced`, never as absolute
numbers.** The presets are multipliers composed on top of
`RrfWeights::from_env`, deliberately, so a preset cannot resurrect a signal an
operator zeroed. Asserting absolutes would bake the default config into the
tests and fail for anyone who tuned theirs.

**Precedence between the env vars and the per-call value is documented on the
field** (the issue asked for it to be documented, not changed): the environment
sets the baseline, the strategy scales it, and `Balanced` is the identity
multiplier rather than a reset to built-in defaults.

---

## #107 — `/api/versions`

**Gap A6 was withdrawn: there is no target-only `/api` route.** The row came
from an extraction that grepped `"/api"` string literals and matched a doc
comment — `routes.rs`'s note that the vendored dashboard talks to
`window.location.origin + "/api"`. `ROUTES` contains 20 patterns, all of which
the reference also serves. The headline table's "21 (20 shared + 1
target-only)" is corrected to 20 shared, and the gap row is struck through
rather than deleted so the next run does not re-derive it.

**Added `version` to `/health` as well as `/api/versions`.** Not in the issue's
criteria, but the dashboard header cannot be satisfied without it: the
reference reads the *node's* version from `/health` and only the *hub's* from
`/api/versions`. Its stated reason is worth keeping — `/health` is
unauthenticated, so it still answers when the API key is wrong or missing,
which is exactly when you most want to know which build you are talking to.

**Re-copied `dashboard/App.jsx` verbatim from the reference rather than
patching in a version header.** The file is vendored under the same convention
as the generated `schema_*.sql` — regenerate, never patch. The copy was 114
lines behind.

The cost: the newer dashboard also calls `/api/analytics/trend`, which does not
exist here until #112. Accepted because every fetch in that component is
individually caught and a failure leaves the trend state at its empty default,
so the chart renders empty rather than the page breaking. Revisit if #112 slips
far enough that an empty panel becomes confusing.

**`probe_hub_version` was added to `sync` rather than making `sync::http`
public.** The API crate has no business making raw HTTP requests, and this
keeps the auth header, the timeout and the cache in one place. Probed live
rather than read from sync state, following the reference's reasoning: a
dashboard started standalone never runs a sync cycle, so anything populated by
that cycle would report `null` forever while looking like a working feature.

---

## #108 — saved and watched searches

**The issue's criterion "a watched search returns only new hits on the second
run" describes behaviour the reference does not have.** Its
`remind_me_run_saved_search` calls the search core and returns whatever comes
back, watched or not (`tools/saved_searches.py`); the unseen-only diff lives in
the background poller, which notifies rather than filtering a tool's output.

Implemented to match: running returns everything, polling diffs. Asking for a
saved search's results and silently getting a partial list because something
polled it earlier would be surprising and unfixable from the caller's side.

**The criterion "both `markdown` and `json` `response_format` variants" is the
same overstatement as #102's.** None of the four reference tools take a
`response_format`. Not added.

**`poll_saved_search` returns the new matches instead of dispatching
notifications.** The transport is the scheduler's half and lands with #117.
Returning them keeps the diff logic — which is the part the issue rightly
flags as most likely to be dropped — complete and testable before any transport
exists, rather than shipping `watch` as an inert stored flag.

**`MemorySearchInput::Default` is hand-written, not derived.** Deriving it gave
`limit: 0` and `token_budget: 0` — not a neutral starting point but a search
that structurally cannot return anything, which is exactly how it failed first
time. The hand-written impl matches what serde supplies for absent keys, so a
programmatically-built input and a minimal JSON one behave identically.

**Polling searches with `include_dormant: true`.** A watch that stopped
reporting a memory because it decayed below the vitality floor would look like
the memory had been deleted.

---

## #109 — edit history and revert

**The issue's scope warning listed seven mutation paths and asked for them to
be audited rather than assumed. The audit answer is one.** The reference writes
`memory_revisions` rows from its update path alone (`tools/crud.py`'s
`_apply_memory_field_update`) — reclassify, normalize, annotate, consolidate
and decompose record nothing.

Followed rather than "corrected", and the reasoning holds: a revision exists to
recover a value a human replaced. The other paths either add derived data
alongside the original or change recomputable classification metadata, and
recording them would bury the edits worth reverting under machine-generated
noise. Both halves are pinned by tests — updates record, reclassify does not —
because "we forgot to wire it up" and "the reference deliberately does not" are
indistinguishable from the outside.

**Reverting to a memory's current state reports `NoChange` rather than writing
a no-op revision.** The alternative is an outbox row that says nothing changed
and a history entry recording a non-edit.

**`TrackedChanges` compares stored representations, not parsed values.** Tags
and metadata are compared as their JSON strings, so a metadata
re-serialisation with reordered keys is not mistaken for an edit.

---

## #110 — contradiction candidates

**The fan-out cap is implemented as part of the tool, not deferred.** It landed
in the reference on 2026-08-02 (`935eb98`), after the gap analysis was first
written, and the T6 row was amended to carry it. Porting the pre-`935eb98` SQL
would have shipped a queue where a single broadly-mentioned entity contributes
74% of the rows — invisible on a small test vault, decisive on a real one.

**No apply tool**, matching the reference: a confirmed contradiction is fixed
with the existing `remind_me_update`, `remind_me_delete`, or an
`remind_me_add` carrying an explicit triple.

---

## #111 — vault digest

**Two of the reference's five digest sections are omitted, not stubbed.**
Upcoming/overdue reminders and sync status read from subsystems this crate
does not have yet (#116 and #114). They are modelled as `Option` and left out
of both the Markdown and the JSON.

A "Reminders: none" line on a build with no reminders subsystem says something
false — it reads as "you have nothing due" when the truth is "nothing here can
tell". Omitting is the honest shape, and filling them in is a one-line change
for whichever issue lands first. `status.rs`'s module docs make the same
argument about hollow stubs.

**The digest has no `include_sensitive`, matching the reference.** It is the
ambient, often-scheduled surface the flag exists to protect, with no per-call
caller intent to opt back in against. The exclusion covers the count as well
as the list — a total that included a sensitive memory would leak that
something is there.

---

## #112 — analytics snapshots and the trend route

**The trend route captures a snapshot on read.** The reference captures from
its scheduler's poll loop; this crate has no always-on loop yet, so a
scheduler-only capture would leave the chart permanently empty on exactly the
installs most likely to open it.

Safe only because capture is idempotent per calendar day — asserted by a route
test that hits the endpoint three times and checks the series still has one
point. Without that, the chart would measure page loads rather than the vault.
When a scheduler lands (#117), it can call the same function; the day check
makes both callers safe together.

**Comparison is by date, not timestamp.** A server restarted three times in a
day would otherwise produce three data points and the trend would read as a
spike that never happened.

**A malformed stored value degrades to an empty map rather than failing the
read.** One bad row should not blank a whole chart.

---

## #113 — `GET /count` on the peer server

**The issue's `approx=1`, `since=` and `by=origin_node` parameters belong to
the *hub's* `/count`, not the peer server's.** The reference's
`peer_server.py` takes no query parameters at all and always reports
`"approximate": false`, with a comment explaining why: a peer has no planner
estimates to offer. `?approx=1` is a Postgres `reltuples` read, and the hub is
a Postgres service — E1, deliberately out of scope.

Implemented the peer's endpoint faithfully: no parameters, `approximate`
present-and-false. The three parameters would arrive with the hub if the hub
were ever in scope.

**Counts deliberately do not filter `deleted_at`.** Both ends of a reconcile
have to count identically. The hub counts every row and reports tombstones
separately, so filtering here would make a healthy peer look permanently
behind by its own tombstone count.

**`version` was added to the peer server's `/health` too.** A reconcile
reports which build each side is running, and that is where the other side
reads it from.

---

## #114 — sync status and repair

**`sync_repair` resets only the pull cursors, never the `_at` liveness
columns.** Those record what actually happened; rewriting them to force a
re-pull would destroy the evidence you were reading when you decided a repair
was needed.

**Repairing a remote with no `sync_log` row reports that rather than
succeeding.** A remote never contacted has nothing to repair, and a false
success sends the caller waiting for a re-pull that is not coming.

**Direction and rate were deliberately separated after a real bug.** The first
implementation gated the drain verdict on elapsed time, and two calls a few
hundred microseconds apart elapse zero whole milliseconds — so a backlog that
had visibly not moved reported `Unknown`. The delta alone answers "is it
moving"; only the per-minute figure needs a clock.

---

## #115 — reconcile against hub and peer

**One classifier serves both remote kinds.** "Local greater than remote means
pushes are not landing" does not depend on which machine is on the other end,
and a second copy is how the two would eventually disagree about what drift
means. `reconcile_peer` and `reconcile_hub` are the same function with a
different base URL and `sync_log` row.

**`NodeAhead` is checked first and unconditionally**, ahead of any lag
reasoning. It is the only direction where records sit on one machine with
nothing coming to fix them, and mixed drift is exactly where reading the
numbers by eye goes wrong — a remote ahead by 38 somewhere is loud, and the 3
records at risk are quiet.

**An unreachable remote returns `Unavailable` rather than a verdict.** A
verdict computed against counts that could not be fetched would be a guess;
the reachability problem is the answer.

**The classifier is tested directly rather than through a live remote.** The
network half is one HTTP GET; driving a fake server through every verdict
would test the harness more than the judgment.

---

## #116 — reminders: set, clear, list

**The reference does not apply `remind_at` on the receiving side of sync, and
this crate does.** `sync.py`'s `_upsert_one` writes 24 columns and neither
`remind_at` nor `sensitive` is among them, even though its own outbox trigger
puts both in the payload — so the reference ships a payload carrying two fields
nobody reads back. The issue's acceptance criteria asked for a round-trip, and
the same call was already made for `sensitive` under #105. A reminder is a
property of the memory, not of the machine holding it: dropping it would make a
reminder set on your laptop invisible on your desktop while every other
property of that same memory arrived intact. Divergence from the reference,
deliberate, with an end-to-end test in `reminders_sync_test.rs`.

**One consequence of that is accepted rather than worked around.**
`reminder_deliveries` is local-only, so a reminder already delivered on one
node is still pending on another and will fire there too. Being told twice on
two machines beats being told on neither because the machine that fired it was
the one you weren't sitting at.

**A past `remind_at` is rejected, not stored.** Stored, it lands straight in
the overdue bucket — the one that means "nothing was running when this came
due" — so a typo would be indistinguishable from a genuine missed delivery.
The reference makes the same call, in a pydantic validator.

**Setting a reminder writes no revision.** In the reference this falls out of a
shared helper's column gate rather than being stated; here it is explicit. The
revision log exists to recover a value a human replaced, and a vault whose
history is half scheduling noise is harder to read back than one that only
records edits.

**Naive timestamps are read as UTC, not local.** Local would be friendlier to
type and worse to store: the same string would mean different instants on two
synced machines, and `remind_at` is compared as a *string* against a UTC `now`
in every window query.

**`Memory` gained `remind_at` and `sensitive`, so every memory-returning tool
now carries both.** `remind_at` is required by the listing. `sensitive` came
with it because the reference's `_fmt_memory_md` — which this crate now
mirrors for the markdown listing — marks sensitive memories with a lock, and a
renderer that cannot see the flag cannot show it. Wider than issue #116 asked
for, and toward the reference in both cases.

**The digest's reminder and sync sections are wired up in the same change.**
Both were modelled as `Option` and omitted while their subsystems did not
exist, with the module docs promising "a one-line change for whichever issue
lands first". #114 landed the sync half and #116 the reminder half, so both
now report real answers — including an explicit "no reminders set" for an
empty vault, which is now a true statement rather than the false one it would
have been before.

---

## #117 — the reminder scheduler and notification channels

The issue said to stop and ask which channels are in scope, because some need
a new crate. Asked, and answered "keep going", so the calls below were made
and are recorded here rather than deferred.

**The webhook channel ships; SMTP does not.** The reference has exactly two,
both gated on env-var presence. Webhook needs no new dependency — this crate
already has a hand-rolled `TcpStream` HTTP client in `sync::http`. SMTP is
free in Python (`smtplib`) and is not in Rust: it needs `lettre` plus a TLS
backend, and the workspace has no TLS dependency at all today. That is a
dependency decision, so it is left for its own issue rather than taken here.

**The webhook is `http://` only, and that is a sharper limitation here than
elsewhere.** `sync::http` and `embedder` both already say "put a reverse proxy
in front", and for a sync peer on your own network that is reasonable. The
webhook endpoints people actually configure — Slack, Discord, ntfy — are
public HTTPS, so this channel cannot reach them directly. Reaching them needs
a TLS-capable client, which is the same dependency decision as SMTP and is
flagged alongside it. What ships is honest and works against a local relay;
it is not yet the whole feature the reference has.

**The issue's "channel failure is logged and retried per the reference" does
not match the reference — it does not retry**, and that is followed here. Its
`poll_once` writes the `reminder_deliveries` row after calling the delivery
hook unconditionally, and `notify()` swallows every channel error, so a
webhook that is down means the reminder is logged, marked delivered, and never
attempted again. The reasoning holds up: the log line is the channel that
cannot fail, so the reminder *has* been delivered somewhere. Retrying would
mean re-logging the same reminder every 60 seconds for as long as the webhook
stayed down, turning one missed notification into an unbounded stream of
duplicates through the channel that was working. Seventh issue whose
acceptance criteria diverge from the reference; commented on the issue.

**"A reminder due while the process was down is delivered on next start" —
confirmed, not skipped.** The issue flagged this as needing checking against
the reference. Its due query is `remind_at <= now` with no lower bound, so a
late reminder fires once on the next pass. Skipping would lose precisely the
reminders a scheduler exists to catch.

**With no channel configured, a due reminder is still logged and still
recorded as delivered.** "Channels are opt-in" governs whether a *notification*
is attempted, not whether the reminder is handled — otherwise a vault with no
webhook would accumulate undelivered reminders forever and re-log every one of
them on every pass.

**The delivery row is written after the hook, not before.** A hook that panics
outright leaves the reminder pending for the next pass. Marked first, a crash
mid-delivery would silently consume it. `INSERT OR IGNORE` keeps the unique
index as the real exactly-once guarantee, so two racing pollers produce one
delivery rather than an error that strands every later reminder in the batch.

**The scheduler thread opens its own connection from a path.**
`rusqlite::Connection` is not `Sync`, so sharing the caller's would trade a
compile error for a runtime serialisation problem. Shutdown is a condvar
rather than a sleep, so stopping does not block for up to a full poll interval.

**The loop carries reminders only.** The reference's equivalent also
piggybacks scheduled digests, watched-search polling, revision compaction and
analytics snapshot capture/retention on the same thread, for stated reasons.
Those belong to already-merged issues (#108, #109, #111, #112) whose functions
exist here but currently run only when a tool is called; wiring them onto this
loop is a follow-up rather than part of "reminders 2/3".

**`sync::http` gained a default port and optional auth.** A webhook URL
usually has no explicit port, and `parse_url` rejected that outright; and a
user's webhook shares no secret with this node, where a bare
`Authorization: Bearer` is a malformed credential some endpoints reject
outright when they would have accepted no header at all. Both are additive —
`post_json`/`get` keep sending the bearer token exactly as before.

---

## #118 — the ICS calendar feed

**Revocation, as the issue asked me to confirm: the reference has none.**
Rotation is deleting the token file, which mints a fresh token on next
resolution and silently invalidates every existing subscription. There is no
revocation list, no second valid token, and no way to revoke one calendar's
access without re-pointing all of them. Mirrored exactly rather than improved
on — a bespoke revocation scheme here would be a security mechanism invented
mid-port, which is worse than a documented limitation.

**A wrong token gets a bare 404, not a 401.** A 401 confirms the route exists
and that a token was checked, which tells a prober they have the right shape
and need only the secret. Matching the reference. The rejected token is also
never logged: a wrong value is still a secret somebody typed, and logs outlive
rotation.

**The token is compared with `constant_time_eq`**, the same helper the API key
already uses. A long secret compared with `==` is guessable a byte at a time
through a timing oracle.

**There is deliberately no "disabled" opt-out**, unlike `REMIND_ME_API_KEY`.
The token *is* the URL path, so falling open would publish every reminder to
anyone who guessed the route. When the token file can be neither read nor
written, an ephemeral per-process token is generated — a feed nobody can
subscribe to beats a feed anybody can read.

**The route is exempt from the `Authorization` gate, and nothing else is.** A
calendar app's "subscribe by URL" polls from the provider's own servers on a
schedule the user does not control, with no way to attach a header. The
exemption is matched on an exact prefix and suffix and tested against the
near-misses that would inherit it.

**"Timezone handling correct for all-day and timed reminders" — the reference
has no all-day events.** Every VEVENT is a timed `DTSTART` in UTC with a `Z`
suffix; there is no `VALUE=DATE` handling anywhere in `ics_export.py`, and
`remind_at` is a timestamp rather than a date, so an all-day reminder is not
representable upstream either. Followed the reference. What *is* asserted is
that a non-UTC offset is converted rather than having its wall-clock digits
copied and a `Z` appended, which is the actual timezone bug available here.
Ninth issue whose acceptance criteria diverge from the reference.

**No iCalendar crate.** The read-only subset needed is one VCALENDAR of
VEVENTs, small enough to write directly and test exhaustively, which is the
call this workspace has made consistently. Folding and escaping each have
their own tests because both produce output that reads fine and fails in a
real client — and an unescaped comma corrupts every VEVENT *after* the one
containing it, not just its own.

**The feed calls `reminders::list_reminders` rather than repeating its SQL.**
The reference re-derives the window inline in `api.py`; here the feed and the
tool share one definition so a calendar cannot disagree with
`remind_me_list_reminders` about what is on it. Uncapped, because a subscriber
wants every reminder rather than the first page.

---

## #119 — `/metrics` and `/manifest.json`

**`/metrics` is off by default and 404s while disabled.** The issue's criteria
do not mention a gate; the reference has one (`REMIND_ME_METRICS_ENABLED`) and
it is implemented. A 404 rather than a 403 or an empty 200 is the point: "off"
should be indistinguishable from "this build does not have it", so a scrape
pointed at a metrics-disabled server fails loudly instead of silently
recording nothing forever.

**Unauthenticated, gated on the enable flag rather than a bearer token** —
matching the reference and this crate's own `/health` posture. Prometheus
scrape configs typically send no custom headers, so requiring one would mean
hand-rolling a static-bearer config for a single target. The tradeoff is
stated plainly in the handler doc rather than hand-waved: while enabled this
reveals usage patterns — which tools are called and how often, search volume,
memory and outbox counts — to anyone who can reach the port. It exposes no
memory *content*.

**Counters vs. gauges, kept as the reference draws the line.** Only genuine
events-over-time are tracked in module state (tool calls, durations, search
tiers, rate-limit rejections) because no query can reconstruct them after the
fact. Anything already answerable by a cheap point-in-time query — total
memories, outbox depth — is computed per scrape and passed in as a
`GaugeSpec`, so it cannot drift from the table it describes.

**Tool-call timing is wired into the MCP dispatch**, not left as a recorder
nothing calls. Without it the three tool families would emit their headers
forever and never a sample, which is valid exposition and a dead metric.
Recorded for failed calls too: a tool that errors still consumed time, and
counting only successes would make a wholly broken tool look like an unused
one.

**No Prometheus client crate.** The text exposition format is a few
`# HELP`/`# TYPE` lines and `name{labels} value` samples. The one thing a
client library would buy is safe concurrent counters, which a `Mutex` around
plain maps covers — the same call this workspace made for the webhook POST,
the HTTP client and the ICS document.

**The exposition is tested by parsing it, not by substring match.** A scrape
Prometheus rejects and one it accepts differ structurally — a `# TYPE` with no
family, a sample whose name has no header — and substring assertions pass on
both. The parser is deliberately strict, and every sample is asserted to have
both a `HELP` and a `TYPE`.

**Search tiers are zero-filled rather than omitted.** A server that has served
no searches still emits all three at zero; omitted, a dashboard query returns
no data and renders as a gap rather than a flat line.

**The manifest has no `icons` key**, matching the reference. There is no icon
asset in the repository, and pointing at one that does not exist is worse than
omitting it — a manifest without icons is valid and the OS falls back to a
glyph.

---

## #120 — named, scope-limited API keys

**Unknown scopes fail closed.** A scope this build does not recognise — hand
edited, or written by a future version — is treated as read-only rather than
read-write. The alternative grants write access on a typo, which is exactly
the failure the issue warns about: a key handed out on the understanding that
it is safe.

**A corrupt or unreadable store authorises nothing.** It is reported and
treated as empty, so a damaged file locks the door rather than opening it.

**The store is re-read on every request rather than cached.** A cached store
keeps honouring a key revoked seconds ago in another terminal, and "revocation
that takes effect at the next restart" is not revocation.

**Permissions are tightened before the rename, not after.** Between a
world-readable create and a later chmod there is a window in which every hash
is readable by any local account.

**401 and 403 are kept distinct.** 401 means "I do not know this credential";
403 means "I know it and it is not allowed to do that". Collapsing them sends
a read-key holder hunting for an auth problem they do not have. The reference
draws the same line.

**The flat `REMIND_ME_API_KEY` stays implicitly read-write.** It was
unscoped-and-full-access before this feature existed, so scoping it now would
silently break a deployment that already depends on it. It is also reserved by
name and not revocable through the store, because it is config-managed and was
never stored there.

**Scope enforcement is tested route by route, not on one representative.** The
failure mode is a single mutating handler reachable by a path the gate does
not cover, and a spot check sails straight past it.

**`verify` checks every key even after a match**, so the work does not depend
on a key's position in the file. Comparison is over fixed-length hex digests
via `constant_time_eq`.

---

## Correction: the tool-count claim at #118

The #118 release note and PR said tool coverage reached "61 of 61 — full
parity". That was wrong: it counted the target-only `remind_me_wiki_import`
toward the reference's 61. The real figure was **60 of 61**, with
`remind_me_api_key` outstanding — which is what #120 adds. Verified by diffing
the two tool-name sets rather than comparing totals, which is what should have
been done the first time; comparing counts cannot notice that one set has an
extra member and is missing a different one. Corrected in `RELEASE_NOTES.md`
and `gap-analysis.md` rather than quietly restated.

---

## #121 — rate limiting

**The limiter runs before authentication on both surfaces.** A limiter that
only engages after a valid credential leaves an unauthenticated flood entirely
unbounded — and that is the flood that matters on an endpoint reachable from
the internet through a tunnel. Placing it first is the whole point.

**`resolve_key` compares the presented secret in constant time**, even though
it runs before authentication and its result is only a bucket name. A
fast-path `==` here would leak the secret to a timing probe that never needed
to authenticate at all — the limiter would become the oracle the auth check
carefully avoids being.

**A rejected call does not extend the window that rejected it.** The stored
count is left untouched on refusal, so a client that backs off meets a clean
window. Counting rejections would push the reset further out on every retry
and a persistent client could never get back in.

**One shared limiter, not one per endpoint.** The limit is a property of the
caller: someone turned away by the webhook should not get a fresh allowance by
switching to the MCP endpoint on the same host.

**A correct secret shares one `auth:known` bucket; everyone else is bucketed
by address.** The legitimate integration is identified, so it is limited as one
client however many addresses it dials from, while every unauthenticated
caller is isolated and cannot exhaust anyone else's allowance. An unknown
address shares `ip:unknown` rather than bypassing the limit — an unidentifiable
caller is precisely the one not to exempt. An empty configured secret never
matches, or a deployment without one would collapse every caller into the
shared bucket and make the limit globally exhaustible by one stranger.

**On by default**, unlike metrics. Both guarded surfaces are internet-reachable
in a documented deployment mode, so the safe default is the protective one and
the opt-out is explicit.

**Single-process, stated rather than implied.** Counters are per process with
no shared store, so two nodes behind one tunnel each enforce the limit
separately. That matches the architecture's non-goals, but an operator who
assumed otherwise would under-provision — so the module says so first.

**Called synchronously from the async middleware.** Safe because the critical
section is a map update with no I/O and nothing awaited while the lock is
held, so it cannot block the executor longer than that takes. Stated in the
call site rather than left for a reader to worry about.

**The window is tested with an injected clock, never by sleeping.** A test that
sleeps a real 60 seconds gets deleted by the first person in a hurry, and one
that sleeps a shortened window is a race waiting to fail on a loaded CI box.
The boundary is pinned in both directions.

---

## #122 — tool profiles

**Tier membership is copied from the reference exactly**, including
`remind_me_server_status` sitting in `CORE` despite otherwise being an ops
tool. It is what reports which profile is active, and a profile you cannot
diagnose from inside a session is a trap.

**An unlisted tool defaults to the most restricted tier.** Anything outside
`CORE` and `MAINTENANCE` is treated as admin/ops, so a tool added later starts
hidden under a narrowed profile rather than smuggling itself onto a surface
someone deliberately trimmed. The default matters more than any individual
assignment, because the list will keep growing.

**An unknown profile falls back to `full`, not to an error and not to empty.**
Refusing to start over a misspelled optimisation would be worse than the
misspelling, and an empty surface would look like a broken server.

**Hidden means gone**: absent from `tools/list` *and* refused on `tools/call`.
Merely undocumented would be worse than having no profiles — a model that
guessed the name would still reach it, and the caller would never learn their
trimmed surface was porous. The refusal also names the way out, so someone who
trimmed too far is not left guessing why a documented tool vanished.

**Pruning happens once, after the whole surface is declared**, rather than by
guarding each entry. Per-entry guards would put the tier decision in 62 places
and let the next tool forget it.

**Prompt pruning is wired although it is a no-op today.** This crate offers no
maintenance prompts yet, so nothing is currently hidden by it — but a prompt
added later is hidden with the tier it drives, rather than sequencing the model
into calls that will be refused.

**Two tests exist because of a mistake made writing them.** The first version
of the hiding test named `remind_me_import`, which this crate does not
advertise — it advertises `remind_me_import_chat`. The test passed anyway,
because anything unlisted is hidden by default, so it was asserting nothing.
There is now a check that every tiered name *and* every ops name the tests
sample is actually advertised. A tier or a test naming a tool that does not
exist is invisible otherwise: not to the table, not to the server, and not to
a consistency check that only compares the two tiers against each other.

---

## #147, #148 — defects found by re-checking the reference

**The gap analysis pinned the reference at `935eb98`, and the reference moved.**
Re-fetching before declaring parity turned up five commits, two of which were
real fixes to defects this port shares. Neither was in any filed issue, because
neither existed when the analysis ran. Re-checking the pin is now part of
closing the loop, not part of opening it.

**#147 — the envelope branch is guarded narrowly on purpose.** It fires only
when `chat_messages` is absent, `message` is an object, and that object carries
`role`/`sender`. A broader test would capture any export that happens to use a
`message` field for something else and change its behaviour silently. There is
a test for exactly that shape falling through unchanged.

**Block filtering keeps typeless blocks that carry `text`.** Two opposite
mistakes are possible here and both are silent: importing `tool_use` payloads
as conversation, and dropping real conversation from older exports that omit
the `type` discriminator. Keeping `type == "text"` *or* (no type and a `text`
key) is the rule that avoids both.

**#148 — a superseded error is demoted, not deleted.** It moves to
`superseded_error` rather than being cleared, for the same reason `sync_repair`
resets only the cursors and leaves the contact timestamps: the record of what
actually happened is what you read when deciding whether to act, and destroying
it to make a status line green is the wrong trade.

**Supersession requires *every* remote to have moved.** rusty's `last_error` is
cycle-level — the first failure across all remotes — so the remote that
produced it cannot be identified from the message. The reference compares
per-remote because its status is per-remote. Requiring all of them here is the
conservative translation: it can leave a stale error reported, but it cannot
declare a still-stuck remote healthy.

**The check fails closed in every ambiguous case** — unparseable timestamp,
missing timestamp, empty `sync_log`, or a remote still at the epoch default.
"Never succeeded" is not "succeeded a long time ago", and failing open would
hide a real outage behind a formatting problem.

---

## #149 — Obsidian vault import

**The issue's own criteria were wrong about the link target, and were
corrected on the issue before any code was written.** I had written that
wikilinks are "recorded in `wiki_links`". The reference resolves each
`[[Note]]` to an **entity**, via the existing entity-upsert machinery, and
links the mentioning memory to it. `wiki_links` belongs to the separate LLM
Wiki layer and is untouched. Two other criteria were also wrong — a dangling
wikilink needs no handling (entity upsert creates what is missing), and
attachment skipping lives in the watcher layer, not the connector.

**No YAML crate.** Real vault frontmatter is overwhelmingly flat, and a full
YAML parser is a large dependency for that. The hand-rolled parser covers
`key: value`, `key: [a, b]`, and `key:` + indented `- item`, and **degrades the
whole block to "no fields" the moment it meets anything else** rather than
extracting half of it — a caller cannot tell which half it got, so partial is
worse than none. The delimiters are stripped either way, so the note's prose
always imports cleanly. Layering a YAML crate in behind the same call site
later is contained and additive.

**Chunking is not reimplemented.** The connector strips frontmatter and hands
the body to the existing per-section Markdown chunker. A second chunker is how
Obsidian notes and plain documents would eventually disagree about what a
section is.

**Mentions attach per chunk, not per note.** Only the wikilinks whose literal
`[[…]]` text landed in a given chunk. Chunking never rewrites text, so the
markup survives verbatim into whichever chunk it fell into. Smeared across the
note, every section would claim every mention.

**A note's tags merge with the caller's rather than replacing them.** The
caller asked for these memories to be tagged a certain way, and the note's own
tags are additional information, not a correction.

**`#2024/review` is a tag; `#123` is not.** Only *wholly* numeric tags are
dropped — the slash is stripped before the digit test so the test answers "is
this a bare number" rather than "does it contain one". My first test asserted
the opposite and was wrong: dropping `#2024/review` would lose the commonest
dated-note scheme there is.

**The heading-anchor limitation is stated, not hidden.** `[[Note#Heading]]`
resolves to an entity for `Note` as a whole. Resolving to a section would need
a section identity the entity graph does not have, and silently forking a
`Note#Heading` entity would be worse than the honest coarser answer.

---

## #150 — Readwise highlight import

**The issue I filed described an API client. The reference has none.** I wrote
criteria for an endpoint, an auth header, pagination, incremental
`updated__gt`, a token environment variable, and a stub server for tests. The
reference makes **no live call against Readwise whatsoever** — it is a file
import, like every connector in that family. The user exports once and hands
over the saved file, which keeps an access token out of this crate entirely.
Corrected on the issue before writing code; this is the third issue in this
loop whose criteria diverged materially from the reference.

**One memory per highlight, not per book.** A highlight is Readwise's own
atomic unit of meaning — nobody re-reads half a highlight the way they might
re-read half a document section. Grouping would make every search hit for one
highlight compete for ranking and embedding budget against every other
highlight in the same book, diluting the retrieval precision the store exists
to provide. The cost — losing the book as connective tissue — is paid back by
attaching its context as metadata to every highlight, demoting it from "shapes
the embedding" to "travels alongside it".

**Kept out of `auto` detection.** A Readwise export and a chat export are both
an unadorned `.json`. Content-sniffing works for chat *markdown* because role
markers are a strong signal; JSON offers no equivalent, and sniffing for a
`highlights`-shaped key would misroute a chat export that merely discusses
Readwise. Silently corrupting a working chat import is strictly worse than
requiring one explicit keyword.

**The `("json" | "jsonl", _) => Chat` rule had to be qualified.** It would
otherwise have swallowed an explicit Readwise request. The new arm matches on
the requested kind first — which `Auto` can never produce, preserving the
point above.

**A malformed top level is refused; malformed parts are skipped.** One bad row
must not cost the user the rest of a large export, but discovering partway
through that the file was never an export is the wrong time to find out — and
"imported successfully, zero memories" is the failure #147 had just fixed
elsewhere.

**My own test was wrong once here too**, in the same way as #149's: I asserted
a note-appending format by string-munging the expected value rather than
writing it out, which obscured what was actually being checked. Rewritten
literally.

---

## #151 — maintenance nudges

**The throttle slot is claimed before the counts run.** The natural ordering —
count the queues, then decide whether anything is worth saying — pays six
`COUNT(*)`s on the search hot path every single time, including on a vault
where nothing is pending. Claiming the timer first bounds how often the *work*
happens rather than how often a notice appears. There is a test asserting the
slot is claimed even when the notice comes back empty, because that is
precisely the case the obvious implementation gets wrong and nothing else would
catch.

**Timers are keyed rather than global.** Independent advisories have different
cadences; sharing one slot means whichever fires first silences the others for
a full interval.

**A queue whose query fails reports 0.** On a partially-migrated database a
missing table would otherwise make a status helper the thing that breaks a
search. Reporting 0 for the broken queue and honestly for the rest is the only
version of this that cannot make things worse than not having it.

**Counts go through each tool's own predicate.** `contradictions::candidate_count`
and `recalibrate::candidate_count` were added reusing the existing
`pairs_sql`/`candidate_where` rather than writing second queries, so the nudge
cannot point at a backlog that draining it does not find. A nudge that
disagrees with the tool it recommends trains the reader to ignore it.

**Only three backlogs are named, and ties break by key.** A list of every queue
is a wall of text nobody acts on. Unstable ordering between calls would make an
unchanged situation read as new information.

**`ever_captured` is a separate field from `captures > 0` being inferable.**
The whole point is that a client where auto-capture was never configured and
one where it was but nothing was worth capturing are both silent. Naming the
state is what makes it visible.

---

## #152 — automation event stream

**My issue's payload criterion was backwards, and correcting it mattered.** I
wrote "events carry enough to act on without a follow-up read". The reference
carries **metadata only** and never content, precisely so the stream cannot
become a content channel. Building what I had written would have put every
memory's text — sensitive ones included — onto whatever URL happened to be
configured, with no per-call intent to check against. The fourth issue this
loop whose criteria diverged materially from the reference.

There is a test asserting the payload has exactly four keys. A fifth field
added carelessly later is the realistic way content leaks in, and no
per-field assertion would catch it.

**Sync-applied writes emit nothing**, which answers my other bad criterion
("distinguishable, or not emitted"). The reference's emit sites are exactly
the local mutation paths; `_upsert_one` is not among them. Emitting there is
how two synced nodes echo each other forever.

**Not reusing the notification path, despite the identical transport.**
Notifications are human-facing and deliberately throttled to avoid alert
fatigue; automation events must never be throttled, because a suppressed
"repeat" is a dropped mutation a consumer needed. Sharing the plumbing would
hand automation the throttle and break it quietly.

**The detached thread's handle is held rather than dropped.** Fire-and-forget
is right for latency — a write must not wait on a webhook — but a handle
dropped immediately can lose the request mid-flight, which is silent. The
in-flight list reaps finished threads on each emit so it cannot grow unbounded.

**The delete event captures its category before the row is gone.** A hard
delete leaves nothing to read it back from, and an event that guessed would be
worse than one that did not carry it.

**Tested against a real socket, not the payload builder.** The no-content
guarantee is a property of what is actually POSTed; a builder-only test would
keep passing if the emit path started sending something else entirely.

---

## #153 — PDF import

**The dependency was proved to build here before a line of import logic was
written.** That is the discipline I committed to for #156 and it applies to
every optional-dependency issue: `pdf-extract` is pure Rust, 71 transitive
crates, ~19 seconds. Had it needed a C++ toolchain, the right answer would
have been to stop and say so rather than merge something that compiles on one
machine.

**Optional feature, not an unconditional dependency.** Most builds do not want
a PDF parser, and the reference reached the same conclusion with a lazily
imported extra. Feature-off behaviour is a **clear refusal naming the flag to
rebuild with** — "unsupported format" would send someone to inspect their file
when the problem is their build.

**CI now compiles and tests the feature-on configuration.** Default-off
features are invisible to a default CI run, so without this the parser could
rot untested and break only for whoever actually turned it on. Adding the step
is part of adding the feature, not a follow-up.

**A PDF with no extractable text is refused rather than imported as nothing.**
A scan parses cleanly and yields empty text on every page. "Imported
successfully, zero memories" is exactly the silent failure #147 had just fixed
for JSONL transcripts, and it would be a regression to reintroduce it one
format over. The message names the cause and points at OCR.

**`raw_bytes` threaded through `import_content`.** A PDF is binary and the
lossy UTF-8 decode text connectors receive has already destroyed it. Changing
that signature is wider than the issue asked for, and unavoidable: there is no
correct way to read a PDF from a mangled string.

**Extraction runs inside `catch_unwind`.** The parser panics rather than
erroring on some malformed inputs. A corrupt attachment must not take down the
process that was merely asked to read it — the same reasoning as the
maintenance counts swallowing their own failures.

**`auto` routes `.pdf`, unlike `readwise`.** A `.pdf` is unambiguous; there is
no second thing it could plausibly be, so the argument that kept Readwise out
of auto-detection does not apply here.

---

## #154 — cloud backup upload

**My issue omitted the security control the whole module exists around.** I
wrote criteria about endpoints, prefixes and credentials and said nothing about
the plaintext gate. When the DB encryption key is unset, the backup file is
plaintext personal data; the reference refuses to upload it without an explicit
opt-in, checked before any client is constructed. Shipping this without that
gate would have turned "enable cloud backup" into "silently start sending an
unencrypted copy of the entire vault to a third party". Fifth issue this loop
whose criteria diverged from the reference, and the highest-consequence one.

**I also had credentials backwards.** I wrote that they should come from "the
same environment variables the reference uses". There are none, deliberately:
the SDK's own credential chain is used, because a parallel secret-storage
convention is one more thing to get right with none of the existing hardening.
A test now pins that no configuration name here looks like a credential.

**The gate is a pure function of the environment.** That is what lets it be
tested with no bucket, no network and the optional feature compiled *out* —
which matters, because gating the test behind the feature flag would mean the
default build never exercises the control that protects the default build.

**Only a truthy opt-in counts.** `0`, `false`, `off`, blank and whitespace all
still refuse. This is the direction where being wrong ships someone's data, so
the permissive set is enumerated rather than inferred from "is the variable
present".

**The upload is reported, not logged and forgotten.** The reference logs and
swallows; `BackupOutcome.upload` carries the result instead, so a refusal
reaches whoever asked for the backup rather than only a log file they may never
read. It still cannot fail the backup — the outcome is a report, never an
error.

**CI clippy-checks the feature-on build rather than running its full tests.**
The SDK is a 233-crate, ~2-minute tree, and the decision that actually matters
is covered unconditionally by the default run. Recorded here because it is a
deliberate reduction in coverage, not an oversight.

---

## #155 — ANN index (part 1; reranker deferred)

**Split, as the issue anticipated.** `usearch` builds here (5 minutes, C++ from
source) and the ANN half is fully testable. The reranker needs an ONNX model
fetched at runtime and cannot be verified in CI at all, so blocking a working,
checkable improvement on an uncheckable one would have been the wrong trade.

**The index narrows; it does not rank.** Trusting ANN distances would make
vector scores subtly different from the brute-force ones and force every
downstream consumer — RRF fusion especially — to care which path ran.
Over-fetching candidates and then computing the exact dot product over that
small set keeps scores identical, and has the second benefit that a category
filter can be applied during exact scoring, which the index cannot express.

**Every failure falls back to brute force.** This is an optimisation over a
path that already returns correct answers, so no failure here may become an
error or a wrong answer. Missing, stale, unreadable, wrong dimension, empty
result: all scan instead.

**A short list falls back rather than being returned.** After a category
filter, the index's candidates may not fill a page. Returning fewer results
than a full scan would have is a retrieval regression nobody would notice,
which makes it worse than the extra scan.

**The key mapping is persisted, and this was a real bug caught before commit.**
The first version held position → rowid in a process-local static populated
during `build`. That would have made the index work only inside the process
that built it and fall back silently everywhere else — a feature that looks
enabled, passes its tests, and never actually runs in production. Exactly the
half-wiring pattern flagged on `sensitive` and `remind_at` earlier in this
loop. The mapping now lives in the sidecar next to the index.

**CI runs this feature's tests rather than only clippy**, unlike #154's
cloud-backup step. The equivalence check is the entire reason the feature is
safe to turn on; a build-only check would confirm it compiles while saying
nothing about whether it returns the right rows.

---

## #156 — image (OCR) and audio (transcription) import

**The issue's stated risk was wrong, and the probes said so before any code was
written.** It expected the native toolchain to block. Nothing blocked: `ocrs`
compiles in 51 seconds and `whisper-rs` in about a minute. What actually
constrains this item is the models, not the build.

**`ocrs`/`rten`, not an ONNX Runtime binding.** The reference picked RapidOCR
because ONNX Runtime was already present for its embedder. That reasoning does
not transfer: the Rust ONNX binding does not *carry* a runtime, it downloads
one at run time on first use. A feature whose central requirement is "never
download anything implicitly" cannot be built on a runtime whose install
strategy is an implicit download, however good it is otherwise. `ocrs` is pure
Rust and takes explicit model paths — the wanted contract rather than something
to work around.

**`symphonia` for decoding, for the reason the reference already established.**
whisper.cpp takes 16 kHz mono samples and decodes nothing. The reference
rejected `pywhispercpp` specifically because it shelled out to a system
`ffmpeg`; `symphonia` is pure Rust and handles all four containers in-process,
so the same rule is honoured rather than re-litigated.

**Resampling filters before it decimates.** Almost no recording is already
16 kHz, and dropping samples to get there aliases everything above 8 kHz down
into the speech band. The failure would present as poor transcription — which
reads as a bad model, not a bad resampler, and so would likely never be traced
back here. A windowed-sinc low-pass runs first.

**Model paths are configuration, and their absence is a loud error.** This is
the one place the port is less convenient than the reference: RapidOCR's models
ship inside its wheel, so the reference needs no configuration at all. Here
three env vars are required, and unset they produce a message naming them and
saying where the files come from. Quietly returning nothing would be
indistinguishable from an image that genuinely had no text in it.

**Deliberately stricter than the reference on downloads.** The reference
fetches a Whisper model from HuggingFace on first use. whisper.cpp takes a file
path instead, and that is the better contract: reading a voice memo should not
pull several hundred megabytes as a side effect.

**The scope really is narrower than the title, and the tests say which part.**
No real recognition or transcription is regression-tested, and cannot be
without a model. What *is* tested unconditionally: the feature-off refusals,
every model-configuration error, and the decode/resample arithmetic. That last
one was worth doing properly rather than writing off as part of the untestable
feature — it is deterministic maths over synthesised audio, including a test
that a 20 kHz tone is filtered out where naive decimation would fold it to
4 kHz.

**CI clippy-checks these rather than running their tests**, unlike #155. There
is no equivalence property here that only a test run can establish, and the
only thing a feature-on test run could add is a model download — the exact
thing the feature forbids.

**Two defects found by reading the reference, both adjacent rather than
central.** PDF imports were recording `source = "document_import"` where the
reference uses `"pdf_import"`; since `normalize` selects on `source IN
('document_import', 'chat_import')`, extracted PDF text was silently enrolled
in a rewriting pass the reference keeps it out of. And three tests used `.png`
as their "unsupported extension" example, so making images supported turned
them into OCR tests — the same trap `.pdf` sprang on #153, now sprung by the
fix for it.

---

## #155 part 2 — the reranker, and why the deferral was wrong

**The reason this was deferred did not survive contact with #156.** It was put
aside as "needs an ONNX model fetched at runtime, so it cannot be verified in
CI at all". That framing assumed the reference's own shape — download on first
use — was the only one available. #156 established the alternative: an explicit
model path, no download, and a refusal that CI can assert. Applied here, the
supposed blocker disappears.

**And the deferral undersold what was checkable by a wide margin.** The
reference's `rerank()` takes an **injectable scorer**. That means the entire
ordering contract — the head/tail split, tie stability, the recorded score, the
degenerate cases, the misbehaving-scorer cases — is testable with no model, no
feature and no runtime. That is precisely the part where a reranker silently
corrupts a result page, and it is now asserted unconditionally, 22 tests' worth.
The part that genuinely needs real weights is narrow: whether a real
cross-encoder orders *well*.

**Reranking may never break search.** Everything else follows from this. Search
already returns correct, useful answers without a reranker, so no failure here
may become an error or a lost result: missing feature, unconfigured model,
unloadable model, tokenizer mismatch, inference failure, wrong number of scores
returned — all return the incoming order untouched.

**The pipeline position was not what I would have guessed, and reading the
reference is what caught it.** It reranks a pool of `max(limit, top_k)` and
truncates *after*. Truncating first — which is what the existing rusty pipeline
did, and what I would have slotted the call behind — would have left the
cross-encoder able only to reorder results that were already going to be
returned, discarding the promotion-from-past-the-cutoff that is most of its
value. Feedback adjustment stays ahead of it, so it perturbs the order feeding
the cross-encoder rather than overriding it.

**On by default, which contradicted my own issue's acceptance criteria.** The
criteria said "both behind feature flags, both off by default". That is right
for the Cargo feature and wrong for the runtime setting: the reference's
`REMIND_ME_RERANK` defaults to `"onnx"`. Both are honoured here — the feature
is off by default, the setting is on — and they compose into the sane outcome,
because "on" without a configured model is a no-op rather than a download. This
is the seventh issue in this loop whose criteria diverged from the reference,
and the same cause every time: writing criteria from the gap table's one-line
summary instead of the source.

**`rerank_score` is deliberately not part of `score`.** Folding the logit into
the fused total would double-count the signal and make the diagnostic
`*_score` fields stop adding up. Reranking permutes; it does not contribute.

**CI runs this feature's tests, unlike `ocr`/`audio`.** There the only thing a
feature-on test run could have added was a model download. Here it adds two
real assertions — that enabling the feature does not make a search download
anything, and that a configured-but-unloadable model still degrades to RRF
order — for about 45 seconds of pure-Rust build.

---

## Process corrections made mid-loop

**A tool can be advertised without being routable, and only clippy notices.**
While working #110 the schema entry landed but the dispatch arm did not — the
string the insertion anchored on had been reformatted. Nothing failed except a
`unused imports` warning, which `-D warnings` turned into a build failure by
luck rather than design. The check is now explicit: cross-reference the
`"name":` entries against the `=> {` dispatch arms and assert the two sets
match. At the time of writing, 53 each, no difference.

**The local gate now runs `cargo test --workspace --no-fail-fast`.** `cargo
test` stops at the first failing binary, and the four pre-existing
`updater::tests` failures (a container git-signing artifact, not a code defect)
abort the run before any integration test executes. Every "clean except the
four known failures" reported on #125–#130 was therefore describing only the
lib binary. CI caught what the local gate could not — the `SyncRecord.sensitive`
deserialisation bug on #130.

**Build and lint checks assert exit status, not `grep '^error'`.** Cargo
colourises its output, so ANSI escapes precede the word `error` and the pattern
never matches. Three real compile errors were reported as clean before this was
noticed.

**CI state is read with `actions_get` / `get_workflow_job`, not
`pull_request_read` / `get_check_runs`.** The latter lags, reporting
`in_progress` for minutes after a job has finished.

---

## #177 — `remind_me_entity` made read-only

**Not an autonomous decision — the user chose this explicitly.** Recorded here
because it changes an existing public tool, which this log exists to make
visible.

The reference's `remind_me_entity` is a lookup: `readOnlyHint: True`,
`EntityLookupInput{name, limit}`, `extra="forbid"`, and `found=false` on an
unknown name. This crate's was an *upsert* taking `{name, kind}`. Same tool
name, opposite effect — a mistyped name returned `found=false` from
`remind_me` while silently creating a junk entity here, which is the kind of
divergence that surfaces as data drift rather than as an error.

Three options were put to the user; they picked (1), match the reference
exactly and move the write elsewhere.

**What changed:**

- `remind_me_entity` now takes `{name, limit}`, calls `entity::entity_profile`
  — already shared with `GET /api/entity`, so the dashboard and an LLM client
  see the same payload — and returns `{found: true, ...profile}` or
  `{found: false, query, message}`.
- The write moved to **`remind_me_entity_upsert`**, a target-only tool. The
  capability is kept, just no longer reachable by a call that meant to read.
- `remind_me_entity_upsert` is in the `core` profile. `remind_me_entity` was
  already there *and could write*, so leaving the upsert out would have cost a
  trimmed profile the ability to create an entity at all — a regression this
  change has no business making.

**`found` is spread alongside the profile's fields, not wrapped around them,**
matching the reference's `{"found": True, **profile}`. A caller written
against one must not have to unwrap the other.

**A miss is not `isError`.** An unknown name is a valid answer to a lookup;
flagging it would make clients retry a question that was already answered.

**The CLI's `entity` subcommand still upserts.** It calls
`entity::upsert_entity` directly rather than going through the tool, and the
reference's CLI has no `entity` subcommand at all, so there is no parity
constraint pulling either way. Left alone deliberately rather than
overlooked.

Revisit if: the reference adds a write path to `remind_me_entity`, or drops
`extra="forbid"` such that the two could converge on a superset instead.


## Watchdog stack dumps: take the system library, behind a feature (ADR-0014)

**Decision:** implement the reference's `faulthandler.dump_traceback_later`
guarantee — every thread's stack, including one wedged in CPU-bound code —
using `rstack-self`, behind an off-by-default, Linux-only `stack-dumps`
feature.

**The gap-analysis undersold the cost, and that is worth recording.** It listed
this as "a stack-dumping crate", on the same footing as ADR-0012's `libc`. It
is not: there is no pure-Rust way to dump another thread's stack, so the real
price is a *system* C library (`libunwind-ptrace`) plus permission to `ptrace`.
That breaks this crate's otherwise-consistent "no system binary" rule for
optional features — the rule that picked `symphonia` over ffmpeg bindings and
`rten` over the ONNX Runtime binding. Off-by-default is what keeps the
exception contained.

**Out-of-process, not a signal handler.** The cheaper option is to signal the
stuck thread and unwind it in the handler, as `pprof-rs` does. Rejected on the
reference's own stated rule, which this module inherits: *the watchdog must
never be the reason a tool call fails.* Capturing a backtrace is not
async-signal-safe — it allocates and takes loader locks — so a thread
interrupted inside one deadlocks permanently, in exactly the situation the
diagnostic exists to explain. A sampling profiler can wear those odds; a
one-shot diagnostic on a wedged server cannot.

**The hook is an interlock.** Capture re-executes `current_exe()`, so a binary
that enabled the feature but forgot `install_stack_dump_hook()` would spawn a
second server. Nothing spawns unless the hook has positively announced itself,
so forgetting it degrades rather than misfires. Pinned by a test that passes in
every test binary in the workspace, none of which can install the hook.

**`WatchdogStatus` deliberately unchanged.** It keeps the reference's exact
`{enabled, threshold_seconds, calls_in_flight}`. A `stack_dumps` field was
considered and rejected: `remind_me_server_status` is a parity surface and a
build-time fact is not worth diverging it for.

**Tested by wedging a real thread**, not by compiling the feature. The first
version of that test was flaky at ~1-in-5 because a debug build turns every
arithmetic helper into a call, so the sampled PC usually landed in `std` where
the unwinder could not recover the caller. The fixture now spins in float
arithmetic that compiles to inline instructions; measured 12/12, then 5/5 full
runs.

Revisit if: macOS becomes worth covering (a different mechanism entirely —
`task_for_pid` and code signing), or if a pure-Rust unwinder that is genuinely
async-signal-safe appears.

## Windows Job object for sidecars: take the FFI dependency (ADR-0013 amendment)

**Decision:** add `windows-sys`, target-gated to `cfg(windows)`, and assign
every sidecar to a job with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` — closing the
last behavioural divergence in `sidecars`.

This reverses the "no FFI" half of ADR-0013, which is why the reversal lives
there as a full amendment rather than only here. The short version: ADR-0013
deferred the work behind a trigger ("Windows becomes a first-class deployment
target, or an orphaned tunnel is observed") that was never going to fire on its
own, while this is a parity effort and the gap was a known divergence. Its own
cost estimate held exactly, so the only remaining argument for waiting was that
nobody had complained.

**ADR-0012 is not reopened.** That refused `libc` for a `kill(0)` probe that
has a pure-`std` alternative. This has none — there is no way to reach
`CreateJobObjectW` from `std`. The dependency is also
`[target.'cfg(windows)'.dependencies]`, so it is absent from the dependency
graph on the platforms this actually runs on today.

**Deliberate divergence — `SetInformationJobObject` failure.** The reference
warns, keeps the handle, and still assigns children to a job that now grants
nothing. This closes the handle and returns `None`. Observable sidecar
behaviour is identical (no auto-kill either way); the difference is only a
leaked kernel handle and a stream of pointless per-child warnings.

**Not runtime-tested, and the code says so.** CI is ubuntu-only. Verification
is `cargo check --target x86_64-pc-windows-gnu` plus a read against the
reference's `ctypes` calls. Stated in the module docs and the ADR rather than
implied by a green check.

Revisit if: a Windows runner joins CI (then test it for real), or the Unix
abnormal-exit row starts mattering — but note that closing *that* would put
this ahead of the reference, which is a different decision than parity.

## E1, the hub: port it, with storage behind a trait (ADR-0015)

**Decision:** port all ten routes as `remind_me_hub` / `rusty-remind-me-hub`,
with a `HubStore` trait and two backends — Postgres (default) and SQLite.

This was the one gap-analysis item deliberately never filed as an issue,
because it is a scope decision rather than an implementation gap: a second
deployable in a second runtime. It was put to a human and came back yes, with
"both backends" chosen over either alone.

**Why a trait and not just Postgres.** The reference is Postgres-only, and a
Postgres-only port is the faithful reading. But it makes a single-operator
self-hosted hub run a database server for a workload one file would serve. And
the trait had to exist anyway for the differential test — the thing that makes
"two backends" a claim rather than a hope.

**Why not just SQLite.** Far smaller, and it would have reused
`sync::server` almost wholesale. But it cannot adopt an existing Postgres
deployment without a data migration, and "successor" means little for a
component whose whole job is holding the shared copy if it cannot take that
copy over.

**Where the trait boundary sits, and why there.** Complete operations only — no
connections, no transactions, no SQL. `apply_record` takes one *already
validated* record and owns the transaction around it, because the
savepoint-per-record isolation is part of the operation rather than something a
caller can be trusted to remember. Validation and canonicalisation live above
the trait, so a backend never sees a malformed record and "malformed" can never
be confused with "storage failed" in the push tally.

**The one deliberate divergence, and it is a bug fix.** The reference's legacy
TIMESTAMPTZ→TEXT migration strips all trailing zeros, turning `.500000` into
`.5` — which `datetime.isoformat()` never emits, despite the reference's own
comment saying the goal is to match it exactly. Under `COLLATE "C"` that sorts
*before* the client's own value, so a migrated row compares as older than the
identical instant on the node that wrote it, corrupting the pull cursor and LWW
alike. This strips only a wholly-zero fraction.

**That bug is the argument for the Postgres tests.** It was invisible in
review and only appeared when the migration ran against a live server. Everything
else about the backend compiles and type-checks perfectly while getting this
wrong.

**`approx` degrades honestly.** SQLite has no planner estimate worth reporting,
so the backend says so, the route falls back to exact counts, and the response
reports `approximate: false`. Labelling a scan "approximate" would be the one
thing the flag must never mean.

Revisit if: the SQLite backend ever needs to *become* a Postgres one in place
(there is no migration between them today, deliberately — they are different
deployments, not two states of one).
