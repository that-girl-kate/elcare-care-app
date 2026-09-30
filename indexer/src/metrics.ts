import client from 'prom-client';
import express from 'express';
import { logger } from './logger.js';
import { requestIdMiddleware } from './api/request-id-middleware.js';

// Re-exported under its old name for backwards compatibility with existing
// call sites/imports. The plain-text console.log implementation that used to
// live here has been replaced by the structured JSON + correlation-ID
// middleware in ./api/request-id-middleware.ts — see that file for behavior.
export { requestIdMiddleware as requestLogger };

// Enable default metrics (CPU, memory, etc.)
client.collectDefaultMetrics();

// Custom Metrics
export const latestLedgerProcessedGauge = new client.Gauge({
  name: 'indexer_latest_ledger_processed',
  help: 'The sequence number of the latest ledger processed by the indexer',
});

export const networkLatestLedgerGauge = new client.Gauge({
  name: 'indexer_network_latest_ledger',
  help: 'The sequence number of the latest ledger on the Stellar network',
});

export const syncLatencyGauge = new client.Gauge({
  name: 'indexer_sync_latency_ledgers',
  help: 'The difference between the latest network ledger and the processed ledger',
});

export const rpcRetryExhaustedCounter = new client.Counter({
  name: 'indexer_rpc_retry_exhausted_total',
  help: 'Total number of times RPC retries were exhausted, indicating sustained failures',
  labelNames: ['operation'],
});

export const decodeErrorsCounter = new client.Counter({
  name: 'indexer_decode_errors_total',
  help: 'Total number of XDR event decode errors encountered during sync',
});

/** Per-event-type decode error counter (labeled). */
export const eventDecodeErrorsCounter = new client.Counter({
  name: 'indexer_decode_errors_by_type_total',
  help: 'Total XDR event decode errors by event type',
  labelNames: ['event_type'],
});

/**
 * Counts events whose `schema_version` is higher than what this indexer
 * build understands (Issue #278). Distinct from `eventDecodeErrorsCounter`:
 * the event decoded structurally fine, it's just a version the indexer
 * hasn't been updated to recognize as safe — a signal that the indexer is
 * behind the deployed contract and needs investigation/upgrade.
 */
export const unsupportedSchemaVersionCounter = new client.Counter({
  name: 'indexer_unsupported_schema_version_total',
  help: 'Total events skipped because their schema_version is not recognized by this indexer build, by event type and version',
  labelNames: ['event_type', 'schema_version'],
});

export const duplicateEventsCounter = new client.Counter({
  name: 'elcarehub_duplicate_events_total',
  help: 'Total number of duplicate on-chain events skipped during idempotent processing',
});

// ── Reentrancy guard monitoring (Issue #204) ──────────────────────────────────

/**
 * Incremented whenever the indexer observes a contract invocation result that
 * contains the ReentrancyGuard error (code 22).  A sustained rate of this
 * counter is a signal that something is probing or exploiting the guard and
 * warrants operator investigation.
 */
export const reentrancyGuardTriggeredTotal = new client.Counter({
  name: 'elcarehub_reentrancy_guard_triggered_total',
  help: 'Total number of times a ReentrancyGuard rejection (error #22) was observed in contract invocation results',
});

export const httpRequestDurationSeconds = new client.Histogram({
  name: 'http_request_duration_seconds',
  help: 'Duration of HTTP requests in seconds',
  labelNames: ['method', 'route', 'status'],
  buckets: [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10],
});

// ── Stall gauge ───────────────────────────────────────────────────────────────

/** Set to 1 when the indexer has stalled (no ledger progress), 0 otherwise. */
export const stalledGauge = new client.Gauge({
  name: 'indexer_stalled',
  help: '1 when the indexer has not advanced within the stall threshold, 0 otherwise',
});

/**
 * Stall event counter, labelled by level: "warning" | "critical" | "fatal".
 *
 * Each counter value reflects the total number of times the stall detector has
 * fired at that severity since the process started.  A dashboard alert on the
 * rate of "fatal" or a sustained rise in "warning" signals degraded sync health.
 */
