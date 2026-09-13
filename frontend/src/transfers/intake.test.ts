import { defineComponent } from 'vue'
import { mount, type VueWrapper } from '@vue/test-utils'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createUploadIntake, type UploadIntake } from './intake'
import type { TransferController } from './controller'

vi.mock('../api', () => ({ errMsg: (error: Error) => error.message }))
vi.mock('../dialog', () => ({ appAlert: vi.fn() }))
vi.mock('element-plus', () => ({ ElMessage: { error: vi.fn() } }))

function deferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((done) => { resolve = done })
  return { promise, resolve }
}
function directory(getFile = vi.fn().mockResolvedValue(new File(['内容'], '照片.jpg')), name = '照片') {
  const values = vi.fn(async function* () { yield { kind: 'file', name: '照片.jpg', getFile } })
  return { kind: 'directory', name, values }
}
const wrappers: VueWrapper[] = []
const picker = vi.fn()
let session: AbortController
let queue: {
  signal: AbortSignal
  enqueue: ReturnType<typeof vi.fn>
  clearDeferredFiles: ReturnType<typeof vi.fn>
  setDeferredFileCount: ReturnType<typeof vi.fn>
  waitForCapacity: ReturnType<typeof vi.fn>
  resolveUploadConflicts: ReturnType<typeof vi.fn>
  onCancelled: ReturnType<typeof vi.fn>
}
function setup() {
  let intake!: UploadIntake
  wrappers.push(mount(defineComponent({ setup() { intake = createUploadIntake(queue as unknown as TransferController); return () => null } })))
  return intake
}
beforeEach(() => {
  picker.mockReset()
  Object.defineProperty(window, 'showDirectoryPicker', { value: picker, configurable: true })
  session = new AbortController()
  queue = {
    signal: session.signal,
    enqueue: vi.fn().mockResolvedValue(undefined),
    clearDeferredFiles: vi.fn(), setDeferredFileCount: vi.fn(),
    waitForCapacity: vi.fn().mockResolvedValue(true),
    resolveUploadConflicts: vi.fn().mockResolvedValue('overwrite'),
    onCancelled: vi.fn().mockReturnValue(() => {}),
  }
})
afterEach(() => {
  for (const wrapper of wrappers.splice(0)) wrapper.unmount()
  Reflect.deleteProperty(window, 'showDirectoryPicker')
})

describe('会话级文件夹扫描', () => {
  it('扫描中切换目录仍使用选择器打开时的目标', async () => {
    const pending = deferred<File>()
    const getFile = vi.fn(() => pending.promise)
    picker.mockResolvedValue(directory(getFile))
    const intake = setup()
    intake.targetDirectory.value = '/原始目录'
    const scan = intake.pickDirectory()
    await vi.waitFor(() => expect(getFile).toHaveBeenCalledOnce())
    intake.targetDirectory.value = '/另一个目录'
    const file = new File(['图片'], '照片.jpg')
    pending.resolve(file)
    await scan
    expect(queue.enqueue).toHaveBeenCalledWith('/原始目录', [{ file, rel: '照片' }], 'overwrite', true)
  })

  it('系统选择器打开期间注销，迟到的目录句柄不能重新启动扫描', async () => {
    const pending = deferred<ReturnType<typeof directory>>()
    picker.mockReturnValue(pending.promise)
    const intake = setup()
    const scan = intake.pickDirectory()
    session.abort()
    const root = directory()
    pending.resolve(root)
    await scan
    expect(root.values).not.toHaveBeenCalled()
    expect(queue.resolveUploadConflicts).not.toHaveBeenCalled()
    expect(queue.enqueue).not.toHaveBeenCalled()
  })

  it('读取文件期间注销，返回的 File 不再入队或触发新文件读取', async () => {
    const pending = deferred<File>()
    const getFile = vi.fn(() => pending.promise)
    picker.mockResolvedValue(directory(getFile))
    const intake = setup()
    const scan = intake.pickDirectory()
    await vi.waitFor(() => expect(getFile).toHaveBeenCalledOnce())
    session.abort()
    pending.resolve(new File(['图片'], '照片.jpg'))
    await scan
    expect(queue.enqueue).not.toHaveBeenCalled()
    expect(getFile).toHaveBeenCalledOnce()
  })

  it('旧扫描迟到的取消结果不会误停新一轮目录扫描', async () => {
    const oldConflict = deferred<string>()
    const newFile = deferred<File>()
    const getFile = vi.fn(() => newFile.promise)
    queue.resolveUploadConflicts.mockImplementationOnce(() => oldConflict.promise)
    picker.mockResolvedValueOnce(directory()).mockResolvedValueOnce(directory(getFile, '新照片'))
    const intake = setup()
    const oldScan = intake.pickDirectory()
    await vi.waitFor(() => expect(queue.resolveUploadConflicts).toHaveBeenCalledOnce())
    const newScan = intake.pickDirectory()
    await vi.waitFor(() => expect(getFile).toHaveBeenCalledOnce())
    oldConflict.resolve('cancel')
    await oldScan
    const file = new File(['新图片'], '照片.jpg')
    newFile.resolve(file)
    await newScan
    expect(queue.enqueue).toHaveBeenCalledWith('/', [{ file, rel: '新照片' }], 'overwrite', true)
  })
})
