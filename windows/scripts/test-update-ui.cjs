const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const html = fs.readFileSync(path.join(__dirname, '../codenotch/ui/settings.html'), 'utf8');
const start = html.indexOf('/* ---- updates ');
const end = html.indexOf('/* ---- start ', start);
assert.ok(start >= 0 && end > start);

const calls = [];
const button = { disabled: false, textContent: '', addEventListener(_event, callback) { this.click = callback; } };
const status = { textContent: '' };
const automatic = {disabled:false,checked:false,addEventListener(_event,callback){this.click=callback;}};
const help = {textContent:''};
let savedAuto = true;
const listeners = {};
const context = vm.createContext({
  document: { getElementById: id => ({'btn-update':button,'update-status':status,'sw-auto-update':automatic,'cap-auto-update':help})[id] },
  ui: text => text,
  invoke: (command,args) => { calls.push(command);if(command==='set_auto_update')savedAuto=args.on;return Promise.resolve(['get_update_state','set_auto_update'].includes(command)?{auto_update:savedAuto,portable:false,signing_ready:true}:{}); },
  drawSwitch(node,state){node.disabled=state.busy;node.checked=state.value;},
  toast(){},
  strip: error => { throw new Error(error); },
  errText: error => String(error),
  window: { __TAURI__: { event: { listen: (name, callback) => { listeners[name] = callback; } } } },
});
vm.runInContext(html.slice(start, end), context);

(async () => {
  await new Promise(setImmediate);
  assert.equal(status.textContent, '', 'unchecked must not say up to date');
  assert.equal(button.textContent, 'Check for updates');
  assert.equal(automatic.checked,true);
  assert.match(help.textContent,/installs verified updates/);
  await automatic.click();
  assert.equal(savedAuto,false);
  assert.equal(automatic.checked,false);
  context.renderUpdate({auto_update:true,portable:true,signing_ready:true});
  assert.match(help.textContent,/Download the installer/);
  context.renderUpdate({auto_update:true,portable:false,signing_ready:false});
  assert.match(help.textContent,/New releases can be downloaded/);

  listeners.update_state({ payload: { available: '1.20.0', checked: true, can_install: false } });
  assert.equal(status.textContent, '1.20.0');
  assert.equal(button.textContent, 'Download installer');
  button.click();
  assert.equal(calls.at(-1), 'open_update_installer');

  context.renderUpdate({ available: '1.20.0', checked: true, can_install: true });
  assert.equal(button.textContent, 'Update');
  button.click();
  assert.equal(calls.at(-1), 'install_update');

  context.renderUpdate({ checking: true,auto_update:true });
  assert.equal(button.disabled, true);
  assert.equal(status.textContent, 'Checking…');
  assert.equal(automatic.disabled,false,'turning automatic mode off remains possible during a check');

  context.renderUpdate({ message: 'Could not check for updates' });
  assert.equal(button.disabled, false);
  assert.equal(status.textContent, 'Could not check for updates');
  button.click();
  assert.equal(calls.at(-1), 'check_for_update');

  context.renderUpdate({ checked: true });
  assert.equal(status.textContent, 'Up to date');
  context.renderUpdate({ available: '1.20.0', checked: true, can_install: false, message: 'Could not install the update' });
  assert.equal(status.textContent, 'Could not install the update');
  assert.equal(button.textContent, 'Download installer');
  console.log('PASS: automatic toggle, portable help, unchecked, manual, signed, busy, error and current update states');
})().catch(error => { console.error(error); process.exitCode = 1; });
