// Models tab: Ready (published profiles) and Discover (gateway routes and the
// OpenRouter catalog), plus the model-profile editor drawer.
const PRICING_SOURCE_LABELS={litellm_route:"LiteLLM route",litellm_cost_map:"LiteLLM cost map",openrouter_catalog:"OpenRouter catalog",local:"Local",operator:"Operator"};
function pricingLine(pricing){
  if(!pricing)return "";
  const fmt=v=>`$${Number(v).toFixed(2)}`;
  return `${fmt(pricing.input_per_mtok)} / ${fmt(pricing.output_per_mtok)} per M tokens · source: ${PRICING_SOURCE_LABELS[pricing.source]||pricing.source}`;
}
const USE_LABELS={durable:"Agent jobs",interactive:"Interactive requests",completion:"Autocomplete (FIM)","durable-coding-agent":"Agent jobs","interactive-coding":"Interactive requests","fast-code-completion":"Autocomplete (FIM)"};
const lastVerifyResults=new Map();

// ------------------------------------------------------------------- Ready

function groupKey(profile){return profile.node_id?`node:${profile.node_id}`:`endpoint:${profile.endpoint_id}`}
function groupLabel(profile,d){
  if(profile.node_id){const node=d.nodes.find(n=>n.id===profile.node_id);return node?node.name:"Unknown machine"}
  const endpoint=d.endpoints.find(e=>e.id===profile.endpoint_id);
  return endpoint?endpoint.name:"Unknown endpoint";
}
function isLocalOnLiteLLM(profile,d){
  const endpoint=d.endpoints.find(e=>e.id===profile.endpoint_id);
  return !profile.node_id&&endpoint?.kind==="litellm"&&profile.data_destination==="operator_network";
}
function disabledStatusHtml(disabled){
  if(!disabled)return badge("ok","Enabled");
  // Retirement is a deliberate stop, not a failure: the ended treatment, not
  // the fault colour. "Disabled" stays visible for the existing UAT text
  // dependency; "Taken out of service" is the honest-states wording.
  const lastEvidence=disabled.evidence?.length?disabled.evidence[disabled.evidence.length-1]:null;
  return `${badge("ended","Disabled")}<div class="meta"><span>Taken out of service</span><span>${esc(disabled.reason)}</span><span>Source: ${esc(disabled.source)}</span><span>Since ${new Date(disabled.since).toLocaleString()}</span>${lastEvidence?`<span>Last check ${new Date(lastEvidence.at).toLocaleString()}</span>`:""}</div>`;
}
function verifyResultsBlock(id){
  const results=lastVerifyResults.get(id);
  if(!results)return "";
  return `<details open class="verify-results"><summary>Verify reasoning results</summary>${results.map(r=>`<div class="meta"><span>${esc(r.level)}</span><span>${r.verified?"Verified":"Not verified"}</span><span>${esc(r.note)}</span>${r.reasoning_tokens!=null?`<span>${esc(String(r.reasoning_tokens))} reasoning tokens</span>`:""}</div>`).join("")}</details>`;
}
function readyRow(profile,d){
  const endpoint=d.endpoints.find(e=>e.id===profile.endpoint_id);
  const disabled=profile.retirement?.disabled;
  const local=isLocalOnLiteLLM(profile,d)?' <span class="badge">Local</span>':'';
  const uses=(profile.uses&&profile.uses.length?profile.uses:[profile.workload_class]).map(u=>USE_LABELS[u]||u).join(", ");
  const served=profile.catalog?.served_model;
  const reasoning=profile.catalog?.reasoning_levels?.length?`${profile.catalog.reasoning_levels.join(", ")}${profile.catalog.reasoning_verified_at?` (verified ${new Date(profile.catalog.reasoning_verified_at).toLocaleDateString()})`:""}`:"model default only";
  const pricing=pricingLine(profile.catalog?.pricing);
  const gateway=endpoint?.kind==="litellm";
  const managed=Boolean(endpoint?.managed_identity?.enabled);
  const indicator=disabled?"ended":profile.last_check_ok===true?"ok":profile.last_check_ok===false?"fault":"unknown";
  return `<div class="item" data-profile="${esc(profile.id)}">
    ${rowIndicator(indicator)}<strong>${esc(profile.name)}${local}</strong>
    <div class="meta">${disabledStatusHtml(disabled)}</div>
    <div class="meta model-names">${modelNamesHtml(profile)}</div>
    <div class="meta"><span>Last health check: ${checkLabel(profile)}</span></div>
    ${served?`<div class="meta"><span>Served model: ${esc(served)}</span></div>`:""}
    <div class="meta"><span>${esc(String(profile.context_tokens))} context tokens</span><span>Uses: ${esc(uses)}</span><span>Thinking: ${profile.thinking==="off"?"Off":"Model default"}</span></div>
    <div class="meta">${PROMPTS_GO[profile.data_destination]?`<span>Prompts go ${esc(PROMPTS_GO[profile.data_destination])}</span>`:'<span class="check-fail">Where prompts go is not declared, so clients cannot use it</span>'}</div>
    <div class="meta"><span>Reasoning: ${esc(reasoning)}</span></div>
    ${pricing?`<div class="meta"><span>${esc(pricing)}</span></div>`:""}
    ${verifyResultsBlock(profile.id)}
    <div class="item-actions">
      <button class="secondary" data-edit-model="${esc(profile.id)}">Edit</button>
      <button class="secondary" data-test-model="${esc(profile.id)}">Test</button>
      ${managed?'':`<button class="secondary" data-generate-model="${esc(profile.id)}">Generate</button>`}
      ${gateway?`<button class="secondary" data-sync-model="${esc(profile.id)}">Refresh from gateway</button> <button class="secondary" data-verify-model="${esc(profile.id)}">Verify reasoning</button>`:""}
      ${disabled?`<button class="secondary" data-enable-model="${esc(profile.id)}">Re-enable</button>`:""}
      <button class="secondary" data-remove-model="${esc(profile.id)}">Remove</button>
    </div>
  </div>`;
}
function renderReady(){
  const d=state.data,container=$("ready-groups");
  if(!d.models.length){emptyState(container,"No models added yet.","Add one above, or publish a gateway route from Discover.");return}
  container.className="";
  const groups=new Map();
  for(const profile of d.models){
    const key=groupKey(profile);
    if(!groups.has(key))groups.set(key,{label:groupLabel(profile,d),profiles:[]});
    groups.get(key).profiles.push(profile);
  }
  container.innerHTML=[...groups.values()].map(group=>`<div class="panel model-group-panel"><h4>${esc(group.label)}</h4><div class="list">${group.profiles.map(profile=>readyRow(profile,d)).join("")}</div></div>`).join("");
  container.querySelectorAll("[data-edit-model]").forEach(button=>button.onclick=()=>openModelDrawer(button.dataset.editModel));
  container.querySelectorAll("[data-test-model]").forEach(button=>button.onclick=()=>testResource(`/api/v1/model-profiles/${button.dataset.testModel}/test`,button,{generate:false}));
  container.querySelectorAll("[data-generate-model]").forEach(button=>button.onclick=()=>testResource(`/api/v1/model-profiles/${button.dataset.generateModel}/test`,button,{generate:true}));
  container.querySelectorAll("[data-sync-model]").forEach(button=>button.onclick=async()=>{
    await refreshGatewayProfile(button.dataset.syncModel,button);
  });
  container.querySelectorAll("[data-verify-model]").forEach(button=>button.onclick=async()=>{
    if(!confirm("Verify reasoning sends a few small paid requests to this model. Continue?"))return;
    const id=button.dataset.verifyModel;button.disabled=true;
    try{const result=await api(`/api/v1/model-profiles/${encodeURIComponent(id)}/verify-reasoning`,{method:"POST",body:"{}"});lastVerifyResults.set(id,result.results||[]);toast(`Verified: ${(result.verified||[]).join(", ")||"none"}`);await refresh()}
    catch(error){fail(error)}finally{button.disabled=false}
  });
  container.querySelectorAll("[data-enable-model]").forEach(button=>button.onclick=async()=>{
    button.disabled=true;
    try{await api(`/api/v1/model-profiles/${encodeURIComponent(button.dataset.enableModel)}/enable`,{method:"POST",body:"{}"});toast("Model profile re-enabled");await refresh()}catch(error){fail(error)}finally{button.disabled=false}
  });
  container.querySelectorAll("[data-remove-model]").forEach(button=>button.onclick=async()=>{
    const id=button.dataset.removeModel,item=d.models.find(x=>x.id===id);
    if(!confirm(`Remove ${item.name}? Existing jobs keep their saved settings and history.`))return;
    button.disabled=true;
    try{await api(`/api/v1/model-profiles/${encodeURIComponent(id)}`,{method:"DELETE"});if(editingModel===id)closeDrawer(true);lastVerifyResults.delete(id);await refresh();toast("Removed")}
    catch(error){fail(error);button.disabled=false}
  });
}

