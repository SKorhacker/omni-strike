"""Independent check of our fillOrder ABI encoding with eth_abi (run after
`cargo test -p oa-adapters builds_a_fill_for_the_cheapest_order`)."""
import json, sys
from eth_abi import decode
from eth_utils import keccak

v = json.load(open("target/thetanuts_fill_vector.json"))
data = bytes.fromhex(v["data"][2:])
sig = "fillOrder((address,uint256,address,bool,address,address,bool,uint256,uint256[],uint256,uint256,uint256,bytes),bytes,address)"
assert data[:4] == keccak(text=sig)[:4], "selector"
order, signature, referrer = decode(["(address,uint256,address,bool,address,address,bool,uint256,uint256[],uint256,uint256,uint256,bytes)", "bytes", "address"], data[4:])
o = v["order"]
checks = {
    "maker": order[0].lower() == o["maker"].lower(),
    "orderExpiryTimestamp": order[1] == int(o["orderExpiryTimestamp"]),
    "collateral": order[2].lower() == o["collateral"].lower(),
    "isCall": order[3] == o["isCall"],
    "priceFeed": order[4].lower() == o["priceFeed"].lower(),
    "implementation": order[5].lower() == o["implementation"].lower(),
    "isLong": order[6] == o["isLong"],
    "maxCollateralUsable": order[7] == int(o["maxCollateralUsable"]),
    "strikes": list(order[8]) == [int(s) for s in o["strikes"]],
    "expiry": order[9] == int(o["expiry"]),
    "price": order[10] == int(o["price"]),
    "numContracts": order[11] == int(v["num_contracts"]),
    "extraOptionData": order[12] == bytes.fromhex((o.get("extraOptionData") or "0x")[2:]),
    "signature": signature == bytes.fromhex(v["signature"][2:]),
    "referrer": int(referrer, 16) == 0,
}
for k, ok in checks.items():
    print(("OK  " if ok else "FAIL"), k)
sys.exit(0 if all(checks.values()) else 1)
