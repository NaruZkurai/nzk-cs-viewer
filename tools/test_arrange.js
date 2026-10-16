// Regression test for the box-select arrange toggle.
//
//   node tools/test_arrange.js
//
// Runs frontend/static/arrange.js in Node against a fake context (the module only
// touches `window`, `Set`/`Map` and the collaborators on `ctx`), then checks that
// Ctrl+Shift+V / H flip on the first press and *un-flip* on the next. The original
// bug derived the flip from a fresh `sortByStep` every time, so it reversed the
// same base order on every press and the selection stayed flipped forever.

const fs = require('fs');
const path = require('path');

const root = path.resolve(__dirname, '..');
const src = fs.readFileSync(path.join(root, 'frontend', 'static', 'arrange.js'), 'utf8');

global.window = {};
// eslint-disable-next-line no-eval
(0, eval)(src);

const IDS = ['A', 'B', 'C'];
const pos = new Map();
const nodeById = new Map();
IDS.forEach((id, i) => {
  nodeById.set(id, { id, label: id, file: 'f.cs', line: i + 1 });
  pos.set(id, { x: 0, y: 0, tx: 0, ty: 0, fixed: false, seeded: true, vx: 0, vy: 0 });
});

const classList = () => ({ add() {}, remove() {}, toggle() {}, contains() { return false; } });
const state = {
  nodeById,
  nodes: IDS.map((id) => nodeById.get(id)),
  edges: [],
  view: { nodes: [] },
  layout: { run: null },
  selected: null,
  camera: { x: 0, y: 0, zoom: 1 }
};
const arrange = window.CSArrange({
  state,
  canvas: { classList: classList() },
  nodeDrawerEl: { classList: classList() },
  toast: () => {},
  posOf: (id) => pos.get(id),
  chipSizeFor: () => ({ w: 100, h: 30 }),
  worldToScreen: (x, y) => ({ x, y }),
  selectNode: () => {},
  loadDetail: () => {},
  renderNodeDrawer: () => {},
  modeIsFind: () => false,
  scheduleViewerSave: () => {}
});
arrange.set(IDS, { silent: true });

const along = (axis) => IDS.slice().sort((a, b) => {
  const pa = pos.get(a);
  const pb = pos.get(b);
  return axis === 'h' ? pa.x - pb.x : pa.y - pb.y;
}).join('');

let failures = 0;
function expect(label, actual, want) {
  const ok = actual === want;
  if (!ok) failures++;
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${label}: ${actual} (want ${want})`);
}

arrange.arrange('v');
expect('V press 1 flips', along('v'), 'CBA');
expect('V press 2 un-flips', (arrange.arrange('v'), along('v')), 'ABC');
expect('V press 3 flips again', (arrange.arrange('v'), along('v')), 'CBA');
expect('H press 1 orders by step', (arrange.arrange('h'), along('h')), 'ABC');
expect('H press 2 flips', (arrange.arrange('h'), along('h')), 'CBA');
expect('H press 3 un-flips', (arrange.arrange('h'), along('h')), 'ABC');

console.log(failures ? `\nFAIL: ${failures} check(s)` : '\nPASS: arrange toggle');
process.exit(failures ? 1 : 0);
