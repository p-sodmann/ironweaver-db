# Step 15b: TLS and mTLS

Status: done
Milestone: M4 Production 1.0
Depends on: step 15a (the principal that a client certificate maps to)

Split out of step 15 on 2026-10-04 (see its "Plan change").

## Goal

Passwords, tokens and data never cross the network in clear unless the operator says so explicitly; clients can authenticate with certificates.

## Tasks

- [x] TLS with rustls for gRPC and REST on the one port (ALPN `h2` and `http/1.1`), on by default: `[tls] cert`, `key` (PEM files), reloaded on SIGHUP or by a file watch (decide in the ADR). Plaintext only with an explicit `[tls] enabled = false`, and then a non-loopback address needs a second explicit flag (the rule 15a set for passwords in clear).
- [x] mTLS: `[tls] client_ca`; a verified client certificate maps to a principal (subject or SAN to user, decided in the ADR), as an authenticator next to 15a's tokens. Optional or required per config.
- [x] Clients: `client::Remote` and `RestRemote`, Python `connect` and `iwctl` take a CA, and a client certificate and key; `https://` endpoints. The console works over HTTPS (Secure cookie, if 15a chose cookies).
- [x] `iwdb-server --probe` and the Docker `HEALTHCHECK` over TLS. `compose.yaml` with a self-signed certificate for development (a script makes one); no certificate in the image.
- [x] Revisit 15a's plaintext flag and ADR 0041's `public`; `guarantees.md` updated.
- [x] Tests: handshake over gRPC and REST, a wrong CA refused, an expired certificate refused, mTLS principal mapping and its role checks, plaintext refused on a non-loopback address without the flags. The conformance suite runs over TLS.
- [x] ADR (TLS defaults, certificate reload, mTLS mapping); config.md, grpc.md, rest.md.

## Decisions

[ADR 0048](../adr/0048-tls-and-mtls.md):

- **Reload:** SIGHUP, not a file watch. A reload that fails keeps the certificate in use.
- **mTLS mapping:** the subject's common name (CN) is an existing user. A bearer token or session cookie in the same request wins.
- **Optional by default:** `[tls] client_auth = "required"` refuses requests without a certificate in the gate, so health stays open.
- **The plaintext flags:** `[server] plaintext_public` stays as the second flag. ADR 0041's `[console] public` stays refused.
- **The probe:** `iwdb-server --probe` reads the configuration and doesn't verify the certificate.
- **Development certificates:** `docker/dev-cert.sh` (openssl or LibreSSL) makes them, and `compose.yaml` mounts them read-only. The image holds none.

Beyond the plan:

- **CSRF:** a REST write authenticated by a client certificate alone needs the CSRF header, as one authenticated by the cookie does.
- **Connection failures:** the Rust clients report a connection that fails before the server answers as `unavailable`, not `internal`.
- **Tests without `rest`:** `crates/iwdb-server/tests/tls.rs` needs `rest`. A gRPC-only build still runs the gRPC conformance suite over TLS.

## Acceptance criteria

- [x] A default server speaks only TLS; plaintext needs explicit flags (`tls_is_the_default_and_plaintext_needs_explicit_flags`, `plaintext_needs_explicit_flags_and_tls_needs_a_certificate`, CI's docker job).
- [x] A client with a valid certificate is authenticated as its mapped principal; without one (when required) it is refused (`a_client_certificate_is_its_users_principal_with_its_roles`, `a_required_client_certificate_leaves_only_health_and_the_pages_open`; also in Python and `iwctl`).

## Non-goals

- Certificate management (ACME, rotation automation).
- OIDC/JWT.
