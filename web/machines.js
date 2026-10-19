function versionNote(node){
  const agent=node.capabilities?.agent_version;
  const current=state.data.version;
  if(!agent)return "Agent version unknown";
  if(agent===current)return `Agent v${agent}`;
  return `Agent v${agent} · controller v${current}`;
}

// Node statuses: online = ok, maintenance = review, revoked (removed) = ended.
// Anything else (offline, sleeping, not yet reported) is the unknown/muted
// treatment — deliberately distinct from "maintenance", which is a choice an
// operator made, not an absence of information.
const NODE_STATE={online:["ok","Online"],maintenance:["review","Paused for maintenance"],revoked:["ended","Removed"]};
function nodeStateBadge(status){const [cls,label]=NODE_STATE[status]||["unknown","Sleeping or offline"];return badge(cls,label)}
function nodeStateBucket(status){return (NODE_STATE[status]||["unknown"])[0]}

let wakeSelected=null,wakeStamp=null,wakeDirty=false,wakeBusy=false;
function renderMachines(){
  const d=state.data;
  const plainHttp=location.protocol==='http:'&&!/^(localhost|127(\.\d+){3}|\[::1\])$/i.test(location.hostname);
  $("node-reach-hint").textContent=`Controller URL: ${location.origin}. If a new machine stays offline, check that its node service is running and can reach this URL.${plainHttp?' Nodes refuse a plain HTTP controller on another machine unless node.toml sets allow_insecure_http = true; prefer an HTTPS address.':''}`;
  updateOptions("wol-node",options(d.nodes.filter(node=>node.status!=="revoked"),x=>x.name));
  if(!wakeBusy)loadWakeSettings();
  const removed=d.nodes.filter(node=>node.status==="revoked");
  $("removed-machines").hidden=!removed.length;
  $("removed-node-list").innerHTML=removed.map(node=>`<div class="item">${rowIndicator("ended")}<strong>${esc(node.name)}</strong>${nodeStateBadge(node.status)}<p class="hint">A new installation requires a new enrollment token.</p></div>`).join("");
  const list=$("node-list"),nodes=d.nodes.filter(node=>node.status!=="revoked");
  if(!nodes.length){emptyState(list,"No machines enrolled.","Enroll one below to submit jobs and serve models.");return}
  const draftMemory=new Map([...list.querySelectorAll("[data-memory-form]")].filter(f=>f.dataset.dirty).map(f=>[f.dataset.memoryForm,f.elements.idle.value]));
  const expanded=new Set([...list.querySelectorAll('details[open][data-node-details]')].map(el=>el.dataset.nodeDetails));
  list.className="list";
  list.innerHTML=nodes.map(node=>{
    const runtimes=(node.capabilities?.runtimes||[]).join(", ")||"No runtimes advertised";
    const wol=node.wol_mac?`WOL ${node.wol_mac}`:"WOL not configured";
    return `<div class="item">${rowIndicator(nodeStateBucket(node.status))}<strong>${esc(node.name)}</strong><div class="meta">${nodeStateBadge(node.status)}<span>${esc(node.platform)}</span><span>${esc(runtimes)}</span></div><div class="meta"><span>${node.last_seen_at?`Last seen ${new Date(node.last_seen_at).toLocaleString()}`:"Never connected"}</span><span>${esc(wol)}</span><span>${esc(versionNote(node))}</span></div><p class="hint">${esc(networkNote(node))}</p><div class="item-actions"><button type="button" class="secondary" data-pause-node="${esc(node.id)}">${node.status==="maintenance"?"Resume inference":"Pause inference"}</button><button type="button" class="secondary" data-remove-node="${esc(node.id)}">Remove machine</button></div></div>`;
  }).join("");
  for(const node of nodes){
    const button=list.querySelector(`[data-remove-node="${CSS.escape(node.id)}"]`);
    button.closest('.item').insertAdjacentHTML('beforeend',nodeDetails(node));
  }
  for(const detail of list.querySelectorAll('[data-node-details]'))detail.open=expanded.has(detail.dataset.nodeDetails);
  for(const f of list.querySelectorAll('[data-memory-form]')){if(draftMemory.has(f.dataset.memoryForm)){f.elements.idle.value=draftMemory.get(f.dataset.memoryForm);f.dataset.dirty='true'}}
}

