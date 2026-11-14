const { chromium } = require('playwright');
const http = require('http'); const fs = require('fs');
(async () => {
  const srv = http.createServer((q, r) => { r.setHeader('content-type','text/html'); r.end(fs.readFileSync(__dirname + '/page.html')); }).listen(0);
  const port = srv.address().port;
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const cdp = await page.context().newCDPSession(page);
  await cdp.send('WebAuthn.enable');
  await cdp.send('WebAuthn.addVirtualAuthenticator', { options: { protocol: 'ctap2', transport: 'internal', hasResidentKey: true, hasUserVerification: true, isUserVerified: true, automaticPresenceSimulation: true } });
  await page.goto(`http://localhost:${port}/`);
  const reg = await page.evaluate(() => window.register());
  console.log('register', JSON.stringify(reg));
  const login = await page.evaluate(() => window.login());
  console.log('login', JSON.stringify(login));
  await browser.close(); srv.close();
})().catch(e => { console.error('FAIL', e); process.exit(1); });
