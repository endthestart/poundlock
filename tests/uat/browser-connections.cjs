// Isolated real controller; verifies Connections/Models configuration lifecycle
// without a model server.
const {chromium}=require('playwright');
const fs=require('node:fs/promises'),assert=require('node:assert/strict');
(async()=>{
 const base=process.env.UAT_BASE;assert.match(base||'',/^http:\/\/127\.0\.0\.1:\d+$/);
 const token=(await fs.readFile(`${process.env.UAT_DATA}/data/admin-token`,'utf8')).trim();
 const browser=await chromium.launch({headless:true});
 try{
  const page=await browser.newPage(),errors=[];page.on('pageerror',e=>errors.push(e.message));page.on('dialog',d=>d.accept());
  await page.goto(base);await page.locator('#token').fill(token);await page.getByRole('button',{name:'Unlock',exact:true}).click();await page.locator('#application').waitFor({state:'visible'});
  await page.getByRole('button',{name:'Connections',exact:true}).click();
  await page.locator('#endpoint-name').fill('Test server');await page.locator('#endpoint-url').fill('http://127.0.0.1:9/v1');await page.locator('#endpoint-key').fill('fixture-secret');
  // A model server declares where it sends prompts once, for all its models.
  await page.locator('#endpoint-save').click();await page.waitForFunction(()=>document.querySelector('#toast').textContent.includes('Choose where this server sends prompts'));
  await page.locator('[name="endpoint-destination"][value="operator_network"]').check();await page.locator('#endpoint-save').click();
  await page.locator('[data-edit-endpoint]').waitFor();assert.match(await page.locator('#configured').textContent(),/Prompts go within my network/);
  await page.locator('[data-edit-endpoint]').click();assert.equal(await page.locator('[name="endpoint-destination"]:checked').inputValue(),'operator_network');
  assert.equal(await page.locator('#endpoint-key').inputValue(),'');assert.equal(await page.locator('#endpoint-edit-note').isVisible(),true);
  await page.locator('#endpoint-name').fill('Renamed server');await page.locator('#endpoint-save').click();
  await page.locator('#configured').getByText('Renamed server',{exact:true}).waitFor();
  await page.locator('[data-edit-endpoint]').click();await page.locator('#endpoint-clear-key').check();await page.locator('#endpoint-save').click();
  await page.waitForFunction(()=>document.querySelector('#endpoint-cancel').hidden);
  await page.locator('[data-edit-endpoint]').click();assert.equal(await page.locator('#endpoint-clear-row').isVisible(),false);await page.locator('#endpoint-cancel').click();

  await page.getByRole('button',{name:'Models',exact:true}).click();await page.locator('#models.active').waitFor();
  await page.locator('#add-model').click();await page.locator('#model-drawer').waitFor({state:'visible'});
  // Step 1, where it runs: a server that is always running.
  assert.equal(await page.locator('#model-steps [aria-current="step"]').textContent(),'Where it runs');
  assert.equal(await page.locator('#model-next').isDisabled(),true);
  await page.locator('#source-servers .choice',{hasText:'Renamed server'}).locator('input').check();await page.locator('#model-next').click();
  // Step 2, which model: the server is down, so the operator types the ID.
  await page.waitForFunction(()=>document.querySelector('#model-choices-note').textContent.includes('did not list'));
  await page.locator('#model-manual-id').fill('fixture');await page.locator('#model-next').click();
  // Step 3, review: the name is suggested, the server's declaration inherited, and nothing reported the context.
  await page.waitForFunction(()=>document.querySelector('#model-name').value==='renamed-server-fixture');
  assert.equal(await page.locator('#model-destination-group').isVisible(),false);assert.match(await page.locator('#model-destination-inherited').textContent(),/within my network, as declared for Renamed server/);
  assert.equal(await page.locator('#model-wake-group').isVisible(),false,'a server without a machine has no readiness choice');
  assert.equal(await page.locator('[name="model-use"]:checked').inputValue(),'durable,interactive');
  assert.match(await page.locator('#model-problems').textContent(),/Enter the context/);
  await page.locator('#model-context').fill('8192');await page.waitForFunction(()=>document.querySelector('#model-output').value==='2048');
  await page.locator('#model-advanced summary').click();await page.locator('#model-label').fill('Example 4B Q4');
  await page.locator('#model-save').click();await page.locator('#model-drawer').waitFor({state:'hidden'});
  await page.locator('[data-edit-model]').waitFor();assert.equal(await page.locator('#ready-groups .item').count(),1);
  const ready=await page.locator('#ready-groups').textContent();assert.match(ready,/Clients use\s*renamed-server-fixture/);assert.match(ready,/Server model ID\s*fixture/);assert.match(ready,/Prompts go within my network/);
  await page.locator('[data-edit-model]').click();await page.locator('#model-drawer').waitFor({state:'visible'});
  assert.equal(await page.locator('#model-steps').isVisible(),false,'editing is one page');
  assert.equal(await page.locator('#model-label').inputValue(),'Example 4B Q4');assert.equal(await page.locator('#model-output').inputValue(),'2048');
  assert.equal(await page.locator('#model-name').evaluate(e=>e.readOnly),true,'a saved model name is fixed');
  assert.match(await page.locator('#model-identity').textContent(),/Clients use\s*renamed-server-fixture/);
  await page.locator('#model-output').fill('1024');
  await page.evaluate(()=>refresh());assert.equal(await page.locator('#model-output').inputValue(),'1024');
  await page.locator('#model-save').click();await page.locator('#model-drawer').waitFor({state:'hidden'});
  await page.locator('[data-edit-model]').click();await page.locator('#model-drawer').waitFor({state:'visible'});
  assert.equal(await page.locator('#model-output').inputValue(),'1024');
  assert.equal(await page.locator('#model-route-banner').isVisible(),false);
  await page.locator('#model-id').fill('changed-model');await page.locator('#model-id').blur();
  // Changing where it runs is said, not done silently.
  assert.equal(await page.locator('#model-route-banner').isVisible(),true);assert.match(await page.locator('#model-route-banner').textContent(),/Prompts now go where Renamed server declares/);
  assert.equal(await page.locator('#model-label').inputValue(),'');
  await page.locator('#model-cancel').click();await page.locator('#model-drawer').waitFor({state:'hidden'});

  await page.getByRole('button',{name:'Connections',exact:true}).click();
  await page.locator('[data-remove-endpoint]').click();await page.waitForFunction(()=>document.querySelector('#toast').classList.contains('error'));assert.match(await page.locator('#toast').textContent(),/model profiles first/);
  await page.getByRole('button',{name:'Models',exact:true}).click();
  await page.locator('[data-remove-model]').click();await page.locator('[data-remove-model]').waitFor({state:'detached'});
  await page.getByRole('button',{name:'Connections',exact:true}).click();
  await page.locator('[data-remove-endpoint]').click();await page.locator('[data-remove-endpoint]').waitFor({state:'detached'});
  await page.reload();await page.locator('#connections').waitFor({state:'visible'});assert.equal(await page.locator('[data-edit-model]').count(),0);assert.deepEqual(errors,[]);
  console.log('PASS: real-controller Connections/Models create/edit/key retention/key removal/profile removal/dependency error/reload workflow. No model inference.');
 }finally{await browser.close()}
})().catch(e=>{console.error(e);process.exitCode=1});
