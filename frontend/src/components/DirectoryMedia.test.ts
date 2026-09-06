import { flushPromises, mount } from '@vue/test-utils'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { entry } from '../test/fixtures'
import DirectoryMedia from './DirectoryMedia.vue'
import { contentFailure, contentVersion } from '../media/source'

const mocks = vi.hoisted(() => ({ acquire: vi.fn() }))
vi.mock('../media/loader', () => ({ mediaLoader: { acquire: mocks.acquire } }))
let view: ReturnType<typeof mount> | undefined
afterEach(() => { view?.unmount(); mocks.acquire.mockReset() })

describe('虚拟条目媒体绑定', () => {
  it('换路径/版本/代次释放引用，晚到旧URL不能串图，卸载取消', async () => {
    const jobs: { resolve: (url: string) => void; release: ReturnType<typeof vi.fn> }[] = []
    mocks.acquire.mockImplementation(() => {
      let resolve!: (url: string) => void
      const result = new Promise<string>((done) => { resolve = done })
      const release = vi.fn(); jobs.push({ resolve, release })
      return { result, release }
    })
    const item = ({ ...entry(1), ...{ name: 'one.jpg', has_thumbnail: true } })
    view = mount(DirectoryMedia, { props: { entry: item, path: '/one.jpg', video: false, generation: 1 }, global: { stubs: { ElIcon: true } } })
    expect(mocks.acquire.mock.calls[0]![0]).toMatchObject({ path: '/one.jpg', thumbnailUrl: '/api/fs/thumbnail?path=%2Fone.jpg' })
    await view.setProps({ path: '/other/one.jpg', generation: 2 })
    expect(jobs[0]!.release).toHaveBeenCalledTimes(1)
    jobs[1]!.resolve('blob:new'); await flushPromises()
    jobs[0]!.resolve('blob:old'); await flushPromises()
    expect(view.find('img').attributes('src')).toBe('blob:new')
    await view.setProps({ entry: { ...item, modified_at: item.modified_at + 1 } })
    expect(jobs[1]!.release).toHaveBeenCalledTimes(1)
    view.unmount(); view = undefined
    expect(jobs[2]!.release).toHaveBeenCalledTimes(1)
  })
  it('视频请求永远无originalUrl，无小图也不绕过loader；失败标记按内容保留', async () => {
    mocks.acquire.mockReturnValue({ result: Promise.resolve(null), release: vi.fn() })
    const item = ({ ...entry(1), ...{ name: 'movie.mp4', has_thumbnail: false } })
    view = mount(DirectoryMedia, { props: { entry: item, path: '/movie.mp4', video: true, generation: 1 }, global: { stubs: { ElIcon: true } } })
    await flushPromises()
    expect(mocks.acquire.mock.calls[0]![0]).toMatchObject({ thumbnailUrl: null, originalUrl: null })
    expect(view.find('img').exists()).toBe(false)
    const failure = contentFailure(item, '/movie.mp4', contentVersion(item)); failure.blocked = true
    expect(contentFailure(item, '/movie.mp4', contentVersion(item)).blocked).toBe(true)
    expect(contentFailure(item, '/moved.mp4', contentVersion(item)).blocked).toBe(false)
  })
})
