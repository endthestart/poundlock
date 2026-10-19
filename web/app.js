(()=>{const legacy=localStorage.getItem("rcharm-token");if(legacy&&!localStorage.getItem("poundlock-token"))localStorage.setItem("poundlock-token",legacy);localStorage.removeItem("rcharm-token")})();
const state={token:localStorage.getItem("poundlock-token")||"",data:null,selectedJob:null,jobTimer:null,reveal:null,nodePoll:null,page:null,session:0};
const $=id=>document.getElementById(id);
const esc=value=>String(value??"").replace(/[&<>"']/g,c=>({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[c]));
const terminal=status=>["completed","failed","cancelled"].includes(status);
function beginSession(){state.session+=1;if(typeof clearPendingJob==="function")clearPendingJob();return state.session}
function sessionExpired(session){return session!==state.session}
function auth(){return {session:state.session,token:state.token}}
function live(snapshot){return !sessionExpired(snapshot.session)&&state.token===snapshot.token}
function staleError(){const error=new Error("Session ended");error.stale=true;return error}
function fail(error){if(!error?.stale)toast(error.message,true)}

async function api(path,options={}){
  const snapshot=auth();
  const response=await fetch(path,{...options,headers:{"content-type":"application/json","authorization":`Bearer ${snapshot.token}`,...options.headers}});
  const body=await response.json().catch(()=>({error:{message:"Invalid server response"}}));
  if(!live(snapshot))throw staleError();
  if(!response.ok){const error=new Error(body.error?.message||`Request failed (${response.status})`);error.status=response.status;throw error}
  return body;
}
async function authorized(path){
  const snapshot=auth();
  const response=await fetch(path,{headers:{authorization:`Bearer ${snapshot.token}`}});
  if(!live(snapshot))throw staleError();
  return {response,snapshot};
}

// Theme menu: System removes the attribute so theme.css's prefers-color-scheme
// rules apply. theme-boot.js applies the stored choice before first paint;
// this only handles later changes. Sign-out must not touch this key.
function applyTheme(value){
  try{if(value==="system")localStorage.removeItem("poundlock-theme");else localStorage.setItem("poundlock-theme",value)}catch{}
  if(value==="system")delete document.documentElement.dataset.theme;else document.documentElement.dataset.theme=value;
}
(function initThemeMenu(){
  let stored="system";
  try{stored=localStorage.getItem("poundlock-theme")||"system"}catch{}
  $("theme-menu").value=stored;
  $("theme-menu").addEventListener("change",()=>applyTheme($("theme-menu").value));
})();

// Empty state: what is absent, then what to do about it. Never apologise.
function emptyState(el,absent,action){el.className="list empty";el.innerHTML=`<span>${esc(absent)}</span><span>${esc(action)}</span>`}

function toast(message,error=false){const el=$("toast");el.textContent=message;el.className=error?"show error":"show";clearTimeout(toast.timer);toast.timer=setTimeout(()=>el.className="",3500)}
function options(items,label){return items.map(item=>`<option value="${esc(item.id)}">${esc(label(item))}</option>`).join("")}
function updateOptions(id,html,selected=$(id).value){
  const select=$(id);
  select.innerHTML=html;
  select.setCustomValidity("");
  if(!selected){if([...select.options].some(option=>option.value===""))select.value="";return}
  if([...select.options].some(option=>option.value===selected)){select.value=selected;return}
  const missing=new Option("Selection unavailable — choose another",selected,true,true);
  missing.disabled=true;select.add(missing);
  select.setCustomValidity("This selection is no longer available. Choose another.");
}
document.addEventListener("change",event=>{if(event.target instanceof HTMLSelectElement)event.target.setCustomValidity("")});
// Generic badge: cls is one of the semantic state buckets in app.css (ok,
// active, review, fault, ended, unknown, unrecognised), or a raw job-status
// value that app.css maps to one of those buckets.
function badge(cls,label){return `<span class="badge ${esc(cls)}">${esc(label)}</span>`}

// Real job statuses: submitted, waiting_node, waiting_capacity, preparing,
// ready, running, completed, failed, cancelled. Anything else is a status
// value this UI does not recognise and gets the pink "unrecognised" treatment
// rather than silently inheriting a meaning it doesn't have.
const JOB_STATUS_LABELS={submitted:"Submitted",waiting_node:"Waiting for a machine",waiting_capacity:"Waiting for capacity",preparing:"Preparing",ready:"Ready",running:"Running",completed:"Finished",failed:"Failed",cancelled:"Cancelled"};
function statusLabel(status){return JOB_STATUS_LABELS[status]||status.replaceAll("_"," ")}
function jobBadge(status){const known=Object.hasOwn(JOB_STATUS_LABELS,status);return badge(known?status:"unrecognised",statusLabel(status))}
// Maps a job status (or any other domain status) to one of the state buckets
// app.css colours (ok/active/review/fault/ended/unknown/unrecognised), for
// the small leading row-indicator dot each list row carries.
const JOB_STATUS_BUCKETS={submitted:"review",waiting_node:"review",waiting_capacity:"review",preparing:"active",ready:"active",running:"active",completed:"ok",failed:"fault",cancelled:"ended"};
function jobBucket(status){return JOB_STATUS_BUCKETS[status]||"unrecognised"}
function rowIndicator(bucket,tick=false){return `<span class="row-indicator ${tick?"tick ":""}${esc(bucket)}" aria-hidden="true"></span>`}
function verificationBadge(job){
  if(job.status!=="completed")return "";
  const status=job.result?.provenance?.verification?.status;
  if(status==="passed")return badge("passed","Checks passed");
  if(status==="failed")return badge("failed","Checks failed");
  return badge("unverified","Unverified");
}

function clearReveal(){
  state.reveal=null;
  document.querySelectorAll("[data-reveal]").forEach(root=>{
    root.hidden=true;
    root.querySelectorAll("[data-secret],input[readonly]").forEach(field=>{field.value=""});
    root.querySelectorAll("pre").forEach(pre=>{pre.textContent=""});
  });
}

function showReveal(rootId,secret){
  clearReveal();
  state.reveal=secret;
  const root=$(rootId);
  root.hidden=false;
  const field=root.querySelector("[data-secret]");
  if(field)field.value=secret;
}

async function copyText(text){
  try{await navigator.clipboard.writeText(text);toast(`Copied ${text}`)}
  catch{toast("Copying is blocked here; select the text instead",true)}
}

async function copyReveal(){
  if(!state.reveal)return;
  try{await navigator.clipboard.writeText(state.reveal);toast("Copied")}
  catch{const field=document.querySelector("[data-reveal]:not([hidden]) [data-secret]");if(field)field.select();toast("Copy the selected value",true)}
}

async function refresh(opts={}){
  const snapshot=auth();
  try{
    if(!opts.keepReveal)clearReveal();
    let data;
    try{data={...await api("/api/v1/overview"),role:"operator"}}
    catch(error){
      if(error.status!==401)throw error;
      const [choices,jobs]=await Promise.all([api("/api/v1/job-options"),api("/api/v1/jobs")]);
      data={...choices,jobs,version:"",nodes:[],endpoints:[],projects:[],api_tokens:[],occupancy:[]};
    }
    if(!live(snapshot)||!state.token)throw staleError();
    state.data=data;
    $("unlock").hidden=true;$("application").hidden=false;
    $("status-text").textContent=state.data.role==="developer"?"Connected · Jobs access":`Connected · v${state.data.version}`;document.querySelector(".status").classList.add("online");
    for(const name of ["nodes","models","connections","access"])document.querySelector(`[data-tab="${name}"]`).hidden=state.data.role!=="operator";
    render();
    syncNavigation();
  }catch(error){
    if(error.stale||sessionExpired(snapshot.session))throw error.stale?error:staleError();
    document.querySelector(".status").classList.remove("online");$("status-text").textContent="Not connected";throw error
  }
}

function canJob(permission){return state.data.role==="operator"||(state.data.permissions||[]).includes(permission)}

function render(){
  const d=state.data;
  renderSummary();
  for(const [id,items,label] of [["job-project",d.projects,x=>x.name],["job-model",d.models,x=>x.name],["job-harness",d.harnesses,x=>`${x.name} ${x.version}`]]){
    updateOptions(id,options(items,label));
  }
  $("job-git-hosts").textContent=`Approved repository hosts: ${(d.git_hosts||[]).join(", ")||"None configured. Ask the operator or upload a folder."}`;
  $("job-network").disabled=!d.outbound_network_allowed;
  $("job-network-hint").textContent=d.outbound_network_allowed?"Allows the agent and verification commands to install dependencies and contact the internet and reachable local services.":"Outbound access is disabled by the operator. Checks run without network access; the agent can contact only its assigned model.";
  const verifier=d.verifier;
  $("job-toolchain").textContent=verifier?`Independent checks use CPython ${verifier.python} and uv ${verifier.uv}. Writable exec: ${(verifier.writable_exec_paths||[]).join(", ")}.`:"This controller does not advertise a verifier toolchain.";
  $("job-registered-option").hidden=d.role!=="operator";
  $("job-form").hidden=!canJob("jobs:submit");
  if(d.role!=="operator"&&$("job-source").value==="registered"){$("job-source").value="snapshot";sourceMode()}
  renderJobs();
  if(typeof renderProjects==="function")renderProjects();
  if(typeof renderConnections==="function")renderConnections();
  if(typeof renderModels==="function")renderModels();
  if(typeof renderMachines==="function")renderMachines();
  renderAccess();
}

function occupancyLabel(){
  const occ=state.data.occupancy||[];
  if(!occ.length)return "";
  return "In use · "+occ.map(slot=>{
    const node=state.data.nodes.find(n=>n.id===slot.node_id);
    const name=node?node.name:slot.node_id.slice(0,8);
    const kind=slot.kind==="job"?"durable job":"interactive";
    const job=slot.job_id?` (${slot.job_id.slice(0,8)})`:"";
    const profiles=(slot.profiles||[]).join(", ");
    const who=profiles?`${profiles}`:`${name} / ${slot.runtime_profile}`;
    return `${who} · ${kind}${job}`;
  }).join("; ");
}

function metricCard(label,value,cls){return `<div class="metric${cls?" "+esc(cls):""}"><strong>${esc(value)}</strong><span>${esc(label)}</span></div>`}
function renderSummary(){
  const d=state.data;
  const online=d.nodes.filter(n=>n.status==="online").length;
  if(d.role==="developer"){
    const inProgress=d.jobs.filter(j=>!terminal(j.status)).length;
    $("summary").innerHTML=metricCard("Jobs",d.jobs.length)+metricCard("In progress",inProgress,inProgress?"attention":"");
    $("occupancy-detail").textContent="";
    return;
  }
  const nonRevoked=d.nodes.filter(n=>n.status!=="revoked").length;
  const missingRunner=d.nodes.some(n=>n.status!=="revoked"&&n.status!=="online");
  const queued=d.jobs.filter(j=>!terminal(j.status)&&j.status!=="running").length;
  const running=d.jobs.filter(j=>j.status==="running").length;
  const disabledModels=d.models.filter(m=>m.retirement?.disabled).length;
  $("summary").innerHTML=[
    metricCard("Controller",`v${d.version}`),
    metricCard("Endpoints",d.endpoints.length),
    metricCard("Profiles",disabledModels?`${d.models.length} (${disabledModels} out)`:d.models.length,disabledModels?"ended":""),
    metricCard("Machines",`${online} / ${nonRevoked}`,missingRunner?"warn":""),
    metricCard("Queued",queued,queued?"attention":""),
    metricCard("Running",running,running?"attention":""),
  ].join("");
  $("occupancy-detail").textContent=occupancyLabel();
}

function renderJobs(){
  const list=$("job-list"),jobs=state.data.jobs;
  if(!jobs.length){emptyState(list,"No jobs yet.","Submit one on the left to get started.");return}
  list.className="list";list.innerHTML=jobs.map(job=>`<button type="button" class="item${job.status==="running"?" in-flight":""}" data-job="${esc(job.id)}">${rowIndicator(jobBucket(job.status))}<strong>${esc(job.prompt.slice(0,90))}</strong><div class="meta">${jobBadge(job.status)}${verificationBadge(job)}<span>${new Date(job.created_at).toLocaleString()}</span><span class="mono">${esc(job.id.slice(0,8))}</span></div></button>`).join("");
  list.querySelectorAll("[data-job]").forEach(item=>item.onclick=()=>showJob(item.dataset.job));
}

async function showJob(id){
  const newlySelected=state.selectedJob!==id;
  const snapshot=auth();
  state.selectedJob=id;clearTimeout(state.jobTimer);
  try{
    const [job,events]=await Promise.all([api(`/api/v1/jobs/${id}`),api(`/api/v1/jobs/${id}/events`)]);
    if(!live(snapshot)||!state.data||state.selectedJob!==id)return;
    const index=state.data.jobs.findIndex(item=>item.id===job.id);
    if(index>=0)state.data.jobs[index]=job;else state.data.jobs.unshift(job);
    renderSummary();renderJobs();
    const detail=$("job-detail");detail.hidden=false;
    const result=job.result,verification=result?.provenance?.verification;
    const previous=result?.provenance?.previous_attempts||[],attempt=previous.length+1;
    detail.innerHTML=`<header><div><p class="eyebrow">JOB <code class="mono">${esc(job.id)}</code></p><h3>${jobBadge(job.status)} ${esc(job.prompt.slice(0,140))}</h3></div>${terminal(job.status)||!canJob("jobs:cancel")?"":`<div class="actions"><button id="cancel-job">Cancel</button></div>`}</header>
      ${job.error?`<p class="check-fail">${esc(job.error.message||job.error.code)}</p>`:""}
      ${result?`<div class="result-summary"><strong>${verification?.status==="passed"?"Checks passed":verification?.status==="failed"?"Checks failed":"Unverified"}</strong><span>${attempt} attempt${attempt===1?"":"s"}</span><span>${result.cleanup?.containers_removed&&result.cleanup?.attempt_directory_removed?"Working files removed":"See cleanup details"}</span></div>${verificationDetails(verification)}${patchDetails(result.patches||[],attempt)}
      ${previous.length?`<details><summary>Earlier attempts (${previous.length})</summary>${previous.map(item=>`<h4>Attempt ${esc(item.number)}</h4>${verificationDetails(item.verification)}${patchDetails(item.patches||[],item.number)}<details><summary>Agent output</summary><pre>${esc(item.output)}</pre></details>`).join("")}</details>`:""}
      <details><summary>Agent output</summary><pre>${esc(result.output||"No output retained.")}</pre></details>`:""}
      <details ${terminal(job.status)?"":"open"}><summary>Timeline</summary><div class="list">${events.map(e=>`<div class="item"><strong>${esc(e.message)}</strong><div class="meta"><span>${new Date(e.created_at).toLocaleTimeString()}</span></div></div>`).join("")}</div></details>
      <details><summary>Job details</summary><h4>Request</h4><pre>${esc(job.prompt)}</pre><h4>Checks and permissions</h4><pre>${esc(JSON.stringify(job.policy||{},null,2))}</pre>${job.error?`<h4>Error details</h4><pre>${esc(JSON.stringify(job.error,null,2))}</pre>`:""}${result?`<h4>Provenance</h4><pre>${esc(JSON.stringify({...result.provenance,previous_attempts:undefined},null,2))}</pre><h4>Cleanup</h4><pre>${esc(JSON.stringify(result.cleanup,null,2))}</pre>`:""}</details>`;
    detail.querySelectorAll("[data-patch]").forEach(button=>button.onclick=()=>downloadPatch(job.id,button.dataset.patch,button.dataset.attempt));
    $("cancel-job")?.addEventListener("click",async()=>{await api(`/api/v1/jobs/${id}/cancel`,{method:"POST",body:"{}"});toast("Cancellation requested");await refresh();await showJob(id)});
    if(newlySelected)detail.scrollIntoView({behavior:"smooth",block:"start"});
    if(!terminal(job.status)&&state.selectedJob===id&&live(snapshot))state.jobTimer=setTimeout(()=>showJob(id),2000);
  }catch(error){fail(error)}
}

function verificationDetails(report){
  if(!report)return `<p class="hint">No independent verification was recorded.</p>`;
  return `<p>${esc(report.reason||"No verification commands were supplied.")}</p>${report.changed_protected_paths?.length?`<p class="check-fail">Changed protected paths: ${esc(report.changed_protected_paths.join(", "))}</p>`:""}${report.flagged_paths?.length?`<p class="check-fail">Review before applying; these change how Git applies or checks out files: ${esc(report.flagged_paths.join(", "))}</p>`:""}${(report.commands||[]).map(check=>`<details><summary>${check.exit_code===0&&!check.truncated&&!check.error?"Passed":"Failed"}: ${esc(check.command)}</summary><p class="hint">${esc(check.cwd||"Starting folder")} · ${esc(check.duration_ms)} ms · Exit ${esc(check.exit_code??"unavailable")}${check.truncated?" · Output truncated":""}</p>${check.error?`<p class="check-fail">${esc(check.error)}</p>`:""}<pre>${esc(check.output||"No output.")}</pre></details>`).join("")}`;
}
function patchDetails(patches,attempt){
  if(!patches.some(patch=>patch.patch))return `<p class="hint">No code changes were retained for this attempt.</p>`;
  return patches.map((patch,index)=>patch.patch?`<section class="patch"><div class="patch-heading"><h4>${esc(patch.root)}</h4><button class="secondary" data-patch="${index}" data-attempt="${esc(attempt)}">Download patch</button></div><p class="hint">Base revision: ${esc(patch.base_revision)}</p><details open><summary>View changes</summary><pre class="diff">${patch.patch.split("\n").map(line=>`<span class="${line.startsWith("+")?"diff-add":line.startsWith("-")?"diff-remove":""}">${esc(line)}</span>`).join("\n")}</pre></details></section>`:"").join("");
}
async function downloadPatch(id,index,attempt){
  const snapshot=auth();
  try{
    const {response}=await authorized(`/api/v1/jobs/${encodeURIComponent(id)}/patches/${index}?attempt=${encodeURIComponent(attempt)}`);
    if(!response.ok)throw new Error(`Patch download failed (${response.status}).`);
    const blob=await response.blob();
    if(!live(snapshot))throw staleError();
    const url=URL.createObjectURL(blob),link=document.createElement("a");
    link.href=url;link.download=`poundlock-${id.slice(0,8)}-attempt-${attempt}-root-${index}.patch`;link.click();setTimeout(()=>URL.revokeObjectURL(url),1000);
  }catch(error){fail(error)}
}

// Managed-identity statuses reported by the controller: active, revoked,
// provision_failed. Anything else is unrecognised.
const IDENTITY_STATE={active:["ok","Active"],revoked:["ended","Revoked"],provision_failed:["fault","Provisioning failed"]};
function identityBadge(status){const [cls,label]=IDENTITY_STATE[status]||["unrecognised",status];return badge(cls,label)}
function renderAccess(){
  const list=$("api-token-list"),tokens=state.data.api_tokens||[];
  if(!tokens.length){emptyState(list,"No application tokens.","Create one below for another editor or integration.")}
  else{
    list.className="list";list.innerHTML=tokens.map(token=>`<div class="item">${rowIndicator(token.revoked_at?"ended":"ok")}<strong>${esc(token.name)}</strong><div class="meta">${badge(token.revoked_at?"ended":"ok",token.revoked_at?"Revoked":"Active")}<span>${esc(token.permissions.join(", "))}</span></div><div class="meta"><span>Created ${new Date(token.created_at).toLocaleString()}</span><span>${token.last_used_at?`Last used ${new Date(token.last_used_at).toLocaleString()}`:"Never used"}</span></div>${token.revoked_at?"":`<div class="item-actions"><button class="secondary" data-revoke-token="${esc(token.id)}">Revoke</button></div>`}</div>`).join("");
    list.querySelectorAll("[data-revoke-token]").forEach(button=>button.onclick=async()=>{if(!confirm("Revoke this token? The application using it loses access immediately, its streams stop, and its queued and running jobs are cancelled."))return;try{await api(`/api/v1/api-tokens/${button.dataset.revokeToken}/revoke`,{method:"POST",body:"{}"});toast("Token revoked");await refresh()}catch(error){fail(error)}});
  }
  const identities=state.data.managed_identities||[],managedList=$("managed-identity-list");
  if(!identities.length){emptyState(managedList,"No managed identities.","They appear once a gateway with separate application identities is configured.")}
  else{
    managedList.className="list";managedList.innerHTML=identities.map(identity=>{const token=tokens.find(item=>item.id===identity.api_token_id),endpoint=(state.data.endpoints||[]).find(item=>item.id===identity.endpoint_id);return `<div class="item"><strong>${esc(token?.name||"Removed application token")}</strong><div class="meta">${identityBadge(identity.status)}<span>${esc(endpoint?.name||"Removed gateway")}</span></div>${identity.last_error?`<p class="check-fail">${esc(identity.last_error)}</p>`:""}<div class="meta"><span>Updated ${new Date(identity.updated_at).toLocaleString()}</span></div></div>`}).join("");
  }
}


function checkLabel(item){
  if(item.last_check_ok===true)return `<span class="check-ok">Passed${item.last_check_message?": "+esc(item.last_check_message):""}</span>${item.last_checked_at?`<div class="meta"><span>${new Date(item.last_checked_at).toLocaleString()}</span></div>`:""}`;
  if(item.last_check_ok===false)return `<span class="check-fail">Failed${item.last_check_message?": "+esc(item.last_check_message):""}</span>${item.last_checked_at?`<div class="meta"><span>${new Date(item.last_checked_at).toLocaleString()}</span></div>`:""}`;
  return `<span class="check-none">Not checked</span>`;
}

$("token-form").addEventListener("submit",async event=>{event.preventDefault();const session=beginSession();state.token=$("token").value;try{await refresh();if(sessionExpired(session))return;localStorage.setItem("poundlock-token",state.token);localStorage.removeItem("rcharm-token")}catch(error){if(error.stale||sessionExpired(session))return;if($("application").hidden)state.token="";toast(error.message,true)}});
$("refresh").onclick=async()=>{try{await refresh();if(state.selectedJob)await showJob(state.selectedJob)}catch(error){fail(error)}};
$("sign-out").onclick=()=>{
  beginSession();
  clearTimeout(state.jobTimer);clearInterval(state.nodePoll);
  state.token="";state.data=null;state.selectedJob=null;state.page=null;state.nodePoll=null;state.jobTimer=null;
  localStorage.removeItem("poundlock-token");localStorage.removeItem("rcharm-token");
  clearReveal();
  $("application").hidden=true;$("unlock").hidden=false;$("token").value="";
  const detail=$("job-detail");if(detail){detail.hidden=true;detail.innerHTML=""}
  document.querySelector(".status").classList.remove("online");$("status-text").textContent="Not connected";
};
function syncNavigation(){
  if(!state.data)return;
  const routes={jobs:"jobs",machines:"nodes",models:"models",connections:"connections",access:"access","linux-guide":"nodes"};
  let page=location.hash.slice(1);
  if(!Object.hasOwn(routes,page)||(routes[page]!=="jobs"&&state.data.role!=="operator"))page="jobs";
  if(location.hash!==`#${page}`)history.replaceState(null,"",`#${page}`);
  if(state.page===page)return;
  state.page=page;
  const tab=routes[page];
  document.querySelectorAll("[data-tab]").forEach(button=>{
    const active=button.dataset.tab===tab;
    button.classList.toggle("active",active);
    if(active)button.setAttribute("aria-current","page");else button.removeAttribute("aria-current");
  });
  document.querySelectorAll(".tab").forEach(section=>section.classList.toggle("active",section.id===tab));
  clearReveal();
  clearInterval(state.nodePoll);state.nodePoll=null;
  if(tab==="nodes")state.nodePoll=setInterval(()=>refresh({keepReveal:true}).catch(()=>{}),4000);
  if(page==="linux-guide"){$("linux-guide").open=true;$("linux-guide").scrollIntoView({block:"start"})}
}
document.querySelectorAll("[data-tab]").forEach(button=>button.onclick=()=>{
  location.hash=button.dataset.tab==="nodes"?"machines":button.dataset.tab;
});
window.addEventListener("hashchange",syncNavigation);
$("api-token-form").addEventListener("submit",async event=>{event.preventDefault();const snapshot=auth();const permissions=[...document.querySelectorAll('[name="api-permission"]:checked')].map(input=>input.value);try{const issued=await api("/api/v1/api-tokens",{method:"POST",body:JSON.stringify({name:$("api-token-name").value,permissions})});if(!live(snapshot))throw staleError();event.target.reset();await refresh();if(!live(snapshot))throw staleError();showReveal("api-token-issued",issued.token);toast("Token created")}catch(error){fail(error)}});
$("copy-api-token").onclick=()=>copyReveal();
$("copied-api-token").onclick=()=>{clearReveal();toast("Token cleared from this page")};
$("reconcile-managed-identities").onclick=async event=>{const button=event.currentTarget;button.disabled=true;try{const result=await api("/api/v1/managed-identities/reconcile",{method:"POST",body:"{}"});await refresh();toast(result.pending?`${result.pending} revocation${result.pending===1?" is":"s are"} still pending`:`Reconciled ${result.reconciled} managed identit${result.reconciled===1?"y":"ies"}`)}catch(error){fail(error)}finally{button.disabled=false}};
