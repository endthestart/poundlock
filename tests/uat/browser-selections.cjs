// Mocked API regression: background refresh must not change form targets.
// The model-profile editor is a modal drawer (aria-modal), so it is only
// exercised while its own tab is active; cross-tab persistence is checked via
// the Jobs and Machines forms, which stay on the page throughout.
const {chromium}=require('playwright');
const fs=require('node:fs/promises'),path=require('node:path'),assert=require('node:assert/strict');
(async()=>{
 const root=path.resolve(__dirname,'../../web');
 const nodes=['a','b'].map(id=>({id,name:'Machine '+id,platform:'linux',status:'online',wol_mac:'02:00:00:00:00:01',capabilities:{runtimes:[id+'-first',id+'-second']}}));
 const data={version:'0.1.0',nodes,endpoints:['a','b'].map(id=>({id,name:id,kind:'openai',base_url:'http://fixture.invalid'})),models:['a','b'].map(id=>({id,name:id,model_id:id,workload_class:'durable-coding-agent'})),projects:[],harnesses:[{id:'pi',name:'Pi',version:'fixture'}],jobs:[],api_tokens:[],occupancy:[],git_hosts:[]};
 let polls=0,wakeTarget;
 const browser=await chromium.launch({headless:true});try{
  const page=await browser.newPage(),errors=[];page.on('pageerror',e=>errors.push(e.message));page.on('dialog',d=>d.accept());
  await page.route('https://selection-check.invalid/**',async route=>{
   const url=new URL(route.request().url());
   if(url.pathname==='/api/v1/overview'){polls++;return route.fulfill({json:data})}
   if(url.pathname.endsWith('/wol'))return route.fulfill({json:{configured:true}});
   if(url.pathname.endsWith('/wake-test')){wakeTarget=url.pathname;return route.fulfill({json:{awake:true,was_online:true}})}
   assert.ok(!url.pathname.startsWith('/api/'),'Unexpected API request');
   const name=url.pathname==='/'?'index.html':url.pathname.slice(1);
   await route.fulfill({body:await fs.readFile(path.join(root,name)),contentType:name.endsWith('.js')?'text/javascript':name.endsWith('.css')?'text/css':name.endsWith('.woff2')?'font/woff2':'text/html'});
  });
  await page.goto('https://selection-check.invalid');await page.locator('#token').fill('fixture');await page.locator('#token-form button').click();await page.locator('#application').waitFor({state:'visible'});
  await page.locator('#job-model').selectOption('b');
  await page.locator('[data-tab="nodes"]').click();await page.locator('#wol-node').selectOption('b');await page.locator('#wake-settings summary').click();await page.locator('#wol-mac').fill('02:00:00:00:00:02');
  const start=polls;data.nodes.reverse();
  const deadline=Date.now()+12000;while(polls<start+2&&Date.now()<deadline)await page.waitForTimeout(100);
  assert.ok(polls>=start+2,'Two automatic refreshes required');
  for(const [id,value] of [['job-model','b'],['wol-node','b'],['wol-mac','02:00:00:00:00:02']])assert.equal(await page.locator('#'+id).inputValue(),value,id);
  await page.locator('#test-wake').click();await page.waitForFunction(()=>!document.querySelector('#test-wake').disabled);assert.equal(wakeTarget,'/api/v1/nodes/b/wake-test');
  data.nodes=data.nodes.filter(n=>n.id!=='b');data.models=data.models.filter(n=>n.id!=='b');
  const refreshed=page.waitForResponse(r=>r.url().endsWith('/overview'));await page.locator('#refresh').click();await refreshed;await page.waitForFunction(()=>!document.querySelector('#wol-node').validity.valid);
  for(const id of ['job-model','wol-node']){assert.equal(await page.locator('#'+id).inputValue(),'b');assert.equal(await page.locator('#'+id).evaluate(e=>e.validity.valid),false,id)}
  await page.locator('#wol-node').selectOption('a');assert.equal(await page.locator('#wol-node').evaluate(e=>e.validity.valid),true);

  // Model editor drawer: scoped to the Models tab, since it is a true modal
  // and blocks the rest of the page (including other tabs) while open.
  await page.locator('[data-tab="models"]').click();await page.locator('#models.active').waitFor();
  await page.locator('#add-model').click();await page.locator('#model-drawer').waitFor({state:'visible'});
  await page.locator('[name="model-source"][value="node:a"]').check();assert.equal(await page.locator('#source-endpoint-row').isVisible(),true);
  await page.evaluate(()=>refresh());assert.equal(await page.locator('[name="model-source"]:checked').inputValue(),'node:a','a background refresh keeps the choice');
  await page.locator('#model-drawer-close').click();await page.locator('#model-drawer').waitFor({state:'hidden'});
  // Editing a saved profile must retain a runtime that the node no longer reports.
  data.models[0]={...data.models[0],endpoint_id:'a',model_id:'fixture',context_tokens:8192,max_output_tokens:1024,wake_policy:'manual',node_id:'a',runtime_profile:'removed-runtime'};
  await page.locator('#refresh').click();await page.locator('[data-edit-model="a"]').click();await page.locator('#model-drawer').waitFor({state:'visible'});
  assert.equal(await page.locator('#model-runtime').inputValue(),'removed-runtime');
  assert.equal(await page.locator('#model-runtime').evaluate(e=>e.validity.valid),false);
  data.nodes[0].capabilities.runtimes=[];
  await page.evaluate(()=>refresh()); // background refresh while the drawer is open; the header Refresh button is inert behind the modal backdrop
  await page.waitForFunction(()=>document.querySelector('#model-runtime').options[0].textContent.includes('No runtime'));
  assert.equal(await page.locator('#model-runtime').inputValue(),'removed-runtime');
  assert.equal(await page.locator('#model-runtime').evaluate(e=>e.validity.valid),false);
  assert.deepEqual(errors,[]);console.log('PASS: automatic refresh and reorder preserve form selections; the model drawer retains an unavailable runtime across a background refresh; Wake targets the selected machine; unavailable choices block submission until replaced. Mocked API only.');
 }finally{await browser.close()}
})().catch(error=>{console.error(error);process.exitCode=1});