$("node-list").addEventListener("click",async event=>{
  const button=event.target.closest("[data-remove-node]");if(!button)return;
  const node=state.data.nodes.find(n=>n.id===button.dataset.removeNode);if(!node)return;
  if(!confirm(`Remove ${node.name}? Its node credential will stop working. Job history is kept. This does not uninstall files on that computer. Active work must finish or be cancelled first.`))return;
  button.disabled=true;
  try{await api(`/api/v1/nodes/${encodeURIComponent(node.id)}`,{method:"DELETE"});await refresh();toast(`${node.name} removed`)}
  catch(error){fail(error);button.disabled=false}
});

function networkNote(node){
  const network=node.capabilities?.network;
  if(!network?.mode)return "Connection details unavailable. Update the node to report them.";
  if(network.mode==="system"){
    const available=(network.interfaces||[]).filter(item=>item.connected).map(item=>`${item.name}: ${(item.addresses||[]).join(", ")}${item.mac?` · MAC ${item.mac}`:""}`).join("; ");
    return `System routing${available?` · Last reported connections: ${available}`:" · No connection details reported"}`;
  }
  const connection=network.status==="connected"?"connected":network.status==="disconnected"?"disconnected or without a usable IP address":"unavailable";
  const addresses=(network.addresses||[]).join(", ")||"no addresses reported";
  return `Selected interface, last reported: ${network.interface} · ${connection} · ${addresses}${network.mac?` · MAC ${network.mac}`:""}.`;
}

