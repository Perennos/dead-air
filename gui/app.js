'use strict';
(() => {
  const token = document.querySelector('meta[name="x-token"]').content;
  const $ = (id) => document.getElementById(id);
  const when = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'medium' });
  const MARKS = {
    ok: ['/brand/mark.svg', 'Stream healthy: colour-bar eyes', ''],
    incident: ['/brand/mark-dead.svg', 'Stream in alert: red eyes', ''],
    uncertain: ['/brand/mark.svg', 'Stream state uncertain: dimmed mark', 'is-dim'],
  };
  let flash = '';
  let running = false;
  let stopped = false;
  const OUTCOME = { sent: 'sent', failed: 'failed', pending: 'queued' };
  const HINTS = {
    ENDPOINT_UNAVAILABLE: 'the variable that holds this destination\'s URL is not set for this program',
    TOKEN_UNAVAILABLE: 'the variable that holds this destination\'s token is not set for this program',
    SMTP_CREDENTIALS_MISSING: 'the SMTP user or password variable is not set for this program',
  };

  const el = (tag, cls, text) => {
    const node = document.createElement(tag);
    if (cls) node.className = cls;
    if (text !== undefined) node.textContent = text;
    return node;
  };
  const stamp = (iso) => {
    const date = new Date(iso);
    const node = el('time', '', isNaN(date) ? String(iso) : when.format(date));
    if (!isNaN(date)) node.dateTime = iso;
    return node;
  };
  const post = (path, payload) => fetch(path, {
    method: 'POST', cache: 'no-store',
    headers: { 'Content-Type': 'application/json', 'X-Token': token },
    body: JSON.stringify(payload || {}),
  });
  const notice = (text, isError) => {
    const node = $('notice');
    if (node.textContent !== text) node.textContent = text;
    node.className = isError ? 'note is-error' : 'note';
    node.hidden = !text;
  };

  function card(s) {
    const [src, alt, dim] = MARKS[s.severity] || MARKS.uncertain;
    const article = el('article', 'card is-' + s.severity);
    article.setAttribute('aria-labelledby', 'name-' + s.id);
    const mark = el('img', dim ? 'mark ' + dim : 'mark');
    mark.src = src; mark.alt = alt; mark.width = 64; mark.height = 64;
    const name = el('h3', '', s.name);
    name.id = 'name-' + s.id;
    const who = el('div');
    who.append(name, el('p', 'host', s.host + ' · ' + s.source));
    const head = el('header', 'card-head');
    head.append(mark, who);
    article.append(head, el('p', 'status is-' + s.severity, s.status));
    if (s.confirming) article.append(el('p', 'confirm', 'Seen in the latest check; not an incident yet.'));

    const facts = el('dl', 'facts');
    const fact = (label, ...nodes) => {
      const row = el('div');
      const value = el('dd');
      value.append(...nodes);
      row.append(el('dt', '', label), value);
      facts.append(row);
    };
    if (s.incidents.length) {
      const list = el('ul', 'plain');
      for (const item of s.incidents) {
        const li = el('li', '', item.kind + ' since ');
        li.append(stamp(item.since));
        list.append(li);
      }
      fact('Open incidents', list);
    } else {
      fact('Open incidents', 'None');
    }
    const alert = s.last_alert;
    if (alert) {
      const detail = alert.detail ? ' (' + alert.detail + ')' : '';
      const outcome = alert.outcome === 'pending' && alert.detail ? 'retrying' : (OUTCOME[alert.outcome] || alert.outcome);
      const nodes = [alert.destination + ', ' + outcome + detail];
      if (alert.at) nodes.push(', ', stamp(alert.at));
      fact('Last alert', ...nodes);
    } else {
      fact('Last alert', 'None yet');
    }
    fact('Last checked', s.last_checked ? stamp(s.last_checked) : 'Not yet');
    article.append(facts);
    return article;
  }

  function render(s) {
    const count = s.stream_count + (s.stream_count === 1 ? ' stream' : ' streams');
    const cycle = s.last_cycle ? 'last cycle ' + when.format(new Date(s.last_cycle)) : 'no cycle yet';
    const w = s.watcher;
    running = w.running;
    const since = running ? new Date(w.started_at) : null;
    let lead = count + ', not watching';
    if (running) lead = 'watching ' + count + ' since ' + new Intl.DateTimeFormat(undefined, { timeStyle: 'short' }).format(since);
    else if (s.watching) lead = 'watching ' + count + ', started elsewhere';
    $('band-stats').textContent = lead + ' · ' + cycle;
    $('watch-toggle').textContent = running ? 'Stop watching' : 'Start watching';
    if (flash) notice(flash, true);
    else if (!running && w.last_line && w.last_line.error) notice('The watcher stopped: ' + w.last_line.error, true);
    else if (s.config_missing) notice('No config file at ' + s.config_path + '. Adding a stream below creates it.', false);
    else if (s.config_error) notice('The config at ' + s.config_path + ' could not be loaded: ' + s.config_error, true);
    else if (s.state_error) notice('The state file could not be read (' + s.state_error + '). Statuses may be out of date.', true);
    else notice('', false);
    $('alerts-line').textContent = s.destinations.length
      ? 'Alerts go to ' + s.destinations.map((d) => d.id + ' (' + d.type + ')').join(', ') + '.'
      : 'No alert destinations are configured. Add a [[destinations]] block to your config, as dead-air.example.toml shows.';
    $('streams').replaceChildren(...s.streams.map(card));
    $('empty').hidden = s.streams.length > 0 || Boolean(s.config_error);
  }

  async function refresh() {
    try {
      const response = await fetch('/api/state', { cache: 'no-store' });
      if (!response.ok) throw new Error(String(response.status));
      render(await response.json());
    } catch (err) {
      notice('Lost contact with Dead Air. Is the program still running? Retrying.', true);
    }
  }

  function result(r) {
    const li = el('li', '', r.id + ' (' + r.type + '): ');
    li.append(el('span', r.ok ? 'ok' : 'bad', r.ok ? 'sent' : 'failed'));
    if (!r.ok) li.append(' - ' + r.detail + (HINTS[r.detail] ? ': ' + HINTS[r.detail] : ''));
    return li;
  }

  $('test-alerts').addEventListener('click', async (event) => {
    const button = event.currentTarget;
    const list = $('alert-results');
    button.disabled = true;
    button.setAttribute('aria-busy', 'true');
    list.replaceChildren(el('li', '', 'Sending a test alert to each destination.'));
    try {
      const response = await post('/api/test-alerts');
      const body = await response.json();
      if (!response.ok) throw new Error(body.error || 'The test could not run.');
      list.replaceChildren(...(body.results.length
        ? body.results.map(result)
        : [el('li', '', 'No alert destinations are configured, so nothing was sent.')]));
    } catch (err) {
      list.replaceChildren(el('li', '', 'The test did not complete: ' + err.message));
    } finally {
      button.disabled = false;
      button.removeAttribute('aria-busy');
    }
  });

  const message = (text, isError) => {
    const node = $('form-msg');
    node.textContent = text;
    node.className = isError ? 'msg is-error' : 'msg';
  };
  $('watch-toggle').addEventListener('click', async (event) => {
    const button = event.currentTarget;
    button.disabled = true;
    button.setAttribute('aria-busy', 'true');
    try {
      const response = await post('/api/watch', { action: running ? 'stop' : 'start' });
      const body = await response.json();
      flash = response.ok ? '' : (body.error || 'The watcher could not change state.');
    } catch (err) {
      flash = 'Could not reach Dead Air. Is the program still running?';
    } finally {
      button.disabled = false;
      button.removeAttribute('aria-busy');
    }
    await refresh();
  });

  $('quit').addEventListener('click', async () => {
    stopped = true;
    try { await post('/api/quit'); } catch (err) { /* the server may close first */ }
    document.body.replaceChildren(el('p', 'note', 'Dead Air has stopped. You can close this tab.'));
  });

  $('add-form').addEventListener('submit', async (event) => {
    event.preventDefault();
    const form = event.currentTarget;
    const button = $('add-submit');
    const name = $('stream-name').value;
    button.disabled = true;
    button.setAttribute('aria-busy', 'true');
    message('', false);
    $('form-ok').textContent = '';
    try {
      const response = await post('/api/streams', { name, url: $('stream-url').value, source: $('stream-source').value });
      const body = await response.json();
      if (response.ok) {
        form.reset();
        $('form-ok').textContent = 'Added ' + name.trim() + '.';
        await refresh();
        $('stream-name').focus();
      } else {
        message(body.error || 'The stream could not be added.', true);
      }
    } catch (err) {
      message('Could not reach Dead Air. Is the program still running?', true);
    } finally {
      button.disabled = false;
      button.removeAttribute('aria-busy');
    }
  });

  setInterval(() => { if (!document.hidden && !stopped) refresh(); }, 15000);
  document.addEventListener('visibilitychange', () => { if (!document.hidden && !stopped) refresh(); });
  refresh();
})();
