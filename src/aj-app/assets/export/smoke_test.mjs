// Smoke test for the HTML export renderer (template.js).
//
// Runs the *real* vendored libraries and template.js against a fixture
// session in a minimal DOM shim, then asserts on the rendered HTML.
// A cargo test launches this script when Node is available. Rust tests cover
// the server-side assembly, while this covers the client-side rendering.
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

// ---- Fixture: exercises every renderer path, including both diff formats. ----
// `projectedTextBody` is the full shape the Rust exporter emits after resolving
// a compact Text body reference.
const projectedTextBody = Array.from({ length: 20 }, (_, i) => 'line ' + (i + 1)).join('\n');
const entries = [
  { id: 'root', thread: 'meta', type: 'system_prompt', text: 'You are aj.', timestamp: '2024-01-01T00:00:00Z' },
  { id: 'u1', parent_id: 'root', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:01Z',
    message: { role: 'user', content: [{ type: 'text', text: 'Fix the **bug**. Here is code:\n```rust\nfn main(){}\n```' }], timestamp: 0 } },
  // The exporter preserves keys but replaces every value before the renderer
  // sees it. This state entry stays out of the default tree, appears under
  // All with a keys-only label, and renders a real navigation target.
  { id: 'env1', parent_id: 'u1', thread: 'user', type: 'env_change', timestamp: '2024-01-01T00:00:01Z',
    env: { BEADS_ACTOR: '[redacted]', SECRET_TOKEN: '[redacted]', ['line\nbidi\u202e']: '[redacted]',
      ['quote"slash\\']: '[redacted]', ['unicode界']: '[redacted]' } },
  { id: 'a1', parent_id: 'u1', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:02Z',
    message: { role: 'assistant', model: 'claude-test', provider: 'anthropic',
      content: [
        { type: 'thinking', thinking: 'Let me look around.', redacted: false },
        { type: 'text', text: 'Reading the file.' },
        { type: 'tool_call', id: 'c1', name: 'read_file', arguments: { path: '/home/me/x.rs' } },
        { type: 'tool_call', id: 'c2', name: 'bash', arguments: { command: 'cargo test' } },
        { type: 'tool_call', id: 'c3', name: 'edit', arguments: { path: '/home/me/x.rs' } },
        { type: 'tool_call', id: 'c6', name: 'edit_file', arguments: { path: '/home/me/y.rs' } },
        { type: 'tool_call', id: 'c7', name: 'edit_file', arguments: { path: '/home/me/z.rs' } },
        { type: 'tool_call', id: 'c8', name: 'edit_file', arguments: { path: '/home/me/multiline.rs' } },
        { type: 'tool_call', id: 'c9', name: 'read_file', arguments: { path: '/home/me/future.rs' } },
        { type: 'tool_call', id: 'c4', name: 'agent', arguments: { task: 'investigate' } },
      ],
      usage: { input: 100, output: 50, cache_read: 0, cache_write: 0, total_tokens: 150, cost: { total: 0.01 }, incomplete: true },
      stop_reason: 'ToolUse', timestamp: 0 } },
  { id: 'r1', parent_id: 'a1', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:03Z',
    message: { role: 'tool_result', tool_call_id: 'c1', tool_name: 'read_file',
      content: [{ type: 'text', text: projectedTextBody }],
      details: { kind: 'text', summary: 'read_file /home/me/x.rs', body: projectedTextBody + '\n' },
      is_error: false, timestamp: 0 } },
  { id: 'r2', parent_id: 'r1', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:04Z',
    message: { role: 'tool_result', tool_call_id: 'c2', tool_name: 'bash',
      content: [{ type: 'text', text: 'out' }],
      details: { kind: 'bash', command: 'cargo test', stdout: Array.from({ length: 12 }, (_, i) => 'out ' + (i + 1)).join('\n'), stderr: Array.from({ length: 7 }, (_, i) => 'warn ' + (i + 1)).join('\n'), exit_code: 1, truncated: false },
      is_error: true, timestamp: 0 } },
  { id: 'r3', parent_id: 'r2', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:05Z',
    message: { role: 'tool_result', tool_call_id: 'c3', tool_name: 'edit',
      content: [{ type: 'text', text: 'ok' }],
      details: { kind: 'diff', format: 'future-v2', path: '/home/me/x.rs',
        lines: ['+ unknown stored line'], before: 'fn main(){}\nold\n', after: 'fn main(){}\nnew\n' },
      is_error: false, timestamp: 0 } },
  { id: 'r3c', parent_id: 'r3', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:05Z',
    message: { role: 'tool_result', tool_call_id: 'c6', tool_name: 'edit_file',
      content: [{ type: 'text', text: 'ok' }],
      details: { kind: 'diff', format: 'display-v1', path: '/home/me/y.rs',
        lines: ['--- a//home/me/y.rs', '+++ b//home/me/y.rs', '  keep', '- stale compact', '+ fresh compact'] },
      is_error: false, timestamp: 0 } },
  { id: 'r3m', parent_id: 'r3c', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:05Z',
    message: { role: 'tool_result', tool_call_id: 'c7', tool_name: 'edit_file',
      content: [{ type: 'text', text: 'malformed compact fallback' }],
      details: { kind: 'diff', format: 'display-v1', path: '/home/me/z.rs',
        lines: ['--- a//home/me/z.rs', '+++ b//home/me/z.rs', 'bogus must not render'] },
      is_error: false, timestamp: 0 } },
  { id: 'r3n', parent_id: 'r3m', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:05Z',
    message: { role: 'tool_result', tool_call_id: 'c8', tool_name: 'edit_file',
      content: [{ type: 'text', text: 'multiline compact fallback' }],
      details: { kind: 'diff', format: 'display-v1', path: '/home/me/multiline.rs',
        lines: ['--- a//home/me/multiline.rs', '+++ b//home/me/multiline.rs', '+ first\n+ must not split'] },
      is_error: false, timestamp: 0 } },
  { id: 'r3f', parent_id: 'r3n', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:05Z',
    message: { role: 'tool_result', tool_call_id: 'c9', tool_name: 'read_file',
      content: [{ type: 'text', text: 'future marker model-facing content' }],
      details: { kind: 'text', summary: 'future marker summary',
        body_ref: { source: 'future_content', append_newline: false } },
      is_error: false, timestamp: 0 } },
  // The spawn event can be persisted after sibling tool results from the same
  // assistant turn, so its parent is the current main-thread head rather than
  // necessarily the assistant entry that contains the `agent` call.
  { id: 'sp', parent_id: 'r3f', thread: 'subagent', agent_id: 1, type: 'sub_agent_spawn', task: 'investigate the bug',
    settings: { provider: 'anthropic', model_id: 'claude-test', thinking: 'off', speed: 'standard', verbosity: '' }, timestamp: '2024-01-01T00:00:06Z' },
  { id: 'sm', parent_id: 'sp', thread: 'subagent', agent_id: 1, type: 'message', timestamp: '2024-01-01T00:00:07Z',
    message: { role: 'assistant', model: 'claude-test', content: [{ type: 'text', text: 'sub-agent finding' }],
      usage: { input: 0, output: 0, cache_read: 0, cache_write: 0, total_tokens: 0, cost: { total: 0 } }, stop_reason: 'Stop', timestamp: 0 } },
  // the agent tool_result (successful report) on the user thread
  { id: 'r4', parent_id: 'r3f', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:08Z',
    message: { role: 'tool_result', tool_call_id: 'c4', tool_name: 'agent',
      content: [{ type: 'text', text: 'sub-agent finding' }],
      details: { kind: 'sub_agent_report', agent_id: 1, task: 'investigate the bug', report: 'sub-agent finding' },
      is_error: false, timestamp: 0 } },
  // A background-task completion notice on the user thread: the typed
  // `role:"task_notification"` shape the exporter emits.
  { id: 'n1', parent_id: 'r4', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:08Z',
    message: { role: 'task_notification', label: 'cargo build', kind: 'bash',
      outcome: { status: 'succeeded' }, body: 'exit code 0' } },
  { id: 'a2', parent_id: 'n1', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:09Z',
    message: { role: 'assistant', model: 'claude-test', content: [{ type: 'text', text: 'Done. <script>alert(1)</script>' },
      { type: 'tool_call', id: 'c5', name: 'agent', arguments: { task: 'double-check' } }],
      usage: { input: 1, output: 1, cache_read: 0, cache_write: 0, total_tokens: 2, cost: { total: 0 } }, stop_reason: 'ToolUse', timestamp: 0 } },
  // A second sub-agent, spawned by a2 (a spine continuation, not a fork
  // child). It must hang one level in off a2 while a2's own continuation
  // (a3) stays on the spine at a2's indent. Exercises the spine layout.
  { id: 'sp2', parent_id: 'a2', thread: 'subagent', agent_id: 2, type: 'sub_agent_spawn', task: 'double-check the work',
    settings: { provider: 'anthropic', model_id: 'claude-test', thinking: 'off', speed: 'standard', verbosity: '' }, timestamp: '2024-01-01T00:00:09Z' },
  { id: 'sm2', parent_id: 'sp2', thread: 'subagent', agent_id: 2, type: 'message', timestamp: '2024-01-01T00:00:09Z',
    message: { role: 'assistant', model: 'claude-test', content: [{ type: 'text', text: 'checked, looks good' }],
      usage: { input: 0, output: 0, cache_read: 0, cache_write: 0, total_tokens: 0, cost: { total: 0 } }, stop_reason: 'Stop', timestamp: 0 } },
  // Adversarial prose: every vector here must render inert.
  { id: 'a3', parent_id: 'a2', thread: 'user', type: 'message', timestamp: '2024-01-01T00:00:10Z',
    message: { role: 'assistant', model: 'claude-test', content: [{ type: 'text', text:
      '[js](javascript:alert(1)) [html](data:text/html,<script>x</script>) ' +
      '[breakout](https://e.com" onmouseover="alert(1)) raw <img src=x onerror=alert(1)> <svg onload=alert(2)>' }],
      usage: { input: 0, output: 0, cache_read: 0, cache_write: 0, total_tokens: 0, cost: { total: 0 } }, stop_reason: 'Stop', timestamp: 0 } },
  // A compaction checkpoint carrying the summarizer's own spend. Its
  // exchange is never a message entry, so this is the only place that
  // money exists: an export that folds message usage alone reports a
  // compacted session as cheaper than it was. Off the active path (the
  // leaf stays a3), because `computeStats` walks every entry rather than
  // the active branch, which is what makes it the right fixture for the
  // fold and not for the tree. Numbers chosen to stay under
  // `formatTokens`' 1000-token rounding so the assertion is exact.
  { id: 'k1', parent_id: 'a3', thread: 'user', type: 'compaction', timestamp: '2024-01-01T00:00:11Z',
    summary: 'earlier turns summarized', first_kept_entry_id: 'a2', tokens_before: 4321,
    usage: { input: 400, output: 99, cache_read: 0, cache_write: 0, total_tokens: 499, cost: { total: 0.25 } } },
  // The absent usage object is direct evidence that another counted
  // summarizer run has no recorded subtotal.
  { id: 'k2', parent_id: 'k1', thread: 'user', type: 'compaction', timestamp: '2024-01-01T00:00:12Z',
    summary: 'legacy unaccounted compaction', first_kept_entry_id: 'a2', tokens_before: 2000 },
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

async function renderData(data) {
  const elements = {};
  for (const id of ['session-data', 'header-container', 'messages', 'tree-container', 'tree-status',
    'sidebar', 'sidebar-overlay', 'hamburger', 'sidebar-close', 'tree-search', 'toggle-subagents']) {
    elements[id] = makeEl('div');
  }
  // Feed the island exactly as the exporter does: gzip-compressed,
  // base64-encoded. This exercises the renderer's real inflate path.
  elements['session-data'].textContent = zlib.gzipSync(JSON.stringify(data)).toString('base64');

  // Filter radio buttons, queried by class (not id) exactly as the page
  // wires them. `default` starts active, matching template.html.
  const filterButtons = ['default', 'no-tools', 'user-only', 'all'].map((mode) => {
    const btn = makeEl('button');
    btn.dataset.filter = mode;
    if (mode === 'default') btn.classList.add('active');
    return btn;
  });

  // The shim does not parse assigned innerHTML. Resolve transcript ids from
  // the real rendered markup so navigation remains observable.
  const scrolledTargets = [];
  const documentShim = {
    getElementById: (id) => {
      if (elements[id]) return elements[id];
      if (!elements['messages'].innerHTML.includes('id="' + id + '"')) return null;
      const renderedElement = makeEl('div');
      renderedElement.scrollIntoView = () => scrolledTargets.push(id);
      return renderedElement;
    },
    createElement: (tag) => makeEl(tag),
    querySelector: () => null,
    querySelectorAll: (sel) => (sel === '.filter-btn' ? filterButtons : []),
    addEventListener: () => {},
  };

  const sandbox = { console, document: documentShim };
  sandbox.window = sandbox;
  sandbox.self = sandbox;
  sandbox.globalThis = sandbox;
  sandbox.getSelection = () => ({ toString: () => '' });
  sandbox.setTimeout = (fn) => fn();
  // Web APIs the data loader needs to inflate the gzip+base64 island. A
  // fresh vm context has the ECMAScript intrinsics but none of these.
  sandbox.atob = atob;
  sandbox.Blob = Blob;
  sandbox.Response = Response;
  sandbox.DecompressionStream = DecompressionStream;
  vm.createContext(sandbox);

  vm.runInContext(read('vendor/marked.min.js'), sandbox, { filename: 'marked.min.js' });
  vm.runInContext(read('template.js'), sandbox, { filename: 'template.js' });

  const deadline = Date.now() + 3000;
  while (!elements['messages'].innerHTML && Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 10));
  }
  if (!elements['messages'].innerHTML) {
    throw new Error('renderer did not produce output (data load failed?)');
  }
  return { elements, filterButtons, scrolledTargets };
}

