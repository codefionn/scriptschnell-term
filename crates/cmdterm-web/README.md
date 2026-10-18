# cmdterm-web

A web server that lets you reach your scriptschnell-term machines from a browser or a
phone. You log in, link machines to your account, and open their terminals. The page is
an installable web app (PWA) that keeps working offline with what it saw last.

The native app stays the way to work on code. This is for the times you only have a
browser: checking on a build from your phone, or answering an agent while away from your
desk.

## How it fits together

```
browser / phone ──HTTPS + WebSocket──▶ cmdterm-web ◀──TCP, hybrid handshake── cmdterm-remote server
                                                                         (laptop, desktop app, VM host)
```

Machines dial out to the web server, so they can sit behind NAT or move between networks.
A machine is any `cmdterm-remote` server: `cmdterm-remote serve`, or the desktop app
when it serves this machine (Settings, Remote machines). The connection uses the same
hybrid post-quantum handshake as remote machines (`crates/cmdterm-remote/README.md`),
with the roles of the TCP connection reversed: the machine connects, logs in with its host
key, and then serves channels to the web server as if it were a client. The machine
checks the web server's key against the one it pinned when it was linked.

The web server sees what passes through a terminal, the same way an SSH jump host would.
Run it on a machine you trust.

## Running it

```sh
cmdterm-web user add alice              # asks for a password
cmdterm-web serve --data-dir /var/lib/cmdterm-web
```

For development, `--dev` creates a user `dev` and prints a login link with a token that
lasts until the process exits. The token rides in the URL fragment, which the browser
never sends to the server, and the page POSTs it, so it stays out of logs and history:

```sh
cmdterm-web serve --data-dir "$SCRATCH/web" --dev
# cmdterm-web: dev login: http://127.0.0.1:8080/auth/token#token=...
```

### Linking a machine

In the web app, open Machines, then Link a machine. It shows a code that works once, for
ten minutes, and the command to run on the machine:

```sh
cmdterm-remote link https://web.example ABCD-EFGH-JK --name laptop
```

The command prints the web server's key fingerprint after linking. Compare it with the
machine key the Machines page shows, or pin it before linking with `--fingerprint
SHA3-256:...`, which refuses the link when the answer carries another key.

The machine stores the link in `links.json` next to its keys. Every `cmdterm-remote`
server on that machine, including the one the desktop app runs, then keeps a connection
to the web server and reconnects when it drops: after 1 second, then twice as long each
time, up to a minute. `cmdterm-remote links` lists the links, `cmdterm-remote unlink
https://web.example` removes one. Removing a machine in the web app refuses its key from
then on.

Terminals the web app opens are sessions (`crates/cmdterm-remote/src/session.rs`): they
keep running when the browser leaves, and every device logged in to the account can attach
to them again.

## Configuration

`web.json` in the data directory. Every key is optional.

```json
{
  "listen": "127.0.0.1:8080",
  "public_url": "https://web.example",
  "link_listen": "0.0.0.0:7724",
  "link_address": "web.example:7724",
  "tls": { "cert": "/etc/cmdterm-web/cert.pem", "key": "/etc/cmdterm-web/key.pem" },
  "password_login": true,
  "oidc": [
    {
      "id": "company",
      "name": "Company SSO",
      "issuer": "https://sso.example.com/realms/main",
      "client_id": "cmdterm-web",
      "client_secret": "...",
      "scopes": ["openid", "email", "profile"],
      "allow": ["*@example.com"]
    }
  ]
}
```

- `listen`: where the web app is served. Default `127.0.0.1:8080`.
- `public_url`: the address browsers use, for redirects and the link command. Default
  `http://` plus `listen`. An `https://` URL makes the session cookie `Secure` and turns on
  HSTS.
- `link_listen`: where machines connect. Default `0.0.0.0:7724`.
- `link_address`: `host:port` machines connect to, handed to them when they link. Default
  the host of `public_url` with the port of `link_listen`.
- `tls`: PEM certificate chain and key. Without it the server speaks plain HTTP; put it
  behind a reverse proxy that terminates TLS, or only use it on `localhost`.
- `password_login`: whether the login page offers user name and password. Default true.
- `oidc`: single sign-on providers (OpenID Connect, authorization code flow with PKCE).
  The redirect URI to register with the provider is
  `<public_url>/auth/oidc/<id>/callback`. `client_secret` can come from the variable
  `CMDTERM_WEB_OIDC_<ID>_SECRET` instead (id upper-cased, `-` as `_`). A sign-in creates
  an account when the e-mail matches `allow` (`*@domain` or a full address) and the
  provider says it is verified. An identity already connected to an account always logs in
  to it. Logged-in users connect more identities on the Account page.

