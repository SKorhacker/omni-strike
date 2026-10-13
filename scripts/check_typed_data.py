"""Independent check: the EIP-712 typed data we hand to wallets hashes to the digest we verify.

Run after `cargo test -p oa-adapters export_typed_data_vector` (writes target/derive_typed_data_vector.json).
"""
import json, sys
from eth_account.messages import encode_typed_data
from eth_utils import keccak

# Optional argument: another vector file (e.g. target/derive_mainnet_typed_data_vector.json).
v = json.load(open(sys.argv[1] if len(sys.argv) > 1 else "target/derive_typed_data_vector.json"))
td = v["typed_data"]
m = encode_typed_data(full_message=td)
digest = "0x" + keccak(b"" + m.version + m.header + m.body).hex()
print("eth_account digest:", digest)
print("omni strike digest:", v["digest"])
sys.exit(0 if digest.lower() == v["digest"].lower() else 1)
