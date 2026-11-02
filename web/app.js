// A token is kept for this tab only, unless the operator asks to remember it on this device.
function storedToken(){return localStorage.getItem("poundlock-token")||sessionStorage.getItem("poundlock-token")||""}
function forgetToken(){localStorage.removeItem("poundlock-token");sessionStorage.removeItem("poundlock-token")}
function storeToken(token,remember){forgetToken();(remember?localStorage:sessionStorage).setItem("poundlock-token",token)}
// Browsers and password managers offer to save any password field, so where text can
// be masked the token field is masked text instead.
if(CSS.supports("-webkit-text-security","disc")){document.getElementById("token").type="text";document.getElementById("token").classList.add("masked")}
const state={token:storedToken(),data:null,selectedJob:null,jobTimer:null,reveal:null,nodePoll:null,nodePolling:false,page:null,session:0};
const $=id=>document.getElementById(id);
const esc=value=>String(value??"").replace(/[&<>"']/g,c=>({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[c]));
const terminal=status=>["completed","failed","cancelled"].includes(status);
function beginSession(){state.session+=1;clearPendingJob();return state.session}
function sessionExpired(session){return session!==state.session}
function auth(){return {session:state.session,token:state.token}}
function live(snapshot){return !sessionExpired(snapshot.session)&&state.token===snapshot.token}
function staleError(){const error=new Error("Session ended");error.stale=true;return error}
function fail(error){if(!error?.stale)toast(error.message,true)}

async function api(path,options={}){
  const snapshot=auth();
  const response=await fetch(path,{...options,headers:{"content-type":"application/json","authorization":`Bearer ${snapshot.token}`,...options.headers}});
  const body=await response.json().catch(()=>({error:{message:response.ok?"Invalid server response":`Can't reach the controller (${response.status})`}}));
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

// An error in an open drawer sits above its footer, in the form's flow: fixed over the drawer it
// covered the fields it was about, and its Dismiss was outside the focus trap (UX audit, 2 Nov).
function seatToast(){
  const el=$("toast"),drawer=el.classList.contains("error")?document.querySelector(".drawer:not([hidden])"):null;
  if(drawer)drawer.querySelector(".drawer-footer").before(el);
  else $("model-drawer-backdrop").before(el);
}
// An error stays until it is dismissed or replaced, so it can be read; it is an alert,
// and its Dismiss button reaches a keyboard (UX audit, 30 Oct).
function toast(message,error=false){
  const el=$("toast");
  el.setAttribute("role",error?"alert":"status");el.className=error?"show error":"show";seatToast();
  el.replaceChildren(document.createTextNode(message));
  if(error){const close=document.createElement("button");close.type="button";close.className="toast-dismiss";close.textContent="Dismiss";close.onclick=()=>{const inDrawer=el.closest(".drawer");el.className="";seatToast();if(inDrawer)drawerFocusable().at(-1)?.focus()};el.append(" ",close)}
  clearTimeout(toast.timer);if(!error)toast.timer=setTimeout(()=>el.className="",3500);
}
// Scrolls glide only for those who have not asked the system for less motion.
function motion(){return matchMedia("(prefers-reduced-motion: reduce)").matches?"auto":"smooth"}
// A hint right after a label is that field's description, read out with it: 53 hints
// were tied to nothing (UX audit, 30 Oct). A label holding a radio group is left alone.
document.querySelectorAll(".hint").forEach((hint,index)=>{
  const label=hint.previousElementSibling;
  if(label?.tagName!=="LABEL")return;
  const controls=label.querySelectorAll("input:not([type=radio]),select,textarea");
  if(controls.length!==1)return;
  hint.id||=`hint-${index}`;
  const control=controls[0],described=control.getAttribute("aria-describedby");
  if(!described?.split(" ").includes(hint.id))control.setAttribute("aria-describedby",[described,hint.id].filter(Boolean).join(" "));
});
// A rebuild keeps the operator's place: the focused control, found again by its
// data attribute (or its details' for a summary), and each scrolled box's offset.
function focusKey(root){
  const el=document.activeElement;if(!el||el===root||!root.contains(el))return null;
  const owner=el.tagName==="SUMMARY"?el.parentElement:el,attr=[...owner.attributes].find(a=>a.name.startsWith("data-"));
  if(!attr)return null;
  const key=`${owner.tagName.toLowerCase()}[${attr.name}="${CSS.escape(attr.value)}"]`;
  return el.tagName==="SUMMARY"?`${key} > summary`:key;
}
function keepPlace(root){return {focus:focusKey(root),scrolls:[...root.querySelectorAll("pre,.list")].map(el=>el.scrollTop)}}
function restorePlace(root,place){
  root.querySelectorAll("pre,.list").forEach((el,index)=>{if(place.scrolls[index])el.scrollTop=place.scrolls[index]});
  if(place.focus)root.querySelector(place.focus)?.focus({preventScroll:true});
}
function options(items,label){return items.map(item=>`<option value="${esc(item.id)}">${esc(label(item))}</option>`).join("")}
function updateOptions(id,html,selected=$(id).value){
  const select=$(id),next=document.createElement("select");
  // Rebuilding an unchanged list on a refresh would close it while it is open.
  next.innerHTML=html;
  if(next.innerHTML===select.innerHTML&&select.value===selected)return;
  select.innerHTML=html;
  select.setCustomValidity("");
  if(!selected){if([...select.options].some(option=>option.value===""))select.value="";return}
  if([...select.options].some(option=>option.value===selected)){select.value=selected;return}
  const missing=new Option("Selection unavailable — choose another",selected,true,true);
  missing.disabled=true;select.add(missing);
  select.setCustomValidity("This selection is no longer available. Choose another.");
}
document.addEventListener("change",event=>{if(event.target instanceof HTMLSelectElement)event.target.setCustomValidity("")});
// The browser marks a field that fails validation on screen; this tells a screen reader too.
document.addEventListener("invalid",event=>event.target.setAttribute("aria-invalid","true"),true);
for(const type of ["input","change"])document.addEventListener(type,event=>{if(event.target.validity?.valid)event.target.removeAttribute("aria-invalid")});
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
  // List rows leave the result out; the controller sends its verification status.
  const status=job.verification;
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
    $("status-text").textContent=state.data.role==="developer"?"Connected · Jobs access":`Connected · v${state.data.version}${state.data.build?` · ${state.data.build}`:""}`;$("status-text").title=$("status-text").textContent;document.querySelector(".status").classList.add("online");
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
  // Only models that can run jobs, as job-options offers them.
  const jobModels=(d.models||[]).filter(model=>(model.uses||[]).includes("durable"));
  for(const [id,items,label] of [["job-project",d.projects,x=>x.name],["job-model",jobModels,x=>x.name],["job-harness",d.harnesses,x=>`${x.name} ${x.version}`]]){
    // The model is chosen on purpose, not whichever sorts first.
    const choose=id==="job-model"&&items.length>1?'<option value="">Choose a model</option>':"";
    updateOptions(id,choose+options(items,label));
  }
  // One harness is no choice: shown, not offered.
  $("job-harness").disabled=(d.harnesses||[]).length===1;
  $("job-git-hosts").textContent=`Approved repository hosts: ${(d.git_hosts||[]).join(", ")||"None configured. Ask the operator or upload a folder."}`;
  $("job-network").disabled=!d.outbound_network_allowed;
  $("job-network-hint").textContent=d.outbound_network_allowed?"The agent and verification commands can install dependencies and reach the internet on ports 80 and 443, never your local network or Poundlock's own services.":"Internet access is turned off by the operator. Checks run without network access; the agent can contact only its assigned model.";
  const verifier=d.verifier;
  $("job-toolchain").textContent=verifier?`Independent checks run with Node.js, CPython ${verifier.python} and uv ${verifier.uv}. Writable exec: ${(verifier.writable_exec_paths||[]).join(", ")}.`:"This controller does not say which tools its checks run with.";
  $("job-registered-option").hidden=d.role!=="operator";
  $("job-form").hidden=!canJob("jobs:submit");
  if(d.role!=="operator"&&$("job-source").value==="registered"){$("job-source").value="snapshot";sourceMode()}
  renderJobs();
  renderProjects();
  renderConnections();
  renderModels();
  renderMachines();
  renderAccess();
}

function occupancyLabel(){
  const occ=state.data.occupancy||[];
  if(!occ.length)return "";
  return "In use · "+occ.map(slot=>{
    const node=state.data.nodes.find(n=>n.id===slot.node_id);
    const name=node?node.name:slot.node_id.slice(0,8);
    const kind=slot.kind==="job"?"running a job":"serving a request";
    const job=slot.job_id?` (${slot.job_id.slice(0,8)})`:"";
    // A compatibility alias names the same profile; show the profile's own name once.
    const named=new Map((state.data.models||[]).flatMap(model=>[[model.name,model.name],...(model.aliases||[]).map(alias=>[alias,model.name])]));
    const profiles=[...new Set((slot.profiles||[]).map(name=>named.get(name)||name))].join(", ");
    const who=profiles?`${profiles}`:`${name} / ${slot.runtime_profile}`;
    return `${who} · ${kind}${job}`;
  }).join("; ");
}

function metricCard(label,value,cls,page){const tag=page?"a":"div";return `<${tag} class="metric${cls?" "+esc(cls):""}"${page?` href="#${esc(page)}"`:""}><strong>${esc(value)}</strong><span>${esc(label)}</span></${tag}>`}
function renderSummary(){
  const d=state.data;
  const online=d.nodes.filter(n=>n.status==="online").length;
  if(d.role==="developer"){
    const inProgress=d.jobs.filter(j=>!terminal(j.status)).length;
    $("summary").innerHTML=metricCard("Jobs",d.jobs.length,"","jobs")+metricCard("In progress",inProgress,inProgress?"attention":"","jobs");
    $("occupancy-detail").textContent="";
    return;
  }
  const nonRevoked=d.nodes.filter(n=>n.status!=="revoked").length;
  const missingRunner=d.nodes.some(n=>n.status!=="revoked"&&n.status!=="online");
  const queued=d.jobs.filter(j=>!terminal(j.status)&&j.status!=="running").length;
  const running=d.jobs.filter(j=>j.status==="running").length;
  const disabledModels=d.models.filter(m=>m.retirement?.disabled).length;
  $("summary").innerHTML=[
    metricCard("Machines online",`${online} / ${nonRevoked}`,missingRunner?"warn":"","machines"),
    metricCard("Models",disabledModels?`${d.models.length} (${disabledModels} out)`:d.models.length,disabledModels?"ended":"","models"),
    metricCard("Servers and gateways",d.endpoints.length,"","connections"),
    metricCard("Queued",queued,queued?"attention":"","jobs"),
    metricCard("Running",running,running?"attention":"","jobs"),
  ].join("");
  $("occupancy-detail").textContent=$("occupancy-detail").title=occupancyLabel();
}

// Nothing tests in the background, so a pass older than a day is history, not health.
function checkFresh(item){return Boolean(item.last_checked_at)&&Date.now()-new Date(item.last_checked_at)<86400000}
function checkIndicator(item){return item.last_check_ok===false?"fault":item.last_check_ok===true&&checkFresh(item)?"ok":"unknown"}
function checkLabel(item){
  if(item.last_check_ok===true)return `<span class="${checkFresh(item)?"check-ok":"check-none"}">${checkFresh(item)?"Test passed":"Last test passed"}${item.last_check_message?": "+esc(item.last_check_message):""}</span>${item.last_checked_at?`<div class="meta"><span>${new Date(item.last_checked_at).toLocaleString()}</span></div>`:""}`;
  if(item.last_check_ok===false)return `<span class="check-fail">Test failed${item.last_check_message?": "+esc(item.last_check_message):""}</span>${item.last_checked_at?`<div class="meta"><span>${new Date(item.last_checked_at).toLocaleString()}</span></div>`:""}`;
  return `<span class="check-none">Not tested yet</span>`;
}

$("token-form").addEventListener("submit",async event=>{event.preventDefault();const session=beginSession();state.token=$("token").value;try{await refresh();if(sessionExpired(session))return;storeToken(state.token,$("remember-token").checked)}catch(error){if(error.stale||sessionExpired(session))return;if($("application").hidden)state.token="";toast(error.message,true)}});
$("refresh").onclick=async()=>{try{await refresh();if(state.selectedJob)await showJob(state.selectedJob)}catch(error){fail(error)}};
$("sign-out").onclick=()=>{
  beginSession();
  clearTimeout(state.jobTimer);clearInterval(state.nodePoll);
  state.token="";state.data=null;state.selectedJob=null;state.page=null;state.nodePoll=null;state.jobTimer=null;
  forgetToken();
  clearReveal();
  $("application").hidden=true;$("unlock").hidden=false;$("token").value="";
  const detail=$("job-detail");if(detail){detail.hidden=true;detail.innerHTML=""}
  document.querySelector(".status").classList.remove("online");$("status-text").textContent="Not connected";
};
function syncNavigation(){
  if(!state.data)return;
  const routes={jobs:"jobs",machines:"nodes",models:"models",connections:"connections",access:"access","linux-guide":"nodes"};
  const [requested,jobLink]=location.hash.slice(1).split("/");
  let page=requested;
  if(!Object.hasOwn(routes,page)||(routes[page]!=="jobs"&&state.data.role!=="operator"))page="jobs";
  const hash=page==="jobs"&&jobLink?`#jobs/${jobLink}`:`#${page}`;
  if(location.hash!==hash)history.replaceState(null,"",hash);
  if(page==="jobs"&&jobLink&&state.selectedJob!==jobLink)showJob(jobLink);
  if(state.page===page)return;
  const previous=state.page;
  state.page=page;
  const tab=routes[page];
  document.querySelectorAll("[data-tab]").forEach(button=>{
    const active=button.dataset.tab===tab;
    button.classList.toggle("active",active);
    if(active){button.setAttribute("aria-current","page");document.title=`${button.textContent.trim()} · poundlock`}
    else button.removeAttribute("aria-current");
  });
  document.querySelectorAll(".tab").forEach(section=>section.classList.toggle("active",section.id===tab));
  clearReveal();
  clearInterval(state.nodePoll);state.nodePoll=null;
  // One poll at a time, and none while the tab is hidden: each read the whole overview (UX audit, 30 Oct).
  if(tab==="nodes")state.nodePoll=setInterval(()=>{
    if(document.hidden||state.nodePolling)return;
    state.nodePolling=true;
    refresh({keepReveal:true}).catch(()=>{}).finally(()=>{state.nodePolling=false});
  },4000);
  // A tab shows what is true now, not what it showed when last opened (UX audit, Oct 2026).
  else if(previous)refresh().catch(()=>{});
  if(page==="linux-guide"){$("linux-guide").open=true;$("linux-guide").scrollIntoView({block:"start"})}
}
document.querySelectorAll("[data-tab]").forEach(button=>button.onclick=()=>{
  location.hash=button.dataset.tab==="nodes"?"machines":button.dataset.tab;
});
window.addEventListener("hashchange",syncNavigation);
