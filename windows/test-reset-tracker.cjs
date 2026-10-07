// Run with node --test test-reset-tracker.cjs. Uses the actual settings renderer without Tauri.
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const { test } = require('node:test');
const html = readFileSync(join(__dirname, 'codenotch/ui/settings.html'), 'utf8');
const source = html.slice(html.indexOf('/* ---- reset tracker '), html.indexOf('/* ---- updates '));
const NOW = 1800000000000;
class Clock extends Date { static now() { return NOW; } }

function view(invokeOverride) {
  const nodes = new Map(), calls = [], listeners = {};
  function element(tag = 'div') {
    const classes = new Set();
    return {
      tag, children: [], attributes: {}, style: {}, dataset: {}, handlers: {}, textContent: '',
      classList: {
        add(name) { classes.add(name); },
        toggle(name, on) { on ? classes.add(name) : classes.delete(name); },
        contains(name) { return classes.has(name); },
      },
      set innerHTML(_) { throw new Error('External reset data must never enter innerHTML'); },
      setAttribute(name, value) { this.attributes[name] = value; },
      appendChild(node) { this.children.push(node); },
      replaceChildren(...children) { this.children = children; },
      addEventListener(name, handler) { this.handlers[name] = handler; },
    };
  }
  const node = id => { if(!nodes.has(id)) nodes.set(id, element()); return nodes.get(id); };
  const context = vm.createContext({
    document: { documentElement: { lang:'en' }, getElementById:node, createElement:element },
    Date:Clock, ui:(_key, fallback) => fallback,
    drawSwitch(el, state) { el.disabled = state.busy; el.setAttribute('aria-checked', String(state.value)); },
    invoke: (command, args) => {
      calls.push([command, args]);
      return invokeOverride ? invokeOverride(command, args) : Promise.resolve({});
    },
    strip:message => { context.lastError = message; }, errText:String, toast() {}, setInterval() {}, curTab:'resets',
    window: { __TAURI__: { event: { listen(name, handler) { listeners[name] = handler; return Promise.resolve(); } } } },
  });
  vm.runInContext(source, context);
  function render(globalState, personal = null) {
    context.globalFixture = globalState; context.personalFixture = personal;
    vm.runInContext('globalResetState=globalFixture;codexResetState=personalFixture;renderResetTracker();', context);
  }
  return { node, context, calls, listeners, render };
}
const event = { id:'reset-1', announced_at:NOW-86400000, summary:'Fixture: a verified Codex reset', url:'https://codex-reset.com/' };
const state = { status:'ok', fetched_at:NOW-60000, notifications:true, last_reset:event, history:[event],
  forecast:{ probability_24h:12, probability_48h:36, confidence:'low', confidence_note:'Fixture history is small.' } };

test('live tick preserves history and personal card nodes while advancing the checked age and countdown', () => {
  const page = view();
  page.render({...state,checked_at:NOW-5000,next_check_at:NOW+60000}, {status:'ok',fetched_at:NOW,windows:[{label:'5h limit',used:.4,resets_at:NOW+1000}]});
  const history = page.node('reset-history').children[0], card = page.node('reset-personal-windows').children[0];
  vm.runInContext('renderResetClock(Date.now()+2000)',page.context);
  assert.equal(page.node('reset-history').children[0],history);
  assert.equal(page.node('reset-personal-windows').children[0],card);
  assert.equal(card.children[1].textContent,'Awaiting usage update');
  assert.equal(page.node('reset-updated').textContent,'Checked 7s ago');
  assert.equal(page.node('reset-next-check').textContent,'Next check 0:58');
});

test('public announcement, historical forecast and account countdown stay distinct', () => {
  const page = view();
  page.render(state, { status:'ok', fetched_at:NOW, windows:[{label:'5h limit', used:.4, resets_at:NOW+4500000}] });
  assert.equal(page.node('reset-latest-title').textContent, 'Latest global reset');
  assert.equal(page.node('reset-prob24').textContent, '12%');
  assert.match(page.node('reset-forecast-note').textContent, /estimate, not a scheduled reset/);
  const card = page.node('reset-personal-windows').children[0];
  assert.equal(card.children[1].textContent, '1h 15m');
  assert.equal(card.children[0].children[1].textContent, '40% used');
  assert.match(page.node('reset-personal-note').textContent, /Personal windows renew on your own schedule/);
});