The ID token comes straight from the provider's token endpoint over TLS, authenticated
with the client secret, so the server checks its issuer, audience, expiry and nonce but
not its signature (OpenID Connect Core 1.0, section 3.1.3.7).

## Security

- Passwords are hashed with Argon2id. Five failed logins for one user name or address
  lock it for a minute, doubling with every further failure up to an hour.
- The session cookie is `HttpOnly`, `SameSite=Lax`, and `__Host-` prefixed and `Secure`
  over HTTPS. Sessions last 30 days after their last use. Only their SHA3-256 hash is
  stored.
- Requests that change something, and every WebSocket, must come from the server's own
  origin (`Origin` matches `Host`), and change requests must be JSON. The `--dev` token
  is POSTed from a page of this origin, never put in a logged URL.
- The link port limits each peer address to 8 handshake attempts per minute, so
  unauthenticated callers cannot keep the post-quantum handshake busy.
- Pairing codes and machine keys belong to one account. A browser only ever reaches the
  machines of its own account.
- Pages are served with a strict Content Security Policy and no referrer.

## HTTP API

Same origin as the web app, JSON bodies, errors as `{"error": "..."}`. Unix times are
seconds.

### Login

| Request | Answer |
| --- | --- |
| `GET /api/auth/options` | `{"password": true, "providers": [{"id", "name"}], "dev": false}` |
| `POST /api/auth/login` `{"username", "password"}` | `200 {"name"}` and the cookie; `401`; `429 {"error", "retry_after"}` |
| `GET /auth/oidc/{id}/start` | Redirects to the provider. Logged in, it connects the identity to the account. |
| `GET /auth/oidc/{id}/callback` | Redirects to `/`, or to `/#login?error=...` |
| `GET /auth/token` | The dev login page; the token is in the URL fragment. |
| `POST /api/auth/token` `{"token": "..."}` | Dev login. Sets the cookie, `{"name": "dev"}`; `401` for a wrong token. |
| `POST /api/auth/logout` | `204` |

### Account

| Request | Answer |
| --- | --- |
| `GET /api/me` | `{"name", "has_password", "identities": [{"provider", "email"}]}`, `401` when logged out |
| `POST /api/me/password` `{"current", "new"}` | `204`. `current` is needed when the account has a password. |
| `DELETE /api/me/identities/{provider}` | `204`. Refused for the last way to log in. |

### Machines

| Request | Answer |
| --- | --- |
| `GET /api/machines` | `[{"id", "name", "online", "last_seen", "fingerprint", "projects": [...], "sessions": [...]}]`. `projects` and `sessions` are what the machine answered last. |
| `POST /api/machines/pair` | `{"code", "expires", "command"}` |
| `PATCH /api/machines/{id}` `{"name"}` | `204` |
| `DELETE /api/machines/{id}` | `204` |
| `GET /api/machines/{id}/projects` | `[{"name", "root", "vcs"}]`, asked now; `503` while offline |
| `GET /api/machines/{id}/sessions` | `[{"name", "attached", "exited"}]`, asked now; `503` while offline |
| `DELETE /api/machines/{id}/sessions/{name}` | `204`, ends the session |
| `POST /v1/link/redeem` `{"code", "name", "key"}` | For `cmdterm-remote link`: `{"machine", "server_key", "address"}` |

### Terminals

`GET /api/machines/{id}/terminal?session=NAME&cols=80&rows=24` upgrades to a WebSocket.
Optional `cwd` sets where a new session's shell starts, and `resume=N` says how many bytes
of the session's output this browser already has, so only the rest is sent.

- Server to browser: binary messages are terminal output. Text messages are JSON:
  `{"type": "attached", "offset": N}` before the output, which starts at byte `N` of the
  session (bytes sent before it restore terminal modes and count toward no offset),
  `{"type": "exit", "code": N}` when the shell ended, `{"type": "error", "message"}`.
- Browser to server: binary messages are input. Text messages are JSON
  `{"type": "resize", "cols", "rows"}`.

## Offline

The service worker (`web/sw.js`) keeps the app's files, so the page opens without a
network. The app keeps the last machine list and up to 256 KiB of output per session in
the browser. Offline it shows them read-only; when the network comes back, or the tab
becomes visible again, it reconnects and resumes each terminal from the byte offset it has.
