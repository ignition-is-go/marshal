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
