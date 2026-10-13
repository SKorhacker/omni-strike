"""Signer for the wallet end-to-end check (scripts/wallet-e2e.ts). THROWAWAY key only.

Reads one JSON request on stdin, prints the signature:
  {"method": "eth_signTypedData_v4", "key": "0x..", "typed_data": {...}}
  {"method": "personal_sign", "key": "0x..", "hex": "0x.."}
  {"method": "address", "key": "0x.."}
Implements what a browser wallet does, with eth_account, so the page's code is checked against an
independent EIP-712 / EIP-191 implementation. Never use a key that holds funds.
"""

import json
import sys

from eth_account import Account
from eth_account.messages import encode_defunct, encode_typed_data

req = json.load(sys.stdin)
acct = Account.from_key(req["key"])
m = req["method"]
if m == "address":
    print(acct.address.lower())
elif m == "eth_signTypedData_v4":
    sig = acct.sign_message(encode_typed_data(full_message=req["typed_data"])).signature
    print("0x" + sig.hex().removeprefix("0x"))
elif m == "personal_sign":
    sig = acct.sign_message(encode_defunct(hexstr=req["hex"])).signature
    print("0x" + sig.hex().removeprefix("0x"))
else:
    sys.exit(f"unknown method {m}")
