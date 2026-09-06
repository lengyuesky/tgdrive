import type { Entry } from '../api'

export interface FrozenTarget { path: string; entry: Readonly<Entry> }
export interface OperationFailure { path: string; error: string }
export interface OperationProgress { completed: number; total: number; failed: number }

export function freezeTargets(base: string, entries: readonly Entry[]): FrozenTarget[] {
  return entries.map((entry) => ({
    path: base === '/' ? '/' + entry.name : base + '/' + entry.name,
    entry: Object.freeze({ ...entry }),
  }))
}

/** 单请求最多 200 项，串行推进；网络失败也记录到对应批次，不丢已完成进度。 */
export async function deleteBatches(
  targets: readonly FrozenTarget[],
  remove: (paths: string[]) => Promise<{ failed: OperationFailure[] }>,
  progress: (state: OperationProgress) => void,
): Promise<OperationFailure[]> {
  const failed: OperationFailure[] = []
  for (let start = 0; start < targets.length; start += 200) {
    const paths = targets.slice(start, start + 200).map((target) => target.path)
    try { failed.push(...(await remove(paths)).failed) }
    catch (error) { failed.push(...paths.map((path) => ({ path, error: String(error) }))) }
    progress({ completed: Math.min(start + 200, targets.length), total: targets.length, failed: failed.length })
  }
  return failed
}

export async function moveSerial(
  targets: readonly FrozenTarget[], destination: string,
  move: (from: string, to: string) => Promise<unknown>,
  progress: (state: OperationProgress) => void,
): Promise<OperationFailure[]> {
  const failed: OperationFailure[] = []
  for (let i = 0; i < targets.length; i++) {
    const target = targets[i]!
    const to = destination === '/' ? '/' + target.entry.name : destination + '/' + target.entry.name
    try { await move(target.path, to) }
    catch (error) { failed.push({ path: target.path, error: String(error) }) }
    progress({ completed: i + 1, total: targets.length, failed: failed.length })
  }
  return failed
}
