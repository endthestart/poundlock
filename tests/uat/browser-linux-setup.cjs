// Exercise the actual setup UI and API against a temporary local controller.
const {chromium}=require('playwright');
const fs=require('node:fs/promises'),path=require('node:path'),os=require('node:os'),assert=require('node:assert/strict'),crypto=require('node:crypto');
const {spawn,execFileSync}=require('node:child_process');
(async()=>{
 const binary=path.resolve(process.env.POUNDLOCK_TEST_CONTROLLER||'target/debug/poundlock');
 const root=await fs.mkdtemp(path.join(os.tmpdir(),'poundlock-linux-ui-'));
 const artifacts=path.join(root,'artifacts');await fs.mkdir(artifacts);
 // Artifact transport fixture: installation itself has a separate real VM test.
 await fs.mkdir(path.join(artifacts,'linux-aarch64'));await fs.writeFile(path.join(artifacts,'linux-aarch64','poundlock'),Buffer.from('linux-ui-download-fixture')); 
 const base='http://127.0.0.1:17433';let logs='';
 const child=spawn(binary,['controller'],{env:{PATH:process.env.PATH,POUNDLOCK_BIND:'127.0.0.1:17433',POUNDLOCK_DATA_DIR:root+'/data',POUNDLOCK_RUNTIME_DIR:root+'/runtime',POUNDLOCK_CLIENT_ARTIFACTS:artifacts},stdio:['ignore','pipe','pipe']});
 for(const stream of [child.stdout,child.stderr])stream.on('data',b=>logs=(logs+b).slice(-8000));
 let browser;
 try{
  let ready=false;const deadline=Date.now()+30000;
  while(Date.now()<deadline){
   assert.equal(child.exitCode,null,`Fixture controller exited before readiness: ${logs}`);
   try{if((await fetch(base+'/readyz',{signal:AbortSignal.timeout(500)})).ok){ready=true;break}}catch{}
   await new Promise(resolve=>setTimeout(resolve,100));
  }
  assert.ok(ready,`Fixture controller did not become ready: ${logs}`);
  const token=(await fs.readFile(root+'/data/admin-token','utf8')).trim();assert.ok(token);
  browser=await chromium.launch({headless:true});const page=await browser.newPage({acceptDownloads:true}),errors=[];page.on('pageerror',e=>errors.push(e.message));
  await page.goto(base);await page.locator('#token').fill(token);await page.locator('#token-form button').click();await page.locator('#application').waitFor({state:'visible'});await page.locator('[data-tab="nodes"]').click();
  await page.locator('#enroll-name').fill('Linux "office"');await page.locator('#enroll-arch').selectOption('aarch64');
  for(const id of ['download-toml','download-binary','download-installer']){
   const downloading=page.waitForEvent('download',{timeout:6000});await page.locator('#'+id).click();let download;try{download=await downloading}catch{throw new Error(id+': '+await page.locator('#toast').textContent()+'; '+errors.join('; '))}await download.saveAs(path.join(root,download.suggestedFilename()));
  }
  const downloadedHash=crypto.createHash('sha256').update(await fs.readFile(root+'/poundlock-linux-aarch64')).digest('hex');
  assert.equal(await page.locator('#node-upgrade-command').textContent(),`sudo bash ./poundlock-node.sh upgrade --binary ./poundlock-linux-aarch64 --sha256 ${downloadedHash}`);
  assert.ok(await page.locator('#node-upgrade').isVisible());assert.ok(await page.locator('#enroll-issued').isHidden());
  await page.locator('#enroll-arch').selectOption('x86_64');assert.ok(await page.locator('#node-upgrade').isHidden());assert.equal(await page.locator('#node-upgrade-command').textContent(),'');await page.locator('#enroll-arch').selectOption('aarch64');
  const config=await fs.readFile(root+'/node.toml','utf8');assert.ok(config.includes('name = "Linux \\"office\\""'));assert.ok(config.includes('auto_update = false'));assert.ok(config.includes('network = { mode = "system" }'));
  execFileSync(binary,['node-check','--require-identity','--config',root+'/node.toml']);
  execFileSync('bash',['-n',root+'/poundlock-node.sh']);
  await page.locator('#enroll-form > button').filter({hasText:'Create token'}).click();await page.locator('#enroll-issued').waitFor({state:'visible'});
  const enrollment=await page.locator('#enroll-secret').inputValue();const command=await page.locator('#enroll-command').textContent();
  assert.ok(command.includes('sudo bash ./poundlock-node.sh install'));assert.ok(command.includes('--sha256'));assert.ok(!command.includes('--user'));assert.ok(!command.includes(enrollment));assert.ok(!command.includes('--enrollment-token'));
  await page.waitForTimeout(4500);assert.equal(await page.locator('#enroll-command').textContent(),command);
  await page.locator('#copied-enroll').click();assert.equal(await page.locator('#enroll-secret').inputValue(),'');assert.equal(await page.locator('#enroll-command').textContent(),'');
  const enrolled=JSON.parse(execFileSync(binary,['node-enroll','--config',root+'/node.toml','--credential-file',root+'/credential.json'],{input:enrollment,encoding:'utf8',env:{...process.env,POUNDLOCK_NODE_DATA_DIR:root}}));
  enrolled.credential=JSON.parse(await fs.readFile(root+'/credential.json','utf8')).credential;
  assert.equal(enrolled.capabilities.inventory.schema_version,1);assert.ok(enrolled.capabilities.inventory.collected_at);assert.ok(enrolled.capabilities.inventory.storage.total_bytes>0);
  const actualNode=(await (await fetch(base+'/api/v1/nodes',{headers:{Authorization:'Bearer '+token}})).json()).find(node=>node.id===enrolled.node_id);
  assert.deepEqual(actualNode.capabilities.inventory,enrolled.capabilities.inventory);
  assert.ok(actualNode.capabilities.network.interfaces.length>0);

  await page.locator('#refresh').click();const remove=page.locator(`[data-remove-node="${enrolled.node_id}"]`);await remove.waitFor();
  page.once('dialog',dialog=>dialog.dismiss());await remove.click();assert.equal(await remove.count(),1);
  page.once('dialog',dialog=>{assert.match(dialog.message(),/Linux "office"/);assert.match(dialog.message(),/does not uninstall/);return dialog.accept()});await remove.click();await remove.waitFor({state:'detached'});
  await page.locator('#removed-machines summary').click();assert.match(await page.locator('#removed-node-list').textContent(),/Linux "office"/);
  const denied=await fetch(base+`/node/v1/nodes/${enrolled.node_id}/status`,{headers:{Authorization:'Bearer '+enrolled.credential}});assert.equal(denied.status,401);
  await page.goto(base+'/docs/linux-node');await page.locator('#linux-guide[open]').waitFor();assert.equal(new URL(page.url()).hash,'#linux-guide');
  assert.ok(await page.locator('#nodes').evaluate(e=>e.classList.contains('active')));assert.match(await page.locator('#linux-guide').textContent(),/Upgrade and roll back/);
  assert.match(await page.locator('#enroll-download-hint').textContent(),/any folder you own/);assert.match(await page.locator('#enroll-form').textContent(),/any available network interface/);
  assert.equal(await page.locator('#wol-node').count(),1);
  assert.equal(await page.locator('#wol-mac').getAttribute('placeholder'),null);
  await page.locator('#linux-guide').evaluate(e=>e.open=false);await page.evaluate(()=>scrollTo(0,0));await page.screenshot({path:root+'/machines-desktop.png',fullPage:true});
  await page.setViewportSize({width:390,height:844});assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);
  await page.screenshot({path:root+'/machines-mobile.png',fullPage:true});
  if(process.env.POUNDLOCK_TEST_SCREENSHOTS){await fs.mkdir(process.env.POUNDLOCK_TEST_SCREENSHOTS,{recursive:true});for(const name of ['machines-desktop.png','machines-mobile.png'])await fs.copyFile(root+'/'+name,process.env.POUNDLOCK_TEST_SCREENSHOTS+'/'+name)}
  assert.deepEqual(errors,[]);console.log('PASS: real native enrollment inventory retained by API, UI/API downloads, quoted machine-name configuration, required-identity preflight, installer syntax, token-free install command, refresh preservation, reveal cleanup, operator removal and cancelled confirmation, credential revocation, integrated guide redirect, unified wake controls and mobile layout. Binary contents are a transport fixture; installation is tested separately in a VM.');
 }finally{
  if(browser)await browser.close();child.kill('SIGTERM');await new Promise(resolve=>child.once('close',resolve));await fs.rm(root,{recursive:true,force:true});
 }
})().catch(e=>{console.error(e.message);process.exitCode=1});
