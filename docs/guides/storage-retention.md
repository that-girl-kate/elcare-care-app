# Storage Retention & TTL Maintenance (Marketplace Contract)

This guide explains how `contracts/soroban-marketplace` manages the lifetime of
its persistent storage: what is kept forever, what gets cleaned up, how that
cleanup stays bounded on Soroban, and how the indexer/operators should
interpret the events the maintenance entry points emit. It accompanies
[Issue #280](https://github.com/Elcare-care/elcare-care-app/issues/280) and the
retention classification comment block at the top of
[`contracts/soroban-marketplace/src/storage.rs`](file:///Users/sam/Desktop/Grantfox/elcare-care-app/contracts/soroban-marketplace/src/storage.rs).

---

## 1. Why this exists

Soroban persistent-storage entries have a **TTL** (time-to-live, measured in
ledgers). Once an entry's TTL lapses it is *archived* — it is not gone
forever (it can be restored), but it is no longer directly readable until
someone pays to restore it. Every read/write this contract performs already
re-extends the TTL of the entry it touches (`storage::bump_entry_ttl`), so a
listing/auction that people actively browse, bid on, or buy never expires by
accident.

The risk this guide addresses is the opposite case: a listing or auction that
is still `Active` but that nobody has read or interacted with for a long
time. Its TTL can lapse **invisibly** — from the contract's point of view
nothing "failed", but the record becomes unexpectedly unavailable to the
indexer or the frontend. Separately, long-lived marketplace history
(terminal listings, auctions, offers) accumulates forever, and an unbounded
sweep over that history would be unsafe on Soroban (every contract
invocation has a hard compute/read budget).

## 2. Retention classification

Every `DataKey` variant falls into exactly one of three classes (see the
full per-key breakdown in the doc comment at the top of `storage.rs`):

| Class | Meaning | Examples |
|---|---|---|
| **Active** | Must never be cleaned up while the record is live. | `Listing`/`Auction` while status is `Active`, `Offer` while `Pending`, the `ActiveListings` index and its position keys, a listing's `ListingPendingOffers` set. |
| **Recoverable** | Safe to clear once terminal/stale; already self-cleaning or covered by an existing bounded entry point today. | `ListingLock`/`AuctionLock` (temporary storage, self-expiring), legacy pre-1.1.0 monolithic indices (cleared by `migrate`/`migrate_step`), emptied index pages, cleared pending-offer sets, escrow records (cleared on release). |
| **Archival** | Retained indefinitely on-chain for provenance/dispute resolution; **never actively deleted** by contract code. | `Listing`/`Auction`/`Offer` records themselves, in *any* status — including terminal (`Sold`, `Cancelled`, `Finalized`, `Accepted`, `Rejected`, `Withdrawn`). |

**The contract never hard-deletes a listing, auction, or offer record**, no
matter how old or how terminal its status is. Deleting historical
marketplace records would destroy the provenance trail an indexer, a
dispute, or a future audit needs. Instead:

- While a record is Active/Pending, the maintenance sweep (below) keeps
  re-extending its TTL so it can never expire invisibly.
- Once a record reaches a terminal status, the sweep simply **stops
  touching it**. Its TTL is left to lapse naturally and Soroban's own
  archival mechanism takes over — the data still exists and is restorable,
  it is just no longer "hot" storage.

This means the indexer should treat on-chain reads of very old terminal
records as **best-effort**: if a `get_listing`/`get_auction`/`get_offer` call
for an old terminal id ever fails because the entry has been archived, that
is expected, not a bug — the indexer's own database (populated from the
event stream as it happened) is the durable historical record; the
contract's copy of terminal history is retained for as long as is
Soroban-economical but is not guaranteed to be perpetually "hot"-readable.

## 3. Bounded maintenance entry points

Both are admin-only (`admin.require_auth()` + comparison against the stored
admin, mirroring every other admin entry point in `contract.rs`) and **never
delete or otherwise touch an Active listing, an Active auction, or a
Pending offer** — they only ever add TTL to those, or clear unrelated
transient lock state.

### `extend_active_ttls(env, admin, max_items) -> u32`

A bounded, resumable TTL-refresh sweep over the live record set:

- Phase 0 walks the `ActiveListings` paged index; phase 1 walks the
  sequential auction id space (`1..=AuctionCount`); once phase 1 finishes it
  wraps back to phase 0. This is intentional and different from
  `migrate_step`: TTL upkeep is an ongoing operational task, not a one-shot
  drain, so as long as at least one listing or auction is Active, calling it
  again will always find something to refresh.
- Processes at most `max_items` records per call, **hard-capped at
  `MAX_MAINTENANCE_ITEMS` (100)** regardless of what the caller passes, so a
  single transaction's read/compute footprint stays bounded no matter what
  is requested.
- Persists its cursor (`TtlSweepProgress`) so a later call resumes exactly
  where the previous one stopped.
- Skips (does nothing to) any record that has reached a terminal status —
  this is the "let it archive" half of the retention policy described above.
- If it finds an id still listed in the `ActiveListings` index, or within
  the auction id space, whose record is missing or whose status no longer
  matches what the index/id-space implied, it emits `TtlAnomalyEvent`
  instead of silently doing nothing. That is a signal of index/state drift
  worth investigating — it should not happen in normal operation.

Recommended operation: an off-chain keeper/cron invokes this periodically
(e.g. every few hours) with a modest `max_items` (10–50). It is safe to call
more or less often — nothing bad happens if it is skipped for a while, since
`bump_entry_ttl` on every organic read already covers the common case; this
sweep is a backstop for the "no one has looked at this listing in months"
scenario.

### `cleanup_expired_locks(env, admin, listing_ids, auction_ids) -> u32`

`ListingLock`/`AuctionLock` reentrancy guards live in Soroban's *temporary*
storage with a short TTL (`REENTRANCY_LOCK_TTL` = 100 ledgers) and are
already explicitly released on every normal exit path of every function that
acquires one; a panic mid-call rolls back the whole transaction including
the lock write, so a lock can never actually leak under normal operation —
Soroban's own temporary-storage expiry would reclaim it regardless. This
entry point is an **operator-triggered safety valve**, not a routine sweep:
if an operator notices (off-chain) that a specific listing/auction id
appears stuck, they can pass its id here to force-clear the lock. It is a
no-op (and safe to retry) for any id whose lock is already absent — calling
it again with ids that are already clear just returns `0`.

Both `listing_ids` and `auction_ids` are capped at a combined
`MAX_MAINTENANCE_ITEMS` (100); ids past the cap are silently ignored rather
than reverting the whole call.

## 4. Events to watch

| Event | Emitted by | Meaning |
|---|---|---|
| `cleanup_summary` (`CleanupSummaryEvent { kind, items_processed, ledger_sequence }`) | Both maintenance entry points, once per call | `kind` is `"ttl_extend"` or `"lock_cleanup"`; `items_processed` is how much work actually happened this call. A keeper can log this to confirm the sweep is running and making progress. |
| `ttl_anomaly` (`TtlAnomalyEvent { subject, id, ledger_sequence }`) | `extend_active_ttls`, only when it finds index/state drift | `subject` is `"listing"` or `"auction"`. This should be rare; if the indexer sees it repeatedly for the same id, treat it as a bug report — the on-chain index says the id is live but the record disagrees. |

Neither event indicates data loss — `extend_active_ttls` never deletes
anything, it only chooses whether to renew TTL, and its anomaly signal is
purely diagnostic.

## 5. What the indexer should retain regardless

Because the contract's own copy of terminal history is allowed to go cold,
the indexer's database (built from the event stream, see
[`docs/guides/indexer-ingestion.md`](file:///Users/sam/Desktop/Grantfox/elcare-care-app/docs/guides/indexer-ingestion.md))
is the durable source of historical truth for the frontend and for disputes —
not a live contract read. The indexer should continue to persist every
`listing_created` / `artwork_sold` / `listing_cancelled` / `auction_created`
/ `auction_resolved` / `offer_*` event exactly as it does today; nothing
about this change affects the event stream itself, only how long the
contract's own copy of the underlying record stays "hot".

---

## 6. TTL keeper operation (Issue #847)

Soroban persistent entries expire unless refreshed. The marketplace exposes
`extend_active_ttls(admin, max_items)` (ProtocolConfig role) which walks the
`ActiveListings` index and auction id space, bumping TTLs and emitting:

| Event | Meaning |
|---|---|
| `cleanup_summary` with `kind = ttl_extend` | One sweep call finished; `items_processed` is the count refreshed |
| `ttl_anomaly` | Index/state drift (active index points at a missing/non-Active record) |

### Recommended schedule

Call the keeper at least daily (far inside the ~10-day `LEDGER_TTL_THRESHOLD`
window). A practical cron:

```cron
0 */12 * * * cd /opt/elcare && RPC_URL=... CONTRACT_ID=... KEEPER_PRIVATE_KEY=... npx tsx scripts/ttl-keeper.ts
```

Or via npm: `npm run ttl:keeper` (same env vars required).

### Manual / emergency sweep

```bash
export RPC_URL=https://soroban-testnet.stellar.org
export CONTRACT_ID=C...
export KEEPER_PRIVATE_KEY=S...
export MAX_ITEMS_PER_CALL=200   # contract hard-caps at 100 per call
npx tsx scripts/ttl-keeper.ts
```

The script loops until `TtlSweepProgress` wraps to phase 0 / cursor 0.
If a call processes zero items while the cursor has not wrapped, it backs off
30 seconds and retries.

### Grafana alerts

| Alert file | Condition |
|---|---|
| `.github/grafana-alerts/ttl-sweep-staleness.json` | No `cleanup_summary{kind="ttl_extend"}` in 72h |
| `.github/grafana-alerts/ttl-anomaly-detected.json` | Any `ttl_anomaly` in 5m |

Indexer counters: `elcarehub_cleanup_summary_total{kind}` and
`elcarehub_ttl_anomaly_total{subject}`.

### Restoring an archived record

If a listing/auction TTL lapses, Soroban archives the entry. To recover:

1. Identify the contract data key (e.g. `DataKey::Listing(id)` / `DataKey::Auction(id)`).
2. Use the Stellar CLI / RPC `restore` footprint for that ledger key so the entry
   becomes live again (pay the restore fee).
3. Immediately run `scripts/ttl-keeper.ts` (or call `extend_active_ttls` /
   `renew_storage`) so the restored entry is re-bumped before the next archival.
4. Prefer the indexer's DB for historical reads of terminal records; on-chain
   restore is for Active records that were unexpectedly archived.
