// Connections tab: inference endpoints (model servers and LiteLLM gateways).
let editingEndpoint=null;

function renderConnections(){
  const d=state.data;
  const list=$("configured"),endpoints=d.endpoints;
  if(!endpoints.length){emptyState(list,"No endpoints configured.","Add a model server or gateway below to publish model profiles.");return}
  list.className="list";
  list.innerHTML=endpoints.map(x=>{
    const indicator=x.last_check_ok===true?"ok":x.last_check_ok===false?"fault":"unknown";
    return `<div class="item" data-endpoint-row="${esc(x.id)}">${rowIndicator(indicator)}<strong>${esc(x.name)}</strong><div class="meta"><span class="pill">${x.kind==="litellm"?"Gateway (LiteLLM)":"Model server"}</span>${x.managed_identity?.enabled?'<span class="pill">Per-client identities</span>':""}<span class="mono">${esc(x.base_url)}</span>${x.public_url?`<span class="mono">${esc(x.public_url)}</span>`:""}</div>${serverDestinationHtml(x,d.models)}<div class="meta">${checkLabel(x)}</div><div class="item-actions"><button class="secondary" data-test-endpoint="${esc(x.id)}">Test</button> <button class="secondary" data-edit-endpoint="${esc(x.id)}">Edit</button>${x.managed_identity?.enabled?` <button class="secondary" data-rotate-endpoint="${esc(x.id)}">Rotate client keys</button>`:""} <button class="secondary" data-remove-endpoint="${esc(x.id)}">Remove</button></div></div>`;
  }).join("");
  list.querySelectorAll("[data-test-endpoint]").forEach(button=>button.onclick=()=>testResource(`/api/v1/endpoints/${button.dataset.testEndpoint}/test`,button,{}));
  list.querySelectorAll("[data-edit-endpoint]").forEach(button=>button.onclick=()=>editEndpoint(button.dataset.editEndpoint));
  list.querySelectorAll("[data-rotate-endpoint]").forEach(button=>button.onclick=async()=>{
    if(!confirm("Rotate every active Poundlock-managed key for this gateway?"))return;
    button.disabled=true;
    try{const result=await api(`/api/v1/endpoints/${encodeURIComponent(button.dataset.rotateEndpoint)}/managed-identities/rotate`,{method:"POST",body:"{}"});toast(`Rotated ${result.rotated} client key${result.rotated===1?"":"s"}`)}catch(error){fail(error)}finally{button.disabled=false}
  });
  list.querySelectorAll("[data-remove-endpoint]").forEach(button=>button.onclick=async()=>{
    const id=button.dataset.removeEndpoint,item=d.endpoints.find(x=>x.id===id);
    if(!confirm(`Remove ${item.name}? Existing jobs keep their saved settings and history.`))return;
    button.disabled=true;
    try{await api(`/api/v1/endpoints/${encodeURIComponent(id)}`,{method:"DELETE"});if(editingEndpoint===id)resetEndpoint();await refresh();toast("Removed")}
    catch(error){fail(error);button.disabled=false}
  });
}

// Where a model server sends prompts, declared once for all its models. A
// gateway declares it per route instead, so it shows nothing here.
const DESTINATION_LABELS={operator_network:"within my network",external:"to an external service"};
function serverDestinationHtml(endpoint,models){
  if(endpoint.data_destination===undefined||endpoint.data_destination===null)return "";
  const own=(models||[]).filter(model=>model.endpoint_id===endpoint.id&&model.data_destination_source==="profile");
  const declared=DESTINATION_LABELS[endpoint.data_destination];
  const line=declared?`Prompts go ${declared}`:"Where prompts go is not declared, so its models cannot be used yet";
  const mixed=own.length?`<p class="hint">${esc(own.map(model=>model.name).join(", "))} still declare${own.length===1?"s":""} their own destination. Declaring it here applies one answer to every model on this server.</p>`:"";
  return `<div class="meta"><span class="${declared?"":"check-fail"}">${esc(line)}</span></div>${mixed}`;
}

async function testResource(path,button,body){
  const snapshot=auth();
  button.disabled=true;
  try{
    await api(path,{method:"POST",body:JSON.stringify(body)});
    if(!live(snapshot))throw staleError();
    toast("Check passed");
  }catch(error){fail(error)}
  try{if(live(snapshot))await refresh()}catch{/* last check still stored */}
  finally{button.disabled=false}
}

