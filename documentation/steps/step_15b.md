# Step 15b: TLS and mTLS

Status: todo
Milestone: M4 Production 1.0
Depends on: step 15a (the principal that a client certificate maps to)

Split out of step 15 on 2026-10-04 (see its "Plan change").

## Goal

Passwords, tokens and data never cross the network in clear unless the operator says so explicitly; clients can authenticate with certificates.

## Tasks

- [ ] TLS with rustls for gRPC and REST on the one port (ALPN `h2` and `http/1.1`), on by default: `[tls] cert`, `key` (PEM files), reloaded on SIGHUP or by a file watch (decide in the ADR). Plaintext only with an explicit `[tls] enabled = false`, and then a non-loopback address needs a second explicit flag (the rule 15a set for passwords in clear).
- [ ] mTLS: `[tls] client_ca`; a verified client certificate maps to a principal (subject or SAN to user, decided in the ADR), as an authenticator next to 15a's tokens. Optional or required per config.
- [ ] Clients: `client::Remote` and `RestRemote`, Python `connect` and `iwctl` take a CA, and a client certificate and key; `https://` endpoints. The console works over HTTPS (Secure cookie, if 15a chose cookies).
- [ ] `iwdb-server --probe` and the Docker `HEALTHCHECK` over TLS. `compose.yaml` with a self-signed certificate for development (a script makes one); no certificate in the image.
- [ ] Revisit 15a's plaintext flag and ADR 0041's `public`; `guarantees.md` updated.
- [ ] Tests: handshake over gRPC and REST, a wrong CA refused, an expired certificate refused, mTLS principal mapping and its role checks, plaintext refused on a non-loopback address without the flags. The conformance suite runs over TLS.
- [ ] ADR (TLS defaults, certificate reload, mTLS mapping); config.md, grpc.md, rest.md.

## Acceptance criteria

- A default server speaks only TLS; plaintext needs explicit flags.
- A client with a valid certificate is authenticated as its mapped principal; without one (when required) it is refused.

## Non-goals

- Certificate management (ACME, rotation automation).
- OIDC/JWT.
