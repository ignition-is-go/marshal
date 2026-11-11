# Browser authentication and agent compatibility

This draft validates browser authentication and agent compatibility. The presence
backend is pending a shared-package command namespace fix. Daemon package
publication also needs a decision because the shared Pulse crates are private
Git dependencies. Do not deploy this draft.

Marshal's operator UI uses Pulse login, the shared participant bubbles, and the
shared profile menu. The default pane layout is Roster on the left at 61.8% and
Session on the right at 38.2%; activities are a flat list.

## Transport boundaries

The daemon owns one Myko state. Its existing `MARSHAL_BIND` listener, defaulting
to loopback port 6155, remains a trusted agent interface. The HTTP hooks on
`MARSHAL_HOOK_BIND`, defaulting to loopback port 6156, retain their existing
protocol. Keep these interfaces on loopback or the private mesh. Agent clients
continue to register and heartbeat through Session SET/DEL and existing commands,
including their self-asserted `asSession` identity.

The optional `MARSHAL_BROWSER_BIND` listener serves the UI, `/auth/user/*`, and
an authenticated `/myko` transport. It uses Myko's native authentication provider
and Pulse's session authenticator, including checks on existing connections after
logout or expiry. It shares the agent server's state; it does not proxy requests
to the trusted listener. Anonymous application operations are denied. Browser
policy allows the console's named reads and commands, denies raw entity events,
and limits sender identities to the verified user's console namespace. The
registration handler derives the operator email from the verified identity.
Browser users can observe the whole Marshal instance; this is not room-level
isolation of agent messages.

The planned UI presence uses one collaboration room for this Marshal instance. Those human
participants are separate from Marshal's agent roster and messaging rooms.
Signing in does not create an agent roster entry. The Console panel retains its
explicit online/offline control.

## Configuration and rollout

Enabling the browser listener requires all these variables. Missing or invalid
configuration fails startup.

| Variable | Value |
| --- | --- |
| `MARSHAL_BROWSER_BIND` | Listener address, for example `127.0.0.1:6157` |
| `MARSHAL_UI_DIR` | Directory containing the built UI's `index.html` |
| `MARSHAL_AUTH_ISSUER` | Trusted OIDC issuer |
| `MARSHAL_AUTH_CLIENT_ID` | Browser application's provider client ID |
| `MARSHAL_AUTH_CLIENT_SECRET` | Confidential client secret, supplied through deployment secrets |
| `MARSHAL_AUTH_ORIGIN` | Exact public browser origin |
| `MARSHAL_AUTH_EMAIL_DOMAINS` | Comma-separated admitted verified email domains |
| `MARSHAL_AUTH_POSTGRES_URL` | Persistent Pulse browser session database |

Register the origin's `/` as the provider callback, matching Pulse's shared login
flow. Route the entire browser origin to the browser listener, including auth
requests and WebSocket upgrades. Remove the old public `/myko` proxy to port
6155. Do not expose ports 6155 or 6156 through public ingress. Deploy the auth
configuration and ingress change with the new UI bundle; the authenticated UI
does not fall back to the trusted agent transport.

The production browser listener requires Postgres. The separate
`browser_fixture` example explicitly uses an in-memory store for disposable
signed-issuer validation. It is not a daemon configuration mode.

## Dependencies and validation

The integration is based on Myko 6.15.1 and pulse-multi-user 0.9.0. All shared
Pulse crates must use the same immutable source revision. Myko now requires raw
client event opt-in; only Session SET/DEL are enabled to preserve agent clients.
Browser policy still rejects both operations.

Run the workspace tests and the signed-issuer browser check:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build -p marshal-daemon --example browser_fixture
(cd crates/ui && NO_COLOR=true trunk build)
(cd scripts/browser && npm ci && npm run verify)
```

Install Bun 1.3.11 before running the browser check. It bundles the historical
Myko 6.11.5 client for Node, whose runtime serves the fixture proxy and drives
Playwright. The check uses a signed local OIDC issuer and an explicitly disposable
session store. Production OIDC configuration, session persistence, and session
expiry are not runtime-validated by this fixture.

The current check covers anonymous admission, rejected email domains, real
browser login, pane geometry, Console registration/heartbeat/offline, verified
operator identity, sender impersonation denial, raw event denial, live logout
revocation, and Myko 6.11.5 agent registration/messaging/rooms/reconnect. Presence
bubbles are not runtime-validated until the room backend is installed.

The verification writes its result, browser screenshot, and fixture log under
`scripts/browser/artifacts/`. It never connects to the live Marshal daemon.