// ---------------------------------------------------------------- Discover

let discoveryData=null,discoveryLoaded=false;
// testResource() is defined in connections.js and reused here for model tests.
function catalogProviderOf(id){return (id||"").split("/")[0]||""}
// The operator-facing address when one is configured; the controller's own
// base URL is often an internal host a browser cannot reach.
function adminUiUrl(endpoint,route=null){
  const base=(endpoint.public_url||endpoint.base_url.replace(/\/v1\/?$/,"")).replace(/\/+$/,"").replace(/\/ui$/,"");
  return base+"/ui/models-and-endpoints/"+(route?`?model_group=${encodeURIComponent(route)}`:"");
}

function routeRow(endpointId,route){
  const published=route.state==="published";
  const changed=Boolean(route.profile?.model_changed);
  const stateBadge=changed?badge("review","Published profile needs refresh"):published?badge("ok","Published"):badge("review","Configured route — not published");
  const endpoint=state.data.endpoints.find(e=>e.id===endpointId);
  const price=route.pricing?pricingLine(route.pricing):"No price reported";
  return `<div class="item" data-route="${esc(route.name)}" data-endpoint="${esc(endpointId)}">
    ${rowIndicator(published?"ok":"review")}<strong class="mono">${esc(route.name)}</strong> ${stateBadge}
    <div class="meta"><span>Upstream: ${esc(route.upstream_model||"unknown")}</span><span>Provider: ${esc(route.provider||"unknown")}</span></div>
    <div class="meta"><span>${esc(price)}</span></div>
    ${changed?`<p class="check-fail">Poundlock still records ${esc(route.profile.served_model||"an unknown model")}. Refresh before using its old reasoning verification; review the profile label and limits separately.</p>`:""}
    <div class="item-actions">
      ${endpoint?`<a class="secondary button-link" href="${esc(adminUiUrl(endpoint,route.name))}" target="_blank" rel="noopener">Edit route in LiteLLM</a>`:""}
      <button class="secondary" data-route-test="${esc(route.name)}" data-route-endpoint="${esc(endpointId)}">Test</button>
      ${published?`<button class="secondary" data-route-sync="${esc(route.profile.id)}">Refresh profile from gateway</button>`:""}
      ${published?"":`<button class="secondary" data-route-publish="${esc(route.name)}" data-route-endpoint="${esc(endpointId)}">Publish</button>`}
    </div>
    <div class="route-test-result meta"></div>
  </div>`;
}
function wireDiscoverGatewayActions(box){
  box.querySelectorAll("[data-route-sync]").forEach(button=>button.onclick=()=>refreshGatewayProfile(button.dataset.routeSync,button));
  box.querySelectorAll("[data-route-test]").forEach(button=>button.onclick=async()=>{
    const item=button.closest(".item"),resultBox=item.querySelector(".route-test-result");
    button.disabled=true;resultBox.textContent="Testing…";resultBox.className="route-test-result meta";
    try{
      const result=await api("/api/v1/discovery/test",{method:"POST",body:JSON.stringify({endpoint_id:button.dataset.routeEndpoint,route:button.dataset.routeTest})});
      resultBox.textContent=result.healthy?"Healthy":`Unhealthy${result.error?": "+result.error:""}`;
      resultBox.className="route-test-result meta "+(result.healthy?"check-ok":"check-fail");
    }catch(error){resultBox.textContent=error.message;resultBox.className="route-test-result meta check-fail"}
    finally{button.disabled=false}
  });
  // Publishing a route is adding a model on the gateway: the same review.
  box.querySelectorAll("[data-route-publish]").forEach(button=>button.onclick=()=>{
    const gateway=(discoveryData?.gateways||[]).find(item=>item.endpoint_id===button.dataset.routeEndpoint);
    const route=gateway?.routes.find(item=>item.name===button.dataset.routePublish);
    openModelDrawer(null,{endpointId:button.dataset.routeEndpoint,model:{id:button.dataset.routePublish,runtime:null,context:route?.context_tokens||null,label:null}});
  });
}
function renderDiscoverGateways(){
  const box=$("discover-gateways"),gateways=discoveryData?.gateways||[];
  if(!gateways.length){emptyState(box,"No LiteLLM gateways configured.","Add a Gateway (LiteLLM) endpoint on Connections to see its routes here.");return}
  box.className="";
  box.innerHTML=gateways.map(gateway=>`<div class="panel discover-gateway"><h4>${esc(gateway.endpoint_name)}</h4>${gateway.error?`<p class="check-fail">${esc(gateway.error)}</p>`:""}<div class="list ${gateway.routes.length?"":"empty"}">${gateway.routes.length?gateway.routes.map(route=>routeRow(gateway.endpoint_id,route)).join(""):"No routes reported."}</div></div>`).join("");
  wireDiscoverGatewayActions(box);
}
function renderDiscoverProviderOptions(){
  const select=$("discover-provider"),models=discoveryData?.catalog?.models||[];
  const providers=[...new Set(models.map(m=>catalogProviderOf(m.id)).filter(Boolean))].sort();
  const current=select.value;
  select.innerHTML='<option value="">All providers</option>'+providers.map(p=>`<option value="${esc(p)}">${esc(p)}</option>`).join("");
  if(providers.includes(current))select.value=current;
}
function renderDiscoverCatalog(){
  if(!discoveryData)return;
  const catalog=discoveryData.catalog;
  $("discover-catalog-note").textContent=catalog?.note||"";
  const box=$("discover-catalog");
  const button=$("discover-catalog-load");
  button.hidden=catalog?.mode==="never";
  if(catalog?.mode==="never"){box.className="list empty";box.textContent="Turned off on this controller (POUNDLOCK_OPENROUTER_CATALOG=never).";return}
  if(!catalog?.requested){box.className="list empty";box.textContent="Not looked up.";return}
  if(catalog?.error){box.className="list empty";box.textContent=catalog.error;return}
  let models=catalog?.models||[];
  const search=$("discover-search").value.trim().toLowerCase();
  const provider=$("discover-provider").value;
  const unroutedOnly=$("discover-unrouted").checked;
  if(search)models=models.filter(m=>(m.id||"").toLowerCase().includes(search)||(m.name||"").toLowerCase().includes(search));
  if(provider)models=models.filter(m=>catalogProviderOf(m.id)===provider);
  if(unroutedOnly)models=models.filter(m=>m.state==="catalog_listing");
  if(!models.length){emptyState(box,"No matching models.","Adjust the search, provider or routed filter.");return}
  box.className="";
  const gateways=(state.data.endpoints||[]).filter(e=>e.kind==="litellm");
  box.innerHTML=models.map(model=>{
    const routed=model.state==="configured_route";
    // Letter badge (see the legend above the list): C = configured route,
    // L = listed by OpenRouter only, not yet routed.
    const letter=`<span class="letter-badge" title="${routed?"Configured route":"Listed by OpenRouter, not yet routed"}">${routed?"C":"L"}</span>`;
    const price=model.pricing?pricingLine(model.pricing):"No price reported";
    const efforts=(model.reasoning_efforts||[]).map(effort=>`<span class="pill">${esc(effort)}</span>`).join("");
    const action=routed?"":gateways.length?`<button class="secondary" data-route-handoff="${esc(model.id)}">Add route in LiteLLM</button>`:'<span class="hint">Configure a LiteLLM gateway to add a route.</span>';
    return `<div class="item">${rowIndicator(routed?"ok":"unknown")}${letter}<strong class="mono">${esc(model.id)}</strong><span>${esc(model.name||model.id)}</span><div class="meta"><span class="mono">${esc(price)}</span>${efforts}</div><div class="item-actions">${action}</div></div>`;
  }).join("");
  box.querySelectorAll("[data-route-handoff]").forEach(button=>button.onclick=()=>showRouteHandoff(button.dataset.routeHandoff));
}
function renderDiscovery(){renderDiscoverGateways();renderDiscoverProviderOptions();renderDiscoverCatalog()}
let catalogWanted=false;
async function loadDiscovery(){
  const snapshot=auth();
  $("discover-gateways").className="list empty";$("discover-gateways").textContent="Loading…";
  try{
    const data=await api(catalogWanted?"/api/v1/discovery?catalog=openrouter":"/api/v1/discovery");
    if(!live(snapshot))return;
    discoveryData=data;discoveryLoaded=true;
    renderDiscovery();
    return true;
  }catch(error){if(live(snapshot)){$("discover-gateways").className="list empty";$("discover-gateways").textContent=error.message;fail(error)}return false}
}
$("discover-refresh").onclick=()=>loadDiscovery();
$("discover-catalog-load").onclick=()=>{catalogWanted=true;loadDiscovery()};
$("discover-search").addEventListener("input",renderDiscoverCatalog);
$("discover-provider").addEventListener("change",renderDiscoverCatalog);
$("discover-unrouted").addEventListener("change",renderDiscoverCatalog);

