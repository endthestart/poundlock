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
    const page=await browser.newPage({viewport:{width:1440,height:900}});
    page.on('dialog',dialog=>dialog.accept());
    await page.goto(`${base}/#access`);
    await page.locator('#token').fill(token);
    await page.getByRole('button',{name:'Unlock',exact:true}).click();
    await page.locator('#api-token-name').waitFor({state:'visible'});

    // The checkboxes carry the controller's labels and its two defaults.
    const choices=await page.locator('[name="api-permission"]').evaluateAll(inputs=>inputs.map(input=>[input.value,input.checked,input.parentElement.textContent]));
    assert.deepEqual(choices.map(([code])=>code),['models:read','inference:invoke','inference:priority','external:invoke','jobs:read','jobs:submit','jobs:cancel']);
    assert.deepEqual(choices.filter(([,checked])=>checked).map(([code])=>code),['models:read','inference:invoke']);
    assert.equal(choices[3][2],'Send prompts and files to external services');

    // A new token's secret shows even when the refresh after it fails.
    await page.route('**/api/v1/overview',route=>route.fulfill({status:503,json:{error:{status:503,message:'held'}}}));
    await page.locator('#api-token-name').fill('editor laptop');
    await page.locator('#api-token-save').click();
    await page.locator('#api-token-issued').waitFor({state:'visible'});
    assert.match(await page.locator('#api-token-issued [data-secret]').inputValue(),/\S{20,}/);
    await page.unroute('**/api/v1/overview');
    await page.locator('#copied-api-token').click();
    await page.locator('#refresh').click();

    const row=page.locator('#api-token-list .item',{hasText:'editor laptop'});
    await row.waitFor();
    assert.match(await row.textContent(),/Send inference requests/);
    await row.getByRole('button',{name:'Edit',exact:true}).click();
    assert.equal(await page.locator('#api-token-heading').textContent(),'Edit editor laptop');
    await page.locator('#api-token-name').fill('editor desktop');
    await page.locator('[name="api-permission"][value="inference:invoke"]').uncheck();
    await page.locator('[name="api-permission"][value="jobs:read"]').check();
    await page.locator('#api-token-save').click();
    const edited=page.locator('#api-token-list .item',{hasText:'editor desktop'});
    await edited.waitFor();
    assert.match(await edited.textContent(),/Read its own jobs and results/);
    assert.doesNotMatch(await edited.textContent(),/Send inference requests/);
    assert.equal(await page.locator('#api-token-heading').textContent(),'Create an application token');
    assert.equal(await page.locator('#api-token-issued').isVisible(),false);

    const tokens=await (await fetch(`${base}/api/v1/api-tokens`,{headers:{authorization:`Bearer ${token}`}})).json();
    const saved=tokens.find(item=>item.name==='editor desktop');
    assert.deepEqual(saved.permissions,['jobs:read','models:read']);
    assert.equal(tokens.filter(item=>item.name.startsWith('editor')).length,1);
    // Revoked tokens leave the list, one click away.
    await edited.getByRole('button',{name:'Revoke',exact:true}).click();
    await page.locator('#api-token-revoked',{hasText:'Show 1 revoked token'}).waitFor();
    assert.equal(await page.locator('#api-token-list .item',{hasText:'editor desktop'}).count(),0);
    await page.locator('#api-token-revoked').click();
    assert.match(await page.locator('#api-token-list .item',{hasText:'editor desktop'}).textContent(),/Revoked/);
    console.log('PASS: Access labels come from the controller; a new secret survives a failed refresh; a token is edited in place; revoked tokens are hidden until asked for');
  }finally{await browser.close()}
})().catch(error=>{console.error(error);process.exitCode=1});
