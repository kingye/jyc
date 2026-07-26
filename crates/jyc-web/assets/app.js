/* ── Token / Auth ── */
function getToken() { return localStorage.getItem('jyc_token'); }
function setToken(t) { localStorage.setItem('jyc_token', t); }
function clearToken() { localStorage.removeItem('jyc_token'); }

function authHeaders() {
  const t = getToken();
  return t ? { 'Authorization': 'Bearer ' + t } : {};
}

/* ── API helpers ── */
async function apiFetch(url, opts = {}) {
  const headers = { ...authHeaders(), ...opts.headers };
  const res = await fetch(url, { ...opts, headers });
  if (res.status === 401) {
    clearToken();
    showLogin(true);
    throw new Error('Unauthorized');
  }
  return res;
}

async function apiGetState() {
  const res = await apiFetch('/state');
  if (!res.ok) throw new Error('Failed to fetch state: ' + res.status);
  return res.json();
}

async function apiInjectMessage(channel, thread, text) {
  const res = await apiFetch('/inject_message', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ channel, thread, text }),
  });
  if (!res.ok) throw new Error('Failed to send message: ' + res.status);
  return res.json();
}

/* ── Login dialog ── */
const loginDialog = document.getElementById('login-dialog');
const tokenInput = document.getElementById('token-input');
const loginError = document.getElementById('login-error');

function showLogin(force) {
  if (!getToken() || force) {
    document.getElementById('login-btn').hidden = false;
    if (force) {
      loginError.hidden = false;
      loginDialog.showModal();
    }
  } else {
    document.getElementById('login-btn').hidden = true;
  }
}

document.addEventListener('DOMContentLoaded', () => {
  const loginBtn = document.getElementById('login-btn');
  if (loginBtn) loginBtn.onclick = () => { loginError.hidden = true; tokenInput.value = ''; loginDialog.showModal(); };

  document.getElementById('login-cancel')?.addEventListener('click', () => loginDialog.close());
  loginDialog.addEventListener('close', () => {
    const val = tokenInput.value.trim();
    if (val) {
      setToken(val);
      loginError.hidden = true;
      document.getElementById('login-btn').hidden = true;
      init();
    }
  });

  if (getToken()) document.getElementById('login-btn').hidden = true;
  init();
});

/* ── State ── */
let state = null;
let activeChannel = null;
let activeThread = null;
let ws = null;
let pollTimer = null;
let statePollTimer = null;
let wsReconnectTimer = null;
let wsReconnectAttempts = 0;
/** True once the active WebSocket reached OPEN state. Used to distinguish
 * initial connection failure (no reconnect, fall through to polling) from
 * a mid-conversation drop (reconnect with backoff). */
let wsWasConnected = false;
const MSG_HISTORY_LIMIT = 100;
const WS_RECONNECT_MAX_DELAY_MS = 30000;

/* ── Main init ── */
async function init() {
  const path = window.location.pathname;
  if (path === '/t/' || path.startsWith('/t/')) {
    initThreadView();
  } else {
    initDashboard();
  }
}

/* ── Dashboard (index page) ── */
async function initDashboard() {
  try {
    state = await apiGetState();
    renderSidebar();
    renderStats();
    showLogin(false);
    showMobileMenuButton();

    // Auto-poll for state updates (clear any previous interval to avoid leak)
    if (statePollTimer) clearInterval(statePollTimer);
    statePollTimer = setInterval(async () => {
      try {
        state = await apiGetState();
        renderSidebar();
        renderStats();
        if (activeThread) updateActiveThreadMessages();
      } catch (e) { /* ignore polling errors */ }
    }, 5000);
  } catch (e) {
    if (e.message === 'Unauthorized') return;
    showError('Failed to connect: ' + e.message);
  }
}

/** Show the mobile menu button only on small screens. */
function showMobileMenuButton() {
  const btn = document.getElementById('mobile-menu-btn');
  if (btn) btn.hidden = window.innerWidth >= 768;
}

/** Re-evaluate mobile menu button visibility on resize. */
window.addEventListener('resize', showMobileMenuButton);

