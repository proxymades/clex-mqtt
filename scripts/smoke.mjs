import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { randomBytes, createHash, createHmac, pbkdf2Sync, timingSafeEqual } from 'node:crypto';
import assert from 'node:assert/strict';
import mqtt from 'mqtt';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const local = fs.existsSync(path.join(root, '.env')) ? fs.readFileSync(path.join(root, '.env'), 'utf8') : '';
const values = Object.fromEntries(local.split(/\r?\n/).filter(line => line && !line.startsWith('#') && line.includes('=')).map(line => { const at = line.indexOf('='); return [line.slice(0, at), line.slice(at + 1).replace(/^["']|["']$/g, '')]; }));
const env = { ...values, ...process.env };
if (env.CLEX_ENV === 'production') throw new Error('This smoke script is for isolated local development only.');
const origin = new URL(env.CLEX_PUBLIC_URL || 'http://127.0.0.1:8085');
if (!['127.0.0.1', 'localhost', '[::1]'].includes(origin.hostname) || origin.protocol !== 'http:') throw new Error('Smoke tests require a local HTTP console.');
if (!env.CLEX_ADMIN_PASSWORD) throw new Error('CLEX_ADMIN_PASSWORD is required in the private local environment.');
let tlsUrl, tlsCa;
if (env.CLEX_SMOKE_TLS_URL || env.CLEX_SMOKE_TLS_CA_FILE) {
  if (!env.CLEX_SMOKE_TLS_URL || !env.CLEX_SMOKE_TLS_CA_FILE) throw new Error('Optional TLS smoke requires both CLEX_SMOKE_TLS_URL and CLEX_SMOKE_TLS_CA_FILE.');
  tlsUrl = new URL(env.CLEX_SMOKE_TLS_URL);
  if (tlsUrl.protocol !== 'mqtts:' || !['127.0.0.1', 'localhost', '[::1]'].includes(tlsUrl.hostname) || tlsUrl.username || tlsUrl.password || tlsUrl.search || tlsUrl.hash || !['', '/'].includes(tlsUrl.pathname)) throw new Error('TLS smoke requires a local mqtts:// broker URL without embedded credentials or options.');
  tlsCa = fs.readFileSync(path.resolve(root, env.CLEX_SMOKE_TLS_CA_FILE));
}
const suffix = randomBytes(8).toString('hex');
const username = `smoke-${suffix}`;
const password = randomBytes(24).toString('base64url');
const topic = `clex/smoke/${suffix}`;
let cookie, csrf, created = false;
const clients = new Set();

async function api(route, { method = 'GET', body } = {}) {
  const response = await fetch(new URL(`/api/v1${route}`, origin), {
    method, signal: AbortSignal.timeout(10000),
    headers: { ...(cookie ? { Cookie: cookie } : {}), ...(csrf ? { 'X-CSRF-Token': csrf } : {}), Origin: origin.origin, ...(body ? { 'Content-Type': 'application/json' } : {}) },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!response.ok) throw new Error(`HTTP ${response.status} for ${route}`);
  return response;
}

async function connect(url, protocolVersion, name, extra = {}, setup) {
  const client = mqtt.connect(url, { username, password, clientId: `${username}-${name}`, protocolVersion, protocolId: protocolVersion === 3 ? 'MQIsdp' : 'MQTT', clean: true, reconnectPeriod: 0, connectTimeout: 7000, ...extra, manualConnect: true });
  clients.add(client);
  client.on('error', () => {});
  const ready = new Promise((resolve, reject) => {
    client.once('connect', packet => {
      // MQTT 5 section 3.2.2.3.13: an omitted property means available.
      const sharedAvailable = packet.properties?.sharedSubscriptionAvailable ?? true;
      if (protocolVersion === 5 && sharedAvailable !== true) {
        reject(new Error(`MQTT 5 CONNACK must advertise shared subscriptions for ${name}`));
      } else resolve();
    });
    client.once('error', () => reject(new Error(`MQTT connection failed for ${name}`)));
  });
  setup?.(client);
  client.connect();
  await ready;
  return client;
}

async function connectScram(url, name) {
  const method = 'SCRAM-SHA-256';
  const nonce = randomBytes(24).toString('base64url');
  const bare = `n=${username},r=${nonce}`;
  let expectedSignature, verificationError, verified = false;
  const client = await connect(url, 5, name, {
    username: undefined, password: undefined,
    properties: { authenticationMethod: method, authenticationData: Buffer.from(`n,,${bare}`) },
  }, (client) => {
    client.handleAuth = (packet, callback) => {
      try {
        assert.equal(packet.reasonCode, 0x18);
        assert.equal(packet.properties.authenticationMethod, method);
        const challenge = packet.properties.authenticationData.toString('utf8');
        const fields = Object.fromEntries(challenge.split(',').map(field => [field.slice(0, 1), field.slice(2)]));
        assert(fields.r.startsWith(nonce) && fields.r.length > nonce.length);
        assert(/^\d+$/.test(fields.i) && Number(fields.i) >= 4096 && Number(fields.i) <= 1_000_000);
        const salted = pbkdf2Sync(password, Buffer.from(fields.s, 'base64'), Number(fields.i), 32, 'sha256');
        const mac = (key, value) => createHmac('sha256', key).update(value).digest();
        const clientKey = mac(salted, 'Client Key');
        const storedKey = createHash('sha256').update(clientKey).digest();
        const final = `c=biws,r=${fields.r}`;
        const transcript = `${bare},${challenge},${final}`;
        const signature = mac(storedKey, transcript);
        const proof = Buffer.from(clientKey.map((byte, index) => byte ^ signature[index]));
        expectedSignature = mac(mac(salted, 'Server Key'), transcript);
        callback(null, { cmd: 'auth', reasonCode: 0x18, properties: { authenticationMethod: method, authenticationData: Buffer.from(`${final},p=${proof.toString('base64')}`) } });
      } catch { callback(new Error('SCRAM challenge verification failed')); }
    };
    client.on('connect', packet => {
      try {
        assert.equal(packet.properties.authenticationMethod, method);
        const final = packet.properties.authenticationData.toString('utf8');
        assert(final.startsWith('v=') && expectedSignature);
        const received = Buffer.from(final.slice(2), 'base64');
        assert(received.length === expectedSignature.length && timingSafeEqual(received, expectedSignature));
        verified = true;
      } catch { verificationError = new Error('SCRAM server signature verification failed'); }
    });
  });
  if (verificationError) throw verificationError;
  assert(verified, 'SCRAM server signature must verify before MQTT use');
  return client;
}

function message(client) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => { client.off('message', receive); reject(new Error('MQTT delivery timed out')); }, 7000);
    function receive(actualTopic, payload, packet) { clearTimeout(timer); resolve({ actualTopic, payload, packet }); }
    client.once('message', receive);
  });
}

async function waitUntil(ready) {
  const deadline = Date.now() + 7000;
  while (!ready()) {
    if (Date.now() >= deadline) throw new Error('MQTT shared delivery timed out');
    await new Promise(resolve => setTimeout(resolve, 20));
  }
}

try {
  const login = await api('/session', { method: 'POST', body: { username: env.CLEX_ADMIN_USERNAME || 'admin', password: env.CLEX_ADMIN_PASSWORD } });
  cookie = login.headers.get('set-cookie')?.split(';')[0];
  csrf = (await login.json()).csrfToken;
  assert(cookie && csrf, 'Session cookie and CSRF token required');
  await api('/users', { method: 'POST', body: { username, password, publishFilters: [`${topic}/#`], subscribeFilters: [`${topic}/#`] } });
  created = true;
  const mqttAddress = env.CLEX_MQTT_ADDR || '127.0.0.1:1883';
  const tcp = `mqtt://${mqttAddress.replace(/^0\.0\.0\.0:/, '127.0.0.1:').replace(/^\[::\]:/, '[::1]:')}`;
  for (const version of [3, 4, 5]) {
    const subscriber = await connect(tcp, version, `sub${version}`);
    const publisher = await connect(tcp, version, `pub${version}`);
    await subscriber.subscribeAsync(`${topic}/v${version}`, { qos: 2 });
    const received = message(subscriber);
    const payload = Buffer.from([0, 1, 2, 127, 128, 254, 255]);
    await publisher.publishAsync(`${topic}/v${version}`, payload, { qos: 2, retain: true });
    const result = await received;
    assert.equal(result.actualTopic, `${topic}/v${version}`);
    assert.deepEqual(result.payload, payload);
    await subscriber.endAsync(); await publisher.endAsync();
    console.log(`PASS: MQTT ${version === 3 ? '3.1' : version === 4 ? '3.1.1' : '5.0'} TCP, binary payload and QoS 2`);
  }
  const scram = await connectScram(tcp, 'scram');
  await scram.subscribeAsync(`${topic}/scram`, { qos: 2 });
  const scramMessage = message(scram);
  const scramPayload = Buffer.from([0, 12, 128, 255]);
  await scram.publishAsync(`${topic}/scram`, scramPayload, { qos: 2 });
  assert.deepEqual((await scramMessage).payload, scramPayload);
  await scram.endAsync();
  console.log('PASS: MQTT 5 SCRAM-SHA-256, independent server-signature verification and binary QoS 2');
  const workers = [await connect(tcp, 5, 'shared-left'), await connect(tcp, 5, 'shared-right')];
  const observer = await connect(tcp, 5, 'shared-observer');
  const sharedPublisher = await connect(tcp, 5, 'shared-publisher');
  const sharedTopic = `${topic}/shared`;
  const sharedFilter = `$share/smoke-${suffix}/${sharedTopic}`;
  const workerMessages = [[], []], observed = [];
  const capture = target => (actualTopic, payload, packet) => target.push({ actualTopic, payload, packet });
  workers.forEach((client, index) => client.on('message', capture(workerMessages[index])));
  observer.on('message', capture(observed));
  for (const worker of workers) {
    const granted = await worker.subscribeAsync(sharedFilter, { qos: 1 });
    assert.equal(granted[0].qos, 1, 'Shared subscribe must authorize the inner topic filter');
  }
  await observer.subscribeAsync(sharedTopic, { qos: 1 });
  const sharedPayloads = Array.from({ length: 7 }, (_, index) => Buffer.from([0, index + 1, 127, 128, 254, 255]));
  for (const payload of sharedPayloads.slice(0, 6)) await sharedPublisher.publishAsync(sharedTopic, payload, { qos: 1, retain: false });
  await waitUntil(() => workerMessages.flat().length >= 6 && observed.length >= 6);
  assert.deepEqual(workerMessages.map(messages => messages.length), [3, 3], 'Two eligible workers must share work evenly');
  assert.equal(observed.length, 6, 'Ordinary observer must receive every publication');
  await workers[0].unsubscribeAsync(sharedFilter);
  await sharedPublisher.publishAsync(sharedTopic, sharedPayloads[6], { qos: 1, retain: false });
  await waitUntil(() => workerMessages[1].length >= 4 && observed.length >= 7);
  await new Promise(resolve => setTimeout(resolve, 200));
  assert.deepEqual(workerMessages.map(messages => messages.length), [3, 4], 'Unsubscribed worker must stop receiving while the remaining worker continues');
  const groupCopies = workerMessages.flat();
  assert.equal(groupCopies.length, sharedPayloads.length, 'Every publication must have exactly one shared group copy');
  assert.equal(observed.length, sharedPayloads.length, 'Ordinary observer must have one copy per publication');
  for (const messages of [groupCopies, observed]) {
    for (const expected of sharedPayloads) {
      const copies = messages.filter(result => result.payload.equals(expected));
      assert.equal(copies.length, 1, 'Binary publication must arrive exactly once per delivery path');
      assert.equal(copies[0].actualTopic, sharedTopic);
      assert.equal(copies[0].packet.qos, 1);
      assert.equal(copies[0].packet.retain, false);
    }
  }
  for (const client of [...workers, observer, sharedPublisher]) await client.endAsync();
  console.log('PASS: MQTT 5 shared CONNACK capability, inner-filter ACL, balanced binary QoS 1 delivery, ordinary observer and unsubscribe');
  const websocket = new URL('/mqtt', origin); websocket.protocol = 'ws:';
  const ws = await connect(websocket.toString(), 5, 'websocket');
  await ws.subscribeAsync(`${topic}/ws`, { qos: 1 });
  const received = message(ws);
  await ws.publishAsync(`${topic}/ws`, 'websocket-ok', { qos: 1 });
  assert.equal((await received).payload.toString(), 'websocket-ok');
  await ws.endAsync();
  console.log('PASS: MQTT 5 WebSocket');
  if (tlsUrl && tlsCa) {
    const tlsOptions = { ca: tlsCa, rejectUnauthorized: true };
    const subscriber = await connect(tlsUrl.toString(), 5, 'tls-sub', tlsOptions);
    const publisher = await connect(tlsUrl.toString(), 5, 'tls-pub', tlsOptions);
    for (const client of [subscriber, publisher]) {
      assert.equal(client.stream.encrypted, true, 'Native MQTT TLS transport required');
      assert.equal(client.stream.authorized, true, 'TLS peer must verify against the explicit CA');
    }
    await subscriber.subscribeAsync(`${topic}/tls`, { qos: 1 });
    const received = message(subscriber);
    const payload = Buffer.from([0, 3, 17, 127, 128, 253, 255]);
    await publisher.publishAsync(`${topic}/tls`, payload, { qos: 1, retain: false });
    const result = await received;
    assert.equal(result.actualTopic, `${topic}/tls`);
    assert.deepEqual(result.payload, payload);
    assert.equal(result.packet.qos, 1);
    assert.equal(result.packet.retain, false);
    await subscriber.endAsync(); await publisher.endAsync();
    console.log('PASS: MQTT 5 native TLS with explicit CA verification, binary payload and QoS 1');
  }
  const response = await api('/retained');
  const retained = await response.json();
  assert(retained.some(item => item.topic === `${topic}/v5`));
  console.log('PASS: authenticated management API and retained view');
  const streamResponse = await api(`/messages?username=${encodeURIComponent(username)}&topic=${encodeURIComponent(`${topic}/#`)}`);
  assert.equal(streamResponse.status, 200);
  const stream = await streamResponse.json();
  assert(stream.messages.length > 0, 'Accepted publications must be visible in the message stream');
  assert(stream.messages.every(item => item.username === username && item.topic.startsWith(`${topic}/`)), 'Stream filters must isolate the selected publisher and topics');
  assert(stream.messages.some(item => item.retain === false), 'Ordinary publications must appear alongside Retain publications');
  console.log('PASS: filtered publication stream, including ordinary MQTT messages');
} finally {
  for (const client of clients) { if (!client.disconnected) await client.endAsync(true).catch(() => {}); }
  if (created) {
    for (const version of [3, 4, 5]) await api('/retained/delete', { method: 'POST', body: { topic: `${topic}/v${version}` } }).catch(() => {});
    await api(`/users/${username}`, { method: 'PATCH', body: { enabled: false, password: randomBytes(24).toString('base64url'), publishFilters: [], subscribeFilters: [] } });
    console.log('Cleanup: temporary MQTT account disabled; retained test values removed.');
  }
  if (cookie && csrf) await api('/session', { method: 'DELETE' }).catch(() => {});
}
