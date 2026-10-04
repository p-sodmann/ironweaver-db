# ADR 0047: Authentication's default, the first admin, and plaintext until TLS

Status: accepted
Date: 2026-10-04

## Context

Authentication (ADRs 0043 to 0046) has to be turned on somehow, and the first user has to come from somewhere. A default password (`admin`/`admin`) is the classic way to ship an exposed database; there must not be one. And until TLS (step 15b), a password sent to the server crosses the network in clear. Points to decide: the default of `[auth] enabled`, how the first admin is made, what a server with authentication on and no users does, and what replaces step 16b's `[console] public` switch.

## Decision

**Authentication is on by default** (`[auth] enabled = true`, `IWDB_AUTH_ENABLED`). Step 15's goal is a server safe to put on a network; an opt-in would leave every forgotten deployment open. Turning it off is one setting, and logs a warning at start.

**No default password. The first admin comes from one of two places:**

- **Offline:** `iwctl user create <data-dir> <name> --admin`, with the server stopped. It opens the store in process (ADR 0045: the file permissions are the boundary) and prompts for the password.
- **The first start:** `IWDB_AUTH_BOOTSTRAP_PASSWORD` (and `IWDB_AUTH_BOOTSTRAP_USER`, default `admin`). A start that finds no users creates that admin and logs it (without the password); a start that finds users ignores the variables with a warning, so leaving them set can't reset a password. They are environment variables only: a password doesn't belong in a config file, and `--check-config` never prints them. An empty variable counts as unset (compose's `${VAR:-}`). They are exempt from the "unknown `IWDB_AUTH_*` variable" check (ADR 0039).

**A server with authentication on and no users refuses to start**, after recovery (it needs the store to know), with exit code 1 and a message that names both ways and the switch to turn authentication off. Nothing is served but health meanwhile.

**Plaintext until TLS.** Until step 15b the server speaks plain TCP. Listening on a non-loopback address now needs `[server] plaintext_public = true` (`IWDB_SERVER_PLAINTEXT_PUBLIC`), with authentication on (passwords, tokens and data cross the network in clear) or off (anyone who reaches the port can change everything). The message of the refusal says which. This replaces `[console] public` of step 16b (ADR 0041): the console is behind the login now, and what remains to opt into is the lack of TLS, for the whole server rather than the console. `[console] public` and `IWDB_CONSOLE_PUBLIC` are refused with a message that names the replacement.

**The Docker image** listens on `0.0.0.0` inside the container and doesn't set the flag: `docker run -e IWDB_SERVER_PLAINTEXT_PUBLIC=true -e IWDB_AUTH_BOOTSTRAP_PASSWORD=...`, with the port published on localhost or a private network. `compose.yaml` sets the flag (it publishes on `127.0.0.1` only) and takes the first admin's password from `IWDB_ADMIN_PASSWORD`. CI's docker job checks that the image refuses to start without the flag, then logs in.

**The other settings** (`[auth]`): `session_lifetime_secs` (43 200), `login_max_failures` (5), `login_window_secs` (60), `login_table_size` (10 000) (ADR 0044). The hash parameters are a library setting (`AuthSettings::hash`), not a config key yet: raising them is rare, and a wrong value would make every login slow.

## Consequences

- An upgrade of a server that ran without authentication fails at its first start until an admin exists: the release notes and `config.md` say how (either way above, or `IWDB_AUTH_ENABLED=false`).
- A server on a non-loopback address that ran fine in step 16b now needs the plaintext flag; step 15b turns the default into TLS.
- The test harnesses (Rust binary tests, the Python fixture) bootstrap an admin and log in; the in-process `Server` of the Rust tests chooses with `Server::auth`.

## Update (step 15b)

TLS came with step 15b and is on by default ([ADR 0048](0048-tls-and-mtls.md)). `[server] plaintext_public` stays, with a narrower meaning: plaintext on a non-loopback address, which also needs `[tls] enabled = false`; with TLS on it has no effect and a warning says so. The image's config expects a mounted certificate instead of the plaintext flag, and CI's docker job checks that the image refuses to start without one.
