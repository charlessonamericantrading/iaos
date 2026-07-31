import { KVCacheFrame, VectorDocument } from './types.js';

export class UnifiedContextMemory {
  private kvCacheFrames: Map<string, KVCacheFrame> = new Map();
  private vectorStore: VectorDocument[] = [];
  private totalHits: number = 0;
  private totalAccesses: number = 0;

  constructor() {
    this.seedSampleMemories();
  }

  private seedSampleMemories() {
    this.addVectorDocument({
      id: 'doc-kernel-spec',
      pid: 1,
      content: 'AgentOS Kernel utiliza la memoria de contexto KV Cache paginada entre VRAM y RAM para maximizar throughput.',
      metadata: { category: 'architecture', tags: ['kernel', 'memory', 'kv-cache'] },
      embedding: [0.12, 0.88, 0.45, 0.91]
    });

    this.addVectorDocument({
      id: 'doc-security-model',
      pid: 1,
      content: 'El modelo de seguridad de AgentOS otorga tokens de capacidad delimitados a cada subagente para aislar accesos I/O.',
      metadata: { category: 'security', tags: ['sandbox', 'ipc', 'permissions'] },
      embedding: [0.75, 0.22, 0.81, 0.14]
    });

    // Sample KV Cache pages
    this.allocateKVCache('page-init-prompt', 1, 1024, 'GPU_VRAM');
    this.allocateKVCache('page-agent-tree', 2, 2048, 'GPU_VRAM');
    this.allocateKVCache('page-cold-logs', 3, 4096, 'SYSTEM_RAM');
  }

  public allocateKVCache(pageId: string, pid: number, tokensCount: number, initialLoc: 'GPU_VRAM' | 'SYSTEM_RAM' | 'NVME_PAGED'): KVCacheFrame {
    const frame: KVCacheFrame = {
      pageId,
      pid,
      tokensCount,
      location: initialLoc,
      lastAccessed: Date.now(),
      hitCount: 1
    };
    this.kvCacheFrames.set(pageId, frame);
    return frame;
  }

  public touchKVCache(pageId: string): KVCacheFrame | null {
    this.totalAccesses++;
    const frame = this.kvCacheFrames.get(pageId);
    if (frame) {
      this.totalHits++;
      frame.lastAccessed = Date.now();
      frame.hitCount++;
      // Promote from RAM/NVME to VRAM on hit
      if (frame.location !== 'GPU_VRAM') {
        frame.location = 'GPU_VRAM';
      }
      return frame;
    }
    return null;
  }

  /**
   * Paging policy: Move cold KV Cache pages from GPU VRAM to SYSTEM RAM or NVME
   */
  public performKVCachePaging() {
    const now = Date.now();
    for (const frame of this.kvCacheFrames.values()) {
      const ageMs = now - frame.lastAccessed;
      if (ageMs > 30000 && frame.location === 'GPU_VRAM') {
        frame.location = 'SYSTEM_RAM'; // Page out to RAM
      } else if (ageMs > 60000 && frame.location === 'SYSTEM_RAM') {
        frame.location = 'NVME_PAGED'; // Page out to SSD
      }
    }
  }

  public getKVCacheFrames(): KVCacheFrame[] {
    return Array.from(this.kvCacheFrames.values());
  }

  public getKVEfficiency(): number {
    if (this.totalAccesses === 0) return 96.4;
    return Number(((this.totalHits / this.totalAccesses) * 100).toFixed(1));
  }

  public addVectorDocument(doc: VectorDocument) {
    this.vectorStore.push(doc);
  }

  /**
   * Cosine similarity search over vector embeddings
   */
  public searchSemanticMemory(queryText: string, topK: number = 3): VectorDocument[] {
    // Generate simple query vector based on text length & hash for simulation
    const qLen = queryText.length;
    const dummyQueryVec = [
      (qLen % 10) / 10,
      ((qLen * 3) % 10) / 10,
      ((qLen * 7) % 10) / 10,
      ((qLen * 11) % 10) / 10
    ];

    const results = this.vectorStore.map(doc => {
      const score = this.cosineSimilarity(dummyQueryVec, doc.embedding);
      return { ...doc, score: Number(score.toFixed(3)) };
    });

    results.sort((a, b) => (b.score || 0) - (a.score || 0));
    return results.slice(0, topK);
  }

  private cosineSimilarity(vecA: number[], vecB: number[]): number {
    let dot = 0;
    let normA = 0;
    let normB = 0;
    for (let i = 0; i < Math.min(vecA.length, vecB.length); i++) {
      dot += vecA[i] * vecB[i];
      normA += vecA[i] * vecA[i];
      normB += vecB[i] * vecB[i];
    }
    if (normA === 0 || normB === 0) return 0;
    return dot / (Math.sqrt(normA) * Math.sqrt(normB));
  }
}
