import { readdir, readFile } from 'node:fs/promises';
import { extname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const websiteRoot = resolve(fileURLToPath(new URL('..', import.meta.url)));
const distRoot = join(websiteRoot, 'dist');
const failures = [];

for (const path of await filesUnder(distRoot)) {
  if (!['.html', '.css'].includes(extname(path))) continue;
  const content = await readFile(path, 'utf8');
  const relativePath = relative(distRoot, path);

  if (/\b(?:href|src)=["']\/(?!nagi(?:\/|["'])|\/)/.test(content)) {
    failures.push(`${relativePath}: contains a root-relative URL outside /nagi`);
  }
  if (/url\(["']?\/(?!nagi(?:\/|["']))/.test(content)) {
    failures.push(`${relativePath}: contains a root-relative CSS URL outside /nagi`);
  }
  if (content.includes('/nagi/nagi/')) {
    failures.push(`${relativePath}: contains a double /nagi prefix`);
  }
}

if (failures.length > 0) {
  console.error(failures.join('\n'));
  process.exitCode = 1;
} else {
  console.log('built site URLs stay inside the /nagi GitHub Pages base');
}

async function filesUnder(directory) {
  const files = [];
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) files.push(...(await filesUnder(path)));
    else if (entry.isFile()) files.push(path);
  }
  return files;
}
