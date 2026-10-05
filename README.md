# Rust Copy Trader

A Rust service that observes one mainnet wallet through Helius LaserStream
gRPC and executes eligible copies on either Surfpool or Solana mainnet. Mainnet
submission uses Helius Sender and is disabled unless two configuration gates
are enabled explicitly.

The source transaction is decoded into an economic input/output intent. The
router builds and simulates direct Raydium CPMM, Raydium CLMM, Orca Whirlpool,
Meteora DLMM, and PumpSwap exact-input routes.

- Native SOL and policy-admitted SPL-token inputs/outputs
- Percentage or fixed copy sizing
- Direct Raydium CPMM/CLMM, Orca Whirlpool, Meteora DLMM, and PumpSwap execution

Token admission defaults to `token_policy.mode = "allowlist"`. To accept every
mint that passes runtime token-safety inspection, use an explicit global safety
envelope:

```toml
[token_policy]
mode = "all"
minimum_input = "0.001"
maximum_input = "0.1"
seed_balance = "10"
```

In all-token mode, `[[tokens]]` entries are optional per-mint overrides and
startup prewarming hints. Previously unseen pairs are discovered on demand.
Surfpool inputs are seeded once per run ID; mainnet inputs must already be held
by the copier wallet.

## Single-hop routing

For one recognized source swap, the decoder extracts a pool hint from outer or
inner instructions, resolving lookup-table addresses. Supported instruction
layouts cover Raydium CPMM/CLMM, Orca Whirlpool, Meteora DLMM, and PumpSwap.
Unknown layouts and multiple recognized swaps fall back to general routing.

The router reads the hinted pool at or after the observed slot and validates its
owner, discriminator, layout, and mint pair. It then builds a fresh exact-input
swap for the copier's amount, accounts, slippage, fees, and blockhash, and simulates
the signed transaction. Source instructions and quoted amounts are never replayed.
This prioritizes the source pool over price comparison across other pools.

A successful source route makes no broad pool-discovery requests. Its pool
descriptor is cached in memory for later observations. Quote state is still read
fresh. Custom adapters reuse the validated source account for this attempt;
the Orca SDK still fetches its own pool state. CLMM tick-array and DLMM bin-array
queries remain part of route preparation.
Periodic refreshes preserve learned pairs outside their configured mint set.

If the source route fails, the router tries cached alternatives, then discovers
the pair. Each DEX's discovered candidates start preparing immediately, while
other DEX discoveries continue. The first successfully simulated route wins;
outstanding discovery and candidate work is cancelled. A failed candidate does
not prevent later discoveries from winning. Source preparation has up to half of
`routing.race_timeout_ms`; the complete routing operation, including fallback
discovery, shares the full deadline. No transaction is sent until a route succeeds.
The cache is rebuilt after restart.

## Setup

1. Copy `config.example.toml` to `config.toml` and configure the source wallet,
   nearest LaserStream region, allowed mints, and sizing limits.
2. Set `HELIUS_API_KEY` and `COPY_TRADER_KEYPAIR_PATH` in `.env`.
   Mainnet LaserStream access requires an eligible Helius plan.
3. For Surfpool execution, keep `execution.target = "surfpool"` and start
   Surfpool against a mainnet RPC datasource:

   ```bash
   NO_DNA=1 surfpool start --rpc-url "$SURFPOOL_DATASOURCE_RPC_URL" --db ./surfnet.sqlite
   ```

4. For mainnet execution, fund the copier wallet with its input tokens and
   enough SOL for account rent, network fees, and the Sender tip. Then set:

   ```toml
   [signal]
   commitment = "processed"

   [execution]
   target = "mainnet"
   allow_live_mainnet = true
   ```

   Keep `?swqos_only=true` on `mainnet.sender_url` for the lower-cost SWQoS
   path (minimum 5,000-lamport tip), or remove it for Sender Max (minimum
   1,000,000-lamport tip). `max_priority_fee_micro_lamports` caps the dynamic
   per-compute-unit priority fee.