test('elapsed or unknown personal reset time never announces available quota', () => {
  const page = view();
  page.render(state, { status:'stale', windows:[{label:'Weekly limit', used:.8, resets_at:NOW-1000}, {label:'5h limit', resets_at:null}] });
  const cards = page.node('reset-personal-windows').children;
  assert.equal(cards[0].children[1].textContent, 'Awaiting usage update');
  assert.equal(cards[1].children[1].textContent, 'Time unknown');
  assert.equal(cards[1].children[0].children[1].textContent, '—');
  assert.match(page.node('reset-personal-note').textContent, /Last known reading/);
});

test('an aged account reading stays marked stale even if its last successful status was ok', () => {
  const page = view();
  page.render(state, {status:'ok', fetched_at:NOW-600000, windows:[{label:'5h limit',used:.2,resets_at:NOW+300000}]});
  assert.match(page.node('reset-personal-note').textContent, /^Last known reading/);
  page.render(state, {status:'ok', fetched_at:NOW+120000, windows:[{label:'5h limit',used:.2,resets_at:NOW+300000}]});
  assert.match(page.node('reset-personal-note').textContent, /^Last known reading/);
});

test('an in-progress background check disables refresh while leaving cached content visible', () => {
  const page = view();
  page.render({...state,refreshing:true});
  assert.equal(page.node('reset-feed').textContent, 'LIVE · syncing');
  assert.equal(page.node('btn-reset-refresh').disabled, true);
  assert.equal(page.node('reset-summary').textContent, event.summary);
});

test('offline cache remains visible and the source opener cannot accept a remote URL', async () => {
  const page = view();
  page.render({...state, status:'stale', error:'Network unavailable', source_url:'javascript:malicious()'});
  assert.equal(page.node('reset-feed').textContent, 'Saved feed');
  assert.equal(page.node('reset-summary').textContent, event.summary);
  assert.equal(page.node('reset-error').hidden, false);
  await page.node('btn-reset-source').handlers.click();
  assert.equal(page.calls.at(-1)[0], 'open_global_reset_source');
  assert.equal(page.calls.at(-1)[1], undefined);
});

test('external summaries and notes are rendered as plain text', () => {
  const page = view(), hostile = '<img src=x onerror=alert(1)>';
  page.render({...state, last_reset:{...event,summary:hostile}, history:[{...event,summary:hostile}],
    forecast:{probability_24h:-5, probability_48h:500, confidence_note:hostile}});
  assert.equal(page.node('reset-summary').textContent, hostile);
  assert.equal(page.node('reset-history').children[0].children[0].textContent, hostile);
  assert.match(page.node('reset-forecast-note').textContent, /<img/);
  assert.equal(page.node('reset-prob24').textContent, '—');
  assert.equal(page.node('reset-prob48').textContent, '—');
});

test('missing public history does not invent a reset and invalid dates are ignored', () => {
  const page = view();
  page.render({status:'offline',history:[{announced_at:Infinity},{announced_at:9000000000000000}],last_reset:{announced_at:Infinity}});
  assert.equal(page.node('reset-latest-title').textContent, 'Watching for resets');
  assert.equal(page.node('reset-history').children.length, 1);
  assert.match(page.node('reset-history').children[0].textContent, /first successful check/);
  assert.equal(page.node('sw-global-reset').disabled, true);
});

test('a newer event wins over an initial state read that finishes late', async () => {
  let resolve;
  const page = view(() => new Promise(done => { resolve = done; }));
  const read = page.context.refreshResetTracker();
  page.listeners.global_reset_state({payload:state});
  resolve({status:'loading', last_reset:null});
  await read;
  assert.equal(page.node('reset-latest-title').textContent, 'Latest global reset');
  assert.equal(page.node('reset-feed').textContent, 'LIVE');
});