export const pollerStallTotal = new client.Counter({
  name: 'elcarehub_poller_stall_total',
  help: 'Total number of stall events detected, labelled by severity level',
  labelNames: ['level'],
});

/**
 * Automatic poller restart counter.
 *
 * Incremented each time the stall detector triggers a stopPoller()/startPoller()
 * cycle.  When this counter reaches 3 the process exits with a non-zero code.
 */
export const pollerRestartTotal = new client.Counter({
  name: 'elcarehub_poller_restart_total',
  help: 'Total number of automatic poller restarts attempted by the stall watchdog',
});

// ── Business KPI Metrics ──────────────────────────────────────────────────────

/** Total listings created (labelled by NFT collection kind). */
export const listingsCreatedTotal = new client.Counter({
  name: 'elcarehub_listings_created_total',
  help: 'Total number of listings created, by NFT collection kind',
  labelNames: ['collection_kind'],
});

/** Total sales (labelled by payment token type). */
export const salesTotalCounter = new client.Counter({
  name: 'elcarehub_sales_total',
  help: 'Total number of artwork sales, by token type',
  labelNames: ['token_type'],
});

/** Total auction finalizations. */
export const auctionFinalizationsTotal = new client.Counter({
  name: 'elcarehub_auction_finalizations_total',
  help: 'Total number of auctions finalized',
});

/** Total offers made. */
export const offersMadeTotal = new client.Counter({
  name: 'elcarehub_offers_made_total',
  help: 'Total number of offers submitted',
});

/** Total offers accepted. */
export const offersAcceptedTotal = new client.Counter({
  name: 'elcarehub_offers_accepted_total',
  help: 'Total number of offers accepted',
});

/** Total SSE connections opened (ever). */
export const sseConnectionsTotal = new client.Counter({
  name: 'elcarehub_sse_connections_total',
  help: 'Total SSE connections opened since the indexer started',
});

/** Current number of active listings. */
export const activeListingsGauge = new client.Gauge({
  name: 'elcarehub_active_listings',
  help: 'Current number of active listings in the marketplace',
});

/** Current number of active auctions. */
export const activeAuctionsGauge = new client.Gauge({
  name: 'elcarehub_active_auctions',
  help: 'Current number of active auctions in the marketplace',
});

/** Current number of live SSE connections. */
export const sseActiveConnectionsGauge = new client.Gauge({
  name: 'elcarehub_sse_active_connections',
  help: 'Current number of active SSE client connections',
});

/** Sync lag in ledgers (alias of syncLatencyGauge for business-facing dashboards). */
export const syncLagLedgersGauge = new client.Gauge({
  name: 'elcarehub_sync_lag_ledgers',
  help: 'Number of ledgers the indexer is behind the network tip',
});

// ── Per-endpoint and per-event histograms ─────────────────────────────────────

/** Per-route API request duration (with method, route, status_code labels). */
export const apiRequestDurationHistogram = new client.Histogram({
  name: 'elcarehub_api_request_duration_seconds',
  help: 'HTTP API request duration in seconds, labelled by method, route, and status code',
  labelNames: ['method', 'route', 'status_code'],
  buckets: [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10],
});

/** Per-event-type processing duration. */
export const eventProcessingDurationHistogram = new client.Histogram({
  name: 'elcarehub_event_processing_duration_seconds',
  help: 'Time spent processing each on-chain event, labelled by event_type',
  labelNames: ['event_type'],
  buckets: [0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1],
});

// Structured request logging (JSON, with correlation IDs) lives in
// ./api/request-id-middleware.ts — this file only owns Prometheus metrics.

// Middleware to track HTTP response times
export function metricsMiddleware(req: express.Request, res: express.Response, next: express.NextFunction) {
  const start = process.hrtime();
  
  res.on('finish', () => {
    const duration = process.hrtime(start);
    const durationInSeconds = duration[0] + duration[1] / 1e9;
    
    // Normalize route to avoid high-cardinality issues.
    // When req.route is absent (404s, pre-routing middleware) we use the fixed
    // placeholder 'unmatched' so raw resource IDs never become label values.
    let route = req.baseUrl + (req.route ? req.route.path : 'unmatched');
    if (!route || route === '') {
      route = 'unmatched';
    }

    httpRequestDurationSeconds.labels(
      req.method,
      route,
      res.statusCode.toString()
    ).observe(durationInSeconds);
  });
  
  next();
}

