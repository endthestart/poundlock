// Isolated controller only.
const {chromium}=require('playwright');
const fs=require('node:fs/promises');
const assert=require('node:assert/strict');
(async()=>{
  const base=process.env.UAT_BASE;
  assert.match(base||'',/^http:\/\/127\.0\.0\.1:\d+$/,'Use an isolated loopback controller');
  const directory=process.env.UAT_DATA;
  const token=(await fs.readFile(`${directory}/data/admin-token`,'utf8')).trim();
  const browser=await chromium.launch({headless:true});
  try{
    const page=await browser.newPage({viewport:{width:1440,height:900}});
    const waiting=[];
    let holds=0;
    await page.route('**/api/v1/overview',async route=>{
      if(holds>0){holds--;await new Promise(resolve=>waiting.push(resolve));}
      return route.continue();
    });
    const arm=()=>{holds+=1};
    const release=()=>{const next=waiting.shift();if(next)next();};
    const signedOut=async()=>{
      await page.locator('#unlock').waitFor({state:'visible'});
      assert.equal(await page.locator('#application').isVisible(),false);
      assert.equal(await page.evaluate(()=>localStorage.getItem('poundlock-token')),null);
    };
    await page.goto(base);
    await page.locator('#token').fill(token);
    await page.getByRole('button',{name:'Unlock',exact:true}).click();
    await page.locator('#application').waitFor({state:'visible'});

    arm();
    const firstHold=page.waitForRequest(r=>r.url().endsWith('/api/v1/overview')&&r.method()==='GET');
    await page.locator('#refresh').click();
    await firstHold;
    await page.getByRole('button',{name:'Sign out',exact:true}).click();
    await signedOut();
    release();
    await page.waitForTimeout(400);
    await signedOut();
    assert.equal(await page.locator('#job-detail').isVisible(),false);

    await page.locator('#token').fill(token);
    await page.getByRole('button',{name:'Unlock',exact:true}).click();
    await page.locator('#application').waitFor({state:'visible'});
    arm();
    const secondHold=page.waitForRequest(r=>r.url().endsWith('/api/v1/overview')&&r.method()==='GET');
    await page.locator('#refresh').click();
    await secondHold;
    await page.getByRole('button',{name:'Sign out',exact:true}).click();
    await signedOut();
    await page.locator('#token').fill(token);
    await page.getByRole('button',{name:'Unlock',exact:true}).click();
    await page.locator('#application').waitFor({state:'visible'});
    const connected=await page.locator('#status-text').textContent();
    release();
    await page.waitForTimeout(400);
    assert.equal(await page.locator('#application').isVisible(),true,'stale refresh must not replace a new session');
    assert.equal(await page.locator('#status-text').textContent(),connected);
    assert.equal(await page.evaluate(()=>localStorage.getItem('poundlock-token')),token);

    await page.getByRole('button',{name:'Access',exact:true}).click();
    let releaseToken;
    const heldToken=new Promise(resolve=>{releaseToken=resolve});
    let holdToken=true;
    await page.route('**/api/v1/api-tokens',async route=>{
      if(route.request().method()==='POST'&&holdToken){holdToken=false;await heldToken}
      return route.continue();
    });
    await page.locator('#api-token-name').fill('stale token');
    const tokenStarted=page.waitForRequest(r=>r.url().endsWith('/api/v1/api-tokens')&&r.method()==='POST');
    await page.locator('#api-token-form button[type="submit"], #api-token-form button:not([type])').click();
    await tokenStarted;
    await page.getByRole('button',{name:'Sign out',exact:true}).click();
    await signedOut();
    releaseToken();
    await page.waitForTimeout(400);
    await signedOut();
    assert.equal(await page.locator('#api-token-issued').isVisible(),false);

    await page.locator('#token').fill(token);
    await page.getByRole('button',{name:'Unlock',exact:true}).click();
    await page.locator('#application').waitFor({state:'visible'});
    await page.getByRole('button',{name:'Machines',exact:true}).click();
    let releaseEnroll;
    const heldEnroll=new Promise(resolve=>{releaseEnroll=resolve});
    let holdEnroll=true;
    await page.route('**/api/v1/nodes/enrollment-tokens',async route=>{
      if(route.request().method()==='POST'&&holdEnroll){holdEnroll=false;await heldEnroll}
      return route.continue();
    });
    await page.locator('#enroll-platform').selectOption('macos');
    await page.locator('#enroll-name').fill('workstation');
    const enrollStarted=page.waitForRequest(r=>r.url().endsWith('/api/v1/nodes/enrollment-tokens')&&r.method()==='POST');
    await page.locator('#enroll-form button[type="submit"], #enroll-form button:not([type])').click();
    await enrollStarted;
    await page.getByRole('button',{name:'Sign out',exact:true}).click();
    await signedOut();
    releaseEnroll();
    await page.waitForTimeout(400);
    await signedOut();
    assert.equal(await page.locator('#enroll-issued').isVisible(),false);

    await page.locator('#token').fill(token);
    await page.getByRole('button',{name:'Unlock',exact:true}).click();
    await page.locator('#application').waitFor({state:'visible'});
    await page.getByRole('button',{name:'Access',exact:true}).click();
    await page.locator('#api-token-name').fill('session fixture');
    await page.locator('#api-token-form button[type="submit"], #api-token-form button:not([type])').click();
    await page.locator('#api-token-issued').waitFor({state:'visible'});
    await page.getByRole('button',{name:'Sign out',exact:true}).click();
    await signedOut();
    assert.equal(await page.locator('#api-token-issued').isVisible(),false);
    console.log('PASS: delayed refresh after sign-out cannot restore private data; a later login stays in the new session; reveals and stored tokens clear');
  }finally{await browser.close()}
})().catch(error=>{console.error(error);process.exitCode=1});
