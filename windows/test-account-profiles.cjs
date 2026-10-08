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
function page(invokeOverride,artwork={}){
  const nodes=new Map(),calls=[],listeners={};
  function element(tag='div'){
    return {tag,children:[],style:{},dataset:{},attributes:{},handlers:{},value:'',textContent:'',open:false,
      set innerHTML(markup){
        if(this.className!=='glyph'||!markup.startsWith('<svg'))throw new Error('Profile metadata must remain plain text');
        this.svgMarkup=markup;
      },
      setAttribute(k,v){this.attributes[k]=v;},appendChild(node){this.children.push(node);if(tag==='select'&&!this.value)this.value=node.value;},
      replaceChildren(...children){this.children=children;},addEventListener(k,v){this.handlers[k]=v;},focus(){this.focused=true;},
      showModal(){this.open=true;},close(){this.open=false;this.handlers.close?.();}};
  }
  const node=id=>{if(!nodes.has(id))nodes.set(id,element(id==='profile-provider'?'select':'div'));return nodes.get(id);};
  const context=vm.createContext({document:{getElementById:node,createElement:element},Date:Clock,Map,URL,glyphs:artwork,ui:(_,fallback)=>fallback,
    navigator:{clipboard:{writeText:async code=>{calls.push(['clipboard',code]);}}},
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
test('browser device challenges display only official HTTPS links and public codes and reopen through native state',async()=>{
  const view=page();view.render(snapshot);
  view.listeners['provider-account-login']({payload:{provider:'codex',profile_id:'p2',busy:true,note:'Continue in the browser.',url:'https://auth.openai.com/codex/device',user_code:'ABCD-EFGH',expires_at:NOW+65000,device_code:'must-not-render',access_token:'must-not-render'}});
  const nodes=flat(view.node('account-profiles'));
  assert.ok(nodes.some(n=>n.textContent==='auth.openai.com'));
  assert.ok(nodes.some(n=>n.textContent==='ABCD-EFGH'));
  assert.ok(nodes.some(n=>n.textContent==='Code expires in 1:05'));
  assert.ok(!nodes.some(n=>n.textContent==='must-not-render'));
  const reopen=nodes.find(n=>n.dataset.accountAction==='reopen');
  view.node('account-profiles').handlers.click({target:{closest:()=>reopen}});
  assert.deepEqual(JSON.parse(JSON.stringify(view.calls.find(([cmd])=>cmd==='reopen_provider_sign_in')[1])),{provider:'codex',accountId:'p2'});
  await view.context.copyBrowserSigninCode('codex','p2');
  assert.ok(view.calls.some(([cmd,value])=>cmd==='clipboard'&&value==='ABCD-EFGH'));
});
test('untrusted challenge URLs and secret-bearing query parameters never become browser actions',()=>{
  const view=page();view.render(snapshot);
  for(const url of ['javascript:alert(1)','https://auth.openai.com.evil.invalid/codex/device','https://auth.openai.com/codex/device?device_code=secret','http://auth.openai.com/codex/device']){
    view.listeners['provider-account-login']({payload:{provider:'codex',profile_id:'p2',busy:true,url,user_code:'not a public code!'}});
    const nodes=flat(view.node('account-profiles'));
    assert.ok(!nodes.some(n=>n.dataset.accountAction==='reopen'));
    assert.ok(!nodes.some(n=>n.dataset.accountAction==='copy-code'));
  }
});
test('completed browser sign-in clears challenge metadata and sensitive manual code before any subsequent renderer',()=>{
  const view=page();view.render({providers:[{...snapshot.providers[0],id:'claude',login_mode:'browser',accounts:snapshot.providers[0].accounts.map(a=>({...a,provider:'claude'}))}]});
  view.listeners['provider-account-login']({payload:{provider:'claude',profile_id:'p2',busy:true,url:'https://claude.com/cai/oauth/authorize'}});
  view.context.openBrowserCodeDialog('claude','p2');view.node('browser-signin-code').value='fixture-sensitive-code';
  assert.equal(view.node('browser-code-dialog').open,true);
  view.listeners['provider-account-login']({payload:{provider:'claude',profile_id:'p2',busy:false,note:'Sign-in complete.',url:'https://claude.com/cai/oauth/authorize'}});
  assert.equal(view.node('browser-code-dialog').open,false);assert.equal(view.node('browser-signin-code').value,'');
  assert.ok(!flat(view.node('account-profiles')).some(n=>n.dataset.accountAction==='reopen'));
});
test('Claude browser fallback code is cleared before its single native IPC transfer and never copied into profile metadata',async()=>{
  let valueAtInvoke;
  const view=page((cmd,args)=>{if(cmd==='complete_provider_sign_in')valueAtInvoke=view.node('browser-signin-code').value;return Promise.resolve({});});
  view.render({providers:[{...snapshot.providers[0],id:'claude',login_mode:'browser',accounts:snapshot.providers[0].accounts.map(a=>({...a,provider:'claude'}))}]});
  view.listeners['provider-account-login']({payload:{provider:'claude',profile_id:'p2',busy:true,url:'https://claude.com/cai/oauth/authorize'}});
  view.context.openBrowserCodeDialog('claude','p2');view.node('browser-signin-code').value='fixture-code#fixture-state';
  await view.node('browser-code-form').handlers.submit({preventDefault(){}});
  assert.equal(valueAtInvoke,'');assert.equal(view.node('browser-signin-code').value,'');
  assert.equal(view.calls.find(([cmd])=>cmd==='complete_provider_sign_in')[1].code,'fixture-code#fixture-state');
  assert.ok(!flat(view.node('account-profiles')).some(n=>n.textContent==='fixture-code#fixture-state'));
});
test('a reopened Settings window recovers the native pending browser challenge without waiting for a new event',async()=>{
  const pending={provider:'codex',profile_id:'p2',busy:true,url:'https://auth.openai.com/codex/device',user_code:'ABCD-EFGH',sequence:5};
  const view=page(cmd=>Promise.resolve(cmd==='get_provider_sign_in_states'?[pending]:snapshot));view.render(snapshot);
  await view.context.refreshProviderSignins();
  const nodes=flat(view.node('account-profiles'));
  assert.ok(nodes.some(n=>n.textContent==='ABCD-EFGH'));
  assert.ok(nodes.some(n=>n.dataset.accountAction==='reopen'));
  assert.ok(nodes.some(n=>n.dataset.accountAction==='cancel-login'));
});
test('a completed native event wins over a slow pending browser snapshot, including a read started after that event',async()=>{
  let finish;const pending=new Promise(resolve=>{finish=resolve;});
  const view=page(cmd=>cmd==='get_provider_sign_in_states'?pending:Promise.resolve(snapshot));view.render(snapshot);
  const reading=view.context.refreshProviderSignins();
  view.listeners['provider-account-login']({payload:{provider:'codex',profile_id:'p2',busy:false,note:'Sign-in complete.',sequence:6}});
  const laterReading=view.context.refreshProviderSignins();
  finish([{provider:'codex',profile_id:'p2',busy:true,url:'https://auth.openai.com/codex/device',user_code:'ABCD-EFGH',sequence:5}]);
  await Promise.all([reading,laterReading]);
  assert.ok(!flat(view.node('account-profiles')).some(n=>n.dataset.accountAction==='reopen'));
  assert.ok(!flat(view.node('account-profiles')).some(n=>n.textContent==='ABCD-EFGH'));
});
test('all built-in provider brand marks remain available in the multiple-account settings cards',()=>{
  const ids=['claude','codex','cursor','grok','copilot','gemini','opencode'];
  const artwork=Object.fromEntries(ids.map(id=>[id,{kind:'svg',svg:readFileSync(join(__dirname,'codenotch/glyphs',id+'.svg'),'utf8')}]));
  const view=page(undefined,artwork);
  view.render({providers:ids.map(id=>({id:id==='gemini'?'antigravity':id,label:id,accounts:[]}))});
  const marks=flat(view.node('account-profiles')).filter(n=>n.svgMarkup);
  assert.equal(marks.length,7);
  assert.deepEqual(marks.map(n=>n.svgMarkup),ids.map(id=>artwork[id].svg));
  assert.ok(marks.every(n=>n.className==='glyph'),'brand marks never fall back to initial-letter tiles');
});
test('account settings preserve user bitmap glyph overrides and use a letter only when no artwork exists',()=>{
  const view=page(undefined,{codex:{kind:'png',url:'data:image/png;base64,fixture'}});
  const mark=view.context.profileProviderGlyph({id:'codex',label:'Codex'});
  assert.equal(mark.children[0].tag,'img');
  assert.equal(mark.children[0].src,'data:image/png;base64,fixture');
  const fallback=view.context.profileProviderGlyph({id:'glm',label:'z.ai'});
  assert.equal(fallback.className,'glyph letter');assert.equal(fallback.textContent,'Z');
});
test('a fresh native glyph event redraws both settings account views and wins over a late empty read',async()=>{
  let finishRead,legacyDraws=0;
  const view=page();
  view.render(snapshot);
  view.context.call=()=>new Promise(resolve=>{finishRead=resolve;});
  view.context.renderAccounts=()=>{legacyDraws++;};
  const glyphState=html.split('// PROVIDER_GLYPH_STATE_START')[1].split('// PROVIDER_GLYPH_STATE_END')[0];
  vm.runInContext(glyphState,view.context);
  const reading=view.context.refreshGlyphs();
  const svg=readFileSync(join(__dirname,'codenotch/glyphs/codex.svg'),'utf8');
  view.context.receiveGlyphs({payload:{codex:{kind:'svg',svg}}});
  finishRead({});await reading;
  assert.equal(legacyDraws,1);
  assert.equal(view.context.glyphs.codex.svg,svg);
  assert.ok(flat(view.node('account-profiles')).some(n=>n.svgMarkup===svg));
});
