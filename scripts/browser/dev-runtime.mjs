import { createServer, request } from 'node:http';
import { connect } from 'node:net';
import { spawn, execFileSync } from 'node:child_process';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { startOidcFixture } from './oidc-provider.mjs';

// Static hosting and transport forwarding for local dev and verification only.
// Authentication is always supplied by the daemon's shared Myko integration.
export async function startDevRuntime({ fixture = true, binary: configuredBinary, port = 0, errors = [] } = {}) {
  if (!fixture) {
    for (const name of ['MARSHAL_AUTH_ISSUER', 'MARSHAL_AUTH_CLIENT_ID', 'MARSHAL_AUTH_CLIENT_SECRET', 'MARSHAL_AUTH_EMAIL_DOMAINS']) {
      if (!process.env[name]) throw new Error(name + ' is required for configured dev authentication');
    }
  }
  const freePort = async () => {
    const server = createServer();
    await new Promise((done) => server.listen(0, '127.0.0.1', done));
    const port = server.address().port;
    await new Promise((done) => server.close(done));
    return port;
  };
  const agentPort = await freePort();
  const hookPort = await freePort();
  let issuer;
  const proxy = createServer((incoming, outgoing) => {
    if (incoming.url.startsWith('/auth/user')) { outgoing.writeHead(404); outgoing.end(); return; }
    if (!incoming.url.startsWith('/oidc/') && !incoming.url.startsWith('/myko') && !incoming.url.startsWith('/.well-known/')) {
      const path = new URL(incoming.url, 'http://fixture').pathname;
      const file = resolve(dist, '.' + (path === '/' ? '/index.html' : path));
      if (!file.startsWith(dist + '/')) { outgoing.writeHead(403); outgoing.end(); return; }
      readFile(file).then((body) => {
        const type = file.endsWith('.wasm') ? 'application/wasm' : file.endsWith('.js') ? 'text/javascript' : file.endsWith('.css') ? 'text/css' : 'text/html';
        outgoing.writeHead(200, {'content-type':type}); outgoing.end(body);
      }).catch(() => { outgoing.writeHead(404); outgoing.end(); });
      return;
    }
    if (incoming.url.startsWith('/oidc/') && !issuer) { outgoing.writeHead(404); outgoing.end(); return; }
    const port = incoming.url.startsWith('/oidc/') ? issuer.port : agentPort;
    const upstream = request({ hostname: '127.0.0.1', port, path: incoming.url, method: incoming.method, headers: incoming.headers }, (response) => {
      outgoing.writeHead(response.statusCode, response.headers); response.pipe(outgoing);
    });
    upstream.on('error', () => { outgoing.writeHead(502); outgoing.end(); });
    incoming.pipe(upstream);
  });
  proxy.on('upgrade', (incoming, socket, head) => {
    const upstream = connect(agentPort, '127.0.0.1', () => {
      upstream.write(`${incoming.method} ${incoming.url} HTTP/${incoming.httpVersion}\r\n` + Object.entries(incoming.headers).map(([name, value]) => `${name}: ${value}`).join('\r\n') + '\r\n\r\n');
      if (head.length) upstream.write(head);
      socket.pipe(upstream).pipe(socket);
    });
    upstream.on('error', () => socket.destroy());
    socket.on('error', () => upstream.destroy());
    socket.on('close', () => upstream.destroy());
  });
  await new Promise((done) => proxy.listen(port, '127.0.0.1', done));
  const origin = !fixture && process.env.MARSHAL_AUTH_ORIGIN ? process.env.MARSHAL_AUTH_ORIGIN : `http://127.0.0.1:${proxy.address().port}`;
  if (fixture) issuer = await startOidcFixture({ appOrigin: origin, internalIssuer: true });
  const target = JSON.parse(execFileSync('cargo', ['metadata', '--no-deps', '--format-version', '1'], {cwd: resolve(import.meta.dirname, '../..'), encoding: 'utf8'})).target_directory;
  const binary = configuredBinary ?? process.env.MARSHAL_BROWSER_FIXTURE ?? resolve(target, process.env.MARSHAL_VERIFY_RESTART ? 'debug/marshal-daemon' : 'debug/examples/browser_fixture');
  const dist = resolve(import.meta.dirname, process.env.MARSHAL_FRONTEND_DIST ?? '../../crates/ui/dist');
  let serverLog = '';
  const startDaemon = () => {
    const daemon = spawn(binary, [], { env: {
      ...process.env, ...(process.env.MARSHAL_VERIFY_OAUTH ? {MARSHAL_AUTH_OAUTH_ISSUER: origin + '/myko/oauth', MARSHAL_AUTH_OAUTH_RESOURCE: origin + '/myko/mcp'} : {}), MARSHAL_BIND: `127.0.0.1:${agentPort}`,
      MARSHAL_HOOK_BIND: `127.0.0.1:${hookPort}`, MARSHAL_AUTH_ORIGIN: origin,
      ...(fixture ? {MARSHAL_AUTH_ISSUER: issuer.issuer, MARSHAL_AUTH_CLIENT_ID: issuer.clientId, MARSHAL_AUTH_CLIENT_SECRET: issuer.clientSecret, MARSHAL_AUTH_EMAIL_DOMAINS:'fixture.test'} : {}),
    }, stdio: ['ignore', 'pipe', 'pipe'] });
    daemon.stdout.on('data', (data) => { serverLog += data; });
    daemon.stderr.on('data', (data) => { serverLog += data; });
    daemon.on('error', (error) => errors.push(error.message));
    return daemon;
  };

  return { origin, proxy, issuer, binary, agentPort, hookPort, startDaemon, get serverLog() { return serverLog; } };
}
