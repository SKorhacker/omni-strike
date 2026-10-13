"""Omni Strike — first NON-CUSTODIAL mainnet order on any onchain venue, or a ZERO-MONEY simulation.

  python scripts/omni_onchain.py VENUE                      # dry run: finds an executable route, shows it
  python scripts/omni_onchain.py VENUE --simulate           # zero money: throwaway key, full prepare/sign,
                                                            #   venue-side validation (see below), nothing spent
  python scripts/omni_onchain.py VENUE --wallet 0x.. [venue flags] --i-confirm-mainnet-real-funds

VENUE: derive | aevo | hyperliquid | hypercall | limitless | thetanuts | callput
(paradex needs Starknet signing and has no API liquidity: not covered.)

What --simulate proves without money (throwaway key generated in memory, never a real account):
  derive       our Action.data is accepted by Derive mainnet `public/decode_action`
  hyperliquid  HL mainnet recovers OUR throwaway address from the signature (same action hash as ours)
  hypercall    the order reaches Hypercall's signature check (their answer is printed)
  thetanuts    the exact fillOrder transaction runs against the LIVE Base contracts (eth_call with a
  callput      simulated USDC balance/allowance for the throwaway address); revert = bug
  aevo         local checks only (Aevo needs the user's API key before it looks at a signature)
  limitless    local checks only (same reason)

Real mode: your key is typed hidden (memory only, never written/printed), must control --wallet (or
--signer). Before anything is signed the script DECODES what will be signed and refuses anything but a
BUY of the requested size on the expected mainnet contract with worst cost <= --max-cost (default 100
USDC). Transaction venues approve EXACTLY the needed USDC (the token caps what can be pulled). Final y/N.
Server: the mainnet-nc server (OA_MAINNET_SERVER, default http://127.0.0.1:8082).
"""
import argparse
import base64
import getpass
import hashlib
import hmac
import json
import os
import secrets
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from decimal import Decimal

from eth_abi import decode as abi_decode
from eth_abi import encode as abi_encode
from eth_account import Account
from eth_account.messages import encode_defunct, encode_typed_data
from eth_utils import keccak

SERVER = os.environ.get("OA_MAINNET_SERVER", "http://127.0.0.1:8082")
BASE_RPC = os.environ.get("OA_BASE_RPC", "https://base-rpc.publicnode.com")
BASE_CHAIN_ID = 8453
USDC_BASE = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"
THETANUTS_BOOK = "0x1bdff855d6811728acadc00989e79143a2bdfded"
CALLPUT_PM = "0x83b04701b227b045cbbaf921377137ff595a54af"
CALLPUT_ROUTER = "0xfc61ba50ae7b9c4260c9f04631ff28d5a2fa4eb2"  # USDC spender for CallPut
HL_EXCHANGE = "https://api.hyperliquid.xyz/exchange"
HYPERCALL_API = "https://api.hypercall.xyz"
DERIVE_API = "https://api.derive.xyz/v3"
ZERO = "0x0000000000000000000000000000000000000000"
ACCEPT = ["SETTLEMENT_INDEX_DIFFERS", "SETTLEMENT_WINDOW_UNKNOWN", "SETTLEMENT_WINDOW_DIFFERS",
          "SETTLEMENT_ASSET_DIFFERS", "PREMIUM_CURRENCY_DIFFERS"]

# family, default quantity per underlying (underlying units; binaries = payout in USDC)
VENUES = {
    "derive": ("dated_option", {"ETH": "0.1", "BTC": "0.01"}),
    "aevo": ("dated_option", {"ETH": "0.1", "BTC": "0.01"}),
    "hypercall": ("dated_option", {"ETH": "0.1", "BTC": "0.01"}),
    # Thetanuts makers require >= 50 USDC of premium per fill: ~3 ETH of near-the-money calls.
    "thetanuts": ("dated_option", {"ETH": "3", "BTC": "0.1"}),
    "callput": ("dated_option", {"ETH": "0.1", "BTC": "0.01"}),
    "hyperliquid": ("binary", {"BTC": "5", "ETH": "5"}),
    "limitless": ("binary", {"BTC": "5", "ETH": "5"}),
}
TX_VENUES = {"thetanuts", "callput"}


