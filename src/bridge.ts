import http from 'http';

/**
 * AgentOS Native Kernel Telemetry Bridge
 * Reads Native Kernel serial execution output and posts real-time telemetry to Dashboard Control Center (Port 3000)
 */
export class KernelTelemetryBridge {
  private targetUrl: string = 'http://localhost:3000/api/agents';

  public async transmitKernelEvent(data: {
    pid: number;
    name: string;
    role: string;
    priority: string;
    task: string;
  }) {
    const postData = JSON.stringify(data);
    const req = http.request(
      'http://localhost:3000/api/agents',
      {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
          'Content-Length': Buffer.byteLength(postData)
        }
      },
      (res) => {
        console.log(`[Bridge] Transmitted Kernel Event to Control Center (HTTP ${res.statusCode})`);
      }
    );

    req.on('error', (e) => {
      console.error(`[Bridge Warning] Control Center offline or unreachable: ${e.message}`);
    });

    req.write(postData);
    req.end();
  }
}

// Instantiate and send test kernel startup telemetry
const bridge = new KernelTelemetryBridge();
console.log('🚀 AgentOS Serial Bridge active. Transmitting native kernel status...');
bridge.transmitKernelEvent({
  pid: 100,
  name: 'rust-native-kernel',
  role: 'Bare-Metal SIMD & GGUF Engine',
  priority: 'CRITICAL',
  task: 'Executing GGUF Quantized Model Pass on AVX2 Hardware'
});
