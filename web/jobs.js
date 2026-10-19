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
  list.innerHTML=projects.map(x=>`<div class="item"><strong>${esc(x.name)}</strong><div class="meta"><span>${esc(x.roots.join(", "))}</span></div>${(x.instruction_files||[]).length?`<div class="meta"><span>${esc(x.instruction_files.join(", "))}</span></div>`:""}<div class="item-actions"><button class="secondary" data-edit-project="${esc(x.id)}">Edit</button> <button class="secondary" data-remove-project="${esc(x.id)}">Remove</button></div></div>`).join("");
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
  $("project-panel").open=true;$("project-name").focus();$("project-panel").scrollIntoView({block:"start",behavior:"smooth"});
}
$("project-cancel").onclick=resetProject;
$("project-form").addEventListener("submit",async event=>{
  event.preventDefault();const button=$("project-save");button.disabled=true;
  const body={name:$("project-name").value,roots:$("project-roots").value.split("\n").map(x=>x.trim()).filter(Boolean),instruction_files:$("project-instructions").value.split("\n").map(x=>x.trim()).filter(Boolean)};
  try{await api("/api/v1/projects"+(editingProject?"/"+encodeURIComponent(editingProject):""),{method:editingProject?"PATCH":"POST",body:JSON.stringify(body)});resetProject();await refresh();toast("Project saved")}
  catch(error){fail(error)}finally{button.disabled=false}
});
