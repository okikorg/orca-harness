function cleanInline(value) {
  return value
    .replace(/[\t\n\r ]+/g, ' ')
    .replace(/ +([,.;:!?])/g, '$1')
    .trim();
}

function escapeText(value) {
  return value.replace(/([\\`*_[\]])/g, '\\$1');
}

function inlineCode(value) {
  const text = value.trim();
  const longestRun = Math.max(0, ...[...text.matchAll(/`+/g)].map(match => match[0].length));
  const fence = '`'.repeat(longestRun + 1);
  const padding = text.startsWith('`') || text.endsWith('`') ? ' ' : '';
  return `${fence}${padding}${text}${padding}${fence}`;
}

function inline(node) {
  if (node.nodeType === 3) return escapeText(node.textContent);
  if (node.nodeType !== 1) return '';

  const tag = node.tagName.toLowerCase();
  const content = () => [...node.childNodes].map(inline).join('');
  if (tag === 'code') return inlineCode(node.textContent);
  if (tag === 'strong' || tag === 'b') return `**${cleanInline(content())}**`;
  if (tag === 'em' || tag === 'i') return `_${cleanInline(content())}_`;
  if (tag === 'a') return `[${cleanInline(content())}](${node.getAttribute('href') || ''})`;
  if (tag === 'br') return '\n';
  if (tag === 'sup') return `<sup>${cleanInline(content())}</sup>`;
  if (tag === 'button' || tag === 'svg') return '';
  return content();
}

function quote(value) {
  return value.trim().split('\n').map(line => `> ${line}`.trimEnd()).join('\n') + '\n\n';
}

function table(node) {
  const rows = [...node.querySelectorAll(':scope > thead > tr, :scope > tbody > tr')];
  if (!rows.length) return '';
  const values = rows.map(row => [...row.children].map(cell =>
    cleanInline([...cell.childNodes].map(inline).join('')).replace(/\|/g, '\\|') || ' '
  ));
  const columns = Math.max(...values.map(row => row.length));
  const padded = values.map(row => [...row, ...Array(columns - row.length).fill(' ')]);
  const header = padded[0];
  return [
    `| ${header.join(' | ')} |`,
    `| ${header.map(() => '---').join(' | ')} |`,
    ...padded.slice(1).map(row => `| ${row.join(' | ')} |`),
    '',
  ].join('\n') + '\n';
}

function list(node, ordered) {
  const items = [...node.children].filter(child => child.tagName?.toLowerCase() === 'li');
  return items.map((item, index) => {
    const marker = ordered ? `${index + 1}.` : '-';
    const text = cleanInline([...item.childNodes].map(child =>
      ['ul', 'ol'].includes(child.tagName?.toLowerCase()) ? '' : inline(child)
    ).join(''));
    const nested = [...item.children]
      .filter(child => ['ul', 'ol'].includes(child.tagName.toLowerCase()))
      .map(child => block(child).trim().split('\n').map(line => `  ${line}`).join('\n'))
      .join('\n');
    return `${marker} ${text}${nested ? `\n${nested}` : ''}`;
  }).join('\n') + '\n\n';
}

function callout(node) {
  const label = node.querySelector(':scope > .callout-label');
  const parts = [...node.children]
    .filter(child => child !== label)
    .map(block)
    .join('')
    .trim();
  const heading = label ? `**${cleanInline(label.textContent)}**${parts ? ' — ' : ''}` : '';
  return quote(`${heading}${parts}`);
}

function step(node) {
  const number = cleanInline(node.querySelector(':scope > .step-number')?.textContent || '');
  const content = node.querySelector(':scope > div');
  const heading = cleanInline(content?.querySelector(':scope > h3')?.textContent || '');
  const rest = content
    ? [...content.children].filter(child => child.tagName.toLowerCase() !== 'h3').map(block).join('').trim()
    : '';
  return `### ${number}${number && heading ? '. ' : ''}${heading}\n\n${rest}\n\n`;
}

function figure(node) {
  const caption = node.querySelector('figcaption');
  if (!caption) return '';
  return quote(`**Diagram:** ${cleanInline([...caption.childNodes].map(inline).join(''))}`);
}

function block(node) {
  if (node.nodeType === 3) return node.textContent.trim() ? `${cleanInline(node.textContent)}\n\n` : '';
  if (node.nodeType !== 1) return '';

  const tag = node.tagName.toLowerCase();
  if (tag === 'svg' || tag === 'button') return '';
  if (tag === 'h1') return `# ${cleanInline(inline(node))}\n\n`;
  if (tag === 'h2') return `## ${cleanInline(inline(node))}\n\n`;
  if (tag === 'h3') return `### ${cleanInline(inline(node))}\n\n`;
  if (tag === 'p') return `${cleanInline([...node.childNodes].map(inline).join(''))}\n\n`;
  if (tag === 'pre') return `\`\`\`text\n${node.textContent.trim()}\n\`\`\`\n\n`;
  if (tag === 'table') return table(node);
  if (tag === 'ul') return list(node, false);
  if (tag === 'ol') return list(node, true);
  if (tag === 'figure') return figure(node);
  if (node.classList.contains('terminal')) {
    const text = node.querySelector('pre')?.textContent.trim() || '';
    return `\`\`\`console\n${text}\n\`\`\`\n\n`;
  }
  if (node.classList.contains('callout')) return callout(node);
  if (node.classList.contains('step')) return step(node);
  return [...node.childNodes].map(block).join('');
}

export function pageToMarkdown(page, html) {
  const document = new DOMParser().parseFromString(`<main>${html}</main>`, 'text/html');
  const body = [...document.querySelector('main').childNodes].map(block).join('');
  return `# ${page.title}\n\n${page.lede}\n\n${body}`
    .replace(/\n{3,}/g, '\n\n')
    .trim() + '\n';
}
