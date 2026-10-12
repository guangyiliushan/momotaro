// Fails when a content page misses its front-matter description, exceeds
// 150 characters, or carries no or an unknown `type:` — the page contract
// both the build and the machine-readable index rely on.
// Run via `pnpm --filter docs check:pages`.
import {readdirSync, readFileSync, statSync} from 'node:fs';
import {dirname, join} from 'node:path';
import {fileURLToPath} from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const ROOTS = [
  join(root, 'docs'),
  join(root, 'i18n', 'zh-CN', 'docusaurus-plugin-content-docs', 'current'),
  join(root, 'i18n', 'ja', 'docusaurus-plugin-content-docs', 'current'),
];
const LIMIT = 150;
const TYPES = new Set(['concept', 'task', 'reference', 'troubleshooting', 'meta']);
const problems = [];

function check(file) {
  const text = readFileSync(file, 'utf8');
  const rel = file.slice(root.length + 1).replaceAll('\\', '/');
  const fence = /^---\r?\n([\s\S]*?)\r?\n---/.exec(text);
  if (!fence) {
    problems.push(`${rel}: no front matter`);
    return;
  }
  const line = /^description:\s*(.*)$/m.exec(fence[1]);
  if (!line) {
    problems.push(`${rel}: missing description`);
  } else {
    const value = line[1].trim().replace(/^["']|["']$/g, '');
    if (value.length === 0) {
      problems.push(`${rel}: empty description`);
    } else if (value.length > LIMIT) {
      problems.push(`${rel}: description is ${value.length} chars (limit ${LIMIT})`);
    }
  }
  const type = /^type:\s*(.*)$/m.exec(fence[1]);
  if (!type) {
    problems.push(`${rel}: missing type`);
  } else if (!TYPES.has(type[1].trim())) {
    problems.push(`${rel}: unknown type "${type[1].trim()}"`);
  }
}

function walk(dir) {
  let entries = [];
  try {
    entries = readdirSync(dir);
  } catch {
    return; // a locale tree may not exist yet
  }
  for (const name of entries) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) {
      walk(p);
    } else if (/\.(md|mdx)$/.test(name) && !name.startsWith('_')) {
      check(p);
    }
  }
}

for (const dir of ROOTS) {
  walk(dir);
}

if (problems.length > 0) {
  console.error(`page check failed (${problems.length}):`);
  for (const p of problems) {
    console.error(`  - ${p}`);
  }
  process.exit(1);
}
console.log('page check ok');
