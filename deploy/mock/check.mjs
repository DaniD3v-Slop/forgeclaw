import { createHmac, randomUUID } from 'node:crypto';

const forge = 'http://forgejo:3000/api/v1';
const daemon = 'http://forgeclaw-mock:3080';
const model = 'http://mock-model:3000';
const token = process.env.FORGECLAW_FORGE_TOKEN;
const password = process.env.FORGECLAW_FORGE_PASSWORD;
const secret = process.env.FORGECLAW_WEBHOOK_SECRET;
const toolAuthorization = process.env.FORGECLAW_MCP_AUTHORIZATION;
if (!token || !password || !secret || !toolAuthorization) throw new Error('mock check requires ForgeClaw test credentials');

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
const calls = async () => (await json(`${model}/healthz`)).calls;
const basic = (name) => ({ authorization: `Basic ${Buffer.from(`${name}:${password}`).toString('base64')}`, 'content-type': 'application/json' });

async function sendWebhook(payload) {
  const body = JSON.stringify(payload);
  const signature = createHmac('sha256', secret).update(body).digest('hex');
  const response = await fetch(`${daemon}/webhook`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'x-forgejo-signature': signature },
    body,
  });
  if (response.status !== 202) throw new Error(`signed webhook returned HTTP ${response.status}`);
}

async function waitForCompletion(before, path, sawCook = false) {
  for (let attempt = 0; attempt < 300; attempt++) {
    const reactions = await api(path);
    const names = new Set((reactions ?? []).map((reaction) => reaction.content));
    if (names.has('🧑‍🍳')) sawCook = true;
    if (names.has('🍳') && !names.has('🧑‍🍳') && await calls() > before) {
      if (!sawCook) throw new Error('running reaction was not observed');
      return;
    }
    await delay(200);
  }
  throw new Error(`ForgeClaw turn did not complete for ${path}`);
}

async function waitForRunning(path) {
  for (let attempt = 0; attempt < 50; attempt++) {
    const reactions = await api(path);
    if ((reactions ?? []).some((reaction) => reaction.content === '🧑‍🍳')) return;
    await delay(50);
  }
  throw new Error(`running reaction was not observed for ${path}`);
}

async function toolCall(session, name, arguments_) {
  return fetch(`${daemon}/tools/call`, {
    method: 'POST',
    headers: {
      authorization: toolAuthorization,
      'content-type': 'application/json',
      'x-forgeclaw-session-key': session,
    },
    body: JSON.stringify({ name, arguments: arguments_ }),
  });
}

