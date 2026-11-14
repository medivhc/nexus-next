import { chromium } from 'playwright-core';
const [relay, id, port] = process.argv.slice(2);
const browser = await chromium.launch({ executablePath: process.env.CHROME || '/usr/bin/google-chrome', args: ['--no-sandbox'] });
const page = await browser.newPage();
page.on('console', m => console.log('console:', m.text()));
await page.goto(`http://localhost:${port}/index.html`);
await page.waitForFunction(() => window.ready);
for (let i = 0; i < 3; i++) console.log('run', JSON.stringify(await page.evaluate(([r, n]) => window.run(r, n), [relay, id])));
await browser.close();
