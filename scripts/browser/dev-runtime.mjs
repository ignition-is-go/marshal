import { createServer, request } from 'node:http';
import { connect } from 'node:net';
import { spawn, execFileSync } from 'node:child_process';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { startOidcFixture } from './oidc-provider.mjs';

// Static hosting and transport forwarding for local dev and verification only.
// The dashboard auth process and Marshal can be run independently.
export async function startDevRuntime({ fixture = true, binary: configuredBinary, port = 0, errors = [] } = {}) {
  const authVariables = ['MARSHAL_UI_AUTH_BINARY', 'PULSE_MARSHAL_UI_AUTH_POSTGRES_URL'];
  if (!fixture) {
    authVariables.push(...['ISSUER', 'CLIENT_ID', 'CLIENT_SECRET', 'EMAIL_DOMAINS'].map(name => 'PULSE_MARSHAL_UI_AUTH_' + name));
  }
  for (const name of authVariables) {
    if (!process.env[name]) throw new Error(name + ' is required for dashboard authentication');
  }
  if (!process.env.MARSHAL_FRONTEND_DIST) {
    throw new Error('MARSHAL_FRONTEND_DIST must point to a built dashboard bundle');
  }
  const dist = resolve(process.env.MARSHAL_FRONTEND_DIST);
  const freePort = async () => {
    const server = createServer();
    await new Promise((done) => server.listen(0, '127.0.0.1', done));
    const port = server.address().port;
    await new Promise((done) => server.close(done));
    return port;
  };
  const agentPort = await freePort();
  const hookPort = await freePort();
  const authBinary = process.env.MARSHAL_UI_AUTH_BINARY;
  const authPort = await freePort();
  const viewPort = await freePort();
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
    const port = incoming.url.startsWith('/oidc/') ? issuer.port : authPort;
    const upstream = request({ hostname: '127.0.0.1', port, path: incoming.url, method: incoming.method, headers: incoming.headers }, (response) => {
      outgoing.writeHead(response.statusCode, response.headers); response.pipe(outgoing);
    });
    upstream.on('error', () => { outgoing.writeHead(502); outgoing.end(); });
    incoming.pipe(upstream);
  });
  proxy.on('upgrade', (incoming, socket, head) => {
    const monitoring = incoming.url === '/marshal-view';
    const path = monitoring ? '/myko' : incoming.url;
    const upstream = connect(monitoring ? viewPort : authPort, '127.0.0.1', () => {
      upstream.write(`${incoming.method} ${path} HTTP/${incoming.httpVersion}\r\n` + Object.entries(incoming.headers).map(([name, value]) => `${name}: ${value}`).join('\r\n') + '\r\n\r\n');
      if (head.length) upstream.write(head);
      socket.pipe(upstream).pipe(socket);
    });
    upstream.on('error', () => socket.destroy());
    socket.on('error', () => upstream.destroy());
    socket.on('close', () => upstream.destroy());
  });
  await new Promise((done) => proxy.listen(port, '127.0.0.1', done));
  const origin = process.env.MARSHAL_DEV_ORIGIN ?? `http://127.0.0.1:${proxy.address().port}`;
  if (fixture) issuer = await startOidcFixture({ appOrigin: origin, internalIssuer: !process.env.MARSHAL_DEV_ORIGIN });
  const target = JSON.parse(execFileSync('cargo', ['metadata', '--no-deps', '--format-version', '1'], {cwd: resolve(import.meta.dirname, '../..'), encoding: 'utf8'})).target_directory;
  const binary = configuredBinary ?? process.env.MARSHAL_BROWSER_FIXTURE ?? resolve(target, 'debug/marshal-daemon');
  let serverLog = '';
  const logging = (process) => {
    process.stdout.on('data', (data) => { serverLog += data; });
    process.stderr.on('data', (data) => { serverLog += data; });
    process.on('error', (error) => errors.push(error.message));
    return process;
  };
  const startAuth = () => logging(spawn(authBinary, [], {
    env: {
      ...process.env,
      PULSE_MARSHAL_UI_AUTH_BIND: `127.0.0.1:${authPort}`,
      PULSE_MARSHAL_UI_AUTH_ORIGIN: origin,
      ...(fixture ? {
        PULSE_MARSHAL_UI_AUTH_ISSUER: issuer.issuer,
        PULSE_MARSHAL_UI_AUTH_CLIENT_ID: issuer.clientId,
        PULSE_MARSHAL_UI_AUTH_CLIENT_SECRET: issuer.clientSecret,
        PULSE_MARSHAL_UI_AUTH_EMAIL_DOMAINS: 'fixture.test',
      } : {}),
    }, stdio: ['ignore', 'pipe', 'pipe'],
  }));
  const startDaemon = (daemonBinary = binary) => {
    const daemon = spawn(daemonBinary, [], { env: {
      ...process.env,
      MARSHAL_BIND: `127.0.0.1:${agentPort}`,
      MARSHAL_VIEW_BIND: `127.0.0.1:${viewPort}`,
      MARSHAL_HOOK_BIND: `127.0.0.1:${hookPort}`,
    }, stdio: ['ignore', 'pipe', 'pipe'] });
    return logging(daemon);
  };

  return { origin, proxy, issuer, binary, dist, agentPort, hookPort, authBinary, authPort, startAuth, startDaemon, get serverLog() { return serverLog; } };
}