function renderSidebar() {
  const list = document.getElementById('thread-list');
  if (!list) return;

  // Group threads by channel
  const groups = {};
  for (const ch of state.channels) {
    groups[ch.name] = { info: ch, threads: [] };
  }
  for (const t of state.threads) {
    if (!groups[t.channel]) groups[t.channel] = { info: { name: t.channel, channel_type: '?' }, threads: [] };
    groups[t.channel].threads.push(t);
  }

  let html = '';
  for (const [chName, group] of Object.entries(groups)) {
    const count = group.threads.length;
    html += '<div class="channel-group">';
    html += `<div class="channel-header"><span>${escHtml(chName)}</span><span class="channel-type">${escHtml(group.info.channel_type)}</span><span class="channel-count">${count}</span></div>`;
    for (const t of group.threads) {
      const active = t.name === activeThread && chName === activeChannel ? ' active' : '';
      const statusClass = t.status === 'processing' ? 'processing' : t.status === 'error' ? 'error' : t.status === 'queued' ? 'queued' : t.status === 'waiting_for_answer' ? 'waiting' : 'idle';
      html += `<div class="thread-item${active}" data-channel="${escHtml(chName)}" data-thread="${escHtml(t.name)}">`;
      html += `<span class="thread-name">${escHtml(t.name)}</span>`;
      html += `<div class="thread-meta"><span class="status-dot ${statusClass}"></span> ${escHtml(t.status)}`;
      if (t.last_active_at) html += ` · ${timeAgo(t.last_active_at)}`;
      html += '</div></div>';
    }
    html += '</div>';
  }

  list.innerHTML = html;

  // Click handler
  list.querySelectorAll('.thread-item').forEach(el => {
    el.addEventListener('click', () => {
      const channel = el.dataset.channel;
      const thread = el.dataset.thread;
      openThread(channel, thread);
    });
  });

  updateStatusBadge();
}

function renderStats() {
  const el = document.getElementById('stats');
  if (!el || !state) return;
  el.innerHTML = `
    <div class="stat-item"><span class="stat-value">${state.stats.active_workers}</span><span class="stat-label">Active</span></div>
    <div class="stat-item"><span class="stat-value">${state.stats.total_threads}</span><span class="stat-label">Threads</span></div>
    <div class="stat-item"><span class="stat-value">${state.stats.errors}</span><span class="stat-label">Errors</span></div>
    <div class="stat-item"><span class="stat-value">${state.stats.messages_processed}</span><span class="stat-label">Processed</span></div>
  `;
}

function updateStatusBadge() {
  const badge = document.getElementById('status-badge');
  if (badge) {
    badge.textContent = state ? 'Connected' : 'Disconnected';
    badge.className = 'status-badge' + (state ? ' online' : '');
  }
}

/* ── Open thread ── */
function openThread(channel, thread) {
  activeChannel = channel;
  activeThread = thread;

  // Highlight in sidebar
  document.querySelectorAll('.thread-item').forEach(el => {
    el.classList.toggle('active', el.dataset.channel === channel && el.dataset.thread === thread);
  });

  // Show chat view
  document.getElementById('welcome-view').hidden = true;
  document.getElementById('chat-view').hidden = false;
  document.getElementById('chat-thread-name').textContent = thread;
  document.getElementById('chat-channel-name').textContent = channel;
  document.getElementById('msg-input').disabled = false;
  document.getElementById('send-btn').disabled = false;

  // Mobile: show chat pane, hide sidebar
  if (window.innerWidth < 768) {
    document.getElementById('sidebar').classList.add('collapsed');
    document.querySelector('.chat-pane').classList.add('active');
  }

  // Disconnect any previous connection
  disconnect();

  // Connect
  connectToThread(channel, thread);
}

/* ── Connection management ── */
function disconnect() {
  if (ws) { ws.close(); ws = null; }
  if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }
  if (wsReconnectTimer) { clearTimeout(wsReconnectTimer); wsReconnectTimer = null; }
  wsReconnectAttempts = 0;
  wsWasConnected = false;
}

function connectToThread(channel, thread) {
  const msgContainer = document.getElementById('messages');
  msgContainer.innerHTML = '<div class="loading">Connecting...</div>';

  // Try WebSocket first (for WS-capable channels)
  tryConnectWS(channel, thread).catch(() => {
    // Fallback: inject_message + polling
    startPolling(channel, thread);
  });
}

