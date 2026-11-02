let sourceFiles=[],pendingJob=null,pendingSession=null;
function clearPendingJob(){pendingJob=null;pendingSession=null}
const ignoredParts=new Set(["node_modules","target",".venv","venv","__pycache__","dist","build"]);
function jobPolicy(){
  const lines=id=>$(id).value.split(/\r?\n/).map(value=>value.trim()).filter(Boolean);
  const commands=lines("job-commands").map(command=>({command,cwd:$("job-check-cwd").value.trim(),timeout_seconds:Number($("job-check-timeout").value)}));
  const protected_paths=lines("job-protected"),max_repairs=Number($("job-repairs").value);
  if(max_repairs && !commands.length && !protected_paths.length)throw new Error("Add a verification command or protected path before enabling repairs.");
  return {verification:{commands,protected_paths},network:$("job-network").checked?"outbound":"inference_only",max_repairs};
}
function sourceMode(){
  const mode=$("job-source").value;
  $("job-upload").hidden=mode!=="snapshot";
  $("job-git").hidden=mode!=="git";
  for(const name of ["url","revision"]){$("job-git-"+name).required=mode==="git";$("job-git-"+name).disabled=mode!=="git"}
  $("job-project-label").hidden=mode!=="registered";
  $("job-project").disabled=mode!=="registered";
  $("job-project").required=mode==="registered";
}
$("job-source").addEventListener("change",sourceMode);
$("job-form").addEventListener("input",()=>{clearPendingJob()});
$("job-folder").addEventListener("change",()=>{
  sourceFiles=[...$("job-folder").files].map(file=>{
    const path=file.webkitRelativePath.split("/").slice(1).join("/")||file.name;
    const parts=path.split("/");
    return {file,path,included:!parts.some(part=>ignoredParts.has(part)||part===".env"||part.startsWith(".env.")||part.endsWith(".pem")||part.endsWith(".key")),executable:false};
  }).filter(item=>!item.path.split("/").some(part=>part.toLowerCase()===".git")).sort((a,b)=>a.path.localeCompare(b.path));
  $("job-files").innerHTML=sourceFiles.map((item,index)=>`<div><label class="check"><input type="checkbox" data-file="${index}" ${item.included?"checked":""}>${esc(item.path)} (${item.file.size} bytes)</label><label class="check"><input type="checkbox" data-executable="${index}">Executable</label></div>`).join("");
  $("job-file-review").hidden=!sourceFiles.length;
  updateFileSummary();
});
$("job-files").addEventListener("change",event=>{
  const input=event.target;
  if(input.dataset.file!==undefined)sourceFiles[Number(input.dataset.file)].included=input.checked;
  if(input.dataset.executable!==undefined)sourceFiles[Number(input.dataset.executable)].executable=input.checked;
  updateFileSummary();
});
function updateFileSummary(){
  const included=sourceFiles.filter(item=>item.included),bytes=included.reduce((total,item)=>total+item.file.size,0);
  $("job-file-summary").textContent=`${included.length} files selected · ${(bytes/1048576).toFixed(2)} MiB · ${sourceFiles.length-included.length} unchecked`;
}
async function jobSource(){
  const mode=$("job-source").value;
  if(mode==="registered")return {project_id:$("job-project").value};
  if(mode==="empty")return {source:{kind:"empty"}};
  if(mode==="git"){
    const source={kind:"git",repository:$("job-git-url").value.trim(),revision:$("job-git-revision").value.trim()};
    const username=$("job-git-username").value.trim(),token=$("job-git-token").value;
    if(username||token){if(!username||!token)throw new Error("Provide both the Git username and repository-read token, or leave both empty.");source.credentials={username,token}}
    return {source};
  }
  const included=sourceFiles.filter(item=>item.included);
  if(!included.length)throw new Error("Select a folder and include at least one file, or choose an empty workspace.");
  if(included.length>10000||included.reduce((sum,item)=>sum+item.file.size,0)>50*1048576)throw new Error("Selected files exceed the 50 MiB or 10,000-file limit.");
  const files=[];
  for(const item of included){
    const bytes=new Uint8Array(await item.file.arrayBuffer());let binary="";
    for(let start=0;start<bytes.length;start+=32768)binary+=String.fromCharCode(...bytes.subarray(start,start+32768));
    files.push({path:item.path,content_base64:btoa(binary),executable:item.executable});
  }
  return {source:{kind:"snapshot",files}};
}
$("job-form").addEventListener("submit",async event=>{
  event.preventDefault();
  const snapshot=auth();
  const controls=[...event.target.elements].map(element=>[element,element.disabled]);
  try{
    if(pendingJob&&pendingSession!==snapshot.session)clearPendingJob();
    if(!pendingJob){
      const fields={idempotency_key:crypto.randomUUID(),model_profile_id:$("job-model").value,harness_profile_id:$("job-harness").value,prompt:$("job-prompt").value,policy:jobPolicy()};
      controls.forEach(([element])=>element.disabled=true);
      const source=await jobSource();
      if(!live(snapshot))throw staleError();
      pendingJob={...fields,...source};
      pendingSession=snapshot.session;
    }
    if(!live(snapshot))throw staleError();
    controls.forEach(([element])=>element.disabled=true);
    const job=await api("/api/v1/jobs",{method:"POST",body:JSON.stringify(pendingJob)});
    if(!live(snapshot))throw staleError();
    clearPendingJob();sourceFiles=[];event.target.reset();$("job-files").replaceChildren();$("job-file-review").hidden=true;$("job-file-summary").textContent="No folder selected.";
    toast("Job accepted. You can close this page and return later.");await refresh();await showJob(job.id);
  }catch(error){fail(error)}
  finally{controls.forEach(([element,disabled])=>element.disabled=disabled);if(live(snapshot))sourceMode()}
});

