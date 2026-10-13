// Browser wallet access through EIP-1193 (window.ethereum) and EIP-6963 (multi-wallet discovery).
// No dependency: the wallet signs and sends; this file only builds requests and reads answers.
// Omni Strike never sees a private key.

export interface Eip1193 {
  request(args: { method: string; params?: unknown[] | object }): Promise<unknown>;
  on?(event: string, cb: (...a: unknown[]) => void): void;
  removeListener?(event: string, cb: (...a: unknown[]) => void): void;
}

export interface WalletInfo {
  id: string;
  name: string;
  icon?: string;
  provider: Eip1193;
}

/** Wallets announced through EIP-6963, plus the legacy injected provider as a fallback. */
export function discoverWallets(onChange: (w: WalletInfo[]) => void): () => void {
  if (typeof window === "undefined") return () => {};
  const found = new Map<string, WalletInfo>();
  const publish = () => onChange([...found.values()]);
  const onAnnounce = (ev: Event) => {
    const d = (ev as CustomEvent).detail as { info?: { uuid: string; name: string; icon?: string }; provider?: Eip1193 };
    if (!d?.info || !d.provider) return;
    found.set(d.info.uuid, { id: d.info.uuid, name: d.info.name, icon: d.info.icon, provider: d.provider });
    publish();
  };
  window.addEventListener("eip6963:announceProvider", onAnnounce);
  window.dispatchEvent(new Event("eip6963:requestProvider"));
  // Legacy fallback after a short wait (wallets that do not implement EIP-6963).
  const t = setTimeout(() => {
    const legacy = (window as unknown as { ethereum?: Eip1193 }).ethereum;
    if (found.size === 0 && legacy) {
      found.set("injected", { id: "injected", name: "Browser wallet", provider: legacy });
      publish();
    }
  }, 400);
  return () => {
    clearTimeout(t);
    window.removeEventListener("eip6963:announceProvider", onAnnounce);
  };
}

export class WalletError extends Error {
  constructor(
    message: string,
    public code?: number,
  ) {
    super(message);
  }
}

/** Wallet errors with their EIP-1193 code (4001 = the user refused). */
async function rpc<T>(p: Eip1193, method: string, params?: unknown[]): Promise<T> {
  try {
    return (await p.request({ method, params })) as T;
  } catch (e) {
    const err = e as { code?: number; message?: string };
    if (err?.code === 4001) throw new WalletError("Rejected in the wallet", 4001);
    throw new WalletError(err?.message ?? String(e), err?.code);
  }
}

export async function connect(p: Eip1193): Promise<string> {
  const accts = await rpc<string[]>(p, "eth_requestAccounts");
  if (!accts?.[0]) throw new WalletError("No authorized account");
  return accts[0].toLowerCase();
}

export async function chainId(p: Eip1193): Promise<number> {
  return Number(await rpc<string>(p, "eth_chainId"));
}

const CHAINS: Record<number, { chainName: string; rpcUrls: string[]; nativeCurrency: { name: string; symbol: string; decimals: number }; blockExplorerUrls: string[] }> = {
  1: { chainName: "Ethereum", rpcUrls: ["https://ethereum-rpc.publicnode.com"], nativeCurrency: { name: "Ether", symbol: "ETH", decimals: 18 }, blockExplorerUrls: ["https://etherscan.io"] },
  8453: { chainName: "Base", rpcUrls: ["https://mainnet.base.org"], nativeCurrency: { name: "Ether", symbol: "ETH", decimals: 18 }, blockExplorerUrls: ["https://basescan.org"] },
  11155111: { chainName: "Sepolia", rpcUrls: ["https://ethereum-sepolia-rpc.publicnode.com"], nativeCurrency: { name: "Sepolia Ether", symbol: "ETH", decimals: 18 }, blockExplorerUrls: ["https://sepolia.etherscan.io"] },
};

export const chainName = (id: number) => CHAINS[id]?.chainName ?? `chain ${id}`;

export function explorerTx(id: number, hash: string): string | null {
  const base = CHAINS[id]?.blockExplorerUrls[0];
  return base ? `${base}/tx/${hash}` : null;
}

/** Wallets refuse eth_signTypedData_v4 when the active chain differs from the domain's chainId. */
export async function ensureChain(p: Eip1193, id: number): Promise<void> {
  if ((await chainId(p)) === id) return;
  const hex = `0x${id.toString(16)}`;
  try {
    await rpc(p, "wallet_switchEthereumChain", [{ chainId: hex }]);
  } catch (e) {
    if (e instanceof WalletError && e.code === 4902 && CHAINS[id]) {
      await rpc(p, "wallet_addEthereumChain", [{ chainId: hex, ...CHAINS[id] }]);
    } else {
      throw e;
    }
  }
  if ((await chainId(p)) !== id) throw new WalletError(`The wallet is not on ${chainName(id)}`);
}

export async function signTypedData(p: Eip1193, from: string, typedData: unknown): Promise<string> {
  return rpc<string>(p, "eth_signTypedData_v4", [from, JSON.stringify(typedData)]);
}

const utf8Hex = (s: string) => `0x${[...new TextEncoder().encode(s)].map((b) => b.toString(16).padStart(2, "0")).join("")}`;

export async function personalSign(p: Eip1193, from: string, text: string): Promise<string> {
  return rpc<string>(p, "personal_sign", [utf8Hex(text), from]);
}

export interface TxRequest {
  to: string;
  data: string;
  value?: string;
}

export async function sendTransaction(p: Eip1193, from: string, tx: TxRequest): Promise<string> {
  return rpc<string>(p, "eth_sendTransaction", [{ from, to: tx.to, data: tx.data, value: tx.value ?? "0x0" }]);
}

/** Waits for the receipt (polling through the wallet's own RPC). Throws if the transaction reverted. */
export async function waitReceipt(p: Eip1193, hash: string, timeoutMs = 240_000): Promise<void> {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    const r = await rpc<{ status?: string } | null>(p, "eth_getTransactionReceipt", [hash]);
    if (r) {
      if (r.status !== "0x1") throw new WalletError(`Transaction ${hash} reverted on-chain`);
      return;
    }
    await new Promise((res) => setTimeout(res, 2000));
  }
  throw new WalletError(`Transaction ${hash} not confirmed after ${timeoutMs / 1000} s`);
}

const word = (hex: string, i: number) => hex.slice(2 + 8 + i * 64, 2 + 8 + (i + 1) * 64);

/** Decodes `approve(spender, amount)` calldata (selector 0x095ea7b3). */
export function decodeApprove(data: string): { spender: string; amount: bigint } | null {
  if (!/^0x095ea7b3[0-9a-fA-F]{128}$/.test(data)) return null;
  return { spender: `0x${word(data, 0).slice(24)}`.toLowerCase(), amount: BigInt(`0x${word(data, 1)}`) };
}

/** ERC-20 allowance(owner, spender) through the wallet's RPC. */
export async function allowance(p: Eip1193, token: string, owner: string, spender: string): Promise<bigint> {
  const pad = (a: string) => a.toLowerCase().replace(/^0x/, "").padStart(64, "0");
  const data = `0xdd62ed3e${pad(owner)}${pad(spender)}`;
  const r = await rpc<string>(p, "eth_call", [{ to: token, data }, "latest"]);
  return BigInt(r && r !== "0x" ? r : "0x0");
}

/** USDC (6 decimals) for display. */
export const usdc = (v: bigint) => (Number(v) / 1e6).toLocaleString("en-US", { maximumFractionDigits: 6 });
