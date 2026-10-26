// Aggregate publication scheduling for the isolated benchmark driver.
import { performance } from 'node:perf_hooks';

export function createPublishPacer(rate, {
  now = () => performance.now(),
  sleep = ms => new Promise(resolve => setTimeout(resolve, ms)),
} = {}) {
  if (!Number.isFinite(rate) || rate < 0) throw new RangeError('Invalid publish rate');
  let next = 0;
  return async () => {
    if (!rate) return;
    const current = now();
    const scheduled = Math.max(current, next);
    next = scheduled + 1000 / rate;
    if (scheduled > current) await sleep(scheduled - current);
  };
}
