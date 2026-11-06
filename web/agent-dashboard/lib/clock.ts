/** The time a page is rendered at. A function of its own, so a page does not read the clock inline. */
export function nowMs(): number {
  return Date.now();
}