function reportBytes(value){
  return typeof value==='number'&&Number.isFinite(value)&&value>=0?`${(value/1073741824).toFixed(1)} GiB`:'Not reported';
}
function reportRow(label,value){return `<dt>${esc(label)}</dt><dd>${esc(value??'Not reported')}</dd>`}
function nodeDetails(node){
  const inv=node.capabilities?.inventory,interfaces=nodeInterfaces(node);
  const models=(state.data.models||[]).filter(model=>model.node_id===node.id);
  const missingRuntimes=models.filter(model=>model.runtime_profile&&!(node.capabilities?.runtimes||[]).includes(model.runtime_profile));
  const gpus=Array.isArray(inv?.gpus)?inv.gpus:[],software=Array.isArray(inv?.software)?inv.software:[];
  const gpuSummary=gpus.map(gpu=>gpu.name||`${gpu.vendor||'GPU'} ${gpu.device_id||''}`).join(', ');
  return `${node.capabilities?.controller_transport==='insecure_http'?'<p class="hint">Connects over plain HTTP (allow_insecure_http in node.toml): its credential and the controller\'s instructions cross the network unencrypted. Put HTTPS in front of the controller and remove the setting.</p>':''}
    <p class="hint">${esc(models.length?`${models.length} model profile${models.length===1?'':'s'} configured`:'No model profile configured')}</p>
    ${missingRuntimes.length?`<p class="hint">${esc(missingRuntimes.map(model=>model.name).join(', '))}: the configured runtime is not reported by this machine. Update the model profile on <a href="#models">Models</a> or restore its runtime in the node configuration.</p>`:''}
    ${modelMemoryPanel(node,models)}
    <button type="button" class="secondary" data-connect-node="${esc(node.id)}">Connect a model server</button>
    <details data-node-details="${esc(node.id)}-setup"><summary>Model server setup help</summary>${installationHelp(node)}</details>
    <details data-node-details="${esc(node.id)}-inventory"><summary>Technical details${gpuSummary?` · ${esc(gpuSummary)}`:''}</summary>
    ${inv?`<p class="hint">Collected ${esc(new Date(inv.collected_at).toLocaleString())}. Last reported values remain available while offline.</p>
      <dl class="node-facts">${reportRow('Operating system',[inv.os?.name,inv.os?.version,inv.os?.arch].filter(Boolean).join(' '))}${reportRow('Kernel',inv.os?.kernel)}${inv.os?.glibc?reportRow('glibc',inv.os.glibc):''}${reportRow('Computer',[inv.os?.machine_vendor,inv.os?.machine_model].filter(Boolean).join(' ')||null)}${reportRow('Processor',inv.cpu?.model)}${reportRow('Logical cores available to node',inv.cpu?.logical_cores)}${reportRow('Memory',reportBytes(inv.memory?.total_bytes))}${inv.memory?.available_bytes!=null?reportRow('Memory available at collection',reportBytes(inv.memory.available_bytes)):''}${reportRow('Node filesystem space available',reportBytes(inv.storage?.available_bytes))}${reportRow('Node filesystem size',reportBytes(inv.storage?.total_bytes))}</dl>
      <h4>Graphics</h4>${gpus.length?gpus.map(gpu=>`<dl class="node-facts">${reportRow('GPU',gpu.name||[gpu.vendor,gpu.device_id].filter(Boolean).join(' '))}${reportRow('GPU memory',gpu.vram||reportBytes(gpu.vram_bytes))}${gpu.driver?reportRow('Driver',[gpu.driver,gpu.driver_version].filter(Boolean).join(' ')):''}${gpu.pci_address?reportRow('PCI address',gpu.pci_address):''}${gpu.metal?reportRow('Metal',gpu.metal):''}</dl>`).join(''):'<p class="hint">GPU details were not reported.</p>'}
      <h4>Software</h4><dl class="node-facts">${software.map(item=>reportRow(item.name,item.status==='detected'?'Detected':'Not detected in checked locations')).join('')}</dl>
      <p class="hint">Software detection does not confirm that a server is running. Custom installations and containers may not be detected.</p>
      ${runtimeIdentityRows(inv,models)}`:`<p class="hint">This node has not reported hardware and software details. ${node.platform==='linux'?'Update it using the <a href="#linux-guide">Linux installation guide</a>.':'Update the node binary and restart its service.'} Keep the existing configuration and enrollment.</p>`}
      <h4>Network interfaces</h4>${interfaces.length?interfaces.map(item=>`<dl class="node-facts">${reportRow('Interface',[item.name,item.kind,item.connected?'connected':'disconnected'].filter(Boolean).join(' · '))}${reportRow('MAC address',item.mac)}${reportRow('IP addresses',(item.addresses||[]).join(', ')||null)}${item.broadcasts?.length?reportRow('Broadcast addresses',item.broadcasts.join(', ')):''}${item.driver?reportRow('Driver',item.driver):''}${item.mtu?reportRow('MTU',item.mtu):''}</dl>`).join(''):'<p class="hint">No interface details reported.</p>'}
      <button type="button" class="secondary" data-download-node="${esc(node.id)}">Download technical report</button>
    </details>`;
}
function runtimeIdentityRows(inv,models){
  const runtimes=inv?.runtimes&&typeof inv.runtimes==='object'?Object.entries(inv.runtimes):[];
  if(!runtimes.length)return '';
  return `<h4>Runtime identity</h4><dl class="node-facts">${runtimes.map(([name,info])=>{
    if(info?.error)return reportRow(name,`Not reported (${info.error})`);
    const application=[info?.application?.name,info?.application?.version].filter(Boolean).join(' ');
    const served=info?.model?`Serving ${esc(info.model)}`:'';
    // The label is typed by an operator; the machine reports what it loaded.
    const labelled=(models||[]).filter(model=>model.runtime_profile===name&&model.model_label);
    const mismatch=info?.model&&labelled.length&&!labelled.some(model=>model.model_label===info.model)
      ? ` — labelled ${labelled.map(model=>esc(model.model_label)).join(', ')}` : '';
    return reportRow(name,[application||'Reported, no application details',served].filter(Boolean).join(' · ')+mismatch);
  }).join('')}</dl><p class="hint">Reported by each runtime's own identity command, not managed by Poundlock. A label that differs from the served model is shown so you can correct one of them.</p>`;
}
function resourceLink(name,url,description){return `<li><a href="${url}" target="_blank" rel="noopener noreferrer">${name}</a> — ${description}</li>`}
function installationHelp(node){
  const inv=node.capabilities?.inventory,os=inv?.os||{},platform=node.platform,arch=os.arch||node.capabilities?.arch;
  const mac=platform==='macos',apple=mac&&arch==='aarch64',intelMac=mac&&arch==='x86_64';
  const gpus=Array.isArray(inv?.gpus)?inv.gpus:[],amd=gpus.some(gpu=>gpu.vendor==='AMD'||/AMD|Radeon/i.test(gpu.name||gpu.vendor||'')),nvidia=gpus.some(gpu=>gpu.vendor==='NVIDIA'),intel=gpus.some(gpu=>gpu.vendor==='Intel');
  const family=`${os.id||''} ${os.family||''}`;
  let links='';
  if(apple||(mac&&!arch))links+=resourceLink('oMLX','https://github.com/jundot/omlx','MLX model serving for Apple silicon, with a macOS app and command-line controls.');
  if(intelMac||(mac&&!arch))links+=resourceLink('ToshLLM','https://github.com/engeldlgado/toshllm','Intel Macs with AMD graphics; includes a model server. Check its hardware and macOS requirements.');
  if(!intelMac)links+=resourceLink('LM Studio','https://lmstudio.ai/docs/app','Desktop model downloads and chat; enable its API server to connect Poundlock.');
  links+=resourceLink('Ollama',mac?'https://docs.ollama.com/macos':'https://docs.ollama.com/linux','Command-line model downloads and a background API server.');
  links+=resourceLink('llama.cpp','https://github.com/ggml-org/llama.cpp/tree/master/tools/server','GGUF model serving with CPU and GPU backend options.');
  let drivers='';
  if(!mac){
    if(amd)drivers+=resourceLink('AMD ROCm compatibility and installation','https://rocm.docs.amd.com/','Check your exact GPU, operating system and backend version.');
    if(nvidia)drivers+=resourceLink('NVIDIA Linux drivers','https://docs.nvidia.com/datacenter/tesla/driver-installation-guide/','Driver installation guidance and distribution requirements.');
    if(intel)drivers+=resourceLink('Intel Linux GPU drivers','https://dgpu-docs.intel.com/driver/client/overview.html','Client GPU driver and compute runtime guidance.');
    drivers+=resourceLink('Ollama GPU support','https://docs.ollama.com/gpu','GPU requirements and ROCm, CUDA and Vulkan backend choices.');
    if(/arch|cachyos/.test(family)){
      drivers+=resourceLink(amd?'Arch ollama-rocm package':'Arch Ollama package',amd?'https://archlinux.org/packages/extra/x86_64/ollama-rocm/':'https://archlinux.org/packages/extra/x86_64/ollama/','Package details and dependencies for Arch-based installations.');
      if(amd)drivers+=resourceLink('Arch AMD Vulkan driver','https://archlinux.org/packages/extra/x86_64/vulkan-radeon/','Mesa Vulkan driver package for a Vulkan-based model server.');
    }
    const distro=/debian|ubuntu/.test(family)?['Debian / Ubuntu Docker installation','https://docs.docker.com/engine/install/']: /fedora|rhel/.test(family)?['Docker on Fedora','https://docs.docker.com/engine/install/fedora/']: /suse/.test(family)?['openSUSE containers documentation','https://doc.opensuse.org/']:['Docker on Linux','https://docs.docker.com/engine/install/'];
    drivers+=resourceLink(...distro,'Optional container installation; host GPU drivers and device access are still required.');
    drivers+=resourceLink('Ollama in Docker','https://docs.ollama.com/docker','Container setup for CPU, NVIDIA, AMD ROCm and Vulkan.');
  }
  return `<p>Install a server and download a model using its own instructions. Check the current hardware requirements before installing.</p>
    <ul class="resource-links">${links}</ul>
    ${drivers?`<details data-node-details="${esc(node.id)}-drivers"><summary>Drivers and OS installation links</summary><ul class="resource-links">${drivers}</ul></details>`:''}
    ${!mac?`<details data-node-details="${esc(node.id)}-advanced"><summary>More Linux serving options</summary><ul class="resource-links">${resourceLink('vLLM','https://docs.vllm.ai/en/latest/getting_started/installation/','Serving for concurrent workloads; check its GPU and operating-system support.')}${resourceLink('SGLang','https://docs.sglang.io/docs/get-started/install','Model serving with hardware-specific installation options.')}</ul></details>`:''}
    <h4>Connect to Poundlock</h4><ol>
      <li>Start the server, load a model, and enable its OpenAI-compatible API.</li>
      <li>Choose a server address the Poundlock controller can reach. A localhost address refers to the controller itself. Use the server’s authentication and network-access instructions.</li>
      <li>Choose <strong>Connect a model server</strong>, save its API URL ending in <code>/v1</code>, and test the endpoint.</li>
      <li>Add a model on Models with this machine, its runtime and the server model ID. Poundlock suggests the model name clients send. Then use <strong>Generate</strong> to test inference.</li>
      <li>For unattended use, follow the server’s startup instructions and check it after reboot or logout. Test wake from sleep if needed.</li>
    </ol><p class="hint">The default always-ready runtime keeps this machine awake during work and uses the server you started. Starting or stopping model software requires explicit local runtime configuration.</p>`;
}
$('node-list').addEventListener('click',event=>{
  const connect=event.target.closest('[data-connect-node]');
  if(connect){
    location.hash='models';syncNavigation();
    switchModelsView('ready');
    openModelDrawer(null,{nodeId:connect.dataset.connectNode});
  }
  const download=event.target.closest('[data-download-node]');
  if(download){
    const node=state.data.nodes.find(item=>item.id===download.dataset.downloadNode);if(!node)return;
    const report={name:node.name,platform:node.platform,status:node.status,last_seen_at:node.last_seen_at,capabilities:node.capabilities,wake:{mac:node.wol_mac,broadcast:node.wol_broadcast}};
    const link=document.createElement('a'),url=URL.createObjectURL(new Blob([JSON.stringify(report,null,2)],{type:'application/json'}));
    link.href=url;link.download='poundlock-node-report.json';link.click();setTimeout(()=>URL.revokeObjectURL(url),1000);
  }
});

