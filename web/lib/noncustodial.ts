// Non-custodial execution from the browser: the server prepares, the USER's wallet signs (Derive)
// or sends the transaction (Thetanuts, CallPut), the server checks and submits. No key leaves the
// wallet; nothing is sent to a venue before the user has approved every step in the wallet.

import type { Dec } from "./types.ts";
import {
  allowance,
  decodeApprove,
  ensureChain,
  personalSign,
  sendTransaction,
  signTypedData,
  usdc,
  waitReceipt,
  WalletError,
  type Eip1193,
} from "./wallet.ts";

export interface PreparedTransaction {
  chain_id: number;
  to: string;
  data: string;
  value?: string;
  num_contracts?: string;
  approval?: { to: string; data: string; spender?: string; note?: string };
  note?: string;
}

export interface PreparedLeg {
  order_id: string;
  leg_index: number;
  platform: string;
  native_id: string;
  side: string;
  qty_native: Dec;
  limit_price_native: Dec;
  max_fee_per_unit: Dec | null;
  typed_data: { domain?: { chainId?: number | string; verifyingContract?: string }; message?: Record<string, unknown> } | null;
  digest: string;
  signature_expires_at: string;
  body_template?: string;
  transaction?: PreparedTransaction;
}

export interface PreparedExecution {
  route_id: string;
  accounts: Record<string, unknown>;
  legs: PreparedLeg[];
  auth_instructions: string;
  expires_at: string;
}

export interface SignedSubmission {
  route_id: string;
  auth_timestamp_ms?: number;
  auth_signature?: string;
  signatures: { order_id: string; signature?: string; tx_hash?: string }[];
}

/** Venues whose whole flow a browser wallet can do on its own (no API secret, no Starknet). */
export const BROWSER_VENUES = ["derive", "thetanuts", "callput"] as const;
export const isBrowserVenue = (p: string) => (BROWSER_VENUES as readonly string[]).includes(p);
/** Venues where the user's wallet sends an irrevocable transaction (vs signing an order). */
export const isTxVenue = (p: string) => p === "thetanuts" || p === "callput";

/** The account object `/v1/noncustodial/prepare` expects for each venue of the route. */
export function accountsFor(platforms: string[], address: string, deriveSubaccount: number | null): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const p of new Set(platforms)) {
    if (p === "derive") {
      if (deriveSubaccount === null) throw new WalletError("Missing Derive subaccount");
      out[p] = { owner: address, subaccount_id: deriveSubaccount };
    } else if (isTxVenue(p)) {
      out[p] = { address };
    } else {
      throw new WalletError(`${p}: browser signing not available (use the SDK)`);
    }
  }
  return out;
}

/**
 * What the page agrees to sign or send, whatever the server returns (defence in depth: a
 * compromised or modified backend cannot make the wallet approve an unknown spender, call an
 * unknown contract or send ETH beyond the CallPut execution fee).
 */
const DERIVE_MATCHING = "0xeb8d770ec18db98db922e9d83260a585b9f0dead";
const DERIVE_CHAINS = [1, 11155111];
const BASE = 8453;
const BASE_USDC = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
const TX_TARGETS: Record<string, { to: string; spender: string }> = {
  thetanuts: { to: "0x1bdff855d6811728acadc00989e79143a2bdfded", spender: "0x1bdff855d6811728acadc00989e79143a2bdfded" },
  callput: { to: "0x83b04701b227b045cbbaf921377137ff595a54af", spender: "0xfc61ba50ae7b9c4260c9f04631ff28d5a2fa4eb2" },
};
/** CallPut keeper execution fee ceiling (0.001 ETH); every other transaction must carry no ETH. */
const MAX_VALUE_WEI: Record<string, bigint> = { thetanuts: BigInt(0), callput: BigInt(10) ** BigInt(15) };

function checkTransaction(l: PreparedLeg, tx: PreparedTransaction, maxUsdc6: bigint): void {
  const t = TX_TARGETS[l.platform];
  const bad = (why: string) => new WalletError(`Leg ${l.leg_index + 1} refused: ${why}`);
  if (!t || Number(tx.chain_id) !== BASE) throw bad("unexpected network");
  if (tx.to.toLowerCase() !== t.to) throw bad(`unexpected contract ${tx.to}`);
  if (BigInt(tx.value ?? "0x0") > MAX_VALUE_WEI[l.platform]) throw bad("unexpected ETH amount");
  if (tx.approval) {
    const a = decodeApprove(tx.approval.data);
    if (!a || tx.approval.to.toLowerCase() !== BASE_USDC) throw bad("unreadable approval or unexpected token");
    if (a.spender !== t.spender) throw bad(`approval for an unexpected contract ${a.spender}`);
    if (a.amount > maxUsdc6) throw bad("USDC approval above the maximum cost");
  }
}

