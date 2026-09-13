// 通过主项目前端工具链构建独立插件；运行服务器不需要 Node.js。
import { createRequire } from 'node:module'
import { fileURLToPath } from 'node:url'
import { dirname, resolve } from 'node:path'
import { copyFile, mkdir, readdir } from 'node:fs/promises'
import { spawn } from 'node:child_process'
const root = dirname(fileURLToPath(import.meta.url))
const frontend = resolve(root, '../frontend')
const require = createRequire(`${frontend}/package.json`)
const modules = `${frontend}/node_modules`
const catalog = resolve(process.env.TGDRIVE_APP_BUILD_OUTPUT ?? `${root}/catalog`)
async function pack(source, destination = catalog) {
  await new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [`${root}/package.mjs`, source, destination], { stdio: 'inherit' })
    child.on('error', reject); child.on('exit', (code) => code === 0 ? resolve() : reject(new Error(`插件打包失败：${code}`)))
  })
}
async function buildReader(name) {
  const { build } = await import(require.resolve('vite'))
  const output = `${root}/.build/${name}`
  await build({ configFile: false, root: `${root}/${name}`, base: './', publicDir: false,
    resolve: { alias: { 'mediabunny': `${modules}/mediabunny/dist/modules/src/index.js`, 'pdfjs-dist': `${modules}/pdfjs-dist`, 'dompurify': `${modules}/dompurify/dist/purify.es.mjs`, '@zip.js/zip.js': `${modules}/@zip.js/zip.js` } },
    build: { outDir: output, emptyOutDir: true, target: 'es2022', sourcemap: false,
      rollupOptions: { output: { entryFileNames: 'assets/main-[hash].js', chunkFileNames: 'assets/chunk-[hash].js', assetFileNames: 'assets/resource-[hash][extname]' } },
      chunkSizeWarningLimit: 1800,
    },
  })
  await copyFile(`${root}/${name}/app.json`, `${output}/app.json`)
  await mkdir(`${output}/licenses`, { recursive: true })
  if (name === 'cinema') {
    await copyFile(`${modules}/mediabunny/LICENSE`, `${output}/licenses/mediabunny.txt`)
    await copyFile(`${root}/cinema/NOTICE.txt`, `${output}/licenses/NOTICE.txt`)
  } else await copyFile(`${modules}/@zip.js/zip.js/LICENSE`, `${output}/licenses/zip-js.txt`)
  if (name === 'books') {
    await copyFile(`${modules}/dompurify/LICENSE`, `${output}/licenses/dompurify.txt`)
    await copyFile(`${modules}/pdfjs-dist/LICENSE`, `${output}/licenses/pdfjs.txt`)
    for (const directory of ['cmaps', 'standard_fonts']) {
      await mkdir(`${output}/${directory}`, { recursive: true })
      for (const item of await readdir(`${modules}/pdfjs-dist/${directory}`, { withFileTypes: true })) {
        if (item.isFile()) await copyFile(`${modules}/pdfjs-dist/${directory}/${item.name}`, `${output}/${directory}/${item.name}`)
      }
    }
  }
  await pack(output)
}

if (process.argv[2]) {
  // 保留原 build:apps 传入已编译源码目录和输出目录的用法。
  await pack(resolve(process.argv[2]), resolve(process.argv[3] ?? catalog))
} else {
  await pack(`${root}/shorts`)
  for (const name of ['books', 'comics', 'cinema']) await buildReader(name)
}
