# Spike: spreading search results across time buckets (Mill Board #568)

Status: **measured 2026-11-27 (a Friday), fixture half, in a cloud session;
settled by D137 (recency opt-in, window dropped).**
Verdict under D136: **both modes fail the pre-registered rule on the
fixture half**, so neither can be kept on that rule, whatever the #1563 set
shows. Window mode hurts; recency mode is neutral on every gated query and
fixes the status-question check.

## Question

Do the two flagged modes help enough to keep?

- **window** (the card as written): a window wider than 3 days is cut into
  `clamp(days / 7, 3, 6)` equal buckets and the ranked list is interleaved
  round-robin, newest bucket first.
- **recency** (the 2026-11-18 card comment, "latest on Attest"): a search
  with no window lifts the best hit of each non-empty bucket among the
  last 2 days, the last 7 days and older into the top `limit`, in place of
  a crowded bucket's lowest-ranked hit, as far as `limit` has room.

## Keep rule (pre-registered, Nano 2026-11-27, pack D136)

- window: keep if recall@10 or MRR on temporal queries rises 2+ points and
  no other kind or strategy drops more than 0.02, on the #59 fixture and on
  the #1563 real-query set.
- recency: the same, plus a status-question set (10+ "latest on X", clock
  relative to the run) where a newest-week relevant hit must reach the top
  10.

## What was built (pack D135)

- `remind_me_core::time_buckets`: pure functions (`spread_window`,
  `reserve_recent`) plus `Modes` and `apply`. The flag is
  `REMIND_ME_TIME_BUCKETS=off|window|recency|both`. Unset, blank or
  unrecognised means off.
- `search_memories_deadlined_with` takes the modes. The search tool
  (`search_with_expansions`) passes the flag's modes, the harness passes
  them directly, and every other caller passes off through
  `search_memories_deadlined`. The modes apply after fusion and rerank, on
  every candidate (the reranked head, then the rest in rank order, so a
  `limit` as large as the rerank pool can still lift a missing bucket),
  before `limit` and the token budget. With the flag off, nothing changes.
  The numbers below were re-run after that change and did not move.
- The time a row is bucketed by is `occurred_at` (a span's `to`) under
  `time_field: occurred`, otherwise the field the window reads (`created_at`
  by default).
- Runner: `tests/time_buckets_spike_test.rs`, ignored
  (`cargo test -p remind_me_core --release --test time_buckets_spike_test --
  --ignored --nocapture`).

## Method

- The #59 harness, modes keyword and hashed, all four strategies.
- Window mode only acts when the search has a window, so it is read in the
  `+dates` modes, where date inference makes the window. As in #59, those
  move with the weekday.
- Recency mode only acts when the search has no window, so it is read in the
  plain modes.
- Base store: the #59 fixture and its 82 queries. The committed baseline is
  untouched.
- Synthetic store: the fixture plus two generated worlds.
  - `status`, gated for recency: 12 subjects. Each has ten rows 8-17 days
    old that match "latest on X" well and are marked as read 30 times today,
    plus one fresh row (1 day old) that is the answer.
  - `window`, reported only, since it is built to show the failure: 6
    subjects. Each has eight rows bunched 25 days back, plus three rows
    spread across the month that are the answer, queried with "in the past
    30 days".

## Results (fixture half)

Temporal pool (kinds temporal, dated, occurred; 20 queries), strategy
`auto`, recall@10 / MRR:

| Mode | off | window | recency |
|---|---|---|---|
| keyword | 0.983 / 0.850 | 0.983 / 0.850 | 0.983 / 0.850 |
| hashed | 0.983 / 0.885 | 0.983 / 0.885 | 0.983 / 0.885 |
| keyword+dates | 1.000 / 0.917 | 1.000 / 0.892 | 1.000 / 0.917 |
| hashed+dates | 1.000 / 0.734 | 1.000 / 0.660 | 1.000 / 0.734 |

- **window**: temporal MRR drops 0.025 (keyword+dates) and 0.074 to 0.092
  (hashed+dates, depending on strategy), with recall unchanged. Interleaving
  pushes a relevant older hit below newer hits in the newest bucket. No other
  kind moved. On the synthetic `window` world, MRR rises 0.80 to 0.94 and
  recall@10 rises up to 0.28, but that world was built to show exactly this
  case. **Fails**: no lift, and a drop larger than 0.02.
- **recency**: no gated query moved, in any mode or strategy, so the drop
  is 0 and the lift is 0. On the status set, hit@10 goes from 1.00 to 1.00
  in keyword mode (the answer was already ninth) and from 0.02 to 1.00 in
  hashed mode, pooled over strategies (48 searches each). **Passes the
  status check, fails part (a)**: no 2-point lift on temporal queries.
- Latency per search, p50 / p95 in µs over 4,848 searches each: off 243 /
  907, window 242 / 909, recency 245 / 913. No measurable cost.

## Reading

- Window mode is a drop on any reading. The card's worry, results bunched on
  one day, is real only when the bunch is irrelevant. When the bunch holds
  the answer, round-robin interleaving costs MRR.
- Recency mode fails D136 only because part (a) asks for a lift on the
  fixture's temporal queries. Those queries already rank their answers
  within the top 10, so a reserved slot never changes them. The mode did
  what it was built for, and changed nothing else. Whether that is enough to
  keep it is a change to a [stated] rule, so it is Nano's call, not the
  harness's.
- The #1563 real-query half was not run. It cannot turn either fixture
  result into a keep under D136.

## Outcome (Nano, 2026-11-27, D137)

Window mode is dropped and its code removed. Recency mode merges behind
`REMIND_ME_TIME_BUCKETS=recency`, off by default, so it can be tried on the
live store. Making it the default is a separate decision; the ignored runner
`tests/time_buckets_spike_test.rs` stays so that call can be made on new
numbers.
