// LiteLLM owns routes. This guided handoff reads them back without creating
// routes, publishing profiles, spending, or changing a client's limits.
let routeHandoff=null;
function showRouteHandoff(modelId){
  const model=discoveryData?.catalog?.models.find(m=>m.id===modelId);
  if(!model)return;
  const gateways=state.data.endpoints.filter(e=>e.kind==="litellm");
  routeHandoff={modelId,gatewayId:gateways.length===1?gateways[0].id:"",routeName:modelId};
  renderRouteHandoff();
  $("route-handoff").scrollIntoView({block:"nearest"});
}
function renderRouteHandoff(){
  const panel=$("route-handoff");
  if(!routeHandoff){panel.hidden=true;panel.innerHTML="";return}
  const gateways=state.data.endpoints.filter(e=>e.kind==="litellm");
  const endpoint=gateways.find(e=>e.id===routeHandoff.gatewayId);
  const routes=discoveryData?.gateways.find(g=>g.endpoint_id===endpoint?.id)?.routes||[];
  const upstream=`openrouter/${routeHandoff.modelId}`;
  panel.hidden=false;
  panel.innerHTML=`<h3>Add or update ${esc(routeHandoff.modelId)}</h3>
    <p>LiteLLM owns the route and its provider credential. Nothing is created or enabled by opening this guide.</p>
    <label>Gateway<select id="handoff-gateway"><option value="">Choose a gateway</option>${gateways.map(e=>`<option value="${esc(e.id)}" ${e.id===routeHandoff.gatewayId?"selected":""}>${esc(e.name)}</option>`).join("")}</select></label>
    <label>Existing route to update, or add a new one<select id="handoff-existing"><option value="">Add a new route</option>${routes.map(r=>`<option value="${esc(r.name)}" ${r.name===routeHandoff.routeName?"selected":""}>${esc(r.name)} — ${esc(r.upstream_model)}</option>`).join("")}</select></label>
    <label>Gateway model name (alias)<input id="handoff-route-name" value="${esc(routeHandoff.routeName)}"></label>
    <p>1. Open Models + Endpoints in LiteLLM. Add a model or edit the selected route, choose OpenRouter and its existing credential, then save.</p>
    <div class="meta"><span>Upstream model: <code>${esc(upstream)}</code></span><button class="secondary" data-copy="${esc(upstream)}">Copy upstream model</button><button class="secondary" id="handoff-copy-alias">Copy alias</button></div>
    <div class="item-actions">${endpoint?`<a id="handoff-open" class="secondary button-link" href="${esc(adminUiUrl(endpoint,routes.some(r=>r.name===routeHandoff.routeName)?routeHandoff.routeName:null))}" target="_blank" rel="noopener">Open LiteLLM Models + Endpoints</a>`:""}</div>
    <p>2. Return here and check the saved route. A new route still needs an explicit Publish; a changed published profile needs a refresh. These steps do not send model requests.</p>
    <div class="item-actions"><button id="handoff-check" class="secondary" ${endpoint?"":"disabled"}>Check saved route</button><button id="handoff-dismiss" class="secondary">Dismiss guide</button></div><p id="handoff-result" role="status"></p>`;
  $("handoff-gateway").onchange=event=>{routeHandoff.gatewayId=event.target.value;routeHandoff.routeName=routeHandoff.modelId;renderRouteHandoff()};
  $("handoff-existing").onchange=event=>{routeHandoff.routeName=event.target.value||routeHandoff.modelId;renderRouteHandoff()};
  $("handoff-route-name").oninput=event=>{routeHandoff.routeName=event.target.value.trim()};
  panel.querySelector("[data-copy]").onclick=event=>copyText(event.currentTarget.dataset.copy);
  $("handoff-copy-alias").onclick=()=>copyText(routeHandoff.routeName);
  $("handoff-dismiss").onclick=()=>{routeHandoff=null;renderRouteHandoff()};
  $("handoff-check").onclick=checkSavedRoute;
}
async function checkSavedRoute(){
  const expected={...routeHandoff},button=$("handoff-check"),result=$("handoff-result");
  button.disabled=true;result.textContent="Reading gateway routes…";
  try{
    if(!await loadDiscovery())throw new Error("Could not read gateway routes; nothing was changed.");
    if(!routeHandoff||routeHandoff.gatewayId!==expected.gatewayId||routeHandoff.routeName!==expected.routeName)return;
    const gateway=discoveryData.gateways.find(g=>g.endpoint_id===expected.gatewayId);
    if(gateway?.error)throw new Error(gateway.error);
    const route=gateway?.routes.find(r=>r.name===expected.routeName);
    const upstream=`openrouter/${expected.modelId}`;
    if(!route)result.textContent=`No route named ${expected.routeName} was reported. Check the alias in LiteLLM and retry.`;
    else if(route.upstream_model!==upstream)result.textContent=`That alias still routes to ${route.upstream_model||"an unknown model"}, not ${upstream}. Nothing was published or refreshed.`;
    else result.textContent=route.state==="published"?route.profile?.model_changed?"Saved route confirmed. Refresh its published profile below, then review the label, limits and reasoning settings.":"Saved route matches the published profile.":"Saved route confirmed. Use Publish below and choose where its prompts go.";
  }catch(error){result.textContent=error.message}
  finally{button.disabled=false}
}
async function refreshGatewayProfile(id,button){
  if(!confirm("Refresh this profile's served model and price from LiteLLM? If the model changed, old reasoning verification is discarded and its routing revision changes. Its name, label, destination and limits stay as configured; review those separately. This sends no model request."))return;
  const before=state.data.models.find(p=>p.id===id)?.catalog?.served_model;
  button.disabled=true;
  try{
    const profile=await api(`/api/v1/model-profiles/${encodeURIComponent(id)}/sync-route`,{method:"POST",body:"{}"});
    const after=profile.catalog?.served_model,changed=after!==before;
    if(changed)lastVerifyResults.delete(id);
    await refresh();
    if(discoveryLoaded)await loadDiscovery();
    toast(changed?`Served model changed: ${before||"unknown"} → ${after||"unknown"}. Old reasoning verification cleared; review the label and limits.`:"Profile refreshed; served model unchanged.");
  }catch(error){fail(error)}finally{button.disabled=false}
}
