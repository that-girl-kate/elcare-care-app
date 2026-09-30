/**
 * scripts/ttl-keeper.ts — Marketplace TTL sweep keeper (Issue #847)
 *
 * Periodically calls `extend_active_ttls` until the on-chain TtlSweepProgress
 * cursor wraps back to (phase=0, cursor=0), indicating a full sweep completed.
 *
 * Environment:
 *   RPC_URL              Soroban RPC endpoint (required)
 *   CONTRACT_ID          Marketplace contract id (required)
 *   KEEPER_PRIVATE_KEY   Secret key with ProtocolConfig role (required)
 *   NETWORK_PASSPHRASE   Defaults to Test SDF Network ; September 2015
 *   MAX_ITEMS_PER_CALL   Default 200 (contract hard-caps at MAX_MAINTENANCE_ITEMS=100)
 *   SWEEP_INTERVAL_HOURS Informational; used when run under cron (default 24)
 *
 * Usage:
 *   npx tsx scripts/ttl-keeper.ts
 */

import {
  Account,
  BASE_FEE,
  Contract,
  Keypair,
  Networks,
  TransactionBuilder,
  nativeToScVal,
  rpc,
  xdr,
} from '@stellar/stellar-sdk';

const RPC_URL = requiredEnv('RPC_URL');
const CONTRACT_ID = requiredEnv('CONTRACT_ID');
const KEEPER_PRIVATE_KEY = requiredEnv('KEEPER_PRIVATE_KEY');
const NETWORK_PASSPHRASE =
  process.env.NETWORK_PASSPHRASE ?? Networks.TESTNET;
const MAX_ITEMS_PER_CALL = Number(process.env.MAX_ITEMS_PER_CALL ?? '200');
const SWEEP_INTERVAL_HOURS = Number(process.env.SWEEP_INTERVAL_HOURS ?? '24');

const BACKOFF_MS = 30_000;
/** DataKey::TtlSweepState discriminant — must match marketplace storage.rs. */
const TTL_SWEEP_STATE_NAME = 'TtlSweepState';

