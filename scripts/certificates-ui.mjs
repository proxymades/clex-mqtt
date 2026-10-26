// Isolated API fixtures exercise certificate management with actual UI assets.
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { randomUUID } from 'node:crypto';
import { chromium, expect } from '@playwright/test';

const baseURL = process.env.CLEX_UI_BASE_URL || 'http://127.0.0.1:8085';
assert(['127.0.0.1', 'localhost', '[::1]'].includes(new URL(baseURL).hostname));
const browser = await chromium.launch({ headless: true });
try {
  const page = await browser.newPage({ viewport: { width: 1440, height: 1000 }, reducedMotion: 'reduce', locale: 'ru-RU' });
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  page.on('console', message => { if (message.type() === 'error') errors.push(message.text()); });
  const publicFixture = '-----BEGIN CERTIFICATE-----\nUI fixture: public certificate input\n-----END CERTIFICATE-----';
  let certificates = [], required = false;
  await page.route('**/api/v1/**', async route => {
    const request = route.request(), url = new URL(request.url()), method = request.method();
    if (method !== 'GET') assert.equal(request.headers()['x-csrf-token'], 'fixture-only');
    let json, status = 200;
    if (url.pathname.endsWith('/session')) json = { username: 'fixture-admin', csrfToken: 'fixture-only' };
    else if (url.pathname.endsWith('/overview')) json = { version: 'fixture', uptimeSeconds: 60, activeConnections: 0, storedSessions: 0, subscriptions: 0, retainedMessages: 0, queuedMessages: 0, incomingMessages: 0, outgoingMessages: 0, rejectedConnections: 0, database: { healthy: true }, listeners: [], capabilities: [] };
    else if (url.pathname.endsWith('/users')) json = [{ username: 'sensor-01', enabled: true, scramReady: true, requireClientCertificate: required, certificateCount: certificates.filter(cert => cert.enabled).length, createdAt: new Date().toISOString(), publishFilters: ['sensors/#'], subscribeFilters: ['sensors/#'], groups: [] }];
    else if (url.pathname.endsWith('/users/sensor-01') && method === 'PATCH') { required = request.postDataJSON().requireClientCertificate; json = { username: 'sensor-01' }; }
    else if (url.pathname.endsWith('/certificates') && method === 'GET') json = { certificates, requireClientCertificate: required, tls: { mode: 'optional', address: '127.0.0.1:8883' } };
    else if (url.pathname.endsWith('/certificates') && method === 'POST') {
      const body = request.postDataJSON();
      assert.equal(body.certificatePem, publicFixture);
      certificates.push({ id: randomUUID(), label: body.label, subject: 'CN=certificate-user, O=CLEX MQTT', fingerprint: Array(32).fill('A1').join(':'), expiresAt: '2099-01-01T00:00:00Z', enabled: true, createdAt: new Date().toISOString() });
      json = { id: certificates.at(-1).id }; status = 201;
    } else if (url.pathname.includes('/certificates/')) {
      const id = url.pathname.split('/').at(-1);
      if (method === 'PATCH') { Object.assign(certificates.find(cert => cert.id === id), request.postDataJSON()); json = { id }; }
      else { assert.equal(method, 'DELETE'); certificates = certificates.filter(cert => cert.id !== id); return route.fulfill({ status: 204 }); }
    } else throw new Error(`Unexpected fixture endpoint ${method} ${url.pathname}`);
    return route.fulfill({ status, json });
  });
  await page.goto(`${baseURL}/admin#users`);
  await page.getByRole('button', { name: 'Сертификаты sensor-01', exact: true }).click();
  const modal = page.getByRole('dialog', { name: 'Сертификаты клиента', exact: true });
  await expect(modal).toHaveCSS('opacity', '1');
  await expect(modal.getByRole('checkbox', { name: 'Требовать сертификат клиента' })).toBeDisabled();
  await modal.getByRole('button', { name: 'Добавить сертификат', exact: true }).click();
  await modal.locator('input[type="file"]').setInputFiles({ name: 'sensor-01.pem', mimeType: 'application/x-pem-file', buffer: Buffer.from(publicFixture) });
  await expect(modal.getByLabel('Открытый сертификат PEM', { exact: true })).toHaveValue(publicFixture);
  await modal.getByLabel('Название сертификата', { exact: true }).fill('Контроллер мастерской');
  await modal.getByRole('button', { name: 'Сохранить сертификат', exact: true }).click();
  await expect(modal.locator('.certificate-card')).toContainText('Контроллер мастерской');
  const policy = modal.getByRole('checkbox', { name: 'Требовать сертификат клиента' });
  await policy.click(); await expect.poll(() => required).toBe(true);
  await expect(policy).toBeChecked();
  await mkdir('work/ui', { recursive: true });
  await page.screenshot({ path: 'work/ui/certificates-desktop.png' });
  await modal.getByRole('button', { name: 'Отключить сертификат Контроллер мастерской', exact: true }).click();
  await expect(modal.locator('.certificate-card')).toContainText('Отключён');
  await modal.getByRole('button', { name: 'Включить сертификат Контроллер мастерской', exact: true }).click();
  await expect(modal.locator('.certificate-card')).toContainText('Активен');
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForFunction(() => document.querySelector('.sidebar').getBoundingClientRect().right <= 1);
  await page.waitForFunction(() => document.documentElement.scrollWidth <= innerWidth + 1);
  const box = await modal.boundingBox(); assert(box.y >= 0 && box.y + box.height <= 845);
  await page.screenshot({ path: 'work/ui/certificates-mobile.png' });
  await modal.getByRole('button', { name: 'Удалить сертификат Контроллер мастерской', exact: true }).click();
  await modal.getByRole('button', { name: 'Подтвердить удаление Контроллер мастерской', exact: true }).click();
  await expect(modal.getByRole('heading', { name: 'Добавьте сертификат клиента', exact: true })).toBeVisible();
  await policy.click(); await expect.poll(() => required).toBe(false); await expect(policy).not.toBeChecked();
  await modal.getByRole('button', { name: 'Добавить сертификат', exact: true }).click();
  await expect(modal.getByLabel('Название сертификата', { exact: true })).toHaveValue('');
  assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1));
  await modal.getByRole('button', { name: 'Отмена добавления', exact: true }).click();
  await modal.locator('.modal-footer').getByRole('button', { name: 'Закрыть', exact: true }).click();
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ ok: true, checks: ['PEM upload', 'certificate metadata', 'require certificate', 'disable/enable', 'confirmed removal', 'desktop/mobile layout', 'clear PEM after save'], screenshots: ['work/ui/certificates-desktop.png', 'work/ui/certificates-mobile.png'] }, null, 2));
} finally { await browser.close(); }
