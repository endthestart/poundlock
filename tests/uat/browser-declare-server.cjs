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
    const page=await browser.newPage({viewport:{width:1440,height:900}}),errors=[];page.on('pageerror',e=>errors.push(e.message));
    const request=async(method,path,data)=>{const response=await page.request.fetch(base+path,{method,data,headers:{authorization:`Bearer ${token}`}});assert.ok(response.ok(),`${method} ${path}: ${response.status()} ${await response.text()}`);return response.json()};
    // A server from before servers declared where prompts go, with a model on it.
    const server=await request('POST','/api/v1/endpoints',{name:'Legacy server',kind:'openai',base_url:'http://127.0.0.1:9/v1',api_key:''});
    await request('POST','/api/v1/model-profiles',{name:'legacy-a',endpoint_id:server.id,model_id:'a',context_tokens:8192,max_output_tokens:1024,uses:['durable'],wake_policy:'never'});
    const declared=async()=>(await request('GET','/api/v1/endpoints')).find(item=>item.id===server.id).data_destination;
    assert.equal(await declared(),'unknown');

    await page.goto(`${base}/#models`);
    // A field that fails validation says so to a screen reader, until it is corrected.
    await page.getByRole('button',{name:'Unlock',exact:true}).click();
    assert.equal(await page.locator('#token').getAttribute('aria-invalid'),'true');
    await page.locator('#token').fill(token);
    assert.equal(await page.locator('#token').getAttribute('aria-invalid'),null);
    await page.getByRole('button',{name:'Unlock',exact:true}).click();
    await page.locator('#add-model').click();await page.locator('#model-drawer').waitFor({state:'visible'});
    await page.locator('#source-servers .choice',{hasText:'Legacy server'}).locator('input').check();await page.locator('#model-next').click();
    await page.waitForFunction(()=>document.querySelector('#model-choices-note').textContent.includes('did not list'));
    await page.locator('#model-manual-id').fill('b');await page.locator('#model-next').click();
    await page.locator('#model-destination-group').waitFor({state:'visible'});
    await page.locator('[name="model-destination"][value="operator_network"]').check();
    await page.locator('#model-context').fill('8192');await page.waitForFunction(()=>document.querySelector('#model-output').value==='2048');
    // A model the controller refuses (here, a bad evidence paste) leaves the server as it was.
    await page.locator('#model-advanced summary').click();await page.locator('#model-capabilities').fill('[{"name":"not-a-capability"}]');
    await page.locator('#model-save').click();
    await page.waitForFunction(()=>document.querySelector('#toast').classList.contains('error'));
    assert.equal(await declared(),'unknown');
    // The error sits in the drawer's flow at any width: it covers no field, and a keyboard reaches Dismiss.
    await page.setViewportSize({width:375,height:700});
    assert.ok(await page.evaluate(()=>{const toast=document.querySelector('#toast');return toast.closest('#model-drawer')&&toast.getBoundingClientRect().top>=document.querySelector('.drawer-body').getBoundingClientRect().bottom-1}));
    assert.ok((await page.locator('.toast-dismiss').boundingBox()).height>=24);
    let tabs=0;for(;tabs<80&&!await page.evaluate(()=>document.activeElement.matches('.toast-dismiss'));tabs++)await page.keyboard.press('Tab');
    assert.ok(tabs<80,'Tab never reached Dismiss inside the drawer');
    await page.keyboard.press('Enter');
    assert.ok(await page.evaluate(()=>!document.querySelector('#toast').closest('#model-drawer')&&!document.querySelector('#toast').classList.contains('show')&&document.querySelector('#model-drawer').contains(document.activeElement)));
    await page.setViewportSize({width:1440,height:900});
    // With an error still showing, the corrected form can be saved.
    await page.locator('#model-save').click();
    await page.waitForFunction(()=>document.querySelector('#toast').classList.contains('error'));
    await page.locator('#model-capabilities').fill('');await page.locator('#model-save').click();
    await page.locator('#model-drawer').waitFor({state:'hidden'});
    assert.equal(await declared(),'operator_network');
    const models=(await request('GET','/api/v1/model-profiles')).filter(item=>item.endpoint_id===server.id);
    assert.deepEqual(models.map(item=>item.data_destination),['operator_network','operator_network']);
    assert.deepEqual(errors,[]);
    console.log('PASS: the Add-model drawer declares an undeclared server with the model in one change, and a refused model leaves the server unchanged');
  }finally{await browser.close()}
})().catch(error=>{console.error(error);process.exitCode=1});