function switchModelsView(view){
  document.querySelectorAll(".seg-btn").forEach(button=>{
    const active=button.dataset.modelView===view;
    button.classList.toggle("active",active);
    button.setAttribute("aria-selected",String(active));
  });
  $("models-ready").classList.toggle("active",view==="ready");
  $("models-discover").classList.toggle("active",view==="discover");
  if(view==="discover"&&!discoveryLoaded)loadDiscovery();
}
document.querySelectorAll(".seg-btn").forEach(button=>button.onclick=()=>switchModelsView(button.dataset.modelView));

// ------------------------------------------------------------------ shared

function renderModels(){
  const d=state.data;
  updateOptions("model-endpoint",'<option value="">Choose a server</option>'+options(d.endpoints,x=>`${x.name} · ${x.kind==="litellm"?"Gateway":"Model server"}`));
  updateOptions("model-node",'<option value="">None: the server is always running</option>'+options(d.nodes.filter(node=>node.status!=="revoked"),x=>`${x.name} · ${x.status==="online"?"Online":"Sleeping or offline"}`));
  syncRuntimeOptions(true);
  syncMachineFields();
  syncModelSettings();
  if(drawerOpen()&&(!setup||setup.step==="review"))renderDestination();
  renderReady();
}

// Both strings a model has, each with a copy button: the name clients send
// and the server's own ID for the weights behind it.
function modelNamesHtml(profile){
  return `<span>Clients use <code class="mono">${esc(profile.name)}</code></span><button type="button" class="secondary copy" data-copy="${esc(profile.name)}" aria-label="Copy the model name ${esc(profile.name)}">Copy</button><span>Server model ID <code class="mono">${esc(profile.model_id)}</code></span><button type="button" class="secondary copy" data-copy="${esc(profile.model_id)}" aria-label="Copy the server model ID ${esc(profile.model_id)}">Copy</button>`;
}
document.addEventListener("click",event=>{
  const button=event.target.closest("button[data-copy]");
  if(button)copyText(button.dataset.copy);
});

function nodeById(id){return (state.data?.nodes||[]).find(item=>item.id===id)}
function endpointById(id){return (state.data?.endpoints||[]).find(item=>item.id===id)}
function modelNode(){return nodeById($("model-node").value)}
function modelEndpoint(){return endpointById($("model-endpoint").value)}
function hasWakeOnLan(node){return Boolean(node?.wol_mac&&node?.wol_broadcast)}
function runtimeIdentity(node,runtime){return node?.capabilities?.inventory?.runtimes?.[runtime]||null}
function radioValue(name){return document.querySelector(`[name="${name}"]:checked`)?.value||""}
function setRadio(name,value){for(const input of document.querySelectorAll(`[name="${name}"]`))input.checked=input.value===value}
function tokens(value){return Number(value).toLocaleString("en-US")}

function syncRuntimeOptions(preserve=false){
  const node=modelNode(),select=$("model-runtime");
  const runtimes=node?.capabilities?.runtimes||[];
  if(!node){
    select.innerHTML='<option value="">Not required</option>';
    select.setCustomValidity("");
    select.disabled=true;
    return;
  }
  select.disabled=false;
  const html=runtimes.length?runtimes.map(name=>`<option value="${esc(name)}">${name==="ollama"?"Ollama — managed loading and unloading":esc(name)}</option>`).join(""):'<option value="">No runtime profiles reported by this machine</option>';
  if(preserve)updateOptions("model-runtime",html);else{select.innerHTML=html;select.setCustomValidity("")}
}

// A machine brings model management and "When a request arrives"; a server
// that is always running has neither, and Poundlock only forwards to it.
const WAKE_HINTS={
  never:()=>"Poundlock forwards at once and does not wait for the model. If the server is down, the client is told so.",
  manual:node=>`Waits while ${node?.name||"the machine"} starts the model. If it is asleep, the request fails at once.`,
  "on-demand":node=>hasWakeOnLan(node)?`Wakes ${node.name} with Wake-on-LAN, then waits for the model.`:`${node?.name||"This machine"} has no Wake-on-LAN settings yet. Add them on Machines, or requests fail while it sleeps.`,
};
function syncMachineFields(){
  const node=modelNode(),runtime=$("model-runtime").value;
  $("model-runtime-row").hidden=!node;
  $("model-wake-group").hidden=!node;
  const wake=radioValue("model-wake");
  $("wake-hint").textContent=node&&WAKE_HINTS[wake]?WAKE_HINTS[wake](node):"";
  const hint=$("model-runtime-hint");
  hint.hidden=!node;
  hint.textContent=runtime==="ollama"?"Ollama loads the model when a request arrives. The server must be this machine's Ollama.":node&&wake==="never"&&runtimeIdentity(node,runtime)?.servable?`${runtime} loads models. Unless the model stays loaded, choose "Start the model" so requests wait for it.`:"";
}
// The default for a machine: wake it when Wake-on-LAN is set up, otherwise
// start the model only while it is on. The draft check says the same.
function defaultWake(node){return node?(hasWakeOnLan(node)?"on-demand":"manual"):"never"}
function syncModelSettings(){
  const uses=radioValue("model-use").split(",");
  $("model-context").min=uses.includes("durable")?8192:1024;
  const managed=$("model-runtime").value==="ollama";$("model-idle-row").hidden=!managed;$("model-idle").disabled=!managed;
  syncAliasOption();
}
function syncAliasOption(){
  const id=$("model-id").value.trim(),name=$("model-name").value.trim(),box=$("model-alias-server-id");
  const owner=id&&(state.data?.models||[]).find(item=>item.id!==editingModel&&(item.name===id||(item.aliases||[]).includes(id)));
  box.disabled=!id||id===name||Boolean(owner);
  if(box.disabled&&!(editingModel&&box.checked&&!owner))box.checked=false;
  $("model-alias-hint").textContent=owner?`Not available: ${owner.name} already uses ${id}.`:id===name&&id?"The model name already is the server model ID.":"For a client set up with the server's own ID. It is not listed in /v1/models, and no other model can use it.";
}