const me = await api('/user');
const name = `forgeclaw-mock-${randomUUID().slice(0, 8)}`;
const repo = `${me.login}/${name}`;
await api('/user/repos', 'POST', { name, private: false, auto_init: true });
let staleToken;
try {
  const issue = await api(`/repos/${repo}/issues`, 'POST', { title: 'Mock OpenClaw turn', body: 'Verify the webhook reaction lifecycle.' });
  const other = await api(`/repos/${repo}/issues`, 'POST', { title: 'Unrelated issue', body: 'Must remain untouched.' });
  const comment = await api(`/repos/${repo}/issues/${issue.number}/comments`, 'POST', { body: `Please check this, @${me.login}.` });
  const staleName = `agent:main:forgejo/${repo}#issue/${issue.number}`;
  staleToken = await json(`${forge}/users/${me.login}/tokens`, {
    method: 'POST', headers: basic(me.login),
    body: JSON.stringify({ name: staleName, scopes: ['write:repository', 'write:issue', 'read:user'] }),
  });
  const before = await calls();
  await sendWebhook({
    action: 'created',
    repository: { full_name: repo },
    issue: { number: issue.number, title: issue.title, body: issue.body, assignees: [] },
    comment: { id: comment.id, body: comment.body, user: { login: 'mock-requester' } },
  });
  await waitForRunning(`/repos/${repo}/issues/comments/${comment.id}/reactions`);
  const allowed = await toolCall(staleName, 'forge_comment', { subject: `${repo}#issue/${issue.number}`, body: 'Local mock write proof.' });
  if (!allowed.ok) throw new Error(`exact-subject forge_comment returned HTTP ${allowed.status}`);
  const denied = await toolCall(staleName, 'forge_comment', { subject: `${repo}#issue/${other.number}`, body: 'This must be rejected.' });
  if (denied.status !== 400) throw new Error(`cross-subject forge_comment returned HTTP ${denied.status}`);
  const ownComments = await api(`/repos/${repo}/issues/${issue.number}/comments`);
  const otherComments = await api(`/repos/${repo}/issues/${other.number}/comments`);
  if (!ownComments.some((item) => item.body === 'Local mock write proof.') || otherComments.length !== 0) {
    throw new Error('exact-subject grant wrote to the wrong Forgejo issue');
  }
  await waitForCompletion(before, `/repos/${repo}/issues/comments/${comment.id}/reactions`, true);
  const afterTurn = await toolCall(staleName, 'forge_comment', { subject: `${repo}#issue/${issue.number}`, body: 'The grant must be gone.' });
  if (afterTurn.status !== 400) throw new Error(`completed turn kept its write grant: HTTP ${afterTurn.status}`);
  console.log('PASS: stale token name, exact-subject write, cross-subject denial, and grant cleanup');

  const assigned = await api(`/repos/${repo}/issues`, 'POST', { title: 'Assigned work', body: 'Implement this request.' });
  const beforeAssignment = await calls();
  await sendWebhook({
    action: 'assigned', repository: { full_name: repo },
    issue: { number: assigned.number, title: assigned.title, body: assigned.body, user: { login: 'mock-requester' }, assignees: [{ login: me.login }] },
  });
  await waitForCompletion(beforeAssignment, `/repos/${repo}/issues/${assigned.number}/reactions`);
  console.log('PASS: assignment starts a turn without a mention');

  const assignedFollowup = await api(`/repos/${repo}/issues/${assigned.number}/comments`, 'POST', { body: 'Continue the assigned work.' });
  const beforeAssignedFollowup = await calls();
  await sendWebhook({
    action: 'created', repository: { full_name: repo },
    issue: { number: assigned.number, title: assigned.title, body: assigned.body, assignees: [{ login: me.login }] },
    comment: { id: assignedFollowup.id, body: assignedFollowup.body, user: { login: 'mock-requester' } },
  });
  await waitForCompletion(beforeAssignedFollowup, `/repos/${repo}/issues/comments/${assignedFollowup.id}/reactions`);
  console.log('PASS: assigned issue follow-up starts a turn without a mention');

  const ignored = await api(`/repos/${repo}/issues/${other.number}/comments`, 'POST', { body: 'No trigger here.' });
  const beforeIgnored = await calls();
  await sendWebhook({
    action: 'created', repository: { full_name: repo },
    issue: { number: other.number, title: other.title, body: other.body, assignees: [] },
    comment: { id: ignored.id, body: ignored.body, user: { login: 'mock-requester' } },
  });
  await delay(1000);
  if (await calls() !== beforeIgnored) throw new Error('unassigned, unmentioned comment started a turn');

  await api(`/repos/${repo}/branches`, 'POST', { old_branch_name: 'main', new_branch_name: 'mock-review' });
  await api(`/repos/${repo}/contents/review.txt`, 'POST', {
    branch: 'mock-review', message: 'Add review fixture', content: Buffer.from('Review this change.\n').toString('base64'),
  });
  const pr = await api(`/repos/${repo}/pulls`, 'POST', { title: 'Review fixture', body: 'Please review.', base: 'main', head: 'mock-review' });
  const beforePr = await calls();
  await sendWebhook({
    action: 'opened', repository: { full_name: repo },
    pull_request: { number: pr.number, title: pr.title, body: pr.body, user: { login: 'mock-requester' }, head: { ref: 'mock-review', repo: { owner: { login: 'mock-requester' } } } },
  });
  await waitForCompletion(beforePr, `/repos/${repo}/issues/${pr.number}/reactions`);
  console.log('PASS: newly opened PR starts a review turn');

  const beforeOwnPr = await calls();
  await sendWebhook({
    action: 'opened', repository: { full_name: repo },
    pull_request: { number: pr.number, title: pr.title, body: pr.body, user: { login: me.login }, head: { ref: 'mock-review', repo: { owner: { login: me.login } } } },
  });
  await delay(1000);
  if (await calls() !== beforeOwnPr) throw new Error('bot-authored PR started its own review turn');

  const followup = await api(`/repos/${repo}/issues/${pr.number}/comments`, 'POST', { body: 'Go ahead.' });
  const beforeFollowup = await calls();
  await sendWebhook({
    action: 'created', repository: { full_name: repo },
    pull_request: { number: pr.number, title: pr.title, body: pr.body, head: { ref: 'mock-review', repo: { owner: { login: me.login } } } },
    comment: { id: followup.id, body: followup.body, user: { login: 'mock-requester' } },
  });
  await waitForCompletion(beforeFollowup, `/repos/${repo}/issues/comments/${followup.id}/reactions`);
  console.log('PASS: follow-up on bot-owned PR starts a turn without a mention');
} finally {
  if (staleToken) await json(`${forge}/users/${me.login}/tokens/${staleToken.id}`, { method: 'DELETE', headers: basic(me.login) });
  await api(`/repos/${repo}`, 'DELETE');
}
