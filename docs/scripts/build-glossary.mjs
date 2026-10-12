// Generates the glossary page for every locale from terminology.yaml.
//
// terminology.yaml is the single source of terms; these pages are derived.
// Regenerate with `pnpm --filter docs glossary`; never edit them by hand.
import {mkdirSync, readFileSync, writeFileSync} from 'node:fs';
import {dirname, join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {load} from 'js-yaml';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const doc = load(readFileSync(join(root, 'terminology.yaml'), 'utf8'));
const terms = doc.terms ?? [];

const LOCALES = {
  en: {
    title: 'Glossary',
    description:
      'One word per concept, defined once — the vocabulary every page draws from.',
    intro:
      'One word per concept, defined once. A term links here at its first appearance on a page.',
    heading: (t) => `## ${t.en} {/* #${t.id} */}`,
    line: (t) => t.definition.en,
    avoidPrefix: 'Avoid',
    out: ['docs', 'glossary.md'],
  },
  'zh-CN': {
    title: '术语表',
    description: '一个概念一个词，定义一次——所有页面共同使用的词汇。',
    intro:
      '一个概念一个词，定一次。术语在页面上第一次出现时链接到这里。',
    heading: (t) => `## ${t.zh} {/* #${t.id} */}`,
    line: (t) => t.definition.zh,
    avoidPrefix: '避免使用',
    out: ['i18n', 'zh-CN', 'docusaurus-plugin-content-docs', 'current', 'glossary.md'],
  },
  ja: {
    title: '用語集',
    description: '1 概念に 1 語、定義は一度だけ——すべてのページが使う語彙。',
    intro:
      '1 概念に 1 語、定義は一度だけ。用語がページに初めて現れるとき、ここへリンクします。',
    heading: (t) => `## ${t.ja} {/* #${t.id} */}`,
    line: (t) => t.definition.ja,
    avoidPrefix: '避ける表記',
    out: ['i18n', 'ja', 'docusaurus-plugin-content-docs', 'current', 'glossary.md'],
  },
};

for (const [, spec] of Object.entries(LOCALES)) {
  const lines = [];
  lines.push('---');
  lines.push(`title: ${spec.title}`);
  lines.push(`description: ${spec.description}`);
  lines.push('type: reference');
  lines.push('sidebar_position: 6');
  lines.push('---');
  lines.push('');
  lines.push(`# ${spec.title} {/* #glossary */}`);
  lines.push('');
  lines.push(spec.intro);
  lines.push('');
  lines.push('{/* generated from terminology.yaml — do not edit by hand */}');
  for (const term of terms) {
    lines.push('');
    lines.push(spec.heading(term));
    lines.push('');
    lines.push(spec.line(term));
    if (Array.isArray(term.avoid) && term.avoid.length > 0) {
      lines.push('');
      lines.push(`_${spec.avoidPrefix}_: ${term.avoid.join(', ')}.`);
    }
  }
  lines.push('');

  const target = join(root, ...spec.out);
  mkdirSync(dirname(target), {recursive: true});
  writeFileSync(target, lines.join('\n'));
  console.log(`wrote ${spec.out.join('/')} (${terms.length} terms)`);
}
