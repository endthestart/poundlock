// Real supervisor/worker containers against a deterministic inference fixture.
// Requires the built image, a Docker socket and its group, and a bridge network.
const fs = require('node:fs/promises');
const http = require('node:http');
const {spawn} = require('node:child_process');
const assert = require('node:assert/strict');
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));

(async () => {
  const mode = process.env.POUNDLOCK_TEST_WORKER_MODE || 'concurrency';
  assert.ok(['concurrency', 'cancel', 'restart', 'deadline', 'verify', 'check-failed', 'repair', 'repair-limit', 'protected', 'missing', 'check-timeout', 'verify-cancel', 'verify-restart', 'repair-restart', 'network', 'network-blocked', 'combined', 'command-help', 'python-uv', 'reasoning-deltas', 'git'].includes(mode));
  const concurrent=['concurrency','combined'].includes(mode);
  const interruptedAgent=['cancel','restart','deadline'].includes(mode);
  const interruptedCheck=['verify-cancel','verify-restart'].includes(mode);
  const repairMode=['repair','repair-limit','repair-restart'].includes(mode);
  const root = await fs.mkdtemp('/tmp/poundlock-workers-');
  let calls = 0, inFlight = 0, peak = 0, logs = '';
  const children = [];
  const model = http.createServer(async (req, res) => {
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    if (req.url === '/v1/models') {
      res.setHeader('content-type', 'application/json');
      return res.end(JSON.stringify({data: [{id: 'fixture'}]}));
    }
    if (req.url !== '/v1/chat/completions') {res.writeHead(404); return res.end();}
    calls++;
    const body = JSON.parse(Buffer.concat(chunks));
    const first = !body.messages.some(message => message.role === 'tool');
    if (interruptedAgent && !first) return; // End the second request through cancellation/deadline/restart.
    if (first && concurrent) {
      inFlight++; peak = Math.max(peak, inFlight);
      await delay(5000);
      inFlight--;
    }
    const repairing=JSON.stringify(body.messages).includes('Repair attempt 1 of');
    if(mode==='repair-restart'&&repairing)return;
    const delta = first ? {role:'assistant', tool_calls:[{index:0,id:'write',type:'function',function:{
      name:'write', arguments:JSON.stringify({path:mode==='protected'?'test_greeting.py':'greeting.txt',content:mode==='protected'?'# removed the checks\n':(repairMode&&(!repairing||mode==='repair-limit'))?'Wrong.\n':'Concurrent worker finished.\n'})
    }}]} : {role:'assistant',content:'Finished.'};
    const packet = (delta, finish_reason) => JSON.stringify({id:'fixture',object:'chat.completion.chunk',created:1,model:'fixture',choices:[{index:0,delta,finish_reason}]});
    res.writeHead(200, {'content-type':'text/event-stream'});
    // A reasoning model keeps streaming reasoning_content even with --thinking
    // off; Pi turns every chunk into its own message_update/thinking_delta
    // line. Enough of them must exceed the old, unfiltered per-stream budget
    // so this mode fails loudly if the harness stops dropping them.
    if (mode==='reasoning-deltas' && first) {
      res.write('data: '+packet({role:'assistant'},null)+'\n\n');
      for (let n=0;n<20000;n++) res.write('data: '+packet({reasoning_content:'reasoning token '+n+' '},null)+'\n\n');
    }
    res.write('data: '+packet(delta,null)+'\n\n');
    res.write('data: '+packet({},first?'tool_calls':'stop')+'\n\n');
    res.end('data: [DONE]\n\n');
  });
  await new Promise(resolve => model.listen(17333,'127.0.0.1',resolve));
  const child = (args, env={}) => {
    const process = spawn('poundlock',args,{env:{...global.process.env,...env},stdio:['ignore','pipe','pipe']});
    for (const stream of [process.stdout,process.stderr]) stream.on('data',chunk=>logs=(logs+chunk).slice(-12000));
    children.push(process); return process;
  };
  const docker = (method,path,body) => new Promise((resolve,reject) => {
    const request = http.request({socketPath:'/var/run/docker.sock',method,path,headers:{'content-type':'application/json'}},response=>{
      const chunks=[]; response.on('data',chunk=>chunks.push(chunk));
      response.on('end',()=>{
        if (response.statusCode>=400) return reject(new Error(`Docker ${method} ${path}: ${response.statusCode}`));
        const bytes=Buffer.concat(chunks); resolve(bytes.length?JSON.parse(bytes):null);
      });
    });
    request.setTimeout(15000,()=>request.destroy(new Error('Docker timeout')));
    request.on('error',reject); request.end(body?JSON.stringify(body):undefined);
  });
  const owned = async () => {
    // The runner writes its identity at startup; a poll can land first under load.
    let owner;
    for(let n=0;;n++){
      try{owner=(await fs.readFile(root+'/results/supervisor-id','utf8')).trim();break}
      catch(error){if(error.code!=='ENOENT'||n>=150)throw error;await new Promise(resolve=>setTimeout(resolve,100))}
    }
    const filter=encodeURIComponent(JSON.stringify({label:['net.poundlock.supervisor='+owner]}));
    const [containers,networks,volumes]=await Promise.all([
      docker('GET','/containers/json?all=true&filters='+filter),
      docker('GET','/networks?filters='+filter),
      docker('GET','/volumes?filters='+filter)
    ]);
    return {containers,networks,volumes:volumes.Volumes||[]};
  };
  let runner, controller;
  try {
    controller=child(['controller'],{POUNDLOCK_ALLOW_JOB_NETWORK:'true',POUNDLOCK_BIND:'127.0.0.1:17331',POUNDLOCK_DATA_DIR:root+'/data',POUNDLOCK_RUNTIME_DIR:root+'/runtime'});
    let token;
    for(let n=0;n<100&&!token;n++) {try {token=(await fs.readFile(root+'/data/admin-token','utf8')).trim();} catch {await delay(100);}}
    assert.ok(token,'controller did not start');
    const api = async (path,body) => {
      const response=await fetch('http://127.0.0.1:17331'+path,{method:body===undefined?'GET':'POST',headers:{authorization:'Bearer '+token,'content-type':'application/json'},body:body===undefined?undefined:JSON.stringify(body),signal:AbortSignal.timeout(10000)});
      assert.ok(response.ok,`${path}: ${response.status}`); return response.json();
    };
    const endpoint=await api('/api/v1/endpoints',{name:'fixture',kind:'openai',base_url:'http://127.0.0.1:17333/v1',api_key:''});
    const profile=await api('/api/v1/model-profiles',{name:'fixture',endpoint_id:endpoint.id,model_id:'fixture',context_tokens:8192,max_output_tokens:1024,workload_class:'durable-coding-agent',wake_policy:'never'});
    const start = () => child(['runner','--controller','http://127.0.0.1:17331','--token-file',root+'/runtime/runner-token','--work-dir',root+'/work','--result-dir',root+'/results','--capacity','2','--wall-seconds',mode==='deadline'?'15':'90'],
      // The fixture's workers are tiny; plan as if the host had room for two real ones.
      {POUNDLOCK_RUNNER_MEMORY_BYTES:String(64*1024**3)});
    runner=start();
    const check="python3 -m unittest -q";
    const networkCommand=`npm --prefix /tmp/dependencies --cache /tmp/npm-cache install --ignore-scripts --no-audit --no-fund --package-lock=false --fetch-retries=0 --fetch-timeout=5000 is-number@7.0.0 && node -e "if(!require('/tmp/dependencies/node_modules/is-number')(42))process.exit(1)"`;
    let commands=[{command:check,timeout_seconds:20}];
    if(mode==='verify'||mode==='concurrency')commands.push({command:'printf verifier-only > greeting.txt; touch verifier-only',timeout_seconds:20},{command:'test "$(cat greeting.txt)" = verifier-only',timeout_seconds:20});
    if(mode==='check-failed')commands=[{command:'exit 1',timeout_seconds:10}];
    if(mode==='command-help')commands=[{command:'--help',timeout_seconds:10}];
    if(mode==='missing')commands=[{command:'poundlock-nonexistent-check',timeout_seconds:10}];
    if(mode==='check-timeout'||interruptedCheck)commands=[{command:'printf check-started; sleep 60 & wait',timeout_seconds:mode==='check-timeout'?1:80}];
    if(interruptedCheck)commands.unshift({command:'printf checkpoint-marker',timeout_seconds:10});
    if(mode==='network'||mode==='network-blocked')commands=[{command:networkCommand,timeout_seconds:20}];
    // A uv-managed CPython 3.14 project: dependency setup, then protected tests.
    if(mode==='python-uv')commands=[{command:'uv sync',timeout_seconds:120},{command:'uv run --frozen pytest -q',timeout_seconds:60}];
    // A Git source is fetched in its own container (L13): the repository's own
    // README must arrive, and the worker's change must apply on top of it.
    if(mode==='git')commands=[{command:'test -f README && test "$(cat greeting.txt)" = "Concurrent worker finished."',timeout_seconds:20}];
    const policy={verification:{commands,protected_paths:[mode==='git'?'README':'test_greeting.py']},max_repairs:repairMode||mode==='missing'?1:0,network:['network','python-uv'].includes(mode)?'outbound':'inference_only'};
    const testCode=mode==='python-uv'
      ?"import sys\nfrom pathlib import Path\n\ndef test_greeting():\n    assert sys.version_info[:2] == (3, 14), sys.version\n    assert Path('greeting.txt').read_text() == 'Concurrent worker finished.\\n'\n"
      :"import unittest\nfrom pathlib import Path\nclass Greeting(unittest.TestCase):\n def test_greeting(self): self.assertEqual(Path('greeting.txt').read_text(), 'Concurrent worker finished.\\n')\n";
    const pyproject='[project]\nname = "poundlock-python-uv"\nversion = "0.0.1"\nrequires-python = ">=3.14"\ndependencies = ["pytest>=8"]\n';
    const extraFiles=mode==='python-uv'?[{path:'pyproject.toml',content_base64:Buffer.from(pyproject).toString('base64')}]:[];
    const jobs=[];
    for(let n=0;n<(concurrent?3:1);n++) jobs.push(await api('/api/v1/jobs',{
      idempotency_key:'worker-'+n,model_profile_id:profile.id,harness_profile_id:'pi',prompt:'Update greeting.txt.',policy:mode==='combined'&&n===0?{...policy,verification:{...policy.verification,commands:[{command:'printf combined-check-started; sleep 60 & wait',timeout_seconds:80}]}}:policy,
      source:mode==='git'?{kind:'git',repository:'https://github.com/octocat/Hello-World',revision:'7fd1a60b01f91b314f59955a4e4d4e80d8edf11d'}:{kind:'snapshot',files:[{path:'greeting.txt',content_base64:Buffer.from('Original.\n').toString('base64')},{path:'test_greeting.py',content_base64:Buffer.from(testCode).toString('base64')},...extraFiles]}
    }));
    if(interruptedAgent) {
      for(let n=0;n<200&&calls<2;n++) await delay(100);
      assert.equal(calls,2,'worker never reached inference: '+logs);
      assert.ok((await owned()).containers.some(container=>container.State==='running'),'no running worker');
      if(mode==='cancel') await api('/api/v1/jobs/'+jobs[0].id+'/cancel',{});
      if(mode==='restart') {
        runner.kill('SIGKILL');
        await new Promise(resolve=>runner.once('exit',resolve));
        runner=start();
      }
    }
    if(interruptedCheck||mode==='repair-restart') {
      for(let n=0;n<500;n++) {
        const resources=await owned();
        if(interruptedCheck&&resources.containers.some(c=>c.Names.some(name=>name.endsWith('-check')))){
          const checkpoint=JSON.parse(await fs.readFile(root+'/results/outbox/'+jobs[0].id+'.json','utf8'));
          if(checkpoint.result.provenance.verification.commands.length)break;
        }
        if(mode==='repair-restart'&&calls>=3)break;
        await delay(100);
      }
      assert.ok((await fs.readdir(root+'/results/outbox')).length,'candidate checkpoint missing');
      if(mode==='verify-cancel')await api('/api/v1/jobs/'+jobs[0].id+'/cancel',{});
      else {runner.kill('SIGKILL');await new Promise(resolve=>runner.once('exit',resolve));runner=start();}
    }
    let finished=[],maxRunning=0,sawQueued=false,combinedCancelled=false;
    for(let n=0;n<900;n++) {
      finished=await Promise.all(jobs.map(job=>api('/api/v1/jobs/'+job.id)));
      maxRunning=Math.max(maxRunning,finished.filter(job=>job.status==='running').length);
      sawQueued ||= finished.some(job=>job.status==='ready');
      if(mode==='combined'&&!combinedCancelled&&(await owned()).containers.some(c=>c.Names.includes('/poundlock-'+jobs[0].attempt_id+'-check'))){await api('/api/v1/jobs/'+jobs[0].id+'/cancel',{});combinedCancelled=true;}
      if(finished.every(job=>['completed','failed','cancelled'].includes(job.status))) break;
      await delay(100);
    }
    if(concurrent) {
      assert.equal(maxRunning,2,'configured worker limit was not respected: '+logs);
      assert.ok(sawQueued,'third job never queued');
      assert.equal(peak,2,'two workers did not make concurrent model requests');
      assert.equal(calls,6);
      for(const job of finished) {
        assert.equal(job.status,mode==='combined'&&job.id===jobs[0].id?'cancelled':'completed',JSON.stringify({job,logs}));
        assert.ok(job.result.patches[0].patch.includes('+Concurrent worker finished.'));
        assert.equal(job.result.cleanup.containers_removed,true);
      }
    } else if(interruptedAgent) {
      const job=finished[0];
      assert.equal(job.status,mode==='cancel'?'cancelled':'failed',JSON.stringify({job,logs}));
      // 'deadline' hangs the model mid-turn (see the mock server above), so
      // the agent's own turn never settles before the wall clock fires; it
      // must be reported as a mid-turn cutoff, not the bare exhausted code,
      // and must skip verification (no repair/verification evidence here).
      const code={cancel:'cancelled_or_lease_lost',restart:'runner_interrupted',deadline:'wall_budget_mid_turn'}[mode];
      assert.equal(job.error.code,code,JSON.stringify({job,logs}));
      assert.equal(calls,2,'an interrupted attempt was silently rerun');
      if(mode!=='restart'){assert.ok(job.result.output.includes('Update greeting.txt.'));assert.ok(job.result.patches[0].patch.includes('+Concurrent worker finished.'),'partial patch was lost');}
      if(mode==='deadline'){
        // B1: the runner-reported wall budget and computed deadline are
        // visible on the job resource once a runner has claimed it.
        assert.equal(job.wall_seconds,15,JSON.stringify({job,logs}));
        assert.ok(job.deadline_at,'deadline_at missing from a claimed job');
      }
    }
    if(!interruptedAgent) {
      for(const job of finished) {
        const report=job.result?.provenance?.verification;
        const combinedVictim=mode==='combined'&&job.id===jobs[0].id;
        const expected=combinedVictim?'cancelled_or_lease_lost':{ 'check-failed':'verification_failed','repair-limit':'verification_failed',protected:'protected_paths_changed',missing:'verification_unavailable','command-help':'verification_unavailable','check-timeout':'verification_interrupted','verify-cancel':'cancelled_or_lease_lost','verify-restart':'runner_interrupted','repair-restart':'runner_interrupted','network-blocked':'verification_failed' }[mode];
        assert.equal(job.status,mode==='verify-cancel'||combinedVictim?'cancelled':expected?'failed':'completed',JSON.stringify({job,logs}));
        if(expected)assert.equal(job.error.code,expected,JSON.stringify({job,logs}));
        else assert.equal(report.status,'passed',JSON.stringify({job,logs}));
        assert.ok(job.result.patches[0].patch.length,'candidate patch missing');
        assert.equal(require('node:crypto').createHash('sha256').update(JSON.stringify(job.result.patches)).digest('hex'),report.candidate_sha256,'verification is not bound to the delivered candidate');
        assert.ok(!job.result.patches[0].patch.includes('verifier-only'),'verification changed the deliverable');
        if(mode==='protected'){assert.deepEqual(report.changed_protected_paths,['test_greeting.py']);assert.equal(report.commands.length,0);}
        if(['repair','repair-limit'].includes(mode)){
          assert.equal(job.result.provenance.attempt_count,2);
          assert.equal(job.result.provenance.previous_attempts[0].verification.status,'failed');
          assert.ok(job.result.provenance.previous_attempts[0].patches[0].patch.includes('+Wrong.'));
          assert.equal(calls,4,'repair must have exactly one fresh agent turn');
        }
        if(interruptedCheck)assert.ok(report.commands[0].output.includes('checkpoint-marker'),'completed check lost during recovery');
        if(mode==='missing')assert.equal(calls,2,'missing commands must not retry');
        if(mode==='reasoning-deltas'){
          // 20000 raw reasoning_content chunks would be several MiB of
          // message_update JSON on their own, well past the 2 MiB per-stream
          // agent output budget; completing without output_limit, with no
          // thinking_delta lines and with the omission summary present, is
          // the fix under test.
          assert.equal(job.error,null,'agent output was truncated: '+JSON.stringify(job.error));
          assert.ok(job.result.output.includes('[poundlock] omitted'),'omitted-events summary missing: '+logs);
          assert.ok(!job.result.output.includes('thinking_delta'),'streaming deltas leaked into retained output');
          assert.ok(job.result.output.length<1.5*1024*1024,'retained output was not bounded: '+job.result.output.length);
        }
        if(mode==='python-uv'){
          assert.equal(report.commands.length,2,JSON.stringify(report));
          assert.ok(report.commands[1].output.includes('1 passed'),JSON.stringify(report));
          assert.ok(!job.result.patches[0].patch.includes('uv.lock')&&!job.result.patches[0].patch.includes('.venv'),'dependency setup leaked into the deliverable');
        }
        if(mode==='repair-restart')assert.equal(calls,3,'restart must not rerun the repair');
        const download=await fetch(`http://127.0.0.1:17331/api/v1/jobs/${job.id}/patches/0`,{headers:{authorization:'Bearer '+token}});
        assert.equal(download.status,200);assert.equal(await download.text(),job.result.patches[0].patch);
        assert.equal((await fetch(`http://127.0.0.1:17331/api/v1/jobs/${job.id}/patches/0`)).status,401);
        // Apply the downloaded patch independently and verify it represents the
        // reported outcome, without trusting the worker's result JSON alone.
        const applied=root+'/apply-'+job.id;await fs.mkdir(applied);
        // A Git job's patch creates greeting.txt on top of the repository.
        if(mode!=='git'){await fs.writeFile(applied+'/greeting.txt','Original.\n');await fs.writeFile(applied+'/test_greeting.py',testCode);}
        await fs.writeFile(applied+'/change.patch',job.result.patches[0].patch);
        const git=spawn('git',['apply','--check','change.patch'],{cwd:applied});
        assert.equal(await new Promise(resolve=>git.once('exit',resolve)),0,'patch does not apply');
      }
    }
    for(const job of finished) {
      if(job.result)for(const field of ['containers_removed','worktrees_removed','attempt_directory_removed'])
        assert.equal(job.result.cleanup[field],true,JSON.stringify({job,logs}));
    }
    const leftovers=await owned();
    assert.deepEqual(Object.fromEntries(Object.entries(leftovers).map(([k,v])=>[k,v.length])),{containers:0,networks:0,volumes:0});
    assert.deepEqual(await fs.readdir(root+'/work'),[]);
    for(let n=0;n<30;n++) {if(!(await fs.readdir(root+'/results/outbox')).length) break; await delay(100);}
    assert.deepEqual(await fs.readdir(root+'/results/outbox'),[]);
    assert.deepEqual(await fs.readdir(root+'/results/active'),[]);
    console.log(`PASS: ${mode}; actual worker lifecycle, durable outcome and zero owned Docker resources. Inference was a protocol fixture.`);
  } finally {
    if(runner && runner.exitCode===null) {
      runner.kill('SIGTERM');
      await new Promise(resolve=>{const timeout=setTimeout(resolve,30000);runner.once('exit',()=>{clearTimeout(timeout);resolve()});});
    }
    controller?.kill('SIGTERM');
    model.closeAllConnections(); model.close();
    // Fixture failure must not pollute the shared CI Docker engine. Assertions
    // above run before this rescue cleanup, which cannot turn failure into pass.
    try {
      const resources=await owned();
      for(const container of resources.containers) await docker('DELETE','/containers/'+container.Id+'?force=true');
      for(const network of resources.networks) {
        const details=await docker('GET','/networks/'+network.Id);
        for(const member of Object.keys(details.Containers||{})) await docker('POST','/networks/'+network.Id+'/disconnect',{Container:member,Force:true});
        await docker('DELETE','/networks/'+network.Id);
      }
      for(const volume of resources.volumes) await docker('DELETE','/volumes/'+volume.Name);
    } catch(error) {console.error('Fixture cleanup:',error.message);}
    for(const process of children) if(process.exitCode===null) process.kill('SIGKILL');
  }
})().catch(error=>{console.error(error.stack);process.exitCode=1;});