// Server model IDs the operator can pick from: what the machine's runtime
// reports it can serve, else what the server lists when it is chosen.
let fetchedIds={endpoint:null,ids:[]};
function renderModelIdOptions(){
  const node=modelNode(),runtime=$("model-runtime").value,identity=runtimeIdentity(node,runtime);
  const reported=Array.isArray(identity?.servable)?identity.servable:[];
  const ids=reported.length?reported:fetchedIds.endpoint===$("model-endpoint").value?fetchedIds.ids:[];
  $("model-id-options").innerHTML=ids.map(id=>`<option value="${esc(id)}"></option>`).join("");
  if(reported.length)$("model-ids-note").textContent=`${node.name}'s ${runtime} can serve ${reported.join(", ")}. Clients never send this ID.`;
}
// The server's own list, asked for as soon as a server is chosen. Listing
// records a connection check on the server and publishes nothing.
async function listServerModels(endpointId){
  const result=await api(`/api/v1/endpoints/${encodeURIComponent(endpointId)}/test`,{method:"POST",body:"{}"});
  return [...new Set((result.model_ids||[]).filter(value=>typeof value==="string"))];
}
async function fetchModelIds(){
  const snapshot=auth(),id=$("model-endpoint").value;
  if(!id){$("model-ids-note").textContent="Choose a server first.";return}
  if(fetchedIds.endpoint===id)return renderModelIdOptions();
  $("list-model-ids").hidden=true;
  $("model-ids-note").textContent="Asking the server which models it has…";
  try{
    const ids=await listServerModels(id);
    if(!live(snapshot)||$("model-endpoint").value!==id)return;
    fetchedIds={endpoint:id,ids};
    $("model-ids-note").textContent=ids.length?`The server lists ${ids.length} model${ids.length===1?"":"s"}. Clients never send this ID.`:"The server lists no models. Load one there, then try again.";
    $("list-model-ids").hidden=Boolean(ids.length);
    renderModelIdOptions();
  }catch(error){
    if(error.stale||!live(snapshot)||$("model-endpoint").value!==id)return;
    $("model-ids-note").textContent=`The server did not list its models: ${error.message}. Type the ID, or check the server and try again.`;
    $("list-model-ids").hidden=false;
  }
}

// Where prompts go is declared, never derived. A model server declares it
// once for all its models; each gateway route declares its own.
const PROMPTS_GO={operator_network:"within my network",external:"to an external service"};
function destinationMode(){
  const endpoint=modelEndpoint();
  if(!endpoint||endpoint.kind==="litellm")return "route";
  const saved=editingModel?state.data.models.find(item=>item.id===editingModel):null;
  if(saved?.data_destination_source==="profile"&&saved.endpoint_id===endpoint.id)return "own";
  return PROMPTS_GO[endpoint.data_destination]?"server":"declare-server";
}
function renderDestination(){
  const endpoint=modelEndpoint(),mode=destinationMode(),group=$("model-destination-group"),inherited=$("model-destination-inherited");
  group.dataset.mode=mode;
  group.hidden=mode==="server";
  inherited.hidden=mode!=="server";
  if(mode==="server"){
    inherited.innerHTML=`Prompts go ${esc(PROMPTS_GO[endpoint.data_destination])}, as declared for ${esc(endpoint.name)} and every model on it. <button type="button" class="secondary copy" data-edit-server="${esc(endpoint.id)}">Change on Connections</button>`;
    return;
  }
  const texts={
    route:["Where does this route send prompts?","Each gateway route has its own answer. Count fallback routes. Poundlock never guesses this."],
    own:["Where do this model's prompts go?","This model keeps its own answer from before servers declared theirs. Declare it for the server on Connections to give all its models one answer."],
    "declare-server":[`Where does ${endpoint?.name||"this server"} send prompts?`,"You answer once for every model on this server. Poundlock never guesses this."],
  };
  $("model-destination-legend").textContent=texts[mode][0];
  $("model-destination-hint").textContent=texts[mode][1];
}
$("model-destination-inherited").addEventListener("click",event=>{
  const button=event.target.closest("[data-edit-server]");
  if(!button||!closeDrawer())return;
  location.hash="connections";syncNavigation();
  editEndpoint(button.dataset.editServer);
});

// ------------------------------------------------------------ add a model
// Three questions in a fixed order: where it runs, which model, then a review
// of what Poundlock derived from them, each value with its source. Going back
// never changes a later answer silently.

let setup=null,draftSequence=0,draftTimer=null;
const STEPS=["source","model","review"];

function showStep(step){
  setup.step=step;
  $("model-steps").hidden=false;
  for(const item of $("model-steps").querySelectorAll("li")){
    item.classList.toggle("done",STEPS.indexOf(item.dataset.step)<STEPS.indexOf(step));
    if(item.dataset.step===step)item.setAttribute("aria-current","step");else item.removeAttribute("aria-current");
  }
  $("model-step-source").hidden=step!=="source";
  $("model-step-model").hidden=step!=="model";
  $("model-form").hidden=step!=="review";
  $("model-back").hidden=step==="source"||setup.fixedSource&&step==="review";
  $("model-next").hidden=step==="review";
  $("model-save").hidden=step!=="review";
  $("model-save-test").hidden=step!=="review";
  if(step==="source")renderSourceStep();
  if(step==="model")renderModelStep();
  if(step==="review")renderReviewStep();
  syncNext();
  const first=step==="source"?$("model-step-source").querySelector("input:checked,input"):step==="model"?$("model-step-model").querySelector("input:checked,input"):$("model-name");
  first?.focus();
}
function syncNext(){
  if(!setup)return;
  const ready=setup.step==="source"?Boolean(setup.endpointId):setup.step==="model"?Boolean(chosenModel()):true;
  $("model-next").disabled=!ready;
}
function choiceHtml(name,value,title,detail,checked){
  return `<label class="choice"><input type="radio" name="${name}" value="${esc(value)}"${checked?" checked":""}><span><strong class="mono">${esc(title)}</strong><span class="hint">${esc(detail)}</span></span></label>`;
}