sourceMode();

// Server repositories: durable-job project configuration, collapsed under Jobs.
let editingProject=null;
function renderProjects(){
  const list=$("project-list"),projects=state.data.projects||[];
  if(!projects.length){emptyState(list,"No server repositories configured.","Add one below to submit jobs against a repository this controller already has.");return}
  list.className="list";
  list.innerHTML=projects.map(x=>`<div class="item"><strong>${esc(x.name)}</strong><div class="meta"><span>${esc(x.roots.join(", "))}</span></div>${(x.instruction_files||[]).length?`<div class="meta"><span>${esc(x.instruction_files.join(", "))}</span></div>`:""}<div class="item-actions"><button class="secondary" data-edit-project="${esc(x.id)}">Edit</button> <button class="secondary danger" data-remove-project="${esc(x.id)}">Remove</button></div></div>`).join("");
  list.querySelectorAll("[data-edit-project]").forEach(button=>button.onclick=()=>editProject(button.dataset.editProject));
  list.querySelectorAll("[data-remove-project]").forEach(button=>button.onclick=async()=>{
    const id=button.dataset.removeProject,item=projects.find(x=>x.id===id);
    if(!confirm(`Remove ${item.name}? Existing jobs keep their saved settings and history.`))return;
    button.disabled=true;
    try{await api(`/api/v1/projects/${encodeURIComponent(id)}`,{method:"DELETE"});if(editingProject===id)resetProject();await refresh();toast("Removed")}
    catch(error){fail(error);button.disabled=false}
  });
}
function resetProject(){editingProject=null;$("project-form").reset();$("project-save").textContent="Save project";$("project-cancel").hidden=true}
function editProject(id){
  const item=state.data.projects.find(x=>x.id===id);if(!item)return;resetProject();editingProject=id;
  $("project-name").value=item.name;$("project-roots").value=item.roots.join("\n");$("project-instructions").value=(item.instruction_files||[]).join("\n");
  $("project-save").textContent="Save changes";$("project-cancel").hidden=false;
  $("project-panel").open=true;$("project-name").focus();$("project-panel").scrollIntoView({block:"start",behavior:motion()});
}
$("project-cancel").onclick=resetProject;
$("project-form").addEventListener("submit",async event=>{
  event.preventDefault();const button=$("project-save");button.disabled=true;
  const body={name:$("project-name").value,roots:$("project-roots").value.split("\n").map(x=>x.trim()).filter(Boolean),instruction_files:$("project-instructions").value.split("\n").map(x=>x.trim()).filter(Boolean)};
  try{await api("/api/v1/projects"+(editingProject?"/"+encodeURIComponent(editingProject):""),{method:editingProject?"PATCH":"POST",body:JSON.stringify(body)});resetProject();await refresh();toast("Project saved")}
  catch(error){fail(error)}finally{button.disabled=false}
});

