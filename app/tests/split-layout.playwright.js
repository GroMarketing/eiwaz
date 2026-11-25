// Regression check for pane sizes after split, move and close. Run with the Playwright MCP
// (browser_run_code_unsafe, filename) while `npx vite --port 1420` serves ui-test.html.
// Every pane alone in its column must be full height; a lone column must be full width.
async (page) => {
  await page.setViewportSize({ width: 1300, height: 800 });
  await page.goto('http://localhost:1420/ui-test.html');
  await page.waitForTimeout(800);
  const open = async () => {
    const direct = page.getByRole('button', { name: 'gemma4:latest (this Mac)' });
    if (await direct.count() && await direct.first().isVisible()) await direct.first().click();
    else { await page.click('#new-btn'); await page.waitForTimeout(200); await page.locator('#new-menu button', { hasText: 'gemma4' }).first().click(); }
    await page.waitForTimeout(500);
  };
  await open(); await open(); await open();
  const drag = async (from, x, y) => {
    const b = await from.boundingBox();
    await page.mouse.move(b.x + b.width / 2, b.y + b.height / 2); await page.mouse.down();
    await page.mouse.move(b.x + b.width / 2 + 20, b.y + b.height / 2 + 20, { steps: 5 });
    await page.mouse.move(x, y, { steps: 12 }); await page.waitForTimeout(150); await page.mouse.up(); await page.waitForTimeout(400);
  };
  const measure = () => page.evaluate(() => {
    const g = document.getElementById('grid').getBoundingClientRect();
    return { grid: [Math.round(g.width), Math.round(g.height)], panes: [...document.querySelectorAll('#grid .pane')].map((p) => { const r = p.getBoundingClientRect(); return `${p.querySelector('.pt')?.textContent} x${Math.round(r.x - g.x)} ${Math.round(r.width)}x${Math.round(r.height)} grow=${p.style.flexGrow} col=${p.parentElement.style.flexGrow}`; }) };
  });
  const tabs = page.locator('#tabs .tab:not(.add-tab)');
  const out = { tabs: await tabs.count(), start: await measure() };
  let g = await page.locator('#grid').boundingBox();
  // 1) second tab onto the bottom edge: stacked
  await drag(tabs.nth(1), g.x + g.width / 2, g.y + g.height - 20);
  out.stacked = await measure();
  // 2) bottom pane onto the right edge of the top pane: the top one is now alone in its column
  const panes = page.locator('#grid .pane');
  const top = await panes.nth(0).boundingBox();
  await drag(panes.nth(1).locator('.pane-head .grip'), top.x + top.width - 20, top.y + top.height / 2);
  out.movedOut = await measure();
  // 3) third tab onto the right edge of the right pane, then close the middle one with ×: columns
  g = await page.locator('#grid').boundingBox();
  await drag(tabs.nth(2), g.x + g.width - 20, g.y + g.height / 2);
  out.threeCols = await measure();
  await page.locator('#grid .pane').nth(1).locator('.pane-head .x').click(); await page.waitForTimeout(300);
  out.closedMiddle = await measure();
  return out;
}
