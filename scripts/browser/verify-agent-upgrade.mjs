import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { createHash, randomUUID } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { writeFile } from 'node:fs/promises';
import { createServer } from 'node:net';
import { setTimeout as sleep } from 'node:timers/promises';
import { MykoClient, ConnectionStatus } from './artifacts/legacy-client.mjs';

const [baseline, candidate] = process.argv.slice(2);
assert.ok(baseline && candidate, 'provide baseline and candidate daemon paths');
assert.ok(process.env.MYKO_POSTGRES_URL, 'configure a disposable coordination database');
const digest = async path => {
  const hash = createHash('sha256');
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest('hex');
};
const baselineHash = await digest(baseline);
const candidateHash = await digest(candidate);
assert.notEqual(candidateHash, baselineHash, 'upgrade requires distinct binaries');

async function freePort() {
  const server = createServer();
  await new Promise((resolve, reject) => server.on('error', reject).listen(0, '127.0.0.1', resolve));
  const {port} = server.address();
  await new Promise(resolve => server.close(resolve));
  return port;
}

async function until(predicate, label) {
  const deadline = Date.now() + 20000;
  while (Date.now() < deadline) {
    if (await predicate()) return;
    await sleep(100);
  }
  throw new Error('timed out waiting for ' + label);
}

const relayPort = await freePort();
const hookPort = await freePort();
const relay = `ws://127.0.0.1:${relayPort}/myko`;
let daemon;
const client = new MykoClient();
let rows = [];
let subscription;
const phases = [];
const command = (commandId, payload) => client.sendCommand({commandId, command:payload}, {timeoutMs:5000});

async function stop() {
  if (!daemon || daemon.exitCode !== null || daemon.signalCode !== null) return;
  const exited = once(daemon, 'exit');
  daemon.kill('SIGTERM');
  await exited;
  assert.equal(daemon.exitCode, 0, 'daemon must stop cleanly');
}

async function start(binary) {
  daemon = spawn(binary, [], {
    env: {
      PATH: process.env.PATH,
      MARSHAL_BIND: `127.0.0.1:${relayPort}`,
      MARSHAL_HOOK_BIND: `127.0.0.1:${hookPort}`,
      MYKO_POSTGRES_URL: process.env.MYKO_POSTGRES_URL,
      MYKO_POSTGRES_TABLE: 'marshal_events',
    },
    stdio: ['ignore', 'ignore', 'inherit'],
  });
  await once(daemon, 'spawn');
  await until(async () => {
    if (daemon.exitCode !== null) throw new Error('daemon exited during startup');
    try {
      const response = await fetch(`http://127.0.0.1:${relayPort}/myko/mcp`, {
        method:'POST', headers:{'content-type':'application/json'},
        body:JSON.stringify({jsonrpc:'2.0', id:1, method:'tools/call', params:{
          name:'execute', arguments:{code:"return await myko.query('GetAllSessions',{});"},
        }}),
        signal:AbortSignal.timeout(1000),
      });
      const {result} = await response.json();
      return response.ok && result && !result.isError;
    } catch {
      return false;
    }
  }, 'daemon MCP readiness');
}

async function roundTrip(phase) {
  const body = `${phase} ${randomUUID()}`;
  const sent = await command('SendMessage', {
    asSession:'upgrade-a', toSessionId:'upgrade-b', body, responseRequired:false,
  });
  const inbox = await command('ReadMessages', {asSession:'upgrade-b', inbox:true, unread:true});
  assert.ok(inbox.messages.some(message => message.messageId === sent.messageId && message.body === body));
  phases.push({phase, messageId:sent.messageId, daemonPid:daemon.pid});
}

try {
  await start(baseline);
  subscription = client.watchQuery({queryId:'GetAllSessions', queryItemType:'Session', query:{}})
    .subscribe({next: value => { rows = value; }});
  client.setAddress(relay);
  await until(() => client.getConnectionStatus() === ConnectionStatus.Connected, 'legacy client');
  for (const id of ['upgrade-a', 'upgrade-b']) {
    await command('RegisterSession', {session:{
      id, pid:42, cwd:'/fixture', connectedAt:Date.now(), operator:'upgrade-fixture', channelsEnabled:false,
    }});
  }
  await until(() => rows.some(row => row.id === 'upgrade-b'), 'initial roster');
  await roundTrip('baseline');

  for (const [phase, binary] of [['candidate', candidate], ['rollback', baseline]]) {
    await stop();
    await until(() => client.getConnectionStatus() !== ConnectionStatus.Connected, 'disconnect');
    await start(binary);
    await until(() => client.getConnectionStatus() === ConnectionStatus.Connected, 'legacy reconnect');
    const inbox = await command('ReadMessages', {asSession:'upgrade-b', inbox:true, unread:true});
    assert.ok(inbox.messages.some(message => message.messageId === phases.at(-1).messageId),
      'previous phase message survives replay');
    await roundTrip(phase);
  }
  const receipt = JSON.stringify({baselineHash, candidateHash, phases}, null, 2);
  if (process.env.MARSHAL_AGENT_UPGRADE_RECEIPT) {
    await writeFile(process.env.MARSHAL_AGENT_UPGRADE_RECEIPT, receipt + '\n');
  }
  console.log(receipt);
} finally {
  subscription?.unsubscribe();
  client.disconnect();
  await stop();
}
