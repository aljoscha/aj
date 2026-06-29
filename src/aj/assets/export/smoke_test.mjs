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
];
const sessionData = { session_id: 'smoke-session', leaf_id: 'a2', entries };

// ---- Minimal DOM shim. ----
const elements = {
  'session-data': { textContent: JSON.stringify(sessionData) },
  'header-container': { innerHTML: '' },
  'messages': { innerHTML: '' },
};
const noopEl = { addEventListener() {}, style: {}, classList: { toggle() {}, add() {}, remove() {} } };
const documentShim = {
  getElementById: (id) => elements[id] || null,
  querySelector: () => null,
  querySelectorAll: () => [],
  addEventListener: () => {},
};

const sandbox = { console, document: documentShim };
sandbox.window = sandbox;
sandbox.self = sandbox;
sandbox.globalThis = sandbox;
vm.createContext(sandbox);

// Load the real libraries then the renderer, exactly as the page does.
vm.runInContext(read('vendor/marked.min.js'), sandbox, { filename: 'marked.min.js' });
vm.runInContext(read('vendor/highlight.min.js'), sandbox, { filename: 'highlight.min.js' });
vm.runInContext(read('template.js'), sandbox, { filename: 'template.js' });

const rendered = elements['header-container'].innerHTML + '\n' + elements['messages'].innerHTML;

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

console.log('');
if (failures) {
  console.error(failures + ' assertion(s) failed');
  process.exit(1);
}
console.log('all checks passed');
