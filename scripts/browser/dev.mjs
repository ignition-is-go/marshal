import { readFile, writeFile, mkdir } from 'node:fs/promises';
import { resolve } from 'node:path';
import { execFileSync } from 'node:child_process';
import { startDevRuntime } from './dev-runtime.mjs';

// Optional mode-0600 JSON configuration keeps secrets out of command arguments.
if (process.env.MARSHAL_DEV_CONFIG) {
  Object.assign(process.env, JSON.parse(await readFile(process.env.MARSHAL_DEV_CONFIG, 'utf8')));
}
if (!process.env.MYKO_POSTGRES_URL) throw new Error('A dedicated dev MYKO_POSTGRES_URL is required');
const database = new URL(process.env.MYKO_POSTGRES_URL);
if (!['127.0.0.1', 'localhost'].includes(database.hostname) || !database.pathname.includes('dev')) {
  throw new Error('Local dev requires a loopback database with dev in its name');
}
const fixture = process.argv.includes('--fixture');
const target = JSON.parse(execFileSync('cargo', ['metadata', '--no-deps', '--format-version', '1'], {cwd:resolve(import.meta.dirname,'../..'),encoding:'utf8'})).target_directory;
const runtime = await startDevRuntime({fixture,binary:resolve(target,'debug/marshal-daemon'),port:Number(process.env.MARSHAL_DEV_PORT ?? 8878)});
const child = runtime.startDaemon();
const artifacts = resolve(import.meta.dirname, 'artifacts/dev');
await mkdir(artifacts, {recursive:true,mode:0o700});
const identity = {origin:runtime.origin,mode:fixture ? 'signed test issuer' : 'configured OIDC',pid:child.pid,agentPort:runtime.agentPort,hookPort:runtime.hookPort,commit:execFileSync('git',['rev-parse','HEAD'],{cwd:resolve(import.meta.dirname,'../..'),encoding:'utf8'}).trim()};
let stopping = false;
async function stop() {
  if (stopping) return;
  stopping = true;
  if (child.exitCode === null && child.signalCode === null) {
    const exited = new Promise(done => child.once('exit',done));
    child.kill('SIGTERM');
    await exited;
  }
  runtime.proxy.closeAllConnections();
  await new Promise(done => runtime.proxy.close(done));
  await runtime.issuer?.close();
  await writeFile(resolve(artifacts,'daemon.log'),runtime.serverLog,{mode:0o600});
}
process.once('SIGINT',stop);
process.once('SIGTERM',stop);
child.once('exit', () => { if (!stopping) {process.exitCode=1;void stop();} });
try {
  let ready = false;
  for (let attempt=0;attempt<100;attempt++) {
    if (child.exitCode !== null) throw new Error('Dev daemon exited before readiness');
    try {
      const response = await fetch(runtime.origin+'/myko/mcp',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({jsonrpc:'2.0',id:1,method:'tools/list',params:{}})});
      if (response.ok) {ready=true;break;}
    } catch {}
    await new Promise(done=>setTimeout(done,200));
  }
  if (!ready) throw new Error('Dev daemon did not become ready');
  await writeFile(resolve(artifacts,'server.json'),JSON.stringify(identity,null,2));
  console.log(`Marshal dev ready: ${identity.origin}\nAuth: ${identity.mode}\nDatabase: dedicated local Postgres\nPID: ${identity.pid}\nMyko and hook listeners remain on loopback.`);
  if (fixture) console.log('Test accounts: Alice and Bob. This does not validate the real identity provider.');
} catch (error) {
  await stop();
  throw error;
}
