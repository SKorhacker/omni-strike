"""NON-CUSTODIAL Derive TESTNET demo: quote -> prepare -> sign LOCALLY (your test key plays the
user's wallet) -> submit. The Omni Strike server never sees the key.

  python scripts/noncustodial_derive.py --i-confirm-testnet-order

Needs: testnet server (cargo run --features testnet ...) started with OA_DERIVE_NONCUSTODIAL=true,
and OA_DERIVE_TESTNET_{WALLET,SUBACCOUNT_ID,SIGNER_KEY} in .env.testnet (read here only, never printed).
Places ONE small IOC order on testnet.api.derive.xyz (test funds).
"""
import argparse
import sys
import time

from eth_account import Account
from eth_account.messages import encode_defunct, encode_typed_data

import testnet_aevo as common  # load_env, http, SERVER (testnet server, :8081)
import testnet_derive as td    # validate, pick of a liquid contract

ACCEPT = ["SETTLEMENT_INDEX_DIFFERS", "SETTLEMENT_WINDOW_UNKNOWN", "SETTLEMENT_WINDOW_DIFFERS",
          "SETTLEMENT_ASSET_DIFFERS", "PREMIUM_CURRENCY_DIFFERS"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--i-confirm-testnet-order", action="store_true")
    ap.add_argument("--underlying", default="ETH")
    a = ap.parse_args()
    env = common.load_env()
    problems, info = td.validate(env)
    if problems:
        print("\n".join("PROBLEM: " + p for p in problems)); print(td.HOW_TO); sys.exit(2)
    if not a.i_confirm_testnet_order:
        print("Dry run. Add --i-confirm-testnet-order to place ONE small IOC order on Derive TESTNET."); sys.exit(0)
    key = env["OA_DERIVE_TESTNET_SIGNER_KEY"]
    account = {"owner": env["OA_DERIVE_TESTNET_WALLET"], "signer": info["signer_address"],
               "subaccount_id": int(env["OA_DERIVE_TESTNET_SUBACCOUNT_ID"])}
    q = None
    for c in common.pick_liquid({"BTC": "0.01", "ETH": "0.1"}, platform="derive"):
        if c["underlying"] != a.underlying:
            continue
        intent = {"underlying": c["underlying"], "option_kind": c["option_kind"], "strike": c["strike"],
                  "expiry": c["expiry"], "side": "buy", "quantity": c["qty"], "accepted_differences": ACCEPT,
                  "venues": ["derive"]}
        s, r = common.http("POST", f"{common.SERVER}/v1/routes/quote", intent)
        if s == 200 and r.get("executable"):
            q = r
            break
    if q is None:
        print("no executable Derive route found (is the testnet server running with OA_DERIVE_NONCUSTODIAL=true?)"); sys.exit(1)
    s, p = common.http("POST", f"{common.SERVER}/v1/noncustodial/prepare",
                       {"route_id": q["route_id"], "route_hash": q["route_hash"], "account": account})
    if s != 200:
        print("prepare failed:", s, p); sys.exit(1)
    sigs = []
    for leg in p["legs"]:
        m = encode_typed_data(full_message=leg["typed_data"])
        sig = Account.sign_message(m, key).signature.hex()
        sigs.append({"order_id": leg["order_id"], "signature": "0x" + sig.removeprefix("0x")})
        print(f"signed leg {leg['leg_index']}: {leg['side']} {leg['qty_native']} {leg['native_id']} @ <= {leg['limit_price_native']}")
    ts = int(time.time() * 1000)
    auth = Account.sign_message(encode_defunct(text=str(ts)), key).signature.hex()
    t0 = time.time()
    s, r = common.http("POST", f"{common.SERVER}/v1/noncustodial/submit",
                       {"route_id": p["route_id"], "auth_timestamp_ms": ts, "auth_signature": "0x" + auth.removeprefix("0x"), "signatures": sigs})
    print(f"submit HTTP {s} in {1000 * (time.time() - t0):.0f} ms")
    if isinstance(r, dict):
        for o in r.get("orders", []):
            print(f"  {o['native_id']}: {o['state']} filled {o['filled_native']} avg {o.get('avg_fill_price_native')} err {o.get('last_error')}")
        for n in r.get("notes", []):
            print("  note:", n)
    else:
        print(r)


if __name__ == "__main__":
    main()
