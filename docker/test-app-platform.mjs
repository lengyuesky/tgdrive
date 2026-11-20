import { execFileSync, spawnSync } from 'node:child_process'
import { randomBytes } from 'node:crypto'
import { closeSync, openSync } from 'node:fs'
import { copyFile, mkdir, mkdtemp, readFile, readdir, rename } from 'node:fs/promises'
import { dirname, join, resolve } from 'node:path'
import { homedir, tmpdir } from 'node:os'
import { fileURLToPath } from 'node:url'
import { parseArgs } from 'node:util'

// 只复制已跟踪 Rust 源码和列出的测试文件，不读取 .env 或真实 data/。
const root = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const appRoot = resolve(root, '../tgdrive-app')
const { values } = parseArgs({ options: {
  'runtime-image': { type: 'string', default: 'tgdrive:local' },
  'builder-image': { type: 'string' }, output: { type: 'string' },
  'cgroup-root': { type: 'string' }, example: { type: 'string' }, cinema: { type: 'string' },
} })
const docker = (...args) => execFileSync('docker', args, { encoding: 'utf8', maxBuffer: 2 * 1024 * 1024 }).trim()
const architecture = docker('image', 'inspect', values['runtime-image'], '--format', '{{.Architecture}}')
if (!['arm64', 'amd64'].includes(architecture)) throw new Error('验收只支持已有的 Linux arm64/amd64 镜像')
const builder = values['builder-image'] || `app-native-builder:${architecture}`
if (docker('image', 'inspect', builder, '--format', '{{.Architecture}}') !== architecture) throw new Error('构建镜像与运行镜像架构不一致')
const directory = values.output ? resolve(values.output) : await mkdtemp(join(tmpdir(), 'tgdrive-linux-'))
if (values.output) {
  await mkdir(directory, { recursive: true })
  if ((await readdir(directory)).length) throw new Error('验收输出目录必须为空，拒绝覆盖现有内容')
}
const source = join(directory, 'tgdrive')
await mkdir(source)
const tracked = execFileSync('git', ['-C', root, 'ls-files', '--cached', '-z'], { encoding: 'utf8' }).split('\0').filter(Boolean)
const extra = ['src/apps/linux_acceptance_tests.rs', 'src/apps/linux_end_to_end_tests.rs', 'docker/app-platform-probe/Cargo.toml',
  'docker/app-platform-probe/Cargo.lock', 'docker/app-platform-probe/src/main.rs']