// ── Cache metrics ─────────────────────────────────────────────────────────────

export const cacheHitsTotal = new client.Counter({
  name: 'elcarehub_cache_hits_total',
  help: 'Total cache hits by resource kind',
  labelNames: ['resource'],
});

export const cacheMissesTotal = new client.Counter({
  name: 'elcarehub_cache_misses_total',
  help: 'Total cache misses by resource kind',
  labelNames: ['resource'],
});

export const cacheStaleBypassTotal = new client.Counter({
  name: 'elcarehub_cache_stale_bypass_total',
  help: 'Total stale cache bypasses by resource kind',
  labelNames: ['resource'],
});

export const cacheBypassTotal = new client.Counter({
  name: 'elcarehub_cache_bypass_total',
  help: 'Total cache bypasses (Redis unavailable) by resource kind',
  labelNames: ['resource'],
});

export const cacheInvalidationsTotal = new client.Counter({
  name: 'elcarehub_cache_invalidations_total',
  help: 'Total cache invalidations by resource kind',
  labelNames: ['resource'],
});

export const cacheInvalidationFailuresTotal = new client.Counter({
  name: 'elcarehub_cache_invalidation_failures_total',
  help: 'Total cache invalidation failures by resource kind',
  labelNames: ['resource'],
});

// ── SSE metrics ───────────────────────────────────────────────────────────────

/** Total events delivered via SSE. */
export const sseEventsDeliveredTotal = new client.Counter({
  name: 'elcarehub_sse_events_delivered_total',
  help: 'Total SSE events delivered to clients',
});

/** Total SSE events dropped due to slow clients (queue overflow). */
export const sseEventsDroppedTotal = new client.Counter({
  name: 'elcarehub_sse_events_dropped_total',
  help: 'Total SSE events dropped due to client queue overflow',
});

/** Total SSE replay requests (clients resuming from lastEventId). */
export const sseReplayRequestsTotal = new client.Counter({
  name: 'elcarehub_sse_replay_requests_total',
  help: 'Total SSE replay requests (clients resuming from lastEventId)',
});

/** Total Redis publish failures (degraded fallback mode triggered). */
export const sseRedisPublishFailuresTotal = new client.Counter({
  name: 'elcarehub_sse_redis_publish_failures_total',
  help: 'Total Redis publish failures (degraded fallback mode triggered)',
});

/** Total degraded-fallback events (Redis unavailable). */
export const sseDegradedFallbackTotal = new client.Counter({
  name: 'elcarehub_sse_degraded_fallback_total',
  help: 'Total events delivered via degraded fallback (in-memory ring buffer)',
});

/** Total subscriber reconnects (Redis pub/sub reconnection events). */
export const sseSubscriberReconnectsTotal = new client.Counter({
  name: 'elcarehub_sse_subscriber_reconnects_total',
  help: 'Total subscriber reconnect events (Redis pub/sub reconnection)',
});

// ── Worker lease metrics ───────────────────────────────────────────────────────

export const indexerLeaseAcquisitionsTotal = new client.Counter({
  name: 'elcarehub_lease_acquisitions_total',
  help: 'Total successful lease acquisitions',
  labelNames: ['role'],
});

export const indexerLeaseRenewalsTotal = new client.Counter({
  name: 'elcarehub_lease_renewals_total',
  help: 'Total successful lease renewals',
  labelNames: ['role'],
});

export const indexerLeaseLostTotal = new client.Counter({
  name: 'elcarehub_lease_lost_total',
  help: 'Total times a worker lost its lease',
  labelNames: ['role'],
});

export const indexerLeaseContentionTotal = new client.Counter({
  name: 'elcarehub_lease_contention_total',
  help: 'Total lease acquisition contentions',
  labelNames: ['role'],
});