function enrollmentNetwork(){
  if($("enroll-network").value==="system")return '{ mode = "system" }';
  const input=$("enroll-interface");
  if(!input.reportValidity())throw new Error("Choose the node's network interface before enrollment.");
  return `{ mode = "interface", name = ${JSON.stringify(input.value)} }`;
}

$("enroll-network").addEventListener("change",()=>{
  const selected=$("enroll-network").value==="interface";
  $("enroll-interface-label").hidden=!selected;
  $("enroll-interface").disabled=!selected;
  $("enroll-interface").required=selected;
});

function installCommand(platform,sha256){
  if(platform==="macos"){
    return `# In the folder with node.toml: enroll once. Paste the token when asked; it is not shown.
~/.local/share/poundlock/bin/poundlock node-enroll --config ./node.toml --credential-file ~/.local/share/poundlock/node-credential.json
# Then start the node.
~/.local/share/poundlock/bin/poundlock node --config ./node.toml --credential-file ~/.local/share/poundlock/node-credential.json --data-dir ~/.local/share/poundlock`;
  }
  return `# Run in the folder containing the three downloaded files.
sudo bash ./poundlock-node.sh install --binary ./poundlock-linux-${$("enroll-arch").value} --sha256 ${sha256} --config ./node.toml
# Paste the token above when the installer prompts. Input is hidden.`;
}

