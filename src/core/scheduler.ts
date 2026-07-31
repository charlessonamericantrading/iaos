import { AgentProcess, ProcessPriority, CapabilityGrant } from './types.js';
import { ModelAbstractionLayer } from './mal.js';
import { UnifiedContextMemory } from './memory.js';
import { ToolSandbox } from './sandbox.js';

export class AgentScheduler {
  private processes: Map<number, AgentProcess> = new Map();
  private nextPid: number = 1;
  private mal: ModelAbstractionLayer;
  private memory: UnifiedContextMemory;
  private sandbox: ToolSandbox;
  private totalTokensProcessed: number = 0;
  private logListener?: (msg: any) => void;

  constructor(mal: ModelAbstractionLayer, memory: UnifiedContextMemory, sandbox: ToolSandbox) {
    this.mal = mal;
    this.memory = memory;
    this.sandbox = sandbox;
    this.initKernelProcesses();
  }

  public setLogListener(listener: (msg: any) => void) {
    this.logListener = listener;
  }

  private emitEvent(type: string, data: any) {
    if (this.logListener) {
      this.logListener({ type, data, timestamp: Date.now() });
    }
  }

  private initKernelProcesses() {
    // PID 1: Init & System Kernel Manager
    this.createProcess({
      name: 'systemd-ai',
      role: 'Init Kernel Supervisor',
      priority: 'CRITICAL',
      maxTokens: 500000,
      capabilities: [{ tool: '*', scope: '*' }]
    });

    // PID 2: Model Abstraction Router Daemon
    this.createProcess({
      name: 'mal-routerd',
      role: 'Local/Cloud Model Routing Daemon',
      priority: 'HIGH',
      maxTokens: 250000,
      capabilities: [{ tool: 'sys:info', scope: '*' }]
    });

    // PID 3: Memory & Vector Paging Daemon
    this.createProcess({
      name: 'ucm-memd',
      role: 'KV Cache & Semantic Memory Pager',
      priority: 'HIGH',
      maxTokens: 250000,
      capabilities: [{ tool: 'fs:read', scope: '*' }]
    });

    // PID 4: Code & Task Synthesis Agent
    this.createProcess({
      name: 'agent-coder-01',
      role: 'Autonomic Code Generator',
      priority: 'NORMAL',
      maxTokens: 100000,
      capabilities: [
        { tool: 'exec:code', scope: '*' },
        { tool: 'fs:read', scope: '*' },
        { tool: 'net:fetch', scope: '*' }
      ]
    });
  }

  public createProcess(params: {
    name: string;
    role: string;
    priority?: ProcessPriority;
    maxTokens?: number;
    parentPid?: number | null;
    capabilities?: CapabilityGrant[];
    task?: string;
  }): AgentProcess {
    const pid = this.nextPid++;
    const proc: AgentProcess = {
      pid,
      name: params.name,
      role: params.role,
      state: 'READY',
      priority: params.priority || 'NORMAL',
      tokensUsed: 0,
      maxTokens: params.maxTokens || 50000,
      parentPid: params.parentPid || null,
      childPids: [],
      capabilities: params.capabilities || [{ tool: 'sys:info', scope: '*' }],
      currentTask: params.task || null,
      createdAt: Date.now(),
      lastActive: Date.now(),
      logs: [`[PID ${pid}] Process spawned: ${params.name} (${params.role})`]
    };

    if (params.parentPid && this.processes.has(params.parentPid)) {
      this.processes.get(params.parentPid)!.childPids.push(pid);
    }

    this.processes.set(pid, proc);
    
    // Allocate KV Cache memory frame
    this.memory.allocateKVCache(`page-pid-${pid}`, pid, 512, 'GPU_VRAM');
    
    this.emitEvent('PROCESS_CREATED', proc);
    return proc;
  }

  public getProcesses(): AgentProcess[] {
    return Array.from(this.processes.values());
  }

  public getProcess(pid: number): AgentProcess | undefined {
    return this.processes.get(pid);
  }

  public terminateProcess(pid: number): boolean {
    const proc = this.processes.get(pid);
    if (!proc) return false;
    proc.state = 'TERMINATED';
    proc.logs.push(`[PID ${pid}] Process terminated by Kernel`);
    this.emitEvent('PROCESS_TERMINATED', proc);
    return true;
  }