export const indexerWorkerLeaseGauge = new client.Gauge({
  name: 'elcarehub_worker_lease_held',
  help: '1 when this instance holds the active worker lease, 0 otherwise',
});

// ── Keeper metrics ────────────────────────────────────────────────────────────
//
// entry_point label values: "expire_listing" | "finalize_auction" | "reclaim_offer"
// outcome      label values: "succeeded" | "failed" | "skipped" | "dry_run"

/** Total keeper action attempts, labelled by entry point and final outcome. */
export const keeperActionsTotal = new client.Counter({
  name: 'keeper_actions_total',
  help: 'Total number of keeper maintenance actions attempted, by entry point and outcome',
  labelNames: ['entry_point', 'outcome'],
});

/** Total XLM fees spent (in stroops) by the keeper, labelled by entry point. */
export const keeperFeesSpentStroops = new client.Counter({
  name: 'keeper_fees_spent_stroops_total',
  help: 'Cumulative transaction fees paid by the keeper in stroops, by entry point',
  labelNames: ['entry_point'],
});

/** Number of times the daily fee budget was exhausted, halting the cycle. */
export const keeperBudgetExhaustedTotal = new client.Counter({
  name: 'keeper_budget_exhausted_total',
  help: 'Number of times the keeper halted because the daily fee budget was exhausted',
});

/** Gauge set to 1 when the daily fee budget is currently exhausted, 0 otherwise. */
export const keeperBudgetExhaustedGauge = new client.Gauge({
  name: 'keeper_budget_exhausted',
  help: '1 when the keeper daily fee budget is currently exhausted, 0 otherwise',
});

/** Number of simulation failures (RPC-level, not contract reverts), by entry point. */
export const keeperSimulationFailuresTotal = new client.Counter({
  name: 'keeper_simulation_failures_total',
  help: 'Number of simulateTransaction failures (RPC errors, not contract reverts)',
  labelNames: ['entry_point'],
});

/** Duration of each full keeper sweep cycle in seconds. */
export const keeperCycleDurationSeconds = new client.Histogram({
  name: 'keeper_cycle_duration_seconds',
  help: 'Duration of a complete keeper sweep cycle in seconds',
  buckets: [0.1, 0.5, 1, 5, 10, 30, 60, 120],
});

/** How many candidates were discovered in the last sweep, by type. */
export const keeperCandidatesDiscovered = new client.Gauge({
  name: 'keeper_candidates_discovered',
  help: 'Number of actionable candidates discovered in the most recent sweep, by target type',
  labelNames: ['target_type'],
});

/** Number of fee-bump escalations triggered, by entry point. */
export const keeperFeeBumpsTotal = new client.Counter({
  name: 'keeper_fee_bumps_total',
  help: 'Number of fee-bump resubmissions triggered due to timeout or fee errors',
  labelNames: ['entry_point'],
});

/** Number of storage entries within 50,000 ledgers of TTL expiry (Issue #280). */
export const elcarehubEntriesNearExpiry = new client.Gauge({
  name: 'elcarehub_entries_near_expiry',
  help: 'Number of listings, auctions, and offers within 50,000 ledgers of their TTL expiry',
});

/** CleanupSummaryEvent counts by kind (ttl_extend | lock_cleanup | storage_renewal) — Issue #847. */
export const elcarehubCleanupSummaryTotal = new client.Counter({
  name: 'elcarehub_cleanup_summary_total',
  help: 'CleanupSummaryEvent emissions from marketplace maintenance entry points, by kind',
  labelNames: ['kind'],
});

/** TtlAnomalyEvent counts — ActiveListings / auction index drift (Issue #847). */
export const elcarehubTtlAnomalyTotal = new client.Counter({
  name: 'elcarehub_ttl_anomaly_total',
  help: 'TtlAnomalyEvent emissions from extend_active_ttls when index/state drift is detected',
  labelNames: ['subject'],
});

// ── Backfill / gap-repair metrics ─────────────────────────────────────────────

/** Number of Open LedgerGap rows currently in the DB (set each gap-repair cycle). */
export const openGapsGauge = new client.Gauge({
  name: 'indexer_open_ledger_gaps',
  help: 'Number of LedgerGap rows currently in Open status',
});

