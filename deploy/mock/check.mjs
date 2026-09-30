import { createHmac, randomUUID } from 'node:crypto';

const forge = 'http://forgejo:3000/api/v1';
const daemon = 'http://forgeclaw-mock:3080';
const model = 'http://mock-model:3000';
const token = process.env.FORGECLAW_FORGE_TOKEN;
const secret = process.env.FORGECLAW_WEBHOOK_SECRET;
if (!token || !secret) throw new Error('mock check requires FORGECLAW_FORGE_TOKEN and FORGECLAW_WEBHOOK_SECRET');

async function json(url, options = {}) {
  const response = await fetch(url, options);
  if (!response.ok) throw new Error(`${options.method || 'GET'} ${new URL(url).pathname}: HTTP ${response.status}`);
  return response.status === 204 ? undefined : response.json();
}
const api = (path, method = 'GET', body) => json(`${forge}${path}`, {
  method,
  headers: { authorization: `token ${token}`, ...(body ? { 'content-type': 'application/json' } : {}) },
  ...(body ? { body: JSON.stringify(body) } : {}),
});
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const me = await api('/user');
const name = `forgeclaw-mock-${randomUUID().slice(0, 8)}`;
const repo = `${me.login}/${name}`;
await api('/user/repos', 'POST', { name, private: false, auto_init: true });
try {
  const issue = await api(`/repos/${repo}/issues`, 'POST', { title: 'Mock OpenClaw turn', body: 'Verify the webhook reaction lifecycle.' });
  const comment = await api(`/repos/${repo}/issues/${issue.number}/comments`, 'POST', { body: `Please check this, @${me.login}.` });
  const before = (await json(`${model}/healthz`)).calls;
  const payload = JSON.stringify({
    action: 'created',
    repository: { full_name: repo },
    issue: { number: issue.number, title: issue.title, body: issue.body, assignees: [] },
    comment: { id: comment.id, body: comment.body, user: { login: 'mock-requester' } },
  });
  const signature = createHmac('sha256', secret).update(payload).digest('hex');
  const response = await fetch(`${daemon}/webhook`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'x-forgejo-signature': signature },
    body: payload,
  });
  if (response.status !== 202) throw new Error(`signed webhook returned HTTP ${response.status}`);
  let sawCook = false;
  let completed = false;
  for (let attempt = 0; attempt < 300; attempt++) {
    const reactions = await api(`/repos/${repo}/issues/comments/${comment.id}/reactions`);
    const names = new Set((reactions ?? []).map((reaction) => reaction.content));
    if (names.has('🧑‍🍳')) sawCook = true;
    if (names.has('🍳') && !names.has('🧑‍🍳')) {
      completed = true;
      break;
    }
    await delay(200);
  }
  const after = (await json(`${model}/healthz`)).calls;
  if (!sawCook) throw new Error('running reaction was not observed');
  if (!completed) throw new Error('completed reaction did not replace running reaction');
  if (after <= before) throw new Error('OpenClaw did not call the mock model');
  console.log('PASS: signed webhook → outbox → OpenClaw mock model → 🧑‍🍳 → 🍳');
} finally {
  await api(`/repos/${repo}`, 'DELETE');
}