// Step 1: a machine Poundlock manages, or a server that is always running.
function renderSourceStep(){
  const d=state.data,nodes=d.nodes.filter(node=>node.status!=="revoked");
  const status=node=>node.status==="online"?"Online":node.status==="maintenance"?"Paused":"Sleeping or offline";
  $("source-machines").innerHTML=nodes.length?nodes.map(node=>choiceHtml("model-source",`node:${node.id}`,node.name,`${status(node)} · ${(node.capabilities?.runtimes||[]).join(", ")||"no runtimes reported"}`,setup.nodeId===node.id)).join(""):'<p class="hint">No machines enrolled.</p>';
  $("source-servers").innerHTML=d.endpoints.length?d.endpoints.map(endpoint=>choiceHtml("model-source",`endpoint:${endpoint.id}`,endpoint.name,`${endpoint.kind==="litellm"?"Gateway":"Model server"} · ${endpoint.base_url}`,!setup.nodeId&&setup.endpointId===endpoint.id)).join(""):'<p class="hint">No servers connected.</p>';
  syncSourceEndpoint();
}
function nodeAddresses(node){
  const listed=typeof nodeInterfaces==="function"?nodeInterfaces(node):[];
  return listed.flatMap(item=>item.addresses||[]).map(address=>String(address).split("/")[0].toLowerCase());
}
// The server a machine's model runs behind: the one its other models use,
// else the one whose address the machine reports. Shown, never assumed.
function suggestServer(node){
  const d=state.data,servers=d.endpoints.filter(endpoint=>endpoint.kind!=="litellm");
  const used=[...new Set(d.models.filter(model=>model.node_id===node.id).map(model=>model.endpoint_id))].filter(id=>servers.some(endpoint=>endpoint.id===id));
  if(used.length===1)return {id:used[0],reason:`Chosen because your other models on ${node.name} use it.`};
  const names=new Set([node.name.toLowerCase(),...nodeAddresses(node)]);
  const matches=servers.filter(endpoint=>{try{const host=new URL(endpoint.base_url).hostname.toLowerCase();return names.has(host)||names.has(host.split(".")[0])}catch{return false}});
  if(matches.length===1)return {id:matches[0].id,reason:`Chosen because its address is ${node.name}'s.`};
  return {id:"",reason:""};
}
function syncSourceEndpoint(){
  const node=nodeById(setup.nodeId),row=$("source-endpoint-row"),hint=$("source-endpoint-hint");
  row.hidden=!node;hint.hidden=!node;
  if(!node)return;
  $("source-endpoint-label").textContent=`Poundlock reaches ${node.name}'s model server through`;
  $("source-endpoint").innerHTML='<option value="">Choose a server</option>'+options(state.data.endpoints,endpoint=>`${endpoint.name} · ${endpoint.base_url}`);
  $("source-endpoint").value=setup.endpointId||"";
  hint.textContent=setup.endpointReason||(state.data.endpoints.length?"Choose the server this machine runs.":"Connect this machine's model server on Connections first.");
}
$("model-step-source").addEventListener("change",event=>{
  if(event.target.name==="model-source"){
    const [kind,id]=event.target.value.split(":");
    setup.touched=true;setup.model=null;
    if(kind==="node"){
      const suggestion=suggestServer(nodeById(id));
      setup.nodeId=id;setup.endpointId=suggestion.id;setup.endpointReason=suggestion.reason;
    }else{setup.nodeId=null;setup.endpointId=id;setup.endpointReason=""}
    syncSourceEndpoint();
  }else if(event.target.id==="source-endpoint"){
    setup.endpointId=event.target.value;setup.endpointReason="";setup.model=null;
    $("source-endpoint-hint").textContent=setup.endpointId?"":"Choose the server this machine runs.";
  }
  syncNext();
});

// Step 2: what the machine's runtimes report, else what the server lists.
function chosenModel(){
  const manual=$("model-manual-id").value.trim();
  if(!manual)return setup.model;
  const runtime=setup.nodeId?$("model-manual-runtime").value:null;
  // A hand-entered model on a machine still needs its runtime chosen.
  return setup.nodeId&&!runtime?null:{id:manual,runtime,context:null,label:null};
}
// The runtime for a hand-entered model: the one the machine's listed models
// use, else its only managed runtime, else the operator chooses.
function defaultManualRuntime(node,rows){
  const listed=[...new Set(rows.map(row=>row.runtime))];
  if(listed.length===1)return listed[0];
  const managed=(node.capabilities?.runtimes||[]).filter(name=>name!=="always-ready");
  if(managed.length===1)return managed[0];
  const runtimes=node.capabilities?.runtimes||[];
  return runtimes.length===1?runtimes[0]:"";
}
// Shown only once something is typed, so it never reads as applying to the
// listed models, which name their own runtime.
function syncManualRuntime(){$("model-manual-runtime-row").hidden=!setup.nodeId||!$("model-manual-id").value.trim()}
async function renderModelStep(){
  const node=nodeById(setup.nodeId),endpoint=endpointById(setup.endpointId),snapshot=auth(),visit=++setup.visits;
  const rows=[],quiet=[];
  for(const runtime of node?.capabilities?.runtimes||[]){
    const identity=runtimeIdentity(node,runtime);
    const described=Array.isArray(identity?.models)&&identity.models.length?identity.models:Array.isArray(identity?.servable)?identity.servable.map(id=>({id})):[];
    if(described.length)for(const model of described)rows.push({id:model.id,runtime,context:model.context_tokens||null,label:model.label||null,from:`${node.name}'s ${runtime}`});
    else quiet.push(runtime);
  }
  $("model-choices-title").textContent=node?`Models ${node.name} can serve`:endpoint.kind==="litellm"?`Routes on ${endpoint.name}`:`Models ${endpoint.name} lists`;
  if(node){
    const current=$("model-manual-runtime").dataset.node===node.id?$("model-manual-runtime").value:"";
    $("model-manual-runtime").innerHTML='<option value="">Choose how it is managed</option>'+(node.capabilities?.runtimes||[]).map(name=>`<option value="${esc(name)}">${esc(name)}</option>`).join("");
    $("model-manual-runtime").dataset.node=node.id;
    $("model-manual-runtime").value=current||defaultManualRuntime(node,rows);
  }
  if(!setup.manual)$("model-manual-id").value="";
  syncManualRuntime();
  setup.rows=rows;paintModelChoices();
  if(node&&!quiet.length){$("model-choices-note").textContent=rows.length?"":`${node.name} reports no models. Enter the server model ID by hand.`;return}
  $("model-choices-note").textContent=`Asking ${endpoint.name} which models it has…`;
  try{
    const ids=await listServerModels(endpoint.id);
    if(!live(snapshot)||setup?.visits!==visit||setup.step!=="model")return;
    // A runtime that reports nothing runs whatever the server lists.
    for(const id of ids)if(!rows.some(row=>row.id===id))rows.push({id,runtime:node?quiet[0]:null,context:null,label:null,from:endpoint.name});
    setup.rows=rows;paintModelChoices();
    $("model-choices-note").textContent=ids.length||rows.length?"":`${endpoint.name} lists no models. Load one there, or enter its ID by hand.`;
  }catch(error){
    if(error.stale||!live(snapshot)||setup?.visits!==visit)return;
    $("model-choices-note").textContent=`${endpoint.name} did not list its models (${error.message}). ${node&&node.status!=="online"?`${node.name} may be asleep. `:""}Enter the server model ID by hand.`;
  }
}
function paintModelChoices(){
  const box=$("model-choices"),models=state.data.models;
  if(!setup.rows.length){box.innerHTML="";return}
  box.innerHTML=setup.rows.map((row,index)=>{
    const named=models.filter(model=>model.endpoint_id===setup.endpointId&&model.model_id===row.id).map(model=>model.name);
    const detail=[row.label,row.context?`${tokens(row.context)} context`:"",row.runtime?`through ${row.runtime}`:"",named.length?`already added as ${named.join(", ")}`:""].filter(Boolean).join(" · ");
    const checked=setup.model&&setup.model.id===row.id&&setup.model.runtime===row.runtime;
    return choiceHtml("model-choice",String(index),row.id,detail||`listed by ${row.from}`,checked);
  }).join("");
}
$("model-step-model").addEventListener("change",event=>{
  if(event.target.name==="model-choice"){
    const row=setup.rows[+event.target.value];
    setup.model={id:row.id,runtime:row.runtime,context:row.context,label:row.label};
    setup.manual=false;$("model-manual-id").value="";setup.touched=true;
  }
  syncNext();
});
$("model-manual-id").addEventListener("input",()=>{
  setup.manual=Boolean($("model-manual-id").value.trim());
  if(setup.manual){setup.model=null;for(const input of document.querySelectorAll('[name="model-choice"]'))input.checked=false;setup.touched=true}
  syncManualRuntime();syncNext();
});