/** Total ledgers covered by open gaps (sum of toLedger - fromLedger + 1). */
export const openGapLedgersTotalGauge = new client.Gauge({
  name: 'indexer_open_ledger_gap_ledgers_total',
  help: 'Total number of ledgers covered by all Open LedgerGap rows',
});

/** Total gap rows created, labelled by source. */
export const gapsCreatedTotal = new client.Counter({
  name: 'indexer_ledger_gaps_created_total',
  help: 'Total LedgerGap rows created, by source (rpc_window_skip | reorg | manual)',
  labelNames: ['source'],
});

/** Total BackfillJob outcomes, labelled by terminal status. */
export const backfillJobsTotal = new client.Counter({
  name: 'indexer_backfill_jobs_total',
  help: 'Total BackfillJob completions, by final status (Completed | Failed | Cancelled)',
  labelNames: ['status'],
});

/** Duration of a complete BackfillJob run in seconds. */
export const backfillDurationSeconds = new client.Histogram({
  name: 'indexer_backfill_duration_seconds',
  help: 'Wall-clock duration of a BackfillJob from Running to terminal state',
  buckets: [1, 5, 15, 30, 60, 120, 300, 600, 1800],
});

/** Ledgers processed per backfill batch (useful for sizing BACKFILL_BATCH_SIZE). */
export const backfillBatchLedgers = new client.Histogram({
  name: 'indexer_backfill_batch_ledgers',
  help: 'Number of ledgers processed in each backfill batch',
  buckets: [100, 500, 1000, 2500, 5000, 10000],
});

/** Events inserted per backfill batch. */
export const backfillBatchInserted = new client.Histogram({
  name: 'indexer_backfill_batch_inserted_events',
  help: 'Number of events inserted in each backfill batch',
  buckets: [0, 1, 10, 50, 100, 500, 1000, 5000],
});

/** Number of concurrent advisory-lock contentions (two workers raced for same job). */
export const backfillLockContentions = new client.Counter({
  name: 'indexer_backfill_lock_contentions_total',
  help: 'Number of times a BackfillJob advisory lock was already held by another worker',
});

// ── User-facing transaction error metrics (#417) ──────────────────────────────

/**
 * Counts every user-facing transaction error by category.
 * Increment whenever a write action (listing, purchase, bid, offer, deploy)
 * fails and the error is surfaced to the user. Labels match TxErrorCategory.
 */
export const txSubmissionErrorsTotal = new client.Counter({
  name: 'elcarehub_tx_submission_errors_total',
  help: 'Total user-facing transaction submission errors, by category',
  labelNames: ['category'],
});

// ── Dead-letter metrics (#287) ────────────────────────────────────────────────

/** Total events that failed to parse and were persisted to dead-letter storage. */
export const deadLetterCreatedTotal = new client.Counter({
  name: 'indexer_dead_letter_created_total',
  help: 'Total events persisted to dead-letter storage, by error code',
  labelNames: ['error_code'],
});

/** Current number of Pending (unresolved) dead-letter records. */
export const deadLetterPendingGauge = new client.Gauge({
  name: 'indexer_dead_letter_pending',
  help: 'Current number of dead-letter records in Pending status',
});

/** Age in seconds of the oldest Pending dead-letter record (0 when none). */
export const deadLetterOldestAgeSeconds = new client.Gauge({
  name: 'indexer_dead_letter_oldest_age_seconds',
  help: 'Age in seconds of the oldest unresolved (Pending) dead-letter event',
});

/**
 * Total dead-letter replay attempts, by outcome.
 * outcome label values: "success" | "parse_null" | "parse_error" | "projection_error" | "duplicate"
 */
export const deadLetterReplayAttemptsTotal = new client.Counter({
  name: 'indexer_dead_letter_replay_attempts_total',
  help: 'Total dead-letter replay attempts, by outcome',
  labelNames: ['outcome'],
});

