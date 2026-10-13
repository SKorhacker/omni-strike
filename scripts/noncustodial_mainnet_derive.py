"""FIRST MAINNET ORDER on Derive v3, NON-CUSTODIAL: quote -> prepare -> YOU sign locally -> submit.

REAL FUNDS. The Omni Strike server never sees your key: it is typed here (hidden input, kept in
memory only, never written or printed) and only signs the EIP-712 payload the server returned,
after this script DECODES the signed bytes and re-checks them: Derive mainnet domain and Action type,
your owner/signer/subaccount, BUY side, amount, worst cost <= --max-cost, asset = the instrument as
read directly from api.derive.xyz, signature expiry <= 10 min.

  python scripts/noncustodial_mainnet_derive.py --owner 0xYOUR_EOA --subaccount 12345
  python scripts/noncustodial_mainnet_derive.py --owner ... --subaccount ... --i-confirm-mainnet-real-funds

Needs: the mainnet-nc server on :8082 (docs/mainnet-noncustodial.md) with OA_DERIVE_NONCUSTODIAL=true
and OA_KILL_SWITCH=false, and USDC in that Derive v3 subaccount. Prefer a SESSION KEY with trade
scope (--signer 0xSESSION) over your wallet key. Places ONE IOC order (no resting order), size
--qty (default 0.01 BTC / 0.1 ETH), after a final y/N confirmation.
"""
import argparse
import getpass
import json
import os
import sys
import time
import urllib.request
from datetime import datetime, timezone
from decimal import Decimal

from eth_abi import decode as abi_decode

from eth_account import Account
from eth_account.messages import encode_defunct, encode_typed_data

SERVER = os.environ.get("OA_MAINNET_SERVER", "http://127.0.0.1:8082")
MAINNET_CHAIN_ID = 1
MATCHING = "0xeb8d770ec18db98db922e9d83260a585b9f0dead"
TRADE_MODULE = "0xb8d20c2b7a1ad2ee33bc50ef10876ed3035b5e7b"
DERIVE_API = "https://api.derive.xyz/v3"
ACCEPT = ["SETTLEMENT_INDEX_DIFFERS", "SETTLEMENT_WINDOW_UNKNOWN", "SETTLEMENT_WINDOW_DIFFERS",
          "SETTLEMENT_ASSET_DIFFERS", "PREMIUM_CURRENCY_DIFFERS"]