// Step 3: the review. The draft check fills in everything else, with where
// each value came from; the operator can change any of it.
function renderReviewStep(){
  const choice=chosenModel();
  setup.model=choice;
  setup.nameEdited=false;setup.contextEdited=false;setup.outputEdited=false;
  $("model-endpoint").value=setup.endpointId;
  $("model-node").value=setup.nodeId||"";
  syncRuntimeOptions();
  if(choice.runtime)$("model-runtime").value=choice.runtime;
  $("model-id").value=choice.id;
  $("model-context").value=choice.context||"";$("model-output").value="";
  $("model-where").hidden=true;
  $("model-runs-on").hidden=false;
  syncMachineFields();syncModelSettings();renderDestination();renderRunsOn();
  refreshDraft(true);
}
function renderRunsOn(){
  const node=modelNode(),endpoint=modelEndpoint(),id=$("model-id").value;
  if(!endpoint)return;
  const where=node?`Runs on <strong>${esc(node.name)}</strong> through ${esc($("model-runtime").value)}, reached at ${esc(endpoint.name)}.`:endpoint.kind==="litellm"?`A route on the gateway <strong>${esc(endpoint.name)}</strong>.`:`Runs on <strong>${esc(endpoint.name)}</strong>, which you keep running.`;
  $("model-runs-on").innerHTML=`${where} Server model ID <code class="mono">${esc(id)}</code>, what ${esc(node?.name||endpoint.name)} calls it. ${setup.fixedSource?"":'<button type="button" class="secondary copy" data-restart-setup>Change</button>'}`;
}
$("model-runs-on").addEventListener("click",event=>{if(event.target.closest("[data-restart-setup]"))showStep("source")});
const SOURCES={runtime:node=>`reported by ${node?.name||"the machine"}`,given:()=>setup.contextEdited?"as entered":"reported by the gateway route",quarter_of_context:()=>"a quarter of the context",unknown:()=>"not reported"};
function renderLimitsSummary(draft){
  const summary=$("model-limits-summary"),limits=$("model-limits"),gateway=modelEndpoint()?.kind==="litellm";
  const context=draft?.context_tokens?.value,output=draft?.max_output_tokens?.value;
  const known=Boolean(context)&&!setup.contextEdited&&!setup.outputEdited;
  if(setup.limitsOpen||(!known&&!gateway)){
    summary.hidden=true;limits.hidden=false;
    for(const id of ["model-context","model-output"])$(id).required=true;
    $("model-budget-hint").textContent=!context?"Nothing reported this model's context. Enter the server's own limit; agent jobs need at least 8,192.":"Use the server's own context limit. Agent jobs need at least 8,192.";
    return;
  }
  limits.hidden=true;summary.hidden=false;
  for(const id of ["model-context","model-output"])$(id).required=!gateway||Boolean($(id).value);
  summary.innerHTML=known?`${tokens(context)} context, ${SOURCES[draft.context_tokens.source](modelNode())}; ${tokens(output)} output, ${SOURCES[draft.max_output_tokens.source]()}. <button type="button" class="secondary copy" data-open-limits>Change</button>`:`Context and output come from the gateway route when it is published. <button type="button" class="secondary copy" data-open-limits>Set them</button>`;
}
$("model-limits-summary").addEventListener("click",event=>{
  if(!event.target.closest("[data-open-limits]"))return;
  setup.limitsOpen=true;renderLimitsSummary(setup.draft);$("model-context").focus();
});
async function refreshDraft(initial=false){
  if(!setup)return;
  const snapshot=auth(),sequence=++draftSequence;
  const body={endpoint_id:$("model-endpoint").value,model_id:$("model-id").value,node_id:$("model-node").value||null,runtime_profile:$("model-runtime").value||null,uses:radioValue("model-use").split(",")};
  if(setup.nameEdited)body.name=$("model-name").value.trim();
  // A typed context, or a gateway route's; a machine's runtime the draft reads itself.
  const context=setup.contextEdited?+$("model-context").value:setup.nodeId?0:setup.model?.context||0;
  if(context)body.context_tokens=context;
  try{
    const draft=await api("/api/v1/model-profiles/draft",{method:"POST",body:JSON.stringify(body)});
    if(!live(snapshot)||sequence!==draftSequence||!setup)return;
    setup.draft=draft;applyDraft(draft,initial);
  }catch(error){if(!error.stale)renderProblems([{message:error.message}])}
}
function scheduleDraft(){clearTimeout(draftTimer);draftTimer=setTimeout(()=>refreshDraft(),250)}
function applyDraft(draft,initial){
  if(!setup.nameEdited)$("model-name").value=draft.name.value;
  if(initial){
    $("model-label").value=draft.model_label.value||"";
    setRadio("model-use",[...draft.uses.value].sort().join(","));
    setRadio("model-wake",draft.wake_policy.value);
    $("model-thinking").value=draft.thinking.value;
    $("model-thinking-hint").textContent=draft.thinking.source==="llama_cpp_runtime"?"Off by default for a llama.cpp model a machine manages: faster, and it applies unless a request states its own reasoning preference.":"Off is faster for most local reasoning models. It applies unless a request states its own reasoning preference.";
  }
  if(draft.context_tokens.value&&!setup.contextEdited)$("model-context").value=draft.context_tokens.value;
  if(draft.max_output_tokens.value&&!setup.outputEdited)$("model-output").value=draft.max_output_tokens.value;
  renderLimitsSummary(draft);
  renderProblems(draft.problems.filter(problem=>problem.code!=="destination_undeclared"));
  syncMachineFields();syncModelSettings();renderDestination();
  if(initial)modelSnapshot=serializeModelForm();
  updateDirtyIndicator();
}
function renderProblems(problems){
  const list=$("model-problems");
  list.hidden=!problems.length;
  list.innerHTML=problems.map(problem=>`<li>${esc(problem.message)}</li>`).join("");
}
$("model-name").addEventListener("input",()=>{if(setup&&!editingModel){setup.nameEdited=Boolean($("model-name").value.trim());scheduleDraft()}});
$("model-context").addEventListener("input",()=>{if(setup&&!editingModel){setup.contextEdited=true;scheduleDraft()}});
$("model-output").addEventListener("input",()=>{if(setup&&!editingModel)setup.outputEdited=true});

// ------------------------------------------------------------------ drawer

let editingModel=null,modelSnapshot=null,modelOpener=null,savedRoute=null;
function drawerOpen(){return !$("model-drawer").hidden}
function serializeModelForm(){
  const ids=["model-name","model-endpoint","model-id","model-context","model-output","model-node","model-runtime","model-thinking","model-aliases","model-label","model-capabilities","model-idle"];
  const values=ids.map(id=>$(id).value);
  values.push(radioValue("model-use"),radioValue("model-wake"),radioValue("model-destination"),$("model-alias-server-id").checked);
  return JSON.stringify(values);
}
function isDirty(){return drawerOpen()&&(Boolean(setup?.touched)||modelSnapshot!==null&&serializeModelForm()!==modelSnapshot)}
function updateDirtyIndicator(){$("model-drawer-dirty").hidden=!isDirty()}
function confirmDiscardIfDirty(){return !isDirty()||confirm(editingModel?"Discard unsaved changes to this model?":"Discard this new model?")}

