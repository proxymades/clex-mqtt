import assert from 'node:assert/strict';
import test from 'node:test';
import { createPublishPacer } from '../scripts/bench_pacing.mjs';
test('unlimited publication does not sleep', async () => {
  const calls = [];
  const pace = createPublishPacer(0, { now: () => 0, sleep: async ms => calls.push(ms) });
  await Promise.all([pace(), pace(), pace()]);
  assert.deepEqual(calls, []);
});
test('concurrent senders share an aggregate schedule', async () => {
  const waits = [];
  const pace = createPublishPacer(10, { now: () => 0, sleep: async ms => waits.push(ms) });
  await Promise.all([pace(), pace(), pace(), pace()]);
  assert.deepEqual(waits, [100, 200, 300]);
});
test('idle time starts a fresh schedule without catch-up', async () => {
  let current = 0;
  const waits = [];
  const pace = createPublishPacer(10, { now: () => current, sleep: async ms => waits.push(ms) });
  await pace();
  current = 1000;
  await pace();
  await pace();
  assert.deepEqual(waits, [100]);
});
test('invalid rates are rejected', () => {
  for (const rate of [-1, NaN, Infinity]) assert.throws(() => createPublishPacer(rate), RangeError);
});
