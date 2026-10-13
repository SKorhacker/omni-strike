"""Independent check of our createOpenPosition encoding with eth_abi (run after
`cargo test -p oa-adapters builds_a_buy_request`)."""
import json, sys
from eth_abi import decode
from eth_utils import keccak

tx = json.load(open("target/callput_open_vector.json"))
data = bytes.fromhex(tx["data"][2:])
sig = "createOpenPosition(uint16,uint8,bool[4],bytes32[4],bool[4],uint256,address[],uint256,uint256,address)"
assert data[:4] == keccak(text=sig)[:4] == bytes.fromhex("4fd5f4af")
v = decode(["uint16", "uint8", "bool[4]", "bytes32[4]", "bool[4]", "uint256", "address[]", "uint256", "uint256", "address"], data[4:])
checks = {
    "underlyingAssetIndex=1 (BTC, from optionId)": v[0] == 1,
    "length=1": v[1] == 1,
    "isBuys": v[2] == (True, False, False, False),
    "optionId": v[3][0].hex() == "0001006ad1d98000000001349800000000000000000000000000000000000000",
    "isCalls": v[4] == (True, False, False, False),
    "minSize=0.1e18": v[5] == 10**17,
    "path=[USDC]": [a.lower() for a in v[6]] == ["0x833589fcd6edb6e08f4c7c32d4f71b54bda02913"],
    "amountIn=650.5 USDC": v[7] == 650_500_000,
    "minOut=0": v[8] == 0,
    "leadTrader=0": int(v[9], 16) == 0,
}
for k, ok in checks.items():
    print(("OK  " if ok else "FAIL"), k)
sys.exit(0 if all(checks.values()) else 1)
