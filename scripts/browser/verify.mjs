import assert from 'node:assert/strict';
import { createServer, request } from 'node:http';
import { connect } from 'node:net';
import { spawn, execFileSync } from 'node:child_process';
import { mkdir, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { chromium, expect } from '@playwright/test';
import { MykoClient, ConnectionStatus, decode } from './artifacts/legacy-client.mjs';
import { randomUUID } from 'node:crypto';
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
const backendPort = await freePort();
const agentPort = await freePort();
const hookPort = await freePort();
let issuer;
const proxy = createServer((incoming, outgoing) => {
  const port = incoming.url.startsWith('/oidc/') ? issuer.port : backendPort;
  const upstream = request({ hostname: '127.0.0.1', port, path: incoming.url, method: incoming.method, headers: incoming.headers }, (response) => {
    outgoing.writeHead(response.statusCode, response.headers); response.pipe(outgoing);
  });
  upstream.on('error', () => { outgoing.writeHead(502); outgoing.end(); });
  incoming.pipe(upstream);
});
proxy.on('upgrade', (incoming, socket, head) => {
  const upstream = connect(backendPort, '127.0.0.1', () => {
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
  ...process.env, MARSHAL_BROWSER_BIND: `127.0.0.1:${backendPort}`, MARSHAL_BIND: `127.0.0.1:${agentPort}`,
  MARSHAL_HOOK_BIND: `127.0.0.1:${hookPort}`, MARSHAL_AUTH_ORIGIN: origin, MARSHAL_UI_DIR: dist,
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
  await expect.poll(async () => { try { return (await fetch(origin + '/health')).status; } catch { return 0; } }, { timeout: 20000 }).toBe(200);
  browser = await chromium.launch({ headless: true, args: ['--no-sandbox', '--disable-dev-shm-usage'] });
  const aliceContext = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
  const bobContext = await browser.newContext({ viewport: { width: 1000, height: 850 } });
  aliceContext.setDefaultTimeout(15000);
  bobContext.setDefaultTimeout(15000);
  const alice = await aliceContext.newPage();
  const bob = await bobContext.newPage();
  let preloginSockets = 0;
  for (const page of [alice, bob]) {
    page.on('pageerror', (error) => errors.push(error.message));
    page.on('websocket', (socket) => {
      preloginSockets++;
      socket.on('framereceived', ({payload}) => {
        try {
          const message = typeof payload === 'string' ? JSON.parse(payload) : decode(payload);
          if (message.event?.includes('error')) transportErrors.push(message);
        } catch {}
      });
    });
  }
  await Promise.all([alice.goto(origin), bob.goto(origin)]);
  await check('anonymous browsers see login and open no application sockets', async () => {
    await expect(alice.getByTestId('auth-google')).toBeVisible({ timeout: 30000 });
    assert.equal(preloginSockets, 0);
    await expect(alice.getByTestId('status-footer')).toHaveCount(0);
    const response = await fetch(origin + '/myko/mcp', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({jsonrpc:'2.0',id:1,method:'tools/call',params:{name:'execute',arguments:{code:"return await myko.query('GetAllSessions',{});"}}}) });
    const rejection = await response.json();
    assert.ok(response.status === 401 || rejection.error || rejection.result?.isError, JSON.stringify(rejection));
  });
  await check('existing Myko 6.11.5 agent registers and heartbeats without login', async () => {
    legacy.setAddress(`ws://127.0.0.1:${agentPort}/myko`);
    legacySubscription = legacy.watchQuery({queryId:'GetAllSessions',queryItemType:'Session',query:{}}).subscribe((rows) => { legacyRows = rows; });
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
      await page.goto(origin);
      await page.getByTestId('auth-google').click();
      issuer.setNextClaims({email:'alice@outside.test'});
      const denied = page.waitForResponse((response) => response.url() === origin + '/auth/user/session' && response.request().method() === 'POST');
      await page.getByRole('button', {name:'Alice',exact:true}).click();
      assert.equal((await denied).status(), 401);
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
  const session = await alice.evaluate(async () => (await fetch('/auth/user/session')).json());
  const ownConsole = `console-${session.user.id}:verification`;
  await check('browser console identity is derived from verified login', async () => {
    const proposed = { ...agentSession(ownConsole), operator: 'forged@fixture.test' };
    const response = await mcp(backendPort, `return await myko.command('RegisterSession', {session:${JSON.stringify(proposed)}});`, session.credential);
    assert.equal(response.status, 200);
    assert.ok(!response.body.result?.isError, JSON.stringify(response.body));
    await expect.poll(() => legacyRows.find((row) => row.id === ownConsole)?.operator).toBe('alice@fixture.test');
  });
  await check('signed-in browsers cannot send as an agent or another user', async () => {
    for (const asSession of ['legacy-a', 'console-another-user:tab']) {
      const response = await mcp(backendPort, `return await myko.command('SendMessage', ${JSON.stringify({asSession,toSessionId:'legacy-b',body:'forbidden spoof',responseRequired:false})});`, session.credential);
      assert.ok(response.body.error || response.body.result?.isError, JSON.stringify(response.body));
    }
    const response = await mcp(backendPort, `return await myko.command('SendMessage', ${JSON.stringify({asSession:ownConsole,toSessionId:'legacy-b',body:'verified operator message',responseRequired:false})});`, session.credential);
    assert.ok(!response.body.error && !response.body.result?.isError, JSON.stringify(response.body));
    const inbox = await command('ReadMessages', {asSession:'legacy-b',inbox:true,sent:false,unread:true});
    assert.ok(JSON.stringify(inbox).includes('verified operator message'));
    assert.ok(!JSON.stringify(inbox).includes('forbidden spoof'));
  });
  const protectedSocket = new WebSocket(`ws://127.0.0.1:${backendPort}/myko`, ['myko', `myko.bearer.${Buffer.from(session.credential).toString('base64url')}`]);
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
    const response = await mcp(backendPort, "return await myko.query('GetAllSessions',{});", session.credential);
    assert.equal(response.status, 401);
    const inbox = await command('ReadMessages', {asSession:'legacy-b',inbox:true,sent:false,unread:true});
    assert.ok(JSON.stringify(inbox).includes('verified operator message'));
  });
  assert.deepEqual(errors, []);
  await writeFile(resolve(artifacts, 'result.json'), JSON.stringify({ status: 'passed', checks }, null, 2));
} catch (error) {
  const page = browser?.contexts()[0]?.pages()[0];
  const visibleText = await page?.locator('body').innerText().catch(() => '');
  await page?.screenshot({path:resolve(artifacts,'failure.png'),fullPage:true}).catch(() => {});
  await writeFile(resolve(artifacts, 'result.json'), JSON.stringify({ status: 'failed', checks, error: error.stack, browserErrors: errors, transportErrors, visibleText, serverLog }, null, 2));
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
