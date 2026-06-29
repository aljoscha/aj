// Smoke test for the HTML export renderer (template.js).
//
// Runs the *real* vendored libraries and template.js against a fixture
// session in a minimal DOM shim, then asserts on the rendered HTML.
// This is a dev tool, not wired into cargo: cargo tests cover the
// server-side assembly, this covers the client-side rendering.
//
//   node src/aj/assets/export/smoke_test.mjs
//
// Exits non-zero on the first failed assertion.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import vm from 'node:vm';
import zlib from 'node:zlib';

const here = dirname(fileURLToPath(import.meta.url));
const read = (p) => readFileSync(join(here, p), 'utf8');

// ---- Fixture: exercises every renderer path. ----
const entries = [
  { id: 'root', thread: 'meta', type: 'system_prompt', text: 'You are aj.', timestamp: '2024-01-01T00:00:00Z' },
  { id: 'u1', parent_id: 'root', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:01Z',
    message: { role: 'user', content: [{ type: 'text', text: 'Fix the **bug**. Here is code:\n```rust\nfn main(){}\n```' }], timestamp: 0 } },
  { id: 'a1', parent_id: 'u1', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:02Z',
    message: { role: 'assistant', model: 'claude-test', provider: 'anthropic',
      content: [
        { type: 'thinking', thinking: 'Let me look around.', redacted: false },
        { type: 'text', text: 'Reading the file.' },
        { type: 'tool_call', id: 'c1', name: 'read_file', arguments: { path: '/home/me/x.rs' } },
        { type: 'tool_call', id: 'c2', name: 'bash', arguments: { command: 'cargo test' } },
        { type: 'tool_call', id: 'c3', name: 'edit', arguments: { path: '/home/me/x.rs' } },
        { type: 'tool_call', id: 'c4', name: 'agent', arguments: { task: 'investigate' } },
      ],
      usage: { input: 100, output: 50, cache_read: 0, cache_write: 0, total_tokens: 150, cost: { total: 0.01 } },
      stop_reason: 'ToolUse', timestamp: 0 } },
  { id: 'r1', parent_id: 'a1', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:03Z',
    message: { role: 'tool_result', tool_call_id: 'c1', tool_name: 'read_file',
      content: [{ type: 'text', text: 'body' }],
      details: { kind: 'text', summary: 'read_file /home/me/x.rs', body: Array.from({ length: 20 }, (_, i) => 'line ' + (i + 1)).join('\n') },
      is_error: false, timestamp: 0 } },
  { id: 'r2', parent_id: 'r1', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:04Z',
    message: { role: 'tool_result', tool_call_id: 'c2', tool_name: 'bash',
      content: [{ type: 'text', text: 'out' }],
      details: { kind: 'bash', command: 'cargo test', stdout: Array.from({ length: 12 }, (_, i) => 'out ' + (i + 1)).join('\n'), stderr: Array.from({ length: 7 }, (_, i) => 'warn ' + (i + 1)).join('\n'), exit_code: 1, truncated: false },
      is_error: true, timestamp: 0 } },
  { id: 'r3', parent_id: 'r2', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:05Z',
    message: { role: 'tool_result', tool_call_id: 'c3', tool_name: 'edit',
      content: [{ type: 'text', text: 'ok' }],
      details: { kind: 'diff', path: '/home/me/x.rs', before: 'fn main(){}\nold\n', after: 'fn main(){}\nnew\n' },
      is_error: false, timestamp: 0 } },
  // sub-agent run spawned by a1
  { id: 'sp', parent_id: 'a1', thread: 'subagent', agent_id: 1, type: 'sub_agent_spawn', task: 'investigate the bug',
    settings: { provider: 'anthropic', model_id: 'claude-test', thinking: 'off', speed: 'standard', verbosity: '' }, timestamp: '2024-01-01T00:00:06Z' },
  { id: 'sm', parent_id: 'sp', thread: 'subagent', agent_id: 1, type: 'message', timestamp: '2024-01-01T00:00:07Z',
    message: { role: 'assistant', model: 'claude-test', content: [{ type: 'text', text: 'sub-agent finding' }],
      usage: { input: 0, output: 0, cache_read: 0, cache_write: 0, total_tokens: 0, cost: { total: 0 } }, stop_reason: 'Stop', timestamp: 0 } },
  // the agent tool_result (successful report) on the user thread
  { id: 'r4', parent_id: 'r3', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:08Z',
    message: { role: 'tool_result', tool_call_id: 'c4', tool_name: 'agent',
      content: [{ type: 'text', text: 'sub-agent finding' }],
      details: { kind: 'sub_agent_report', agent_id: 1, task: 'investigate the bug', report: 'sub-agent finding' },
      is_error: false, timestamp: 0 } },
  { id: 'a2', parent_id: 'r4', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:09Z',
    message: { role: 'assistant', model: 'claude-test', content: [{ type: 'text', text: 'Done. <script>alert(1)</script>' }],
      usage: { input: 1, output: 1, cache_read: 0, cache_write: 0, total_tokens: 2, cost: { total: 0 } }, stop_reason: 'Stop', timestamp: 0 } },
  // Adversarial prose: every vector here must render inert.
  { id: 'a3', parent_id: 'a2', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:10Z',
    message: { role: 'assistant', model: 'claude-test', content: [{ type: 'text', text:
      '[js](javascript:alert(1)) [html](data:text/html,<script>x</script>) ' +
      '[breakout](https://e.com" onmouseover="alert(1)) raw <img src=x onerror=alert(1)> <svg onload=alert(2)>' }],
      usage: { input: 0, output: 0, cache_read: 0, cache_write: 0, total_tokens: 0, cost: { total: 0 } }, stop_reason: 'Stop', timestamp: 0 } },
  // A sibling branch off u1 (an edited/retried prompt) to exercise the
  // tree's branch connectors. It is off the active path to a3.
  { id: 'u1b', parent_id: 'u1', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:02Z',
    message: { role: 'user', content: [{ type: 'text', text: 'alternative branch' }], timestamp: 0 } },
];
const sessionData = { session_id: 'smoke-session', leaf_id: 'a3', entries };