const { elements, filterButtons, scrolledTargets } = await renderData(sessionData);

const rendered = elements['header-container'].innerHTML + '\n' + elements['messages'].innerHTML;

// The tree builds DOM nodes (not innerHTML), so flatten their text.
function nodeText(el) {
  let s = (el._text || '') + (el._html || '');
  for (const c of el.children || []) s += nodeText(c);
  return s;
}
const treeText = elements['tree-container'].children.map(nodeText).join('\n');
const treeStatus = elements['tree-status'].textContent;

// Per-tree-node depth, read from the prefix span (3 glyph columns per
// indent level), plus its content and entry id. Lets the layout
// assertions check indentation relationships directly.
function treeRows() {
  return elements['tree-container'].children.map((n) => ({
    prefix: (n.children[0] && n.children[0]._text) || '',
    indent: Math.floor(((n.children[0] && n.children[0]._text) || '').length / 3),
    content: nodeText(n.children[2] || makeEl('div')),
    id: n.dataset.id,
  }));
}

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

// Extract the balanced <div>...</div> that opens at the element whose tag
// carries `marker` (e.g. an `id="..."` attribute), so we can assert on
// what is and isn't nested inside that element.
function divRegion(html, marker) {
  const at = html.indexOf(marker);
  if (at < 0) return '';
  const open = html.lastIndexOf('<div', at);
  const tag = /<\/?div\b/g;
  tag.lastIndex = open;
  let depth = 0, m;
  while ((m = tag.exec(html))) {
    depth += m[0] === '</div' ? -1 : 1;
    if (depth === 0) return html.slice(open, html.indexOf('>', m.index) + 1);
  }
  return html.slice(open);
}