5. Validate dependencies, balances, and the selected backend, then run:

   ```bash
   cargo run -- --config config.toml doctor
   cargo run -- --config config.toml run
   ```

Inspect the durable journal with:

```bash
cargo run -- --config config.toml status --limit 20
```

Run the isolated Surfpool execution test when the `surfpool` CLI is installed:

```bash
cargo test --test integration surfpool_executes_signed_transfer -- --ignored
```

## Logs

Logs are compact and grouped by copy attempt. At the default level, dependency
noise is suppressed. Long signatures are shortened in log context while the
full values remain in SQLite and the `status` output.

Each persisted execution skip emits an INFO `copy skipped` event with the full
`source_signature` and `skip_reason` (for example, `BelowMinimum` or
`InsufficientBalance`), so it can be matched to the `copy timings` event.

Use `RUST_LOG` when diagnosing a subsystem:

```bash
# Include internal RPC, recovery, DEX, and mint-inspection details.
RUST_LOG=rust_copy_trader=debug cargo run -- --config config.toml run

# Focus on one subsystem while keeping normal application progress.
RUST_LOG=rust_copy_trader=info,rust_copy_trader::signal=debug cargo run -- --config config.toml run
```

LaserStream and RPC URLs are intentionally omitted from log fields because they
can contain API keys.

## Local database workbench

Start the read-only UI using Python's standard library:

```bash
python3 -B db_ui/server.py
```

Open **http://127.0.0.1:8765**. The server defaults to this project's
`copy_trader.sqlite`, binds only to loopback, and can stay open while the trader
runs. Stop it with Ctrl+C. For another database or port:

```bash
python3 -B db_ui/server.py --db /path/to/journal.sqlite --port 8766
```

- **Copy journal:** target/status filters, signature/pool search, source and copy
  slots, route and submission timings, and a detailed trade inspector.
- **Execution flow:** an expandable mainnet code map showing awaited I/O, concurrent
  groups, synchronous work, and background tasks. Highlight a type to follow its
  role; this is an architectural view, not a measured trace. Open `/#flow`.
- **Tables:** paginated, sortable/searchable records, column metadata, and schema SQL.
- **SQL console:** read-only queries, example queries, and keyboard execution
  with Cmd/Ctrl+Enter. Results stop at 500 rows; queries have a two-second execution
  budget and bounded SQLite lock waits.

Use Refresh to reload current data; there is no background polling. The UI opens
the file in SQLite read-only mode and does not run migrations. Landing slots or
new timing fields appear as unavailable on older schemas. Binary values show
their size; long text is truncated for display. Full-size SQLite integers outside
JavaScript's safe range are returned as strings.

Writes, schema changes, attached databases, extension loading, and cross-origin
API access are blocked. All assets are local; no trading data is uploaded.

Validate the UI backend without touching your trading database:

```bash
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s db_ui -p 'test_*.py'
node --check db_ui/static/app.js
```

## Landing slots

The journal stores the source's observed slot in `source_transactions.slot` and
its copy's confirmed landing slot in `copy_attempts.landed_slot`. The latter comes
from the transaction status, not the RPC response's current context slot. It is
saved before balance reconciliation, including for confirmed on-chain failures,
and is also captured when startup recovery resolves uncertain submissions.

`status` displays `source_slot`, `copy_slot`, and `slot_delta` (copy minus source).
The delta is only shown for mainnet copies; Surfpool uses a separate ledger.
These are slot numbers, not block heights or block hashes. Source observations
can be processed and subject to forks; copy slots require confirmed or finalized
status. Historical attempts without a recorded slot display `-`; this migration
does not backfill them. No extra RPC calls are added to the submission path.

## Copy latency

Each processed live observation emits a `copy timings` log and stores optional
`timings_json` on its source row, including skipped and failed copies. `status`
prints the available JSON or `-` for historical/unmeasured rows. Durations use a
monotonic clock and milliseconds:

- `ingestion_queue_ms`: local stream receipt through decoding, journaling, and
  waiting for the sequential execution worker.
- `preparation_ms`, `discovery_ms`, `route_ms`, `post_route_ms`: work before send;
  post-route includes winner handling and serialization, but no route or signed-transaction writes.
