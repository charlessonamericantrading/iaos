export type ProcessState = 'READY' | 'RUNNING' | 'BLOCKED' | 'SUSPENDED' | 'TERMINATED';

export type ProcessPriority = 'CRITICAL' | 'HIGH' | 'NORMAL' | 'BACKGROUND';

export interface CapabilityGrant {
  tool: string;
  scope: string; // e.g. 'fs:read:/logs', 'net:outbound:api.google.com', 'exec:js'
  expiresAt?: number;
}

export interface AgentProcess {
  pid: number;
  name: string;
  role: string;
  state: ProcessState;
  priority: ProcessPriority;
  tokensUsed: number;
  maxTokens: number;
  parentPid: number | null;
  childPids: number[];
  capabilities: CapabilityGrant[];
  currentTask: string | null;
  createdAt: number;
  lastActive: number;
  logs: string[];
}

export type ProviderType = 'LOCAL' | 'CLOUD';

export interface ModelEndpoint {
  id: string;
  name: string;
  provider: ProviderType;
  family: string; // e.g., 'llama3.2', 'gemini-1.5', 'claude-3-5-sonnet'
  latencyMs: number;
  costPer1kTokensUSD: number;
  vramUsageMB: number;
  isAvailable: boolean;
  capabilities: ('text' | 'vision' | 'code' | 'tool_use')[];
}

export interface RouteDecision {
  selectedEndpoint: ModelEndpoint;
  fallbackChain: string[];
  reason: string;
  estimatedCost: number;
  estimatedLatencyMs: number;
}

export interface KVCacheFrame {
  pageId: string;
  pid: number;
  tokensCount: number;
  location: 'GPU_VRAM' | 'SYSTEM_RAM' | 'NVME_PAGED';
  lastAccessed: number;
  hitCount: number;
}

export interface VectorDocument {
  id: string;
  pid: number;
  content: string;
  metadata: Record<string, any>;
  embedding: number[]; // Simulated vector dimension
  score?: number;
}

export interface SystemMetrics {
  uptimeSeconds: number;
  activeAgents: number;
  totalTokensProcessed: number;
  kvCacheEfficiencyPercent: number;
  vramUsedMB: number;
  vramTotalMB: number;
  ramUsedMB: number;
  localVsCloudRatio: { local: number; cloud: number };
  throughputTokensPerSec: number;
}
