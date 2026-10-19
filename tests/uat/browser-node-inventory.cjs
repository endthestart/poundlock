// Browser fixtures cover hardware-specific help, retained inventory and wake
// settings. Real enrollment and persistence have separate API/VM checks.
const {chromium}=require('playwright');
const fs=require('node:fs/promises'),path=require('node:path'),assert=require('node:assert/strict');
(async()=>{
 const root=path.resolve(__dirname,'../../web');
 const linux={id:'linux',name:'Linux workstation',platform:'linux',status:'online',credential:'never-export-me',capabilities:{arch:'x86_64',runtimes:['always-ready'],inventory:{schema_version:1,collected_at:'2026-09-08T12:00:00Z',os:{name:'CachyOS',id:'cachyos',family:'arch',arch:'x86_64'},cpu:{model:'Fixture CPU',logical_cores:16},memory:{total_bytes:34359738368},storage:{available_bytes:100000000000},gpus:[{vendor:'AMD',name:'AMD Radeon RX 9070 XT',vram_bytes:17179869184}],software:[{name:'Ollama',status:'not_detected'}]},network:{mode:'system',interfaces:[{name:'enp1s0',mac:'02:11:22:33:44:55',addresses:['192.0.2.9'],broadcasts:['192.0.2.255'],connected:true,kind:'physical'},{name:'wlan0',mac:'02:11:22:33:44:66',addresses:['198.51.100.9'],broadcasts:['198.51.100.255'],connected:false,kind:'wireless'}]}}};
 const arm={id:'arm',name:'Apple workstation',platform:'macos',status:'offline',capabilities:{arch:'aarch64',runtimes:['always-ready']}};
 const intel={id:'intel',name:'Intel workstation',platform:'macos',status:'offline',capabilities:{arch:'x86_64',runtimes:['always-ready']}};
 const data={version:'0.1.0',nodes:[linux,arm,intel],endpoints:[{id:"fixture",name:"Existing server",kind:"openai",base_url:"http://fixture.invalid/v1"}],models:[],projects:[],harnesses:[],jobs:[],api_tokens:[],occupancy:[],git_hosts:[]};
 const writes=[],errors=[];let polls=0;
 const browser=await chromium.launch({headless:true});try{
  const page=await browser.newPage({viewport:{width:1440,height:1000},acceptDownloads:true});page.on('pageerror',e=>errors.push(e.message));page.on('dialog',d=>d.accept());
  await page.route('https://inventory-check.invalid/**',async route=>{
   const url=new URL(route.request().url());
   if(url.pathname==='/api/v1/endpoints/fixture/test')return route.fulfill({json:{reachable:true,model_visible:true,model_ids:['example-model']}});
   if(url.pathname==='/api/v1/overview'){polls++;return route.fulfill({json:data})}
   if(url.pathname.endsWith('/wol')){writes.push({path:url.pathname,body:route.request().postDataJSON()});linux.wol_mac=writes.at(-1).body.mac;linux.wol_broadcast=writes.at(-1).body.broadcast;return route.fulfill({json:{configured:true}})}
   if(url.pathname.endsWith('/wake-test')){writes.push({path:url.pathname});return route.fulfill({json:{awake:true,was_online:false}})}
   assert.ok(!url.pathname.startsWith('/api/'),'Unexpected API request');
   const name=url.pathname==='/'?'index.html':url.pathname.slice(1);await route.fulfill({body:await fs.readFile(path.join(root,name)),contentType:name.endsWith('.js')?'text/javascript':name.endsWith('.css')?'text/css':name.endsWith('.woff2')?'font/woff2':'text/html'});
  });
  await page.goto('https://inventory-check.invalid/#machines');await page.locator('#token').fill('fixture');await page.locator('#token-form button').click();await page.locator('#nodes.active').waitFor();
  assert.equal(await page.locator('#wol-mac').inputValue(),'');assert.equal(await page.locator('#wol-mac').getAttribute('placeholder'),null);assert.equal(await page.locator('#test-wake').isDisabled(),true);
  await page.locator('#wake-interface').selectOption('enp1s0');assert.equal(await page.locator('#wol-mac').inputValue(),'02:11:22:33:44:55');assert.equal(await page.locator('#wol-broadcast').inputValue(),'192.0.2.255:9');assert.equal(writes.length,0);
  await page.locator('#save-wake').click();await page.waitForFunction(()=>!document.querySelector('#test-wake').disabled);assert.equal(writes.length,1);
  await page.locator('#wake-interface').selectOption('wlan0');assert.equal(await page.locator('#wol-mac').inputValue(),'02:11:22:33:44:66');
  await page.locator('#test-wake').click();await page.waitForFunction(()=>!document.querySelector('#test-wake').disabled);assert.equal(writes.length,2);assert.ok(writes[1].path.endsWith('/wake-test'));assert.equal(linux.wol_mac,'02:11:22:33:44:55','Wake must not save unsaved edits');
  const detail=page.locator('[data-node-details="linux-inventory"]');await detail.locator(':scope > summary').click();assert.match(await detail.textContent(),/16.0 GiB/);assert.match(await detail.textContent(),/02:11:22:33:44:55/);
  const setup=page.locator('[data-node-details="linux-setup"]');await setup.locator(':scope > summary').click();await setup.getByText('Drivers and OS installation links',{exact:true}).click();assert.ok(await setup.getByRole('link',{name:'Arch ollama-rocm package'}).isVisible());assert.equal(await setup.getByRole('link',{name:'oMLX',exact:true}).count(),0);
  const start=polls;linux.status='offline';await page.locator('#refresh').click();await page.waitForFunction(()=>document.querySelector('[data-node-details="linux-inventory"]').open);assert.ok(polls>start);assert.match(await detail.textContent(),/RX 9070 XT/);assert.equal(await page.locator('#wol-mac').inputValue(),'02:11:22:33:44:66','Refresh preserves unsaved interface choice');
  const downloadEvent=page.waitForEvent('download');await page.locator('[data-download-node="linux"]').click();const download=await downloadEvent;const report=JSON.parse(await fs.readFile(await download.path(),'utf8'));assert.equal(report.capabilities.inventory.gpus[0].vendor,'AMD');assert.equal(report.status,'offline');assert.ok(!JSON.stringify(report).includes('never-export-me'));
  for(const [id,expected,absent] of [['arm','oMLX','ToshLLM'],['intel','ToshLLM','oMLX']]){const help=page.locator(`[data-node-details="${id}-setup"]`);await help.locator(':scope > summary').click();assert.equal(await help.getByRole('link',{name:expected,exact:true}).count(),1);assert.equal(await help.getByRole('link',{name:absent,exact:true}).count(),0)}
  linux.capabilities.inventory.gpus[0].name='<img src=x onerror="window.inventoryXss=true">';await page.locator('#refresh').click();await page.waitForTimeout(100);assert.equal(await page.evaluate(()=>window.inventoryXss),undefined);assert.match(await detail.textContent(),/<img/);
  // Connecting a model server from a machine starts "Add a model" with that machine chosen.
  await page.locator('[data-connect-node="linux"]').click();await page.locator('#models.active').waitFor();await page.locator('#model-drawer').waitFor({state:'visible'});
  assert.equal(await page.locator('[name="model-source"]:checked').inputValue(),'node:linux');
  assert.equal(await page.locator('#source-endpoint').inputValue(),'','no server matches the machine, so the operator picks one');
  await page.locator('#source-endpoint').selectOption('fixture');await page.locator('#model-next').click();
  // A runtime that reports no models runs what the server lists.
  await page.locator('#model-choices .choice').first().waitFor();assert.match(await page.locator('#model-choices').textContent(),/example-model.*through always-ready/);
  assert.equal(await page.locator('#model-manual-runtime-row').isVisible(),false);await page.locator('#model-manual-id').fill('typed-by-hand');assert.equal(await page.locator('#model-manual-runtime').inputValue(),'always-ready','the only runtime');await page.locator('#model-manual-id').fill('');
  await page.locator('#model-back').click();assert.equal(await page.locator('#source-endpoint').inputValue(),'fixture','going back keeps the answer');
  await page.locator('#model-drawer-close').click();await page.locator('#model-drawer').waitFor({state:'hidden'});
  data.models.push({id:'old-profile',name:'Existing model',node_id:'linux',runtime_profile:'removed-runtime'});await page.locator('#refresh').click();await page.waitForFunction(()=>document.querySelector('#node-list').textContent.includes('configured runtime is not reported'));
  await page.locator('[data-tab="nodes"]').click();await page.locator('#nodes.active').waitFor();await page.setViewportSize({width:390,height:844});assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);
  const screenshots=process.env.POUNDLOCK_TEST_SCREENSHOTS;if(screenshots){await fs.mkdir(screenshots,{recursive:true});await page.screenshot({path:path.join(screenshots,'inventory-mobile.png'),fullPage:true});await page.setViewportSize({width:1440,height:1100});await page.screenshot({path:path.join(screenshots,'inventory-desktop.png'),fullPage:true})}
  assert.deepEqual(errors,[]);console.log('PASS: reported MAC/broadcast, explicit adapter choice, independent wake test, unsaved edits across refresh, offline inventory, safe report export, hardware-specific upstream links, escaped GPU metadata, setup handoff and mobile layout. Fixture API; no physical wake claimed.');
 }finally{await browser.close()}
})().catch(e=>{console.error(e.stack);process.exitCode=1});
