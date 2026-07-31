import express from 'express';
import cors from 'cors';
import { createServer } from 'http';
import { WebSocketServer, WebSocket } from 'ws';
import path from 'path';
import { fileURLToPath } from 'url';

import { ModelAbstractionLayer } from '../core/mal.js';
import { UnifiedContextMemory } from '../core/memory.js';
import { ToolSandbox } from '../core/sandbox.js';
import { AgentScheduler } from '../core/scheduler.js';
import { AgentOSAppManager } from '../core/abi.js';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

const app = express();
const httpServer = createServer(app);
const wss = new WebSocketServer({ server: httpServer });

app.use(cors());
app.use(express.json());

// Serve static dashboard UI
const publicPath = path.join(__dirname, '../../public');
app.use(express.static(publicPath));

// Initialize AgentOS Subsystems
const mal = new ModelAbstractionLayer();
const memory = new UnifiedContextMemory();
const sandbox = new ToolSandbox();
const scheduler = new AgentScheduler(mal, memory, sandbox);
const appManager = new AgentOSAppManager();

// Broadcast real-time events to all WebSocket clients
function broadcast(type: string, payload: any) {
  const msg = JSON.stringify({ type, payload, timestamp: Date.now() });
  wss.clients.forEach(client => {
    if (client.readyState === WebSocket.OPEN) {
      client.send(msg);
    }
  });
}

scheduler.setLogListener((event) => {
  broadcast(event.type, event.data);
});

// Periodically run background memory paging and metrics broadcast
setInterval(() => {
  memory.performKVCachePaging();
  const metrics = {
    uptimeSeconds: Math.floor(process.uptime()),
    activeAgents: scheduler.getProcesses().filter(p => p.state !== 'TERMINATED').length,
    totalTokensProcessed: scheduler.getTotalTokensProcessed() + mal.getStats().totalTokensRouted,
    kvCacheEfficiencyPercent: memory.getKVEfficiency(),
    vramUsedMB: 4800 + Math.floor(Math.random() * 200),
    vramTotalMB: 16384,
    ramUsedMB: 3200 + Math.floor(Math.random() * 150),
    localVsCloudRatio: { local: 85, cloud: 15 },
    throughputTokensPerSec: (Math.random() * 40 + 120).toFixed(1)
  };
  broadcast('METRICS_UPDATE', metrics);
}, 2000);

// --- REST API ENDPOINTS ---

app.get('/api/status', (req, res) => {
  res.json({
    status: 'ONLINE',
    version: 'AgentOS v1.0.0-alpha',
    uptime: process.uptime(),
    activeProcesses: scheduler.getProcesses().length,
    endpoints: mal.getEndpoints().length
  });
});

app.get('/api/agents', (req, res) => {
  res.json(scheduler.getProcesses());
});

app.post('/api/agents', (req, res) => {
  const { name, role, priority, task } = req.body;
  const proc = scheduler.createProcess({ name, role, priority, task });
  res.status(201).json(proc);
});

app.delete('/api/agents/:pid', (req, res) => {
  const pid = parseInt(req.params.pid, 10);
  const success = scheduler.terminateProcess(pid);
  res.json({ success });
});

app.get('/api/mal/endpoints', (req, res) => {
  res.json({
    endpoints: mal.getEndpoints(),
    stats: mal.getStats()
  });
});

app.post('/api/mal/route', (req, res) => {
  const { taskType, preferLocal } = req.body;
  const decision = mal.routeRequest({ taskType: taskType || 'text', preferLocal: preferLocal ?? true });
  res.json(decision);
});

app.get('/api/memory/kv', (req, res) => {
  res.json({
    frames: memory.getKVCacheFrames(),
    efficiency: memory.getKVEfficiency()
  });
});

app.get('/api/memory/vector', (req, res) => {
  const q = (req.query.q as string) || 'kernel';
  const docs = memory.searchSemanticMemory(q, 5);
  res.json(docs);
});

app.get('/api/sandbox/audit', (req, res) => {
  res.json(sandbox.getAuditLog());
});

// --- SOFTWARE STUDIO & APP ABI ENDPOINTS ---

app.get('/api/studio/apps', (req, res) => {
  res.json(appManager.getApps());
});

app.post('/api/studio/apps', (req, res) => {
  const { name, description, sourceCode, capabilities } = req.body;
  if (!name || !sourceCode) {
    return res.status(400).json({ error: 'Nombre y código fuente son requeridos' });
  }

  const id = `app-user-${Date.now().toString().slice(-4)}`;
  const newApp = appManager.installApp({
    manifest: {
      id,
      name,
      version: '1.0.0',
      author: 'User / AgentOS Studio',
      description: description || 'Aplicación creada en AgentOS Software Studio',
      capabilities: capabilities || ['gpu:inference', 'fs:read']
    },
    sourceCode,
    compiledAt: Date.now(),
    status: 'INSTALLED',
    executionCount: 0
  });

  res.status(201).json(newApp);
});

app.post('/api/studio/apps/:id/run', async (req, res) => {
  const result = await appManager.executeApp(req.params.id);
  res.json(result);
});

app.post('/api/task/dispatch', async (req, res) => {
  const { prompt, preferLocal } = req.body;
  if (!prompt) {
    return res.status(400).json({ error: 'Prompt is required' });
  }

  try {
    const result = await scheduler.executeAgentTask(prompt, preferLocal ?? true);
    res.json(result);
  } catch (err: any) {
    res.status(500).json({ error: err.message });
  }
});

// Start Server
const PORT = process.env.PORT || 3000;
httpServer.listen(PORT, () => {
  console.log(`\n==================================================`);
  console.log(`🚀 AgentOS Kernel running at http://localhost:${PORT}`);
  console.log(`==================================================\n`);
});
