// Isolated controller only. Every cost here is a fixture's: nothing leaves the machine.
const {chromium}=require('playwright');
const http=require('node:http');
const fs=require('node:fs/promises');
const assert=require('node:assert/strict');
(async()=>{
  const base=process.env.UAT_BASE;
  assert.match(base||'',/^http:\/\/127\.0\.0\.1:\d+$/,'Use an isolated loopback controller');
  const token=(await fs.readFile(`${process.env.UAT_DATA}/data/admin-token`,'utf8')).trim();

  // A provider that says each answer cost $0.60, streamed or not.
  const usage={prompt_tokens:100000,completion_tokens:20000,cost:0.6};
  let upstreamCalls=0;
  const upstream=http.createServer((request,response)=>{
    let raw='';
    request.on('data',chunk=>raw+=chunk);
    request.on('end',()=>{
      if(request.url.endsWith('/models')){response.setHeader('content-type','application/json');response.end(JSON.stringify({data:[{id:'fixture'}]}));return}
      upstreamCalls++;
      const body=JSON.parse(raw);
      if(body.stream){
        response.setHeader('content-type','text/event-stream');
        response.write(`data: ${JSON.stringify({choices:[{delta:{content:'ok'}}]})}\n\n`);
        if(body.stream_options?.include_usage)response.write(`data: ${JSON.stringify({choices:[],usage})}\n\n`);
        response.end('data: [DONE]\n\n');
        return;
      }
      response.setHeader('content-type','application/json');
      response.end(JSON.stringify({choices:[{message:{role:'assistant',content:'ok'},finish_reason:'stop'}],usage}));
    });
  });
  await new Promise(resolve=>upstream.listen(0,'127.0.0.1',resolve));
  const upstreamBase=`http://127.0.0.1:${upstream.address().port}/v1`;

  const browser=await chromium.launch({headless:true});
  try{
    const page=await browser.newPage({viewport:{width:1440,height:900}}),errors=[];page.on('pageerror',e=>errors.push(e.message));
    const request=async(method,path,data)=>{const response=await page.request.fetch(base+path,{method,data,headers:{authorization:`Bearer ${token}`}});assert.ok(response.ok(),`${method} ${path}: ${response.status()} ${await response.text()}`);return response.json()};
    const gateway=await request('POST','/api/v1/endpoints',{name:'Frontier gateway',kind:'openai',base_url:upstreamBase,api_key:'fixture'});
    await request('POST','/api/v1/model-profiles',{name:'frontier-test',endpoint_id:gateway.id,model_id:'fixture',context_tokens:8192,max_output_tokens:1024,uses:['interactive'],wake_policy:'never',data_destination:'external'});
    const local=await request('POST','/api/v1/endpoints',{name:'Local server',kind:'openai',base_url:upstreamBase,api_key:'fixture'});
    await request('POST','/api/v1/model-profiles',{name:'local-test',endpoint_id:local.id,model_id:'fixture',context_tokens:8192,max_output_tokens:1024,uses:['interactive'],wake_policy:'never',data_destination:'operator_network'});
    const app=(await request('POST','/api/v1/api-tokens',{name:'Budget fixture app',permissions:['models:read','inference:invoke','external:invoke']})).token;
    const asApp=(method,path,data)=>page.request.fetch(base+path,{method,data,headers:{authorization:`Bearer ${app}`}});
    const chat=(model,stream=false)=>asApp('POST','/v1/chat/completions',{model,stream,max_tokens:16,messages:[{role:'user',content:'hello'}]});
    const row=async model=>(await (await asApp('GET','/v1/models')).json()).data.find(item=>item.id===model);
    const refused=async(response,code)=>{assert.equal(response.status(),403,await response.text());assert.equal((await response.json()).error.code,code)};
    const spent=async()=>(await request('GET','/api/v1/frontier-budget')).spend.total_usd;

    await page.goto(`${base}/#access`);
    await page.locator('#token').fill(token);
    await page.getByRole('button',{name:'Unlock',exact:true}).click();
    await page.locator('#frontier-budget-form').waitFor({state:'visible'});
    await page.waitForFunction(()=>document.querySelector('#frontier-budget-standing').textContent.includes('No ceiling set'));

    // Without a ceiling, external spend is recorded and nothing is refused.
    assert.equal((await chat('frontier-test')).status(),200);
    assert.equal(await spent(),0.6);
    await page.locator('#refresh').click();
    await page.locator('#frontier-budget-spend .item',{hasText:'Budget fixture app'}).filter({hasText:'$0.60'}).waitFor();

    // A value being typed survives a refresh until it is saved.
    await page.locator('#frontier-budget-weekly').fill('1');
    await page.locator('#refresh').click();
    await page.waitForLoadState('networkidle');
    assert.equal(await page.locator('#frontier-budget-weekly').inputValue(),'1');
    await page.locator('#frontier-budget-warn').fill('50');
    await page.locator('#frontier-budget-form').getByRole('button',{name:'Save budget'}).click();
    await page.locator('#frontier-budget-standing .item',{hasText:'This week'}).waitFor();

    // A ceiling can't meter a model with no price, so it is refused until it has one.
    await refused(await chat('frontier-test'),'frontier_budget_unpriced');
    assert.equal((await row('frontier-test')).availability,'blocked');
    await page.goto(`${base}/#models`);
    const model=page.locator('[data-profile]',{hasText:'frontier-test'});
    await model.getByText('No price: it cannot run while a frontier budget is set').waitFor();
    await model.locator('.more-actions summary').click();
    await model.getByRole('button',{name:'Set price',exact:true}).click();
    await model.locator('[name="input"]').fill('1');
    await model.locator('[name="output"]').fill('5');
    await model.getByRole('button',{name:'Save price',exact:true}).click();
    await page.waitForFunction(()=>!document.querySelector('[data-profile] .check-fail')?.textContent.includes('No price'));

    // A stream asks for its usage and is charged the provider's figure; $1.20 of $1 is used up.
    const stream=await chat('frontier-test',true);
    assert.equal(stream.status(),200);
    assert.match(await stream.text(),/\[DONE\]/);
    assert.ok(stream.headers()['x-poundlock-frontier-budget-remaining'],'the response says what is left');
    assert.equal(await spent(),1.2);
    const before=upstreamCalls;
    await refused(await chat('frontier-test'),'frontier_budget_exhausted');
    assert.equal(upstreamCalls,before,'a refused request never leaves');
    assert.equal((await row('frontier-test')).blocked_reason,'frontier_budget_exhausted');
    const told=await (await asApp('GET','/api/v1/usage')).json();
    assert.equal(told.frontier_budget.state,'exhausted');
    assert.doesNotMatch(JSON.stringify(told.frontier_budget),/Budget fixture app/,'an application sees totals, never who spent');

    // Local models are never affected, and never charged.
    assert.equal((await chat('local-test')).status(),200);
    assert.equal(await spent(),1.2);

    await page.goto(`${base}/#access`);
    await page.locator('#frontier-budget-standing .item',{hasText:'Used up'}).waitFor();

    // Past the warning share, clients are told before the ceiling.
    await request('PUT','/api/v1/frontier-budget',{weekly_usd:2,warn_at:0.5});
    const warned=await row('frontier-test');
    assert.equal(warned.availability,'available');
    assert.equal(warned.warning_reason,'frontier_budget_low');
    assert.equal(warned.poundlock.frontier_budget.state,'low');

    // Raising the ceiling lets work continue at once.
    await request('PUT','/api/v1/frontier-budget',{weekly_usd:10,warn_at:0.5});
    assert.equal((await chat('frontier-test')).status(),200);
    assert.deepEqual(errors,[]);
    console.log('PASS: external spend is metered with or without a ceiling; a ceiling refuses unpriced and used-up models and warns first; local models are untouched; raising it resumes work');
  }finally{
    await browser.close();
    upstream.close();
  }
})().catch(error=>{console.error(error);process.exit(1)});
