// Isolated controller only.
const {chromium}=require('playwright');
const fs=require('node:fs/promises');
const assert=require('node:assert/strict');
(async()=>{
  const base=process.env.UAT_BASE;
  assert.match(base||'',/^http:\/\/127\.0\.0\.1:\d+$/,'Use an isolated loopback controller');
  const token=(await fs.readFile(`${process.env.UAT_DATA}/data/admin-token`,'utf8')).trim();
  const browser=await chromium.launch({headless:true});
  try{
    const page=await browser.newPage();
    const stored=()=>page.evaluate(()=>[localStorage.getItem('poundlock-token'),sessionStorage.getItem('poundlock-token')]);
    await page.goto(base);
    assert.equal(await page.locator('#token').getAttribute('autocomplete'),'off');
    // By default the token is kept for this tab only, and survives a reload there.
    await page.locator('#token').fill(token);await page.getByRole('button',{name:'Unlock',exact:true}).click();
    await page.locator('#application').waitFor({state:'visible'});
    assert.deepEqual(await stored(),[null,token]);
    await page.reload();await page.locator('#application').waitFor({state:'visible'});

    // A saved token that stops working is dropped, with why.
    await page.evaluate(()=>{sessionStorage.clear();localStorage.setItem('poundlock-token','revoked-token')});
    await page.reload();
    await page.waitForFunction(()=>document.querySelector('#toast').textContent.includes('no longer works'));
    assert.equal(await page.locator('#unlock').isVisible(),true);
    assert.deepEqual(await stored(),[null,null]);
    // The error stays until it is dismissed.
    await page.waitForTimeout(4000);
    assert.equal(await page.locator('#toast').evaluate(el=>el.classList.contains('show')),true);
    await page.locator('#toast').getByRole('button',{name:'Dismiss',exact:true}).click();
    assert.equal(await page.locator('#toast').evaluate(el=>el.classList.contains('show')),false);
    console.log('PASS: the token is kept for the tab unless remembered, a dead saved token is dropped with a message, and errors stay until dismissed');
  }finally{await browser.close()}
})().catch(error=>{console.error(error);process.exitCode=1});
