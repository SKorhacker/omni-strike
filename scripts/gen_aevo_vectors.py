"""Generate EIP-712 reference vectors for Aevo order signing with the reference implementation
(eth_account). The key below is a PUBLIC TEST KEY (keccak("cow"), from the EIP-712 spec
examples) and must never hold funds. Output: fixtures/aevo_signing_vectors.json.

  python scripts/gen_aevo_vectors.py
"""
import json
import os

from eth_account import Account
from eth_account.messages import encode_typed_data
from eth_utils import keccak

TEST_KEY = "0x" + keccak(text="cow").hex().removeprefix("0x")
ORDER_TYPES = {
    "Order": [
        {"name": "maker", "type": "address"},
        {"name": "isBuy", "type": "bool"},
        {"name": "limitPrice", "type": "uint256"},
        {"name": "amount", "type": "uint256"},
        {"name": "salt", "type": "uint256"},
        {"name": "instrument", "type": "uint256"},
        {"name": "timestamp", "type": "uint256"},
    ]
}
DOMAINS = {
    "testnet": {"name": "Aevo Testnet", "version": "1", "chainId": 11155111},
    "mainnet": {"name": "Aevo Mainnet", "version": "1", "chainId": 1},
}
CASES = [
    {"domain": "testnet", "maker": "0x0000000000000000000000000000000000000001", "isBuy": True,
     "limitPrice": 101700000, "amount": 1000000, "salt": 123456, "instrument": 127909, "timestamp": 1791200000},
    {"domain": "testnet", "maker": "0xCD2a3d9F938E13CD947Ec05AbC7FE734Df8DD826", "isBuy": False,
     "limitPrice": 1, "amount": 10000, "salt": 999999999999, "instrument": 1, "timestamp": 1690434000},
    {"domain": "mainnet", "maker": "0x00000000000000000000000000000000DeaDBeef", "isBuy": True,
     "limitPrice": 2**64 + 5, "amount": 2 * 10**6, "salt": 0, "instrument": 142360, "timestamp": 1791273600},
]

acct = Account.from_key(TEST_KEY)
out = {"signer_address": acct.address, "test_key_note": "keccak('cow') public test key", "cases": []}
for c in CASES:
    msg = {k: v for k, v in c.items() if k != "domain"}
    signable = encode_typed_data(domain_data=DOMAINS[c["domain"]], message_types=ORDER_TYPES, message_data=msg)
    signed = Account.sign_message(signable, private_key=TEST_KEY)
    out["cases"].append({
        **{k: (str(v) if isinstance(v, int) and not isinstance(v, bool) else v) for k, v in c.items()},
        "digest": "0x" + signed.message_hash.hex().removeprefix("0x"),
        "signature": "0x" + signed.signature.hex().removeprefix("0x"),
    })
path = os.path.join(os.path.dirname(__file__), "..", "fixtures", "aevo_signing_vectors.json")
with open(path, "w") as f:
    json.dump(out, f, indent=1)
print("written", os.path.abspath(path), "signer", acct.address)