/** Total dead-letter records successfully re-projected into domain tables. */
export const deadLetterReplayProjectedTotal = new client.Counter({
  name: 'indexer_dead_letter_replay_projected_total',
  help: 'Total dead-letter records whose events were successfully re-projected',
});

// ── Snapshot metrics ──────────────────────────────────────────────────────────

/** Total IndexerSnapshot rows written. */
export const snapshotsWrittenTotal = new client.Counter({
  name: 'indexer_snapshots_written_total',
  help: 'Total immutable ledger snapshots written',
});

/** Total snapshot RPC verifications, by result. */
export const snapshotVerificationsTotal = new client.Counter({
  name: 'indexer_snapshot_verifications_total',
  help: 'Total snapshot RPC verifications, by result (match | mismatch | error)',
  labelNames: ['result'],
});

/** Gauge set to 1 when the most-recent snapshot has a hash mismatch, 0 otherwise. */
export const snapshotHashMismatchGauge = new client.Gauge({
  name: 'indexer_snapshot_hash_mismatch',
  help: '1 when the most-recent verified snapshot has a hash mismatch; requires operator action',
});

// ── Reconciliation metrics (#288) ─────────────────────────────────────────────

/** Total field-level discrepancies found per reconciliation run, by model and field. */
export const reconcilerDiscrepanciesTotal = new client.Counter({
  name: 'indexer_reconciler_discrepancies_total',
  help: 'Total discrepancies detected during reconciliation, by model and field',
  labelNames: ['model', 'field'],
});

/** Total deterministic repairs applied (or logged in dry-run), by model. */
export const reconcilerRepairsTotal = new client.Counter({
  name: 'indexer_reconciler_repairs_total',
  help: 'Total repairs applied (or dry-run) by the reconciler, by model',
  labelNames: ['model', 'dry_run'],
});

/** Number of records with detected drift in the last reconciliation run. */
export const reconcilerDriftGauge = new client.Gauge({
  name: 'indexer_reconciler_drift_records',
  help: 'Number of records with detected drift in the most recent reconciliation run',
});

/** Total reconciliation runs completed, labelled by outcome and dry_run mode. */
export const reconcilerRunsTotal = new client.Counter({
  name: 'indexer_reconciler_runs_total',
  help: 'Total reconciliation runs completed, by outcome (ok | error) and dry_run mode',
  labelNames: ['outcome', 'dry_run'],
});

/** Total records skipped during a reconciliation run, by skip reason. */
export const reconcilerSkippedTotal = new client.Counter({
  name: 'indexer_reconciler_skipped_total',
  help: 'Total records skipped during reconciliation, by reason (rpc_error | budget_exhausted | decode_error)',
  labelNames: ['reason'],
});

// ── Financial reconciliation metrics (Issue #XXX) ─────────────────────────────

/** Total financial reconciliation runs completed, by outcome. */
export const financialReconcileRunsTotal = new client.Counter({
  name: 'financial_reconcile_runs_total',
  help: 'Total financial reconciliation runs completed, by outcome (ok | error)',
  labelNames: ['outcome', 'dry_run'],
});

/** Total financial drifts detected, by entity type and severity. */
export const financialDriftsDetectedTotal = new client.Counter({
  name: 'financial_drifts_detected_total',
  help: 'Total financial drifts detected during reconciliation, by entity type and severity',
  labelNames: ['entity_type', 'severity'],
});

/** Total financial alerts raised, by entity type. */
export const financialAlertsRaisedTotal = new client.Counter({
  name: 'financial_alerts_raised_total',
  help: 'Total financial alerts raised during reconciliation, by entity type',
  labelNames: ['entity_type'],
});

/** Current number of unresolved financial drifts, by severity. */
export const financialDriftsOpenGauge = new client.Gauge({
  name: 'financial_drifts_open',
  help: 'Current number of unresolved financial drifts, by severity',
  labelNames: ['severity'],
});

/** Protocol-level aggregate totals for reconciliation verification. */
export const financialProtocolAggregateGauge = new client.Gauge({
  name: 'financial_protocol_aggregate',
  help: 'Protocol-level aggregate totals for financial reconciliation verification',
  labelNames: ['metric'], // protocol_fees, royalties, sales, refunds
});

