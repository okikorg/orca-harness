import { pageToMarkdown } from './markdown.js';

const pages = await fetch('./pages/index.json').then(response => {
  if (!response.ok) throw new Error(`Could not load page registry (${response.status})`);
  return response.json();
});

const nav = document.querySelector('#nav');
const article = document.querySelector('#article');
const outline = document.querySelector('#outline');
const copyTemplate = document.querySelector('#copy-icon');
const bodyCache = new Map();
const legacyRoutes = new Map([
  ['sdk/sdk-install', 'sdk-first-agent/sdk-install'],
  ['sdk/sdk-first-agent', 'sdk-first-agent/sdk-build-agent'],
  ['sdk/sdk-tools', 'sdk-first-agent/sdk-tools-policy'],
  ['sdk/sdk-runs', 'sdk-background-lifecycle/sdk-run-handle'],
  ['sdk/sdk-sessions', 'sdk-first-agent/sdk-persistence'],
  ['sdk/sdk-state', 'sdk-host-assembly/sdk-state-services'],
  ['sdk/sdk-long-runs', 'sdk-host-assembly/sdk-recovery'],
  ['sdk/sdk-errors', 'sdk-host-assembly/sdk-partial-outcomes'],
]);
let renderSequence = 0;

async function loadBody(page) {
  if (!bodyCache.has(page.id)) {
    bodyCache.set(page.id, fetch(`./pages/${page.id}.html`).then(response => {
      if (!response.ok) throw new Error(`Could not load ${page.id} (${response.status})`);
      return response.text();
    }));
  }
  return bodyCache.get(page.id);
}

function renderNav(activePage) {
  const groups = [...new Set(pages.map(page => page.group))];
  nav.innerHTML = groups.map(group =>
    `<div class="nav-group"><span class="nav-group-title">${group}</span>${pages
      .filter(page => page.group === group)
      .map(page => {
        const classes = [
          'nav-link',
          page.parent ? 'nav-link-child' : '',
          page.id === activePage.id ? 'active' : '',
          activePage.parent === page.id ? 'parent-active' : '',
        ].filter(Boolean).join(' ');
        const current = page.id === activePage.id ? ' aria-current="page"' : '';
        return `<a class="${classes}" href="#${page.id}"${current}>${page.label}</a>`;
      })
      .join('')}</div>`
  ).join('');
}

function addCopyButtons() {
  article.querySelectorAll('.terminal').forEach(block => {
    const button = document.createElement('button');
    button.className = 'copy';
    button.type = 'button';
    button.title = 'Copy command';
    button.setAttribute('aria-label', 'Copy command');
    button.append(copyTemplate.content.cloneNode(true));
    button.addEventListener('click', async () => {
      await navigator.clipboard.writeText(block.querySelector('pre').innerText.replace(/^\$\s?/gm, ''));
      button.classList.add('copied');
      setTimeout(() => button.classList.remove('copied'), 1100);
    });
    block.append(button);
  });
}

async function render() {
  const sequence = ++renderSequence;
  let route = location.hash.slice(1);
  const replacement = legacyRoutes.get(route);
  if (replacement) {
    route = replacement;
    history.replaceState(null, '', `#${replacement}`);
  }
  const pageId = route.split('/')[0] || 'start';
  const page = pages.find(candidate => candidate.id === pageId) || pages[0];
  const body = await loadBody(page);
  if (sequence !== renderSequence) return;

  document.title = `${page.label} — Orcacode`;
  renderNav(page);
  article.innerHTML = `<p class="kicker">${page.group}</p><h1 class="title">${page.title}</h1><p class="lede">${page.lede}</p><div class="meta"><span>EXTERNAL USER DOCUMENTATION</span><span>Updated from source · 2026-09-24</span><button class="markdown-copy" type="button">Copy Markdown</button></div>${body}`;
  const markdownButton = article.querySelector('.markdown-copy');
  markdownButton.addEventListener('click', async () => {
    await navigator.clipboard.writeText(pageToMarkdown(page, body));
    markdownButton.textContent = 'Markdown copied';
    markdownButton.classList.add('copied');
    setTimeout(() => {
      markdownButton.textContent = 'Copy Markdown';
      markdownButton.classList.remove('copied');
    }, 1400);
  });
  addCopyButtons();

  const sections = [...article.querySelectorAll('.section')];
  outline.innerHTML = sections.map(section =>
    `<a class="outline-link" href="#${page.id}/${section.id}">${section.querySelector('h2').textContent}</a>`
  ).join('');

  document.querySelector('.sidebar').classList.remove('open');
  document.body.classList.remove('nav-open');
  document.querySelector('[data-toggle-nav]').setAttribute('aria-expanded', 'false');
  requestAnimationFrame(() => {
    const anchor = location.hash.split('/')[1];
    if (anchor) document.getElementById(anchor)?.scrollIntoView();
    else window.scrollTo(0, 0);
  });
}

window.addEventListener('hashchange', () => void render());
await render();

const menu = document.querySelector('[data-toggle-nav]');
menu.addEventListener('click', () => {
  const sidebar = document.querySelector('.sidebar');
  const open = sidebar.classList.toggle('open');
  document.body.classList.toggle('nav-open', open);
  menu.setAttribute('aria-expanded', String(open));
});

const dialog = document.querySelector('#search-dialog');
const input = document.querySelector('#search-input');
const results = document.querySelector('#search-results');
let searchablePages;
let matches = [];
let selected = 0;

async function loadSearchIndex() {
  if (!searchablePages) {
    searchablePages = Promise.all(pages.map(async page => ({ ...page, body: await loadBody(page) })));
  }
  return searchablePages;
}

async function search(query = '') {
  const allPages = await loadSearchIndex();
  const needle = query.trim().toLowerCase();
  matches = allPages.filter(page => !needle ||
    `${page.label} ${page.title} ${page.lede} ${page.body.replace(/<[^>]+>/g, ' ')}`.toLowerCase().includes(needle)
  );
  selected = Math.min(selected, Math.max(matches.length - 1, 0));
  results.innerHTML = matches.length
    ? matches.map((page, index) => `<button type="button" class="search-result ${index === selected ? 'selected' : ''}" data-id="${page.id}" role="option"><b>${page.label}</b><span>${page.title}</span></button>`).join('')
    : '<p class="search-hint">No matching pages.</p>';
}

async function openSearch() {
  dialog.showModal();
  input.value = '';
  selected = 0;
  results.innerHTML = '<p class="search-hint">Loading pages…</p>';
  await search();
  input.focus();
}

document.querySelector('[data-open-search]').addEventListener('click', () => void openSearch());
input.addEventListener('input', () => {
  selected = 0;
  void search(input.value);
});
results.addEventListener('click', event => {
  const button = event.target.closest('[data-id]');
  if (button) {
    location.hash = button.dataset.id;
    dialog.close();
  }
});
window.addEventListener('keydown', event => {
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') {
    event.preventDefault();
    void openSearch();
  }
  if (!dialog.open) return;
  if (event.key === 'ArrowDown') {
    event.preventDefault();
    selected = Math.min(selected + 1, matches.length - 1);
    void search(input.value);
  }
  if (event.key === 'ArrowUp') {
    event.preventDefault();
    selected = Math.max(selected - 1, 0);
    void search(input.value);
  }
  if (event.key === 'Enter' && matches[selected]) {
    event.preventDefault();
    location.hash = matches[selected].id;
    dialog.close();
  }
});
