"""Reference vectors for Derive v3 order signing, following the "No SDK" example of
https://docs.derive.xyz/authentication/action-signing step by step (eth_abi + eth_account).
PUBLIC TEST KEY keccak("cow") — never holds funds. Output: fixtures/derive_signing_vectors.json.

  python scripts/gen_derive_vectors.py
"""
import json
import os
from decimal import Decimal

from eth_abi import encode as abi_encode
from eth_account import Account
from eth_account.messages import encode_defunct
from eth_utils import keccak, to_checksum_address

MATCHING = "0xeB8d770ec18DB98Db922E9D83260A585b9F0DeAD"
TRADE_MODULE = "0xB8D20c2B7a1Ad2EE33Bc50eF10876eD3035b5e7b"
ACTION_TYPEHASH = bytes.fromhex("4d7a9f27c403ff9c0f19bce61d76d82f9aa29f8d6d4b0c5474607d9770d1af17")
KEY = "0x" + keccak(text="cow").hex().removeprefix("0x")
signer = Account.from_key(KEY)


def e18(v):
    s = int(Decimal(v) * 10**18)
    assert s % 10**6 == 0
    return s


def domain(chain_id):
    return keccak(abi_encode(["bytes32", "bytes32", "bytes32", "uint256", "address"], [
        keccak(b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
        keccak(b"Matching"), keccak(b"1.0"), chain_id, to_checksum_address(MATCHING)]))


CASES = [
    {"chain_id": 11155111, "subaccount_id": 9, "nonce": 1791210000000123456, "expiry": 1791210300,
     "owner": "0x00000000000000000000000000000000DeaDBeef", "asset": "0x1111111111111111111111111111111111111111",
     "sub_id": "39614082545622357598554435968", "limit_price": "310", "amount": "1", "max_fee": "0.01", "is_bid": True},
    {"chain_id": 11155111, "subaccount_id": 123456, "nonce": 1791210000999999999, "expiry": 1791299999,
     "owner": signer.address, "asset": "0xd0711b9eBE84b778483709CDe62BacFDBAE13623",
     "sub_id": "39614158566543496798565322368", "limit_price": "92000.5", "amount": "0.01", "max_fee": "75.25", "is_bid": True},
    {"chain_id": 1, "subaccount_id": 1, "nonce": 1, "expiry": 2, "owner": signer.address,
     "asset": "0x2222222222222222222222222222222222222222", "sub_id": "0", "limit_price": "0.000001",
     "amount": "123.456789012345", "max_fee": "0", "is_bid": False},
]

out = {"signer_address": signer.address, "test_key_note": "keccak('cow') public test key",
       "domain_testnet": "0x" + domain(11155111).hex().removeprefix("0x"),
       "domain_mainnet": "0x" + domain(1).hex().removeprefix("0x"),
       "action_typehash": "0x" + keccak(b"Action(uint256 subaccountId,uint256 nonce,address module,bytes data,uint256 expiry,address owner,address signer)").hex().removeprefix("0x"),
       "cases": []}
for c in CASES:
    data = abi_encode(["address", "uint256", "int256", "int256", "uint256", "uint256", "bool"],
                      [to_checksum_address(c["asset"]), int(c["sub_id"]), e18(c["limit_price"]), e18(c["amount"]), e18(c["max_fee"]), c["subaccount_id"], c["is_bid"]])
    action = keccak(abi_encode(["bytes32", "uint256", "uint256", "address", "bytes32", "uint256", "address", "address"],
                               [ACTION_TYPEHASH, c["subaccount_id"], c["nonce"], to_checksum_address(TRADE_MODULE), keccak(data), c["expiry"], to_checksum_address(c["owner"]), signer.address]))
    digest = keccak(b"\x19\x01" + domain(c["chain_id"]) + action)
    sig = Account.unsafe_sign_hash(digest, KEY).signature
    out["cases"].append({**{k: (str(v) if isinstance(v, int) and not isinstance(v, bool) else v) for k, v in c.items()},
                         "encoded_data": "0x" + data.hex(), "digest": "0x" + digest.hex().removeprefix("0x"),
                         "signature": "0x" + sig.hex().removeprefix("0x")})
ts = "1791210000000"
out["login"] = {"timestamp": ts, "signature": "0x" + Account.sign_message(encode_defunct(text=ts), KEY).signature.hex().removeprefix("0x")}
p = os.path.join(os.path.dirname(__file__), "..", "fixtures", "derive_signing_vectors.json")
json.dump(out, open(p, "w"), indent=1)
print("written", os.path.abspath(p), "domain_testnet", out["domain_testnet"])
