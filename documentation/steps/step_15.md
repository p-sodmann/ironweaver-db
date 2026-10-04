# Step 15: Security

Status: todo
Milestone: M4 Production 1.0
Depends on: step 11 (can run in parallel with steps 12 to 14)

## Goal

Safe to expose on a network: encrypted, authenticated, authorised and resource-limited.

## Tasks

- [ ] TLS on by default for gRPC and REST; plaintext only with an explicit flag.
- [ ] Authentication: static API tokens and mTLS; hooks for OIDC/JWT later.
- [ ] Authorisation: roles per namespace (read / write / admin), stored in the catalog (like Postgres roles and GRANT).
- [ ] Resource limits per client and namespace: query budgets, rate limits, max memory per namespace (reject writes above it; built on step 16d's memory accounting and process-wide limit).
- [ ] Audit log of admin and catalog operations.
- [ ] `SECURITY.md` with a disclosure process. No telemetry.

## Acceptance criteria

- Tests for every role × operation combination, over both gRPC and REST.
- One client over its limits can't starve another (stress test).