test('a completed worker event wins over a late manual refresh command response', async () => {
  let resolve;
  const page = view(() => new Promise(done => { resolve = done; }));
  page.render({status:'ok',notifications:true});
  const refreshing = page.node('btn-reset-refresh').handlers.click();
  page.listeners.global_reset_state({payload:state});
  resolve({status:'loading',refreshing:true,last_reset:null});
  await refreshing;
  assert.equal(page.node('reset-latest-title').textContent, 'Latest global reset');
  assert.equal(page.node('reset-feed').textContent, 'LIVE');
  assert.equal(page.node('btn-reset-refresh').disabled, false);
});

test('preview uses the global command and re-enables the button on an error', async () => {
  const page = view(() => Promise.reject(new Error('preview failed')));
  const button = page.node('btn-global-reset-preview');
  await button.handlers.click({currentTarget:button});
  assert.equal(page.calls[0][0], 'preview_global_reset_alert');
  assert.equal(button.disabled, false);
  assert.match(page.context.lastError, /Test notification failed/);
});

test('notification toggle displays the persisted command result when a follow-up read fails', async () => {
  const page = view(command => command === 'set_global_reset_notifications' ? Promise.resolve(false) : Promise.reject(new Error('read unavailable')));
  page.render(state);
  await page.node('sw-global-reset').handlers.click();
  assert.equal(page.calls[0][0], 'set_global_reset_notifications');
  assert.equal(page.calls[0][1].on, false);
  assert.equal(page.node('sw-global-reset').attributes['aria-checked'], 'false');
  assert.equal(page.node('sw-global-reset').disabled, false);
});

test('LIVE requires a fresh successful own check and a current public source', () => {
  const page = view();
  page.render({...state,checked_at:NOW,fetched_at:NOW-999999});
  assert.equal(page.node('reset-feed').textContent,'LIVE');
  page.render({...state,checked_at:NOW-91000});
  assert.equal(page.node('reset-feed').textContent,'Saved feed');
  page.render({...state,checked_at:NOW,cached:true});
  assert.equal(page.node('reset-feed').textContent,'Saved feed');
  page.render({...state,checked_at:NOW,feed:{stale:true}});
  assert.equal(page.node('reset-feed').textContent,'Source delayed');
  page.render({...state,checked_at:NOW,source_expires_at:NOW-1});
  assert.equal(page.node('reset-feed').textContent,'Source delayed');
});

test('next public check has a visible second-by-second countdown separate from account reset clocks', () => {
  const page=view();page.render({...state,checked_at:NOW-12000,next_check_at:NOW+45000});
  assert.equal(page.node('reset-updated').textContent,'Checked 12s ago');
  assert.equal(page.node('reset-next-check').textContent,'Next check 0:45');
  assert.equal(page.context.resetCheckCountdown(NOW+6000,NOW),'Next check 0:06');
});

test('banked availability describes the public lifecycle and never reports an own credit balance', () => {
  const page=view();page.render({...state,banked:{...event,kind:'banked',banked_state:'available',summary:'Fixture public banked update'},banked_notifications:true});
  assert.equal(page.node('reset-banked-state').textContent,'Source reports available');
  assert.match(page.node('reset-banked-detail').textContent,/not your own banked-credit balance/);
  page.render({...state,banked:{...event,preview:true,banked_state:'available'}});
  assert.equal(page.node('reset-banked-state').textContent,'Unconfirmed');
});

test('public timeline includes non-reset lifecycle and signal events without marking them confirmed', () => {
  const page=view();page.render({...state,events:[{...event,kind:'banked',banked_state:'arriving',confirmed:false},{...event,id:'signal',kind:'signal',confirmed:false}]});
  const rows=page.node('reset-history').children;
  assert.equal(rows[0].children[0].children[0].textContent,'banked');
  assert.equal(rows[0].children[0].children[1].textContent,'arriving');
  assert.equal(rows[1].children[0].children[1].textContent,'Source update');
});

test('banked preview uses its own endpoint and leaves the personal and global reset commands alone',async()=>{
  const page=view();const button=page.node('btn-banked-reset-preview');
  await button.handlers.click({currentTarget:button});
  assert.equal(page.calls[0][0],'preview_banked_reset_alert');assert.equal(button.disabled,false);
});
