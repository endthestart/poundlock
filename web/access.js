let editingToken=null,showRevoked=false;
// Console-only explanations; the labels themselves come from the controller.
const PERMISSION_HINTS={
  "inference:priority":"For an application a person is waiting on, such as Dispatch's manager or a chat app like Open WebUI: a request that names no priority waits as yours. Without it, every request waits in line as ordinary work.",
  "external:invoke":"Required for models whose prompts go to an external service. This may send private data and use paid services.",
};
const DEFAULT_PERMISSIONS=["models:read","inference:invoke"];
// Managed-identity statuses reported by the controller: active, revoked,
// provision_failed. Anything else is unrecognised.
const IDENTITY_STATE={active:["ok","Active"],revoked:["ended","Revoked"],provision_failed:["fault","Provisioning failed"]};
function identityBadge(status){const [cls,label]=IDENTITY_STATE[status]||["unrecognised",status];return badge(cls,label)}
function permissionLabel(code){return (state.data.token_permissions||[]).find(item=>item.code===code)?.label||code}
function renderPermissionChoices(){
  const fieldset=$("api-token-permissions");
  if(fieldset.querySelector('[name="api-permission"]'))return;
  if(!Array.isArray(state.data.token_permissions))throw new Error("The controller did not publish its token permissions.");
  fieldset.insertAdjacentHTML("beforeend",state.data.token_permissions.map(({code,label})=>`<label class="check"><input type="checkbox" name="api-permission" value="${esc(code)}"${DEFAULT_PERMISSIONS.includes(code)?" checked":""}>${esc(label)}</label>${PERMISSION_HINTS[code]?`<p class="hint">${esc(PERMISSION_HINTS[code])}</p>`:""}`).join(""));
}
function renderAccess(){
  if(state.data.role!=="operator")return;
  renderPermissionChoices();
  const list=$("api-token-list"),tokens=state.data.api_tokens||[];
  if(editingToken&&!tokens.some(token=>token.id===editingToken&&!token.revoked_at))resetToken();
  // Active tokens first; revoked ones only on request, since they accumulate for good.
  const revoked=tokens.filter(token=>token.revoked_at).length;
  const shown=[...tokens].filter(token=>showRevoked||!token.revoked_at).sort((a,b)=>Boolean(a.revoked_at)-Boolean(b.revoked_at)||a.internal-b.internal||a.name.localeCompare(b.name));
  $("api-token-revoked").hidden=!revoked;$("api-token-revoked").textContent=showRevoked?"Hide revoked tokens":`Show ${revoked} revoked token${revoked===1?"":"s"}`;
  $("api-token-revoked").setAttribute("aria-expanded",String(showRevoked));
  if(!shown.length){emptyState(list,"No active application tokens.","Create one with the form beside this list for another editor or integration.")}
  else{
    // Poundlock's own check principal is listed for completeness: nobody holds its
    // secret, so it is never "used" and has nothing to edit or revoke.
    const used=token=>token.internal?"Poundlock's own checks; nobody holds its secret":token.last_used_at?`Last used ${new Date(token.last_used_at).toLocaleString()}`:"Never used";
    list.className="list";list.innerHTML=shown.map(token=>`<div class="item">${rowIndicator(token.revoked_at?"ended":"ok")}<strong>${esc(token.name)}</strong><div class="meta">${badge(token.revoked_at?"ended":"ok",token.revoked_at?"Revoked":token.internal?"Internal":"Active")}</div><div class="meta">${token.permissions.map(code=>`<span>${esc(permissionLabel(code))}</span>`).join("")}</div><div class="meta"><span>Created ${new Date(token.created_at).toLocaleString()}</span><span>${esc(used(token))}</span></div>${token.revoked_at||token.internal?"":`<div class="item-actions"><button class="secondary" data-edit-token="${esc(token.id)}">Edit</button><button class="secondary danger" data-revoke-token="${esc(token.id)}">Revoke</button></div>`}</div>`).join("");
    list.querySelectorAll("[data-edit-token]").forEach(button=>button.onclick=()=>editToken(button.dataset.editToken));
    list.querySelectorAll("[data-revoke-token]").forEach(button=>button.onclick=async()=>{if(!confirm("Revoke this token? The application using it loses access immediately, its streams stop, and its queued and running jobs are cancelled. A replacement token won't see this token's jobs."))return;try{await api(`/api/v1/api-tokens/${button.dataset.revokeToken}/revoke`,{method:"POST",body:"{}"});toast("Token revoked");await refresh()}catch(error){fail(error)}});
  }
  renderFrontierBudget();
  const identities=state.data.managed_identities||[],managedList=$("managed-identity-list");
  if(!identities.length){emptyState(managedList,"No application keys.","They appear once a gateway gives each application its own key.")}
  else{
    managedList.className="list";managedList.innerHTML=identities.map(identity=>{const token=tokens.find(item=>item.id===identity.api_token_id),endpoint=(state.data.endpoints||[]).find(item=>item.id===identity.endpoint_id);return `<div class="item"><strong>${esc(token?.name||"Removed application token")}</strong><div class="meta">${identityBadge(identity.status)}<span>${esc(endpoint?.name||"Removed gateway")}</span></div>${identity.last_error?`<p class="check-fail">${esc(identity.last_error)}</p>`:""}<div class="meta"><span>Updated ${new Date(identity.updated_at).toLocaleString()}</span></div></div>`}).join("");
  }
}
const BUDGET_STATE={ok:["ok","Within budget"],low:["review","Running low"],exhausted:["fault","Used up"]};
function money(value){const amount=Number(value||0);return `$${amount.toFixed(amount>0&&amount<0.01?4:2)}`}
function renderFrontierBudget(){
  const view=state.data.frontier_budget;if(!view)return;
  const form=$("frontier-budget-form");
  if(!form.dataset.dirty){
    const settings=view.settings||{};
    $("frontier-budget-weekly").value=settings.weekly_usd??"";
    $("frontier-budget-monthly").value=settings.monthly_usd??"";
    $("frontier-budget-warn").value=Math.round((settings.warn_at??0.8)*100);
    $("frontier-budget-offset").value=(settings.utc_offset_minutes??0)/60;
  }
  const standing=$("frontier-budget-standing"),periods=view.status?.periods||[];
  if(!periods.length){emptyState(standing,"No ceiling set.","External spend is still recorded below.")}
  else{
    standing.className="list";standing.innerHTML=periods.map(period=>{const [cls,label]=BUDGET_STATE[period.state]||["unrecognised",period.state];return `<div class="item">${rowIndicator(cls)}<strong>${period.period==="weekly"?"This week":"This month"}: ${esc(money(period.spent_usd))} of ${esc(money(period.limit_usd))}</strong><div class="meta">${badge(cls,label)}<span>${esc(money(period.remaining_usd))} left</span>${period.in_flight_usd>0?`<span>${esc(money(period.in_flight_usd))} held for requests in flight</span>`:""}<span>Resets ${new Date(period.resets_at).toLocaleString()}</span></div></div>`}).join("");
  }
  const spend=$("frontier-budget-spend"),lines=view.spend?.lines||[];
  if(!lines.length){emptyState(spend,"No external spend recorded.",`Since ${new Date(view.spend?.since||Date.now()).toLocaleDateString()}.`)}
  else{
    spend.className="list";spend.innerHTML=lines.map(line=>`<div class="item"><strong>${esc(money(line.cost_usd))} · ${esc(line.api_token_name||(line.api_token_id==="operator"?"Operator":"Removed application token"))}</strong><div class="meta"><span>${esc(line.profile_name||"Removed model")}</span><span>${line.requests} request${line.requests===1?"":"s"}</span>${line.estimated_requests?`<span>${line.estimated_requests} charged an estimate</span>`:""}</div></div>`).join("");
  }
}
$("frontier-budget-form").addEventListener("input",event=>{event.currentTarget.dataset.dirty="1"});
$("frontier-budget-form").addEventListener("submit",async event=>{
  event.preventDefault();const form=event.currentTarget;
  const amount=id=>{const value=$(id).value.trim();return value===""?undefined:Number(value)};
  const body={weekly_usd:amount("frontier-budget-weekly"),monthly_usd:amount("frontier-budget-monthly"),warn_at:Number($("frontier-budget-warn").value)/100,utc_offset_minutes:Math.round(Number($("frontier-budget-offset").value)*60)};
  try{state.data.frontier_budget=await api("/api/v1/frontier-budget",{method:"PUT",body:JSON.stringify(body)});delete form.dataset.dirty;renderFrontierBudget();toast("Frontier budget saved")}catch(error){fail(error)}
});
function resetToken(){
  editingToken=null;$("api-token-form").reset();
  $("api-token-heading").textContent="Create an application token";$("api-token-save").textContent="Create token";
  $("api-token-cancel").hidden=true;$("api-token-edit-note").hidden=true;
}
function editToken(id){
  const token=state.data.api_tokens.find(item=>item.id===id);if(!token)return;
  resetToken();clearReveal();editingToken=id;
  $("api-token-name").value=token.name;
  for(const input of document.querySelectorAll('[name="api-permission"]'))input.checked=token.permissions.includes(input.value);
  $("api-token-heading").textContent=`Edit ${token.name}`;$("api-token-save").textContent="Save changes";
  $("api-token-cancel").hidden=false;$("api-token-edit-note").hidden=false;
  $("api-token-name").focus();$("api-token-form").scrollIntoView({block:"start",behavior:motion()});
}
$("api-token-form").addEventListener("submit",async event=>{
  event.preventDefault();
  const snapshot=auth(),editing=editingToken;
  const body=JSON.stringify({name:$("api-token-name").value,permissions:[...document.querySelectorAll('[name="api-permission"]:checked')].map(input=>input.value)});
  try{
    if(editing){await api(`/api/v1/api-tokens/${encodeURIComponent(editing)}`,{method:"PATCH",body});resetToken();toast("Token saved");await refresh();return}
    const issued=await api("/api/v1/api-tokens",{method:"POST",body});
    if(!live(snapshot))throw staleError();
    resetToken();showReveal("api-token-issued",issued.token);toast("Token created");
    await refresh({keepReveal:true});
  }catch(error){fail(error)}
});
$("api-token-cancel").onclick=resetToken;
$("api-token-revoked").onclick=()=>{showRevoked=!showRevoked;renderAccess()};
$("copy-api-token").onclick=()=>copyReveal();
$("copied-api-token").onclick=()=>{clearReveal();toast("Token cleared from this page")};
$("reconcile-managed-identities").onclick=async event=>{const button=event.currentTarget;button.disabled=true;try{const result=await api("/api/v1/managed-identities/reconcile",{method:"POST",body:"{}"});await refresh();toast(result.pending?`${result.pending} revocation${result.pending===1?" is":"s are"} still pending`:`Reconciled ${result.reconciled} managed identit${result.reconciled===1?"y":"ies"}`)}catch(error){fail(error)}finally{button.disabled=false}};
