(async function () {
  'use strict';

  // ============================================================
  // DATA LOADING
  // ============================================================
  //
  // The session is embedded in a <script> island as gzip-compressed,
  // base64-encoded JSON:
  //   { session_id, leaf_id, entries: [ConversationEntry, ...] }
  // Entries keep their on-disk shape except that session environment values
  // are redacted and valid diff details are canonicalized by the Rust exporter
  // before embedding.
  //
  // The load is async (inflate is stream-based) and fails on a browser
  // without DecompressionStream, so we bail with a visible message
  // rather than leave a blank page.
  let data;
  try {
    data = await loadSessionData();
  } catch (e) {
    showLoadError();
    console.error('aj export: could not load the session data', e);
    return;
  }
  const entries = data.entries || [];

  // Inflate and parse the data island. The exporter gzips the JSON and
  // base64-encodes it; the base64 alphabet has no `<`, so the payload is
  // inert in its script element. We reverse both steps here.
  async function loadSessionData() {
    const raw = document.getElementById('session-data').textContent.trim();
    const bytes = Uint8Array.from(atob(raw), (c) => c.charCodeAt(0));
    const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream('gzip'));
    return JSON.parse(await new Response(stream).text());
  }

  function showLoadError() {
    const msg = document.getElementById('messages');
    if (msg) {
      msg.innerHTML = '<div class="error-text">This session export could not be loaded. ' +
        'It needs a browser with gzip DecompressionStream support (2023 or newer).</div>';
    }
  }

  const byId = new Map(entries.map((e) => [e.id, e]));
  // Append position, used as a stable tiebreaker when sibling branches
  // share (or lack) a timestamp.
  const orderIndex = new Map(entries.map((e, i) => [e.id, i]));

  // The active user-thread tip, computed by the exporter. Fall back to
  // the last user-thread entry in append order if it is missing.
  const defaultLeaf = data.leaf_id || deriveLeaf();
  function deriveLeaf() {
    for (let i = entries.length - 1; i >= 0; i--) {
      if (entries[i].thread === 'user') return entries[i].id;
    }
    return entries.length ? entries[entries.length - 1].id : null;
  }

  // Deep-link parameters: open on a specific branch/message when the URL
  // carries them (set by a copy-link button on a prior visit).
  function urlParam(name) {
    try {
      const q = (window.location && window.location.search) || '';
      const m = q.match(new RegExp('[?&]' + name + '=([^&]*)'));
      return m ? decodeURIComponent(m[1]) : null;
    } catch (e) {
      return null;
    }
  }
  const urlLeafId = urlParam('leafId');
  const urlTargetId = urlParam('targetId');

  // Sub-agent runs live on their own `subagent` thread, keyed by
  // `agent_id`. A `sub_agent_spawn` entry roots each run. Its persisted
  // parent is the main-thread head when the spawn event is handled, which
  // can be a sibling tool result appended after the assistant's `agent`
  // call. Walk back to that assistant so the run still nests beneath the
  // turn that spawned it.
  const spawnsByParent = new Map();
  const spawnByAgentId = new Map();
  const subThread = new Map();

  function spawnHostId(spawn) {
    let cur = spawn.parent_id != null ? byId.get(spawn.parent_id) : null;
    const seen = new Set();
    while (cur && cur.thread !== 'subagent' && !seen.has(cur.id)) {
      seen.add(cur.id);
      const m = cur.type === 'message' ? cur.message : null;
      if (m && m.role === 'assistant' && (m.content || []).some((b) => b.type === 'tool_call' && b.name === 'agent')) {
        return cur.id;
      }
      cur = cur.parent_id != null ? byId.get(cur.parent_id) : null;
    }
    return spawn.parent_id;
  }

  for (const e of entries) {
    if (e.type === 'sub_agent_spawn') {
      const hostId = spawnHostId(e);
      if (!spawnsByParent.has(hostId)) spawnsByParent.set(hostId, []);
      spawnsByParent.get(hostId).push(e);
      spawnByAgentId.set(e.agent_id, e);
    }
    if (e.thread === 'subagent' && e.agent_id != null) {
      if (!subThread.has(e.agent_id)) subThread.set(e.agent_id, []);
      subThread.get(e.agent_id).push(e);
    }
  }

  // Tool-result lookup so an assistant's tool call can render its result
  // inline; tool-call lookup so a tree node can name a tool result.
  const resultByCallId = new Map();
  const toolCallById = new Map();
  for (const e of entries) {
    const m = e.type === 'message' ? e.message : null;
    if (!m) continue;
    if (m.role === 'tool_result' && m.tool_call_id) resultByCallId.set(m.tool_call_id, m);
    if (m.role === 'assistant') {
      for (const b of m.content || []) {
        if (b.type === 'tool_call') toolCallById.set(b.id, { name: b.name, arguments: b.arguments, entry_id: e.id });
      }
    }
  }

  function hasInlineRun(toolCallId, result) {
    const agentId = result && result.details && result.details.agent_id;
    const spawn = agentId != null ? spawnByAgentId.get(agentId) : null;
    const call = toolCallId ? toolCallById.get(toolCallId) : null;
    const childEntries = agentId != null ? subThread.get(agentId) : null;
    return !!(spawn && call && spawnHostId(spawn) === call.entry_id && childEntries &&
      childEntries.some((e) => e.type === 'message' && e.message && e.message.role === 'assistant'));
  }

  // ============================================================
  // HELPERS
  // ============================================================

  function escapeHtml(text) {
    return String(text)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;');
  }

  function replaceTabs(text) {
    return text.replace(/\t/g, '    ');
  }

  function normalize(s) {
    return s.replace(/[\n\t]/g, ' ').trim();
  }

  function truncate(s, maxLen) {
    maxLen = maxLen || 100;
    return s.length <= maxLen ? s : s.slice(0, maxLen) + '\u2026';
  }

  function shortenPath(p) {
    if (typeof p !== 'string') return '';
    const m = p.match(/^\/(?:Users|home)\/[^/]+(\/.*)?$/);
    return m ? '~' + (m[1] || '') : p;
  }

  function formatTokens(count) {
    if (count < 1000) return String(count);
    if (count < 10000) return (count / 1000).toFixed(1) + 'k';
    if (count < 1000000) return Math.round(count / 1000) + 'k';
    return (count / 1000000).toFixed(1) + 'M';
  }

  function formatDate(ts) {
    if (!ts) return 'unknown';
    const d = new Date(ts);
    return isNaN(d.getTime()) ? 'unknown' : d.toLocaleString();
  }

  // Restrict markdown link/image URLs to a scheme allow-list so a
  // transcript cannot smuggle a javascript: URL into the shared page.
  function sanitizeUrl(value) {
    const href = String(value || '').trim().replace(/[\u0000-\u001f\u007f]/g, '');
    if (!href) return href;
    const scheme = href.match(/^([A-Za-z][A-Za-z0-9+.-]*):/);
    if (scheme && !/^(https?|mailto|tel|ftp)$/i.test(scheme[1])) return null;
    return href;
  }

  function textOf(content) {
    if (typeof content === 'string') return content;
    if (Array.isArray(content)) {
      return content.filter((c) => c.type === 'text' && c.text).map((c) => c.text).join('');
    }
    return '';
  }

  // ============================================================
  // MARKDOWN
  // ============================================================
  //
  // Configure marked to render authored prose only. Raw HTML in the
  // source is shown as literal text (the html/tag tokenizers return
  // undefined), matching the TUI and keeping a shared transcript from
  // injecting markup. Code blocks and inline code are escaped and shown
  // in a monospace block. We do not syntax-highlight.
  marked.use({
    breaks: true,
    gfm: true,
    tokenizer: {
      html() { return undefined; },
      tag() { return undefined; },
    },
    renderer: {
      link(token) {
        const href = sanitizeUrl(token.href);
        if (href === null) return this.parser.parseInline(token.tokens);
        const title = token.title ? ' title="' + escapeHtml(token.title) + '"' : '';
        return '<a href="' + escapeHtml(href) + '"' + title + '>' + this.parser.parseInline(token.tokens) + '</a>';
      },
      image(token) {
        const href = sanitizeUrl(token.href);
        if (href === null) return escapeHtml(token.text || '');
        const title = token.title ? ' title="' + escapeHtml(token.title) + '"' : '';
        return '<img src="' + escapeHtml(href) + '" alt="' + escapeHtml(token.text || '') + '"' + title + '>';
      },
      code(token) {
        return '<pre><code>' + escapeHtml(token.text) + '</code></pre>';
      },
      codespan(token) {
        return '<code>' + escapeHtml(token.text) + '</code>';
      },
    },
  });

  function md(text) {
    return marked.parse(text == null ? '' : String(text));
  }

  // ============================================================
  // TOOL OUTPUT (expandable preview)
  // ============================================================
  //
  // Long output collapses to a preview, with the full text behind an
  // inline toggle (the global "O" shortcut expands all of them at once).
  // `head` keeps the first lines, `tail` keeps the last lines, matching
  // the TUI's per-variant truncation.
  const TEXT_LINES = 10;
  const BASH_LINES = 5;
  const TOGGLE = "if(window.getSelection().toString())return;this.classList.toggle('expanded')";

  function outputBlock(text, mode, limit, cls) {
    text = replaceTabs(text);
    let lines = text.split('\n');
    if (lines.length && lines[lines.length - 1] === '') lines.pop();
    const clsAttr = cls ? ' ' + cls : '';
    const pre = (s) => '<pre>' + escapeHtml(s) + '</pre>';

    if (lines.length <= limit) {
      return '<div class="tool-output' + clsAttr + '">' + pre(lines.join('\n')) + '</div>';
    }

    const full = '<div class="output-full">' + pre(lines.join('\n')) + '</div>';
    let preview;
    if (mode === 'head') {
      const hidden = lines.length - limit;
      preview = '<div class="output-preview">' + pre(lines.slice(0, limit).join('\n')) +
        '<div class="expand-hint">\u2026 (' + hidden + ' more lines)</div></div>';
    } else {
      const split = lines.length - limit;
      preview = '<div class="output-preview"><div class="expand-hint">\u2026 (' + split +
        ' earlier lines)</div>' + pre(lines.slice(split).join('\n')) + '</div>';
    }
    return '<div class="tool-output expandable' + clsAttr + '" onclick="' + TOGGLE + '">' + preview + full + '</div>';
  }

  // ============================================================
  // DIFF (line-level, LCS with a 3-line context window)
  // ============================================================

  function lcsDiff(a, b) {
    const n = a.length, m = b.length;
    // Guard against pathological inputs: the DP table is O(n*m).
    if (n * m > 4000000) {
      return a.map((l) => ({ tag: 'del', line: l })).concat(b.map((l) => ({ tag: 'add', line: l })));
    }
    const dp = [];
    for (let i = 0; i <= n; i++) dp.push(new Int32Array(m + 1));
    for (let i = n - 1; i >= 0; i--) {
      for (let j = m - 1; j >= 0; j--) {
        dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
      }
    }
    const out = [];
    let i = 0, j = 0;
    while (i < n && j < m) {
      if (a[i] === b[j]) { out.push({ tag: 'eq', line: a[i] }); i++; j++; }
      else if (dp[i + 1][j] >= dp[i][j + 1]) { out.push({ tag: 'del', line: a[i] }); i++; }
      else { out.push({ tag: 'add', line: b[j] }); j++; }
    }
    while (i < n) out.push({ tag: 'del', line: a[i++] });
    while (j < m) out.push({ tag: 'add', line: b[j++] });
    return out;
  }

  function renderStoredDiff(lines) {
    let html = '<div class="tool-diff">';
    for (const line of lines) {
      if (line.startsWith('--- a/') || line.startsWith('+++ b/')) continue;
      const cls = line.startsWith('- ') ? 'diff-removed' :
        line.startsWith('+ ') ? 'diff-added' : 'diff-context';
      html += '<div class="' + cls + '">' + escapeHtml(replaceTabs(line)) + '</div>';
    }
    return html + '</div>';
  }

  function renderDiff(before, after) {
    const ops = lcsDiff(before.split('\n'), after.split('\n'));
    const CONTEXT = 3;
    const keep = ops.map((_, idx) => {
      const lo = Math.max(0, idx - CONTEXT), hi = Math.min(ops.length - 1, idx + CONTEXT);
      for (let k = lo; k <= hi; k++) if (ops[k].tag !== 'eq') return true;
      return false;
    });
    let html = '<div class="tool-diff">';
    let lastKept = -2;
    for (let idx = 0; idx < ops.length; idx++) {
      if (!keep[idx]) continue;
      if (idx > lastKept + 1) html += '<div class="diff-context">\u2026</div>';
      lastKept = idx;
      const op = ops[idx];
      const cls = op.tag === 'del' ? 'diff-removed' : op.tag === 'add' ? 'diff-added' : 'diff-context';
      const sign = op.tag === 'del' ? '-' : op.tag === 'add' ? '+' : ' ';
      html += '<div class="' + cls + '">' + sign + ' ' + escapeHtml(replaceTabs(op.line)) + '</div>';
    }
    return html + '</div>';
  }

  // ============================================================
  // MESSAGE RENDERING
  // ============================================================

  function renderToolResultFallback(result) {
    const text = result ? textOf(result.content) : '';
    return text ? outputBlock(text, 'head', TEXT_LINES) : '';
  }

  function renderToolDetails(name, result) {
    const details = result && result.details;
    if (!details || !details.kind) {
      return renderToolResultFallback(result);
    }
    switch (details.kind) {
      case 'text': {
        let html = '';
        if (details.summary) html += '<div class="summary">' + escapeHtml(details.summary) + '</div>';
        if (typeof details.body === 'string') {
          if (details.body) html += outputBlock(details.body, 'head', TEXT_LINES);
        } else {
          html += renderToolResultFallback(result);
        }
        return html;
      }
      case 'diff': {
        const canonical = details.format === 'display-v1' &&
          typeof details.path === 'string' && Array.isArray(details.lines) &&
          details.lines.every((line) => typeof line === 'string' &&
            !line.includes('\r') && !line.includes('\n') &&
            (line === '--- a/' + details.path ||
              line === '+++ b/' + details.path ||
              line === '\u2026' || line.startsWith('+ ') ||
              line.startsWith('- ') || line.startsWith('  ')));
        if (canonical) {
          return '<div class="summary">' + escapeHtml(details.path) + '</div>' +
            renderStoredDiff(details.lines);
        }
        // Valid legacy details are normalized by the exporter. Keep this path
        // for malformed or unrecognized records that still have usable snapshots.
        if (typeof details.before === 'string' && typeof details.after === 'string') {
          const path = typeof details.path === 'string' ? details.path : name;
          return '<div class="summary">' + escapeHtml(path) + '</div>' +
            renderDiff(details.before, details.after);
        }
        return renderToolResultFallback(result);
      }
      case 'bash': {
        let html = '<div class="tool-command">$ ' + escapeHtml(details.command || '') + '</div>';
        if (details.stdout) html += outputBlock(details.stdout, 'tail', BASH_LINES);
        if (details.stderr) html += outputBlock(details.stderr, 'tail', BASH_LINES, 'stderr');
        if (details.truncated) html += '<div class="summary">[output truncated]</div>';
        if (details.exit_code != null && details.exit_code !== 0) {
          html += '<div class="summary">exit code ' + escapeHtml(String(details.exit_code)) + '</div>';
        }
        return html;
      }
      case 'sub_agent_report':
        // Reached only for a failed run kept by renderToolCall; the
        // success path is shown by the inline sub-agent box.
        return '<div class="summary">' + escapeHtml(details.task || '') + '</div>' + md(details.report || '');
      case 'todos': {
        let html = '<ul class="todos">';
        for (const item of details.items || []) {
          const cls = item.status === 'completed' ? 'done' : item.status === 'in-progress' ? 'doing' : 'todo';
          const mark = item.status === 'completed' ? '[x]' : item.status === 'in-progress' ? '[~]' : '[ ]';
          html += '<li class="' + cls + '">' + mark + ' ' + escapeHtml(item.content) + '</li>';
        }
        return html + '</ul>';
      }
      case 'image': {
        let html = details.summary ? '<div class="summary">' + escapeHtml(details.summary) + '</div>' : '';
        const img = (result.content || []).find((c) => c.type === 'image');
        if (img) html += imageTag(img, 'tool-image');
        return html;
      }
      case 'json': {
        const copy = Object.assign({}, details);
        delete copy.kind;
        return '<pre>' + escapeHtml(JSON.stringify(copy, null, 2)) + '</pre>';
      }
      default:
        return '';
    }
  }

  function imageTag(img, cls) {
    return '<img class="' + cls + '" alt="image" src="data:' + escapeHtml(img.mime_type || 'image/png') +
      ';base64,' + escapeHtml(img.data || '') + '">';
  }

  function renderToolExecution(call, result) {
    const isError = (result && result.is_error) || false;
    const cls = result ? (isError ? 'error' : 'success') : 'pending';
    let html = '<div class="tool-execution ' + cls + '" id="tool-call-' + escapeHtml(call.id) + '">';
    html += '<div class="tool-header"><span class="tool-name">' + escapeHtml(call.name) + '</span></div>';
    html += renderToolDetails(call.name, result);
    return html + '</div>';
  }

  function renderToolCall(call) {
    const result = resultByCallId.get(call.id);
    if (call.name === 'agent') {
      // A persisted spawn renders the successful report in its inline box.
      // Legacy or incomplete logs have only the tool result, so keep that
      // result visible unless a matching run has renderable child content.
      return result && (result.is_error || !hasInlineRun(call.id, result)) ? renderToolExecution(call, result) : '';
    }
    return renderToolExecution(call, result);
  }

  // A small button that copies a deep link to its message. The handler
  // is delegated from #messages (see init).
  function copyLinkButton(id) {
    return '<button class="copy-link-btn" data-entry-id="' + escapeHtml(id) +
      '" title="Copy link to this message" aria-label="Copy link to this message">' +
      '<svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">' +
      '<path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/>' +
      '<path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/></svg></button>';
  }

  function renderAssistant(entry) {
    const msg = entry.message;

    // Build the message body (text, thinking, error) first. Tool calls
    // are not part of the body: they render as sibling blocks after the
    // bubble, matching the TUI where each tool execution is its own block
    // beneath the assistant turn rather than nested inside it.
    let body = '';
    for (const block of msg.content || []) {
      if (block.type === 'text' && block.text && block.text.trim()) {
        body += '<div class="markdown-content">' + md(block.text) + '</div>';
      } else if (block.type === 'thinking' && block.thinking && block.thinking.trim()) {
        body += '<div class="thinking-block">' +
          '<div class="thinking-text" onclick="' + TOGGLE + '">' + escapeHtml(block.thinking) + '</div>' +
          '<div class="thinking-collapsed">Thinking \u2026</div></div>';
      }
    }
    // A failed turn records its cause on the message, not in a block.
    if ((msg.stop_reason === 'Error' || msg.stop_reason === 'Aborted') && msg.error) {
      body += '<div class="error-text">' + escapeHtml(msg.error.category) + ': ' + escapeHtml(msg.error.message) + '</div>';
    }

    // Skip the bubble entirely for a tool-only turn so we don't leave an
    // empty "Assistant" box hanging above its tool executions.
    let html = '';
    if (body) {
      html += '<div class="msg assistant" id="entry-' + escapeHtml(entry.id) + '">' + copyLinkButton(entry.id) +
        '<div class="role">Assistant <span class="model">' + escapeHtml(msg.model || '') + '</span></div>' +
        body + '</div>';
    } else {
      // Tool-only assistant turns have no bubble, but their sidebar row still
      // needs a stable target immediately before the rendered tool blocks.
      html += '<span class="entry-anchor" id="entry-' + escapeHtml(entry.id) + '"></span>';
    }

    for (const block of msg.content || []) {
      if (block.type === 'tool_call') html += renderToolCall(block);
    }

    // Sub-agent runs spawned by this message render inline beneath it.
    for (const spawn of spawnsByParent.get(entry.id) || []) {
      html += renderSubAgent(spawn);
    }
    return html;
  }

  function renderSubAgent(spawn) {
    let html = '<details class="subagent" id="subagent-' + escapeHtml(String(spawn.agent_id)) + '"><summary>' +
      '<span class="sub-head">sub-agent #' + escapeHtml(String(spawn.agent_id)) + '</span> ' +
      '<span class="sub-task">' + escapeHtml(normalize(spawn.task || '')) + '</span></summary>';
    for (const e of subThread.get(spawn.agent_id) || []) {
      html += renderEntrySafe(e);
    }
    return html + '</details>';
  }

  function renderUser(entry) {
    const content = entry.message.content;
    let html = '<div class="msg user" id="entry-' + escapeHtml(entry.id) + '">' + copyLinkButton(entry.id) + '<div class="role">User</div>';
    if (Array.isArray(content)) {
      for (const c of content) {
        if (c.type === 'image') html += imageTag(c, 'message-image');
      }
    }
    const text = textOf(content);
    if (text.trim()) html += '<div class="markdown-content">' + md(text) + '</div>';
    return html + '</div>';
  }

  function renderTaskNotification(entry) {
    const m = entry.message;
    const outcome = m.outcome || {};
    const status = outcome.status || 'succeeded';
    const succeeded = status === 'succeeded';
    const cls = succeeded ? 'task-notification' : 'task-notification err';
    let verb = succeeded ? 'succeeded' : (status === 'killed' ? 'was killed' : 'failed');
    if (status === 'failed' && outcome.code != null) {
      verb += ' (exit ' + escapeHtml(String(outcome.code)) + ')';
    }
    let html = '<div class="' + cls + '" id="entry-' + escapeHtml(entry.id) + '">' +
      copyLinkButton(entry.id) +
      '<div class="task-notification-head">task ' + escapeHtml(normalize(m.label || '')) + ' ' + verb + '</div>';
    if (m.body && m.body.trim()) {
      html += '<pre class="task-notification-body">' + escapeHtml(m.body) + '</pre>';
    }
    return html + '</div>';
  }

  // Environment values are redacted before this renderer receives them and
  // remain unread here. Keys are arbitrary persisted text. Quote delimiters
  // and render every non-ASCII or control code point as inert ASCII text,
  // matching the replay boundary's terminal-safe representation.
  function quoteDisplayText(value) {
    let out = '"';
    for (const ch of String(value)) {
      if (ch === '"') out += '\\"';
      else if (ch === '\'') out += "\\'";
      else if (ch === '\\') out += '\\\\';
      else if (ch === '\0') out += '\\0';
      else if (ch === '\n') out += '\\n';
      else if (ch === '\r') out += '\\r';
      else if (ch === '\t') out += '\\t';
      else {
        const cp = ch.codePointAt(0);
        out += cp < 0x20 || cp > 0x7E
          ? '\\u{' + cp.toString(16) + '}'
          : ch;
      }
    }
    return out + '"';
  }

  function formatEnvKeys(entry) {
    const keys = Object.keys((entry && entry.env) || {}).sort();
    return keys.length ? keys.map(quoteDisplayText).join(', ') : 'none';
  }

  function renderEntry(entry) {
    if (entry.type === 'message') {
      const msg = entry.message;
      if (!msg) return '';
      if (msg.role === 'user') return renderUser(entry);
      if (msg.role === 'assistant') return renderAssistant(entry);
      if (msg.role === 'task_notification') return renderTaskNotification(entry);
      // Tool results are shown inline under their call.
      return '';
    }
    if (entry.type === 'compaction') {
      return '<div class="compaction" id="entry-' + escapeHtml(entry.id) + '"><div class="compaction-head">context compacted</div>' +
        md(entry.summary || '') + '</div>';
    }
    if (entry.type === 'model_change') {
      return '<div class="model-change" id="entry-' + escapeHtml(entry.id) + '">switched model to <span class="model-name">' +
        escapeHtml(entry.provider + '/' + entry.model_id) + '</span></div>';
    }
    if (entry.type === 'thinking_change') {
      return '<div class="notice" id="entry-' + escapeHtml(entry.id) + '">thinking: ' + escapeHtml(entry.level) + '</div>';
    }
    if (entry.type === 'speed_change') {
      return '<div class="notice" id="entry-' + escapeHtml(entry.id) + '">speed: ' + escapeHtml(entry.speed) + '</div>';
    }
    if (entry.type === 'verbosity_change') {
      return '<div class="notice" id="entry-' + escapeHtml(entry.id) + '">verbosity: ' + escapeHtml(entry.verbosity) + '</div>';
    }
    if (entry.type === 'env_change') {
      return '<div class="notice" id="entry-' + escapeHtml(entry.id) + '">session environment keys: ' +
        escapeHtml(formatEnvKeys(entry)) + '</div>';
    }
    return '';
  }

  // Per-entry isolation: a single malformed record renders a placeholder
  // instead of aborting the whole transcript.
  function renderEntrySafe(entry) {
    try {
      return renderEntry(entry);
    } catch (e) {
      return '<div class="error-text">[failed to render entry ' + escapeHtml((entry && entry.id) || '?') + ']</div>';
    }
  }

  // ============================================================
  // HEADER / STATS
  // ============================================================

  // Fold one `usage` object into the running totals. Both spenders come
  // through here: an assistant message, and a compaction checkpoint whose
  // summarizer exchange is never a message entry and so has nowhere else
  // to be counted.
  function foldUsage(s, usage) {
    s.tokens.input += usage.input || 0;
    s.tokens.output += usage.output || 0;
    s.tokens.cacheRead += usage.cache_read || 0;
    s.tokens.cacheWrite += usage.cache_write || 0;
    if (usage.cost) s.cost += usage.cost.total || 0;
    if (usage.incomplete === true) s.incomplete = true;
  }

  function computeStats() {
    const s = {
      user: 0, assistant: 0, toolResults: 0, toolCalls: 0, compactions: 0,
      tokens: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, cost: 0,
      incomplete: false,
      models: new Set(),
    };
    for (const e of entries) {
      if (e.type === 'compaction') {
        s.compactions++;
        // Folded only when present: an entry written before compaction was
        // accounted carries no usage, and folding that as zero would report
        // the spend as free rather than as unknown.
        if (e.usage) foldUsage(s, e.usage);
        else s.incomplete = true;
      }
      if (e.type !== 'message' || !e.message) continue;
      const m = e.message;
      // A task notification carries `role:"task_notification"`, so it is
      // naturally excluded from the user counter and every other stat.
      if (m.role === 'user') s.user++;
      else if (m.role === 'tool_result') s.toolResults++;
      else if (m.role === 'assistant') {
        s.assistant++;
        if (m.model) s.models.add(m.provider ? m.provider + '/' + m.model : m.model);
        if (m.usage) foldUsage(s, m.usage);
        s.toolCalls += (m.content || []).filter((c) => c.type === 'tool_call').length;
      }
    }
    return s;
  }

  function renderHeader() {
    const s = computeStats();
    const tokenParts = [];
    if (s.tokens.input) tokenParts.push('\u2191' + formatTokens(s.tokens.input));
    if (s.tokens.output) tokenParts.push('\u2193' + formatTokens(s.tokens.output));
    if (s.tokens.cacheRead) tokenParts.push('R' + formatTokens(s.tokens.cacheRead));
    if (s.tokens.cacheWrite) tokenParts.push('W' + formatTokens(s.tokens.cacheWrite));

    const msgParts = [];
    if (s.user) msgParts.push(s.user + ' user');
    if (s.assistant) msgParts.push(s.assistant + ' assistant');
    if (s.toolResults) msgParts.push(s.toolResults + ' tool results');
    if (s.compactions) msgParts.push(s.compactions + ' compactions');

    const firstTs = entries.length ? entries[0].timestamp : null;
    let html = '<div class="header"><h1>Session: ' + escapeHtml(data.session_id || 'unknown') + '</h1>' +
      '<div class="help-bar"><span class="help-hint">T toggle thinking \u00b7 O toggle tools</span>' +
      '<div class="help-actions">' +
      '<button type="button" class="header-toggle-btn" data-action="toggle-thinking">Toggle thinking</button>' +
      '<button type="button" class="header-toggle-btn" data-action="toggle-tools">Toggle tools</button>' +
      '<button type="button" class="download-json-btn" data-action="download-json" title="Download session as JSONL">\u2193 JSONL</button>' +
      '</div></div>' +
      '<div class="header-info">' +
      infoItem('Date', formatDate(firstTs)) +
      infoItem('Models', Array.from(s.models).join(', ') || 'unknown') +
      infoItem('Messages', msgParts.join(', ') || '0') +
      infoItem('Tool calls', String(s.toolCalls)) +
      infoItem('Tokens', tokenParts.join(' ') || '0') +
      infoItem('Cost', '$' + s.cost.toFixed(4)) +
      (s.incomplete ? infoItem('Usage', 'partial (recorded usage only)') : '') +
      '</div></div>';

    const sysEntry = entries.find((e) => e.type === 'system_prompt');
    if (sysEntry) {
      html += '<details class="system-prompt" id="entry-' + escapeHtml(sysEntry.id) + '"><summary class="system-prompt-header">System prompt</summary>' +
        '<pre>' + escapeHtml(sysEntry.text) + '</pre></details>';
    }
    return html;
  }

  function infoItem(label, value) {
    return '<span class="info-label">' + escapeHtml(label) + ':</span><span class="info-value">' + escapeHtml(value) + '</span>';
  }

  // ============================================================
  // TREE DATA (pure: flat entries -> nodes and layout)
  // ============================================================
  //
  // The tree shows the conversation, its branches, and (toggleable)
  // sub-agent runs. A sub-agent run hangs off the assistant that spawned
  // it, using the same resolved host as the inline transcript box.
  // Session-state and system entries are kept but hidden by the default
  // filter. The layout (indent, ASCII
  // connectors, gutters) draws each branch point one level deeper, so the
  // conversation's branch structure reads at a glance.

  const treeEntries = entries;

  function treeParentId(entry) {
    return entry.type === 'sub_agent_spawn' ? spawnHostId(entry) : entry.parent_id;
  }

  function buildTree() {
    const nodeMap = new Map();
    const roots = [];
    for (const entry of treeEntries) nodeMap.set(entry.id, { entry, children: [] });
    for (const entry of treeEntries) {
      const node = nodeMap.get(entry.id);
      const parentId = treeParentId(entry);
      const parent = parentId != null && parentId !== entry.id ? nodeMap.get(parentId) : null;
      if (parent) parent.children.push(node);
      else roots.push(node);
    }
    const ts = (e) => { const t = Date.parse(e.timestamp); return isNaN(t) ? 0 : t; };
    const cmp = (a, b) => ts(a.entry) - ts(b.entry) || orderIndex.get(a.entry.id) - orderIndex.get(b.entry.id);
    // Sort siblings iteratively (an explicit stack) rather than
    // recursing on tree depth, so a long linear session cannot overflow
    // the call stack.
    const stack = [...roots];
    while (stack.length) {
      const node = stack.pop();
      node.children.sort(cmp);
      for (const c of node.children) stack.push(c);
    }
    return roots;
  }

  let treeNodeMap = null;
  function findNewestLeaf(nodeId) {
    if (!treeNodeMap) {
      treeNodeMap = new Map();
      const stack = buildTree();
      while (stack.length) {
        const n = stack.pop();
        treeNodeMap.set(n.entry.id, n);
        for (const c of n.children) stack.push(c);
      }
    }
    let cur = treeNodeMap.get(nodeId);
    if (!cur) return nodeId;
    // Follow the newest child to a leaf, but stay on the conversation:
    // never descend into a sub-agent run. Those are reached by clicking
    // the sub-agent node itself, not by walking a conversation branch.
    while (cur.children.length > 0) {
      const onThread = cur.children.filter((c) => c.entry.thread !== 'subagent');
      if (onThread.length === 0) break;
      cur = onThread[onThread.length - 1];
    }
    return cur.entry.id;
  }

  function pathTo(targetId) {
    const path = [];
    const seen = new Set();
    let cur = byId.get(targetId);
    // `seen` guards against a malformed log whose parent_id forms a
    // cycle, which would otherwise hang the viewer.
    while (cur && !seen.has(cur.id)) {
      seen.add(cur.id);
      path.unshift(cur);
      if (!cur.parent_id || cur.parent_id === cur.id) break;
      cur = byId.get(cur.parent_id);
    }
    return path;
  }

  function activePathIds(targetId) {
    return new Set(pathTo(targetId).map((e) => e.id));
  }

  // The conversation entry a sub-agent run belongs to: walk up out of the
  // sub-agent thread to the assistant that spawned it. For a conversation
  // entry this returns the entry itself. `seen` guards a malformed cycle.
  function conversationHost(entryId) {
    let cur = byId.get(entryId);
    const seen = new Set();
    while (cur && cur.thread === 'subagent' && !seen.has(cur.id)) {
      seen.add(cur.id);
      cur = cur.parent_id != null ? byId.get(cur.parent_id) : null;
    }
    return cur ? cur.id : entryId;
  }

  // The conversation leaf to open when navigating to `targetId`. A
  // sub-agent target resolves to its host's branch leaf, so the full
  // conversation stays visible and the run shows in its inline box. A
  // conversation target resolves to its own branch leaf.
  function leafForTarget(targetId) {
    const e = byId.get(targetId);
    if (e && e.thread === 'subagent') return findNewestLeaf(conversationHost(targetId));
    return findNewestLeaf(targetId);
  }

  // Collect every node into a flat list (no layout). Order here is
  // irrelevant: the filter runs over this list, and the visible layout
  // (order, indent, connectors) is computed afterwards in `layoutVisible`.
  // Iterative so a deep linear session can't overflow the call stack.
  function collectNodes(roots) {
    const out = [];
    const stack = [...roots];
    while (stack.length) {
      const node = stack.pop();
      out.push({ node });
      for (const c of node.children) stack.push(c);
    }
    return out;
  }

  function buildTreePrefix(flatNode) {
    const { indent, showConnector, isLast, gutters, isVirtualRootChild, multipleRoots } = flatNode;
    // With multiple roots we add one synthetic indent level for the
    // virtual root, so subtract it back out for display. The connector
    // (the corner glyph) sits one level shallower than the node itself.
    const displayIndent = multipleRoots ? Math.max(0, indent - 1) : indent;
    const connector = showConnector && !isVirtualRootChild ? (isLast ? '\u2514\u2500 ' : '\u251c\u2500 ') : '';
    const connectorPosition = connector ? displayIndent - 1 : -1;
    const totalChars = displayIndent * 3;
    const chars = [];
    for (let i = 0; i < totalChars; i++) {
      const level = Math.floor(i / 3);
      const posInLevel = i % 3;
      const gutter = gutters.find((g) => g.position === level);
      if (gutter) {
        chars.push(posInLevel === 0 ? (gutter.show ? '\u2502' : ' ') : ' ');
      } else if (connector && level === connectorPosition) {
        if (posInLevel === 0) chars.push(isLast ? '\u2514' : '\u251c');
        else if (posInLevel === 1) chars.push('\u2500');
        else chars.push(' ');
      } else {
        chars.push(' ');
      }
    }
    return chars.join('');
  }

  // Lay out the visible nodes as a branch tree and return them in display
  // order with indent / connector / gutter fields set.
  //
  // The conversation runs down a spine at the base indent. Two things
  // hang one level in off a spine node and draw a connector: a sub-agent
  // run (its `sub_agent_spawn`, rendered before the conversation
  // continues) and a genuine conversation fork (an edited/retried prompt,
  // i.e. more than one conversation child). A lone continuation stays on
  // the spine, so spawning a sub-agent does not indent the rest of the
  // main thread. Each node reattaches to its nearest visible ancestor, so
  // a filter that hides intermediate entries doesn't break the structure.
  //
  // `justBranched` marks a node its parent hung in with a connector, so
  // the node's own subtree indents one further to stay grouped under it.
  // `gutters` are the vertical bars to continue for unfinished ancestor
  // branches, and `isVirtualRootChild` suppresses a connector for the
  // synthetic level added when there is more than one visible root.
  function layoutVisible(visible, allFlat, activeIds) {
    if (visible.length === 0) return [];
    const flatById = new Map(allFlat.map((f) => [f.node.entry.id, f]));
    const visById = new Map(visible.map((f) => [f.node.entry.id, f]));
    const visibleIds = new Set(visById.keys());

    // Nearest ancestor that survived the filter (null if none). `seen`
    // guards a malformed parent_id cycle (pathTo guards the same).
    function visibleAncestor(id) {
      const seen = new Set();
      let item = flatById.get(id);
      let pid = item && treeParentId(item.node.entry);
      while (pid != null && !seen.has(pid)) {
        seen.add(pid);
        if (visibleIds.has(pid)) return pid;
        item = flatById.get(pid);
        pid = item && treeParentId(item.node.entry);
      }
      return null;
    }

    const childrenOf = new Map([[null, []]]);
    for (const f of visible) {
      const id = f.node.entry.id;
      const anc = visibleAncestor(id);
      if (!childrenOf.has(anc)) childrenOf.set(anc, []);
      childrenOf.get(anc).push(id);
    }
    const isSpawn = (id) => visById.get(id).node.entry.type === 'sub_agent_spawn';

    // Which subtrees contain the active leaf (post-order, iterative), so a
    // forked conversation shows its active branch first.
    const containsActive = new Map();
    const order = [];
    const markStack = [...childrenOf.get(null)];
    while (markStack.length) {
      const id = markStack.pop();
      order.push(id);
      for (const c of childrenOf.get(id) || []) markStack.push(c);
    }
    for (let i = order.length - 1; i >= 0; i--) {
      const id = order[i];
      let has = activeIds.has(id);
      for (const c of childrenOf.get(id) || []) if (containsActive.get(c)) has = true;
      containsActive.set(id, has);
    }

    // A node's children in display order: sub-agent runs first (in spawn
    // order), then the conversation (active branch first). `orderIndex`
    // breaks ties by append position so the order is stable.
    function split(ids) {
      const oi = (id) => orderIndex.get(id) || 0;
      const spawns = ids.filter(isSpawn).sort((a, b) => oi(a) - oi(b));
      const lines = ids
        .filter((id) => !isSpawn(id))
        .sort((a, b) => Number(containsActive.get(b)) - Number(containsActive.get(a)) || oi(a) - oi(b));
      return { spawns, lines };
    }

    const rootIds = childrenOf.get(null);
    const multipleRoots = rootIds.length > 1;
    const result = [];

    // Stack frame: [id, indent, justBranched, showConnector, isLast,
    // gutters, isVirtualRootChild].
    const stack = [];
    {
      const { spawns, lines } = split(rootIds);
      const ordered = [...spawns, ...lines];
      for (let i = ordered.length - 1; i >= 0; i--) {
        const isLast = i === ordered.length - 1;
        stack.push([ordered[i], multipleRoots ? 1 : 0, multipleRoots, multipleRoots, isLast, [], multipleRoots]);
      }
    }
    while (stack.length > 0) {
      const [id, indent, justBranched, showConnector, isLast, gutters, isVirtualRootChild] = stack.pop();
      const f = visById.get(id);
      f.indent = indent;
      f.showConnector = showConnector;
      f.isLast = isLast;
      f.gutters = gutters;
      f.isVirtualRootChild = isVirtualRootChild;
      f.multipleRoots = multipleRoots;
      result.push(f);

      const { spawns, lines } = split(childrenOf.get(id) || []);
      const branched = lines.length > 1;
      // Children that hang one level in and draw a connector: every
      // sub-agent run, plus the conversation children when the thread
      // forks. A lone continuation stays on the spine.
      const connectorKids = branched ? [...spawns, ...lines] : spawns;
      const connectorSet = new Set(connectorKids);
      const lastConnectorId = connectorKids.length ? connectorKids[connectorKids.length - 1] : null;
      const ordered = [...spawns, ...lines];

      // A lone conversation continuation that stays on this node's spine
      // (not bumped one level in by `justBranched`) and follows one or
      // more sub-agent runs. When present, the runs' connector column
      // threads down to it: the last run draws `├─` (not `└─`) and its
      // body keeps the `│`, so the spine reads as one line from the
      // spawning message, past the runs, to the continuation, whose
      // marker sits on the same column. When the continuation is bumped
      // (inside a fork) the columns no longer line up, so we don't extend.
      const continuation = !branched && lines.length === 1 ? lines[0] : null;
      const continuationOnSpine = continuation != null && !(justBranched && indent > 0);

      const connectorDisplayed = showConnector && !isVirtualRootChild;
      const currentDisplayIndent = multipleRoots ? Math.max(0, indent - 1) : indent;
      const connectorPosition = Math.max(0, currentDisplayIndent - 1);
      const childGutters = connectorDisplayed ? [...gutters, { position: connectorPosition, show: !isLast }] : gutters;

      for (let i = ordered.length - 1; i >= 0; i--) {
        const childId = ordered[i];
        let childIndent, childShowConnector, childJustBranched, childIsLast;
        if (connectorSet.has(childId)) {
          childIndent = indent + 1;
          childShowConnector = true;
          childJustBranched = true;
          childIsLast = childId === lastConnectorId && !continuationOnSpine;
        } else {
          // Spine continuation: no connector. It still indents one more
          // when it directly follows a hang-in (so a fork or run body
          // stays grouped), otherwise it stays on the spine.
          childIndent = justBranched && indent > 0 ? indent + 1 : indent;
          childShowConnector = false;
          childJustBranched = false;
          childIsLast = true;
        }
        stack.push([childId, childIndent, childJustBranched, childShowConnector, childIsLast, childGutters, false]);
      }
    }
    return result;
  }

  // ============================================================
  // TREE DISPLAY TEXT
  // ============================================================

  // Build a short tree label for a tool call. The result is plain text
  // and MUST be escaped before it reaches innerHTML (the sole caller in
  // `treeNodeHtml` does this).
  function formatToolCall(name, args) {
    args = args || {};
    const path = (p) => shortenPath(String(p || ''));
    switch (name) {
      case 'read_file': case 'read': return '[read: ' + path(args.path || args.file_path) + ']';
      case 'write_file': case 'write': return '[write: ' + path(args.path || args.file_path) + ']';
      case 'edit_file': case 'edit': return '[edit: ' + path(args.path || args.file_path) + ']';
      case 'bash': return '[bash: ' + truncate(normalize(String(args.command || '')), 50) + ']';
      case 'agent': return '[agent: ' + truncate(normalize(String(args.task || '')), 50) + ']';
      default: return '[' + name + ': ' + truncate(JSON.stringify(args), 40) + ']';
    }
  }

  function role(cls, label) { return '<span class="' + cls + '">' + label + '</span>'; }
  function treeMuted(s) { return '<span class="tree-muted">' + s + '</span>'; }

  function treeNodeHtml(entry) {
    switch (entry.type) {
      case 'message': {
        const m = entry.message;
        if (!m) return treeMuted('[message]');
        if (m.role === 'user') {
          return role('tree-role-user', 'user:') + ' ' + escapeHtml(truncate(normalize(textOf(m.content))));
        }
        if (m.role === 'assistant') {
          const t = normalize(textOf(m.content));
          if (t) return role('tree-role-assistant', 'assistant:') + ' ' + escapeHtml(truncate(t));
          if (m.stop_reason === 'Aborted') return role('tree-role-assistant', 'assistant:') + ' ' + treeMuted('(aborted)');
          if (m.error) return role('tree-role-assistant', 'assistant:') + ' <span class="tree-error">' + escapeHtml(truncate(m.error.message || '')) + '</span>';
          return role('tree-role-assistant', 'assistant:') + ' ' + treeMuted('(tool calls)');
        }
        if (m.role === 'tool_result') {
          const call = m.tool_call_id ? toolCallById.get(m.tool_call_id) : null;
          const label = call ? formatToolCall(call.name, call.arguments) : '[' + (m.tool_name || 'tool') + ']';
          return '<span class="tree-role-tool">' + escapeHtml(label) + '</span>';
        }
        return treeMuted('[' + escapeHtml(m.role) + ']');
      }
      case 'sub_agent_spawn':
        return '<span class="tree-sub-label">\u21b3 sub-agent #' + escapeHtml(String(entry.agent_id)) + '</span> ' +
          escapeHtml(truncate(normalize(entry.task || '')));
      case 'compaction': return '<span class="tree-compaction">[compaction]</span>';
      case 'system_prompt': return treeMuted('[system prompt]');
      case 'model_change': return treeMuted('[model: ' + escapeHtml(entry.model_id) + ']');
      case 'thinking_change': return treeMuted('[thinking: ' + escapeHtml(entry.level) + ']');
      case 'speed_change': return treeMuted('[speed: ' + escapeHtml(entry.speed) + ']');
      case 'verbosity_change': return treeMuted('[verbosity: ' + escapeHtml(entry.verbosity) + ']');
      case 'env_change': return treeMuted('[environment: ' + escapeHtml(formatEnvKeys(entry)) + ']');
      default: return treeMuted('[' + escapeHtml(entry.type) + ']');
    }
  }

  function searchableText(entry) {
    const parts = [entry.type];
    if (entry.type === 'message' && entry.message) {
      const m = entry.message;
      parts.push(m.role, textOf(m.content));
      if (m.role === 'tool_result') {
        parts.push(m.tool_name || '');
        const call = m.tool_call_id ? toolCallById.get(m.tool_call_id) : null;
        if (call) parts.push(call.name, JSON.stringify(call.arguments || {}));
      }
    } else if (entry.type === 'sub_agent_spawn') {
      parts.push('sub-agent', entry.task || '');
    } else if (entry.type === 'compaction') {
      parts.push(entry.summary || '');
    } else if (entry.type === 'model_change') {
      parts.push(entry.model_id || '');
    } else if (entry.type === 'env_change') {
      parts.push(...Object.keys(entry.env || {}));
    }
    return parts.join(' ').toLowerCase();
  }

  // ============================================================
  // FILTERING
  // ============================================================

  let searchQuery = '';
  const DEFAULT_FILTERS = { user: true, assistant: true, tools: false, subagents: false, state: false };
  const entryFilters = Object.assign({}, DEFAULT_FILTERS);
  const STATE_TYPES = new Set([
    'system_prompt', 'model_change', 'thinking_change', 'speed_change',
    'verbosity_change', 'env_change', 'compaction',
  ]);

  function entryCategory(entry) {
    if (entry.thread === 'subagent') return 'subagents';
    if (STATE_TYPES.has(entry.type)) return 'state';
    if (entry.type !== 'message') return null;
    if (!entry.message) return null;
    if (entry.message.role === 'user') return 'user';
    if (entry.message.role === 'assistant') return 'assistant';
    if (entry.message.role === 'tool_result' || entry.message.role === 'task_notification') return 'tools';
    return null;
  }

  function structurallyVisible(entry) {
    return entryCategory(entry) != null;
  }

  // Returns the laid-out visible nodes plus the stable number of categorized
  // entries. Toggles and search narrow only the numerator.
  function filterNodes(flatNodes, activeIds) {
    const tokens = searchQuery.toLowerCase().split(/\s+/).filter(Boolean);
    const structural = flatNodes.filter((flat) => structurallyVisible(flat.node.entry));
    const visible = structural.filter((flat) => {
      const entry = flat.node.entry;
      const category = entryCategory(entry);
      if (!entryFilters[category]) return false;
      if (tokens.length > 0) {
        const text = searchableText(entry);
        if (!tokens.every((t) => text.includes(t))) return false;
      }
      return true;
    });
    return { visible: layoutVisible(visible, flatNodes, activeIds), total: structural.length };
  }

  // ============================================================
  // TREE RENDER
  // ============================================================

  let currentLeafId = defaultLeaf;
  let currentTargetId = defaultLeaf;

  // Rebuild the whole node list every time. The visible set, sibling
  // order, and connector glyphs all depend on the active leaf, so an
  // in-place marker update would leave the tree out of sync after a
  // navigation. The trees are small enough that a full rebuild is cheap.
  function renderTree() {
    const roots = buildTree();
    const activeIds = activePathIds(currentLeafId);
    const flatNodes = collectNodes(roots);
    const { visible: filtered, total } = filterNodes(flatNodes, activeIds);
    const container = document.getElementById('tree-container');

    container.innerHTML = '';
    for (const flat of filtered) {
      const entry = flat.node.entry;
      const div = document.createElement('div');
      div.className = 'tree-node';
      if (activeIds.has(entry.id)) div.classList.add('in-path');
      if (entry.id === currentTargetId) div.classList.add('active');
      if (entry.thread === 'subagent') div.classList.add('tree-subagent');
      div.dataset.id = entry.id;

      const prefix = document.createElement('span');
      prefix.className = 'tree-prefix';
      prefix.textContent = buildTreePrefix(flat);
      const marker = document.createElement('span');
      marker.className = 'tree-marker';
      marker.textContent = activeIds.has(entry.id) ? '\u2022 ' : '  ';
      const content = document.createElement('span');
      content.className = 'tree-content';
      content.innerHTML = treeNodeHtml(entry);

      div.append(prefix, marker, content);
      div.addEventListener('click', () => {
        if (window.getSelection().toString()) return;
        // Open the entry's conversation branch and scroll to it. A
        // sub-agent row routes to its host branch (its run shows inline
        // there), so the full conversation stays visible.
        navigateTo(leafForTarget(entry.id), 'target', entry.id);
      });
      container.appendChild(div);
    }

    document.getElementById('tree-status').textContent = filtered.length + ' / ' + total + ' entries';
    setTimeout(() => {
      const active = container.querySelector('.tree-node.active');
      if (active) active.scrollIntoView({ block: 'nearest' });
    }, 0);
  }

  // ============================================================
  // NAVIGATION
  // ============================================================

  // Tool results render inside their assistant tool-call block, so route
  // a scroll there. A successful `agent` result has no tool block (the
  // sub-agent box shows it instead), so route those to the box. Anything
  // else scrolls to its own element.
  function scrollTargetId(entryId) {
    const entry = byId.get(entryId);
    if (entry && entry.type === 'message' && entry.message && entry.message.role === 'tool_result') {
      const det = entry.message.details;
      if (det && det.kind === 'sub_agent_report' && hasInlineRun(entry.message.tool_call_id, entry.message)) {
        return 'subagent-' + det.agent_id;
      }
      if (entry.message.tool_call_id) return 'tool-call-' + entry.message.tool_call_id;
    }
    // The spawn node has no element of its own; scroll to its run's box.
    if (entry && entry.type === 'sub_agent_spawn' && entry.agent_id != null) {
      return 'subagent-' + entry.agent_id;
    }
    return 'entry-' + entryId;
  }

  function navigateTo(leafId, scrollMode, scrollToEntryId) {
    currentLeafId = leafId;
    currentTargetId = scrollToEntryId || leafId;

    try {
      document.getElementById('header-container').innerHTML = renderHeader();
    } catch (e) {
      document.getElementById('header-container').innerHTML = '<div class="error-text">[failed to render header]</div>';
    }
    attachHeaderHandlers();

    const messages = document.getElementById('messages');
    let html = '';
    // Sub-agent entries are not rendered in the main flow; they appear in
    // the collapsible box under the assistant that spawned them.
    for (const entry of pathTo(leafId)) {
      if (entry.thread !== 'subagent') html += renderEntrySafe(entry);
    }
    messages.innerHTML = html;

    // Render the tree last and in isolation: it already rendered the
    // transcript, so a tree failure leaves the content intact.
    try {
      renderTree();
    } catch (e) {
      document.getElementById('tree-status').textContent = 'tree failed to render';
    }

    // Close the mobile sidebar after picking a node.
    closeSidebar();

    setTimeout(() => {
      if (scrollMode === 'target' && scrollToEntryId) {
        // A sub-agent target lives inside a collapsed box (possibly
        // nested), so open it and any ancestor sub-agent boxes first.
        let cur = byId.get(scrollToEntryId);
        const opened = new Set();
        while (cur && cur.thread === 'subagent' && !opened.has(cur.id)) {
          opened.add(cur.id);
          if (cur.agent_id != null) {
            const box = document.getElementById('subagent-' + cur.agent_id);
            if (box) box.open = true;
          }
          cur = cur.parent_id != null ? byId.get(cur.parent_id) : null;
        }
        const el = document.getElementById(scrollTargetId(scrollToEntryId)) || document.getElementById('entry-' + scrollToEntryId);
        if (el) {
          el.scrollIntoView({ block: 'start' });
          el.classList.add('highlight');
          setTimeout(() => el.classList.remove('highlight'), 2000);
        }
      }
    }, 0);
  }

  // ============================================================
  // INITIALIZATION
  // ============================================================

  let thinkingExpanded = true;
  let toolsExpanded = false;

  function toggleThinking() {
    thinkingExpanded = !thinkingExpanded;
    document.querySelectorAll('.thinking-text').forEach((el) => { el.style.display = thinkingExpanded ? '' : 'none'; });
    document.querySelectorAll('.thinking-collapsed').forEach((el) => { el.style.display = thinkingExpanded ? 'none' : 'block'; });
  }

  function toggleTools() {
    toolsExpanded = !toolsExpanded;
    document.querySelectorAll('.tool-output.expandable').forEach((el) => { el.classList.toggle('expanded', toolsExpanded); });
  }

  function attachHeaderHandlers() {
    const t = document.querySelector('[data-action="toggle-thinking"]');
    const o = document.querySelector('[data-action="toggle-tools"]');
    const d = document.querySelector('[data-action="download-json"]');
    if (t) t.addEventListener('click', toggleThinking);
    if (o) o.addEventListener('click', toggleTools);
    if (d) d.addEventListener('click', downloadSessionJson);
  }

  // ----- share / download -----

  // A deep link to one message: the current branch leaf plus the target
  // entry, as query params this page reads on load.
  //
  // NOTE: for a `file://` export the base is the absolute local path, so
  // the link only resolves on the same machine. It comes into its own
  // for a hosted copy, where the same params address the same message.
  function buildShareUrl(entryId) {
    try {
      const base = ((window.location && window.location.href) || '').split('?')[0];
      return base + '?leafId=' + encodeURIComponent(currentLeafId) + '&targetId=' + encodeURIComponent(entryId);
    } catch (e) {
      return '';
    }
  }

  async function copyToClipboard(text, button) {
    let ok = false;
    try {
      if (navigator.clipboard && navigator.clipboard.writeText) {
        await navigator.clipboard.writeText(text);
        ok = true;
      }
    } catch (e) { /* fall through to the execCommand path */ }
    if (!ok) {
      try {
        const ta = document.createElement('textarea');
        ta.value = text;
        ta.style.position = 'fixed';
        ta.style.opacity = '0';
        document.body.appendChild(ta);
        ta.select();
        ok = document.execCommand('copy');
        document.body.removeChild(ta);
      } catch (e) { /* clipboard unavailable */ }
    }
    if (ok && button) {
      const original = button.innerHTML;
      button.innerHTML = '\u2713';
      button.classList.add('copied');
      setTimeout(() => { button.innerHTML = original; button.classList.remove('copied'); }, 1500);
    }
  }

  // Reconstruct the session as JSONL (one entry per line). There is no
  // header line in the aj format, so the entries alone round-trip.
  function downloadSessionJson() {
    const jsonl = entries.map((e) => JSON.stringify(e)).join('\n');
    const blob = new Blob([jsonl], { type: 'application/x-ndjson' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = (data.session_id || 'session') + '.jsonl';
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
    URL.revokeObjectURL(url);
  }

  const sidebar = document.getElementById('sidebar');
  const overlay = document.getElementById('sidebar-overlay');
  const hamburger = document.getElementById('hamburger');
  function openSidebar() { sidebar.classList.add('open'); overlay.classList.add('open'); }
  function closeSidebar() { sidebar.classList.remove('open'); overlay.classList.remove('open'); }
  hamburger.addEventListener('click', openSidebar);
  overlay.addEventListener('click', closeSidebar);
  document.getElementById('sidebar-close').addEventListener('click', closeSidebar);

  const searchInput = document.getElementById('tree-search');
  searchInput.addEventListener('input', (e) => { searchQuery = e.target.value; renderTree(); });

  // Copy-link buttons are delegated from #messages, which survives the
  // innerHTML rewrites that each navigation does.
  document.getElementById('messages').addEventListener('click', (e) => {
    const btn = e.target.closest && e.target.closest('.copy-link-btn');
    if (!btn) return;
    e.stopPropagation();
    copyToClipboard(buildShareUrl(btn.dataset.entryId), btn);
  });

  // Drag the divider to resize the sidebar, with the width persisted per
  // browser. No-ops where the resizer is absent (e.g. a non-DOM test
  // harness).
  function setupSidebarResize() {
    const resizer = document.getElementById('sidebar-resizer');
    if (!resizer) return;
    const KEY = 'aj-export:sidebar-width';
    const root = document.documentElement;
    const isMobile = () => window.matchMedia && window.matchMedia('(max-width: 900px)').matches;
    const clamp = (w) => Math.max(220, Math.min(640, w));
    const apply = (w) => root.style.setProperty('--sidebar-width', Math.round(clamp(w)) + 'px');
    const save = (w) => { try { localStorage.setItem(KEY, String(Math.round(clamp(w)))); } catch (e) {} };
    try { const saved = Number(localStorage.getItem(KEY)); if (saved) apply(saved); } catch (e) {}

    resizer.addEventListener('pointerdown', (e) => {
      if (isMobile()) return;
      e.preventDefault();
      const startX = e.clientX;
      const startW = sidebar.getBoundingClientRect().width;
      document.body.classList.add('sidebar-resizing');
      const onMove = (ev) => apply(startW + (ev.clientX - startX));
      const onUp = () => {
        document.body.classList.remove('sidebar-resizing');
        window.removeEventListener('pointermove', onMove);
        window.removeEventListener('pointerup', onUp);
        window.removeEventListener('pointercancel', onUp);
        save(sidebar.getBoundingClientRect().width);
      };
      window.addEventListener('pointermove', onMove);
      window.addEventListener('pointerup', onUp);
      window.addEventListener('pointercancel', onUp);
    });
    resizer.addEventListener('dblclick', () => { if (!isMobile()) { apply(340); save(340); } });
  }

  // Click any image to view it full-size. Click the backdrop or press
  // Escape to close.
  function setupImageModal() {
    const modal = document.getElementById('image-modal');
    const modalImg = document.getElementById('modal-image');
    if (!modal || !modalImg) return;
    const close = () => modal.classList.remove('open');
    document.getElementById('content').addEventListener('click', (e) => {
      const img = e.target.closest && e.target.closest('.message-image, .tool-image');
      if (!img) return;
      modalImg.src = img.src;
      modal.classList.add('open');
    });
    modal.addEventListener('click', close);
    document.addEventListener('keydown', (e) => { if (e.key === 'Escape') close(); });
  }

  const defaultFilterButton = document.getElementById('filter-default');
  const entryFilterButtons = Array.from(document.querySelectorAll('[data-entry-filter]'));

  function filtersAreDefault() {
    return Object.keys(DEFAULT_FILTERS).every((key) => entryFilters[key] === DEFAULT_FILTERS[key]);
  }

  function updateFilterButtons() {
    const isDefault = filtersAreDefault();
    defaultFilterButton.classList.toggle('active', isDefault);
    defaultFilterButton.setAttribute('aria-pressed', String(isDefault));
    for (const button of entryFilterButtons) {
      const active = entryFilters[button.dataset.entryFilter];
      button.classList.toggle('active', active);
      button.setAttribute('aria-pressed', String(active));
    }
  }

  function applyFilterChange() {
    updateFilterButtons();
    // A hand-crafted deep link can use a sub-agent entry as its leaf. Re-home
    // it when that category is hidden so the active branch remains coherent.
    const leaf = byId.get(currentLeafId);
    if (!entryFilters.subagents && leaf && leaf.thread === 'subagent') {
      navigateTo(leafForTarget(currentLeafId), 'none');
      return;
    }
    renderTree();
  }

  defaultFilterButton.addEventListener('click', () => {
    Object.assign(entryFilters, DEFAULT_FILTERS);
    applyFilterChange();
  });
  for (const button of entryFilterButtons) {
    button.addEventListener('click', () => {
      const category = button.dataset.entryFilter;
      entryFilters[category] = !entryFilters[category];
      applyFilterChange();
    });
  }
  updateFilterButtons();

  function isEditable(el) {
    if (!el) return false;
    const tag = el.tagName;
    return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || tag === 'BUTTON';
  }

  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') { searchInput.value = ''; searchQuery = ''; renderTree(); }
    if (isEditable(document.activeElement)) return;
    const key = e.key.toLowerCase();
    if (key === 't') { e.preventDefault(); toggleThinking(); }
    else if (key === 'o') { e.preventDefault(); toggleTools(); }
  });

  setupSidebarResize();
  setupImageModal();

  // Open on the deep-linked message when present. If the URL's leaf and
  // target disagree (a hand-edited or cross-branch link), fall back to
  // the target's own branch so the scroll target always exists.
  if (urlTargetId && byId.has(urlTargetId)) {
    const onLeaf = urlLeafId && byId.has(urlLeafId) && activePathIds(urlLeafId).has(urlTargetId);
    navigateTo(onLeaf ? urlLeafId : leafForTarget(urlTargetId), 'target', urlTargetId);
  } else if (defaultLeaf) {
    navigateTo(urlLeafId && byId.has(urlLeafId) ? urlLeafId : defaultLeaf, 'none');
  }
})().catch((e) => console.error('aj export: renderer error', e));