/** Per-token aggregate totals for reconciliation verification. */
export const financialTokenAggregateGauge = new client.Gauge({
  name: 'financial_token_aggregate',
  help: 'Per-token aggregate totals for financial reconciliation verification',
  labelNames: ['token', 'metric'],
});

/** Per-collection aggregate totals for reconciliation verification. */
export const financialCollectionAggregateGauge = new client.Gauge({
  name: 'financial_collection_aggregate',
  help: 'Per-collection aggregate totals for financial reconciliation verification',
  labelNames: ['collection', 'metric'],
});

/** Per-ledger aggregate totals for reconciliation verification. */
export const financialLedgerAggregateGauge = new client.Gauge({
  name: 'financial_ledger_aggregate',
  help: 'Current aggregate totals for financial reconciliation verification (ledger_sequence logged, not labeled)',
  labelNames: ['metric'],
});

/** Duration of financial reconciliation runs in seconds. */
export const financialReconcileDurationSeconds = new client.Histogram({
  name: 'financial_reconcile_duration_seconds',
  help: 'Duration of financial reconciliation runs in seconds',
  buckets: [1, 5, 10, 30, 60, 120, 300],
});

/** Age in seconds of the oldest unresolved financial drift. */
export const financialDriftOldestAgeSeconds = new client.Gauge({
  name: 'financial_drift_oldest_age_seconds',
  help: 'Age in seconds of the oldest unresolved financial drift (0 when none)',
});

// ── RPC pagination metrics (Issue #487) ──────────────────────────────────────

/** Total RPC event pages fetched during ingestion. */
export const rpcPagesFetchedTotal = new client.Counter({
  name: 'indexer_rpc_pages_fetched_total',
  help: 'Total RPC event pages fetched during ingestion',
});

/** Total times the poller was rate-limited (HTTP 429) by the RPC provider. */
export const rpcRateLimitedTotal = new client.Counter({
  name: 'indexer_rpc_rate_limited_total',
  help: 'Total rate-limit (429) responses received from the RPC provider',
});

/** Current adaptive page size being used for RPC event queries. */
export const rpcAdaptivePageSizeGauge = new client.Gauge({
  name: 'indexer_rpc_adaptive_page_size',
  help: 'Current adaptive page size used for RPC getEvents queries',
});

// ── Abuse / anomaly detection metrics (Issue #539) ────────────────────────────
//
// route_family: 'search' | 'sse' | 'wallet-activity' | 'tx-lookup'
// key_type:     'wallet' | 'ip_hash' — the label is always the KIND of key,
//               never the raw wallet address or IP itself, so metric labels
//               never become a long-lived per-user identity log. A wallet
//               address is a public blockchain identifier, not verified
//               identity — see indexer/src/api/abuse-detection.ts.

/** Total requests rejected for exceeding a per-route-family abuse quota. */
export const abuseQuotaExceededTotal = new client.Counter({
  name: 'elcarehub_abuse_quota_exceeded_total',
  help: 'Total requests rejected for exceeding a per-route-family abuse quota, by route family and key type',
  labelNames: ['route_family', 'key_type'],
});

/** Total abuse anomaly signals detected (quota breaches, blocklist hits, etc). */
export const abuseAnomalyDetectedTotal = new client.Counter({
  name: 'elcarehub_abuse_anomaly_detected_total',
  help: 'Total abuse anomaly signals detected, by route family, key type, and reason',
  labelNames: ['route_family', 'key_type', 'reason'],
});

/** Total requests rejected because the key was on the temporary abuse blocklist. */
export const abuseBlockedRequestsTotal = new client.Counter({
  name: 'elcarehub_abuse_blocked_requests_total',
  help: 'Total requests rejected because the key was on the temporary abuse blocklist, by route family and key type',
  labelNames: ['route_family', 'key_type'],
});

/** Current number of keys (wallet or IP hash) on the temporary abuse blocklist. */
export const abuseBlocklistActiveGauge = new client.Gauge({
  name: 'elcarehub_abuse_blocklist_active',
  help: 'Current number of keys (wallet or IP hash) on the temporary abuse blocklist',
});

