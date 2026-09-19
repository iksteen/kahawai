import { chromium } from 'playwright';
import fs from 'node:fs';

const creds = Object.fromEntries(
  fs.readFileSync(process.env.HOME + '/.config/kahawai/claude-test-account', 'utf8')
    .split('\n').filter(l => l.includes('=')).map(l => {
      const i = l.indexOf('='); return [l.slice(0, i).trim().toLowerCase(), l.slice(i + 1).trim()];
    }));

const BASE = 'http://127.0.0.1:8420';
const LIB = '01KY8K0CW57HMSWP3P46T4QT60';
const ITEM = '01KY5X52KHQ99WVF0H92EJ4EWV';

const browser = await chromium.launch({ executablePath: process.env.HOME + '/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome', args: ['--autoplay-policy=no-user-gesture-required'] });
const ctx = await browser.newContext({ ignoreHTTPSErrors: true });
const page = await ctx.newPage();

const hits = [];
page.on('request', r => {
  const u = r.url();
  if (/\/subtitles\/|\/playback\/sessions(\?|$)/.test(u)) { hits.push(u); console.log('REQ', u.replace(BASE, '')); }
});
page.on('console', m => { if (/error/i.test(m.type())) console.log('CONSOLE', m.text().slice(0, 160)); });

await page.goto(BASE + '/app/', { waitUntil: 'domcontentloaded' });
// log in
try {
  await page.fill('input[type="text"], input[name="username"]', creds.username, { timeout: 8000 });
  await page.fill('input[type="password"]', creds.password);
  await page.keyboard.press('Enter');
  await page.waitForTimeout(3000);
} catch { console.log('(no login form — already authenticated?)'); }

await page.goto(`${BASE}/app/library/${LIB}/item/${ITEM}/play`, { waitUntil: 'domcontentloaded' });
await page.waitForTimeout(20000);
console.log('TITLE:', (await page.title()));
fs.writeFileSync('/tmp/claude-1000/-home-ingmar-src-third-kahawai-android/ee18b06d-3b26-4361-921b-26e227e133ee/scratchpad/detail.html', await page.content());
await page.screenshot({ path: '/tmp/claude-1000/-home-ingmar-src-third-kahawai-android/ee18b06d-3b26-4361-921b-26e227e133ee/scratchpad/web_detail.png', fullPage: false });
console.log('SUBTITLE REQUESTS SO FAR:', hits.length);
await browser.close();
