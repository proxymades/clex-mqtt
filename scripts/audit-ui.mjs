import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { readFile, readdir, mkdir, access } from 'node:fs/promises';
import path from 'node:path';
import { randomUUID } from 'node:crypto';
import { chromium, expect } from '@playwright/test';

const root = process.cwd();
const cache = path.join(root, '.cache', 'playwright');
const revision = (await readdir(cache)).filter((name) => name.startsWith('chromium_headless_shell-')).sort((a, b) => b.localeCompare(a, undefined, { numeric: true }))[0];
assert(revision, 'An isolated project Chromium headless shell is required.');
let executable;
for (const directory of await readdir(path.join(cache, revision), { withFileTypes: true })) {
  if (!directory.isDirectory()) continue;
  const candidate = path.join(cache, revision, directory.name, process.platform === 'win32' ? 'headless_shell.exe' : 'chrome-headless-shell');
  try { await access(candidate); executable = candidate; break; } catch {}
}
assert(executable, 'The project headless-shell executable is missing.');
const profile = path.join(root, '.cache', 'tmp', 'audit-browser-' + randomUUID());
await mkdir(profile, { recursive: true });
const dist = path.join(root, 'frontend', 'dist');
const server = createServer(async (req, res) => {
  const url = new URL(req.url, 'http://localhost');
  const file = path.resolve(dist, '.' + (['/', '/admin', '/account', '/account/register'].includes(url.pathname) ? '/index.html' : url.pathname));
  if (!file.startsWith(dist + path.sep)) { res.writeHead(403).end(); return; }
  try {
    const body = await readFile(file);
    res.setHeader('Content-Type', file.endsWith('.js') ? 'text/javascript' : file.endsWith('.css') ? 'text/css' : file.endsWith('.svg') ? 'image/svg+xml' : 'text/html');
    res.end(body);
  } catch { res.writeHead(404).end(); }
});
await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
let context;
try {
  const base = 'http://127.0.0.1:' + server.address().port;
  context = await chromium.launchPersistentContext(profile, { executablePath: executable, headless: true, viewport: { width: 1440, height: 1000 }, reducedMotion: 'reduce' });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  const fixtures = Array.from({ length: 80 }, (_, i) => ({
    id: 200 - i, actor: i === 1 ? '<img src=x onerror=alert(1)>' : 'fixture-' + i,
    action: ['admin.login', 'admin.login_failed', 'admin.login_limited'][i % 3],
    result: ['success', 'failed', 'limited'][i % 3],
    reason: [null, 'invalid_credentials', 'rate_limit'][i % 3],
    ip: i % 2 ? '2001:db8:1234:5678:abcd:1234:5678:abcd' : '192.0.2.10',
    target: null, createdAt: '2026-01-01T12:00:00Z',
  }));
  await page.route('**/api/v1/**', async (route) => {
    const url = new URL(route.request().url());
    let json;
    if (url.pathname.endsWith('/session')) json = { username: 'fixture-admin', csrfToken: 'fixture-csrf' };
    else if (url.pathname.endsWith('/overview')) json = { version: 'test', uptimeSeconds: 0, activeConnections: 0 };
    else if (url.pathname.endsWith('/audit')) {
      let rows = fixtures;
      if (url.searchParams.has('before')) rows = rows.filter((row) => row.id < Number(url.searchParams.get('before')));
      if (url.searchParams.has('result')) rows = rows.filter((row) => row.result === url.searchParams.get('result'));
      if (url.searchParams.has('ip')) rows = rows.filter((row) => row.ip === url.searchParams.get('ip'));
      if (url.searchParams.has('search')) rows = rows.filter((row) => row.actor.includes(url.searchParams.get('search')));
      json = rows.slice(0, Number(url.searchParams.get('limit') || 100));
    } else throw new Error('Unexpected fixture endpoint ' + url.pathname);
    await route.fulfill({ json });
  });
  await page.goto(base + '/admin#audit');
  await expect(page.locator('.audit-table tbody tr')).toHaveCount(50);
  await expect(page.locator('.audit-table')).toContainText('Вход в панель');
  await expect(page.locator('.audit-table')).toContainText('Неудачный вход');
  await expect(page.locator('.audit-table')).toContainText('Вход ограничен');
  assert.equal(await page.locator('.audit-table img').count(), 0, 'Untrusted actor strings must render as text.');
  await page.getByRole('button', { name: 'Далее', exact: true }).click();
  await expect(page.locator('.audit-table tbody tr')).toHaveCount(30);
  await expect(page.getByText('Страница 2 · 30 записей', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: 'Назад', exact: true }).click();
  await expect(page.locator('.audit-table tbody tr')).toHaveCount(50);
  await page.getByLabel('Результат входа', { exact: true }).selectOption('failed');
  await page.getByLabel('IP-адрес', { exact: true }).fill('192.0.2.10');
  await page.getByRole('button', { name: 'Применить', exact: true }).click();
  await expect(page.locator('.audit-table tbody tr')).toHaveCount(13);
  await page.getByRole('button', { name: 'Сбросить', exact: true }).click();
  await expect(page.locator('.audit-table tbody tr')).toHaveCount(50);
  await mkdir(path.join(root, 'work', 'ui'), { recursive: true });
  await page.screenshot({ path: 'work/ui/audit-desktop.png', fullPage: false });
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(page.getByLabel('Поиск по журналу', { exact: true })).toBeVisible();
  await page.waitForFunction(() => document.documentElement.scrollWidth <= innerWidth + 1, undefined, { timeout: 2000 }).catch(() => {});
  const layout = await page.evaluate(() => ({
    viewport: innerWidth, width: document.documentElement.scrollWidth,
    offenders: [...document.querySelectorAll('.card,.table-scroll,.table-footer,.audit-filters,.main-shell')].map((node) => {
      const box = node.getBoundingClientRect(); return { name: node.className, left: box.left, right: box.right, width: box.width };
    }),
  }));
  await page.screenshot({ path: 'work/ui/audit-mobile.png', fullPage: false });
  assert(layout.width <= layout.viewport + 1, 'Audit screen must not overflow: ' + JSON.stringify(layout));
  await page.screenshot({ path: 'work/ui/audit-mobile.png', fullPage: false });
  assert.deepEqual(errors, []);
  console.log('Audit UI passed: status labels, IP, safe untrusted text, pagination, combined filters, reset, desktop/mobile layout.');
} finally {
  if (context) await context.close();
  await new Promise((resolve) => server.close(resolve));
}