console.log('header / stats');
has('session id', 'smoke-session');
// 101 from the assistant messages plus the compaction's 400: an export
// that folds message usage alone stops at 101 here and at $0.0100 below.
has('token totals', '\u2191501');
has('cost', '$0.2600');
has('compactions counted', '2 compactions');
check('usage status appears once', rendered.split('partial (recorded usage only)').length - 1 === 1);
has('usage status label', '<span class="info-label">Usage:</span><span class="info-value">partial (recorded usage only)</span>');
const completeData = JSON.parse(JSON.stringify(sessionData));
delete completeData.entries.find((entry) => entry.id === 'a1').message.usage.incomplete;
completeData.entries = completeData.entries.filter((entry) => entry.id !== 'k2');
const completeElements = (await renderData(completeData)).elements;
check('complete legacy header has no usage status', !completeElements['header-container'].innerHTML.includes('partial (recorded usage only)'));
const explicitData = JSON.parse(JSON.stringify(sessionData));
explicitData.entries = explicitData.entries.filter((entry) => entry.id !== 'k2');
const explicitElements = (await renderData(explicitData)).elements;
check('explicit incomplete usage marks the header', explicitElements['header-container'].innerHTML.includes('partial (recorded usage only)'));
const missingCompactionData = JSON.parse(JSON.stringify(sessionData));
delete missingCompactionData.entries.find((entry) => entry.id === 'a1').message.usage.incomplete;
const missingCompactionElements = (await renderData(missingCompactionData)).elements;
check('missing compaction usage marks the header', missingCompactionElements['header-container'].innerHTML.includes('partial (recorded usage only)'));
has('system prompt', 'You are aj.');
has('download JSONL button', 'download-json-btn');
has('copy-link button', 'class="copy-link-btn"');