for (const relative of new Set([...tracked, ...extra])) {
  if (!(relative === 'Cargo.toml' || relative === 'Cargo.lock' || relative.startsWith('src/') ||
    relative.startsWith('migrations/') || relative.startsWith('docker/app-platform-probe/'))) continue
  const destination = join(source, relative)
  await mkdir(dirname(destination), { recursive: true })
  await copyFile(join(root, relative), destination)
}
const { createRustSnapshot } = await import(`${appRoot}/tools-v1/source-snapshot.mjs`)
await rename(await createRustSnapshot(appRoot), join(directory, 'tgdrive-app'))
const output = join(directory, 'output')
await mkdir(output)
if (values.cinema && !values.example) throw new Error('--cinema 必须与 --example 一起提供')
if (values.example) await copyFile(resolve(values.example), join(directory, 'example-backend.tgapp'))
if (values.cinema) await copyFile(resolve(values.cinema), join(directory, 'cinema.tgapp'))
const imageContext = join(directory, 'image-context')
await mkdir(imageContext)
await copyFile(join(root, 'docker/Dockerfile.app-platform-test'), join(imageContext, 'Dockerfile'))
const suffix = `${process.pid}-${randomBytes(4).toString('hex')}`
const image = `tgdrive-app-platform-test:${suffix}`
const container = `tgdrive-app-platform-${suffix}`
const triple = architecture === 'arm64' ? 'aarch64-unknown-linux-gnu' : 'x86_64-unknown-linux-gnu'
let imageCreated = false
const quietly = (...args) => { try { execFileSync('docker', args, { stdio: 'ignore' }) } catch {} }
async function run(label, args, logName) {
  const log = join(output, logName)
  const fd = openSync(log, 'w')
  let result
  try { result = spawnSync('docker', args, { stdio: ['ignore', fd, fd] }) } finally { closeSync(fd) }
  const text = await readFile(log, 'utf8')
  if (result.error || result.status !== 0) {
    console.error(text.split('\n').slice(-18).join('\n'))
    throw new Error(`${label} 失败，完整日志：${log}`, { cause: result.error })
  }
  for (const line of text.split('\n').filter(line => /test result:|隔离验收：|端到端：/.test(line))) console.log(line)
  console.log(`${label}通过，日志：${log}`)
}
console.log(`Linux 验收只读源码快照与日志：${directory}`)
try {
  await run('测试镜像构建', ['build', '--pull=false',
    '--build-arg', `RUNTIME_IMAGE=${values['runtime-image']}`, '--build-arg', `BUILDER_IMAGE=${builder}`,
    '--tag', image, imageContext], 'image-build.log')
  imageCreated = true
  const endToEnd = values.example ? `TGDRIVE_TEST_HOST_EXECUTABLE=/work/output/host/debug/tgdrive TGDRIVE_TEST_EXAMPLE_PACKAGE=/work/example-backend.tgapp ${values.cinema ? 'TGDRIVE_TEST_CINEMA_PACKAGE=/work/cinema.tgapp ' : ''}cargo test --offline --locked apps::linux_end_to_end_tests::linux_static_packages_end_to_end -- --ignored --exact --nocapture --test-threads=1\n` : ''
  const command = `set -eu
chmod 555 /tmp /var/tmp
cargo fmt --check
cargo fmt --manifest-path docker/app-platform-probe/Cargo.toml --check
cargo build --offline --locked
cargo clippy --offline --locked --all-targets -- -D warnings
cargo test --offline --locked
cargo build --offline --locked --release --target ${triple} --manifest-path docker/app-platform-probe/Cargo.toml --target-dir /work/output/probe
cargo clippy --offline --locked --target ${triple} --manifest-path docker/app-platform-probe/Cargo.toml --target-dir /work/output/probe -- -D warnings
TGDRIVE_TEST_HOST_EXECUTABLE=/work/output/host/debug/tgdrive TGDRIVE_TEST_PROBE_EXECUTABLE=/work/output/probe/${triple}/release/tgdrive-isolation-probe cargo test --offline --locked apps::linux_acceptance_tests::linux_root_isolation -- --ignored --exact --nocapture --test-threads=1
${endToEnd}`
  const args = ['run', '--rm', '--pull=never', '--init', '--name', container, '--platform', `linux/${architecture}`,
    '--network', 'none', '--user', '0:0',
    '--mount', `type=bind,source=${directory},target=/work,readonly`,
    '--mount', `type=bind,source=${output},target=/work/output`,
    '--mount', `type=bind,source=${join(homedir(), '.cargo/registry')},target=/usr/local/cargo/registry,readonly`,
    '--env', 'CARGO_TARGET_DIR=/work/output/host', '--env', 'RUSTFLAGS=', '--env', 'CC=cc', '--env', 'AR=ar',
    '--env', 'TGDRIVE_TEST_HOST_SECRET=must-not-reach-application']
  if (values['cgroup-root']) {
    args.push('--cgroupns=host', '--mount', `type=bind,source=${resolve(values['cgroup-root'])},target=/sys/fs/cgroup/tgdrive-apps`,
      '--env', 'TGDRIVE_APP_CGROUP_ROOT=/sys/fs/cgroup/tgdrive-apps', '--env', 'TGDRIVE_TEST_REQUIRE_CGROUP=1')
  }
  args.push(image, '-c', command)
  await run('Linux 全量门禁及真实隔离验收', args, 'linux-acceptance.log')
} finally {
  quietly('rm', '--force', container)
  if (imageCreated) quietly('image', 'rm', image)
}
console.log('Linux 隔离验收完成；临时容器和测试镜像已清理，快照与日志保留供复查。')
