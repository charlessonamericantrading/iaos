/**
 * AgentOS Software Development Kit (SDK)
 * Official SDK for building AI-native software applications and agents on AgentOS
 */

export interface PredictOptions {
  taskType?: 'text' | 'code' | 'vision' | 'tool_use';
  preferLocal?: boolean;
  maxLatencyMs?: number;
}

export interface AppManifest {
  id: string;
  name: string;
  version: string;
  author: string;
  description: string;
  capabilities: string[];
}

export class AgentOSSDK {
  public manifest: AppManifest;

  constructor(manifest: AppManifest) {
    this.manifest = manifest;
  }

  // --- Model Abstraction Layer (MAL) APIs ---
  public async predict(prompt: string, options?: PredictOptions): Promise<{ text: string; tokensUsed: number; endpoint: string }> {
    const res = await fetch('/api/task/dispatch', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ prompt, preferLocal: options?.preferLocal ?? true })
    });
    const data = (await res.json()) as any;
    return {
      text: data.response || '',
      tokensUsed: data.tokensProcessed || 100,
      endpoint: data.route?.selectedEndpoint?.name || 'Local NPU Engine'
    };
  }

  // --- Context Memory (UCM) APIs ---
  public async searchMemory(query: string): Promise<any[]> {
    const res = await fetch(`/api/memory/vector?q=${encodeURIComponent(query)}`);
    return (await res.json()) as any[];
  }

  // --- Process & Agent Management APIs ---
  public async spawnAgent(name: string, role: string, task?: string): Promise<any> {
    const res = await fetch('/api/agents', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name, role, priority: 'NORMAL', task })
    });
    return (await res.json()) as any;
  }

  // --- I/O & Tool Execution APIs ---
  public log(message: string) {
    console.log(`[App: ${this.manifest.name}] ${message}`);
  }
}