console.log('messages');
has('user markdown bold', '<strong>bug</strong>');
has('code fence rendered as block', '<pre><code>');
has('code fence content escaped', 'fn main(){}');
has('assistant model label', 'claude-test');
has('thinking block', 'thinking-block');
hasnt('off-path environment target is absent before navigation', 'id="entry-env1"');
hasnt('environment entry never renders redacted values', '[redacted]');
hasnt('environment entry never renders a raw bidi override', '\u202e');
hasnt('environment entry never renders raw non-ASCII key text', '界');

console.log('tools');
has('read_file summary', 'read_file /home/me/x.rs');
has('read_file head truncation', 'more lines');
has('read_file keeps head', 'line 1');
has('bash command', '$ cargo test');
has('bash tail truncation', 'earlier lines');
has('bash error styling', 'tool-execution error');
has('bash exit code', 'exit code 1');
has('stderr class', 'tool-output expandable stderr');
has('legacy fallback diff added', '<div class="diff-added">+ new</div>');
has('legacy fallback diff removed', '<div class="diff-removed">- old</div>');
hasnt('unknown compact lines rejected', 'unknown stored line');
has('malformed compact uses model-facing fallback', 'malformed compact fallback');
hasnt('arbitrary compact line not rendered', 'bogus must not render');
has('multiline compact uses model-facing fallback', 'multiline compact fallback');
hasnt('multiline compact line not rendered', 'must not split');
has('future text marker keeps summary', 'future marker summary');
has('future text marker uses model-facing fallback', 'future marker model-facing content');
has('compact edit diff added', '<div class="diff-added">+ fresh compact</div>');
has('compact edit diff removed', '<div class="diff-removed">- stale compact</div>');
hasnt('stored diff old header suppressed', '--- a/');
hasnt('stored diff new header suppressed', '+++ b/');
check('compact diff path renders once', rendered.split('/home/me/y.rs').length - 1 === 1);
// Tool executions are siblings of the assistant bubble, not nested inside
// it (matching the TUI). The bubble for a1 must close before its tools.
const a1box = divRegion(elements['messages'].innerHTML, 'id="entry-a1"');
check('assistant bubble present for a turn with text', a1box.includes('Reading the file'));
check('tool execution is a sibling, not nested in the assistant bubble', !!a1box && !a1box.includes('tool-execution'));