# ------------------------------------------------------------------------------------------ helpers
def http(method, url, body=None, headers=None, timeout=60, raw_body=None):
    h = {"content-type": "application/json", "user-agent": "omni-strike-onchain"}
    if url.startswith(SERVER) and os.environ.get("OA_API_TOKEN"):
        h["authorization"] = "Bearer " + os.environ["OA_API_TOKEN"]
    h.update(headers or {})
    data = raw_body.encode() if raw_body is not None else (json.dumps(body).encode() if body is not None else None)
    req = urllib.request.Request(url, data, h, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            txt = r.read().decode()
            return r.status, (json.loads(txt) if txt.strip().startswith(("{", "[")) else txt)
    except urllib.error.HTTPError as e:
        txt = e.read().decode(errors="replace")
        try:
            return e.code, json.loads(txt)
        except Exception:
            return e.code, txt


def rpc(method, params):
    s, r = http("POST", BASE_RPC, {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}, timeout=30)
    if s != 200 or not isinstance(r, dict):
        raise RuntimeError(f"Base RPC {method}: HTTP {s} {r}")
    return r


def die(msg):
    sys.exit(msg)


def same(a, b):
    return isinstance(a, str) and isinstance(b, str) and a.lower() == b.lower()


# ------------------------------------------------------------------------------------------ route
WHY = {}


def find_route(venue, underlying, qty):
    family, _ = VENUES[venue]
    now = time.time()
    tried = 0
    WHY.clear()
    for kind in ("call", "put"):
        s, r = http("GET", f"{SERVER}/v1/instruments?underlying={underlying}&option_kind={kind}&family={family}&limit=1000")
        if s != 200:
            die(f"server {SERVER} refused /v1/instruments ({s}): {r}")
        groups = []
        for g in r["groups"]:
            if not any(l["key"]["platform"] == venue for l in g["listings"]):
                continue
            t = datetime.fromisoformat(g["expiry"].replace("Z", "+00:00")).timestamp() - now
            lo = 600 if family == "binary" else 2 * 86400
            if lo < t < 45 * 86400:
                groups.append(g)
        by_exp = {}
        for g in groups:
            by_exp.setdefault(g["expiry"], []).append(g)
        for exp, gs in sorted(by_exp.items())[:3]:
            gs.sort(key=lambda g: Decimal(g["strike"]))
            mid = len(gs) // 2
            for g in sorted(gs, key=lambda g: abs(gs.index(g) - mid))[:6]:
                intent = {"underlying": underlying, "option_kind": kind, "strike": g["strike"], "expiry": exp,
                          "side": "buy", "quantity": qty, "accepted_differences": ACCEPT, "venues": [venue]}
                if family == "binary":
                    intent["family"] = "binary"
                tried += 1
                s, q = http("POST", f"{SERVER}/v1/routes/quote", intent)
                if s == 200 and q.get("executable") and q["plan"] and all(
                        l["instrument"]["platform"] == venue for l in q["plan"]["legs"]):
                    return q
                if s != 200:
                    WHY[f"HTTP {s}: {str(q)[:120]}"] = WHY.get(f"HTTP {s}: {str(q)[:120]}", 0) + 1
                else:
                    rs = [r for c in q.get("candidates", []) if c["instrument"]["platform"] == venue
                          for r in c["reasons"]] or q.get("not_executable_reasons") or ["?"]
                    for r in rs:
                        WHY[r[:120]] = WHY.get(r[:120], 0) + 1
                if tried > 40:
                    return None
    return None


# ------------------------------------------------------------------------------------------ checks
def check_derive(leg, acct, qty, max_cost):
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    import noncustodial_mainnet_derive as d
    return d.decode_leg(leg, acct["owner"], acct.get("signer") or acct["owner"], int(acct["subaccount_id"]),
                        qty, max_cost)[0]


def check_aevo(leg, acct, qty, max_cost):
    td, p = leg["typed_data"], []
    d, m = td["domain"], td["message"]
    if td.get("primaryType") != "Order" or d.get("name") != "Aevo Mainnet" or int(d.get("chainId", 0)) != 1:
        p.append("not an Aevo MAINNET Order")
    if not same(m["maker"], acct["maker"]):
        p.append("maker differs from your account")
    if m["isBuy"] is not True:
        p.append("signed order is a SELL")
    amount = Decimal(m["amount"]) / Decimal(10 ** 6)
    if amount != Decimal(qty):
        p.append(f"signed amount {amount} != {qty}")
    worst = Decimal(m["limitPrice"]) / Decimal(10 ** 6) * amount
    if worst > max_cost:
        p.append(f"worst premium {worst} USDC > --max-cost {max_cost}")
    b = json.loads(leg["body_template"])
    for k_body, k_msg in [("maker", "maker"), ("is_buy", "isBuy"), ("amount", "amount"), ("limit_price", "limitPrice"),
                          ("salt", "salt"), ("timestamp", "timestamp"), ("instrument", "instrument")]:
        if str(b[k_body]).lower() != str(m[k_msg]).lower():
            p.append(f"request body {k_body} differs from the signed order")
    if b.get("signature") != "__SIGNATURE__":
        p.append("unexpected body template")
    return p


def check_hyperliquid(leg, acct, qty, max_cost):
    from hyperliquid.utils.signing import action_hash
    td, p = leg["typed_data"], []
    d, m = td["domain"], td["message"]
    if d.get("name") != "Exchange" or int(d.get("chainId", 0)) != 1337 or m.get("source") != "a":
        p.append("not a Hyperliquid MAINNET agent payload")
    body = json.loads(leg["body_template"] or "{}")
    action, nonce = body.get("action"), body.get("nonce")
    if not action:
        return p + ["server did not return the action to check"]
    h = "0x" + action_hash(action, None, nonce, None).hex()
    if not same(h, m["connectionId"]):
        p.append("the signed hash is NOT the hash of the returned action")
    o = action["orders"]
    if len(o) != 1 or o[0]["b"] is not True or o[0]["r"] is not False:
        p.append("action is not a single non-reduce BUY")
    else:
        worst = Decimal(o[0]["p"]) * Decimal(o[0]["s"])
        if worst > max_cost:
            p.append(f"worst cost {worst} > --max-cost {max_cost}")
    return p


def check_hypercall(leg, acct, qty, max_cost):
    td, p = leg["typed_data"], []
    d, m = td["domain"], td["message"]
    if d.get("name") != "Hypercall" or int(d.get("chainId", 0)) != 999:
        p.append("not a Hypercall payload")
    if not same(m["wallet"], acct["wallet"]):
        p.append("wallet differs")
    if m["side"] != "Buy" or m["tif"] not in ("ioc", "fok"):
        p.append("not an immediate BUY")
    worst = Decimal(m["size"]) * Decimal(m["price"])
    if worst > max_cost:
        p.append(f"worst premium {worst} > --max-cost {max_cost}")
    return p


def check_limitless(leg, acct, qty, max_cost):
    td, p = leg["typed_data"], []
    d, m = td["domain"], td["message"]
    if d.get("name") != "Limitless CTF Exchange" or int(d.get("chainId", 0)) != BASE_CHAIN_ID:
        p.append("not a Limitless Base order")
    if not same(m["maker"], acct["maker"]) or not same(m["signer"], acct["maker"]):
        p.append("maker/signer differ from your EOA")
    if int(m["side"]) != 0:
        p.append("signed order is a SELL")
    spend = Decimal(m["makerAmount"]) / Decimal(10 ** 6)
    if spend > max_cost:
        p.append(f"spend cap {spend} USDC > --max-cost {max_cost}")
    o = json.loads(leg["body_template"])["order"]
    for k in ("salt", "maker", "signer", "tokenId", "makerAmount", "takerAmount", "side", "feeRateBps"):
        if str(o.get(k)).lower() != str(m[k]).lower():
            p.append(f"request body {k} differs from the signed order")
    return p


def check_tx(venue, leg, wallet, max_cost):
    tx, p = leg["transaction"], []
    to, sel = (THETANUTS_BOOK, None) if venue == "thetanuts" else (CALLPUT_PM, "0x4fd5f4af")
    if int(tx["chain_id"]) != BASE_CHAIN_ID or not same(tx["to"], to):
        p.append(f"transaction is not to the {venue} contract on Base")
    if sel and not tx["data"].lower().startswith(sel):
        p.append("unexpected function")
    if venue == "thetanuts" and not tx["data"].lower().startswith("0x" + keccak(text=fill_order_sig()).hex()[:8]):
        p.append("not OptionBook.fillOrder")
    value = int(tx.get("value", "0x0"), 16)
    if value > 10 ** 15:
        p.append(f"ETH value {value} wei is more than 0.001 ETH")
    ap = tx["approval"]
    if not same(ap["to"], USDC_BASE) or not ap["data"].lower().startswith("0x095ea7b3"):
        p.append("approval is not USDC.approve")
    else:
        spender, amount = abi_decode(["address", "uint256"], bytes.fromhex(ap["data"][10:]))
        if not same(spender, THETANUTS_BOOK if venue == "thetanuts" else CALLPUT_ROUTER):
            p.append("approval spender is not the venue's USDC spender")
        if Decimal(amount) / Decimal(10 ** 6) > max_cost:
            p.append(f"USDC approval {Decimal(amount) / 10 ** 6} > --max-cost {max_cost}")
    return p


CALLPUT_FEED = "https://app-data-base.s3.ap-southeast-1.amazonaws.com/market-data.json"


def find_callput_spread(underlying, qty):
    """CallPut opens only 2-leg DEBIT spreads: buy a near-the-money call, sell the next strike up."""
    now = time.time()
    s, r = http("GET", f"{SERVER}/v1/instruments?underlying={underlying}&option_kind=call&limit=1000")
    if s != 200:
        die(f"server refused /v1/instruments ({s}): {r}")
    by_exp = {}
    for g in r["groups"]:
        if any(l["key"]["platform"] == "callput" for l in g["listings"]):
            t = datetime.fromisoformat(g["expiry"].replace("Z", "+00:00")).timestamp() - now
            if 86400 < t < 30 * 86400:
                by_exp.setdefault(g["expiry"], []).append(Decimal(g["strike"]))
    for exp, ks in sorted(by_exp.items())[:3]:
        ks.sort()
        mid = len(ks) // 2
        for i in sorted(range(len(ks) - 1), key=lambda i: abs(i - mid))[:6]:
            body = {"underlying": underlying, "quantity": qty, "accepted_differences": ACCEPT, "venues": ["callput"],
                    "legs": [{"option_kind": "call", "strike": str(ks[i]), "expiry": exp, "side": "buy"},
                             {"option_kind": "call", "strike": str(ks[i + 1]), "expiry": exp, "side": "sell"}]}
            s, q = http("POST", f"{SERVER}/v1/strategies/route", body)
            if s == 200 and q.get("executable"):
                q["plan"] = q["route"]
                return q
            if s == 200:
                for why in q.get("not_executable_reasons", []):
                    WHY[why[:120]] = WHY.get(why[:120], 0) + 1
            else:
                WHY[f"HTTP {s}: {str(q)[:120]}"] = WHY.get(f"HTTP {s}: {str(q)[:120]}", 0) + 1
    return None


def check_callput_spread(legs):
    """Both legs must carry the SAME transaction, encoding [BUY, SELL] of exactly these two options
    (ids checked against CallPut's own public feed)."""
    p = []
    if len(legs) != 2 or {l["side"] for l in legs} != {"buy", "sell"}:
        return ["expected a 2-leg buy/sell spread"]
    if legs[0]["transaction"]["data"] != legs[1]["transaction"]["data"]:
        p.append("spread legs do not share one transaction")
    buy = next(l for l in legs if l["side"] == "buy")
    sell = next(l for l in legs if l["side"] == "sell")
    a = abi_decode(["uint16", "uint8", "bool[4]", "bytes32[4]", "bool[4]", "uint256", "address[]", "uint256",
                    "uint256", "address"], bytes.fromhex(buy["transaction"]["data"][10:]))
    if a[1] != 2 or tuple(a[2][:2]) != (True, False):
        p.append("encoded request is not [BUY, SELL]")
    s, feed = http("GET", CALLPUT_FEED)
    ids = {}
    for u in feed["data"]["market"].values():
        for e in u["options"].values():
            for side in ("call", "put"):
                for o in e[side]:
                    ids[o["instrument"]] = o["optionId"].lower()
    for k, leg in ((0, buy), (1, sell)):
        if ids.get(leg["native_id"]) != "0x" + a[3][k].hex():
            p.append(f"encoded option {k} is not {leg['native_id']} (CallPut feed)")
    if Decimal(a[5]) / Decimal(10 ** 18) != Decimal(buy["qty_native"]):
        p.append("encoded size differs from the leg size")
    return p


def fill_order_sig():
    return ("fillOrder((address,uint256,address,bool,address,address,bool,uint256,uint256[],uint256,uint256,"
            "uint256,bytes),bytes,address)")


CHECKS = {"derive": check_derive, "aevo": check_aevo, "hyperliquid": check_hyperliquid,
          "hypercall": check_hypercall, "limitless": check_limitless}


# ------------------------------------------------------------------------------------------ accounts
def account_for(venue, a, wallet):
    signer = a.signer or wallet
    if venue == "derive":
        return {"owner": wallet, "signer": signer, "subaccount_id": int(a.subaccount or 1)}
    if venue == "aevo":
        return {"maker": wallet, "signer": signer, "api_key": a.api_key or "simulation-key"}
    if venue in ("hyperliquid", "hypercall"):
        return {"wallet": wallet, "signer": signer}
    if venue == "limitless":
        return {"maker": wallet, "owner_id": int(a.owner_id or 1), "fee_rate_bps": int(a.fee_rate_bps or 0),
                "api_token_id": a.api_key or "simulation-token"}
    return {"address": wallet}


# ------------------------------------------------------------------------------------------ simulation
def usdc_override(holder, spender):
    big = "0x" + (10 ** 13).to_bytes(32, "big").hex()  # 10M USDC
    bal = "0x" + keccak(abi_encode(["address", "uint256"], [holder, 9])).hex()
    inner = keccak(abi_encode(["address", "uint256"], [holder, 10]))
    alw = "0x" + keccak(abi_encode(["address"], [spender]) + inner).hex()
    return {USDC_BASE: {"stateDiff": {bal: big, alw: big}}, holder: {"balance": hex(10 ** 18)}}


def simulate_tx(venue, leg, wallet):
    tx = leg["transaction"]
    call = {"from": wallet, "to": tx["to"], "data": tx["data"], "value": tx.get("value", "0x0")}
    spender = abi_decode(["address", "uint256"], bytes.fromhex(tx["approval"]["data"][10:]))[0]
    r = rpc("eth_call", [call, "latest", usdc_override(wallet, spender)])
    if "error" in r:
        return False, f"REVERT on live Base contracts: {r['error']}"
    return True, f"fillOrder/createOpenPosition executes on live Base contracts (returned {r['result'][:66]}…)"


def simulate_signed(venue, leg, key, acct):
    sig = sign_typed(key, leg["typed_data"])
    if venue == "derive":
        s, r = http("POST", f"{DERIVE_API}/public/decode_action",
                    {"action_type": "order", "encoded_data": leg["typed_data"]["message"]["data"]})
        ok = s == 200 and isinstance(r, dict) and "result" in r
        return ok, f"Derive mainnet decode_action: {json.dumps(r)[:300]}"
    if venue == "hyperliquid":
        body = json.loads(leg["body_template"])
        raw = bytes.fromhex(sig[2:])
        payload = {"action": body["action"], "nonce": body["nonce"], "vaultAddress": None,
                   "signature": {"r": "0x" + raw[:32].hex(), "s": "0x" + raw[32:64].hex(), "v": raw[64]}}
        s, r = http("POST", HL_EXCHANGE, payload)
        txt = json.dumps(r)
        ok = acct["wallet"].lower() in txt.lower()
        return ok, (f"Hyperliquid mainnet answer: {txt[:300]}" +
                    ("\n  -> HL recovered OUR throwaway address: same action hash + signature scheme" if ok else
                     "\n  -> HL did NOT recover our address: signature scheme mismatch"))
    if venue == "hypercall":
        m = leg["typed_data"]["message"]
        body = {"wallet": m["wallet"], "symbol": m["symbol"], "price": m["price"], "size": m["size"], "side": m["side"],
                "tif": m["tif"], "route": m["route"], "client_id": m["clientId"], "nonce": int(m["nonce"]), "signature": sig}
        s, r = http("POST", f"{HYPERCALL_API}/order", body)
        return None, f"Hypercall answer (unfunded throwaway wallet, HTTP {s}): {json.dumps(r)[:300]}"
    # aevo / limitless: venue needs the user's API key first; local proof only
    rec = Account.recover_message(encode_typed_data(full_message=leg["typed_data"]), signature=sig)
    return same(rec, acct.get("signer") or acct.get("maker")), \
        f"local: signature over the typed data recovers {rec} (venue check needs your API key)"


# ------------------------------------------------------------------------------------------ signing / sending
def sign_typed(key, td):
    return "0x" + Account.sign_message(encode_typed_data(full_message=td), key).signature.hex().removeprefix("0x")


def send_base_tx(key, to, data, value):
    acct = Account.from_key(key)
    nonce = int(rpc("eth_getTransactionCount", [acct.address, "pending"])["result"], 16)
    base_fee = int(rpc("eth_getBlockByNumber", ["latest", False])["result"]["baseFeePerGas"], 16)
    tip = int(rpc("eth_maxPriorityFeePerGas", [])["result"], 16)
    call = {"from": acct.address, "to": to, "data": data, "value": hex(value)}
    est = rpc("eth_estimateGas", [call])
    if "error" in est:
        raise RuntimeError(f"transaction would revert: {est['error']}")
    tx = {"chainId": BASE_CHAIN_ID, "nonce": nonce, "to": to, "data": data, "value": value,
          "gas": int(int(est["result"], 16) * 1.3), "maxPriorityFeePerGas": tip, "maxFeePerGas": base_fee * 2 + tip,
          "type": 2}
    signed = Account.sign_transaction(tx, key)
    raw = getattr(signed, "raw_transaction", None) or signed.rawTransaction
    r = rpc("eth_sendRawTransaction", ["0x" + raw.hex().removeprefix("0x")])
    if "error" in r:
        raise RuntimeError(f"send failed: {r['error']}")
    h = r["result"]
    print(f"  sent {h}, waiting for the receipt…")
    for _ in range(90):
        rc = rpc("eth_getTransactionReceipt", [h]).get("result")
        if rc:
            if int(rc["status"], 16) != 1:
                raise RuntimeError(f"transaction {h} REVERTED")
            return h
        time.sleep(2)
    raise RuntimeError(f"no receipt for {h} after 3 min (check basescan before retrying)")


def allowance(owner, spender):
    data = "0xdd62ed3e" + abi_encode(["address", "address"], [owner, spender]).hex()
    return int(rpc("eth_call", [{"to": USDC_BASE, "data": data}, "latest"])["result"], 16)


def venue_auth(venue, leg, sig, a):
    if venue == "aevo":
        secret = getpass.getpass("Aevo API SECRET (hidden, memory only): ").strip()
        ts = str(time.time_ns())
        body = leg["body_template"].replace("__SIGNATURE__", sig)
        msg = f"{a.api_key},{ts},POST,/orders,{body}"
        return {"timestamp_ns": ts, "hmac": hmac.new(secret.encode(), msg.encode(), hashlib.sha256).hexdigest()}
    if venue == "limitless":
        secret = getpass.getpass("Limitless API token SECRET (base64, hidden): ").strip()
        ts = datetime.now(timezone.utc).isoformat()
        body = leg["body_template"].replace("__SIGNATURE__", sig)
        msg = f"{ts}\nPOST\n/orders\n{body}"
        return {"timestamp": ts, "hmac": base64.b64encode(
            hmac.new(base64.b64decode(secret), msg.encode(), hashlib.sha256).digest()).decode()}
    return None


# ------------------------------------------------------------------------------------------ main
def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("venue", choices=sorted(VENUES))
    ap.add_argument("--underlying", default="ETH", choices=["BTC", "ETH"])
    ap.add_argument("--qty", help="quantity (underlying units; binaries: payout in USDC)")
    ap.add_argument("--max-cost", type=Decimal, default=Decimal("100"))
    ap.add_argument("--simulate", action="store_true", help="zero money: throwaway key, nothing spent")
    ap.add_argument("--wallet", help="your wallet / owner / maker address (real mode)")
    ap.add_argument("--signer", help="session key / API agent / Aevo signing key address (default: wallet)")
    ap.add_argument("--subaccount", help="Derive subaccount id")
    ap.add_argument("--api-key", help="Aevo API key / Limitless API token id (identifiers, not secrets)")
    ap.add_argument("--owner-id", help="Limitless profile id")
    ap.add_argument("--fee-rate-bps", help="Limitless rank.feeRateBps")
    ap.add_argument("--i-confirm-mainnet-real-funds", action="store_true")
    a = ap.parse_args()
    venue = a.venue
    qty = a.qty or VENUES[venue][1][a.underlying]
    real = a.i_confirm_mainnet_real_funds
    if real and a.simulate:
        die("choose --simulate OR --i-confirm-mainnet-real-funds")

    q = find_callput_spread(a.underlying, qty) if venue == "callput" else find_route(venue, a.underlying, qty)
    if q is None:
        why = "\n".join(f"  {n}x {r}" for r, n in sorted(WHY.items(), key=lambda x: -x[1])[:8]) or "  no listing found"
        die(f"no executable {venue} route for {qty} {a.underlying}. Router reasons:\n{why}")
    for l in q["plan"]["legs"]:
        print(f"route: {l['side'].upper()} {l['qty_native']} {l['instrument']['native_id']} on {venue}, "
              f"limit {l['limit_price_native']}, expected total {l['expected_total']}")
    if not (real or a.simulate):
        die("Dry run. --simulate = zero-money test; --i-confirm-mainnet-real-funds = real order.")

    if a.simulate:
        key = "0x" + secrets.token_hex(32)
        wallet = Account.from_key(key).address
        a.signer = None
        print(f"simulation with a THROWAWAY key (address {wallet}, no funds, discarded at exit)")
    else:
        if not a.wallet:
            die("--wallet is required in real mode")
        wallet = a.wallet
    acct = account_for(venue, a, wallet)
    s, p = http("POST", f"{SERVER}/v1/noncustodial/prepare",
                {"route_id": q["route_id"], "route_hash": q["route_hash"], "accounts": {venue: acct}})
    if s != 200:
        die(f"prepare refused ({s}): {p}")
    legs = p["legs"]
    n_legs = 2 if venue == "callput" else 1
    if len(legs) != n_legs or any(l["platform"] != venue for l in legs):
        die(f"REFUSING: expected exactly {n_legs} leg(s) on {venue}")
    leg = next(l for l in legs if l["side"] == "buy")
    problems = check_tx(venue, leg, wallet, a.max_cost) if venue in TX_VENUES else \
        CHECKS[venue](leg, acct, qty, a.max_cost)
    if venue == "callput":
        problems += check_callput_spread(legs)
    if problems:
        die("REFUSING TO SIGN: " + "; ".join(problems))
    print(f"pre-sign checks OK ({'transaction' if venue in TX_VENUES else 'decoded typed data'}): "
          f"BUY {leg['qty_native']} {leg['native_id']} @ <= {leg['limit_price_native']}, cost cap {a.max_cost} USDC")

    if a.simulate:
        ok, msg = simulate_tx(venue, leg, wallet) if venue in TX_VENUES else simulate_signed(venue, leg, key, acct)
        print(("PASS " if ok else "FAIL " if ok is False else "INFO ") + msg)
        sys.exit(0 if ok is not False else 1)

    if input(f"Sign and send this MAINNET {venue} order? [y/N] ").strip().lower() != "y":
        die("cancelled, nothing sent")
    key = getpass.getpass(f"private key of {acct.get('signer') or wallet} (hidden, memory only): ").strip()
    me = Account.from_key(key).address
    expected = wallet if venue in TX_VENUES else (acct.get("signer") or wallet)
    if not same(me, expected):
        die("that key does not control the expected address; nothing sent")
    sub = {"route_id": p["route_id"], "signatures": []}
    if venue in TX_VENUES:
        tx = leg["transaction"]
        need = abi_decode(["address", "uint256"], bytes.fromhex(tx["approval"]["data"][10:]))[1]
        spender = abi_decode(["address", "uint256"], bytes.fromhex(tx["approval"]["data"][10:]))[0]
        if allowance(wallet, spender) < need:
            print(f"approving exactly {Decimal(need) / 10 ** 6} USDC to the {venue} contract")
            send_base_tx(key, USDC_BASE, tx["approval"]["data"], 0)
        h = send_base_tx(key, tx["to"], tx["data"], int(tx.get("value", "0x0"), 16))
        # One transaction per venue request: every leg it carries (CallPut spread) gets the same hash.
        for l in legs:
            sub["signatures"].append({"order_id": l["order_id"], "tx_hash": h})
    else:
        sig = sign_typed(key, leg["typed_data"])
        entry = {"order_id": leg["order_id"], "signature": sig}
        va = venue_auth(venue, leg, sig, a)
        if va:
            entry["venue_auth"] = va
        sub["signatures"].append(entry)
        if venue == "derive":
            ts = int(time.time() * 1000)
            sub["auth_timestamp_ms"] = ts
            sub["auth_signature"] = "0x" + Account.sign_message(encode_defunct(text=str(ts)), key).signature.hex().removeprefix("0x")
    del key
    t0 = time.time()
    s, r = http("POST", f"{SERVER}/v1/noncustodial/submit", sub)
    print(f"submit HTTP {s} in {1000 * (time.time() - t0):.0f} ms")
    if isinstance(r, dict):
        for o in r.get("orders", []):
            print(f"  {o['native_id']}: {o['state']} filled {o['filled_native']} avg {o.get('avg_fill_price_native')} "
                  f"venue id {o.get('venue_order_id')} err {o.get('last_error')}")
        for n in r.get("notes", []):
            print("  note:", n)
    else:
        print(r)


if __name__ == "__main__":
    main()