- `source_route_ms`: time spent validating and preparing the source pool, including
  failed attempts; this is a component of `route_ms`, not an additional duration.
- `receipt_to_send_start_ms` and `receipt_to_send_response_ms`: separate offsets
  from receipt to the Sender call and its return (including errors).
- `sender_request_ms`, `post_send_journal_ms`, `confirmation_ms`, `reconciliation_ms`: subsequent stages.

Unreached stages are omitted. Failed stages retain elapsed time. These are local
pipeline timings, not source-chain-to-stream latency. Existing second-resolution
journal timestamps and `route_latency_ms` retain their meanings. The additive
migration runs at service startup; historical timing data is not synthesized.

### Source-direct execution

Set `mainnet.skip = true` and
`mainnet.fixed_priority_fee_micro_lamports = 100000` for the speed-first mainnet path.
The fixed price must not exceed `mainnet.max_priority_fee_micro_lamports`.
`skip = false` restores normal routing, quoting, dynamic fees, and simulation.
The default is `false`; `source_direct` remains an alias for older configs.
Set only one of these two keys. Changes take effect after restarting the trader.
The example keeps this mode disabled; the local `config.toml` enables it with
`execution.slippage_bps = 5000` (50%).

This mode requires one recognized, validated source pool. It never searches or
races alternative pools; missing, ambiguous, invalid, or failed source routes stop
that copy. Background pool-catalog discovery is also disabled. All five adapters
use `floor(copy_input * source_output / source_input)` as the output reference,
then apply configured slippage to that reference. Zero/invalid estimates and
arithmetic overflow are rejected. This is a source-price estimate, not a fresh
liquidity quote; a different trade size or subsequent pool movement can make it
inaccurate. Pool/account validation, token safety checks, duplicate reservation,
transaction signing, confirmation, and reconciliation remain enabled.

No quote engine, `getPriorityFeeEstimate`, `getFeeForMessage` pre-send check, or
`simulateTransaction` runs in this mode. Sender already uses `skipPreflight=true`.
The network checks execution and fees; failed landed transactions can still cost
fees. Pool, mint, token-account, blockhash and balance reads remain; Raydium CLMM
and Meteora still read tick/bin accounts needed to construct instructions. Orca
derives tick-array addresses directly from the validated pool without fetching
quote state. SOL wrapping still fetches rent.

Timing uses `route_instruction_build_ms` instead of `route_quote_ms`; skipped
simulation and dynamic-fee timers are absent. `simulation_json` explicitly records
`{"skipped":true,"reason":"source_direct"}`. `route_final_checks_ms` in this mode
only waits for the output-balance snapshot used for reconciliation. Route and
signed-transaction journal writes still happen after submission.

The routing and simulation descriptions below apply to normal mode
(`mainnet.skip = false`) unless stated otherwise.

Mint safety reads are batched per copy. Mainnet candidates share validated mint
metadata, one ATA lookup, and one fresh confirmed blockhash across source, cached,
and discovered routes. Shared reads run concurrently with source validation,
discovery, and adapter preparation. Initial mint inspection overlaps the intent
journal write, SOL-input balance read, and source-pool prefetch. The intent write must succeed before
reserving an attempt or routing. A slow or failed prefetch does not delay mint
inspection; the router validates the prefetched account before use or falls back.
Prefetch uses `minContextSlot` and a fetch timeout of half the routing budget.

Once each final transaction is built, its fee check runs alongside exact simulation.
Candidates share one successful SOL/output balance snapshot per attempt, prefetched
at route entry alongside shared preparation and quoting; SOL output reuses the SOL
balance response. A failed SOL prefetch is retried by final checks. Snapshots are
never reused across trade attempts. A candidate must pass both simulation and the
funds check before it can win. Route and signed-transaction persistence run after the send response (including send errors).
Write failures are logged without stopping confirmation. A crash between submission
and persistence can leave the local journal without the submitted signature.
Startup marks these attempts `unknown` and never retries them automatically.
Orca reuses the prefetched source pool and fetches tick arrays, mints, the adaptive
oracle (when applicable), and potential token accounts in one RPC batch. Epoch and
rent reads run concurrently with the batch. The SDK quotes and builds instructions
from this per-attempt snapshot; no quote state is cached across trades. Missing
snapshot requests fail explicitly. Quotes, slippage limits, SOL wrapping, fee
estimation, and exact simulation retain their existing behavior. Cached alternative
pools need an initial pool read before this batch.

