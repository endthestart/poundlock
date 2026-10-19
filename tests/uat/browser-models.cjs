// Mocked API regression for the Models tab: Ready (disabled reason/source/last
// check, served model, pricing source, re-enable), Discover (gateway routes,
// publish requiring an explicit destination, the OpenRouter catalog and its
// not-proof note, search/filter), the editor drawer's dirty indicator and
// discard confirmation, and that a background refresh never clobbers an open
// draft. Fixture API only; no real gateway or model server involved.
const {chromium}=require('playwright');
const fs=require('node:fs/promises'),path=require('node:path'),assert=require('node:assert/strict');
(async()=>{
 const root=path.resolve(__dirname,'../../web');
 const node={id:'linux',name:'Linux workstation',platform:'linux',status:'online',capabilities:{runtimes:['always-ready']}};
 const endpointServer={id:'server',name:'Home llama.cpp',kind:'openai',base_url:'http://fixture.invalid/v1',data_destination:'operator_network'};
 const endpointGateway={id:'gw',name:'LiteLLM gateway',kind:'litellm',base_url:'http://gw.invalid/v1',public_url:'https://litellm.example.invalid',managed_identity:{enabled:false}};
 const p1={id:'p1',name:'Local coding',endpoint_id:'server',model_id:'local-model',node_id:'linux',context_tokens:8192,max_output_tokens:2048,uses:['durable'],workload_class:'durable-coding-agent',wake_policy:'never',data_destination:'operator_network',thinking:'model_default',aliases:[],capabilities:[],catalog:{},retirement:{},created_at:'2026-09-01T00:00:00Z',last_checked_at:'2026-09-22T09:00:00Z',last_check_ok:true};
 const p2={id:'p2',name:'Disabled model',endpoint_id:'gw',model_id:'published-route',node_id:null,context_tokens:32768,max_output_tokens:4096,uses:['interactive'],workload_class:'interactive-coding',wake_policy:'never',data_destination:'external',thinking:'model_default',aliases:[],capabilities:[],
  catalog:{served_model:'openrouter/vendor/model-x',provider:'openrouter',pricing:{input_per_mtok:2,output_per_mtok:10,source:'litellm_route',refreshed_at:'2026-09-22T09:00:00Z'},reasoning_levels:[]},
  retirement:{disabled:{reason:'model not found upstream',source:'relay',since:'2026-09-20T10:00:00Z',evidence:[{at:'2026-09-20T10:00:00Z',status:404,source:'relay',detail:'not found'}]}},
  created_at:'2026-09-01T00:00:00Z',last_checked_at:'2026-09-20T10:00:00Z',last_check_ok:false,last_check_message:'model not found upstream'};
 const p3={id:'p3',name:'configured-route',endpoint_id:'gw',model_id:'configured-route',node_id:null,context_tokens:32768,max_output_tokens:8192,uses:['interactive'],workload_class:'interactive-coding',wake_policy:'never',data_destination:'operator_network',thinking:'model_default',aliases:[],capabilities:[],catalog:{provider:'openrouter',served_model:'openrouter/vendor/model-y',pricing:{input_per_mtok:1,output_per_mtok:3,source:'litellm_route',refreshed_at:'2026-09-23T00:00:00Z'},reasoning_levels:[]},retirement:{},created_at:'2026-09-23T00:00:00Z'};
 // A typical GPU node: llamacpp reports its models, Ollama reports none.
 const orion={id:'orion',name:'orion',platform:'linux',status:'online',capabilities:{runtimes:['ollama','llamacpp'],inventory:{runtimes:{llamacpp:{servable:['qwen3.8-27b-mtp'],models:[{id:'qwen3.8-27b-mtp',label:'Qwen3.8-27B IQ3_S',context_tokens:131072}]}}}}};
 const data={version:'0.1.0',nodes:[node,orion],endpoints:[endpointServer,endpointGateway],models:[p1,p2],projects:[],harnesses:[],jobs:[],api_tokens:[],occupancy:[],git_hosts:[]};
 const discovery={gateways:[{endpoint_id:'gw',endpoint_name:'LiteLLM gateway',routes:[
   {name:'configured-route',upstream_model:'openrouter/vendor/model-y',provider:'openrouter',context_tokens:32768,supports_reasoning:true,stored_in:'config_file',pricing:{input_per_mtok:1,output_per_mtok:3,source:'litellm_route'},state:'configured_route',profile:null},
   {name:'published-route',upstream_model:'openrouter/vendor/model-x',provider:'openrouter',context_tokens:32768,supports_reasoning:true,stored_in:'config_file',pricing:null,state:'published',profile:{id:'p2',name:'Disabled model',status:'disabled'}},
  ]}],
  catalog:{provider:'openrouter',mode:'on-request',requested:true,fetched_at:'2026-09-23T00:00:00Z',note:'Listed by OpenRouter. A listing is not proof this account may use the model, nor that a request will succeed.',models:[
   {id:'vendor/model-y',name:'Model Y',created:0,context_tokens:32768,pricing:{input_per_mtok:1,output_per_mtok:3,source:'openrouter_catalog'},reasoning_efforts:['low','high'],default_effort:'medium',expires:null,state:'configured_route'},
   {id:'other/model-z',name:'Model Z',created:0,context_tokens:16384,pricing:{input_per_mtok:0.5,output_per_mtok:1.5,source:'openrouter_catalog'},reasoning_efforts:[],default_effort:null,expires:null,state:'catalog_listing'},
  ]}};
 let overviewPolls=0,discoveryHits=0,syncHits=0;const drafts=[];
 const browser=await chromium.launch({headless:true});try{
  const page=await browser.newPage({viewport:{width:1440,height:1100}}),errors=[];page.on('pageerror',e=>errors.push(e.message));
  await page.route('https://models-check.invalid/**',async route=>{
   const request=route.request(),url=new URL(request.url()),p=url.pathname;
   if(p==='/api/v1/overview'){overviewPolls++;return route.fulfill({json:data})}
   if(p==='/api/v1/model-profiles/p2/enable'&&request.method()==='POST'){data.models[1]={...data.models[1],retirement:{}};return route.fulfill({json:data.models[1]})}
   if(p==='/api/v1/discovery'&&request.method()==='GET'){discoveryHits++;const asked=url.searchParams.get('catalog')==='openrouter';return route.fulfill({json:asked?discovery:{...discovery,catalog:{provider:'openrouter',mode:'on-request',requested:false,models:[]}}})}
   if(p==='/api/v1/model-profiles/p2/sync-route'&&request.method()==='POST'){
    syncHits++;data.models[1].catalog={...data.models[1].catalog,served_model:'openrouter/vendor/model-next',reasoning_levels:[],reasoning_verified_at:null};
    discovery.gateways[0].routes[1].profile.model_changed=false;
    discovery.gateways[0].routes[1].profile.served_model='openrouter/vendor/model-next';
    return route.fulfill({json:data.models[1]});
   }
   if(p==='/api/v1/discovery/publish'&&request.method()==='POST'){
    const body=request.postDataJSON();
    assert.equal(body.endpoint_id,'gw');assert.equal(body.route,'configured-route');assert.equal(body.data_destination,'operator_network');
    assert.equal(body.name,'configured-route');assert.deepEqual(body.uses,['durable','interactive']);
    data.models.push(p3);
    return route.fulfill({json:p3});
   }
   if(p==='/api/v1/endpoints/server/test')return route.fulfill({json:{model_ids:['local-model','other-model']}});
   // The draft check's answer for these fixtures; the real rules have their own tests.
   if(p==='/api/v1/model-profiles/draft'){
    const b=request.postDataJSON(),gateway=b.endpoint_id==='gw',context=b.context_tokens??null;
    drafts.push(b);
    const name=b.name||(gateway?b.model_id:`home-llama-cpp-${b.model_id}`);
    return route.fulfill({json:{server_model_id:b.model_id,name:{value:name,source:b.name?'given':gateway?'route':'server_and_model'},model_label:{value:null,source:'unknown'},
     context_tokens:{value:context,source:context?'given':'unknown'},max_output_tokens:{value:context?Math.min(32768,Math.floor(context/4)):null,source:context?'quarter_of_context':'unknown'},
     uses:{value:['durable','interactive'],source:'default'},wake_policy:{value:'never',source:'no_machine'},thinking:{value:'model_default',source:'default'},
     data_destination:gateway?{value:null,source:'per_route'}:{value:'operator_network',source:'endpoint'},server_model_id_alias:{available:!gateway,used_by:null},existing:[],
     problems:context||gateway?[]:[{code:'context_unknown',message:"Enter the context from the server's settings; nothing reported it."}]}});
   }
   assert.ok(!p.startsWith('/api/'),'Unexpected API request: '+p);
   const name=p==='/'?'index.html':p.slice(1);
   await route.fulfill({body:await fs.readFile(path.join(root,name)),contentType:name.endsWith('.js')?'text/javascript':name.endsWith('.css')?'text/css':name.endsWith('.woff2')?'font/woff2':'text/html'});
  });
  await page.goto('https://models-check.invalid');await page.locator('#token').fill('fixture');await page.locator('#token-form button').click();await page.locator('#application').waitFor({state:'visible'});

  // Tabs: Setup is gone, replaced by Machines/Models/Connections.
  for(const name of ['Jobs','Machines','Models','Connections','Access'])assert.equal(await page.getByRole('button',{name,exact:true}).count(),1,name);
  assert.equal(await page.getByRole('button',{name:'Setup',exact:true}).count(),0);

  await page.getByRole('button',{name:'Models',exact:true}).click();await page.locator('#models.active').waitFor();
  const readyText=await page.locator('#ready-groups').textContent();
  // Both strings, each with a copy button: what clients send and the server's own ID.
  assert.match(readyText,/Clients use\s*Local coding/);assert.match(readyText,/Server model ID\s*local-model/);
  await page.context().grantPermissions(['clipboard-read','clipboard-write'],{origin:'https://models-check.invalid'});
  await page.locator('[data-profile="p1"] [data-copy="local-model"]').click();
  assert.equal(await page.evaluate(()=>navigator.clipboard.readText()),'local-model');
  // A machine's model shows "When a request arrives"; the stored value maps to its choice.
  await page.locator('[data-edit-model="p1"]').click();await page.locator('#model-drawer').waitFor({state:'visible'});
  assert.equal(await page.locator('#model-wake-group').isVisible(),true);
  assert.equal(await page.locator('[name="model-wake"]:checked').inputValue(),'never');
  assert.match(await page.locator('#model-wake-group').textContent(),/I keep it running/);
  assert.equal(await page.locator('[name="model-use"]:checked').inputValue(),'durable');
  assert.match(await page.locator('#model-identity').textContent(),/Server model ID\s*local-model/);
  await page.locator('[name="model-wake"][value="manual"]').check();
  assert.match(await page.locator('#wake-hint').textContent(),/fails at once/);
  page.once('dialog',dialog=>dialog.accept());await page.locator('#model-cancel').click();await page.locator('#model-drawer').waitFor({state:'hidden'});
  assert.match(readyText,/Disabled/);assert.match(readyText,/model not found upstream/);assert.match(readyText,/relay/);
  assert.match(readyText,/openrouter\/vendor\/model-x/);assert.match(readyText,/LiteLLM route/);
  assert.equal(await page.locator('[data-enable-model="p2"]').count(),1);

  await page.locator('[data-enable-model="p2"]').click();
  await page.waitForFunction(()=>document.querySelector('[data-enable-model="p2"]')===null);
  assert.doesNotMatch(await page.locator('#ready-groups').textContent(),/Source: relay/);

  // Discover loads on demand, not on every overview poll.
  assert.equal(discoveryHits,0);
  await page.locator('[data-model-view="discover"]').click();
  await page.waitForFunction(()=>document.querySelectorAll('#discover-gateways .item').length===2);
  assert.equal(discoveryHits,1);
  const refreshed1=page.waitForResponse(r=>r.url().includes('/api/v1/overview'));
  await page.locator('#refresh').click();await refreshed1;await page.waitForTimeout(50);
  assert.equal(discoveryHits,1,'refresh must not refetch discovery');

  const gatewayText=await page.locator('#discover-gateways').textContent();
  assert.match(gatewayText,/Configured route — not published/);assert.match(gatewayText,/Published/);
  assert.equal(await page.locator('[data-route="configured-route"] [data-route-publish]').count(),1);
  assert.equal(await page.locator('[data-route="published-route"] [data-route-publish]').count(),0);

  // The catalog is a request to openrouter.ai: nothing is sent until the operator asks.
  assert.match(await page.locator('#discover-catalog').textContent(),/Not looked up/);
  await page.locator('#discover-catalog-load').click();
  await page.waitForFunction(()=>document.querySelectorAll('#discover-catalog .item').length===2);
  assert.equal(discoveryHits,2);
  assert.match(await page.locator('#discover-catalog-note').textContent(),/not proof/);
  let catalogText=await page.locator('#discover-catalog').textContent();
  assert.match(catalogText,/Model Y/);assert.match(catalogText,/Model Z/);
  assert.equal(await page.locator('#discover-catalog button',{hasText:'Add route in LiteLLM'}).count(),1);
  await page.locator('[data-route-handoff="other/model-z"]').click();
  const guide=page.locator('#route-handoff');await guide.waitFor({state:'visible'});
  assert.match(await guide.textContent(),/openrouter\/other\/model-z/);
  const catalogLink=page.locator('#handoff-open');
  assert.equal(await catalogLink.getAttribute('href'),'https://litellm.example.invalid/ui/models-and-endpoints/');
  assert.equal(await catalogLink.getAttribute('target'),'_blank');
  assert.match(await catalogLink.getAttribute('rel'),/noopener/);
  await page.locator('#handoff-copy-alias').click();
  assert.equal(await page.evaluate(()=>navigator.clipboard.readText()),'other/model-z');
  await page.locator('#handoff-check').click();
  await page.waitForFunction(()=>document.querySelector('#handoff-result').textContent.includes('No route named'));
  await page.locator('#handoff-existing').selectOption('published-route');
  assert.equal(await page.locator('#handoff-open').getAttribute('href'),'https://litellm.example.invalid/ui/models-and-endpoints/?model_group=published-route');
  await page.locator('#handoff-check').click();
  await page.waitForFunction(()=>document.querySelector('#handoff-result').textContent.includes('Nothing was published or refreshed'));
  assert.equal(syncHits,0);
  await page.locator('#handoff-dismiss').click();await guide.waitFor({state:'hidden'});

  // A gateway edit does not silently mutate the saved profile or its limits.
  discovery.gateways[0].routes[1].upstream_model='openrouter/vendor/model-next';
  discovery.gateways[0].routes[1].profile.model_changed=true;
  discovery.gateways[0].routes[1].profile.served_model='openrouter/vendor/model-x';
  await page.locator('#discover-refresh').click();
  await page.waitForFunction(()=>document.querySelector('#discover-gateways').textContent.includes('Published profile needs refresh'));
  page.once('dialog',dialog=>dialog.dismiss());
  await page.locator('[data-route-sync="p2"]').click();assert.equal(syncHits,0);
  page.once('dialog',dialog=>dialog.accept());
  await page.locator('[data-route-sync="p2"]').click();
  await page.waitForFunction(()=>document.querySelector('#toast').textContent.includes('Old reasoning verification cleared'));
  assert.equal(syncHits,1);assert.equal(data.models[1].context_tokens,32768);
  assert.equal(data.models[1].max_output_tokens,4096);assert.equal(data.models[1].name,'Disabled model');

  await page.locator('#discover-search').fill('model-z');
  catalogText=await page.locator('#discover-catalog').textContent();
  assert.match(catalogText,/Model Z/);assert.doesNotMatch(catalogText,/Model Y/);
  await page.locator('#discover-search').fill('');
  await page.locator('#discover-unrouted').check();
  catalogText=await page.locator('#discover-catalog').textContent();
  assert.match(catalogText,/Model Z/);assert.doesNotMatch(catalogText,/Model Y/);
  await page.locator('#discover-unrouted').uncheck();

  // Publish is "Add a model" on the gateway: straight to the review, keeping
  // the route name, and each route declares where its prompts go.
  await page.locator('[data-route="configured-route"] [data-route-publish]').click();
  await page.locator('#model-drawer').waitFor({state:'visible'});
  assert.equal(await page.locator('#model-steps [aria-current="step"]').textContent(),'Review');
  await page.waitForFunction(()=>document.querySelector('#model-name').value==='configured-route');
  assert.equal(drafts.at(-1).context_tokens,32768,'the route\'s reported context goes to the draft check');
  assert.match(await page.locator('#model-runs-on').textContent(),/A route on the gateway LiteLLM gateway/);
  assert.match(await page.locator('#model-destination-legend').textContent(),/Where does this route send prompts/);
  await page.locator('#model-save').click();
  await page.waitForFunction(()=>document.querySelector('#toast').classList.contains('error'));
  assert.match(await page.locator('#toast').textContent(),/Choose where prompts go/);
  await page.locator('[name="model-destination"][value="operator_network"]').check();
  await page.locator('#model-save').click();
  await page.locator('#model-drawer').waitFor({state:'hidden'});
  await page.waitForFunction(()=>document.querySelector('#toast').textContent.includes('Added configured-route'));
  await page.locator('[data-model-view="ready"]').click();await page.locator('#models-ready.active').waitFor();
  await page.locator('[data-edit-model="p3"]').click();await page.locator('#model-drawer').waitFor({state:'visible'});
  assert.match(await page.locator('#model-drawer-title').textContent(),/configured-route/);

  // Drawer dirty indicator and confirm-on-discard, both dismiss and accept.
  assert.equal(await page.locator('#model-drawer-dirty').isVisible(),false);
  assert.equal(await page.locator('#model-name').evaluate(e=>e.readOnly),true,'a published name is fixed');
  await page.locator('#model-output').fill('4096');
  assert.equal(await page.locator('#model-drawer-dirty').isVisible(),true);
  page.once('dialog',dialog=>dialog.dismiss());
  await page.locator('#model-cancel').click();
  assert.equal(await page.locator('#model-drawer').isHidden(),false);
  assert.equal(await page.locator('#model-output').inputValue(),'4096');
  page.once('dialog',dialog=>dialog.accept());
  await page.locator('#model-cancel').click();
  await page.locator('#model-drawer').waitFor({state:'hidden'});

  // Add a model: three steps in a fixed order, then a review a refresh must not clobber.
  await page.locator('#add-model').click();await page.locator('#model-drawer').waitFor({state:'visible'});
  assert.equal(await page.locator('#model-steps [aria-current="step"]').textContent(),'Where it runs');
  // A hand-entered ID on a machine is managed like its listed models, and the
  // choice appears only once something is typed.
  await page.locator('[name="model-source"][value="node:orion"]').check();await page.locator('#source-endpoint').selectOption('server');await page.locator('#model-next').click();
  await page.locator('#model-choices .choice',{hasText:'qwen3.8-27b-mtp'}).waitFor();
  assert.equal(await page.locator('#model-manual-runtime-row').isVisible(),false);
  await page.locator('#model-manual-id').fill('hand-typed');
  assert.equal(await page.locator('#model-manual-runtime-row').isVisible(),true);
  assert.equal(await page.locator('#model-manual-runtime').inputValue(),'llamacpp');
  await page.locator('#model-manual-id').fill('');assert.equal(await page.locator('#model-manual-runtime-row').isVisible(),false);
  await page.locator('#model-back').click();
  await page.locator('[name="model-source"][value="endpoint:server"]').check();await page.locator('#model-next').click();
  await page.locator('#model-choices .choice').nth(1).waitFor();
  assert.match(await page.locator('#model-choices .choice').first().textContent(),/already added as Local coding/);
  await page.locator('#model-choices .choice',{hasText:'other-model'}).locator('input').check();await page.locator('#model-next').click();
  await page.waitForFunction(()=>document.querySelector('#model-name').value==='home-llama-cpp-other-model');
  assert.match(await page.locator('#model-destination-inherited').textContent(),/within my network, as declared for Home llama.cpp/);
  assert.equal(await page.locator('#model-limits').isVisible(),true,'an unreported context is asked for');
  await page.locator('#model-context').fill('65536');await page.waitForFunction(()=>document.querySelector('#model-output').value==='16384');
  await page.locator('#model-name').fill('Draft profile');
  // The drawer is a modal; the header Refresh button is inert behind its
  // backdrop, so a background refresh is exercised directly here.
  await page.evaluate(()=>refresh());
  assert.equal(await page.locator('#model-name').inputValue(),'Draft profile');
  page.once('dialog',dialog=>dialog.accept());await page.locator('#model-drawer-close').click();await page.locator('#model-drawer').waitFor({state:'hidden'});

  await page.setViewportSize({width:390,height:844});
  // Name what overflows, so a font-metric difference on another OS is diagnosable.
  const overflow=await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth?[...document.querySelectorAll('body *')].filter(e=>e.getBoundingClientRect().right>innerWidth+1&&e.offsetParent!==null).slice(0,8).map(e=>`${e.tagName.toLowerCase()}${e.id?'#'+e.id:''}.${[...e.classList].join('.')} right=${Math.round(e.getBoundingClientRect().right)} "${(e.textContent||'').trim().slice(0,40)}"`):[]);
  assert.deepEqual(overflow,[],`page overflows at 390px: ${overflow.join(' | ')}`);

  assert.deepEqual(errors,[]);
  console.log('PASS: Ready shows disabled reason/source/last-check and re-enable, served model and pricing source, Discover routes/publish/catalog with not-proof note and search filtering, drawer dirty indicator and discard confirmation, refresh preserves an open draft, mobile layout. Fixture API only.');
 }finally{await browser.close()}
})().catch(e=>{console.error(e.stack||e);process.exitCode=1});
