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
test('the notch retains newer provider artwork when a delayed initial IPC read contains an empty cache',async()=>{
  let finishRead,draws=0;
  const ctx=vm.createContext({glyphs:{},invoke:()=>new Promise(resolve=>{finishRead=resolve;}),
    renderRing(){draws++;},renderCard(){},card:{classList:{contains:()=>false}}});
  const glyphState=html.split('// PROVIDER_GLYPH_STATE_START')[1].split('// PROVIDER_GLYPH_STATE_END')[0];
  vm.runInContext(glyphState,ctx);
  const reading=ctx.refreshGlyphs();
  const svg=readFileSync(join(__dirname,'codenotch/glyphs/codex.svg'),'utf8');
  ctx.receiveGlyphs({payload:{codex:{kind:'svg',svg}}});
  finishRead({});await reading;
  assert.equal(ctx.glyphs.codex.svg,svg);assert.equal(draws,1);
});
test('the notch updates changed SVG and bitmap artwork without replacing the hovered provider cell',()=>{
  let cellRebuilds=0,glyphWrites=0;
  const element=()=>({innerHTML:'',classList:{toggle(){}},textContent:''});
  const nodes=Object.fromEntries(['svg.ring','svg.reading','svg.activity','.pct','.glyph','.ringwrap'].map(id=>[id,element()]));
  Object.defineProperty(nodes['.glyph'],'innerHTML',{get(){return this.markup;},set(markup){glyphWrites++;this.markup=markup;}});
  const cell={classList:{toggle(){}},querySelector:selector=>nodes[selector]};
  const pill={dataset:{cells:'codex'},classList:{toggle(){}},querySelector:()=>cell,
    set innerHTML(_){cellRebuilds++;}};
  const p={id:'codex',base:'codex',name:'Codex',glyph:'Cx',snap:{status:'ok',windows:[]}};
  const first=readFileSync(join(__dirname,'codenotch/glyphs/codex.svg'),'utf8');
  const replacement=readFileSync(join(__dirname,'codenotch/glyphs/claude.svg'),'utf8');
  const ctx=vm.createContext({glyphs:{codex:{kind:'svg',svg:first}},pill,providers:()=>[p],edgeIsVertical:()=>false,
    headlineOf:()=>null,weeklyRing:'off',refreshing:{},workState:()=> 'idle',staleOf:()=>false,reportHot(){},HOLE:'#000',TRACK:'#333'});
  vm.runInContext(html.slice(html.indexOf('function glyphHtml('),html.indexOf('// Provider table')),ctx);
  vm.runInContext(html.slice(html.indexOf('function drawSvg('),html.indexOf('\nfunction resetCopy(')),ctx);
  ctx.renderRing();assert.equal(nodes['.glyph'].innerHTML,`<span class="mark">${first}</span>`);
  ctx.glyphs.codex={kind:'svg',svg:replacement};ctx.renderRing();
  assert.equal(nodes['.glyph'].innerHTML,`<span class="mark">${replacement}</span>`);
  ctx.glyphs.codex={kind:'png',url:'data:image/png;base64,first'};ctx.renderRing();
  ctx.glyphs.codex={kind:'png',url:'data:image/png;base64,replacement'};ctx.renderRing();
  assert.match(nodes['.glyph'].innerHTML,/base64,replacement/);
  ctx.renderRing();
  assert.equal(glyphWrites,4,'unchanged artwork does not recreate the SVG on every reading');
  assert.equal(cellRebuilds,0,'the hovered cell and animation nodes keep their identity');
});
