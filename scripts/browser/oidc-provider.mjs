// Signed OIDC test issuer from pulse-multi-user v0.9.0, commit 63fa4018.
// Kept here so Marshal verification is reproducible without a sibling checkout.
import { createServer } from 'node:http';
import { createHash, generateKeyPairSync, randomBytes, randomInt, sign } from 'node:crypto';

const databaseConnection = 'Username-Password-Authentication';
const identities = ['Alice', 'Bob', 'Charlie', 'Dana', 'Ellis', 'Fran', 'Gale'];
const htmlEscape = (value) => String(value).replace(/[&<>"']/g, (character) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[character]);

export async function startOidcFixture({ appOrigin, internalIssuer = false }) {
  let issuer = appOrigin + '/oidc/';
  const clientId = 'pulse-multi-user-local-example';
  const clientSecret = randomBytes(32).toString('hex');
  const { privateKey, publicKey } = generateKeyPairSync('rsa', { modulusLength: 2048 });
  const kid = randomBytes(12).toString('hex');
  const codes = new Map();
  const inbox = new Map();
  let nextClaims = {};
  const encode = (value) => Buffer.from(JSON.stringify(value)).toString('base64url');
  const token = (email, nonce, connection) => {
    const account = identities.find((name) => email === name.toLowerCase() + '@fixture.test');
    const claims = {
      iss: issuer, sub: connection === databaseConnection ? 'auth0|fixture-' + email : 'fixture:' + email, aud: clientId, azp: clientId,
      iat: Math.floor(Date.now() / 1000), exp: Math.floor(Date.now() / 1000) + 3600,
      email, email_verified: true, name: account ?? email.split('@')[0],
      ...(account === 'Alice' ? { picture: appOrigin + '/oidc/avatar.svg' } : {}),
      ...(nonce ? { nonce } : {}), ...nextClaims,
    };
    nextClaims = {};
    const payload = encode({ alg: 'RS256', typ: 'JWT', kid }) + '.' + encode(claims);
    return payload + '.' + sign('RSA-SHA256', Buffer.from(payload), privateKey).toString('base64url');
  };
  const json = (response, status, body) => {
    response.writeHead(status, { 'content-type': 'application/json', 'cache-control': 'no-store' });
    response.end(JSON.stringify(body));
  };
  const server = createServer(async (request, response) => {
    try {
      const url = new URL(request.url, issuer);
      if (!url.pathname.startsWith('/oidc/')) return json(response, 404, { error: 'not_found' });
      const pathname = url.pathname.slice('/oidc'.length);
      if (request.method === 'GET' && pathname === '/.well-known/openid-configuration') {
        return json(response, 200, { issuer, jwks_uri: issuer + 'jwks', authorization_endpoint: issuer + 'authorize', token_endpoint: issuer + 'oauth/token' });
      }
      if (request.method === 'GET' && pathname === '/jwks') {
        return json(response, 200, { keys: [{ ...publicKey.export({ format: 'jwk' }), kid, alg: 'RS256', use: 'sig' }] });
      }
      if (request.method === 'GET' && pathname === '/avatar.svg') {
        response.writeHead(200, { 'content-type': 'image/svg+xml' });
        return response.end('<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32"><rect width="32" height="32" rx="16" fill="#00bbcc"/><circle cx="16" cy="12" r="6" fill="white"/><path d="M5 29a11 11 0 0 1 22 0" fill="white"/></svg>');
      }
      if (request.method === 'GET' && pathname === '/test/inbox') {
        const code = inbox.get(url.searchParams.get('email'));
        return json(response, code ? 200 : 404, code ? { code } : { error: 'no_code' });
      }
      if (request.method === 'GET' && pathname === '/authorize') {
        const connection = url.searchParams.get('connection');
        if (connection === databaseConnection) {
          if (url.searchParams.get('screen_hint') !== 'login') return json(response, 400, { error: 'invalid_request' });
          response.writeHead(200, { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' });
          return response.end('<!doctype html><html lang="en"><meta charset="utf-8"><title>Existing account sign-in</title><h1>Sign in to an existing account</h1><form method="post" action="' + htmlEscape(url.pathname + url.search) + '"><label>Email<input name="email" type="email" autocomplete="username" required></label><label>Password<input name="password" type="password" autocomplete="current-password" required></label><button type="submit">Sign in</button></form></html>');
        }
        if (connection !== 'google-oauth2') return json(response, 400, { error: 'invalid_request' });
        const buttons = identities.map((name) => '<button type="submit" name="identity" value="' + name + '">' + name + '</button>').join('');
        response.writeHead(200, { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' });
        return response.end('<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>Local validation sign-in</title><style>body{margin:0;min-height:100vh;display:grid;place-items:center;background:#101114;color:#eee;font:16px system-ui}main{max-width:30rem;padding:2rem;border:1px solid #34383e;border-radius:1rem}p{line-height:1.6;color:#aab1bc}form{display:grid;grid-template-columns:1fr 1fr;gap:.75rem}button{padding:1rem;border:1px solid #47505c;border-radius:.5rem;background:#20242c;color:white;cursor:pointer}button:hover,button:focus{border-color:#00bbcc}small{color:#00bbcc}</style><main><small>LOCAL VALIDATION ISSUER</small><h1>Choose a test identity</h1><p>This local provider exercises the shared signed Google code flow. These accounts are only for this example. Open another browser to choose a different person.</p><form method="post" action="' + htmlEscape(url.pathname + url.search) + '">' + buttons + '</form></main></html>');
      }
      if (request.method === 'GET' && pathname === '/v2/logout') {
        const returnTo = url.searchParams.get('returnTo');
        if (url.searchParams.get('client_id') !== clientId || returnTo !== appOrigin + '/') {
          return json(response, 400, { error: 'invalid_request' });
        }
        response.writeHead(302, { location: returnTo, 'cache-control': 'no-store' });
        return response.end();
      }
      let raw = '';
      for await (const part of request) {
        raw += part.toString();
        if (Buffer.byteLength(raw) > 8192) return json(response, 413, { error: 'too_large' });
      }
      if (request.method === 'POST' && pathname === '/authorize') {
        const form = new URLSearchParams(raw);
        const connection = url.searchParams.get('connection');
        const identity = connection === databaseConnection
          ? identities.find((name) => form.get('email') === name.toLowerCase() + '@fixture.test' && form.get('password') === 'fixture-password')
          : connection === 'google-oauth2' ? form.get('identity') : undefined;
        const redirect = new URL(url.searchParams.get('redirect_uri'));
        if (!identities.includes(identity) || redirect.href !== appOrigin + '/' || url.searchParams.get('client_id') !== clientId || url.searchParams.get('code_challenge_method') !== 'S256') {
          return json(response, 400, { error: 'invalid_request' });
        }
        const code = randomBytes(24).toString('hex');
        codes.set(code, { email: identity.toLowerCase() + '@fixture.test', nonce: url.searchParams.get('nonce'), challenge: url.searchParams.get('code_challenge'), redirect: redirect.href, connection });
        redirect.searchParams.set('code', code);
        redirect.searchParams.set('state', url.searchParams.get('state') ?? '');
        response.writeHead(302, { location: redirect.href, 'cache-control': 'no-store' });
        return response.end();
      }
      const contentType = request.headers['content-type']?.split(';')[0];
      const body = !raw ? {} : contentType === 'application/x-www-form-urlencoded'
        ? Object.fromEntries(new URLSearchParams(raw))
        : JSON.parse(raw);
      if (body.client_id !== clientId || body.client_secret !== clientSecret) return json(response, 401, { error: 'unauthorized' });
      if (request.method === 'POST' && pathname === '/passwordless/start') {
        if (typeof body.email !== 'string' || !body.email.endsWith('@fixture.test')) return json(response, 403, { error: 'access_denied' });
        inbox.set(body.email, String(randomInt(1_000_000)).padStart(6, '0'));
        return json(response, 200, {});
      }
      if (request.method === 'POST' && pathname === '/oauth/token') {
        if (body.grant_type === 'http://auth0.com/oauth/grant-type/passwordless/otp') {
          const expected = inbox.get(body.username);
          if (!expected || expected !== body.otp) return json(response, 400, { error: 'invalid_grant' });
          inbox.delete(body.username);
          return json(response, 200, { id_token: token(body.username), token_type: 'Bearer' });
        }
        if (body.grant_type === 'authorization_code') {
          const pending = codes.get(body.code);
          codes.delete(body.code);
          const challenge = typeof body.code_verifier === 'string' ? createHash('sha256').update(body.code_verifier).digest('base64url') : null;
          if (!pending || challenge !== pending.challenge || body.redirect_uri !== pending.redirect) return json(response, 400, { error: 'invalid_grant' });
          return json(response, 200, { id_token: token(pending.email, pending.nonce, pending.connection), token_type: 'Bearer' });
        }
      }
      return json(response, 404, { error: 'not_found' });
    } catch { return json(response, 400, { error: 'invalid_request' }); }
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  if (internalIssuer) issuer = 'http://127.0.0.1:' + server.address().port + '/oidc/';
  return {
    issuer, clientId, clientSecret, port: server.address().port,
    codeFor: (email) => inbox.get(email),
    setNextClaims: (claims) => { nextClaims = claims; },
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}
