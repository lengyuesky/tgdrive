export interface Point { x: number; y: number }
export function distance(a: Point, b: Point) { return Math.hypot(a.x - b.x, a.y - b.y) }
export function swipeDirection(dx: number, dy: number, zoom: number, multiple: boolean) {
  if (multiple || zoom !== 1 || Math.abs(dx) < 60 || Math.abs(dx) < Math.abs(dy) * 1.4) return 0
  return dx < 0 ? 1 : -1
}
export function boundedScale(value: number) { return Math.min(4, Math.max(1, value)) }