$("enroll-form").addEventListener("submit",async event=>{
  event.preventDefault();
  const snapshot=auth();
  const name=$("enroll-name").value.trim();
  const platform=$("enroll-platform").value;
  try{
    enrollmentNetwork();
    let sha256;
    if(platform==="linux"){
      const binaries=await api("/api/v1/nodes/binaries");
      if(!live(snapshot))throw staleError();
      const binary=binaries.platforms.find(item=>item.os==="linux"&&item.arch===$("enroll-arch").value&&item.available);
      if(!binary)throw new Error("No Linux binary is published for that processor yet.");
      sha256=binary.sha256;
    }
    if(!live(snapshot))throw staleError();
    if(!name)throw new Error("Enter the machine name before creating its token.");
    const value=await api("/api/v1/nodes/enrollment-tokens",{method:"POST",body:JSON.stringify({valid_minutes:15,node_name:name})});
    if(!live(snapshot))throw staleError();
    showReveal("enroll-issued",value.token);
    $("enroll-command").textContent=installCommand(platform,sha256);
    toast("Enrollment token created");
  }catch(error){fail(error)}
});
$("copy-enroll-token").onclick=()=>copyReveal();
$("copied-enroll").onclick=()=>{clearReveal();toast("Token cleared from this page")};
$("download-toml").onclick=async()=>{
  try{
    const network=enrollmentNetwork();
    const body=await api("/api/v1/nodes/config-example");
    const name=$("enroll-name").value.trim();
    if(!name)throw new Error("Enter the machine name before downloading its configuration.");
    const blob=new Blob([`controller = ${JSON.stringify(location.origin)}\nname = ${JSON.stringify(name)}\n`+body.toml.replace(/^network = .*$/m,`network = ${network}`)],{type:"text/plain"});
    const link=document.createElement("a");
    link.href=URL.createObjectURL(blob);
    link.download=body.name||"node.toml";
    link.click();
    URL.revokeObjectURL(link.href);
    toast("Downloaded node.toml");
  }catch(error){fail(error)}
};
$("download-binary").onclick=async()=>{
  const snapshot=auth();
  const platform=$("enroll-platform").value;
  const os=platform==="macos"?"macos":"linux";
  const arch=$("enroll-arch").value;
  $("download-binary").disabled=true;clearUpgradeCommand();
  try{
    const {response}=await authorized(`/api/v1/nodes/binaries/${os}/${arch}`);
    if(!response.ok){
      const body=await response.json().catch(()=>({error:{message:`Download failed (${response.status})`}}));
      if(!live(snapshot))throw staleError();
      throw new Error(body.error?.message||`Download failed (${response.status})`);
    }
    const blob=await response.blob();
    if(!live(snapshot))throw staleError();
    const expected=response.headers.get("x-poundlock-sha256");
    if(!/^[a-f0-9]{64}$/.test(expected||''))throw new Error("Binary checksum is unavailable. Download again.");
    if(crypto.subtle){
      const hash=Array.from(new Uint8Array(await crypto.subtle.digest("SHA-256",await blob.arrayBuffer())),byte=>byte.toString(16).padStart(2,"0")).join("");
      if(!live(snapshot))throw staleError();
      if(hash!==expected)throw new Error("Binary download failed its SHA-256 check. Download again.");
    }
    const link=document.createElement("a");
    link.href=URL.createObjectURL(blob);
    link.download=`poundlock-${os}-${arch}`;
    link.click();
    URL.revokeObjectURL(link.href);
    if(os==='linux'&&$("enroll-platform").value===platform&&$("enroll-arch").value===arch){
      $("node-upgrade-command").textContent=`sudo bash ./poundlock-node.sh upgrade --binary ./poundlock-linux-${arch} --sha256 ${expected}`;
      $("node-upgrade").hidden=false;
    }
    toast("Downloaded node binary");
  }catch(error){fail(error)}finally{$("download-binary").disabled=false}
};
function clearUpgradeCommand(){$("node-upgrade").hidden=true;$("node-upgrade-command").textContent=''}
for(const id of ['enroll-platform','enroll-arch'])$(id).addEventListener('change',clearUpgradeCommand);
$("download-installer").onclick=async()=>{
  const snapshot=auth();
  try{
    const {response}=await authorized("/api/v1/nodes/linux-installer");
    if(!response.ok)throw new Error("Installer download failed. Sign in and try again.");
    const blob=await response.blob();
    if(!live(snapshot))throw staleError();
    const link=document.createElement("a");link.href=URL.createObjectURL(blob);link.download="poundlock-node.sh";link.click();URL.revokeObjectURL(link.href);
  }catch(error){fail(error)}
};