def http(method, url, body=None, timeout=30):
    headers = {"content-type": "application/json"}
    if os.environ.get("OA_API_TOKEN"):
        headers["authorization"] = "Bearer " + os.environ["OA_API_TOKEN"]
    req = urllib.request.Request(url, json.dumps(body).encode() if body is not None else None, headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, json.load(r)
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.load(e)
        except Exception:
            return e.code, str(e)


def pick(underlying, qty):
    """Near-the-money Derive contracts 2-30 days out, via the server's own catalog."""
    now = datetime.now(timezone.utc).timestamp()
    out = []
    for kind in ("call", "put"):
        s, r = http("GET", f"{SERVER}/v1/instruments?underlying={underlying}&option_kind={kind}&limit=1000", timeout=60)
        if s != 200:
            sys.exit(f"server not reachable or refused ({s}): {r}")
        by_exp = {}
        for g in r["groups"]:
            if not any(l["key"]["platform"] == "derive" for l in g["listings"]):
                continue
            t = datetime.fromisoformat(g["expiry"].replace("Z", "+00:00")).timestamp() - now
            if 2 * 86400 < t < 30 * 86400:
                by_exp.setdefault(g["expiry"], []).append(g)
        for exp, gs in sorted(by_exp.items())[:2]:
            gs.sort(key=lambda g: float(g["strike"]))
            mid = len(gs) // 2
            for g in sorted(gs, key=lambda g: abs(gs.index(g) - mid))[:5]:
                out.append({"underlying": underlying, "option_kind": kind, "strike": g["strike"], "expiry": exp})
    return out


ACTION_TYPES = [
    {"name": "subaccountId", "type": "uint256"}, {"name": "nonce", "type": "uint256"},
    {"name": "module", "type": "address"}, {"name": "data", "type": "bytes"},
    {"name": "expiry", "type": "uint256"}, {"name": "owner", "type": "address"},
    {"name": "signer", "type": "address"},
]
E18 = Decimal(10) ** 18


def derive_instrument(name):
    """Asset address / sub id read DIRECTLY from Derive (not from the Omni Strike server)."""
    req = urllib.request.Request(f"{DERIVE_API}/public/get_instrument", json.dumps({"instrument_name": name}).encode(),
                                 {"content-type": "application/json", "user-agent": "omni-strike-check"})
    with urllib.request.urlopen(req, timeout=20) as r:
        return json.load(r)["result"]


def decode_leg(leg, owner, signer, subaccount, qty, max_cost):
    """Decode the bytes that will be signed and refuse anything but the order we intend.
    Returns (problems, decoded order dict built ONLY from the signed bytes)."""
    td = leg["typed_data"]
    d, m = td["domain"], td["message"]
    problems = []
    if td.get("primaryType") != "Action" or td["types"].get("Action") != ACTION_TYPES:
        problems.append("not a Derive Action type")
    if int(d["chainId"]) != MAINNET_CHAIN_ID:
        problems.append(f"chainId {d['chainId']} is not Ethereum mainnet")
    if d["verifyingContract"].lower() != MATCHING or d["name"] != "Matching" or d["version"] != "1.0":
        problems.append("unexpected EIP-712 domain")
    if m["module"].lower() != TRADE_MODULE:
        problems.append("module is not Derive's TRADE_MODULE")
    if m["owner"].lower() != owner.lower() or m["signer"].lower() != signer.lower():
        problems.append("owner/signer differ from yours")
    if int(m["subaccountId"]) != subaccount:
        problems.append("subaccount differs from yours")
    if not (time.time() < int(m["expiry"]) <= time.time() + 600):
        problems.append("signature expiry is not within the next 10 minutes")
    asset, sub_id, limit, amount, max_fee, recipient, is_bid = abi_decode(
        ["address", "uint256", "int256", "int256", "uint256", "uint256", "bool"],
        bytes.fromhex(m["data"].removeprefix("0x")))
    order = {"is_bid": is_bid, "amount": Decimal(amount) / E18, "limit": Decimal(limit) / E18,
             "max_fee": Decimal(max_fee) / E18}
    order["worst_cost"] = order["amount"] * (order["limit"] + order["max_fee"])
    if not is_bid:
        problems.append("the signed order is a SELL")
    if order["amount"] != Decimal(qty):
        problems.append(f"signed amount {order['amount']} != requested {qty}")
    if recipient != subaccount:
        problems.append("recipient subaccount differs from yours")
    if order["limit"] <= 0 or order["worst_cost"] > max_cost:
        problems.append(f"worst cost {order['worst_cost']} USDC exceeds --max-cost {max_cost}")
    ins = derive_instrument(leg["native_id"])
    if asset.lower() != ins["base_asset_address"].lower() or str(sub_id) != str(ins["base_asset_sub_id"]):
        problems.append("signed asset is not the instrument shown (checked on api.derive.xyz)")
    return problems, order


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--owner", required=True, help="your Derive v3 wallet (EOA or multisig) address")
    ap.add_argument("--subaccount", required=True, type=int)
    ap.add_argument("--signer", help="session key address (default: the owner signs)")
    ap.add_argument("--underlying", default="ETH", choices=["BTC", "ETH"])
    ap.add_argument("--qty", help="contracts (default 0.01 BTC / 0.1 ETH)")
    ap.add_argument("--max-cost", type=Decimal, default=Decimal("100"),
                    help="refuse to sign if amount x (limit + max fee) exceeds this, USDC (default 100)")
    ap.add_argument("--i-confirm-mainnet-real-funds", action="store_true")
    a = ap.parse_args()
    qty = a.qty or {"BTC": "0.01", "ETH": "0.1"}[a.underlying]
    signer = a.signer or a.owner

    s, h = http("GET", f"{SERVER}/health/ready")
    print(f"server {SERVER}: HTTP {s}")
    q = None
    for c in pick(a.underlying, qty):
        intent = dict(c, side="buy", quantity=qty, accepted_differences=ACCEPT, venues=["derive"])
        s, r = http("POST", f"{SERVER}/v1/routes/quote", intent)
        if s == 200 and r.get("executable") and all(l["instrument"]["platform"] == "derive" for l in r["plan"]["legs"]):
            q = r
            break
    if q is None:
        sys.exit("no executable Derive mainnet route (server mode MAINNET_NONCUSTODIAL? OA_DERIVE_NONCUSTODIAL=true? kill switch?)")
    for l in q["plan"]["legs"]:
        print(f"route: BUY {l['qty_native']} {l['instrument']['native_id']} limit {l['limit_price_native']} USDC")
    if not a.i_confirm_mainnet_real_funds:
        sys.exit("Dry run (nothing prepared). Add --i-confirm-mainnet-real-funds to sign and send this ONE IOC order.")

    account = {"owner": a.owner, "signer": signer, "subaccount_id": a.subaccount}
    s, p = http("POST", f"{SERVER}/v1/noncustodial/prepare",
                {"route_id": q["route_id"], "route_hash": q["route_hash"], "account": account})
    if s != 200:
        sys.exit(f"prepare refused ({s}): {p}")
    if len(p["legs"]) != 1 or p["legs"][0]["platform"] != "derive":
        sys.exit("REFUSING TO SIGN: expected exactly one Derive leg")
    for leg in p["legs"]:
        problems, o = decode_leg(leg, a.owner, signer, a.subaccount, qty, a.max_cost)
        if problems:
            sys.exit("REFUSING TO SIGN: " + "; ".join(problems))
        # Shown from the DECODED signed bytes, not from the server's JSON fields.
        print(f"to sign (decoded): BUY {o['amount']} {leg['native_id']} @ <= {o['limit']} USDC, max fee/unit "
              f"{o['max_fee']}, worst cost {o['worst_cost']} USDC, signature valid until {leg['signature_expires_at']}")
    if input("Sign and send this MAINNET order? [y/N] ").strip().lower() != "y":
        sys.exit("cancelled, nothing sent")
    key = getpass.getpass(f"private key of {signer} (hidden, memory only): ").strip()
    acct = Account.from_key(key)
    if acct.address.lower() != signer.lower():
        sys.exit("that key does not control the signer address; nothing sent")
    sigs = []
    for leg in p["legs"]:
        m = encode_typed_data(full_message=leg["typed_data"])
        sigs.append({"order_id": leg["order_id"], "signature": "0x" + acct.sign_message(m).signature.hex().removeprefix("0x")})
    ts = int(time.time() * 1000)
    auth = "0x" + acct.sign_message(encode_defunct(text=str(ts))).signature.hex().removeprefix("0x")
    del key, acct
    t0 = time.time()
    s, r = http("POST", f"{SERVER}/v1/noncustodial/submit",
                {"route_id": p["route_id"], "auth_timestamp_ms": ts, "auth_signature": auth, "signatures": sigs})
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
