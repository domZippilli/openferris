const form = document.querySelector('#composer');
const input = document.querySelector('#input');
const messages = document.querySelector('#messages');
const status = document.querySelector('#status');
const button = form.querySelector('button');

function addMessage(role, text = '') {
  document.querySelector('.welcome')?.remove();
  const el = document.createElement('div');
  el.className = `message ${role}`;
  el.textContent = text;
  messages.append(el);
  el.scrollIntoView({ behavior: 'smooth', block: 'end' });
  return el;
}

async function checkHealth() {
  try { status.textContent = (await fetch('/api/health')).ok ? 'Assistant online' : 'Daemon offline'; }
  catch { status.textContent = 'Service offline'; }
}

form.addEventListener('submit', async (event) => {
  event.preventDefault();
  const message = input.value.trim();
  if (!message || button.disabled) return;
  addMessage('user', message);
  input.value = ''; input.style.height = 'auto'; button.disabled = true;
  const assistant = addMessage('assistant');
  const progress = document.createElement('div');
  progress.className = 'progress'; progress.textContent = 'Thinking…'; messages.append(progress);

  try {
    const response = await fetch('/api/chat', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ message }) });
    if (!response.ok) throw new Error(await response.text() || `Request failed (${response.status})`);
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let pending = '', streamed = '';
    while (true) {
      const { value, done } = await reader.read();
      pending += decoder.decode(value || new Uint8Array(), { stream: !done });
      const lines = pending.split('\n'); pending = lines.pop();
      for (const line of lines) {
        if (!line.trim()) continue;
        const payload = JSON.parse(line).kind;
        if (payload.Progress) progress.textContent = payload.Progress.text;
        if (payload.AssistantChunk) { streamed += payload.AssistantChunk.text; assistant.textContent = streamed; }
        if (payload.Done) assistant.textContent = payload.Done.text;
        if (payload.Error) throw new Error(payload.Error.message);
        assistant.scrollIntoView({ behavior: 'smooth', block: 'end' });
      }
      if (done) break;
    }
  } catch (error) {
    assistant.classList.add('error'); assistant.textContent = `Error: ${error.message}`;
  } finally {
    progress.remove(); button.disabled = false; input.focus(); checkHealth();
  }
});

input.addEventListener('keydown', (event) => {
  if (event.key === 'Enter' && !event.shiftKey) { event.preventDefault(); form.requestSubmit(); }
});
input.addEventListener('input', () => { input.style.height = 'auto'; input.style.height = `${input.scrollHeight}px`; });
checkHealth(); input.focus();
