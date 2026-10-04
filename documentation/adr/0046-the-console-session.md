# ADR 0046: The console's session: an HttpOnly cookie and a CSRF header

Status: accepted
Date: 2026-10-04

## Context

The operator console (ADR 0037, served by the server per ADR 0041) is a set of static pages that read and write through the REST API on their own origin. With authentication (ADR 0045) it needs a login, and the browser has to hold the session between requests and pages. The two usual ways:

- **A bearer token in `sessionStorage`** (never `localStorage`, which outlives the tab and is shared by every tab). Any script running in the page, including one injected through an XSS hole, can read it and send it elsewhere, where it stays valid until it expires. The console renders data written by others (node attributes, labels); React escapes text, but one mistake in the vendored components would turn into a stolen token.
- **An HttpOnly cookie.** No script can read it, so an XSS hole can act only while the page is open, as the logged-in user, and can't take the session away. But the browser sends cookies by itself, so the server must make sure a request comes from the console and not from another page (CSRF).

## Decision

**The session is a cookie**, `iwdb_session`, set by `POST /v1/auth/login` with `"cookie": true` (the answer's `token` is then empty, so the console's script never sees the token): `HttpOnly; SameSite=Strict; Path=/; Max-Age=<the session's lifetime>`. Logout clears it (`Max-Age=0`) and ends the session on the server. The console's REST Source logs in this way; other clients send the bearer header.

**Not `Secure` until the server has TLS (step 15b).** A `Secure` cookie isn't stored over plain HTTP, except on `localhost` in some browsers, so it would break the console on any other address while adding nothing: without TLS the cookie, like the password, crosses the network in clear. Step 15b adds `Secure` together with HTTPS.

**CSRF.** Two layers:

1. `SameSite=Strict`: browsers don't attach the cookie to requests started by another site. That covers other sites, but "site" ignores the port: another application on `localhost:3000` is the same site as a server on `localhost:7600`.
2. **A request authenticated by the cookie with a method other than GET or HEAD must carry `X-Iwdb-Csrf`** (any value), checked in the gate; otherwise `permission_denied`. A page of another origin can't add a custom header without a CORS preflight, which the server never answers (ADR 0030). The console's REST Source sends it with every request.

The JSON-only content-type guard of ADR 0030 isn't enough on its own: it refuses non-empty bodies that aren't `application/json`, but several routes take an empty body (creating and dropping a namespace, logout, revoking a grant or a token), and a cross-site HTML form can send those with any content type. The CSRF header closes that. GET and HEAD stay possible with the cookie alone because they change nothing and their answers can't be read cross-origin; that is what lets the change stream's `EventSource` (which can't set headers) work with the cookie.

**The pages.** Both pages run inside a gate (`IW.ui.AuthGate`) that asks the Source for the session (`GET /v1/auth/whoami`) and shows a login (the design system's controls and words) without one. When a later call answers 401, the Source tells its listeners (`onAuth`) and the login appears over the page, which stays mounted, so staged edits survive a lost session. The rail shows the user and LOG OUT. A server with authentication off answers the session at once and shows no login. On a plain-HTTP page off localhost, the login says that the password crosses the network in clear.

**The mock** has a login too (`admin` / `admin`, `reader` / `reader`), remembered for the tab in `sessionStorage`: a flag of the mock, not a credential.

**`serve.py`** (the development proxy) passes `Authorization`, `Cookie` and `X-Iwdb-Csrf` through and the server's `Set-Cookie` back. The cookie has no `Domain`, so it belongs to whichever origin served the page (the proxy's or the server's).

## Consequences

- An XSS hole in the console could act as the logged-in user while the page is open, but couldn't steal the session.
- Scripts and the other clients are unaffected: they use bearer tokens, which need no CSRF header.
- Checked in a real browser (headless Chromium) against a server on `/console/` and through `serve.py`: the cookie is HttpOnly and SameSite=Strict, `document.cookie` and the storages don't hold it, a read user's commit is 403, and logout returns to the login. The REST tests check the cookie's attributes, the CSRF rule and logout.
