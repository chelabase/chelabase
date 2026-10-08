import { describe, expect, it } from 'vitest';
import { studioUrl } from './studio-url';

describe('studioUrl', () => {
  it('studio_url_trailing_slash_is_normalised', () => {
    expect(studioUrl('https://a.test/')).toBe('https://a.test');
    expect(studioUrl('https://a.test//')).toBe('https://a.test');
    expect(studioUrl('https://a.test')).toBe('https://a.test');
    expect(studioUrl(undefined)).toBe('http://localhost:3000');
    expect(studioUrl()).toBe('http://localhost:3000');
    expect(studioUrl('')).toBe('http://localhost:3000');
    expect(studioUrl('   ')).toBe('http://localhost:3000');
    expect(studioUrl(' https://a.test/ ')).toBe('https://a.test');
  });
});
