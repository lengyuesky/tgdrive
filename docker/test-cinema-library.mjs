import { execFile, execFileSync, spawn } from 'node:child_process'
import { randomBytes } from 'node:crypto'
import { copyFile, cp, mkdir, mkdtemp, readFile, rename, writeFile } from 'node:fs/promises'
import { dirname, join, resolve } from 'node:path'
import { homedir, tmpdir } from 'node:os'
import { fileURLToPath } from 'node:url'
import { parseArgs, promisify } from 'node:util'

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const { values } = parseArgs({ options: {
  'runtime-image': { type: 'string', default: 'tgdrive-stage2-local:latest' },
  'builder-image': { type: 'string' }, cinema: { type: 'string' }, output: { type: 'string' }, 'cargo-target': { type: 'string' },
} })
if (!values.cinema) throw new Error('须用 --cinema 指定已经核验的真实影视 Linux 应用包')
const docker = (...args) => execFileSync('docker', args, { encoding: 'utf8', timeout: 30_000, maxBuffer: 2 * 1024 * 1024 }).trim()
const quietly = (...args) => { try { docker(...args) } catch {} }
const architecture = docker('image', 'inspect', values['runtime-image'], '--format', '{{.Architecture}}')
if (!['arm64', 'amd64'].includes(architecture)) throw new Error('只支持 Linux arm64/amd64 镜像')
const builder = values['builder-image'] ?? `app-native-builder:${architecture}`
if (docker('image', 'inspect', builder, '--format', '{{.Architecture}}') !== architecture) throw new Error('两个本地镜像的架构不一致')
const output = values.output ? resolve(values.output) : await mkdtemp(join(tmpdir(), 'tgdrive-cinema-browser-'))
if (values.output) await mkdir(output)
const source = join(output, 'tgdrive'), logs = join(output, 'logs')
await mkdir(source); await mkdir(logs)
// 只冻结跟踪的 Rust 源码；合成数据库由 cfg(test) 在容器内创建，不读取配置或真实数据。
const tracked = execFileSync('git', ['-C', root, 'ls-files', '--cached', '-z'], { encoding: 'utf8' }).split('\0').filter(Boolean)
for (const path of tracked.filter(path => ['Cargo.toml', 'Cargo.lock'].includes(path) || path.startsWith('src/') || path.startsWith('migrations/'))) {
  const destination = join(source, path); await mkdir(dirname(destination), { recursive: true }); await copyFile(join(root, path), destination)
}
const { createRustSnapshot } = await import(`${resolve(root, '../tgdrive-app')}/tools-v1/source-snapshot.mjs`)
await rename(await createRustSnapshot(resolve(root, '../tgdrive-app')), join(output, 'tgdrive-app'))
await cp(join(root, 'frontend/dist'), join(output, 'static'), { recursive: true })
await copyFile(resolve(values.cinema), join(output, 'cinema.tgapp'))
const work = join(output, 'work'); await mkdir(work)
const cargoTarget = values['cargo-target'] ? resolve(values['cargo-target']) : join(work, 'target')
await mkdir(cargoTarget, { recursive: true }); await mkdir(join(work, 'target'), { recursive: true })
const imageContext = join(output, 'image-context'); await mkdir(imageContext)
await copyFile(join(root, 'docker/Dockerfile.app-platform-test'), join(imageContext, 'Dockerfile'))
const suffix = `${process.pid}-${randomBytes(4).toString('hex')}`
const image = `tgdrive-cinema-browser:${suffix}`, container = `tgdrive-cinema-browser-${suffix}`, network = `${container}-network`
let imageCreated = false, networkCreated = false, containerCreated = false
async function run(program, args, log, options = {}) {
  const { open } = await import('node:fs/promises')
  const file = await open(join(logs, log), 'wx')
  try {
    await new Promise((resolvePromise, reject) => {
      const child = spawn(program, args, { cwd: root, ...options, stdio: ['ignore', file.fd, file.fd] })
      child.on('error', reject)
      child.on('exit', code => code === 0 ? resolvePromise() : reject(new Error(`${log} 失败（${code}），详见 ${join(logs, log)}`)))
    })
  } finally { await file.close() }
}
console.log(`真实影视浏览器回归输出：${output}`)
try {
  await run('docker', ['build', '--pull=false', '--build-arg', `RUNTIME_IMAGE=${values['runtime-image']}`, '--build-arg', `BUILDER_IMAGE=${builder}`, '--tag', image, imageContext], 'image.log')
  imageCreated = true
  const mounts = ['--mount', `type=bind,source=${output},target=/work,readonly`, '--mount', `type=bind,source=${work},target=/work/work`,
    '--mount', `type=bind,source=${cargoTarget},target=/work/work/target`,
    '--mount', `type=bind,source=${join(homedir(), '.cargo/registry')},target=/usr/local/cargo/registry,readonly`]
  await run('docker', ['run', '--rm', '--network', 'none', '--user', '0:0', ...mounts, '--env', 'CARGO_TARGET_DIR=/work/work/target',
    '--env', 'RUSTFLAGS=', '--env', 'CC=cc', '--env', 'AR=ar', image, '-ec', 'cargo build --offline --locked && cargo test --offline --locked --no-run'], 'compile.log')
  console.log('真实宿主及测试构建完成，正在运行网页添加媒体库流程')
  // 新版 Docker 的 internal 网络不发布端口；单独建测试网络，唯一入口绑定本机回环。
  docker('network', 'create', network); networkCreated = true
  docker('run', '--detach', '--init', '--name', container, '--network', network, '--publish', '127.0.0.1::8080', '--user', '0:0', ...mounts,
    '--env', 'CARGO_TARGET_DIR=/work/work/target', '--env', 'RUSTFLAGS=', '--env', 'CC=cc', '--env', 'AR=ar',
    '--env', 'TGDRIVE_TEST_HOST_EXECUTABLE=/work/work/target/debug/tgdrive', '--env', 'TGDRIVE_TEST_CINEMA_PACKAGE=/work/cinema.tgapp',
    '--env', 'TGDRIVE_TEST_STATIC_DIR=/work/static', '--env', 'TGDRIVE_TEST_BROWSER_READY=/work/work/ready', '--env', 'TGDRIVE_TEST_BROWSER_DONE=/work/work/done',
    image, '-ec', 'chmod 555 /tmp /var/tmp; cargo test --offline --locked apps::linux_end_to_end_tests::linux_cinema_library_browser -- --ignored --exact --nocapture --test-threads=1')
  containerCreated = true
  const deadline = Date.now() + 30_000
  while (true) {
    try { await readFile(join(work, 'ready')); break } catch {}
    if (Date.now() > deadline || docker('inspect', '--format', '{{.State.Running}}', container) !== 'true') throw new Error('合成宿主没有就绪')
    await new Promise(resolvePromise => setTimeout(resolvePromise, 200))
  }
  const binding = docker('port', container, '8080/tcp')
  if (!/^127\.0\.0\.1:\d+$/.test(binding)) throw new Error('测试端口必须只发布到本机')
  let browserFailure
  try {
    await run('npm', ['exec', '--', 'playwright', 'test', '--config', 'playwright.integration.config.ts'], 'browser.log', {
      cwd: join(root, 'frontend'), env: { ...process.env, TGDRIVE_SYNTHETIC_ORIGIN: `http://${binding}`, PLAYWRIGHT_OUTPUT_DIR: join(output, 'browser-results') },
    })
  } catch (error) { browserFailure = error }
  finally { await writeFile(join(work, 'done'), 'done\n') }
  const { stdout: exit } = await promisify(execFile)('docker', ['wait', container], { timeout: 30_000 })
  await writeFile(join(logs, 'server.log'), docker('logs', container))
  if (browserFailure) throw browserFailure
  if (exit.trim() !== '0') throw new Error('浏览器结束后的真实授权或进程回收断言失败')
  console.log('通过：真实影视页面、SDK、HTTP/SQLite、独立 UID 后端；取消授权保留草稿；两库新增并只授权选定目录；重开持久保留')
} catch (error) {
  if (containerCreated) await writeFile(join(logs, 'server-failure.log'), docker('logs', '--tail', '60', container))
  throw error
} finally {
  if (containerCreated) quietly('rm', '--force', container)
  if (networkCreated) quietly('network', 'rm', network)
  if (imageCreated) quietly('image', 'rm', image)
}
