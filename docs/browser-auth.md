# Dashboard sign-in and Marshal access

Marshal serves agent coordination over Myko. Bind the agent WebSocket, HTTP
MCP, and hooks to loopback or a trusted VPN network. Agents on that network
can keep their existing MCP configuration and coordinate without credentials.

The separate Pulse Marshal UI repository owns the browser dashboard and its
Pulse sign-in and presence service. Browser credential handling lives on that
service. The dashboard reads the Marshal roster, rooms, and message feed over
a separate Myko WebSocket.

## Read-only monitoring

Set `MARSHAL_VIEW_BIND` to an unused loopback address such as
`127.0.0.1:6162` to enable monitoring. It starts after Myko replay and uses
the same live store as the agent listener. The Myko gateway rejects commands,
client events and HTTP MCP on this listener.

On the restricted dashboard origin, route `/myko` to the Pulse auth service.
Route the dashboard's `/marshal-view` WebSocket to `/myko` on
`MARSHAL_VIEW_BIND`. Keep the proxy inside the VPN boundary. The read-only
socket exposes the coordination data available to trusted clients.

## Authenticated HTTP MCP

To publish MCP outside the VPN, configure a separate loopback listener with
`MARSHAL_MCP_AUTH_BIND`. Set `MARSHAL_MCP_AUTH_ISSUER` to the Auth0 issuer,
`MARSHAL_MCP_AUTH_AUDIENCE` to the Marshal API audience, and
`MARSHAL_MCP_AUTH_RESOURCE` to the published HTTPS MCP endpoint. Myko advertises
OAuth resource metadata and validates the issuer, audience, and token expiry.
Marshal requires the `marshal:coordinate` scope and limits the credential to
coordination operations.

On the public HTTPS origin, forward `/myko/mcp` and these metadata paths
to the authenticated listener without rewriting the request path.

- `/.well-known/oauth-protected-resource/myko/mcp`
- `/.well-known/oauth-protected-resource`

Keep the WebSocket relay, hook endpoints, and dashboard origin inside the VPN.
Existing VPN MCP configurations need no changes.

Verify the public URL from outside the VPN. An anonymous MCP coordination
request returns `401` with a `WWW-Authenticate` header. Follow its
`resource_metadata` URL and confirm the metadata lists the public MCP URL,
the configured issuer, and the `marshal:coordinate` scope. Repeat the request
with a token issued for the configured audience and scope. From inside the
VPN, run the agent delivery check against the existing MCP and hook endpoints.

## Local compatibility checks

Build Marshal and the dashboard's auth service and WASM bundle in their
respective checkouts. Set the following variables before running the verifier.

| Variable | Value |
| --- | --- |
| `MARSHAL_FRONTEND_DIST` | Built dashboard bundle directory |
| `MARSHAL_UI_AUTH_BINARY` | Path to the compiled Pulse dashboard auth service |
| `PULSE_MARSHAL_UI_AUTH_POSTGRES_URL` | Disposable Postgres database for browser auth |
| `MARSHAL_BROWSER_FIXTURE` | Optional daemon binary override |

```sh
cargo build --locked -p marshal-daemon
npm ci --prefix scripts/browser
npm --prefix scripts/browser run verify
```

This uses a signed local identity provider. Production identity-provider
behavior and VPN reachability require separate rollout checks. Browser
sessions may reset during the upgrade. Existing VPN agent clients should
reconnect with the same configuration and exchange messages.


For the daemon cutover, run the independent agent verifier with the released
baseline and candidate binaries. Set `MYKO_POSTGRES_URL` to an empty disposable
coordination database. The verifier uses the same legacy Myko client, relay
address, and database throughout the upgrade and rollback.

```sh
cd scripts/browser
bun build legacy-client.mjs --target=node --format=esm --outfile=artifacts/legacy-client.mjs
node verify-agent-upgrade.mjs /path/to/baseline/marshal-daemon /path/to/candidate/marshal-daemon
```

Set `MARSHAL_AGENT_UPGRADE_RECEIPT` to a file path to save the binary hashes
and per-phase message and process IDs as JSON.