// ---- Minimal DOM shim (enough for the renderer's init + tree build). ----
function makeEl(tag) {
  const el = {
    tagName: (tag || 'div').toUpperCase(),
    _text: '', _html: '', children: [], dataset: {}, style: {},
    classList: {
      _s: new Set(),
      add(c) { this._s.add(c); },
      remove(c) { this._s.delete(c); },
      toggle(c, f) { const on = f === undefined ? !this._s.has(c) : f; on ? this._s.add(c) : this._s.delete(c); return on; },
      contains(c) { return this._s.has(c); },
    },
    appendChild(c) { this.children.push(c); return c; },
    append(...cs) { for (const c of cs) this.children.push(c); },
    addEventListener(type, fn) { (this._on || (this._on = {}))[type] = ((this._on && this._on[type]) || []).concat(fn); },
    scrollIntoView() {},
    querySelector() { return null; },
    querySelectorAll() { return []; },
  };
  Object.defineProperty(el, 'textContent', { get() { return this._text; }, set(v) { this._text = String(v); } });
  Object.defineProperty(el, 'innerHTML', { get() { return this._html; }, set(v) { this._html = String(v); this.children = []; } });
  Object.defineProperty(el, 'className', { get() { return [...this.classList._s].join(' '); }, set(v) { this.classList._s = new Set(String(v).split(/\s+/).filter(Boolean)); } });
  return el;
}

const elements = {};
for (const id of ['session-data', 'header-container', 'messages', 'tree-container', 'tree-status',
  'sidebar', 'sidebar-overlay', 'hamburger', 'sidebar-close', 'tree-search']) {
  elements[id] = makeEl('div');
}
// Feed the island exactly as the exporter does: gzip-compressed,
// base64-encoded. This exercises the renderer's real inflate path.
elements['session-data'].textContent = zlib.gzipSync(JSON.stringify(sessionData)).toString('base64');

const documentShim = {
  getElementById: (id) => elements[id] || null,
  createElement: (tag) => makeEl(tag),
  querySelector: () => null,
  querySelectorAll: () => [],
  addEventListener: () => {},
};

const sandbox = { console, document: documentShim };
sandbox.window = sandbox;
sandbox.self = sandbox;
sandbox.globalThis = sandbox;
sandbox.getSelection = () => ({ toString: () => '' });
sandbox.setTimeout = (fn) => fn();
// Web APIs the data loader needs to inflate the gzip+base64 island. A
// fresh vm context has the ECMAScript intrinsics (Uint8Array, Promise)
// but none of these, so hand them in from the host.
sandbox.atob = atob;
sandbox.Blob = Blob;
sandbox.Response = Response;
sandbox.DecompressionStream = DecompressionStream;
vm.createContext(sandbox);

// Load the real libraries then the renderer, exactly as the page does.
vm.runInContext(read('vendor/marked.min.js'), sandbox, { filename: 'marked.min.js' });
vm.runInContext(read('vendor/highlight.min.js'), sandbox, { filename: 'highlight.min.js' });
vm.runInContext(read('template.js'), sandbox, { filename: 'template.js' });

