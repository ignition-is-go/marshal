import assert from 'node:assert/strict';
import { createServer, request } from 'node:http';
import { connect } from 'node:net';
import { spawn, execFileSync } from 'node:child_process';
import { mkdir, writeFile, readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { chromium, expect } from '@playwright/test';
import { MykoClient, ConnectionStatus, decode } from './artifacts/legacy-client.mjs';
import { randomUUID, randomBytes, createHash } from 'node:crypto';
import { startOidcFixture } from './oidc-provider.mjs';

const artifacts = resolve(import.meta.dirname, process.env.MARSHAL_BROWSER_ARTIFACTS ?? 'artifacts');
await mkdir(artifacts, { recursive: true });
const checks = [];
const errors = [];
const transportErrors = [];
const check = async (name, action) => { await action(); checks.push(name); console.log('PASS ' + name); };
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
await new Promise((done) => proxy.listen(0, '127.0.0.1', done));
const origin = `http://127.0.0.1:${proxy.address().port}`;
issuer = await startOidcFixture({ appOrigin: origin, internalIssuer: true });
const target = JSON.parse(execFileSync('cargo', ['metadata', '--no-deps', '--format-version', '1'], {cwd: resolve(import.meta.dirname, '../..'), encoding: 'utf8'})).target_directory;
const binary = process.env.MARSHAL_BROWSER_FIXTURE ?? resolve(target, 'debug/examples/browser_fixture');
const dist = resolve(import.meta.dirname, process.env.MARSHAL_FRONTEND_DIST ?? '../../crates/ui/dist');
let serverLog = '';
const child = spawn(binary, [], { env: {
  ...process.env, ...(process.env.MARSHAL_VERIFY_OAUTH ? {MARSHAL_AUTH_OAUTH_ISSUER: origin + '/myko/oauth', MARSHAL_AUTH_OAUTH_RESOURCE: origin + '/myko/mcp'} : {}), MARSHAL_BIND: `127.0.0.1:${agentPort}`,
  MARSHAL_HOOK_BIND: `127.0.0.1:${hookPort}`, MARSHAL_AUTH_ORIGIN: origin,
  MARSHAL_AUTH_ISSUER: issuer.issuer, MARSHAL_AUTH_CLIENT_ID: issuer.clientId, MARSHAL_AUTH_CLIENT_SECRET: issuer.clientSecret,
}, stdio: ['ignore', 'pipe', 'pipe'] });
child.stdout.on('data', (data) => { serverLog += data; });
child.stderr.on('data', (data) => { serverLog += data; });
child.on('error', (error) => errors.push(error.message));
let browser;
const sockets = [];
const legacy = new MykoClient();
let legacyRows = [];
let legacySubscription;
const agentSession = (id) => ({ id, pid: 42, cwd: '/fixture', connectedAt: Date.now(), operator: 'legacy-agent', agentId: 'existing-client', channelsEnabled: false });
const emit = (item, changeType) => legacy.sendEvent({ item, changeType, itemType: 'Session', createdAt: new Date().toISOString(), tx: randomUUID(), sourceId: 'marshal-browser-verification' });
const command = (commandId, command) => legacy.sendCommand({ commandId, command }, { timeoutMs: 10000 });
const mcp = async (port, code, token) => {
  const response = await fetch(`http://127.0.0.1:${port}/myko/mcp`, { method: 'POST', headers: { 'content-type': 'application/json', ...(token ? {authorization: `Bearer ${token}`} : {}) }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'tools/call', params: {name: 'execute', arguments: {code}} }) });
  return { status: response.status, body: await response.json() };
};
try {
  await expect.poll(async () => { try { return (await mcp(agentPort, "return await myko.query('GetAllSessions',{});")).status; } catch { return 0; } }, { timeout: 20000 }).toBe(process.env.MARSHAL_VERIFY_OAUTH ? 401 : 200);
  browser = await chromium.launch({ headless: true, args: ['--no-sandbox', '--disable-dev-shm-usage'] });
  if (process.env.MARSHAL_VERIFY_OAUTH) {
    await check('standard MCP OAuth discovery, consent, exchange, refresh and logout use shared Myko', async () => {
      const resource = origin + '/myko/mcp';
      const metadata = await (await fetch(origin + '/.well-known/oauth-authorization-server/myko/oauth')).json();
      assert.equal(metadata.issuer, origin + '/myko/oauth');
      const redirect = origin + '/oauth-callback';
      const registration = await fetch(metadata.registration_endpoint, {method:'POST', headers:{'content-type':'application/json'}, body:JSON.stringify({client_name:'Marshal MCP fixture', redirect_uris:[redirect], token_endpoint_auth_method:'none', grant_types:['authorization_code','refresh_token']})});
      assert.equal(registration.status,201);
      const client = await registration.json();
      const verifier = randomBytes(48).toString('base64url');
      const authorize = new URL(metadata.authorization_endpoint);
      authorize.search = new URLSearchParams({response_type:'code',client_id:client.client_id,redirect_uri:redirect,resource,code_challenge_method:'S256',code_challenge:createHash('sha256').update(verifier).digest('base64url'),state:'marshal-fixture'}).toString();
      const context = await browser.newContext();
      await context.route('**/auth/user/**', route => route.abort());
      await context.route(redirect + '?**', route => route.fulfill({status:200,body:'Authorized'}));
      const page = await context.newPage();
      await page.goto(authorize.href);
      await page.getByTestId('auth-google').click();
      await page.getByRole('button',{name:'Alice',exact:true}).click();
      await expect(page.getByTestId('oauth-consent')).toBeVisible({timeout:30000});
      assert.ok(!page.url().startsWith(redirect));
      await page.getByTestId('oauth-allow').click();
      await page.waitForURL(redirect + '?**');
      const callback = new URL(page.url());
      assert.equal(callback.searchParams.get('state'),'marshal-fixture');
      assert.equal(callback.searchParams.get('iss'),metadata.issuer);
      const exchange = new URLSearchParams({grant_type:'authorization_code',client_id:client.client_id,redirect_uri:redirect,resource,code:callback.searchParams.get('code'),code_verifier:verifier});
      const reply = await fetch(metadata.token_endpoint,{method:'POST',body:exchange});
      assert.equal(reply.status,200);
      const token = await reply.json();
      const query = "return await myko.query('GetAllSandboxRuns',{});";
      const result = await mcp(proxy.address().port,query,token.access_token);
      assert.equal(result.status,200);
      assert.ok(!result.body.error && !result.body.result?.isError,JSON.stringify(result.body));
      assert.equal((await fetch(metadata.token_endpoint,{method:'POST',body:exchange})).status,400);
      const refreshed = await fetch(metadata.token_endpoint,{method:'POST',body:new URLSearchParams({grant_type:'refresh_token',client_id:client.client_id,resource,refresh_token:token.refresh_token})});
      assert.equal(refreshed.status,200);
      const next = await refreshed.json();
      assert.equal((await mcp(proxy.address().port,query,next.access_token)).status,200);
      await page.goto(origin);
      await page.getByRole('button',{name:'Account menu',exact:true}).click();
      await page.getByRole('button',{name:'Sign out',exact:true}).click();
      await expect(page.getByTestId('auth-google')).toBeVisible();
      assert.equal((await mcp(proxy.address().port,query,next.access_token)).status,401);
    });
  } else {
  const aliceContext = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
  const bobContext = await browser.newContext({ viewport: { width: 1000, height: 850 } });
  aliceContext.setDefaultTimeout(15000);
  bobContext.setDefaultTimeout(15000);
  const alice = await aliceContext.newPage();
  const bob = await bobContext.newPage();
  let preloginSockets = 0;
  const authReplies = [];
  for (const context of [aliceContext, bobContext]) await context.route('**/auth/user/**', route => route.abort());
  for (const page of [alice, bob]) {
    page.on('pageerror', (error) => errors.push(error.message));
    page.on('websocket', (socket) => {
      preloginSockets++;
      socket.on('framereceived', ({payload}) => {
        try {
          const message = typeof payload === 'string' ? JSON.parse(payload) : decode(payload);
          if (page === alice && message.data?.response?.body?.credential) authReplies.push(message.data.response.body);
          if (message.event?.includes('error')) transportErrors.push(message);
        } catch {}
      });
    });
  }
  await Promise.all([alice.goto(origin), bob.goto(origin)]);
  await check('anonymous browsers bootstrap auth through Myko before mounting the application', async () => {
    await expect(alice.getByTestId('auth-google')).toBeVisible({ timeout: 30000 });
    await expect.poll(() => preloginSockets).toBeGreaterThanOrEqual(2);
    await expect(alice.getByTestId('status-footer')).toHaveCount(0);
    const response = await fetch(origin + '/myko/mcp', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({jsonrpc:'2.0',id:1,method:'tools/call',params:{name:'execute',arguments:{code:"return await myko.query('GetAllSessions',{});"}}}) });
    const rejection = await response.json();
    assert.equal(response.status, 200);
    assert.ok(!rejection.error && !rejection.result?.isError, JSON.stringify(rejection));
    const protectedCall = await mcp(agentPort, "return await myko.command('RegisterOperatorConsole',{session:{id:'console-anonymous:tab'}});");
    assert.ok(protectedCall.status === 401 || protectedCall.body.error || protectedCall.body.result?.isError);
  });
  await check('existing Myko 6.11.5 agent registers and heartbeats without login', async () => {
    legacy.setAddress(`ws://127.0.0.1:${agentPort}/myko`);
    legacySubscription = legacy.watchQuery({queryId:'GetAllSessions',queryItemType:'Session',query:{}}).subscribe({next: (rows) => { legacyRows = rows; }, error: error => errors.push(error.message)});
    await expect.poll(() => legacy.getConnectionStatus()).toBe(ConnectionStatus.Connected);
    emit(agentSession('legacy-a'), 'SET');
    emit(agentSession('legacy-b'), 'SET');
    await expect.poll(() => legacyRows.some((row) => row.id === 'legacy-a')).toBe(true);
    await command('SetSessionLastActivityAt', {id: 'legacy-a', lastActivityAt: 123456789});
    await expect.poll(() => legacyRows.find((row) => row.id === 'legacy-a')?.lastActivityAt).toBe(123456789);
  });
  await check('existing agent messaging and named room commands retain their protocol', async () => {
    const sent = await command('SendMessage', {asSession:'legacy-a',toSessionId:'legacy-b',body:'legacy message',responseRequired:false});
    assert.ok(sent.messageId);
    const inbox = await command('ReadMessages', {asSession:'legacy-b',inbox:true,sent:false,unread:true});
    assert.ok(JSON.stringify(inbox).includes('legacy message'));
    const room = await command('JoinRoom', {asSession:'legacy-a',name:'compatibility'});
    assert.equal(room.name, 'compatibility');
    await command('LeaveRoom', {asSession:'legacy-a',room:room.roomId});
  });
  await check('existing agent reconnect and Session DEL/SET remain accepted', async () => {
    emit(agentSession('legacy-a'), 'DEL');
    await expect.poll(() => legacyRows.some((row) => row.id === 'legacy-a')).toBe(false);
    legacy.setAddress(null);
    legacy.setAddress(`ws://127.0.0.1:${agentPort}/myko`);
    await expect.poll(() => legacy.getConnectionStatus()).toBe(ConnectionStatus.Connected);
    emit(agentSession('legacy-a'), 'SET');
    await expect.poll(() => legacyRows.some((row) => row.id === 'legacy-a')).toBe(true);
    const response = await mcp(agentPort, "return await myko.query('GetAllSessions', {});");
    assert.equal(response.status, 200);
    assert.ok(JSON.stringify(response.body).includes('legacy-a'));
  });
  const login = async (page, name) => {
    await page.getByTestId('auth-google').click();
    await page.getByRole('button', { name, exact: true }).click();
    await expect(page.getByTestId('status-footer')).toBeVisible({ timeout: 30000 });
    await expect(page.getByRole('button', { name: 'Account menu', exact: true })).toBeVisible();
  };
  await check('signed identities outside the admitted email domain are denied', async () => {
    const context = await browser.newContext();
    context.setDefaultTimeout(15000);
    try {
      const page = await context.newPage();
      let denied = false;
      page.on('websocket', socket => socket.on('framereceived', ({payload}) => {
        try { const message = typeof payload === 'string' ? JSON.parse(payload) : decode(payload); denied ||= message.data?.response?.status === 401; } catch {}
      }));
      await page.goto(origin);
      await page.getByTestId('auth-google').click();
      issuer.setNextClaims({email:'alice@outside.test'});
      await page.getByRole('button', {name:'Alice',exact:true}).click();
      await expect.poll(() => denied).toBe(true);
      await expect(page.getByTestId('auth-google')).toBeVisible();
      await expect(page.getByTestId('auth-google')).toBeVisible();
      await expect(page.getByTestId('status-footer')).toHaveCount(0);
    } finally { await context.close(); }
  });
  await check('signed Google code exchange mounts Marshal and the profile footer', async () => { await login(alice, 'Alice'); });
  await check('default panes use Roster left, Session right at the golden ratio', async () => {
    // Active pane wrappers carry their activity ID in Mullion's rendered DOM.
    const geometry = await alice.evaluate(() => [...document.querySelectorAll('.pane-scroll')].map((node) => ({ text: node.innerText, x: node.getBoundingClientRect().x, width: node.getBoundingClientRect().width })));
    assert.equal(geometry.length, 2);
    assert.match(geometry[0].text, /roster/i);
    assert.match(geometry[1].text, /session/i);
    assert.ok(geometry[0].x < geometry[1].x);
    assert.ok(Math.abs(geometry[0].width / (geometry[0].width + geometry[1].width) - 0.618034) < 0.01);
  });
  await check('flat activities open the verified Console and its heartbeat stops offline', async () => {
    await expect(alice.getByRole('button', {name:'Views', exact:true})).toHaveCount(0);
    await alice.getByRole('button', {name:'Console', exact:true}).first().click();
    const existing = new Set(legacyRows.map((row) => row.id));
    await alice.getByRole('button', {name:'go online', exact:true}).click();
    await expect.poll(() => legacyRows.find((row) => !existing.has(row.id) && row.operator === 'alice@fixture.test')?.id).toBeTruthy();
    const row = legacyRows.find((row) => !existing.has(row.id) && row.operator === 'alice@fixture.test');
    const initial = row.lastActivityAt;
    await expect.poll(() => legacyRows.find((candidate) => candidate.id === row.id)?.lastActivityAt, {timeout:8000}).toBeGreaterThan(initial);
    await alice.getByRole('button', {name:'go offline', exact:true}).click();
    await expect.poll(() => legacyRows.some((candidate) => candidate.id === row.id)).toBe(false);
    await alice.getByRole('button', {name:'Roster', exact:true}).first().click();
  });
  await expect.poll(() => authReplies.length).toBeGreaterThan(0);
  const session = authReplies.at(-1);
  const ownConsole = `console-${session.user.id}:verification`;
  await check('browser console identity is derived from verified login', async () => {
    const proposed = { ...agentSession(ownConsole), operator: 'forged@fixture.test' };
    const response = await mcp(agentPort, `return await myko.command('RegisterOperatorConsole', {session:${JSON.stringify(proposed)}});`, session.credential);
    assert.equal(response.status, 200);
    assert.ok(!response.body.result?.isError, JSON.stringify(response.body));
    await expect.poll(() => legacyRows.find((row) => row.id === ownConsole)?.operator).toBe('alice@fixture.test');
  });
  await check('verified operator identities cannot be impersonated', async () => {
    for (const operation of ['RegisterSession', 'RegisterOperatorConsole']) {
      const anonymous = await mcp(agentPort, `return await myko.command('${operation}', {session:${JSON.stringify(agentSession(ownConsole))}});`);
      assert.ok(anonymous.status === 401 || anonymous.body.error || anonymous.body.result?.isError);
    }
    for (const asSession of ['console-another-user:tab']) {
      const response = await mcp(agentPort, `return await myko.command('SendMessage', ${JSON.stringify({asSession,toSessionId:'legacy-b',body:'forbidden spoof',responseRequired:false})});`, session.credential);
      assert.ok(response.body.error || response.body.result?.isError, JSON.stringify(response.body));
    }
    const response = await mcp(agentPort, `return await myko.command('SendMessage', ${JSON.stringify({asSession:ownConsole,toSessionId:'legacy-b',body:'verified operator message',responseRequired:false})});`, session.credential);
    assert.ok(!response.body.error && !response.body.result?.isError, JSON.stringify(response.body));
    const inbox = await command('ReadMessages', {asSession:'legacy-b',inbox:true,sent:false,unread:true});
    assert.ok(JSON.stringify(inbox).includes('verified operator message'));
    assert.ok(!JSON.stringify(inbox).includes('forbidden spoof'));
  });
  const protectedSocket = new WebSocket(`ws://127.0.0.1:${agentPort}/myko`, ['myko', `myko.bearer.${Buffer.from(session.credential).toString('base64url')}`]);
  sockets.push(protectedSocket);
  const socketMessages = [];
  protectedSocket.addEventListener('message', (event) => { if (typeof event.data === 'string') socketMessages.push(JSON.parse(event.data)); });
  await expect.poll(() => protectedSocket.readyState).toBe(WebSocket.OPEN);
  await check('authenticated WebSockets reject raw Session injection', async () => {
    const injected = `console-${session.user.id}:raw-injection`;
    protectedSocket.send(JSON.stringify({event:'ws:m:event',data:{item:agentSession(injected),changeType:'SET',itemType:'Session',createdAt:new Date().toISOString(),tx:randomUUID(),sourceId:'browser-verification'}}));
    const tx = randomUUID();
    protectedSocket.send(JSON.stringify({event:'ws:m:query',data:{queryId:'GetAllSessions',queryItemType:'Session',query:{tx,createdAt:new Date().toISOString()}}}));
    await expect.poll(() => socketMessages.some((message) => message.event === 'ws:m:query-response' && message.data.tx === tx)).toBe(true);
    assert.ok(!JSON.stringify(socketMessages).includes(injected));
  });
  await alice.screenshot({ path: resolve(artifacts, 'marshal-footer.png'), fullPage: true });
  await check('second signed-in browser mounts independently', async () => { await login(bob, 'Bob'); });
  await check('operator bubbles share presence without changing the agent roster', async () => {
    await expect(alice.locator('.pulse-room-participant[aria-label^="Bob"]')).toBeVisible();
    await expect(bob.locator('.pulse-room-participant[aria-label^="Alice"]')).toBeVisible();
    assert.ok(!legacyRows.some((row) => row.operator === 'bob@fixture.test'));
    await bob.screenshot({path:resolve(artifacts,'marshal-presence.png'),fullPage:true});
  });
  await check('logout removes signed-in chrome, revokes browser access, and preserves agents', async () => {
    await alice.getByRole('button', { name: 'Account menu', exact: true }).click();
    await alice.getByRole('button', { name: 'Sign out', exact: true }).click();
    await expect(alice.getByTestId('auth-google')).toBeVisible();
    await expect(alice.getByTestId('status-footer')).toHaveCount(0);
    await expect(bob.getByTestId('status-footer')).toBeVisible();
    await expect(bob.locator('.pulse-room-participant[aria-label^="Alice"]')).toHaveCount(0);
    await expect.poll(() => protectedSocket.readyState, {timeout:10000}).toBe(WebSocket.CLOSED);
    const response = await mcp(agentPort, "return await myko.query('GetAllSessions',{});", session.credential);
    assert.equal(response.status, 401);
    const inbox = await command('ReadMessages', {asSession:'legacy-b',inbox:true,sent:false,unread:true});
    assert.ok(JSON.stringify(inbox).includes('verified operator message'));
  });
  }
  assert.deepEqual(errors, []);
  await writeFile(resolve(artifacts, process.env.MARSHAL_VERIFY_OAUTH ? 'oauth-result.json' : 'result.json'), JSON.stringify({ status: 'passed', checks }, null, 2));
} catch (error) {
  const page = browser?.contexts()[0]?.pages()[0];
  const visibleText = await page?.locator('body').innerText().catch(() => '');
  await page?.screenshot({path:resolve(artifacts,'failure.png'),fullPage:true}).catch(() => {});
  await writeFile(resolve(artifacts, process.env.MARSHAL_VERIFY_OAUTH ? 'oauth-result.json' : 'result.json'), JSON.stringify({ status: 'failed', checks, error: error.stack, browserErrors: errors, transportErrors, visibleText, serverLog }, null, 2));
  throw error;
} finally {
  for (const socket of sockets) socket.close();
  legacySubscription?.unsubscribe();
  legacy.disconnect();
  await browser?.close();
  child.kill('SIGTERM');
  proxy.closeAllConnections();
  await new Promise((done) => proxy.close(done));
  await issuer.close();
  await writeFile(resolve(artifacts, 'server.log'), serverLog);
}
