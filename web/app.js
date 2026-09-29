"use strict";
const $ = id => document.getElementById(id);
const enc = new TextEncoder(), dec = new TextDecoder();
const now = () => Math.floor(Date.now() / 1000);
const uid = () => crypto.randomUUID();
const b64 = bytes => { let out=""; for (const b of bytes) out+=String.fromCharCode(b); return btoa(out); };
const bytes = text => Uint8Array.from(atob(text), c=>c.charCodeAt(0));
const hash = async data => [...new Uint8Array(await crypto.subtle.digest("SHA-256",data))].map(b=>b.toString(16).padStart(2,"0")).join("");
const read = (key, fallback) => { try { return JSON.parse(localStorage.getItem(key)) ?? fallback; } catch { return fallback; } };
let identity, peers=read("mutlink.peers",{}), login=JSON.parse(sessionStorage.getItem("mutlink.login")||"null");
let current=read("mutlink.task",null), cursor=0, streaming=null, polling=false, submitting=false, tick=0;
const pending=new Map(), seen=new Map();
function notice(message){ $("notice").textContent=message; }
async function api(path, body, method) {
  const response=await fetch(path,{method:method||(body?"POST":"GET"),headers:{
    ...(body?{"Content-Type":"application/json"}:{}),...(login?{"Authorization":`Bearer ${login.token}`}:{})
  },body:body?JSON.stringify(body):undefined,signal:AbortSignal.timeout(15000),redirect:"error"});
  if(!response.ok) throw new Error(response.status===400?"请求被拒绝，请检查账号、设备权限或登录有效期。":`连接失败 (${response.status})`);
  return response.json();
}
async function seal(target, payload) {
  const peer=peers[target]; if(!peer) throw new Error("请先导入设备身份并核对指纹。");
  const id=uid(), nonce=nacl.randomBytes(24);
  const body={version:1,id,from:identity.device.id,to:target,issued_at:now(),expires_at:now()+120,payload};
  return {id,from:body.from,to:target,nonce:b64(nonce),ciphertext:b64(nacl.box(enc.encode(JSON.stringify(body)),nonce,bytes(peer.public_key),bytes(identity.secret)))};
}
function open(packet) {
  const peer=peers[packet.from];
  if(!peer||packet.to!==identity.device.id||packet.ciphertext.length>2*1024*1024) throw new Error("未授权的消息");
  const raw=nacl.box.open(bytes(packet.ciphertext),bytes(packet.nonce),bytes(peer.public_key),bytes(identity.secret));
  if(!raw) throw new Error("消息验证失败");
  const body=JSON.parse(dec.decode(raw));
  if(body.version!==1||body.id!==packet.id||body.from!==packet.from||body.to!==packet.to||
      body.expires_at<=now()||body.issued_at>now()+30||body.expires_at-body.issued_at>120||
      body.expires_at<=body.issued_at||seen.has(packet.id)) throw new Error("消息过期或重复");
  seen.set(packet.id,now());
  return body.payload;
}
async function remote(target, action, payload) {
  const id=uid(), packet=await seal(target,{kind:"request",body:{id,action,payload,expires_at:now()+90}});
  return new Promise((resolve,reject)=>{
    const timer=setTimeout(()=>{pending.delete(id);reject(new Error("设备未回应。任务可能已收到，请按原任务 ID 恢复进度。"));},45000);
    pending.set(id,{target,resolve,reject,timer});
    api("/packets",packet).catch(error=>{clearTimeout(timer);pending.delete(id);reject(error);});
  });
}
async function receive() {
  for(const packet of await api("/packets")){
    try{
      const payload=open(packet), waiting=pending.get(payload.body?.id);
      if(payload.kind==="response"&&waiting?.target===packet.from){
        clearTimeout(waiting.timer);pending.delete(payload.body.id);
        payload.body.ok?waiting.resolve(payload.body.data):waiting.reject(new Error(payload.body.data.error||"远端执行失败"));
      }
    }catch{/* Drop unauthenticated, expired and replayed packets. */}
    await api(`/packets/${encodeURIComponent(packet.id)}`,null,"DELETE");
  }
  for(const [id,time] of seen) if(time<now()-900) seen.delete(id);
}
function saveCurrent(){localStorage.setItem("mutlink.task",JSON.stringify(current));}
function busy(){return submitting||(current&&["queued","running","waiting_approval"].includes(current.task.state));}
function controls(){
  const active=busy();
  $("target").disabled=!!active;$("new-session").disabled=!!active;
  $("send").disabled=!!active||!peers[$("target").value];
  $("cancel").disabled=!current||!active;
  $("task-state").textContent=current?`${current.task.state} · ${current.task.id}`:"准备就绪";
  $("session-name").textContent=current?`会话 · ${current.task.session_id.slice(0,8)}`:"新会话";
}
async function refresh(preferred){
  const list=await api("/devices"), selected=(typeof preferred==="string"?preferred:"")||$("target").value||current?.target||"";
  $("target").replaceChildren(new Option("选择一台设备",""));
  for(const entry of list){
    const device=entry.device;
    if(device.id===identity.device.id)continue;
    const trusted=!!peers[device.id];
    const option=new Option(`${entry.online?"●":"○"} ${device.name} · ${trusted?"已配对":"待配对"}`,device.id);
    $("target").append(option);
  }
  if(![...$("target").options].some(o=>o.value===selected)&&peers[selected]){
    $("target").append(new Option(`○ ${peers[selected].name} · 离线`,selected));
  }
  $("target").value=selected;
  targetState();controls();
}
function targetState(){
  const target=$("target").value;
  $("target-state").textContent=!target?"选择一台电脑作为执行目标。":peers[target]?"已核对设备身份 · 端到端加密":"先在「设备配对」中核对这台设备。";
}
function article(role,text,tool=false){
  $("empty")?.remove();
  const el=document.createElement("article");if(tool)el.className="tool";
  const heading=document.createElement("div");heading.className="role";heading.textContent=role;
  const pre=document.createElement("pre");pre.textContent=text;
  el.append(heading,pre);$("conversation").append(el);
  while($("conversation").children.length>300)$("conversation").firstElementChild.remove();
  return pre;
}
function event(e){
  cursor=Math.max(cursor,e.seq);
  const d=e.data, panel=$("conversation"), follow=panel.scrollHeight-panel.scrollTop-panel.clientHeight<100;
  if(e.kind==="user"){streaming=null;article("你",d.text);}
  if(e.kind==="delta"){if(!streaming)streaming=article("MUTLINK","");streaming.textContent+=d.text;}
  if(e.kind==="tool_start"){streaming=null;article(d.tool,JSON.stringify(d.arguments,null,2),true);}
  if(e.kind==="tool_result")article(d.output.ok?"完成":"未执行",JSON.stringify(d.output,null,2),true);
  if(e.kind==="approval")article("等待确认","请在执行设备打开 mutlink，按 Ctrl+A 审阅此次操作。",true);
  if(e.kind==="state"&&d.error)article(d.state,d.error,true);
  if(follow)panel.scrollTop=panel.scrollHeight;
}
let following=false;
async function follow(){
  if(following||!current)return;
  following=true;const task=current;
  try{
    const events=await remote(task.target,"events",{task_id:task.task.id,after:cursor});
    if(current!==task)return;
    events.forEach(event);
    const state=await remote(task.target,"task",{task_id:task.task.id});
    if(current!==task)return;
    task.task=state;saveCurrent();controls();
  }catch(e){notice(e.message);}
  finally{following=false;}
}
async function cycle(){
  if(!login||polling)return;
  polling=true;
  try{
    await api("/devices",{...identity.device,last_seen:now()});
    await receive();
    if(tick++%5===0)await refresh();
    if(current&&!following)void follow();
  }catch(e){notice(`${e.message} 正在重连…`);}
  finally{polling=false;}
}
function showWorkspace(){
  $("login-panel").hidden=!!login;$("workspace").hidden=!login;$("logout").hidden=!login;
  $("fingerprint").textContent=identity.device.id;
  if(login){notice(`已登录 ${login.account} · ${identity.device.name}`);void cycle();}
}
function on(id,type,fn){$(id).addEventListener(type,event=>{Promise.resolve(fn(event)).catch(e=>notice(e.message));});}
on("login-form","submit",async e=>{
  e.preventDefault();const button=e.submitter;button.disabled=true;
  try{
    identity.device.name=$("device-name").value.trim();localStorage.setItem("mutlink.identity",JSON.stringify(identity));
    login=await api("/auth",{account:$("account").value.trim(),password:$("password").value,device_id:identity.device.id,register:$("register").checked});
    sessionStorage.setItem("mutlink.login",JSON.stringify(login));$("password").value="";
    showWorkspace();
  }finally{button.disabled=false;}
});
on("logout","click",async()=>{
  await api("/logout",{});login=null;sessionStorage.removeItem("mutlink.login");
  current=null;saveCurrent();$("conversation").replaceChildren();cursor=0;streaming=null;
  showWorkspace();notice("已退出登录；本机身份与已核对的设备保留。");
});
on("export","click",()=>{
  const url=URL.createObjectURL(new Blob([JSON.stringify(identity.device,null,2)],{type:"application/json"}));
  const a=document.createElement("a");a.href=url;a.download="mutlink-mobile-public.json";a.click();setTimeout(()=>URL.revokeObjectURL(url),1000);
});
on("trust","click",async()=>{
  const file=$("peer-file").files[0];if(!file||file.size>65536)throw new Error("请选择有效的电脑身份 JSON。");
  const peer=JSON.parse(await file.text()), fingerprint=$("peer-fingerprint").value.trim();
  const key=bytes(peer.public_key), id=await hash(key);
  if(key.length!==32||id!==peer.id||id!==fingerprint||id===identity.device.id)throw new Error("完整指纹与公钥不一致。");
  peers[id]=peer;localStorage.setItem("mutlink.peers",JSON.stringify(peers));
  notice(`已信任 ${peer.name}。还需在电脑导入本机身份并允许执行任务。`);await refresh(id);
});
on("revoke","click",()=>{
  const id=$("target").value;if(!id)throw new Error("先选择设备。");
  delete peers[id];localStorage.setItem("mutlink.peers",JSON.stringify(peers));targetState();controls();notice("已撤销本机对该设备的信任。");
});
on("target","change",()=>{current=null;saveCurrent();cursor=0;streaming=null;$("conversation").replaceChildren();targetState();controls();});
on("refresh","click",refresh);
on("new-session","click",()=>{current=null;saveCurrent();cursor=0;streaming=null;$("conversation").replaceChildren();controls();});
on("composer","submit",async e=>{
  e.preventDefault();if(busy())return;
  const target=$("target").value,prompt=$("prompt").value.trim(),plan=$("plan").checked;
  if(!target||!prompt)throw new Error("选择设备并填写任务。");
  let saved=read("mutlink.unsent",null);
  const session=current?.task.session_id||null;
  if(!saved||saved.target!==target||saved.request.prompt!==prompt||saved.request.plan!==plan||saved.request.session_id!==session)
    saved={target,request:{id:uid(),session_id:session,prompt,plan}};
  localStorage.setItem("mutlink.unsent",JSON.stringify(saved));$("resume-id").value=saved.request.id;
  notice(`正在发送 · ${saved.request.id}`);submitting=true;controls();
  try{
    const task=await remote(target,"run",saved.request);
    current={target,task};saveCurrent();localStorage.removeItem("mutlink.unsent");
    $("prompt").value="";notice("设备已接收任务。");
  }finally{submitting=false;controls();}
});
on("cancel","click",async()=>{
  if(current)await remote(current.target,"cancel",{task_id:current.task.id});
  notice("已请求取消任务。");
});
on("resume","click",async()=>{
  if(busy())throw new Error("请先等待或取消当前任务。");
  const target=$("target").value, task=await remote(target,"task",{task_id:$("resume-id").value.trim()});
  current={target,task};cursor=0;streaming=null;$("conversation").replaceChildren();saveCurrent();controls();void follow();
});
async function init(){
  if(!window.isSecureContext||!crypto.subtle)throw new Error("请通过 HTTPS 访问控制台（本机调试可使用 localhost）。");
  identity=read("mutlink.identity",null);
  if(!identity){
    const keys=nacl.box.keyPair();
    identity={secret:b64(keys.secretKey),device:{id:await hash(keys.publicKey),public_key:b64(keys.publicKey),name:"我的手机",platform:"Browser controller",endpoint:null,last_seen:now(),capabilities:[{id:"agent.control",version:"1.0.0",description:"Submit and inspect encrypted remote tasks",risk:"read",available:true}]}};
    localStorage.setItem("mutlink.identity",JSON.stringify(identity));
  }
  showWorkspace();if(!login)notice("登录同一账号，发现你的其他设备。");
  setInterval(cycle,1000);controls();
}
init().catch(e=>notice(e.message));
