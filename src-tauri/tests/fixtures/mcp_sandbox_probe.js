// 沙箱边界探针 MCP server（stdio / JSON-RPC 2.0 / NDJSON）。
//
// 只给 tests/mcp_sandbox.rs 用：工具直接做文件读写与 HTTPS 请求，用来验证
// AppContainer 容器里的**行为边界**（而不是断言配置值）。工具描述刻意保持中性，
// 避免被注入过滤器命中而改变工具面。
import fs from 'node:fs';
import https from 'node:https';
import readline from 'node:readline';

const TOOLS = [
  {
    name: 'read_file',
    description: 'Read a text file and return its contents.',
    inputSchema: {
      type: 'object',
      properties: { path: { type: 'string' } },
      required: ['path'],
    },
  },
  {
    name: 'write_file',
    description: 'Write text to a file at the given path.',
    inputSchema: {
      type: 'object',
      properties: { path: { type: 'string' }, text: { type: 'string' } },
      required: ['path', 'text'],
    },
  },
  {
    name: 'temp_dir',
    description: 'Return the TEMP directory this process runs with.',
    inputSchema: { type: 'object', properties: {} },
  },
  {
    name: 'fetch_url',
    description: 'Perform an HTTPS GET and return the HTTP status code.',
    inputSchema: {
      type: 'object',
      properties: { url: { type: 'string' } },
      required: ['url'],
    },
  },
];

function reply(id, result) {
  process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id, result }) + '\n');
}

function replyText(id, text, isError = false) {
  reply(id, { content: [{ type: 'text', text }], isError });
}

const rl = readline.createInterface({ input: process.stdin });

rl.on('line', (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  if (msg.id === undefined || msg.id === null) {
    return;
  }
  switch (msg.method) {
    case 'initialize':
      reply(msg.id, {
        protocolVersion: '2025-06-18',
        capabilities: { tools: {} },
        serverInfo: { name: 'sandbox-probe', version: '1.0.0' },
      });
      break;
    case 'tools/list':
      reply(msg.id, { tools: TOOLS });
      break;
    case 'tools/call':
      callTool(msg.id, msg.params ?? {});
      break;
    default:
      process.stdout.write(
        JSON.stringify({
          jsonrpc: '2.0',
          id: msg.id,
          error: { code: -32601, message: `Method not found: ${msg.method}` },
        }) + '\n',
      );
  }
});

function callTool(id, params) {
  const name = params.name;
  const args = params.arguments ?? {};
  try {
    switch (name) {
      case 'read_file':
        replyText(id, fs.readFileSync(args.path, 'utf8'));
        return;
      case 'write_file':
        // 不自动建父目录：父目录不存在也应当如实报错，而不是替调用方绕开边界。
        fs.writeFileSync(args.path, args.text, 'utf8');
        replyText(id, 'written');
        return;
      case 'temp_dir':
        replyText(id, process.env.TEMP ?? '<unset>');
        return;
      case 'fetch_url': {
        const request = https.get(args.url, { timeout: 10000 }, (response) => {
          response.resume();
          replyText(id, `status=${response.statusCode}`);
        });
        request.on('timeout', () => request.destroy(new Error('timeout')));
        request.on('error', (error) => replyText(id, `ERROR ${error.code ?? error.message}`, true));
        return;
      }
      default:
        replyText(id, `ERROR unknown tool: ${name}`, true);
    }
  } catch (error) {
    replyText(id, `ERROR ${error.code ?? error.message}`, true);
  }
}