PumpSwap batches configs, mints, vaults, and the copier's WSOL account into one
state request, decoding balances and supply locally. Candidates still estimate
their own fees and simulate the exact signed transaction.

Shared preparation and fallback discovery consume the route timeout.
`discovery_ms` measures intervals waiting solely for discovery when no candidate
race is active; discovery that overlaps candidate work is included in `route_ms`.
`route_ms` and `route_latency_ms` exclude only those discovery-only intervals.
`discovery_ms` is zero when the source pool or cached candidates suffice.
New optional fields break down routing work:

- `source_pool_read_ms`: prefetched source-account RPC time, including work during
  initial preparation.
- `route_source_pool_fetch_ms`: remaining source fetch wait and validation inside
  routing; it excludes prefetch time already spent during initial preparation.
- `route_shared_preparation_ms`: shared ATA reads and cached blockhash lookup (RPC
  on a cold/stale cache).
- `route_balance_prefetch_ms`: per-attempt SOL/output balance prefetch, overlapping
  preparation, discovery, and quoting.
- `route_quote_ms`: adapter state reads, quoting, and instruction construction.
- `route_fee_estimation_ms`: candidate fee estimation and fee instructions.
- `route_simulation_ms`: exact signed-transaction simulation.
- `route_final_checks_ms`: final fee and balance checks, overlapping simulation.

These stage values sum elapsed milliseconds across attempted candidates, including
failures and cancellation. Stages and candidates overlap, so these values must
not be added together to derive wall time. Unstarted stages are omitted.
Final checks have moved from `post_route_ms` into routing, while source prefetch
partly moves into `preparation_ms`. Compare `receipt_to_send_start_ms` to measure
overall improvement across versions, rather than comparing a single stage.

Copies remain sequential through confirmation and balance reconciliation.

Mainnet blockhash preparation uses a process-local cache, warmed before signal and
execution workers start. A background task refreshes it every second after the
previous refresh completes. Entries are usable for less than two seconds measured
from request start; refresh calls have a 1.5-second timeout. Each refresh fetches
confirmed blockhash and block height concurrently, retaining `lastValidBlockHeight`
and requiring more than 20 blocks of remaining validity at the observed height.
Cold/stale readers share a refresh; fresh readers do not wait on background RPC.
Refresh failures do not extend freshness, and expired entries require a successful
RPC fetch. The maintenance future stops with the service. This adds roughly one
blockhash and one block-height request per second while idle.

This cache also helps the first trade on an unseen pool. Pool quotes, ATA checks,
transaction-specific fee estimates, final fee checks, and exact signed simulation
still use their existing validation paths. Surfpool continues fetching blockhashes
after market preparation. No cross-trade wallet balance cache is used.

Mainnet startup pings the configured Sender origin, then pings every 30 seconds
between completed warmups using the shared HTTP client. Each ping has a two-second
timeout; failures do not stop execution. Surfpool never starts this warmer.

Compare `receipt_to_send_start_ms` across comparable successful runs, separating
cold discovery from known pairs and inspecting failures separately. Two historical
successes are not enough to claim reliable median/p95 improvements. The controlled
mock-RPC tests validate call reductions without sending any live transactions.

## Database and logging overhead

Each copy's existing timing JSON now includes a `database` object, for example:

```json
{"database":{"persist_signed":{"elapsed_us":812,"calls":1}},"db_pre_send_us":2640}
```

Database durations use a monotonic clock at microsecond resolution, from entry
into the awaited Store method through its return. They include pool acquisition,
SQLite lock waits, SQL work, and commit. Repeated calls are summed per operation;
failed calls also retain their elapsed duration. No extra database writes are
introduced to record individual operations.