// Pi's JSON event stream (docs/json.md) read as what the agent said and did.
// The harness appends the agent's stderr after a separator (harness.rs), and
// trimming or the runner's "[poundlock] …" notes leave lines that are not
// events: those are shown as notes. Output with no events at all shows as it is.
function agentOutput(output){
  if(!output)return "<pre>No output retained.</pre>";
  const [stdout,stderr=""]=output.split("\n--- stderr ---\n");
  const steps=[];let events=0;
  for(const line of stdout.split("\n").filter(line=>line.trim())){
    let event;try{event=JSON.parse(line);events++}catch{
      steps.push(`<p class="hint">${line.startsWith("{")?"An event cut off where the output was trimmed.":esc(line.replace(/^\[poundlock\] /,"").slice(0,300))}</p>`);
      continue;
    }
    const message=event?.type==="message_end"?event.message:null;
    if(message?.role==="assistant")for(const part of message.content||[]){
      if(part.type==="text"&&part.text?.trim())steps.push(`<p>${esc(part.text.trim())}</p>`);
      if(part.type==="toolCall")steps.push(`<p class="hint mono">→ ${esc(part.name)} ${esc(String(Object.values(part.arguments||{})[0]??"").slice(0,160))}</p>`);
    }
    if(message?.role==="toolResult"&&message.isError)steps.push(`<p class="check-fail">${esc(message.toolName||"Tool")} failed: ${esc((message.content||[]).map(part=>part.text||"").join(" ").slice(0,300))}</p>`);
  }
  if(!events)return `<pre>${esc(output)}</pre>`;
  const errors=stderr.trim()?`<details><summary>The agent's error output</summary><pre>${esc(stderr)}</pre></details>`:"";
  return `${steps.join("")||'<p class="hint">The agent said nothing and called no tools.</p>'}${errors}<details><summary>Raw event stream</summary><pre>${esc(stdout)}</pre></details>`;
}
// A prompt's first line, without the Markdown heading marks a brief starts with.
function jobTitle(prompt){return (prompt.split("\n").find(line=>line.trim())||prompt).replace(/^\s*#+\s*/,"").slice(0,90)}
function renderJobs(){
  const list=$("job-list"),jobs=state.data.jobs;
  if(!jobs.length){emptyState(list,"No jobs yet.","Submit one with the form beside this list.");return}
  list.className="list";list.innerHTML=jobs.map(job=>`<button type="button" class="item${job.status==="running"?" in-flight":""}${job.id===state.selectedJob?" selected":""}" data-job="${esc(job.id)}"${job.id===state.selectedJob?' aria-current="true"':""}>${rowIndicator(jobBucket(job.status))}<strong>${esc(jobTitle(job.prompt))}</strong><div class="meta">${jobBadge(job.status)}${verificationBadge(job)}<span>${new Date(job.created_at).toLocaleString()}</span><span class="mono">${esc(job.id.slice(0,8))}</span></div></button>`).join("");
  list.querySelectorAll("[data-job]").forEach(item=>item.onclick=()=>showJob(item.dataset.job));
  if(jobs.length>=state.data.jobs_limit)list.insertAdjacentHTML("beforeend",`<p class="hint">Showing the newest ${esc(state.data.jobs_limit)} jobs. Older jobs are kept and can be read through the API.</p>`);
}

let renderedJob={id:null,html:""};
function closeJob(){
  const closed=state.selectedJob;
  clearTimeout(state.jobTimer);state.selectedJob=null;renderedJob={id:null,html:""};
  const detail=$("job-detail");detail.hidden=true;detail.innerHTML="";
  history.replaceState(null,"","#jobs");renderJobs();
  // Focus goes back to the job's row rather than to the page.
  if(closed)document.querySelector(`[data-job="${CSS.escape(closed)}"]`)?.focus();
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
    // A poll keeps what the reader opened and leaves a selection alone.
    const selection=getSelection();
    if(!newlySelected&&selection&&!selection.isCollapsed&&detail.contains(selection.anchorNode)){
      if(!terminal(job.status))state.jobTimer=setTimeout(()=>showJob(id),2000);
      return;
    }
    const opened=newlySelected?null:new Map([...detail.querySelectorAll("details")].map(item=>[item.querySelector("summary")?.textContent,item.open]));
    const result=job.result,verification=result?.provenance?.verification;
    const previous=result?.provenance?.previous_attempts||[],attempt=previous.length+1;
    const html=`<header><div><p class="eyebrow">JOB <code class="mono">${esc(job.id)}</code></p><h3>${jobBadge(job.status)} ${esc(jobTitle(job.prompt))}</h3></div><div class="actions">${terminal(job.status)||!canJob("jobs:cancel")?"":`<button id="cancel-job">Cancel</button>`}<button id="close-job" type="button" class="secondary">Close</button></div></header>
      ${job.error?`<p class="check-fail">${esc(job.error.message||job.error.code)}</p>`:""}
      ${result?`<div class="result-summary"><strong>${verification?.status==="passed"?"Checks passed":verification?.status==="failed"?"Checks failed":"Unverified"}</strong><span>${attempt} attempt${attempt===1?"":"s"}</span><span>${result.cleanup?.containers_removed&&result.cleanup?.attempt_directory_removed?"Working files removed":"See cleanup details"}</span></div>${verificationDetails(verification)}${patchDetails(result.patches||[],attempt)}
      ${previous.length?`<details><summary>Earlier attempts (${previous.length})</summary>${previous.map(item=>`<h4>Attempt ${esc(item.number)}</h4>${verificationDetails(item.verification)}${patchDetails(item.patches||[],item.number)}<details><summary>Agent output</summary>${agentOutput(item.output)}</details>`).join("")}</details>`:""}
      <details><summary>Agent output</summary>${agentOutput(result.output)}</details>`:""}
      <details ${terminal(job.status)?"":"open"}><summary>Timeline</summary><div class="list">${events.map(e=>`<div class="item"><strong>${esc(e.message)}</strong><div class="meta"><span>${new Date(e.created_at).toLocaleTimeString()}</span></div></div>`).join("")}</div></details>
      <details><summary>Job details</summary><h4>Request</h4><pre>${esc(job.prompt)}</pre><h4>Checks and permissions</h4><pre>${esc(JSON.stringify(job.policy||{},null,2))}</pre>${job.error?`<h4>Error details</h4><pre>${esc(JSON.stringify(job.error,null,2))}</pre>`:""}${result?`<h4>Provenance</h4><pre>${esc(JSON.stringify({...result.provenance,previous_attempts:undefined},null,2))}</pre><h4>Cleanup</h4><pre>${esc(JSON.stringify(result.cleanup,null,2))}</pre>`:""}</details>`;
    // Unchanged, the detail is left as it is; changed, it keeps the reader's focus and scroll.
    if(!newlySelected&&renderedJob.id===id&&renderedJob.html===html){
      if(!terminal(job.status))state.jobTimer=setTimeout(()=>showJob(id),2000);
      return;
    }
    const place=newlySelected?null:keepPlace(detail);
    detail.innerHTML=html;renderedJob={id,html};
    if(opened)for(const item of detail.querySelectorAll("details")){const open=opened.get(item.querySelector("summary")?.textContent);if(open!==undefined)item.open=open}
    if(place)restorePlace(detail,place);
    detail.querySelectorAll("[data-patch]").forEach(button=>button.onclick=()=>downloadPatch(job.id,button.dataset.patch,button.dataset.attempt));
    $("close-job").onclick=closeJob;
    // The open job is in the address, so it can be linked and reopened.
    if(location.hash!==`#jobs/${id}`)history.replaceState(null,"",`#jobs/${id}`);
    $("cancel-job")?.addEventListener("click",async event=>{
      if(!confirm("Cancel this job? Its work stops and its uploaded source is deleted. The prompt, results and events are kept."))return;
      const button=event.currentTarget;button.disabled=true;
      try{await api(`/api/v1/jobs/${id}/cancel`,{method:"POST",body:"{}"});toast("Cancellation requested");await refresh();await showJob(id)}
      catch(error){button.disabled=false;fail(error)}
    });
    if(newlySelected){
      // Opening a job moves focus to it, so a keyboard user lands on what opened (UX audit, 30 Oct).
      const heading=detail.querySelector("h3");heading?.setAttribute("tabindex","-1");heading?.focus({preventScroll:true});
      detail.scrollIntoView({behavior:motion(),block:"start"});
    }
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