console.log('sub-agent');has('sub-agent box', 'class="subagent"');
has('sub-agent id and task', 'sub-agent #1');
has('sub-agent task text', 'investigate the bug');
has('sub-agent nested message', 'sub-agent finding');
check('agent report not duplicated', rendered.split('sub-agent finding').length - 1 === 1);
const legacySubData = JSON.parse(JSON.stringify(sessionData));
legacySubData.entries = legacySubData.entries.filter((entry) => entry.id !== 'sp' && entry.id !== 'sm');
const legacySubView = await renderData(legacySubData);
check('legacy agent report remains visible without a spawn root', legacySubView.elements['messages'].innerHTML.includes('sub-agent finding'));
const legacyAgentRow = legacySubView.elements['tree-container'].children.find((n) => nodeText(n).includes('[agent:'));
check('legacy agent report remains in the tree', !!legacyAgentRow);
if (legacyAgentRow) fire(legacyAgentRow, 'click');
check('legacy agent result navigates to its tool block', legacySubView.scrolledTargets.includes('tool-call-c4'));
const incompleteSubData = JSON.parse(JSON.stringify(sessionData));
incompleteSubData.entries = incompleteSubData.entries.filter((entry) => entry.id !== 'sm');
const incompleteSubView = await renderData(incompleteSubData);
check('agent report remains visible when its child transcript is missing', incompleteSubView.elements['messages'].innerHTML.includes('sub-agent finding'));
check('incomplete agent report remains in the tree',
  incompleteSubView.elements['tree-container'].children.some((n) => nodeText(n).includes('[agent:')));

