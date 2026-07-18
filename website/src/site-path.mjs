export const siteBase = '/nagi';

export function withSiteBase(pathname) {
  if (!pathname.startsWith('/')) return pathname;
  if (pathname === siteBase || pathname.startsWith(`${siteBase}/`)) return pathname;
  return `${siteBase}${pathname}`;
}

export function withoutSiteBase(pathname) {
  if (pathname === siteBase) return '/';
  if (pathname.startsWith(`${siteBase}/`)) return pathname.slice(siteBase.length);
  return pathname;
}