/** Total times abuse detection failed open because Redis was unavailable. */
export const abuseDetectionRedisFailureTotal = new client.Counter({
  name: 'elcarehub_abuse_detection_redis_failures_total',
  help: 'Total times abuse detection failed open (request allowed) because Redis was unavailable, by operation',
  labelNames: ['operation'],
});

// ── Expose metrics handler ────────────────────────────────────────────────────

export async function handleMetrics(req: express.Request, res: express.Response) {
  try {
    res.set('Content-Type', client.register.contentType);
    res.end(await client.register.metrics());
  } catch (err) {
    logger.error('Failed to retrieve metrics', { err });
    res.status(500).end('Failed to retrieve metrics');
  }
}

// ── Autoscaling-friendly metrics (normalized gauges with bounded labels) ─────

/**
 * Normalized lag behind network tip (ledgers).
 * Scale-out signal: increasing lag over time indicates need for more indexer instances.
 * Recovery signal: sudden spike followed by decline indicates gap repair or re-org recovery.
 */
export const ledgerLagGauge = new client.Gauge({
  name: 'indexer_ledger_lag',
  help: 'Current number of ledgers the indexer is behind the network tip',
});

/**
 * Number of unresolved LedgerGap rows.
 * Scale-out signal: stable/low count is healthy; gaps fill naturally as indexer catches up.
 * Recovery signal: sudden increase indicates chain re-org or RPC window skip; requires operator review.
 */
export const openGapsCountGauge = new client.Gauge({
  name: 'indexer_open_gaps_count',
  help: 'Current number of unresolved LedgerGap rows',
});

/**
 * Total ledgers covered by open gaps.
 * Scale-out signal: not a direct scaling signal (gaps are fixed sequentially).
 * Recovery signal: large values indicate significant catch-up work needed.
 */
export const openGapsLedgersTotalGauge = new client.Gauge({
  name: 'indexer_open_gaps_ledgers_total',
  help: 'Total number of ledgers covered by all open LedgerGap rows',
});

/**
 * RPC call latency histogram (seconds).
 * Scale-out signal: sustained high latency may indicate need for more indexer instances to spread load.
 * Recovery signal: latency spikes during gap repair/recovery are expected and transient.
 */
export const rpcLatencySeconds = new client.Histogram({
  name: 'indexer_rpc_latency_seconds',
  help: 'RPC call latency in seconds (histogram for p50/p95/p99)',
  buckets: [0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30],
});

/**
 * Current number of connections in use from the database connection pool.
 * Scale-out signal: pool saturation (接近 connection_limit) indicates need for more indexer instances.
 * Recovery signal: not a direct scaling signal, but sustained saturation slows all operations.
 */
export const dbPoolConnectionsUsedGauge = new client.Gauge({
  name: 'indexer_db_pool_connections_used',
  help: 'Current number of connections in use from the read DB pool',
});

/**
 * Current number of active SSE client connections.
 * Scale-out signal: this is per-instance; for Kubernetes HPA, use the ratio to target connections per pod.
 */
export const sseConnectionsGauge = new client.Gauge({
  name: 'indexer_sse_connections',
  help: 'Current number of active SSE client connections',
});

/**
 * Current number of pending jobs in the metadata (IPFS) queue.
 * Scale-out signal: sustained backlog indicates need for more indexer instances.
 * Recovery signal: transient spikes during gap repair or batch backfill are expected.
 */
export const metadataQueueDepthGauge = new client.Gauge({
  name: 'indexer_metadata_queue_depth',
  help: 'Current number of pending jobs in the metadata enrichment queue',
});

/**
 * Metadata queue depth broken down by priority tier (high, normal, low).
 * Scale-out signal: high-priority backlog indicates real-time content is backing up.
 * Recovery signal: low-priority backlog growing during gap repair is expected.
 */
export const metadataQueueDepthByPriorityGauge = new client.Gauge({
  name: 'indexer_metadata_queue_depth_by_priority',
  help: 'Metadata queue depth by priority tier (high, normal, low)',
  labelNames: ['priority'],
});
