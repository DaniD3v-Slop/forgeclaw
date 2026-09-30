import http from 'node:http';

let calls = 0;
const reply = 'Mock agent completed the ForgeClaw turn.';
const server = http.createServer(async (request, response) => {
  if (request.method === 'GET' && request.url === '/healthz') {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ calls }));
    return;
  }
  if (request.method === 'GET' && request.url === '/v1/models') {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ object: 'list', data: [{ id: 'agent', object: 'model' }] }));
    return;
  }
  if (request.method !== 'POST' || request.url !== '/v1/chat/completions') {
    response.writeHead(404).end();
    return;
  }
  let body = '';
  for await (const chunk of request) body += chunk;
  const input = JSON.parse(body);
  calls += 1;
  await new Promise((resolve) => setTimeout(resolve, 2500));
  if (!input.stream) {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ id: `mock-${calls}`, object: 'chat.completion', created: 0, model: 'agent', choices: [{ index: 0, message: { role: 'assistant', content: reply }, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 8, total_tokens: 9 } }));
    return;
  }
  response.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
  const chunk = (delta, finish_reason = null) => response.write(`data: ${JSON.stringify({ id: `mock-${calls}`, object: 'chat.completion.chunk', created: 0, model: 'agent', choices: [{ index: 0, delta, finish_reason }] })}\n\n`);
  chunk({ role: 'assistant' });
  chunk({ content: reply });
  chunk({}, 'stop');
  response.end('data: [DONE]\n\n');
});
server.listen(3000, '0.0.0.0');
