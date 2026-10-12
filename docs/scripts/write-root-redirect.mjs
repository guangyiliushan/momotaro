// Writes the site-root hop after `docusaurus build`.
//
// Every locale lives under its own path segment, so `/momotaro/` itself is
// served by this small meta-refresh page pointing at the default locale.
// The Docusaurus build never touches the build root directory, so writing
// this file after the build is stable across rebuilds.
import {existsSync, writeFileSync} from 'node:fs';
import {dirname, join} from 'node:path';
import {fileURLToPath} from 'node:url';

const build = join(dirname(fileURLToPath(import.meta.url)), '..', 'build');
const target = './en/';

if (!existsSync(build)) {
  console.error('build/ is missing — run `docusaurus build` first');
  process.exit(1);
}

const html = `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta http-equiv="refresh" content="0; url=${target}" />
    <meta name="robots" content="noindex" />
    <title>Momotaro</title>
  </head>
  <body>
    <a href="${target}">Continue to the documentation</a>
  </body>
</html>
`;

writeFileSync(join(build, 'index.html'), html);
console.log(`wrote build/index.html → ${target}`);