function currentRoute(){return [$("model-endpoint").value,$("model-id").value.trim(),$("model-node").value,$("model-runtime").value].join("\n")}
// Changing where a model runs is a different route under the same name.
// Said, never done silently: a gateway route is asked again, and a model
// server's declaration is shown.
function syncRouteBanner(){
  const banner=$("model-route-banner");
  const changed=editingModel!==null&&savedRoute!==null&&currentRoute()!==savedRoute;
  renderDestination();
  const mode=destinationMode(),endpoint=modelEndpoint();
  if(changed&&!banner.dataset.shown){
    banner.dataset.shown="1";
    if(mode!=="server")setRadio("model-destination","");
    $("model-label").value="";
  }
  banner.hidden=!changed;
  const destination=mode==="server"?`Prompts now go where ${endpoint.name} declares: ${PROMPTS_GO[endpoint.data_destination]}.`:"Confirm where prompts go again.";
  banner.textContent=changed?`You changed where this runs. ${destination} The model label was cleared, because the server may now serve a different model.`:"";
}

function resetModelForm(){
  $("model-form").reset();
  fetchedIds={endpoint:null,ids:[]};
  $("model-id-options").innerHTML="";
  $("model-ids-note").textContent="What the server calls this model. Clients never send it.";
  $("list-model-ids").hidden=true;
  delete $("model-route-banner").dataset.shown;
  $("model-route-banner").hidden=true;
  $("model-identity").hidden=true;
  $("model-name").readOnly=false;
  $("model-where").hidden=false;$("model-runs-on").hidden=true;
  $("model-limits").hidden=false;$("model-limits-summary").hidden=true;
  for(const id of ["model-context","model-output"])$(id).required=true;
  renderProblems([]);
  setRadio("model-destination","");
  setRadio("model-wake","never");
  for(const id of ["model-steps","model-step-source","model-step-model","model-back","model-next","model-save-test"])$(id).hidden=true;
  $("model-form").hidden=false;$("model-save").hidden=false;
  syncRuntimeOptions();
  syncMachineFields();
  syncModelSettings();
}
function usesValue(item){
  const uses=item.uses&&item.uses.length?item.uses:[{"durable-coding-agent":"durable","interactive-coding":"interactive","fast-code-completion":"completion"}[item.workload_class]];
  return [...uses].sort().join(",");
}
function fillModelForm(item){
  for(const [field,key] of [["name","name"],["endpoint","endpoint_id"],["id","model_id"],["label","model_label"],["context","context_tokens"],["output","max_output_tokens"],["thinking","thinking"],["node","node_id"]]){
    const input=$("model-"+field),value=String(item[key]??(key==="thinking"?"model_default":""));
    if(input instanceof HTMLSelectElement)updateOptions(input.id,input.innerHTML,value);else input.value=value;
  }
  setRadio("model-use",usesValue(item));
  setRadio("model-wake",item.wake_policy||"never");
  setRadio("model-destination",item.data_destination||"");
  const aliases=item.aliases||[];
  $("model-alias-server-id").checked=aliases.includes(item.model_id);
  $("model-aliases").value=aliases.filter(alias=>alias!==item.model_id).join(", ");
  syncRuntimeOptions();updateOptions("model-runtime",$("model-runtime").innerHTML,item.runtime_profile||"");
  $("model-idle").value=item.idle_seconds??0;
  $("model-capabilities").value=(item.capabilities||[]).length?JSON.stringify(item.capabilities,null,2):"";
  syncMachineFields();syncModelSettings();renderModelIdOptions();
}
function renderRouteFacts(item){
  const box=$("model-route-facts"),catalog=item?.catalog;
  if(!catalog||(!catalog.served_model&&!catalog.provider&&!catalog.pricing&&!(catalog.reasoning_levels||[]).length)){box.hidden=true;box.querySelector("dl").innerHTML="";return}
  const rows=[];
  if(catalog.served_model)rows.push(["Served model",catalog.served_model]);
  if(catalog.provider)rows.push(["Provider",catalog.provider]);
  if(catalog.pricing)rows.push(["Pricing",pricingLine(catalog.pricing)]);
  rows.push(["Verified reasoning levels",(catalog.reasoning_levels||[]).length?catalog.reasoning_levels.join(", "):"model default only"]);
  box.querySelector("dl").innerHTML=rows.map(([k,v])=>`<dt>${esc(k)}</dt><dd>${esc(v)}</dd>`).join("");
  box.hidden=false;
}
function showDrawer(){
  $("model-drawer").hidden=false;$("model-drawer-backdrop").hidden=false;
  if(editingModel)$("model-endpoint").focus();
}
// `presets`: {nodeId} from a machine's "Connect a model server", or
// {endpointId, model} from Discover's Publish.
function openModelDrawer(id,presets={}){
  if(drawerOpen()&&!confirmDiscardIfDirty())return;
  editingModel=id||null;
  modelOpener=document.activeElement;
  setup=null;
  resetModelForm();
  if(id){
    const item=state.data.models.find(x=>x.id===id);if(!item)return;
    fillModelForm(item);
    $("model-drawer-title").textContent=`Edit ${item.name}`;
    $("model-save").textContent="Save changes";
    $("model-publish-note").textContent="Saved changes apply to clients at once.";
    // The name is fixed once the model exists: clients depend on it.
    $("model-name").readOnly=true;
    $("model-name-hint").textContent="Clients send exactly this, so it cannot change. Add another name under Advanced instead.";
    $("model-identity").innerHTML=modelNamesHtml(item);$("model-identity").hidden=false;
    renderRouteFacts(item);
    renderDestination();
    savedRoute=currentRoute();
    modelSnapshot=serializeModelForm();
    updateDirtyIndicator();
    showDrawer();
    return;
  }
  $("model-drawer-title").textContent="Add a model";
  $("model-save").textContent="Add model";
  $("model-publish-note").textContent="Nothing is published until you add the model.";
  $("model-name-hint").textContent="Clients send exactly this. It is not the server's own model ID. It cannot be changed after the model is added.";
  renderRouteFacts(null);
  setup={step:"source",nodeId:null,endpointId:null,endpointReason:"",model:null,rows:[],visits:0,touched:false,manual:false,fixedSource:false,limitsOpen:false,draft:null};
  savedRoute=null;modelSnapshot=null;
  showDrawer();
  if(presets.endpointId&&presets.model){
    // A gateway route from Discover: only the review is left.
    setup.endpointId=presets.endpointId;setup.model=presets.model;setup.fixedSource=true;
    showStep("review");
  }else if(presets.nodeId){
    const suggestion=suggestServer(nodeById(presets.nodeId));
    setup.nodeId=presets.nodeId;setup.endpointId=suggestion.id;setup.endpointReason=suggestion.reason;
    showStep(suggestion.id?"model":"source");
  }else showStep("source");
  updateDirtyIndicator();
}
function closeDrawer(force=false){
  if(!force&&!confirmDiscardIfDirty())return false;
  $("model-drawer").hidden=true;$("model-drawer-backdrop").hidden=true;
  editingModel=null;modelSnapshot=null;savedRoute=null;setup=null;clearTimeout(draftTimer);
  if(modelOpener&&document.contains(modelOpener))modelOpener.focus();
  modelOpener=null;
  return true;
}
function drawerFocusable(){
  return [...$("model-drawer").querySelectorAll('button,[href],input,select,textarea,[tabindex]')]
    .filter(el=>!el.disabled&&el.tabIndex!==-1&&el.offsetParent!==null);
}
function trapDrawerFocus(event){
  if(event.key!=="Tab"||!drawerOpen())return;
  const focusable=drawerFocusable();
  if(!focusable.length)return;
  const first=focusable[0],last=focusable[focusable.length-1],active=document.activeElement;
  if(event.shiftKey&&(active===first||!$("model-drawer").contains(active))){event.preventDefault();last.focus()}
  else if(!event.shiftKey&&active===last){event.preventDefault();first.focus()}
}
document.addEventListener("keydown",event=>{
  if(event.key==="Escape"&&drawerOpen()){event.preventDefault();closeDrawer();return}
  trapDrawerFocus(event);
});
$("model-form").addEventListener("input",updateDirtyIndicator);
$("model-form").addEventListener("change",updateDirtyIndicator);
$("model-drawer").addEventListener("change",updateDirtyIndicator);
$("add-model").onclick=()=>openModelDrawer(null);
$("model-cancel").onclick=()=>closeDrawer();
$("model-drawer-close").onclick=()=>closeDrawer();
$("model-drawer-backdrop").onclick=()=>closeDrawer();
$("model-next").onclick=()=>{if(setup)showStep(STEPS[STEPS.indexOf(setup.step)+1])};
$("model-back").onclick=()=>{if(setup)showStep(STEPS[STEPS.indexOf(setup.step)-1])};

