import { ModelEndpoint, RouteDecision } from './types.js';

export class ModelAbstractionLayer {
  private endpoints: Map<string, ModelEndpoint> = new Map();
  private totalRoutingCost: number = 0;
  private totalTokensRouted: number = 0;

  constructor() {
    this.initializeEndpoints();
  }

  private initializeEndpoints() {
    // Local endpoints (NPU/GPU accelerated)
    this.registerEndpoint({
      id: 'local-ollama-llama3',
      name: 'Llama 3.2 8B (Local NPU/GPU)',
      provider: 'LOCAL',
      family: 'llama3.2',
      latencyMs: 18,
      costPer1kTokensUSD: 0.0,
      vramUsageMB: 4800,
      isAvailable: true,
      capabilities: ['text', 'code', 'tool_use']
    });

    this.registerEndpoint({
      id: 'local-gemma-2b',
      name: 'Gemma 2 2B (Ultra Fast Local)',
      provider: 'LOCAL',
      family: 'gemma2',
      latencyMs: 8,
      costPer1kTokensUSD: 0.0,
      vramUsageMB: 1800,
      isAvailable: true,
      capabilities: ['text', 'code']
    });

    // Cloud API endpoints
    this.registerEndpoint({
      id: 'cloud-gemini-2.5-flash',
      name: 'Google Gemini 2.5 Flash (API)',
      provider: 'CLOUD',
      family: 'gemini-2.5-flash',
      latencyMs: 140,
      costPer1kTokensUSD: 0.0001,
      vramUsageMB: 0,
      isAvailable: true,
      capabilities: ['text', 'vision', 'code', 'tool_use']
    });

    this.registerEndpoint({
      id: 'cloud-claude-3.5-sonnet',
      name: 'Anthropic Claude 3.5 Sonnet (API)',
      provider: 'CLOUD',
      family: 'claude-3.5-sonnet',
      latencyMs: 320,
      costPer1kTokensUSD: 0.003,
      vramUsageMB: 0,
      isAvailable: true,
      capabilities: ['text', 'vision', 'code', 'tool_use']
    });
  }

  public registerEndpoint(endpoint: ModelEndpoint) {
    this.endpoints.set(endpoint.id, endpoint);
  }

  public getEndpoints(): ModelEndpoint[] {
    return Array.from(this.endpoints.values());
  }

  /**
   * Intelligently select the optimal LLM endpoint based on task policy
   */
  public routeRequest(options: {
    taskType: 'text' | 'vision' | 'code' | 'tool_use';
    preferLocal?: boolean;
    maxLatencyMs?: number;
    requireVision?: boolean;
  }): RouteDecision {
    const available = Array.from(this.endpoints.values()).filter(e => e.isAvailable);

    let candidates = available;
    if (options.requireVision) {
      candidates = candidates.filter(e => e.capabilities.includes('vision'));
    }

    if (options.preferLocal) {
      const localCandidates = candidates.filter(e => e.provider === 'LOCAL');
      if (localCandidates.length > 0) {
        candidates = localCandidates;
      }
    }

    // Sort by latency & availability
    candidates.sort((a, b) => a.latencyMs - b.latencyMs);

    const selected = candidates[0] || available[0];
    const fallbackChain = available
      .filter(e => e.id !== selected.id)
      .map(e => e.id);

    const reason = selected.provider === 'LOCAL'
      ? `Ultra-low latency local NPU/GPU execution (${selected.latencyMs}ms, $0 cost)`
      : `High-capability Cloud API routing (${selected.name})`;

    return {
      selectedEndpoint: selected,
      fallbackChain,
      reason,
      estimatedCost: selected.costPer1kTokensUSD * 0.5,
      estimatedLatencyMs: selected.latencyMs
    };
  }

  public async executeInference(endpointId: string, prompt: string): Promise<{ text: string; tokensUsed: number; latencyMs: number }> {
    const endpoint = this.endpoints.get(endpointId) || Array.from(this.endpoints.values())[0];
    const startTime = Date.now();
    
    // Simulate real execution token stream & delay
    const simulatedLatency = endpoint.latencyMs + Math.floor(Math.random() * 20);
    await new Promise(res => setTimeout(res, Math.min(simulatedLatency, 150)));

    const tokensUsed = Math.floor(prompt.length / 4) + Math.floor(Math.random() * 80) + 40;
    this.totalTokensRouted += tokensUsed;
    this.totalRoutingCost += (tokensUsed / 1000) * endpoint.costPer1kTokensUSD;

    let text = `[AgentOS MAL Kernel - ${endpoint.name}]\nEjecutando respuesta optimizada para el prompt.\nToken throughput: ${(tokensUsed / (simulatedLatency / 1000)).toFixed(1)} tok/s.`;

    return {
      text,
      tokensUsed,
      latencyMs: Date.now() - startTime
    };
  }

  public getStats() {
    return {
      totalTokensRouted: this.totalTokensRouted,
      totalRoutingCostUSD: this.totalRoutingCost,
      activeEndpointsCount: this.endpoints.size
    };
  }
}