/* ── WebSocket chat ── */
async function tryConnectWS(channel, thread) {
  return new Promise((resolve, reject) => {
    const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const host = window.location.host;
    const url = `${proto}//${host}/ws/${encodeURIComponent(channel)}`;

    // NOTE: The browser WebSocket API does NOT support custom headers
    // (Authorization, etc.). When the server has auth enabled, the WS
    // upgrade will be rejected with 401 and the caller falls back to
    // polling (which uses the token via fetch headers). State-based chat
    // still works; only the real-time push channel is degraded.
    const socket = new WebSocket(url);
    socket.onopen = () => {
      wsReconnectAttempts = 0;
      wsWasConnected = true;
      // Subscribe to thread
      socket.send(JSON.stringify({ type: 'subscribe', thread }));
      ws = socket;
      resolve();
    };
    socket.onmessage = (ev) => {
      try {
        const msg = JSON.parse(ev.data);
        handleWSMessage(msg, channel, thread);
      } catch (e) { /* ignore parse errors */ }
    };
    socket.onerror = () => { reject(new Error('WS failed')); };
    socket.onclose = () => {
      if (ws === socket) ws = null;
      // Only auto-reconnect when the WS was previously connected and the
      // conversation is still active. Initial connection failures (auth,
      // non-WS channel, 404) must NOT trigger reconnect — the caller has
      // already fallen through to polling.
      const wasConnected = wsWasConnected;
      wsWasConnected = false;
      if (wasConnected && activeThread === thread && activeChannel === channel) {
        wsReconnectAttempts += 1;
        const delay = Math.min(1000 * 2 ** wsReconnectAttempts, WS_RECONNECT_MAX_DELAY_MS);
        wsReconnectTimer = setTimeout(() => {
          wsReconnectTimer = null;
          if (activeThread === thread && activeChannel === channel) {
            connectToThread(channel, thread);
          }
        }, delay);
      }
    };

    // Timeout: if WS doesn't open in 2s, fall back to polling
    setTimeout(() => {
      if (!ws) {
        socket.close();
        reject(new Error('WS timeout'));
      }
    }, 2000);
  });
}

function handleWSMessage(msg, channel, thread) {
  const msgContainer = document.getElementById('messages');
  if (!msgContainer) return;

  if (msg.type === 'history') {
    // Load history
    msgContainer.innerHTML = '';
    for (const entry of msg.messages) {
      addMessage(entry.sender, entry.text, entry.timestamp);
    }
    if (msgContainer.children.length === 0) {
      msgContainer.innerHTML = '<div class="loading">No messages yet. Start the conversation!</div>';
    }
    msgContainer.scrollTop = msgContainer.scrollHeight;
  } else if (msg.type === 'reply') {
    // Real-time reply
    addMessage('ai', msg.text);
    msgContainer.scrollTop = msgContainer.scrollHeight;
  }
}

/* ── Polling for non-WS channels ── */
function startPolling(channel, thread) {
  // Clear any existing poll timer to prevent interval leak when startPolling
  // is called more than once for the same thread (defensive — disconnect()
  // should normally handle this).
  if (pollTimer) { clearInterval(pollTimer); pollTimer = null; }

  const msgContainer = document.getElementById('messages');
  msgContainer.innerHTML = '<div class="loading">Loading messages...</div>';
  delete msgContainer.dataset.msgCount;

  async function poll() {
    try {
      const s = await apiGetState();
      const t = s.threads.find(th => th.name === thread && th.channel === channel);
      if (!t) {
        renderMessages([], `Thread '${thread}' not found.`);
        return;
      }
      const msgs = t.recent_messages || [];
      const prevCount = Number(msgContainer.dataset.msgCount) || 0;
      // Re-render when message count changes (new message arrived or first load).
      if (msgs.length !== prevCount) {
        if (msgs.length === 0) {
          renderMessages([], 'No messages yet. Start the conversation!');
        } else {
          renderMessages(msgs.slice(-MSG_HISTORY_LIMIT));
        }
      }
      updateThreadStatus(t);
    } catch (e) { /* ignore poll errors */ }
  }

  /** Replace messages container contents with the given list of messages, or a
   *  placeholder when empty. */
  function renderMessages(msgs, placeholder) {
    msgContainer.innerHTML = '';
    if (msgs.length === 0) {
      const empty = document.createElement('div');
      empty.className = 'loading';
      empty.textContent = placeholder || 'No messages yet.';
      msgContainer.appendChild(empty);
    } else {
      for (const m of msgs) {
        addMessage(m.sender === 'ai' ? 'ai' : 'user', m.text, m.timestamp);
      }
      msgContainer.scrollTop = msgContainer.scrollHeight;
    }
    msgContainer.dataset.msgCount = String(msgs.length);
  }

  // Initial load
  poll();
  pollTimer = setInterval(poll, 5000);
}

function updateThreadStatus(t) {
  const statusEl = document.getElementById('chat-thread-status');
  if (statusEl && t) {
    statusEl.textContent = t.status;
    statusEl.className = 'status-tag ' + (t.status === 'processing' ? 'processing' : t.status === 'error' ? 'error' : 'idle');
  }
}

