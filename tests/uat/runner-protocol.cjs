// Run inside the built supervisor image with Docker socket access. This is a deterministic protocol
// fixture for the actual Pi harness; it does not claim real-model acceptance.
const fs=require('node:fs/promises');const http=require('node:http');const {spawn}=require('node:child_process');const assert=require('node:assert/strict');
(async()=>{
 const key=process.env.POUNDLOCK_TEST_INFERENCE_KEY||'';const modelError=process.env.POUNDLOCK_TEST_MODEL_ERROR==='1';const isolation=process.env.POUNDLOCK_TEST_WORKER_ISOLATION==='1';
 const root=await fs.mkdtemp('/tmp/poundlock-runner-');const children=[];let calls=0;let logs='';
 const upstream=http.createServer(async(req,res)=>{
  for await(const chunk of req){} // Consume request before replying.
  if(req.url==='/v1/models'){res.setHeader('content-type','application/json');return res.end(JSON.stringify({data:[{id:'fixture'}]}))}
  if(req.url!=='/v1/chat/completions'){res.writeHead(404);return res.end()}
  assert.equal(req.headers.authorization,key?'Bearer '+key:undefined,'unexpected model authentication');
  calls++;if(modelError){res.writeHead(401,{'content-type':'application/json'});return res.end(JSON.stringify({error:{message:'Fixture model rejected authentication',type:'authentication_error'}}))}res.setHeader('content-type','text/event-stream');
  const script=`set -eu
 test ! -S /var/run/docker.sock
 test ! -e '${root}/runtime/runner-token'
 test ! -e /results/supervisor-id
 test -z "\${POUNDLOCK_TEST_INFERENCE_KEY-}"
 test "$(id -u)" = 10001
 test ! -w /usr/local/bin
 ! kill -STOP 1 2>/dev/null
 mkdir -p .git/info
 git config diff.external /bin/false
 git config filter.poison.clean /bin/false
 git config filter.poison.required true
 printf '*.txt filter=poison\\n' > .git/info/attributes
 printf 'Hello from the protocol fixture.\\n' > greeting.txt
 node -e 'const net=require("node:net");const s=net.connect({host:"1.1.1.1",port:80});s.on("connect",()=>process.exit(17));s.on("error",()=>process.exit(0));setTimeout(()=>process.exit(0),500);'
 printf 'POUNDLOCK_ISOLATION_OK\\n'
 `;
  const tool=isolation?{name:'bash',arguments:JSON.stringify({command:script})}:{name:'write',arguments:JSON.stringify({path:'greeting.txt',content:'Hello from the protocol fixture.\n'})};
  const delta=calls===1?{role:'assistant',tool_calls:[{index:0,id:'fixture-write',type:'function',function:tool}]}:{role:'assistant',content:'Updated greeting.txt. This is fixture output.'};
  const packet=(delta,finish_reason)=>JSON.stringify({id:'fixture-'+calls,object:'chat.completion.chunk',created:1,model:'fixture',choices:[{index:0,delta,finish_reason}]});
  res.write('data: '+packet(delta,null)+'\n\n');res.write('data: '+packet({},calls===1?'tool_calls':'stop')+'\n\n');res.end('data: [DONE]\n\n');
 });
 await new Promise(resolve=>upstream.listen(17333,'127.0.0.1',resolve));
 function child(args,env){const process=spawn('poundlock',args,{env:{...global.process.env,...env},stdio:['ignore','pipe','pipe']});for(const stream of [process.stdout,process.stderr])stream.on('data',chunk=>logs=(logs+chunk).slice(-12000));children.push(process);return process}
 try{
  child(['controller'],{POUNDLOCK_BIND:'127.0.0.1:17331',POUNDLOCK_DATA_DIR:root+'/data',POUNDLOCK_RUNTIME_DIR:root+'/runtime'});
  let token;
  for(let n=0;n<100&&!token;n++){try{token=(await fs.readFile(root+'/data/admin-token','utf8')).trim()}catch{await new Promise(r=>setTimeout(r,100))}}
  assert.ok(token,'controller failed to initialize');
  const api=async(path,data)=>{const response=await fetch('http://127.0.0.1:17331'+path,{method:data===undefined?'GET':'POST',headers:{authorization:'Bearer '+token,'content-type':'application/json'},body:data===undefined?undefined:JSON.stringify(data),signal:AbortSignal.timeout(10000)});assert.ok(response.ok,`${path}: ${response.status}`);return response.json()};
  const endpoint=await api('/api/v1/endpoints',{name:'fixture',kind:'openai',base_url:'http://127.0.0.1:17333/v1',api_key:key});
  const model=await api('/api/v1/model-profiles',{name:'fixture',endpoint_id:endpoint.id,model_id:'fixture',context_tokens:8192,max_output_tokens:1024,workload_class:'durable-coding-agent',wake_policy:'never'});
  child(['runner','--controller','http://127.0.0.1:17331','--token-file',root+'/runtime/runner-token','--projects-dir',root+'/projects','--work-dir',root+'/work','--result-dir',root+'/results','--wall-seconds','60'],{});
  const accepted=await api('/api/v1/jobs',{idempotency_key:'fixture',model_profile_id:model.id,harness_profile_id:'pi',prompt:'Change greeting.txt to the requested greeting.',source:{kind:'snapshot',files:[{path:'greeting.txt',content_base64:Buffer.from('Original greeting.\n').toString('base64')}]}});
  let job;
  for(let n=0;n<600;n++){job=await api('/api/v1/jobs/'+accepted.id);if(['completed','failed','cancelled'].includes(job.status))break;await new Promise(r=>setTimeout(r,100))}
  assert.equal(job.status,modelError?'failed':'completed',JSON.stringify({error:job.error,output:job.result?.output,logs}));
  if(modelError)assert.equal(job.error.code,'model_request_failed');else assert.ok(job.result.patches[0].patch.includes('+Hello from the protocol fixture.'));
  assert.equal(job.result.provenance.source.kind,'snapshot');assert.equal(job.result.provenance.verification.status,'unverified');
  assert.equal(job.result.cleanup.attempt_directory_removed,true);assert.equal(job.result.cleanup.containers_removed,true);assert.equal(job.result.provenance.execution.isolation,'docker');
  assert.equal((await fs.readdir(root+'/work')).length,0);
  for(let n=0;n<30;n++){if(!(await fs.readdir(root+'/results/outbox')).length)break;await new Promise(r=>setTimeout(r,100))}
  assert.equal((await fs.readdir(root+'/results/outbox')).length,0);
  assert.equal(calls,modelError?1:2);
  if(isolation){const events=job.result.output.split('\n').flatMap(line=>{try{return [JSON.parse(line)]}catch{return []}});assert.ok(events.some(event=>event.type==='tool_execution_end'&&event.toolName==='bash'&&!event.isError&&JSON.stringify(event.result).includes('POUNDLOCK_ISOLATION_OK')),'isolation assertions did not finish successfully');}

  console.log((modelError?'Expected model failure was classified correctly. ':'')+'PASS: real Pi harness consumed supplied source, executed a fixture tool call, returned the expected patch and unverified provenance, cleaned workspace, and received result acknowledgement. No real model was used.');
 }finally{for(const process of children)process.kill('SIGTERM');upstream.close();}
})().catch(error=>{console.error(error.message);process.exitCode=1});
