const { chromium } = require(process.env.MARSHAL_PLAYWRIGHT_MODULE || 'playwright');
const path = require('node:path');
const os = require('node:os');
const fs = require('node:fs');
const [url, artifactPath] = process.argv.slice(2);
if (!url || !artifactPath) throw Error('Usage: node check-ui.cjs <local-ui-url> <sandbox-run.json>');
const host = new URL(url).hostname;
if (!(host === 'localhost' || host.startsWith('127.') || host === '[::1]')) {
  throw Error('Run UI checks only against an isolated loopback test stack');
}
const screenshots = process.env.MARSHAL_UI_SCREENSHOT_DIR || path.join(os.tmpdir(), 'marshal-ui-check');
fs.mkdirSync(screenshots, { recursive: true });
const artifact = JSON.parse(fs.readFileSync(artifactPath, 'utf8'));
const lastMessage = artifact.timeline.filter(event => event.kind === 'message').at(-1);
const number = value => value.toLocaleString('en-US');

(async () => {
  const browser = await chromium.launch({
    headless: true, executablePath: process.env.MARSHAL_CHROMIUM_PATH, args: ['--no-sandbox'],
  });
  try {
    const page = await browser.newPage({ viewport: { width: 1500, height: 1100 } });
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    async function openZoo(target) {
      await target.goto(url + '/?reset');
      await target.getByRole('button', { name: 'Roster', exact: true }).first().waitFor();
      if (await target.getByRole('button', { name: 'Roster', exact: true }).count() !== 1) {
        throw Error('Default layout must have one pane');
      }
      await target.getByRole('button', { name: 'Zoo', exact: true }).click();
      await target.getByLabel('Import Zoo run').waitFor({ state: 'attached' });
    }
    async function chooseRun(target, id) {
      await target.locator(`.sb-run[data-run-id="${id}"]`).click();
    }
    async function checkMessages(target) {
      const value = await target.locator('.sb-stats > div').filter({ hasText: 'Messages' }).locator('strong').innerText();
      if (value !== number(artifact.messages)) throw Error('Incorrect message count');
    }
    async function importFile(file) {
      await page.getByLabel('Import Zoo run').setInputFiles(file);
      await page.getByRole('status').filter({ hasText: 'Imported' }).waitFor({ state: 'attached' });
    }

    await openZoo(page);
    await importFile(artifactPath);
    await page.locator('.sb-outcome').filter({ hasText: 'Completed' }).waitFor();
    await checkMessages(page);
    await page.getByText(lastMessage.text, { exact: true }).waitFor();
    const runCount = await page.locator('.sb-run').count();
    await importFile(artifactPath);
    if (await page.locator('.sb-run').count() !== runCount) throw Error('Reimport duplicated run');

    await page.getByLabel('Search Zoo runs').fill('no-such-run');
    await page.getByText('No matching runs.', { exact: true }).waitFor();
    await page.getByLabel('Search Zoo runs').fill('');
    await page.getByLabel('Search timeline').fill(lastMessage.text);
    if (await page.locator('.sb-message').count() !== 1) throw Error('Timeline search did not filter');
    await page.getByLabel('Search timeline').fill('');
    await page.getByLabel('Filter by agent').selectOption(artifact.roles.keeper);
    const rows = await page.locator('.sb-resource-row strong').allTextContents();
    if (rows.some(role => role !== 'keeper')) throw Error('Agent filter included unrelated resource actions');
    await page.getByLabel('Filter by agent').selectOption('');
    await page.getByLabel('Include model turns and tools').check();
    await page.locator('.sb-trace-event').first().waitFor();
    await page.getByLabel('Include model turns and tools').uncheck();

    await page.getByRole('tab', { name: 'Conversation', exact: true }).focus();
    await page.getByRole('tab', { name: 'Conversation', exact: true }).press('ArrowRight');
    await page.getByLabel('Zoo baseline').waitFor();
    await page.getByRole('tab', { name: 'Compare', exact: true }).press('ArrowRight');
    await page.getByText('Marshal instructions', { exact: true }).waitFor();
    await page.locator('.sb-context pre').waitFor();
    await page.getByRole('tab', { name: 'Context', exact: true }).press('Home');
    await page.getByLabel('Search timeline').waitFor();

    await importFile(path.resolve(__dirname, '../../crates/daemon/tests/fixtures/sandbox-run.json'));
    await chooseRun(page, artifact.run_id);
    await page.getByRole('tab', { name: 'Compare', exact: true }).click();
    if (await page.getByLabel('Zoo baseline').locator(`option[value="${artifact.run_id}"]`).count()) {
      throw Error('A run cannot be its own baseline');
    }
    await page.getByLabel('Zoo baseline').selectOption('sandbox-fixture-1');
    await page.getByText('Model differs', { exact: true }).waitFor();
    const cells = await page.locator('tbody tr').filter({ has: page.locator('th').filter({ hasText: /^Messages$/ }) }).locator('td').allTextContents();
    const difference = artifact.messages - 1;
    const expectedDelta = difference === 0 ? '—' : `${difference > 0 ? '+' : '-'}${number(Math.abs(difference))}`;
    if (cells[0] !== number(artifact.messages) || cells[1] !== '1' || cells[2] !== expectedDelta) {
      throw Error('Comparison counts or delta are incorrect');
    }
    if (await page.getByRole('tab', { name: 'Compare', exact: true }).getAttribute('aria-selected') !== 'true') {
      throw Error('Selecting a baseline reset the active tab');
    }
    await page.screenshot({ path: path.join(screenshots, 'comparison.png') });
    await page.getByRole('tab', { name: 'Conversation', exact: true }).click();
    await page.mouse.move(1450, 20);
    await page.screenshot({ path: path.join(screenshots, 'overview.png') });
    await page.getByText(lastMessage.text, { exact: true }).scrollIntoViewIfNeeded();
    await page.screenshot({ path: path.join(screenshots, 'timeline.png') });
    await page.setViewportSize({ width: 390, height: 844 });
    await page.locator('.pane-scroll').evaluate(element => { element.scrollTop = 0; });
    await page.screenshot({ path: path.join(screenshots, 'mobile.png') });
    if (await page.evaluate(() => document.documentElement.scrollWidth > innerWidth)) throw Error('Mobile page overflows');
    await page.setViewportSize({ width: 1500, height: 1100 });

    const invalid = { ...artifact, schema_version: 2 };
    await page.getByLabel('Import Zoo run').setInputFiles({
      name: 'invalid.json', mimeType: 'application/json', buffer: Buffer.from(JSON.stringify(invalid)),
    });
    await page.getByRole('status').filter({ hasText: 'Unsupported sandbox artifact version' }).waitFor();
    const other = await browser.newPage();
    await openZoo(other);
    await other.locator('.sb-run').nth(1).waitFor();
    await chooseRun(other, artifact.run_id);
    await checkMessages(other);
    await other.close();

    await page.goto(url + '/');
    await page.getByRole('button', { name: 'Split H', exact: true }).click();
    await page.waitForFunction(() => JSON.parse(localStorage.getItem('marshal-ui-layout')).tree.Split);
    await page.goto(url + '/');
    await page.getByRole('button', { name: 'Roster', exact: true }).nth(1).waitFor();
    await page.evaluate(() => {
      const saved = JSON.parse(localStorage.getItem('marshal-ui-layout'));
      saved.version = 2;
      localStorage.setItem('marshal-ui-layout', JSON.stringify(saved));
    });
    await page.reload();
    await page.getByRole('button', { name: 'Roster', exact: true }).waitFor();
    await page.getByRole('button', { name: 'reset layout', exact: true }).click();
    await page.getByRole('button', { name: 'Roster', exact: true }).waitFor();
    if (errors.length) throw Error(errors.join('\n'));
    console.log(JSON.stringify({ import: true, reimport: true, search: true, agentFilter: true,
      comparisonDelta: true, keyboardTabs: true, context: true, fullTrace: true, mobile: true,
      invalidRejected: true, sharedHistory: true, layoutMigration: true, pageErrors: errors, screenshots }));
  } finally {
    await browser.close();
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