function enrollmentPlatform(){
  const linux=$("enroll-platform").value==="linux";
  $("download-installer").hidden=!linux;
  $("enroll-download-title").textContent=linux?"2. Download to the Linux computer":"2. Download to the Mac";
  $("enroll-download-hint").textContent=linux?"Save all three files together in any folder you own, such as ~/Downloads/poundlock. If downloading on another computer, copy them to Linux first. The installer takes care of permanent locations; keep the installer for later maintenance.":"Save node.toml in your working folder and place the native binary at ~/.local/share/poundlock/bin/poundlock on the Mac. Make the binary executable.";
  $("enroll-install-hint").textContent=linux?"Open a terminal in that folder (for example, cd ~/Downloads/poundlock). Create a token when ready, then run the command shown below. The installer prompts for your token and installs the service.":"Create a token, then run the displayed commands from the folder containing node.toml. Enrollment asks for the token without showing it, so it stays out of your shell history.";
  clearReveal();
}
$("enroll-platform").addEventListener("change",enrollmentPlatform);
for(const id of ["enroll-name","enroll-arch","enroll-network","enroll-interface"])$(id).addEventListener("input",clearReveal);
function loadWakeSettings(){
  const id=$("wol-node").value,node=state.data.nodes.find(n=>n.id===id&&n.status!=="revoked");
  const stamp=JSON.stringify([node?.wol_mac,node?.wol_broadcast,node?.capabilities?.network]);
  if(wakeSelected===id&&(wakeDirty||wakeStamp===stamp))return;
  const changed=wakeSelected!==id;
  wakeSelected=id;wakeStamp=stamp;wakeDirty=false;
  const interfaces=nodeInterfaces(node).filter(item=>validWakeMac(item.mac));
  $("wake-interface").innerHTML='<option value="">Choose an interface</option>'+interfaces.map(item=>`<option value="${esc(item.name)}">${esc(item.name)} · ${esc(item.mac)}${item.kind?` · ${esc(item.kind)}`:""}</option>`).join('')+'<option value="manual">Enter manually</option>';
  const saved=interfaces.find(item=>item.mac.toLowerCase()===node?.wol_mac?.toLowerCase());
  $("wake-interface").value=saved?.name||(node?.wol_mac?'manual':interfaces.length===1&&interfaces[0].kind!=='virtual'?interfaces[0].name:'');
  updateWakeInterface();
  if(node?.wol_mac){$("wol-mac").value=node.wol_mac;$("wol-broadcast").value=node.wol_broadcast||"255.255.255.255:9"}
  $("test-wake").disabled=!node?.wol_mac;
  $("save-wake").disabled=!node;
  $("wake-settings-note").textContent=node?.wol_mac?`Saved wake address: ${node.wol_mac}. Test from sleep; waits up to 60 seconds for the node.`:node?'Choose a wake interface and save its settings first.':'Enroll a machine to configure wake.';
  if(changed)$("wake-settings").open=!!node&&!node.wol_mac;
  $("wake-result").textContent="";
}
function nodeInterfaces(node){
  const network=node?.capabilities?.network;
  return Array.isArray(network?.interfaces)?network.interfaces:network?.interface?[{name:network.interface,mac:network.mac,addresses:network.addresses,connected:network.status==='connected'}]:[];
}
function validWakeMac(mac){return typeof mac==='string'&&/^[0-9a-f]{2}(:[0-9a-f]{2}){5}$/i.test(mac)&&!(parseInt(mac.slice(0,2),16)&1)&&mac!=='00:00:00:00:00:00'}
function updateWakeInterface(){
  const node=state.data.nodes.find(n=>n.id===$("wol-node").value),choice=$("wake-interface").value;
  const item=nodeInterfaces(node).find(item=>item.name===choice);
  $("wol-mac").readOnly=choice!=='manual';
  $("wol-mac").value=item?.mac||'';
  $("wol-broadcast").value=item?.broadcasts?.length===1?`${item.broadcasts[0]}:9`:'255.255.255.255:9';
  $("wake-interface-note").textContent=item?`Reported by ${item.name}${item.addresses?.length?` · ${item.addresses.join(', ')}`:''}. Wake support needs a test from sleep.`:choice==='manual'?'Enter the address of the adapter that will receive wake requests.':'Select a reported adapter, or enter its address manually.';
}
$("wol-node").addEventListener("change",()=>{wakeDirty=false;loadWakeSettings()});
$("wake-interface").addEventListener("change",()=>{wakeDirty=true;updateWakeInterface()});
$("wol-form").addEventListener("input",()=>{wakeDirty=true});
$("wol-form").addEventListener("submit",event=>{event.preventDefault();wakeAction(false)});
$("test-wake").addEventListener("click",()=>wakeAction(true));
async function wakeAction(test){
  if(wakeBusy||!$("wol-node").reportValidity())return;
  const snapshot=auth();
  const id=$("wol-node").value;
  const controls=[...$("wake-panel").querySelectorAll("button,input,select")];controls.forEach(control=>control.disabled=true);wakeBusy=true;
  const resultText=$("wake-result");resultText.textContent=test?"Waiting for the node to respond…":"Saving settings…";
  try{
    if(test){
      const result=await api(`/api/v1/nodes/${encodeURIComponent(id)}/wake-test`,{method:"POST",body:JSON.stringify({timeout_seconds:60})});
      if(!live(snapshot))throw staleError();
      resultText.textContent=result.was_online?"Machine was already online; fresh heartbeat confirmed. Test from sleep to check wake support.":result.awake?"Machine responded after the wake request. Use Test on Models to check inference.":"Wake test finished";
    }else{
      await api(`/api/v1/nodes/${encodeURIComponent(id)}/wol`,{method:"PUT",body:JSON.stringify({mac:$("wol-mac").value,broadcast:$("wol-broadcast").value})});
      if(!live(snapshot))throw staleError();
      wakeDirty=false;wakeStamp=null;resultText.textContent="Wake settings saved.";
    }
    await refresh({keepReveal:true});
  }catch(error){if(!error.stale){resultText.textContent=error.message;toast(error.message,true)}}
  finally{
    wakeBusy=false;controls.forEach(control=>control.disabled=false);
    if(!live(snapshot))return;
    const result=resultText.textContent;loadWakeSettings();resultText.textContent=result;
    $("test-wake").disabled=!state.data.nodes.find(node=>node.id===$("wol-node").value&&node.status!=="revoked")?.wol_mac;
  }
}

