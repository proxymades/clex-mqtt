// MQTT.js load driver for the isolated broker created by benchmark.py.
import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { connectAsync } from 'mqtt';
import { createPublishPacer } from './bench_pacing.mjs';

const config = JSON.parse(process.env.CLEX_BENCH_CONFIG || '{}');
const address = new URL(process.env.CLEX_BENCH_URL || '');
assert.equal(address.protocol, 'mqtt:');
assert(['127.0.0.1', '[::1]', 'localhost'].includes(address.hostname));
assert(!address.username && !address.password);
const clients = new Set();
const profiles = [];
let current = null;
const ns = () => process.hrtime.bigint();
const ms = start => Number(ns() - start) / 1e6;
const pause = delay => new Promise(resolve => setTimeout(resolve, delay));
function stats(values, count = values.length) {
  if (!count) return { count: 0, min: null, p50: null, p95: null, p99: null, max: null };
  const sorted = values.subarray(0, count).sort();
  const percentile = value => Number(sorted[Math.max(0, Math.ceil(count * value) - 1)].toFixed(3));
  return { count, min: Number(sorted[0].toFixed(3)), p50: percentile(.5), p95: percentile(.95), p99: percentile(.99), max: Number(sorted[count - 1].toFixed(3)) };
}
async function profile(qos) {
  const connections = new Float64Array(config.publishers + config.subscribers);
  const total = config.publishers * config.messages;
  const expected = total * config.subscribers;
  const latency = new Float64Array(expected), acknowledgements = new Float64Array(total);
  const marker = randomBytes(8), prefix = `bench/${marker.toString('hex')}/q${qos}`;
  const pattern = Buffer.alloc(config.payloadBytes, 0xa5);
  let received = 0, submittedCount = 0, ackCount = 0, duplicates = 0, corrupt = 0, reordered = 0, connectionIndex = 0, failed = false;
  current = () => ({ qos, plannedPublications: total, submitted: submittedCount, completedPublications: ackCount, expectedDeliveries: expected, received, missing: expected - received, duplicates, corrupt, reordered });
  const observations = Array.from({ length: config.subscribers }, () => ({ seen: new Uint8Array(Math.ceil(total / 8)), count: 0, previous: new Int32Array(config.publishers).fill(-1) }));
  let resolveDelivered;
  const delivered = new Promise(resolve => { resolveDelivered = resolve; });
  const connect = async (kind, index) => {
    const started = ns();
    const client = await connectAsync(address.href, {
      username: process.env.CLEX_BENCH_USERNAME, password: process.env.CLEX_BENCH_PASSWORD,
      clientId: `${marker.toString('hex')}-${qos}-${kind}-${index}`,
      protocolVersion: config.protocol, clean: true, reconnectPeriod: 0, connectTimeout: 10000,
      resubscribe: false, keepalive: 30,
      ...(config.protocol === 5 ? { properties: { receiveMaximum: 128 } } : {}),
    });
    clients.add(client); client.on('error', () => { failed = true; });
    client.on('close', () => { if (clients.has(client)) failed = true; });
    connections[connectionIndex++] = ms(started);
    return client;
  };
  const subscribers = await Promise.all(observations.map(async (observation, index) => {
    const client = await connect('sub', index);
    client.on('message', (topic, payload) => {
      if (payload.length !== config.payloadBytes || !payload.subarray(0, 8).equals(marker) || !payload.subarray(24).equals(pattern.subarray(24))) { corrupt++; return; }
      const publisher = payload.readUInt32BE(8), sequence = payload.readUInt32BE(12);
      if (publisher >= config.publishers || sequence >= config.messages || topic !== `${prefix}/${publisher}`) { corrupt++; return; }
      const position = publisher * config.messages + sequence, bit = 1 << (position % 8);
      if (observation.seen[position >> 3] & bit) { duplicates++; return; }
      observation.seen[position >> 3] |= bit;
      if (sequence < observation.previous[publisher]) reordered++;
      observation.previous[publisher] = Math.max(sequence, observation.previous[publisher]);
      const elapsed = Number(ns() - payload.readBigUInt64BE(16)) / 1e6;
      if (elapsed < 0) { corrupt++; return; }
      latency[received++] = elapsed; observation.count++;
      if (received === expected) resolveDelivered();
    });
    const granted = await client.subscribeAsync(`${prefix}/#`, { qos });
    if (granted.length !== 1 || granted[0].qos !== qos) throw new Error('Benchmark subscription rejected');
    return client;
  }));
  const publishers = await Promise.all(Array.from({ length: config.publishers }, (_, index) => connect('pub', index)));
  if (config.subscriberPauseMs) {
    for (const client of subscribers) client.stream.pause();
    setTimeout(() => { for (const client of subscribers) client.stream.resume(); }, config.subscriberPauseMs);
  }
  const started = ns();
  const pace = createPublishPacer(config.publishRate || 0);
  await Promise.all(publishers.map(async (client, publisher) => {
    let sequence = 0;
    await Promise.all(Array.from({ length: Math.min(config.window, config.messages) }, async () => {
      while (sequence < config.messages) {
        const current = sequence++;
        await pace();
        const payload = Buffer.from(pattern);
        marker.copy(payload); payload.writeUInt32BE(publisher, 8); payload.writeUInt32BE(current, 12);
        const submitted = ns(); payload.writeBigUInt64BE(submitted, 16);
        submittedCount++;
        await client.publishAsync(`${prefix}/${publisher}`, payload, { qos, retain: false });
        acknowledgements[ackCount++] = ms(submitted);
      }
    }));
  }));
  const publishSeconds = ms(started) / 1000;
  let timer;
  try {
    await Promise.race([delivered, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error('Benchmark delivery deadline exceeded')), config.timeout * 1000); })]);
  } finally { clearTimeout(timer); }
  const seconds = ms(started) / 1000;
  // Keep readers alive briefly to observe trailing repeats before disconnect.
  await pause(150);
  const result = {
    qos, protocol: config.protocol, publishers: config.publishers, subscribers: config.subscribers,
    messagesPerPublisher: config.messages, payloadBytes: config.payloadBytes, window: config.window,
    subscriberPauseMs: config.subscriberPauseMs || 0,
    publishRate: config.publishRate || 0,
    published: total, expectedDeliveries: expected, received, duplicates, corrupt, reordered,
    missing: expected - received, seconds: Number(seconds.toFixed(4)),
    deliveriesPerSubscriber: observations.map(observation => observation.count),
    publishCompletionPerSecond: Number((total / publishSeconds).toFixed(1)),
    deliveredPerSecond: Number((received / seconds).toFixed(1)),
    connectionLatencyMs: stats(connections), deliveryLatencyMs: stats(latency, received),
    publishCompletionLatencyMs: stats(acknowledgements, ackCount),
  };
  for (const client of [...publishers, ...subscribers]) { clients.delete(client); await client.endAsync(); }
  if (failed || corrupt || result.missing || reordered) throw new Error('Benchmark validation failed');
  return result;
}

try {
  for (const qos of config.qos) {
    let deadline;
    try {
      profiles.push(await Promise.race([profile(qos), new Promise((_, reject) => { deadline = setTimeout(() => reject(new Error('MQTT profile deadline exceeded')), config.timeout * 1000); })]));
      current = null;
    } finally { clearTimeout(deadline); }
  }
  process.stdout.write(JSON.stringify({ ok: true, profiles }) + '\n');
} catch {
  process.stdout.write(JSON.stringify({ ok: false, profiles, failedProfile: current?.() ?? null }) + '\n');
  process.stderr.write('MQTT benchmark failed: check delivery, connection, quota, or deadline in the isolated fixture.\n');
  process.exitCode = 1;
} finally {
  await Promise.allSettled([...clients].map(client => { clients.delete(client); return client.endAsync(true); }));
}