function modelFormBody(){
  const aliases=$("model-aliases").value.split(/[,\s]+/).filter(Boolean);
  const modelId=$("model-id").value.trim();
  if($("model-alias-server-id").checked&&modelId&&!aliases.includes(modelId))aliases.push(modelId);
  const node=$("model-node").value,runtime=$("model-runtime").value;
  const body={aliases,endpoint_id:$("model-endpoint").value,model_id:modelId,model_label:$("model-label").value.trim()||null,uses:radioValue("model-use").split(","),wake_policy:node?radioValue("model-wake")||defaultWake(modelNode()):"never",thinking:$("model-thinking").value,node_id:node||(editingModel?"":null),runtime_profile:runtime||(editingModel?"":null),idle_seconds:runtime==="ollama"?+$("model-idle").value:null};
  if($("model-context").value)body.context_tokens=+$("model-context").value;
  if($("model-output").value)body.max_output_tokens=+$("model-output").value;
  if(!editingModel)body.name=$("model-name").value.trim();
  // Without a machine the choice is always "I keep it running"; an edit
  // leaves whatever value is stored rather than rewriting it.
  if(editingModel&&!node)delete body.wake_policy;
  const mode=destinationMode();
  if(mode==="route"||mode==="own")body.data_destination=radioValue("model-destination");
  // Moved to a server that declares its own: restate it, as the banner said.
  else if(mode==="server"&&editingModel&&currentRoute()!==savedRoute)body.data_destination=modelEndpoint().data_destination;
  return body;
}
async function saveModel(test){
  const mode=destinationMode(),chosen=radioValue("model-destination");
  if(mode!=="server"&&!chosen){toast("Choose where prompts go before saving",true);document.querySelector('[name="model-destination"]').focus();return}
  // A limit hidden behind its summary still has to be valid; show it if not.
  if($("model-limits").hidden&&![$("model-context"),$("model-output")].every(input=>input.checkValidity())){setup.limitsOpen=true;renderLimitsSummary(setup.draft)}
  if(!$("model-form").reportValidity())return;
  const buttons=[$("model-save"),$("model-save-test")];for(const button of buttons)button.disabled=true;
  const body=modelFormBody(),editing=editingModel,endpoint=modelEndpoint();
  // Blank leaves stored evidence unchanged on PATCH (and empty on create).
  // This is a paste-and-replace of the Toolkit's block, not a field editor.
  const pastedCapabilities=$("model-capabilities").value.trim();
  if(pastedCapabilities){
    try{
      const parsed=JSON.parse(pastedCapabilities);
      if(!Array.isArray(parsed))throw new Error("Capability evidence must be a JSON array");
      body.capabilities=parsed;
    }catch(error){fail(new Error(`Capability evidence: ${error.message}`));for(const button of buttons)button.disabled=false;return}
  }
  try{
    // A server's declaration is its own resource: declared first, then the
    // model inherits it like every other model on that server.
    if(mode==="declare-server")await api(`/api/v1/endpoints/${encodeURIComponent(endpoint.id)}`,{method:"PATCH",body:JSON.stringify({data_destination:chosen})});
    let saved;
    if(!editing&&endpoint.kind==="litellm"){
      const publish={endpoint_id:endpoint.id,route:body.model_id,name:body.name,data_destination:body.data_destination,uses:body.uses,aliases:body.aliases,model_label:body.model_label,thinking:body.thinking};
      if(body.context_tokens)publish.context_tokens=body.context_tokens;
      if(body.max_output_tokens)publish.max_output_tokens=body.max_output_tokens;
      saved=await api("/api/v1/discovery/publish",{method:"POST",body:JSON.stringify(publish)});
      discoveryLoaded=false;
      if(body.capabilities)await api(`/api/v1/model-profiles/${encodeURIComponent(saved.id)}`,{method:"PATCH",body:JSON.stringify({capabilities:body.capabilities})});
    }else{
      saved=await api("/api/v1/model-profiles"+(editing?"/"+encodeURIComponent(editing):""),{method:editing?"PATCH":"POST",body:JSON.stringify(body)});
    }
    closeDrawer(true);
    await refresh();
    toast(editing?"Model saved":`Added ${saved.name}`);
    if(test){
      const managed=Boolean(endpoint.managed_identity?.enabled);
      const button=document.querySelector(`[data-${managed?"test":"generate"}-model="${CSS.escape(saved.id)}"]`);
      await testResource(`/api/v1/model-profiles/${encodeURIComponent(saved.id)}/test`,button||$("add-model"),{generate:!managed});
    }
  }
  catch(error){fail(error);try{await refresh()}catch{/* the error is already shown */}}
  finally{for(const button of buttons)button.disabled=false}
}
$("model-form").addEventListener("submit",event=>{event.preventDefault();saveModel(false)});
$("model-save-test").addEventListener("click",()=>saveModel(true));
$("model-runtime").addEventListener("change",()=>{syncMachineFields();syncModelSettings();renderModelIdOptions()});
for(const input of document.querySelectorAll('[name="model-use"]'))input.addEventListener("change",()=>{syncModelSettings();if(setup)scheduleDraft()});
for(const input of document.querySelectorAll('[name="model-wake"]'))input.addEventListener("change",syncMachineFields);
let previousNode="";
$("model-node").addEventListener("focus",()=>{previousNode=$("model-node").value});
$("model-node").addEventListener("change",()=>{
  syncRuntimeOptions();
  // Only a newly chosen machine gets the default; moving between machines
  // keeps the operator's answer.
  if(!previousNode||!$("model-node").value)setRadio("model-wake",defaultWake(modelNode()));
  previousNode=$("model-node").value;
  syncMachineFields();syncModelSettings();renderModelIdOptions();
});
$("model-endpoint").addEventListener("change",()=>fetchModelIds());
$("model-id").addEventListener("focus",()=>{if(editingModel&&fetchedIds.endpoint!==$("model-endpoint").value)fetchModelIds()});
$("model-id").addEventListener("input",syncAliasOption);
$("model-name").addEventListener("input",syncAliasOption);
$("list-model-ids").addEventListener("click",()=>{fetchedIds={endpoint:null,ids:[]};fetchModelIds()});
for(const id of ["model-endpoint","model-id","model-node","model-runtime"]){
  $(id).addEventListener("change",syncRouteBanner);
}