if(state.token)refresh().catch(()=>{});


function modelMemoryPanel(node,models){
  const supported=(node.capabilities?.runtimes||[]).includes('ollama');
  const memory=node.capabilities?.model_memory;
  const managed=models.filter(model=>model.runtime_profile==='ollama');
  const rows=memory?.models||[];
  const observed=node.status==='online'&&memory?.status==='observed';
  return `<details data-node-details="${esc(node.id)}-memory"><summary>Model memory</summary>
    ${supported?`<p class="hint">Ollama models stay available during active work. Choose when to release memory after work finishes.</p>
    ${managed.map(model=>`<form data-memory-form="${esc(model.id)}"><label>${esc(model.name)} — unload<select name="idle"><option value="0"${!(model.idle_seconds>0)?' selected':''}>After work finishes</option><option value="60"${model.idle_seconds===60?' selected':''}>After 1 minute</option><option value="300"${model.idle_seconds===300?' selected':''}>After 5 minutes</option><option value="1200"${model.idle_seconds===1200?' selected':''}>After 20 minutes</option>${model.idle_seconds>0&&![60,300,1200].includes(model.idle_seconds)?`<option value="${esc(String(model.idle_seconds))}" selected>After ${esc(String(model.idle_seconds))} seconds</option>`:''}</select></label><button type="submit" class="secondary">Save memory setting</button></form>`).join('')}
    ${!managed.length?'<p>No managed Ollama profile configured. Choose Ollama in the Models drawer when connecting your model.</p>':''}`:'<p>Ollama is off on this machine. To manage it here, add <code>[ollama]</code> with <code>enabled = true</code> to its node.toml and restart the node. Other runtimes use their configured lifecycle manager.</p>'}
    <p class="hint">${observed?(rows.some(m=>m.loaded)?'Models currently loaded:':'No models loaded in Ollama.'):'Current model memory is not confirmed. Last reported values follow.'}${memory?.checked_at?` Checked ${esc(new Date(memory.checked_at).toLocaleString())}.`:''}</p>
    ${rows.map(m=>`<p><strong>${esc(m.model)}</strong> · ${m.loaded?'Loaded':'Not loaded'}${m.loaded?` · ${esc(reportBytes(m.gpu_bytes))} GPU memory`:''} · ${m.managed?(m.active_leases?'Work in progress':'Idle'):'Loaded outside Poundlock'}${m.error?`<br><span role="status">Cleanup needs attention: ${esc(m.error)}</span>`:''}</p>`).join('')}
    ${memory?.status==='unavailable'?'<p role="status">Ollama is unavailable. Check that its service is running on this machine.</p>':''}
    <p class="hint">Saved changes apply to the next request or job. Models loaded directly by another application remain under that application's control.</p>
  </details>`;
}
$('node-list').addEventListener('submit',async event=>{
  const f=event.target.closest('[data-memory-form]');if(!f)return;event.preventDefault();
  const button=f.querySelector('button');button.disabled=true;
  try{await api(`/api/v1/model-profiles/${encodeURIComponent(f.dataset.memoryForm)}`,{method:'PATCH',body:JSON.stringify({idle_seconds:Number(f.elements.idle.value)})});delete f.dataset.dirty;toast('Memory setting saved');await refresh()}catch(error){fail(error)}finally{button.disabled=false}
});

$('node-list').addEventListener('click',async event=>{
  const button=event.target.closest('[data-pause-node]');if(!button)return;
  const node=state.data.nodes.find(n=>n.id===button.dataset.pauseNode);if(!node)return;
  button.disabled=true;
  try{await api(`/api/v1/nodes/${encodeURIComponent(node.id)}/maintenance`,{method:'POST',body:JSON.stringify({paused:node.status!=='maintenance'})});toast(node.status==='maintenance'?'Inference resumed':'Inference paused');await refresh()}catch(error){fail(error)}finally{button.disabled=false}
});

$('node-list').addEventListener('change',event=>{const f=event.target.closest('[data-memory-form]');if(f)f.dataset.dirty='true'});