export type Step = (text: string) => void;

/**
 * Collects every signature / transaction hash for a prepared execution, in the safest order:
 * 1. Derive order signatures (free, nothing is sent: refusing here costs nothing);
 * 2. transactions (USDC approval only if the current allowance is lower, then the order itself),
 *    each awaited until mined — these are irrevocable;
 * 3. the Derive auth timestamp last, so it is fresh when the server submits.
 */
export async function collectSignatures(p: Eip1193, address: string, prep: PreparedExecution, step: Step): Promise<SignedSubmission> {
  const sigs = new Map<string, { order_id: string; signature?: string; tx_hash?: string }>();
  for (const l of prep.legs) {
    if (!isBrowserVenue(l.platform)) throw new WalletError(`${l.platform}: browser signing not available`);
  }

  const derive = prep.legs.filter((l) => l.platform === "derive");
  for (const l of derive) {
    const td = l.typed_data;
    const chain = Number(td?.domain?.chainId);
    const owner = String(td?.message?.owner ?? "").toLowerCase();
    const contract = String(td?.domain?.verifyingContract ?? "").toLowerCase();
    if (!DERIVE_CHAINS.includes(chain) || contract !== DERIVE_MATCHING || owner !== address)
      throw new WalletError(`Leg ${l.leg_index + 1}: inconsistent data to sign (wallet ${owner || "?"}, contract ${contract || "?"})`);
    await ensureChain(p, chain);
    step(`Signing Derive order ${l.native_id} (nothing is sent at this stage)`);
    sigs.set(l.order_id, { order_id: l.order_id, signature: await signTypedData(p, address, td) });
  }

  // Approvals may never exceed twice the signed worst cost (Thetanuts asks 2x premium for its fee).
  const maxUsdc6 = BigInt(Math.ceil(maxCost(prep.legs) * 2 * 1e6)) + BigInt(1_000_000);
  for (const l of prep.legs.filter((x) => isTxVenue(x.platform))) {
    if (!l.transaction) throw new WalletError(`Jambe ${l.leg_index + 1} : transaction absente`);
    checkTransaction(l, l.transaction, maxUsdc6);
  }

  // CallPut legs share ONE transaction: send each distinct transaction once.
  const sent = new Map<string, string>();
  for (const l of prep.legs.filter((x) => isTxVenue(x.platform))) {
    const tx = l.transaction;
    if (!tx) throw new WalletError(`Jambe ${l.leg_index + 1} : transaction absente`);
    let hash = sent.get(tx.data);
    if (!hash) {
      await ensureChain(p, Number(tx.chain_id));
      if (tx.approval) {
        const a = decodeApprove(tx.approval.data);
        if (!a) throw new WalletError("Unreadable USDC approval: stopping");
        const current = await allowance(p, tx.approval.to, address, a.spender);
        if (current < a.amount) {
          step(`Exact USDC approval: ${usdc(a.amount)} USDC for ${a.spender}`);
          const ah = await sendTransaction(p, address, { to: tx.approval.to, data: tx.approval.data });
          step(`Approval sent (${ah}), waiting for confirmation…`);
          await waitReceipt(p, ah);
        }
      }
      step(`Sending the ${l.platform} ${l.native_id} transaction from your wallet`);
      hash = await sendTransaction(p, address, { to: tx.to, data: tx.data, value: tx.value });
      step(`Transaction sent (${hash}), waiting for confirmation…`);
      await waitReceipt(p, hash);
      sent.set(tx.data, hash);
    }
    sigs.set(l.order_id, { order_id: l.order_id, tx_hash: hash });
  }

  const sub: SignedSubmission = { route_id: prep.route_id, signatures: prep.legs.map((l) => sigs.get(l.order_id)!) };
  if (derive.length > 0) {
    const ts = Date.now();
    step("Derive authentication signature (timestamp)");
    sub.auth_timestamp_ms = ts;
    sub.auth_signature = await personalSign(p, address, String(ts));
  }
  return sub;
}

/** Worst premium + fee ceiling of the prepared legs, in the premium currency. */
export function maxCost(legs: PreparedLeg[]): number {
  return legs.reduce((s, l) => s + Number(l.qty_native) * (Number(l.limit_price_native) + Number(l.max_fee_per_unit ?? 0)), 0);
}
