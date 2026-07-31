import { CapabilityGrant } from './types.js';

export interface ToolExecutionResult {
  success: boolean;
  toolName: string;
  output: any;
  executionTimeMs: number;
  permissionGranted: boolean;
  error?: string;
}

export class ToolSandbox {
  private auditLog: { timestamp: number; pid: number; tool: string; granted: boolean; args: any }[] = [];

  public verifyCapability(pid: number, agentCapabilities: CapabilityGrant[], requiredTool: string, scope?: string): boolean {
    const hasGrant = agentCapabilities.some(c => {
      if (c.tool !== '*' && c.tool !== requiredTool) return false;
      if (scope && c.scope !== '*' && !scope.startsWith(c.scope.replace('*', ''))) return false;
      return true;
    });

    this.auditLog.push({
      timestamp: Date.now(),
      pid,
      tool: requiredTool,
      granted: hasGrant,
      args: { scope }
    });

    return hasGrant;
  }

  public async executeTool(
    pid: number,
    capabilities: CapabilityGrant[],
    toolName: string,
    args: Record<string, any>
  ): Promise<ToolExecutionResult> {
    const startTime = Date.now();

    const allowed = this.verifyCapability(pid, capabilities, toolName, args.scope || '*');
    if (!allowed) {
      return {
        success: false,
        toolName,
        output: null,
        executionTimeMs: Date.now() - startTime,
        permissionGranted: false,
        error: `Permission Denied: Agent PID ${pid} lacks capability '${toolName}'`
      };
    }

    // Execute sandbox tools
    try {
      let output: any = null;
      switch (toolName) {
        case 'sys:info':
          output = {
            os: 'AgentOS v1.0 Kernel (x86_64 / ARM64)',
            nodeVersion: process.version,
            memory: process.memoryUsage(),
            uptime: process.uptime()
          };
          break;

        case 'fs:read':
          output = `[Sandbox Virtual FS] Read simulated content of ${args.path || '/var/log/agent.log'}: "System online. KV cache optimal."`;
          break;

        case 'net:fetch':
          output = {
            status: 200,
            url: args.url || 'https://api.agentos.internal/status',
            data: { status: 'healthy', latency: '4ms' }
          };
          break;

        case 'exec:code':
          // Controlled JS calculation
          const expression = args.code || '2 + 2';
          output = { expression, result: eval(expression) };
          break;

        default:
          output = `[Tool ${toolName}] Default output executed safely.`;
      }

      return {
        success: true,
        toolName,
        output,
        executionTimeMs: Date.now() - startTime,
        permissionGranted: true
      };
    } catch (err: any) {
      return {
        success: false,
        toolName,
        output: null,
        executionTimeMs: Date.now() - startTime,
        permissionGranted: true,
        error: err.message
      };
    }
  }

  public getAuditLog() {
    return this.auditLog.slice(-50);
  }
}