function updateActiveThreadMessages() {
  if (!activeThread || !state) return;
  const t = state.threads.find(th => th.name === activeThread && th.channel === activeChannel);
  if (!t) return;
  const msgContainer = document.getElementById('messages');
  if (!msgContainer || msgContainer.querySelector('.loading')) return;

  const msgs = t.recent_messages || [];
  const prevCount = Number(msgContainer.dataset.msgCount) || 0;
  if (msgs.length > prevCount) {
    // New messages arrived
    for (let i = prevCount; i < msgs.length; i++) {
      const m = msgs[i];
      addMessage(m.sender === 'ai' ? 'ai' : 'user', m.text, m.timestamp);
    }
    msgContainer.dataset.msgCount = String(msgs.length);
    msgContainer.scrollTop = msgContainer.scrollHeight;
  }
  updateThreadStatus(t);
}

/* ── Send message ── */
function sendMessage() {
  const input = document.getElementById('msg-input');
  const text = input.value.trim();
  if (!text || !activeChannel || !activeThread) return;

  // Show user message immediately
  addMessage('user', text);
  input.value = '';

  if (ws && ws.readyState === WebSocket.OPEN) {
    // Use WebSocket
    ws.send(JSON.stringify({ type: 'message', thread: activeThread, text }));
  } else {
    // Use inject_message + polling
    apiInjectMessage(activeChannel, activeThread, text).catch(e => {
      if (e.message !== 'Unauthorized') showError('Failed to send: ' + e.message);
    });
  }
}

/* ── Message rendering ── */
function addMessage(sender, text, timestamp) {
  const container = document.getElementById('messages');
  if (!container) return;

  // Remove loading message
  const loading = container.querySelector('.loading');
  if (loading) loading.remove();

  const div = document.createElement('div');
  div.className = 'msg ' + sender;
  div.textContent = text;

  if (timestamp) {
    const time = document.createElement('span');
    time.className = 'msg-time';
    time.textContent = formatTime(timestamp);
    div.appendChild(time);
  }

  container.appendChild(div);
  container.scrollTop = container.scrollHeight;
}

/* ── Thread view (separate page) ── */
async function initThreadView() {
  const path = window.location.pathname;
  const parts = path.split('/').filter(Boolean);
  const threadName = parts[1] || '';

  try {
    state = await apiGetState();
    const t = state.threads.find(th => th.name === threadName);
    if (!t) throw new Error('Thread not found');

    activeChannel = t.channel;
    activeThread = t.name;

    document.getElementById('thread-title').textContent = t.name;
    document.getElementById('chat-thread-name').textContent = t.name;
    document.getElementById('chat-channel-name').textContent = t.channel;
    document.getElementById('msg-input').disabled = false;
    document.getElementById('send-btn').disabled = false;

    connectToThread(t.channel, t.name);
    showLogin(false);
  } catch (e) {
    if (e.message === 'Unauthorized') return;
    showError(e.message);
  }
}

/* ── Utilities ── */
function escHtml(s) {
  const d = document.createElement('div');
  d.textContent = s;
  return d.innerHTML;
}

function formatTime(ts) {
  if (!ts) return '';
  try {
    const d = new Date(ts);
    return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  } catch (e) { return ''; }
}

function timeAgo(ts) {
  if (!ts) return '';
  try {
    const now = Date.now();
    const then = new Date(ts).getTime();
    const sec = Math.floor((now - then) / 1000);
    if (sec < 60) return 'just now';
    if (sec < 3600) return Math.floor(sec / 60) + 'm ago';
    if (sec < 86400) return Math.floor(sec / 3600) + 'h ago';
    return Math.floor(sec / 86400) + 'd ago';
  } catch (e) { return ''; }
}

function showError(msg) {
  const el = document.getElementById('error-message');
  const d = document.getElementById('error-dialog');
  if (el && d) { el.textContent = msg; d.showModal(); }
}

/* ── Send on Enter (Shift+Enter for newline) ── */
document.addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && !e.shiftKey) {
    const input = document.getElementById('msg-input');
    if (document.activeElement === input) {
      e.preventDefault();
      sendMessage();
    }
  }
});

/* ── Send button ── */
document.getElementById('send-btn')?.addEventListener('click', sendMessage);

/* ── Mobile menu toggle ── */
document.getElementById('mobile-menu-btn')?.addEventListener('click', () => {
  document.getElementById('sidebar').classList.toggle('collapsed');
  document.querySelector('.chat-pane').classList.toggle('active');
});
