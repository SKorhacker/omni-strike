# Security policy

Omni Strike builds and forwards trading orders, so security reports are taken seriously.

## Reporting a vulnerability

**Do not open a public issue.** Use GitHub's private reporting:
[Report a vulnerability](https://github.com/SKorhacker/omni-strike/security/advisories/new).

Include the affected version or commit, the component (server, adapters, web UI, SDK, deploy),
reproduction steps and the impact you expect. You will get an acknowledgement within 7 days.
Please give us a reasonable time to ship a fix before disclosing.

Never send private keys, seed phrases or API secrets in a report — a key that has been shared
anywhere must be considered compromised and replaced.

## Scope

In scope, with priority:

1. Anything that lets a third party place, modify or replay an order, or move funds, in
   `MAINNET_NONCUSTODIAL` or `TESTNET` mode (signature verification, single-use routes and
   prepared executions, caps, kill switch, mode gates).
2. Authentication and authorization bypass (API token, integrator keys, admin token,
   wallet-signed endpoints), cross-user data access.
3. SSRF, injection, request smuggling, denial of service with a small request.
4. Leaks of secrets or user data in logs, errors, the database or container images.
5. Web UI flaws that could make a user sign something other than what is displayed.

Out of scope: findings that require a malicious operator (the operator controls the server by
design), venue-side bugs, missing hardening on a deployment that does not follow
`docs/deployment.md`, and rate-limit tuning.

## Supported versions

Only the latest commit on `main` receives fixes.

## Design summary

See [README › Security model](README.md#security-model) and
[docs/mainnet-noncustodial.md](docs/mainnet-noncustodial.md) for the guarantees enforced in code.
