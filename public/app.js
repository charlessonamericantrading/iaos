document.addEventListener('DOMContentLoaded', () => {
  // Navigation Tabs
  const navItems = document.querySelectorAll('.nav-item');
  const tabPanels = document.querySelectorAll('.tab-panel');
  const pageTitle = document.getElementById('pageTitle');

  const tabTitles = {
    dispatcher: 'Task Dispatcher & Agent Console',
    agents: 'Process Manager & Agent Tree',
    mal: 'Model Abstraction Layer (MAL)',
    memory: 'Unified Context Memory & KV Paging',
    sandbox: 'Tool Sandbox & Permission Audit',
    studio: 'Software Studio & App IDE'
  };

  navItems.forEach(item => {
    item.addEventListener('click', () => {
      const targetTab = item.getAttribute('data-tab');
      navItems.forEach(n => n.classList.remove('active'));
      tabPanels.forEach(p => p.classList.remove('active'));

      item.classList.add('active');
      const panel = document.getElementById(`tab-${targetTab}`);
      if (panel) panel.classList.add('active');

      if (tabTitles[targetTab]) pageTitle.innerText = tabTitles[targetTab];

      // Refresh tab data
      if (targetTab === 'agents') loadAgents();
      if (targetTab === 'mal') loadMAL();
      if (targetTab === 'memory') loadMemory();
      if (targetTab === 'sandbox') loadSandboxAudit();
      if (targetTab === 'studio') loadSoftwareStudio();
    });
  });

  // WebSocket Connection
  let ws;
  function connectWebSocket() {
    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const wsUrl = `${protocol}//${window.location.host}`;
    ws = new WebSocket(wsUrl);

    ws.onopen = () => {
      appendLogEntry('[SYSTEM] Connected to AgentOS Kernel WebSocket', 'sys');
    };

    ws.onmessage = (event) => {
      try {
        const msg = JSON.parse(event.data);
        handleKernelEvent(msg);
      } catch (err) {
        console.error('Failed to parse WS message', err);
      }
    };

    ws.onclose = () => {
      appendLogEntry('[SYSTEM] WebSocket disconnected. Reconnecting in 3s...', 'term');
      setTimeout(connectWebSocket, 3000);
    };
  }

  connectWebSocket();

  function handleKernelEvent(msg) {
    if (msg.type === 'METRICS_UPDATE') {
      updateMetricsUI(msg.payload);
    } else if (msg.type === 'PROCESS_CREATED') {
      appendLogEntry(`[PROCESS] Spawned PID ${msg.payload.pid}: ${msg.payload.name} (${msg.payload.role})`, 'proc');
    } else if (msg.type === 'PROCESS_TERMINATED') {
      appendLogEntry(`[PROCESS] Terminated PID ${msg.payload.pid}`, 'term');
    } else if (msg.type === 'AGENT_LOG') {
      appendLogEntry(`[PID ${msg.payload.pid}] ${msg.payload.log}`, 'proc');
    }
  }

  function appendLogEntry(text, cls = 'sys') {
    const stream = document.getElementById('eventStreamLog');
    if (!stream) return;
    const div = document.createElement('div');
    div.className = `log-entry ${cls}`;
    const time = new Date().toLocaleTimeString();
    div.innerText = `[${time}] ${text}`;
    stream.appendChild(div);
    stream.scrollTop = stream.scrollHeight;
  }

  function updateMetricsUI(m) {
    document.getElementById('activeAgentsVal').innerText = m.activeAgents;
    document.getElementById('kvEfficiencyVal').innerText = `${m.kvCacheEfficiencyPercent}%`;
    document.getElementById('vramVal').innerText = `${(m.vramUsedMB / 1024).toFixed(1)} / ${(m.vramTotalMB / 1024).toFixed(0)} GB`;
    document.getElementById('throughputVal').innerText = `${m.throughputTokensPerSec} t/s`;

    // Uptime formatter
    const s = m.uptimeSeconds;
    const hrs = Math.floor(s / 3600).toString().padStart(2, '0');
    const mins = Math.floor((s % 3600) / 60).toString().padStart(2, '0');
    const secs = (s % 60).toString().padStart(2, '0');
    document.getElementById('uptimeBadge').innerText = `${hrs}:${mins}:${secs}`;
  }

  // --- Task Dispatcher Chat ---
  const btnSendTask = document.getElementById('btnSendTask');
  const taskInput = document.getElementById('taskInput');
  const chatLog = document.getElementById('chatLog');
  const preferLocalToggle = document.getElementById('preferLocalToggle');

  async function sendTask() {
    const prompt = taskInput.value.trim();
    if (!prompt) return;

    // User message
    const userDiv = document.createElement('div');
    userDiv.className = 'chat-message user';
    userDiv.innerText = prompt;
    chatLog.appendChild(userDiv);
    taskInput.value = '';
    chatLog.scrollTop = chatLog.scrollHeight;

    // Loading Kernel message
    const loadingDiv = document.createElement('div');
    loadingDiv.className = 'chat-message kernel';
    loadingDiv.innerText = '[AgentOS Kernel] Planificando subagentes, consultando UCM y enrutando MAL...';
    chatLog.appendChild(loadingDiv);

    try {
      const res = await fetch('/api/task/dispatch', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ prompt, preferLocal: preferLocalToggle.checked })
      });
      const data = await res.json();
      loadingDiv.innerText = data.response;
      chatLog.scrollTop = chatLog.scrollHeight;
    } catch (err) {
      loadingDiv.innerText = `[Error de Ejecución Kernel] ${err.message}`;
    }
  }

  btnSendTask.addEventListener('click', sendTask);
  taskInput.addEventListener('keypress', (e) => {
    if (e.key === 'Enter') sendTask();
  });

  // --- Agents Table ---
  async function loadAgents() {
    try {
      const res = await fetch('/api/agents');
      const agents = await res.json();
      const tbody = document.querySelector('#agentsTable tbody');
      tbody.innerHTML = '';

      agents.forEach(a => {
        const tr = document.createElement('tr');
        tr.innerHTML = `
          <td><strong>PID ${a.pid}</strong></td>
          <td>${a.name}</td>
          <td>${a.role}</td>
          <td><span class="badge ${a.state.toLowerCase()}">${a.state}</span></td>
          <td>${a.priority}</td>
          <td>${a.tokensUsed.toLocaleString()} / ${a.maxTokens.toLocaleString()}</td>
          <td><code>${a.capabilities.map(c => c.tool).join(', ')}</code></td>
          <td>
            ${a.pid > 4 ? `<button class="btn btn-secondary btn-kill" data-pid="${a.pid}" style="padding:4px 8px; font-size:11px;">Terminar</button>` : '<span style="color:#6b7280;">System</span>'}
          </td>
        `;
        tbody.appendChild(tr);
      });

      document.querySelectorAll('.btn-kill').forEach(btn => {
        btn.addEventListener('click', async () => {
          const pid = btn.getAttribute('data-pid');
          await fetch(`/api/agents/${pid}`, { method: 'DELETE' });
          loadAgents();
        });
      });
    } catch (err) {
      console.error('Failed to load agents', err);
    }
  }

  document.getElementById('btnRefreshAgents').addEventListener('click', loadAgents);

  // --- MAL Endpoints & Router Test ---
  async function loadMAL() {
    try {
      const res = await fetch('/api/mal/endpoints');
      const data = await res.json();
      const container = document.getElementById('endpointsList');
      container.innerHTML = '';

      data.endpoints.forEach(ep => {
        const div = document.createElement('div');
        div.className = 'endpoint-item';
        div.innerHTML = `
          <div>
            <strong>${ep.name}</strong> <span class="badge ${ep.provider === 'LOCAL' ? 'ready' : 'running'}">${ep.provider}</span>
            <div style="font-size:12px; color:#9ca3af; margin-top:4px;">
              Latencia: ${ep.latencyMs}ms | Costo: $${ep.costPer1kTokensUSD}/1k tok | VRAM: ${ep.vramUsageMB} MB
            </div>
          </div>
          <span style="font-size:12px; font-family:monospace; color:#818cf8;">${ep.capabilities.join(', ')}</span>
        `;
        container.appendChild(div);
      });
    } catch (err) {
      console.error('Failed to load MAL', err);
    }
  }

  document.getElementById('btnTestRoute').addEventListener('click', async () => {
    const taskType = document.getElementById('routerTaskType').value;
    const res = await fetch('/api/mal/route', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ taskType, preferLocal: true })
    });
    const decision = await res.json();
    const box = document.getElementById('routeResultBox');
    box.style.display = 'block';
    box.innerHTML = `
      <div style="background:rgba(99,102,241,0.15); border:1px solid #6366f1; border-radius:8px; padding:14px; font-size:13px;">
        <h4 style="color:#a5b4fc; margin-bottom:6px;">Decisión de Enrutado MAL</h4>
        <p><strong>Endpoint Seleccionado:</strong> ${decision.selectedEndpoint.name}</p>
        <p><strong>Razón:</strong> ${decision.reason}</p>
        <p><strong>Cadena de Fallback:</strong> ${decision.fallbackChain.join(' -> ')}</p>
        <p><strong>Latencia Estimada:</strong> ${decision.estimatedLatencyMs} ms</p>
      </div>
    `;
  });

  // --- Memory Manager (UCM) ---
  async function loadMemory() {
    try {
      const res = await fetch('/api/memory/kv');
      const data = await res.json();
      const grid = document.getElementById('kvFramesGrid');
      grid.innerHTML = '';

      data.frames.forEach(f => {
        const div = document.createElement('div');
        div.className = 'kv-frame-card';
        const locCls = f.location === 'GPU_VRAM' ? 'loc-vram' : (f.location === 'SYSTEM_RAM' ? 'loc-ram' : 'loc-nvme');
        div.innerHTML = `
          <div>
            <strong>${f.pageId}</strong> (PID ${f.pid})
            <div style="font-size:11px; color:#9ca3af;">Tokens: ${f.tokensCount} | Hits: ${f.hitCount}</div>
          </div>
          <span class="${locCls}">${f.location}</span>
        `;
        grid.appendChild(div);
      });
    } catch (err) {
      console.error('Failed to load memory', err);
    }
  }

  document.getElementById('btnVectorSearch').addEventListener('click', async () => {
    const q = document.getElementById('vectorSearchInput').value;
    const res = await fetch(`/api/memory/vector?q=${encodeURIComponent(q)}`);
    const docs = await res.json();
    const container = document.getElementById('vectorResultsList');
    container.innerHTML = '';

    docs.forEach(d => {
      const div = document.createElement('div');
      div.style.background = 'rgba(255,255,255,0.03)';
      div.style.padding = '10px 14px';
      div.style.borderRadius = '8px';
      div.style.marginBottom = '8px';
      div.style.fontSize = '13px';
      div.innerHTML = `
        <div style="display:flex; justify-content:space-between; margin-bottom:4px;">
          <strong>${d.id}</strong>
          <span style="color:#10b981; font-weight:bold;">Score Similitud: ${d.score}</span>
        </div>
        <div style="color:#d1d5db;">${d.content}</div>
      `;
      container.appendChild(div);
    });
  });

  // --- Tool Sandbox Audit ---
  async function loadSandboxAudit() {
    try {
      const res = await fetch('/api/sandbox/audit');
      const audit = await res.json();
      const tbody = document.querySelector('#auditTable tbody');
      tbody.innerHTML = '';

      audit.reverse().forEach(a => {
        const tr = document.createElement('tr');
        const time = new Date(a.timestamp).toLocaleTimeString();
        tr.innerHTML = `
          <td>${time}</td>
          <td>PID ${a.pid}</td>
          <td><code>${a.tool}</code></td>
          <td>${JSON.stringify(a.args)}</td>
          <td>
            <span class="badge ${a.granted ? 'ready' : 'terminated'}">
              ${a.granted ? 'CONCEDIDO' : 'DENEGADO'}
            </span>
          </td>
        `;
        tbody.appendChild(tr);
      });
    } catch (err) {
      console.error('Failed to load sandbox audit', err);
    }
  }

  // --- Software Studio & Apps API ---
  async function loadSoftwareStudio() {
    try {
      const res = await fetch('/api/studio/apps');
      const apps = await res.json();
      const container = document.getElementById('installedAppsList');
      container.innerHTML = '';

      apps.forEach(app => {
        const div = document.createElement('div');
        div.className = 'endpoint-item';
        div.innerHTML = `
          <div>
            <strong>${app.manifest.name}</strong> <span class="badge ready">v${app.manifest.version}</span>
            <div style="font-size:12px; color:#9ca3af; margin-top:4px;">
              ${app.manifest.description}
            </div>
            <div style="font-size:11px; color:#6ee7b7; margin-top:4px; font-family:var(--font-mono);">
              Ejecuciones: ${app.executionCount} | Permisos: ${app.manifest.capabilities.join(', ')}
            </div>
          </div>
          <button class="btn btn-secondary btn-run-app" data-appid="${app.manifest.id}" style="padding:6px 12px; font-size:12px;">
            <i class="fa-solid fa-play"></i> Ejecutar App
          </button>
        `;
        container.appendChild(div);
      });

      document.querySelectorAll('.btn-run-app').forEach(btn => {
        btn.addEventListener('click', async () => {
          const appId = btn.getAttribute('data-appid');
          const consoleBox = document.getElementById('appRunConsole');
          consoleBox.style.display = 'block';
          consoleBox.innerText = `[Kernel Sandbox] Ejecutando app ${appId}...`;

          const resRun = await fetch(`/api/studio/apps/${appId}/run`, { method: 'POST' });
          const runData = await resRun.json();
          consoleBox.innerText = runData.output;
          loadSoftwareStudio();
        });
      });
    } catch (err) {
      console.error('Failed to load software studio apps', err);
    }
  }

  document.getElementById('btnDeployApp')?.addEventListener('click', async () => {
    const name = document.getElementById('appNameInput').value;
    const sourceCode = document.getElementById('appCodeInput').value;
    if (!name || !sourceCode) return;

    const res = await fetch('/api/studio/apps', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        name,
        description: 'Aplicación creada en AgentOS Software Studio',
        sourceCode,
        capabilities: ['gpu:inference', 'fs:read', 'exec:code']
      })
    });

    if (res.ok) {
      const consoleBox = document.getElementById('appRunConsole');
      consoleBox.style.display = 'block';
      consoleBox.innerText = `[AgentOS Compiler] Aplicación '${name}' compilada y desplegada en el Kernel con éxito.`;
      loadSoftwareStudio();
    }
  });

  // Initial load
  loadAgents();
});
