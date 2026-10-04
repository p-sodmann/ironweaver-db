# ADR 0044: Password hashing, sessions and API tokens

Status: accepted
Date: 2026-10-04

## Context

Step 15a logs users in with passwords and gives scripts API tokens. Points to decide: how passwords are stored and checked, what a login returns, how long it lasts and how it ends, what an API token is, how failed logins are slowed down, and how secrets stay out of logs and error messages.

## Decision

**Passwords: argon2id**, with RustCrypto's `argon2` crate (pure Rust, the RustCrypto project's password-hashing implementation of RFC 9106). Stored as a PHC string (`$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>`), which records the parameters, so they can be raised later. A new hash uses the configured parameters (default: 19 MiB, 2 passes, 1 lane, OWASP's recommendation) and a 16-byte salt from the OS (`getrandom`); a successful login whose hash has other parameters replaces it with the current ones. Verification is argon2's, which takes the parameters from the hash and compares the results in constant time. Passwords are 8 to 1024 bytes (the upper bound limits the work an unauthenticated request can cause).

**Unknown users take as long as wrong passwords**: a login for a name that doesn't exist verifies against a dummy hash with the current parameters. Both answer `unauthenticated` with the same message, "wrong user or password".

**A login returns a session token.** 256 random bits from the OS, as `iwdb_` and 64 hex digits (the prefix lets secret scanners find leaked ones). The server keeps only its SHA-256 (a fast hash is enough for 256 random bits, and lookups stay O(1)), with the user, the user's password epoch at login, and an expiry (`[auth] session_lifetime_secs`, default 12 hours).

- **Sessions are in memory**, in the server process. A restart logs everyone out. This keeps logins off the WAL (a commit per login would make the system namespace grow with every browser tab) and makes revocation trivial. The table is bounded (65 536 sessions; at the limit the one that expires first goes).
- **A session ends** at its expiry, at logout, when the user's password changes (its epoch grows, checked on every request, also when another process changed it), and when the user is deleted (the record is gone).

**API tokens** are the same kind of bearer token, durable: a node `token:<sha256 hex>` in the system namespace (ADR 0043), with a name unique per user, a creation time and an optional expiry (none by default). Shown once, when made. They survive restarts and password changes (as GitHub's personal access tokens do) and end when revoked by name, when they expire, or with their user.

**Authentication** of a request: the gate hashes the bearer token and looks it up among the sessions, then among the API tokens (one node read), then reads the user (one node read) for its admin flag and grants. No cache: a revoked grant takes effect at the next request.

**Failed logins are slowed down** per user name and per client address: after `login_max_failures` (default 5) failures within `login_window_secs` (default 60), further logins of that user or from that address are refused (`unauthenticated`, "too many failed logins: try again in N s") without checking the password, until the window has passed. A success clears the user's count. The table is bounded (`login_table_size`, default 10 000 entries; the least recently seen goes first). The per-user limit means an attacker can lock a known user out for a minute at a time; per-address alone would let a botnet guess one user's password freely. Both are counted.

**Secrets never leave as text.** Passwords and tokens travel inside the code as `iwdb_query::Secret`, whose `Debug` and `Display` print `***`; only hashing, comparing and sending read them. No error message contains one (a malformed `authorization` header is refused without echoing it). Failed logins are logged at `warn` (target `iwdb::auth`) with the user name (quoted, so control characters can't forge log lines) and the client address. A test runs the binary at `debug` through logins, failures, tokens and password changes and checks that no line holds any of the secrets.

New dependencies, in `iwdb` only: `argon2` (password hashes; RustCrypto), `sha2` (token hashes; RustCrypto, already in the tree) and `getrandom` (salts and tokens from the OS; already in the tree). They are declared in `crates/iwdb/Cargo.toml` rather than the workspace manifest.

## Consequences

- A login costs one argon2 verification (tens of milliseconds at the default parameters, on a worker thread, not the async runtime). Requests after it cost a SHA-256 and two node reads.
- A restart ends every session; scripts should use API tokens.
- Raising the hash parameters needs no migration: old hashes still verify and are replaced at the next login.
- The slowdown is per server process and forgotten at a restart; it limits online guessing, not offline attacks on a stolen data directory (whose file permissions are the boundary, ADR 0045).
