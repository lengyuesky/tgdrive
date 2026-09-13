// 独立浏览器夹具：合成测试视频、虚拟网盘目录、合成应用目录，不读取项目 .env。
import { spawn } from 'node:child_process'
import { access, mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { resolve, dirname } from 'node:path'
import { fileURLToPath } from 'node:url'
import { crc32 } from 'node:zlib'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../../..')
await access(`${root}/frontend/dist/index.html`).catch(() => {
  throw new Error('请先执行 npm run check 构建前端')
})

const temporary = await mkdtemp(`${tmpdir()}/tgdrive-apps-browser-`)
const video = `${temporary}/video.webm`
const filesDir = `${temporary}/files`
const catalogDir = `${temporary}/catalog`

await mkdir(filesDir, { recursive: true })
await mkdir(catalogDir, { recursive: true })
await writeFile(`${filesDir}/测试文档.txt`, '自包含测试文件内容')

export function makeZip(files) {
  const entries = []
  let offset = 0
  const localChunks = []
  for (const [name, content] of Object.entries(files)) {
    const data = typeof content === 'string' ? Buffer.from(content, 'utf-8') : Buffer.from(content)
    const nameBuf = Buffer.from(name, 'utf-8')
    const crc = crc32(data)
    const size = data.length
    const header = Buffer.alloc(30)
    header.writeUInt32LE(0x04034b50, 0)
    header.writeUInt16LE(20, 4)
    header.writeUInt16LE(0, 6)
    header.writeUInt16LE(0, 8)
    header.writeUInt16LE(0, 10)
    header.writeUInt16LE(0, 12)
    header.writeUInt32LE(crc, 14)
    header.writeUInt32LE(size, 18)
    header.writeUInt32LE(size, 22)
    header.writeUInt16LE(nameBuf.length, 26)
    header.writeUInt16LE(0, 28)
    localChunks.push(header, nameBuf, data)
    entries.push({ nameBuf, crc, size, offset })
    offset += header.length + nameBuf.length + data.length
  }
  const cdChunks = []
  let cdOffset = offset
  let cdSize = 0
  for (const entry of entries) {
    const cd = Buffer.alloc(46)
    cd.writeUInt32LE(0x02014b50, 0)
    cd.writeUInt16LE(0x031e, 4)
    cd.writeUInt16LE(20, 6)
    cd.writeUInt16LE(0, 8)
    cd.writeUInt16LE(0, 10)
    cd.writeUInt16LE(0, 12)
    cd.writeUInt16LE(0, 14)
    cd.writeUInt32LE(entry.crc, 16)
    cd.writeUInt32LE(entry.size, 20)
    cd.writeUInt32LE(entry.size, 24)
    cd.writeUInt16LE(entry.nameBuf.length, 28)
    cd.writeUInt16LE(0, 30)
    cd.writeUInt16LE(0, 32)
    cd.writeUInt16LE(0, 34)
    cd.writeUInt16LE(0, 36)
    cd.writeUInt32LE((0o100644 << 16) >>> 0, 38)
    cd.writeUInt32LE(entry.offset, 42)
    cdChunks.push(cd, entry.nameBuf)
    cdSize += cd.length + entry.nameBuf.length
  }
  const eocd = Buffer.alloc(22)
  eocd.writeUInt32LE(0x06054b50, 0)
  eocd.writeUInt16LE(0, 4)
  eocd.writeUInt16LE(0, 6)
  eocd.writeUInt16LE(entries.length, 8)
  eocd.writeUInt16LE(entries.length, 10)
  eocd.writeUInt32LE(cdSize, 12)
  eocd.writeUInt32LE(cdOffset, 16)
  eocd.writeUInt16LE(0, 20)
  return Buffer.concat([...localChunks, ...cdChunks, eocd])
}

// 预置合成应用以支持目录列表验证
const sampleApp = makeZip({
  'app.json': JSON.stringify({
    id: 'books',
    name: '图书',
    version: '1.0.0',
    api_version: 2,
    min_host_version: '0.1.0',
    description: '自包含合成测试图书应用',
    author: 'tgdrive',
    entry: 'index.html',
    permissions: ['files.read'],
    settings: [],
  }),
  'index.html': '<!doctype html><html><body><h1>Books</h1></body></html>',
})
await writeFile(`${catalogDir}/books-1.0.0.tgapp`, sampleApp)

function runCmd(command, args, options = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { stdio: 'inherit', ...options })
    child.on('error', reject)
    child.on('exit', (code) => (code === 0 ? resolve() : reject(new Error(`${command} 退出码：${code}`))))
  })
}

try {
  await runCmd('ffmpeg', [
    '-hide_banner',
    '-loglevel',
    'error',
    '-f',
    'lavfi',
    '-i',
    'testsrc2=duration=2:size=320x568:rate=15',
    '-c:v',
    'libvpx-vp9',
    '-b:v',
    '150k',
    '-an',
    video,
  ])
} catch (error) {
  await rm(temporary, { recursive: true, force: true })
  throw new Error('合成浏览器测试视频失败，请检查 ffmpeg', { cause: error })
}

const child = spawn(
  'cargo',
  ['test', '--locked', 'apps::tests::browser_fixture', '--', '--ignored', '--exact', '--nocapture'],
  {
    cwd: root,
    stdio: 'inherit',
    env: {
      ...process.env,
      TGDRIVE_TEST_VIDEO: video,
      TGDRIVE_TEST_FILES_DIR: filesDir,
      TGDRIVE_APP_CATALOG_DIR: catalogDir,
      CARGO_TARGET_DIR: process.env.CARGO_TARGET_DIR ?? `${tmpdir()}/tgdrive-codex-target`,
    },
  }
)

for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => child.kill(signal))
child.on('error', async (error) => {
  console.error(error)
  await rm(temporary, { recursive: true, force: true })
  process.exit(1)
})
child.on('exit', async (code) => {
  await rm(temporary, { recursive: true, force: true })
  process.exit(code ?? 0)
})
