const {readFileSync}=require('node:fs');
const {join}=require('node:path');
const vm=require('node:vm');
const assert=require('node:assert/strict');
const {test}=require('node:test');
const html=readFileSync(join(__dirname,'codenotch/ui/notch.html'),'utf8');
const source=html.split('// ACCOUNT_PROFILE_METADATA_START')[1].split('// ACCOUNT_PROFILE_METADATA_END')[0];
const NOW=1800000000000;
function context(invoke){
  const ctx=vm.createContext({Map,esc:s=>String(s).replaceAll('<','&lt;'),textCopy:s=>s,invoke:invoke||(()=>Promise.resolve({providers:[]})),
    renderRing(){},renderCard(){},card:{classList:{contains:()=>false}},
    usage:{windows:[]},codexSnap:{windows:[{label:'Previous profile quota'}]},cursorSnap:{windows:[]},grokSnap:{windows:[]},copilotSnap:{windows:[]},glmSnap:{windows:[]},opencodeSnap:{windows:[]},agSnap:{windows:[{label:'Previous Antigravity quota'}]}});
  vm.runInContext(source,ctx);return ctx;
}
function snapshot(id,label='Fixture account',provider='codex'){
  return {providers:[{id:provider,active_account_id:id,accounts:[{id,label,status:'saved',active:true}]}]};
}
test('notch labels the selected profile and only fresh account usage gets a LIVE badge',()=>{
  const ctx=context();ctx.applyNotchAccountMetadata(snapshot('work','<Fixture Work>'));
  const p={id:'codex',base:'codex'};
  assert.match(ctx.notchAccountHtml(p,{status:'ok',fetched_at:NOW,windows:[]},NOW),/&lt;Fixture Work>/);
  assert.match(ctx.notchAccountHtml(p,{status:'ok',fetched_at:NOW,windows:[]},NOW),/>LIVE</);
  assert.match(ctx.notchAccountHtml(p,{status:'ok',fetched_at:NOW-301000,windows:[{}]},NOW),/>Cached</);
});
test('changing profiles removes the previous account quota immediately',()=>{
  const ctx=context();ctx.applyNotchAccountMetadata(snapshot('work'));ctx.applyNotchAccountMetadata(snapshot('personal'));
  assert.equal(ctx.codexSnap.windows.length,0);assert.equal(ctx.codexSnap.fetched_at,0);
  assert.match(ctx.codexSnap.note,/selected account/);
});
test('Antigravity metadata accepts the canonical provider and the old gemini ring ID',()=>{
  const ctx=context();ctx.applyNotchAccountMetadata(snapshot('work','Work','antigravity'));
  assert.equal(ctx.notchActiveProfile('gemini').id,'work');
  ctx.applyNotchAccountMetadata(snapshot('personal','Personal','antigravity'));
  assert.equal(ctx.agSnap.windows.length,0);
});
test('profile metadata is not wrongly applied to another Claude account row',()=>{
  const ctx=context();ctx.applyNotchAccountMetadata(snapshot('work','Selected','claude'));
  assert.equal(ctx.notchAccountHtml({id:'claude@other',base:'claude'},{status:'ok',fetched_at:NOW,windows:[]},NOW),'');
});
test('a late initial metadata read cannot replace a newer account selection',async()=>{
  let resolve;const ctx=context(()=>new Promise(done=>{resolve=done;}));
  const reading=ctx.refreshNotchAccountMetadata();
  vm.runInContext('notchAccountRevision++',ctx);ctx.applyNotchAccountMetadata(snapshot('personal'));
  resolve(snapshot('work'));await reading;
  assert.equal(ctx.notchActiveProfile('codex').id,'personal');
});
test('a late usage IPC response cannot repopulate the previous account quota after a selection event',async()=>{
  let resolve;const ctx=context(()=>new Promise(done=>{resolve=done;}));
  ctx.applyNotchAccountMetadata(snapshot('work'));
  const reading=ctx.readNotchProviderSnapshot('get_codex',next=>{ctx.codexSnap=next;});
  vm.runInContext('notchAccountRevision++',ctx);ctx.applyNotchAccountMetadata(snapshot('personal'));
  resolve({status:'ok',fetched_at:NOW,windows:[{label:'Old account quota'}]});await reading;
  assert.equal(ctx.codexSnap.windows.length,0);
});