Measured operations are `record_observation`, `mark_intent`, `reserve_attempt`,
`mark_route` (including its transaction commit), `persist_signed`, `mark_skipped`,
`record_landed_slot`, `update_attempt`, and `record_timings`. Surfpool on-demand
seeding also measures `is_seeded` and `record_seeded_balance`. Startup/recovery,
background maintenance, and the cursor update after enqueueing are outside this
per-copy collector; contention they cause can still appear in measured waits.

- `db_pre_send_us`: sum of measured DB calls completed before the send starts;
  absent when no send was attempted.
- `db_post_send_us`: measured DB time after that point, including final status
  writes; absent when no send was attempted.
- `db_total_us`: all measured DB time for this observation.

Database calls can overlap RPC work, so their summed durations are not the time
that would be saved by removing them. They are not additional pipeline time.

SQLite connections explicitly use WAL and `synchronous=FULL`. Attempt reservations
remain awaited before sending to prevent duplicate execution. Route and signed-transaction
writes are awaited after submission, measured by `post_send_journal_ms`. WAL allows
readers to coexist with a writer; writes still serialize.

The worker logs `copy timings` immediately, then queues the same JSON without
waiting for persistence. One background task commits up to 32 available timing
records per transaction, using a bounded 256-record queue. If the queue is full
or a batch fails, it warns and drops those database timing records; the original
logs remain available. Timing persistence failures do not stop trading. Both log
and stored totals exclude background timing writes. Historical logs may include
`record_timings` from the previous synchronous writer.

The timing queue drains when the execution worker returns normally or with an
error. Process termination (including the service's current Ctrl-C shutdown) can
lose pending telemetry. Critical recovery records do not use this queue. Console
logging remains synchronous.

Summarize recent successful mainnet samples without starting the trading service:

```bash
python3 scripts/report_db_timings.py --db copy_trader.sqlite --target mainnet --status landed --limit 500
```

The read-only report shows sample count, median, and nearest-rank p95 in
microseconds per operation and for total DB/receipt-to-send time. Missing metrics
are omitted, not filled with zero. `--status all` groups attempted copies by
outcome instead of mixing failed and successful copies. The limit applies to
recent matching journal rows; older rows without DB instrumentation are skipped.
Compare known-pair/cold-discovery workloads separately when interpreting results.
The script uses only Python's standard library; no live network calls occur.

For a controlled console-writer comparison:

```bash
cargo run --offline --example logging_latency -- --samples 200 --sink-delay-us 1000 --rpc-delay-ms 2
```

This replays identical modeled preparation/route delays and log events against a
synchronous writer and `tracing-appender`'s bounded background writer. It prints
median/p95 modeled receipt-to-send-start time and dropped-line counts. The sink
is deliberately delayed and discards output; it does not measure your terminal
or mainnet performance. Adjust `--sink-delay-us`, `--rpc-delay-ms`, and `--capacity`
to explore congestion. Dropped lines trade completeness for latency. The example
uses no keys, RPC endpoints, or database, and submits no transactions.

## Safety model

Mainnet mode accepts successful `processed` trade signals. This intentionally
trades finality for latency, so a source transaction can still be dropped by a
fork after its copy has been submitted.
It requires either an explicit mint allowlist or an opt-in global token safety
envelope,
simulates the exact signed transaction in normal mode, submits it once with Sender retries disabled,
then persists the route and signed transaction, and never executes trades recovered after a
disconnect. In normal mode, a priority-fee lookup or exact simulation failure stops the copy. Source-direct mode skips both steps as described above.

Surfpool-only account seeding, slot alignment, and account resets are never run
for a mainnet target. `doctor` performs read-only connectivity and balance
checks and never submits a transaction.

The LaserStream SDK handles HTTP/2 keepalives and transport reconnection. SDK
replay is intentionally disabled: slot gaps trigger the existing
HTTP history audit, recovered signatures are persisted as `missed_offline`, and
only protobuf transaction updates received on the live stream can execute.
