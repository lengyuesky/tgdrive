import { execFile } from 'node:child_process'
import { mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { resolve } from 'node:path'
import { promisify } from 'node:util'
import { afterEach, expect, it } from 'vitest'

const run = promisify(execFile)
const apps = resolve(process.cwd(), '../apps')
const temporary: string[] = []
async function fixture() {
  const source = await mkdtemp(`${tmpdir()}/tgdrive-package-test-`)
  temporary.push(source)
  await writeFile(`${source}/app.json`, JSON.stringify({ id: 'custom-viewer', name: '自定义阅读器', version: '1.2.3', api_version: 2, min_host_version: '0.1.0', author: '测试', description: '合成打包夹具', entry: 'index.html', permissions: [], settings: [] }))
  await writeFile(`${source}/index.html`, '<!doctype html><html lang="zh-CN"><head><script defer src="./tgdrive-sdk.js"></script></head><body>独立插件</body></html>')
  return source
}
afterEach(async () => { await Promise.all(temporary.splice(0).map((path) => rm(path, { recursive: true, force: true }))) })

it('build:apps 保留指定静态源码目录和输出目录，与直接打包生成相同包', async () => {
  const source = await fixture()
  // 输出放在源码目录的同级，防止第二次打包把第一次的产物一起收进包。
  const output = await mkdtemp(`${tmpdir()}/tgdrive-package-output-`)
  temporary.push(output)
  await run(process.execPath, [`${apps}/build.mjs`, source, `${output}/legacy`])
  await run(process.execPath, [`${apps}/package.mjs`, source, `${output}/direct`])
  const name = 'custom-viewer-1.2.3.tgapp'
  expect(await readdir(`${output}/legacy`)).toEqual([name])
  expect(await readFile(`${output}/legacy/${name}`)).toEqual(await readFile(`${output}/direct/${name}`))
})

it('指定源码而未给输出参数时仍尊重目录环境配置', async () => {
  const source = await fixture()
  const output = await mkdtemp(`${tmpdir()}/tgdrive-package-output-`)
  temporary.push(output)
  await run(process.execPath, [`${apps}/build.mjs`, source], { env: { ...process.env, TGDRIVE_APP_BUILD_OUTPUT: output } })
  expect(await readdir(output)).toEqual(['custom-viewer-1.2.3.tgapp'])
})
