# Step 15: Security

Status: in progress (split into 15a to 15d)
Milestone: M4 Production 1.0
Depends on: step 11 (can run in parallel with steps 12 to 14)

## Goal

Safe to expose on a network: encrypted, authenticated, authorised and resource-limited.

## Tasks

- [x] TLS on by default for gRPC and REST; plaintext only with an explicit flag (15b).
- [x] Authentication: users with passwords, API tokens (15a) and mTLS (15b); hooks for OIDC/JWT later (a principal comes from `Authenticate`, ADRs 0045 and 0048).
- [x] Authorisation: roles per namespace (read / write / admin), stored durably with one write path (like Postgres roles and GRANT) (15a).
- [ ] Resource limits per client and namespace: query budgets, rate limits, max memory per namespace (reject writes above it; built on step 16d's memory accounting and process-wide limit) (15d).
- [ ] Audit log of admin and catalog operations (15c).
- [ ] `SECURITY.md` with a disclosure process. No telemetry (15c).

## Plan change

2026-10-04: step 15 is too big for one PR, so it was split into ordered sub-steps, like step 16 into 16b to 16i. Step 15 is done when they are. The tasks above stay as the overview; each is ticked when its sub-step is done. The owner asked for password login first, so the console and the API are behind authentication before the console work continues (step 16c on); "static API tokens" became users with passwords plus API tokens, and the roles are no longer "in the catalog" (a namespace's catalog can't hold server-wide users; where they live is 15a's ADR).

| Sub-step | Covers | Why here |
|---|---|---|
| [15a](step_15a.md) | users, passwords, sessions and API tokens; roles per namespace and a server-wide admin; the authorisation point; the console's login; `iwctl`, Python and Rust clients log in | Everything else needs a principal: mTLS maps a certificate to one, the audit log names one, limits are counted per one. Comes first so the console's later work (16c on) is built behind it. |
| [15b](step_15b.md) | TLS by default (rustls), plaintext only with an explicit flag; mTLS | Without it 15a's passwords cross the network in clear, so it follows right after; mTLS is a second authenticator for 15a's principals. |
| [15c](step_15c.md) | audit log; `SECURITY.md` | Needs only 15a's principal and authorisation point (where entries are emitted). Small; can run in parallel with 15b. |
| [15d](step_15d.md) | rate limits, budgets, concurrency and memory per client and namespace; the stress test | Needs 15a (who is counted), 16c (registry, metrics) and 16d (memory accounting), so it comes last and after them. |

## Acceptance criteria

- Tests for every role × operation combination, over both gRPC and REST (15a).
- One client over its limits can't starve another (stress test) (15d).