  /**
   * Execute an asynchronous task using the AgentOS Kernel pipeline
   */
  public async executeAgentTask(taskPrompt: string, preferLocal: boolean = true): Promise<{
    agent: AgentProcess;
    subagent?: AgentProcess;
    response: string;
    route: any;
    toolResult?: any;
    tokensProcessed: number;
  }> {
    // Spawn user worker agent
    const agent = this.createProcess({
      name: `worker-task-${Date.now().toString().slice(-4)}`,
      role: 'Task Executor Agent',
      priority: 'HIGH',
      task: taskPrompt,
      capabilities: [
        { tool: 'exec:code', scope: '*' },
        { tool: 'sys:info', scope: '*' },
        { tool: 'fs:read', scope: '*' }
      ]
    });

    agent.state = 'RUNNING';
    agent.logs.push(`[Task Dispatch] Received user task: "${taskPrompt}"`);
    this.emitEvent('AGENT_LOG', { pid: agent.pid, log: `Iniciando tarea: ${taskPrompt}` });

    // Step 1: Consult Semantic Memory
    const relevantDocs = this.memory.searchSemanticMemory(taskPrompt, 2);
    agent.logs.push(`[UCM Memory] Retrieved ${relevantDocs.length} relevant vector context documents.`);

    // Step 2: Route request through Model Abstraction Layer (MAL)
    const route = this.mal.routeRequest({
      taskType: taskPrompt.includes('código') || taskPrompt.includes('code') ? 'code' : 'text',
      preferLocal
    });
    agent.logs.push(`[MAL Router] Selected endpoint: ${route.selectedEndpoint.name} (${route.reason})`);

    // Step 3: Spawn a subagent if task is complex
    let subagent: AgentProcess | undefined;
    if (taskPrompt.length > 20) {
      subagent = this.createProcess({
        name: `subagent-calc-${agent.pid}`,
        role: 'Subagent Helper Node',
        parentPid: agent.pid,
        task: `Subtask calculations for PID ${agent.pid}`
      });
      subagent.state = 'RUNNING';
      subagent.logs.push(`[Subagent] Spawned by parent PID ${agent.pid}`);
    }

    // Step 4: Execute LLM inference
    const llmRes = await this.mal.executeInference(route.selectedEndpoint.id, taskPrompt);
    agent.tokensUsed += llmRes.tokensUsed;
    this.totalTokensProcessed += llmRes.tokensUsed;

    // Step 5: Execute safe tool if requested in prompt
    let toolResult: any = undefined;
    if (taskPrompt.toLowerCase().includes('info') || taskPrompt.toLowerCase().includes('sistema') || taskPrompt.toLowerCase().includes('calc')) {
      const toolToRun = taskPrompt.includes('calc') ? 'exec:code' : 'sys:info';
      toolResult = await this.sandbox.executeTool(
        agent.pid,
        agent.capabilities,
        toolToRun,
        { code: 'Math.pow(2, 10) + 42' }
      );
      agent.logs.push(`[Tool Sandbox] Executed '${toolToRun}' -> ${JSON.stringify(toolResult.output)}`);
    }

    agent.state = 'READY';
    agent.lastActive = Date.now();
    if (subagent) {
      subagent.state = 'TERMINATED';
    }

    const finalResponse = `${llmRes.text}\n\n[Kernel Metrics]\n- Endpoint: ${route.selectedEndpoint.name}\n- Latencia: ${llmRes.latencyMs}ms\n- Tokens procesados: ${llmRes.tokensUsed}\n${toolResult ? `- Resultado de Herramienta (${toolResult.toolName}): ${JSON.stringify(toolResult.output)}` : ''}`;

    this.emitEvent('TASK_COMPLETE', { pid: agent.pid, response: finalResponse });
    return {
      agent,
      subagent,
      response: finalResponse,
      route,
      toolResult,
      tokensProcessed: llmRes.tokensUsed
    };
  }

  public getTotalTokensProcessed() {
    return this.totalTokensProcessed;
  }
}
