// Theme menu regression: all four Catppuccin flavours, persistence, System
// following prefers-color-scheme, reduced motion, and mobile layout. Fixture
// API only; no real controller involved.
//
// The login form is bypassed with an init script that seeds the stored token,
// so refresh() runs automatically on load (see machines.js). That keeps this
// suite about the theme menu, not the login flow (covered elsewhere), and
// avoids interacting with the unlock form under emulated colour schemes.
const {chromium}=require('playwright');
const fs=require('node:fs/promises'),path=require('node:path'),assert=require('node:assert/strict');
(async()=>{
 const root=path.resolve(__dirname,'../../web');
 const data={version:'0.1.0',nodes:[],endpoints:[],models:[],projects:[],harnesses:[],jobs:[],api_tokens:[],occupancy:[],git_hosts:[]};
 const screenshots=process.env.UAT_ARTIFACTS||process.env.POUNDLOCK_TEST_SCREENSHOTS||'/tmp/poundlock-theme-screenshots';
 await fs.mkdir(screenshots,{recursive:true});
 const browser=await chromium.launch({headless:true});
 try{
  async function openPage(options={}){
   const page=await browser.newPage(options);
   await page.addInitScript(()=>{try{localStorage.setItem('poundlock-token','fixture')}catch{}});
   await page.route('https://theme-check.invalid/**',async r=>{
    const url=new URL(r.request().url());
    if(url.pathname==='/api/v1/overview')return r.fulfill({json:data});
    if(url.pathname==='/favicon.ico')return r.fulfill({status:404,body:''});
    assert.ok(!url.pathname.startsWith('/api/'),'Unexpected API request: '+url.pathname);
    const name=url.pathname==='/'?'index.html':url.pathname.slice(1);
    await r.fulfill({body:await fs.readFile(path.join(root,name)),contentType:name.endsWith('.js')?'text/javascript':name.endsWith('.css')?'text/css':name.endsWith('.woff2')?'font/woff2':'text/html'});
   });
   await page.goto('https://theme-check.invalid');
   await page.locator('#application').waitFor({state:'visible'});
   return page;
  }

  // Each flavour's --canvas value, from theme.css: the body background must
  // render it exactly, both right after selecting the flavour and after a
  // reload (the persisted choice must render identically).
  const CANVAS_RGB={latte:'rgb(220, 224, 232)',frappe:'rgb(35, 38, 52)',macchiato:'rgb(24, 25, 38)',mocha:'rgb(17, 17, 27)'};

  for(const flavour of ['latte','frappe','macchiato','mocha']){
   const page=await openPage({viewport:{width:1440,height:900}});
   const errors=[];page.on('pageerror',e=>errors.push(e.message));
   await page.locator('#theme-menu').selectOption(flavour);
   assert.equal(await page.evaluate(()=>document.documentElement.dataset.theme),flavour);
   assert.equal(await page.evaluate(()=>getComputedStyle(document.body).backgroundColor),CANVAS_RGB[flavour],`${flavour}: body background must match its canvas colour`);
   await page.reload();
   await page.locator('#application').waitFor({state:'visible'});
   assert.equal(await page.evaluate(()=>document.documentElement.dataset.theme),flavour,`${flavour}: choice must persist across reload`);
   assert.equal(await page.evaluate(()=>getComputedStyle(document.body).backgroundColor),CANVAS_RGB[flavour],`${flavour}: persisted theme must render the same colours`);
   await page.screenshot({path:path.join(screenshots,`theme-${flavour}-desktop.png`),fullPage:true});
   await page.setViewportSize({width:390,height:844});
   assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false,`${flavour}: no horizontal overflow at 390px`);
   await page.screenshot({path:path.join(screenshots,`theme-${flavour}-mobile.png`),fullPage:true});
   assert.deepEqual(errors,[],`${flavour}: no console errors`);
   await page.close();
  }

  // System follows prefers-color-scheme, and is the default with no stored key.
  {
   const page=await browser.newPage({viewport:{width:1440,height:900}});
   await page.addInitScript(()=>{try{localStorage.setItem('poundlock-token','fixture')}catch{}});
   await page.route('https://theme-check.invalid/**',async r=>{
    const url=new URL(r.request().url());
    if(url.pathname==='/api/v1/overview')return r.fulfill({json:data});
    if(url.pathname==='/favicon.ico')return r.fulfill({status:404,body:''});
    const name=url.pathname==='/'?'index.html':url.pathname.slice(1);
    await r.fulfill({body:await fs.readFile(path.join(root,name)),contentType:name.endsWith('.js')?'text/javascript':name.endsWith('.css')?'text/css':name.endsWith('.woff2')?'font/woff2':'text/html'});
   });
   await page.emulateMedia({colorScheme:'dark'});
   await page.goto('https://theme-check.invalid');
   await page.locator('#application').waitFor({state:'visible'});
   assert.equal(await page.evaluate(()=>document.documentElement.dataset.theme),undefined,'System leaves data-theme unset');
   const dark=await page.evaluate(()=>getComputedStyle(document.body).backgroundColor);
   assert.equal(dark,CANVAS_RGB.mocha,'System + prefers dark must render the Mocha canvas colour');
   await page.emulateMedia({colorScheme:'light'});
   await page.reload();
   await page.locator('#application').waitFor({state:'visible'});
   const light=await page.evaluate(()=>getComputedStyle(document.body).backgroundColor);
   assert.equal(light,CANVAS_RGB.latte,'System + prefers light must render the Latte canvas colour');
   assert.notEqual(dark,light,'System must follow prefers-color-scheme');
   // Choosing a flavour then System again must fall back to prefers-color-scheme.
   await page.locator('#theme-menu').selectOption('mocha');
   assert.equal(await page.evaluate(()=>document.documentElement.dataset.theme),'mocha');
   await page.locator('#theme-menu').selectOption('system');
   assert.equal(await page.evaluate(()=>document.documentElement.dataset.theme),undefined);
   assert.equal(await page.evaluate(()=>localStorage.getItem('poundlock-theme')),null);
   await page.close();
  }

  // Sign-out must not clear the stored theme choice.
  {
   const page=await openPage({viewport:{width:1440,height:900}});
   await page.locator('#theme-menu').selectOption('macchiato');
   await page.getByRole('button',{name:'Sign out',exact:true}).click();
   await page.locator('#unlock').waitFor({state:'visible'});
   assert.equal(await page.evaluate(()=>localStorage.getItem('poundlock-theme')),'macchiato','sign-out must not clear the theme key');
   assert.equal(await page.evaluate(()=>document.documentElement.dataset.theme),'macchiato');
   await page.close();
  }

  // Reduced motion disables the animated live-connection dot.
  {
   const page=await openPage({viewport:{width:1440,height:900},reducedMotion:'reduce'});
   const animationName=await page.locator('#status-dot').evaluate(el=>getComputedStyle(el).animationName);
   assert.equal(animationName,'none','reduced motion must disable the live-connection breathe animation');
   await page.close();
  }

  console.log(`PASS: theme menu switches all four Catppuccin flavours with a persisted, distinct canvas colour; System follows prefers-color-scheme and clears the stored key; sign-out keeps the theme choice; reduced motion disables the live-connection animation; no overflow at 390px. Fixture API; screenshots in ${screenshots}.`);
 }finally{await browser.close()}
})().catch(e=>{console.error(e.stack||e);process.exitCode=1});
