// Add an OpenRouter catalog model as a LiteLLM route and publish it, once the
// operator confirms the card (ADR 0001, amended 27 Oct 2026). The route takes
// the catalog's prices so LiteLLM counts its spend; nothing is written until
// "Add and publish".
let routeAdd=null;
function catalogModel(id){return discoveryData?.catalog?.models.find(m=>m.id===id)}
function defaultOutput(model,context){return Math.min(model.max_output_tokens||Math.min(Math.floor(context/4),32768),context)}
function showRouteAdd(modelId){
  const model=catalogModel(modelId);
  if(!model)return;
  const gateways=state.data.endpoints.filter(e=>e.kind==="litellm");
  // A context the catalog doesn't list is left for the operator to enter, not guessed.
  const context=model.context_tokens||0;
  routeAdd={modelId,gatewayId:gateways.length===1?gateways[0].id:"",route:modelId.split("/").pop(),credential:"",credentials:null,context,output:context?defaultOutput(model,context):0,destination:""};
  renderRouteAdd();
  if(routeAdd.gatewayId)loadCredentials();
  $("route-add").scrollIntoView({block:"nearest"});
}
async function loadCredentials(){
  const gatewayId=routeAdd.gatewayId;
  try{
    const {credentials}=await api(`/api/v1/discovery/credentials?endpoint_id=${encodeURIComponent(gatewayId)}`);
    if(!routeAdd||routeAdd.gatewayId!==gatewayId)return;
    routeAdd.credentials=credentials;
    routeAdd.credential=(credentials.find(c=>c.provider==="openrouter"||c.name==="openrouter")||credentials[0])?.name||"";
  }catch(error){if(routeAdd?.gatewayId===gatewayId){routeAdd.credentials=[];routeAdd.credentialError=error.message}}
  renderRouteAdd();
}
function renderRouteAdd(){
  const panel=$("route-add");
  if(!routeAdd){panel.hidden=true;panel.innerHTML="";return}
  const model=catalogModel(routeAdd.modelId);
  const gateways=state.data.endpoints.filter(e=>e.kind==="litellm");
  const credentials=routeAdd.credentials||[];
  panel.hidden=false;
  panel.innerHTML=`<h3>Add ${esc(model?.name||routeAdd.modelId)} from OpenRouter</h3>
    <p>Poundlock adds this route to LiteLLM with the prices below, so its spend is counted, then publishes it. Nothing is written until you choose Add and publish.</p>
    <div class="meta"><span>Upstream model: <code>openrouter/${esc(routeAdd.modelId)}</code></span><span>${esc(model?.pricing?pricingLine(model.pricing):"No price listed")}</span></div>
    <div class="meta"><span>Listed context: ${esc(model?.context_tokens??"not listed")}</span><span>Listed maximum output: ${esc(model?.max_output_tokens??"not listed")}</span></div>
    <label>Gateway<select id="route-add-gateway"><option value="">Choose a gateway</option>${gateways.map(e=>`<option value="${esc(e.id)}" ${e.id===routeAdd.gatewayId?"selected":""}>${esc(e.name)}</option>`).join("")}</select></label>
    <label>Provider credential in LiteLLM<select id="route-add-credential" ${credentials.length?"":"disabled"}>${credentials.length?credentials.map(c=>`<option value="${esc(c.name)}" ${c.name===routeAdd.credential?"selected":""}>${esc(c.name)}${c.provider?` (${esc(c.provider)})`:""}</option>`).join(""):`<option>${esc(routeAdd.credentialError||(routeAdd.gatewayId?(routeAdd.credentials?"No credentials in LiteLLM":"Loading…"):"Choose a gateway first"))}</option>`}</select></label>
    <label>Route name applications ask for<input id="route-add-name" value="${esc(routeAdd.route)}"></label>
    <label>Context tokens<input id="route-add-context" type="number" min="1" value="${routeAdd.context||""}" placeholder="${model?.context_tokens?"":"Not listed: enter it"}"></label>
    <label>Maximum output tokens<input id="route-add-output" type="number" min="1" value="${routeAdd.output||""}"></label>
    <fieldset><legend>Where prompts go</legend>
      <label class="check"><input type="radio" name="route-add-destination" value="external" ${routeAdd.destination==="external"?"checked":""}>To an external service</label>
      <label class="check"><input type="radio" name="route-add-destination" value="operator_network" ${routeAdd.destination==="operator_network"?"checked":""}>Within my network</label>
    </fieldset>
    <div class="item-actions"><button id="route-add-confirm" ${routeAddReady()?"":"disabled"}>Add and publish</button><button id="route-add-dismiss" class="secondary">Cancel</button></div><p id="route-add-result" role="status"></p>`;
  $("route-add-gateway").onchange=event=>{routeAdd.gatewayId=event.target.value;routeAdd.credentials=null;routeAdd.credential="";routeAdd.credentialError="";renderRouteAdd();if(routeAdd.gatewayId)loadCredentials()};
  // A field change updates only what depends on it, so focus and typing survive.
  const sync=()=>{$("route-add-confirm").disabled=!routeAddReady()};
  $("route-add-credential").onchange=event=>{routeAdd.credential=event.target.value;sync()};
  $("route-add-name").oninput=event=>{routeAdd.route=event.target.value.trim();sync()};
  // The output default follows the context until the operator types an output of their own.
  $("route-add-context").oninput=event=>{
    routeAdd.context=Number(event.target.value);
    if(!routeAdd.outputEdited){routeAdd.output=routeAdd.context>0?defaultOutput(model||{},routeAdd.context):0;$("route-add-output").value=routeAdd.output||""}
    sync();
  };
  $("route-add-output").oninput=event=>{routeAdd.output=Number(event.target.value);routeAdd.outputEdited=true;sync()};
  panel.querySelectorAll("[name=route-add-destination]").forEach(input=>input.onchange=()=>{routeAdd.destination=input.value;sync()});
  $("route-add-dismiss").onclick=()=>{routeAdd=null;renderRouteAdd()};
  $("route-add-confirm").onclick=confirmRouteAdd;
}
function routeAddReady(){return Boolean(routeAdd.gatewayId&&routeAdd.credential&&routeAdd.route&&routeAdd.destination&&routeAdd.context>0&&routeAdd.output>0)}
async function confirmRouteAdd(){
  const button=$("route-add-confirm"),result=$("route-add-result");
  button.disabled=true;result.textContent="Adding the route in LiteLLM…";
  try{
    const profile=await api("/api/v1/discovery/add-route",{method:"POST",body:JSON.stringify({endpoint_id:routeAdd.gatewayId,catalog_id:routeAdd.modelId,route:routeAdd.route,credential:routeAdd.credential,context_tokens:routeAdd.context,max_output_tokens:routeAdd.output,data_destination:routeAdd.destination})});
    routeAdd=null;renderRouteAdd();
    await refresh();await loadDiscovery();
    toast(`Added and published ${profile.name}`);
  }catch(error){result.textContent=error.message;button.disabled=false}
}
async function refreshGatewayProfile(id,button){
  if(!confirm("Refresh the served model and price from LiteLLM? If the model changed, old reasoning verification is discarded and its routing revision changes. Its name, label, destination and limits stay as configured; review those separately. This sends no model request."))return;
  const before=state.data.models.find(p=>p.id===id)?.catalog?.served_model;
  button.disabled=true;
  try{
    const profile=await api(`/api/v1/model-profiles/${encodeURIComponent(id)}/sync-route`,{method:"POST",body:"{}"});
    const after=profile.catalog?.served_model,changed=after!==before;
    if(changed)lastVerifyResults.delete(id);
    await refresh();
    if(discoveryLoaded)await loadDiscovery();
    toast(changed?`Served model changed: ${before||"unknown"} → ${after||"unknown"}. Old reasoning verification cleared; review the label and limits.`:"Refreshed; served model unchanged.");
  }catch(error){fail(error)}finally{button.disabled=false}
}
