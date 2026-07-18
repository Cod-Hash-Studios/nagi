import { describe, expect, test } from 'bun:test';
import { siteBase, withSiteBase, withoutSiteBase } from './site-path.mjs';

describe('GitHub Pages paths', () => {
  test('uses the repository base', () => {
    expect(siteBase).toBe('/nagi');
  });

  test.each([
    ['/', '/nagi/'],
    ['/docs/', '/nagi/docs/'],
    ['/nagi/docs/', '/nagi/docs/'],
    ['https://github.com/Cod-Hash-Studios/nagi', 'https://github.com/Cod-Hash-Studios/nagi'],
  ])('prefixes %s as %s', (pathname, expected) => {
    expect(withSiteBase(pathname)).toBe(expected);
  });

  test.each([
    ['/nagi', '/'],
    ['/nagi/', '/'],
    ['/nagi/docs/', '/docs/'],
    ['/docs/', '/docs/'],
  ])('strips %s as %s', (pathname, expected) => {
    expect(withoutSiteBase(pathname)).toBe(expected);
  });
});