function requiredEnv(name: string): string {
  const v = process.env[name];
  if (!v) {
    throw new Error(`Missing required env var ${name}`);
  }
  return v;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

interface TtlSweepProgress {
  phase: number;
  cursor: bigint;
}

function buildTtlSweepStateKey(contractId: string): xdr.LedgerKey {
  // Instance/persistent contract data key for the unit variant DataKey::TtlSweepState.
  const keyScVal = xdr.ScVal.scvVec([xdr.ScVal.scvSymbol(TTL_SWEEP_STATE_NAME)]);
  return xdr.LedgerKey.contractData(
    new xdr.LedgerKeyContractData({
      contract: Contract.fromString(contractId).address().toScAddress(),
      key: keyScVal,
      durability: xdr.ContractDataDurability.persistent(),
    }),
  );
}

function parseTtlSweepProgress(entry: xdr.LedgerEntry | undefined): TtlSweepProgress {
  if (!entry) {
    return { phase: 0, cursor: 0n };
  }
  const data = entry.data().contractData().val();
  // TtlSweepProgress is a struct { phase: u32, cursor: u64 }
  if (data.switch().name !== 'scvMap') {
    // Some encodings use scvVec for structs; handle both.
    if (data.switch().name === 'scvVec') {
      const vec = data.vec() ?? [];
      const phase = Number(vec[0]?.u32?.() ?? 0);
      const cursor = BigInt(vec[1]?.u64?.().toString() ?? '0');
      return { phase, cursor };
    }
    return { phase: 0, cursor: 0n };
  }
  const map = data.map() ?? [];
  let phase = 0;
  let cursor = 0n;
  for (const entry of map) {
    const name = entry.key().sym()?.toString();
    if (name === 'phase') phase = Number(entry.val().u32());
    if (name === 'cursor') cursor = BigInt(entry.val().u64().toString());
  }
  return { phase, cursor };
}

async function readSweepProgress(server: rpc.Server): Promise<TtlSweepProgress> {
  const key = buildTtlSweepStateKey(CONTRACT_ID);
  const res = await server.getLedgerEntries(key);
  const entry = res.entries?.[0]?.val;
  return parseTtlSweepProgress(entry);
}

function extractCleanupSummary(events: rpc.Api.GetTransactionResponse['resultMetaXdr']): {
  kind?: string;
  items_processed?: number;
} {
  // Best-effort: callers also rely on return value of extend_active_ttls.
  void events;
  return {};
}

async function callExtendActiveTtls(
  server: rpc.Server,
  source: Keypair,
  maxItems: number,
): Promise<{ itemsProcessed: number; ttlAnomalies: number }> {
  const account = await server.getAccount(source.publicKey());
  const contract = new Contract(CONTRACT_ID);

  const tx = new TransactionBuilder(account as Account, {
    fee: BASE_FEE,
    networkPassphrase: NETWORK_PASSPHRASE,
  })
    .addOperation(
      contract.call(
        'extend_active_ttls',
        nativeToScVal(source.publicKey(), { type: 'address' }),
        nativeToScVal(maxItems, { type: 'u32' }),
      ),
    )
    .setTimeout(180)
    .build();

  const prepared = await server.prepareTransaction(tx);
  prepared.sign(source);

  const send = await server.sendTransaction(prepared);
  if (send.status === 'ERROR') {
    throw new Error(`sendTransaction failed: ${JSON.stringify(send)}`);
  }

  let status = await server.getTransaction(send.hash);
  const deadline = Date.now() + 120_000;
  while (
    status.status === rpc.Api.GetTransactionStatus.NOT_FOUND &&
    Date.now() < deadline
  ) {
    await sleep(2000);
    status = await server.getTransaction(send.hash);
  }

  if (status.status !== rpc.Api.GetTransactionStatus.SUCCESS) {
    throw new Error(`extend_active_ttls tx ${send.hash} status=${status.status}`);
  }

  let itemsProcessed = 0;
  let ttlAnomalies = 0;
  try {
    const meta = status.resultMetaXdr;
    // Return value is u32 items_processed.
    const ret = status.returnValue;
    if (ret) {
      itemsProcessed = Number(ret.u32?.() ?? 0);
    }
    void meta;
    void extractCleanupSummary;
  } catch {
    // Keep itemsProcessed at 0 if decoding fails; cursor delta still drives the loop.
  }

  console.log(
    JSON.stringify({
      event: 'cleanup_summary',
      kind: 'ttl_extend',
      items_processed: itemsProcessed,
      ttl_anomalies: ttlAnomalies,
      tx: send.hash,
    }),
  );

  return { itemsProcessed, ttlAnomalies };
}

async function sweepOnce(): Promise<void> {
  const server = new rpc.Server(RPC_URL, { allowHttp: RPC_URL.startsWith('http://') });
  const keeper = Keypair.fromSecret(KEEPER_PRIVATE_KEY);
  const started = Date.now();

  console.log(
    JSON.stringify({
      event: 'ttl_keeper_start',
      contract: CONTRACT_ID,
      max_items_per_call: MAX_ITEMS_PER_CALL,
      sweep_interval_hours: SWEEP_INTERVAL_HOURS,
      keeper: keeper.publicKey(),
    }),
  );

  let previous = await readSweepProgress(server);
  let totalProcessed = 0;
  let rounds = 0;
  const startPhase = previous.phase;
  const startCursor = previous.cursor;
  let wrapped = false;

  while (!wrapped) {
    rounds += 1;
    const remainingEstimate = Math.max(1, MAX_ITEMS_PER_CALL);
    const maxItems = Math.min(MAX_ITEMS_PER_CALL, remainingEstimate);

    const { itemsProcessed } = await callExtendActiveTtls(server, keeper, maxItems);
    totalProcessed += itemsProcessed;

    const next = await readSweepProgress(server);

    if (
      itemsProcessed === 0 &&
      !(next.phase === 0 && next.cursor === 0n) &&
      (next.phase !== previous.phase || next.cursor !== previous.cursor)
    ) {
      // Cursor moved without reporting items — continue.
    } else if (
      itemsProcessed === 0 &&
      next.phase === previous.phase &&
      next.cursor === previous.cursor &&
      !(next.phase === 0 && next.cursor === 0n && rounds > 1)
    ) {
      console.warn(
        JSON.stringify({
          event: 'ttl_keeper_stall',
          message: 'zero items processed and cursor unchanged; backing off 30s',
          phase: next.phase,
          cursor: next.cursor.toString(),
        }),
      );
      await sleep(BACKOFF_MS);
    }

    // Full sweep completed when we wrap back to phase 0 / cursor 0 after making progress,
    // or when a call finds nothing to do on an empty marketplace.
    if (next.phase === 0 && next.cursor === 0n) {
      if (rounds > 1 || (startPhase === 0 && startCursor === 0n && itemsProcessed === 0)) {
        wrapped = true;
      } else if (itemsProcessed > 0 && previous.phase === 1) {
        wrapped = true;
      }
    }

    // Safety: also stop if we completed phase 1 wrap in a single observation.
    if (
      previous.phase === 1 &&
      next.phase === 0 &&
      next.cursor === 0n
    ) {
      wrapped = true;
    }

    previous = next;

    // Bound runaway loops on huge deployments.
    if (rounds > 10_000) {
      throw new Error('ttl-keeper exceeded 10000 rounds without wrapping');
    }
  }

  console.log(
    JSON.stringify({
      event: 'ttl_keeper_complete',
      duration_ms: Date.now() - started,
      rounds,
      items_processed: totalProcessed,
      final_phase: previous.phase,
      final_cursor: previous.cursor.toString(),
    }),
  );
}

sweepOnce().catch((err) => {
  console.error(JSON.stringify({ event: 'ttl_keeper_error', error: String(err) }));
  process.exit(1);
});
