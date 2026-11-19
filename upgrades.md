Eva01 Screening Engine: Full Design
Nov 18, 2026 · @Michael Justin dulin
Replace the per-account exact re-check with an incremental, vectorised health screen that covers all ~60k accounts in about 1 ms per price update. Today's exact code stays as the verdict for the handful of accounts the screen flags. Health is linear in per-bank unit values, so a price change only needs a multiply-add per position, not a full re-evaluation of every holder.
v2: Sections 14–16 are the definitive plan for the coding agents. They merge this design with an outside review and supersede sections 3–13 wherever they differ. Sections 1–13 remain as the reasoning behind them.
1. Goal and constraints
Goal: every account checked within 10 ms of an update being applied, with a screening pass of about 1 ms and an exact verdict on flagged accounts in a few ms more. This design is the check path only; section 9 covers what still bounds landing speed.
#
Requirement
How the design meets it
R1
All accounts screened in under 10 ms per update
Flat arrays grouped by bank, one pass per moved bank
R2
No new false negatives: anything today's path would flag, the screen flags
The screen only filters; it may over-flag but never under-flag (section 5)
R3
Exact health, premium and sizing stay as they are
The existing size_on_accounts / prepare path is the only verdict
R4
No change to transaction building or on-chain behaviour
Only the evaluation front half changes
R5
Fits in cache; no per-update allocation on the hot path
About 60k accounts × a few positions ≈ a few MB, plain f64 and u32 arrays
Non-goals: changing sizing, premium or tag rules; changing how bundles are built or sent.
2. Where the time goes today
The exact check costs about 100–160 µs of wall time per account across the whole thread pool, so checking all 60k accounts takes 7–10 s, and the single loop that runs it is saturated. The numbers below come from the live log of a 60 s window; the code-level causes are read from the source, not profiled.
Measure (live log, ~60 s)
Value
Reading
Change cycles
15–23 per minute, 1.3–1.5 s median
About 28 s of each minute
Sweep slices
17–25 per minute, slowest 2.4–3.3 s
Fills the rest of the minute
Evaluations
360k–400k per minute (~6.2k/s)
Each account about 6 times a minute
Skipped by health watch
1.2M–1.6M per minute
The watch works, but the rest is still too much
Accounts watched
~60k
The whole working set
Worst cycle
18 s once
A stall, cause unknown
Structural causes
• One serial loop. Liquidator::start runs the change cycle, then the sweep slice, on one thread. A price update that lands during a sweep slice waits up to ~3 s, then its own cycle takes ~1.4 s.
• Results held to the end of the batch. queue.offer is called after check_accounts finishes, so a liquidatable account found early in a 1.4 s cycle is not queued until the whole batch is done.
• No ordering. par_iter over a set-derived vector evaluates in arbitrary order; the account closest to the threshold is no more likely to go first than a dust account.
• Dust is evaluated like everything else. The sizing line in the log is an account with $0.31 of repayable debt, a profit ceiling of about $0.016 against a $1 minimum.
Likely per-evaluation costs (to confirm by profiling)
• program_accounts clones the full Account (data included) of every bank and oracle for each account and each pricing mode; OraclesCache::try_get_account and BankWrapper also return clones.
• Oracle prices are parsed again for every account, though they are identical for every account in a cycle.
• Accounts within 2% of the threshold are priced three or four times (Newest, Chain, Pushed).
• get_health_components runs twice (maintenance, then equity) and re-reconciles e-mode each time.
• The marginfi accounts map is behind one RwLock and every read clones a ~2 KB MarginfiAccountWrapper.
3. Design overview
Updates from the three sources reach one state thread, which keeps unit values and a position table and runs the screen. Only accounts the screen flags go to the existing exact check; an audit thread keeps comparing the screen against exact results.
4. Position table and the health math
The screen keeps, for each account, a running estimate of its maintenance health, and updates it with one multiply-add per position whenever a bank's unit value moves. This works because marginfi's get_health_components is linear in per-bank prices: for each active balance it computes amount × biased price × weight, then sums assets and subtracts liabilities.
H_i = \sum_{a \in assets(i)} c_{ia}\, U^{A}_{b(a)} \;-\; \sum_{l \in liabs(i)} c_{il}\, U^{L}_{b(l)} \;-\; P_i(t)
• Unit value U per bank and side: the maintenance price (low-biased for assets, high-biased for liabilities, confidence applied) times the share value. Eva already computes exactly this in unit_value in src/liquidator.rs, so the screen reuses it.
• Coefficient c per position: shares × weight ÷ 10^decimals. The weight is the account's own maintenance weight, including e-mode. It changes only when the account or its banks' config changes.
• Premium P_i(t): the projected variable-borrow premium on premium-active banks; it grows with time, so it is handled separately (section 6).
Data layout
Structure of arrays, grouped by bank so a price move touches contiguous memory:
Array
Contents
Size (60k accounts, ~3 positions each)
h[acct]
f64 health estimate per account
~0.5 MB
trip[acct]
f64 per-account hot threshold
~0.5 MB
liab_usd[acct]
f64 equity-priced debt, for the dust floor
~0.5 MB
holders[bank]
rows of (acct: u32, coef: f64, side)
~200k rows × 16 B ≈ 3 MB
unit[bank][side]
current U per bank and side, two copies (Newest, Chain)
a few KB
Total is a few MB: it stays in L2/L3, so a pass is memory-bound at cache speed.
Update step
When a bank's unit value changes by ΔU on one side, the screener runs one loop over that bank's holders:
h[a] \mathrel{+}= \pm\, c_{a}\,\Delta U \quad\text{for each holder row } (a, c_a)
and pushes a onto the hot list if h[a] < trip[a]. That is a gather/scatter over at most the holders of one bank (about 20–40k rows for the biggest banks), so roughly 50–100 µs. All banks that moved in one batch are processed together, so one batch is still about 0.1–0.3 ms.
Keeping the estimate honest
Whenever an account gets an exact evaluation, the screener stores the residual r = H_exact − H_est and folds it into h. That absorbs fixed-point rounding and anything the linear model does not capture, so the estimate re-anchors to the program's own arithmetic on every exact check.
5. Screening rule and correctness
The screen is a filter, never a verdict: it may flag an account that turns out healthy, but it must not miss one the exact path would flag. That holds when every way the estimate can be wrong either has a bound smaller than the trip margin or forces the account to the exact tier.
The rule
An account goes to the exact tier when all three hold:
1.  Near or past the line: h[a] < trip[a], where trip is a small relative margin of the account's gross value, θ × (A + L). Start at θ = 0.1%, tuned by the shadow run.
2.  Worth a liquidation: liab_usd[a] is above the dust floor, about $4.3 (a $1 profit needs debt above min_profit / (0.95 × 0.245), the 25% tag premium cap less the 0.5% safety margin). A tighter floor of about $19 applies to an untagged account at the base premium, but the $4.3 floor keeps tag candidates visible.
3.  Eligible: it has deposits and liabilities, and is not excluded by mint or asset tag (checked once per account change, not per tick).
Why it cannot miss
Source of error
Size
Handling
Fixed-point vs f64 rounding
~1e-12 relative
Far below θ; also re-anchored by the residual on every exact check
Price bias and confidence
None: they are inside the per-bank U, computed by the existing exact code
n/a
Share-value growth (interest)
None: part of U, updated when the bank account updates
n/a
Weights, e-mode, bank config
Discrete
Any change rebuilds the holders' coefficients and sends them to the exact tier
Account balance change
Discrete
The account is re-added to the table and evaluated exactly
Oracle becomes stale or unpriceable
Discrete jump
Bank flagged; all holders go to the exact tier (matches today's behaviour)
Variable-borrow premium growth
Slow drift
Section 6
Two price views
Eva sizes on the newest price (Newest, usually Hermes) and also checks the chain's price (Chain) for accounts near the threshold, because an account can be liquidatable on the chain's older price after Hermes recovered. The screener keeps one h array per view and flags an account if either is under its trip level. That doubles the arrays, not the algorithm.
Hot versus warm
• Hot (h under the tight trip level): exact check now.
• Warm (within about 1% above it): no exact check on each tick. These are the accounts to pre-stage (section 9), since they are the likeliest next liquidations.
6. Special cases
Each case below is either exact in the linear model or sent to the exact tier, so none of them weakens the no-miss guarantee.
Case
What the program does
Screen handling
E-mode
Maintenance weight depends on the account's liability banks and collateral tags (reconcile_emode_configs)
Weight is computed on account or bank-config change with the existing function and stored in c; a group update (same-asset leverage) rebuilds all
Variable-borrow premium
Projected premium on premium-active liabilities, growing with time
Accounts with a premium-active liability are always-hot, or carry a drift term rate × Δt from the last exact check; confirm how many there are
Stale or unpriceable oracle
The leg is skipped and flagged (err_code)
The bank's valid_until (publish time + max age) is tracked; when it passes, or unit_value returns an error, all holders go to the exact tier
Confidence too wide
Same as unpriceable
Same
Staked banks
Price from SOL pool, LST mint supply and on-ramp accounts
Any of those oracle accounts updating re-runs unit_value for the bank (get_banks_for_oracle already maps them)
Kamino and JupLend
Price includes the venue's exchange rate
unit_value already reads the reserve or lending state; the 60 s polling staleness is unchanged (optional fix below)
Paused or reduce-only bank
Still counted at maintenance
No special handling; the status is in the bank config
Isolated-tier asset
Counts as zero collateral
Coefficient is zero
Account in a flash loan
Health errors
Excluded; re-added on the next account update
Empty stake pool
Program cannot price the account
Excluded, as process_account does today
Optional: remove the 60 s staleness on Kamino and JupLend
The fetcher polls the reserves every 60 s and patches the slot or timestamp so marginfi accepts them. Eva could instead subscribe to those accounts on Geyser and apply the same patch on receipt. That gives them the same freshness as every other account and removes the one input that can be a minute behind. It would need the patch moved into the Geyser handler and the fetcher kept as a fallback.
7. Exact tier, streaming and threading
The screener runs on one dedicated thread that is the only writer of its tables, so it needs no locks. The exact tier runs on the existing rayon pool but only on flagged accounts, ordered by urgency, and each verdict goes to the queue the moment it is ready.
Threads
Thread
Job
Notes
Geyser / Hermes receivers
Decode, filter by group, deserialize MarginfiAccount and banks
As today; pushes events to the state thread
State thread (new)
Apply events to Cache, recompute unit_value for moved banks, update the position table, run the screen, emit hot accounts
Single writer, pinned to a core, spin-polls its channel briefly before parking to avoid wake-up latency
Exact pool
process_account on hot accounts in priority order
Reuses the current evaluator; results stream out
Workers (8)
Re-evaluate, build and send
Unchanged
Audit thread (new, low priority)
Rolling exact re-check of a slice of accounts, compares with h
Replaces the 30 s sweep; section 8
Priority and streaming
• The hot list is ordered by most negative margin first, then by the largest possible profit (min(debt, collateral) × premium).
• Each exact verdict calls a new queue.upsert(intent) or queue.retire(account) immediately, instead of queue.offer(checked, intents) after the whole batch.
• The existing re-evaluation in attempt_liquidation stays: a worker still re-checks on the freshest state before sending.
Removing avoidable cost from the exact path
• Store accounts, banks and oracles as Arc<Account>, so program_accounts stops cloning data. MarginfiAccountsCache returns Arc<MarginfiAccountWrapper>.
• Compute each bank's price once per update, not once per account; the screener already does this for unit_value.
• Split the RwLock<IndexMap> so reads on the exact path never wait on the state thread's writes.
What this replaces
HealthWatch, accounts_to_evaluate, Sweep and the post-batch queue.offer are replaced by the screener and the streaming hand-off. size_on_accounts, prepare, TagPolicy and the executor are unchanged.
8. Validation and rollout
The screen goes live only after it has run beside the current path and missed nothing. Each phase ships on its own and is useful without the next.
Phase
Work
Exit criterion
0. Measure
Add per-stage timers and counters; log the distribution of account margin and of debt size; profile one loaded cycle with a flamegraph
We know core count, dust share, hot-set size and the real hotspot
1. Quick wins
Move the sweep to its own thread; stream results to the queue; add the dust floor; order the batch by margin
Reaction time falls from ~4–5 s to about one cycle
2. Cheaper exact path
Arc<Account>, per-bank price memo, split the accounts lock
Per-account exact cost measured against the 100–160 µs baseline
3. Shadow screen
Build the position table and screener; run beside the old path; log every account the old path flags that the screen did not
Zero misses over several days including a volatile period
4. Cut over
Drive evaluation from the screener; turn the old sweep into the audit thread
Audit finds no account the screen missed
5. Pre-staging
Pre-build transactions for warm accounts; raise wallet inventory; tune the swap gate
Landing latency, not check latency, is the bottleneck
Tests
• Differential test: for a recorded snapshot of accounts and prices, compare h against the exact get_health_components for every account and several random price walks; the error must stay under the trip margin.
• Replay harness: replay a logged Geyser and Hermes stream through both paths and compare which accounts are flagged and when.
• Property checks: every discrete event (account change, config change, stale oracle, group update) must put the affected accounts on the exact tier.
Metrics to keep live
• Stage latencies as histograms: receive → applied, applied → screened, flagged → verdict, verdict → queued, queued → sent.
• Hot-set size per tick and the number of warm accounts.
• Audit mismatches (must be zero) and the worst |h − h_exact| seen.
• State-thread loop time and channel depth, so saturation is visible before it costs a liquidation.
9. Performance budget and what still bounds landing
The check should drop from seconds to about 1–5 ms, after which the data feed and the swap quote set the pace. The figures below are engineering estimates from the data sizes and the measured baseline, not benchmarks; phase 0 and the shadow run replace them with measurements.
Stage
Estimate
Basis
Oracle account → unit_value for affected banks
20–100 µs per bank
One price parse per bank, not per account
Screen pass over moved banks' holders
0.1–0.3 ms
~60k multiply-adds on cache-resident arrays
Exact check of hot accounts
1–3 ms
A few to a few dozen hot accounts at ~0.1–0.5 ms each across the pool; depends on the measured hot-set size
Verdict → queue → worker wake
<0.5 ms
Streaming hand-off, workers already idle
Total, apply to queued
~1–5 ms
Versus ~4–5 s today
What the check does not fix
• Data arrival. Geyser at processed commitment and Hermes deliver a price tens to a couple of hundred milliseconds after it happens. The check cannot start sooner.
• Swap quotes. The sandwich needs a route. DexGate admits 2 quotes at a time, at least 500 ms apart, and each venue can take up to 1 s. Once the check is fast, this is the next bottleneck.
• Wallet-funded plans skip the quote. They go first when they fund at least 10% of the profit. With $50 per inventory token most real liquidations cannot, so raising INVENTORY_TARGET_USD on the big debt tokens moves more of them onto the fast path.
• Pre-staging. For warm accounts, build the liquidation instructions and the lookup-table compile ahead of time, so a trigger only fills in amounts and the price push.
• Slot timing. A bundle still lands only when a leader takes it, about every 400 ms.
10. Open questions and decisions
The design is settled except for six facts to measure and three choices that are yours.
To measure (phase 0)
[ ] How many CPU cores does the Vultr box have? Answered: _vps_setup.sh describes a 2-vCPU box; upgrading is phase 0 (section 15).
[ ] How many of the 60k accounts have under about $20 of debt? This sets how much the dust floor removes.
[ ] What is the distribution of account margin? It sets the hot-set size and therefore the exact-tier cost per tick.
[ ] How many accounts hold a premium-active liability? It decides always-hot versus a drift term.
[ ] What caused the single 18 s cycle in the log? It may be a stall that the redesign would not remove.
[ ] Where do the 100–160 µs per evaluation actually go? A flamegraph of one loaded cycle answers it.
To decide
• Definition of "10 ms": from an update being applied to every account screened, or from the network message arriving to a verdict queued? This design hits the first easily and the second in about 5 ms of compute.
• Starting trip margin θ: 0.1% of gross value is a safe default; a tighter value cuts exact checks but leaves less room for the unmodelled.
• Kamino and JupLend on Geyser: move them off the 60 s poll (section 6), or leave as is for now.
11. Design review: is this optimal?
On detection compute, the screen is close to the practical optimum: once it is in, the check is well under 1% of the time from a price change to a landed bundle. As an overall plan it is not optimal. It has three flaws of its own, and it leaves out the execution side, which becomes the bottleneck the moment detection is fast.
What holds up
• The linear model is sound. In the marginfi source (get_health_components, tag mrgn-0.1.12-rc2), maintenance health sums, per active balance, amount × biased real-time price × weight. The weight depends only on the account's balances and bank config. The variable-borrow premium is added separately, which section 6 already covers.
• Going further on compute is not worth it. A sorted trigger-price index, SIMD or a GPU would cut the screen from ~0.2 ms to microseconds. Data arrival alone is tens to hundreds of milliseconds, so the saving would not change who lands first.
Flaws in the screening design
#
Flaw
Consequence
Fix
1
It flags on health under a margin, not on "a liquidation worth at least MIN_PROFIT"
An account that is unhealthy but not worth liquidating (debt of $5–$19 at the base premium, say) stays hot and gets an exact check on every tick. An account that has only just crossed zero is usually too small to pay, because the stay-unhealthy bound caps the repay at −H ÷ (health gain per dollar).
At each exact check, store the account's best-pair health gain per dollar, debt, collateral, equity and premium. The screen then computes a cheap upper bound on profit, min(debt, collateral ÷ (1+p), equity ÷ p, −h ÷ g) × p, and flags only when it reaches MIN_PROFIT. That gives a trigger price per account, not just a margin.
2
No Geyser server-side group filter
The ingest thread still decodes every marginfi account update from every group before the screen sees anything
Add memcmp group filters to the subscription (offsets to verify against a live node)
3
The exact verdict still runs up to four pricing passes (Newest, Chain, Pushed, and a without-push sizing)
The 0.1–0.5 ms estimate per hot account may be optimistic
Decide once per tick which feeds are behind the chain, and run the Pushed pass only for accounts whose banks use those feeds
The 10 ms target should also be measured from the moment a message is received, not from when it is applied. Otherwise queueing ahead of the state thread is invisible.
12. Execution-side gaps
Once detection takes milliseconds, the execution path decides who lands first, and it has seconds of avoidable delay. Each row below was checked in the code; the effects marked as estimates were not measured.
#
Gap
Evidence in code
Fix
E1
Swap quotes are serialized across the whole process
DexGate in src/rebalancer.rs: 2 concurrent operations, at least 500 ms apart, shared by every worker and the rebalancer; each venue may take up to 1 s (SWAP_VENUE_TIMEOUT). In a burst of 10 sandwiches the last waits at least 4.5 s for its turn (estimate).
Repay from inventory first so most liquidations need no quote; give each venue its own gate sized to its paid rate limit; keep the rebalancer on a separate client so it never holds a liquidation slot; pre-quote collateral → debt for warm accounts
E2
Workers block until a bundle settles
run_worker → try_execute → land_bundle → landed_before_expiry polls every 250 ms until landed or lost. With 8 workers, a burst queues behind bundles that are only being watched.
Send, then hand the signature to an async settlement watcher and free the worker at once; the queue's per-account in-flight guard stays
E3
Every liquidation requests the maximum compute
cu_limit_ix in src/wrappers/liquidator_account.rs is MAX_COMPUTE_UNIT_LIMIT (1.4M CU). The pusher's own comment says requested compute weighs against the tip in the block engine's auction.
Request a measured budget per plan type and route, plus a margin; a wallet-funded plan without a swap is predictable
E4
One Jito endpoint, and 1 request per second without a key
DEFAULT_BUNDLE_ENDPOINT is a single URL; UNAUTHENTICATED_REQUESTS = 1, and status checks use the same budget
Set JITO_API_KEY (20 requests per second in the code); send each bundle to several regional block engines in parallel
E5
Tip is a fixed 8% of profit
TipEstimator::tip_for: profit share between the hourly tip floor and the cap
Raise the share where the account keeps losing races; on a contested $1–$10 liquidation the tip decides the winner, and any tip below the profit still pays
E6
A price push adds 2–3 transactions
PythPusher posts the VAA, verifies it, updates the feeds and closes the account
Worth checking whether the push oracle accepts a single-transaction update; I believe it requires the full encoded VAA, so this may not be improvable
E7
Data arrives from a remote provider at processed commitment
Server and provider region unknown
Put the server in the same region as the Geyser provider and a Jito block engine; race two Geyser providers and two Hermes endpoints; consider Jito ShredStream with a local node for the earliest data
A profit lever, not a speed lever
The 0.5% premium margin and the 0.95 size haircut together cost about 14% of profit at the 5.5% base premium (5.0% taken instead of 5.5%, times 0.95). That is headroom competitors can spend on tips. With the push sized on the exact prices the program reads, both margins could likely be tightened for bundled plans; they exist to avoid the 6068 and 6090 failures, so change them only with the failure rate logged.
13. Revised priority order
This replaces the phase order in section 8: measure first, then config changes, then the execution fixes, with the screener after them because the execution side now costs more time than detection. The phase 3 shadow run and its zero-miss exit criterion still apply to the screener.
Step
Work
Why here
Effort
1
End-to-end timers from message received to bundle landed, plus a log of every lost race: who won, and by how many slots
Tells you whether you lose on detection or on execution before you build anything
1–2 days
2
Config only: set JITO_API_KEY; raise INVENTORY_TARGET_USD on the main debt tokens; move the server next to the Geyser provider and a block engine
No code; inventory size is a capital and price-risk decision for you
Hours
3
Async settlement (E2), streaming results to the queue, the sweep on its own thread, the dust floor
Removes the seconds of waiting in both halves
~1 week
4
Quote path (E1): inventory first, per-venue gates, rebalancer on its own client, pre-quotes for warm accounts
The next bottleneck once workers are free
~1 week
5
Screener with the profit-bound trigger (flaw 1) and Geyser group filters (flaw 2), in shadow mode first
Takes detection from seconds to milliseconds
2–3 weeks
6
Measured CU budgets (E3), regional Jito fan-out (E4), adaptive tips (E5)
Wins more of the auctions you are already in
~1 week
7
Data ingress (E7): second providers, ShredStream
Biggest remaining latency, highest cost
Infrastructure
The step 1 timers decide the order of steps 3–5. If lost races are decided within the same slot, execution and tips matter most. If you are seconds late, detection does.
14. v2 architecture
Four stages joined by channels, one deterministic state engine, and exact evaluation as the only verdict. Detection never waits on evaluation, quotes or settlement, and execution never touches the state engine's tables.
What changed since v1
• Deployed code. The VPS builds github.com/0dotxyz/eva01 main (_vps_setup.sh). The local checkout is 9bb279f (14 Nov 2026); confirm the server matches with git -C ~/eva01 log -1. An outside review read an older fork (nicedreamsbt/eva01), so its findings about a Geyser processor holding a Liquidator mutex do not apply. Here, ingestion already hands off over a channel and execution already runs on queue-fed workers. The remaining coupling is that one loop applies updates, evaluates and sweeps.
• Hardware. _vps_setup.sh describes a 2-vCPU box. Rayon evaluation, 8 workers, the receivers and the rebalancer all share two cores, so upgrading the box is phase 0.
• Adopted from the outside review: an error envelope instead of a fixed margin, versioned candidate states, ranking by expected profit, the dependency index as its own component, and latency measured from network arrival.
The four stages
Stage
Thread
Does
Never does
1. Ingestion
Geyser and Hermes receivers
Decode; filter by group (server-side memcmp); stamp each event with its version (slot, or publish time) and receive time; publish it
Evaluate, or take any state lock
2. State engine
One dedicated thread, pinned to a core
Apply events to the cache and the dependency index; recompute unit values once per batch for the banks that moved; update incremental health and error envelopes
Wait on an exact check, a quote or a send
3. Candidate detector
Same thread, at the end of each batch
Update candidate states; compute the profit upper bound; rank; publish hot candidates
Run an exact evaluation
4. Evaluation and execution
Exact pool, the 8 workers, a settlement watcher
Exact verdict on an immutable snapshot; sizing, build, send; settle asynchronously; report the outcome back
Write to the state engine's tables
Channels. Ingestion to state is never lossy: account and bank events are coalesced by key, keeping the newest version, and nothing is dropped. State to evaluation is a bounded priority queue. When it is full, the lowest-value entry leaves the queue, but its account stays Hot and is promoted again on the next batch. The exact pool reads immutable Arc snapshots the state engine publishes per account, bank and oracle, so it never waits on a lock.
Dependency index (its own component)
Accounts and banks get dense u32 ids so every index is an array, not a hash map.
Index
Maps
Updated when
Oracle → banks
Oracle key to the banks priced by it
A bank's config changes
Bank → position rows
Bank to (account, coefficient, side)
An account or a bank's config changes
Account → rows
Account to its row handles, for replacement
The account changes
Account → anchor
Last exact health, its versions and time
An exact verdict arrives
Key → generation
Every account, bank and oracle to a counter
Every applied update
Incremental health with an error envelope
• Estimate: h_est per account and per pricing view (Newest, Chain), in f64, updated by coef × ΔU per moved bank.
• Envelope: E_max per account. Each applied delta adds its worst-case rounding, |coef × ΔU| × 2⁻⁵². Accounts with a premium-active liability also add premium rate × elapsed time. An exact verdict resets h_est to the exact health and E_max to zero.
• Promotion test: use the lower bound h_est − E_max, never the estimate alone.
• Discrete events (an account change, a bank config change, a stale or unpriceable oracle, a group change) set the affected accounts' envelopes to infinity, which forces an exact check.
• Why f64: the envelope makes it provably conservative and it is the fastest. If an envelope ever grows near the trip level, benchmark fixed-point I80F48 for that path.
Profit-aware detection
At each exact verdict the engine stores the terms RepayBounds::max_repay uses: debt, collateral, equity, the closeout flag, and the smallest positive health gain per dollar across the account's pairs (g). The detector then bounds profit from above:
\text{profit}_{ub} = 0.95\,p_{max}\,\min\!\Big(\text{debt},\ \tfrac{\text{collateral}}{1+p_{max}},\ \tfrac{\text{equity}}{p_{max}},\ \tfrac{-h_{lower}}{g}\Big)
Here p_max is the largest premium the account could earn: up to 0.245 if it is tagged or taggable, otherwise the base premium less the 0.5% margin. The equity term applies only to a solvent account, and the health term only above the closeout threshold and when g > 0. If the bound cannot prove an account uneconomic, the account is promoted. Replay must check the bound: any account whose exact profit exceeds it is a bug.
Candidate state machine
State
Entered when
Behaviour
Normal
Lower-bound health above the warm band
No work
Warm
Within the warm band, or the profit bound is near MIN_PROFIT
Pre-stage instructions; no exact check
Hot
Lower-bound health under zero and the profit bound at least MIN_PROFIT, or envelope infinite
Queued for an exact check
Evaluating
An exact check is running
Further changes set a dirty flag; re-queued when it returns
Qualified
Exact verdict is liquidatable and its versions are still current
Handed to the execution queue
In flight
A bundle or transaction is out
The existing queue rules apply (cooldown, lost race, wait for the landed slot)
Retired
Exact verdict says not liquidatable or not worth it
Re-anchors health; back to Normal or Warm
Every candidate carries its account version and the generations of its banks and oracles. A verdict with older versions than the current state is discarded, and the account is re-promoted if it is still Hot. Invalidation is lazy: entries are checked when taken, not removed from every queue.
Ranking: profit bound first (exact profit once known), then the more negative lower-bound health. A large account that has just crossed beats a deeply underwater account with almost no repayable debt.
15. v2 phase plan and latency targets
Hardware and measurement come first, then decoupling and execution fixes, then the state engine; each phase ships on its own. This replaces the orders in sections 8 and 13.
Phase
Work
Exit criterion
0. Box and baseline
Move to a box with 16+ dedicated cores next to the Geyser provider and a Jito block engine; confirm the deployed commit; set JITO_API_KEY
Same binary, cycle times re-measured on the new box
1. Measure
Timers from network arrival to landed bundle, per stage; log every lost race (winner, slot gap); margin and debt-size histograms
We know whether races are lost on detection or on execution
2. Decouple
Settlement moved to an async watcher (E2); results streamed to the queue; the sweep on its own thread; receivers stamp versions and receive time; dust floor
No stage waits on another's full workload
3. Execution path
Inventory first; a gate per swap venue; rebalancer on its own client; measured CU budgets (E3); regional Jito fan-out (E4)
Quote and send latency measured under a burst
4. State engine
Dependency index; incremental health with error envelopes; batched bank deltas; Arc snapshots; Geyser group filters
Differential test: lower bound never above the exact health
5. Detector
Profit upper bound; candidate state machine with versions; ranking; streaming hand-off
Replay: profit bound never below exact profit
6. Shadow
Run beside the current path on live data and on replays of recorded Geyser and Hermes streams, including a volatile period
Zero misses over several days
7. Cutover
The detector drives evaluation; the old sweep becomes a low-priority full audit
Audit mismatches stay at zero
Adaptive tips (E5) and data-ingress upgrades (E7) follow once phase 1 shows where races are lost.
Latency targets (from network arrival)
These are engineering targets, not measurements. Track p50, p95, p99 and the worst case for each.
Stage
Target
Message received → event published
Under 0.5 ms
Event published → state applied
Under 1 ms
State applied → affected accounts screened
Under 2 ms
Candidate → exact verdict queued
Under 5 ms
Whole detection pipeline
Under 10 ms
Also watch the ingestion channel's depth and the state thread's busy share: a fast screen behind a growing backlog would still be slow.
16. Decisions, open items and what not to build
Most design choices are now settled; the open items are facts to measure and two choices for the owner.
Settled
Decision
Choice
Reason
Verdict
Existing exact code only (size_on_accounts, prepare)
Matches the program; the screen only filters
Number representation
f64 with an error envelope
Provably conservative and fastest; fixed-point only if an envelope nears the trip level
Screen trigger
Lower-bound health under zero and profit upper bound at least MIN_PROFIT
Avoids re-checking unhealthy accounts that cannot pay
Concurrency
One state-engine thread as the only writer; snapshots for readers
No locks on the hot path; deterministic
Stale verdicts
Version checks with lazy invalidation
A result from an old snapshot never overwrites newer state
Ranking
Expected profit, then urgency
Value first, not depth underwater
Latency measurement
From network arrival
Exposes queueing ahead of the engine
Open
[ ] Confirm the server runs 9bb279f (or name the commit it does run).
[ ] Choose the new box: cores, region, and the Geyser provider it sits next to.
[ ] Phase 1 results: where races are lost, the margin distribution, the dust share, and how many accounts hold a premium-active liability.
[ ] Decide the inventory size per debt token: a capital and price-risk call.
[ ] Decide whether to move Kamino and JupLend onto Geyser (section 6).
Not building
• A GPU screen, SIMD before profiling, or a sorted trigger-price index: the screen is already far below data-arrival latency.
• Several independent screeners: one state engine keeps results consistent.
• Re-checking every account on each update, or rebuilding coefficients on price moves: price moves only change unit values.
• Exact checks on warm accounts each tick: they are pre-staged instead.
• Filtering without an error bound: every filter must be provably conservative.