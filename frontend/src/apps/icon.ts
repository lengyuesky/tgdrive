/** 应用图标配色：由应用 ID 哈希生成稳定的渐变背景，同一应用每次刷新颜色不变，不同应用色相不同。 */
export function appIconStyle(id: string): { background: string } {
  let hash = 0
  for (let i = 0; i < id.length; i++) hash = (Math.imul(hash, 31) + id.charCodeAt(i)) | 0
  const hue = Math.abs(hash) % 360
  return { background: `linear-gradient(140deg, hsl(${hue} 85% 64%), hsl(${(hue + 45) % 360} 78% 52%))` }
}