console.log('task notification');
has('task notification block', 'class="task-notification"');
has('task notification head', 'task cargo build succeeded');
has('task notification body', 'exit code 0');
hasnt('notice not rendered as a user bubble', 'class="msg user" id="entry-n1"');
has('user stat excludes the notice', '2 user');

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
check('default tree hides environment state', !treeText.includes('[environment:'));

// Spine layout: a sub-agent run hangs one level in off the message that
// spawned it and renders before the conversation continues, while the
// main thread stays on its spine (not indented by the run). sp2 is
// spawned by a2; a3 is a2's conversation continuation.
// The status denominator is the structural set (what the widest filter
// can show given the sub-agent toggle), not the raw entry count. So
// "All" with no search reads N / N, and narrowing the mode lowers the
// numerator while the denominator holds.
console.log('filter denominator');
{
  const byMode = (m) => filterButtons.find((b) => b.dataset.filter === m);
  fire(byMode('no-tools'), 'click');
  check('no-tools tree hides environment state',
    !elements['tree-container'].children.some((n) => n.dataset.id === 'env1'));
  fire(byMode('all'), 'click');
  const all = elements['tree-status'].textContent.match(/(\d+) \/ (\d+)/);
  check('all filter reads N / N', !!all && all[1] === all[2]);
  const envNode = elements['tree-container'].children.find((n) => n.dataset.id === 'env1');
  check('all filter gives environment state a keys-only row',
    !!envNode && nodeText(envNode).includes('environment:') &&
      nodeText(envNode).includes('BEADS_ACTOR') && nodeText(envNode).includes('SECRET_TOKEN') &&
      nodeText(envNode).includes('line\\nbidi\\u{202e}') &&
      !nodeText(envNode).includes('[redacted]') && !nodeText(envNode).includes('\u202e'));
  if (envNode) {
    fire(envNode, 'click');
    const envTranscript = elements['messages'].innerHTML;
    check('environment row navigates to its rendered transcript target',
      envTranscript.includes('id="entry-env1"'));
    check('environment target names keys without values',
      envTranscript.includes('session environment keys: &quot;BEADS_ACTOR&quot;, &quot;SECRET_TOKEN&quot;') &&
        envTranscript.includes('line\\nbidi\\u{202e}') &&
        envTranscript.includes('quote\\&quot;slash\\\\') &&
        envTranscript.includes('unicode\\u{754c}') &&
        !envTranscript.includes('[redacted]') && !envTranscript.includes('\u202e') &&
        !envTranscript.includes('界'));
    check('environment target lookup reaches the scroll path',
      scrolledTargets.includes('entry-env1'));
    const activeEnv = elements['tree-container'].children.find((n) => n.classList.contains('active'));
    check('environment row becomes the active target', !!activeEnv && activeEnv.dataset.id === 'env1');
  }
  fire(byMode('no-tools'), 'click');
  check('no-tools hides environment state at the active head',
    !elements['tree-container'].children.some((n) => n.dataset.id === 'env1'));
  fire(byMode('user-only'), 'click');
  check('user-only hides environment state at the active head',
    !elements['tree-container'].children.some((n) => n.dataset.id === 'env1'));
  const narrow = elements['tree-status'].textContent.match(/(\d+) \/ (\d+)/);
  check('narrowing keeps the same denominator', !!narrow && narrow[2] === all[2]);
  check('narrowing lowers the numerator', !!narrow && Number(narrow[1]) < Number(all[1]));
  fire(byMode('default'), 'click'); // restore the default view for later checks
  check('default hides environment state at the active head',
    !elements['tree-container'].children.some((n) => n.dataset.id === 'env1'));
}