// The renderer loads its data asynchronously (inflate is async), so wait
// for the first render to land before asserting.
const deadline = Date.now() + 3000;
while (!elements['messages'].innerHTML && Date.now() < deadline) {
  await new Promise((r) => setTimeout(r, 10));
}
if (!elements['messages'].innerHTML) {
  console.error('renderer did not produce output (data load failed?)');
  process.exit(1);
}

const rendered = elements['header-container'].innerHTML + '\n' + elements['messages'].innerHTML;

// The tree builds DOM nodes (not innerHTML), so flatten their text.
function nodeText(el) {
  let s = (el._text || '') + (el._html || '');
  for (const c of el.children || []) s += nodeText(c);
  return s;
}
const treeText = elements['tree-container'].children.map(nodeText).join('\n');
const treeStatus = elements['tree-status'].textContent;

// Fire a stored listener with a minimal event (for the navigation test).
function fire(el, type) {
  ((el._on && el._on[type]) || []).forEach((fn) => fn({ stopPropagation() {}, preventDefault() {}, target: el }));
}

// ---- Assertions. ----
let failures = 0;
function check(label, cond) {
  if (cond) {
    console.log('  ok   ' + label);
  } else {
    console.error('  FAIL ' + label);
    failures++;
  }
}
function has(label, needle) {
  check(label, rendered.includes(needle));
}
function hasnt(label, needle) {
  check(label + ' (absent)', !rendered.includes(needle));
}

console.log('header / stats');
has('session id', 'smoke-session');
has('token totals', '\u2191101');
has('cost', '$0.0100');
has('system prompt', 'You are aj.');
has('download JSONL button', 'download-json-btn');
has('copy-link button', 'class="copy-link-btn"');

console.log('messages');
has('user markdown bold', '<strong>bug</strong>');
has('code fence highlighted', 'hljs');
has('assistant model label', 'claude-test');
has('thinking block', 'thinking-block');

console.log('tools');
has('read_file summary', 'read_file /home/me/x.rs');
has('read_file head truncation', 'more lines');
has('read_file keeps head', 'line 1');
has('bash command', '$ cargo test');
has('bash tail truncation', 'earlier lines');
has('bash error styling', 'tool-execution error');
has('bash exit code', 'exit code 1');
has('stderr class', 'tool-output expandable stderr');
has('edit diff added', 'diff-added');
has('edit diff removed', 'diff-removed');

console.log('sub-agent');
has('sub-agent box', 'class="subagent"');
has('sub-agent id and task', 'sub-agent #1');
has('sub-agent task text', 'investigate the bug');
has('sub-agent nested message', 'sub-agent finding');
check('agent report not duplicated', rendered.split('sub-agent finding').length - 1 === 1);

console.log('security');
hasnt('raw script not live', '<script>alert(1)');
has('script escaped to text', '&lt;script&gt;');
hasnt('javascript: link blocked', 'href="javascript');
hasnt('data:text/html link blocked', 'href="data:text/html');
hasnt('attribute breakout blocked', 'e.com" onmouseover');
hasnt('raw img not live', '<img src=x onerror');
hasnt('raw svg not live', '<svg onload');

console.log('tree');
check('tree has nodes', elements['tree-container'].children.length > 0);
check('tree user node', treeText.includes('user:'));
check('tree assistant node', treeText.includes('assistant:'));
check('tree tool node', treeText.includes('[bash:') || treeText.includes('[read:'));
check('tree status line', /\d+ \/ \d+ entries/.test(treeStatus));
check('tree node text escaped', !treeText.includes('<script>alert'));
check('tree shows branch sibling', treeText.includes('alternative branch'));
check('tree draws branch connectors', treeText.includes('\u251c') || treeText.includes('\u2514'));

// Navigating to a sibling branch must rebuild the tree (not just update
// markers), so the node set and the status line stay in sync. This
// guards the stale-tree regression.
console.log('navigation');
const branchNode = elements['tree-container'].children.find((n) => n.dataset.id === 'u1b');
check('sibling branch node present', !!branchNode);
if (branchNode) {
  fire(branchNode, 'click');
  check('navigation switched branch', elements['messages'].innerHTML.includes('alternative branch'));
  const m = elements['tree-status'].textContent.match(/(\d+) \/ \d+/);
  check('tree rebuilt: node count matches status', !!m && Number(m[1]) === elements['tree-container'].children.length);
  const active = elements['tree-container'].children.find((n) => n.classList.contains('active'));
  check('active node moved to clicked branch', !!active && active.dataset.id === 'u1b');
}

console.log('');
if (failures) {
  console.error(failures + ' assertion(s) failed');
  process.exit(1);
}
console.log('all checks passed');
