export const TABLE_PAGE_SIZE = 100;

export function viewportPages(start: number, end: number, total: number): number[] {
  const first = Math.max(0, Math.floor(start / TABLE_PAGE_SIZE) - 1) * TABLE_PAGE_SIZE;
  const last = Math.min(total, end + TABLE_PAGE_SIZE);
  const pages: number[] = [];
  for (let offset = first; offset < last; offset += TABLE_PAGE_SIZE) pages.push(offset);
  return pages;
}

/** Bounded LRU cache; returning to an evicted page fetches it again. */
export class TablePageCache<T> {
  private pages = new Map<number, T>();
  get size() {
    return this.pages.size;
  }
  clear() {
    this.pages.clear();
  }
  has(offset: number) {
    return this.pages.has(offset);
  }
  get(offset: number): T | undefined {
    const value = this.pages.get(offset);
    if (value !== undefined) {
      this.pages.delete(offset);
      this.pages.set(offset, value);
    }
    return value;
  }
  set(offset: number, value: T) {
    this.pages.delete(offset);
    this.pages.set(offset, value);
    while (this.pages.size > 32) this.pages.delete(this.pages.keys().next().value!);
  }
}