console.log('layout');
{
  const rows = treeRows();
  const at = (id) => rows.find((r) => r.id === id);
  const has2 = (s) => rows.find((r) => r.content.includes(s));
  const a1 = at('a1'), a2 = at('a2'), a3 = at('a3');
  const sp1 = has2('sub-agent #1'), sp2 = has2('sub-agent #2');
  check('sub-agent run hangs one level in off its spawning message', !!(a2 && sp2) && sp2.indent === a2.indent + 1);
  check('delayed spawn root hangs off its spawning message', !!(a1 && sp1) && sp1.indent === a1.indent + 1);
  check('main thread is not indented by a sub-agent run', !!(a2 && a3) && a3.indent === a2.indent);
  check('sub-agent run renders before the conversation continues', !!(sp2 && a3) && rows.indexOf(sp2) < rows.indexOf(a3));
  // A run followed by a spine continuation keeps the connector open (├)
  // so the spine threads down to the continuation.
  check('run before a spine continuation keeps the connector open', !!sp2 && sp2.prefix.includes('\u251c') && !sp2.prefix.includes('\u2514'));
}

// Sub-agent runs appear in the tree by default and the toggle hides them.
console.log('sub-agents');
check('sub-agent run shown in tree by default', treeText.includes('sub-agent #1'));
check('sub-agent task in tree', treeText.includes('investigate the bug'));
check('sub-agent message in tree', treeText.includes('sub-agent finding'));
// The successful `agent` tool result is not listed as its own tree node
// while sub-agent rows are shown: the spawn node already names the task.
check('agent tool result not duplicated in tree', !treeText.includes('[agent:'));
const subToggle = elements['toggle-subagents'];
check('sub-agent toggle present', !!subToggle);
if (subToggle) {
  check('sub-agent toggle active by default', subToggle.classList.contains('active'));
  fire(subToggle, 'click');
  const hidden = elements['tree-container'].children.map(nodeText).join('\n');
  check('sub-agent hidden after toggle', !hidden.includes('sub-agent #1'));
  check('conversation still present when hidden', hidden.includes('user:'));
  // With the spawn rows gone, the agent result is the only trace of the
  // run left on the conversation thread, so it reappears.
  check('agent tool result shown in tree when sub-agents hidden', hidden.includes('[agent:'));
  fire(subToggle, 'click');
  const reshown = elements['tree-container'].children.map(nodeText).join('\n');
  check('sub-agent shown again after toggling back', reshown.includes('sub-agent #1'));
}

// Clicking a sub-agent row opens its inline box and keeps the full
// conversation visible (it routes to the host assistant's branch).
const spawnNode = elements['tree-container'].children.find((n) => n.dataset.id === 'sp');
check('sub-agent spawn node present', !!spawnNode);
if (spawnNode) {
  fire(spawnNode, 'click');
  check('clicking sub-agent renders its inline box', elements['messages'].innerHTML.includes('id="subagent-1"'));
  check('clicking sub-agent keeps full conversation', elements['messages'].innerHTML.includes('Done.'));
}

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
