const {readFileSync}=require('node:fs');
const {join}=require('node:path');
const vm=require('node:vm');
const assert=require('node:assert/strict');
const {test}=require('node:test');
const html=readFileSync(join(__dirname,'codenotch/ui/settings.html'),'utf8');
const source=html.slice(html.indexOf('/* ---- account profiles '),html.indexOf('/* ---- Appearance: Show'));
const NOW=1800000000000;
class Clock extends Date{static now(){return NOW;}}
const snapshot={providers:[{id:'codex',label:'Codex',login_mode:'cli',login_available:true,login_note:'Official Codex sign-in.',active_account_id:'p1',accounts:[
  {id:'p1',provider:'codex',label:'Work',identity:'sample@example.invalid',status:'saved',source:'local',active:true},
  {id:'p2',provider:'codex',label:'Personal',status:'needsAuth',source:'local',active:false},
]}]};
function page(invokeOverride){
  const nodes=new Map(),calls=[],listeners={};
  function element(tag='div'){
    return {tag,children:[],style:{},dataset:{},attributes:{},handlers:{},value:'',textContent:'',open:false,
      set innerHTML(_){throw new Error('Profile metadata must remain plain text');},
      setAttribute(k,v){this.attributes[k]=v;},appendChild(node){this.children.push(node);if(tag==='select'&&!this.value)this.value=node.value;},
      replaceChildren(...children){this.children=children;},addEventListener(k,v){this.handlers[k]=v;},focus(){this.focused=true;},
      showModal(){this.open=true;},close(){this.open=false;this.handlers.close?.();}};
  }
  const node=id=>{if(!nodes.has(id))nodes.set(id,element(id==='profile-provider'?'select':'div'));return nodes.get(id);};
  const context=vm.createContext({document:{getElementById:node,createElement:element},Date:Clock,Map,ui:(_,fallback)=>fallback,
    invoke:(cmd,args)=>{calls.push([cmd,args]);return invokeOverride?invokeOverride(cmd,args):Promise.resolve(snapshot);},
    codexResetState:{windows:[{label:'Old account usage'}]},codexResetRevision:0,refreshCodexResets(){context.codexRefreshed=true;},
    renderResetTracker(){},errText:e=>String(e.message||e),window:{__TAURI__:{event:{listen(name,callback){listeners[name]=callback;return Promise.resolve();}}}}});
  vm.runInContext(source,context);
  return {node,context,calls,listeners,render:state=>context.applyAccountProfiles(state)};
}
function flat(node){return [node,...node.children.flatMap(flat)];}
test('saved credentials are never mistaken for a recently verified account',()=>{
  const view=page();
  assert.equal(view.context.profileAuthStatus({status:'saved'},NOW).label,'Credentials saved');
  assert.equal(view.context.profileAuthStatus({status:'connected'},NOW).label,'Credentials saved');
  assert.equal(view.context.profileAuthStatus({status:'connected',verified_at:NOW-301000},NOW).label,'Credentials saved');
  assert.equal(view.context.profileAuthStatus({status:'connected',verified_at:NOW},NOW).label,'Connected');
});
test('profile labels and identities are rendered as text with no credential fields returned to the view',()=>{
  const view=page(),hostile='<img onerror=alert(1)>';
  view.render({providers:[{...snapshot.providers[0],accounts:[{...snapshot.providers[0].accounts[0],label:hostile,identity:hostile}]}]});
  const nodes=flat(view.node('account-profiles'));
  assert.ok(nodes.some(n=>n.textContent===hostile));
  assert.ok(nodes.every(n=>n.tag!=='img'));
  assert.equal(view.node('profile-credential').value,'');
});
test('providers without an available native login offer only actual supported connection actions',()=>{
  const view=page();view.render({providers:[{...snapshot.providers[0],login_available:false,manual_key_available:true}]});
  const actions=flat(view.node('account-profiles')).filter(n=>n.dataset.accountAction).map(n=>n.dataset.accountAction);
  assert.ok(!actions.includes('login'));assert.ok(actions.includes('key'));assert.ok(actions.includes('select'));
});
test('switching active profiles discards the old account reading before requesting the new one',()=>{
  const view=page();view.render(snapshot);
  view.render({providers:[{...snapshot.providers[0],active_account_id:'p2',accounts:snapshot.providers[0].accounts.map(a=>({...a,active:a.id==='p2'}))}]});
  assert.equal(view.context.codexResetState,null);assert.equal(view.context.codexRefreshed,true);
  assert.equal(view.context.activeProfileFor('codex').label,'Personal');
});
test('a newer profile selection event wins over a slow initial accounts read',async()=>{
  let resolve;const view=page(()=>new Promise(done=>{resolve=done;}));
  const reading=view.context.refreshAccountProfiles();
  const selected={providers:[{...snapshot.providers[0],active_account_id:'p2',accounts:snapshot.providers[0].accounts.map(a=>({...a,active:a.id==='p2'}))}]};
  view.listeners['accounts-updated']({payload:selected});resolve(snapshot);await reading;
  assert.equal(view.context.activeProfileFor('codex').id,'p2');
});
test('manually entered keys are cleared before IPC and again when the dialog closes',async()=>{
  let valueAtInvoke;
  const view=page((cmd,args)=>{if(cmd==='connect_provider_account')valueAtInvoke=view.node('profile-credential').value;return Promise.resolve(snapshot);});
  view.render(snapshot);view.context.openProfileDialog('codex','p1');
  view.node('profile-credential').value='fixture-secret';
  await view.node('profile-dialog-form').handlers.submit({preventDefault(){}});
  assert.equal(valueAtInvoke,'');assert.equal(view.node('profile-credential').value,'');
  const call=view.calls.find(([cmd])=>cmd==='connect_provider_account');
  assert.equal(call[1].provider,'codex');assert.equal(call[1].accountId,'p1');
  assert.equal(view.node('profile-dialog').open,false);
});
test('login progress is profile-specific and does not mark credentials as connected',()=>{
  const view=page();view.render(snapshot);
  view.listeners['provider-account-login']({payload:{provider:'codex',profile_id:'p2',busy:true,note:'Finish sign-in in the browser.'}});
  const nodes=flat(view.node('account-profiles'));
  assert.equal(nodes.filter(n=>n.textContent==='Signing in…').length,1);
  assert.ok(nodes.some(n=>n.textContent==='Credentials saved'));
  assert.ok(!nodes.some(n=>n.textContent==='Connected'));
});
