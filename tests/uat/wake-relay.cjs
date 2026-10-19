// Run in the built image as UID 10001 with no capabilities and --network none.
const fs=require('node:fs/promises'),http=require('node:http'),dgram=require('node:dgram'),assert=require('node:assert/strict'),{spawn}=require('node:child_process');
(async()=>{
 const dir=await fs.mkdtemp('/tmp/poundlock-wake-');await fs.chmod(dir,0o700);const socketPath=dir+'/wake.sock';
 const udp=dgram.createSocket('udp4');await new Promise(resolve=>udp.bind(0,'127.0.0.1',resolve));const packets=[];udp.on('message',data=>packets.push(data));
 const child=spawn('poundlock',['wake-relay','--socket',socketPath],{env:{...process.env,TOKIO_WORKER_THREADS:'2'},stdio:['ignore','pipe','pipe']});let logs='';child.stderr.on('data',data=>logs+=data);child.stdout.on('data',data=>logs+=data);
 const request=(path,body)=>new Promise((resolve,reject)=>{const r=http.request({socketPath,path,method:body?'POST':'GET',headers:{'content-type':'application/json'}},response=>{response.resume();response.on('end',()=>resolve(response.statusCode))});r.on('error',reject);r.setTimeout(2000,()=>r.destroy(new Error('relay request timeout')));r.end(body?JSON.stringify(body):undefined)});
 try{
  for(let n=0;n<100;n++){try{await fs.stat(socketPath);break}catch{await new Promise(r=>setTimeout(r,20))}}
  assert.equal(await request('/healthz'),200,logs);
  assert.equal(await request('/wake',{mac:'02:00:00:00:00:01',broadcast:'127.0.0.1:'+udp.address().port}),200);
  for(let n=0;n<100&&packets.length<3;n++)await new Promise(r=>setTimeout(r,10));
  assert.equal(packets.length,3);const expected=Buffer.from('ff'.repeat(6)+'020000000001'.repeat(16),'hex');for(const packet of packets)assert.deepEqual(packet,expected);
  assert.equal(await request('/wake',{mac:'invalid',broadcast:'127.0.0.1:9'}),400);
  assert.equal(await request('/wake',{mac:'02:00:00:00:00:01',broadcast:'example.com:9'}),400);
  console.log('PASS: unprivileged packaged wake relay accepted only its Unix-socket request and emitted the exact three UDP magic packets. Physical LAN wake remains a separate acceptance gate.');
 }finally{if(child.exitCode===null&&child.signalCode===null){child.kill('SIGTERM');await new Promise(resolve=>child.once('exit',resolve))}udp.close();await fs.rm(dir,{recursive:true,force:true})}
})().catch(error=>{console.error(error.message);process.exitCode=1});