function syncManagedEndpoint(){
  const gateway=$("endpoint-kind").value==="litellm",enabled=$("endpoint-connection-mode").value==="managed_applications";
  $("endpoint-destination-group").hidden=gateway;
  $("endpoint-managed").hidden=!gateway;
  $("endpoint-managed-policy").hidden=!gateway||!enabled;
  for(const id of ["endpoint-managed-budget","endpoint-managed-budget-duration","endpoint-managed-duration","endpoint-managed-rpm","endpoint-managed-tpm"])$(id).disabled=!gateway||!enabled;
}
function resetEndpoint(){editingEndpoint=null;$("endpoint-form").reset();$("endpoint-save").textContent="Save endpoint";$("endpoint-cancel").hidden=true;$("endpoint-clear-row").hidden=true;$("endpoint-edit-note").hidden=true;syncManagedEndpoint()}
function editEndpoint(id){
  const item=state.data.endpoints.find(x=>x.id===id);if(!item)return;
  resetEndpoint();editingEndpoint=id;
  for(const [field,key] of [["name","name"],["kind","kind"],["url","base_url"],["public-url","public_url"]])$("endpoint-"+field).value=item[key]||"";
  for(const input of document.querySelectorAll('[name="endpoint-destination"]'))input.checked=input.value===item.data_destination;
  const managed=item.managed_identity||{};$("endpoint-connection-mode").value=managed.enabled?"managed_applications":"gateway_only";$("endpoint-managed-budget").value=managed.max_budget??"";$("endpoint-managed-budget-duration").value=managed.budget_duration??"";$("endpoint-managed-duration").value=managed.duration??"";$("endpoint-managed-rpm").value=managed.rpm_limit??"";$("endpoint-managed-tpm").value=managed.tpm_limit??"";syncManagedEndpoint();
  $("endpoint-save").textContent="Save changes";$("endpoint-cancel").hidden=false;$("endpoint-clear-row").hidden=!item.api_key_set;$("endpoint-edit-note").hidden=!item.api_key_set;
  $("endpoint-name").focus();$("endpoint-form").scrollIntoView({block:"start",behavior:"smooth"});
}
$("endpoint-form").addEventListener("submit",async event=>{
  event.preventDefault();const button=$("endpoint-save");button.disabled=true;
  const optionalNumber=id=>$(id).value?+$(id).value:null;
  const managed=$("endpoint-kind").value==="litellm"&&$("endpoint-connection-mode").value==="managed_applications";
  const body={name:$("endpoint-name").value,kind:$("endpoint-kind").value,base_url:$("endpoint-url").value,public_url:$("endpoint-public-url").value.trim(),managed_identity:{enabled:managed,max_budget:managed?optionalNumber("endpoint-managed-budget"):null,budget_duration:managed?$("endpoint-managed-budget-duration").value.trim()||null:null,duration:managed?$("endpoint-managed-duration").value.trim()||null:null,rpm_limit:managed?optionalNumber("endpoint-managed-rpm"):null,tpm_limit:managed?optionalNumber("endpoint-managed-tpm"):null}};
  if(!editingEndpoint||$("endpoint-key").value||$("endpoint-clear-key").checked)body.api_key=$("endpoint-clear-key").checked?"":$("endpoint-key").value;
  const destination=document.querySelector('[name="endpoint-destination"]:checked')?.value;
  if(body.kind==="openai"){
    if(destination)body.data_destination=destination;
    else if(!editingEndpoint){toast("Choose where this server sends prompts",true);button.disabled=false;return}
  }
  try{await api("/api/v1/endpoints"+(editingEndpoint?"/"+encodeURIComponent(editingEndpoint):""),{method:editingEndpoint?"PATCH":"POST",body:JSON.stringify(body)});resetEndpoint();await refresh();toast("Endpoint saved")}
  catch(error){fail(error)}finally{button.disabled=false}
});
$("endpoint-kind").onchange=syncManagedEndpoint;$("endpoint-connection-mode").onchange=syncManagedEndpoint;
$("endpoint-cancel").onclick=resetEndpoint;
