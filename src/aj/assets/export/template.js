(function () {
  'use strict';

  // ============================================================
  // DATA LOADING
  // ============================================================
  //
  // The session is embedded as a JSON object in a <script> island:
  //   { session_id, leaf_id, entries: [ConversationEntry, ...] }
  // Entries are the verbatim on-disk records (snake_case fields, the
  // `type`/`role`/`kind` tags exactly as serialized by aj-session).
  const data = JSON.parse(document.getElementById('session-data').textContent);
  const entries = data.entries || [];
  const byId = new Map(entries.map((e) => [e.id, e]));

  // The active user-thread tip, computed by the exporter. Fall back to
  // the last user-thread entry in append order if it is missing.
  const leafId = data.leaf_id || deriveLeaf();
  function deriveLeaf() {
    for (let i = entries.length - 1; i >= 0; i--) {
      if (entries[i].thread === 'user') return entries[i].id;
    }
    return entries.length ? entries[entries.length - 1].id : null;
  }

  // Sub-agent runs live on their own `subagent` thread, keyed by
  // `agent_id`. A `sub_agent_spawn` entry roots each run and is parented
  // at the assistant message that spawned it, so we index spawns by that
  // parent to nest the run inline under that message.
  const spawnsByParent = new Map();
  const subThread = new Map();
  for (const e of entries) {
    if (e.type === 'sub_agent_spawn') {
      if (!spawnsByParent.has(e.parent_id)) spawnsByParent.set(e.parent_id, []);
      spawnsByParent.get(e.parent_id).push(e);
    }
    if (e.thread === 'subagent' && e.agent_id != null) {
      if (!subThread.has(e.agent_id)) subThread.set(e.agent_id, []);
      subThread.get(e.agent_id).push(e);
    }
  }

  // Tool-result lookup so an assistant's tool call can render its result
  // inline (the standalone tool_result entry renders nothing).
  const resultByCallId = new Map();
  for (const e of entries) {
    const m = e.type === 'message' ? e.message : null;
    if (m && m.role === 'tool_result' && m.tool_call_id) {
      resultByCallId.set(m.tool_call_id, m);
    }
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

  // Coerce to a string for display, or null when the value is the wrong
  // type (a malformed tool argument). Callers show an "[invalid arg]".
  function str(value) {
    if (typeof value === 'string') return value;
    if (value == null) return '';
    return null;
  }

  function replaceTabs(text) {
    return text.replace(/\t/g, '    ');
  }

  function truncate(s, maxLen) {
    maxLen = maxLen || 100;
    return s.length <= maxLen ? s : s.slice(0, maxLen) + '\u2026';
  }

  function normalize(s) {
    return s.replace(/[\n\t]/g, ' ').trim();
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

  function getLanguageFromPath(filePath) {
    const ext = (filePath.split('.').pop() || '').toLowerCase();
    const map = {
      ts: 'typescript', tsx: 'typescript', js: 'javascript', jsx: 'javascript',
      py: 'python', rb: 'ruby', rs: 'rust', go: 'go', java: 'java',
      c: 'c', cpp: 'cpp', h: 'c', hpp: 'cpp', cs: 'csharp', php: 'php',
      sh: 'bash', bash: 'bash', zsh: 'bash', sql: 'sql', html: 'html',
      css: 'css', scss: 'scss', json: 'json', yaml: 'yaml', yml: 'yaml',
      xml: 'xml', md: 'markdown', toml: 'ini', dockerfile: 'dockerfile',
    };
    return map[ext];
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
  // injecting markup. Code blocks are syntax-highlighted; inline code is
  // escaped.
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
        return '<pre><code class="hljs">' + highlight(token.text, token.lang) + '</code></pre>';
      },
      codespan(token) {
        return '<code>' + escapeHtml(token.text) + '</code>';
      },
    },
  });

  function highlight(code, lang) {
    try {
      if (lang && hljs.getLanguage(lang)) return hljs.highlight(code, { language: lang }).value;
      return hljs.highlightAuto(code).value;
    } catch (e) {
      return escapeHtml(code);
    }
  }

  function md(text) {
    return marked.parse(text || '');
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

  function renderToolDetails(name, result) {
    const details = result && result.details;
    if (!details || !details.kind) {
      // No structured details (older logs): fall back to the result's
      // model-facing text.
      const text = result ? textOf(result.content) : '';
      return text ? outputBlock(text, 'head', TEXT_LINES) : '';
    }
    switch (details.kind) {
      case 'text': {
        let html = '';
        if (details.summary) html += '<div class="summary">' + escapeHtml(details.summary) + '</div>';
        if (details.body) html += outputBlock(details.body, 'head', TEXT_LINES);
        return html;
      }
      case 'diff':
        return '<div class="summary">' + escapeHtml(details.path) + '</div>' +
          renderDiff(details.before || '', details.after || '');
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
      // The successful report is shown by the inline sub-agent box; only
      // surface a genuine failure here so it is not lost.
      return result && result.is_error ? renderToolExecution(call, result) : '';
    }
    return renderToolExecution(call, result);
  }

  function renderAssistant(entry) {
    const msg = entry.message;
    let html = '<div class="msg assistant" id="entry-' + escapeHtml(entry.id) + '">';
    html += '<div class="role">Assistant <span class="model">' + escapeHtml(msg.model || '') + '</span></div>';

    for (const block of msg.content || []) {
      if (block.type === 'text' && block.text && block.text.trim()) {
        html += '<div class="markdown-content">' + md(block.text) + '</div>';
      } else if (block.type === 'thinking' && block.thinking && block.thinking.trim()) {
        html += '<div class="thinking-block">' +
          '<div class="thinking-text" onclick="' + TOGGLE + '">' + escapeHtml(block.thinking) + '</div>' +
          '<div class="thinking-collapsed">Thinking \u2026</div></div>';
      }
    }
    for (const block of msg.content || []) {
      if (block.type === 'tool_call') html += renderToolCall(block);
    }

    // A failed turn records its cause on the message, not in a block.
    if ((msg.stop_reason === 'Error' || msg.stop_reason === 'Aborted') && msg.error) {
      html += '<div class="error-text">' + escapeHtml(msg.error.category) + ': ' + escapeHtml(msg.error.message) + '</div>';
    }

    html += '</div>';

    // Sub-agent runs spawned by this message render inline beneath it.
    for (const spawn of spawnsByParent.get(entry.id) || []) {
      html += renderSubAgent(spawn);
    }
    return html;
  }

  function renderSubAgent(spawn) {
    let html = '<details class="subagent"><summary>' +
      '<span class="sub-head">sub-agent #' + escapeHtml(String(spawn.agent_id)) + '</span> ' +
      '<span class="sub-task">' + escapeHtml(normalize(spawn.task || '')) + '</span></summary>';
    for (const e of subThread.get(spawn.agent_id) || []) {
      html += renderEntry(e);
    }
    return html + '</details>';
  }

  function renderUser(entry) {
    const content = entry.message.content;
    let html = '<div class="msg user" id="entry-' + escapeHtml(entry.id) + '"><div class="role">User</div>';
    if (Array.isArray(content)) {
      for (const c of content) {
        if (c.type === 'image') html += imageTag(c, 'message-image');
      }
    }
    const text = textOf(content);
    if (text.trim()) html += '<div class="markdown-content">' + md(text) + '</div>';
    return html + '</div>';
  }

  function renderEntry(entry) {
    if (entry.type === 'message') {
      const role = entry.message.role;
      if (role === 'user') return renderUser(entry);
      if (role === 'assistant') return renderAssistant(entry);
      // Tool results are shown inline under their call.
      return '';
    }
    if (entry.type === 'compaction') {
      return '<div class="compaction"><div class="compaction-head">context compacted</div>' +
        md(entry.summary || '') + '</div>';
    }
    if (entry.type === 'model_change') {
      return '<div class="model-change">switched model to <span class="model-name">' +
        escapeHtml(entry.provider + '/' + entry.model_id) + '</span></div>';
    }
    if (entry.type === 'thinking_change') {
      return '<div class="notice">thinking: ' + escapeHtml(entry.level) + '</div>';
    }
    if (entry.type === 'speed_change') {
      return '<div class="notice">speed: ' + escapeHtml(entry.speed) + '</div>';
    }
    if (entry.type === 'verbosity_change') {
      return '<div class="notice">verbosity: ' + escapeHtml(entry.verbosity) + '</div>';
    }
    return '';
  }

  // ============================================================
  // PATH (root -> leaf over the user thread)
  // ============================================================

  function pathTo(targetId) {
    const path = [];
    let cur = byId.get(targetId);
    while (cur) {
      path.unshift(cur);
      if (!cur.parent_id || cur.parent_id === cur.id) break;
      cur = byId.get(cur.parent_id);
    }
    return path;
  }

  // ============================================================
  // HEADER / STATS
  // ============================================================

  function computeStats() {
    const s = {
      user: 0, assistant: 0, toolResults: 0, toolCalls: 0, compactions: 0,
      tokens: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, cost: 0,
      models: new Set(),
    };
    for (const e of entries) {
      if (e.type === 'compaction') s.compactions++;
      if (e.type !== 'message') continue;
      const m = e.message;
      if (m.role === 'user') s.user++;
      else if (m.role === 'tool_result') s.toolResults++;
      else if (m.role === 'assistant') {
        s.assistant++;
        if (m.model) s.models.add(m.provider ? m.provider + '/' + m.model : m.model);
        if (m.usage) {
          s.tokens.input += m.usage.input || 0;
          s.tokens.output += m.usage.output || 0;
          s.tokens.cacheRead += m.usage.cache_read || 0;
          s.tokens.cacheWrite += m.usage.cache_write || 0;
          if (m.usage.cost) s.cost += m.usage.cost.total || 0;
        }
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
      '</div></div>' +
      '<div class="header-info">' +
      infoItem('Date', formatDate(firstTs)) +
      infoItem('Models', Array.from(s.models).join(', ') || 'unknown') +
      infoItem('Messages', msgParts.join(', ') || '0') +
      infoItem('Tool calls', String(s.toolCalls)) +
      infoItem('Tokens', tokenParts.join(' ') || '0') +
      infoItem('Cost', '$' + s.cost.toFixed(4)) +
      '</div></div>';

    const sysEntry = entries.find((e) => e.type === 'system_prompt');
    if (sysEntry && sysEntry.text) {
      html += '<details class="system-prompt"><summary class="system-prompt-header">System prompt</summary>' +
        '<pre>' + escapeHtml(sysEntry.text) + '</pre></details>';
    }
    return html;
  }

  function infoItem(label, value) {
    return '<span class="info-label">' + label + ':</span><span class="info-value">' + escapeHtml(value) + '</span>';
  }

  // ============================================================
  // INITIALIZATION
  // ============================================================

  function renderConversation() {
    document.getElementById('header-container').innerHTML = renderHeader();
    const messages = document.getElementById('messages');
    const path = leafId ? pathTo(leafId) : [];
    let html = '';
    for (const entry of path) html += renderEntry(entry);
    messages.innerHTML = html;
    attachHeaderHandlers();
  }

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
    if (t) t.addEventListener('click', toggleThinking);
    if (o) o.addEventListener('click', toggleTools);
  }

  function isEditable(el) {
    if (!el) return false;
    const tag = el.tagName;
    return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || tag === 'BUTTON';
  }

  document.addEventListener('keydown', (e) => {
    if (isEditable(document.activeElement)) return;
    const key = e.key.toLowerCase();
    if (key === 't') { e.preventDefault(); toggleThinking(); }
    else if (key === 'o') { e.preventDefault(); toggleTools(); }
  });

  renderConversation();
})();
