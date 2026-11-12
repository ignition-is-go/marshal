# Browser authentication and agent compatibility

Marshal follows [rship #845](https://github.com/ignition-is-go/rship/pull/845):
`PreparedMykoAuth` installs the shared Pulse authentication provider on the existing
Myko listener. `MykoAuthService` is installed after replay. The UI provides Myko
before `provide_auth`, then mounts its authenticated application through the shared
`AuthGate` and `provide_authenticated_myko`. There is no application HTTP auth
router, second Myko listener, custom login implementation, or credential store.

The UI uses shared participant bubbles and profile menu. Roster occupies the left
61.8% of the default layout and Session the right 38.2%. Activities are flat.

## Agent contract

Existing coordination operations explicitly use Myko's public-operation and public
Session SET/DEL declarations, following the reference's SDK contract. Ordinary
agent registrations, heartbeat, messaging, named rooms, and roster queries retain
their existing protocol. These identities remain self-asserted. Keep coordination
and the existing HTTP hook listener on loopback or the private mesh. Browser login
does not make that coordination data private.

Verified operator consoles use a separate protected `RegisterOperatorConsole`
command. The server derives the operator email and binds the console ID to the
verified user. Public agent operations cannot create, mutate, delete, or send as a
verified console. Presence participants are separate from the agent roster;
signing in alone does not register an agent.

With the optional OAuth issuer/resource pair configured, Myko supplies standard
MCP discovery, authorization handoff, and token admission. MCP hosts authenticate
against the selected server using their existing OAuth support. Marshal adds no
client-specific login flow or authenticated binary. Anonymous native coordination
operations retain the explicitly public contract. Hook transport remains the
existing trusted-network interface. No deployment changes are included.

## Configuration

Without auth configuration, the daemon retains its existing trusted-agent mode.
Configuring auth requires all five values below and a persistent session database.
Partial configuration fails startup.

| Variable | Value |
| --- | --- |
| `MARSHAL_AUTH_ISSUER` | Trusted OIDC issuer |
| `MARSHAL_AUTH_CLIENT_ID` | Browser provider client ID |
| `MARSHAL_AUTH_CLIENT_SECRET` | Provider secret |
| `MARSHAL_AUTH_ORIGIN` | Exact browser origin |
| `MARSHAL_AUTH_EMAIL_DOMAINS` | Comma-separated admitted verified domains |
| `MARSHAL_AUTH_POSTGRES_URL` | Session database; falls back to `MYKO_POSTGRES_URL` |
| `MARSHAL_AUTH_CA_CERT` | Optional PEM root for a private development identity provider; default public roots remain trusted |
| `MARSHAL_AUTH_OAUTH_ISSUER` | Optional Myko OAuth authorization server issuer |
| `MARSHAL_AUTH_OAUTH_RESOURCE` | Optional protected MCP resource; requires OAuth issuer |

Serve the built UI with the existing static hosting arrangement. Forward `/myko`
HTTP requests and WebSocket upgrades to `MARSHAL_BIND`, including Myko's OAuth
metadata routes when OAuth is enabled. The shared browser callback is `/`.
`/auth/user` is the shared client scope, not a Marshal HTTP route.

## Validation

Dependencies use Myko 6.15.1 and pulse-multi-user revision `7c3d286`, including
[pulse-multi-user #35](https://github.com/ignition-is-go/pulse-multi-user/pull/35)
for explicit presence operation names. Shared Pulse crates remain private source
dependencies; crates.io packaging is unresolved and this PR is not a release.

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build -p marshal-daemon --example browser_fixture
(cd crates/ui && NO_COLOR=true trunk build)
(cd scripts/browser && npm ci && npm run verify && MARSHAL_VERIFY_OAUTH=1 npm run verify)
```

The browser fixture uses a signed local OIDC issuer, disposable in-memory sessions,
and a fixture static proxy. It blocks `/auth/user/*` and sends browser and historical
Myko 6.11.5 agent traffic to one Myko listener. Artifacts go under
`scripts/browser/artifacts`. Production OIDC configuration, database persistence,
and session expiry require separate deployment validation.

The optional OAuth fixture validates discovery, explicit consent, PKCE code exchange,
code replay rejection, refresh, protected MCP execution, and browser logout revoking
the delegated MCP token. It uses the same shared production integration.

## Isolated development setup

Build the actual daemon and UI bundle, then point it at a dedicated local Postgres
17 database whose name contains `dev`. Keep the database separate from both the
live Marshal database and other applications' development databases.

```sh
cargo build --locked -p marshal-daemon --bin marshal-daemon
(cd crates/ui && NO_COLOR=true trunk build)
```

For a local manual walkthrough with the signed test issuer:

```sh
export MYKO_POSTGRES_URL # Set privately to the dedicated local dev database URL.
node scripts/browser/dev.mjs --fixture
```

The server listens at `http://127.0.0.1:8878`. Alice and Bob are test accounts.
This mode runs the real daemon with persisted auth sessions and coordination
state, but does not validate the real identity provider. Other machines can use
an SSH forward to the dev host's loopback port. The launcher does not bind public
or mesh interfaces.

For real provider sign-in, register a dedicated development OAuth application
with callback and origin `http://localhost:8878/`, set the five required
`MARSHAL_AUTH_*` variables including `MARSHAL_AUTH_ORIGIN=http://localhost:8878`, and run the launcher without
`--fixture`. Use `http://localhost:8878/` consistently with that registration.
A protected JSON file containing environment keys can be supplied through
`MARSHAL_DEV_CONFIG` instead of exporting individual values. Do not commit it.
The launcher defaults to the test issuer only when `--fixture` is explicit.

To repeat the automated checks against the actual daemon and database:

```sh
MARSHAL_VERIFY_RESTART=1 npm --prefix scripts/browser run verify
```

This requires the dedicated database URL and the built daemon. The check exercises
browser login, two-user presence, old agent compatibility, graceful daemon stop,
new-process startup, durable console/message replay, browser session restoration,
presence reconnect, and logout revocation after restart. It never resets the live
Marshal daemon. CI runs this check against its own temporary Postgres cluster.

Before production rollout, validate the dedicated dev OAuth application with real
sign-in, the intended HTTPS origin and ingress, MCP host consent, and the existing
agent launchers. Session expiry and the final release/package artifact remain
separate acceptance checks. A local signed-issuer pass does not satisfy those gates.

For a mesh-accessible dev URL, keep the local launcher on loopback behind a
separate HTTPS proxy. Set `MARSHAL_DEV_ORIGIN` to that HTTPS origin so the test
issuer and callback use the address opened in the browser. When the dev issuer
uses a private CA, set `MARSHAL_AUTH_CA_CERT` to its PEM root for the daemon and
`NODE_EXTRA_CA_CERTS` for the launcher. The daemon uses the shared Pulse
`AuthState::new_with_http_client` integration with an additional trusted root;
certificate verification and HTTPS-only issuer requests remain enabled. No auth
routes or token implementation are added to the proxy.

The dev UI consumes the shared `NotificationToast` / `NotificationToasts`
primitives from pulse-leptos-ui revision `059ca743`. A single toast stack reads
Marshal's existing `Notifications` store alongside the rightmost footer bell.
Toast expiry or dismissal preserves bell history. The immutable workspace patch
keeps multi-user widgets and Marshal on the same shared UI crate; CI authenticates
that private source alongside the existing private dependencies. Replace this
source pin with the published shared release during release packaging.
