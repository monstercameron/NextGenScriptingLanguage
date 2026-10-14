'use strict';
const $ = (selector) => document.querySelector(selector);
const toast = (message) => { const el = $('#copy-status'); el.textContent = message; el.classList.add('show'); setTimeout(() => el.classList.remove('show'), 2200); };
document.querySelectorAll('[data-copy]').forEach(button => button.addEventListener('click', async () => {
  try { await navigator.clipboard.writeText(document.getElementById(button.dataset.copy).textContent); toast('Copied. Go make something.'); }
  catch { toast('Select the code and copy it with your keyboard.'); }
}));
const escapeHTML = value => value.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;').replaceAll('"', '&quot;');
function highlight(source) {
  return source.split('\n').map(line => {
    if (line.trimStart().startsWith('#')) return `<span class="comment">${escapeHTML(line)}</span>`;
    return line.split(/("(?:\\.|[^"\\])*")/g).map((part, i) => i % 2 ? `<span class="str">${escapeHTML(part)}</span>` : escapeHTML(part).replace(/\b(accept|as|with|default|run|pipe to|then|from|every|listen to|whenever|show|if|otherwise|read|save to|with arguments|and keep status)\b/g, '<span class="kw">$1</span>')).join('');
  }).join('\n');
}
let entries = [], selected = '', filter = 'all';
function selectExample(entry) {
  selected = entry.file;
  $('#example-file').textContent = entry.file;
  $('#example-title').textContent = entry.title;
  $('#example-description').textContent = entry.description;
  $('#example-source').innerHTML = highlight(entry.source);
  $('#example-command').textContent = entry.command;
  $('#example-download').href = entry.download;
  $('#example-github').href = 'https://github.com/monstercameron/NextGenScriptingLanguage/blob/main/' + entry.file;
  renderList();
}
function renderList() {
  const list = $('#example-list'); list.replaceChildren();
  entries.filter(entry => filter === 'all' || entry.category === filter).forEach(entry => {
    const button = document.createElement('button'); button.className = 'example-card' + (entry.file === selected ? ' active' : ''); button.setAttribute('aria-pressed', String(entry.file === selected));
    const tag = document.createElement('span'); tag.textContent = entry.category;
    const title = document.createElement('b'); title.textContent = entry.title;
    button.append(tag, title); button.addEventListener('click', () => selectExample(entry)); list.append(button);
  });
}
document.querySelectorAll('[data-filter]').forEach(button => button.addEventListener('click', () => {
  filter = button.dataset.filter;
  document.querySelectorAll('[data-filter]').forEach(item => {item.classList.toggle('active', item === button); item.setAttribute('aria-pressed', String(item === button));});
  const visible = entries.filter(entry => filter === 'all' || entry.category === filter);
  if (visible.length && !visible.some(entry => entry.file === selected)) selectExample(visible[0]); else renderList();
}));
fetch('assets/data/examples.json').then(response => {if(!response.ok) throw Error('Examples unavailable'); return response.json();}).then(data => { entries = data; const initial = entries.find(entry => filter === 'all' || entry.category === filter); if (initial) selectExample(initial); else renderList(); }).catch(() => { $('#example-list').textContent = 'Examples could not load. Browse the full collection on GitHub using the link above.'; });
const benchFiles = {first: 'process-inputs-v0.9.0-dev-macos-arm64.json', repeat: 'process-inputs-v0.9.0-dev-macos-arm64-repeat.json'};
let benchmarkRequest = 0;
async function renderBenchmark() {
  const request = ++benchmarkRequest, filename = benchFiles[$('#benchmark-run').value];
  try {
    const response = await fetch('assets/data/' + filename); if (!response.ok) throw Error('No benchmark data'); const data = await response.json(); if(request !== benchmarkRequest) return;
    const before = data.results.environment.baseline.p50_us, after = data.results.environment.candidate.p50_us;
    $('#bench-improvement').textContent = Math.round((1-after/before)*100) + '%';
    $('#raw-benchmark').href = 'assets/data/' + filename;
    for (const [key, selector] of [['environment', '#benchmark-bars'], ['literal_control', '#control-bars']]) {
      const rows = data.results[key], maximum = Math.max(...Object.values(rows).map(row => row.p50_us)), host = $(selector); host.replaceChildren();
      for (const [name,label] of [['baseline','Execute · previous source'],['candidate','Execute · 0.9.0-dev'],['bash','Bash · equivalent native output']]) {
        const row = document.createElement('div'); row.className = 'bar-row ' + name;
        const top = document.createElement('div'); top.className = 'bar-top'; const text = document.createElement('span'); text.textContent = label; const value = document.createElement('b'); value.textContent = (rows[name].p50_us/1000).toFixed(2) + ' ms'; top.append(text,value);
        const track = document.createElement('div'); track.className = 'bar-track'; const bar = document.createElement('div'); bar.className = 'bar-fill'; bar.style.width = (rows[name].p50_us/maximum*100) + '%'; track.append(bar); row.append(top,track); host.append(row);
      }
    }
  } catch { $('#benchmark-bars').textContent = 'Chart unavailable. Use the raw sample link.'; }
}
$('#benchmark-run').addEventListener('change',renderBenchmark); renderBenchmark();
