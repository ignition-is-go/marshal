import { startDevRuntime } from './dev-runtime.mjs';
import assert from 'node:assert/strict';
import { mkdir, writeFile, readFile, readdir } from 'node:fs/promises';
import { resolve } from 'node:path';
import { chromium, expect } from '@playwright/test';
import { MykoClient, ConnectionStatus, decode } from './artifacts/legacy-client.mjs';
import { randomUUID, createHash } from 'node:crypto';

const artifacts = resolve(import.meta.dirname, process.env.MARSHAL_BROWSER_ARTIFACTS ?? 'artifacts');
await mkdir(artifacts, { recursive: true });
const checks = [];
const errors = [];
const transportErrors = [];
const check = async (name, action) => { await action(); checks.push(name); console.log('PASS ' + name); };
const runtime = await startDevRuntime({errors});
const {origin, proxy, issuer, binary, dist, agentPort, authBinary, startAuth, startDaemon} = runtime;
const frontendFiles = (await readdir(dist)).filter(name => /\.(html|js|wasm)$/.test(name)).sort();
assert.ok(frontendFiles.includes('index.html') && frontendFiles.some(name => name.endsWith('.wasm')),
  'browser verification requires a built UI bundle');
const frontend = Object.fromEntries(await Promise.all(frontendFiles.map(async name => [
  name, createHash('sha256').update(await readFile(resolve(dist, name))).digest('hex'),
])));
const artifact = {binary, sha256:createHash('sha256').update(await readFile(binary)).digest('hex'), postgres:Boolean(process.env.MYKO_POSTGRES_URL), issuer:'signed test issuer', frontend};
artifact.auth = { binary: authBinary, sha256: createHash('sha256').update(await readFile(authBinary)).digest('hex') };
const authChild = startAuth();
const child = startDaemon();
let browser;
const legacy = new MykoClient();
let legacyRows = [];
let legacySubscription;
const agentSession = (id) => ({ id, pid: 42, cwd: '/fixture', connectedAt: Date.now(), operator: 'legacy-agent', agentId: 'existing-client', channelsEnabled: false });
const emit = (item, changeType) => legacy.sendEvent({ item, changeType, itemType: 'Session', createdAt: new Date().toISOString(), tx: randomUUID(), sourceId: 'marshal-browser-verification' });
const command = (commandId, command) => legacy.sendCommand({ commandId, command }, { timeoutMs: 10000 });
const mcp = async (port, code) => {
  const response = await fetch(`http://127.0.0.1:${port}/myko/mcp`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'tools/call', params: {name: 'execute', arguments: {code}} }) });
  return { status: response.status, body: await response.json() };
};
try {
  await expect.poll(async () => { try { return (await mcp(agentPort, "return await myko.query('GetAllSessions',{});")).status; } catch { return 0; } }, { timeout: 20000 }).toBe(200);
  browser = await chromium.launch({ headless: true, args: ['--no-sandbox', '--disable-dev-shm-usage'] });
  const aliceContext = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
  const bobContext = await browser.newContext({ viewport: { width: 1000, height: 850 } });
  aliceContext.setDefaultTimeout(15000);
  bobContext.setDefaultTimeout(15000);
  const alice = await aliceContext.newPage();
  const bob = await bobContext.newPage();
  let preloginSockets = 0;
  for (const context of [aliceContext, bobContext]) await context.route('**/auth/user/**', route => route.abort());
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
  await check('anonymous browsers bootstrap auth through Myko before mounting the application', async () => {
    await expect(alice.getByTestId('auth-google')).toBeVisible({ timeout: 30000 });
    await expect.poll(() => preloginSockets).toBeGreaterThanOrEqual(2);
    await expect(alice.getByTestId('status-footer')).toHaveCount(0);
    const response = await fetch(origin + '/myko/mcp', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({jsonrpc:'2.0',id:1,method:'tools/call',params:{name:'execute',arguments:{code:"return await myko.query('GetAllSessions',{});"}}}) });
    const rejection = await response.json();
    assert.equal(response.status, 200);
    assert.ok(rejection.error || rejection.result?.isError, 'Pulse auth endpoint must not serve Marshal roster');
    const relay = await mcp(agentPort, "return await myko.query('GetAllSessions',{});");
    assert.equal(relay.status, 200);
    assert.ok(!relay.body.error && !relay.body.result?.isError, JSON.stringify(relay.body));
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
  await check('native wake bridges observe acknowledgements and stop treating read messages as unread', async () => {
    let reads = [];
    const subscription = legacy.watchQuery({queryId:'GetAllMessageReads',queryItemType:'MessageRead',query:{}}).subscribe({next: rows => { reads = rows; }, error: error => errors.push(error.message)});
    try {
      const sent = await command('SendMessage', {asSession:'legacy-a',toSessionId:'legacy-b',body:'wake acknowledgement probe',responseRequired:false});
      const ack = await command('AckMessages', {asSession:'legacy-b',messageIds:[sent.messageId]});
      assert.equal(ack.newlyAcked, 1);
      await expect.poll(() => reads.some(read => read.messageId === sent.messageId && read.sessionId === 'legacy-b')).toBe(true);
      const unread = await command('ReadMessages', {asSession:'legacy-b',inbox:true,unread:true});
      assert.ok(unread.messages.every(message => message.messageId !== sent.messageId));
    } finally { subscription.unsubscribe(); }
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
  await check('dashboard exposes monitoring views without Marshal write controls', async () => {
    await expect(alice.getByRole('button', {name:'Console', exact:true})).toHaveCount(0);
    await expect(alice.getByText('legacy-b', {exact:true}).first()).toBeVisible();
    await expect(alice.getByRole('button', {name:'go online', exact:true})).toHaveCount(0);
    await alice.getByRole('button', {name:'Zoo', exact:true}).first().click();
    await expect(alice.getByRole('heading', {name:'Marshal Zoo', exact:true})).toBeVisible();
    await expect(alice.getByLabel('Import Zoo run')).toHaveCount(0);
    await alice.getByRole('button', {name:'Roster', exact:true}).first().click();
  });
  await check('Pulse presence and logout operate independently of the Marshal relay', async () => {
    await login(bob, 'Bob');
    await expect(alice.locator('.pulse-room-participant[aria-label^="Bob"]')).toBeVisible();
    await alice.getByRole('button', {name:'Account menu', exact:true}).click();
    await alice.getByRole('button', {name:'Sign out', exact:true}).click();
    await expect(alice.getByTestId('auth-google')).toBeVisible();
    await expect(bob.locator('.pulse-room-participant[aria-label^="Alice"]')).toHaveCount(0);
    const response = await mcp(agentPort, "return await myko.query('GetAllSessions',{});");
    assert.equal(response.status, 200);
    assert.ok(JSON.stringify(response.body).includes('legacy-b'));
  });
  assert.deepEqual(errors, []);
  await writeFile(resolve(artifacts, 'result.json'), JSON.stringify({ status: 'passed', artifact, checks }, null, 2));
} catch (error) {
  const page = browser?.contexts()[0]?.pages()[0];
  const visibleText = await page?.locator('body').innerText().catch(() => '');
  await page?.screenshot({path:resolve(artifacts,'failure.png'),fullPage:true}).catch(() => {});
  await writeFile(resolve(artifacts, 'result.json'), JSON.stringify({ status: 'failed', artifact, checks, error: error.stack, browserErrors: errors, transportErrors, visibleText, serverLog: runtime.serverLog }, null, 2));
  throw error;
} finally {
  legacySubscription?.unsubscribe();
  legacy.disconnect();
  await browser?.close();
  child.kill('SIGTERM');
  authChild.kill('SIGTERM');
  proxy.closeAllConnections();
  await new Promise((done) => proxy.close(done));
  await issuer.close();
  await writeFile(resolve(artifacts, 'server.log'), runtime.serverLog);
}
